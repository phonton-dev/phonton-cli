use phonton_sandbox::ToolCall;

#[derive(Debug, Clone)]
pub struct ParsedCommand {
    pub original: String,
    pub call: ToolCall,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRunSummary {
    pub command: String,
    pub exit_code: Option<i32>,
    pub stdout_preview: String,
    pub stderr_preview: String,
}

pub fn parse_prompt_command(_input: &str) -> Option<ParsedCommand> {
    let input = _input.trim();
    let command = if input == "/run" {
        return None;
    } else if let Some(rest) = input.strip_prefix("/run") {
        if !rest.chars().next().is_some_and(char::is_whitespace) {
            return None;
        }
        let rest = rest.trim_start();
        rest
    } else if let Some(rest) = input.strip_prefix('!') {
        let rest = rest.trim_start();
        if rest.is_empty() {
            return None;
        }
        rest
    } else {
        return None;
    };

    let call = if contains_shell_metachar(command) {
        ToolCall::Bash {
            command: command.to_string(),
        }
    } else {
        match split_command_words(command) {
            Some(mut words) if !words.is_empty() => {
                let program = words.remove(0);
                ToolCall::Run {
                    program,
                    args: words,
                }
            }
            _ => ToolCall::Bash {
                command: command.to_string(),
            },
        }
    };

    Some(ParsedCommand {
        original: command.to_string(),
        call,
    })
}

pub fn summarize_output(
    _command: &str,
    _exit_code: Option<i32>,
    _stdout: &[u8],
    _stderr: &[u8],
) -> CommandRunSummary {
    CommandRunSummary {
        command: _command.to_string(),
        exit_code: _exit_code,
        stdout_preview: preview_bytes(_stdout, 180),
        stderr_preview: preview_bytes(_stderr, 180),
    }
}

fn contains_shell_metachar(command: &str) -> bool {
    let shell_tokens = ["&&", "||", "|", ";", "&", ">", "<", "`", "$(", "${"];
    shell_tokens.iter().any(|token| command.contains(token))
}

fn split_command_words(command: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut chars = command.chars().peekable();
    let mut quote: Option<char> = None;

    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => current.push(c),
            (None, '\'' | '"') => quote = Some(ch),
            (None, c) if c.is_whitespace() => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
                while chars.peek().is_some_and(|c| c.is_whitespace()) {
                    chars.next();
                }
            }
            (None, c) => current.push(c),
        }
    }

    if quote.is_some() {
        return None;
    }
    if !current.is_empty() {
        words.push(current);
    }
    Some(words)
}

fn preview_bytes(bytes: &[u8], limit: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.chars().count() <= limit {
        return text.into_owned();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push_str(&format!("\n[truncated - {} bytes total]", bytes.len()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_run_parses_simple_command_as_structured_run() {
        let parsed = parse_prompt_command("/run cargo test -p phonton-cli")
            .expect("/run should parse as a command");

        assert_eq!(parsed.original, "cargo test -p phonton-cli");
        match parsed.call {
            ToolCall::Run { program, args } => {
                assert_eq!(program, "cargo");
                assert_eq!(args, vec!["test", "-p", "phonton-cli"]);
            }
            other => panic!("expected structured run, got {other:?}"),
        }
    }

    #[test]
    fn bang_parses_command_shorthand() {
        let parsed = parse_prompt_command("!npm run build").expect("! should parse as command");

        assert_eq!(parsed.original, "npm run build");
        match parsed.call {
            ToolCall::Run { program, args } => {
                assert_eq!(program, "npm");
                assert_eq!(args, vec!["run", "build"]);
            }
            other => panic!("expected structured run, got {other:?}"),
        }
    }

    #[test]
    fn shell_metacharacters_route_to_approval_gated_bash() {
        let parsed =
            parse_prompt_command("!npm test && npm run build").expect("shell command should parse");

        match parsed.call {
            ToolCall::Bash { command } => assert_eq!(command, "npm test && npm run build"),
            other => panic!("expected bash command, got {other:?}"),
        }
    }

    #[test]
    fn single_ampersand_routes_to_approval_gated_bash() {
        let parsed =
            parse_prompt_command("!npm test & npm run build").expect("shell command should parse");

        match parsed.call {
            ToolCall::Bash { command } => assert_eq!(command, "npm test & npm run build"),
            other => panic!("expected bash command, got {other:?}"),
        }
    }

    #[test]
    fn run_prefix_must_be_a_standalone_command() {
        assert!(parse_prompt_command("/runfoo").is_none());
        assert!(parse_prompt_command("/run").is_none());
        assert!(parse_prompt_command("/run\tcargo test").is_some());
    }

    #[test]
    fn summarize_output_truncates_long_streams() {
        let stdout = "x".repeat(300);
        let summary = summarize_output("npm test", Some(0), stdout.as_bytes(), b"");

        assert_eq!(summary.command, "npm test");
        assert_eq!(summary.exit_code, Some(0));
        assert!(summary.stdout_preview.len() < stdout.len());
        assert!(summary.stdout_preview.contains("truncated"));
    }
}
