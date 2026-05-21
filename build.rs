fn main() {
    println!("cargo::rerun-if-changed=coro/minicoro.c");
    cc::Build::new()
        .file("coro/minicoro.c")
        .define("MINICORO_IMPL", None)
        .define("MCO_USE_VMEM_ALLOCATOR", None)
        .flag("-std=c11")
        .flag("-O2")
        .compile("minicoro");
}
