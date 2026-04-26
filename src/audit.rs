use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
pub struct SkillEntry {
    pub name: String,
    pub path: PathBuf,
    pub has_skill_md: bool,
    pub is_symlink: bool,
    pub description: Option<String>,
    pub issues: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditResult {
    pub skills_dir: PathBuf,
    pub total: usize,
    pub healthy: usize,
    pub broken: usize,
    pub skills: Vec<SkillEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_diffs: Option<Vec<ManifestDiff>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub similar_pairs: Vec<SimilarPair>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SimilarPair {
    pub skill_a: String,
    pub skill_b: String,
    /// Jaccard similarity of description word sets (0.0–1.0)
    pub score: f32,
    pub desc_a: String,
    pub desc_b: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ManifestDiff {
    pub name: String,
    pub kind: DiffKind,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffKind {
    Missing,
    Orphan,
}

pub fn scan_skills(skills_dir: &Path) -> Result<AuditResult> {
    if !skills_dir.exists() {
        return Ok(AuditResult {
            skills_dir: skills_dir.to_path_buf(),
            total: 0,
            healthy: 0,
            broken: 0,
            skills: Vec::new(),
            manifest_diffs: None,
            similar_pairs: Vec::new(),
        });
    }

    let mut skills = Vec::new();

    let entries = std::fs::read_dir(skills_dir)
        .with_context(|| format!("reading skills directory: {}", skills_dir.display()))?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        let is_symlink = entry.file_type()?.is_symlink();
        let resolved = if is_symlink {
            std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone())
        } else {
            path.clone()
        };

        if !resolved.is_dir() {
            continue;
        }

        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        let skill_md = resolved.join("SKILL.md");
        let has_skill_md = skill_md.exists();
        let mut issues = Vec::new();
        let mut description = None;

        if is_symlink && !resolved.exists() {
            issues.push("broken symlink".to_string());
        } else if !has_skill_md {
            issues.push("SKILL.md missing".to_string());
        } else {
            match std::fs::read_to_string(&skill_md) {
                Ok(content) => {
                    if content.trim().is_empty() {
                        issues.push("SKILL.md is empty".to_string());
                    } else {
                        description = extract_frontmatter_field(&content, "description");
                        if description.is_none() {
                            issues.push("no description in SKILL.md frontmatter".to_string());
                        }
                    }
                }
                Err(e) => {
                    issues.push(format!("SKILL.md unreadable: {}", e));
                }
            }
        }

        skills.push(SkillEntry {
            name,
            path,
            has_skill_md,
            is_symlink,
            description,
            issues,
        });
    }

    skills.sort_by(|a, b| a.name.cmp(&b.name));

    let total = skills.len();
    let broken = skills.iter().filter(|s| !s.issues.is_empty()).count();

    let similar_pairs = find_similar_pairs(&skills, 0.3);

    Ok(AuditResult {
        skills_dir: skills_dir.to_path_buf(),
        total,
        healthy: total - broken,
        broken,
        skills,
        manifest_diffs: None,
        similar_pairs,
    })
}

/// Compute Jaccard similarity between two description word sets.
/// Returns pairs with score >= threshold, sorted descending by score.
pub fn find_similar_pairs(skills: &[SkillEntry], threshold: f32) -> Vec<SimilarPair> {
    let mut pairs = Vec::new();
    for i in 0..skills.len() {
        let a = &skills[i];
        let Some(desc_a) = &a.description else { continue };
        let tokens_a = description_tokens(desc_a);
        if tokens_a.is_empty() {
            continue;
        }
        for j in (i + 1)..skills.len() {
            let b = &skills[j];
            let Some(desc_b) = &b.description else { continue };
            let tokens_b = description_tokens(desc_b);
            if tokens_b.is_empty() {
                continue;
            }
            let intersection = tokens_a.intersection(&tokens_b).count();
            let union = tokens_a.union(&tokens_b).count();
            if union == 0 {
                continue;
            }
            let score = intersection as f32 / union as f32;
            if score >= threshold {
                pairs.push(SimilarPair {
                    skill_a: a.name.clone(),
                    skill_b: b.name.clone(),
                    score,
                    desc_a: desc_a.clone(),
                    desc_b: desc_b.clone(),
                });
            }
        }
    }
    pairs.sort_by(|x, y| y.score.partial_cmp(&x.score).unwrap_or(std::cmp::Ordering::Equal));
    pairs
}

static STOP_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "do", "for", "from", "has", "have", "in",
    "is", "it", "its", "not", "of", "on", "or", "the", "to", "use", "used", "via", "with",
];

fn description_tokens(desc: &str) -> HashSet<String> {
    let stop: HashSet<&str> = STOP_WORDS.iter().copied().collect();
    desc.split(|c: char| !c.is_alphanumeric())
        .map(|w| w.to_lowercase())
        .filter(|w| w.len() >= 2 && !stop.contains(w.as_str()))
        .collect()
}

pub fn compare_manifest(
    audit: &mut AuditResult,
    manifest_path: &Path,
) -> Result<()> {
    let content = std::fs::read_to_string(manifest_path)
        .with_context(|| format!("reading manifest: {}", manifest_path.display()))?;

    let manifest_names: Vec<String> = content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.to_string())
        .collect();

