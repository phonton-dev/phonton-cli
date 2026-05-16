use phonton_types::PermissionMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandCategory {
    Loop,
    Trust,
    Session,
    Config,
    Shell,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FocusView {
    #[default]
    Plan,
    Receipt,
    Problems,
    Code,
    Commands,
    Context,
    Tokens,
    Log,
}

impl FocusView {
    pub fn parse(input: &str) -> Option<Self> {
        match input.trim().to_ascii_lowercase().as_str() {
            "plan" | "contract" => Some(Self::Plan),
            "receipt" | "review" => Some(Self::Receipt),
            "problems" | "problem" | "diagnostics" | "diagnostic" | "diag" => Some(Self::Problems),
            "code" | "diff" => Some(Self::Code),
            "commands" | "command" | "cmd" | "run" => Some(Self::Commands),
            "context" | "memory" | "influence" => Some(Self::Context),
            "tokens" | "why-tokens" | "cost" => Some(Self::Tokens),
            "log" | "flight-log" | "flight" => Some(Self::Log),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "Plan",
            Self::Receipt => "Receipt",
            Self::Problems => "Problems",
            Self::Code => "Code",
            Self::Commands => "Commands",
            Self::Context => "Context",
            Self::Tokens => "Tokens",
            Self::Log => "Log",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Plan => Self::Receipt,
            Self::Receipt => Self::Problems,
            Self::Problems => Self::Code,
            Self::Code => Self::Commands,
            Self::Commands => Self::Context,
            Self::Context => Self::Tokens,
            Self::Tokens => Self::Log,
            Self::Log => Self::Plan,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashAction {
    GoalMode,
    TaskMode,
    AskMode,
    SubmitAsk(String),
    PreviewPlan(String),
    ApprovePlan,
    OpenSettings,
    ToggleLog,
    OpenMemory,
    OpenHistory,
    ClearGoals,
    DeleteSelectedGoal,
    Quit,
    ShowStatus,
    ShowCommands,
    ShowPermissions,
    ShowTrust,
    RevokeCurrentTrust,
    SetPermissionMode(PermissionMode),
    ShowContext,
    CompactContext,
    ShowProblems,
    RetryGoal,
    ShowWhyTokens,
    StopGoal,
    OpenGoals,
    SetFocus(FocusView),
    CopyFocus,
    RerunCommand,
    ShowStats,
    ShowReview,
    ManageModel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub args: &'static str,
    pub description: &'static str,
    pub category: CommandCategory,
    pub action: SlashAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashParse {
    NotCommand,
    RunCommand,
    Command(SlashAction),
    Unknown {
        command: String,
        suggestion: Option<String>,
    },
}

pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "/goal",
        aliases: &[],
        args: "",
        description: "switch to goal mode",
        category: CommandCategory::Loop,
        action: SlashAction::GoalMode,
    },
    CommandSpec {
        name: "/task",
        aliases: &[],
        args: "",
        description: "switch to direct task mode",
        category: CommandCategory::Loop,
        action: SlashAction::TaskMode,
    },
    CommandSpec {
        name: "/ask",
        aliases: &[],
        args: "[question]",
        description: "open Ask or ask a question directly",
        category: CommandCategory::Loop,
        action: SlashAction::AskMode,
    },
    CommandSpec {
        name: "/plan",
        aliases: &[],
        args: "<goal>",
        description: "preview a GoalContract and plan before execution",
        category: CommandCategory::Loop,
        action: SlashAction::SetFocus(FocusView::Plan),
    },
    CommandSpec {
        name: "/approve",
        aliases: &[],
        args: "",
        description: "execute the selected plan preview",
        category: CommandCategory::Loop,
        action: SlashAction::ApprovePlan,
    },
    CommandSpec {
        name: "/settings",
        aliases: &["/config"],
        args: "",
        description: "open provider, model, and budget settings",
        category: CommandCategory::Config,
        action: SlashAction::OpenSettings,
    },
    CommandSpec {
        name: "/model",
        aliases: &[],
        args: "set <name>|manage",
        description: "open model management in Settings",
        category: CommandCategory::Config,
        action: SlashAction::ManageModel,
    },
    CommandSpec {
        name: "/status",
        aliases: &[],
        args: "",
        description: "show version, provider, model, workspace, and session state",
        category: CommandCategory::Trust,
        action: SlashAction::ShowStatus,
    },
    CommandSpec {
        name: "/permissions",
        aliases: &[],
        args: "set <mode>",
        description: "show or set sandbox, approval, and trust status",
        category: CommandCategory::Trust,
        action: SlashAction::ShowPermissions,
    },
    CommandSpec {
        name: "/trust",
        aliases: &[],
        args: "current|list|revoke-current",
        description: "show or revoke workspace trust records",
        category: CommandCategory::Trust,
        action: SlashAction::ShowTrust,
    },
    CommandSpec {
        name: "/context",
        aliases: &[],
        args: "",
        description: "show prompt context usage and token sections",
        category: CommandCategory::Trust,
        action: SlashAction::ShowContext,
    },
    CommandSpec {
        name: "/compact",
        aliases: &["/compress", "/compact-context"],
        args: "",
        description: "compact active worker context",
        category: CommandCategory::Trust,
        action: SlashAction::CompactContext,
    },
    CommandSpec {
        name: "/problems",
        aliases: &["/diagnostics"],
        args: "",
        description: "open verifier and failure diagnostics",
        category: CommandCategory::Trust,
        action: SlashAction::ShowProblems,
    },
    CommandSpec {
        name: "/retry",
        aliases: &["/repair"],
        args: "",
        description: "retry the selected failed goal with compact diagnostics",
        category: CommandCategory::Loop,
        action: SlashAction::RetryGoal,
    },
    CommandSpec {
        name: "/why-tokens",
        aliases: &[],
        args: "",
        description: "explain the latest prompt token buckets",
        category: CommandCategory::Trust,
        action: SlashAction::ShowWhyTokens,
    },
    CommandSpec {
        name: "/goals",
        aliases: &["/switch"],
        args: "",
        description: "open the searchable goal switcher",
        category: CommandCategory::Loop,
        action: SlashAction::OpenGoals,
    },
    CommandSpec {
        name: "/focus",
        aliases: &[],
        args: "plan|receipt|problems|code|commands|context|tokens|log",
        description: "switch the Active panel focus view",
        category: CommandCategory::Loop,
        action: SlashAction::SetFocus(FocusView::Receipt),
    },
    CommandSpec {
        name: "/diff",
        aliases: &["/code"],
        args: "",
        description: "open the verified diff/code focus view",
        category: CommandCategory::Loop,
        action: SlashAction::SetFocus(FocusView::Code),
    },
    CommandSpec {
        name: "/copy",
        aliases: &[],
        args: "",
        description: "copy the current focus view to the clipboard",
        category: CommandCategory::Loop,
        action: SlashAction::CopyFocus,
    },
    CommandSpec {
        name: "/rerun",
        aliases: &[],
        args: "",
        description: "rerun the most recent command",
        category: CommandCategory::Shell,
        action: SlashAction::RerunCommand,
    },
    CommandSpec {
        name: "/stats",
        aliases: &[],
        args: "",
        description: "show session token, goal, and command stats",
        category: CommandCategory::Trust,
        action: SlashAction::ShowStats,
    },
    CommandSpec {
        name: "/stop",
        aliases: &["/cancel"],
        args: "",
        description: "cancel the selected running goal",
        category: CommandCategory::Loop,
        action: SlashAction::StopGoal,
    },
    CommandSpec {
        name: "/memory",
        aliases: &[],
        args: "show",
        description: "open inspectable task memory",
        category: CommandCategory::Trust,
        action: SlashAction::OpenMemory,
    },
    CommandSpec {
        name: "/history",
        aliases: &["/sessions", "/resume"],
        args: "",
        description: "open task/session history",
        category: CommandCategory::Session,
        action: SlashAction::OpenHistory,
    },
    CommandSpec {
        name: "/log",
        aliases: &[],
        args: "",
        description: "toggle the Flight Log evidence trail",
        category: CommandCategory::Trust,
        action: SlashAction::ToggleLog,
    },
    CommandSpec {
        name: "/review",
        aliases: &[],
        args: "",
        description: "show review receipt guidance for the latest task",
        category: CommandCategory::Trust,
        action: SlashAction::ShowReview,
    },
    CommandSpec {
        name: "/commands",
        aliases: &["/help", "/?"],
        args: "",
        description: "show commands and keyboard help",
        category: CommandCategory::Loop,
        action: SlashAction::ShowCommands,
    },
    CommandSpec {
        name: "/clear",
        aliases: &[],
        args: "",
        description: "clear the visible local goal list",
        category: CommandCategory::Session,
        action: SlashAction::ClearGoals,
    },
    CommandSpec {
        name: "/delete",
        aliases: &[],
        args: "",
        description: "delete the selected visible goal",
        category: CommandCategory::Session,
        action: SlashAction::DeleteSelectedGoal,
    },
    CommandSpec {
        name: "/quit",
        aliases: &["/exit"],
        args: "",
        description: "save session and ask to exit",
        category: CommandCategory::Session,
        action: SlashAction::Quit,
    },
    CommandSpec {
        name: "/run",
        aliases: &["!"],
        args: "<cmd>",
        description: "run a sandboxed command",
        category: CommandCategory::Shell,
        action: SlashAction::ShowCommands,
    },
];

pub fn parse_slash_command(input: &str) -> SlashParse {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return SlashParse::NotCommand;
    }
    if let Some(rest) = trimmed.strip_prefix('!') {
        return if rest.trim().is_empty() {
            SlashParse::Unknown {
                command: "!".into(),
                suggestion: Some("!<cmd>".into()),
            }
        } else {
            SlashParse::RunCommand
        };
    }
    if !trimmed.starts_with('/') {
        return SlashParse::NotCommand;
    }

    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let head = parts.next().unwrap_or_default();
    let rest = parts.next().unwrap_or_default().trim();

    if head == "/run" {
        return if rest.is_empty() {
            SlashParse::Unknown {
                command: head.into(),
                suggestion: Some("/run <cmd>".into()),
            }
        } else {
            SlashParse::RunCommand
        };
    }

    if head == "/plan" {
        return if rest.is_empty() {
            SlashParse::Unknown {
                command: head.into(),
                suggestion: Some("/plan <goal>".into()),
            }
        } else {
            SlashParse::Command(SlashAction::PreviewPlan(rest.to_string()))
        };
    }

    if head == "/permissions" {
        let mut parts = rest.split_whitespace();
        let subcommand = parts.next().unwrap_or_default();
        if subcommand.is_empty() {
            return SlashParse::Command(SlashAction::ShowPermissions);
        }
        if subcommand != "set" {
            return SlashParse::Unknown {
                command: format!("/permissions {subcommand}"),
                suggestion: Some(
                    "/permissions set ask|read-only|workspace-write|full-access".into(),
                ),
            };
        }
        let mode = parts.next().unwrap_or_default();
        return if let Some(mode) = PermissionMode::parse(mode) {
            SlashParse::Command(SlashAction::SetPermissionMode(mode))
        } else {
            SlashParse::Unknown {
                command: "/permissions set".into(),
                suggestion: Some(
                    "/permissions set ask|read-only|workspace-write|full-access".into(),
                ),
            }
        };
    }

    if head == "/trust" {
        let subcommand = rest.split_whitespace().next().unwrap_or_default();
        return match subcommand {
            "" | "current" | "list" => SlashParse::Command(SlashAction::ShowTrust),
            "revoke-current" => SlashParse::Command(SlashAction::RevokeCurrentTrust),
            other => SlashParse::Unknown {
                command: format!("/trust {other}"),
                suggestion: Some("/trust current|list|revoke-current".into()),
            },
        };
    }

    if head == "/focus" {
        return if let Some(view) = FocusView::parse(rest) {
            SlashParse::Command(SlashAction::SetFocus(view))
        } else {
            SlashParse::Unknown {
                command: "/focus".into(),
                suggestion: Some(
                    "/focus plan|receipt|problems|code|commands|context|tokens|log".into(),
                ),
            }
        };
    }

    if head == "/ask" {
        return if rest.is_empty() {
            SlashParse::Command(SlashAction::AskMode)
        } else {
            SlashParse::Command(SlashAction::SubmitAsk(rest.to_string()))
        };
    }

    if let Some(spec) = find_command(head) {
        return SlashParse::Command(spec.action.clone());
    }

    SlashParse::Unknown {
        command: head.to_string(),
        suggestion: nearest_command(head),
    }
}

pub fn find_command(input: &str) -> Option<&'static CommandSpec> {
    COMMANDS
        .iter()
        .find(|spec| spec.name == input || spec.aliases.contains(&input))
}

pub fn command_suggestions(input: &str) -> Vec<&'static CommandSpec> {
    let query = input
        .trim()
        .trim_start_matches('/')
        .trim_start_matches('!')
        .to_ascii_lowercase();
    if query.is_empty() {
        return COMMANDS.iter().collect();
    }

    let mut matches: Vec<&'static CommandSpec> = COMMANDS
        .iter()
        .filter(|spec| command_matches(spec, &query))
        .collect();
    matches.sort_by_key(|spec| suggestion_rank(spec, &query));
    matches
}

