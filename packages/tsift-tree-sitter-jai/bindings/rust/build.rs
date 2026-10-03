//! Compiles the generated Jai parser and its external scanner.
//!
//! Mirrors upstream's `bindings/rust/build.rs`, except that `scanner.c` is
//! compiled too: the grammar declares externals, and upstream's build script
//! leaves the scanner block commented out.

fn main() {
    let src_dir = std::path::Path::new("src");

    let mut c_config = cc::Build::new();
    c_config.std("c11").include(src_dir).warnings(false);

    #[cfg(target_env = "msvc")]
    c_config.flag("-utf-8");

    for file in ["parser.c", "scanner.c"] {
        let path = src_dir.join(file);
        println!("cargo:rerun-if-changed={}", path.display());
        c_config.file(path);
    }
    println!(
        "cargo:rerun-if-changed={}",
        src_dir.join("tree_sitter").display()
    );

    c_config.compile("tree-sitter-jai");
}
