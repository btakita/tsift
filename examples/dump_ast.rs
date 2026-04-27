// Quick AST dump for Zig, Bash, Markdown
use tree_sitter::Parser;

fn dump(lang_name: &str, ts_lang: tree_sitter::Language, source: &str, _depth: usize) {
    let mut parser = Parser::new();
    parser.set_language(&ts_lang).unwrap();
    let tree = parser.parse(source.as_bytes(), None).unwrap();
    println!("=== {} ===", lang_name);
    println!("Source:\n{}\n", source);
    fn walk(node: tree_sitter::Node, source: &str, indent: usize) {
        let text = &source[node.byte_range()];
        let short = if text.len() > 60 { &text[..60] } else { text };
        let short = short.replace('\n', "\\n");
        println!(
            "{}{} [{}-{}] {:?}",
            "  ".repeat(indent),
            node.kind(),
            node.start_position().row,
            node.end_position().row,
            short
        );
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                walk(cursor.node(), source, indent + 1);
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }
    walk(tree.root_node(), source, 0);
    println!();
}

fn main() {
    // Zig: need struct, enum, union, const, fn
    dump(
        "Zig",
        tree_sitter_zig::LANGUAGE.into(),
        r#"
const std = @import("std");
pub fn main() !void {}
const Point = struct { x: i32, y: i32 };
const Color = enum { red, green, blue };
const Result = union(enum) { ok: i32, err: []const u8 };
"#
        .trim(),
        4,
    );

    // Bash: function + alias
    dump(
        "Bash",
        tree_sitter_bash::LANGUAGE.into(),
        r#"
#!/bin/bash
hello() { echo hi; }
function world { echo world; }
alias ll='ls -la'
alias grep='grep --color=auto'
"#
        .trim(),
        4,
    );

    // Markdown: headings + fenced code blocks
    dump(
        "Markdown",
        tree_sitter_md::LANGUAGE.into(),
        r#"
# Title

## Section

```rust
fn main() {}
```

```python
def hello():
    pass
```

### Subsection
"#
        .trim(),
        4,
    );
}
