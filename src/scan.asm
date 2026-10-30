; JTokens scanner, MASM x64 (Windows x64 ABI).
;
; The hot path: walks raw JSONL bytes from Claude Code and Codex session logs
; and writes one 64-byte record per usage event. No JSON parser: SIMD substring
; search (AVX2, SSE2 fallback) finds the fixed keys, then digits are parsed in place.
;
; Buffers passed in must have at least 64 readable zero bytes after `len`.

OPTION CASEMAP:NONE

REC_SIZE  equ 64
R_IN      equ 0         ; input tokens (codex: total input incl. cached)
R_OUT     equ 8         ; output tokens
R_CR      equ 16        ; cache read tokens
R_CW      equ 24        ; claude: cache creation total / codex: reasoning
R_C1H     equ 32        ; claude: 1h cache creation / codex: cumulative total
R_ID      equ 40        ; claude: FNV-1a of message.id
R_MOFF    equ 48        ; offset of model string in buffer
R_TS      equ 52        ; unix seconds (UTC)
R_MLEN    equ 56        ; model string length
R_KIND    equ 58        ; 1 = usage, 2 = model change (codex)
R_FLAGS   equ 59        ; bit0 = fast mode

; locals (above 32 bytes of shadow space)
L_LE      equ 32
L_MSG     equ 40
L_RBASE   equ 48
L_RLEN    equ 56

.data
ALIGN 4
g_avx2    dd 0

s_nl      db 10
s_usage   db '"usage":{"input_tokens":'
S_USAGE   equ $ - s_usage
s_tasst   db '"type":"assistant"'
S_TASST   equ $ - s_tasst
s_msg     db '"message":{'
S_MSG     equ $ - s_msg
s_role    db '"role":"assistant"'
S_ROLE    equ $ - s_role
s_model   db '"model":"'
S_MODEL   equ $ - s_model
s_id      db '"id":"'
S_ID      equ $ - s_id
s_ccit    db '"cache_creation_input_tokens":'
S_CCIT    equ $ - s_ccit
s_crit    db '"cache_read_input_tokens":'
S_CRIT    equ $ - s_crit
s_out     db '"output_tokens":'
S_OUT     equ $ - s_out
s_e1h     db '"ephemeral_1h_input_tokens":'
S_E1H     equ $ - s_e1h
s_fast    db '"speed":"fast"'
S_FAST    equ $ - s_fast
s_ts      db '"timestamp":"'
S_TS      equ $ - s_ts
s_tc      db '"payload":{"type":"token_count"'
S_TC      equ $ - s_tc
s_turn    db '"type":"turn_context"'
S_TURN    equ $ - s_turn
s_last    db '"last_token_usage":{'
S_LAST    equ $ - s_last
s_total   db '"total_token_usage":{'
S_TOTAL   equ $ - s_total
s_in      db '"input_tokens":'
S_IN      equ $ - s_in
s_cin     db '"cached_input_tokens":'
S_CIN     equ $ - s_cin
s_rout    db '"reasoning_output_tokens":'
S_ROUT    equ $ - s_rout
s_ttok    db '"total_tokens":'
S_TTOK    equ $ - s_ttok
s_tshead  db '{"timestamp":"'
S_TSHEAD  equ $ - s_tshead

.code

; rcx = base, rdx = len, needle label, needle length -> rax offset or -1
FINDR MACRO base, ln, ndl, nl
    mov rcx, base
    mov rdx, ln
    lea r8, ndl
    mov r9, nl
    call tk_find
ENDM

; Search the region stored in [rsp+L_RBASE]/[rsp+L_RLEN] for a key, parse the
; number after it into a record field. Leaves the field untouched if missing.
GETNUM MACRO ndl, nl, field
    LOCAL skip
    mov r10, [rsp+L_RBASE]
    add r10, r12
    mov r11, [rsp+L_RLEN]
    FINDR r10, r11, ndl, nl
    cmp rax, -1
    je skip
    mov rcx, [rsp+L_RBASE]
    add rcx, r12
    add rcx, rax
    add rcx, nl
    call parse_num
    mov qword ptr [r14 + field], rax
