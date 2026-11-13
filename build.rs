fn main() {
    println!("cargo:rerun-if-changed=src/scan.asm");
    println!("cargo:rerun-if-changed=assets/jtokens.ico");
    cc::Build::new().file("src/scan.asm").compile("jtscan");

    // Exe icon and version info (shown in Explorer > Properties > Details).
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/jtokens.ico")
        .set("ProductName", "JTokens")
        .set("FileDescription", "JTokens")
        .set("CompanyName", "JSystems")
        .set("LegalCopyright", "Copyright (c) 2026 Josephivgi. MIT License.")
        .set("OriginalFilename", "jtokens.exe");
    res.compile().expect("Windows resources (icon, version info)");
}
