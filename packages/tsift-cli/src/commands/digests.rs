use std::fs;
use std::io::Read as _;
use std::path::Path;

use anyhow::{Context, Result, bail};
use tsift_agent_doc::{session_cost, session_digest, session_review};
use tsift_digest::{diff_digest, metric_digest};
use tsift_quality::lint;

use crate::output::{OutputFormat, ResponseBudget, ToolEnvelopeSummary};
use crate::{
    MetricDigestOptions, build_context_pack_report, build_session_review_budget_report,
    build_session_review_next_context_budget_report, build_traversal_graph,
    diff_digest_empty_message, diff_digest_mode_display, diff_digest_mode_label,
    diff_digest_status_label, diff_digest_summary_label, envelope_metric, format_compact_count,
    metric_digest_gate_label, metric_digest_trend_label, print_context_pack_human,
    print_json_or_envelope, print_session_review_budget_human,
    print_session_review_next_context_budget_human, render_log_digest_from_input,
    render_test_digest_from_input, shell_quote, to_json_schema, truncate_for_compact,
    verify_convex_projection_snapshot,
};

pub(crate) fn cmd_diff_digest(
    path: &Path,
    cached: bool,
    revision: Option<&str>,
    max_parsed_files: usize,
    format: OutputFormat,
) -> Result<()> {
    let parsed_files_cap = if max_parsed_files == 0 {
        None
    } else {
        Some(max_parsed_files)
    };
    let report = diff_digest::compute(
        path,
        diff_digest::DiffDigestOptions {
            cached,
            revision,
            max_parsed_files: parsed_files_cap,
        },
    )?;
    if format.json_output {
        println!(
            "{}",
            to_json_schema(
                &report,
                format.pretty,
                format.terse,
                format.ultra_terse,
                format.schema
            )?
        );
        return Ok(());
    }

    if report.files.is_empty() {
        println!("{}", diff_digest_empty_message(&report));
        return Ok(());
    }

    if format.compact {
        println!(
            "diff mode:{} files:{} summaries:{} syms:{} edges:+{}/-{}",
            diff_digest_mode_label(report.mode),
            report.files_changed,
            report.files_with_current_summaries,
            report.symbols_touched,
            report.call_edges_added,
            report.call_edges_removed
        );
        for file in &report.files {
            let symbols = if file.touched_symbols.is_empty() {
                "-".to_string()
            } else {
                truncate_for_compact(&file.touched_symbols.join(","), 60)
            };
            println!(
                "{} status:{} syms:{} sums:{} edges:+{}/-{}",
                file.path,
                diff_digest_status_label(file.status),
                symbols,
                diff_digest_summary_label(file.summary_state),
                file.added_call_edges.len(),
                file.removed_call_edges.len()
            );
        }
        return Ok(());
    }

    println!("Diff digest ({})", diff_digest_mode_display(&report));
    println!("  files changed:                 {}", report.files_changed);
    println!(
        "  files with current summaries: {}",
        report.files_with_current_summaries
    );
    println!("  touched symbols:              {}", report.symbols_touched);
    println!(
        "  call edges:                   +{} / -{}",
        report.call_edges_added, report.call_edges_removed
    );

    for file in &report.files {
        println!();
        println!("{} [{}]", file.path, diff_digest_status_label(file.status));
        if file.touched_symbols.is_empty() {
            println!("  touched symbols: none");
        } else {
            println!("  touched symbols: {}", file.touched_symbols.join(", "));
        }
        println!(
            "  cached summaries: {}",
            diff_digest_summary_label(file.summary_state)
        );
        for summary in &file.current_summaries {
            println!(
                "    - {}: {}",
                summary.symbol,
                truncate_for_compact(&summary.summary, 160)
            );
        }
        if !file.added_call_edges.is_empty() {
            println!("  call edges added:");
            for edge in &file.added_call_edges {
                println!("    - {}", edge);
            }
        }
        if !file.removed_call_edges.is_empty() {
            println!("  call edges removed:");
            for edge in &file.removed_call_edges {
                println!("    - {}", edge);
            }
        }
        for warning in &file.warnings {
            println!("  warning: {}", warning);
        }
    }

    Ok(())
}

pub(crate) fn cmd_test_digest(
    path: &Path,
    input_path: Option<&Path>,
    runner: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let input = match input_path {
        Some(file_path) => fs::read_to_string(file_path)
            .with_context(|| format!("reading test output: {}", file_path.display()))?,
        None => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("reading test output from stdin")?;
            buf
        }
    };
    if input.trim().is_empty() {
        bail!("no test output provided; pass --input <file> or pipe runner output on stdin");
    }

    render_test_digest_from_input(path, &input, runner, format)
}

pub(crate) fn cmd_log_digest(
    path: &Path,
    input_path: Option<&Path>,
    format: OutputFormat,
) -> Result<()> {
    let input = match input_path {
        Some(file_path) => fs::read_to_string(file_path)
            .with_context(|| format!("reading log output: {}", file_path.display()))?,
        None => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("reading log output from stdin")?;
            buf
        }
    };
    if input.trim().is_empty() {
        bail!("no log output provided; pass --input <file> or pipe log output on stdin");
    }

    render_log_digest_from_input(path, &input, format)
}