skip:
ENDM

ZEROREC MACRO
    xor eax, eax
    mov qword ptr [r14 + 0], rax
    mov qword ptr [r14 + 8], rax
    mov qword ptr [r14 + 16], rax
    mov qword ptr [r14 + 24], rax
    mov qword ptr [r14 + 32], rax
    mov qword ptr [r14 + 40], rax
    mov qword ptr [r14 + 48], rax
    mov qword ptr [r14 + 56], rax
ENDM

PROLOG8 MACRO
    push rbx
    .pushreg rbx
    push rbp
    .pushreg rbp
    push rsi
    .pushreg rsi
    push rdi
    .pushreg rdi
    push r12
    .pushreg r12
    push r13
    .pushreg r13
    push r14
    .pushreg r14
    push r15
    .pushreg r15
    sub rsp, 72
    .allocstack 72
    .endprolog
ENDM

EPILOG8 MACRO
    add rsp, 72
    pop r15
    pop r14
    pop r13
    pop r12
    pop rdi
    pop rsi
    pop rbp
    pop rbx
ENDM

;------------------------------------------------------------------------------
; void tk_init(void)  - detect AVX2 (CPU + OS YMM state)
;------------------------------------------------------------------------------
tk_init PROC FRAME
    push rbx
    .pushreg rbx
    .endprolog
    mov g_avx2, 0
    mov eax, 1
    xor ecx, ecx
    cpuid
    bt ecx, 27              ; OSXSAVE
    jnc ti_done
    bt ecx, 28              ; AVX
    jnc ti_done
    xor ecx, ecx
    xgetbv
    and eax, 6
    cmp eax, 6              ; XMM + YMM state enabled
    jne ti_done
    mov eax, 7
    xor ecx, ecx
    cpuid
    bt ebx, 5               ; AVX2
    jnc ti_done
    mov g_avx2, 1
ti_done:
    pop rbx
    ret
tk_init ENDP

;------------------------------------------------------------------------------
; int64 tk_has_avx2(void)
;------------------------------------------------------------------------------
tk_has_avx2 PROC
    mov eax, g_avx2
    ret
tk_has_avx2 ENDP

;------------------------------------------------------------------------------
; int64 tk_find(const u8* hay, u64 len, const u8* needle, u64 nlen)
; Returns offset of first match or -1. Compares the first and last needle byte
; over 32 (or 16) positions at a time, then verifies the middle.
; Clobbers only volatile registers plus saved rsi/rdi/rbx.
;------------------------------------------------------------------------------
tk_find PROC FRAME
    push rsi
    .pushreg rsi
    push rdi
    .pushreg rdi
    push rbx
    .pushreg rbx
    .endprolog

    mov rax, -1
    test r9, r9
    jz f_ret
    cmp rdx, r9
    jb f_ret
    mov rsi, rcx            ; hay
    mov rdi, r8             ; needle
    sub rdx, r9
    inc rdx                 ; rdx = candidate positions
    lea r8, [r9 - 1]        ; r8 = last index in needle
    xor r10, r10            ; i
    movzx eax, byte ptr [rdi]
    movzx ecx, byte ptr [rdi + r8]
    cmp g_avx2, 0
    je f_sse

    vmovd xmm0, eax
    vpbroadcastb ymm0, xmm0
    vmovd xmm1, ecx
    vpbroadcastb ymm1, xmm1
f_avx_loop:
    lea rax, [r10 + 32]
    cmp rax, rdx
    ja f_avx_end
    lea rbx, [rsi + r10]
    vpcmpeqb ymm2, ymm0, ymmword ptr [rbx]
    vpcmpeqb ymm3, ymm1, ymmword ptr [rbx + r8]
    vpand ymm2, ymm2, ymm3
    vpmovmskb r11d, ymm2
    test r11d, r11d
    jnz f_avx_cand
f_avx_next:
    add r10, 32
    jmp f_avx_loop
f_avx_cand:
    bsf ecx, r11d
    lea rbx, [rsi + r10]
    add rbx, rcx
    mov eax, 1
