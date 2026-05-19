fn main() {
    println!("cargo::rerun-if-changed=asm/switch.S");
    println!("cargo::rerun-if-changed=asm/elf_entry.S");
    cc::Build::new()
        .file("asm/switch.S")
        .flag("-std=c11")
        .flag("-O2")
        .compile("vproc_switch");
    cc::Build::new()
        .file("asm/elf_entry.S")
        .flag("-std=c11")
        .flag("-O2")
        .compile("vproc_elf_entry");
}