pub(crate) fn cmd_context_pack(
    path: &Path,
    test_input: Option<&Path>,
    runner: Option<&str>,
    log_input: Option<&Path>,
    format: OutputFormat,
    budget: ResponseBudget,
    convex_snapshot: Option<&Path>,
) -> Result<()> {
    if let Some(snapshot) = convex_snapshot {
        let root = lint::resolve_project_root_or_canonical_path(path)?;
        build_traversal_graph(&root, path, None)?;
        verify_convex_projection_snapshot(&root, None, snapshot)?;
    }
    let report = build_context_pack_report(path, test_input, runner, log_input, budget)?;
    if format.json_output {
        print_json_or_envelope(
            &report,
            &format,
            "context-pack",
            "handoff",
            ToolEnvelopeSummary {
                text: format!("context pack for {}", report.target),
                metrics: vec![
                    envelope_metric("prompt_targets", report.next_context.prompt_target_total),
                    envelope_metric("files_changed", report.diff_digest.files_changed),
                    envelope_metric("test", &report.test_digest.status),
                    envelope_metric("log", &report.log_digest.status),
                ],
            },
            report.next_context.truncated
                || report.diff_digest.truncated
                || report
                    .test_digest
                    .report
                    .as_ref()
                    .map(|entry| entry.truncated)
                    .unwrap_or(false)
                || report
                    .log_digest
                    .report
                    .as_ref()
                    .map(|entry| entry.truncated)
                    .unwrap_or(false),
            report.resume_commands.clone(),
        )?;
        return Ok(());
    }

    print_context_pack_human(&report, format.compact);
    Ok(())
}

pub(crate) fn cmd_metric_digest(
    options: MetricDigestOptions<'_>,
    format: OutputFormat,
) -> Result<()> {
    let input = match options.input_path {
        Some(file_path) => fs::read_to_string(file_path)
            .with_context(|| format!("reading metric input: {}", file_path.display()))?,
        None => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("reading metric input from stdin")?;
            buf
        }
    };
    if input.trim().is_empty() {
        bail!("no metric input provided; pass --input <file> or pipe JSON/NDJSON on stdin");
    }

    let baseline = match options.baseline_path {
        Some(file_path) => Some(
            fs::read_to_string(file_path)
                .with_context(|| format!("reading metric baseline: {}", file_path.display()))?,
        ),
        None => None,
    };

    let report = metric_digest::compute(
        &input,
        baseline.as_deref(),
        options.metrics,
        options.lower_is_better,
        options.higher_is_better,
        options.history,
        options.top,
    )?;
    if format.json_output {
        println!(
            "{}",
            to_json_schema(
                &report,
                format.pretty,
                format.terse,
                format.ultra_terse,
                format.schema
            )?
        );
        return Ok(());
    }

    if format.compact {
        let previous = report
            .previous_run
            .as_ref()
            .map(|run| run.label.as_str())
            .unwrap_or("-");
        println!(
            "metric runs:{} current:{} previous:{} metrics:{} imp:{} reg:{}",
            report.runs_loaded,
            report.current_run.label,
            previous,
            report.shared_metrics.max(report.current_run.metrics.len()),
            report.top_improvements.len(),
            report.top_regressions.len()
        );
        for delta in &report.metric_deltas {
            println!(
                "{} current:{} prev:{} delta:{} trend:{}",
                delta.metric,
                metric_digest::format_number(delta.current),
                metric_digest::format_number(delta.previous),
                metric_digest::format_number(delta.delta),
                metric_digest_trend_label(delta.trend)
            );
        }
        if let Some(gate) = &report.community_search_gate {
            println!(
                "community-search-gate decision:{} workloads:{} diagnostics:{}",
                metric_digest_gate_label(gate.decision),
                gate.workloads.len(),
                gate.diagnostics.len()
            );
        }
        for warning in &report.warnings {
            println!("warning: {warning}");
        }
        return Ok(());
    }

    println!("Metric digest");
    println!("  runs loaded:    {}", report.runs_loaded);
    println!("  current:        {}", report.current_run.label);
    match report.previous_run.as_ref() {
        Some(previous) => println!("  previous:       {}", previous.label),
        None => println!("  previous:       (none)"),
    }
    println!("  shared metrics: {}", report.shared_metrics);

    if report.metric_deltas.is_empty() {
        println!();
        println!("Current metrics:");
        for (metric, value) in &report.current_run.metrics {
            println!("  {}: {}", metric, metric_digest::format_number(*value));
        }
    } else {
        println!();
        println!("Current vs previous:");
        for delta in &report.metric_deltas {
            let percent = delta
                .percent_delta
                .map(|value| format!(", {value:+.2}%"))
                .unwrap_or_default();
            println!(
                "  {}: {} (prev {}, delta {:+}{}; {})",
                delta.metric,
                metric_digest::format_number(delta.current),
                metric_digest::format_number(delta.previous),
                metric_digest::format_number(delta.delta),
                percent,
                metric_digest_trend_label(delta.trend)
            );
        }
    }

    if !report.top_improvements.is_empty() {
        println!();
        println!("Top improvements:");
        for delta in &report.top_improvements {
            println!(
                "  {}: {} -> {}",
                delta.metric,
                metric_digest::format_number(delta.previous),
                metric_digest::format_number(delta.current)
            );
        }
    }

    if !report.top_regressions.is_empty() {
        println!();
        println!("Top regressions:");
        for delta in &report.top_regressions {
            println!(
                "  {}: {} -> {}",
                delta.metric,
                metric_digest::format_number(delta.previous),
                metric_digest::format_number(delta.current)
            );
        }
    }

    if !report.news_table_markdown.is_empty() {
        println!();
        println!("News-ready table:");
        println!("{}", report.news_table_markdown);
    }

    if let Some(gate) = &report.community_search_gate {
        println!();
        println!("Community search gate:");
        println!("  decision: {}", metric_digest_gate_label(gate.decision));
        println!(
            "  thresholds: duration +{:.1}%, handle coverage {:.1}%, duplicate precision {:.2}, top stability {:.2}",
            gate.max_duration_regression_percent,
            gate.min_handle_coverage_pct,
            gate.min_duplicate_name_precision,
            gate.min_top_community_stability
        );
        for workload in &gate.workloads {
            println!(
                "  {}: {}",
                workload.workload,
                metric_digest_gate_label(workload.status)
            );
            if let Some(duration) = workload.duration_micros {
                let regression = workload
                    .duration_regression_percent
                    .map(|value| format!(" ({value:+.2}%)"))
                    .unwrap_or_default();
                println!(
                    "    duration_micros: {}{}",
                    metric_digest::format_number(duration),
                    regression
                );
            }
            if let Some(coverage) = workload.handle_coverage_pct {
                println!(
                    "    handle_coverage_pct: {}",
                    metric_digest::format_number(coverage)
                );
            }
            if let Some(precision) = workload.duplicate_name_precision {
                println!(
                    "    duplicate_name_precision: {}",
                    metric_digest::format_number(precision)
                );
            }
            if let Some(stability) = workload.top_community_stability {
                println!(
                    "    top_community_stability: {}",
                    metric_digest::format_number(stability)
                );
            }
            for diagnostic in &workload.diagnostics {
                println!("    warning: {diagnostic}");
            }
        }
    }

    for warning in &report.warnings {
        println!("warning: {warning}");
    }
    Ok(())
}