f_avx_v:
    cmp rax, r8
    jae f_hit_vz
    mov cl, byte ptr [rbx + rax]
    cmp cl, byte ptr [rdi + rax]
    jne f_avx_miss
    inc rax
    jmp f_avx_v
f_avx_miss:
    lea eax, [r11 - 1]
    and r11d, eax
    jnz f_avx_cand
    jmp f_avx_next
f_avx_end:
    vzeroupper
    jmp f_tail
f_hit_vz:
    vzeroupper
    jmp f_hit

f_sse:
    movd xmm0, eax
    punpcklbw xmm0, xmm0
    punpcklwd xmm0, xmm0
    pshufd xmm0, xmm0, 0
    movd xmm1, ecx
    punpcklbw xmm1, xmm1
    punpcklwd xmm1, xmm1
    pshufd xmm1, xmm1, 0
f_sse_loop:
    lea rax, [r10 + 16]
    cmp rax, rdx
    ja f_tail
    lea rbx, [rsi + r10]
    movdqu xmm2, xmmword ptr [rbx]
    movdqu xmm3, xmmword ptr [rbx + r8]
    pcmpeqb xmm2, xmm0
    pcmpeqb xmm3, xmm1
    pand xmm2, xmm3
    pmovmskb r11d, xmm2
    test r11d, r11d
    jnz f_sse_cand
f_sse_next:
    add r10, 16
    jmp f_sse_loop
f_sse_cand:
    bsf ecx, r11d
    lea rbx, [rsi + r10]
    add rbx, rcx
    mov eax, 1
f_sse_v:
    cmp rax, r8
    jae f_hit
    mov cl, byte ptr [rbx + rax]
    cmp cl, byte ptr [rdi + rax]
    jne f_sse_miss
    inc rax
    jmp f_sse_v
f_sse_miss:
    lea eax, [r11 - 1]
    and r11d, eax
    jnz f_sse_cand
    jmp f_sse_next

f_tail:
    cmp r10, rdx
    jae f_none
    lea rbx, [rsi + r10]
    mov cl, byte ptr [rbx]
    cmp cl, byte ptr [rdi]
    jne f_tail_next
    mov cl, byte ptr [rbx + r8]
    cmp cl, byte ptr [rdi + r8]
    jne f_tail_next
    mov eax, 1
f_tail_v:
    cmp rax, r8
    jae f_hit
    mov cl, byte ptr [rbx + rax]
    cmp cl, byte ptr [rdi + rax]
    jne f_tail_next
    inc rax
    jmp f_tail_v
f_tail_next:
    inc r10
    jmp f_tail

f_hit:
    mov rax, rbx
    sub rax, rsi
    jmp f_ret
f_none:
    mov rax, -1
f_ret:
    pop rbx
    pop rdi
    pop rsi
    ret
tk_find ENDP

;------------------------------------------------------------------------------
; parse_num: rcx = ptr to ASCII digits -> rax (stops at first non-digit)
;------------------------------------------------------------------------------
parse_num PROC
    xor eax, eax
pn_loop:
    movzx edx, byte ptr [rcx]
    sub edx, '0'
    cmp edx, 9
    ja pn_done
    imul rax, rax, 10
    add rax, rdx
    inc rcx
    jmp pn_loop
pn_done:
    ret
parse_num ENDP

;------------------------------------------------------------------------------
; fnv_str: rcx = ptr, hashes bytes until '"' (max 160) -> rax FNV-1a 64
;------------------------------------------------------------------------------
fnv_str PROC
    mov rax, 0cbf29ce484222325h
    mov r8, 100000001b3h
    xor r9d, r9d
fv_loop:
    movzx edx, byte ptr [rcx + r9]
    cmp edx, '"'
    je fv_done
    test edx, edx
    jz fv_done
    xor rax, rdx
    imul rax, r8
    inc r9
    cmp r9, 160
    jb fv_loop
fv_done:
    ret
fnv_str ENDP

; two ASCII digits at [rcx+off] -> reg32 (clobbers edx)
DIG2 MACRO reg, off
    movzx reg, byte ptr [rcx + off]
    sub reg, '0'
    imul reg, reg, 10
    movzx edx, byte ptr [rcx + off + 1]
    sub edx, '0'
    add reg, edx
