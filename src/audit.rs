use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
pub struct SkillEntry {
    pub name: String,
    pub path: PathBuf,
    pub has_skill_md: bool,
    pub is_symlink: bool,
    pub description: Option<String>,
    pub issues: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invocation_count: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillUsage {
    pub skill: String,
    pub count: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct CleanupEntry {
    pub skill: String,
    pub reasons: Vec<String>,
    pub token_estimate: usize,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Vec<SkillUsage>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cleanup: Option<Vec<CleanupEntry>>,
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
            usage: None,
            cleanup: None,
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
            invocation_count: None,
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
        usage: None,
        cleanup: None,
    })
}

/// Compute Jaccard similarity between two description word sets.
/// Returns pairs with score >= threshold, sorted descending by score.
pub fn find_similar_pairs(skills: &[SkillEntry], threshold: f32) -> Vec<SimilarPair> {
    let mut pairs = Vec::new();
    for i in 0..skills.len() {
        let a = &skills[i];
        let Some(desc_a) = &a.description else {
            continue;
        };
        let tokens_a = description_tokens(desc_a);
        if tokens_a.is_empty() {
            continue;
        }
        for b in skills.iter().skip(i + 1) {
            let Some(desc_b) = &b.description else {
                continue;
            };
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
    pairs.sort_by(|x, y| {
        y.score
            .partial_cmp(&x.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
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

pub fn compare_manifest(audit: &mut AuditResult, manifest_path: &Path) -> Result<()> {
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

pub fn track_usage(audit: &mut AuditResult) -> Result<()> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let projects_dir = PathBuf::from(&home).join(".claude/projects");
    if !projects_dir.exists() {
        audit.usage = Some(Vec::new());
        return Ok(());
    }

    let mut counts: HashMap<String, u32> = HashMap::new();

    for project_entry in std::fs::read_dir(&projects_dir)? {
        let project_entry = project_entry?;
        let project_path = project_entry.path();
        if !project_path.is_dir() {
            continue;
        }
        scan_session_dir(&project_path, &mut counts)?;
    }

    let installed: HashSet<String> = audit.skills.iter().map(|s| s.name.clone()).collect();

    for skill in &mut audit.skills {
        let count = counts.get(skill.name.as_str()).copied().unwrap_or(0);
        skill.invocation_count = Some(count);
    }

    let mut usage_list: Vec<SkillUsage> = audit
        .skills
        .iter()
        .map(|s| SkillUsage {
            skill: s.name.clone(),
            count: s.invocation_count.unwrap_or(0),
        })
        .collect();

    for (name, count) in &counts {
        if !installed.contains(name.as_str()) {
            usage_list.push(SkillUsage {
                skill: name.clone(),
                count: *count,
            });
        }
    }

    usage_list.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.skill.cmp(&b.skill)));
    audit.usage = Some(usage_list);
    Ok(())
}

fn scan_session_dir(dir: &Path, counts: &mut HashMap<String, u32>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "jsonl") {
            scan_jsonl(&path, counts).ok();
        } else if path.is_dir() {
            scan_session_dir(&path, counts).ok();
        }
    }
    Ok(())
}

fn scan_jsonl(path: &Path, counts: &mut HashMap<String, u32>) -> Result<()> {
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    for line in reader.lines() {
        let line = line?;
        if !line.contains("\"Skill\"") {
            continue;
        }
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) {
            extract_skill_names(&val, counts);
        }
    }
    Ok(())
}

fn extract_skill_names(val: &serde_json::Value, counts: &mut HashMap<String, u32>) {
    if let Some(content) = val
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
    {
        for block in content {
            if block.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                && block.get("name").and_then(|n| n.as_str()) == Some("Skill")
                && let Some(skill_name) = block
                    .get("input")
                    .and_then(|i| i.get("skill"))
                    .and_then(|s| s.as_str())
            {
                let base_name = skill_name.split(':').next().unwrap_or(skill_name);
                *counts.entry(base_name.to_string()).or_insert(0) += 1;
            }
        }
    }
}

pub fn generate_cleanup(audit: &mut AuditResult) {
    let mut entries = Vec::new();

    for skill in &audit.skills {
        let mut reasons = Vec::new();

        if !skill.issues.is_empty() {
            reasons.push(format!("health: {}", skill.issues.join(", ")));
        }

        if skill.invocation_count == Some(0) {
            reasons.push("never used in any session".to_string());
        }

        let is_duplicate = audit
            .similar_pairs
            .iter()
            .any(|p| (p.skill_a == skill.name || p.skill_b == skill.name) && p.score >= 0.5);
        if is_duplicate {
            reasons.push("high similarity with another skill (≥50%)".to_string());
        }

        if !reasons.is_empty() {
            let token_estimate = estimate_skill_tokens(&skill.path);
            entries.push(CleanupEntry {
                skill: skill.name.clone(),
                reasons,
                token_estimate,
            });
        }
    }

    entries.sort_by(|a, b| b.token_estimate.cmp(&a.token_estimate));
    audit.cleanup = Some(entries);
}

fn estimate_skill_tokens(skill_dir: &Path) -> usize {
    let mut total_bytes: u64 = 0;
    if let Ok(entries) = walkdir(skill_dir) {
        for path in entries {
            if let Ok(meta) = std::fs::metadata(&path)
                && meta.is_file()
            {
                total_bytes += meta.len();
            }
        }
    }
    (total_bytes as usize) / 4
}

fn walkdir(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    if !dir.is_dir() {
        return Ok(files);
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            files.extend(walkdir(&path)?);
        } else {
            files.push(path);
        }
    }
    Ok(files)
}