    let installed: HashSet<&str> = audit.skills.iter().map(|s| s.name.as_str()).collect();
    let expected: HashSet<&str> = manifest_names.iter().map(|s| s.as_str()).collect();

    let mut diffs = Vec::new();

    for name in &manifest_names {
        if !installed.contains(name.as_str()) {
            diffs.push(ManifestDiff {
                name: name.clone(),
                kind: DiffKind::Missing,
            });
        }
    }

    for skill in &audit.skills {
        if !expected.contains(skill.name.as_str()) {
            diffs.push(ManifestDiff {
                name: skill.name.clone(),
                kind: DiffKind::Orphan,
            });
        }
    }

    audit.manifest_diffs = Some(diffs);
    Ok(())
}

fn extract_frontmatter_field(content: &str, field: &str) -> Option<String> {
    let mut in_frontmatter = false;
    let prefix = format!("{}:", field);

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "---" {
            if in_frontmatter {
                return None;
            }
            in_frontmatter = true;
            continue;
        }
        if !in_frontmatter {
            continue;
        }
        if trimmed.starts_with(&prefix) {
            let value = trimmed[prefix.len()..].trim();
            let unquoted = value
                .trim_start_matches('"')
                .trim_end_matches('"')
                .trim_start_matches('\'')
                .trim_end_matches('\'');
            if !unquoted.is_empty() {
                return Some(unquoted.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn scan_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let result = scan_skills(dir.path()).unwrap();
        assert_eq!(result.total, 0);
        assert_eq!(result.healthy, 0);
        assert_eq!(result.broken, 0);
    }

    #[test]
    fn scan_nonexistent_dir() {
        let result = scan_skills(Path::new("/nonexistent/skills")).unwrap();
        assert_eq!(result.total, 0);
    }

    #[test]
    fn scan_healthy_skill() {
        let dir = tempfile::tempdir().unwrap();
        let skill = dir.path().join("my-skill");
        fs::create_dir(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\ndescription: A test skill\n---\n# my-skill\nDoes things.\n",
        )
        .unwrap();

        let result = scan_skills(dir.path()).unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.healthy, 1);
        assert_eq!(result.broken, 0);
        assert_eq!(result.skills[0].name, "my-skill");
        assert_eq!(
            result.skills[0].description.as_deref(),
            Some("A test skill")
        );
        assert!(result.skills[0].issues.is_empty());
    }

    #[test]
    fn scan_missing_skill_md() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("broken")).unwrap();

        let result = scan_skills(dir.path()).unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.broken, 1);
        assert!(result.skills[0].issues.contains(&"SKILL.md missing".to_string()));
    }

    #[test]
    fn scan_empty_skill_md() {
        let dir = tempfile::tempdir().unwrap();
        let skill = dir.path().join("empty");
        fs::create_dir(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "").unwrap();

        let result = scan_skills(dir.path()).unwrap();
        assert_eq!(result.broken, 1);
        assert!(result.skills[0]
            .issues
            .contains(&"SKILL.md is empty".to_string()));
    }

    #[test]
    fn scan_no_description_in_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let skill = dir.path().join("no-desc");
        fs::create_dir(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "---\nname: no-desc\n---\n# no-desc\n").unwrap();

        let result = scan_skills(dir.path()).unwrap();
        assert_eq!(result.broken, 1);
        assert!(result.skills[0]
            .issues
            .iter()
            .any(|i| i.contains("no description")));
    }

    #[test]
    fn scan_multiple_skills_sorted() {
        let dir = tempfile::tempdir().unwrap();
        for name in &["zebra", "alpha", "mid"] {
            let skill = dir.path().join(name);
            fs::create_dir(&skill).unwrap();
            fs::write(
                skill.join("SKILL.md"),
                format!("---\ndescription: {name} skill\n---\n"),
            )
            .unwrap();
        }

        let result = scan_skills(dir.path()).unwrap();
        assert_eq!(result.total, 3);
        assert_eq!(result.skills[0].name, "alpha");
        assert_eq!(result.skills[1].name, "mid");
        assert_eq!(result.skills[2].name, "zebra");
    }

    #[test]
    fn manifest_missing_and_orphan() {
        let dir = tempfile::tempdir().unwrap();
        let skill = dir.path().join("installed");
        fs::create_dir(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\ndescription: installed\n---\n",
        )
        .unwrap();

        let manifest = dir.path().join("manifest.txt");
        fs::write(&manifest, "installed\nexpected-but-missing\n").unwrap();

        let mut result = scan_skills(dir.path()).unwrap();
        compare_manifest(&mut result, &manifest).unwrap();

        let diffs = result.manifest_diffs.unwrap();
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].name, "expected-but-missing");
        assert!(matches!(diffs[0].kind, DiffKind::Missing));
    }

    #[test]
    fn manifest_orphan_detected() {
        let dir = tempfile::tempdir().unwrap();
        let skill = dir.path().join("orphan");
        fs::create_dir(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\ndescription: orphan skill\n---\n",
        )
        .unwrap();

        let manifest = dir.path().join("manifest.txt");
        fs::write(&manifest, "# expected skills\nother-skill\n").unwrap();

        let mut result = scan_skills(dir.path()).unwrap();
        compare_manifest(&mut result, &manifest).unwrap();

        let diffs = result.manifest_diffs.unwrap();
        let orphans: Vec<_> = diffs
            .iter()
            .filter(|d| matches!(d.kind, DiffKind::Orphan))
            .collect();
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].name, "orphan");
    }

    #[test]
    fn extract_quoted_description() {
        let content = "---\ndescription: \"A quoted description\"\n---\n";
        assert_eq!(
            extract_frontmatter_field(content, "description"),
            Some("A quoted description".to_string())
        );
    }

    #[test]
    fn extract_unquoted_description() {
        let content = "---\ndescription: Simple description\n---\n";
        assert_eq!(
            extract_frontmatter_field(content, "description"),
            Some("Simple description".to_string())
        );
    }

    #[test]
    fn extract_missing_field() {
        let content = "---\nname: test\n---\n";
        assert_eq!(extract_frontmatter_field(content, "description"), None);
    }

    #[test]
    fn files_in_skills_dir_ignored() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("not-a-skill.txt"), "just a file").unwrap();

        let result = scan_skills(dir.path()).unwrap();
        assert_eq!(result.total, 0);
    }

    // --- similar_pairs / duplicate detection ---

    fn make_skill(name: &str, description: &str) -> SkillEntry {
        SkillEntry {
            name: name.to_string(),
            path: PathBuf::from(name),
            has_skill_md: true,
            is_symlink: false,
            description: Some(description.to_string()),
            issues: Vec::new(),
        }
    }

    #[test]
    fn similar_pairs_identical_descriptions() {
        let skills = vec![
            make_skill("skill-a", "search code and files"),
            make_skill("skill-b", "search code and files"),
        ];
        let pairs = find_similar_pairs(&skills, 0.3);
        assert_eq!(pairs.len(), 1);
        assert!((pairs[0].score - 1.0).abs() < 0.01);
        assert_eq!(pairs[0].skill_a, "skill-a");
        assert_eq!(pairs[0].skill_b, "skill-b");
    }

    #[test]
    fn similar_pairs_high_overlap() {
        let skills = vec![
            make_skill("search-tool", "fast semantic search over code symbols"),
            make_skill("code-search", "semantic search over code and symbols"),
        ];
        let pairs = find_similar_pairs(&skills, 0.3);
        assert_eq!(pairs.len(), 1);
        assert!(pairs[0].score >= 0.3);
    }

    #[test]
    fn similar_pairs_no_overlap() {
        let skills = vec![
            make_skill("graph-tool", "visualize dependency graph"),
            make_skill("email-tool", "draft and send emails"),
        ];
        let pairs = find_similar_pairs(&skills, 0.3);
        assert_eq!(pairs.len(), 0);
    }

    #[test]
    fn similar_pairs_below_threshold() {
        let skills = vec![
            make_skill("skill-a", "search files"),
            make_skill("skill-b", "analyze graph structure"),
        ];
        let pairs = find_similar_pairs(&skills, 0.3);
        assert_eq!(pairs.len(), 0);
    }

    #[test]
    fn similar_pairs_sorted_descending() {
        let skills = vec![
            make_skill("a", "search code symbols files index"),
            make_skill("b", "search code symbols index"),
            make_skill("c", "search code symbols files index queries"),
        ];
        let pairs = find_similar_pairs(&skills, 0.3);
        // All should have scores, sorted descending
        for i in 1..pairs.len() {
            assert!(pairs[i - 1].score >= pairs[i].score);
        }
    }

    #[test]
    fn similar_pairs_skips_no_description() {
        let mut no_desc = make_skill("no-desc", "");
        no_desc.description = None;
        let skills = vec![
            no_desc,
            make_skill("skill-a", "search code files"),
            make_skill("skill-b", "search code files"),
        ];
        let pairs = find_similar_pairs(&skills, 0.3);
        // no-desc is skipped; only a/b pair counted
        assert_eq!(pairs.len(), 1);
    }

    #[test]
    fn similar_pairs_stop_words_excluded() {
        // "the a and for to" are all stop words — empty token sets → no pair
        let skills = vec![
            make_skill("skill-a", "the a and for to"),
            make_skill("skill-b", "the a and for to in"),
        ];
        let pairs = find_similar_pairs(&skills, 0.3);
        assert_eq!(pairs.len(), 0);
    }

    #[test]
    fn scan_detects_similar_skills() {
        let dir = tempfile::tempdir().unwrap();
        for (name, desc) in &[
            ("search-a", "search code symbols"),
            ("search-b", "search code symbols"),
            ("email", "draft and send emails"),
        ] {
            let skill = dir.path().join(name);
            fs::create_dir(&skill).unwrap();
            fs::write(
                skill.join("SKILL.md"),
                format!("---\ndescription: {desc}\n---\n"),
            )
            .unwrap();
        }
        let result = scan_skills(dir.path()).unwrap();
        assert_eq!(result.similar_pairs.len(), 1);
        assert_eq!(result.similar_pairs[0].skill_a, "search-a");
        assert_eq!(result.similar_pairs[0].skill_b, "search-b");
    }
}