pub fn render_command_label(spec: &CommandSpec) -> String {
    if spec.args.is_empty() {
        format!("{} - {}", spec.name, spec.description)
    } else {
        format!("{} {} - {}", spec.name, spec.args, spec.description)
    }
}

pub fn complete_command_prefix(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') || trimmed.contains(char::is_whitespace) {
        return None;
    }
    let spec = command_suggestions(trimmed).into_iter().next()?;
    if spec.args.is_empty() {
        Some(spec.name.to_string())
    } else {
        Some(format!("{} ", spec.name))
    }
}

pub fn unknown_command_message(command: &str, suggestion: Option<&str>) -> String {
    match suggestion {
        Some(suggestion) => format!("Unknown command `{command}`. Did you mean `{suggestion}`?"),
        None => format!("Unknown command `{command}`. Type `/commands` for available commands."),
    }
}

fn command_matches(spec: &CommandSpec, query: &str) -> bool {
    let name = spec.name.trim_start_matches('/').to_ascii_lowercase();
    let description = spec.description.to_ascii_lowercase();
    name.contains(query)
        || description.contains(query)
        || spec.aliases.iter().any(|alias| {
            alias
                .trim_start_matches('/')
                .trim_start_matches('!')
                .to_ascii_lowercase()
                .contains(query)
        })
}

