#[test]
fn root_reexports_algorithm_tokensave_and_memory_crates() {
    let scc = tsift::algorithms::tarjan_scc(&[]);
    assert_eq!(scc.total_components, 0);

    assert_eq!(
        std::any::type_name::<tsift::tokensave::TokensaveDb>(),
        "tsift_tokensave::TokensaveDb"
    );
    assert_eq!(tsift::memory::MEMORY_CONTRACT_VERSION, "tsift-memory-v1");
}
