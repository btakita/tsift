//! Compiles the vendored Jai tree-sitter grammar when `lang-jai` is enabled.
//!
//! Jai is a closed-beta language with no grammar crate on crates.io, so the
//! generated `parser.c` and the external `scanner.c` from
//! constantitus/tree-sitter-jai (MIT-0, see `vendor/tree-sitter-jai/`) are
//! built here and exposed as `tree_sitter_jai()` to `lang.rs`.

fn main() {
    #[cfg(feature = "lang-jai")]
    build_jai();
}

#[cfg(feature = "lang-jai")]
fn build_jai() {
    let src_dir = std::path::Path::new("vendor/tree-sitter-jai/src");
    let mut build = cc::Build::new();
    build.std("c11").include(src_dir).warnings(false);
    #[cfg(target_env = "msvc")]
    build.flag("-utf-8");
    for file in ["parser.c", "scanner.c"] {
        let path = src_dir.join(file);
        println!("cargo:rerun-if-changed={}", path.display());
        build.file(path);
    }
    println!(
        "cargo:rerun-if-changed={}",
        src_dir.join("tree_sitter").display()
    );
    build.compile("tree-sitter-jai");
}