ENDM

;------------------------------------------------------------------------------
; parse_ts: rcx = ptr to "YYYY-MM-DDTHH:MM:SS" -> eax unix seconds (UTC)
; days_from_civil (Howard Hinnant), valid for years >= 1970 here.
;------------------------------------------------------------------------------
parse_ts PROC
    DIG2 r8d, 0
    imul r8d, r8d, 100
    DIG2 eax, 2
    add r8d, eax            ; r8d = year
    DIG2 r9d, 5             ; r9d = month
    DIG2 r10d, 8            ; r10d = day
    cmp r9d, 2
    ja pt_m
    dec r8d                 ; Jan/Feb belong to previous year
    add r9d, 9
    jmp pt_m2
pt_m:
    sub r9d, 3
pt_m2:
    ; era = y / 400, yoe = y % 400
    mov eax, r8d
    xor edx, edx
    mov r11d, 400
    div r11d                ; eax = era, edx = yoe
    imul eax, eax, 146097
    mov r11d, eax           ; r11d = era * 146097
    mov r8d, edx            ; r8d = yoe
    ; doy = (153*mp + 2)/5 + d - 1
    imul eax, r9d, 153
    add eax, 2
    xor edx, edx
    push rbx
    mov ebx, 5
    div ebx
    add eax, r10d
    dec eax
    mov r9d, eax            ; r9d = doy
    ; doe = yoe*365 + yoe/4 - yoe/100 + doy
    imul r10d, r8d, 365
    mov eax, r8d
    shr eax, 2
    add r10d, eax
    mov eax, r8d
    xor edx, edx
    mov ebx, 100
    div ebx
    sub r10d, eax
    add r10d, r9d
    pop rbx
    add r10d, r11d
    sub r10d, 719468        ; days since 1970-01-01
    imul r10d, r10d, 86400
    DIG2 eax, 11
    imul eax, eax, 3600
    add r10d, eax
    DIG2 eax, 14
    imul eax, eax, 60
    add r10d, eax
    DIG2 eax, 17
    add eax, r10d
    ret
parse_ts ENDP

;------------------------------------------------------------------------------
; u64 tk_scan_claude(const u8* buf, u64 len, Rec* out, u64 max)
; One record per assistant message usage block.
;------------------------------------------------------------------------------
tk_scan_claude PROC FRAME
    PROLOG8
    mov r12, rcx            ; buf
    mov r13, rdx            ; len
    mov r14, r8             ; out cursor
    mov rsi, r9             ; max
    xor r15, r15            ; count
    xor rbx, rbx            ; pos
c_loop:
    cmp r15, rsi
    jae c_done
    cmp rbx, r13
    jae c_done
    lea r10, [r12 + rbx]
    mov r11, r13
    sub r11, rbx
    FINDR r10, r11, s_usage, S_USAGE
    cmp rax, -1
    je c_done
    lea rdi, [rbx + rax]    ; rdi = marker offset
    ; walk back to line start (not before pos)
    mov rbp, rdi
c_back:
    cmp rbp, rbx
    jbe c_back_done
    cmp byte ptr [r12 + rbp - 1], 10
    je c_back_done
    dec rbp
    jmp c_back
c_back_done:
    ; line end
    lea r10, [r12 + rdi]
    mov r11, r13
    sub r11, rdi
    FINDR r10, r11, s_nl, 1
    cmp rax, -1
    jne c_le
    mov rax, r13
    sub rax, rdi
