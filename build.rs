fn main() {
    println!("cargo:rerun-if-changed=src/scan.asm");
    cc::Build::new().file("src/scan.asm").compile("jtscan");
}