pub fn write_report(audit: &AuditResult, path: &Path) -> Result<()> {
    let mut report = String::new();
    report.push_str("# Skill Audit Report\n\n");
    report.push_str(&format!("**Generated:** {}\n\n", chrono_now()));
    report.push_str(&format!(
        "**Skills directory:** `{}`\n\n",
        audit.skills_dir.display()
    ));
    report.push_str(&format!(
        "| Metric | Count |\n|--------|-------|\n| Total | {} |\n| Healthy | {} |\n| Broken | {} |\n\n",
        audit.total, audit.healthy, audit.broken
    ));

    report.push_str("## Skills\n\n");
    report.push_str(
        "| Status | Name | Description | Uses |\n|--------|------|-------------|------|\n",
    );
    for skill in &audit.skills {
        let status = if skill.issues.is_empty() {
            "ok"
        } else {
            "broken"
        };
        let desc = skill.description.as_deref().unwrap_or("-");
        let uses = skill
            .invocation_count
            .map(|c| c.to_string())
            .unwrap_or_else(|| "-".to_string());
        report.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            status, skill.name, desc, uses
        ));
    }

    if !audit.similar_pairs.is_empty() {
        report.push_str("\n## Possible Duplicates\n\n");
        report.push_str("| Score | Skill A | Skill B |\n|-------|---------|--------|\n");
        for pair in &audit.similar_pairs {
            report.push_str(&format!(
                "| {:.0}% | {} | {} |\n",
                pair.score * 100.0,
                pair.skill_a,
                pair.skill_b
            ));
        }
    }

    if let Some(diffs) = &audit.manifest_diffs
        && !diffs.is_empty()
    {
        report.push_str("\n## Manifest Diffs\n\n");
        report.push_str("| Name | Status |\n|------|--------|\n");
        for diff in diffs {
            let label = match diff.kind {
                DiffKind::Missing => "missing",
                DiffKind::Orphan => "orphan",
            };
            report.push_str(&format!("| {} | {} |\n", diff.name, label));
        }
    }

    if let Some(cleanup) = &audit.cleanup
        && !cleanup.is_empty()
    {
        report.push_str("\n## Cleanup Recommendations\n\n");
        report
            .push_str("| Skill | Token Savings | Reasons |\n|-------|---------------|--------|\n");
        for entry in cleanup {
            report.push_str(&format!(
                "| {} | ~{} | {} |\n",
                entry.skill,
                format_tokens(entry.token_estimate),
                entry.reasons.join("; ")
            ));
        }
        let total: usize = cleanup.iter().map(|e| e.token_estimate).sum();
        report.push_str(&format!(
            "\n**Total potential savings:** ~{}\n",
            format_tokens(total)
        ));
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, &report)
        .with_context(|| format!("writing report to {}", path.display()))?;
    Ok(())
}

fn chrono_now() -> String {
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs();
    let days = secs / 86400;
    let year = 1970 + (days * 400 / 146097);
    format!("{}-xx-xx (epoch {})", year, secs)
}