c_le:
    add rax, rdi
    mov [rsp+L_LE], rax
    lea rbx, [rax + 1]      ; next search starts at next line

    ; must be an assistant line
    lea r10, [r12 + rbp]
    mov r11, [rsp+L_LE]
    sub r11, rbp
    FINDR r10, r11, s_tasst, S_TASST
    cmp rax, -1
    je c_loop
    ; usage must sit inside the top-level message object
    lea r10, [r12 + rbp]
    mov r11, rdi
    sub r11, rbp
    FINDR r10, r11, s_msg, S_MSG
    cmp rax, -1
    je c_loop
    add rax, rbp
    add rax, S_MSG
    mov [rsp+L_MSG], rax
    lea r10, [r12 + rax]
    mov r11, rdi
    sub r11, rax
    FINDR r10, r11, s_role, S_ROLE
    cmp rax, -1
    je c_loop

    ZEROREC

    ; model
    mov rax, [rsp+L_MSG]
    lea r10, [r12 + rax]
    mov r11, rdi
    sub r11, rax
    FINDR r10, r11, s_model, S_MODEL
    cmp rax, -1
    je c_nomodel
    add rax, [rsp+L_MSG]
    add rax, S_MODEL
    mov dword ptr [r14 + R_MOFF], eax
    lea rcx, [r12 + rax]
    xor edx, edx
c_mlen:
    movzx eax, byte ptr [rcx + rdx]
    cmp eax, '"'
    je c_mlen_done
    test eax, eax
    jz c_mlen_done
    cmp edx, 200
    jae c_mlen_done
    inc edx
    jmp c_mlen
c_mlen_done:
    mov word ptr [r14 + R_MLEN], dx
c_nomodel:

    ; message id hash
    mov rax, [rsp+L_MSG]
    lea r10, [r12 + rax]
    mov r11, rdi
    sub r11, rax
    FINDR r10, r11, s_id, S_ID
    cmp rax, -1
    je c_noid
    mov rcx, [rsp+L_MSG]
    add rcx, r12
    add rcx, rax
    add rcx, S_ID
    call fnv_str
    mov qword ptr [r14 + R_ID], rax
c_noid:

    ; input_tokens directly follows the marker
    lea rcx, [r12 + rdi + S_USAGE]
    call parse_num
    mov qword ptr [r14 + R_IN], rax

    ; usage region: up to 1500 bytes after the marker
    lea rax, [rdi + S_USAGE]
    mov [rsp+L_RBASE], rax
    mov rcx, [rsp+L_LE]
    sub rcx, rax
    cmp rcx, 1500
    jbe c_rl
    mov rcx, 1500
c_rl:
    mov [rsp+L_RLEN], rcx

    GETNUM s_ccit, S_CCIT, R_CW
    GETNUM s_crit, S_CRIT, R_CR
    GETNUM s_out, S_OUT, R_OUT
    GETNUM s_e1h, S_E1H, R_C1H

    mov r10, [rsp+L_RBASE]
    add r10, r12
    mov r11, [rsp+L_RLEN]
    FINDR r10, r11, s_fast, S_FAST
    cmp rax, -1
    je c_nofast
    or byte ptr [r14 + R_FLAGS], 1
c_nofast:

    ; timestamp: after usage first, else before it
    lea r10, [r12 + rdi]
    mov r11, [rsp+L_LE]
    sub r11, rdi
    FINDR r10, r11, s_ts, S_TS
    cmp rax, -1
    je c_ts_before
    lea rcx, [r12 + rdi + S_TS]
    add rcx, rax
    jmp c_ts_parse
c_ts_before:
    lea r10, [r12 + rbp]
    mov r11, rdi
    sub r11, rbp
    FINDR r10, r11, s_ts, S_TS
    cmp rax, -1
    je c_ts_none
    lea rcx, [r12 + rbp + S_TS]
    add rcx, rax
c_ts_parse:
    call parse_ts
    mov dword ptr [r14 + R_TS], eax
c_ts_none:

    ; skip empty usage (synthetic messages)
    mov rax, [r14 + R_IN]
    or rax, [r14 + R_OUT]
    or rax, [r14 + R_CR]
    or rax, [r14 + R_CW]
    jz c_loop
    mov byte ptr [r14 + R_KIND], 1
    add r14, REC_SIZE
    inc r15
    jmp c_loop
c_done:
    mov rax, r15
    EPILOG8
    ret
tk_scan_claude ENDP

