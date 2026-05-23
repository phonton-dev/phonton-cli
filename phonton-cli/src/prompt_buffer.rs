use phonton_types::{
    PromptArtifact, PromptArtifactKind, PromptArtifactRole, MAX_PROMPT_ARTIFACT_CHARS,
};

const PASTE_ARTIFACT_CHAR_THRESHOLD: usize = 600;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmittedPrompt {
    pub description: String,
    pub display_text: String,
    pub prompt_artifacts: Vec<PromptArtifact>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteOutcome {
    InsertedInline,
    AddedArtifact,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptBuffer {
    text: String,
    cursor: usize,
    artifacts: Vec<PromptArtifact>,
    next_artifact_id: u64,
    notice: Option<String>,
}

impl Default for PromptBuffer {
    fn default() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            artifacts: Vec::new(),
            next_artifact_id: 1,
            notice: None,
        }
    }
}

impl PromptBuffer {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn artifacts(&self) -> &[PromptArtifact] {
        &self.artifacts
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    pub fn set_notice(&mut self, notice: impl Into<String>) {
        self.notice = Some(notice.into());
    }

    pub fn clear_notice(&mut self) {
        self.notice = None;
    }

    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.artifacts.is_empty()
    }

    pub fn char_count(&self) -> usize {
        char_count(&self.text)
    }

    pub fn insert_char(&mut self, c: char) {
        self.cursor = insert_char_at(&mut self.text, self.cursor, c);
        self.clear_notice();
    }

    pub fn insert_text(&mut self, text: &str) {
        for c in text.chars() {
            self.cursor = insert_char_at(&mut self.text, self.cursor, c);
        }
        self.clear_notice();
    }

    pub fn delete_char_before(&mut self) {
        if self.cursor == 0 && self.text.is_empty() {
            self.artifacts.pop();
            return;
        }
        self.cursor = delete_char_before(&mut self.text, self.cursor);
        self.clear_notice();
    }

    pub fn delete_word_before(&mut self) {
        self.cursor = delete_word_before(&mut self.text, self.cursor);
        self.clear_notice();
    }

    pub fn clear_before_cursor(&mut self) {
        let end = byte_idx(&self.text, self.cursor);
        self.text.replace_range(..end, "");
        self.cursor = 0;
        self.clear_notice();
    }

    pub fn clear_after_cursor(&mut self) {
        let start = byte_idx(&self.text, self.cursor);
        self.text.replace_range(start.., "");
        self.clear_notice();
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        if self.cursor < self.char_count() {
            self.cursor += 1;
        }
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.char_count();
    }

    pub fn handle_paste(&mut self, text: &str) -> PasteOutcome {
        let normalized = normalize_paste(text);
        if normalized.is_empty() {
            return PasteOutcome::InsertedInline;
        }

        if should_be_artifact(&normalized) {
            let artifact = self.build_paste_artifact(&normalized);
            self.artifacts.push(artifact);
            self.clear_notice();
            PasteOutcome::AddedArtifact
        } else {
            self.insert_text(&normalized);
            PasteOutcome::InsertedInline
        }
    }

    pub fn remove_artifact(&mut self, index: usize) -> Option<PromptArtifact> {
        if index < self.artifacts.len() {
            Some(self.artifacts.remove(index))
        } else {
            None
        }
    }

    pub fn remove_last_artifact(&mut self) -> Option<PromptArtifact> {
        self.artifacts.pop()
    }

    pub fn submit(&mut self) -> Option<SubmittedPrompt> {
        if self.is_empty() {
            return None;
        }

        let typed = self.text.trim().to_string();
        let mut artifacts = std::mem::take(&mut self.artifacts);
        self.text.clear();
        self.cursor = 0;
        self.notice = None;

        if typed.is_empty() {
            if !artifacts.is_empty() {
                artifacts[0].role = PromptArtifactRole::MainRequest;
                for artifact in artifacts.iter_mut().skip(1) {
                    artifact.role = PromptArtifactRole::Context;
                }
                let display_text =
                    format!("paste: {}", first_non_empty_line(&artifacts[0].text, 60));
                return Some(SubmittedPrompt {
                    description: artifacts[0].text.clone(),
                    display_text,
                    prompt_artifacts: artifacts,
                });
            }
            return None;
        }

        for artifact in &mut artifacts {
            artifact.role = PromptArtifactRole::Context;
        }
        Some(SubmittedPrompt {
            display_text: typed.clone(),
            description: typed,
            prompt_artifacts: artifacts,
        })
    }

