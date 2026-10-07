//! Exact local-plan review before the TUI admits execution.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use phonton_types::local_run::ReviewedLocalPlan;
use phonton_types::TaskId;

/// Render-safe pending decision. Execution retains the original typed plan.
#[derive(Debug, Clone)]
pub struct PendingLocalPlan {
    pub task_id: TaskId,
    pub lines: Vec<String>,
    pub scroll: u16,
}

impl PendingLocalPlan {
    pub fn new(task_id: TaskId, reviewed: &ReviewedLocalPlan) -> Self {
        let p = &reviewed.plan;
        let r = &p.request;
        let repository = r.repository.display().to_string();
        let mut lines = vec![
            format!("Goal: {}", r.goal),
            format!(
                "Repository: {}",
                repository.strip_prefix(r"\\?\").unwrap_or(&repository)
            ),
            String::new(),
            "WILL EDIT".into(),
        ];
        for f in &p.files {
            lines.push(format!("  {}  ({})", f.path.display(), f.reason));
        }
        if let Some(c) = &p.creation {
            lines.push(format!("  {}  (new file)", c.path.display()));
        }
        lines.extend([String::new(), "CHECKS (exact arguments)".into()]);
        if let Some(c) = &r.preparation {
            lines.push(format!("  setup {}", command(c)));
        }
        for c in &r.checks {
            lines.push(format!("  {}", command(c)));
        }
        lines.push(
            if r.approve_host_execution {
                "  Allowed by your session choice; project code runs without isolation."
            } else {
                "  Not allowed: the candidate will stay unverified."
            }
            .into(),
        );
        lines.push(String::new());
        if let Some(m) = &reviewed.model_selection {
            lines.push(format!(
                "MODEL {} · {} context tokens",
                m.model, m.context_tokens
            ));
        }
        lines.extend([
            format!(
                "BUDGET {} model calls · {} output tokens · {} s",
                r.budget.generations, r.budget.generated_tokens, r.budget.wall_seconds
            ),
            "Edits stay in a copy until you apply the reviewed result.".into(),
        ]);
        // The pending-approval note is stale once the session allowed checks.
        let notes: Vec<&String> = p
            .warnings
            .iter()
            .filter(|w| !(r.approve_host_execution && w.contains("require separate host approval")))
            .collect();
        if !notes.is_empty() {
            lines.extend([String::new(), "NOTES".into()]);
            lines.extend(notes.into_iter().cloned());
        }
        Self {
            task_id,
            lines,
            scroll: 0,
        }
    }

    /// Wrap without hiding wide Unicode paths or command arguments.
    pub fn wrapped_lines(&self, width: usize) -> Vec<String> {
        let width = width.max(1);
        let mut result = Vec::new();
        for line in &self.lines {
            let mut current = String::new();
            let mut used = 0;
            for ch in line.chars() {
                let cells = ratatui::text::Span::raw(ch.to_string()).width();
                if used + cells > width && !current.is_empty() {
                    result.push(std::mem::take(&mut current));
                    used = 0;
                }
                current.push(ch);
                used += cells;
            }
            result.push(current);
        }
        result
    }

    pub fn clamp_scroll(&mut self, area: ratatui::layout::Rect) {
        let width = area.width.saturating_sub(4).clamp(1, 108).saturating_sub(2);
        let height = area.height.saturating_sub(2).clamp(1, 34).saturating_sub(2);
        let max = self
            .wrapped_lines(width as usize)
            .len()
            .saturating_sub(height as usize)
            .min(u16::MAX as usize) as u16;
        self.scroll = self.scroll.min(max);
    }

    /// Only Y approves. Enter, pasted text and modified keys cannot approve.
    pub fn key(&mut self, key: KeyEvent) -> Option<bool> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        if key.code == KeyCode::Esc
            || key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            return Some(false);
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return None;
        }
        match key.code {
            KeyCode::Char('y' | 'Y') => Some(true),
            KeyCode::Char('n' | 'N') => Some(false),
            KeyCode::Up => {
                self.scroll = self.scroll.saturating_sub(1);
                None
            }
            KeyCode::Down => {
                self.scroll = self.scroll.saturating_add(1);
                None
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(8);
                None
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(8);
                None
            }
            KeyCode::Home => {
                self.scroll = 0;
                None
            }
            _ => None,
        }
    }
}

fn command(c: &phonton_types::local_run::LocalCheck) -> String {
    let args: Vec<&str> = std::iter::once(c.program.as_str())
        .chain(c.args.iter().map(String::as_str))
        .collect();
    // Serializing strings cannot fail; preserve argument boundaries in the UI.
    serde_json::to_string(&args).unwrap_or_else(|_| "[unavailable]".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt() -> PendingLocalPlan {
        PendingLocalPlan {
            task_id: TaskId::new(),
            lines: vec!["exact plan".into()],
            scroll: 0,
        }
    }

    #[test]
    fn plan_requires_explicit_unmodified_yes_and_can_be_denied() {
        let mut p = prompt();
        assert_eq!(
            p.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            p.key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(
            p.key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
            Some(true)
        );
        assert_eq!(
            p.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(false)
        );
        assert_eq!(
            p.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
            Some(false)
        );
    }

    #[test]
    fn held_yes_cannot_approve_the_next_plan() {
        assert_eq!(
            prompt().key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
            Some(true)
        );
        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            assert_eq!(
                prompt().key(KeyEvent::new_with_kind(
                    KeyCode::Char('y'),
                    KeyModifiers::NONE,
                    kind
                )),
                None
            );
        }
    }

    #[test]
    fn wide_paths_wrap_losslessly_and_scroll_stays_reachable() {
        let mut p = prompt();
        p.lines = vec!["CHECK [\"python\",\"測試目錄/測試套件.py\"]".into()];
        let wrapped = p.wrapped_lines(14);
        assert_eq!(wrapped.concat(), p.lines[0]);
        assert!(wrapped
            .iter()
            .all(|l| ratatui::text::Line::raw(l.as_str()).width() <= 14));
        p.scroll = u16::MAX;
        p.clamp_scroll(ratatui::layout::Rect::new(0, 0, 20, 6));
        let end = p.scroll;
        p.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(p.scroll, end.saturating_sub(1));
    }

    #[test]
    fn commands_preserve_argument_boundaries_and_control_characters() {
        let c = phonton_types::local_run::LocalCheck {
            program: "python".into(),
            args: vec!["test suite.py".into(), "a\nb".into()],
        };
        assert_eq!(command(&c), r#"["python","test suite.py","a\nb"]"#);
    }
}
