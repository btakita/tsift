use crate::{init, rewrite_command, session_digest, status};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const FAST_CORPUS_SEEDS: u64 = 12;
const FAST_CORPUS_STEPS: usize = 40;
const MEDIUM_CORPUS_SEEDS: u64 = 64;
const MEDIUM_CORPUS_STEPS: usize = 120;

#[derive(Default)]
struct Coverage {
    hits: BTreeSet<&'static str>,
}

impl Coverage {
    fn mark(&mut self, key: &'static str) {
        self.hits.insert(key);
    }

    fn merge(&mut self, other: Coverage) {
        self.hits.extend(other.hits);
    }

    fn require(&self, expected: &[&'static str]) {
        let missing = expected
            .iter()
            .copied()
            .filter(|key| !self.hits.contains(key))
            .collect::<Vec<_>>();
        assert!(missing.is_empty(), "missing sim coverage: {missing:?}");
    }
}

#[derive(Clone, Copy)]
enum Step {
    SessionLivePrompt,
    SessionInstructionBallast,
    RewriteLongSessionRead,
    RewriteShortSessionRead,
    RewriteTestCommand,
    RewriteLogCommand,
    RewriteDiffCommand,
    RewriteMetacharacterPassthrough,
    StatusMissingInstructions,
    StatusStaleInstructions,
    StatusCurrentInstructions,
}

impl Step {
    fn from_index(index: u64) -> Self {
        match index % 11 {
            0 => Self::SessionLivePrompt,
            1 => Self::SessionInstructionBallast,
            2 => Self::RewriteLongSessionRead,
            3 => Self::RewriteShortSessionRead,
            4 => Self::RewriteTestCommand,
            5 => Self::RewriteLogCommand,
            6 => Self::RewriteDiffCommand,
            7 => Self::RewriteMetacharacterPassthrough,
            8 => Self::StatusMissingInstructions,
            9 => Self::StatusStaleInstructions,
            _ => Self::StatusCurrentInstructions,
        }
    }
}

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed ^ 0x9e37_79b9_7f4a_7c15)
    }

    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }
}

struct SimWorld {
    root: TempDir,
    coverage: Coverage,
    next_file: usize,
}

impl SimWorld {
    fn new() -> Self {
        Self {
            root: TempDir::new().unwrap(),
            coverage: Coverage::default(),
            next_file: 0,
        }
    }

    fn run_steps(seed: u64, steps: usize) -> Coverage {
        let mut rng = Rng::new(seed);
        let mut world = SimWorld::new();
        for _ in 0..steps {
            world.apply(Step::from_index(rng.next()));
        }
        world.coverage
    }

    fn run_trace(trace: &[Step]) -> Coverage {
        let mut world = SimWorld::new();
        for step in trace {
            world.apply(*step);
        }
        world.coverage
    }

    fn apply(&mut self, step: Step) {
        match step {
            Step::SessionLivePrompt => self.session_live_prompt(),
            Step::SessionInstructionBallast => self.session_instruction_ballast(),
            Step::RewriteLongSessionRead => self.rewrite_long_session_read(),
            Step::RewriteShortSessionRead => self.rewrite_short_session_read(),
            Step::RewriteTestCommand => self.rewrite_test_command(),
            Step::RewriteLogCommand => self.rewrite_log_command(),
            Step::RewriteDiffCommand => self.rewrite_diff_command(),
            Step::RewriteMetacharacterPassthrough => self.rewrite_metacharacter_passthrough(),
            Step::StatusMissingInstructions => self.status_missing_instructions(),
            Step::StatusStaleInstructions => self.status_stale_instructions(),
            Step::StatusCurrentInstructions => self.status_current_instructions(),
        }
    }

