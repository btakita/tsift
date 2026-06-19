use anyhow::{Result, bail};
use serde::Serialize;

use crate::output::{OutputFormat, ToolEnvelopeSummary};
use crate::{envelope_metric, print_json_or_envelope};

#[derive(Serialize)]
pub(crate) struct WorkflowStep {
    pub(crate) name: &'static str,
    pub(crate) goal: &'static str,
    pub(crate) command: &'static str,
    pub(crate) preserves: Vec<&'static str>,
    pub(crate) next: Vec<&'static str>,
}

#[derive(Serialize)]
pub(crate) struct WorkflowRecipe {
    pub(crate) topic: &'static str,
    pub(crate) summary: &'static str,
    pub(crate) handle_contract: Vec<&'static str>,
    pub(crate) steps: Vec<WorkflowStep>,
}

pub(crate) fn search_workflow_recipe() -> WorkflowRecipe {
    WorkflowRecipe {
        topic: "search",
        summary: "Chain exact search, semantic search, explain, summarize, and digest commands without dropping the stable handles emitted by each envelope.",
        handle_contract: vec![
            "Keep every handle with its originating command, query, path, and strategy.",
            "Use each step's expand command for deeper context, but cite the parent handle in notes and follow-up prompts.",
            "Prefer --envelope plus --budget normal when handing results to an agent so handles, follow_up commands, and truncation state stay machine-readable.",
        ],
        steps: vec![
            WorkflowStep {
                name: "exact-anchor",
                goal: "Start from a literal identifier, file path, error text, or prior handle label.",
                command: "tsift --envelope search \"<literal>\" --exact --path . --budget normal",
                preserves: vec![
                    "summary.handle",
                    "report.symbols[].handle",
                    "report.hits[].handle",
                ],
                next: vec![
                    "Run the matching report.symbols[].expand or report.hits[].expand command before broadening the query.",
                ],
            },
            WorkflowStep {
                name: "semantic-search",
                goal: "Broaden from the exact anchor to lexical, vector, or hybrid retrieval while keeping search-family handles.",
                command: "tsift --envelope search \"<concept>\" --path . --strategy hybrid --budget normal",
                preserves: vec![
                    "sfam-* symbol-family handles",
                    "shit-* content-hit handles",
                    "follow_up[]",
                ],
                next: vec![
                    "Use a symbol-family expand command for more search results, or pass the selected symbol name to explain.",
                ],
            },
            WorkflowStep {
                name: "explain-symbol",
                goal: "Expand a selected symbol into definitions, callers, callees, and community context.",
                command: "tsift --envelope explain \"<symbol>\" --path . --budget normal",
                preserves: vec![
                    "edef-* definition handles",
                    "ecall-* caller handles",
                    "eces-* callee handles",
                ],
                next: vec![
                    "Run edge expand commands for neighboring symbols, or summarize the selected symbol/file when the cache is available.",
                ],
            },
            WorkflowStep {
                name: "summarize-selection",
                goal: "Read cached summaries for the selected symbol or file without mutating the summary cache.",
                command: "tsift summarize \"<symbol>\" --path . --json",
                preserves: vec![
                    "summary refs emitted by search, explain, test-digest, log-digest, diff-digest, and context-pack",
                ],
                next: vec![
                    "If summaries are missing, run the status-recommended summarize --extract command outside the read-only query path.",
                ],
            },
            WorkflowStep {
                name: "digest-expansion",
                goal: "Expand from code navigation into changed files, tests, logs, or session context while retaining digest artifact handles.",
                command: "tsift --envelope context-pack <path> --test-input test.log --log-input build.log --budget normal",
                preserves: vec![
                    "artifact handles",
                    "touched symbol handles",
                    "digest summary handles",
                    "resume_commands[]",
                ],
                next: vec![
                    "Use resume_commands[] or each digest entry's expand command, and carry forward the original search/explain handle that motivated the digest.",
                ],
            },
        ],
    }
}

