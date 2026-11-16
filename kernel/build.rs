fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-arg=-T{dir}/linker.ld");
    // position-independent; -znotext: the precompiled libcore has absolute pointers in read-only data, which the relocations fix
    for a in ["-pie", "--no-dynamic-linker", "-znorelro", "-znotext"] {
        println!("cargo:rustc-link-arg={a}");
    }
    println!("cargo:rerun-if-changed=linker.ld");
    println!("cargo:rerun-if-changed=src/boot.S");
}