;------------------------------------------------------------------------------
; u64 tk_scan_codex(const u8* buf, u64 len, Rec* out, u64 max)
; kind 1 per token_count event (last_token_usage), kind 2 per turn_context model.
;------------------------------------------------------------------------------
tk_scan_codex PROC FRAME
    PROLOG8
    mov r12, rcx
    mov r13, rdx
    mov r14, r8
    mov rsi, r9
    xor r15, r15
    xor rbx, rbx
x_loop:
    cmp r15, rsi
    jae x_done
    cmp rbx, r13
    jae x_done
    lea r10, [r12 + rbx]
    mov r11, r13
    sub r11, rbx
    FINDR r10, r11, s_nl, 1
    cmp rax, -1
    jne x_le
    mov rax, r13
    sub rax, rbx
x_le:
    mov rbp, rbx            ; line start
    lea rdi, [rbx + rax]    ; line end
    mov [rsp+L_LE], rdi
    lea rbx, [rdi + 1]
    mov rax, rdi
    sub rax, rbp
    cmp rax, 40
    jb x_loop
    ; header window: first 300 bytes
    mov r11, rax
    cmp r11, 300
    jbe x_hw
    mov r11, 300
x_hw:
    mov [rsp+L_RLEN], r11
    lea r10, [r12 + rbp]
    FINDR r10, r11, s_tc, S_TC
    cmp rax, -1
    jne x_token
    lea r10, [r12 + rbp]
    mov r11, [rsp+L_RLEN]
    FINDR r10, r11, s_turn, S_TURN
    cmp rax, -1
    jne x_turn
    jmp x_loop

x_token:
    ZEROREC
    lea r10, [r12 + rbp]
    mov r11, rdi
    sub r11, rbp
    FINDR r10, r11, s_last, S_LAST
    cmp rax, -1
    je x_loop
    add rax, rbp
    mov [rsp+L_RBASE], rax
    mov rcx, rdi
    sub rcx, rax
    cmp rcx, 500
    jbe x_rl
    mov rcx, 500
x_rl:
    mov [rsp+L_RLEN], rcx
    GETNUM s_in, S_IN, R_IN
    GETNUM s_cin, S_CIN, R_CR
    GETNUM s_out, S_OUT, R_OUT
    GETNUM s_rout, S_ROUT, R_CW
    ; cumulative total, used to drop repeated events
    lea r10, [r12 + rbp]
    mov r11, rdi
    sub r11, rbp
    FINDR r10, r11, s_total, S_TOTAL
    cmp rax, -1
    je x_nototal
    add rax, rbp
    mov [rsp+L_RBASE], rax
    mov rcx, rdi
    sub rcx, rax
    cmp rcx, 500
    jbe x_rl2
    mov rcx, 500
x_rl2:
    mov [rsp+L_RLEN], rcx
    GETNUM s_ttok, S_TTOK, R_C1H
x_nototal:
    mov byte ptr [r14 + R_KIND], 1
    jmp x_stamp

x_turn:
    ZEROREC
    lea r10, [r12 + rbp]
    mov r11, rdi
    sub r11, rbp
    FINDR r10, r11, s_model, S_MODEL
    cmp rax, -1
    je x_loop
    add rax, rbp
    add rax, S_MODEL
    mov dword ptr [r14 + R_MOFF], eax
    lea rcx, [r12 + rax]
    xor edx, edx
x_mlen:
    movzx eax, byte ptr [rcx + rdx]
    cmp eax, '"'
    je x_mlen_done
    test eax, eax
    jz x_mlen_done
    cmp edx, 200
    jae x_mlen_done
    inc edx
    jmp x_mlen
x_mlen_done:
    mov word ptr [r14 + R_MLEN], dx
    mov byte ptr [r14 + R_KIND], 2

x_stamp:
    lea r10, [r12 + rbp]
    mov r11, S_TSHEAD
    FINDR r10, r11, s_tshead, S_TSHEAD
    test rax, rax
    jnz x_emit
    lea rcx, [r12 + rbp + S_TSHEAD]
    call parse_ts
    mov dword ptr [r14 + R_TS], eax
x_emit:
    add r14, REC_SIZE
    inc r15
    jmp x_loop
x_done:
    mov rax, r15
    EPILOG8
    ret
tk_scan_codex ENDP

END
