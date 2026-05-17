use std::path::{Path, PathBuf};
use std::sync::Arc;

use phonton_context::{CharHeuristic, TiktokenCounter, TokenCounter};
use phonton_types::{
    ContextMention, ContextMentionKind, ContextMentionStatus, McpServerDefinition, Permission,
    TrustLevel,
};

const DEFAULT_DIRECTORY_ENTRY_LIMIT: usize = 16;
const MAX_SYMBOL_SCAN_FILES: usize = 500;
const MAX_SYMBOL_SCAN_BYTES: u64 = 512 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct RawContextMention {
    raw: String,
    target: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MentionCapabilityCatalog {
    pub(crate) mcp_servers: Vec<McpMentionServer>,
}

impl MentionCapabilityCatalog {
    pub(crate) fn from_mcp_servers(servers: &[McpServerDefinition]) -> Self {
        Self {
            mcp_servers: servers
                .iter()
                .map(|server| McpMentionServer {
                    id: server.id.as_str().to_string(),
                    enabled: server.enabled,
                    trust: server.trust,
                    permissions: server.permissions.clone(),
                    tools: Vec::new(),
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpMentionServer {
    pub(crate) id: String,
    pub(crate) enabled: bool,
    pub(crate) trust: TrustLevel,
    pub(crate) permissions: Vec<Permission>,
    pub(crate) tools: Vec<String>,
}

pub(crate) struct ContextMentionResolver {
    workspace_root: PathBuf,
    catalog: MentionCapabilityCatalog,
    counter: Arc<dyn TokenCounter>,
    directory_entry_limit: usize,
}

impl ContextMentionResolver {
    pub(crate) fn new(workspace_root: impl AsRef<Path>, catalog: MentionCapabilityCatalog) -> Self {
        let root = workspace_root
            .as_ref()
            .canonicalize()
            .unwrap_or_else(|_| workspace_root.as_ref().to_path_buf());
        let counter: Arc<dyn TokenCounter> = TiktokenCounter::new()
            .map(|counter| Arc::new(counter) as Arc<dyn TokenCounter>)
            .unwrap_or_else(|_| Arc::new(CharHeuristic));
        Self {
            workspace_root: root,
            catalog,
            counter,
            directory_entry_limit: DEFAULT_DIRECTORY_ENTRY_LIMIT,
        }
    }

    pub(crate) fn resolve_text(&self, text: &str) -> Vec<ContextMention> {
        parse_context_mentions(text)
            .into_iter()
            .map(|mention| self.resolve_one(mention))
            .collect()
    }

    fn resolve_one(&self, mention: RawContextMention) -> ContextMention {
        if let Some(symbol) = mention.target.strip_prefix("symbol:") {
            let symbol = symbol.trim().to_string();
            return self.resolve_symbol(mention, &symbol);
        }
        if let Some(mcp) = mention.target.strip_prefix("mcp:") {
            let mcp = mcp.trim().to_string();
            return self.resolve_mcp(mention, &mcp);
        }
        self.resolve_path(mention)
    }

    fn resolve_path(&self, mention: RawContextMention) -> ContextMention {
        let candidate = PathBuf::from(&mention.target);
        let resolved = if candidate.is_absolute() {
            candidate
        } else {
            self.workspace_root.join(candidate)
        };

        let Ok(canonical) = resolved.canonicalize() else {
            return base_mention(
                mention,
                ContextMentionKind::Unknown,
                ContextMentionStatus::Unresolved,
                "attachment",
                Some("path not found in workspace".into()),
            );
        };

        let Ok(metadata) = std::fs::metadata(&canonical) else {
            return base_mention(
                mention,
                ContextMentionKind::Unknown,
                ContextMentionStatus::Unresolved,
                "attachment",
                Some("path metadata could not be read".into()),
            );
        };

        if !canonical.starts_with(&self.workspace_root) {
            let kind = if metadata.is_dir() {
                ContextMentionKind::Directory
            } else if metadata.is_file() {
                ContextMentionKind::File
            } else {
                ContextMentionKind::Unknown
            };
            return base_mention(
                mention,
                kind,
                ContextMentionStatus::PermissionGated,
                "attachment",
                Some("path resolves outside workspace".into()),
            );
        }

        let relative = canonical
            .strip_prefix(&self.workspace_root)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| canonical.clone());
        let label = display_path(&relative);

        if metadata.is_dir() {
            return self.resolve_directory(mention, relative, label, &canonical);
        }

        if metadata.is_file() {
            return self.resolve_file(mention, relative, label, &canonical, metadata.len());
        }

        base_mention(
            mention,
            ContextMentionKind::Unknown,
            ContextMentionStatus::Unsupported,
            "attachment",
            Some("path is neither a file nor a directory".into()),
        )
    }

    fn resolve_file(
        &self,
        mention: RawContextMention,
        relative: PathBuf,
        label: String,
        canonical: &Path,
        size_bytes: u64,
    ) -> ContextMention {
        let text = if size_bytes <= MAX_SYMBOL_SCAN_BYTES {
            std::fs::read_to_string(canonical).unwrap_or_default()
        } else {
            String::new()
        };
        ContextMention {
            raw: mention.raw,
            label,
            kind: ContextMentionKind::File,
            status: ContextMentionStatus::Resolved,
            path: Some(relative),
            symbol: None,
            server: None,
            tool: None,
            source_bucket: "attachment".into(),
            estimated_tokens: Some(self.estimated_tokens(&text).max(1)),
            note: Some(format!("{size_bytes} bytes")),
        }
    }

    fn resolve_directory(
        &self,
        mention: RawContextMention,
        relative: PathBuf,
        label: String,
        canonical: &Path,
    ) -> ContextMention {
        let mut entries = Vec::new();
        if let Ok(read_dir) = std::fs::read_dir(canonical) {
            for entry in read_dir.flatten() {
                entries.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        entries.sort();
        let total = entries.len();
        let summary = entries
            .iter()
            .take(self.directory_entry_limit)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        let note = if total > self.directory_entry_limit {
            format!(
                "directory summary includes {} of {} entries",
                self.directory_entry_limit, total
            )
        } else {
            format!("directory summary includes {total} entries")
        };

        ContextMention {
            raw: mention.raw,
            label,
            kind: ContextMentionKind::Directory,
            status: ContextMentionStatus::Resolved,
            path: Some(relative),
            symbol: None,
            server: None,
            tool: None,
            source_bucket: "attachment".into(),
            estimated_tokens: Some(self.estimated_tokens(&summary).max(1)),
            note: Some(note),
        }
    }

    fn resolve_symbol(&self, mention: RawContextMention, symbol: &str) -> ContextMention {
        if symbol.is_empty() {
            return base_mention(
                mention,
                ContextMentionKind::Symbol,
                ContextMentionStatus::Unresolved,
                "code",
                Some("symbol mention is empty".into()),
            );
        }

        if let Some(path) = find_symbol_path(&self.workspace_root, symbol) {
            let label = format!("symbol:{symbol}");
            let note = format!("symbol found in {}", display_path(&path));
            return ContextMention {
                raw: mention.raw,
                label,
                kind: ContextMentionKind::Symbol,
                status: ContextMentionStatus::Resolved,
                path: Some(path),
                symbol: Some(symbol.into()),
                server: None,
                tool: None,
                source_bucket: "code".into(),
                estimated_tokens: Some(self.estimated_tokens(symbol).max(1)),
                note: Some(note),
            };
        }

        base_mention(
            mention,
            ContextMentionKind::Symbol,
            ContextMentionStatus::Unresolved,
            "code",
            Some("symbol not found in scanned workspace files".into()),
        )
    }

    fn resolve_mcp(&self, mention: RawContextMention, target: &str) -> ContextMention {
        let (server_id, tool_name) = target
            .split_once('/')
            .map(|(server, tool)| (server.trim(), Some(tool.trim())))
            .unwrap_or((target.trim(), None));
        let kind = if tool_name.is_some() {
            ContextMentionKind::McpTool
        } else {
            ContextMentionKind::McpServer
        };
        if server_id.is_empty() {
            return base_mention(
                mention,
                kind,
                ContextMentionStatus::Unresolved,
                "mcp",
                Some("MCP server id is empty".into()),
            );
        }

        let Some(server) = self
            .catalog
            .mcp_servers
            .iter()
            .find(|server| server.id == server_id)
        else {
            return ContextMention {
                raw: mention.raw,
                label: target.into(),
                kind,
                status: ContextMentionStatus::Unresolved,
                path: None,
                symbol: None,
                server: Some(server_id.into()),
                tool: tool_name.map(str::to_string),
                source_bucket: "mcp".into(),
                estimated_tokens: None,
                note: Some("MCP server is not configured".into()),
            };
        };

        if !server.enabled {
            return ContextMention {
                raw: mention.raw,
                label: target.into(),
                kind,
                status: ContextMentionStatus::Unsupported,
                path: None,
                symbol: None,
                server: Some(server_id.into()),
                tool: tool_name.map(str::to_string),
                source_bucket: "mcp".into(),
                estimated_tokens: None,
                note: Some("MCP server is disabled".into()),
            };
        }

        if let Some(tool) = tool_name {
            if tool.is_empty()
                || (!server.tools.is_empty()
                    && !server.tools.iter().any(|candidate| candidate == tool))
            {
                return ContextMention {
                    raw: mention.raw,
                    label: target.into(),
                    kind,
                    status: ContextMentionStatus::Unresolved,
                    path: None,
                    symbol: None,
                    server: Some(server_id.into()),
                    tool: Some(tool.into()),
                    source_bucket: "mcp".into(),
                    estimated_tokens: None,
                    note: Some("MCP tool is not declared by the server".into()),
                };
            }
        }

        let gated = requires_mcp_approval(server.trust, &server.permissions);
        let status = if gated {
            ContextMentionStatus::PermissionGated
        } else {
            ContextMentionStatus::Resolved
        };
        let metadata_note = if tool_name.is_some() && server.tools.is_empty() {
            "; tool list not advertised in extension metadata"
        } else {
            ""
        };
        let note = if gated {
            Some(format!(
                "{} MCP capability requires approval{}",
                server.trust, metadata_note
            ))
        } else {
            Some(format!("{} MCP capability{}", server.trust, metadata_note))
        };

        ContextMention {
            raw: mention.raw,
            label: target.into(),
            kind,
            status,
            path: None,
            symbol: None,
            server: Some(server_id.into()),
            tool: tool_name.map(str::to_string),
            source_bucket: "mcp".into(),
            estimated_tokens: Some(self.estimated_tokens(target).max(1)),
            note,
        }
    }

    fn estimated_tokens(&self, text: &str) -> u64 {
        self.counter.count(text) as u64
    }
}

fn parse_context_mentions(text: &str) -> Vec<RawContextMention> {
    let mut mentions = Vec::new();
    let mut iter = text.char_indices().peekable();

    while let Some((at_idx, ch)) = iter.next() {
        if ch != '@' {
            continue;
        }

        let Some(&(next_idx, next_ch)) = iter.peek() else {
            continue;
        };

        let (raw, target) = if next_ch == '"' || next_ch == '\'' {
            let quote = next_ch;
            iter.next();
            let start = next_idx + quote.len_utf8();
            let mut end = start;
            let mut raw_end = start;
            for (idx, c) in iter.by_ref() {
                if c == quote {
                    raw_end = idx + c.len_utf8();
                    break;
                }
                end = idx + c.len_utf8();
                raw_end = end;
            }
            (
                text[at_idx..raw_end].to_string(),
                text[start..end].trim().to_string(),
            )
        } else if next_ch == '[' {
            iter.next();
            let start = next_idx + next_ch.len_utf8();
            let mut end = start;
            let mut raw_end = start;
            for (idx, c) in iter.by_ref() {
                if c == ']' {
                    raw_end = idx + c.len_utf8();
                    break;
                }
                end = idx + c.len_utf8();
                raw_end = end;
            }
            (
                text[at_idx..raw_end].to_string(),
                text[start..end].trim().to_string(),
            )
        } else {
            let start = next_idx;
            let mut end = text.len();
            while let Some(&(idx, c)) = iter.peek() {
                if c.is_whitespace() || matches!(c, ',' | ';' | ')' | '(' | '<' | '>' | '`') {
                    end = idx;
                    break;
                }
                iter.next();
            }
            let target = text[start..end]
                .trim_matches(|c: char| matches!(c, '.' | '!' | '?' | ']' | '}'))
                .trim()
                .to_string();
            (format!("@{target}"), target)
        };

        if !target.is_empty() {
            mentions.push(RawContextMention { raw, target });
        }
    }

    mentions
}

pub(crate) fn render_context_mentions(mentions: &[ContextMention]) -> String {
    if mentions.is_empty() {
        return String::from("Context mentions: none");
    }

    let mut out = String::from("Context mentions\n");
    for mention in mentions {
        let status = match mention.status {
            ContextMentionStatus::Resolved => "resolved",
            ContextMentionStatus::Unresolved => "missing",
            ContextMentionStatus::Unsupported => "unsupported",
            ContextMentionStatus::PermissionGated => "gated",
        };
        let kind = match mention.kind {
            ContextMentionKind::File => "file",
            ContextMentionKind::Directory => "directory",
            ContextMentionKind::Symbol => "symbol",
            ContextMentionKind::McpServer => "mcp_server",
            ContextMentionKind::McpTool => "mcp_tool",
            ContextMentionKind::Unknown => "unknown",
        };
        let tokens = mention
            .estimated_tokens
            .map(|tokens| format!("{tokens} tok"))
            .unwrap_or_else(|| "-".into());
        let note = mention.note.as_deref().unwrap_or("");
        out.push_str(&format!(
            "  {status:<10} {raw:<28} {kind:<10} {tokens:<10} {note}\n",
            raw = mention.raw
        ));
    }
    out
}

fn base_mention(
    mention: RawContextMention,
    kind: ContextMentionKind,
    status: ContextMentionStatus,
    source_bucket: &str,
    note: Option<String>,
) -> ContextMention {
    ContextMention {
        raw: mention.raw,
        label: mention.target,
        kind,
        status,
        path: None,
        symbol: None,
        server: None,
        tool: None,
        source_bucket: source_bucket.into(),
        estimated_tokens: None,
        note,
    }
}

fn requires_mcp_approval(trust: TrustLevel, permissions: &[Permission]) -> bool {
    matches!(trust, TrustLevel::MutatingTool | TrustLevel::NetworkedTool)
        || permissions
            .iter()
            .any(|permission| *permission != Permission::FsReadWorkspace)
}

fn find_symbol_path(workspace_root: &Path, symbol: &str) -> Option<PathBuf> {
    let mut stack = vec![workspace_root.to_path_buf()];
    let mut scanned = 0usize;

    while let Some(dir) = stack.pop() {
        let Ok(read_dir) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if should_skip_entry(&name) {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if !is_symbol_scan_file(&path) {
                continue;
            }
            scanned += 1;
            if scanned > MAX_SYMBOL_SCAN_FILES {
                return None;
            }
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            if metadata.len() > MAX_SYMBOL_SCAN_BYTES {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if text.contains(symbol) {
                return path
                    .strip_prefix(workspace_root)
                    .map(Path::to_path_buf)
                    .ok();
            }
        }
    }

    None
}

fn should_skip_entry(name: &str) -> bool {
    matches!(
        name,
        ".git" | "target" | "node_modules" | "dist" | ".next" | ".phonton"
    )
}

fn is_symbol_scan_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "rs" | "ts"
                    | "tsx"
                    | "js"
                    | "jsx"
                    | "py"
                    | "go"
                    | "java"
                    | "kt"
                    | "swift"
                    | "c"
                    | "cc"
                    | "cpp"
                    | "h"
                    | "hpp"
                    | "md"
                    | "toml"
                    | "json"
            )
        })
        .unwrap_or(false)
}

fn display_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use anyhow::Result;
    use phonton_types::{ContextMentionKind, ContextMentionStatus, Permission, TrustLevel};

    use super::*;

    #[test]
    fn parses_supported_context_mention_forms() {
        let mentions = parse_context_mentions(
            r#"use @src/lib.rs @docs/ @"notes file.md" @[path with spaces.md] @symbol:GoalContract @mcp:filesystem @mcp:github/create_issue"#,
        );

        let targets: Vec<_> = mentions
            .iter()
            .map(|mention| mention.target.as_str())
            .collect();
        assert_eq!(
            targets,
            vec![
                "src/lib.rs",
                "docs/",
                "notes file.md",
                "path with spaces.md",
                "symbol:GoalContract",
                "mcp:filesystem",
                "mcp:github/create_issue"
            ]
        );
        assert_eq!(mentions[2].raw, r#"@"notes file.md""#);
        assert_eq!(mentions[3].raw, "@[path with spaces.md]");
    }

    #[test]
    fn resolves_files_directories_symbols_and_mcp_mentions() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let src = temp.path().join("src");
        let docs = temp.path().join("docs");
        std::fs::create_dir_all(&src)?;
        std::fs::create_dir_all(&docs)?;
        std::fs::write(src.join("lib.rs"), "pub struct GoalContract;\n")?;
        std::fs::write(docs.join("intro.md"), "# Intro\n")?;

        let catalog = MentionCapabilityCatalog {
            mcp_servers: vec![
                McpMentionServer {
                    id: "filesystem".into(),
                    enabled: true,
                    trust: TrustLevel::ReadOnlyTool,
                    permissions: vec![Permission::FsReadWorkspace],
                    tools: vec!["read_file".into()],
                },
                McpMentionServer {
                    id: "github".into(),
                    enabled: true,
                    trust: TrustLevel::NetworkedTool,
                    permissions: vec![Permission::NetworkRequest],
                    tools: vec!["create_issue".into()],
                },
            ],
        };
        let resolver = ContextMentionResolver::new(temp.path(), catalog);

        let mentions = resolver.resolve_text(
            "inspect @src/lib.rs @docs/ @symbol:GoalContract @mcp:filesystem/read_file @mcp:github/create_issue",
        );

        assert_eq!(mentions[0].kind, ContextMentionKind::File);
        assert_eq!(mentions[0].status, ContextMentionStatus::Resolved);
        assert_eq!(mentions[0].path.as_deref(), Some(Path::new("src/lib.rs")));
        assert!(mentions[0].estimated_tokens.unwrap_or_default() > 0);

        assert_eq!(mentions[1].kind, ContextMentionKind::Directory);
        assert_eq!(mentions[1].status, ContextMentionStatus::Resolved);
        assert!(mentions[1]
            .note
            .as_deref()
            .unwrap_or_default()
            .contains("1 entries"));

        assert_eq!(mentions[2].kind, ContextMentionKind::Symbol);
        assert_eq!(mentions[2].status, ContextMentionStatus::Resolved);
        assert_eq!(mentions[2].symbol.as_deref(), Some("GoalContract"));

        assert_eq!(mentions[3].kind, ContextMentionKind::McpTool);
        assert_eq!(mentions[3].status, ContextMentionStatus::Resolved);
        assert_eq!(mentions[3].server.as_deref(), Some("filesystem"));
        assert_eq!(mentions[3].tool.as_deref(), Some("read_file"));

        assert_eq!(mentions[4].kind, ContextMentionKind::McpTool);
        assert_eq!(mentions[4].status, ContextMentionStatus::PermissionGated);
        assert_eq!(mentions[4].server.as_deref(), Some("github"));
        assert_eq!(mentions[4].tool.as_deref(), Some("create_issue"));
        Ok(())
    }

    #[test]
    fn unresolved_and_outside_workspace_mentions_are_visible() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let outside = tempfile::NamedTempFile::new()?;
        let outside_path = outside.path().display().to_string();
        let resolver =
            ContextMentionResolver::new(temp.path(), MentionCapabilityCatalog::default());

        let mentions = resolver.resolve_text(&format!(
            "use @missing.rs @mcp:unknown/tool @{}",
            outside_path
        ));

        assert_eq!(mentions[0].kind, ContextMentionKind::Unknown);
        assert_eq!(mentions[0].status, ContextMentionStatus::Unresolved);
        assert!(mentions[0]
            .note
            .as_deref()
            .unwrap_or_default()
            .contains("not found"));

        assert_eq!(mentions[1].kind, ContextMentionKind::McpTool);
        assert_eq!(mentions[1].status, ContextMentionStatus::Unresolved);

        assert_eq!(mentions[2].status, ContextMentionStatus::PermissionGated);
        assert!(mentions[2]
            .note
            .as_deref()
            .unwrap_or_default()
            .contains("outside workspace"));
        Ok(())
    }

    #[test]
    fn renders_compact_preflight_rows() -> Result<()> {
        let temp = tempfile::tempdir()?;
        std::fs::write(temp.path().join("README.md"), "# Project\n")?;
        let resolver =
            ContextMentionResolver::new(temp.path(), MentionCapabilityCatalog::default());
        let mentions = resolver.resolve_text("fix @README.md and @missing.rs");

        let rendered = render_context_mentions(&mentions);

        assert!(rendered.contains("Context mentions"));
        assert!(rendered.contains("resolved"));
        assert!(rendered.contains("@README.md"));
        assert!(rendered.contains("missing"));
        assert!(rendered.contains("@missing.rs"));
        Ok(())
    }

    #[test]
    fn normalizes_relative_paths_with_platform_separators() -> Result<()> {
        let temp = tempfile::tempdir()?;
        std::fs::create_dir(temp.path().join("src"))?;
        std::fs::write(temp.path().join("src").join("main.rs"), "fn main() {}\n")?;
        let resolver =
            ContextMentionResolver::new(temp.path(), MentionCapabilityCatalog::default());

        let mentions = resolver.resolve_text("open @src/main.rs");

        assert_eq!(mentions[0].path, Some(PathBuf::from("src").join("main.rs")));
        assert_eq!(mentions[0].label, "src/main.rs");
        Ok(())
    }
}
