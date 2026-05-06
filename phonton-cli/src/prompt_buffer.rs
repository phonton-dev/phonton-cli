#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSubmission {
    pub display_text: String,
    pub model_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptArtifact {
    pub chip: String,
    pub text: String,
    pub line_count: usize,
    pub char_count: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptBuffer {
    text: String,
    cursor: usize,
    artifacts: Vec<PromptArtifact>,
}

const PASTE_LINE_THRESHOLD: usize = 2;
const PASTE_CHAR_THRESHOLD: usize = 600;
const MAX_PASTE_ARTIFACT_CHARS: usize = 12_000;

impl PromptBuffer {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            artifacts: Vec::new(),
        }
    }

    pub fn from_text(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = char_count(&text);
        Self {
            text,
            cursor,
            artifacts: Vec::new(),
        }
    }

    pub fn display_text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = char_count(&self.text);
        self.artifacts.clear();
    }

    pub fn insert_char(&mut self, c: char) {
        self.insert_str(&c.to_string());
    }

    pub fn insert_text(&mut self, text: &str) {
        self.insert_str(text);
    }

    pub fn insert_paste(&mut self, text: &str) {
        if should_collapse_paste(text) {
            let artifact = make_paste_artifact(text);
            let chip = artifact.chip.clone();
            self.artifacts.push(artifact);
            self.insert_str(&chip);
        } else {
            self.insert_str(text);
        }
    }

    pub fn delete_char_before_cursor(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let new_cursor = self.cursor - 1;
        let start = byte_index_at(&self.text, new_cursor);
        let end = byte_index_at(&self.text, self.cursor);
        self.text.replace_range(start..end, "");
        self.cursor = new_cursor;
    }

    pub fn delete_word_before_cursor(&mut self) {
        let chars: Vec<char> = self.text.chars().collect();
        let mut i = self.cursor.min(chars.len());
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        let start = byte_index_at(&self.text, i);
        let end = byte_index_at(&self.text, self.cursor);
        self.text.replace_range(start..end, "");
        self.cursor = i;
    }

    pub fn clear_before_cursor(&mut self) {
        let end = byte_index_at(&self.text, self.cursor);
        self.text.replace_range(..end, "");
        self.cursor = 0;
    }

    pub fn clear_after_cursor(&mut self) {
        let start = byte_index_at(&self.text, self.cursor);
        self.text.replace_range(start.., "");
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        if self.cursor < char_count(&self.text) {
            self.cursor += 1;
        }
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = char_count(&self.text);
    }

    pub fn take_submission(&mut self) -> Option<PromptSubmission> {
        let display_text = std::mem::take(&mut self.text);
        self.cursor = 0;
        let visible_artifacts = artifacts_visible_in_display(&display_text, &self.artifacts);
        if display_text.trim().is_empty() && visible_artifacts.is_empty() {
            self.artifacts.clear();
            return None;
        }

        let mut model_text = display_text.clone();
        if !visible_artifacts.is_empty() {
            if !model_text.ends_with('\n') {
                model_text.push_str("\n\n");
            }
            model_text.push_str("# Pasted prompt artifacts\n");
            for (idx, artifact) in visible_artifacts.iter().enumerate() {
                model_text.push_str(&format!(
                    "## paste-{} ({} {}, {}{})\n",
                    idx + 1,
                    artifact.line_count,
                    if artifact.line_count == 1 {
                        "line"
                    } else {
                        "lines"
                    },
                    format_char_count(artifact.char_count),
                    if artifact.truncated {
                        ", truncated"
                    } else {
                        ""
                    },
                ));
                model_text.push_str("<paste-content>\n");
                model_text.push_str(&artifact.text);
                if !artifact.text.ends_with('\n') {
                    model_text.push('\n');
                }
                model_text.push_str("</paste-content>\n");
            }
        }
        self.artifacts.clear();
        Some(PromptSubmission {
            display_text,
            model_text,
        })
    }

    fn insert_str(&mut self, text: &str) {
        let idx = byte_index_at(&self.text, self.cursor);
        self.text.insert_str(idx, text);
        self.cursor += char_count(text);
    }
}

fn artifacts_visible_in_display(
    display_text: &str,
    artifacts: &[PromptArtifact],
) -> Vec<PromptArtifact> {
    let mut visible = Vec::new();
    let mut search_from = 0;
    for artifact in artifacts {
        if let Some(pos) = display_text[search_from..].find(&artifact.chip) {
            search_from += pos + artifact.chip.len();
            visible.push(artifact.clone());
        }
    }
    visible
}