pub(crate) fn cmd_session_digest(
    path: &Path,
    input_path: Option<&Path>,
    source: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let input = match input_path {
        Some(file_path) => fs::read_to_string(file_path)
            .with_context(|| format!("reading session transcript: {}", file_path.display()))?,
        None => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("reading session transcript from stdin")?;
            buf
        }
    };
    if input.trim().is_empty() {
        bail!("no session input provided; pass --input <file> or pipe transcript on stdin");
    }

    let report = session_digest::compute(path, &input, source)?;
    if format.json_output {
        println!(
            "{}",
            to_json_schema(
                &report,
                format.pretty,
                format.terse,
                format.ultra_terse,
                format.schema
            )?
        );
        return Ok(());
    }

    if format.compact {
        println!(
            "session src:{} prompts:{} cmds:{} files:{} syms:{} fails:{} runtime:{} churn:{} closeout:{}",
            report.source,
            report.prompt_target_count,
            report.command_groups,
            report.file_groups,
            report.symbol_groups,
            report.failure_groups,
            report.runtime_event_groups,
            report.restart_churn_groups,
            report.closeout_groups
        );
        for prompt in &report.prompt_targets {
            println!("prompt: {}", truncate_for_compact(prompt, 100));
        }
        for command in &report.commands {
            println!(
                "cmd count:{} {}",
                command.occurrences,
                truncate_for_compact(&command.command, 100)
            );
        }
        for failure in &report.failures {
            println!(
                "fail {} count:{} {}",
                failure.kind,
                failure.occurrences,
                truncate_for_compact(&failure.message, 100)
            );
        }
        for event in &report.runtime_events {
            println!(
                "runtime count:{} {}",
                event.occurrences,
                truncate_for_compact(&event.event, 100)
            );
        }
        for churn in &report.restart_churn {
            let suffix = churn
                .max_restart_count
                .map(|value| format!(" max_restart:{}", value))
                .unwrap_or_default();
            println!(
                "churn {} count:{}{} {}",
                churn.family,
                churn.occurrences,
                suffix,
                truncate_for_compact(&churn.sample, 100)
            );
        }
        for entry in &report.closeout {
            println!(
                "closeout {} count:{} {}",
                entry.kind,
                entry.occurrences,
                truncate_for_compact(&entry.detail, 100)
            );
        }
        for warning in &report.warnings {
            println!("warning: {warning}");
        }
        return Ok(());
    }

    println!("Session digest ({})", report.source);
    println!("  transcript items: {}", report.transcript_items);
    println!("  prompt targets:   {}", report.prompt_target_count);
    println!("  commands:         {}", report.command_groups);
    println!("  touched files:    {}", report.file_groups);
    println!("  touched symbols:  {}", report.symbol_groups);
    println!("  failures:         {}", report.failure_groups);
    println!("  runtime events:   {}", report.runtime_event_groups);
    println!("  restart churn:    {}", report.restart_churn_groups);
    println!("  closeout:         {}", report.closeout_groups);

    if !report.prompt_targets.is_empty() {
        println!();
        println!("Prompt targets:");
        for prompt in &report.prompt_targets {
            println!("  - {}", prompt);
        }
    }

    if !report.commands.is_empty() {
        println!();
        println!("Commands:");
        for command in &report.commands {
            println!("  - {} ({})", command.command, command.occurrences);
        }
    }

    if !report.touched_files.is_empty() {
        println!();
        println!("Touched files:");
        for path in &report.touched_files {
            println!("  - {} ({})", path.path, path.occurrences);
        }
    }

    if !report.touched_symbols.is_empty() {
        println!();
        println!("Touched symbols:");
        for symbol in &report.touched_symbols {
            println!("  - {} ({})", symbol.symbol, symbol.occurrences);
        }
    }

    if !report.failures.is_empty() {
        println!();
        println!("Failures:");
        for failure in &report.failures {
            println!(
                "  - [{}] {} ({})",
                failure.kind, failure.message, failure.occurrences
            );
        }
    }

    if !report.runtime_events.is_empty() {
        println!();
        println!("Runtime events:");
        for event in &report.runtime_events {
            println!("  - {} ({})", event.event, event.occurrences);
        }
    }

    if !report.restart_churn.is_empty() {
        println!();
        println!("Restart churn:");
        for churn in &report.restart_churn {
            match churn.max_restart_count {
                Some(max_restart_count) => println!(
                    "  - {} ({}) max_restart={} sample: {}",
                    churn.family, churn.occurrences, max_restart_count, churn.sample
                ),
                None => println!(
                    "  - {} ({}) sample: {}",
                    churn.family, churn.occurrences, churn.sample
                ),
            }
        }
    }

    if !report.closeout.is_empty() {
        println!();
        println!("Closeout evidence:");
        for entry in &report.closeout {
            println!(
                "  - [{}] {} ({})",
                entry.kind, entry.detail, entry.occurrences
            );
        }
    }

    for warning in &report.warnings {
        println!("warning: {warning}");
    }
    Ok(())
}

