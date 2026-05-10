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
        FocusView::Receipt,
        FocusView::Problems,
        FocusView::Code,
        FocusView::Commands,
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
        FocusView::Receipt => "f cycle",
        FocusView::Problems => "p problems  r retry",
        FocusView::Code => "[ ] file  PgUp/PgDn",
        FocusView::Commands => "[ ] command  /rerun",
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
        let mut out = format!(
            "Code {}/{}\nfile: {}\n",
            selected_file.min(groups.len().saturating_sub(1)) + 1,
            groups.len(),
            path.display()
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
    out.push_str("\nRepair\n- Press r or run /retry to queue a repair with compact diagnostics.\n- Use /why-tokens to inspect retry/context token buckets.\n");

    let groups = diff_hunks_by_file(goal);
    if let Some((path, hunks)) = groups.get(selected_file.min(groups.len().saturating_sub(1))) {
        out.push_str(&format!(
            "\nChanged excerpt {}/{}\nfile: {}\n",
            selected_file.min(groups.len().saturating_sub(1)) + 1,
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
