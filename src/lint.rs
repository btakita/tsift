use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Clone, Serialize)]
pub struct LintResult {
    pub file: String,
    pub annotations: Vec<Annotation>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Annotation {
    pub line: usize,
    pub column: usize,
    pub text: String,
    pub entity: String,
    pub kind: AnnotationKind,
    pub suggestion: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AnnotationKind {
    Symbol,
    Heading,
    Bold,
}

pub fn lint_markdown(path: &Path, entities: &HashSet<String>) -> Result<LintResult> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;

    let mut annotations = Vec::new();
    let mut in_code_block = false;

    for (line_idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_code_block = !in_code_block;
            continue;
        }
        if in_code_block {
            continue;
        }
        if trimmed.starts_with('#') || trimmed.starts_with("<!--") {
            continue;
        }

        find_unannotated(line, line_idx + 1, entities, &mut annotations);
    }

    Ok(LintResult {
        file: path.display().to_string(),
        annotations,
    })
}

fn find_unannotated(
    line: &str,
    line_num: usize,
    entities: &HashSet<String>,
    annotations: &mut Vec<Annotation>,
) {
    for entity in entities {
        let mut search_from = 0;
        while let Some(pos) = line[search_from..].find(entity.as_str()) {
            let abs_pos = search_from + pos;

            if is_already_annotated(line, abs_pos, entity.len()) {
                search_from = abs_pos + entity.len();
                continue;
            }

            if !is_word_boundary(line, abs_pos, entity.len()) {
                search_from = abs_pos + entity.len();
                continue;
            }

            let kind = guess_annotation_kind(entity);
            let suggestion = match kind {
                AnnotationKind::Symbol => format!("`{}`", entity),
                AnnotationKind::Bold => format!("**{}**", entity),
                AnnotationKind::Heading => format!("[{}](#{})", entity, entity.to_lowercase().replace(' ', "-")),
            };

            annotations.push(Annotation {
                line: line_num,
                column: abs_pos + 1,
                text: entity.clone(),
                entity: entity.clone(),
                kind,
                suggestion,
            });

            search_from = abs_pos + entity.len();
        }
    }
}

fn is_already_annotated(line: &str, pos: usize, len: usize) -> bool {
    let before = if pos > 0 { &line[..pos] } else { "" };
    let after_end = pos + len;
    let after = if after_end < line.len() {
        &line[after_end..]
    } else {
        ""
    };

    // backtick-wrapped
    if before.ends_with('`') && after.starts_with('`') {
        return true;
    }
    // bold-wrapped
    if before.ends_with("**") && after.starts_with("**") {
        return true;
    }
    // link text
    if before.ends_with('[') && after.starts_with("](") {
        return true;
    }
    // inside inline code span
    let backtick_count_before = before.chars().filter(|&c| c == '`').count();
    if backtick_count_before % 2 == 1 {
        return true;
    }

    false
}

fn is_word_boundary(line: &str, pos: usize, len: usize) -> bool {
    let before_ok = pos == 0
        || line.as_bytes().get(pos - 1).is_none_or(|&b| {
            !b.is_ascii_alphanumeric() && b != b'_'
        });
    let after_end = pos + len;
    let after_ok = after_end >= line.len()
        || line.as_bytes().get(after_end).is_none_or(|&b| {
            !b.is_ascii_alphanumeric() && b != b'_'
        });
    before_ok && after_ok
}

fn guess_annotation_kind(entity: &str) -> AnnotationKind {
    if entity.contains('_') || entity.contains("::") || entity.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
        AnnotationKind::Symbol
    } else if entity.chars().next().is_some_and(|c| c.is_uppercase()) && entity.contains(' ') {
        AnnotationKind::Heading
    } else {
        AnnotationKind::Bold
    }
}

pub fn collect_entities_from_db(db_path: &Path) -> Result<HashSet<String>> {
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;

    let mut entities = HashSet::new();

    let mut stmt = conn.prepare("SELECT DISTINCT name FROM symbols WHERE length(name) >= 4")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    for name in rows.flatten() {
        entities.insert(name);
    }

    Ok(entities)
}

