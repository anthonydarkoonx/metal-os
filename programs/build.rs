fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    // METAL_PIE=1: the position-independent build (see the Makefile's `programs`); each program carries its own name (-soname)
    let pie = std::env::var_os("METAL_PIE").is_some();
    println!("cargo:rustc-link-arg=-T{dir}/{}", if pie { "link-pie.ld" } else { "link.ld" });
    if pie {
        for e in std::fs::read_dir(format!("{dir}/src/bin")).unwrap().flatten() {
            if let Some(n) = e.file_name().to_str().and_then(|n| n.strip_suffix(".rs")) {
                println!("cargo:rustc-link-arg-bin={n}=-soname={n}");
            }
        }
    }
    println!("cargo:rerun-if-changed=link.ld");
    println!("cargo:rerun-if-changed=link-pie.ld");
    println!("cargo:rerun-if-changed=src/bin");
    println!("cargo:rerun-if-env-changed=METAL_PIE");
}