pub(crate) fn cmd_session_cost(
    input_path: Option<&Path>,
    source: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let input = match input_path {
        Some(file_path) => fs::read_to_string(file_path)
            .with_context(|| format!("reading session-cost input: {}", file_path.display()))?,
        None => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("reading session-cost input from stdin")?;
            buf
        }
    };
    if input.trim().is_empty() {
        bail!(
            "no session-cost input provided; pass --input <file> or pipe transcript/log data on stdin"
        );
    }

    let report = session_cost::compute(&input, source)?;
    if format.json_output {
        println!(
            "{}",
            to_json_schema(
                &report,
                format.pretty,
                format.terse,
                format.ultra_terse,
                format.schema
            )?
        );
        return Ok(());
    }

    if format.compact {
        let cache_ratio = report
            .cached_input_ratio
            .map(|value| format!("{value:.2}%"))
            .unwrap_or_else(|| "-".to_string());
        println!(
            "session-cost src:{} samples:{} prompt:{} cached:{} cache_ratio:{} output:{} total:{} runtime:{} churn:{} loops:{} file_reads:{}",
            report.source,
            report.usage_samples,
            format_compact_count(report.prompt_tokens),
            format_compact_count(report.cached_input_tokens),
            cache_ratio,
            format_compact_count(report.output_tokens),
            format_compact_count(report.total_tokens),
            report.total_runtime_events,
            report.restart_churn_groups,
            report.loop_clusters.len(),
            report.file_read_diagnostics.len()
        );
        for turn in &report.largest_turns {
            println!(
                "turn total:{} prompt:{} cached:{} output:{} label:{}",
                format_compact_count(turn.total_tokens),
                format_compact_count(turn.prompt_tokens),
                format_compact_count(turn.cached_input_tokens),
                format_compact_count(turn.output_tokens),
                truncate_for_compact(&turn.label, 72)
            );
        }
        for event in &report.runtime_events {
            println!("event count:{} {}", event.occurrences, event.event);
        }
        for churn in &report.restart_churn {
            let suffix = churn
                .max_restart_count
                .map(|value| format!(" max_restart:{}", value))
                .unwrap_or_default();
            println!(
                "churn {} count:{}{} {}",
                churn.family,
                churn.occurrences,
                suffix,
                truncate_for_compact(&churn.sample, 100)
            );
        }
        for cluster in &report.loop_clusters {
            println!(
                "loop {} count:{} streak:{} {}",
                cluster.kind,
                cluster.occurrences,
                cluster.max_consecutive,
                truncate_for_compact(&cluster.label, 100)
            );
        }
        for diagnostic in &report.file_read_diagnostics {
            println!(
                "file-read count:{} duplicate_tokens:{} range:{} path:{} follow_up:{}",
                diagnostic.occurrences,
                format_compact_count(diagnostic.duplicate_estimated_tokens),
                diagnostic.range,
                truncate_for_compact(&diagnostic.path, 80),
                diagnostic
                    .follow_up_commands
                    .iter()
                    .map(|command| truncate_for_compact(command, 100))
                    .collect::<Vec<_>>()
                    .join(" || ")
            );
        }
        if let Some(plan) = &report.prompt_cache_plan {
            let analytics = plan.analytics.as_ref();
            let trend = analytics
                .map(|analytics| analytics.trend.as_str())
                .unwrap_or("-");
            let average_ratio = analytics
                .and_then(|analytics| analytics.average_cached_input_ratio.as_deref())
                .unwrap_or("-");
            let net_cached_input_tokens = analytics
                .map(|analytics| {
                    if analytics.net_cached_input_tokens < 0 {
                        format!(
                            "-{}",
                            format_compact_count(analytics.net_cached_input_tokens.unsigned_abs())
                        )
                    } else {
                        format_compact_count(analytics.net_cached_input_tokens as u64)
                    }
                })
                .unwrap_or_else(|| "-".to_string());
            println!(
                "prompt-cache status:{} feasible:{} trend:{} avg_cache_ratio:{} net_cached:{} adapters:{} actions:{}",
                plan.status,
                plan.feasible,
                trend,
                average_ratio,
                net_cached_input_tokens,
                plan.provider_adapters.len(),
                plan.actions.len()
            );
            for action in &plan.actions {
                println!(
                    "prompt-cache-action {} {} {}",
                    action.severity,
                    action.kind,
                    truncate_for_compact(&action.message, 100)
                );
            }
        }
        for guardrail in &report.guardrails {
            println!(
                "guardrail {} {} {}",
                guardrail.severity,
                guardrail.kind,
                truncate_for_compact(&guardrail.message, 100)
            );
        }
        for warning in &report.warnings {
            println!("warning: {warning}");
        }
        return Ok(());
    }

    println!("Session cost digest ({})", report.source);
    println!("  records:                {}", report.record_count);
    println!("  usage samples:          {}", report.usage_samples);
    println!("  prompt tokens:          {}", report.prompt_tokens);
    println!("  cached input tokens:    {}", report.cached_input_tokens);
    println!(
        "  cache creation tokens:  {}",
        report.cache_creation_input_tokens
    );
    println!("  output tokens:          {}", report.output_tokens);
    println!(
        "  reasoning output:       {}",
        report.reasoning_output_tokens
    );
    println!("  total tokens:           {}", report.total_tokens);
    if let Some(ratio) = report.cached_input_ratio {
        println!("  cached input ratio:     {ratio:.2}%");
    }
    println!(
        "  largest turn total:     {}",
        report.largest_turn_total_tokens
    );
    println!("  runtime events:         {}", report.total_runtime_events);
    println!("  runtime groups:         {}", report.runtime_event_groups);
    println!("  restart churn groups:   {}", report.restart_churn_groups);
    println!("  loop clusters:          {}", report.loop_clusters.len());
    println!(
        "  repeated file reads:    {}",
        report.file_read_diagnostics.len()
    );
    if let Some(max_restart_count) = report.max_restart_count {
        println!("  max restart count:      {}", max_restart_count);
    }

    if !report.largest_turns.is_empty() {
        println!();
        println!("Largest turns:");
        for turn in &report.largest_turns {
            println!(
                "  - {}: total {} | prompt {} | cached {} | output {} | reasoning {}",
                turn.label,
                turn.total_tokens,
                turn.prompt_tokens,
                turn.cached_input_tokens,
                turn.output_tokens,
                turn.reasoning_output_tokens
            );
        }
    }

    if !report.runtime_events.is_empty() {
        println!();
        println!("Runtime churn:");
        for event in &report.runtime_events {
            println!("  - {} ({})", event.event, event.occurrences);
        }
    }

    if !report.restart_churn.is_empty() {
        println!();
        println!("Restart churn:");
        for churn in &report.restart_churn {
            match churn.max_restart_count {
                Some(max_restart_count) => println!(
                    "  - {} ({}) max_restart={} sample: {}",
                    churn.family, churn.occurrences, max_restart_count, churn.sample
                ),
                None => println!(
                    "  - {} ({}) sample: {}",
                    churn.family, churn.occurrences, churn.sample
                ),
            }
        }
    }

    if !report.loop_clusters.is_empty() {
        println!();
        println!("Loop clusters:");
        for cluster in &report.loop_clusters {
            println!(
                "  - [{}] {} ({}) max_consecutive={}",
                cluster.kind, cluster.label, cluster.occurrences, cluster.max_consecutive
            );
        }
    }

    if !report.file_read_diagnostics.is_empty() {
        println!();
        println!("Repeated file reads:");
        for diagnostic in &report.file_read_diagnostics {
            println!(
                "  - {} {} ({}) duplicate tokens ~{}",
                diagnostic.path,
                diagnostic.range,
                diagnostic.occurrences,
                diagnostic.duplicate_estimated_tokens
            );
            for command in &diagnostic.follow_up_commands {
                println!("    follow-up: {command}");
            }
        }
    }

    if let Some(plan) = &report.prompt_cache_plan {
        println!();
        println!("Prompt cache plan:");
        println!(
            "  - status: {} | feasible: {} | cached: {} | creation: {}",
            plan.status,
            plan.feasible,
            plan.observed_cached_input_tokens,
            plan.observed_cache_creation_tokens
        );
        if let Some(ratio) = &plan.observed_cached_input_ratio {
            println!("  - observed ratio: {ratio}");
        }
        if let Some(analytics) = &plan.analytics {
            let average_ratio = analytics
                .average_cached_input_ratio
                .as_deref()
                .unwrap_or("-");
            let first_ratio = analytics.first_cached_input_ratio.as_deref().unwrap_or("-");
            let last_ratio = analytics.last_cached_input_ratio.as_deref().unwrap_or("-");
            let delta = analytics.cached_input_ratio_delta.as_deref().unwrap_or("-");
            let read_to_creation = analytics
                .cache_read_to_creation_ratio
                .as_deref()
                .unwrap_or("-");
            println!(
                "  - analytics: samples={} trend={} effective={} avg={} first={} last={} delta={} net_cached={} read/create={}",
                analytics.sample_count,
                analytics.trend,
                analytics.effective,
                average_ratio,
                first_ratio,
                last_ratio,
                delta,
                analytics.net_cached_input_tokens,
                read_to_creation
            );
            for turn in &analytics.timeline {
                let cache_ratio = turn.cached_input_ratio.as_deref().unwrap_or("-");
                let creation_ratio = turn.cache_creation_ratio.as_deref().unwrap_or("-");
                println!(
                    "    timeline {}: prompt={} cached={} creation={} cache_ratio={} creation_ratio={}",
                    turn.label,
                    turn.prompt_tokens,
                    turn.cached_input_tokens,
                    turn.cache_creation_input_tokens,
                    cache_ratio,
                    creation_ratio
                );
            }
            if analytics.timeline_truncated {
                println!("    timeline truncated to first and latest samples");
            }
        }
        for adapter in &plan.provider_adapters {
            println!(
                "  - provider {}: {} ({})",
                adapter.provider,
                adapter.status,
                adapter.requirements.join("; ")
            );
        }
        for action in &plan.actions {
            println!(
                "  - {} {}: {} ({})",
                action.severity, action.kind, action.message, action.guidance
            );
        }
    }

    if !report.guardrails.is_empty() {
        println!();
        println!("Guardrails:");
        for guardrail in &report.guardrails {
            println!(
                "  - [{}:{}] {} | guidance: {}",
                guardrail.severity, guardrail.kind, guardrail.message, guardrail.guidance
            );
        }
    }

    for warning in &report.warnings {
        println!("warning: {warning}");
    }
    Ok(())
}

