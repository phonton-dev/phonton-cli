//! Active-panel focus surfaces.
//!
//! This module keeps the receipt/code/problems/commands/log projections out
//! of the TUI control loop. It owns only rendering helpers and pure text
//! payload builders; state transitions stay in `main.rs`.

use std::path::PathBuf;

use phonton_types::{DiffHunk, DiffLine, OrchestratorEvent, TaskStatus, VerifyLayer};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::command_runner::CommandRunSummary;
use crate::tui_commands::FocusView;
use crate::{short, GoalEntry, ACCENT, ACCENT_HI, BG_DEEP, DANGER, MUTED, SUCCESS, WARN};

pub(crate) fn append_focus_tabs(lines: &mut Vec<Line<'static>>, active: FocusView) {
    lines.push(Line::raw(""));
    let tabs = [
        FocusView::Plan,
        FocusView::Receipt,
        FocusView::Problems,
        FocusView::Code,
        FocusView::Commands,
        FocusView::Context,
        FocusView::Tokens,
        FocusView::Log,
    ];
    let mut spans = vec![Span::styled(
        "Focus ",
        Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
    )];
    for (idx, view) in tabs.iter().copied().enumerate() {
        let selected = view == active;
        let style = if selected {
            Style::default()
                .fg(BG_DEEP)
                .bg(ACCENT_HI)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(MUTED)
        };
        spans.push(Span::styled(format!(" {} ", view.as_str()), style));
        if idx + 1 < tabs.len() {
            spans.push(Span::styled(" | ", Style::default().fg(MUTED)));
        } else {
            spans.push(Span::raw("  "));
        }
    }
    let hint = match active {
        FocusView::Plan => "contract  verify plan",
        FocusView::Receipt => "f cycle  d diff",
        FocusView::Problems => "p problems  r retry",
        FocusView::Code => "[ ] file  PgUp/PgDn",
        FocusView::Commands => "[ ] command  /rerun",
        FocusView::Context => "/context  /memory",
        FocusView::Tokens => "/why-tokens",
        FocusView::Log => "PgUp/PgDn  End tail",
    };
    spans.push(Span::styled(hint, Style::default().fg(MUTED)));
    lines.push(Line::from(spans));
}

pub(crate) fn focused_file_count(goal: &GoalEntry) -> usize {
    let groups = diff_hunks_by_file(goal);
    if !groups.is_empty() {
        groups.len()
    } else {
        goal.state
            .as_ref()
            .and_then(|state| state.handoff_packet.as_ref())
            .map(|handoff| handoff.changed_files.len())
            .unwrap_or(0)
    }
}

fn diff_hunks_by_file(goal: &GoalEntry) -> Vec<(PathBuf, Vec<DiffHunk>)> {
    let mut groups: Vec<(PathBuf, Vec<DiffHunk>)> = Vec::new();
    for record in &goal.flight_log {
        if let OrchestratorEvent::SubtaskReviewReady { diff_hunks, .. } = &record.event {
            for hunk in diff_hunks {
                if let Some((_, hunks)) =
                    groups.iter_mut().find(|(path, _)| path == &hunk.file_path)
                {
                    hunks.push(hunk.clone());
                } else {
                    groups.push((hunk.file_path.clone(), vec![hunk.clone()]));
                }
            }
        }
    }
    groups
}

pub(crate) fn append_code_focus_lines(
    lines: &mut Vec<Line<'static>>,
    goal: &GoalEntry,
    selected_file: usize,
) {
    for line in code_focus_text(goal, selected_file).lines() {
        lines.push(Line::from(Span::styled(
            line.to_string(),
            code_focus_line_style(line),
        )));
    }
}

fn code_focus_line_style(line: &str) -> Style {
    if line.starts_with("@@") {
        return Style::default().fg(ACCENT_HI).add_modifier(Modifier::BOLD);
    }
    if line.starts_with("Code ") || line.starts_with("file:") {
        return Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
    }
    if line.starts_with('-') {
        return Style::default().fg(DANGER);
    }
    if let Some(rest) = line.strip_prefix('+') {
        let trimmed = rest.trim_start();
        if trimmed.starts_with('#') || trimmed.starts_with("//") || trimmed.starts_with("/*") {
            return Style::default().fg(MUTED);
        }
        if trimmed.starts_with('"') || trimmed.starts_with('\'') || trimmed.contains(" = \"") {
            return Style::default().fg(WARN);
        }
        if trimmed.starts_with("fn ")
            || trimmed.starts_with("pub ")
            || trimmed.starts_with("class ")
            || trimmed.starts_with("def ")
            || trimmed.starts_with("function ")
            || trimmed.starts_with("const ")
            || trimmed.starts_with("let ")
            || trimmed.starts_with("var ")
            || trimmed.starts_with("import ")
            || trimmed.starts_with("from ")
            || trimmed.starts_with("use ")
        {
            return Style::default().fg(ACCENT_HI);
        }
        return Style::default().fg(SUCCESS);
    }
    if line.starts_with(' ') {
        return Style::default().fg(MUTED);
    }
    Style::default().fg(Color::White)
}

