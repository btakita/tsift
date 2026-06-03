use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::output::{OutputFormat, ToolEnvelopeSummary};
use crate::{
    envelope_metric, print_json_or_envelope, shell_quote,
    stable_handle,
};

#[derive(Deserialize, Serialize)]
pub(crate) struct TokenSavingsFixture {
    pub schema_version: u64,
    #[serde(default)]
    pub description: String,
    pub token_estimate: String,
    pub cases: Vec<TokenSavingsFixtureCase>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct TokenSavingsFixtureCase {
    pub name: String,
    pub surface: String,
    pub minimum_savings_percent: f64,
    pub raw_symbols: Vec<TokenSavingsRawSymbol>,
    pub tagpath_families: Vec<TokenSavingsFamily>,
    #[serde(default)]
    pub session_review_inputs: Option<TokenSavingsSessionReviewInputs>,
    #[serde(default)]
    pub context_pack_inputs: Option<TokenSavingsContextPackInputs>,
    #[serde(default)]
    pub source_read_inputs: Option<TokenSavingsSourceReadInputs>,
    #[serde(default)]
    pub markdown_projection_inputs: Option<TokenSavingsMarkdownProjectionInputs>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct TokenSavingsRawSymbol {
    pub identifier: String,
    pub file: String,
    pub line: u64,
    pub context: String,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct TokenSavingsFamily {
    pub canonical: String,
    pub count: usize,
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct TokenSavingsSessionReviewInputs {
    pub prompt_targets: Vec<serde_json::Value>,
    pub sessions: Vec<serde_json::Value>,
    pub commands: Vec<serde_json::Value>,
    pub touched_files: Vec<serde_json::Value>,
    pub touched_symbols: Vec<serde_json::Value>,
    pub failures: Vec<serde_json::Value>,
    pub guardrails: Vec<serde_json::Value>,
    pub largest_turns: Vec<serde_json::Value>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct TokenSavingsContextPackInputs {
    pub next_context: Vec<serde_json::Value>,
    pub diff: Vec<serde_json::Value>,
    pub test: Vec<serde_json::Value>,
    pub log: Vec<serde_json::Value>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct TokenSavingsSourceReadInputs {
    pub reads: Vec<TokenSavingsSourceReadInput>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct TokenSavingsSourceReadInput {
    pub command: String,
    pub file: String,
    pub raw_start: u64,
    pub raw_lines: u64,
    pub raw_excerpt: String,
    pub envelope_start: u64,
    pub envelope_lines: u64,
    pub required_line_anchors: Vec<u64>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct TokenSavingsMarkdownProjectionInputs {
    pub documents: Vec<TokenSavingsMarkdownProjectionInput>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct TokenSavingsMarkdownProjectionInput {
    pub command: String,
    pub file: String,
    pub raw_markdown: String,
    pub outline_nodes: Vec<String>,
    pub selected_nodes: Vec<String>,
    pub expand: String,
}

#[derive(Serialize)]
pub(crate) struct TokenSavingsEnvelopeFamily {
    pub handle: String,
    pub tag_alias: String,
    pub count: usize,
    pub expand: String,
}

#[derive(Serialize)]
pub(crate) struct TokenSavingsSessionReviewEnvelope<'a> {
    pub section: &'a str,
    pub handle: String,
    pub count: usize,
    pub expand: String,
}

#[derive(Serialize)]
pub(crate) struct TokenSavingsContextPackEnvelope<'a> {
    pub section: &'a str,
    pub handle: String,
    pub count: usize,
    pub expand: String,
}

#[derive(Serialize)]
pub(crate) struct TokenSavingsSourceReadEnvelope {
    pub handle: String,
    pub file: String,
    pub start: u64,
    pub lines: u64,
    pub required_line_anchors: Vec<u64>,
    pub expand: String,
}

#[derive(Serialize)]
pub(crate) struct TokenSavingsMarkdownProjectionEnvelope {
    pub handle: String,
    pub file: String,
    pub outline_nodes: usize,
    pub selected_nodes: Vec<String>,
    pub expand: String,
}

#[derive(Serialize)]
pub(crate) struct TokenSavingsCaseReport {
    pub name: String,
    pub surface: String,
    pub raw_symbol_count: usize,
    pub family_count: usize,
    pub raw_bytes: usize,
    pub envelope_bytes: usize,
    pub byte_delta: usize,
    pub raw_estimated_tokens: usize,
    pub envelope_estimated_tokens: usize,
    pub estimated_token_delta: usize,
    pub savings_percent: f64,
    pub minimum_savings_percent: f64,
    pub status: String,
}

#[derive(Serialize)]
pub(crate) struct TokenSavingsTotals {
    pub cases: usize,
    pub raw_bytes: usize,
    pub envelope_bytes: usize,
    pub byte_delta: usize,
    pub raw_estimated_tokens: usize,
    pub envelope_estimated_tokens: usize,
    pub estimated_token_delta: usize,
    pub savings_percent: f64,
}

#[derive(Serialize)]
pub(crate) struct TokenSavingsReport {
    pub schema_version: u64,
    pub token_estimate: String,
    pub pass: bool,
    pub totals: TokenSavingsTotals,
    pub cases: Vec<TokenSavingsCaseReport>,
}

fn savings_percent(raw_bytes: usize, envelope_bytes: usize) -> f64 {
    if raw_bytes == 0 || envelope_bytes >= raw_bytes {
        0.0
    } else {
        ((raw_bytes - envelope_bytes) as f64 / raw_bytes as f64) * 100.0
    }
}

fn token_savings_expand_command(surface: &str, canonical: &str) -> String {
    let query = canonical.replace('_', " ");
    match surface {
        "explain" => format!(
            "tsift --envelope explain {} --budget normal",
            shell_quote(canonical)
        ),
        "session-review" => format!("tsift summarize {}", shell_quote(canonical)),
        "context-pack" => {
            "tsift --envelope context-pack <target> --test-input <test.log> --log-input <build.log> --budget normal"
                .to_string()
        }
        _ => format!(
            "tsift --envelope search {} --budget normal",
            shell_quote(&query)
        ),
    }
}

pub(crate) fn token_savings_envelope_families(
    case: &TokenSavingsFixtureCase,
) -> Vec<TokenSavingsEnvelopeFamily> {
    case.tagpath_families
        .iter()
        .map(|family| {
            let key = format!("{}:{}:{}", case.surface, case.name, family.canonical);
            TokenSavingsEnvelopeFamily {
                handle: stable_handle("tfam", &key),
                tag_alias: family.canonical.replace('_', "/"),
                count: family.count,
                expand: token_savings_expand_command(&case.surface, &family.canonical),
            }
        })
        .collect()
}

fn token_savings_context_pack_raw_bytes(inputs: &TokenSavingsContextPackInputs) -> Result<usize> {
    Ok(serde_json::to_vec(inputs)?.len())
}

pub(crate) fn token_savings_session_review_raw_bytes(
    inputs: &TokenSavingsSessionReviewInputs,
) -> Result<usize> {
    Ok(serde_json::to_vec(inputs)?.len())
}

fn token_savings_source_read_raw_bytes(inputs: &TokenSavingsSourceReadInputs) -> Result<usize> {
    Ok(serde_json::to_vec(&inputs.reads)?.len())
}

fn token_savings_markdown_projection_raw_bytes(
    inputs: &TokenSavingsMarkdownProjectionInputs,
) -> Result<usize> {
    Ok(serde_json::to_vec(&inputs.documents)?.len())
}

pub(crate) fn token_savings_session_review_envelope(
    case: &TokenSavingsFixtureCase,
    inputs: &TokenSavingsSessionReviewInputs,
) -> Vec<TokenSavingsSessionReviewEnvelope<'static>> {
    let mut rows = vec![
        TokenSavingsSessionReviewEnvelope {
            section: "prompt_targets",
            handle: stable_handle("tsr", &format!("{}:prompt_targets", case.name)),
            count: inputs.prompt_targets.len(),
            expand: "tsift session-review <target> --json".to_string(),
        },
        TokenSavingsSessionReviewEnvelope {
            section: "sessions",
            handle: stable_handle("tsr", &format!("{}:sessions", case.name)),
            count: inputs.sessions.len(),
            expand: "tsift session-review <target> --json".to_string(),
        },
        TokenSavingsSessionReviewEnvelope {
            section: "commands",
            handle: stable_handle("tsr", &format!("{}:commands", case.name)),
            count: inputs.commands.len(),
            expand: "tsift session-digest --source auto --input <transcript> --json".to_string(),
        },
        TokenSavingsSessionReviewEnvelope {
            section: "files",
            handle: stable_handle("tsr", &format!("{}:files", case.name)),
            count: inputs.touched_files.len(),
            expand: "tsift session-review <target> --json".to_string(),
        },
        TokenSavingsSessionReviewEnvelope {
            section: "symbols",
            handle: stable_handle("tsr", &format!("{}:symbols", case.name)),
            count: inputs.touched_symbols.len(),
            expand: "tsift --envelope search <symbol> --budget normal".to_string(),
        },
        TokenSavingsSessionReviewEnvelope {
            section: "failures",
            handle: stable_handle("tsr", &format!("{}:failures", case.name)),
            count: inputs.failures.len(),
            expand: "tsift session-review <target> --json".to_string(),
        },
        TokenSavingsSessionReviewEnvelope {
            section: "guardrails",
            handle: stable_handle("tsr", &format!("{}:guardrails", case.name)),
            count: inputs.guardrails.len(),
            expand: "tsift session-cost --input <transcript> --json".to_string(),
        },
        TokenSavingsSessionReviewEnvelope {
            section: "largest_turns",
            handle: stable_handle("tsr", &format!("{}:largest_turns", case.name)),
            count: inputs.largest_turns.len(),
            expand: "tsift session-cost --input <transcript> --json".to_string(),
        },
    ];
    rows.retain(|row| row.count > 0);
    rows
}

pub(crate) fn token_savings_context_pack_envelope(
    case: &TokenSavingsFixtureCase,
    inputs: &TokenSavingsContextPackInputs,
) -> Vec<TokenSavingsContextPackEnvelope<'static>> {
    let mut rows = vec![
        TokenSavingsContextPackEnvelope {
            section: "next_context",
            handle: stable_handle("tcp", &format!("{}:next_context", case.name)),
            count: inputs.next_context.len(),
            expand: "tsift session-review --next-context <target> --json".to_string(),
        },
        TokenSavingsContextPackEnvelope {
            section: "diff",
            handle: stable_handle("tcp", &format!("{}:diff", case.name)),
            count: inputs.diff.len(),
            expand: "tsift diff-digest . --json".to_string(),
        },
        TokenSavingsContextPackEnvelope {
            section: "test",
            handle: stable_handle("tcp", &format!("{}:test", case.name)),
            count: inputs.test.len(),
            expand: "tsift test-digest --path . < test.log".to_string(),
        },
        TokenSavingsContextPackEnvelope {
            section: "log",
            handle: stable_handle("tcp", &format!("{}:log", case.name)),
            count: inputs.log.len(),
            expand: "tsift log-digest --path . < build.log".to_string(),
        },
    ];
    rows.retain(|row| row.count > 0);
    rows
}

pub(crate) fn token_savings_source_read_envelope(
    case: &TokenSavingsFixtureCase,
    inputs: &TokenSavingsSourceReadInputs,
) -> Result<Vec<TokenSavingsSourceReadEnvelope>> {
    inputs
        .reads
        .iter()
        .map(|read| {
            if read.envelope_lines == 0 {
                bail!(
                    "source-read fixture {} has an empty envelope window for {}",
                    case.name,
                    read.command
                );
            }
            let envelope_end = read
                .envelope_start
                .saturating_add(read.envelope_lines)
                .saturating_sub(1);
            for anchor in &read.required_line_anchors {
                if *anchor < read.envelope_start || *anchor > envelope_end {
                    bail!(
                        "source-read fixture {} hides required line anchor {} for {} outside {}-{}",
                        case.name,
                        anchor,
                        read.command,
                        read.envelope_start,
                        envelope_end
                    );
                }
            }
            Ok(TokenSavingsSourceReadEnvelope {
                handle: stable_handle("tsrc", &format!("{}:{}", case.name, read.command)),
                file: read.file.clone(),
                start: read.envelope_start,
                lines: read.envelope_lines,
                required_line_anchors: read.required_line_anchors.clone(),
                expand: format!(
                    "tsift --envelope source-read {} --start {} --lines {} --budget normal",
                    shell_quote(&read.file),
                    read.envelope_start,
                    read.envelope_lines
                ),
            })
        })
        .collect()
}

pub(crate) fn token_savings_markdown_projection_envelope(
    case: &TokenSavingsFixtureCase,
    inputs: &TokenSavingsMarkdownProjectionInputs,
) -> Result<Vec<TokenSavingsMarkdownProjectionEnvelope>> {
    inputs
        .documents
        .iter()
        .map(|doc| {
            if doc.outline_nodes.is_empty() {
                bail!(
                    "markdown projection fixture {} has no outline nodes for {}",
                    case.name,
                    doc.command
                );
            }
            if doc.selected_nodes.is_empty() {
                bail!(
                    "markdown projection fixture {} has no selected-node expansion handles for {}",
                    case.name,
                    doc.command
                );
            }
            Ok(TokenSavingsMarkdownProjectionEnvelope {
                handle: stable_handle("tmd", &format!("{}:{}", case.name, doc.command)),
                file: doc.file.clone(),
                outline_nodes: doc.outline_nodes.len(),
                selected_nodes: doc.selected_nodes.clone(),
                expand: doc.expand.clone(),
            })
        })
        .collect()
}

pub(crate) fn build_token_savings_report(fixture: &TokenSavingsFixture) -> Result<TokenSavingsReport> {
    let mut cases = Vec::new();
    let mut total_raw_bytes = 0;
    let mut total_envelope_bytes = 0;

    for case in &fixture.cases {
        let mut raw_bytes = serde_json::to_vec(&case.raw_symbols)?.len();
        let envelope = token_savings_envelope_families(case);
        let mut envelope_bytes = serde_json::to_vec(&envelope)?.len();
        if let Some(inputs) = &case.session_review_inputs {
            raw_bytes += token_savings_session_review_raw_bytes(inputs)?;
            envelope_bytes +=
                serde_json::to_vec(&token_savings_session_review_envelope(case, inputs))?.len();
        }
        if let Some(inputs) = &case.context_pack_inputs {
            raw_bytes += token_savings_context_pack_raw_bytes(inputs)?;
            envelope_bytes +=
                serde_json::to_vec(&token_savings_context_pack_envelope(case, inputs))?.len();
        }
        if let Some(inputs) = &case.source_read_inputs {
            raw_bytes += token_savings_source_read_raw_bytes(inputs)?;
            envelope_bytes +=
                serde_json::to_vec(&token_savings_source_read_envelope(case, inputs)?)?.len();
        }
        if let Some(inputs) = &case.markdown_projection_inputs {
            raw_bytes += token_savings_markdown_projection_raw_bytes(inputs)?;
            envelope_bytes +=
                serde_json::to_vec(&token_savings_markdown_projection_envelope(case, inputs)?)?
                    .len();
        }
        let byte_delta = raw_bytes.saturating_sub(envelope_bytes);
        let raw_estimated_tokens = crate::estimated_tokens_from_bytes(raw_bytes);
        let envelope_estimated_tokens = crate::estimated_tokens_from_bytes(envelope_bytes);
        let estimated_token_delta = raw_estimated_tokens.saturating_sub(envelope_estimated_tokens);
        let savings_pct = savings_percent(raw_bytes, envelope_bytes);
        let pass = savings_pct >= case.minimum_savings_percent;

        total_raw_bytes += raw_bytes;
        total_envelope_bytes += envelope_bytes;
        cases.push(TokenSavingsCaseReport {
            name: case.name.clone(),
            surface: case.surface.clone(),
            raw_symbol_count: case.raw_symbols.len(),
            family_count: case.tagpath_families.len(),
            raw_bytes,
            envelope_bytes,
            byte_delta,
            raw_estimated_tokens,
            envelope_estimated_tokens,
            estimated_token_delta,
            savings_percent: savings_pct,
            minimum_savings_percent: case.minimum_savings_percent,
            status: if pass { "pass" } else { "fail" }.to_string(),
        });
    }

    let total_byte_delta = total_raw_bytes.saturating_sub(total_envelope_bytes);
    let total_raw_estimated_tokens = crate::estimated_tokens_from_bytes(total_raw_bytes);
    let total_envelope_estimated_tokens = crate::estimated_tokens_from_bytes(total_envelope_bytes);
    let total_estimated_token_delta =
        total_raw_estimated_tokens.saturating_sub(total_envelope_estimated_tokens);
    let pass = cases.iter().all(|case| case.status == "pass");

    Ok(TokenSavingsReport {
        schema_version: fixture.schema_version,
        token_estimate: fixture.token_estimate.clone(),
        pass,
        totals: TokenSavingsTotals {
            cases: cases.len(),
            raw_bytes: total_raw_bytes,
            envelope_bytes: total_envelope_bytes,
            byte_delta: total_byte_delta,
            raw_estimated_tokens: total_raw_estimated_tokens,
            envelope_estimated_tokens: total_envelope_estimated_tokens,
            estimated_token_delta: total_estimated_token_delta,
            savings_percent: savings_percent(total_raw_bytes, total_envelope_bytes),
        },
        cases,
    })
}

fn print_token_savings_human(report: &TokenSavingsReport) {
    println!(
        "surface\tcase\traw_bytes\tenvelope_bytes\tbyte_delta\traw_tokens\tenvelope_tokens\ttoken_delta\tsavings_percent\tminimum_percent\tstatus"
    );
    for case in &report.cases {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.1}\t{:.1}\t{}",
            case.surface,
            case.name,
            case.raw_bytes,
            case.envelope_bytes,
            case.byte_delta,
            case.raw_estimated_tokens,
            case.envelope_estimated_tokens,
            case.estimated_token_delta,
            case.savings_percent,
            case.minimum_savings_percent,
            case.status
        );
    }
    println!(
        "total\tall\t{}\t{}\t{}\t{}\t{}\t{}\t{:.1}\t-\t{}",
        report.totals.raw_bytes,
        report.totals.envelope_bytes,
        report.totals.byte_delta,
        report.totals.raw_estimated_tokens,
        report.totals.envelope_estimated_tokens,
        report.totals.estimated_token_delta,
        report.totals.savings_percent,
        if report.pass { "pass" } else { "fail" }
    );
}

pub(crate) fn cmd_token_savings(fixture_path: &Path, fail_under: bool, format: OutputFormat) -> Result<()> {
    let fixture_body = std::fs::read_to_string(fixture_path)
        .with_context(|| format!("reading token-savings fixture: {}", fixture_path.display()))?;
    let fixture: TokenSavingsFixture = serde_json::from_str(&fixture_body)
        .with_context(|| format!("parsing token-savings fixture: {}", fixture_path.display()))?;
    let report = build_token_savings_report(&fixture)?;

    if format.json_output {
        print_json_or_envelope(
            &report,
            &format,
            "token-savings",
            "report",
            ToolEnvelopeSummary {
                text: "token-savings report".to_string(),
                metrics: vec![
                    envelope_metric("cases", report.totals.cases),
                    envelope_metric("raw_tokens", report.totals.raw_estimated_tokens),
                    envelope_metric("envelope_tokens", report.totals.envelope_estimated_tokens),
                    envelope_metric("token_delta", report.totals.estimated_token_delta),
                    envelope_metric(
                        "savings_percent",
                        format!("{:.1}", report.totals.savings_percent),
                    ),
                ],
            },
            false,
            vec![],
        )?;
    } else {
        print_token_savings_human(&report);
    }

    if fail_under && !report.pass {
        bail!("token-savings threshold failed");
    }
    Ok(())
}