    fn session_live_prompt(&mut self) {
        let text = "\
---
prompt_presets:
  '#spec-test-build-install-commit-push': update spec + tests
---
## Exchange
### Session Summary
- 2026-05-12 [#done] do old work
<!-- agent:boundary:old -->
do [#t275]. spec-test-build-install-commit-push
";
        let prompts = session_digest::extract_prompt_targets_from_text_block(text, true);
        assert_eq!(
            prompts,
            vec!["do [#t275]. spec-test-build-install-commit-push"]
        );
        self.coverage.mark("session/live_prompt");
    }

    fn session_instruction_ballast(&mut self) {
        let text = "\
## Workflow
- [ ] [#old] completed archive row
<!-- tsift:code-navigation v=0.1.0 -->
**Imperative edits are executable directives**
/agent-doc <FILE>
";
        let prompts = session_digest::extract_prompt_targets_from_text_block(text, false);
        assert!(prompts.is_empty(), "ballast produced prompts: {prompts:?}");
        self.coverage.mark("session/instruction_ballast");
    }

    fn rewrite_long_session_read(&mut self) {
        let session = self.write_session_file(90);
        let rewritten = rewrite_command(&format!("cat {}", session.display())).unwrap();
        assert!(rewritten.contains("tsift session-digest --path"));
        assert!(rewritten.contains("--source markdown"));
        self.coverage.mark("rewrite/long_session_read");
    }

    fn rewrite_short_session_read(&mut self) {
        let session = self.write_session_file(12);
        let rewritten = rewrite_command(&format!("cat {}", session.display()));
        assert_eq!(rewritten, None);
        self.coverage.mark("rewrite/short_session_passthrough");
    }

    fn rewrite_test_command(&mut self) {
        let rewritten = rewrite_command("cargo test --lib").unwrap();
        assert!(rewritten.contains("__digest-runner"));
        assert!(rewritten.contains("--kind \"test\""));
        assert!(rewritten.contains("--runner \"cargo\""));
        self.coverage.mark("rewrite/test_digest");
    }

    fn rewrite_log_command(&mut self) {
        let rewritten = rewrite_command("cargo build --release").unwrap();
        assert!(rewritten.contains("__digest-runner"));
        assert!(rewritten.contains("--kind \"log\""));
        self.coverage.mark("rewrite/log_digest");
    }

    fn rewrite_diff_command(&mut self) {
        assert_eq!(
            rewrite_command("git diff"),
            Some("tsift diff-digest .".to_string())
        );
        assert_eq!(
            rewrite_command("git diff --cached"),
            Some("tsift diff-digest --cached .".to_string())
        );
        self.coverage.mark("rewrite/diff_digest");
    }

    fn rewrite_metacharacter_passthrough(&mut self) {
        assert_eq!(rewrite_command("cargo test | head"), None);
        self.coverage.mark("rewrite/metacharacter_passthrough");
    }

    fn status_missing_instructions(&mut self) {
        let dir = self.empty_project_dir("missing");
        let report = status::check_status(&dir).unwrap();
        assert!(matches!(
            report.instructions,
            init::InstructionStatus::Missing
        ));
        assert_eq!(
            report.recommendations.run.as_deref(),
            Some("tsift init && tsift index .")
        );
        self.coverage.mark("status/missing_instructions");
    }

    fn status_stale_instructions(&mut self) {
        let dir = self.empty_project_dir("stale");
        write_instruction_file(&dir, "0.0.1");
        let report = status::check_status(&dir).unwrap();
        assert!(matches!(
            report.instructions,
            init::InstructionStatus::Stale { .. }
        ));
        assert_eq!(
            report.recommendations.run.as_deref(),
            Some("tsift init && tsift index .")
        );
        self.coverage.mark("status/stale_instructions");
    }

    fn status_current_instructions(&mut self) {
        let dir = self.empty_project_dir("current");
        write_instruction_file(&dir, init::TSIFT_VERSION);
        let report = status::check_status(&dir).unwrap();
        assert!(matches!(
            report.instructions,
            init::InstructionStatus::Current { .. }
        ));
        assert_eq!(report.recommendations.run.as_deref(), Some("tsift index ."));
        self.coverage.mark("status/current_instructions");
    }

    fn write_session_file(&mut self, lines: usize) -> PathBuf {
        let path = self.next_path("session", "md");
        let mut body = String::from("---\nagent_doc_session: tsift-sim\n---\n\n## Exchange\n");
        for index in 0..lines {
            body.push_str(&format!("line {index}: do [#sim]. run tests?\n"));
        }
        fs::write(&path, body).unwrap();
        path
    }

    fn empty_project_dir(&mut self, label: &str) -> PathBuf {
        let path = self.next_path(label, "dir");
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn next_path(&mut self, prefix: &str, suffix: &str) -> PathBuf {
        self.next_file += 1;
        self.root
            .path()
            .join(format!("{prefix}-{}.{suffix}", self.next_file))
    }
}

fn write_instruction_file(dir: &Path, version: &str) {
    fs::write(
        dir.join("AGENTS.md"),
        format!(
            "<!-- tsift:code-navigation v={version} -->\n## Code Navigation\n<!-- /tsift:code-navigation -->\n"
        ),
    )
    .unwrap();
}

fn required_coverage() -> &'static [&'static str] {
    &[
        "session/live_prompt",
        "session/instruction_ballast",
        "rewrite/long_session_read",
        "rewrite/short_session_passthrough",
        "rewrite/test_digest",
        "rewrite/log_digest",
        "rewrite/diff_digest",
        "rewrite/metacharacter_passthrough",
        "status/missing_instructions",
        "status/stale_instructions",
        "status/current_instructions",
    ]
}

#[test]
fn sim_world_named_edge_trace_covers_session_rewrite_status_edges() {
    let coverage = SimWorld::run_trace(&[
        Step::SessionLivePrompt,
        Step::SessionInstructionBallast,
        Step::RewriteLongSessionRead,
        Step::RewriteShortSessionRead,
        Step::RewriteTestCommand,
        Step::RewriteLogCommand,
        Step::RewriteDiffCommand,
        Step::RewriteMetacharacterPassthrough,
        Step::StatusMissingInstructions,
        Step::StatusStaleInstructions,
        Step::StatusCurrentInstructions,
    ]);
    coverage.require(required_coverage());
}

#[test]
fn sim_world_fast_seed_corpus_preserves_core_invariants() {
    let mut coverage = Coverage::default();
    for seed in 0..FAST_CORPUS_SEEDS {
        coverage.merge(SimWorld::run_steps(seed, FAST_CORPUS_STEPS));
    }
    coverage.require(required_coverage());
}

#[test]
#[ignore = "wider deterministic sim corpus; run in CI with `make ci-full`"]
fn sim_world_medium_seed_corpus_runs_wider_deterministic_budget() {
    let mut coverage = Coverage::default();
    for seed in 0..MEDIUM_CORPUS_SEEDS {
        coverage.merge(SimWorld::run_steps(seed, MEDIUM_CORPUS_STEPS));
    }
    coverage.require(required_coverage());
}