pub(crate) fn code_focus_text(goal: &GoalEntry, selected_file: usize) -> String {
    let groups = diff_hunks_by_file(goal);
    if let Some((path, hunks)) = groups.get(selected_file.min(groups.len().saturating_sub(1))) {
        let (total_added, total_removed) = diff_counts(groups.iter().flat_map(|(_, hunks)| hunks));
        let (file_added, file_removed) = diff_counts(hunks.iter());
        let mut out = format!(
            "Code {}/{}  files {}  +{} -{}\nfile: {}  hunks {}  +{} -{}\n",
            selected_file.min(groups.len().saturating_sub(1)) + 1,
            groups.len(),
            groups.len(),
            total_added,
            total_removed,
            path.display(),
            hunks.len(),
            file_added,
            file_removed
        );
        for hunk in hunks.iter().take(4) {
            out.push_str(&format!(
                "@@ -{},{} +{},{} @@\n",
                hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
            ));
            for line in hunk.lines.iter().take(40) {
                match line {
                    DiffLine::Context(text) => {
                        out.push(' ');
                        out.push_str(text);
                    }
                    DiffLine::Added(text) => {
                        out.push('+');
                        out.push_str(text);
                    }
                    DiffLine::Removed(text) => {
                        out.push('-');
                        out.push_str(text);
                    }
                }
                out.push('\n');
            }
            if hunk.lines.len() > 40 {
                out.push_str(&format!(
                    "... {} more hunk line(s)\n",
                    hunk.lines.len() - 40
                ));
            }
        }
        return out;
    }

    if let Some(handoff) = goal
        .state
        .as_ref()
        .and_then(|state| state.handoff_packet.as_ref())
    {
        let mut out = String::from("Code\n");
        if handoff.changed_files.is_empty() {
            out.push_str("No changed files recorded yet.");
        } else {
            for file in &handoff.changed_files {
                out.push_str(&format!(
                    "- {} +{} -{} {}\n",
                    file.path.display(),
                    file.added_lines,
                    file.removed_lines,
                    file.summary
                ));
            }
        }
        return out;
    }

    "Code\nNo diff hunks recorded yet.".into()
}

fn diff_counts<'a>(hunks: impl Iterator<Item = &'a DiffHunk>) -> (usize, usize) {
    let mut added = 0;
    let mut removed = 0;
    for hunk in hunks {
        for line in &hunk.lines {
            match line {
                DiffLine::Added(_) => added += 1,
                DiffLine::Removed(_) => removed += 1,
                DiffLine::Context(_) => {}
            }
        }
    }
    (added, removed)
}

pub(crate) fn append_problems_focus_lines(
    lines: &mut Vec<Line<'static>>,
    goal: &GoalEntry,
    selected_file: usize,
) {
    for line in problems_focus_text(goal, selected_file).lines() {
        let style = if line.starts_with("fail") || line.starts_with("error") {
            Style::default().fg(DANGER).add_modifier(Modifier::BOLD)
        } else if line.starts_with("- [") || line.starts_with("- layer") {
            Style::default().fg(WARN)
        } else if line.starts_with('+') {
            Style::default().fg(SUCCESS)
        } else if line.starts_with("@@") || line.starts_with("file:") {
            Style::default().fg(ACCENT_HI)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(Span::styled(line.to_string(), style)));
    }
}

