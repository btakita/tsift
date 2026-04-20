use crate::lang::{Lang, Symbol};
use anyhow::Result;
use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSite {
    pub callee: String,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallEdge {
    pub caller: String,
    pub callee: String,
    pub caller_line: usize,
    pub call_site_line: usize,
}

impl Lang {
    pub fn call_query(&self) -> Option<&'static str> {
        match self {
            #[cfg(feature = "lang-rust")]
            Self::Rust => Some(r#"
                (call_expression function: (identifier) @call.name)
                (call_expression function: (field_expression field: (field_identifier) @call.name))
                (call_expression function: (scoped_identifier name: (identifier) @call.name))
                (macro_invocation macro: (identifier) @call.name)
            "#),
            #[cfg(feature = "lang-python")]
            Self::Python => Some(r#"
                (call function: (identifier) @call.name)
                (call function: (attribute attribute: (identifier) @call.name))
            "#),
            #[cfg(feature = "lang-typescript")]
            Self::TypeScript | Self::Tsx => Some(r#"
                (call_expression function: (identifier) @call.name)
                (call_expression function: (member_expression property: (property_identifier) @call.name))
            "#),
            #[cfg(feature = "lang-javascript")]
            Self::JavaScript | Self::Jsx => Some(r#"
                (call_expression function: (identifier) @call.name)
                (call_expression function: (member_expression property: (property_identifier) @call.name))
            "#),
            #[cfg(feature = "lang-kotlin")]
            Self::Kotlin => Some(r#"
                (call_expression (simple_identifier) @call.name)
            "#),
            _ => None,
        }
    }
}

pub fn extract_call_sites(lang: Lang, source: &[u8]) -> Result<Vec<CallSite>> {
    let query_str = match lang.call_query() {
        Some(q) => q,
        None => return Ok(Vec::new()),
    };
    let mut parser = Parser::new();
    let ts_lang = lang.tree_sitter_language();
    parser.set_language(&ts_lang)?;
    let tree = parser.parse(source, None)
        .ok_or_else(|| anyhow::anyhow!("parse failed"))?;
    let query = Query::new(&ts_lang, query_str)?;
    let mut cursor = QueryCursor::new();
    let mut sites = Vec::new();
    let capture_names: Vec<String> = query.capture_names().iter().map(|s| s.to_string()).collect();

    let mut matches = cursor.matches(&query, tree.root_node(), source);
    while let Some(m) = matches.next() {
        for capture in m.captures {
            let name = &capture_names[capture.index as usize];
            if name == "call.name" {
                let callee = capture.node.utf8_text(source)
                    .unwrap_or("<invalid utf8>")
                    .to_string();
                sites.push(CallSite {
                    callee,
                    line: capture.node.start_position().row,
                });
            }
        }
    }
    Ok(sites)
}

pub fn resolve_edges(symbols: &[Symbol], call_sites: &[CallSite]) -> Vec<CallEdge> {
    let mut edges = Vec::new();
    for site in call_sites {
        let caller = symbols.iter()
            .filter(|s| s.kind == "function" || s.kind == "class" || s.kind == "mod")
            .filter(|s| site.line >= s.line && site.line <= s.end_line)
            .min_by_key(|s| s.end_line - s.line);
        if let Some(caller) = caller {
            edges.push(CallEdge {
                caller: caller.name.clone(),
                callee: site.callee.clone(),
                caller_line: caller.line,
                call_site_line: site.line,
            });
        }
    }
    edges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "lang-rust")]
    #[test]
    fn rust_direct_call() {
        let source = b"fn helper() {}\nfn main() { helper(); }";
        let sites = extract_call_sites(Lang::Rust, source).unwrap();
        assert!(sites.iter().any(|s| s.callee == "helper"), "got: {:?}", sites);
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn rust_method_call() {
        let source = b"fn main() { vec.push(1); }";
        let sites = extract_call_sites(Lang::Rust, source).unwrap();
        assert!(sites.iter().any(|s| s.callee == "push"), "got: {:?}", sites);
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn rust_scoped_call() {
        let source = b"fn main() { Vec::new(); }";
        let sites = extract_call_sites(Lang::Rust, source).unwrap();
        assert!(sites.iter().any(|s| s.callee == "new"), "got: {:?}", sites);
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn rust_macro_call() {
        let source = b"fn main() { println!(\"hi\"); }";
        let sites = extract_call_sites(Lang::Rust, source).unwrap();
        assert!(sites.iter().any(|s| s.callee == "println"), "got: {:?}", sites);
    }

    #[cfg(feature = "lang-python")]
    #[test]
    fn python_direct_call() {
        let source = b"def helper(): pass\ndef main(): helper()";
        let sites = extract_call_sites(Lang::Python, source).unwrap();
        assert!(sites.iter().any(|s| s.callee == "helper"), "got: {:?}", sites);
    }

    #[cfg(feature = "lang-python")]
    #[test]
    fn python_method_call() {
        let source = b"def main(): obj.method()";
        let sites = extract_call_sites(Lang::Python, source).unwrap();
        assert!(sites.iter().any(|s| s.callee == "method"), "got: {:?}", sites);
    }

    #[cfg(feature = "lang-typescript")]
    #[test]
    fn typescript_direct_call() {
        let source = b"function helper() {}\nfunction main() { helper(); }";
        let sites = extract_call_sites(Lang::TypeScript, source).unwrap();
        assert!(sites.iter().any(|s| s.callee == "helper"), "got: {:?}", sites);
    }

    #[cfg(feature = "lang-typescript")]
    #[test]
    fn typescript_method_call() {
        let source = b"function main() { arr.push(1); }";
        let sites = extract_call_sites(Lang::TypeScript, source).unwrap();
        assert!(sites.iter().any(|s| s.callee == "push"), "got: {:?}", sites);
    }

    #[cfg(feature = "lang-javascript")]
    #[test]
    fn javascript_call() {
        let source = b"function main() { helper(); obj.method(); }";
        let sites = extract_call_sites(Lang::JavaScript, source).unwrap();
        assert!(sites.iter().any(|s| s.callee == "helper"), "got: {:?}", sites);
        assert!(sites.iter().any(|s| s.callee == "method"), "got: {:?}", sites);
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn resolve_edges_basic() {
        let symbols = vec![
            Symbol { name: "main".into(), kind: "function".into(), line: 1, end_line: 3 },
            Symbol { name: "helper".into(), kind: "function".into(), line: 5, end_line: 7 },
        ];
        let sites = vec![
            CallSite { callee: "helper".into(), line: 2 },
            CallSite { callee: "println".into(), line: 6 },
        ];
        let edges = resolve_edges(&symbols, &sites);
        assert_eq!(edges.len(), 2);
        assert_eq!(edges[0].caller, "main");
        assert_eq!(edges[0].callee, "helper");
        assert_eq!(edges[1].caller, "helper");
        assert_eq!(edges[1].callee, "println");
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn resolve_edges_nested_picks_innermost() {
        let symbols = vec![
            Symbol { name: "outer".into(), kind: "function".into(), line: 0, end_line: 10 },
            Symbol { name: "inner".into(), kind: "function".into(), line: 2, end_line: 5 },
        ];
        let sites = vec![CallSite { callee: "foo".into(), line: 3 }];
        let edges = resolve_edges(&symbols, &sites);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].caller, "inner");
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn resolve_edges_top_level_call_excluded() {
        let symbols = vec![
            Symbol { name: "main".into(), kind: "function".into(), line: 5, end_line: 10 },
        ];
        let sites = vec![CallSite { callee: "foo".into(), line: 2 }];
        let edges = resolve_edges(&symbols, &sites);
        assert!(edges.is_empty());
    }

    #[test]
    fn no_call_query_returns_empty() {
        #[cfg(feature = "lang-markdown")]
        {
            let sites = extract_call_sites(Lang::Markdown, b"# Hello").unwrap();
            assert!(sites.is_empty());
        }
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn full_roundtrip_rust() {
        let source = b"fn helper() { println!(\"hi\"); }\nfn main() { helper(); Vec::new(); }";
        let symbols = Lang::Rust.extract_symbols(source).unwrap();
        let sites = extract_call_sites(Lang::Rust, source).unwrap();
        let edges = resolve_edges(&symbols, &sites);
        let main_calls: Vec<&str> = edges.iter()
            .filter(|e| e.caller == "main")
            .map(|e| e.callee.as_str())
            .collect();
        assert!(main_calls.contains(&"helper"), "main should call helper, got: {:?}", main_calls);
        assert!(main_calls.contains(&"new"), "main should call new, got: {:?}", main_calls);
    }
}