pub(crate) fn kg_workflow_recipe() -> WorkflowRecipe {
    WorkflowRecipe {
        topic: "kg",
        summary: "Build, refresh, and query the local Knowledge Graph (`.tsift/graph.db`) so semantic entity/relation evidence augments search and explain without re-extracting on every read.",
        handle_contract: vec![
            "Extract once per source, then read with `kg evidence`/`kg status`; do not re-run `kg extract` to answer a read.",
            "Keep the `--source-ref` you extracted under — `kg refresh` and `kg evidence` key off it.",
            "Prefer --envelope plus --budget normal so entity/relation handles and follow_up commands stay machine-readable.",
        ],
        steps: vec![
            WorkflowStep {
                name: "smoke-check",
                goal: "Confirm the Ollama-served extractor model is reachable before extracting.",
                command: "tsift kg smoke --unload --json",
                preserves: vec!["report.profile", "report.model"],
                next: vec![
                    "If smoke fails, start/point at the Ollama server before extracting; otherwise proceed to extract.",
                ],
            },
            WorkflowStep {
                name: "extract",
                goal: "Extract semantic entities/relations from a source into the project graph.db.",
                command: "tsift kg extract --input <file> --source-ref <file> --json",
                preserves: vec!["report.entities[]", "report.relations[]", "kg_source"],
                next: vec![
                    "Re-run per source you want covered; entities/relations persist into .tsift/graph.db with model confidence.",
                ],
            },
            WorkflowStep {
                name: "status",
                goal: "Report graph.db state — existence and node/edge counts by kind.",
                command: "tsift kg status --json",
                preserves: vec![
                    "report.total_nodes",
                    "report.total_edges",
                    "report.nodes_by_kind",
                ],
                next: vec!["Use kg refresh to find sources that changed since extraction."],
            },
            WorkflowStep {
                name: "refresh",
                goal: "Detect (or with --apply, re-extract) sources that drifted since extraction.",
                command: "tsift kg refresh --json   # add --apply to re-extract stale sources",
                preserves: vec!["report.stale_sources[]"],
                next: vec!["Run with --apply to refresh stale sources on demand."],
            },
            WorkflowStep {
                name: "evidence",
                goal: "Look up KG evidence for a symbol/kind — the agent-doc read seam over graph.db.",
                command: "tsift --envelope kg evidence --symbol \"<symbol>\" --json",
                preserves: vec!["matched semantic_entity nodes", "linked relation edges"],
                next: vec![
                    "Restrict with --kind (e.g. --kind semantic_entity); cite evidence alongside search/explain results and carry the source-ref forward.",
                ],
            },
        ],
    }
}

fn workflow_recipe(topic: &str) -> Result<WorkflowRecipe> {
    match topic {
        "search" | "search-handles" | "search-workflow" => Ok(search_workflow_recipe()),
        "kg" | "knowledge-graph" | "kg-workflow" => Ok(kg_workflow_recipe()),
        other => bail!("unknown workflow `{other}`; available workflows: search, kg"),
    }
}

fn print_workflow_human(recipe: &WorkflowRecipe, compact: bool) {
    if compact {
        println!("workflow:{} steps:{}", recipe.topic, recipe.steps.len());
        for step in &recipe.steps {
            println!("  {} cmd:{}", step.name, step.command);
        }
        return;
    }

    println!("Workflow: {}", recipe.topic);
    println!("{}", recipe.summary);
    println!();
    println!("Handle contract:");
    for item in &recipe.handle_contract {
        println!("  - {item}");
    }
    println!();
    println!("Steps:");
    for (index, step) in recipe.steps.iter().enumerate() {
        println!("  {}. {} - {}", index + 1, step.name, step.goal);
        println!("     cmd: {}", step.command);
        println!("     preserves: {}", step.preserves.join(", "));
        println!("     next: {}", step.next.join(" "));
    }
}

pub(crate) fn cmd_workflow(topic: &str, format: OutputFormat) -> Result<()> {
    let recipe = workflow_recipe(topic)?;
    if format.json_output {
        print_json_or_envelope(
            &recipe,
            &format,
            "workflow",
            recipe.topic,
            ToolEnvelopeSummary {
                text: recipe.summary.to_string(),
                metrics: vec![envelope_metric("steps", recipe.steps.len())],
            },
            false,
            recipe
                .steps
                .iter()
                .map(|step| step.command.to_string())
                .collect(),
        )
    } else {
        print_workflow_human(&recipe, format.compact);
        Ok(())
    }
}
