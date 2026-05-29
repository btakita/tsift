//! Shared output infrastructure for tsift-cli command handlers.
//!
//! Phase 1a-i of `#split-tsift-cli-subcommands`:
//! - `OutputFormat` — per-command format flags (compact / pretty / terse / schema / envelope)
//! - `ResponseBudget` + `ResponseBudgetPreset` — adaptive item/byte budgets for envelope-wrapped previews
//! - `DEFAULT_BUDGET_*` constants
//!
//! `ToolEnvelope`, `TranscriptArtifactRef`, `TagpathSearchOpts`, `TagpathAnnotationDiagnostic`,
//! and the `annotate_*_with_tagpath` family stay in `lib.rs` for now; they pull in
//! cross-cutting types (`CommunityResult`, `CommunityMemberAmbiguityDiagnostic`) that are
//! cleaner to move alongside the graph/community command cluster in a later sub-phase.

use clap::ValueEnum;

pub(crate) const DEFAULT_BUDGET_ITEMS: usize = 5;
pub(crate) const DEFAULT_BUDGET_BYTES: usize = 160;
pub(crate) const DEFAULT_FOLLOW_UP_ITEMS: usize = 4;

#[derive(Clone, Copy)]
pub(crate) struct OutputFormat {
    pub json_output: bool,
    pub compact: bool,
    pub pretty: bool,
    pub terse: bool,
    pub schema: bool,
    pub envelope: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ResponseBudget {
    pub max_items: Option<usize>,
    pub max_bytes: Option<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum ResponseBudgetPreset {
    Small,
    Normal,
    Deep,
    Auto,
}

impl ResponseBudget {
    pub fn new(max_items: Option<usize>, max_bytes: Option<usize>) -> Self {
        Self {
            max_items,
            max_bytes,
        }
    }

    pub fn from_cli(
        max_items: Option<usize>,
        max_bytes: Option<usize>,
        preset: Option<ResponseBudgetPreset>,
        envelope: bool,
    ) -> Self {
        let preset = preset.or_else(|| envelope.then_some(ResponseBudgetPreset::Auto));
        let Some(preset) = preset else {
            return Self::new(max_items, max_bytes);
        };

        let defaults = preset.resolve();
        Self::new(
            max_items.or(defaults.max_items),
            max_bytes.or(defaults.max_bytes),
        )
    }

    pub fn is_active(self) -> bool {
        self.max_items.is_some() || self.max_bytes.is_some()
    }

    pub fn preview_items(self) -> usize {
        self.max_items.unwrap_or(DEFAULT_BUDGET_ITEMS)
    }

    pub fn preview_bytes(self) -> usize {
        self.max_bytes.unwrap_or(DEFAULT_BUDGET_BYTES)
    }

    pub fn follow_up_items(self) -> usize {
        self.preview_items().max(DEFAULT_FOLLOW_UP_ITEMS)
    }
}

impl ResponseBudgetPreset {
    pub fn resolve(self) -> ResponseBudget {
        match self {
            ResponseBudgetPreset::Small => ResponseBudget::new(Some(3), Some(120)),
            ResponseBudgetPreset::Normal => {
                ResponseBudget::new(Some(DEFAULT_BUDGET_ITEMS), Some(DEFAULT_BUDGET_BYTES))
            }
            ResponseBudgetPreset::Deep => ResponseBudget::new(Some(10), Some(240)),
            ResponseBudgetPreset::Auto => adaptive_response_budget(),
        }
    }
}

fn adaptive_response_budget() -> ResponseBudget {
    let context_window = [
        "TSIFT_CONTEXT_WINDOW",
        "CODEX_CONTEXT_WINDOW",
        "CLAUDE_CONTEXT_WINDOW",
    ]
    .iter()
    .find_map(|key| {
        std::env::var(key)
            .ok()
            .and_then(|value| value.replace('_', "").parse::<usize>().ok())
    });

    match context_window {
        Some(window) if window <= 64_000 => ResponseBudgetPreset::Small.resolve(),
        Some(window) if window >= 200_000 => ResponseBudgetPreset::Deep.resolve(),
        _ => ResponseBudgetPreset::Normal.resolve(),
    }
}