impl Default for PromptBuffer {
    fn default() -> Self {
        Self::new()
    }
}

fn should_collapse_paste(text: &str) -> bool {
    line_count(text) >= PASTE_LINE_THRESHOLD || char_count(text) > PASTE_CHAR_THRESHOLD
}

fn make_paste_artifact(text: &str) -> PromptArtifact {
    let char_count = char_count(text);
    let line_count = line_count(text);
    let truncated = char_count > MAX_PASTE_ARTIFACT_CHARS;
    let stored = if truncated {
        text.chars().take(MAX_PASTE_ARTIFACT_CHARS).collect()
    } else {
        text.to_string()
    };
    let chip = format!(
        "[paste: {} {}, {}]",
        line_count,
        if line_count == 1 { "line" } else { "lines" },
        format_char_count(char_count)
    );
    PromptArtifact {
        chip,
        text: stored,
        line_count,
        char_count,
        truncated,
    }
}

fn line_count(text: &str) -> usize {
    text.lines().count().max(1)
}

fn char_count(text: &str) -> usize {
    text.chars().count()
}

fn byte_index_at(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len())
}

fn format_char_count(chars: usize) -> String {
    if chars >= 1000 {
        format!("{:.1}k chars", chars as f64 / 1000.0)
    } else {
        format!("{chars} chars")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiline_paste_collapses_to_chip_but_submits_full_text() {
        let mut buffer = PromptBuffer::new();
        buffer.insert_char('x');
        buffer.insert_paste("line one\nline two\nline three");

        assert_eq!(buffer.display_text(), "x[paste: 3 lines, 28 chars]");

        let submission = buffer.take_submission().expect("prompt should submit");
        assert_eq!(submission.display_text, "x[paste: 3 lines, 28 chars]");
        assert!(submission.model_text.starts_with("x"));
        assert!(submission.model_text.contains("# Pasted prompt artifacts"));
        assert!(submission
            .model_text
            .contains("line one\nline two\nline three"));
    }

    #[test]
    fn long_single_line_paste_collapses_to_chip() {
        let mut buffer = PromptBuffer::new();
        let text = "a".repeat(601);
        buffer.insert_paste(&text);

        assert_eq!(buffer.display_text(), "[paste: 1 line, 601 chars]");
        assert!(buffer
            .take_submission()
            .expect("prompt should submit")
            .model_text
            .contains(&text));
    }

    #[test]
    fn short_paste_inserts_plain_text() {
        let mut buffer = PromptBuffer::new();
        buffer.insert_paste("short paste");

        assert_eq!(buffer.display_text(), "short paste");
        assert_eq!(
            buffer
                .take_submission()
                .expect("prompt should submit")
                .model_text,
            "short paste"
        );
    }

    #[test]
    fn editing_shortcuts_update_text_and_cursor() {
        let mut buffer = PromptBuffer::new();
        for c in "make chess please".chars() {
            buffer.insert_char(c);
        }

        buffer.delete_word_before_cursor();
        assert_eq!(buffer.display_text(), "make chess ");

        buffer.clear_before_cursor();
        assert_eq!(buffer.display_text(), "");

        for c in "abc".chars() {
            buffer.insert_char(c);
        }
        buffer.clear_after_cursor();
        assert_eq!(buffer.display_text(), "abc");
        assert_eq!(buffer.cursor(), 3);
    }

    #[test]
    fn cleared_paste_chip_does_not_submit_hidden_artifact() {
        let mut buffer = PromptBuffer::new();
        buffer.insert_paste("line one\nline two");
        assert!(buffer.display_text().starts_with("[paste:"));

        buffer.clear_before_cursor();

        assert_eq!(buffer.display_text(), "");
        assert_eq!(buffer.take_submission(), None);
    }

    #[test]
    fn deleted_paste_chip_is_not_attached_to_new_prompt() {
        let mut buffer = PromptBuffer::new();
        buffer.insert_paste("line one\nline two");
        buffer.clear_before_cursor();
        buffer.insert_text("make a README");

        let submission = buffer.take_submission().expect("prompt should submit");

        assert_eq!(submission.display_text, "make a README");
        assert_eq!(submission.model_text, "make a README");
    }
}