    fn build_paste_artifact(&mut self, normalized: &str) -> PromptArtifact {
        let original_chars = char_count(normalized);
        let original_lines = logical_line_count(normalized);
        let truncated = original_chars > MAX_PROMPT_ARTIFACT_CHARS;
        let text = if truncated {
            normalized.chars().take(MAX_PROMPT_ARTIFACT_CHARS).collect()
        } else {
            normalized.to_string()
        };
        let id = format!("paste-{}", self.next_artifact_id);
        self.next_artifact_id += 1;
        let label = format!(
            "[paste: {} lines, {}]",
            original_lines,
            format_char_count(original_chars)
        );
        PromptArtifact {
            id,
            kind: PromptArtifactKind::PastedText,
            role: PromptArtifactRole::Context,
            label,
            original_chars,
            original_lines,
            text,
            truncated,
            note: truncated.then(|| {
                format!(
                    "pasted text truncated to the first {} chars",
                    MAX_PROMPT_ARTIFACT_CHARS
                )
            }),
        }
    }
}

pub fn normalize_paste(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn should_be_artifact(text: &str) -> bool {
    logical_line_count(text) >= 2 || char_count(text) > PASTE_ARTIFACT_CHAR_THRESHOLD
}

fn logical_line_count(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.split('\n').count()
    }
}

fn format_char_count(chars: usize) -> String {
    if chars >= 1000 {
        let whole = chars / 1000;
        let tenths = (chars % 1000) / 100;
        if tenths == 0 {
            format!("{whole}k chars")
        } else {
            format!("{whole}.{tenths}k chars")
        }
    } else {
        format!("{chars} chars")
    }
}

fn first_non_empty_line(text: &str, max_chars: usize) -> String {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .unwrap_or("pasted prompt");
    let mut out: String = line.chars().take(max_chars).collect();
    if char_count(line) > max_chars {
        out.push_str("...");
    }
    out
}

fn insert_char_at(s: &mut String, char_idx: usize, c: char) -> usize {
    let byte_idx = byte_idx(s, char_idx);
    s.insert(byte_idx, c);
    char_idx + 1
}

fn delete_char_before(s: &mut String, char_idx: usize) -> usize {
    if char_idx == 0 {
        return 0;
    }
    let new_idx = char_idx - 1;
    let start = byte_idx(s, new_idx);
    let end = byte_idx(s, char_idx);
    s.replace_range(start..end, "");
    new_idx
}

fn delete_word_before(s: &mut String, char_idx: usize) -> usize {
    let chars: Vec<char> = s.chars().collect();
    let mut i = char_idx.min(chars.len());
    while i > 0 && chars[i - 1].is_whitespace() {
        i -= 1;
    }
    while i > 0 && !chars[i - 1].is_whitespace() {
        i -= 1;
    }
    let start = byte_idx(s, i);
    let end = byte_idx(s, char_idx);
    s.replace_range(start..end, "");
    i
}

fn byte_idx(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}

fn char_count(s: &str) -> usize {
    s.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiline_paste_becomes_chip_artifact() {
        let mut buffer = PromptBuffer::default();
        assert_eq!(
            buffer.handle_paste("do x\r\ndo y\rdo z"),
            PasteOutcome::AddedArtifact
        );
        assert_eq!(buffer.text(), "");
        assert_eq!(buffer.artifacts().len(), 1);
        assert_eq!(buffer.artifacts()[0].label, "[paste: 3 lines, 14 chars]");
        assert_eq!(buffer.artifacts()[0].text, "do x\ndo y\ndo z");
    }

    #[test]
    fn short_single_line_paste_inserts_inline() {
        let mut buffer = PromptBuffer::default();
        assert_eq!(
            buffer.handle_paste("make chess"),
            PasteOutcome::InsertedInline
        );
        assert_eq!(buffer.text(), "make chess");
        assert!(buffer.artifacts().is_empty());
    }

    #[test]
    fn artifact_only_submit_uses_pasted_text_as_main_request() {
        let mut buffer = PromptBuffer::default();
        buffer.handle_paste("do x\ndo y");

        let submitted = buffer.submit().expect("submitted prompt");

        assert_eq!(submitted.description, "do x\ndo y");
        assert_eq!(submitted.display_text, "paste: do x");
        assert_eq!(
            submitted.prompt_artifacts[0].role,
            PromptArtifactRole::MainRequest
        );
        assert!(buffer.is_empty());
    }

    #[test]
    fn typed_prompt_makes_paste_context() {
        let mut buffer = PromptBuffer::default();
        buffer.insert_text("use this context");
        buffer.handle_paste("line one\nline two");

        let submitted = buffer.submit().expect("submitted prompt");

        assert_eq!(submitted.description, "use this context");
        assert_eq!(
            submitted.prompt_artifacts[0].role,
            PromptArtifactRole::Context
        );
    }

    #[test]
    fn backspace_on_empty_text_removes_last_artifact() {
        let mut buffer = PromptBuffer::default();
        buffer.handle_paste("one\ntwo");
        buffer.delete_char_before();
        assert!(buffer.artifacts().is_empty());
    }
}