#[allow(dead_code)]
pub(crate) fn cmd_session_review(
    path: &Path,
    next_context: bool,
    format: OutputFormat,
) -> Result<()> {
    cmd_session_review_with_budget(path, next_context, format, ResponseBudget::default())
}

pub(crate) fn cmd_session_review_with_budget(
    path: &Path,
    next_context: bool,
    format: OutputFormat,
    budget: ResponseBudget,
) -> Result<()> {
    let report = session_review::compute(path)?;
    if budget.is_active() {
        if next_context {
            let budget_report =
                build_session_review_next_context_budget_report(&report, budget, None);
            if format.json_output {
                print_json_or_envelope(
                    &budget_report,
                    &format,
                    "session-review",
                    "next-context-preview",
                    ToolEnvelopeSummary {
                        text: format!("next-context preview for {}", budget_report.target),
                        metrics: vec![
                            envelope_metric("prompt_targets", budget_report.prompt_target_total),
                            envelope_metric("files", budget_report.touched_file_total),
                            envelope_metric("symbols", budget_report.touched_symbol_total),
                            envelope_metric("failures", budget_report.unresolved_failure_total),
                        ],
                    },
                    budget_report.truncated,
                    budget_report.next_digest_commands.clone(),
                )?;
            } else {
                print_session_review_next_context_budget_human(&budget_report);
            }
        } else {
            let budget_report = build_session_review_budget_report(&report, budget);
            if format.json_output {
                let mut follow_up = vec![format!(
                    "tsift session-review {} --next-context --json",
                    shell_quote(&budget_report.target)
                )];
                if let Some(session) = budget_report.sessions.first() {
                    follow_up.push(session.expand.clone());
                }
                print_json_or_envelope(
                    &budget_report,
                    &format,
                    "session-review",
                    "preview",
                    ToolEnvelopeSummary {
                        text: format!("session review preview for {}", budget_report.target),
                        metrics: vec![
                            envelope_metric("sessions", budget_report.sessions_matched),
                            envelope_metric("prompt_targets", budget_report.prompt_targets.len()),
                            envelope_metric("failures", budget_report.failures.len()),
                            envelope_metric("total_tokens", budget_report.total_tokens),
                        ],
                    },
                    budget_report.truncated,
                    follow_up,
                )?;
            } else {
                print_session_review_budget_human(&budget_report);
            }
        }
        return Ok(());
    }
    if next_context {
        if format.json_output {
            print_json_or_envelope(
                &report.next_context,
                &format,
                "session-review",
                "next-context",
                ToolEnvelopeSummary {
                    text: format!("next-context for {}", report.next_context.target),
                    metrics: vec![
                        envelope_metric(
                            "prompt_targets",
                            report.next_context.active_prompt_targets.len(),
                        ),
                        envelope_metric("files", report.next_context.touched_files.len()),
                        envelope_metric("symbols", report.next_context.touched_symbols.len()),
                        envelope_metric("failures", report.next_context.unresolved_failures.len()),
                    ],
                },
                false,
                report.next_context.next_digest_commands.clone(),
            )?;
            return Ok(());
        }

        println!("Next context");
        println!("  target:                 {}", report.next_context.target);
        println!(
            "  prompt targets:         {}",
            report.next_context.active_prompt_targets.len()
        );
        println!(
            "  touched files:          {}",
            report.next_context.touched_files.len()
        );
        println!(
            "  touched symbols:        {}",
            report.next_context.touched_symbols.len()
        );
        println!(
            "  unresolved failures:    {}",
            report.next_context.unresolved_failures.len()
        );
        println!(
            "  last verification:      {}",
            report.next_context.last_verification.status
        );
        println!(
            "  verification detail:    {}",
            report.next_context.last_verification.detail
        );

        if !report.next_context.active_prompt_targets.is_empty() {
            println!();
            println!("Active prompt targets:");
            for prompt in &report.next_context.active_prompt_targets {
                println!("  - {}", prompt);
            }
        }

        if !report.next_context.touched_files.is_empty() {
            println!();
            println!("Touched files:");
            for path in &report.next_context.touched_files {
                println!("  - {}", path);
            }
        }

        if !report.next_context.touched_symbols.is_empty() {
            println!();
            println!("Touched symbols:");
            for symbol in &report.next_context.touched_symbols {
                println!("  - {}", symbol);
            }
        }

        if !report.next_context.unresolved_failures.is_empty() {
            println!();
            println!("Unresolved failures:");
            for failure in &report.next_context.unresolved_failures {
                println!(
                    "  - [{}] {} ({}){}{}",
                    failure.kind,
                    failure.message,
                    failure.occurrences,
                    failure
                        .command
                        .as_ref()
                        .map(|command| format!(" command: {command}"))
                        .unwrap_or_default(),
                    failure
                        .session_path
                        .as_ref()
                        .map(|path| format!(" session: {path}"))
                        .unwrap_or_default()
                );
            }
        }

        println!();
        println!("Next digest commands:");
        for command in &report.next_context.next_digest_commands {
            println!("  - {}", command);
        }
        return Ok(());
    }

    if format.json_output {
        let mut follow_up = vec![format!(
            "tsift session-review {} --next-context --json",
            shell_quote(&report.target)
        )];
        follow_up.extend(report.next_context.next_digest_commands.clone());
        print_json_or_envelope(
            &report,
            &format,
            "session-review",
            "report",
            ToolEnvelopeSummary {
                text: format!("session review for {}", report.target),
                metrics: vec![
                    envelope_metric("sessions", report.sessions_matched),
                    envelope_metric("prompt_targets", report.prompt_target_count),
                    envelope_metric("failures", report.failure_groups),
                    envelope_metric("file_reads", report.file_read_diagnostics.len()),
                    envelope_metric("aggregate_total_tokens", report.total_tokens),
                    envelope_metric(
                        "latest_session_total_tokens",
                        report
                            .latest_session_cost
                            .as_ref()
                            .map_or(0, |cost| cost.total_tokens),
                    ),
                ],
            },
            false,
            follow_up,
        )?;
        return Ok(());
    }

    if format.compact {
        let cache_ratio = report
            .cached_input_ratio
            .map(|value| format!("{value:.2}%"))
            .unwrap_or_else(|| "-".to_string());
        let latest_total = report
            .latest_session_cost
            .as_ref()
            .map(|cost| format_compact_count(cost.total_tokens))
            .unwrap_or_else(|| "-".to_string());
        let latest_largest_turn = report
            .latest_session_cost
            .as_ref()
            .map(|cost| format_compact_count(cost.largest_turn_total_tokens))
            .unwrap_or_else(|| "-".to_string());
        println!(
            "session-review target:{} kind:{} matched:{} claude:{} codex:{} agent_doc:{} aggregate_prompt:{} aggregate_cached:{} aggregate_cache_ratio:{} aggregate_output:{} aggregate_total:{} latest_total:{} latest_largest_turn:{} loops:{} file_reads:{}",
            report.target,
            report.target_kind,
            report.sessions_matched,
            report.claude_sessions,
            report.codex_sessions,
            report.agent_doc_logs,
            format_compact_count(report.prompt_tokens),
            format_compact_count(report.cached_input_tokens),
            cache_ratio,
            format_compact_count(report.output_tokens),
            format_compact_count(report.total_tokens),
            latest_total,
            latest_largest_turn,
            report.loop_clusters.len(),
            report.file_read_diagnostics.len()
        );
        for session in &report.sessions {
            println!(
                "session {} total:{} largest_turn:{} prompts:{} fails:{} matched_by:{} path:{}",
                session.source,
                format_compact_count(session.total_tokens),
                format_compact_count(session.largest_turn_total_tokens),
                session.prompt_target_count,
                session.failure_groups,
                session.matched_by.join(","),
                truncate_for_compact(&session.path, 96)
            );
        }
        for prompt in &report.prompt_targets {
            println!(
                "prompt count:{} {}",
                prompt.occurrences,
                truncate_for_compact(&prompt.text, 100)
            );
        }
        for failure in &report.failures {
            println!(
                "fail {} count:{} {}{}{}",
                failure.kind,
                failure.occurrences,
                truncate_for_compact(&failure.message, 100),
                failure
                    .command
                    .as_ref()
                    .map(|command| format!(" command:{}", truncate_for_compact(command, 80)))
                    .unwrap_or_default(),
                failure
                    .session_path
                    .as_ref()
                    .map(|path| format!(" session:{}", truncate_for_compact(path, 80)))
                    .unwrap_or_default()
            );
        }
        for cluster in &report.loop_clusters {
            println!(
                "loop {} count:{} streak:{} {}",
                cluster.kind,
                cluster.occurrences,
                cluster.max_consecutive,
                truncate_for_compact(&cluster.label, 100)
            );
        }
        for diagnostic in &report.file_read_diagnostics {
            println!(
                "file-read count:{} duplicate_tokens:{} range:{} path:{} follow_up:{}",
                diagnostic.occurrences,
                format_compact_count(diagnostic.duplicate_estimated_tokens),
                diagnostic.range,
                truncate_for_compact(&diagnostic.path, 80),
                diagnostic
                    .follow_up_commands
                    .iter()
                    .map(|command| truncate_for_compact(command, 100))
                    .collect::<Vec<_>>()
                    .join(" || ")
            );
        }
        for guardrail in &report.guardrails {
            println!(
                "guardrail {} {} {}",
                guardrail.severity,
                guardrail.kind,
                truncate_for_compact(&guardrail.message, 100)
            );
        }
        for warning in &report.warnings {
            println!("warning: {warning}");
        }
        return Ok(());
    }

    println!("Session review ({})", report.target_kind);
    println!("  root:                   {}", report.root);
    println!("  target:                 {}", report.target);
    println!("  sessions considered:    {}", report.sessions_considered);
    println!("  sessions matched:       {}", report.sessions_matched);
    println!("  Claude sessions:        {}", report.claude_sessions);
    println!("  Codex sessions:         {}", report.codex_sessions);
    println!("  agent-doc logs:         {}", report.agent_doc_logs);
    println!("  prompt targets:         {}", report.prompt_target_count);
    println!("  commands:               {}", report.command_groups);
    println!("  touched files:          {}", report.file_groups);
    println!("  touched symbols:        {}", report.symbol_groups);
    println!("  failures:               {}", report.failure_groups);
    println!("  runtime events:         {}", report.runtime_event_groups);
    println!("  restart churn:          {}", report.restart_churn_groups);
    println!("  closeout:               {}", report.closeout_groups);
    println!("  loop clusters:          {}", report.loop_clusters.len());
    println!(
        "  repeated file reads:    {}",
        report.file_read_diagnostics.len()
    );
    println!("  aggregate usage samples: {}", report.usage_samples);
    println!("  aggregate prompt tokens: {}", report.prompt_tokens);
    println!("  aggregate cached input: {}", report.cached_input_tokens);
    println!(
        "  aggregate cache create: {}",
        report.cache_creation_input_tokens
    );
    println!("  aggregate output tokens: {}", report.output_tokens);
    println!(
        "  aggregate reasoning out: {}",
        report.reasoning_output_tokens
    );
    println!("  aggregate total tokens: {}", report.total_tokens);
    if let Some(ratio) = report.cached_input_ratio {
        println!("  aggregate cache ratio:  {ratio:.2}%");
    }
    println!(
        "  aggregate largest turn: {}",
        report.largest_turn_total_tokens
    );
    if let Some(latest) = &report.latest_session_cost {
        println!("  latest session tokens:  {}", latest.total_tokens);
        println!(
            "  latest session largest: {}",
            latest.largest_turn_total_tokens
        );
    }

    if !report.sessions.is_empty() {
        println!();
        println!("Matched sessions:");
        for session in &report.sessions {
            println!(
                "  - [{}] {} | total {} | largest turn {} | prompts {} | failures {} | matched by {}",
                session.source,
                session.path,
                session.total_tokens,
                session.largest_turn_total_tokens,
                session.prompt_target_count,
                session.failure_groups,
                session.matched_by.join(", ")
            );
        }
    }

    if !report.prompt_targets.is_empty() {
        println!();
        println!("Prompt targets:");
        for prompt in &report.prompt_targets {
            println!("  - {} ({})", prompt.text, prompt.occurrences);
        }
    }

    if !report.commands.is_empty() {
        println!();
        println!("Commands:");
        for command in &report.commands {
            println!("  - {} ({})", command.command, command.occurrences);
        }
    }

    if !report.failures.is_empty() {
        println!();
        println!("Failures:");
        for failure in &report.failures {
            println!(
                "  - [{}] {} ({}){}{}",
                failure.kind,
                failure.message,
                failure.occurrences,
                failure
                    .command
                    .as_ref()
                    .map(|command| format!(" command: {command}"))
                    .unwrap_or_default(),
                failure
                    .session_path
                    .as_ref()
                    .map(|path| format!(" session: {path}"))
                    .unwrap_or_default()
            );
        }
    }

    if !report.restart_churn.is_empty() {
        println!();
        println!("Restart churn:");
        for churn in &report.restart_churn {
            match churn.max_restart_count {
                Some(max_restart_count) => println!(
                    "  - {} ({}) max_restart={} sample: {}",
                    churn.family, churn.occurrences, max_restart_count, churn.sample
                ),
                None => println!(
                    "  - {} ({}) sample: {}",
                    churn.family, churn.occurrences, churn.sample
                ),
            }
        }
    }

    if !report.closeout.is_empty() {
        println!();
        println!("Closeout evidence:");
        for entry in &report.closeout {
            println!(
                "  - [{}] {} ({})",
                entry.kind, entry.detail, entry.occurrences
            );
        }
    }

    if !report.loop_clusters.is_empty() {
        println!();
        println!("Loop clusters:");
        for cluster in &report.loop_clusters {
            println!(
                "  - [{}] {} ({}) max_consecutive={}",
                cluster.kind, cluster.label, cluster.occurrences, cluster.max_consecutive
            );
        }
    }

    if !report.file_read_diagnostics.is_empty() {
        println!();
        println!("Repeated file reads:");
        for diagnostic in &report.file_read_diagnostics {
            println!(
                "  - {} {} ({}) duplicate tokens ~{}",
                diagnostic.path,
                diagnostic.range,
                diagnostic.occurrences,
                diagnostic.duplicate_estimated_tokens
            );
            for command in &diagnostic.follow_up_commands {
                println!("    follow-up: {command}");
            }
        }
    }

    if !report.largest_turns.is_empty() {
        println!();
        println!("Largest turns:");
        for turn in &report.largest_turns {
            println!(
                "  - [{}] {}: total {} | prompt {} | cached {} | output {} | reasoning {}",
                turn.source,
                turn.label,
                turn.total_tokens,
                turn.prompt_tokens,
                turn.cached_input_tokens,
                turn.output_tokens,
                turn.reasoning_output_tokens
            );
        }
    }

    if !report.guardrails.is_empty() {
        println!();
        println!("Guardrails:");
        for guardrail in &report.guardrails {
            println!(
                "  - [{}:{}] {} | guidance: {}",
                guardrail.severity, guardrail.kind, guardrail.message, guardrail.guidance
            );
        }
    }

    for warning in &report.warnings {
        println!("warning: {warning}");
    }
    Ok(())
}