pub(crate) fn problems_focus_text(goal: &GoalEntry, selected_file: usize) -> String {
    let mut out = String::from("Problems\n");
    let diagnostics = problem_diagnostics(goal);
    if diagnostics.is_empty() {
        out.push_str("No verifier problems recorded for this goal.");
        return out;
    }
    out.push_str(&format!("failure type: {}\n", goal_failure_kind(goal)));
    for item in diagnostics.iter().take(10) {
        out.push_str(&format!("- {item}\n"));
    }
    if diagnostics.len() > 10 {
        out.push_str(&format!(
            "... {} more diagnostic(s)\n",
            diagnostics.len() - 10
        ));
    }
    if let Some(note) = token_note(goal) {
        out.push_str(&format!("\nTokens\n{note}\n"));
    }
    if let Some(note) = routing_note(goal) {
        out.push_str(&format!("\nRouting\n{note}\n"));
    }
    out.push_str("\nRepair\n- Press r or run /retry to queue a repair with compact diagnostics.\n- Use /why-tokens to inspect retry/context token buckets.\n");

    let groups = diff_hunks_by_file(goal);
    let selected_group = problem_excerpt_index(&diagnostics, &groups, selected_file);
    if let Some((path, hunks)) = groups.get(selected_group) {
        out.push_str(&format!(
            "\nChanged excerpt {}/{}\nfile: {}\n",
            selected_group + 1,
            groups.len(),
            path.display()
        ));
        for hunk in hunks.iter().take(1) {
            out.push_str(&format!(
                "@@ -{},{} +{},{} @@\n",
                hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
            ));
            for line in hunk.lines.iter().take(12) {
                match line {
                    DiffLine::Context(text) => {
                        out.push(' ');
                        out.push_str(text);
                    }
                    DiffLine::Added(text) => {
                        out.push('+');
                        out.push_str(text);
                    }
                    DiffLine::Removed(text) => {
                        out.push('-');
                        out.push_str(text);
                    }
                }
                out.push('\n');
            }
        }
    }
    out
}

