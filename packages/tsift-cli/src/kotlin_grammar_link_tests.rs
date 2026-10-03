//! Pins the Kotlin grammar the linked `tree_sitter_kotlin` symbol resolves to
//! (#kotlinsymclash).
//!
//! This binary links both `tsift-graph` (indexing) and `tsift-astgrep`
//! (structural patterns). Two grammar crates exporting `tree_sitter_kotlin`
//! in one link let the linker pick either definition, so both consumers could
//! silently parse Kotlin with `tree-sitter-kotlin-sg` while their queries were
//! written for `tree-sitter-kotlin-ng`. Asserting kotlin-ng-only shape from
//! *both* entry points catches a second definition creeping back in.

use tree_sitter::Language;

fn assert_is_kotlin_ng(grammar: &Language, who: &str) {
    assert!(
        grammar.field_id_for_name("name").is_some(),
        "{who}: Kotlin grammar has no `name` field — the linked `tree_sitter_kotlin` \
         is tree-sitter-kotlin-sg, not tree-sitter-kotlin-ng (#kotlinsymclash)"
    );
    assert_ne!(
        grammar.id_for_node_kind("identifier", true),
        0,
        "{who}: Kotlin grammar has no named `identifier` kind"
    );
    assert_eq!(
        grammar.id_for_node_kind("simple_identifier", true),
        0,
        "{who}: Kotlin grammar has `simple_identifier` — that is tree-sitter-kotlin-sg"
    );
}

#[test]
fn indexer_and_structural_engine_link_the_same_kotlin_ng_grammar() {
    let indexer = tsift_graph::Lang::Kotlin.tree_sitter_language();
    let structural = tsift_astgrep::AstGrepLang::Kotlin.tree_sitter_language();
    assert_is_kotlin_ng(&indexer, "tsift-graph");
    assert_is_kotlin_ng(&structural, "tsift-astgrep");
    assert_eq!(indexer.node_kind_count(), structural.node_kind_count());
    assert_eq!(indexer.field_count(), structural.field_count());
}

#[test]
fn kotlin_symbols_index_with_named_declarations() {
    // End-to-end: the indexer's Kotlin tag query uses the `name` field, which
    // fails to compile under kotlin-sg ("Invalid field name `name`").
    let source = "class Greeter {\n    fun greet(who: String) = println(who)\n}\n";
    let symbols = tsift_graph::Lang::Kotlin
        .extract_symbols(source.as_bytes())
        .expect("Kotlin symbols must extract under the linked grammar");
    let names: Vec<_> = symbols.iter().map(|symbol| symbol.name.as_str()).collect();
    assert!(
        names.contains(&"Greeter"),
        "missing class symbol: {names:?}"
    );
    assert!(
        names.contains(&"greet"),
        "missing function symbol: {names:?}"
    );
}

/// Grammar crates that export the same C entry point as one tsift links. Any
/// pair present together in the resolved dependency graph is a link-order race.
const CLASHING_GRAMMAR_CRATES: &[(&str, &str, &str)] = &[
    (
        "tree_sitter_kotlin",
        "tree-sitter-kotlin-ng",
        "tree-sitter-kotlin-sg",
    ),
    (
        "tree_sitter_kotlin",
        "tree-sitter-kotlin-ng",
        "tree-sitter-kotlin",
    ),
];

#[test]
fn lockfile_resolves_one_crate_per_grammar_symbol() {
    // Order-independent guard: the runtime tests above only fail when the
    // linker happens to keep the wrong definition, which depends on module and
    // crate order. The lockfile records every grammar crate the workspace can
    // link under any feature set, so a second definition is visible here
    // regardless of which one the linker would pick.
    let lock_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock");
    let Ok(lock) = std::fs::read_to_string(&lock_path) else {
        // Packaged crate (crates.io tarball) has no workspace lockfile.
        return;
    };
    let lock: toml::Value = toml::from_str(&lock).expect("Cargo.lock parses");
    let packages = lock["package"].as_array().expect("Cargo.lock has packages");
    let mut grammar_versions: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
    for package in packages {
        let name = package["name"].as_str().unwrap_or_default();
        if name.starts_with("tree-sitter-") && name != "tree-sitter-language" {
            grammar_versions
                .entry(name)
                .or_default()
                .push(package["version"].as_str().unwrap_or_default());
        }
    }
    for (name, versions) in &grammar_versions {
        // Two semver-incompatible copies of one grammar crate export the same
        // `tree_sitter_<lang>` symbol just like two differently-named crates.
        assert_eq!(
            versions.len(),
            1,
            "{name} resolves to several versions {versions:?}; each exports the same C symbol"
        );
    }
    for (symbol, kept, clashing) in CLASHING_GRAMMAR_CRATES {
        assert!(
            !(grammar_versions.contains_key(kept) && grammar_versions.contains_key(clashing)),
            "{kept} and {clashing} both export `{symbol}`; exactly one may be linked \
             (#kotlinsymclash) — keep ast-grep-language's Kotlin feature disabled"
        );
    }
}