pub fn collect_entities_from_markdown(path: &Path) -> Result<HashSet<String>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;

    let mut entities = HashSet::new();

    for line in content.lines() {
        let trimmed = line.trim();
        // headings
        if let Some(heading) = trimmed.strip_prefix('#') {
            let heading = heading.trim_start_matches('#').trim();
            if heading.len() >= 4 {
                entities.insert(heading.to_string());
            }
        }
        // bold terms
        let mut in_bold = false;
        for part in trimmed.split("**") {
            if in_bold && part.len() >= 4 {
                entities.insert(part.to_string());
            }
            in_bold = !in_bold;
        }
        // backtick terms
        let mut in_code = false;
        for part in trimmed.split('`') {
            if in_code && part.len() >= 4 {
                entities.insert(part.to_string());
            }
            in_code = !in_code;
        }
    }

    Ok(entities)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn find_unannotated_plain_text() {
        let entities: HashSet<String> = ["scan_skills".to_string()].into();
        let mut annotations = Vec::new();
        find_unannotated("The scan_skills function works.", 1, &entities, &mut annotations);
        assert_eq!(annotations.len(), 1);
        assert_eq!(annotations[0].text, "scan_skills");
        assert_eq!(annotations[0].column, 5);
        assert_eq!(annotations[0].suggestion, "`scan_skills`");
    }

    #[test]
    fn skip_already_backtick_wrapped() {
        let entities: HashSet<String> = ["scan_skills".to_string()].into();
        let mut annotations = Vec::new();
        find_unannotated("The `scan_skills` function works.", 1, &entities, &mut annotations);
        assert!(annotations.is_empty());
    }

    #[test]
    fn skip_already_bold_wrapped() {
        let entities: HashSet<String> = ["AuditResult".to_string()].into();
        let mut annotations = Vec::new();
        find_unannotated("The **AuditResult** struct.", 1, &entities, &mut annotations);
        assert!(annotations.is_empty());
    }

    #[test]
    fn skip_link_text() {
        let entities: HashSet<String> = ["SPEC".to_string()].into();
        let mut annotations = Vec::new();
        find_unannotated("See [SPEC](spec.md) for details.", 1, &entities, &mut annotations);
        assert!(annotations.is_empty());
    }

    #[test]
    fn word_boundary_prevents_partial_match() {
        let entities: HashSet<String> = ["scan".to_string()].into();
        let mut annotations = Vec::new();
        find_unannotated("The scanning process.", 1, &entities, &mut annotations);
        assert!(annotations.is_empty());
    }

    #[test]
    fn multiple_occurrences_on_same_line() {
        let entities: HashSet<String> = ["test".to_string()].into();
        let mut annotations = Vec::new();
        find_unannotated("Run test then check test output.", 1, &entities, &mut annotations);
        assert_eq!(annotations.len(), 2);
    }

    #[test]
    fn lint_skips_code_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.md");
        fs::write(
            &file,
            "# Doc\n\nscan_skills works.\n\n```\nscan_skills in code block\n```\n",
        )
        .unwrap();
        let entities: HashSet<String> = ["scan_skills".to_string()].into();
        let result = lint_markdown(&file, &entities).unwrap();
        assert_eq!(result.annotations.len(), 1);
        assert_eq!(result.annotations[0].line, 3);
    }

    #[test]
    fn lint_skips_headings_and_comments() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.md");
        fs::write(
            &file,
            "# scan_skills\n\n<!-- scan_skills -->\n\nPlain scan_skills here.\n",
        )
        .unwrap();
        let entities: HashSet<String> = ["scan_skills".to_string()].into();
        let result = lint_markdown(&file, &entities).unwrap();
        assert_eq!(result.annotations.len(), 1);
        assert_eq!(result.annotations[0].line, 5);
    }

    #[test]
    fn collect_entities_from_markdown_extracts_headings_bold_code() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.md");
        fs::write(
            &file,
            "# Architecture\n\nThe **AuditResult** struct uses `scan_skills` internally.\n",
        )
        .unwrap();
        let entities = collect_entities_from_markdown(&file).unwrap();
        assert!(entities.contains("Architecture"));
        assert!(entities.contains("AuditResult"));
        assert!(entities.contains("scan_skills"));
    }

    #[test]
    fn guess_annotation_kind_symbols() {
        assert!(matches!(guess_annotation_kind("scan_skills"), AnnotationKind::Symbol));
        assert!(matches!(guess_annotation_kind("std::path"), AnnotationKind::Symbol));
    }

    #[test]
    fn guess_annotation_kind_headings() {
        assert!(matches!(guess_annotation_kind("Audit Result"), AnnotationKind::Heading));
    }

    #[test]
    fn guess_annotation_kind_bold() {
        assert!(matches!(guess_annotation_kind("AuditResult"), AnnotationKind::Bold));
    }
}