fn problem_excerpt_index(
    diagnostics: &[String],
    groups: &[(PathBuf, Vec<DiffHunk>)],
    selected_file: usize,
) -> usize {
    if groups.is_empty() {
        return 0;
    }
    let diagnostic_text = diagnostics
        .iter()
        .map(|item| item.replace('\\', "/").to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join("\n");
    if let Some((idx, _)) = groups.iter().enumerate().find(|(_, (path, _))| {
        diagnostic_text.contains(
            &path
                .to_string_lossy()
                .replace('\\', "/")
                .to_ascii_lowercase(),
        )
    }) {
        return idx;
    }
    selected_file.min(groups.len().saturating_sub(1))
}

pub(crate) fn problem_diagnostics(goal: &GoalEntry) -> Vec<String> {
    let mut items = Vec::new();
    for record in &goal.flight_log {
        match &record.event {
            OrchestratorEvent::VerifyFail {
                layer,
                errors,
                attempt,
                ..
            } => {
                if errors.is_empty() {
                    items.push(format!("layer {layer:?} failed on attempt {attempt}"));
                } else {
                    for error in errors {
                        items.push(format!(
                            "[{}] {}",
                            format!("{layer:?}").to_ascii_lowercase(),
                            short(error, 220)
                        ));
                    }
                }
            }
            OrchestratorEvent::SubtaskFailed {
                reason, attempt, ..
            } => items.push(format!(
                "subtask failed on attempt {attempt}: {}",
                short(reason, 220)
            )),
            _ => {}
        }
    }
    if let TaskStatus::Failed { reason, .. } = &goal.status {
        if !items.iter().any(|item| item.contains(reason)) {
            items.push(format!("task failed: {}", short(reason, 220)));
        }
    }
    items
}

pub(crate) fn compact_problem_diagnostics(goal: &GoalEntry, max_items: usize) -> String {
    let diagnostics = problem_diagnostics(goal);
    if diagnostics.is_empty() {
        return "- no explicit verifier diagnostic was recorded".into();
    }
    diagnostics
        .iter()
        .take(max_items)
        .map(|item| format!("- {item}"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn goal_failure_kind(goal: &GoalEntry) -> &'static str {
    for record in goal.flight_log.iter().rev() {
        match &record.event {
            OrchestratorEvent::VerifyFail { layer, errors, .. } => {
                if matches!(layer, VerifyLayer::Syntax) {
                    return "syntax";
                }
                if errors.iter().any(|error| {
                    let lower = error.to_ascii_lowercase();
                    lower.contains("quality") || lower.contains("gate")
                }) {
                    return "quality";
                }
                return "verify";
            }
            OrchestratorEvent::SubtaskFailed { reason, .. } => {
                let lower = reason.to_ascii_lowercase();
                if lower.contains("provider") || lower.contains("dispatch") {
                    return "provider";
                }
                if lower.contains("command") || lower.contains("exit") {
                    return "command";
                }
                if lower.contains("quality") || lower.contains("gate") {
                    return "quality";
                }
            }
            _ => {}
        }
    }
    if let TaskStatus::Failed { reason, .. } = &goal.status {
        let lower = reason.to_ascii_lowercase();
        if lower.contains("syntax") {
            "syntax"
        } else if lower.contains("quality") || lower.contains("gate") {
            "quality"
        } else if lower.contains("provider") || lower.contains("dispatch") {
            "provider"
        } else if lower.contains("command") || lower.contains("exit") {
            "command"
        } else {
            "failed"
        }
    } else {
        "none"
    }
}

pub(crate) fn append_log_focus_lines(lines: &mut Vec<Line<'static>>, goal: &GoalEntry) {
    for line in log_focus_text(goal).lines() {
        lines.push(Line::from(Span::styled(
            line.to_string(),
            Style::default().fg(MUTED),
        )));
    }
}

pub(crate) fn receipt_focus_text(goal: &GoalEntry) -> String {
    let Some(state) = &goal.state else {
        return format!(
            "Receipt\ngoal: {}\nNo state snapshot yet.",
            goal.description
        );
    };
    let Some(handoff) = &state.handoff_packet else {
        return format!(
            "Receipt\ngoal: {}\nNo handoff packet yet.",
            goal.description
        );
    };
    let mut out = format!(
        "Receipt\n{}\n{}\n",
        handoff.headline,
        deterministic_summary(handoff)
    );
    out.push_str(&format!(
        "files: {} +{} -{}\n",
        handoff.diff_stats.files_changed,
        handoff.diff_stats.added_lines,
        handoff.diff_stats.removed_lines
    ));
    for file in &handoff.changed_files {
        out.push_str(&format!("- {} {}\n", file.path.display(), file.summary));
    }
    for gap in &handoff.known_gaps {
        out.push_str(&format!("gap: {gap}\n"));
    }
    out
}

fn deterministic_summary(handoff: &phonton_types::HandoffPacket) -> String {
    let checks = handoff.verification.passed.len();
    let findings = handoff.verification.findings.len();
    let gaps = handoff.known_gaps.len();
    let tokens = handoff.token_usage.budget_tokens();
    format!(
        "Summary: changed {} file(s) (+{} -{}), checks {} pass/{} finding(s), gaps {}, tokens {}.",
        handoff.diff_stats.files_changed,
        handoff.diff_stats.added_lines,
        handoff.diff_stats.removed_lines,
        checks,
        findings,
        gaps,
        tokens
    )
}

fn token_note(goal: &GoalEntry) -> Option<String> {
    let mut total = 0_u64;
    let mut repair = 0_u64;
    let mut target_exceeded = None;
    for record in &goal.flight_log {
        if let OrchestratorEvent::PromptManifest { manifest, .. } = &record.event {
            total = total.saturating_add(manifest.total_estimated_tokens);
            if manifest.repair_attempt || manifest.attempt > 1 {
                repair = repair.saturating_add(manifest.total_estimated_tokens);
            }
            if manifest.target_exceeded {
                target_exceeded = Some(manifest.over_target_tokens);
            }
        }
    }
    if total == 0 {
        return None;
    }
    let mut note = format!("estimated prompt tokens: {total}");
    if repair > 0 {
        note.push_str(&format!("; repair attempts: {repair}"));
    }
    if let Some(over) = target_exceeded {
        note.push_str(&format!("; context target exceeded by {over}"));
    }
    Some(note)
}

fn routing_note(goal: &GoalEntry) -> Option<String> {
    let desc = goal.description.to_ascii_lowercase();
    let broad_generated = desc.contains("chess")
        || desc.contains("game")
        || desc.contains("app")
        || desc.contains("html")
        || desc.contains("web");
    if !broad_generated {
        return None;
    }
    let used_kimi = goal.flight_log.iter().any(|record| {
        matches!(
            &record.event,
            OrchestratorEvent::Thinking { model_name, .. }
                if model_name.to_ascii_lowercase().contains("kimi")
        )
    });
    if used_kimi {
        Some(
            "Kimi was used for a broad generated-code task; if quality gates repeat, use a stronger code model or narrow the prompt.".into(),
        )
    } else {
        None
    }
}

pub(crate) fn commands_focus_text(runs: &[CommandRunSummary], selected_run: usize) -> String {
    let mut out = String::from("Commands\n");
    if runs.is_empty() {
        out.push_str("No commands have run yet.");
        return out;
    }
    for (idx, run) in runs.iter().enumerate() {
        let marker = if idx == selected_run { ">" } else { "-" };
        let exit = run
            .exit_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "running/blocked".into());
        let duration = run
            .duration_ms
            .map(|ms| format!("{ms}ms"))
            .unwrap_or_else(|| "duration unknown".into());
        out.push_str(&format!(
            "{marker} {}  exit={}  {duration}\n",
            run.command, exit
        ));
        if !run.stdout_preview.is_empty() && run.stdout_preview != "running..." {
            out.push_str(&format!("stdout: {}\n", run.stdout_preview));
        }
        if !run.stderr_preview.is_empty() {
            out.push_str(&format!("stderr: {}\n", run.stderr_preview));
        }
    }
    out
}

pub(crate) fn log_focus_text(goal: &GoalEntry) -> String {
    let mut out = String::from("Log\n");
    if goal.flight_log.is_empty() {
        out.push_str("No flight-log events yet.");
        return out;
    }
    for record in goal.flight_log.iter().rev().take(12).rev() {
        out.push_str(&format!("{} {}\n", record.kind(), record.render_line()));
    }
    out
}

pub(crate) fn plan_focus_text(goal: &GoalEntry) -> String {
    let mut out = String::from("Plan\n");
    let Some(contract) = goal
        .state
        .as_ref()
        .and_then(|state| state.goal_contract.as_ref())
    else {
        out.push_str("No GoalContract recorded yet.");
        return out;
    };

    out.push_str(&format!(
        "goal: {}\nclass: {:?}  confidence: {}%\n",
        contract.goal, contract.task_class, contract.confidence_percent
    ));
    out.push_str(&format!(
        "criteria: {}  slices: {}  expected artifacts: {}  likely files: {}\n",
        contract.acceptance_criteria.len(),
        contract.acceptance_slices.len(),
        contract.expected_artifacts.len(),
        contract.likely_files.len()
    ));
    if !contract.acceptance_criteria.is_empty() {
        out.push_str("\nAcceptance\n");
        for criterion in contract.acceptance_criteria.iter().take(6) {
            out.push_str(&format!("- {}\n", short(criterion, 110)));
        }
    }
    if !contract.acceptance_slices.is_empty() {
        out.push_str("\nSlices\n");
        for slice in contract.acceptance_slices.iter().take(8) {
            let artifact = slice
                .artifact_path
                .as_ref()
                .map(|p| format!(" -> {}", p.display()))
                .unwrap_or_default();
            out.push_str(&format!(
                "- {}{}: {}\n",
                slice.id,
                artifact,
                short(&slice.criterion, 92)
            ));
        }
    }
    if !contract.verify_plan.is_empty() {
        out.push_str("\nVerify\n");
        for step in contract.verify_plan.iter().take(6) {
            let layer = step
                .layer
                .map(|layer| format!(" [{layer:?}]"))
                .unwrap_or_default();
            let command = step
                .command
                .as_ref()
                .map(|cmd| format!("  $ {}", cmd.command.join(" ")))
                .unwrap_or_default();
            out.push_str(&format!("- {}{}{}\n", step.name, layer, command));
        }
    }
    if !contract.run_plan.is_empty() {
        out.push_str("\nRun\n");
        for command in contract.run_plan.iter().take(4) {
            out.push_str(&format!(
                "- {}: {}\n",
                command.label,
                command.command.join(" ")
            ));
        }
    }
    if !contract.assumptions.is_empty() {
        out.push_str("\nAssumptions\n");
        for assumption in contract.assumptions.iter().take(4) {
            out.push_str(&format!("- {}\n", short(assumption, 100)));
        }
    }
    if !contract.clarification_questions.is_empty() {
        out.push_str("\nClarification\n");
        for question in contract.clarification_questions.iter().take(4) {
            out.push_str(&format!("- {}\n", short(question, 100)));
        }
    }
    out
}

pub(crate) fn context_focus_text(goal: &GoalEntry) -> String {
    let mut out = String::from("Context\n");
    let mut selected = 0usize;
    let mut selected_tokens = 0usize;
    let mut prompt_count = 0usize;
    let mut buckets = phonton_types::ContextBucketSummary::default();
    for record in &goal.flight_log {
        match &record.event {
            OrchestratorEvent::ContextSelected {
                slices,
                total_token_count,
                ..
            } => {
                selected = selected.saturating_add(slices.len());
                selected_tokens = selected_tokens.saturating_add(*total_token_count);
            }
            OrchestratorEvent::PromptManifest { manifest, .. } => {
                prompt_count = prompt_count.saturating_add(1);
                buckets.add_prompt_manifest(manifest);
            }
            _ => {}
        }
    }
    out.push_str(&format!(
        "selected slices: {}  indexed tokens: {}  prompt manifests: {}\n",
        selected, selected_tokens, prompt_count
    ));
    out.push_str(&format!(
        "code: {} selected  {} omitted  memory: {}  artifacts: {}\n",
        buckets.selected_code_tokens,
        buckets.omitted_candidate_tokens,
        buckets.memory_tokens,
        buckets.artifact_tokens
    ));
    out.push_str(&format!(
        "retry diagnostics: {}  tools: {}  deduped: {}  cached: {}\n",
        buckets.retry_diagnostic_tokens,
        buckets.tool_output_tokens,
        buckets.deduped_tokens,
        buckets.cached_tokens
    ));
    if selected == 0 && prompt_count == 0 {
        out.push_str("No context evidence has been recorded yet.");
    }
    out
}

pub(crate) fn tokens_focus_text(goal: &GoalEntry) -> String {
    let mut out = String::from("Tokens\n");
    let mut first_attempt = 0_u64;
    let mut repair_attempts = 0_u64;
    let mut total = 0_u64;
    let mut omitted = 0_u64;
    let mut deduped = 0_u64;
    for record in &goal.flight_log {
        if let OrchestratorEvent::PromptManifest { manifest, .. } = &record.event {
            total = total.saturating_add(manifest.total_estimated_tokens);
            omitted = omitted.saturating_add(manifest.omitted_code_tokens);
            deduped = deduped.saturating_add(manifest.deduped_tokens);
            if manifest.repair_attempt || manifest.attempt > 1 {
                repair_attempts = repair_attempts.saturating_add(manifest.total_estimated_tokens);
            } else {
                first_attempt = first_attempt.saturating_add(manifest.total_estimated_tokens);
            }
        }
    }
    let provider = goal
        .state
        .as_ref()
        .and_then(|state| state.handoff_packet.as_ref())
        .map(|handoff| handoff.token_usage)
        .unwrap_or_default();
    out.push_str(&format!(
        "prompt estimate: {}  first attempt: {}  repair: {}\n",
        total, first_attempt, repair_attempts
    ));
    out.push_str(&format!(
        "omitted candidate context: {}  deduped: {}\n",
        omitted, deduped
    ));
    out.push_str(&format!(
        "provider input: {}  output: {}  cached: {}  estimated: {}\n",
        provider.input_tokens, provider.output_tokens, provider.cached_tokens, provider.estimated
    ));
    out.push_str("Provider-reported tokens remain the billing source of truth.");
    out
}

pub(crate) fn append_command_run_lines(
    lines: &mut Vec<Line<'static>>,
    runs: &[CommandRunSummary],
    selected_run: usize,
) {
    if runs.is_empty() {
        return;
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Commands",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )));
    for (visible_idx, run) in runs.iter().rev().take(8).enumerate() {
        let (label, color) = match run.exit_code {
            None if run.stdout_preview == "running..." => ("run", WARN),
            Some(0) => ("ok", SUCCESS),
            Some(_) => ("fail", DANGER),
            None => ("blocked", DANGER),
        };
        let source_idx = runs.len().saturating_sub(1).saturating_sub(visible_idx);
        let marker = if source_idx == selected_run { ">" } else { " " };
        let duration = run
            .duration_ms
            .map(|ms| format!("  {ms}ms"))
            .unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled(marker, Style::default().fg(ACCENT_HI)),
            Span::styled(
                format!("  {label} "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(short(&run.command, 72), Style::default().fg(Color::White)),
            Span::styled(duration, Style::default().fg(MUTED)),
        ]));
        if !run.stdout_preview.is_empty() && run.stdout_preview != "running..." {
            lines.push(Line::from(vec![
                Span::styled("    out ", Style::default().fg(MUTED)),
                Span::styled(short(&run.stdout_preview, 90), Style::default().fg(MUTED)),
            ]));
        }
        if !run.stderr_preview.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("    err ", Style::default().fg(WARN)),
                Span::styled(short(&run.stderr_preview, 90), Style::default().fg(MUTED)),
            ]));
        }
    }
}
