//! Cross-session prompt-cache effectiveness history (#avbq).
//!
//! `session-cost` / `session-review` evaluate prompt-cache effectiveness for a
//! single transcript at a time, so the gate is per-fixture / per-transcript
//! only. A regression that only shows up when you compare *across* runs — a
//! steadily falling cached-input ratio, net cached tokens sliding negative, or
//! read/create regressions creeping in over successive sessions — is invisible
//! to that point-in-time view.
//!
//! This module persists one effectiveness sample per matched session under
//! `<root>/.tsift/prompt-cache-history/<key>.jsonl` (newline-delimited JSON,
//! oldest first) and compares each new sample against the previous recorded
//! sample for the same session key so cross-run regressions become detectable.
//! Recording is idempotent: re-running over the same unchanged session does not
//! append a duplicate sample.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::session_cost::{prompt_cache_read_create_regression, signed_token_delta};

pub const PROMPT_CACHE_HISTORY_SCHEMA_VERSION: u64 = 1;

/// Minimum cached-input-ratio percentage-point drop between consecutive runs
/// that counts as a cross-run regression. Small run-to-run jitter is expected;
/// only a meaningful slide is flagged.
const CACHED_RATIO_DROP_THRESHOLD_PCT: f64 = 5.0;

/// Cap the on-disk history so a long-lived project does not grow the JSONL file
/// without bound. Only the trailing window is retained — cross-run comparison
/// only needs the previous sample, the window is for trend inspection.
const MAX_HISTORY_SAMPLES: usize = 200;

/// One persisted prompt-cache effectiveness reading for a single session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptCacheEffectivenessSample {
    pub schema_version: u64,
    pub recorded_at_unix_secs: u64,
    pub session_source: String,
    pub session_path: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub session_modified_unix_secs: Option<u64>,
    pub prompt_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cached_input_ratio: Option<f64>,
    pub net_cached_input_tokens: i64,
    pub read_create_regressions: usize,
}

impl PromptCacheEffectivenessSample {
    /// Build a sample from a session's raw token totals, deriving the
    /// `cached_input_ratio`, `net_cached_input_tokens`, and
    /// `read_create_regressions` exactly the way the per-fixture effectiveness
    /// report does so persisted history stays consistent with the live gate.
    #[allow(clippy::too_many_arguments)]
    pub fn from_tokens(
        recorded_at_unix_secs: u64,
        session_source: impl Into<String>,
        session_path: impl Into<String>,
        session_modified_unix_secs: Option<u64>,
        prompt_tokens: u64,
        cached_input_tokens: u64,
        cache_creation_input_tokens: u64,
    ) -> Self {
        let cached_input_ratio = (prompt_tokens > 0).then_some(
            ((cached_input_tokens as f64) / (prompt_tokens as f64) * 10_000.0).round() / 100.0,
        );
        let net_cached_input_tokens =
            signed_token_delta(cached_input_tokens, cache_creation_input_tokens);
        let read_create_regressions = usize::from(
            prompt_cache_read_create_regression(cached_input_tokens, cache_creation_input_tokens)
                .is_some(),
        );
        Self {
            schema_version: PROMPT_CACHE_HISTORY_SCHEMA_VERSION,
            recorded_at_unix_secs,
            session_source: session_source.into(),
            session_path: session_path.into(),
            session_modified_unix_secs,
            prompt_tokens,
            cached_input_tokens,
            cache_creation_input_tokens,
            cached_input_ratio,
            net_cached_input_tokens,
            read_create_regressions,
        }
    }

    /// The content identity of a session reading, used to skip re-recording an
    /// unchanged session. Excludes `recorded_at_unix_secs` so re-running the
    /// same session at a later time does not append a duplicate row.
    fn identity(&self) -> (Option<u64>, u64, u64, u64) {
        (
            self.session_modified_unix_secs,
            self.prompt_tokens,
            self.cached_input_tokens,
            self.cache_creation_input_tokens,
        )
    }
}