fn suggestion_rank(spec: &CommandSpec, query: &str) -> usize {
    let name = spec.name.trim_start_matches('/').to_ascii_lowercase();
    if name == query || (query == "r" && spec.name == "/run") {
        0
    } else if name.starts_with(query) {
        1
    } else if spec.aliases.iter().any(|alias| {
        alias
            .trim_start_matches('/')
            .trim_start_matches('!')
            .to_ascii_lowercase()
            .starts_with(query)
    }) {
        2
    } else {
        3
    }
}

fn nearest_command(command: &str) -> Option<String> {
    let normalized = command.trim_start_matches('/').to_ascii_lowercase();
    COMMANDS
        .iter()
        .flat_map(|spec| std::iter::once(spec.name).chain(spec.aliases.iter().copied()))
        .filter(|candidate| candidate.starts_with('/'))
        .map(|candidate| {
            let distance = levenshtein(
                &normalized,
                &candidate.trim_start_matches('/').to_ascii_lowercase(),
            );
            (candidate, distance)
        })
        .filter(|(_, distance)| *distance <= 3)
        .min_by_key(|(_, distance)| *distance)
        .map(|(candidate, _)| candidate.to_string())
}

fn levenshtein(a: &str, b: &str) -> usize {
    let mut costs: Vec<usize> = (0..=b.chars().count()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = i;
        costs[0] = i + 1;
        for (j, cb) in b.chars().enumerate() {
            let old = costs[j + 1];
            let replacement = if ca == cb { prev } else { prev + 1 };
            costs[j + 1] = (costs[j + 1] + 1).min(costs[j] + 1).min(replacement);
            prev = old;
        }
    }
    *costs.last().unwrap_or(&0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_and_config_share_action() {
        assert_eq!(
            parse_slash_command("/settings"),
            SlashParse::Command(SlashAction::OpenSettings)
        );
        assert_eq!(
            parse_slash_command("/config"),
            SlashParse::Command(SlashAction::OpenSettings)
        );
    }

    #[test]
    fn command_suggestions_include_settings_alias() {
        let suggestions = command_suggestions("/sett");

        assert!(suggestions.iter().any(|item| item.name == "/settings"));
        assert!(suggestions
            .iter()
            .any(|item| item.aliases.contains(&"/config")));
    }

    #[test]
    fn slash_prefix_completion_returns_best_command() {
        assert_eq!(complete_command_prefix("/sett"), Some("/settings".into()));
        assert_eq!(complete_command_prefix("/r"), Some("/run ".into()));
    }

    #[test]
    fn context_compact_and_stop_parse_as_commands() {
        assert_eq!(
            parse_slash_command("/context"),
            SlashParse::Command(SlashAction::ShowContext)
        );
        assert_eq!(
            parse_slash_command("/compact"),
            SlashParse::Command(SlashAction::CompactContext)
        );
        assert_eq!(
            parse_slash_command("/compact-context"),
            SlashParse::Command(SlashAction::CompactContext)
        );
        assert_eq!(
            parse_slash_command("/stop"),
            SlashParse::Command(SlashAction::StopGoal)
        );
    }

    #[test]
    fn goal_focus_copy_rerun_stats_and_compress_parse_as_commands() {
        assert_eq!(
            parse_slash_command("/goals"),
            SlashParse::Command(SlashAction::OpenGoals)
        );
        assert_eq!(
            parse_slash_command("/switch"),
            SlashParse::Command(SlashAction::OpenGoals)
        );
        assert_eq!(
            parse_slash_command("/focus code"),
            SlashParse::Command(SlashAction::SetFocus(FocusView::Code))
        );
        assert_eq!(
            parse_slash_command("/focus plan"),
            SlashParse::Command(SlashAction::SetFocus(FocusView::Plan))
        );
        assert_eq!(
            parse_slash_command("/diff"),
            SlashParse::Command(SlashAction::SetFocus(FocusView::Code))
        );
        assert_eq!(
            parse_slash_command("/code"),
            SlashParse::Command(SlashAction::SetFocus(FocusView::Code))
        );
        assert_eq!(
            parse_slash_command("/focus commands"),
            SlashParse::Command(SlashAction::SetFocus(FocusView::Commands))
        );
        assert_eq!(
            parse_slash_command("/focus problems"),
            SlashParse::Command(SlashAction::SetFocus(FocusView::Problems))
        );
        assert_eq!(
            parse_slash_command("/focus context"),
            SlashParse::Command(SlashAction::SetFocus(FocusView::Context))
        );
        assert_eq!(
            parse_slash_command("/focus tokens"),
            SlashParse::Command(SlashAction::SetFocus(FocusView::Tokens))
        );
        assert_eq!(
            parse_slash_command("/copy"),
            SlashParse::Command(SlashAction::CopyFocus)
        );
        assert_eq!(
            parse_slash_command("/rerun"),
            SlashParse::Command(SlashAction::RerunCommand)
        );
        assert_eq!(
            parse_slash_command("/stats"),
            SlashParse::Command(SlashAction::ShowStats)
        );
        assert_eq!(
            parse_slash_command("/compress"),
            SlashParse::Command(SlashAction::CompactContext)
        );
        assert_eq!(
            parse_slash_command("/problems"),
            SlashParse::Command(SlashAction::ShowProblems)
        );
        assert_eq!(
            parse_slash_command("/diagnostics"),
            SlashParse::Command(SlashAction::ShowProblems)
        );
        assert_eq!(
            parse_slash_command("/retry"),
            SlashParse::Command(SlashAction::RetryGoal)
        );
        assert_eq!(
            parse_slash_command("/repair"),
            SlashParse::Command(SlashAction::RetryGoal)
        );
        assert_eq!(
            parse_slash_command("/why-tokens"),
            SlashParse::Command(SlashAction::ShowWhyTokens)
        );
        assert_eq!(
            parse_slash_command("/ask why did verification fail?"),
            SlashParse::Command(SlashAction::SubmitAsk("why did verification fail?".into()))
        );
        assert_eq!(
            parse_slash_command("/ask"),
            SlashParse::Command(SlashAction::AskMode)
        );
        assert_eq!(
            parse_slash_command("/plan build a chess board"),
            SlashParse::Command(SlashAction::PreviewPlan("build a chess board".into()))
        );
        assert_eq!(
            parse_slash_command("/approve"),
            SlashParse::Command(SlashAction::ApprovePlan)
        );
    }

    #[test]
    fn permissions_set_parses_mode() {
        assert_eq!(
            parse_slash_command("/permissions set read-only"),
            SlashParse::Command(SlashAction::SetPermissionMode(
                phonton_types::PermissionMode::ReadOnly
            ))
        );
        assert_eq!(
            parse_slash_command("/permissions set full-access"),
            SlashParse::Command(SlashAction::SetPermissionMode(
                phonton_types::PermissionMode::FullAccess
            ))
        );
    }

    #[test]
    fn trust_commands_parse_without_touching_permissions_aliases() {
        assert_eq!(
            parse_slash_command("/trust"),
            SlashParse::Command(SlashAction::ShowTrust)
        );
        assert_eq!(
            parse_slash_command("/trust list"),
            SlashParse::Command(SlashAction::ShowTrust)
        );
        assert_eq!(
            parse_slash_command("/trust revoke-current"),
            SlashParse::Command(SlashAction::RevokeCurrentTrust)
        );
    }

    #[test]
    fn unknown_command_returns_nearest_suggestion() {
        let parsed = parse_slash_command("/settngs");

        match parsed {
            SlashParse::Unknown { suggestion, .. } => {
                assert_eq!(suggestion.as_deref(), Some("/settings"))
            }
            other => panic!("expected unknown slash command, got {other:?}"),
        }
    }
}