fn format_tokens(tokens: usize) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M tokens", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}K tokens", tokens as f64 / 1_000.0)
    } else {
        format!("{} tokens", tokens)
    }
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
        assert!(
            result.skills[0]
                .issues
                .contains(&"SKILL.md missing".to_string())
        );
    }

    #[test]
    fn scan_empty_skill_md() {
        let dir = tempfile::tempdir().unwrap();
        let skill = dir.path().join("empty");
        fs::create_dir(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "").unwrap();

        let result = scan_skills(dir.path()).unwrap();
        assert_eq!(result.broken, 1);
        assert!(
            result.skills[0]
                .issues
                .contains(&"SKILL.md is empty".to_string())
        );
    }

    #[test]
    fn scan_no_description_in_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let skill = dir.path().join("no-desc");
        fs::create_dir(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: no-desc\n---\n# no-desc\n",
        )
        .unwrap();

        let result = scan_skills(dir.path()).unwrap();
        assert_eq!(result.broken, 1);
        assert!(
            result.skills[0]
                .issues
                .iter()
                .any(|i| i.contains("no description"))
        );
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
        fs::write(skill.join("SKILL.md"), "---\ndescription: installed\n---\n").unwrap();

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
            invocation_count: None,
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

    // --- usage tracking ---

    #[test]
    fn extract_skill_names_from_tool_use() {
        let json_line = r#"{"message":{"content":[{"type":"tool_use","name":"Skill","input":{"skill":"agent-doc","args":"plan.md"}}]}}"#;
        let val: serde_json::Value = serde_json::from_str(json_line).unwrap();
        let mut counts = HashMap::new();
        extract_skill_names(&val, &mut counts);
        assert_eq!(counts.get("agent-doc"), Some(&1));
    }

    #[test]
    fn extract_skill_names_strips_plugin_prefix() {
        let json_line = r#"{"message":{"content":[{"type":"tool_use","name":"Skill","input":{"skill":"codex:rescue","args":""}}]}}"#;
        let val: serde_json::Value = serde_json::from_str(json_line).unwrap();
        let mut counts = HashMap::new();
        extract_skill_names(&val, &mut counts);
        assert_eq!(counts.get("codex"), Some(&1));
        assert_eq!(counts.get("codex:rescue"), None);
    }

    #[test]
    fn extract_skill_names_ignores_non_skill_tools() {
        let json_line = r#"{"message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#;
        let val: serde_json::Value = serde_json::from_str(json_line).unwrap();
        let mut counts = HashMap::new();
        extract_skill_names(&val, &mut counts);
        assert!(counts.is_empty());
    }

    #[test]
    fn extract_skill_names_multiple_in_one_message() {
        let json_line = r#"{"message":{"content":[
            {"type":"tool_use","name":"Skill","input":{"skill":"agent-doc","args":"a.md"}},
            {"type":"tool_use","name":"Skill","input":{"skill":"tsift","args":"search foo"}}
        ]}}"#;
        let val: serde_json::Value = serde_json::from_str(json_line).unwrap();
        let mut counts = HashMap::new();
        extract_skill_names(&val, &mut counts);
        assert_eq!(counts.get("agent-doc"), Some(&1));
        assert_eq!(counts.get("tsift"), Some(&1));
    }

    #[test]
    fn scan_jsonl_counts_skills() {
        let dir = tempfile::tempdir().unwrap();
        let jsonl = dir.path().join("session.jsonl");
        let content = concat!(
            r#"{"message":{"content":[{"type":"tool_use","name":"Skill","input":{"skill":"agent-doc","args":"a.md"}}]}}"#,
            "\n",
            r#"{"message":{"content":[{"type":"tool_use","name":"Skill","input":{"skill":"agent-doc","args":"b.md"}}]}}"#,
            "\n",
            r#"{"message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#,
            "\n",
        );
        fs::write(&jsonl, content).unwrap();
        let mut counts = HashMap::new();
        scan_jsonl(&jsonl, &mut counts).unwrap();
        assert_eq!(counts.get("agent-doc"), Some(&2));
        assert_eq!(counts.len(), 1);
    }

    // --- cleanup recommendations ---

    #[test]
    fn generate_cleanup_flags_broken_skills() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("broken")).unwrap();
        let skill = dir.path().join("ok");
        fs::create_dir(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "---\ndescription: fine\n---\n").unwrap();

        let mut result = scan_skills(dir.path()).unwrap();
        generate_cleanup(&mut result);
        let cleanup = result.cleanup.unwrap();
        assert!(cleanup.iter().any(|e| e.skill == "broken"));
        assert!(!cleanup.iter().any(|e| e.skill == "ok"));
    }

    #[test]
    fn generate_cleanup_flags_never_used() {
        let mut result = AuditResult {
            skills_dir: PathBuf::from("/tmp"),
            total: 2,
            healthy: 2,
            broken: 0,
            skills: vec![
                {
                    let mut s = make_skill("used", "does things");
                    s.invocation_count = Some(5);
                    s
                },
                {
                    let mut s = make_skill("unused", "does other things");
                    s.invocation_count = Some(0);
                    s
                },
            ],
            manifest_diffs: None,
            similar_pairs: Vec::new(),
            usage: None,
            cleanup: None,
        };
        generate_cleanup(&mut result);
        let cleanup = result.cleanup.unwrap();
        assert!(
            cleanup
                .iter()
                .any(|e| e.skill == "unused" && e.reasons.iter().any(|r| r.contains("never used")))
        );
        assert!(!cleanup.iter().any(|e| e.skill == "used"));
    }

    #[test]
    fn generate_cleanup_flags_high_similarity_duplicates() {
        let mut result = AuditResult {
            skills_dir: PathBuf::from("/tmp"),
            total: 2,
            healthy: 2,
            broken: 0,
            skills: vec![
                make_skill("search-a", "search code"),
                make_skill("search-b", "search code"),
            ],
            manifest_diffs: None,
            similar_pairs: vec![SimilarPair {
                skill_a: "search-a".to_string(),
                skill_b: "search-b".to_string(),
                score: 0.8,
                desc_a: "search code".to_string(),
                desc_b: "search code".to_string(),
            }],
            usage: None,
            cleanup: None,
        };
        generate_cleanup(&mut result);
        let cleanup = result.cleanup.unwrap();
        assert_eq!(cleanup.len(), 2);
        assert!(
            cleanup
                .iter()
                .all(|e| e.reasons.iter().any(|r| r.contains("similarity")))
        );
    }

    #[test]
    fn generate_cleanup_sorted_by_token_estimate() {
        let dir = tempfile::tempdir().unwrap();
        let small = dir.path().join("small");
        fs::create_dir(&small).unwrap();
        fs::write(small.join("SKILL.md"), "x").unwrap();

        let big = dir.path().join("big");
        fs::create_dir(&big).unwrap();
        fs::write(big.join("SKILL.md"), "x".repeat(10000)).unwrap();

        let mut result = AuditResult {
            skills_dir: dir.path().to_path_buf(),
            total: 2,
            healthy: 0,
            broken: 2,
            skills: vec![
                SkillEntry {
                    name: "small".to_string(),
                    path: small,
                    has_skill_md: true,
                    is_symlink: false,
                    description: None,
                    issues: vec!["broken".to_string()],
                    invocation_count: None,
                },
                SkillEntry {
                    name: "big".to_string(),
                    path: big,
                    has_skill_md: true,
                    is_symlink: false,
                    description: None,
                    issues: vec!["broken".to_string()],
                    invocation_count: None,
                },
            ],
            manifest_diffs: None,
            similar_pairs: Vec::new(),
            usage: None,
            cleanup: None,
        };
        generate_cleanup(&mut result);
        let cleanup = result.cleanup.unwrap();
        assert_eq!(cleanup[0].skill, "big");
        assert!(cleanup[0].token_estimate > cleanup[1].token_estimate);
    }

    // --- report ---

    #[test]
    fn write_report_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let report_path = dir.path().join("reports/audit.md");
        let result = AuditResult {
            skills_dir: PathBuf::from("/test/skills"),
            total: 2,
            healthy: 1,
            broken: 1,
            skills: vec![
                {
                    let mut s = make_skill("good", "a good skill");
                    s.invocation_count = Some(10);
                    s
                },
                {
                    let mut s = make_skill("bad", "a bad skill");
                    s.issues = vec!["SKILL.md missing".to_string()];
                    s.invocation_count = Some(0);
                    s
                },
            ],
            manifest_diffs: None,
            similar_pairs: Vec::new(),
            usage: None,
            cleanup: Some(vec![CleanupEntry {
                skill: "bad".to_string(),
                reasons: vec!["health: SKILL.md missing".to_string()],
                token_estimate: 500,
            }]),
        };
        write_report(&result, &report_path).unwrap();
        let content = fs::read_to_string(&report_path).unwrap();
        assert!(content.contains("# Skill Audit Report"));
        assert!(content.contains("good"));
        assert!(content.contains("bad"));
        assert!(content.contains("Cleanup Recommendations"));
        assert!(content.contains("500 tokens"));
    }

    #[test]
    fn format_tokens_units() {
        assert_eq!(format_tokens(500), "500 tokens");
        assert_eq!(format_tokens(1500), "1.5K tokens");
        assert_eq!(format_tokens(1_500_000), "1.5M tokens");
    }
}