/// A single detected cross-run regression between the previous and current
/// recorded sample for one session key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptCacheCrossRunRegression {
    pub kind: String,
    pub detail: String,
}

/// The result of recording a new sample: where it landed, the previous reading
/// it was compared against, and any cross-run regressions detected.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PromptCacheCrossRunComparison {
    pub session_source: String,
    pub session_path: String,
    pub samples_recorded: usize,
    pub appended: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<PromptCacheEffectivenessSample>,
    pub current: PromptCacheEffectivenessSample,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub regressions: Vec<PromptCacheCrossRunRegression>,
}

impl PromptCacheCrossRunComparison {
    pub fn has_regression(&self) -> bool {
        !self.regressions.is_empty()
    }
}

/// Directory holding the per-session prompt-cache history JSONL files.
pub fn prompt_cache_history_dir(root: &Path) -> PathBuf {
    root.join(".tsift/prompt-cache-history")
}

/// Stable, filesystem-safe key for a `(source, path)` session identity. The
/// sanitized source is kept human-readable as a prefix and a hash of the full
/// `(source, path)` pair disambiguates collisions after sanitization.
pub fn prompt_cache_history_key(session_source: &str, session_path: &str) -> String {
    let mut hasher = DefaultHasher::new();
    session_source.hash(&mut hasher);
    "\u{0}".hash(&mut hasher);
    session_path.hash(&mut hasher);
    let digest = hasher.finish();
    format!("{}-{digest:016x}", sanitize_key_component(session_source))
}

fn sanitize_key_component(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        "session".to_string()
    } else {
        trimmed.chars().take(40).collect()
    }
}

/// Path to the JSONL history file for a `(source, path)` session identity.
pub fn prompt_cache_history_path(root: &Path, session_source: &str, session_path: &str) -> PathBuf {
    prompt_cache_history_dir(root).join(format!(
        "{}.jsonl",
        prompt_cache_history_key(session_source, session_path)
    ))
}

/// Load the persisted history for a session key, oldest first. Malformed lines
/// are skipped so a partially-written file does not break analysis.
pub fn load_prompt_cache_history(
    root: &Path,
    session_source: &str,
    session_path: &str,
) -> Vec<PromptCacheEffectivenessSample> {
    let path = prompt_cache_history_path(root, session_source, session_path);
    let Ok(text) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<PromptCacheEffectivenessSample>(line).ok())
        .collect()
}

/// Record a new effectiveness sample for a session and compare it against the
/// previous recorded sample for the same key.
///
/// Idempotent: if the most recent persisted sample has the same content
/// identity (session mtime + token totals) the sample is not re-appended, but
/// the cross-run comparison against the prior reading is still returned so the
/// regression stays visible on repeat runs. Returns the comparison; the on-disk
/// write is best-effort and a write failure surfaces as `Err`.
pub fn record_prompt_cache_sample(
    root: &Path,
    sample: PromptCacheEffectivenessSample,
) -> Result<PromptCacheCrossRunComparison> {
    let existing = load_prompt_cache_history(root, &sample.session_source, &sample.session_path);

    // Compare against the most recent *distinct* prior reading.
    let previous = existing
        .iter()
        .rev()
        .find(|prior| prior.identity() != sample.identity())
        .cloned();
    let regressions = detect_cross_run_regressions(previous.as_ref(), &sample);

    let already_recorded = existing
        .last()
        .is_some_and(|last| last.identity() == sample.identity());

    let mut samples = existing;
    let appended = !already_recorded;
    if appended {
        samples.push(sample.clone());
        if samples.len() > MAX_HISTORY_SAMPLES {
            let overflow = samples.len() - MAX_HISTORY_SAMPLES;
            samples.drain(0..overflow);
        }
        write_prompt_cache_history(root, &sample.session_source, &sample.session_path, &samples)?;
    }

    Ok(PromptCacheCrossRunComparison {
        session_source: sample.session_source.clone(),
        session_path: sample.session_path.clone(),
        samples_recorded: samples.len(),
        appended,
        previous,
        current: sample,
        regressions,
    })
}

