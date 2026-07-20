#[test]
fn root_reexports_algorithm_tokensave_and_memory_crates() {
    let scc = tsift::algorithms::tarjan_scc(&[]);
    assert_eq!(scc.total_components, 0);

    assert_eq!(
        std::any::type_name::<tsift::tokensave::TokensaveDb>(),
        "tsift_tokensave::TokensaveDb"
    );
    assert_eq!(tsift::memory::MEMORY_CONTRACT_VERSION, "tsift-memory-v1");
    assert_eq!(
        tsift::memgraphrag::MEMGRAPHRAG_CONTRACT_VERSION,
        "tsift-memgraphrag-v1"
    );
}

#[test]
fn root_reexports_structural_astgrep_engine() {
    // `tsift ast-grep` is the CLI face of this crate; the library face has to
    // stay reachable for embedders that skip the binary.
    let langs = tsift::astgrep::AstGrepLang::all();
    assert!(!langs.is_empty(), "default build compiles no grammars");
    assert!(langs.iter().any(|l| l.name() == "rust"));

    let hits = tsift::astgrep::search_source(
        "fn a() { foo(1); }",
        tsift::astgrep::AstGrepLang::Rust,
        "foo($A)",
    )
    .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].captures.get("A").map(String::as_str), Some("1"));
}