fn write_prompt_cache_history(
    root: &Path,
    session_source: &str,
    session_path: &str,
    samples: &[PromptCacheEffectivenessSample],
) -> Result<()> {
    let path = prompt_cache_history_path(root, session_source, session_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "creating prompt-cache history directory: {}",
                parent.display()
            )
        })?;
    }
    let mut body = String::new();
    for sample in samples {
        let line = serde_json::to_string(sample)
            .context("serializing prompt-cache effectiveness sample")?;
        body.push_str(&line);
        body.push('\n');
    }
    fs::write(&path, body)
        .with_context(|| format!("writing prompt-cache history: {}", path.display()))?;
    Ok(())
}

/// Compare a new sample against the previous recorded sample and flag the
/// regression classes #avbq tracks: a falling cached-input ratio, net cached
/// tokens sliding (especially crossing into negative territory), and new
/// read/create regressions.
pub fn detect_cross_run_regressions(
    previous: Option<&PromptCacheEffectivenessSample>,
    current: &PromptCacheEffectivenessSample,
) -> Vec<PromptCacheCrossRunRegression> {
    let Some(previous) = previous else {
        return Vec::new();
    };
    let mut regressions = Vec::new();

    if let (Some(prev_ratio), Some(curr_ratio)) =
        (previous.cached_input_ratio, current.cached_input_ratio)
    {
        let drop = prev_ratio - curr_ratio;
        if drop >= CACHED_RATIO_DROP_THRESHOLD_PCT {
            regressions.push(PromptCacheCrossRunRegression {
                kind: "cached_input_ratio_drop".to_string(),
                detail: format!(
                    "cached_input_ratio fell {drop:.2} points ({prev_ratio:.2}% -> {curr_ratio:.2}%) vs previous run"
                ),
            });
        }
    }

    if current.net_cached_input_tokens < previous.net_cached_input_tokens {
        let crossed_negative =
            previous.net_cached_input_tokens >= 0 && current.net_cached_input_tokens < 0;
        let detail = if crossed_negative {
            format!(
                "net_cached_input_tokens went negative ({} -> {}) — the session now spends more on cache creation than it saves on reads",
                previous.net_cached_input_tokens, current.net_cached_input_tokens
            )
        } else {
            format!(
                "net_cached_input_tokens fell {} -> {} vs previous run",
                previous.net_cached_input_tokens, current.net_cached_input_tokens
            )
        };
        // Only report a plain decline when it crosses zero or is a large slide;
        // otherwise net-token jitter would be noisy. Crossing negative is always
        // reported; a same-sign decline is reported when it more than halves.
        if crossed_negative
            || (previous.net_cached_input_tokens > 0
                && current.net_cached_input_tokens * 2 < previous.net_cached_input_tokens)
        {
            regressions.push(PromptCacheCrossRunRegression {
                kind: "net_cached_input_tokens_drop".to_string(),
                detail,
            });
        }
    }

    if current.read_create_regressions > previous.read_create_regressions {
        regressions.push(PromptCacheCrossRunRegression {
            kind: "read_create_regressions_increase".to_string(),
            detail: format!(
                "read_create_regressions rose {} -> {} vs previous run",
                previous.read_create_regressions, current.read_create_regressions
            ),
        });
    }

    regressions
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample(
        recorded_at: u64,
        modified: u64,
        prompt: u64,
        cached: u64,
        creation: u64,
    ) -> PromptCacheEffectivenessSample {
        PromptCacheEffectivenessSample::from_tokens(
            recorded_at,
            "claude",
            "/proj/session.jsonl",
            Some(modified),
            prompt,
            cached,
            creation,
        )
    }

    #[test]
    fn from_tokens_derives_ratio_net_and_regression() {
        let s = sample(100, 1, 1_000, 800, 100);
        assert_eq!(s.cached_input_ratio, Some(80.0));
        assert_eq!(s.net_cached_input_tokens, 700);
        // cached/creation = 8.0 >= 2.0 -> no regression
        assert_eq!(s.read_create_regressions, 0);

        let degraded = sample(100, 1, 1_000, 100, 800);
        assert_eq!(degraded.net_cached_input_tokens, -700);
        // cached/creation = 0.125 < 2.0 -> regression
        assert_eq!(degraded.read_create_regressions, 1);
    }

    #[test]
    fn first_recording_has_no_previous_and_no_regression() {
        let dir = tempdir().unwrap();
        let comparison =
            record_prompt_cache_sample(dir.path(), sample(100, 1, 1_000, 800, 100)).unwrap();
        assert!(comparison.appended);
        assert_eq!(comparison.samples_recorded, 1);
        assert!(comparison.previous.is_none());
        assert!(!comparison.has_regression());
    }

    #[test]
    fn cross_run_ratio_drop_is_detected_and_persisted() {
        let dir = tempdir().unwrap();
        record_prompt_cache_sample(dir.path(), sample(100, 1, 1_000, 900, 100)).unwrap();
        let comparison =
            record_prompt_cache_sample(dir.path(), sample(200, 2, 1_000, 700, 100)).unwrap();

        assert_eq!(comparison.samples_recorded, 2);
        assert!(comparison.previous.is_some());
        let kinds: Vec<_> = comparison
            .regressions
            .iter()
            .map(|r| r.kind.as_str())
            .collect();
        assert!(
            kinds.contains(&"cached_input_ratio_drop"),
            "expected ratio-drop regression, got {kinds:?}"
        );

        // Persisted across "runs": a fresh load sees both readings oldest-first.
        let loaded = load_prompt_cache_history(dir.path(), "claude", "/proj/session.jsonl");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].cached_input_ratio, Some(90.0));
        assert_eq!(loaded[1].cached_input_ratio, Some(70.0));
    }

    #[test]
    fn net_cached_going_negative_is_flagged() {
        let dir = tempdir().unwrap();
        record_prompt_cache_sample(dir.path(), sample(100, 1, 1_000, 800, 100)).unwrap();
        let comparison =
            record_prompt_cache_sample(dir.path(), sample(200, 2, 1_000, 100, 800)).unwrap();
        let kinds: Vec<_> = comparison
            .regressions
            .iter()
            .map(|r| r.kind.as_str())
            .collect();
        assert!(kinds.contains(&"net_cached_input_tokens_drop"));
        assert!(kinds.contains(&"read_create_regressions_increase"));
    }

    #[test]
    fn unchanged_session_is_not_re_recorded() {
        let dir = tempdir().unwrap();
        record_prompt_cache_sample(dir.path(), sample(100, 1, 1_000, 800, 100)).unwrap();
        // Same identity (mtime + tokens), later recorded_at: must not append.
        let comparison =
            record_prompt_cache_sample(dir.path(), sample(500, 1, 1_000, 800, 100)).unwrap();
        assert!(!comparison.appended);
        assert_eq!(comparison.samples_recorded, 1);
        let loaded = load_prompt_cache_history(dir.path(), "claude", "/proj/session.jsonl");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].recorded_at_unix_secs, 100);
    }

    #[test]
    fn small_ratio_jitter_is_not_a_regression() {
        let dir = tempdir().unwrap();
        record_prompt_cache_sample(dir.path(), sample(100, 1, 1_000, 900, 100)).unwrap();
        // 90% -> 88% is a 2-point dip, below the 5-point threshold.
        let comparison =
            record_prompt_cache_sample(dir.path(), sample(200, 2, 1_000, 880, 100)).unwrap();
        assert!(!comparison.has_regression(), "{:?}", comparison.regressions);
    }

    #[test]
    fn history_key_is_filesystem_safe_and_stable() {
        let a = prompt_cache_history_key("claude", "/home/x/proj/Plan File.md");
        let b = prompt_cache_history_key("claude", "/home/x/proj/Plan File.md");
        assert_eq!(a, b);
        assert!(a.starts_with("claude-"));
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        let other = prompt_cache_history_key("codex", "/home/x/proj/Plan File.md");
        assert_ne!(a, other);
    }
}
