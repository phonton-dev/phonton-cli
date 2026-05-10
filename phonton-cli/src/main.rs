#![allow(clippy::too_many_arguments)]
#![allow(clippy::await_holding_lock)]

//! Terminal entry point — Ratatui task board for Phonton.
//!
//! Three-pane layout, Goal/Task/Ask modes, live `GlobalState` streamed from
//! the orchestrator over a `watch` channel:
//!
//! ```text
//! ┌─ Goals ──────────┬─ Active subtasks / verify log ───────────────────┐
//! │ ▸ goal one       │ [running]  Implement parse_callsites  (Cheap)    │
//! │ ▪ goal two       │ [verifying] attempt 2                            │
//! │                  │ [done]     Write integration tests for ...       │
//! │                  │                                                  │
//! │                  │ tokens: 1.2k / budget ∞  |  baseline 5.0k  (-76%)│
//! ├──────────────────┴──────────────────────────────────────────────────┤
//! │ goal › _                                                            │
//! └─────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! Modes:
//! - **Goal** (default): Enter queues the typed goal for planning + running.
//! - **Task**: single-subtask fast path — reuses the same orchestration spine.
//! - **Ask**: `Ctrl+;` toggles a side panel for stateless Q&A that does
//!   *not* touch active-goal context. (The spec calls for `Cmd+;`; on
//!   POSIX terminals we bind the equivalent Ctrl chord, which most
//!   keymaps surface the same way.)
//!
//! The CLI owns an orchestrator handle and a stub [`WorkerDispatcher`] that
//! produces a trivial diff per subtask — this is intentional. Wiring a real
//! provider is a configuration choice the user makes via
//! Provider configuration is loaded from `~/.phonton/config.toml` (see
//! [`config::load`]) and falls back to environment variables when the file
//! is absent. The contract the TUI depends on is the
//! `watch::Receiver<GlobalState>`.

mod command_runner;
mod config;
mod contract_preflight;
mod demo;
mod doctor;
mod extensions_cli;
mod focus;
mod mcp_cli;
mod memory_cli;
mod plan_preview;
mod prompt_buffer;
mod review;
mod run_command;
mod trust;
mod tui_commands;

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use async_trait::async_trait;
use base64::{engine::general_purpose, Engine as _};
use command_runner::{parse_prompt_command, summarize_output_with_duration, CommandRunSummary};
use contract_preflight::apply_workspace_preflight;
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use focus::{
    append_code_focus_lines, append_command_run_lines, append_focus_tabs, append_log_focus_lines,
    append_problems_focus_lines, code_focus_text, commands_focus_text, compact_problem_diagnostics,
    focused_file_count, goal_failure_kind, log_focus_text, problem_diagnostics,
    problems_focus_text, receipt_focus_text,
};
use phonton_diff::DiffApplier;
use phonton_extensions::{load_extensions, DiagnosticSeverity, ExtensionLoadOptions, ExtensionSet};
use phonton_mcp::{McpApprovalDecision, McpApprovalRequest, McpApprover};
use phonton_orchestrator::{BudgetGuard, Orchestrator, WorkerDispatcher};
use phonton_planner::{decompose_with_memory_store, Goal};
use phonton_providers::{
    discover_models, pick_default_from_list, provider_for, select_best_working_model, Provider,
};
use phonton_sandbox::{ExecutionGuard, Sandbox};
use phonton_store::{Store, TaskRecord};
use phonton_types::{
    BudgetLimits, ContextManifest, CoverageSummary, DiffHunk, DiffLine, EventRecord, ExtensionId,
    GlobalState, HandoffPacket, MemoryRecord, ModelPricing, ModelTier, OrchestratorEvent,
    OrchestratorMessage, OutcomeLedger, Permission, PermissionLedger, PermissionMode,
    PlannerOutput, PromptAttachment, PromptAttachmentKind, PromptContextManifest,
    ProviderConfig as ApiProviderConfig, ProviderKind, SessionGoalSnapshot, SessionSnapshot,
    SessionTotals, Subtask, SubtaskId, SubtaskResult, SubtaskStatus, TaskId, TaskStatus,
    TokenUsage, VerifyLayer, VerifyResult,
};
use prompt_buffer::PromptBuffer;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tui_commands::{
    command_suggestions, complete_command_prefix, parse_slash_command, render_command_label,
    unknown_command_message, FocusView, SlashAction, SlashParse,
};

// ---------------------------------------------------------------------------
// Visual identity
// ---------------------------------------------------------------------------

// Curated palette - cool slate base with cyan/violet/magenta accents.
const ACCENT: Color = Color::Rgb(99, 179, 237); // cyan-300
const ACCENT_HI: Color = Color::Rgb(160, 215, 250); // cyan-200 highlight
const SUCCESS: Color = Color::Rgb(72, 199, 142);
const WARN: Color = Color::Rgb(246, 173, 85);
const DANGER: Color = Color::Rgb(252, 129, 74);
const MUTED: Color = Color::Rgb(113, 128, 150);
const DIM: Color = Color::Rgb(74, 85, 104);
const BG_PANEL: Color = Color::Rgb(26, 32, 44);
const BG_DEEP: Color = Color::Rgb(18, 22, 33);
const VIOLET: Color = Color::Rgb(159, 122, 234);
#[allow(dead_code)]
const PINK: Color = Color::Rgb(237, 100, 166);

// Gradient endpoints used by the logo / accents.
const GRAD_A: (u8, u8, u8) = (99, 179, 237); // cyan
const GRAD_B: (u8, u8, u8) = (159, 122, 234); // violet
const GRAD_C: (u8, u8, u8) = (237, 100, 166); // pink
const GRAD_D: (u8, u8, u8) = (69, 144, 255); // electric blue
const LOGO_GLOW: (u8, u8, u8) = (209, 232, 255);
const LOGO_SHADOW: (u8, u8, u8) = (42, 48, 82);

const UI_TICK_MS: u64 = 80;
const LOGO_SHIMMER_SPEED: f32 = 0.026;
const LOGO_ROW_PHASE: f32 = 0.11;
const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

const LOGO: &[&str] = &[
    "██████╗ ██╗  ██╗ ██████╗ ███╗   ██╗████████╗ ██████╗ ███╗   ██╗",
    "██╔══██╗██║  ██║██╔═══██╗████╗  ██║╚══██╔══╝██╔═══██╗████╗  ██║",
    "██████╔╝███████║██║   ██║██╔██╗ ██║   ██║   ██║   ██║██╔██╗ ██║",
    "██╔═══╝ ██╔══██║██║   ██║██║╚██╗██║   ██║   ██║   ██║██║╚██╗██║",
    "██║     ██║  ██║╚██████╔╝██║ ╚████║   ██║   ╚██████╔╝██║ ╚████║",
    "╚═╝     ╚═╝  ╚═╝ ╚═════╝ ╚═╝  ╚═══╝   ╚═╝    ╚═════╝ ╚═╝  ╚═══╝",
    "  ░▒▓█████████████████████████████████████████████████████▓▒░  ",
];

const LOGO_WIDTH_THRESHOLD: u16 = 70;
static NEXT_MCP_APPROVAL_ID: AtomicU64 = AtomicU64::new(1);

/// Options that control interactive TUI launch behavior.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LaunchOptions {
    /// Resume the latest saved session snapshot for the current workspace.
    pub resume_last_session: bool,
}

// ---------------------------------------------------------------------------
// Visual helpers — gradient + pill primitives
// ---------------------------------------------------------------------------

#[inline]
fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    let v = a as f32 + (b as f32 - a as f32) * t.clamp(0.0, 1.0);
    v.round().clamp(0.0, 255.0) as u8
}

/// Linearly interpolate between two RGB colors.
fn grad(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> Color {
    Color::Rgb(
        lerp_u8(a.0, b.0, t),
        lerp_u8(a.1, b.1, t),
        lerp_u8(a.2, b.2, t),
    )
}

/// Three-stop gradient (a → b → c) sampled at t ∈ [0, 1].
fn grad3(t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        grad(GRAD_A, GRAD_B, t * 2.0)
    } else {
        grad(GRAD_B, GRAD_C, (t - 0.5) * 2.0)
    }
}

/// Four-stop animated logo palette: violet -> pink -> electric blue -> cyan,
/// looping back to violet so the cycle is seamless.
fn logo_grad(t: f32) -> Color {
    let t = t - t.floor();
    let stops = [GRAD_B, GRAD_C, GRAD_D, GRAD_A, GRAD_B];
    let seg = t * 4.0;
    let i = (seg as usize).min(3);
    grad(stops[i], stops[i + 1], seg - i as f32)
}

/// Build a horizontally-gradient-colored line from `text`. `phase` shifts the
/// gradient to produce a subtle shimmer when called per frame.
fn gradient_line(text: &str, phase: f32, bold: bool) -> Line<'static> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len().max(1) as f32;
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(chars.len());
    let modifier = if bold {
        Modifier::BOLD
    } else {
        Modifier::empty()
    };
    for (i, ch) in chars.into_iter().enumerate() {
        if ch == ' ' {
            spans.push(Span::raw(" "));
            continue;
        }
        let mut t = (i as f32) / n + phase;
        t = t - t.floor();
        let color = grad3(t);
        spans.push(Span::styled(
            ch.to_string(),
            Style::default().fg(color).add_modifier(modifier),
        ));
    }
    Line::from(spans)
}

fn logo_line(text: &str, phase: f32, row_idx: usize) -> Line<'static> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len().max(1) as f32;
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(chars.len());
    let wave_a = (phase + row_idx as f32 * LOGO_ROW_PHASE).fract();
    let wave_b = (phase * 1.6 - row_idx as f32 * 0.045 + 0.37).fract();

    for (i, ch) in chars.into_iter().enumerate() {
        if ch == ' ' {
            spans.push(Span::raw(" "));
            continue;
        }

        let x = i as f32 / n;
        let base = logo_grad((x * 0.9 + phase * 0.8 + row_idx as f32 * 0.05).fract());
        let base_color = base_rgb(base);
        let dist = |w: f32| -> f32 {
            let raw = (x - w).abs();
            raw.min(1.0 - raw)
        };
        let d_a = dist(wave_a);
        let d_b = dist(wave_b);

        let style = match ch {
            '░' | '▒' | '▓' => {
                let body = match ch {
                    '▓' => 0.55,
                    '▒' => 0.32,
                    _ => 0.16,
                };
                let glow = if d_a < 0.14 {
                    (1.0 - d_a / 0.14) * 0.45
                } else {
                    0.0
                };
                Style::default().fg(grad(LOGO_SHADOW, base_color, (body + glow).clamp(0.0, 0.9)))
            }
            '╗' | '╔' | '╝' | '╚' | '║' | '═' => {
                let darkened = grad(LOGO_SHADOW, base_color, 0.6);
                let lift = if d_a < 0.07 {
                    (1.0 - d_a / 0.07) * 0.35
                } else {
                    0.0
                };
                Style::default()
                    .fg(grad(base_rgb(darkened), LOGO_GLOW, lift))
                    .add_modifier(Modifier::BOLD)
            }
            _ => {
                let glint_a = if d_a < 0.08 {
                    (1.0 - d_a / 0.08) * 0.65
                } else {
                    0.0
                };
                let glint_b = if d_b < 0.05 {
                    (1.0 - d_b / 0.05) * 0.45
                } else {
                    0.0
                };
                let breathing = ((phase * std::f32::consts::TAU
                    + x * std::f32::consts::TAU * 1.4
                    + row_idx as f32 * 0.65)
                    .sin()
                    + 1.0)
                    * 0.08;
                Style::default()
                    .fg(grad(
                        base_color,
                        LOGO_GLOW,
                        (glint_a + glint_b + breathing).clamp(0.0, 0.78),
                    ))
                    .add_modifier(Modifier::BOLD)
            }
        };
        spans.push(Span::styled(ch.to_string(), style));
    }

    Line::from(spans)
}

fn base_rgb(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => GRAD_B,
    }
}

/// Render text as a "pill" — small inline badge with a colored bg.
fn pill(text: &str, bg: Color, fg: Color) -> Span<'static> {
    Span::styled(
        format!(" {} ", text),
        Style::default().bg(bg).fg(fg).add_modifier(Modifier::BOLD),
    )
}

/// Build a unicode progress bar of `width` cells, filled `filled_frac` of the
/// way through with the cyan-to-violet gradient.
fn gradient_bar(filled_frac: f32, width: usize) -> Vec<Span<'static>> {
    let frac = filled_frac.clamp(0.0, 1.0);
    let total_eighths = (frac * (width as f32) * 8.0).round() as usize;
    let full = total_eighths / 8;
    let rem = total_eighths % 8;
    let partials = ['▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let mut spans = Vec::with_capacity(width);
    for i in 0..width {
        let t = if width <= 1 {
            0.0
        } else {
            i as f32 / (width as f32 - 1.0)
        };
        let color = grad3(t);
        if i < full {
            spans.push(Span::styled("█".to_string(), Style::default().fg(color)));
        } else if i == full && rem > 0 {
            let ch = partials[rem - 1];
            spans.push(Span::styled(ch.to_string(), Style::default().fg(color)));
        } else {
            spans.push(Span::styled("·".to_string(), Style::default().fg(DIM)));
        }
    }
    spans
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

/// Interaction mode. Maps 1:1 to the positioning-document's task-board
/// vocabulary: goal mode is the default, ask mode is a side channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Typing a goal to decompose + run.
    Goal,
    /// Typing a direct single subtask (same orchestration path).
    Task,
    /// Side-channel Q&A — isolated context, doesn't touch goals.
    Ask,
    /// Settings screen.
    Settings,
    /// Browse local cross-session memory.
    Memory,
    /// Browse recent task history.
    History,
    /// Command palette for quick actions.
    CommandPalette,
}

/// Lightweight ambient status for the local semantic index / Nexus config.
#[derive(Debug, Clone, Default)]
pub struct NexusStatus {
    /// True when a `nexus.json` was discovered at or above the workspace.
    pub active: bool,
    /// Number of sibling repos declared by the discovered config.
    pub repo_count: usize,
    /// Human-facing status or error detail.
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsField {
    Provider,
    Model,
    ApiKey,
    AccountId,
    BaseUrl,
    MaxTokens,
    MaxUsdCents,
}

#[derive(Debug, Clone, Default)]
pub struct ModelPickerState {
    /// Full list fetched from the provider's models endpoint.
    pub all_models: Vec<String>,
    /// Subset matching the live filter text.
    pub filtered: Vec<String>,
    /// Cursor within `filtered`.
    pub selected: usize,
    /// Scroll offset for the visible window.
    pub scroll: usize,
    /// Typing in the picker filters by this string.
    pub filter: String,
    /// True while the background fetch is in-flight.
    pub loading: bool,
}

impl ModelPickerState {
    pub fn rebuild_filter(&mut self) {
        let lc = self.filter.to_lowercase();
        self.filtered = if lc.is_empty() {
            self.all_models.clone()
        } else {
            self.all_models
                .iter()
                .filter(|m| m.to_lowercase().contains(&lc))
                .cloned()
                .collect()
        };
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
        self.scroll = self.scroll.min(self.selected);
    }
}

#[derive(Debug, Clone, Default)]
pub struct GoalSwitcherState {
    pub open: bool,
    pub filter: String,
    pub selected: usize,
}

impl GoalSwitcherState {
    pub fn filtered_indices(&self, goals: &[GoalEntry]) -> Vec<usize> {
        let query = self.filter.trim().to_ascii_lowercase();
        goals
            .iter()
            .enumerate()
            .filter_map(|(idx, goal)| {
                if query.is_empty()
                    || goal.description.to_ascii_lowercase().contains(&query)
                    || goal_status_label(&goal.status).contains(&query)
                {
                    Some(idx)
                } else {
                    None
                }
            })
            .collect()
    }

    fn clamp(&mut self, goals: &[GoalEntry]) {
        self.selected = self
            .selected
            .min(self.filtered_indices(goals).len().saturating_sub(1));
    }
}

#[derive(Debug, Clone)]
pub struct SettingsState {
    pub active_field: SettingsField,
    pub provider: String,
    pub model: String,
    pub api_key: String,
    pub account_id: String,
    pub base_url: String,
    pub max_tokens: String,
    pub max_usd_cents: String,
    pub permission_mode: PermissionMode,
    pub message: Option<String>,
    /// Whether the model picker overlay is visible.
    pub picker_open: bool,
    pub picker: ModelPickerState,
    /// Status of the last model validation: None = untested,
    /// Some(true) = passed, Some(false) = failed.
    pub model_ok: Option<bool>,
}

impl SettingsState {
    pub fn new(cfg: &crate::config::Config) -> Self {
        Self {
            active_field: SettingsField::Provider,
            provider: cfg.provider.name.clone(),
            model: cfg.provider.model.clone().unwrap_or_default(),
            api_key: cfg.provider.api_key.clone().unwrap_or_default(),
            account_id: cfg.provider.account_id.clone().unwrap_or_default(),
            base_url: cfg.provider.base_url.clone().unwrap_or_default(),
            max_tokens: cfg
                .budget
                .max_tokens
                .map(|t| t.to_string())
                .unwrap_or_default(),
            max_usd_cents: cfg
                .budget
                .max_usd_cents
                .map(|c| c.to_string())
                .unwrap_or_default(),
            permission_mode: cfg.permissions.mode,
            message: None,
            picker_open: false,
            picker: ModelPickerState::default(),
            model_ok: None,
        }
    }
}

fn non_empty_setting(value: &str) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn apply_settings_to_config(settings: &SettingsState, cfg: &mut config::Config) {
    cfg.provider.name = settings.provider.clone();
    cfg.provider.model = non_empty_setting(&settings.model);
    cfg.provider.api_key = non_empty_setting(&settings.api_key);
    cfg.provider.account_id = non_empty_setting(&settings.account_id);
    cfg.provider.base_url = non_empty_setting(&settings.base_url);
    cfg.budget.max_tokens = settings.max_tokens.parse().ok();
    cfg.budget.max_usd_cents = settings.max_usd_cents.parse().ok();
    cfg.permissions.mode = settings.permission_mode;
}

/// A queued or running top-level goal entry in the left panel.
#[derive(Debug, Clone)]
pub struct GoalEntry {
    /// Free-form goal text the user typed.
    pub description: String,
    /// Latest status snapshot seen on the watch channel for this goal.
    pub status: TaskStatus,
    /// Most recent `GlobalState` snapshot, if any — drives the centre pane.
    pub state: Option<GlobalState>,
    /// Stable task id — used to correlate Flight Log events with the goal.
    pub task_id: TaskId,
    /// Every [`EventRecord`] observed for this goal, oldest first.
    pub flight_log: Vec<EventRecord>,
    /// Index into `state.checkpoints` the user is hovering over in the
    /// checkpoint picker. `None` when the picker has no focus.
    pub checkpoint_cursor: Option<usize>,
}

/// Render-safe view of one MCP approval request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingMcpApproval {
    /// Unique request id owned by the TUI approval bridge.
    pub id: u64,
    /// Goal index that triggered this request.
    pub goal_index: usize,
    /// MCP server id.
    pub server_id: ExtensionId,
    /// Tool name, or `server/start` when the server process itself needs consent.
    pub tool_name: String,
    /// Permissions declared by the server.
    pub permissions: Vec<Permission>,
    /// Human-readable approval reason from the guard/runtime.
    pub reason: String,
}

impl PendingMcpApproval {
    fn from_request(id: u64, goal_index: usize, request: McpApprovalRequest) -> Self {
        Self {
            id,
            goal_index,
            server_id: request.server_id,
            tool_name: request.tool_name,
            permissions: request.permissions,
            reason: request.reason,
        }
    }
}

impl GoalEntry {
    fn new(description: String) -> Self {
        Self {
            description,
            status: TaskStatus::Queued,
            state: None,
            task_id: TaskId::new(),
            flight_log: Vec::new(),
            checkpoint_cursor: None,
        }
    }
}

/// Complete TUI app state.
///
/// Deliberately owns no terminal/IO handles — pure data, so it can be
/// rendered in unit tests via [`ratatui::backend::TestBackend`] without
/// touching a real terminal.
#[derive(Debug, Clone)]
pub struct App {
    /// Active mode — drives the input bar legend and Enter semantics.
    pub mode: Mode,
    /// Goal list, in insertion order. Index 0 is the newest.
    pub goals: Vec<GoalEntry>,
    /// The goal currently highlighted in the left pane (index into `goals`).
    pub selected: usize,
    /// Goal-bar input buffer, including collapsed paste artifacts.
    pub goal_prompt: PromptBuffer,
    /// Ask-mode input buffer, preserved across mode toggles.
    pub ask_prompt: PromptBuffer,
    /// Most recent ask-mode answer, for display in the side panel.
    pub ask_answer: Option<String>,
    /// Vertical scroll offset for the Ask answer panel.
    pub ask_scroll: usize,
    /// True while an ask-mode provider call is in flight; drives the
    /// thinking spinner in the Ask panel.
    pub ask_pending: bool,
    /// When `true`, the render loop exits on the next frame.
    pub should_quit: bool,
    /// True when Ctrl+C/Esc requested exit and the user must confirm it.
    pub quit_confirmation_open: bool,
    /// Monotonic tick counter driving the running-tag spinner animation.
    pub spinner_frame: usize,
    /// True when the Flight Log panel is open. Toggled by Shift+L.
    pub flight_log_open: bool,
    /// Settings modal state.
    pub settings: SettingsState,
    /// Command palette input.
    pub palette_input: String,
    /// Command palette selected index.
    pub palette_selected: usize,
    /// Mode to restore after closing the palette.
    pub prev_mode: Mode,
    /// Submitted prompt history, newest last.
    pub prompt_history: Vec<String>,
    /// Cursor into prompt history while browsing with Up/Down.
    pub prompt_history_cursor: Option<usize>,
    /// Recent user-run commands.
    pub command_runs: Vec<CommandRunSummary>,
    /// One-line prompt feedback for slash-command errors and trust-loop hints.
    pub command_notice: Option<String>,
    /// Session-best token savings percentage (vs naive baseline). Updated
    /// whenever a goal completes with a higher savings rate than seen before.
    pub best_savings_pct: Option<i64>,
    /// Flash counter — non-zero for a few ticks after a new personal best
    /// is set, driving the savings line highlight. Decremented each tick.
    pub new_best_ticks: u8,
    /// True when the help overlay is visible. Toggled by `?`.
    pub help_open: bool,
    /// Flight Log scroll offset. `None` means "tail" — always pinned to
    /// the newest entry. `Some(n)` is the row offset from the top of the
    /// wrapped log; pressing `End` returns to tail mode.
    pub flight_log_scroll: Option<usize>,
    /// Last memory records loaded from the persistent store.
    pub memory_records: Vec<MemoryRecord>,
    /// Last task-history rows loaded from the persistent store.
    pub history_records: Vec<TaskRecord>,
    /// In-view history browser filter.
    pub history_filter: String,
    /// Selected row within the filtered history browser.
    pub history_selected: usize,
    /// Vertical scroll offset for the history browser.
    pub history_scroll: usize,
    /// Local index/Nexus status shown in the ambient system strip.
    pub nexus_status: NexusStatus,
    /// Path of the SQLite store backing this session.
    pub store_path: Option<std::path::PathBuf>,
    /// MCP approval requests awaiting an explicit user decision.
    pub pending_mcp_approvals: Vec<PendingMcpApproval>,
    /// Cursor into `pending_mcp_approvals` when more than one request is queued.
    pub mcp_approval_selected: usize,
    /// Latest prompt-section token manifest emitted by a worker.
    pub last_prompt_manifest: Option<PromptContextManifest>,
    /// Sum of prompt manifest totals observed in this TUI session.
    pub session_prompt_tokens: u64,
    /// Tokens the user has explicitly compacted from the visible context meter.
    pub compacted_prompt_tokens: u64,
    /// Active panel focus view.
    pub focus_view: FocusView,
    /// Vertical scroll offset for the active focus surface.
    pub focus_scroll: usize,
    /// Cursor into changed files shown by Code focus.
    pub focused_changed_file: usize,
    /// Cursor into command runs shown by Commands focus.
    pub focused_command_run: usize,
    /// Searchable goal switcher overlay.
    pub goal_switcher: GoalSwitcherState,
}

impl App {
    pub fn new(cfg: &crate::config::Config) -> Self {
        Self {
            mode: Mode::Goal,
            goals: Vec::new(),
            selected: 0,
            goal_prompt: PromptBuffer::new(),
            ask_prompt: PromptBuffer::new(),
            ask_answer: None,
            ask_scroll: 0,
            ask_pending: false,
            should_quit: false,
            quit_confirmation_open: false,
            spinner_frame: 0,
            flight_log_open: false,
            settings: SettingsState::new(cfg),
            palette_input: String::new(),
            palette_selected: 0,
            prev_mode: Mode::Goal,
            prompt_history: Vec::new(),
            prompt_history_cursor: None,
            command_runs: Vec::new(),
            command_notice: None,
            best_savings_pct: None,
            new_best_ticks: 0,
            help_open: false,
            flight_log_scroll: None,
            memory_records: Vec::new(),
            history_records: Vec::new(),
            history_filter: String::new(),
            history_selected: 0,
            history_scroll: 0,
            nexus_status: NexusStatus::default(),
            store_path: None,
            pending_mcp_approvals: Vec::new(),
            mcp_approval_selected: 0,
            last_prompt_manifest: None,
            session_prompt_tokens: 0,
            compacted_prompt_tokens: 0,
            focus_view: FocusView::Receipt,
            focus_scroll: 0,
            focused_changed_file: 0,
            focused_command_run: 0,
            goal_switcher: GoalSwitcherState::default(),
        }
    }

    /// Remove the currently-selected goal, if any. Keeps `selected` valid.
    pub fn delete_selected_goal(&mut self) {
        if self.selected < self.goals.len() {
            self.goals.remove(self.selected);
            if self.selected >= self.goals.len() {
                self.selected = self.goals.len().saturating_sub(1);
            }
        }
    }

    fn apply_slash_action(&mut self, action: SlashAction) -> Option<Intent> {
        self.help_open = false;
        self.settings.message = None;
        self.command_notice = None;
        match action {
            SlashAction::GoalMode => {
                self.mode = Mode::Goal;
                None
            }
            SlashAction::TaskMode => {
                self.mode = Mode::Task;
                None
            }
            SlashAction::AskMode => {
                self.mode = Mode::Ask;
                self.ask_scroll = 0;
                None
            }
            SlashAction::SubmitAsk(question) => {
                self.mode = Mode::Ask;
                self.ask_prompt.set_text("");
                self.ask_answer = None;
                self.ask_scroll = 0;
                self.ask_pending = true;
                Some(Intent::Ask(question))
            }
            SlashAction::OpenSettings | SlashAction::ManageModel => {
                self.mode = Mode::Settings;
                None
            }
            SlashAction::ShowCommands => {
                self.help_open = true;
                None
            }
            SlashAction::ToggleLog => {
                self.flight_log_open = !self.flight_log_open;
                if self.flight_log_open {
                    self.flight_log_scroll = None;
                }
                None
            }
            SlashAction::OpenMemory => {
                self.mode = Mode::Memory;
                Some(Intent::OpenMemory)
            }
            SlashAction::OpenHistory => {
                self.mode = Mode::History;
                Some(Intent::OpenHistory)
            }
            SlashAction::ClearGoals => {
                self.goals.clear();
                self.selected = 0;
                self.mode = Mode::Goal;
                None
            }
            SlashAction::DeleteSelectedGoal => {
                self.delete_selected_goal();
                None
            }
            SlashAction::Quit => self.request_quit_confirmation(),
            SlashAction::ShowStatus => {
                self.mode = Mode::Ask;
                self.ask_answer = Some(self.status_command_summary());
                None
            }
            SlashAction::ShowPermissions => {
                self.mode = Mode::Ask;
                self.ask_answer = Some(self.permissions_command_summary());
                None
            }
            SlashAction::ShowTrust => {
                self.mode = Mode::Ask;
                self.ask_answer = Some(self.trust_command_summary());
                None
            }
            SlashAction::RevokeCurrentTrust => {
                self.mode = Mode::Ask;
                self.ask_answer = Some("Trust\nRevoking current workspace trust...".into());
                Some(Intent::RevokeCurrentTrust)
            }
            SlashAction::SetPermissionMode(mode) => {
                self.settings.permission_mode = mode;
                self.mode = Mode::Ask;
                self.ask_answer = Some(self.permissions_command_summary());
                self.command_notice = Some(format!("Permission mode set to `{mode}`"));
                Some(Intent::SaveSettings)
            }
            SlashAction::ShowContext => {
                self.mode = Mode::Ask;
                self.ask_answer = Some(self.context_command_summary());
                None
            }
            SlashAction::CompactContext => {
                let intent = self.compact_context_intent();
                self.mode = Mode::Ask;
                self.ask_answer = Some(self.compact_command_summary(intent.is_some()));
                intent
            }
            SlashAction::ShowProblems => {
                self.focus_view = FocusView::Problems;
                self.focus_scroll = 0;
                self.command_notice = Some("Active focus: Problems".into());
                None
            }
            SlashAction::RetryGoal => self.retry_selected_goal_intent(),
            SlashAction::ShowWhyTokens => {
                self.mode = Mode::Ask;
                self.ask_answer = Some(self.why_tokens_command_summary());
                None
            }
            SlashAction::StopGoal => self.stop_selected_goal_intent(),
            SlashAction::OpenGoals => {
                self.goal_switcher.open = true;
                self.goal_switcher.filter.clear();
                self.goal_switcher.selected = self.selected.min(self.goals.len().saturating_sub(1));
                None
            }
            SlashAction::SetFocus(view) => {
                self.focus_view = view;
                self.command_notice = Some(format!("Active focus: {}", view.as_str()));
                None
            }
            SlashAction::CopyFocus => Some(Intent::CopyFocus(self.focus_text())),
            SlashAction::RerunCommand => {
                if let Some(command) = self.command_runs.last().map(|run| run.command.clone()) {
                    Some(Intent::RunCommand(format!("/run {command}")))
                } else {
                    self.command_notice = Some("No command has run yet.".into());
                    None
                }
            }
            SlashAction::ShowStats => {
                self.mode = Mode::Ask;
                self.ask_answer = Some(self.stats_command_summary());
                None
            }
            SlashAction::ShowReview => {
                self.mode = Mode::Ask;
                self.ask_answer = Some(self.review_command_summary());
                None
            }
        }
    }

    fn status_command_summary(&self) -> String {
        let totals = self.session_totals();
        let workspace = std::env::current_dir()
            .ok()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "unknown".into());
        let store = self
            .store_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "not open".into());
        format!(
            "Status\nversion: v{}\nprovider: {}\nmodel: {}\npermissions: {}\nworkspace: {}\nstore: {}\ngoals: {} total, {} complete, {} failed\ntokens: {} used, {} estimated saved\ncontext: {} latest prompt tokens, {} session prompt tokens",
            env!("CARGO_PKG_VERSION"),
            self.settings.provider,
            self.settings.model,
            self.settings.permission_mode,
            workspace,
            store,
            totals.goals,
            totals.completed,
            totals.failed,
            totals.tokens_used,
            totals.estimated_tokens_saved,
            self.last_prompt_manifest
                .as_ref()
                .map(|m| m.total_estimated_tokens)
                .unwrap_or(0),
            self.session_prompt_tokens
        )
    }

    fn permissions_command_summary(&self) -> String {
        let pending = self.pending_mcp_approvals.len();
        let workspace_trust = std::env::current_dir()
            .ok()
            .and_then(|path| trust::trust_record(&path))
            .map(|record| {
                format!(
                    "{} ({}, last seen {})",
                    record.display_name, record.source, record.last_seen_at
                )
            })
            .unwrap_or_else(|| "untrusted current workspace".into());
        format!(
            "Permissions\nmode: {}\nworkspace trust: {}\nread-only: blocks file writes and approval-gates commands\nask: safe workspace actions run, risky actions require approval\nworkspace-write: allowlisted commands and workspace writes run\nfull-access: non-sensitive actions run without approval\nmcp approvals pending: {pending}\ncommands: /permissions set ask|read-only|workspace-write|full-access",
            self.settings.permission_mode,
            workspace_trust
        )
    }

    fn trust_command_summary(&self) -> String {
        let current = std::env::current_dir()
            .ok()
            .and_then(|path| trust::trust_record(&path))
            .map(|record| {
                format!(
                    "current: {} ({})\npath: {}\nmode: {}\ntrusted: {}\nlast seen: {}",
                    record.display_name,
                    record.source,
                    record.canonical_path,
                    record.permission_mode,
                    record.trusted_at,
                    record.last_seen_at
                )
            })
            .unwrap_or_else(|| "current: untrusted".into());
        let records = trust::list_trust_records();
        let mut out = format!("Trust\n{current}\n\nknown workspaces: {}", records.len());
        for record in records.iter().take(8) {
            out.push_str(&format!(
                "\n- {}  {}  {}",
                record.display_name, record.permission_mode, record.source
            ));
        }
        out.push_str("\ncommands: /trust current, /trust list, /trust revoke-current");
        out
    }

    fn context_command_summary(&self) -> String {
        let Some(m) = &self.last_prompt_manifest else {
            return format!(
                "Context\nlatest prompt: none yet\nsession prompt total: {}\ncompacted by user: {}\nworker compression: automatic near the context threshold\ncommand: /compact",
                self.session_prompt_tokens, self.compacted_prompt_tokens
            );
        };
        format!(
            "Context\nlatest prompt total: {}\ncontext target: {}{}\nattempt: {}{}\nsystem: {}\ngoal: {}\nmemory/context: {}\nrepo map: {}\nrepo code: {}\nomitted code: {}\nattachments: {}\nmcp/tools: {}\nretry errors: {}\nbudget: {}\nauto-compacted: {}\ndeduped: {}\nsession prompt total: {}\ncompacted by user: {}\ncommand: /compact",
            m.total_estimated_tokens,
            if m.context_target_tokens == 0 {
                "unknown".into()
            } else {
                m.context_target_tokens.to_string()
            },
            if m.target_exceeded {
                format!(" (target exceeded by {})", m.over_target_tokens)
            } else {
                String::new()
            },
            m.attempt,
            if m.repair_attempt { " repair" } else { "" },
            m.system_tokens,
            m.user_goal_tokens,
            m.memory_tokens,
            m.repo_map_tokens,
            m.code_context_tokens,
            m.omitted_code_tokens,
            m.attachment_tokens,
            m.mcp_tool_tokens,
            m.retry_error_tokens,
            m.budget_limit
                .map(|limit| limit.to_string())
                .unwrap_or_else(|| "unknown".into()),
            m.compacted_tokens,
            m.deduped_tokens,
            self.session_prompt_tokens,
            self.compacted_prompt_tokens
        )
    }

    fn why_tokens_command_summary(&self) -> String {
        let Some(m) = &self.last_prompt_manifest else {
            return "Why tokens?\nNo provider prompt manifest has been recorded yet. Run a goal first, then use /why-tokens again.".into();
        };
        let active_goal = self
            .goals
            .get(self.selected)
            .map(|goal| goal.description.as_str())
            .unwrap_or("none");
        let (first_attempt, repair_attempts, context_artifacts, retry_tokens) =
            self.prompt_attempt_buckets();
        let routing_note = self.routing_note_for_goal(active_goal);
        format!(
            "Why tokens?\ngoal: {}\ntotal prompt estimate: {}\ncontext target: {}{}\nattempt buckets:\n- first attempt: {}\n- repair attempts: {}\n- context/artifacts: {}\n- verifier retry diagnostics: {}\n- system: {} provider instructions\n- goal: {} current request tokens\n- memory/context: {} retained prior context tokens\n- repo map: {} compact orientation tokens\n- code: {} selected repository context tokens\n- omitted code: {} candidate tokens skipped by the context compiler\n- attachments: {} pasted/image/file artifact tokens\n- tools: {} MCP or tool instruction tokens\n- retry diagnostics: {} verifier/provider repair tokens\ncompacted before send: {}\ndeduped before send: {}\n{}provider-reported completion tokens remain the billing source of truth.",
            active_goal,
            m.total_estimated_tokens,
            if m.context_target_tokens == 0 {
                "unknown".into()
            } else {
                m.context_target_tokens.to_string()
            },
            if m.target_exceeded {
                format!(" (target exceeded by {})", m.over_target_tokens)
            } else {
                String::new()
            },
            first_attempt,
            repair_attempts,
            context_artifacts,
            retry_tokens,
            m.system_tokens,
            m.user_goal_tokens,
            m.memory_tokens,
            m.repo_map_tokens,
            m.code_context_tokens,
            m.omitted_code_tokens,
            m.attachment_tokens,
            m.mcp_tool_tokens,
            m.retry_error_tokens,
            m.compacted_tokens,
            m.deduped_tokens,
            routing_note
        )
    }

    fn prompt_attempt_buckets(&self) -> (u64, u64, u64, u64) {
        let mut first_attempt = 0_u64;
        let mut repair_attempts = 0_u64;
        let mut context_artifacts = 0_u64;
        let mut retry_tokens = 0_u64;
        let mut saw_manifest = false;
        if let Some(goal) = self.goals.get(self.selected) {
            for record in &goal.flight_log {
                if let OrchestratorEvent::PromptManifest { manifest, .. } = &record.event {
                    saw_manifest = true;
                    if manifest.repair_attempt || manifest.attempt > 1 {
                        repair_attempts =
                            repair_attempts.saturating_add(manifest.total_estimated_tokens);
                    } else {
                        first_attempt =
                            first_attempt.saturating_add(manifest.total_estimated_tokens);
                    }
                    context_artifacts = context_artifacts
                        .saturating_add(manifest.memory_tokens)
                        .saturating_add(manifest.repo_map_tokens)
                        .saturating_add(manifest.code_context_tokens)
                        .saturating_add(manifest.attachment_tokens);
                    retry_tokens = retry_tokens.saturating_add(manifest.retry_error_tokens);
                }
            }
        }
        if !saw_manifest {
            if let Some(manifest) = &self.last_prompt_manifest {
                if manifest.repair_attempt || manifest.attempt > 1 {
                    repair_attempts = manifest.total_estimated_tokens;
                } else {
                    first_attempt = manifest.total_estimated_tokens;
                }
                context_artifacts = manifest
                    .memory_tokens
                    .saturating_add(manifest.repo_map_tokens)
                    .saturating_add(manifest.code_context_tokens)
                    .saturating_add(manifest.attachment_tokens);
                retry_tokens = manifest.retry_error_tokens;
            }
        }
        (
            first_attempt,
            repair_attempts,
            context_artifacts,
            retry_tokens,
        )
    }

    fn routing_note_for_goal(&self, goal: &str) -> String {
        let provider = self.settings.provider.to_ascii_lowercase();
        let model = self.settings.model.to_ascii_lowercase();
        let goal = goal.to_ascii_lowercase();
        let broad_generated = goal.contains("chess")
            || goal.contains("game")
            || goal.contains("app")
            || goal.contains("html")
            || goal.contains("web");
        if provider.contains("cloudflare") && model.contains("kimi") && broad_generated {
            "routing note: Cloudflare Kimi is high-risk for broad generated-code tasks in current evidence; if quality gates repeat, use a stronger code model or narrower prompt.\n".into()
        } else {
            String::new()
        }
    }

    fn stats_command_summary(&self) -> String {
        let totals = self.session_totals();
        let active = self
            .goals
            .iter()
            .filter(|goal| {
                matches!(
                    goal.status,
                    TaskStatus::Planning | TaskStatus::Running { .. }
                )
            })
            .count();
        format!(
            "Stats\ngoals: {} total, {} active, {} review, {} done, {} failed\ntokens: {} used, {} estimated saved\ncontext: {} latest prompt tokens, {} session prompt tokens\ncommands: {} recent runs",
            totals.goals,
            active,
            totals.reviewing,
            totals.completed,
            totals.failed,
            totals.tokens_used,
            totals.estimated_tokens_saved,
            self.last_prompt_manifest
                .as_ref()
                .map(|m| m.total_estimated_tokens)
                .unwrap_or(0),
            self.session_prompt_tokens,
            self.command_runs.len()
        )
    }

    fn compact_command_summary(&self, sent_to_worker: bool) -> String {
        let target = if sent_to_worker {
            "active worker context was asked to compact"
        } else {
            "no active worker context is selected"
        };
        format!(
            "Compact\n{target}\nlatest prompt meter reset locally\ncompacted by user: {}\nworker compression also runs automatically near its threshold",
            self.compacted_prompt_tokens
        )
    }

    fn compact_context_intent(&mut self) -> Option<Intent> {
        if let Some(manifest) = self.last_prompt_manifest.take() {
            self.compacted_prompt_tokens = self
                .compacted_prompt_tokens
                .saturating_add(manifest.total_estimated_tokens);
        }
        if self.selected_goal_can_be_controlled() {
            Some(Intent::CompactContext {
                goal_index: self.selected,
            })
        } else {
            None
        }
    }

    fn stop_selected_goal_intent(&mut self) -> Option<Intent> {
        if self.selected_goal_can_be_controlled() {
            if let Some(goal) = self.goals.get_mut(self.selected) {
                goal.status = TaskStatus::Rejected;
            }
            self.command_notice = Some("Stop requested for selected goal.".into());
            Some(Intent::StopGoal {
                goal_index: self.selected,
            })
        } else {
            self.command_notice = Some("No running goal is selected.".into());
            None
        }
    }

    fn retry_selected_goal_intent(&mut self) -> Option<Intent> {
        let Some(goal) = self.goals.get(self.selected) else {
            self.command_notice = Some("No selected goal to retry.".into());
            return None;
        };
        if !matches!(goal.status, TaskStatus::Failed { .. }) && problem_diagnostics(goal).is_empty()
        {
            self.command_notice = Some("Selected goal has no verifier failure to repair.".into());
            return None;
        }
        let diagnostics = compact_problem_diagnostics(goal, 6);
        let prompt = format!(
            "Repair the previous failed Phonton goal.\n\nOriginal goal:\n{}\n\nVerifier diagnostics:\n{}\n\nInstructions:\n- Fix the reported failure with the smallest reviewable diff.\n- Keep output runnable and concise.\n- Run static syntax/build verification before review.\n- Do not claim success unless the verifier passes.",
            goal.description,
            diagnostics
        );
        self.command_notice = Some("Retry queued with compact diagnostics.".into());
        Some(Intent::QueueGoal(prompt))
    }

    fn selected_goal_can_be_controlled(&self) -> bool {
        self.goals
            .get(self.selected)
            .map(|goal| {
                matches!(
                    goal.status,
                    TaskStatus::Planning | TaskStatus::Running { .. }
                )
            })
            .unwrap_or(false)
    }

    fn review_command_summary(&self) -> String {
        if let Some(goal) = self.goals.get(self.selected) {
            let status = match &goal.status {
                TaskStatus::Queued => "queued",
                TaskStatus::Planning => "planning",
                TaskStatus::Running { .. } => "running",
                TaskStatus::Reviewing { .. } => "review ready",
                TaskStatus::Done { .. } => "done",
                TaskStatus::Failed { .. } => "failed",
                TaskStatus::NeedsClarification { .. } => "needs clarification",
                TaskStatus::Paused { .. } => "paused",
                TaskStatus::Rejected => "rejected",
            };
            format!(
                "Review\nselected goal: {}\nstatus: {}\nreceipt: run `phonton review latest` outside the TUI for the exportable handoff packet\nflight log: use /log for the evidence trail",
                goal.description, status
            )
        } else {
            "Review\nNo goal is selected yet. Submit a goal first, then use /review or `phonton review latest`.".into()
        }
    }

    /// Summarize the visible session for the exit receipt.
    pub fn session_totals(&self) -> SessionTotals {
        let mut totals = SessionTotals {
            goals: self.goals.len(),
            best_savings_pct: self.best_savings_pct,
            ..SessionTotals::default()
        };
        for goal in &self.goals {
            match goal.status {
                TaskStatus::Done { .. } => totals.completed += 1,
                TaskStatus::Failed { .. } => totals.failed += 1,
                TaskStatus::Reviewing { .. } => totals.reviewing += 1,
                _ => {}
            }
            totals.tokens_used = totals.tokens_used.saturating_add(session_goal_tokens(goal));
            totals.naive_baseline_tokens = totals
                .naive_baseline_tokens
                .saturating_add(session_goal_baseline(goal));
        }
        totals.estimated_tokens_saved =
            token_delta_vs_naive(totals.naive_baseline_tokens, totals.tokens_used);
        totals
    }

    /// Build a durable snapshot for resuming this workspace later.
    pub fn to_session_snapshot(&self, workspace: String, saved_at: u64) -> SessionSnapshot {
        SessionSnapshot {
            workspace,
            saved_at,
            selected_goal: self.selected.min(self.goals.len().saturating_sub(1)),
            goal_input: self.goal_prompt.display_text().to_string(),
            ask_input: self.ask_prompt.display_text().to_string(),
            ask_answer: self.ask_answer.clone(),
            prompt_history: bounded_prompt_history(&self.prompt_history, 100),
            best_savings_pct: self.best_savings_pct,
            goals: self
                .goals
                .iter()
                .map(|goal| SessionGoalSnapshot {
                    description: goal.description.clone(),
                    status: goal.status.clone(),
                    state: goal.state.clone(),
                    task_id: goal.task_id,
                    flight_log: goal.flight_log.clone(),
                })
                .collect(),
            totals: self.session_totals(),
        }
    }

    /// Restore review-safe session state from a saved snapshot.
    pub fn restore_session_snapshot(&mut self, snapshot: SessionSnapshot) {
        self.goals = snapshot
            .goals
            .into_iter()
            .map(|goal| GoalEntry {
                description: goal.description,
                status: goal.status,
                state: goal.state,
                task_id: goal.task_id,
                flight_log: goal.flight_log,
                checkpoint_cursor: None,
            })
            .collect();
        self.selected = snapshot
            .selected_goal
            .min(self.goals.len().saturating_sub(1));
        self.goal_prompt = PromptBuffer::from_text(snapshot.goal_input);
        self.ask_prompt = PromptBuffer::from_text(snapshot.ask_input);
        self.ask_answer = snapshot.ask_answer;
        self.ask_scroll = 0;
        self.prompt_history = bounded_prompt_history(&snapshot.prompt_history, 100);
        self.prompt_history_cursor = None;
        self.ask_pending = false;
        self.best_savings_pct = snapshot.best_savings_pct;
        self.new_best_ticks = 0;
        self.mode = Mode::Goal;
        self.flight_log_open = false;
        self.help_open = false;
        self.quit_confirmation_open = false;
        self.pending_mcp_approvals.clear();
        self.mcp_approval_selected = 0;
        self.last_prompt_manifest = None;
        self.session_prompt_tokens = 0;
        self.compacted_prompt_tokens = 0;
        self.focus_view = FocusView::Receipt;
        self.focused_changed_file = 0;
        self.focused_command_run = 0;
        self.goal_switcher = GoalSwitcherState::default();
    }

    fn request_quit_confirmation(&mut self) -> Option<Intent> {
        self.quit_confirmation_open = true;
        self.help_open = false;
        None
    }

    fn handle_quit_confirmation_key(&mut self, key: KeyEvent) -> Option<Intent> {
        match key.code {
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.quit_confirmation_open = false;
                self.should_quit = true;
                Some(Intent::Quit)
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                self.quit_confirmation_open = false;
                None
            }
            _ => None,
        }
    }
}

fn session_goal_tokens(goal: &GoalEntry) -> u64 {
    goal.state
        .as_ref()
        .map(|s| s.tokens_used)
        .unwrap_or_else(|| match goal.status {
            TaskStatus::Reviewing { tokens_used, .. } | TaskStatus::Done { tokens_used, .. } => {
                tokens_used
            }
            _ => 0,
        })
}

fn session_goal_baseline(goal: &GoalEntry) -> u64 {
    goal.state
        .as_ref()
        .map(|s| s.estimated_naive_tokens)
        .unwrap_or_else(|| match goal.status {
            TaskStatus::Reviewing {
                tokens_used,
                estimated_savings_tokens,
            } => tokens_used.saturating_add(estimated_savings_tokens),
            _ => 0,
        })
}

fn token_delta_vs_naive(naive_baseline_tokens: u64, tokens_used: u64) -> i64 {
    if naive_baseline_tokens >= tokens_used {
        let saved = naive_baseline_tokens - tokens_used;
        saved.min(i64::MAX as u64) as i64
    } else {
        let over = tokens_used - naive_baseline_tokens;
        -(over.min(i64::MAX as u64) as i64)
    }
}

fn bounded_prompt_history(history: &[String], max_entries: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let start = history.len().saturating_sub(max_entries.saturating_mul(2));
    for item in history.iter().skip(start) {
        let trimmed = item.trim();
        if trimmed.is_empty() {
            continue;
        }
        if out.last().is_some_and(|last| last == trimmed) {
            continue;
        }
        out.push(trimmed.to_string());
        if out.len() > max_entries {
            out.remove(0);
        }
    }
    out
}

fn char_count(s: &str) -> usize {
    s.chars().count()
}

/// Best-effort heuristic for "did the user paste an API key into the
/// Goal bar?" — fires on the well-known prefixes for every provider we
/// support, plus a generic high-entropy single-token fallback.
///
/// Conservative on purpose: it should never reject a legitimate goal
/// (which is invariably multiple words separated by spaces) but should
/// catch a pasted key whether or not the user knew which provider it
/// came from.
pub fn looks_like_api_key(s: &str) -> bool {
    let s = s.trim();
    // Multi-word inputs are almost certainly goals, not keys. A pasted
    // key is a single contiguous token; "make a chess game" isn't.
    if s.contains(char::is_whitespace) {
        return false;
    }
    // Provider-specific prefixes — these are unambiguous.
    let prefixes = [
        "sk-ant-",  // Anthropic
        "sk-or-",   // OpenRouter
        "sk-proj-", // OpenAI project keys
        "sk-",      // OpenAI / DeepSeek (keep last so longer prefixes win)
        "AIza",     // Google AI Studio (Gemini)
        "ya29.",    // Google OAuth (rare but seen)
        "xai-",     // xAI / Grok
        "gsk_",     // Groq
        "tgp_v1_",  // Together
        "key_",     // Together (legacy) / generic
        "or-",      // OpenRouter short
    ];
    if prefixes.iter().any(|p| s.starts_with(p)) {
        return true;
    }
    // Generic fallback: a 30+ char token of [A-Za-z0-9_-] with mixed
    // case and at least one digit looks like a key, not a goal.
    if s.len() >= 30
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && s.chars().any(|c| c.is_ascii_digit())
        && s.chars().any(|c| c.is_ascii_alphabetic())
    {
        return true;
    }
    false
}

fn paste_contains_likely_secret(text: &str) -> bool {
    text.lines().any(|line| {
        line.split(|c: char| {
            c.is_whitespace() || matches!(c, '"' | '\'' | '`' | ':' | '=' | ',' | ';')
        })
        .any(looks_like_api_key)
    })
}

impl Default for App {
    fn default() -> Self {
        let default_cfg = crate::config::Config {
            provider: crate::config::ProviderConfig {
                name: "anthropic".to_string(),
                api_key: None,
                model: None,
                account_id: None,
                base_url: None,
            },
            budget: crate::config::BudgetConfig {
                max_tokens: None,
                max_usd_cents: None,
            },
            permissions: crate::config::PermissionsConfig::default(),
        };
        Self::new(&default_cfg)
    }
}

impl App {
    /// Currently highlighted goal, if any.
    pub fn current_goal(&self) -> Option<&GoalEntry> {
        self.goals.get(self.selected)
    }

    /// Apply a `GlobalState` snapshot to the goal at `index`. Updates both
    /// the per-goal cached state and the task-level status.
    pub fn apply_state(&mut self, index: usize, state: GlobalState) {
        // Check for a new session-best savings percentage before storing.
        if state.estimated_naive_tokens > 0 {
            let pct = savings_pct(&state);
            if let Some(p) = pct {
                let is_new_best = self.best_savings_pct.is_none_or(|best| p > best);
                if is_new_best {
                    self.best_savings_pct = Some(p);
                    self.new_best_ticks = 12;
                }
            }
        }
        if let Some(g) = self.goals.get_mut(index) {
            g.status = state.task_status.clone();
            g.state = Some(state);
        }
        if index == self.selected
            && matches!(
                self.goals.get(index).map(|g| &g.status),
                Some(TaskStatus::Failed { .. })
            )
        {
            self.focus_view = FocusView::Problems;
            self.focus_scroll = 0;
        }
    }

    /// Append a flight-log event to the goal at `index`.
    pub fn apply_event(&mut self, index: usize, event: EventRecord) {
        let should_default_code_focus = index == self.selected
            && matches!(
                &event.event,
                OrchestratorEvent::SubtaskReviewReady { diff_hunks, .. } if !diff_hunks.is_empty()
            );
        let should_default_problems_focus = index == self.selected
            && matches!(
                &event.event,
                OrchestratorEvent::VerifyFail { .. } | OrchestratorEvent::SubtaskFailed { .. }
            );
        if let OrchestratorEvent::PromptManifest { manifest, .. } = &event.event {
            self.record_prompt_manifest(manifest.clone());
        }
        if let Some(g) = self.goals.get_mut(index) {
            g.flight_log.push(event);
        }
        if should_default_problems_focus {
            self.focus_view = FocusView::Problems;
            self.focus_scroll = 0;
            return;
        }
        if should_default_code_focus && self.focus_view == FocusView::Receipt {
            self.focus_view = FocusView::Code;
        }
    }

    pub fn record_prompt_manifest(&mut self, manifest: PromptContextManifest) {
        self.session_prompt_tokens = self
            .session_prompt_tokens
            .saturating_add(manifest.total_estimated_tokens);
        self.last_prompt_manifest = Some(manifest);
    }

    pub fn active_focus_view_for_current_goal(&self) -> FocusView {
        self.focus_view
    }

    pub fn focus_text(&self) -> String {
        let Some(goal) = self.current_goal() else {
            return "Phonton\nNo goal selected.".into();
        };
        match self.active_focus_view_for_current_goal() {
            FocusView::Receipt => receipt_focus_text(goal),
            FocusView::Problems => problems_focus_text(goal, self.focused_changed_file),
            FocusView::Code => code_focus_text(goal, self.focused_changed_file),
            FocusView::Commands => {
                commands_focus_text(&self.command_runs, self.focused_command_run)
            }
            FocusView::Log => log_focus_text(goal),
        }
    }

    fn cycle_focus_view(&mut self) {
        self.focus_view = self.active_focus_view_for_current_goal().next();
        self.focus_scroll = 0;
    }

    fn previous_goal(&mut self) {
        self.selected = self.selected.saturating_sub(1);
        self.focus_scroll = 0;
        self.clamp_focus_indices();
        self.default_to_code_focus_for_reviewable_goal();
    }

    fn next_goal(&mut self) {
        if self.selected + 1 < self.goals.len() {
            self.selected += 1;
        }
        self.focus_scroll = 0;
        self.clamp_focus_indices();
        self.default_to_code_focus_for_reviewable_goal();
    }

    fn jump_to_goal_number(&mut self, number: usize) {
        if number > 0 && number <= self.goals.len() {
            self.selected = number - 1;
            self.focus_scroll = 0;
            self.clamp_focus_indices();
            self.default_to_code_focus_for_reviewable_goal();
        }
    }

    fn clamp_focus_indices(&mut self) {
        let changed_len = self.current_goal().map(focused_file_count).unwrap_or(0);
        self.focused_changed_file = self.focused_changed_file.min(changed_len.saturating_sub(1));
        self.focused_command_run = self
            .focused_command_run
            .min(self.command_runs.len().saturating_sub(1));
    }

    fn default_to_code_focus_for_reviewable_goal(&mut self) {
        if self
            .current_goal()
            .is_some_and(|goal| matches!(goal.status, TaskStatus::Failed { .. }))
        {
            self.focus_view = FocusView::Problems;
            self.focus_scroll = 0;
            return;
        }
        if self.focus_view == FocusView::Receipt
            && self.current_goal().map(focused_file_count).unwrap_or(0) > 0
        {
            self.focus_view = FocusView::Code;
            self.focus_scroll = 0;
        }
    }

    /// Insert pasted text into the active prompt surface. Long or multi-line
    /// content is collapsed into a prompt artifact by [`PromptBuffer`].
    pub fn handle_paste(&mut self, text: &str) {
        match self.mode {
            Mode::Goal | Mode::Task => {
                if looks_like_api_key(text.trim()) {
                    self.help_open = false;
                    self.settings.active_field = SettingsField::ApiKey;
                    self.settings.message = Some(
                        "That looked like an API key — open Settings (/settings) and paste it into the API Key field, not the Goal bar."
                            .into(),
                    );
                    self.mode = Mode::Settings;
                    return;
                }
                if paste_contains_likely_secret(text) {
                    self.command_notice = Some(
                        "Paste blocked: it looks like it contains credentials. Use /settings for API keys."
                            .into(),
                    );
                    return;
                }
                self.goal_prompt.insert_paste(text)
            }
            Mode::Ask => self.ask_prompt.insert_paste(text),
            Mode::Settings => self.paste_into_settings(text),
            _ => {}
        }
    }

    fn paste_into_settings(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let text = text.trim_end_matches('\n');
        match self.settings.active_field {
            SettingsField::Provider => self.settings.provider.push_str(text),
            SettingsField::Model => {
                self.settings.model.push_str(text);
                self.settings.model_ok = None;
            }
            SettingsField::ApiKey => self.settings.api_key.push_str(text),
            SettingsField::AccountId => self.settings.account_id.push_str(text),
            SettingsField::BaseUrl => self.settings.base_url.push_str(text),
            SettingsField::MaxTokens => self.settings.max_tokens.push_str(
                &text
                    .chars()
                    .filter(|c| c.is_ascii_digit())
                    .collect::<String>(),
            ),
            SettingsField::MaxUsdCents => self.settings.max_usd_cents.push_str(
                &text
                    .chars()
                    .filter(|c| c.is_ascii_digit())
                    .collect::<String>(),
            ),
        }
        self.settings.message = None;
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_visible_surface(-3),
            MouseEventKind::ScrollDown => self.scroll_visible_surface(3),
            _ => {}
        }
    }

    fn scroll_visible_surface(&mut self, delta: isize) {
        if self.flight_log_open && matches!(self.mode, Mode::Goal | Mode::Task) {
            if delta < 0 {
                let cur = self.flight_log_scroll.unwrap_or(usize::MAX);
                self.flight_log_scroll = Some(cur.saturating_sub(delta.unsigned_abs()));
            } else if let Some(s) = self.flight_log_scroll {
                self.flight_log_scroll = Some(s.saturating_add(delta as usize));
            }
            return;
        }

        if self.mode == Mode::Ask {
            if delta < 0 {
                self.ask_scroll = self.ask_scroll.saturating_sub(delta.unsigned_abs());
            } else {
                self.ask_scroll = self.ask_scroll.saturating_add(delta as usize);
            }
            return;
        }

        if matches!(self.mode, Mode::Goal | Mode::Task) {
            if delta < 0 {
                self.focus_scroll = self.focus_scroll.saturating_sub(delta.unsigned_abs());
            } else {
                self.focus_scroll = self.focus_scroll.saturating_add(delta as usize);
            }
        }
    }

    pub fn push_command_run(&mut self, summary: CommandRunSummary) {
        self.command_runs.push(summary);
        self.focused_command_run = self.command_runs.len().saturating_sub(1);
        const MAX_COMMAND_RUNS: usize = 20;
        if self.command_runs.len() > MAX_COMMAND_RUNS {
            let overflow = self.command_runs.len() - MAX_COMMAND_RUNS;
            self.command_runs.drain(0..overflow);
            self.focused_command_run = self.focused_command_run.saturating_sub(overflow);
        }
    }

    /// Add an MCP approval prompt and focus the newest request.
    pub fn push_mcp_approval(&mut self, prompt: PendingMcpApproval) {
        self.pending_mcp_approvals.push(prompt);
        self.mcp_approval_selected = self.pending_mcp_approvals.len().saturating_sub(1);
    }

    fn active_mcp_approval(&self) -> Option<&PendingMcpApproval> {
        self.pending_mcp_approvals.get(
            self.mcp_approval_selected
                .min(self.pending_mcp_approvals.len().saturating_sub(1)),
        )
    }

    fn resolve_selected_mcp_approval(&mut self, approved: bool) -> Option<Intent> {
        if self.pending_mcp_approvals.is_empty() {
            return None;
        }
        let idx = self
            .mcp_approval_selected
            .min(self.pending_mcp_approvals.len().saturating_sub(1));
        let prompt = self.pending_mcp_approvals.remove(idx);
        self.mcp_approval_selected = self
            .mcp_approval_selected
            .min(self.pending_mcp_approvals.len().saturating_sub(1));
        Some(Intent::ResolveMcpApproval {
            approval_id: prompt.id,
            approved,
        })
    }

    /// Translate a key event into a state transition. Pure function so the
    /// key-handling logic is unit-testable without a terminal.
    ///
    /// Returns `Some(Intent)` when the caller should act on the outside
    /// world (queue a new goal, issue an ask, exit).
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Intent> {
        if self.quit_confirmation_open {
            return self.handle_quit_confirmation_key(key);
        }

        if self.goal_switcher.open {
            return self.handle_goal_switcher_key(key);
        }

        if !self.pending_mcp_approvals.is_empty() {
            return self.handle_mcp_approval_key(key);
        }

        if key.modifiers.contains(KeyModifiers::ALT) && matches!(self.mode, Mode::Goal | Mode::Task)
        {
            match key.code {
                KeyCode::Up => {
                    self.previous_goal();
                    return None;
                }
                KeyCode::Down => {
                    self.next_goal();
                    return None;
                }
                KeyCode::Char(c) if c.is_ascii_digit() => {
                    if let Some(number) = c.to_digit(10) {
                        self.jump_to_goal_number(number as usize);
                    }
                    return None;
                }
                _ => {}
            }
        }

        // Global shortcuts first, regardless of mode.
        if matches!(key.code, KeyCode::Esc) {
            if self.help_open {
                self.help_open = false;
                return None;
            }
            if self.flight_log_open {
                self.flight_log_open = false;
                return None;
            }
            if matches!(
                self.mode,
                Mode::Ask | Mode::Settings | Mode::Memory | Mode::History | Mode::CommandPalette
            ) {
                self.mode = Mode::Goal;
                return None;
            }
            return self.request_quit_confirmation();
        }

        // `?` toggles the help overlay anywhere it isn't legitimate text input.
        if matches!(key.code, KeyCode::Char('?'))
            && !matches!(
                self.mode,
                Mode::Ask | Mode::Settings | Mode::Memory | Mode::History | Mode::CommandPalette
            )
            && self.goal_prompt.is_empty()
        {
            self.help_open = !self.help_open;
            return None;
        }
        // While help is up, swallow keystrokes so they don't leak into the
        // input buffer behind it. Esc handled above.
        if self.help_open {
            return None;
        }

        // Ctrl+/ keeps the legacy command palette available while plain '/'
        // is now real prompt input for slash commands like `/run`.
        if matches!(key.code, KeyCode::Char('/'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && !matches!(self.mode, Mode::CommandPalette | Mode::Ask | Mode::Settings)
        {
            self.prev_mode = self.mode;
            self.mode = Mode::CommandPalette;
            self.palette_input.clear();
            self.palette_selected = 0;
            return None;
        }

        // We keep simple shortcuts like Shift+L for the flight log but remove
        // complex Ctrl combinations as requested.
        let is_l = matches!(key.code, KeyCode::Char('L'))
            || (matches!(key.code, KeyCode::Char('l'))
                && key.modifiers.contains(KeyModifiers::SHIFT));

        if is_l
            && !matches!(
                self.mode,
                Mode::Ask | Mode::Settings | Mode::Memory | Mode::History | Mode::CommandPalette
            )
        {
            self.flight_log_open = !self.flight_log_open;
            // Reset scroll to tail mode whenever the log is reopened.
            if self.flight_log_open {
                self.flight_log_scroll = None;
            }
            return None;
        }
        // While the Flight Log is open, navigation keys scroll it instead
        // of touching the goal input. This is the only way to read full
        // multi-line errors that wrap past the viewport.
        if self.flight_log_open && matches!(self.mode, Mode::Goal | Mode::Task) {
            match key.code {
                KeyCode::Up => {
                    let cur = self.flight_log_scroll.unwrap_or(usize::MAX);
                    self.flight_log_scroll = Some(cur.saturating_sub(1));
                    return None;
                }
                KeyCode::Down => {
                    if let Some(s) = self.flight_log_scroll {
                        // Saturating add keeps us within bounds; clamp
                        // happens in render against the actual log length.
                        self.flight_log_scroll = Some(s.saturating_add(1));
                    }
                    return None;
                }
                KeyCode::PageUp => {
                    let cur = self.flight_log_scroll.unwrap_or(usize::MAX);
                    self.flight_log_scroll = Some(cur.saturating_sub(10));
                    return None;
                }
                KeyCode::PageDown => {
                    if let Some(s) = self.flight_log_scroll {
                        self.flight_log_scroll = Some(s.saturating_add(10));
                    }
                    return None;
                }
                KeyCode::Home => {
                    self.flight_log_scroll = Some(0);
                    return None;
                }
                KeyCode::End => {
                    self.flight_log_scroll = None;
                    return None;
                }
                _ => {}
            }
        }
        if !self.flight_log_open
            && matches!(self.mode, Mode::Goal | Mode::Task)
            && self.goal_prompt.is_empty()
        {
            match key.code {
                KeyCode::PageUp => {
                    self.focus_scroll = self.focus_scroll.saturating_sub(12);
                    return None;
                }
                KeyCode::PageDown => {
                    self.focus_scroll = self.focus_scroll.saturating_add(12);
                    return None;
                }
                KeyCode::Home => {
                    self.focus_scroll = 0;
                    return None;
                }
                KeyCode::End => {
                    self.focus_scroll = usize::MAX;
                    return None;
                }
                _ => {}
            }
        }
        if self.mode == Mode::Ask && self.ask_prompt.is_empty() {
            match key.code {
                KeyCode::PageUp => {
                    self.ask_scroll = self.ask_scroll.saturating_sub(12);
                    return None;
                }
                KeyCode::PageDown => {
                    self.ask_scroll = self.ask_scroll.saturating_add(12);
                    return None;
                }
                KeyCode::Up => {
                    self.ask_scroll = self.ask_scroll.saturating_sub(1);
                    return None;
                }
                KeyCode::Down => {
                    self.ask_scroll = self.ask_scroll.saturating_add(1);
                    return None;
                }
                KeyCode::Home => {
                    self.ask_scroll = 0;
                    return None;
                }
                KeyCode::End => {
                    self.ask_scroll = usize::MAX;
                    return None;
                }
                _ => {}
            }
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    return self.request_quit_confirmation();
                }
                // Cmd/Ctrl+; toggles the Ask side panel.
                KeyCode::Char(';') => {
                    self.mode = if self.mode == Mode::Ask {
                        Mode::Goal
                    } else {
                        Mode::Ask
                    };
                    return None;
                }
                // Ctrl+D deletes the highlighted goal (only meaningful in
                // Goal/Task mode; in Ask/Settings the input swallows it).
                KeyCode::Char('d')
                    if matches!(self.mode, Mode::Goal | Mode::Task) && !self.goals.is_empty() =>
                {
                    self.delete_selected_goal();
                    return None;
                }
                _ => {}
            }
        }

        match self.mode {
            Mode::Goal | Mode::Task => self.handle_goal_key(key),
            Mode::Ask => self.handle_ask_key(key),
            Mode::Settings => self.handle_settings_key(key),
            Mode::Memory => None,
            Mode::History => self.handle_history_key(key),
            Mode::CommandPalette => self.handle_palette_key(key),
        }
    }

    fn handle_history_key(&mut self, key: KeyEvent) -> Option<Intent> {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Goal;
            }
            KeyCode::Char('r') if self.history_filter.is_empty() => {
                return Some(Intent::OpenHistory);
            }
            KeyCode::Up => {
                self.history_selected = self.history_selected.saturating_sub(1);
                if self.history_selected < self.history_scroll {
                    self.history_scroll = self.history_selected;
                }
            }
            KeyCode::Down => {
                let max = self.filtered_history_indices().len().saturating_sub(1);
                self.history_selected = (self.history_selected + 1).min(max);
                if self.history_selected > self.history_scroll.saturating_add(12) {
                    self.history_scroll = self.history_selected.saturating_sub(12);
                }
            }
            KeyCode::PageUp => {
                self.history_selected = self.history_selected.saturating_sub(10);
                self.history_scroll = self.history_scroll.saturating_sub(10);
            }
            KeyCode::PageDown => {
                let max = self.filtered_history_indices().len().saturating_sub(1);
                self.history_selected = (self.history_selected + 10).min(max);
                self.history_scroll = self.history_scroll.saturating_add(10).min(max);
            }
            KeyCode::Home => {
                self.history_selected = 0;
                self.history_scroll = 0;
            }
            KeyCode::End => {
                let max = self.filtered_history_indices().len().saturating_sub(1);
                self.history_selected = max;
                self.history_scroll = max.saturating_sub(12);
            }
            KeyCode::Backspace => {
                self.history_filter.pop();
                self.history_selected = 0;
                self.history_scroll = 0;
            }
            KeyCode::Char(c) => {
                self.history_filter.push(c);
                self.history_selected = 0;
                self.history_scroll = 0;
            }
            _ => {}
        }
        self.clamp_history_selection();
        None
    }

    fn filtered_history_indices(&self) -> Vec<usize> {
        let query = self.history_filter.trim().to_ascii_lowercase();
        self.history_records
            .iter()
            .enumerate()
            .filter_map(|(idx, row)| {
                if query.is_empty()
                    || row.goal_text.to_ascii_lowercase().contains(&query)
                    || task_status_label(&row.status).contains(&query)
                    || row.id.to_string().contains(&query)
                {
                    Some(idx)
                } else {
                    None
                }
            })
            .collect()
    }

    fn selected_history_record(&self) -> Option<&TaskRecord> {
        let indices = self.filtered_history_indices();
        indices
            .get(self.history_selected.min(indices.len().saturating_sub(1)))
            .and_then(|idx| self.history_records.get(*idx))
    }

    fn clamp_history_selection(&mut self) {
        let max = self.filtered_history_indices().len().saturating_sub(1);
        self.history_selected = self.history_selected.min(max);
        self.history_scroll = self.history_scroll.min(max);
    }

    fn handle_goal_switcher_key(&mut self, key: KeyEvent) -> Option<Intent> {
        match key.code {
            KeyCode::Esc => {
                self.goal_switcher.open = false;
            }
            KeyCode::Enter => {
                let filtered = self.goal_switcher.filtered_indices(&self.goals);
                if let Some(goal_index) = filtered.get(self.goal_switcher.selected).copied() {
                    self.selected = goal_index;
                    self.clamp_focus_indices();
                    self.default_to_code_focus_for_reviewable_goal();
                }
                self.goal_switcher.open = false;
            }
            KeyCode::Up => {
                self.goal_switcher.selected = self.goal_switcher.selected.saturating_sub(1);
            }
            KeyCode::Down => {
                let max = self
                    .goal_switcher
                    .filtered_indices(&self.goals)
                    .len()
                    .saturating_sub(1);
                self.goal_switcher.selected = (self.goal_switcher.selected + 1).min(max);
            }
            KeyCode::Backspace => {
                self.goal_switcher.filter.pop();
                self.goal_switcher.selected = 0;
            }
            KeyCode::Char(c) => {
                self.goal_switcher.filter.push(c);
                self.goal_switcher.selected = 0;
            }
            _ => {}
        }
        self.goal_switcher.clamp(&self.goals);
        None
    }

    fn handle_mcp_approval_key(&mut self, key: KeyEvent) -> Option<Intent> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            return self.request_quit_confirmation();
        }

        match key.code {
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.resolve_selected_mcp_approval(true)
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                self.resolve_selected_mcp_approval(false)
            }
            KeyCode::Up => {
                self.mcp_approval_selected = self.mcp_approval_selected.saturating_sub(1);
                None
            }
            KeyCode::Down => {
                if self.mcp_approval_selected + 1 < self.pending_mcp_approvals.len() {
                    self.mcp_approval_selected += 1;
                }
                None
            }
            _ => None,
        }
    }

    fn handle_palette_key(&mut self, key: KeyEvent) -> Option<Intent> {
        let filtered_options = command_suggestions(&self.palette_input);

        match key.code {
            KeyCode::Esc => {
                self.mode = self.prev_mode;
                None
            }
            KeyCode::Enter => {
                if filtered_options.is_empty() {
                    return None;
                }
                let selected = filtered_options[self.palette_selected % filtered_options.len()];
                self.mode = self.prev_mode;
                self.apply_slash_action(selected.action.clone())
            }
            KeyCode::Up => {
                self.palette_selected = self.palette_selected.saturating_sub(1);
                None
            }
            KeyCode::Down => {
                if !filtered_options.is_empty() {
                    self.palette_selected =
                        (self.palette_selected + 1).min(filtered_options.len() - 1);
                }
                None
            }
            KeyCode::Char(c) => {
                self.palette_input.push(c);
                self.palette_selected = 0;
                None
            }
            KeyCode::Backspace => {
                self.palette_input.pop();
                self.palette_selected = 0;
                None
            }
            _ => None,
        }
    }

    fn handle_goal_key(&mut self, key: KeyEvent) -> Option<Intent> {
        // Ctrl+Up / Ctrl+Down navigate the checkpoint picker inside the
        // currently selected goal.  'r' triggers a rollback to the
        // cursor-highlighted checkpoint.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Up => {
                    if let Some(g) = self.goals.get_mut(self.selected) {
                        let max = g.state.as_ref().map(|s| s.checkpoints.len()).unwrap_or(0);
                        if max > 0 {
                            let cur = g.checkpoint_cursor.unwrap_or(0);
                            g.checkpoint_cursor = Some(cur.saturating_sub(1));
                        }
                    }
                    return None;
                }
                KeyCode::Down => {
                    if let Some(g) = self.goals.get_mut(self.selected) {
                        let max = g.state.as_ref().map(|s| s.checkpoints.len()).unwrap_or(0);
                        if max > 0 {
                            let cur = g.checkpoint_cursor.unwrap_or(0);
                            g.checkpoint_cursor = Some((cur + 1).min(max.saturating_sub(1)));
                        }
                    }
                    return None;
                }
                _ => {}
            }
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('w') => {
                    self.goal_prompt.delete_word_before_cursor();
                    return None;
                }
                KeyCode::Char('u') => {
                    self.goal_prompt.clear_before_cursor();
                    return None;
                }
                KeyCode::Char('k') => {
                    self.goal_prompt.clear_after_cursor();
                    return None;
                }
                KeyCode::Char('v') => return Some(Intent::PasteClipboard),
                _ => {}
            }
        }
        match key.code {
            KeyCode::Enter => {
                let submission = self.goal_prompt.take_submission()?;
                let display_text = submission.display_text;
                let text = submission.model_text;
                // Guard against the user pasting an API key into the
                // goal bar (it has happened — see issue with AIza... in
                // the wild). Keys would otherwise be sent verbatim to
                // the LLM as the user prompt, which is both unhelpful
                // and a credential leak. Detect, refuse, and surface a
                // clear redirect to Settings instead.
                if looks_like_api_key(text.trim()) {
                    self.help_open = false;
                    self.settings.message = Some(
                        "That looked like an API key — open Settings (/settings) and \
                         paste it into the API Key field, not the Goal bar."
                            .into(),
                    );
                    self.mode = Mode::Settings;
                    return None;
                }
                self.prompt_history.push(display_text.clone());
                self.prompt_history_cursor = None;
                if let Some(model) = display_text.trim().strip_prefix("/model set ") {
                    let model = model.trim();
                    if model.is_empty() {
                        self.command_notice = Some("Usage: /model set <name>".into());
                        return None;
                    }
                    self.settings.model = model.to_string();
                    self.command_notice = Some(format!("Model set to `{model}`"));
                    self.mode = Mode::Settings;
                    return Some(Intent::SaveSettings);
                }
                match parse_slash_command(&display_text) {
                    SlashParse::RunCommand => return Some(Intent::RunCommand(display_text)),
                    SlashParse::Command(action) => return self.apply_slash_action(action),
                    SlashParse::Unknown {
                        command,
                        suggestion,
                    } => {
                        let message = unknown_command_message(&command, suggestion.as_deref());
                        self.settings.message = Some(message.clone());
                        self.command_notice = Some(message);
                        return None;
                    }
                    SlashParse::NotCommand => {
                        if parse_prompt_command(&display_text).is_some() {
                            return Some(Intent::RunCommand(display_text));
                        }
                    }
                }
                self.goals.insert(0, GoalEntry::new(display_text));
                self.selected = 0;
                if self.mode == Mode::Task {
                    Some(Intent::QueueTask(text))
                } else {
                    Some(Intent::QueueGoal(text))
                }
            }
            KeyCode::Backspace => {
                self.command_notice = None;
                self.goal_prompt.delete_char_before_cursor();
                None
            }
            KeyCode::Left => {
                self.goal_prompt.move_left();
                None
            }
            KeyCode::Right => {
                self.goal_prompt.move_right();
                None
            }
            KeyCode::Home => {
                self.goal_prompt.move_home();
                None
            }
            KeyCode::End => {
                self.goal_prompt.move_end();
                None
            }
            KeyCode::Up => {
                if self.goal_prompt.is_empty() && !self.prompt_history.is_empty() {
                    let idx = self
                        .prompt_history_cursor
                        .map(|i| i.saturating_sub(1))
                        .unwrap_or_else(|| self.prompt_history.len().saturating_sub(1));
                    self.prompt_history_cursor = Some(idx);
                    if let Some(entry) = self.prompt_history.get(idx) {
                        self.goal_prompt.set_text(entry.clone());
                    }
                } else {
                    self.previous_goal();
                }
                None
            }
            KeyCode::Down => {
                if let Some(idx) = self.prompt_history_cursor {
                    if idx + 1 < self.prompt_history.len() {
                        let next = idx + 1;
                        self.prompt_history_cursor = Some(next);
                        if let Some(entry) = self.prompt_history.get(next) {
                            self.goal_prompt.set_text(entry.clone());
                        }
                    } else {
                        self.prompt_history_cursor = None;
                        self.goal_prompt.set_text("");
                    }
                } else {
                    self.next_goal();
                }
                None
            }
            KeyCode::Tab => {
                if let Some(completion) = complete_command_prefix(self.goal_prompt.display_text()) {
                    self.goal_prompt.set_text(completion);
                }
                None
            }
            KeyCode::Char('r') if self.goal_prompt.is_empty() => {
                if self
                    .current_goal()
                    .is_some_and(|goal| matches!(goal.status, TaskStatus::Failed { .. }))
                {
                    return self.retry_selected_goal_intent();
                }
                // Rollback shortcut — only when the goal bar is empty so the
                // user can still type words starting with 'r' normally.
                let goal_index = self.selected;
                if let Some(g) = self.goals.get(goal_index) {
                    if let (Some(cursor), Some(state)) = (g.checkpoint_cursor, g.state.as_ref()) {
                        if let Some(cp) = state.checkpoints.get(cursor) {
                            return Some(Intent::Rollback {
                                goal_index,
                                to_seq: cp.seq,
                            });
                        }
                    }
                }
                self.goal_prompt.insert_char('r');
                None
            }
            KeyCode::Char('p') if self.goal_prompt.is_empty() => {
                self.focus_view = FocusView::Problems;
                self.focus_scroll = 0;
                None
            }
            KeyCode::Char('f') if self.goal_prompt.is_empty() => {
                self.cycle_focus_view();
                None
            }
            KeyCode::Char('[') if self.goal_prompt.is_empty() => {
                match self.active_focus_view_for_current_goal() {
                    FocusView::Code => {
                        self.focused_changed_file = self.focused_changed_file.saturating_sub(1)
                    }
                    FocusView::Commands => {
                        self.focused_command_run = self.focused_command_run.saturating_sub(1)
                    }
                    _ => {}
                }
                None
            }
            KeyCode::Char(']') if self.goal_prompt.is_empty() => {
                match self.active_focus_view_for_current_goal() {
                    FocusView::Code => {
                        let max = self
                            .current_goal()
                            .map(focused_file_count)
                            .unwrap_or(0)
                            .saturating_sub(1);
                        self.focused_changed_file = (self.focused_changed_file + 1).min(max);
                    }
                    FocusView::Commands => {
                        let max = self.command_runs.len().saturating_sub(1);
                        self.focused_command_run = (self.focused_command_run + 1).min(max);
                    }
                    _ => {}
                }
                None
            }
            KeyCode::Char(c) => {
                self.command_notice = None;
                self.goal_prompt.insert_char(c);
                None
            }
            _ => None,
        }
    }

    fn handle_ask_key(&mut self, key: KeyEvent) -> Option<Intent> {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('w') => {
                    self.ask_prompt.delete_word_before_cursor();
                    return None;
                }
                KeyCode::Char('u') => {
                    self.ask_prompt.clear_before_cursor();
                    return None;
                }
                KeyCode::Char('k') => {
                    self.ask_prompt.clear_after_cursor();
                    return None;
                }
                KeyCode::Char('v') => return Some(Intent::PasteClipboard),
                _ => {}
            }
        }
        match key.code {
            KeyCode::Enter => {
                let submission = self.ask_prompt.take_submission()?;
                self.prompt_history.push(submission.display_text);
                self.prompt_history_cursor = None;
                self.ask_answer = None;
                self.ask_scroll = 0;
                self.ask_pending = true;
                Some(Intent::Ask(submission.model_text))
            }
            KeyCode::Backspace => {
                self.ask_prompt.delete_char_before_cursor();
                None
            }
            KeyCode::Left => {
                self.ask_prompt.move_left();
                None
            }
            KeyCode::Right => {
                self.ask_prompt.move_right();
                None
            }
            KeyCode::Home => {
                self.ask_prompt.move_home();
                None
            }
            KeyCode::End => {
                self.ask_prompt.move_end();
                None
            }
            KeyCode::Char(c) => {
                self.ask_prompt.insert_char(c);
                None
            }
            _ => None,
        }
    }

    fn handle_settings_key(&mut self, key: KeyEvent) -> Option<Intent> {
        // --- Model picker overlay navigation ---
        // When the picker is open it consumes all keys so nothing leaks
        // to the field-navigation layer below.
        if self.settings.picker_open {
            match key.code {
                KeyCode::Esc => {
                    self.settings.picker_open = false;
                    self.settings.picker.filter.clear();
                    self.settings.picker.rebuild_filter();
                }
                KeyCode::Enter => {
                    if let Some(m) = self
                        .settings
                        .picker
                        .filtered
                        .get(self.settings.picker.selected)
                    {
                        self.settings.model = m.clone();
                        self.settings.model_ok = None;
                        self.settings.message =
                            Some(format!("Model set to `{m}`. Ctrl+T to test."));
                    }
                    self.settings.picker_open = false;
                    self.settings.picker.filter.clear();
                    self.settings.picker.rebuild_filter();
                    return Some(Intent::SaveSettings);
                }
                KeyCode::Up if self.settings.picker.selected > 0 => {
                    self.settings.picker.selected -= 1;
                    if self.settings.picker.selected < self.settings.picker.scroll {
                        self.settings.picker.scroll = self.settings.picker.selected;
                    }
                }
                KeyCode::Down => {
                    let max = self.settings.picker.filtered.len().saturating_sub(1);
                    if self.settings.picker.selected < max {
                        self.settings.picker.selected += 1;
                        const VISIBLE: usize = 8;
                        if self.settings.picker.selected >= self.settings.picker.scroll + VISIBLE {
                            self.settings.picker.scroll =
                                self.settings.picker.selected + 1 - VISIBLE;
                        }
                    }
                }
                KeyCode::Backspace => {
                    self.settings.picker.filter.pop();
                    self.settings.picker.selected = 0;
                    self.settings.picker.scroll = 0;
                    self.settings.picker.rebuild_filter();
                }
                KeyCode::Char(c) => {
                    self.settings.picker.filter.push(c);
                    self.settings.picker.selected = 0;
                    self.settings.picker.scroll = 0;
                    self.settings.picker.rebuild_filter();
                }
                _ => {}
            }
            return None;
        }

        // --- Global shortcuts (active regardless of focused field) ---
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('t') | KeyCode::Char('T') => {
                    return Some(Intent::TestConnection);
                }
                KeyCode::Char('d') | KeyCode::Char('D') => {
                    return Some(Intent::DetectModels);
                }
                _ => {}
            }
        }

        // --- Provider field: left/right cycles the provider list ---
        if self.settings.active_field == SettingsField::Provider
            && matches!(key.code, KeyCode::Left | KeyCode::Right)
        {
            let providers = crate::config::KNOWN_PROVIDERS;
            if !providers.is_empty() {
                let cur = providers
                    .iter()
                    .position(|p| *p == self.settings.provider)
                    .unwrap_or(0);
                let delta = if matches!(key.code, KeyCode::Right) {
                    1
                } else {
                    providers.len() - 1
                };
                let next = (cur + delta) % providers.len();
                self.settings.provider = providers[next].to_string();
                // Clear the model + cached list — a model name for
                // one provider is meaningless on another.
                self.settings.model.clear();
                self.settings.picker.all_models.clear();
                self.settings.picker.filtered.clear();
                self.settings.model_ok = None;
                self.settings.message = None;
                return Some(Intent::SaveSettings);
            }
            return None;
        }

        // --- Model field: Enter opens the picker ---
        if self.settings.active_field == SettingsField::Model && key.code == KeyCode::Enter {
            return Some(Intent::OpenModelPicker);
        }

        match key.code {
            KeyCode::Enter => Some(Intent::SaveSettings),
            KeyCode::Tab => {
                self.settings.active_field = match self.settings.active_field {
                    SettingsField::Provider => SettingsField::Model,
                    SettingsField::Model => SettingsField::ApiKey,
                    SettingsField::ApiKey => SettingsField::AccountId,
                    SettingsField::AccountId => SettingsField::BaseUrl,
                    SettingsField::BaseUrl => SettingsField::MaxTokens,
                    SettingsField::MaxTokens => SettingsField::MaxUsdCents,
                    SettingsField::MaxUsdCents => SettingsField::Provider,
                };
                None
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.settings.active_field = match self.settings.active_field {
                    SettingsField::Provider => SettingsField::MaxUsdCents,
                    SettingsField::Model => SettingsField::Provider,
                    SettingsField::ApiKey => SettingsField::Model,
                    SettingsField::AccountId => SettingsField::ApiKey,
                    SettingsField::BaseUrl => SettingsField::AccountId,
                    SettingsField::MaxTokens => SettingsField::BaseUrl,
                    SettingsField::MaxUsdCents => SettingsField::MaxTokens,
                };
                None
            }
            KeyCode::Down => {
                self.settings.active_field = match self.settings.active_field {
                    SettingsField::Provider => SettingsField::Model,
                    SettingsField::Model => SettingsField::ApiKey,
                    SettingsField::ApiKey => SettingsField::AccountId,
                    SettingsField::AccountId => SettingsField::BaseUrl,
                    SettingsField::BaseUrl => SettingsField::MaxTokens,
                    SettingsField::MaxTokens => SettingsField::MaxUsdCents,
                    SettingsField::MaxUsdCents => SettingsField::Provider,
                };
                None
            }
            KeyCode::Backspace => {
                match self.settings.active_field {
                    SettingsField::Provider => {
                        self.settings.provider.pop();
                    }
                    SettingsField::Model => {
                        self.settings.model.pop();
                        self.settings.model_ok = None;
                    }
                    SettingsField::ApiKey => {
                        self.settings.api_key.pop();
                    }
                    SettingsField::AccountId => {
                        self.settings.account_id.pop();
                    }
                    SettingsField::BaseUrl => {
                        self.settings.base_url.pop();
                    }
                    SettingsField::MaxTokens => {
                        self.settings.max_tokens.pop();
                    }
                    SettingsField::MaxUsdCents => {
                        self.settings.max_usd_cents.pop();
                    }
                }
                self.settings.message = None;
                Some(Intent::SaveSettings)
            }
            KeyCode::Char(c) => {
                match self.settings.active_field {
                    SettingsField::Provider => {
                        self.settings.provider.push(c);
                    }
                    SettingsField::Model => {
                        self.settings.model.push(c);
                        self.settings.model_ok = None;
                    }
                    SettingsField::ApiKey => {
                        self.settings.api_key.push(c);
                    }
                    SettingsField::AccountId => {
                        self.settings.account_id.push(c);
                    }
                    SettingsField::BaseUrl => {
                        self.settings.base_url.push(c);
                    }
                    SettingsField::MaxTokens => {
                        if c.is_ascii_digit() {
                            self.settings.max_tokens.push(c);
                        }
                    }
                    SettingsField::MaxUsdCents => {
                        if c.is_ascii_digit() {
                            self.settings.max_usd_cents.push(c);
                        }
                    }
                }
                self.settings.message = None;
                Some(Intent::SaveSettings)
            }
            _ => None,
        }
    }
}

/// Side-effecting actions the event loop must execute on the app's behalf.
///
/// Kept as an enum so [`App::handle_key`] stays pure and the driver decides
/// how to issue them (spawn an orchestrator task, call the ask provider,
/// teardown, etc.).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// User submitted a goal — caller should spawn an orchestration task.
    QueueGoal(String),
    /// User submitted a direct single subtask. Skips planner decomposition.
    QueueTask(String),
    /// User submitted an ask-mode question. Isolated from goal context.
    Ask(String),
    /// User submitted `/run <cmd>` or `!<cmd>` from the prompt bar.
    RunCommand(String),
    /// User requested copying the current focus surface to the OS clipboard.
    CopyFocus(String),
    /// User requested OS clipboard paste. On Windows this supports Win+V
    /// selections that land in the clipboard but are not emitted as terminal
    /// bracketed-paste events.
    PasteClipboard,
    /// User triggered a rollback to a specific checkpoint seq for the
    /// currently selected goal.
    Rollback { goal_index: usize, to_seq: u32 },
    /// User requested context compaction for the selected active goal.
    CompactContext { goal_index: usize },
    /// User requested cancellation of the selected active goal.
    StopGoal { goal_index: usize },
    /// User approved or denied a pending MCP approval prompt.
    ResolveMcpApproval { approval_id: u64, approved: bool },
    /// Save settings.
    SaveSettings,
    /// Test the configured provider/model/api-key by issuing one tiny
    /// chat request. Result lands back in `SettingsState::message`.
    TestConnection,
    /// Discover models accessible to the configured provider/api-key.
    /// On success the first sensible model is written into the Model
    /// field; the full list is summarised in `SettingsState::message`.
    DetectModels,
    /// Open the model picker overlay and kick off a background model
    /// list fetch if the list is empty.
    OpenModelPicker,
    /// Open the memory browser and refresh records from the store.
    OpenMemory,
    /// Open the history browser and refresh rows from the store.
    OpenHistory,
    /// Revoke trust for the current workspace.
    RevokeCurrentTrust,
    /// User accepted the workspace-trust prompt — proceed into the TUI.
    AcceptTrust,
    /// User declined trust. Caller should exit cleanly.
    DeclineTrust,
    /// Quit the TUI.
    Quit,
}

// ---------------------------------------------------------------------------
// Token-savings rendering
// ---------------------------------------------------------------------------

/// Render the token/savings summary surfaced at the bottom of the centre
/// pane. Plain-text form — pulled out so it's unit-testable. The live TUI
/// uses [`render_savings_line_styled`] for the colored version.
pub fn render_savings_line(state: Option<&GlobalState>) -> String {
    let Some(s) = state else {
        return "  ⚡ — tok  |  saved — vs naive  |  Σ baseline: —".into();
    };
    let pct = savings_pct(s);
    let pct_txt = pct.map(|p| format!("{p}%")).unwrap_or_else(|| "—".into());
    format!(
        "  ⚡ {} tok  |  saved {} vs naive  |  Σ baseline: {}",
        s.tokens_used, pct_txt, s.estimated_naive_tokens
    )
}

fn savings_pct(s: &GlobalState) -> Option<i64> {
    if s.estimated_naive_tokens == 0 {
        return None;
    }
    let diff = s.estimated_naive_tokens as i64 - s.tokens_used as i64;
    Some(((diff as f64 / s.estimated_naive_tokens as f64) * 100.0).round() as i64)
}

/// Styled version of [`render_savings_line`]. Colors the savings percentage
/// according to how much we saved: SUCCESS when >50%, WARN when 10–50%.
/// When `new_best_ticks > 0` an amber "★ best!" flash is appended so the
/// user knows they just beat their session record.
pub fn render_savings_line_styled(
    state: Option<&GlobalState>,
    best_savings_pct: Option<i64>,
    new_best_ticks: u8,
) -> Line<'static> {
    let Some(s) = state else {
        return Line::from(Span::styled(
            "  ⚡ — tok  |  saved — vs naive  |  Σ baseline: —",
            Style::default().fg(MUTED),
        ));
    };
    let pct = savings_pct(s);
    let (pct_txt, pct_style) = match pct {
        Some(p) if p > 50 => (
            format!("{p}%"),
            Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
        ),
        Some(p) if p >= 10 => (
            format!("{p}%"),
            Style::default().fg(WARN).add_modifier(Modifier::BOLD),
        ),
        Some(p) => (format!("{p}%"), Style::default().fg(MUTED)),
        None => ("—".into(), Style::default().fg(MUTED)),
    };
    let best_span = match best_savings_pct {
        Some(b) if new_best_ticks > 0 => Span::styled(
            format!("  ★ NEW BEST {b}%!"),
            Style::default()
                .fg(SUCCESS)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        ),
        Some(b) => Span::styled(format!("  best {b}%"), Style::default().fg(MUTED)),
        None => Span::raw(""),
    };
    // Gradient mini-gauge showing how much of the naive baseline we've
    // saved (full bar = 100% savings, empty bar = no savings).
    let frac = pct
        .map(|p| (p as f32 / 100.0).clamp(0.0, 1.0))
        .unwrap_or(0.0);
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(
        "  ⚡ ",
        Style::default().fg(WARN).add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(
        format!("{} tok", s.tokens_used),
        Style::default().fg(ACCENT_HI).add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled("  saved ", Style::default().fg(MUTED)));
    spans.push(Span::styled(pct_txt, pct_style));
    spans.push(Span::raw("  "));
    spans.extend(gradient_bar(frac, 14));
    spans.push(Span::styled(
        format!("  vs Σ {}", s.estimated_naive_tokens),
        Style::default().fg(MUTED),
    ));
    spans.push(best_span);
    Line::from(spans)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Top-level frame renderer. Public so integration tests can render the
/// whole UI into a [`ratatui::backend::TestBackend`] and assert.
pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    // Once a goal is queued, collapse the large ASCII logo down to a slim
    // one-line header so the work area gets the screen real estate.
    let want_full_logo = app.goals.is_empty() && area.width >= LOGO_WIDTH_THRESHOLD;
    let splash_h: u16 = if want_full_logo {
        LOGO.len() as u16 + 2
    } else {
        1
    };

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(splash_h),
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);

    let splash_row = outer[0];
    let body = outer[1];
    let input_row = outer[2];
    let footer_row = outer[3];

    render_splash(frame, splash_row, app);

    let body_chunks: Vec<Rect> = if app.mode == Mode::Ask {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(25),
                Constraint::Percentage(45),
                Constraint::Percentage(30),
            ])
            .split(body)
            .to_vec()
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
            .split(body)
            .to_vec()
    };

    render_goals(frame, body_chunks[0], app);
    if app.mode == Mode::Memory {
        render_memory(frame, body_chunks[1], app);
    } else if app.mode == Mode::History {
        render_history(frame, body_chunks[1], app);
    } else if app.flight_log_open {
        render_flight_log(frame, body_chunks[1], app);
    } else {
        render_centre(frame, body_chunks[1], app);
    }
    if app.mode == Mode::Ask {
        render_ask(frame, body_chunks[2], app);
    }
    render_input(frame, input_row, app);
    if command_drawer_visible(app) {
        render_command_drawer(frame, area, input_row, app);
    }
    render_footer(frame, footer_row, app);

    if app.mode == Mode::Settings {
        render_settings(frame, area, app);
    }
    if app.mode == Mode::CommandPalette {
        render_palette(frame, area, app);
    }
    if app.goal_switcher.open {
        render_goal_switcher(frame, area, app);
    }
    if app.help_open {
        render_help(frame, area);
    }
    if !app.pending_mcp_approvals.is_empty() {
        render_mcp_approval(frame, area, app);
    }
    if app.quit_confirmation_open {
        render_quit_confirmation(frame, area, app);
    }
}

fn command_drawer_visible(app: &App) -> bool {
    matches!(app.mode, Mode::Goal | Mode::Task)
        && app.goal_prompt.display_text().starts_with('/')
        && !app.help_open
        && !app.quit_confirmation_open
}

fn render_command_drawer(frame: &mut Frame, area: Rect, input_row: Rect, app: &App) {
    let suggestions = command_suggestions(app.goal_prompt.display_text());
    if suggestions.is_empty() {
        return;
    }
    let max_rows = suggestions.len().min(6);
    let popup_h = max_rows as u16 + 2;
    if input_row.y <= area.y + 1 {
        return;
    }
    let popup_w = area.width.saturating_sub(6).clamp(44, 90);
    let popup_area = Rect {
        x: area.x + 3,
        y: input_row.y.saturating_sub(popup_h),
        width: popup_w,
        height: popup_h,
    };
    frame.render_widget(Clear, popup_area);
    let block = Block::default()
        .title(Span::styled(
            " Commands ",
            Style::default().fg(ACCENT_HI).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(BG_DEEP));
    frame.render_widget(block.clone(), popup_area);
    let inner = block.inner(popup_area);
    let lines: Vec<ListItem> = suggestions
        .iter()
        .take(max_rows)
        .map(|spec| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    spec.name,
                    Style::default().fg(ACCENT_HI).add_modifier(Modifier::BOLD),
                ),
                Span::raw(if spec.args.is_empty() { "" } else { " " }),
                Span::styled(spec.args, Style::default().fg(WARN)),
                Span::styled("  ", Style::default().fg(DIM)),
                Span::styled(spec.description, Style::default().fg(MUTED)),
            ]))
            .style(Style::default().bg(BG_DEEP))
        })
        .collect();
    frame.render_widget(List::new(lines), inner);
}

/// Centred modal listing every keybinding in one place. Toggled by `?`,
/// dismissed with `?` again or `Esc`. Drawn last so it always sits on top.
fn render_help(frame: &mut Frame, area: Rect) {
    let rows: &[(&str, &str)] = &[
        ("Enter", "submit goal / question"),
        ("/settings", "open provider, model, and budget settings"),
        ("/status", "show provider, model, session, and token state"),
        (
            "/memory",
            "inspect memories that can influence future tasks",
        ),
        ("/review", "use `phonton review latest` outside the TUI"),
        ("/problems", "inspect verifier failures and repair hints"),
        ("/retry", "repair the selected failed goal with diagnostics"),
        ("/why-tokens", "explain latest prompt token buckets"),
        ("/ask", "ask a stateless question without queueing a goal"),
        ("/commands", "show command and keyboard help"),
        ("/run", "run a command through the sandbox"),
        ("!", "run command shorthand, e.g. !npm test"),
        ("Ctrl+/", "open the command palette"),
        ("?", "toggle this help"),
        ("Ctrl+;", "toggle the Ask side panel"),
        ("Ctrl+V", "paste from Windows clipboard"),
        ("Shift+L", "toggle the Flight Log"),
        ("Ctrl+D", "delete the selected goal"),
        ("Ctrl+W", "delete the previous word in the input"),
        ("Ctrl+U/K", "clear before / after the caret"),
        ("Tab", "complete slash command prefix"),
        ("↑ / ↓", "move selection in Goals (or palette)"),
        ("← / →", "move caret in the input bar"),
        ("Home / End", "jump to start/end of the input"),
        ("Ctrl+↑↓", "move the checkpoint cursor"),
        ("p", "open Problems focus (input empty)"),
        (
            "r",
            "retry failed goal or rollback checkpoint (input empty)",
        ),
        ("Ctrl+C", "ask to save and quit"),
        ("Esc", "close overlay / cancel / ask to quit"),
    ];

    // Fit the modal to the longest description so wrapping never bites.
    let longest = rows
        .iter()
        .map(|(k, v)| k.chars().count() + v.chars().count() + 4)
        .max()
        .unwrap_or(40);
    let popup_w = (longest as u16 + 6)
        .min(area.width.saturating_sub(2))
        .max(40);
    let popup_h = (rows.len() as u16 + 6).min(area.height.saturating_sub(2));
    let popup_area = Rect {
        x: area.x + (area.width.saturating_sub(popup_w)) / 2,
        y: area.y + (area.height.saturating_sub(popup_h)) / 2,
        width: popup_w,
        height: popup_h,
    };

    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .title(Span::styled(
            " Keyboard Shortcuts ",
            Style::default().fg(ACCENT_HI).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(BG_DEEP));
    frame.render_widget(block.clone(), popup_area);
    let inner = block.inner(popup_area);

    let key_w = rows
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(8);
    let mut lines: Vec<Line> = Vec::with_capacity(rows.len() + 2);
    lines.push(Line::raw(""));
    for (k, v) in rows {
        let pad = " ".repeat(key_w.saturating_sub(k.chars().count()));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("{}{}", k, pad),
                Style::default().fg(ACCENT_HI).add_modifier(Modifier::BOLD),
            ),
            Span::raw("   "),
            Span::styled((*v).to_string(), Style::default().fg(Color::White)),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "  press ? or Esc to close",
        Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
    )));

    let p = Paragraph::new(lines).style(Style::default().bg(BG_DEEP));
    frame.render_widget(p, inner);
}

/// Confirmation modal shown before ending an interactive session.
fn render_quit_confirmation(frame: &mut Frame, area: Rect, app: &App) {
    let popup_w = area.width.saturating_sub(4).clamp(48, 76);
    let popup_h = area.height.saturating_sub(2).clamp(13, 17);
    let popup_area = Rect {
        x: area.x + (area.width.saturating_sub(popup_w)) / 2,
        y: area.y + (area.height.saturating_sub(popup_h)) / 2,
        width: popup_w,
        height: popup_h,
    };

    frame.render_widget(Clear, popup_area);
    let block = Block::default()
        .title(Span::styled(
            " End Session ",
            Style::default().fg(WARN).add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Line::from(vec![
            Span::styled(" Enter/Y save + exit ", Style::default().fg(SUCCESS)),
            Span::styled("  N/Esc cancel ", Style::default().fg(MUTED)),
        ]))
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(WARN))
        .style(Style::default().bg(BG_DEEP));
    frame.render_widget(block.clone(), popup_area);

    let totals = app.session_totals();
    let saved_label = if totals.estimated_tokens_saved >= 0 {
        "saved"
    } else {
        "over"
    };
    let saved_value = totals.estimated_tokens_saved.saturating_abs();
    let best = totals
        .best_savings_pct
        .map(|p| format!("{p}%"))
        .unwrap_or_else(|| "n/a".into());
    let lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "  Save this workspace session and exit Phonton?",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled("  Goals      ", Style::default().fg(MUTED)),
            Span::styled(
                totals.goals.to_string(),
                Style::default().fg(ACCENT_HI).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  done ", Style::default().fg(MUTED)),
            Span::styled(totals.completed.to_string(), Style::default().fg(SUCCESS)),
            Span::styled("  review ", Style::default().fg(MUTED)),
            Span::styled(totals.reviewing.to_string(), Style::default().fg(WARN)),
            Span::styled("  failed ", Style::default().fg(MUTED)),
            Span::styled(totals.failed.to_string(), Style::default().fg(DANGER)),
        ]),
        Line::from(vec![
            Span::styled("  Tokens     ", Style::default().fg(MUTED)),
            Span::styled(
                totals.tokens_used.to_string(),
                Style::default().fg(ACCENT_HI).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  vs baseline ", Style::default().fg(MUTED)),
            Span::styled(
                totals.naive_baseline_tokens.to_string(),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Efficiency ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{saved_label} {saved_value}"),
                Style::default().fg(if totals.estimated_tokens_saved >= 0 {
                    SUCCESS
                } else {
                    WARN
                }),
            ),
            Span::styled("  best ", Style::default().fg(MUTED)),
            Span::styled(best, Style::default().fg(SUCCESS)),
        ]),
        Line::raw(""),
        Line::from(Span::styled(
            "  The session snapshot is saved locally for `phonton -r`.",
            Style::default().fg(MUTED),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .style(Style::default().bg(BG_DEEP)),
        block.inner(popup_area),
    );
}

/// Focused modal for approval-gated MCP operations.
fn render_mcp_approval(frame: &mut Frame, area: Rect, app: &App) {
    let Some(prompt) = app.active_mcp_approval() else {
        return;
    };

    let popup_w = if area.width > 50 {
        area.width.saturating_sub(4).min(86)
    } else {
        area.width.saturating_sub(2).max(1)
    };
    let popup_h = if area.height > 16 {
        14
    } else {
        area.height.saturating_sub(2).max(1)
    };
    let popup_area = Rect {
        x: area.x + (area.width.saturating_sub(popup_w)) / 2,
        y: area.y + (area.height.saturating_sub(popup_h)) / 2,
        width: popup_w,
        height: popup_h,
    };

    frame.render_widget(Clear, popup_area);

    let count = app.pending_mcp_approvals.len();
    let title = if count > 1 {
        format!(" MCP Approval {}/{} ", app.mcp_approval_selected + 1, count)
    } else {
        " MCP Approval ".into()
    };
    let block = Block::default()
        .title(Span::styled(
            title,
            Style::default().fg(WARN).add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Line::from(vec![
            Span::styled(" Enter/Y approve ", Style::default().fg(SUCCESS)),
            Span::styled("  N/Esc deny ", Style::default().fg(DANGER)),
            Span::styled("  Up/Down select ", Style::default().fg(MUTED)),
        ]))
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(WARN))
        .style(Style::default().bg(BG_DEEP));
    frame.render_widget(block.clone(), popup_area);

    let inner = block.inner(popup_area);
    let permissions = permissions_label(&prompt.permissions);
    let reason_width = inner.width.saturating_sub(4) as usize;
    let mut lines: Vec<Line> = vec![
        Line::raw(""),
        Line::from(vec![
            Span::styled("  server  ", Style::default().fg(MUTED)),
            Span::styled(
                prompt.server_id.to_string(),
                Style::default().fg(ACCENT_HI).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("  tool    ", Style::default().fg(MUTED)),
            Span::styled(
                prompt.tool_name.clone(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("  perms   ", Style::default().fg(MUTED)),
            Span::styled(permissions, Style::default().fg(WARN)),
        ]),
        Line::raw(""),
        Line::from(Span::styled(
            "  Reason",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
    ];
    for line in wrap_text(&prompt.reason, reason_width).into_iter().take(4) {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(line, Style::default().fg(Color::White)),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "  Approving lets this one MCP operation run. Denying returns the failure to the worker.",
        Style::default().fg(MUTED),
    )));

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .style(Style::default().bg(BG_DEEP)),
        inner,
    );
}

fn render_splash(frame: &mut Frame, area: Rect, app: &App) {
    // Keep the normal ANSI Shadow wordmark. The terminal-corruption fix is
    // keeping semantic-index downloads silent while Ratatui owns the screen.
    let header_phase = (app.spinner_frame as f32) * LOGO_SHIMMER_SPEED;
    let logo_phase = 0.17;
    if area.height >= LOGO.len() as u16 + 2 && area.width >= LOGO_WIDTH_THRESHOLD {
        let mut lines: Vec<Line> = Vec::with_capacity(LOGO.len() + 2);
        lines.push(Line::raw(""));
        lines.extend(
            LOGO.iter()
                .enumerate()
                .map(|(row_idx, row)| logo_line(row, logo_phase, row_idx)),
        );
        lines.push(Line::from(Span::styled(
            format!("v{}", env!("CARGO_PKG_VERSION")),
            Style::default().fg(MUTED),
        )));
        let p = Paragraph::new(lines)
            .alignment(Alignment::Center)
            .style(Style::default().bg(BG_DEEP));
        frame.render_widget(p, area);
    } else {
        // Compact one-line header - gradient "phonton" + dim subtitle.
        let mut spans = gradient_line("✦ phonton", header_phase * 0.8, true).spans;
        spans.push(Span::styled("  ── ", Style::default().fg(DIM)));
        spans.push(Span::styled(
            "agentic dev environment",
            Style::default().fg(MUTED),
        ));
        spans.push(Span::styled("  ·  ", Style::default().fg(DIM)));
        spans.push(Span::styled(
            format!("v{}", env!("CARGO_PKG_VERSION")),
            Style::default().fg(MUTED),
        ));
        let p = Paragraph::new(Line::from(spans))
            .alignment(Alignment::Center)
            .style(Style::default().bg(BG_DEEP));
        frame.render_widget(p, area);
    }
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    let key = Style::default()
        .bg(DIM)
        .fg(ACCENT_HI)
        .add_modifier(Modifier::BOLD);
    let txt = Style::default().fg(MUTED);
    let dim = Style::default().fg(DIM);
    let sep = Span::styled("  ·  ", dim);

    let spans: Vec<Span<'static>> = match app.mode {
        Mode::Goal | Mode::Task => vec![
            Span::styled("Enter", key),
            Span::styled(" run  ", txt),
            sep.clone(),
            Span::styled("/run", key),
            Span::styled(" command  ", txt),
            sep.clone(),
            Span::styled("!", key),
            Span::styled(" cmd  ", txt),
            sep.clone(),
            Span::styled("Ctrl+V", key),
            Span::styled(" paste  ", txt),
            sep.clone(),
            Span::styled("?", key),
            Span::styled(" help  ", txt),
            sep.clone(),
            Span::styled("Alt+↑↓", key),
            Span::styled(" goal  ", txt),
            sep.clone(),
            Span::styled("f", key),
            Span::styled(" focus  ", txt),
            sep.clone(),
            Span::styled("Ctrl+;", key),
            Span::styled(" ask  ", txt),
            sep.clone(),
            Span::styled("Shift+L", key),
            Span::styled(" log  ", txt),
            sep.clone(),
            Span::styled("Ctrl+D", key),
            Span::styled(" del  ", txt),
            sep,
            Span::styled("Esc", key),
            Span::styled(" exit?", txt),
        ],
        Mode::Ask => vec![
            Span::styled("Enter", key),
            Span::styled(" send  ", txt),
            sep.clone(),
            Span::styled("Ctrl+V", key),
            Span::styled(" paste  ", txt),
            sep.clone(),
            Span::styled("Ctrl+;", key),
            Span::styled(" close ask  ", txt),
            sep,
            Span::styled("Esc", key),
            Span::styled(" cancel", txt),
        ],
        Mode::Settings => vec![
            Span::styled("Enter", key),
            Span::styled(" save  ", txt),
            sep.clone(),
            Span::styled("Tab", key),
            Span::styled(" next field  ", txt),
            sep.clone(),
            Span::styled("←→", key),
            Span::styled(" cycle provider  ", txt),
            sep,
            Span::styled("Esc", key),
            Span::styled(" cancel", txt),
        ],
        Mode::Memory | Mode::History => vec![
            Span::styled("/", key),
            Span::styled(" commands  ", txt),
            sep.clone(),
            Span::styled("Esc", key),
            Span::styled(" back to goals", txt),
        ],
        Mode::CommandPalette => vec![
            Span::styled("type", key),
            Span::styled(" filter  ", txt),
            sep.clone(),
            Span::styled("↑↓", key),
            Span::styled(" select  ", txt),
            sep.clone(),
            Span::styled("Enter", key),
            Span::styled(" run  ", txt),
            sep,
            Span::styled("Esc", key),
            Span::styled(" close", txt),
        ],
    };

    let p = Paragraph::new(Line::from(spans)).alignment(Alignment::Center);
    frame.render_widget(p, area);
}

fn render_palette(frame: &mut Frame, area: Rect, app: &App) {
    let all_options = command_suggestions("");
    let filtered_options = command_suggestions(&app.palette_input);

    let block = Block::default()
        .title(Line::from(vec![
            Span::styled(" ", Style::default()),
            Span::styled("◆ ", Style::default().fg(VIOLET)),
            Span::styled(
                "Command Palette",
                Style::default().fg(ACCENT_HI).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ", Style::default()),
        ]))
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Thick)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(BG_DEEP));

    let popup_w = 72;
    let popup_h = (all_options.len() as u16 + 4).max(8);
    let popup_area = Rect {
        x: area.x + (area.width.saturating_sub(popup_w)) / 2,
        y: area.y + (area.height.saturating_sub(popup_h)) / 2,
        width: popup_w.min(area.width),
        height: popup_h.min(area.height),
    };

    frame.render_widget(Clear, popup_area);
    frame.render_widget(block, popup_area);

    let inner = popup_area.inner(ratatui::layout::Margin {
        vertical: 1,
        horizontal: 2,
    });
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Search
            Constraint::Length(1), // Separator
            Constraint::Min(1),    // List
        ])
        .split(inner);

    let search_p = Paragraph::new(Line::from(vec![
        Span::styled(
            "/ ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::raw(&app.palette_input),
    ]));
    frame.render_widget(search_p, chunks[0]);
    frame.render_widget(
        Paragraph::new("─".repeat(inner.width as usize)).style(Style::default().fg(MUTED)),
        chunks[1],
    );

    let list_items: Vec<ListItem> = filtered_options
        .iter()
        .enumerate()
        .map(|(i, spec)| {
            let selected = i == app.palette_selected % filtered_options.len().max(1);
            let (marker, style) = if selected {
                (
                    "▍ ",
                    Style::default()
                        .fg(BG_DEEP)
                        .bg(ACCENT_HI)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                ("  ", Style::default().fg(MUTED))
            };
            ListItem::new(Line::from(format!(
                "{marker}{}",
                render_command_label(spec)
            )))
            .style(style)
        })
        .collect();

    if filtered_options.is_empty() {
        frame.render_widget(
            Paragraph::new("  (no matches)").style(Style::default().fg(MUTED)),
            chunks[2],
        );
    } else {
        let list = List::new(list_items);
        frame.render_widget(list, chunks[2]);
    }
}

fn render_goal_switcher(frame: &mut Frame, area: Rect, app: &App) {
    let popup_w = area.width.saturating_sub(8).clamp(48, 90);
    let popup_h = area.height.saturating_sub(6).clamp(8, 18);
    let popup_area = Rect {
        x: area.x + (area.width.saturating_sub(popup_w)) / 2,
        y: area.y + (area.height.saturating_sub(popup_h)) / 2,
        width: popup_w.min(area.width),
        height: popup_h.min(area.height),
    };

    frame.render_widget(Clear, popup_area);
    let block = Block::default()
        .title(Span::styled(
            " Goals ",
            Style::default().fg(ACCENT_HI).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Thick)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(BG_DEEP));
    frame.render_widget(block, popup_area);

    let inner = popup_area.inner(ratatui::layout::Margin {
        vertical: 1,
        horizontal: 2,
    });
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "/",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::raw(app.goal_switcher.filter.clone()),
        ])),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new("─".repeat(inner.width as usize)).style(Style::default().fg(MUTED)),
        chunks[1],
    );

    let filtered = app.goal_switcher.filtered_indices(&app.goals);
    let items: Vec<ListItem> = filtered
        .iter()
        .enumerate()
        .map(|(row, goal_idx)| {
            let goal = &app.goals[*goal_idx];
            let selected = row == app.goal_switcher.selected;
            let style = if selected {
                Style::default()
                    .fg(BG_DEEP)
                    .bg(ACCENT_HI)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(MUTED)
            };
            let status = if matches!(goal.status, TaskStatus::Failed { .. }) {
                goal_failure_kind(goal)
            } else {
                goal_status_label(&goal.status)
            };
            ListItem::new(Line::from(format!(
                "{:>2}. {:<7} {}",
                goal_idx + 1,
                status,
                short(&goal.description, inner.width.saturating_sub(14) as usize)
            )))
            .style(style)
        })
        .collect();
    if items.is_empty() {
        frame.render_widget(
            Paragraph::new("No matching goals.").style(Style::default().fg(MUTED)),
            chunks[2],
        );
    } else {
        frame.render_widget(List::new(items), chunks[2]);
    }
    frame.render_widget(
        Paragraph::new("Alt+1-9 jump · Enter select · Esc close").style(Style::default().fg(MUTED)),
        chunks[3],
    );
}

fn render_goals(frame: &mut Frame, area: Rect, app: &App) {
    let items: Vec<ListItem> = app
        .goals
        .iter()
        .enumerate()
        .map(|(i, g)| {
            let selected = i == app.selected;
            let (marker, base_style) = if selected {
                (
                    "▍ ",
                    Style::default().fg(ACCENT_HI).add_modifier(Modifier::BOLD),
                )
            } else {
                ("  ", Style::default().fg(MUTED))
            };
            let mut spans = vec![Span::styled(marker, base_style)];
            spans.push(Span::styled(
                format!("{:>2} ", i + 1),
                Style::default().fg(if selected { ACCENT_HI } else { MUTED }),
            ));
            spans.extend(status_tag_spans(&g.status, app.spinner_frame));
            if matches!(g.status, TaskStatus::Failed { .. }) {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(
                    goal_failure_kind(g),
                    Style::default().fg(WARN).add_modifier(Modifier::BOLD),
                ));
            }
            // Parallel-worker indicator: one spinner glyph per concurrently
            // active subtask, capped at 5 so the sidebar stays readable.
            // Each glyph is drawn at a different phase of the spinner so
            // the row visibly *moves* — making it obvious that multiple
            // workers are in flight at once, not just one.
            let active_count = g
                .state
                .as_ref()
                .map(|s| s.active_workers.len())
                .unwrap_or(0);
            if active_count > 0 {
                spans.push(Span::raw(" "));
                let visible = active_count.min(5);
                for i in 0..visible {
                    let frame_idx = (app.spinner_frame.wrapping_add(i * 2) / 4) % SPINNER.len();
                    let ch = SPINNER[frame_idx];
                    spans.push(Span::styled(
                        ch.to_string(),
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                    ));
                }
                if active_count > visible {
                    spans.push(Span::styled(
                        format!("+{}", active_count - visible),
                        Style::default().fg(MUTED),
                    ));
                }
            }
            spans.push(Span::raw(" "));
            spans.extend(artifact_text_spans(&short(&g.description, 40), base_style));
            ListItem::new(Line::from(spans))
        })
        .collect();
    let goal_count = app.goals.len();
    let title_text = if goal_count == 0 {
        " Goals ".to_string()
    } else {
        format!(" Goals ({}) ", goal_count)
    };
    let goals_focused = matches!(app.mode, Mode::Goal | Mode::Task);
    let goals_border = if goals_focused { ACCENT } else { DIM };
    let goals_border_type = if goals_focused {
        ratatui::widgets::BorderType::Thick
    } else {
        ratatui::widgets::BorderType::Rounded
    };
    let list = List::new(items).style(Style::default().bg(BG_PANEL)).block(
        Block::default()
            .title(Span::styled(
                title_text,
                Style::default().fg(ACCENT_HI).add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_type(goals_border_type)
            .border_style(Style::default().fg(goals_border))
            .style(Style::default().bg(BG_PANEL)),
    );

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(9)])
        .split(area);

    frame.render_widget(list, chunks[0]);

    let sys_info = vec![
        Line::from(vec![
            Span::styled(" ◆ ", Style::default().fg(ACCENT)),
            Span::styled("provider  ", Style::default().fg(MUTED)),
            Span::styled(
                &app.settings.provider,
                Style::default().fg(ACCENT_HI).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(" ⌬ ", Style::default().fg(VIOLET)),
            Span::styled("model     ", Style::default().fg(MUTED)),
            Span::styled(
                if app.settings.model.is_empty() {
                    "(default)"
                } else {
                    &app.settings.model
                },
                Style::default().fg(ACCENT_HI),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                " ▣ ",
                Style::default().fg(if app.flight_log_open { SUCCESS } else { DIM }),
            ),
            Span::styled("log       ", Style::default().fg(MUTED)),
            Span::styled(
                if app.flight_log_open {
                    "Open"
                } else {
                    "Closed"
                },
                Style::default().fg(if app.flight_log_open { SUCCESS } else { MUTED }),
            ),
        ]),
        Line::from(vec![
            Span::styled(" PM ", Style::default().fg(WARN)),
            Span::styled("perm      ", Style::default().fg(MUTED)),
            Span::styled(
                app.settings.permission_mode.as_str(),
                Style::default().fg(ACCENT_HI),
            ),
        ]),
        Line::from(vec![
            Span::styled(" CTX ", Style::default().fg(ACCENT)),
            Span::styled("context   ", Style::default().fg(MUTED)),
            Span::styled(context_meter_label(app), Style::default().fg(ACCENT_HI)),
        ]),
    ];
    let mut sys_info = sys_info;
    sys_info.push(Line::from(vec![
        Span::styled(
            " N ",
            Style::default().fg(if app.nexus_status.active {
                SUCCESS
            } else {
                DIM
            }),
        ),
        Span::styled("nexus     ", Style::default().fg(MUTED)),
        Span::styled(
            nexus_label(&app.nexus_status),
            Style::default().fg(if app.nexus_status.active {
                SUCCESS
            } else {
                MUTED
            }),
        ),
    ]));
    sys_info.push(Line::from(vec![
        Span::styled(" DB ", Style::default().fg(ACCENT)),
        Span::styled("store     ", Style::default().fg(MUTED)),
        Span::styled(
            app.store_path
                .as_ref()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("(memory)"),
            Style::default().fg(ACCENT_HI),
        ),
    ]));

    let sys_p = Paragraph::new(sys_info)
        .style(Style::default().bg(BG_PANEL))
        .block(
            Block::default()
                .title(Span::styled(
                    " System ",
                    Style::default().fg(VIOLET).add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_type(ratatui::widgets::BorderType::Rounded)
                .border_style(Style::default().fg(DIM))
                .style(Style::default().bg(BG_PANEL)),
        );
    frame.render_widget(sys_p, chunks[1]);
}

fn render_centre(frame: &mut Frame, area: Rect, app: &App) {
    let has_active = if let Some(g) = app.current_goal() {
        g.state
            .as_ref()
            .map(|s| !s.active_workers.is_empty())
            .unwrap_or(false)
    } else {
        false
    };

    let border_color = if has_active { ACCENT_HI } else { DIM };

    // Pulsing effect for the "Active" indicator
    let pulse_colors = [SUCCESS, ACCENT_HI, SUCCESS, MUTED];
    let pulse_idx = (app.spinner_frame / 8) % pulse_colors.len();
    let pulse_color = if has_active {
        pulse_colors[pulse_idx]
    } else {
        MUTED
    };

    let mut block = Block::default()
        .title(Line::from(vec![
            Span::styled(" ", Style::default()),
            Span::styled("◉ ", Style::default().fg(pulse_color)),
            Span::styled(
                "Active",
                Style::default().fg(ACCENT_HI).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ", Style::default()),
        ]))
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(BG_PANEL));

    let Some(g) = app.current_goal() else {
        let label = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
        let muted = Style::default().fg(MUTED);
        let example = Style::default().fg(Color::White);
        let mut welcome_spans: Vec<Span<'static>> = vec![Span::styled("  Welcome to ", muted)];
        let title_chars: Vec<char> = "phonton".chars().collect();
        let n = title_chars.len() as f32;
        for (i, ch) in title_chars.into_iter().enumerate() {
            let t = (i as f32) / (n - 1.0).max(1.0);
            welcome_spans.push(Span::styled(
                ch.to_string(),
                Style::default().fg(grad3(t)).add_modifier(Modifier::BOLD),
            ));
        }
        welcome_spans.push(Span::styled(".", muted));
        let mut lines = vec![
            Line::raw(""),
            Line::from(welcome_spans),
            Line::raw(""),
            Line::from(Span::styled(
                "  Type a goal below and press Enter — it will be planned, executed",
                muted,
            )),
            Line::from(Span::styled(
                "  by parallel workers, and verified before any diff lands.",
                muted,
            )),
            Line::raw(""),
            Line::from(Span::styled("  Try one of:", label)),
            Line::from(vec![
                Span::styled(
                    "    ▸ ",
                    Style::default().fg(grad3(0.0)).add_modifier(Modifier::BOLD),
                ),
                Span::styled("Add a Cargo command for running tests in CI", example),
            ]),
            Line::from(vec![
                Span::styled(
                    "    ▸ ",
                    Style::default().fg(grad3(0.5)).add_modifier(Modifier::BOLD),
                ),
                Span::styled("Refactor render_input into smaller helpers", example),
            ]),
            Line::from(vec![
                Span::styled(
                    "    ▸ ",
                    Style::default().fg(grad3(1.0)).add_modifier(Modifier::BOLD),
                ),
                Span::styled("Write integration tests for the orchestrator", example),
            ]),
            Line::raw(""),
            Line::from(Span::styled("  Shortcuts:", label)),
            Line::from(vec![
                Span::styled(
                    "    /run",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled("    sandboxed command", muted),
            ]),
            Line::from(vec![
                Span::styled(
                    "    !",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled("       command shorthand", muted),
            ]),
            Line::from(vec![
                Span::styled(
                    "    Ctrl+V",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled("  paste clipboard", muted),
            ]),
            Line::from(vec![
                Span::styled(
                    "    Ctrl+;",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled("  toggle Ask side panel", muted),
            ]),
            Line::from(vec![
                Span::styled(
                    "    Shift+L",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" open the Flight Log", muted),
            ]),
            Line::from(vec![
                Span::styled(
                    "    Esc",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled("     save + exit?", muted),
            ]),
        ];
        append_command_run_lines(&mut lines, &app.command_runs, app.focused_command_run);
        let p = Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false });
        frame.render_widget(p, area);
        return;
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(vec![Span::styled(
        "goal: ",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )]));
    if let Some(last) = lines.last_mut() {
        last.spans.extend(artifact_text_spans(
            &g.description,
            Style::default().fg(Color::White),
        ));
    }
    lines.push(Line::raw(""));

    if let Some(state) = &g.state {
        for w in &state.active_workers {
            let mut spans = status_tag_spans(&w.status_as_task(), app.spinner_frame);
            spans.push(Span::raw(" "));
            let label = worker_display_description(&w.subtask_description);
            spans.push(Span::raw(short(&label, 50)));
            if w.is_thinking {
                let frame_idx = (app.spinner_frame / 4) % SPINNER.len();
                let frame_ch = SPINNER[frame_idx];
                spans.push(Span::styled(
                    format!("  {} thinking…", frame_ch),
                    Style::default()
                        .fg(Color::Rgb(180, 100, 255))
                        .add_modifier(Modifier::BOLD | Modifier::ITALIC),
                ));
            }
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                format!("({})", w.model_tier),
                Style::default().fg(MUTED),
            ));
            lines.push(Line::from(spans));
        }
        lines.push(Line::raw(""));
        lines.push(render_savings_line_styled(
            Some(state),
            app.best_savings_pct,
            app.new_best_ticks,
        ));

        if let TaskStatus::Failed {
            reason,
            failed_subtask,
        } = &state.task_status
        {
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![
                Span::styled(
                    "Failure: ",
                    Style::default().fg(DANGER).add_modifier(Modifier::BOLD),
                ),
                Span::styled(reason.clone(), Style::default().fg(Color::White)),
            ]));
            if let Some(subtask) = failed_subtask {
                lines.push(Line::from(vec![
                    Span::styled("  subtask ", Style::default().fg(MUTED)),
                    Span::styled(
                        subtask.to_string(),
                        Style::default().fg(ACCENT_HI).add_modifier(Modifier::BOLD),
                    ),
                ]));
            }
        }

        if let TaskStatus::NeedsClarification { questions } = &state.task_status {
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![
                Span::styled(
                    "Needs clarification",
                    Style::default().fg(WARN).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" before workers run", Style::default().fg(MUTED)),
            ]));
            for question in questions.iter().take(5) {
                lines.push(Line::from(vec![
                    Span::styled("  - ", Style::default().fg(WARN)),
                    Span::styled(question.clone(), Style::default().fg(Color::White)),
                ]));
            }
            lines.push(Line::from(vec![
                Span::styled("  answer: ", Style::default().fg(MUTED)),
                Span::styled(
                    "submit a more specific goal, for example `make chess as a terminal game`",
                    Style::default().fg(MUTED),
                ),
            ]));
        }

        append_focus_tabs(&mut lines, app.active_focus_view_for_current_goal());
        match app.active_focus_view_for_current_goal() {
            FocusView::Receipt => {
                if let Some(handoff) = &state.handoff_packet {
                    append_handoff_lines(&mut lines, handoff);
                } else {
                    lines.push(Line::from(Span::styled(
                        "  No handoff packet yet.",
                        Style::default().fg(MUTED),
                    )));
                }
            }
            FocusView::Problems => {
                append_problems_focus_lines(&mut lines, g, app.focused_changed_file)
            }
            FocusView::Code => append_code_focus_lines(&mut lines, g, app.focused_changed_file),
            FocusView::Commands => {
                append_command_run_lines(&mut lines, &app.command_runs, app.focused_command_run)
            }
            FocusView::Log => append_log_focus_lines(&mut lines, g),
        }

        // Checkpoint picker — one line per landed subtask, newest last.
        // Marked with a "↶ N" tag for "Rollback to step N" — the actual
        // rollback is dispatched through the orchestrator's control
        // channel (see `OrchestratorMessage::RollbackRequest`); this
        // panel only surfaces the picker.
        if !state.checkpoints.is_empty()
            && app.active_focus_view_for_current_goal() == FocusView::Receipt
        {
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                format!(
                    "Checkpoints ({} — Ctrl+↑↓ select, 'r' rollback):",
                    state.checkpoints.len()
                ),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )));
            let cursor = g.checkpoint_cursor;
            for (i, cp) in state.checkpoints.iter().enumerate() {
                let oid_short: String = cp.commit_oid.chars().take(8).collect();
                let is_selected = cursor == Some(i);
                let marker_style = if is_selected {
                    Style::default().fg(WARN).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(ACCENT)
                };
                let marker = if is_selected {
                    format!("  ▶ #{:>2}  ", cp.seq)
                } else {
                    format!("  ↶ #{:>2}  ", cp.seq)
                };
                lines.push(Line::from(vec![
                    Span::styled(marker, marker_style),
                    Span::styled(format!("{}  ", oid_short), Style::default().fg(MUTED)),
                    Span::raw(short(&cp.message, 50)),
                ]));
            }
        }
    } else {
        lines.push(Line::from(Span::styled(
            "(waiting for first state snapshot…)",
            Style::default().fg(MUTED),
        )));
    }

    let inner_h = area.height.saturating_sub(2) as usize;
    let max_scroll = lines.len().saturating_sub(inner_h);
    let focus_scroll = app.focus_scroll.min(max_scroll);
    if max_scroll > 0 {
        block = block.title_bottom(Line::from(Span::styled(
            format!(
                " PgUp/PgDn scroll {}/{} · Home/End ",
                focus_scroll, max_scroll
            ),
            Style::default().fg(MUTED),
        )));
    }
    let p = Paragraph::new(lines)
        .wrap(Wrap { trim: true })
        .scroll((focus_scroll.min(u16::MAX as usize) as u16, 0))
        .block(block);
    frame.render_widget(p, area);
}

fn append_handoff_lines(lines: &mut Vec<Line<'static>>, handoff: &HandoffPacket) {
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Result",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(handoff.headline.clone(), Style::default().fg(Color::White)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  files ", Style::default().fg(MUTED)),
        Span::styled(
            handoff.diff_stats.files_changed.to_string(),
            Style::default().fg(ACCENT_HI).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  +", Style::default().fg(MUTED)),
        Span::styled(
            handoff.diff_stats.added_lines.to_string(),
            Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  -", Style::default().fg(MUTED)),
        Span::styled(
            handoff.diff_stats.removed_lines.to_string(),
            Style::default().fg(DANGER).add_modifier(Modifier::BOLD),
        ),
    ]));

    if !handoff.changed_files.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "Changed files",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )));
        for file in handoff.changed_files.iter().take(6) {
            lines.push(Line::from(vec![
                Span::styled("  - ", Style::default().fg(ACCENT_HI)),
                Span::styled(
                    short(&file.path.display().to_string(), 44),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("  +", Style::default().fg(MUTED)),
                Span::styled(file.added_lines.to_string(), Style::default().fg(SUCCESS)),
                Span::styled(" -", Style::default().fg(MUTED)),
                Span::styled(file.removed_lines.to_string(), Style::default().fg(DANGER)),
                Span::styled("  ", Style::default()),
                Span::styled(short(&file.summary, 62), Style::default().fg(MUTED)),
            ]));
        }
        if handoff.changed_files.len() > 6 {
            lines.push(Line::from(Span::styled(
                format!("  +{} more file(s)", handoff.changed_files.len() - 6),
                Style::default().fg(MUTED),
            )));
        }
    }

    if !handoff.verification.passed.is_empty() || !handoff.verification.findings.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "Verification",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )));
        for passed in handoff.verification.passed.iter().take(4) {
            lines.push(Line::from(vec![
                Span::styled(
                    "  pass ",
                    Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
                ),
                Span::styled(short(passed, 86), Style::default().fg(MUTED)),
            ]));
        }
        for finding in handoff.verification.findings.iter().take(3) {
            lines.push(Line::from(vec![
                Span::styled(
                    "  warn ",
                    Style::default().fg(WARN).add_modifier(Modifier::BOLD),
                ),
                Span::styled(short(finding, 86), Style::default().fg(MUTED)),
            ]));
        }
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Run",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )));
    if handoff.run_commands.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No run command inferred yet.",
            Style::default().fg(MUTED),
        )));
    } else {
        for command in handoff.run_commands.iter().take(3) {
            lines.push(Line::from(vec![
                Span::styled(
                    "  $ ",
                    Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
                ),
                Span::styled(command.command.join(" "), Style::default().fg(Color::White)),
            ]));
        }
    }

    if !handoff.known_gaps.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "Known gaps",
            Style::default().fg(WARN).add_modifier(Modifier::BOLD),
        )));
        for gap in handoff.known_gaps.iter().take(4) {
            lines.push(Line::from(vec![
                Span::styled("  - ", Style::default().fg(WARN)),
                Span::styled(short(gap, 90), Style::default().fg(MUTED)),
            ]));
        }
    }
}

/// Render the Flight Log — raw [`EventRecord`] stream for the currently
/// selected goal. Toggleable with Shift+L, dismissable with Esc.
///
/// Long event payloads (e.g. provider error bodies that include URLs and
/// JSON) are wrapped instead of being clipped at the right edge, and the
/// pane scrolls with PgUp/PgDn/Home/End so the user can read the full
/// history. Auto-tails to the newest entry whenever the user hasn't
/// manually scrolled (`flight_log_scroll == None`).
fn render_flight_log(frame: &mut Frame, area: Rect, app: &App) {
    let scroll_hint = if app.flight_log_scroll.is_some() {
        " ↑↓ PgUp/PgDn scroll · End=tail · Shift+L close "
    } else {
        " ↑↓ PgUp/PgDn scroll · Shift+L close "
    };
    let block = Block::default()
        .title(Span::styled(
            " Flight Log ",
            Style::default().fg(VIOLET).add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Line::from(Span::styled(
            scroll_hint,
            Style::default().fg(MUTED),
        )))
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(VIOLET));

    let Some(g) = app.current_goal() else {
        let p = Paragraph::new(Line::from(Span::styled(
            "No goal selected.",
            Style::default().fg(MUTED),
        )))
        .block(block);
        frame.render_widget(p, area);
        return;
    };

    if g.flight_log.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled(
            "(no events yet — the orchestrator hasn't reported any state changes)",
            Style::default().fg(MUTED),
        )))
        .block(block);
        frame.render_widget(p, area);
        return;
    }

    // Build a *wrapped* line list. Each event becomes a header line with
    // the timestamp + tag and then one or more continuation lines for the
    // payload, soft-wrapped to the visible width. Continuation lines are
    // indented under the tag column so the eye can still scan timestamps.
    let inner_w = area.width.saturating_sub(2) as usize;
    let header_w = 10 + 1 + 14 + 1; // ts + space + tag + space
    let payload_w = inner_w.saturating_sub(header_w).max(20);

    let mut lines: Vec<Line> = Vec::new();
    for rec in g.flight_log.iter() {
        let (color, tag) = event_style(rec);
        let payload = rec.render_line();
        let chunks = wrap_text(&payload, payload_w);
        if chunks.is_empty() {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{:>10} ", fmt_ts(rec.timestamp_ms)),
                    Style::default().fg(MUTED),
                ),
                Span::styled(
                    format!("{:<14} ", tag),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
            ]));
            continue;
        }
        for (i, chunk) in chunks.iter().enumerate() {
            if i == 0 {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{:>10} ", fmt_ts(rec.timestamp_ms)),
                        Style::default().fg(MUTED),
                    ),
                    Span::styled(
                        format!("{:<14} ", tag),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(chunk.clone()),
                ]));
            } else {
                // Continuation: pad to the payload column so the wrapped
                // text aligns under the first chunk — much easier to read.
                lines.push(Line::from(vec![
                    Span::raw(" ".repeat(header_w)),
                    Span::raw(chunk.clone()),
                ]));
            }
        }
    }

    let inner_h = area.height.saturating_sub(2) as usize;
    let total = lines.len();
    let max_scroll = total.saturating_sub(inner_h);
    // None == "tail mode": always show the newest content. Some(n) == the
    // user has scrolled, n is the offset from the top.
    let scroll = app.flight_log_scroll.unwrap_or(max_scroll).min(max_scroll);
    let visible: Vec<Line> = lines.into_iter().skip(scroll).take(inner_h).collect();

    let p = Paragraph::new(visible)
        .wrap(Wrap { trim: false })
        .block(block);
    frame.render_widget(p, area);
}

fn render_memory(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .title(Span::styled(
            " Memory ",
            Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Line::from(Span::styled(
            " / commands · Esc back ",
            Style::default().fg(MUTED),
        )))
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(SUCCESS))
        .style(Style::default().bg(BG_PANEL));

    let mut lines = Vec::new();
    if app.memory_records.is_empty() {
        lines.push(Line::from(Span::styled(
            "No memory records yet.",
            Style::default().fg(MUTED),
        )));
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "Workers will add decisions, constraints, rejected approaches, and conventions here as goals complete.",
            Style::default().fg(MUTED),
        )));
    } else {
        for rec in app.memory_records.iter().take(20) {
            let (kind, body) = memory_record_summary(rec);
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{kind:<10} "),
                    Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
                ),
                Span::raw(short(&body, 90)),
            ]));
        }
        if let Some(row) = app.selected_history_record() {
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "Selected",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(vec![
                Span::styled("id ", Style::default().fg(MUTED)),
                Span::styled(row.id.to_string(), Style::default().fg(ACCENT_HI)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("goal ", Style::default().fg(MUTED)),
                Span::styled(
                    short(&row.goal_text, 120),
                    Style::default().fg(Color::White),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled("status ", Style::default().fg(MUTED)),
                Span::styled(
                    task_status_label(&row.status),
                    Style::default().fg(ACCENT_HI),
                ),
                Span::styled("  tokens ", Style::default().fg(MUTED)),
                Span::styled(row.total_tokens.to_string(), Style::default().fg(ACCENT_HI)),
            ]));
            if let Some(ledger) = &row.outcome_ledger {
                if let Some(handoff) = &ledger.handoff {
                    lines.push(Line::from(vec![
                        Span::styled("receipt ", Style::default().fg(MUTED)),
                        Span::styled(short(&handoff.headline, 120), Style::default().fg(SUCCESS)),
                    ]));
                }
            }
        }
    }

    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }).block(block),
        area,
    );
}

fn render_history(frame: &mut Frame, area: Rect, app: &App) {
    let filtered = app.filtered_history_indices();
    let block = Block::default()
        .title(Span::styled(
            format!(" History {}/{} ", filtered.len(), app.history_records.len()),
            Style::default().fg(ACCENT_HI).add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Line::from(Span::styled(
            " / commands · Esc back ",
            Style::default().fg(MUTED),
        )))
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(BG_PANEL));

    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("filter: ", Style::default().fg(MUTED)),
        Span::styled(
            if app.history_filter.is_empty() {
                "<none>".into()
            } else {
                app.history_filter.clone()
            },
            Style::default().fg(ACCENT_HI),
        ),
    ]));
    lines.push(Line::raw(""));
    if app.history_records.is_empty() {
        lines.push(Line::from(Span::styled(
            "No persisted task history yet.",
            Style::default().fg(MUTED),
        )));
    } else if filtered.is_empty() {
        lines.push(Line::from(Span::styled(
            "No history rows match this filter.",
            Style::default().fg(MUTED),
        )));
    } else {
        let visible_count = (area.height.saturating_sub(9) as usize).clamp(6, 20);
        let start = app.history_scroll.min(filtered.len().saturating_sub(1));
        for (visible_idx, row_index) in filtered
            .iter()
            .copied()
            .enumerate()
            .skip(start)
            .take(visible_count)
        {
            let Some(row) = app.history_records.get(row_index) else {
                continue;
            };
            let status = task_status_label(&row.status);
            let outcome = row
                .outcome_ledger
                .as_ref()
                .and_then(|ledger| ledger.handoff.as_ref())
                .map(|handoff| {
                    format!(
                        "{} files +{} -{}  ",
                        handoff.diff_stats.files_changed,
                        handoff.diff_stats.added_lines,
                        handoff.diff_stats.removed_lines
                    )
                })
                .unwrap_or_default();
            let selected = visible_idx == app.history_selected;
            lines.push(Line::from(vec![
                Span::styled(
                    if selected { "> " } else { "  " },
                    Style::default().fg(ACCENT_HI),
                ),
                Span::styled(
                    format!("{status:<9} "),
                    if selected {
                        Style::default()
                            .fg(BG_DEEP)
                            .bg(ACCENT_HI)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(ACCENT_HI).add_modifier(Modifier::BOLD)
                    },
                ),
                Span::styled(
                    format!("{} tok  ", row.total_tokens),
                    Style::default().fg(MUTED),
                ),
                Span::styled(outcome, Style::default().fg(SUCCESS)),
                Span::raw(short(&row.goal_text, 90)),
            ]));
        }
    }

    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }).block(block),
        area,
    );
}

fn task_status_label(status: &serde_json::Value) -> &'static str {
    if let Some(s) = status.as_str() {
        return match s {
            "Queued" => "queued",
            "Planning" => "planning",
            "Rejected" => "rejected",
            _ => "task",
        };
    }
    if status.get("Done").is_some() {
        "done"
    } else if status.get("Reviewing").is_some() {
        "review"
    } else if status.get("Failed").is_some() {
        "failed"
    } else if status.get("NeedsClarification").is_some() {
        "clarify"
    } else if status.get("Running").is_some() {
        "running"
    } else if status.get("Paused").is_some() {
        "paused"
    } else {
        "task"
    }
}

fn goal_status_label(status: &TaskStatus) -> &'static str {
    match status {
        TaskStatus::Queued => "queued",
        TaskStatus::Planning => "planning",
        TaskStatus::Running { .. } => "running",
        TaskStatus::Reviewing { .. } => "review",
        TaskStatus::Done { .. } => "done",
        TaskStatus::Failed { .. } => "failed",
        TaskStatus::NeedsClarification { .. } => "clarify",
        TaskStatus::Paused { .. } => "paused",
        TaskStatus::Rejected => "rejected",
    }
}

fn memory_record_summary(record: &MemoryRecord) -> (&'static str, String) {
    match record {
        MemoryRecord::Decision { title, body, .. } => ("Decision", format!("{title}: {body}")),
        MemoryRecord::Constraint {
            statement,
            rationale,
        } => ("Constraint", format!("{statement}: {rationale}")),
        MemoryRecord::RejectedApproach { summary, reason } => {
            ("Rejected", format!("{summary}: {reason}"))
        }
        MemoryRecord::Convention { rule, scope } => {
            let scope = scope.as_deref().unwrap_or("global");
            ("Convention", format!("{scope}: {rule}"))
        }
    }
}

fn nexus_label(status: &NexusStatus) -> String {
    if !status.message.is_empty() {
        short(&status.message, 22)
    } else if status.active {
        format!("{} repos", status.repo_count)
    } else {
        "single repo".into()
    }
}

fn context_meter_label(app: &App) -> String {
    let latest = app
        .last_prompt_manifest
        .as_ref()
        .map(|m| m.total_estimated_tokens)
        .unwrap_or(0);
    if latest == 0 {
        "none".into()
    } else if app.compacted_prompt_tokens > 0 {
        format!("{} tok, {} compacted", latest, app.compacted_prompt_tokens)
    } else {
        format!("{latest} tok")
    }
}

fn permissions_label(permissions: &[Permission]) -> String {
    if permissions.is_empty() {
        return "none".into();
    }
    permissions
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Soft-wrap `s` into width-`w` chunks. Splits on whitespace where it
/// can; otherwise hard-breaks mid-token so a 400-char URL or JSON blob
/// still appears in full.
fn wrap_text(s: &str, w: usize) -> Vec<String> {
    if w == 0 {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in s.split_inclusive(|c: char| c.is_whitespace() || c == ',' || c == ';') {
        let cur_len = current.chars().count();
        let word_len = word.chars().count();
        if cur_len + word_len > w {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            // If the word itself is wider than the column, hard-break it
            // into width-w pieces. Iterate by char_indices to stay
            // unicode-safe.
            if word_len > w {
                let mut buf = String::new();
                for ch in word.chars() {
                    if buf.chars().count() >= w {
                        out.push(std::mem::take(&mut buf));
                    }
                    buf.push(ch);
                }
                current = buf;
                continue;
            }
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn event_style(rec: &EventRecord) -> (Color, &'static str) {
    use phonton_types::OrchestratorEvent as E;
    match &rec.event {
        E::TaskStarted { .. } => (ACCENT, "task-started"),
        E::TaskCompleted { .. } => (SUCCESS, "task-done"),
        E::TaskFailed { .. } => (DANGER, "task-failed"),
        E::SubtaskDispatched { .. } => (ACCENT, "dispatch"),
        E::ContextSelected { .. } => (VIOLET, "context"),
        E::PromptManifest { .. } => (VIOLET, "prompt"),
        E::ContextCompacted { .. } => (WARN, "compact"),
        E::ExtensionLoaded { .. } => (ACCENT, "ext-loaded"),
        E::ExtensionSkipped { .. } => (WARN, "ext-skipped"),
        E::ExtensionConflict { .. } => (WARN, "ext-conflict"),
        E::SteeringApplied { .. } => (VIOLET, "steering"),
        E::SkillApplied { .. } => (VIOLET, "skill"),
        E::McpServerAvailable { .. } => (ACCENT, "mcp-server"),
        E::McpToolRequested { .. } => (WARN, "mcp-request"),
        E::McpToolApproved { .. } => (SUCCESS, "mcp-approve"),
        E::McpToolDenied { .. } => (DANGER, "mcp-denied"),
        E::McpToolCompleted { .. } => (SUCCESS, "mcp-done"),
        E::SubtaskCompleted { .. } => (SUCCESS, "subtask-done"),
        E::SubtaskReviewReady { .. } => (SUCCESS, "review-ready"),
        E::SubtaskFailed { .. } => (DANGER, "subtask-fail"),
        E::VerifyPass { .. } => (SUCCESS, "verify-pass"),
        E::VerifyFail { .. } => (WARN, "verify-fail"),
        E::VerifyEscalated { .. } => (WARN, "escalate"),
        E::TokenMilestone { .. } => (MUTED, "tokens"),
        E::Thinking { .. } => (VIOLET, "thinking"),
        E::CheckpointCreated { .. } => (SUCCESS, "checkpoint"),
        E::RollbackPerformed { .. } => (WARN, "rollback"),
        E::ReviewDecision { .. } => (ACCENT, "review"),
    }
}

/// Format a unix-epoch millisecond timestamp as `HH:MM:SS` local-ish time.
/// Avoids pulling in `chrono`; good enough for a log viewer.
fn fmt_ts(ms: u64) -> String {
    let secs = ms / 1000;
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    format!("{:02}:{:02}:{:02}", h, m, s)
}

fn render_ask(frame: &mut Frame, area: Rect, app: &App) {
    let mut lines = vec![
        Line::from(Span::styled(
            "Ask mode (Ctrl+; to close, Esc to cancel)",
            Style::default().fg(VIOLET).add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
    ];
    if app.ask_pending {
        let frame_idx = (app.spinner_frame / 4) % SPINNER.len();
        let frame_ch = SPINNER[frame_idx];
        lines.push(Line::from(vec![
            Span::styled(
                format!("{frame_ch} "),
                Style::default().fg(VIOLET).add_modifier(Modifier::BOLD),
            ),
            Span::styled("thinking…", Style::default().fg(MUTED)),
        ]));
    } else if let Some(ans) = &app.ask_answer {
        lines.push(Line::from(Span::styled(
            "A:",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )));
        lines.extend(render_rich_text_lines(ans));
    } else {
        lines.push(Line::from(Span::styled(
            "(no answer yet)",
            Style::default().fg(MUTED),
        )));
    }
    let viewport = area.height.saturating_sub(2) as usize;
    let max_scroll = lines.len().saturating_sub(viewport);
    let scroll = app.ask_scroll.min(max_scroll);
    let p = Paragraph::new(lines)
        .wrap(Wrap { trim: true })
        .block(
            Block::default()
                .title(Span::styled(
                    format!(" Ask  {scroll}/{max_scroll}  PgUp/PgDn "),
                    Style::default().fg(VIOLET).add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_type(ratatui::widgets::BorderType::Rounded)
                .border_style(Style::default().fg(VIOLET)),
        )
        .scroll((scroll as u16, 0));
    frame.render_widget(p, area);
}

fn render_rich_text_lines(text: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut in_code = false;
    for raw in text.lines() {
        let line = raw.to_string();
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") {
            in_code = !in_code;
            lines.push(Line::from(Span::styled(
                line,
                Style::default().fg(ACCENT_HI),
            )));
            continue;
        }
        let style = if in_code {
            Style::default().fg(ACCENT_HI)
        } else if trimmed.starts_with('#') {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else if trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
            || numbered_list_prefix(trimmed)
        {
            Style::default().fg(Color::White)
        } else if status_line_prefix(trimmed) == Some(DANGER) {
            Style::default().fg(DANGER).add_modifier(Modifier::BOLD)
        } else if status_line_prefix(trimmed) == Some(WARN) {
            Style::default().fg(WARN)
        } else if status_line_prefix(trimmed) == Some(SUCCESS) {
            Style::default().fg(SUCCESS)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(Span::styled(line, style)));
    }
    lines
}

fn numbered_list_prefix(line: &str) -> bool {
    let Some((digits, rest)) = line.split_once('.') else {
        return false;
    };
    !digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit()) && rest.starts_with(' ')
}

fn status_line_prefix(line: &str) -> Option<Color> {
    let lower = line.to_ascii_lowercase();
    if lower.starts_with("error") || lower.starts_with("fail") || lower.starts_with("failed") {
        Some(DANGER)
    } else if lower.starts_with("warn") || lower.starts_with("warning") {
        Some(WARN)
    } else if lower.starts_with("pass") || lower.starts_with("success") {
        Some(SUCCESS)
    } else {
        None
    }
}

fn render_input(frame: &mut Frame, area: Rect, app: &App) {
    let (icon, mode_label, buf, cursor) = match app.mode {
        Mode::Goal => (
            "›",
            " GOAL ",
            app.goal_prompt.display_text(),
            app.goal_prompt.cursor(),
        ),
        Mode::Task => (
            "›",
            " TASK ",
            app.goal_prompt.display_text(),
            app.goal_prompt.cursor(),
        ),
        Mode::Ask => (
            "?",
            " ASK ",
            app.ask_prompt.display_text(),
            app.ask_prompt.cursor(),
        ),
        Mode::Settings => (
            "⚙",
            " SETTINGS ",
            app.goal_prompt.display_text(),
            app.goal_prompt.cursor(),
        ),
        Mode::Memory => (
            "M",
            " MEMORY ",
            app.goal_prompt.display_text(),
            app.goal_prompt.cursor(),
        ),
        Mode::History => (
            "H",
            " HISTORY ",
            app.goal_prompt.display_text(),
            app.goal_prompt.cursor(),
        ),
        Mode::CommandPalette => (
            "/",
            " COMMAND ",
            app.goal_prompt.display_text(),
            app.goal_prompt.cursor(),
        ),
    };

    let mode_style = match app.mode {
        Mode::Goal => Style::default()
            .bg(ACCENT)
            .fg(BG_PANEL)
            .add_modifier(Modifier::BOLD),
        Mode::Task => Style::default()
            .bg(WARN)
            .fg(BG_PANEL)
            .add_modifier(Modifier::BOLD),
        Mode::Ask => Style::default()
            .bg(VIOLET)
            .fg(BG_PANEL)
            .add_modifier(Modifier::BOLD),
        Mode::Settings => Style::default()
            .bg(ACCENT)
            .fg(BG_PANEL)
            .add_modifier(Modifier::BOLD),
        Mode::Memory | Mode::History => Style::default()
            .bg(SUCCESS)
            .fg(BG_PANEL)
            .add_modifier(Modifier::BOLD),
        Mode::CommandPalette => Style::default()
            .bg(ACCENT)
            .fg(BG_PANEL)
            .add_modifier(Modifier::BOLD),
    };

    let border_color = match app.mode {
        Mode::Ask => VIOLET,
        Mode::Task => WARN,
        Mode::Memory | Mode::History => SUCCESS,
        _ => ACCENT,
    };

    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(BG_DEEP));
    if let Some(notice) = &app.command_notice {
        block = block.title_bottom(Line::from(Span::styled(
            format!(" {notice} "),
            Style::default().fg(WARN),
        )));
    }

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Split inner row: prompt on the left, right-aligned mode badge on the right.
    let badge_w = mode_label.chars().count() as u16;
    let row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Length(badge_w)])
        .split(inner);

    let prompt_prefix = format!(" {icon} ");
    let prompt_prefix_w = prompt_prefix.chars().count() as u16;

    // Horizontal scroll so the caret is always visible inside the input slot.
    let input_width = row[0].width.saturating_sub(prompt_prefix_w) as usize;
    let total_chars = char_count(buf);
    let cursor_clamped = cursor.min(total_chars);
    let scroll = cursor_clamped.saturating_sub(input_width.saturating_sub(1));
    let visible: String = buf.chars().skip(scroll).take(input_width.max(1)).collect();

    let mut prompt_spans = vec![Span::styled(
        prompt_prefix,
        Style::default()
            .fg(border_color)
            .add_modifier(Modifier::BOLD),
    )];
    prompt_spans.extend(artifact_text_spans(
        &visible,
        Style::default().fg(Color::White),
    ));
    let prompt = Paragraph::new(Line::from(prompt_spans)).style(Style::default().bg(BG_DEEP));
    frame.render_widget(prompt, row[0]);

    let badge = Paragraph::new(Line::from(Span::styled(mode_label, mode_style)))
        .alignment(Alignment::Right)
        .style(Style::default().bg(BG_DEEP));
    frame.render_widget(badge, row[1]);

    // Draw a native terminal cursor instead of a manual in-buffer caret.
    // Native cursors are handled efficiently by the terminal emulator and
    // don't flicker on every frame draw.
    if !matches!(
        app.mode,
        Mode::Settings | Mode::Memory | Mode::History | Mode::CommandPalette
    ) {
        let cx = row[0].x + prompt_prefix_w + (cursor_clamped - scroll) as u16;
        let cy = row[0].y;
        if cx < row[0].x + row[0].width {
            frame.set_cursor_position((cx, cy));
        }
    }
}

/// Styled pill-badge rendering of a [`TaskStatus`]. `spinner_frame` drives
/// the running-state animation; callers increment it once per tick.
fn status_tag_spans(s: &TaskStatus, spinner_frame: usize) -> Vec<Span<'static>> {
    match s {
        TaskStatus::Queued => vec![pill("queued", Color::Rgb(60, 60, 60), ACCENT_HI)],
        TaskStatus::Planning => vec![pill("plan", ACCENT, BG_DEEP)],
        TaskStatus::Running {
            completed, total, ..
        } => {
            let frame_idx = (spinner_frame / 4) % SPINNER.len();
            let ch = SPINNER[frame_idx];
            vec![
                Span::styled(
                    format!("{ch} "),
                    Style::default()
                        .fg(Color::Rgb(255, 170, 0))
                        .add_modifier(Modifier::BOLD),
                ),
                pill(
                    &format!("run {completed}/{total}"),
                    Color::Rgb(255, 150, 0),
                    BG_DEEP,
                ),
            ]
        }
        TaskStatus::Reviewing { .. } => vec![pill("review", Color::Rgb(180, 100, 255), BG_DEEP)],
        TaskStatus::Done { .. } => vec![pill("done", Color::Rgb(0, 200, 100), BG_DEEP)],
        TaskStatus::Failed { .. } => vec![pill("fail", Color::Rgb(255, 50, 50), Color::White)],
        TaskStatus::NeedsClarification { .. } => {
            vec![pill("clarify", Color::Rgb(255, 200, 0), BG_DEEP)]
        }
        TaskStatus::Paused {
            limit,
            observed,
            ceiling,
        } => {
            vec![pill(
                &format!("paused — {limit} {observed}/{ceiling}"),
                Color::Rgb(255, 200, 0),
                BG_DEEP,
            )]
        }
        TaskStatus::Rejected => vec![Span::styled(
            " rej ",
            Style::default()
                .bg(Color::Rgb(100, 0, 0))
                .fg(Color::Rgb(200, 200, 200))
                .add_modifier(Modifier::CROSSED_OUT),
        )],
    }
}

pub(crate) fn short(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    } else {
        s.to_string()
    }
}

const ARTIFACT_CHIP_COLORS: [Color; 8] = [
    Color::Rgb(99, 179, 237),
    Color::Rgb(160, 122, 234),
    Color::Rgb(237, 100, 166),
    Color::Rgb(72, 199, 142),
    Color::Rgb(246, 173, 85),
    Color::Rgb(69, 144, 255),
    Color::Rgb(56, 217, 169),
    Color::Rgb(255, 121, 198),
];

fn artifact_chip_color(chip: &str) -> Color {
    let hash = chip.bytes().fold(0usize, |acc, b| {
        acc.wrapping_mul(33).wrapping_add(b as usize)
    });
    ARTIFACT_CHIP_COLORS[hash % ARTIFACT_CHIP_COLORS.len()]
}

fn artifact_text_spans(text: &str, base_style: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = text;
    while let Some(start) = find_artifact_chip_start(rest) {
        if start > 0 {
            spans.push(Span::styled(rest[..start].to_string(), base_style));
        }
        let tail = &rest[start..];
        let Some(end) = tail.find(']').map(|idx| idx + 1) else {
            spans.push(Span::styled(tail.to_string(), base_style));
            return spans;
        };
        let chip = &tail[..end];
        spans.push(Span::styled(
            chip.to_string(),
            Style::default()
                .fg(BG_DEEP)
                .bg(artifact_chip_color(chip))
                .add_modifier(Modifier::BOLD),
        ));
        rest = &tail[end..];
    }
    if !rest.is_empty() {
        spans.push(Span::styled(rest.to_string(), base_style));
    }
    spans
}

fn find_artifact_chip_start(text: &str) -> Option<usize> {
    match (text.find("[paste:"), text.find("[image:")) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn worker_display_description(description: &str) -> String {
    let trimmed = description.trim();
    if trimmed.starts_with("# Prior context") {
        return trimmed
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or(trimmed)
            .trim()
            .to_string();
    }
    trimmed.lines().next().unwrap_or(trimmed).trim().to_string()
}

/// Helper for rendering a `SubtaskStatus` using the same taxonomy as a
/// `TaskStatus` — lets the centre pane reuse [`status_tag`] on workers.
trait SubtaskStatusExt {
    fn status_as_task(&self) -> TaskStatus;
}
impl SubtaskStatusExt for phonton_types::WorkerState {
    fn status_as_task(&self) -> TaskStatus {
        match &self.status {
            SubtaskStatus::Queued => TaskStatus::Queued,
            SubtaskStatus::Ready => TaskStatus::Queued,
            SubtaskStatus::Dispatched | SubtaskStatus::Running { .. } => TaskStatus::Running {
                active_subtasks: vec![self.subtask_id],
                completed: 0,
                total: 1,
            },
            SubtaskStatus::Done { .. } => TaskStatus::Done {
                tokens_used: self.tokens_used,
                wall_time_ms: 0,
            },
            SubtaskStatus::Failed { reason, .. } => TaskStatus::Failed {
                reason: reason.clone(),
                failed_subtask: Some(self.subtask_id),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Stub dispatcher — the CLI drives the orchestrator without a real provider
// ---------------------------------------------------------------------------

/// Trivial dispatcher: every subtask "succeeds" with a one-line Rust diff.
/// A real install wires the worker crate in instead; this keeps the CLI
/// runnable out of the box without an API key on disk.
///
/// Holds an [`Arc<Sandbox>`] even though the stub diff path never executes
/// commands — the field is present so the real `WorkerDispatcher` that
/// replaces this struct slots in without changing the CLI construction
/// site. The sandbox is scoped to the orchestrator's working directory.
pub struct StubDispatcher {
    #[allow(dead_code)]
    sandbox: Arc<Sandbox>,
}

impl StubDispatcher {
    pub fn new(sandbox: Arc<Sandbox>) -> Self {
        Self { sandbox }
    }
}

#[async_trait]
impl WorkerDispatcher for StubDispatcher {
    async fn dispatch(
        &self,
        subtask: Subtask,
        _prior_errors: Vec<String>,
        _attempt: u8,
        _msg_tx: Option<tokio::sync::mpsc::Sender<OrchestratorMessage>>,
    ) -> Result<SubtaskResult> {
        let hunks = vec![DiffHunk {
            file_path: format!("phonton-types/src/stub_{}.rs", subtask.id).into(),
            old_start: 1,
            old_count: 0,
            new_start: 1,
            new_count: 1,
            lines: vec![DiffLine::Added("fn stub() -> u32 { 0 }".into())],
        }];
        Ok(SubtaskResult {
            id: subtask.id,
            status: SubtaskStatus::Done {
                tokens_used: 120,
                diff_hunk_count: hunks.len(),
            },
            diff_hunks: hunks,
            model_tier: subtask.model_tier,
            verify_result: VerifyResult::Pass {
                layer: VerifyLayer::Syntax,
            },
            provider: phonton_types::ProviderKind::Anthropic,
            model_name: String::new(),
            token_usage: TokenUsage::estimated(120),
        })
    }
}

// ---------------------------------------------------------------------------
// Event loop
// ---------------------------------------------------------------------------

/// Incoming event for the event loop — either a user key or an async
/// state update the driver received on one of the watch channels.
enum LoopEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Paste(String),
    StateUpdate(usize, Box<GlobalState>),
    AskAnswer(String),
    McpApprovalRequested {
        prompt: PendingMcpApproval,
        reply_tx: oneshot::Sender<McpApprovalDecision>,
    },
    /// One-shot result of a Settings-panel "Test connection" round-trip.
    /// Carries the formatted ✓/✗ message that lands in
    /// `SettingsState::message`.
    TestResult(String),
    /// One-shot result of a Settings-panel "Detect models" round-trip.
    /// `Ok((picked_model, summary))` rewrites the Model field and
    /// reports; `Err(msg)` reports failure only.
    DetectResult(Result<(String, String), String>),
    /// Background model-list fetch completed for the picker overlay.
    /// Carries the full list on success or an error string.
    ModelsLoaded(Result<Vec<String>, String>),
    /// A prompt-bar command finished running through the sandbox.
    CommandFinished(CommandRunSummary),
    FlightEvent(usize, EventRecord),
    Tick,
}

/// Per-goal control channel sender, stored so the event loop can dispatch
/// rollback requests to the right orchestrator instance.
struct GoalControl {
    /// Sender end of the orchestrator's control channel.
    control_tx: mpsc::Sender<OrchestratorMessage>,
}

/// Shared registry mapping goal index → control handle.
type ControlRegistry = Arc<std::sync::Mutex<HashMap<usize, GoalControl>>>;

/// Approval bridge from the MCP runtime into the TUI event loop.
#[derive(Clone)]
struct TuiMcpApprover {
    goal_index: usize,
    tx: mpsc::Sender<LoopEvent>,
}

impl TuiMcpApprover {
    fn new(goal_index: usize, tx: mpsc::Sender<LoopEvent>) -> Self {
        Self { goal_index, tx }
    }
}

#[async_trait]
impl McpApprover for TuiMcpApprover {
    async fn approve(&self, request: McpApprovalRequest) -> McpApprovalDecision {
        let id = NEXT_MCP_APPROVAL_ID.fetch_add(1, Ordering::Relaxed);
        let prompt = PendingMcpApproval::from_request(id, self.goal_index, request);
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(LoopEvent::McpApprovalRequested { prompt, reply_tx })
            .await
            .is_err()
        {
            return McpApprovalDecision::Denied;
        }
        reply_rx.await.unwrap_or(McpApprovalDecision::Denied)
    }
}

fn deny_pending_mcp_approvals(approvals: &mut HashMap<u64, oneshot::Sender<McpApprovalDecision>>) {
    for (_, reply_tx) in approvals.drain() {
        let _ = reply_tx.send(McpApprovalDecision::Denied);
    }
}

const SEMANTIC_INDEX_TIMEOUT_SECS: u64 = 120;

fn default_store_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".phonton").join("store.sqlite3"))
}

fn open_persistent_store() -> Result<Store> {
    let path = default_store_path()
        .ok_or_else(|| anyhow::anyhow!("could not determine ~/.phonton path"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Store::open(path)
}

fn detect_nexus_status(root: &std::path::Path) -> NexusStatus {
    match phonton_index::discover_nexus_config(root) {
        Ok(Some(cfg)) => NexusStatus {
            active: true,
            repo_count: cfg.repos.len(),
            message: format!("{} repos", cfg.repos.len()),
        },
        Ok(None) => NexusStatus {
            active: false,
            repo_count: 0,
            message: "single repo".into(),
        },
        Err(e) => NexusStatus {
            active: false,
            repo_count: 0,
            message: format!("nexus error: {e}"),
        },
    }
}

async fn build_semantic_context(
    root: &std::path::Path,
) -> Option<Arc<phonton_worker::SemanticContext>> {
    let root = root.to_path_buf();
    let build = async move {
        let embedder = phonton_index::Embedder::new()?;
        let index = match phonton_index::discover_nexus_config(&root) {
            Ok(Some(cfg)) => {
                phonton_index::index_workspace_with_nexus_using_embedder(&root, &cfg, &embedder)
                    .await
            }
            Ok(None) => phonton_index::index_workspace_using_embedder(&root, &embedder).await,
            Err(e) => Err(e),
        }?;
        anyhow::Ok(Arc::new(phonton_worker::SemanticContext {
            embedder,
            index,
        }))
    };

    match tokio::time::timeout(Duration::from_secs(SEMANTIC_INDEX_TIMEOUT_SECS), build).await {
        Ok(Ok(ctx)) => Some(ctx),
        Ok(Err(e)) => {
            tracing::warn!("semantic index unavailable: {e}");
            None
        }
        Err(_) => {
            tracing::warn!(
                "semantic index timed out after {SEMANTIC_INDEX_TIMEOUT_SECS}s; continuing without indexed context"
            );
            None
        }
    }
}

/// Load a provider for ask-mode (stateless Q&A) using the config file or
/// env vars. Returns `None` when no key is available.
fn load_ask_provider(cfg: &config::Config) -> Option<Arc<dyn Provider>> {
    let api_key = provider_key_for_run(&cfg.provider)?;
    let model = cfg
        .provider
        .model
        .clone()
        .unwrap_or_else(|| default_model_for(&cfg.provider.name));
    let provider_cfg = make_api_provider_config(
        &cfg.provider.name,
        api_key,
        model,
        cfg.provider.account_id.clone(),
        cfg.provider.base_url.clone(),
    )?;
    Some(Arc::from(provider_for(provider_cfg)))
}

fn provider_requires_key(name: &str) -> bool {
    !matches!(name, "ollama" | "custom" | "openai-compatible")
}

fn cloudflare_base_url(
    account_id: Option<String>,
    base_url_or_account: Option<String>,
) -> Option<String> {
    let raw = account_id
        .or(base_url_or_account)
        .or_else(|| std::env::var("CLOUDFLARE_ACCOUNT_ID").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;
    if raw.starts_with("http://") || raw.starts_with("https://") {
        Some(raw)
    } else {
        Some(format!(
            "https://api.cloudflare.com/client/v4/accounts/{raw}/ai/v1"
        ))
    }
}

fn provider_probe_base_url(
    provider: &str,
    account_id: Option<String>,
    base_url: Option<String>,
) -> Option<String> {
    if provider == "cloudflare" {
        cloudflare_base_url(account_id, base_url)
    } else {
        base_url
    }
}

fn provider_key_for_run(cfg: &config::ProviderConfig) -> Option<String> {
    config::resolve_api_key(cfg).or_else(|| {
        if provider_requires_key(&cfg.name) {
            None
        } else {
            Some(String::new())
        }
    })
}

/// Build an [`ApiProviderConfig`] from the provider name, resolved key,
/// model, and optional base URL. Returns `None` for unknown provider names.
///
/// When `base_url` is set for `openai` / `openrouter`, the request is
/// routed through the OpenAI-compatible adaptor instead of the hard-coded
/// endpoint — this is what makes self-hosted proxies (LiteLLM, vLLM,
/// LM Studio) actually receive traffic.
fn make_api_provider_config(
    name: &str,
    api_key: String,
    model: String,
    account_id: Option<String>,
    base_url: Option<String>,
) -> Option<ApiProviderConfig> {
    // Empty-string base URLs come from the Settings panel when the user
    // hasn't typed anything — treat them as "unset".
    let base_url = base_url.filter(|s| !s.trim().is_empty());
    match name {
        "anthropic" => Some(ApiProviderConfig::Anthropic { api_key, model }),
        "openai" => match &base_url {
            Some(url) => Some(ApiProviderConfig::OpenAiCompatible {
                name: "openai".into(),
                api_key,
                model,
                base_url: url.clone(),
            }),
            None => Some(ApiProviderConfig::OpenAI { api_key, model }),
        },
        "openrouter" => match &base_url {
            Some(url) => Some(ApiProviderConfig::OpenAiCompatible {
                name: "openrouter".into(),
                api_key,
                model,
                base_url: url.clone(),
            }),
            None => Some(ApiProviderConfig::OpenRouter { api_key, model }),
        },
        "gemini" => Some(ApiProviderConfig::Gemini { api_key, model }),
        "agentrouter" => Some(ApiProviderConfig::AgentRouter { api_key, model }),
        "cloudflare" => cloudflare_base_url(account_id, base_url).map(|url| {
            ApiProviderConfig::OpenAiCompatible {
                name: "cloudflare".into(),
                api_key,
                model,
                base_url: url,
            }
        }),
        "ollama" => Some(ApiProviderConfig::Ollama {
            base_url: base_url.unwrap_or_else(|| "http://localhost:11434".into()),
            model,
        }),
        // Friendly aliases for common OpenAI-compatible endpoints. Users
        // who pick these don't need to type a base URL.
        "deepseek" => Some(ApiProviderConfig::OpenAiCompatible {
            name: "deepseek".into(),
            api_key,
            model,
            base_url: base_url.unwrap_or_else(|| "https://api.deepseek.com/v1".into()),
        }),
        "xai" | "grok" => Some(ApiProviderConfig::OpenAiCompatible {
            name: "xai".into(),
            api_key,
            model,
            base_url: base_url.unwrap_or_else(|| "https://api.x.ai/v1".into()),
        }),
        "groq" => Some(ApiProviderConfig::OpenAiCompatible {
            name: "groq".into(),
            api_key,
            model,
            base_url: base_url.unwrap_or_else(|| "https://api.groq.com/openai/v1".into()),
        }),
        "together" => Some(ApiProviderConfig::OpenAiCompatible {
            name: "together".into(),
            api_key,
            model,
            base_url: base_url.unwrap_or_else(|| "https://api.together.xyz/v1".into()),
        }),
        // Fully custom: caller must supply `base_url`. Without one the
        // request would have nowhere to go, so return None.
        "custom" | "openai-compatible" => base_url.map(|url| ApiProviderConfig::OpenAiCompatible {
            name: "custom".into(),
            api_key,
            model,
            base_url: url,
        }),
        _ => None,
    }
}

fn provider_config_failure_message(name: &str) -> String {
    match name {
        "cloudflare" => concat!(
            "Cloudflare requires an Account ID or full Workers AI base URL. ",
            "Set Account ID in Settings or set CLOUDFLARE_ACCOUNT_ID."
        )
        .into(),
        "custom" | "openai-compatible" => {
            "OpenAI-compatible providers require a Base URL in Settings.".into()
        }
        other => format!("Unknown provider `{other}`."),
    }
}

fn provider_config_with_model(template: &ApiProviderConfig, model: String) -> ApiProviderConfig {
    match template {
        ApiProviderConfig::Anthropic { api_key, .. } => ApiProviderConfig::Anthropic {
            api_key: api_key.clone(),
            model,
        },
        ApiProviderConfig::OpenAI { api_key, .. } => ApiProviderConfig::OpenAI {
            api_key: api_key.clone(),
            model,
        },
        ApiProviderConfig::OpenRouter { api_key, .. } => ApiProviderConfig::OpenRouter {
            api_key: api_key.clone(),
            model,
        },
        ApiProviderConfig::Gemini { api_key, .. } => ApiProviderConfig::Gemini {
            api_key: api_key.clone(),
            model,
        },
        ApiProviderConfig::Ollama { base_url, .. } => ApiProviderConfig::Ollama {
            base_url: base_url.clone(),
            model,
        },
        ApiProviderConfig::AgentRouter { api_key, .. } => ApiProviderConfig::AgentRouter {
            api_key: api_key.clone(),
            model,
        },
        ApiProviderConfig::OpenAiCompatible {
            name,
            api_key,
            base_url,
            ..
        } => ApiProviderConfig::OpenAiCompatible {
            name: name.clone(),
            api_key: api_key.clone(),
            model,
            base_url: base_url.clone(),
        },
    }
}

/// Smoke-test a provider configuration end-to-end.
///
/// Builds the provider via `make_api_provider_config_with_url`, issues a
/// single tiny chat request, and returns either the model's reply (for
/// the success message) or a string description of the failure. Lives
/// here rather than in `phonton-providers` because the resolution of
/// "what backend the user picked from a string" is CLI-specific.
async fn test_provider(
    name: String,
    api_key: String,
    model: String,
    account_id: Option<String>,
    base_url: Option<String>,
) -> Result<String, String> {
    if api_key.trim().is_empty() && provider_requires_key(&name) {
        return Err("no API key — paste one in the API Key field or set the env var".into());
    }
    let cfg = make_api_provider_config(&name, api_key, model, account_id, base_url)
        .ok_or_else(|| provider_config_failure_message(&name))?;
    let provider: Arc<dyn Provider> = Arc::from(provider_for(cfg));
    let resp = provider
        .call(
            "You are a terse assistant. Respond only with JSON.",
            "Return exactly {\"ok\":true} as JSON.",
            &[],
        )
        .await
        .map_err(|e| format!("{e}"))?;
    Ok(resp.content)
}

fn render_settings(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .title(Line::from(vec![
            Span::styled(" ", Style::default()),
            Span::styled("⚙ ", Style::default().fg(VIOLET)),
            Span::styled(
                "Settings",
                Style::default().fg(ACCENT_HI).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ", Style::default()),
        ]))
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Thick)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(BG_DEEP));

    let popup_w = 72u16;
    let popup_h = 27u16;
    let popup_area = Rect {
        x: area.x + (area.width.saturating_sub(popup_w)) / 2,
        y: area.y + (area.height.saturating_sub(popup_h)) / 2,
        width: popup_w.min(area.width),
        height: popup_h.min(area.height),
    };

    frame.render_widget(Clear, popup_area);
    frame.render_widget(block, popup_area);

    let inner = popup_area.inner(ratatui::layout::Margin {
        vertical: 2,
        horizontal: 2,
    });
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Provider
            Constraint::Length(3), // Model
            Constraint::Length(3), // API Key
            Constraint::Length(3), // Account ID
            Constraint::Length(3), // Base URL
            Constraint::Length(3), // Max Tokens
            Constraint::Length(3), // Max USD Cents
            Constraint::Min(1),    // Message
            Constraint::Length(2), // Instructions
        ])
        .split(inner);

    let field_style = |f: SettingsField| {
        if app.settings.active_field == f {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(MUTED)
        }
    };

    // Provider row — left/right arrow cycles
    let provider_text = if app.settings.active_field == SettingsField::Provider {
        format!("◀ {} ▶", app.settings.provider)
    } else {
        app.settings.provider.clone()
    };
    let provider_p = Paragraph::new(provider_text).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Provider (← → to cycle) ")
            .border_style(field_style(SettingsField::Provider)),
    );
    frame.render_widget(provider_p, chunks[0]);

    // Model row — show validation badge + hint for the picker
    let model_status = match app.settings.model_ok {
        Some(true) => Span::styled(" ✓", Style::default().fg(SUCCESS)),
        Some(false) => Span::styled(" ✗", Style::default().fg(DANGER)),
        None => Span::raw(""),
    };
    let model_title = if app.settings.active_field == SettingsField::Model {
        " Model (Enter = pick list, Ctrl+T = test) "
    } else {
        " Model "
    };
    let model_line = Line::from(vec![Span::raw(app.settings.model.as_str()), model_status]);
    let model_p = Paragraph::new(model_line).block(
        Block::default()
            .borders(Borders::ALL)
            .title(model_title)
            .border_style(field_style(SettingsField::Model)),
    );
    frame.render_widget(model_p, chunks[1]);

    // API key row — masked
    let masked_key = if app.settings.api_key.is_empty() {
        String::new()
    } else {
        "*".repeat(app.settings.api_key.len())
    };
    let key_p = Paragraph::new(masked_key).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" API Key (leave empty for env var) ")
            .border_style(field_style(SettingsField::ApiKey)),
    );
    frame.render_widget(key_p, chunks[2]);

    let account_p = Paragraph::new(app.settings.account_id.as_str()).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Account ID (Cloudflare) ")
            .border_style(field_style(SettingsField::AccountId)),
    );
    frame.render_widget(account_p, chunks[3]);

    let url_p = Paragraph::new(app.settings.base_url.as_str()).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Base URL override ")
            .border_style(field_style(SettingsField::BaseUrl)),
    );
    frame.render_widget(url_p, chunks[4]);

    let tokens_p = Paragraph::new(app.settings.max_tokens.as_str()).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Max Tokens / Session ")
            .border_style(field_style(SettingsField::MaxTokens)),
    );
    frame.render_widget(tokens_p, chunks[5]);

    let cents_p = Paragraph::new(app.settings.max_usd_cents.as_str()).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Max Cents / Session ")
            .border_style(field_style(SettingsField::MaxUsdCents)),
    );
    frame.render_widget(cents_p, chunks[6]);

    if let Some(msg) = &app.settings.message {
        let colour = if msg.starts_with('✗') {
            DANGER
        } else {
            SUCCESS
        };
        let msg_p = Paragraph::new(msg.as_str())
            .style(Style::default().fg(colour))
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true });
        frame.render_widget(msg_p, chunks[7]);
    }

    let instructions = Paragraph::new(Line::from(vec![
        Span::styled("Tab", Style::default().fg(ACCENT)),
        Span::raw(" nav  "),
        Span::styled("Enter", Style::default().fg(ACCENT)),
        Span::raw(" save  "),
        Span::styled("Ctrl+T", Style::default().fg(ACCENT)),
        Span::raw(" test  "),
        Span::styled("Ctrl+D", Style::default().fg(ACCENT)),
        Span::raw(" detect  "),
        Span::styled("Esc", Style::default().fg(ACCENT)),
        Span::raw(" close"),
    ]))
    .alignment(Alignment::Center);
    frame.render_widget(instructions, chunks[8]);

    // --- Model picker overlay ---
    if app.settings.picker_open {
        render_model_picker(frame, popup_area, app);
    }
}

/// Renders the model-picker list as an overlay anchored below the Model
/// field inside the settings popup.
fn render_model_picker(frame: &mut Frame, settings_area: Rect, app: &App) {
    // Position: same x as settings popup, just below the Model field
    // (which sits at y+5 inside the popup). Height = 12 rows.
    const VISIBLE: usize = 8;
    let picker_w = settings_area.width;
    let picker_h = (VISIBLE as u16) + 4; // list rows + borders + filter + count
    let picker_y = (settings_area.y + 7).min(
        settings_area
            .y
            .saturating_add(settings_area.height)
            .saturating_sub(picker_h),
    );
    let picker_area = Rect {
        x: settings_area.x,
        y: picker_y,
        width: picker_w,
        height: picker_h.min(settings_area.height),
    };

    frame.render_widget(Clear, picker_area);

    let picker = &app.settings.picker;

    // Title: loading spinner or count
    let title = if picker.loading {
        let spinner = SPINNER[app.spinner_frame % SPINNER.len()];
        format!(" {spinner} Fetching models… ")
    } else {
        let n = picker.filtered.len();
        let total = picker.all_models.len();
        if picker.filter.is_empty() {
            format!(" {n} models — type to filter ")
        } else {
            format!(" {n}/{total} — filter: {} ", picker.filter)
        }
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .title(title.as_str())
        .border_style(Style::default().fg(VIOLET))
        .style(Style::default().bg(BG_DEEP));

    frame.render_widget(block, picker_area);

    let inner = picker_area.inner(ratatui::layout::Margin {
        vertical: 1,
        horizontal: 1,
    });

    if picker.loading {
        let p = Paragraph::new("Fetching…")
            .style(Style::default().fg(MUTED))
            .alignment(Alignment::Center);
        frame.render_widget(p, inner);
        return;
    }

    if picker.filtered.is_empty() {
        let msg = if picker.all_models.is_empty() {
            "No models found"
        } else {
            "No matches"
        };
        let p = Paragraph::new(msg)
            .style(Style::default().fg(MUTED))
            .alignment(Alignment::Center);
        frame.render_widget(p, inner);
        return;
    }

    let scroll = picker.scroll;
    let selected = picker.selected;
    let cur_model = &app.settings.model;

    let visible_models: Vec<ListItem> = picker
        .filtered
        .iter()
        .enumerate()
        .skip(scroll)
        .take(VISIBLE)
        .map(|(i, m)| {
            let is_selected = i == selected;
            let is_current = m == cur_model;
            let prefix = if is_current { "● " } else { "  " };
            let label = format!("{prefix}{m}");
            let style = if is_selected {
                Style::default()
                    .fg(BG_DEEP)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else if is_current {
                Style::default().fg(SUCCESS)
            } else {
                Style::default().fg(Color::White)
            };
            ListItem::new(label).style(style)
        })
        .collect();

    // Scroll indicator on the right edge
    let total = picker.filtered.len();
    let scroll_info = if total > VISIBLE {
        format!("↑↓ {}/{}", selected + 1, total)
    } else {
        String::new()
    };
    if !scroll_info.is_empty() {
        let info_p = Paragraph::new(scroll_info.as_str())
            .style(Style::default().fg(DIM))
            .alignment(Alignment::Right);
        // render in the last line of inner
        let info_area = Rect {
            x: inner.x,
            y: inner.y + inner.height.saturating_sub(1),
            width: inner.width,
            height: 1,
        };
        frame.render_widget(info_p, info_area);
    }

    let list_area = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: inner.height.saturating_sub(1),
    };

    let list = List::new(visible_models);
    frame.render_widget(list, list_area);
}

/// Default model per provider when none is specified in config.
///
/// These are the cheapest reasonable picks per backend so a user with no
/// model preference still gets sensible behaviour. Override in Settings.
fn default_model_for(provider: &str) -> String {
    match provider {
        "anthropic" => "claude-haiku-4-5-20251001".into(),
        "openai" => "gpt-4o-mini".into(),
        "openrouter" => "openai/gpt-4o-mini".into(),
        // `gemini-flash-latest` is an always-current alias that points at
        // whichever flash model is generally available on free-tier keys.
        // `gemini-2.5-flash` exists on most keys but the alias avoids
        // surprises when Google rotates the GA model. The Gemini provider
        // also auto-routes to a working model on 404 (see
        // `phonton-providers::GeminiProvider`).
        "gemini" => "gemini-flash-latest".into(),
        "agentrouter" => "claude-sonnet-4-5".into(),
        "cloudflare" => "@cf/moonshotai/kimi-k2.6".into(),
        "ollama" => "llama3.2:3b".into(),
        "deepseek" => "deepseek-chat".into(),
        "xai" | "grok" => "grok-2-mini".into(),
        "groq" => "llama-3.3-70b-versatile".into(),
        "together" => "meta-llama/Llama-3.3-70B-Instruct-Turbo".into(),
        _ => "unknown".into(),
    }
}

/// Print the `phonton --help` text. Plain stdout — runs before the TUI
/// touches the terminal, so it composes with shell pipes / `less`.
fn print_help() {
    println!(
        "phonton — agentic dev environment\n\
         \n\
         USAGE:\n  \
         phonton\n  \
         phonton -r|--resume\n  \
         phonton <SUBCOMMAND>\n\
         \n\
         SUBCOMMANDS:\n  \
         (none)            Launch the interactive TUI (default)\n  \
         init              Create ~/.phonton/config.toml if it is missing\n  \
         ask <question>    One-shot Q&A using the configured provider\n  \
         demo trust-loop   Print the evidence-trail demo loop\n  \
         doctor            Check config, store, trust, git, cargo, and Nexus\n  \
         extensions        Inspect loaded steering, skills, MCP, and profiles\n  \
         skills            Inspect loaded skills\n  \
         steering          Inspect loaded steering rules\n  \
         mcp               List configured MCP servers and explicitly call tools\n  \
         plan <goal>       Preview the task DAG without changing files\n  \
         review [task-id]  Show verified diff review payloads\n  \
         run [task-id]     Run a receipt-suggested command from latest task\n  \
         memory            List, edit, delete, and pin persistent memory\n  \
         config path       Print the resolved config file path\n  \
         config edit       Open the config in $EDITOR (or notepad on Windows)\n  \
         config show       Dump the resolved config as TOML\n  \
         version           Print version and exit\n  \
         help              Print this help and exit\n\
         \n\
         FLAGS:\n  \
         -r, --resume     Resume the saved session for this workspace\n  \
         -h, --help        Same as `help`\n  \
         -V, --version     Same as `version`\n\
         \n\
         CONFIG:\n  \
         Settings live in ~/.phonton/config.toml. Override the provider key with\n  \
         ANTHROPIC_API_KEY, OPENAI_API_KEY, TOGETHER_API_KEY, etc.\n\
         \n\
         TUI SLASH COMMANDS:\n  \
         /settings, /config, /status, /context, /compact, /compress,\n  \
         /problems, /diagnostics, /retry, /repair, /why-tokens, /ask <question>,\n  \
         /goals, /switch, /focus <view>, /copy, /rerun, /stats, /stop,\n  \
         /review, /memory, /permissions set <mode>, /trust, /model set <name>,\n  \
         /commands, /run <cmd>, and !<cmd>\n\
         \n\
         DEMO:\n  \
         phonton demo trust-loop [--json]\n\
         \n\
         DOCTOR:\n  \
         phonton doctor [--json] [--provider]\n\
         \n\
         PLAN PREVIEW:\n  \
         phonton plan [--json] [--no-memory] [--no-tests] <goal>\n\
         \n\
         REVIEW:\n  \
         phonton review [--json|--markdown] [latest|<task-id>]\n  \
         phonton review approve [--json] [latest|<task-id>]\n  \
         phonton review reject [--json] [latest|<task-id>]\n  \
         phonton review rollback [--json] [latest|<task-id>] <seq>\n\
         \n\
         RUN:\n  \
         phonton run [--json] [--index <n>] [latest|<task-id>]\n\
         \n\
         MCP:\n  \
         phonton mcp list [--json]\n  \
         phonton mcp tools <server-id> [--json] [--yes]\n  \
         phonton mcp call <server-id> <tool-name> [json-args] [--json] [--yes]\n\
         \n\
         EXTENSIONS:\n  \
         phonton extensions list [--json]\n  \
         phonton extensions doctor [--json]\n  \
         phonton skills list [--json]\n  \
         phonton steering list [--json]\n\
         \n\
         MEMORY:\n  \
         phonton memory list [--json] [--kind <kind>] [--topic <text>] [--limit <n>]\n  \
         phonton memory edit <id> <text>\n  \
         phonton memory delete <id>\n  \
         phonton memory pin <id>\n  \
         phonton memory unpin <id>\n"
    );
}

fn print_version() {
    println!("phonton {}", env!("CARGO_PKG_VERSION"));
}

fn run_init() -> Result<()> {
    let path =
        config::config_path().ok_or_else(|| anyhow::anyhow!("could not resolve config path"))?;
    if path.exists() {
        println!("Phonton already initialized at {}", path.display());
    } else {
        config::save(&config::Config::default())?;
        println!("Created {}", path.display());
    }
    println!("Next: phonton doctor");
    println!("Then: phonton plan \"add validation to config loading\"");
    Ok(())
}

pub fn render_trust_demo() -> String {
    demo::render_trust_demo()
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn workspace_session_key(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

/// Render the plain terminal receipt printed after a confirmed TUI exit.
pub fn render_exit_receipt(totals: &SessionTotals) -> String {
    let saved_line = if totals.estimated_tokens_saved >= 0 {
        format!(
            "estimated saved vs naive: {}",
            totals.estimated_tokens_saved
        )
    } else {
        format!(
            "estimated over naive: {}",
            totals.estimated_tokens_saved.saturating_abs()
        )
    };
    let best = totals
        .best_savings_pct
        .map(|p| format!("{p}%"))
        .unwrap_or_else(|| "n/a".into());
    format!(
        "Session saved\n\
         goals: {}  completed: {}  reviewing: {}  failed: {}\n\
         tokens used: {}\n\
         naive baseline: {}\n\
         {}\n\
         best savings: {}",
        totals.goals,
        totals.completed,
        totals.reviewing,
        totals.failed,
        totals.tokens_used,
        totals.naive_baseline_tokens,
        saved_line,
        best
    )
}

/// Parse arguments that launch the interactive TUI instead of a subcommand.
pub fn launch_options_from_args(args: &[String]) -> Option<LaunchOptions> {
    match args {
        [] => Some(LaunchOptions::default()),
        [flag] if flag == "-r" || flag == "--resume" => Some(LaunchOptions {
            resume_last_session: true,
        }),
        _ => None,
    }
}

/// Handle CLI subcommands that exit before the TUI launches.
/// Returns `Ok(Some(_))` when the TUI should launch, or `Ok(None)` when a
/// subcommand was handled and the caller should exit.
async fn handle_cli_args() -> Result<Option<LaunchOptions>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(options) = launch_options_from_args(&args) {
        return Ok(Some(options));
    }
    match args[0].as_str() {
        "-h" | "--help" | "help" => {
            print_help();
            Ok(None)
        }
        "-V" | "--version" | "version" => {
            print_version();
            Ok(None)
        }
        "init" => {
            run_init()?;
            Ok(None)
        }
        "demo" => {
            let code = demo::run(&args[1..]).await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(None)
        }
        "config" => {
            let sub = args.get(1).map(|s| s.as_str()).unwrap_or("path");
            match sub {
                "path" => match config::config_path() {
                    Some(p) => println!("{}", p.display()),
                    None => {
                        eprintln!("phonton: could not resolve config path (HOME unset?)");
                        std::process::exit(1);
                    }
                },
                "edit" => {
                    let path = config::config_path()
                        .ok_or_else(|| anyhow::anyhow!("could not resolve config path"))?;
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent).ok();
                    }
                    if !path.exists() {
                        // Seed with current resolved config so the editor opens
                        // a non-empty buffer with the keys the user can tweak.
                        let cfg = config::load().unwrap_or_default();
                        let _ = config::save(&cfg);
                    }
                    let editor = std::env::var("EDITOR").unwrap_or_else(|_| {
                        if cfg!(windows) {
                            "notepad".into()
                        } else {
                            "nano".into()
                        }
                    });
                    let status = std::process::Command::new(&editor).arg(&path).status();
                    match status {
                        Ok(s) if s.success() => {}
                        Ok(s) => {
                            eprintln!("phonton: {} exited with {}", editor, s);
                            std::process::exit(s.code().unwrap_or(1));
                        }
                        Err(e) => {
                            eprintln!("phonton: failed to launch {}: {}", editor, e);
                            std::process::exit(1);
                        }
                    }
                }
                "show" => {
                    let cfg = config::load().unwrap_or_default();
                    match toml::to_string_pretty(&cfg) {
                        Ok(s) => println!("{}", s),
                        Err(e) => {
                            eprintln!("phonton: failed to serialize config: {}", e);
                            std::process::exit(1);
                        }
                    }
                }
                other => {
                    eprintln!("phonton: unknown `config` subcommand: {}\n", other);
                    print_help();
                    std::process::exit(2);
                }
            }
            Ok(None)
        }
        "doctor" => {
            let working_dir =
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let code = doctor::run(&working_dir, &args[1..]).await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(None)
        }
        "extensions" => {
            let working_dir =
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let code = extensions_cli::run(&working_dir, &args[1..]).await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(None)
        }
        "skills" => {
            let working_dir =
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let code = extensions_cli::run_skills(&working_dir, &args[1..]).await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(None)
        }
        "steering" => {
            let working_dir =
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let code = extensions_cli::run_steering(&working_dir, &args[1..]).await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(None)
        }
        "mcp" => {
            let working_dir =
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let code = mcp_cli::run(&working_dir, &args[1..]).await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(None)
        }
        "plan" => {
            let code = plan_preview::run(&args[1..]).await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(None)
        }
        "review" => {
            let code = review::run(&args[1..]).await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(None)
        }
        "run" => {
            let code = run_command::run(&args[1..]).await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(None)
        }
        "memory" => {
            let code = memory_cli::run(&args[1..]).await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(None)
        }
        "ask" => {
            let question = args.get(1..).map(|a| a.join(" ")).unwrap_or_default();
            if question.trim().is_empty() {
                eprintln!("phonton: `ask` requires a question.\n  e.g. phonton ask \"how do I add a feature flag?\"");
                std::process::exit(2);
            }
            let cfg = config::load().unwrap_or_default();
            let provider = load_ask_provider(&cfg).ok_or_else(|| {
                anyhow::anyhow!(
                    "no provider configured — set an API key (e.g. ANTHROPIC_API_KEY) \
                     or run `phonton` and configure one in Settings"
                )
            })?;
            match provider
                .call("You are a helpful coding assistant.", &question, &[])
                .await
            {
                Ok(resp) => {
                    println!("{}", resp.content);
                    Ok(None)
                }
                Err(e) => {
                    eprintln!("phonton ask: {}", e);
                    std::process::exit(1);
                }
            }
        }
        other if other.starts_with('-') => {
            eprintln!("phonton: unknown flag {}\n", other);
            print_help();
            std::process::exit(2);
        }
        other => {
            eprintln!("phonton: unknown subcommand `{}`\n", other);
            print_help();
            std::process::exit(2);
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let launch_options = match handle_cli_args().await? {
        Some(options) => options,
        None => return Ok(()),
    };
    // Load configuration first so the rest of startup can use it.
    let mut cfg = config::load().unwrap_or_default();

    // First-run / no-model-set flow: probe the configured key for a
    // working model before we even draw the TUI. Bounded to ~6 seconds
    // (discovery + up to 3 tiny pings) so cold start stays snappy. If
    // the user already picked a model we leave it alone — they get to
    // override the auto-pick from Settings via Ctrl+D anyway.
    if cfg.provider.model.is_none() {
        if let Some(api_key) = provider_key_for_run(&cfg.provider) {
            let detect = tokio::time::timeout(
                std::time::Duration::from_secs(8),
                select_best_working_model(
                    &cfg.provider.name,
                    &api_key,
                    cfg.provider.base_url.as_deref(),
                    3,
                ),
            )
            .await;
            if let Ok(Ok(Some(model))) = detect {
                cfg.provider.model = Some(model);
                // Persist so the next launch is instant. Best-effort —
                // a broken HOME / readonly dotfile shouldn't abort
                // startup.
                let _ = config::save(&cfg);
            }
        }
    }

    // Sandbox scoped to the orchestrator's working directory (CWD at
    // launch). Shared across every spawned goal so tool-execution policy
    // is uniform across the session.
    let working_dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let workspace_key = workspace_session_key(&working_dir);
    let mut app = App::new(&cfg);
    app.nexus_status = detect_nexus_status(&working_dir);

    let store = match open_persistent_store() {
        Ok(s) => {
            app.store_path = Some(s.path().to_path_buf());
            s
        }
        Err(e) => {
            app.settings.message = Some(format!(
                "Persistent store unavailable ({e}); using in-memory store."
            ));
            Store::in_memory()?
        }
    };
    let store = Arc::new(std::sync::Mutex::new(store));
    let ask_provider = load_ask_provider(&cfg);

    // Workspace-trust gate. Before we touch the terminal, confirm the
    // user wants Phonton operating in this folder. Skips silently if
    // the workspace was previously trusted or `PHONTON_TRUST_ALL=1` is
    // set. On decline we exit before entering the alternate screen so
    // the user's normal terminal stays clean.
    if !trust::prompt_if_needed(&working_dir)? {
        return Ok(());
    }
    let trust_source = trust::trust_record(&working_dir)
        .map(|record| record.source)
        .unwrap_or(phonton_types::WorkspaceTrustSource::JsonRecord);
    let _ = trust::record_trust_with_mode(&working_dir, cfg.permissions.mode, trust_source);
    if let Some(record) = trust::trust_record(&working_dir) {
        if let Ok(s) = store.lock() {
            let _ = s.upsert_workspace_trust(&record);
        }
    }

    if launch_options.resume_last_session {
        match store.lock() {
            Ok(s) => match s.load_session_snapshot(&workspace_key) {
                Ok(Some(snapshot)) => app.restore_session_snapshot(snapshot),
                Ok(None) => {
                    app.settings.message = Some("No saved session for this workspace yet.".into());
                }
                Err(e) => {
                    app.settings.message = Some(format!("Could not load saved session: {e}"));
                }
            },
            Err(_) => {
                app.settings.message =
                    Some("Could not load saved session: store lock poisoned.".into());
            }
        }
    }

    let sandbox = Arc::new(Sandbox::new_with_mode(
        working_dir.clone(),
        "phonton-cli".to_string(),
        cfg.permissions.mode,
    ));
    let controls: ControlRegistry = Arc::new(std::sync::Mutex::new(HashMap::new()));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    execute!(stdout, SetCursorStyle::SteadyBar)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let (evt_tx, mut evt_rx) = mpsc::channel::<LoopEvent>(512);
    spawn_input_task(evt_tx.clone());

    let store_for_exit = Arc::clone(&store);
    let result = run_app(
        &mut terminal,
        &mut app,
        &mut evt_rx,
        evt_tx.clone(),
        store,
        ask_provider,
        sandbox,
        controls,
        cfg,
        working_dir,
    )
    .await;
    let exit_snapshot = if result.is_ok() && app.should_quit {
        Some(app.to_session_snapshot(workspace_key, now_unix_secs()))
    } else {
        None
    };

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        crossterm::cursor::Show,
        SetCursorStyle::DefaultUserShape,
    )?;
    terminal.show_cursor()?;
    if let Some(snapshot) = exit_snapshot {
        match store_for_exit.lock() {
            Ok(s) => {
                if let Err(e) = s.save_session_snapshot(&snapshot) {
                    eprintln!("phonton: failed to save session snapshot: {e}");
                }
            }
            Err(_) => eprintln!("phonton: failed to save session snapshot: store lock poisoned"),
        }
        println!("{}", render_exit_receipt(&snapshot.totals));
    }
    result
}

fn spawn_input_task(tx: mpsc::Sender<LoopEvent>) {
    std::thread::spawn(move || loop {
        // Poll on a modest cadence. This keeps input responsive while
        // preventing the splash animation and terminal cursor from feeling
        // like they are flashing on every frame.
        if event::poll(Duration::from_millis(UI_TICK_MS)).unwrap_or(false) {
            match event::read() {
                Ok(Event::Key(k)) if k.kind != event::KeyEventKind::Release => {
                    let mut keys = vec![k];
                    drain_queued_key_events(&mut keys);
                    if let Some(text) = bracketless_paste_text(&keys) {
                        if tx.blocking_send(LoopEvent::Paste(text)).is_err() {
                            break;
                        }
                    } else {
                        for key in keys {
                            if tx.blocking_send(LoopEvent::Key(key)).is_err() {
                                return;
                            }
                        }
                    }
                }
                Ok(Event::Paste(text)) => {
                    if let Err(_err) = tx.blocking_send(LoopEvent::Paste(text)) {
                        break;
                    }
                }
                Ok(Event::Mouse(mouse)) => {
                    if let Err(_err) = tx.blocking_send(LoopEvent::Mouse(mouse)) {
                        break;
                    }
                }
                _ => {}
            }
        } else if tx.blocking_send(LoopEvent::Tick).is_err() {
            break;
        }
    });
}

fn drain_queued_key_events(keys: &mut Vec<KeyEvent>) {
    while keys.len() < 4096 && event::poll(Duration::from_millis(1)).unwrap_or(false) {
        match event::read() {
            Ok(Event::Key(k)) if k.kind != event::KeyEventKind::Release => keys.push(k),
            Ok(_) => {}
            Err(_) => break,
        }
    }
}

fn bracketless_paste_text(keys: &[KeyEvent]) -> Option<String> {
    if keys.len() < 2 {
        return None;
    }

    let mut text = String::new();
    let mut line_breaks = 0usize;
    let mut printable = 0usize;
    for key in keys {
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        match key.code {
            KeyCode::Char(c) => {
                text.push(c);
                printable += 1;
            }
            KeyCode::Enter => {
                text.push('\n');
                line_breaks += 1;
            }
            KeyCode::Tab => {
                text.push('\t');
                printable += 1;
            }
            _ => return None,
        }
    }

    if printable == 0 {
        return None;
    }

    let looks_like_multiline_paste = line_breaks >= 2 || (line_breaks >= 1 && keys.len() >= 8);
    let looks_like_long_single_line_paste = line_breaks == 0 && printable >= 64;
    if looks_like_multiline_paste || looks_like_long_single_line_paste {
        Some(text)
    } else {
        None
    }
}

fn read_clipboard_text() -> std::result::Result<Option<String>, String> {
    #[cfg(windows)]
    {
        let output = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", "Get-Clipboard -Raw"])
            .output()
            .map_err(|e| e.to_string())?;
        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(if err.is_empty() {
                "PowerShell Get-Clipboard failed".into()
            } else {
                err
            });
        }
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        let text = text.trim_end_matches(['\r', '\n']).to_string();
        if text.is_empty() {
            Ok(None)
        } else {
            Ok(Some(text))
        }
    }
    #[cfg(not(windows))]
    {
        Err("clipboard paste is currently implemented for Windows only".into())
    }
}

fn write_clipboard_text(text: &str) -> std::result::Result<(), String> {
    #[cfg(windows)]
    {
        let mut child = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", "Set-Clipboard"])
            .stdin(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        if let Some(stdin) = child.stdin.as_mut() {
            use std::io::Write;
            stdin
                .write_all(text.as_bytes())
                .map_err(|e| e.to_string())?;
        }
        let output = child.wait_with_output().map_err(|e| e.to_string())?;
        if output.status.success() {
            Ok(())
        } else {
            let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
            Err(if err.is_empty() {
                "PowerShell Set-Clipboard failed".into()
            } else {
                err
            })
        }
    }
    #[cfg(not(windows))]
    {
        let _ = text;
        Err("clipboard copy is currently implemented for Windows only".into())
    }
}

async fn run_app<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    rx: &mut mpsc::Receiver<LoopEvent>,
    tx: mpsc::Sender<LoopEvent>,
    store: Arc<std::sync::Mutex<Store>>,
    ask_provider: Option<Arc<dyn Provider>>,
    mut sandbox: Arc<Sandbox>,
    controls: ControlRegistry,
    mut cfg: config::Config,
    working_dir: std::path::PathBuf,
) -> Result<()> {
    // Mutable so Save Settings can swap in a freshly-built provider after
    // the user changes the API key / model / provider in the TUI. Without
    // this the Ask side panel would keep using the original credentials
    // until the user restarted the CLI.
    let mut ask_provider = ask_provider;
    let mut approval_replies: HashMap<u64, oneshot::Sender<McpApprovalDecision>> = HashMap::new();
    loop {
        terminal.draw(|f| render(f, app))?;
        let Some(evt) = rx.recv().await else { break };
        match evt {
            LoopEvent::Tick => {
                app.spinner_frame = app.spinner_frame.wrapping_add(1);
                if app.new_best_ticks > 0 {
                    app.new_best_ticks -= 1;
                }
            }
            LoopEvent::Paste(text) => {
                app.handle_paste(&text);
            }
            LoopEvent::Mouse(mouse) => app.handle_mouse(mouse),
            LoopEvent::Key(k) => {
                if let Some(intent) = app.handle_key(k) {
                    match intent {
                        Intent::Quit => {
                            deny_pending_mcp_approvals(&mut approval_replies);
                            break;
                        }
                        Intent::PasteClipboard => match read_clipboard_text() {
                            Ok(Some(text)) => app.handle_paste(&text),
                            Ok(None) => {
                                app.settings.message = Some("Clipboard is empty.".into());
                            }
                            Err(e) => {
                                app.settings.message = Some(format!("Clipboard paste failed: {e}"));
                            }
                        },
                        Intent::CopyFocus(text) => match write_clipboard_text(&text) {
                            Ok(()) => {
                                app.command_notice = Some("Copied current focus view.".into());
                            }
                            Err(e) => {
                                app.command_notice = Some(format!("Copy failed: {e}"));
                            }
                        },
                        Intent::RunCommand(text) => {
                            if let Some(parsed) = parse_prompt_command(&text) {
                                let command = parsed.original.clone();
                                app.push_command_run(CommandRunSummary {
                                    command: command.clone(),
                                    exit_code: None,
                                    stdout_preview: "running...".into(),
                                    stderr_preview: String::new(),
                                    duration_ms: None,
                                });
                                let tx2 = tx.clone();
                                let sandbox = Arc::clone(&sandbox);
                                tokio::spawn(async move {
                                    let started = std::time::Instant::now();
                                    let summary = match sandbox.run_tool(parsed.call).await {
                                        Ok(output) => summarize_output_with_duration(
                                            &command,
                                            output.status.code(),
                                            &output.stdout,
                                            &output.stderr,
                                            Some(started.elapsed().as_millis() as u64),
                                        ),
                                        Err(e) => CommandRunSummary {
                                            command,
                                            exit_code: None,
                                            stdout_preview: String::new(),
                                            stderr_preview: e.to_string(),
                                            duration_ms: Some(started.elapsed().as_millis() as u64),
                                        },
                                    };
                                    let _ = tx2.send(LoopEvent::CommandFinished(summary)).await;
                                });
                            }
                        }
                        Intent::QueueGoal(text) | Intent::QueueTask(text) => {
                            let direct_task = app.mode == Mode::Task;
                            // `handle_key` inserts the goal at index 0.
                            let task_id = app.goals.first().map(|g| g.task_id).unwrap_or_default();
                            // Sync any in-memory Settings inputs into cfg so
                            // this goal uses the *currently displayed*
                            // provider/model/key — not the stale on-disk
                            // version. Otherwise editing Settings without
                            // explicitly saving would silently route goals
                            // through the previous provider while the System
                            // panel showed the new one (a real footgun).
                            apply_settings_to_config(&app.settings, &mut cfg);
                            let tx_goal = tx.clone();
                            let store_goal = Arc::clone(&store);
                            let sandbox_goal = Arc::clone(&sandbox);
                            let controls_goal = Arc::clone(&controls);
                            let cfg_goal = cfg.clone();
                            let working_dir_goal = working_dir.clone();
                            tokio::spawn(async move {
                                spawn_goal(
                                    0,
                                    task_id,
                                    text,
                                    direct_task,
                                    tx_goal,
                                    store_goal,
                                    sandbox_goal,
                                    controls_goal,
                                    cfg_goal,
                                    working_dir_goal,
                                )
                                .await;
                            });
                        }
                        Intent::Rollback { goal_index, to_seq } => {
                            if let Ok(reg) = controls.lock() {
                                if let Some(gc) = reg.get(&goal_index) {
                                    let _ = gc
                                        .control_tx
                                        .try_send(OrchestratorMessage::RollbackRequest { to_seq });
                                }
                            }
                        }
                        Intent::CompactContext { goal_index } => {
                            if let Ok(reg) = controls.lock() {
                                if let Some(gc) = reg.get(&goal_index) {
                                    let _ =
                                        gc.control_tx.try_send(OrchestratorMessage::CompactContext);
                                }
                            }
                        }
                        Intent::StopGoal { goal_index } => {
                            if let Ok(reg) = controls.lock() {
                                if let Some(gc) = reg.get(&goal_index) {
                                    let _ =
                                        gc.control_tx.try_send(OrchestratorMessage::UserCancelled);
                                }
                            }
                        }
                        Intent::ResolveMcpApproval {
                            approval_id,
                            approved,
                        } => {
                            if let Some(reply_tx) = approval_replies.remove(&approval_id) {
                                let decision = if approved {
                                    McpApprovalDecision::Approved
                                } else {
                                    McpApprovalDecision::Denied
                                };
                                let _ = reply_tx.send(decision);
                            }
                        }
                        Intent::SaveSettings => {
                            apply_settings_to_config(&app.settings, &mut cfg);
                            sandbox = Arc::new(Sandbox::new_with_mode(
                                working_dir.clone(),
                                "phonton-cli".to_string(),
                                cfg.permissions.mode,
                            ));

                            // Swap the in-memory ask provider so the next
                            // Ctrl+; question uses the new credentials
                            // immediately — without this Save would only
                            // affect goals (which read cfg per-spawn) and
                            // leave Ask stuck on the startup provider.
                            ask_provider = load_ask_provider(&cfg);

                            match config::save(&cfg) {
                                Ok(_) => {
                                    let where_ = match ask_provider {
                                        Some(_) => "Settings saved — Ask + new goals use them now.",
                                        None => "Settings saved — but no working API key resolved yet (Ask disabled).",
                                    };
                                    app.settings.message = Some(where_.into());
                                }
                                Err(e) => app.settings.message = Some(format!("Error saving: {e}")),
                            }
                        }
                        Intent::TestConnection => {
                            // Spawn the smoke test off-thread so the UI
                            // doesn't freeze during the round-trip.
                            let provider_name = app.settings.provider.clone();
                            let model = if app.settings.model.is_empty() {
                                default_model_for(&provider_name)
                            } else {
                                app.settings.model.clone()
                            };
                            let key = if app.settings.api_key.is_empty() {
                                let stub = config::ProviderConfig {
                                    name: provider_name.clone(),
                                    api_key: None,
                                    model: None,
                                    account_id: if app.settings.account_id.is_empty() {
                                        None
                                    } else {
                                        Some(app.settings.account_id.clone())
                                    },
                                    base_url: None,
                                };
                                provider_key_for_run(&stub).unwrap_or_default()
                            } else {
                                app.settings.api_key.clone()
                            };
                            let base = if app.settings.base_url.is_empty() {
                                None
                            } else {
                                Some(app.settings.base_url.clone())
                            };
                            let account_id = if app.settings.account_id.is_empty() {
                                None
                            } else {
                                Some(app.settings.account_id.clone())
                            };
                            app.settings.message =
                                Some(format!("Testing {provider_name} with model {model}…"));
                            let tx2 = tx.clone();
                            tokio::spawn(async move {
                                let result = test_provider(
                                    provider_name.clone(),
                                    key,
                                    model,
                                    account_id,
                                    base,
                                )
                                .await;
                                // Re-use the AskAnswer channel as a generic
                                // "string back to settings" — main loop
                                // routes it onto settings.message when in
                                // settings mode.
                                let msg = match result {
                                    Ok(reply) => format!(
                                        "✓ Connected — got reply ({} chars). Key works.",
                                        reply.len()
                                    ),
                                    Err(e) => format!("✗ Connection failed: {e}"),
                                };
                                let _ = tx2.send(LoopEvent::TestResult(msg)).await;
                            });
                        }
                        Intent::DetectModels => {
                            let provider_name = app.settings.provider.clone();
                            let key = if app.settings.api_key.is_empty() {
                                let stub = config::ProviderConfig {
                                    name: provider_name.clone(),
                                    api_key: None,
                                    model: None,
                                    account_id: if app.settings.account_id.is_empty() {
                                        None
                                    } else {
                                        Some(app.settings.account_id.clone())
                                    },
                                    base_url: None,
                                };
                                provider_key_for_run(&stub).unwrap_or_default()
                            } else {
                                app.settings.api_key.clone()
                            };
                            let base = if app.settings.base_url.is_empty() {
                                None
                            } else {
                                Some(app.settings.base_url.clone())
                            };
                            let account_id = if app.settings.account_id.is_empty() {
                                None
                            } else {
                                Some(app.settings.account_id.clone())
                            };
                            let probe_base =
                                provider_probe_base_url(&provider_name, account_id, base);
                            if key.trim().is_empty() && provider_requires_key(&provider_name) {
                                app.settings.message =
                                    Some("Detect failed: no API key in field or env var.".into());
                            } else {
                                app.settings.message =
                                    Some(format!("Detecting models for {provider_name}…"));
                                let tx2 = tx.clone();
                                tokio::spawn(async move {
                                    // List the catalogue first — needed
                                    // for the summary regardless of
                                    // probe outcome.
                                    let list_res = discover_models(
                                        &provider_name,
                                        &key,
                                        probe_base.as_deref(),
                                    )
                                    .await;
                                    let payload = match list_res {
                                        Ok(models) if models.is_empty() => Err(format!(
                                            "✗ {provider_name}: key valid but no models accessible."
                                        )),
                                        Ok(models) => {
                                            // Probe top candidates so we
                                            // pick a model the key can
                                            // actually call right now,
                                            // not just one in the
                                            // catalogue.
                                            let probed = select_best_working_model(
                                                &provider_name,
                                                &key,
                                                probe_base.as_deref(),
                                                3,
                                            )
                                            .await
                                            .ok()
                                            .flatten();
                                            let picked = probed
                                                .or_else(|| {
                                                    pick_default_from_list(&provider_name, &models)
                                                })
                                                .unwrap_or_else(|| models[0].clone());
                                            let preview: Vec<String> =
                                                models.iter().take(5).cloned().collect();
                                            let more = if models.len() > 5 {
                                                format!(" … (+{} more)", models.len() - 5)
                                            } else {
                                                String::new()
                                            };
                                            let summary = format!(
                                                "✓ {} models found. Picked `{}` (probed). Sample: {}{}",
                                                models.len(),
                                                picked,
                                                preview.join(", "),
                                                more
                                            );
                                            Ok((picked, summary))
                                        }
                                        Err(e) => Err(format!("✗ Detect failed: {e}")),
                                    };
                                    let _ = tx2.send(LoopEvent::DetectResult(payload)).await;
                                });
                            }
                        }
                        Intent::OpenModelPicker => {
                            app.settings.picker_open = true;
                            app.settings.picker.filter.clear();
                            app.settings.picker.selected = 0;
                            app.settings.picker.scroll = 0;
                            // If we already have a list, just open it.
                            // Otherwise kick off a background fetch.
                            if !app.settings.picker.all_models.is_empty() {
                                app.settings.picker.rebuild_filter();
                            } else {
                                app.settings.picker.loading = true;
                                app.settings.picker.filtered.clear();
                                let provider_name = app.settings.provider.clone();
                                let key = if app.settings.api_key.is_empty() {
                                    let stub = config::ProviderConfig {
                                        name: provider_name.clone(),
                                        api_key: None,
                                        model: None,
                                        account_id: if app.settings.account_id.is_empty() {
                                            None
                                        } else {
                                            Some(app.settings.account_id.clone())
                                        },
                                        base_url: None,
                                    };
                                    provider_key_for_run(&stub).unwrap_or_default()
                                } else {
                                    app.settings.api_key.clone()
                                };
                                let base = if app.settings.base_url.is_empty() {
                                    None
                                } else {
                                    Some(app.settings.base_url.clone())
                                };
                                let account_id = if app.settings.account_id.is_empty() {
                                    None
                                } else {
                                    Some(app.settings.account_id.clone())
                                };
                                let probe_base =
                                    provider_probe_base_url(&provider_name, account_id, base);
                                let tx2 = tx.clone();
                                tokio::spawn(async move {
                                    let res = discover_models(
                                        &provider_name,
                                        &key,
                                        probe_base.as_deref(),
                                    )
                                    .await
                                    .map_err(|e| e.to_string());
                                    let _ = tx2.send(LoopEvent::ModelsLoaded(res)).await;
                                });
                            }
                        }
                        Intent::OpenMemory => match store.lock() {
                            Ok(s) => match s.query_memory(None, None, 50).await {
                                Ok(rows) => app.memory_records = rows,
                                Err(e) => {
                                    app.settings.message = Some(format!("Memory load failed: {e}"))
                                }
                            },
                            Err(_) => {
                                app.settings.message =
                                    Some("Memory load failed: store lock poisoned".into())
                            }
                        },
                        Intent::OpenHistory => match store.lock() {
                            Ok(s) => match s.list_tasks(50).await {
                                Ok(rows) => {
                                    app.history_records = rows;
                                    app.clamp_history_selection();
                                }
                                Err(e) => {
                                    app.settings.message = Some(format!("History load failed: {e}"))
                                }
                            },
                            Err(_) => {
                                app.settings.message =
                                    Some("History load failed: store lock poisoned".into())
                            }
                        },
                        Intent::RevokeCurrentTrust => {
                            let revoked = trust::revoke_trust(&working_dir).unwrap_or(false);
                            if revoked {
                                if let Ok(s) = store.lock() {
                                    let workspace = workspace_session_key(&working_dir);
                                    let _ = s.delete_workspace_trust(&workspace);
                                }
                                app.ask_answer = Some(
                                    "Trust\nCurrent workspace trust revoked. Restart Phonton to re-approve before more work."
                                        .into(),
                                );
                            } else {
                                app.ask_answer =
                                    Some("Trust\nCurrent workspace was not trusted.".into());
                            }
                        }
                        Intent::AcceptTrust | Intent::DeclineTrust => {
                            // Trust prompt is handled before the TUI loop
                            // starts; if we ever see one in here it is
                            // a no-op.
                        }
                        Intent::Ask(q) => {
                            // Stateless ask: no orchestrator involvement,
                            // no goal context, no memory write.
                            let tx2 = tx.clone();
                            let provider = ask_provider.clone();
                            app.ask_pending = true;
                            app.ask_answer = None;
                            app.ask_scroll = 0;
                            tokio::spawn(async move {
                                let a = match provider {
                                    Some(p) => match p
                                        .call("You are a helpful coding assistant.", &q, &[])
                                        .await
                                    {
                                        Ok(resp) => resp.content,
                                        Err(e) => format!("ask failed: {e}"),
                                    },
                                    None => "Set ANTHROPIC_API_KEY or OPENAI_API_KEY \
                                        to enable ask mode."
                                        .to_string(),
                                };
                                let _ = tx2.send(LoopEvent::AskAnswer(a)).await;
                            });
                        }
                    }
                }
            }
            LoopEvent::StateUpdate(idx, state) => app.apply_state(idx, *state),
            LoopEvent::FlightEvent(idx, ev) => app.apply_event(idx, ev),
            LoopEvent::AskAnswer(a) => {
                app.ask_pending = false;
                app.ask_answer = Some(a);
                app.ask_scroll = 0;
            }
            LoopEvent::CommandFinished(summary) => {
                app.push_command_run(summary);
            }
            LoopEvent::McpApprovalRequested { prompt, reply_tx } => {
                approval_replies.insert(prompt.id, reply_tx);
                app.push_mcp_approval(prompt);
            }
            LoopEvent::TestResult(msg) => {
                app.settings.model_ok = Some(msg.starts_with('✓'));
                app.settings.message = Some(msg);
            }
            LoopEvent::DetectResult(res) => match res {
                Ok((picked, summary)) => {
                    app.settings.model = picked.clone();
                    app.settings.model_ok = None;
                    // Also populate the picker cache with the probed list
                    // so the user can open it immediately without another
                    // fetch.
                    app.settings.message = Some(summary);
                }
                Err(msg) => {
                    app.settings.message = Some(msg);
                }
            },
            LoopEvent::ModelsLoaded(res) => {
                app.settings.picker.loading = false;
                match res {
                    Ok(models) => {
                        // Pre-select the currently configured model in
                        // the list so the cursor starts on it.
                        let cur = &app.settings.model;
                        let sel = models.iter().position(|m| m == cur).unwrap_or(0);
                        app.settings.picker.all_models = models;
                        app.settings.picker.selected = sel;
                        app.settings.picker.scroll = sel.saturating_sub(3);
                        app.settings.picker.rebuild_filter();
                    }
                    Err(e) => {
                        app.settings.picker_open = false;
                        app.settings.message = Some(format!("✗ Could not fetch models: {e}"));
                    }
                }
            }
        }
        if app.should_quit {
            deny_pending_mcp_approvals(&mut approval_replies);
            break;
        }
    }
    Ok(())
}

const MAX_TEXT_ATTACHMENT_BYTES: u64 = 64 * 1024;
const MAX_IMAGE_ATTACHMENT_BYTES: u64 = 5 * 1024 * 1024;

fn single_task_plan(description: String, attachments: Vec<PromptAttachment>) -> PlannerOutput {
    let goal_contract = Goal::new(description.clone())
        .with_attachments(attachments.clone())
        .contract();
    let subtask = Subtask {
        id: SubtaskId::new(),
        description,
        model_tier: ModelTier::Standard,
        dependencies: Vec::new(),
        attachments,
        status: SubtaskStatus::Queued,
    };

    PlannerOutput {
        subtasks: vec![subtask],
        estimated_total_tokens: 1_200,
        naive_baseline_tokens: 4_000,
        coverage_summary: CoverageSummary::default(),
        goal_contract: Some(goal_contract),
    }
}

fn prepare_goal_attachments(text: &str, working_dir: &Path) -> Vec<PromptAttachment> {
    let workspace_root = working_dir
        .canonicalize()
        .unwrap_or_else(|_| working_dir.to_path_buf());
    let mut seen = HashSet::<PathBuf>::new();
    let mut attachments = Vec::new();

    for raw in extract_file_mentions(text) {
        if let Some(attachment) = load_prompt_attachment(&raw, working_dir, &workspace_root) {
            let key = workspace_root.join(&attachment.path);
            if seen.insert(key) {
                attachments.push(attachment);
            }
        }
    }

    attachments
}

fn extract_file_mentions(text: &str) -> Vec<String> {
    let mut mentions = Vec::new();
    let mut iter = text.char_indices().peekable();

    while let Some((_, ch)) = iter.next() {
        if ch != '@' {
            continue;
        }

        let Some(&(next_idx, next_ch)) = iter.peek() else {
            continue;
        };

        let raw = if next_ch == '"' || next_ch == '\'' {
            let quote = next_ch;
            iter.next();
            let start = next_idx + quote.len_utf8();
            let mut end = start;
            for (idx, c) in iter.by_ref() {
                if c == quote {
                    end = idx;
                    break;
                }
                end = idx + c.len_utf8();
            }
            text[start..end].trim().to_string()
        } else if next_ch == '[' {
            iter.next();
            let start = next_idx + next_ch.len_utf8();
            let mut end = start;
            for (idx, c) in iter.by_ref() {
                if c == ']' {
                    end = idx;
                    break;
                }
                end = idx + c.len_utf8();
            }
            text[start..end].trim().to_string()
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
            text[start..end]
                .trim_matches(|c: char| matches!(c, '.' | ':' | '!' | '?' | ']' | '}'))
                .trim()
                .to_string()
        };

        if !raw.is_empty() {
            mentions.push(raw);
        }
    }

    mentions
}

fn load_prompt_attachment(
    raw: &str,
    working_dir: &Path,
    workspace_root: &Path,
) -> Option<PromptAttachment> {
    let candidate = PathBuf::from(raw);
    let resolved = if candidate.is_absolute() {
        candidate
    } else {
        working_dir.join(candidate)
    };
    let canonical = resolved.canonicalize().ok()?;
    if !canonical.starts_with(workspace_root) {
        return None;
    }

    let metadata = std::fs::metadata(&canonical).ok()?;
    if !metadata.is_file() {
        return None;
    }

    let path = canonical
        .strip_prefix(workspace_root)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| canonical.clone());
    let size_bytes = metadata.len();
    let mime_type = mime_for_path(&canonical).map(str::to_string);

    if is_image_path(&canonical) {
        let (data_base64, truncated, note) = if size_bytes <= MAX_IMAGE_ATTACHMENT_BYTES {
            let bytes = std::fs::read(&canonical).ok()?;
            (Some(general_purpose::STANDARD.encode(bytes)), false, None)
        } else {
            (
                None,
                true,
                Some(format!(
                    "image payload omitted because it is larger than {} bytes",
                    MAX_IMAGE_ATTACHMENT_BYTES
                )),
            )
        };
        return Some(PromptAttachment {
            path,
            kind: PromptAttachmentKind::Image,
            mime_type,
            size_bytes,
            text: None,
            data_base64,
            truncated,
            note,
        });
    }

    use std::io::Read;
    let mut file = std::fs::File::open(&canonical).ok()?;
    let mut bytes = Vec::with_capacity(size_bytes.min(MAX_TEXT_ATTACHMENT_BYTES) as usize);
    file.by_ref()
        .take(MAX_TEXT_ATTACHMENT_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    let truncated = bytes.len() as u64 > MAX_TEXT_ATTACHMENT_BYTES;
    if truncated {
        bytes.truncate(MAX_TEXT_ATTACHMENT_BYTES as usize);
    }
    if bytes.contains(&0) {
        return Some(PromptAttachment {
            path,
            kind: PromptAttachmentKind::Unsupported,
            mime_type,
            size_bytes,
            text: None,
            data_base64: None,
            truncated: false,
            note: Some("binary file was mentioned but not attached as text".into()),
        });
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let note = truncated.then(|| {
        format!(
            "file content truncated to the first {} bytes",
            MAX_TEXT_ATTACHMENT_BYTES
        )
    });

    Some(PromptAttachment {
        path,
        kind: PromptAttachmentKind::Text,
        mime_type,
        size_bytes,
        text: Some(text),
        data_base64: None,
        truncated,
        note,
    })
}

fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            matches!(
                e.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg"
            )
        })
        .unwrap_or(false)
}

fn mime_for_path(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg") | Some("jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        Some("bmp") => Some("image/bmp"),
        Some("svg") => Some("image/svg+xml"),
        Some("rs") => Some("text/x-rust"),
        Some("ts") => Some("text/typescript"),
        Some("tsx") => Some("text/tsx"),
        Some("js") => Some("text/javascript"),
        Some("jsx") => Some("text/jsx"),
        Some("py") => Some("text/x-python"),
        Some("md") | Some("mdx") => Some("text/markdown"),
        Some("json") => Some("application/json"),
        Some("toml") => Some("application/toml"),
        Some("yaml") | Some("yml") => Some("application/yaml"),
        Some("txt") | Some("log") => Some("text/plain"),
        Some("html") => Some("text/html"),
        Some("css") => Some("text/css"),
        _ => None,
    }
}

fn outcome_ledger_from_state(task_id: TaskId, state: &GlobalState) -> Option<OutcomeLedger> {
    let handoff = state.handoff_packet.clone()?;
    Some(OutcomeLedger {
        task_id,
        goal_contract: state.goal_contract.clone(),
        context_manifest: ContextManifest::default(),
        permission_ledger: PermissionLedger::default(),
        verify_report: handoff.verification.clone(),
        handoff: Some(handoff),
    })
}

fn state_for_plan_status(plan: &PlannerOutput, task_status: TaskStatus) -> GlobalState {
    GlobalState {
        task_status,
        goal_contract: plan.goal_contract.clone(),
        handoff_packet: None,
        active_workers: Vec::new(),
        tokens_used: 0,
        tokens_budget: None,
        estimated_naive_tokens: plan.naive_baseline_tokens,
        checkpoints: Vec::new(),
    }
}

async fn spawn_goal(
    goal_index: usize,
    task_id: TaskId,
    text: String,
    direct_task: bool,
    tx: mpsc::Sender<LoopEvent>,
    store: Arc<std::sync::Mutex<Store>>,
    sandbox: Arc<Sandbox>,
    controls: ControlRegistry,
    cfg: config::Config,
    working_dir: std::path::PathBuf,
) {
    let _ = tx
        .send(LoopEvent::StateUpdate(
            goal_index,
            Box::new(GlobalState {
                task_status: TaskStatus::Planning,
                goal_contract: None,
                handoff_packet: None,
                active_workers: Vec::new(),
                tokens_used: 0,
                tokens_budget: cfg.budget.max_tokens,
                estimated_naive_tokens: 0,
                checkpoints: Vec::new(),
            }),
        ))
        .await;

    let attachments = prepare_goal_attachments(&text, &working_dir);
    if let Ok(g) = store.lock() {
        let _ = g.upsert_task(task_id, &text, &TaskStatus::Planning, 0);
    }
    let memory_store = phonton_memory::MemoryStore::new(Arc::clone(&store)).await;

    let plan_result = if direct_task {
        Ok(single_task_plan(text.clone(), attachments.clone()))
    } else {
        let goal = Goal::new(text.clone()).with_attachments(attachments.clone());
        decompose_with_memory_store(&goal, &memory_store).await
    };
    let mut plan = match plan_result {
        Ok(p) => p,
        Err(_) => return,
    };
    apply_workspace_preflight(&mut plan, &working_dir, &text);

    let planning_state = state_for_plan_status(&plan, TaskStatus::Planning);
    let _ = tx
        .send(LoopEvent::StateUpdate(goal_index, Box::new(planning_state)))
        .await;

    if provider_key_for_run(&cfg.provider).is_none() && provider_requires_key(&cfg.provider.name) {
        let reason = format!(
            "provider `{}` needs an API key before dispatch; open /settings or set the provider env var",
            cfg.provider.name
        );
        let failed = GlobalState {
            task_status: TaskStatus::Failed {
                reason: reason.clone(),
                failed_subtask: None,
            },
            goal_contract: plan.goal_contract.clone(),
            handoff_packet: None,
            active_workers: Vec::new(),
            tokens_used: 0,
            tokens_budget: cfg.budget.max_tokens,
            estimated_naive_tokens: plan.naive_baseline_tokens,
            checkpoints: Vec::new(),
        };
        if let Ok(g) = store.lock() {
            let _ = g.upsert_task(task_id, &text, &failed.task_status, 0);
        }
        let _ = tx
            .send(LoopEvent::StateUpdate(goal_index, Box::new(failed)))
            .await;
        return;
    }

    let (state_tx, mut state_rx) = watch::channel(GlobalState {
        task_status: TaskStatus::Planning,
        goal_contract: plan.goal_contract.clone(),
        handoff_packet: None,
        active_workers: Vec::new(),
        tokens_used: 0,
        tokens_budget: None,
        estimated_naive_tokens: plan.naive_baseline_tokens,
        checkpoints: Vec::new(),
    });

    // Broadcast channel for structured telemetry. Capacity is generous so
    // a slow TUI subscriber never drops events from the store writer.
    let (event_tx, _) = broadcast::channel::<EventRecord>(1024);
    let mut event_rx_ui = event_tx.subscribe();
    let mut event_rx_store = event_tx.subscribe();

    let extension_set = load_extensions(&ExtensionLoadOptions::for_workspace(&working_dir));
    apply_extension_context_to_plan(&mut plan, &extension_set);
    publish_extension_events(task_id, &extension_set, &event_tx);

    let naive = plan.naive_baseline_tokens;
    let semantic_context = build_semantic_context(&working_dir).await;
    let mcp_runtime = if extension_set.mcp_servers.is_empty() {
        None
    } else {
        let approver = Arc::new(TuiMcpApprover::new(goal_index, tx.clone()));
        Some(Arc::new(
            phonton_mcp::McpRuntime::new(
                extension_set.mcp_servers.clone(),
                ExecutionGuard::new_with_mode(working_dir.clone(), cfg.permissions.mode),
            )
            .with_approver(approver)
            .with_event_sink(task_id, event_tx.clone()),
        ))
    };

    let dispatcher: Arc<dyn WorkerDispatcher> =
        if let Some(api_key) = provider_key_for_run(&cfg.provider) {
            let provider_name = cfg.provider.name.clone();
            let account_id = cfg.provider.account_id.clone();
            let base_url = cfg.provider.base_url.clone();
            // CRITICAL: when the user (or auto-detect) picked a specific model,
            // honour it for *every* tier. The previous behaviour was to call
            // `model_for_tier(provider, tier)` and silently override the chosen
            // model with the hard-coded tier default — so a Gemini key that
            // only has access to `gemma-4-31b-it` would 404 the moment goal
            // dispatch tried `gemini-2.5-flash`. Test/Ask used the configured
            // model and worked; goals didn't, and the gap was invisible.
            let configured_model = cfg.provider.model.clone();
            let validation_model = configured_model.clone().unwrap_or_else(|| {
                phonton_providers::model_for_tier(&provider_name, phonton_types::ModelTier::Cheap)
            });
            let Some(provider_template) = make_api_provider_config(
                &provider_name,
                api_key.clone(),
                validation_model,
                account_id,
                base_url,
            ) else {
                let reason = provider_config_failure_message(&provider_name);
                let failed = GlobalState {
                    task_status: TaskStatus::Failed {
                        reason: reason.clone(),
                        failed_subtask: None,
                    },
                    goal_contract: plan.goal_contract.clone(),
                    handoff_packet: None,
                    active_workers: Vec::new(),
                    tokens_used: 0,
                    tokens_budget: None,
                    estimated_naive_tokens: plan.naive_baseline_tokens,
                    checkpoints: Vec::new(),
                };
                if let Ok(g) = store.lock() {
                    let _ = g.upsert_task(task_id, &text, &failed.task_status, 0);
                }
                let _ = tx
                    .send(LoopEvent::StateUpdate(goal_index, Box::new(failed)))
                    .await;
                return;
            };

            let factory = move |tier: phonton_types::ModelTier| {
                let model = configured_model
                    .clone()
                    .unwrap_or_else(|| phonton_providers::model_for_tier(&provider_name, tier));
                provider_for(provider_config_with_model(&provider_template, model))
            };

            let guard = ExecutionGuard::new_with_mode(working_dir.clone(), cfg.permissions.mode);
            let mut d =
                phonton_worker::dispatcher::RealDispatcher::new(factory, guard, sandbox.clone())
                    .with_task_id(task_id)
                    .with_memory(memory_store.clone());
            if let Some(ctx) = semantic_context.clone() {
                d = d.with_semantic_context(ctx);
            }
            if let Some(runtime) = mcp_runtime.clone() {
                d = d.with_mcp_runtime(runtime);
            }
            Arc::new(d)
        } else {
            Arc::new(StubDispatcher::new(sandbox.clone()))
        };

    // Wire phonton-diff so the orchestrator takes a checkpoint commit
    // after every subtask passes verify.
    let diff_applier = DiffApplier::open(&working_dir)
        .ok()
        .map(|d| Arc::new(std::sync::Mutex::new(d)));

    // Control channel for rollback requests from the UI.
    let (ctrl_tx, ctrl_rx) = mpsc::channel::<OrchestratorMessage>(8);
    if let Ok(mut reg) = controls.lock() {
        reg.insert(
            goal_index,
            GoalControl {
                control_tx: ctrl_tx,
            },
        );
    }

    let limits = BudgetLimits {
        max_tokens: cfg.budget.max_tokens,
        max_usd_micros: cfg.budget.max_usd_micros(),
    };
    let mut budget_guard = BudgetGuard::new(limits);
    if let Some(model) = cfg.provider.model.as_deref() {
        if cfg.provider.name == "ollama" {
            budget_guard = budget_guard.with_price(
                ProviderKind::Ollama,
                model,
                ModelPricing {
                    input_usd_micros_per_mtok: 0,
                    output_usd_micros_per_mtok: 0,
                },
            );
        } else if cfg.provider.name == "cloudflare" && model == "@cf/moonshotai/kimi-k2.6" {
            budget_guard = budget_guard.with_price(
                ProviderKind::Cloudflare,
                model,
                ModelPricing {
                    input_usd_micros_per_mtok: 950_000,
                    output_usd_micros_per_mtok: 4_000_000,
                },
            );
        }
    }

    let mut orch = Orchestrator::new(dispatcher)
        .with_naive_baseline(naive)
        .with_budget_guard(budget_guard)
        .with_working_dir(working_dir.clone())
        .with_memory(memory_store)
        .with_event_sink(task_id, text.clone(), event_tx)
        .with_control_channel(ctrl_rx);
    if let Some(da) = diff_applier {
        orch = orch.with_diff_applier(da);
    }

    // Drive the orchestrator and forward every `GlobalState` update.
    let tx_updates = tx.clone();
    let store_for_states = store.clone();
    let goal_text_for_states = text.clone();
    tokio::spawn(async move {
        while state_rx.changed().await.is_ok() {
            let s = state_rx.borrow().clone();
            if let Ok(g) = store_for_states.lock() {
                let _ = g.upsert_task(
                    task_id,
                    &goal_text_for_states,
                    &s.task_status,
                    s.tokens_used,
                );
                if let Some(ledger) = outcome_ledger_from_state(task_id, &s) {
                    let _ = g.upsert_outcome_ledger(&ledger);
                }
            }
            if tx_updates
                .send(LoopEvent::StateUpdate(goal_index, Box::new(s)))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // Forward events to the TUI Flight Log.
    let tx_events = tx.clone();
    tokio::spawn(async move {
        loop {
            match event_rx_ui.recv().await {
                Ok(rec) => {
                    if tx_events
                        .send(LoopEvent::FlightEvent(goal_index, rec))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // Persist every event — rusqlite is sync, so hop onto spawn_blocking
    // whenever we have a record to write.
    let store_for_events = store.clone();
    tokio::spawn(async move {
        loop {
            match event_rx_store.recv().await {
                Ok(rec) => {
                    let store = store_for_events.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        if let Ok(g) = store.lock() {
                            let _ = g.append_event(&rec);
                        }
                    })
                    .await;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    tokio::spawn(async move {
        let _ = orch.run_task(plan, state_tx).await;
    });
}

fn apply_extension_context_to_plan(plan: &mut PlannerOutput, extension_set: &ExtensionSet) {
    let preamble = extension_set.render_prompt_preamble();
    if preamble.is_empty() {
        return;
    }

    for subtask in &mut plan.subtasks {
        subtask.description = format!("{preamble}\n\n{}", subtask.description);
    }
}

fn publish_extension_events(
    task_id: TaskId,
    extension_set: &ExtensionSet,
    event_tx: &broadcast::Sender<EventRecord>,
) {
    for manifest in &extension_set.manifests {
        send_event(
            task_id,
            event_tx,
            OrchestratorEvent::ExtensionLoaded {
                extension_id: manifest.id.clone(),
                kind: manifest.kind,
                source: manifest.source,
                enabled: manifest.enabled,
            },
        );
    }

    for conflict in &extension_set.conflicts {
        send_event(
            task_id,
            event_tx,
            OrchestratorEvent::ExtensionConflict {
                extension_id: conflict.id.clone(),
                lower_source: conflict.lower_source,
                higher_source: conflict.higher_source,
                detail: conflict.detail.clone(),
            },
        );
    }

    for diagnostic in &extension_set.diagnostics {
        let severity = match diagnostic.severity {
            DiagnosticSeverity::Warn => "warn",
            DiagnosticSeverity::Error => "error",
        };
        let reason = match &diagnostic.path {
            Some(path) => format!("{severity}: {} ({})", diagnostic.message, path.display()),
            None => format!("{severity}: {}", diagnostic.message),
        };
        send_event(
            task_id,
            event_tx,
            OrchestratorEvent::ExtensionSkipped {
                extension_id: None,
                kind: None,
                source: diagnostic.source,
                reason,
            },
        );
    }

    for rule in &extension_set.steering {
        send_event(
            task_id,
            event_tx,
            OrchestratorEvent::SteeringApplied {
                rule_id: rule.id.clone(),
                severity: rule.severity,
                target: "worker-context".into(),
            },
        );
    }

    for skill in &extension_set.skills {
        if skill.content.trim().is_empty() {
            continue;
        }
        send_event(
            task_id,
            event_tx,
            OrchestratorEvent::SkillApplied {
                skill_id: skill.definition.id.clone(),
                version: skill.definition.version.clone(),
                target: "worker-context".into(),
            },
        );
    }

    for server in &extension_set.mcp_servers {
        send_event(
            task_id,
            event_tx,
            OrchestratorEvent::McpServerAvailable {
                server_id: server.id.clone(),
                permissions: server.permissions.clone(),
            },
        );
    }
}

fn send_event(
    task_id: TaskId,
    event_tx: &broadcast::Sender<EventRecord>,
    event: OrchestratorEvent,
) {
    let record = EventRecord {
        task_id,
        timestamp_ms: current_timestamp_ms(),
        event,
    };
    let _ = event_tx.send(record);
}

fn current_timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use phonton_types::{
        AppliesTo, ExtensionSource, LLMResponse, McpServerDefinition, McpTransport, TrustLevel,
    };
    use ratatui::backend::TestBackend;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn approval_prompt(id: u64) -> PendingMcpApproval {
        PendingMcpApproval {
            id,
            goal_index: 0,
            server_id: ExtensionId::new("docs"),
            tool_name: "read_file".into(),
            permissions: vec![Permission::FsReadWorkspace],
            reason: "read docs from the current workspace".into(),
        }
    }

    fn review_ready_event(path: &str) -> EventRecord {
        EventRecord {
            task_id: TaskId::new(),
            timestamp_ms: 1,
            event: OrchestratorEvent::SubtaskReviewReady {
                subtask_id: SubtaskId::new(),
                description: "make chess".into(),
                tier: ModelTier::Standard,
                tokens_used: 10,
                token_usage: TokenUsage::estimated(10),
                cost: phonton_types::CostSummary::default(),
                diff_hunks: vec![DiffHunk {
                    file_path: PathBuf::from(path),
                    old_start: 1,
                    old_count: 1,
                    new_start: 1,
                    new_count: 2,
                    lines: vec![
                        DiffLine::Removed("print('Hello')".into()),
                        DiffLine::Added("print('Chess')".into()),
                        DiffLine::Context("return".into()),
                    ],
                }],
                verify_result: VerifyResult::Pass {
                    layer: VerifyLayer::Syntax,
                },
                provider: ProviderKind::OpenAiCompatible,
                model_name: "fixture".into(),
            },
        }
    }

    fn verify_fail_event(path: &str) -> EventRecord {
        EventRecord {
            task_id: TaskId::new(),
            timestamp_ms: 1,
            event: OrchestratorEvent::VerifyFail {
                subtask_id: SubtaskId::new(),
                layer: VerifyLayer::Syntax,
                errors: vec![format!(
                    "[python syntax] {path}:398: unterminated or invalid string"
                )],
                attempt: 1,
            },
        }
    }

    fn failed_state(reason: &str) -> GlobalState {
        GlobalState {
            task_status: TaskStatus::Failed {
                reason: reason.into(),
                failed_subtask: Some(SubtaskId::new()),
            },
            goal_contract: None,
            handoff_packet: None,
            active_workers: Vec::new(),
            tokens_used: 123,
            tokens_budget: None,
            estimated_naive_tokens: 1000,
            checkpoints: Vec::new(),
        }
    }

    #[derive(Clone, Default)]
    struct McpE2eProvider {
        calls: Arc<AtomicU64>,
    }

    #[async_trait]
    impl Provider for McpE2eProvider {
        async fn call(
            &self,
            _system: &str,
            user: &str,
            _slice_origins: &[phonton_types::SliceOrigin],
        ) -> Result<LLMResponse> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let content = if call == 0 {
                r#"MCP_TOOL_CALL {"server":"fixture","tool":"read_context","arguments":{"path":"README.md"}}"#
                    .to_string()
            } else {
                if !user.contains("fixture-value-from-mcp") {
                    return Err(anyhow::anyhow!(
                        "worker prompt did not include MCP tool result: {user}"
                    ));
                }
                "\
--- /dev/null
+++ b/src/mcp_fixture.rs
@@ -0,0 +1,3 @@
+pub fn mcp_fixture() -> &'static str {
+    \"fixture-value-from-mcp\"
+}
"
                .to_string()
            };

            Ok(LLMResponse {
                content,
                input_tokens: 10,
                output_tokens: 8,
                cached_tokens: 0,
                cache_creation_tokens: 0,
                provider: ProviderKind::OpenAiCompatible,
                model_name: "fake-mcp-e2e".into(),
            })
        }

        fn kind(&self) -> ProviderKind {
            ProviderKind::OpenAiCompatible
        }

        fn model(&self) -> String {
            "fake-mcp-e2e".into()
        }

        fn clone_box(&self) -> Box<dyn Provider> {
            Box::new(self.clone())
        }
    }

    fn compile_fake_mcp_server(dir: &Path) -> Result<PathBuf> {
        let src = dir.join("fake_mcp_server.rs");
        let exe = dir.join(if cfg!(windows) {
            "fake_mcp_server.exe"
        } else {
            "fake_mcp_server"
        });
        std::fs::write(&src, FAKE_MCP_SERVER_SOURCE)?;

        let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
        let output = Command::new(rustc).arg(&src).arg("-o").arg(&exe).output()?;
        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "failed to compile fake MCP server\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(exe)
    }

    const FAKE_MCP_SERVER_SOURCE: &str = r#"
use std::io::{self, BufRead, Write};

fn main() {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => break,
        };
        if line.contains("\"method\":\"notifications/initialized\"") {
            continue;
        }
        let id = extract_id(&line).unwrap_or_else(|| "null".to_string());
        let result = if line.contains("\"method\":\"initialize\"") {
            "{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{\"tools\":{}},\"serverInfo\":{\"name\":\"fixture\",\"version\":\"0.0.1\"}}"
        } else if line.contains("\"method\":\"tools/list\"") {
            "{\"tools\":[{\"name\":\"read_context\",\"title\":\"Read Context\",\"description\":\"returns fixture context\",\"inputSchema\":{\"type\":\"object\",\"properties\":{\"path\":{\"type\":\"string\"}}}}]}"
        } else if line.contains("\"method\":\"tools/call\"") {
            "{\"content\":[{\"type\":\"text\",\"text\":\"fixture-value-from-mcp\"}],\"isError\":false}"
        } else {
            "{\"content\":[{\"type\":\"text\",\"text\":\"unknown method\"}],\"isError\":true}"
        };
        writeln!(stdout, "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{}}}", id, result).unwrap();
        stdout.flush().unwrap();
    }
}

fn extract_id(line: &str) -> Option<String> {
    let marker = "\"id\":";
    let start = line.find(marker)? + marker.len();
    let rest = &line[start..];
    let id: String = rest
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect();
    if id.is_empty() { None } else { Some(id) }
}
"#;

    #[test]
    fn local_providers_can_run_without_api_keys() {
        let ollama = config::ProviderConfig {
            name: "ollama".into(),
            api_key: None,
            model: Some("llama3.2:3b".into()),
            account_id: None,
            base_url: None,
        };
        assert_eq!(provider_key_for_run(&ollama).as_deref(), Some(""));

        let custom = config::ProviderConfig {
            name: "openai-compatible".into(),
            api_key: None,
            model: Some("local-model".into()),
            account_id: None,
            base_url: Some("http://localhost:1234/v1".into()),
        };
        assert_eq!(provider_key_for_run(&custom).as_deref(), Some(""));
    }

    #[test]
    fn hosted_providers_still_require_api_keys() {
        assert!(provider_requires_key("openai"));
        assert!(provider_requires_key("anthropic"));
        assert!(provider_requires_key("cloudflare"));
        assert!(!provider_requires_key("ollama"));
        assert!(!provider_requires_key("openai-compatible"));
    }

    #[test]
    fn cloudflare_account_id_builds_workers_ai_base_url() {
        let cfg = make_api_provider_config(
            "cloudflare",
            "cf-token".into(),
            "@cf/moonshotai/kimi-k2.6".into(),
            Some("abc123".into()),
            None,
        )
        .expect("cloudflare config should build from account id");

        match cfg {
            ApiProviderConfig::OpenAiCompatible { name, base_url, .. } => {
                assert_eq!(name, "cloudflare");
                assert_eq!(
                    base_url,
                    "https://api.cloudflare.com/client/v4/accounts/abc123/ai/v1"
                );
            }
            other => panic!("unexpected provider config: {other:?}"),
        }
    }

    #[test]
    fn cloudflare_without_account_reports_config_failure() {
        let cfg = make_api_provider_config(
            "cloudflare",
            "cf-token".into(),
            "@cf/moonshotai/kimi-k2.6".into(),
            None,
            None,
        );
        assert!(cfg.is_none());
        assert!(provider_config_failure_message("cloudflare").contains("Account ID"));
    }

    #[tokio::test]
    async fn test_provider_missing_cloudflare_account_reports_account_id() {
        let err = test_provider(
            "cloudflare".into(),
            "cf-token".into(),
            "@cf/moonshotai/kimi-k2.6".into(),
            None,
            None,
        )
        .await
        .expect_err("missing Cloudflare account should fail before network call");

        assert!(err.contains("Account ID"));
        assert!(!err.contains("unknown provider"));
    }

    #[test]
    fn provider_config_template_replaces_model_without_losing_endpoint() {
        let template = make_api_provider_config(
            "cloudflare",
            "cf-token".into(),
            "@cf/moonshotai/kimi-k2.6".into(),
            Some("abc123".into()),
            None,
        )
        .expect("cloudflare template should build from account id");

        let updated = provider_config_with_model(&template, "tier-model".into());
        match updated {
            ApiProviderConfig::OpenAiCompatible {
                name,
                api_key,
                model,
                base_url,
            } => {
                assert_eq!(name, "cloudflare");
                assert_eq!(api_key, "cf-token");
                assert_eq!(model, "tier-model");
                assert_eq!(
                    base_url,
                    "https://api.cloudflare.com/client/v4/accounts/abc123/ai/v1"
                );
            }
            other => panic!("unexpected provider config: {other:?}"),
        }
    }

    #[test]
    fn typing_a_goal_appends_to_buffer() {
        let mut app = App::default();
        for c in "add fn foo".chars() {
            assert!(app.handle_key(key(c)).is_none());
        }
        assert_eq!(app.goal_prompt.display_text(), "add fn foo");
    }

    #[test]
    fn paste_event_collapses_long_prompt_content() {
        let mut app = App::default();
        app.handle_paste("line one\nline two");

        assert_eq!(app.goal_prompt.display_text(), "[paste: 2 lines, 17 chars]");
    }

    #[test]
    fn ctrl_v_requests_clipboard_paste() {
        let mut app = App::default();
        let intent = app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));

        assert_eq!(intent, Some(Intent::PasteClipboard));
    }

    #[test]
    fn pasted_api_key_redirects_to_settings_before_artifact_creation() {
        let mut app = App::default();

        app.handle_paste("sk-ant-FAKE_TEST_KEY_123456");

        assert_eq!(app.mode, Mode::Settings);
        assert_eq!(app.settings.active_field, SettingsField::ApiKey);
        assert!(app.goal_prompt.display_text().is_empty());
        assert!(app
            .settings
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("API key"));
    }

    #[test]
    fn bracketless_multiline_paste_burst_collapses_to_one_goal() {
        let mut keys = Vec::new();
        for ch in "# Benchmark Prompt".chars() {
            keys.push(key(ch));
        }
        keys.push(enter());
        keys.push(enter());
        for ch in "1. Do x".chars() {
            keys.push(key(ch));
        }
        keys.push(enter());
        for ch in "2. Do y".chars() {
            keys.push(key(ch));
        }

        let text = bracketless_paste_text(&keys).expect("burst should become paste text");
        let mut app = App::default();
        app.handle_paste(&text);

        assert!(app.goal_prompt.display_text().starts_with("[paste:"));
        assert!(app.goals.is_empty());
        assert!(matches!(
            app.handle_key(enter()),
            Some(Intent::QueueGoal(goal)) if goal.contains("# Benchmark Prompt")
        ));
        assert_eq!(app.goals.len(), 1);
    }

    #[test]
    fn short_typing_with_enter_is_not_treated_as_paste() {
        let keys = vec![key('g'), key('o'), enter()];

        assert!(bracketless_paste_text(&keys).is_none());
    }

    #[test]
    fn control_shortcuts_are_not_treated_as_paste() {
        let keys = vec![ctrl('v'), key('x'), enter(), key('y')];

        assert!(bracketless_paste_text(&keys).is_none());
    }

    #[test]
    fn multiline_paste_with_secret_is_blocked() {
        let mut app = App::default();

        app.handle_paste("goal context\nkey=sk-ant-FAKE_TEST_KEY_123456");

        assert!(app.goal_prompt.display_text().is_empty());
        assert!(app
            .command_notice
            .as_deref()
            .unwrap_or_default()
            .contains("blocked"));
    }

    #[test]
    fn paste_in_settings_updates_active_field() {
        let mut app = App {
            mode: Mode::Settings,
            ..App::default()
        };
        app.settings.active_field = SettingsField::ApiKey;

        app.handle_paste("sk-ant-FAKE_TEST_KEY_123456");

        assert_eq!(app.settings.api_key, "sk-ant-FAKE_TEST_KEY_123456");
    }

    #[test]
    fn run_command_submission_does_not_queue_goal() {
        let mut app = App::default();
        for ch in "/run cargo test".chars() {
            app.handle_key(key(ch));
        }

        let intent = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(intent, Some(Intent::RunCommand("/run cargo test".into())));
        assert!(app.goals.is_empty());
    }

    #[test]
    fn slash_settings_opens_settings_without_queueing_goal() {
        let mut app = App::default();
        for ch in "/settings".chars() {
            app.handle_key(key(ch));
        }

        let intent = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(intent, None);
        assert_eq!(app.mode, Mode::Settings);
        assert!(app.goals.is_empty());
    }

    #[test]
    fn slash_config_alias_opens_settings() {
        let mut app = App::default();
        for ch in "/config".chars() {
            app.handle_key(key(ch));
        }

        let intent = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(intent, None);
        assert_eq!(app.mode, Mode::Settings);
        assert!(app.goals.is_empty());
    }

    #[test]
    fn unknown_slash_command_is_not_queued_as_goal() {
        let mut app = App::default();
        for ch in "/settngs".chars() {
            app.handle_key(key(ch));
        }

        let intent = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(intent, None);
        assert!(app.goals.is_empty());
        assert!(app
            .settings
            .message
            .as_deref()
            .unwrap_or("")
            .contains("Unknown command"));
    }

    #[test]
    fn tab_completes_slash_command_prefix() {
        let mut app = App::default();
        for ch in "/sett".chars() {
            app.handle_key(key(ch));
        }

        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

        assert_eq!(app.goal_prompt.display_text(), "/settings");
    }

    #[test]
    fn slash_status_opens_visible_status_surface() {
        let mut app = App::default();
        for ch in "/status".chars() {
            app.handle_key(key(ch));
        }

        let intent = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(intent, None);
        assert_eq!(app.mode, Mode::Ask);
        assert!(app.ask_answer.as_deref().unwrap_or("").contains("Status"));
        assert!(app.goals.is_empty());
    }

    #[test]
    fn slash_context_opens_context_surface() {
        let mut app = App::default();
        app.record_prompt_manifest(phonton_types::PromptContextManifest {
            system_tokens: 10,
            user_goal_tokens: 5,
            memory_tokens: 3,
            attachment_tokens: 2,
            code_context_tokens: 0,
            repo_map_tokens: 0,
            omitted_code_tokens: 0,
            context_target_tokens: 3_500,
            attempt: 1,
            repair_attempt: false,
            target_exceeded: false,
            over_target_tokens: 0,
            mcp_tool_tokens: 1,
            retry_error_tokens: 0,
            total_estimated_tokens: 21,
            budget_limit: Some(120_000),
            compacted_tokens: 0,
            deduped_tokens: 0,
        });
        for ch in "/context".chars() {
            app.handle_key(key(ch));
        }

        let intent = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(intent, None);
        assert_eq!(app.mode, Mode::Ask);
        let answer = app.ask_answer.as_deref().unwrap_or("");
        assert!(answer.contains("Context"));
        assert!(answer.contains("total: 21"));
        assert!(app.goals.is_empty());
    }

    #[test]
    fn slash_compact_requests_context_compaction() {
        let mut app = App::default();
        app.goals.insert(0, GoalEntry::new("make chess".into()));
        app.goals[0].status = TaskStatus::Running {
            active_subtasks: Vec::new(),
            completed: 0,
            total: 1,
        };

        for ch in "/compact".chars() {
            app.handle_key(key(ch));
        }

        let intent = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(intent, Some(Intent::CompactContext { goal_index: 0 }));
        assert_eq!(app.mode, Mode::Ask);
        assert!(app.ask_answer.as_deref().unwrap_or("").contains("Compact"));
    }

    #[test]
    fn slash_stop_requests_selected_goal_cancel() {
        let mut app = App::default();
        app.goals.insert(0, GoalEntry::new("make chess".into()));
        app.goals[0].status = TaskStatus::Running {
            active_subtasks: Vec::new(),
            completed: 0,
            total: 1,
        };

        for ch in "/stop".chars() {
            app.handle_key(key(ch));
        }

        let intent = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(intent, Some(Intent::StopGoal { goal_index: 0 }));
        assert!(!matches!(app.goals[0].status, TaskStatus::Queued));
    }

    #[test]
    fn slash_permissions_set_updates_mode_and_requests_save() {
        let mut app = App::default();
        for ch in "/permissions set read-only".chars() {
            app.handle_key(key(ch));
        }

        let intent = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(intent, Some(Intent::SaveSettings));
        assert_eq!(
            app.settings.permission_mode,
            phonton_types::PermissionMode::ReadOnly
        );
        assert!(app
            .ask_answer
            .as_deref()
            .unwrap_or("")
            .contains("mode: read-only"));
    }

    #[test]
    fn slash_model_set_updates_model_and_requests_save() {
        let mut app = App::default();
        for ch in "/model set gpt-5-mini".chars() {
            app.handle_key(key(ch));
        }

        let intent = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(intent, Some(Intent::SaveSettings));
        assert_eq!(app.settings.model, "gpt-5-mini");
        assert_eq!(app.mode, Mode::Settings);
    }

    #[test]
    fn slash_command_drawer_renders_suggestions() {
        let backend = TestBackend::new(100, 28);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::default();
        for ch in "/sett".chars() {
            app.handle_key(key(ch));
        }

        terminal.draw(|f| render(f, &app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let dump: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(dump.contains("/settings"));
        assert!(dump.contains("provider"));
    }

    #[test]
    fn workspace_preflight_adds_npm_verification_and_run_plan() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("package.json"),
            r#"{"scripts":{"build":"vite build","test":"vitest","dev":"vite"}}"#,
        )
        .unwrap();
        let mut plan = single_task_plan("make chess".into(), Vec::new());

        apply_workspace_preflight(&mut plan, temp.path(), "make chess");

        let contract = plan.goal_contract.unwrap();
        assert!(contract.verify_plan.iter().any(|step| {
            step.command
                .as_ref()
                .map(|cmd| cmd.command == vec!["npm".to_string(), "run".into(), "build".into()])
                .unwrap_or(false)
        }));
        assert!(contract.verify_plan.iter().any(|step| {
            step.command
                .as_ref()
                .map(|cmd| cmd.command == vec!["npm".to_string(), "test".into()])
                .unwrap_or(false)
        }));
        assert!(contract
            .run_plan
            .iter()
            .any(|cmd| cmd.command == vec!["npm".to_string(), "run".into(), "dev".into()]));
    }

    #[test]
    fn workspace_preflight_defaults_stackless_chess_without_clarifying() {
        let temp = tempfile::tempdir().unwrap();
        let mut plan = single_task_plan("make chess".into(), Vec::new());

        apply_workspace_preflight(&mut plan, temp.path(), "make chess");

        let contract = plan.goal_contract.unwrap();
        assert!(contract.clarification_questions.is_empty());
        assert!(contract
            .assumptions
            .iter()
            .any(|assumption| assumption.contains("defaulting to a self-contained Python")));
        assert!(contract
            .likely_files
            .iter()
            .any(|path| path == &std::path::PathBuf::from("chess.py")));
    }

    #[test]
    fn stackless_chess_preflight_remains_dispatchable_in_goal_mode() {
        let temp = tempfile::tempdir().unwrap();
        let mut plan = single_task_plan("make chess".into(), Vec::new());

        apply_workspace_preflight(&mut plan, temp.path(), "make chess");

        let contract = plan.goal_contract.unwrap();
        assert!(contract.clarification_questions.is_empty());
        assert!(!contract.run_plan.is_empty());
        assert!(contract
            .verify_plan
            .iter()
            .any(|step| step.command.is_some()));
    }

    #[test]
    fn npm_chess_preflight_is_dispatchable_after_run_and_verify_plan() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("package.json"),
            r#"{"scripts":{"build":"vite build","test":"vitest","dev":"vite"}}"#,
        )
        .unwrap();
        let mut plan = single_task_plan("make chess".into(), Vec::new());

        apply_workspace_preflight(&mut plan, temp.path(), "make chess");

        let contract = plan.goal_contract.unwrap();
        assert!(contract.clarification_questions.is_empty());
        assert!(!contract.run_plan.is_empty());
        assert!(contract
            .verify_plan
            .iter()
            .any(|step| step.command.is_some()));
    }

    #[test]
    fn prompt_history_restores_last_submission_when_input_empty() {
        let mut app = App::default();
        for ch in "make chess".chars() {
            app.handle_key(key(ch));
        }
        let _ = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));

        assert_eq!(app.goal_prompt.display_text(), "make chess");
    }

    #[test]
    fn session_snapshot_carries_bounded_prompt_history() {
        let app = App {
            prompt_history: vec![
                "make chess".into(),
                "make chess".into(),
                "fix validation".into(),
            ],
            ..App::default()
        };

        let snapshot = app.to_session_snapshot("C:\\workspace".into(), 123);

        assert_eq!(
            snapshot.prompt_history,
            vec!["make chess".to_string(), "fix validation".to_string()]
        );
    }

    #[test]
    fn enter_queues_a_goal_and_clears_input() {
        let mut app = App::default();
        for c in "hello".chars() {
            app.handle_key(key(c));
        }
        let intent = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(intent, Some(Intent::QueueGoal("hello".into())));
        assert_eq!(app.goals.len(), 1);
        assert_eq!(app.goal_prompt.display_text(), "");
    }

    #[test]
    fn enter_in_task_mode_emits_direct_task_intent() {
        let mut app = App {
            mode: Mode::Task,
            ..App::default()
        };
        for c in "write one focused test".chars() {
            app.handle_key(key(c));
        }
        let intent = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            intent,
            Some(Intent::QueueTask("write one focused test".into()))
        );
        assert_eq!(app.goals.len(), 1);
    }

    #[test]
    fn enter_on_empty_is_a_noop() {
        let mut app = App::default();
        let r = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(r.is_none());
        assert_eq!(app.goals.len(), 0);
    }

    #[test]
    fn goal_mentions_attach_text_and_images() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let src = temp.path().join("src");
        std::fs::create_dir(&src)?;
        std::fs::write(src.join("lib.rs"), "pub fn old() {}\n")?;
        std::fs::write(temp.path().join("screen.png"), [0x89, b'P', b'N', b'G'])?;

        let attachments =
            prepare_goal_attachments("fix @src/lib.rs based on @screen.png", temp.path());

        assert_eq!(attachments.len(), 2);
        let text = attachments
            .iter()
            .find(|a| a.path.as_path() == Path::new("src/lib.rs"))
            .expect("text attachment");
        assert_eq!(text.kind, PromptAttachmentKind::Text);
        assert!(text.text.as_deref().unwrap_or("").contains("old"));

        let image = attachments
            .iter()
            .find(|a| a.path.as_path() == Path::new("screen.png"))
            .expect("image attachment");
        assert_eq!(image.kind, PromptAttachmentKind::Image);
        assert_eq!(image.mime_type.as_deref(), Some("image/png"));
        assert!(image.data_base64.is_some());
        Ok(())
    }

    #[test]
    fn quoted_goal_mentions_support_spaces() -> Result<()> {
        let temp = tempfile::tempdir()?;
        std::fs::write(temp.path().join("notes file.md"), "# Notes\n")?;

        let attachments =
            prepare_goal_attachments("use @\"notes file.md\" while editing", temp.path());

        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].path, PathBuf::from("notes file.md"));
        assert!(attachments[0]
            .text
            .as_deref()
            .unwrap_or("")
            .contains("Notes"));
        Ok(())
    }

    #[test]
    fn mcp_approval_enter_approves_and_removes_prompt() {
        let mut app = App::default();
        app.push_mcp_approval(approval_prompt(42));

        let intent = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            intent,
            Some(Intent::ResolveMcpApproval {
                approval_id: 42,
                approved: true
            })
        );
        assert!(app.pending_mcp_approvals.is_empty());
    }

    #[test]
    fn mcp_approval_esc_denies_without_quitting() {
        let mut app = App::default();
        app.push_mcp_approval(approval_prompt(7));

        let intent = app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(
            intent,
            Some(Intent::ResolveMcpApproval {
                approval_id: 7,
                approved: false
            })
        );
        assert!(!app.should_quit);
    }

    #[test]
    fn mcp_approval_arrows_select_between_prompts() {
        let mut app = App::default();
        app.push_mcp_approval(approval_prompt(1));
        app.push_mcp_approval(approval_prompt(2));
        assert_eq!(app.mcp_approval_selected, 1);

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        let intent = app.handle_key(key('n'));
        assert_eq!(
            intent,
            Some(Intent::ResolveMcpApproval {
                approval_id: 1,
                approved: false
            })
        );
        assert_eq!(app.pending_mcp_approvals.len(), 1);
        assert_eq!(app.pending_mcp_approvals[0].id, 2);
    }

    #[tokio::test]
    async fn worker_mcp_e2e_uses_tui_approval_and_verified_diff() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let server_exe = compile_fake_mcp_server(temp.path())?;

        let (approval_tx, mut approval_rx) = mpsc::channel::<LoopEvent>(8);
        let approval_driver = tokio::spawn(async move {
            let mut app = App::default();
            let mut approved = Vec::new();
            while let Some(event) = approval_rx.recv().await {
                let LoopEvent::McpApprovalRequested { prompt, reply_tx } = event else {
                    continue;
                };
                let prompt_id = prompt.id;
                app.push_mcp_approval(prompt);
                let intent = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                match intent {
                    Some(Intent::ResolveMcpApproval {
                        approval_id,
                        approved: true,
                    }) => {
                        assert_eq!(approval_id, prompt_id);
                        approved.push(approval_id);
                        let _ = reply_tx.send(McpApprovalDecision::Approved);
                    }
                    other => panic!("expected MCP approval intent, got {other:?}"),
                }
            }
            approved
        });

        let server = McpServerDefinition {
            id: ExtensionId::new("fixture"),
            name: "Fixture MCP".into(),
            source: ExtensionSource::Workspace,
            transport: McpTransport::Stdio {
                command: server_exe.display().to_string(),
                args: Vec::new(),
            },
            trust: TrustLevel::ReadOnlyTool,
            permissions: vec![Permission::FsReadOutsideWorkspace],
            applies_to: AppliesTo::default(),
            env: Vec::new(),
            enabled: true,
        };
        let runtime = Arc::new(
            phonton_mcp::McpRuntime::new(
                vec![server],
                ExecutionGuard::new(temp.path().to_path_buf()),
            )
            .with_approver(Arc::new(TuiMcpApprover::new(0, approval_tx.clone()))),
        );
        drop(approval_tx);

        let subtask = Subtask {
            id: SubtaskId::new(),
            description: "Use MCP fixture context and add a Rust helper".into(),
            model_tier: ModelTier::Cheap,
            dependencies: Vec::new(),
            attachments: Vec::new(),
            status: SubtaskStatus::Queued,
        };
        let provider = McpE2eProvider::default();
        let provider_calls = Arc::clone(&provider.calls);
        let worker = phonton_worker::Worker::new(
            Box::new(provider),
            ExecutionGuard::new(temp.path().to_path_buf()),
        )
        .with_mcp_runtime(Arc::clone(&runtime));

        let result = worker.execute(subtask, Vec::new()).await?;
        drop(worker);
        drop(runtime);
        let approvals = tokio::time::timeout(Duration::from_secs(5), approval_driver).await??;

        assert!(
            matches!(result.status, SubtaskStatus::Done { .. }),
            "worker should finish after MCP result, got {:?}",
            result.status
        );
        assert!(
            matches!(result.verify_result, VerifyResult::Pass { .. }),
            "final diff must be verified, got {:?}",
            result.verify_result
        );
        assert_eq!(result.diff_hunks.len(), 1);
        assert_eq!(
            result.diff_hunks[0].file_path,
            PathBuf::from("src/mcp_fixture.rs")
        );
        assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
        assert!(
            approvals.len() >= 2,
            "expected tool and server/start approvals, got {approvals:?}"
        );
        Ok(())
    }

    #[test]
    fn ctrl_semicolon_toggles_ask_mode() {
        let mut app = App::default();
        assert_eq!(app.mode, Mode::Goal);
        app.handle_key(ctrl(';'));
        assert_eq!(app.mode, Mode::Ask);
        app.handle_key(ctrl(';'));
        assert_eq!(app.mode, Mode::Goal);
    }

    #[test]
    fn ask_enter_emits_ask_intent_without_touching_goals() {
        let mut app = App::default();
        app.goals.push(GoalEntry::new("parent goal".into()));
        app.handle_key(ctrl(';'));
        for c in "what now".chars() {
            app.handle_key(key(c));
        }
        let intent = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(intent, Some(Intent::Ask("what now".into())));
        // Ask must not pollute the existing goal list.
        assert_eq!(app.goals.len(), 1);
        assert_eq!(app.goals[0].description, "parent goal");
    }

    #[test]
    fn slash_ask_question_emits_ask_intent_without_queueing_goal() {
        let mut app = App::default();
        for c in "/ask why tokens?".chars() {
            app.handle_key(key(c));
        }

        let intent = app.handle_key(enter());

        assert_eq!(intent, Some(Intent::Ask("why tokens?".into())));
        assert_eq!(app.mode, Mode::Ask);
        assert!(app.ask_pending);
        assert!(app.goals.is_empty());
    }

    #[test]
    fn ask_answer_scrolls_when_prompt_is_empty() {
        let mut app = App {
            mode: Mode::Ask,
            ask_answer: Some(
                (0..40)
                    .map(|n| format!("line {n}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            ..App::default()
        };

        app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(app.ask_scroll, 12);
        app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(app.ask_scroll, 0);
    }

    #[test]
    fn ask_rich_text_renderer_keeps_markdown_shape() {
        let lines = render_rich_text_lines("# Head\n- item\n```rust\nfn main() {}\n```");

        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0].spans[0].content, "# Head");
        assert_eq!(lines[1].spans[0].content, "- item");
        assert_eq!(lines[3].spans[0].content, "fn main() {}");
    }

    #[test]
    fn esc_from_ask_returns_to_goal_without_quitting() {
        let mut app = App::default();
        app.handle_key(ctrl(';'));
        assert_eq!(app.mode, Mode::Ask);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Goal);
        assert!(!app.should_quit);
    }

    #[test]
    fn esc_from_goal_opens_quit_confirmation() {
        let mut app = App::default();
        let r = app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(r, None);
        assert!(!app.should_quit);
        assert!(app.quit_confirmation_open);
    }

    #[test]
    fn ctrl_c_opens_quit_confirmation_without_quitting() {
        let mut app = App::default();
        let r = app.handle_key(ctrl('c'));
        assert_eq!(r, None);
        assert!(!app.should_quit);
        assert!(app.quit_confirmation_open);
    }

    #[test]
    fn quit_confirmation_enter_emits_quit() {
        let mut app = App::default();
        app.handle_key(ctrl('c'));
        let r = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(r, Some(Intent::Quit));
        assert!(app.should_quit);
        assert!(!app.quit_confirmation_open);
    }

    #[test]
    fn quit_confirmation_esc_cancels() {
        let mut app = App::default();
        app.handle_key(ctrl('c'));
        let r = app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(r, None);
        assert!(!app.should_quit);
        assert!(!app.quit_confirmation_open);
    }

    #[test]
    fn resume_flag_parses_to_tui_launch() {
        let args = vec!["-r".to_string()];
        assert_eq!(
            launch_options_from_args(&args),
            Some(LaunchOptions {
                resume_last_session: true
            })
        );
        let args = vec!["--resume".to_string()];
        assert_eq!(
            launch_options_from_args(&args),
            Some(LaunchOptions {
                resume_last_session: true
            })
        );
    }

    #[test]
    fn arrow_keys_move_selection() {
        let mut app = App::default();
        app.goals
            .extend(["a", "b", "c"].iter().map(|s| GoalEntry::new((*s).into())));
        app.selected = 0;
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.selected, 1);
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.selected, 2);
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.selected, 2); // clamp
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn alt_goal_shortcuts_switch_even_with_prompt_text() {
        let mut app = App::default();
        app.goals
            .extend(["a", "b", "c"].iter().map(|s| GoalEntry::new((*s).into())));
        app.selected = 1;
        app.goal_prompt.set_text("draft goal");

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::ALT));
        assert_eq!(app.selected, 2);
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
        assert_eq!(app.selected, 1);
        app.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::ALT));
        assert_eq!(app.selected, 0);
        assert_eq!(app.goal_prompt.display_text(), "draft goal");
    }

    #[test]
    fn goal_switcher_selects_filtered_goal() {
        let mut app = App::default();
        app.goals.extend(
            ["make chess", "write docs"]
                .iter()
                .map(|s| GoalEntry::new((*s).into())),
        );

        for ch in "/goals".chars() {
            app.handle_key(key(ch));
        }
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            None
        );
        assert!(app.goal_switcher.open);

        for ch in "docs".chars() {
            app.handle_key(key(ch));
        }
        assert_eq!(app.goal_switcher.filtered_indices(&app.goals), vec![1]);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            None
        );
        assert_eq!(app.selected, 1);
        assert!(!app.goal_switcher.open);
    }

    #[test]
    fn focus_defaults_to_code_for_review_hunks_and_can_cycle() {
        let mut app = App::default();
        app.goals.push(GoalEntry::new("make chess".into()));
        app.apply_event(0, review_ready_event("chess.py"));

        assert_eq!(app.active_focus_view_for_current_goal(), FocusView::Code);
        app.handle_key(key('f'));
        assert_eq!(app.focus_view, FocusView::Commands);
    }

    #[test]
    fn failed_goal_defaults_to_problems_focus() {
        let mut app = App::default();
        app.goals.push(GoalEntry::new("make chess".into()));
        app.apply_event(
            0,
            EventRecord {
                task_id: TaskId::new(),
                timestamp_ms: 1,
                event: OrchestratorEvent::Thinking {
                    subtask_id: SubtaskId::new(),
                    model_name: "kimi-k2.6".into(),
                },
            },
        );
        app.apply_event(0, verify_fail_event("chess.py"));
        app.apply_state(0, failed_state("syntax verification failed"));

        assert_eq!(
            app.active_focus_view_for_current_goal(),
            FocusView::Problems
        );
        assert!(app.focus_text().contains("[python syntax] chess.py"));
        assert!(app.focus_text().contains("Kimi was used"));
    }

    #[test]
    fn problems_shortcuts_open_and_retry_failed_goal() {
        let mut app = App::default();
        app.goals.push(GoalEntry::new("make chess".into()));
        app.apply_event(0, verify_fail_event("chess.py"));
        app.apply_state(0, failed_state("syntax verification failed"));
        app.focus_view = FocusView::Receipt;

        assert_eq!(app.handle_key(key('p')), None);
        assert_eq!(app.focus_view, FocusView::Problems);

        match app.handle_key(key('r')) {
            Some(Intent::QueueGoal(prompt)) => {
                assert!(prompt.contains("Repair the previous failed Phonton goal"));
                assert!(prompt.contains("[python syntax] chess.py"));
            }
            other => panic!("expected retry repair goal, got {other:?}"),
        }
    }

    #[test]
    fn code_focus_text_preserves_diff_markers() {
        let mut app = App::default();
        app.goals.push(GoalEntry::new("make chess".into()));
        app.apply_event(0, review_ready_event("chess.py"));

        let text = app.focus_text();

        assert!(text.contains("Code"));
        assert!(text.contains("chess.py"));
        assert!(text.contains("+print('Chess')"));
        assert!(text.contains("-print('Hello')"));
    }

    #[test]
    fn page_keys_scroll_active_code_focus_when_prompt_empty() {
        let mut app = App::default();
        app.goals.push(GoalEntry::new("make chess".into()));
        app.apply_event(0, review_ready_event("chess.py"));
        app.focus_view = FocusView::Code;

        app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert!(app.focus_scroll > 0);

        app.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(app.focus_scroll, 0);
    }

    #[test]
    fn mouse_wheel_scrolls_active_focus_surface() {
        let mut app = App::default();
        app.goals.push(GoalEntry::new("make chess".into()));
        app.apply_event(0, review_ready_event("chess.py"));
        app.focus_view = FocusView::Code;

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 10,
            row: 10,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.focus_scroll > 0);

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 10,
            row: 10,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.focus_scroll, 0);
    }

    #[test]
    fn artifact_chip_style_is_colorful_and_stable() {
        let paste = artifact_chip_color("[paste: 30 lines, 1.5k chars]");
        let image = artifact_chip_color("[image: board.png]");

        assert_eq!(paste, artifact_chip_color("[paste: 30 lines, 1.5k chars]"));
        assert_ne!(paste, Color::White);
        assert_ne!(image, Color::White);
    }

    #[test]
    fn copy_and_rerun_build_expected_intents() {
        let mut app = App::default();
        app.goals.push(GoalEntry::new("make chess".into()));
        app.apply_event(0, review_ready_event("chess.py"));
        app.push_command_run(CommandRunSummary {
            command: "python chess.py".into(),
            exit_code: Some(0),
            stdout_preview: "ok".into(),
            stderr_preview: String::new(),
            duration_ms: Some(42),
        });

        for ch in "/copy".chars() {
            app.handle_key(key(ch));
        }
        match app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)) {
            Some(Intent::CopyFocus(text)) => assert!(text.contains("chess.py")),
            other => panic!("expected copy intent, got {other:?}"),
        }

        for ch in "/rerun".chars() {
            app.handle_key(key(ch));
        }
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Intent::RunCommand("/run python chess.py".into()))
        );
    }

    #[test]
    fn command_completion_does_not_move_code_focus() {
        let mut app = App::default();
        app.goals.push(GoalEntry::new("make chess".into()));
        app.apply_event(0, review_ready_event("chess.py"));

        app.push_command_run(CommandRunSummary {
            command: "python chess.py".into(),
            exit_code: Some(0),
            stdout_preview: "ok".into(),
            stderr_preview: String::new(),
            duration_ms: Some(42),
        });

        assert_eq!(app.active_focus_view_for_current_goal(), FocusView::Code);
    }

    #[test]
    fn focus_text_payloads_cover_code_commands_and_log() {
        let mut app = App::default();
        app.goals.push(GoalEntry::new("make chess".into()));
        app.apply_event(0, review_ready_event("chess.py"));
        app.push_command_run(CommandRunSummary {
            command: "python chess.py".into(),
            exit_code: Some(0),
            stdout_preview: "ok".into(),
            stderr_preview: String::new(),
            duration_ms: Some(42),
        });

        app.focus_view = FocusView::Receipt;
        assert!(app.focus_text().contains("Receipt"));

        app.focus_view = FocusView::Problems;
        assert!(app.focus_text().contains("Problems"));

        app.focus_view = FocusView::Code;
        assert!(app.focus_text().contains("chess.py"));

        app.focus_view = FocusView::Commands;
        assert!(app.focus_text().contains("python chess.py"));

        app.focus_view = FocusView::Log;
        assert!(app.focus_text().contains("review-ready"));
    }

    #[test]
    fn savings_line_shows_percent_when_baseline_known() {
        let s = GlobalState {
            task_status: TaskStatus::Queued,
            goal_contract: None,
            handoff_packet: None,
            active_workers: Vec::new(),
            tokens_used: 200,
            tokens_budget: None,
            estimated_naive_tokens: 1000,
            checkpoints: Vec::new(),
        };
        let line = render_savings_line(Some(&s));
        assert!(line.contains("200"));
        assert!(line.contains("1000"));
        assert!(line.contains("80%"));
    }

    #[test]
    fn savings_line_handles_missing_state() {
        let line = render_savings_line(None);
        assert!(line.contains("baseline"));
    }

    #[test]
    fn app_session_snapshot_restores_goals_and_inputs() {
        let mut app = App::default();
        let task_id = TaskId::new();
        let snapshot = phonton_types::SessionSnapshot {
            workspace: "C:\\workspace".into(),
            saved_at: 123,
            selected_goal: 0,
            goal_input: "draft follow-up".into(),
            ask_input: "summarize state".into(),
            ask_answer: Some("resume support is pending".into()),
            prompt_history: vec!["ship session resume".into()],
            best_savings_pct: Some(80),
            goals: vec![phonton_types::SessionGoalSnapshot {
                description: "ship session resume".into(),
                status: TaskStatus::Queued,
                state: None,
                task_id,
                flight_log: Vec::new(),
            }],
            totals: phonton_types::SessionTotals::default(),
        };

        app.restore_session_snapshot(snapshot);

        assert_eq!(app.goals.len(), 1);
        assert_eq!(app.goals[0].description, "ship session resume");
        assert_eq!(app.goals[0].task_id, task_id);
        assert_eq!(app.goal_prompt.display_text(), "draft follow-up");
        assert_eq!(app.ask_prompt.display_text(), "summarize state");
        assert_eq!(app.ask_answer.as_deref(), Some("resume support is pending"));
        assert_eq!(app.prompt_history, vec!["ship session resume"]);
        assert_eq!(app.best_savings_pct, Some(80));
    }

    #[test]
    fn settings_sync_persists_cloudflare_account_id() {
        let mut cfg = config::Config::default();
        let mut settings = SettingsState::new(&cfg);
        settings.provider = "cloudflare".into();
        settings.model = "@cf/moonshotai/kimi-k2.6".into();
        settings.api_key = "cf-token".into();
        settings.account_id = "account-123".into();
        settings.base_url.clear();
        settings.max_tokens = "12345".into();
        settings.max_usd_cents = "99".into();

        apply_settings_to_config(&settings, &mut cfg);

        assert_eq!(cfg.provider.name, "cloudflare");
        assert_eq!(
            cfg.provider.model.as_deref(),
            Some("@cf/moonshotai/kimi-k2.6")
        );
        assert_eq!(cfg.provider.api_key.as_deref(), Some("cf-token"));
        assert_eq!(cfg.provider.account_id.as_deref(), Some("account-123"));
        assert_eq!(cfg.provider.base_url, None);
        assert_eq!(cfg.budget.max_tokens, Some(12345));
        assert_eq!(cfg.budget.max_usd_cents, Some(99));
    }

    #[test]
    fn worker_label_hides_memory_preamble() {
        let description =
            "# Prior context from memory\n- Honour previous decisions\n\nImplement chess board";
        assert_eq!(
            worker_display_description(description),
            "Implement chess board"
        );
        assert_eq!(
            worker_display_description("Write integration tests\nwith details"),
            "Write integration tests"
        );
    }

    #[test]
    fn session_totals_sum_tokens_and_estimated_savings() {
        let mut app = App::default();
        app.goals.push(GoalEntry {
            description: "done".into(),
            status: TaskStatus::Done {
                tokens_used: 250,
                wall_time_ms: 1,
            },
            state: Some(GlobalState {
                task_status: TaskStatus::Done {
                    tokens_used: 250,
                    wall_time_ms: 1,
                },
                goal_contract: None,
                handoff_packet: None,
                active_workers: Vec::new(),
                tokens_used: 250,
                tokens_budget: None,
                estimated_naive_tokens: 1_000,
                checkpoints: Vec::new(),
            }),
            task_id: TaskId::new(),
            flight_log: Vec::new(),
            checkpoint_cursor: None,
        });
        app.goals.push(GoalEntry {
            description: "failed".into(),
            status: TaskStatus::Failed {
                reason: "verify failed".into(),
                failed_subtask: None,
            },
            state: Some(GlobalState {
                task_status: TaskStatus::Failed {
                    reason: "verify failed".into(),
                    failed_subtask: None,
                },
                goal_contract: None,
                handoff_packet: None,
                active_workers: Vec::new(),
                tokens_used: 100,
                tokens_budget: None,
                estimated_naive_tokens: 300,
                checkpoints: Vec::new(),
            }),
            task_id: TaskId::new(),
            flight_log: Vec::new(),
            checkpoint_cursor: None,
        });
        app.best_savings_pct = Some(75);

        let totals = app.session_totals();

        assert_eq!(totals.goals, 2);
        assert_eq!(totals.completed, 1);
        assert_eq!(totals.failed, 1);
        assert_eq!(totals.tokens_used, 350);
        assert_eq!(totals.naive_baseline_tokens, 1_300);
        assert_eq!(totals.estimated_tokens_saved, 950);
        assert_eq!(totals.best_savings_pct, Some(75));
    }

    #[test]
    fn exit_receipt_includes_estimated_token_savings() {
        let receipt = render_exit_receipt(&phonton_types::SessionTotals {
            goals: 2,
            completed: 1,
            failed: 0,
            reviewing: 1,
            tokens_used: 350,
            naive_baseline_tokens: 1_300,
            estimated_tokens_saved: 950,
            best_savings_pct: Some(75),
        });

        assert!(receipt.contains("Session saved"));
        assert!(receipt.contains("tokens used: 350"));
        assert!(receipt.contains("estimated saved vs naive: 950"));
        assert!(receipt.contains("best savings: 75%"));
    }

    #[test]
    fn renders_without_panicking_on_empty_state() {
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = App::default();
        terminal.draw(|f| render(f, &app)).unwrap();
    }

    #[test]
    fn splash_logo_is_compact_and_shadowed() {
        let max_width = LOGO.iter().map(|row| char_count(row)).max().unwrap_or(0);
        assert!(max_width <= LOGO_WIDTH_THRESHOLD as usize);
        assert!(
            LOGO[0].contains("██████╗"),
            "logo should use the standard ANSI Shadow wordmark"
        );
        assert!(
            LOGO.last().unwrap_or(&"").contains("░▒▓"),
            "logo should keep the soft glow strip"
        );
    }

    #[test]
    fn renders_shadow_logo_on_wide_splash() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = App::default();
        terminal.draw(|f| render(f, &app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let dump: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(dump.contains("██████"));
        assert!(dump.contains("╚═════╝"));
        assert!(dump.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))));
    }

    #[test]
    fn wide_splash_logo_is_stable_across_ticks() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::default();
        terminal.draw(|f| render(f, &app)).unwrap();
        let first = format!("{:?}", terminal.backend().buffer());

        app.spinner_frame = 64;
        terminal.draw(|f| render(f, &app)).unwrap();
        let second = format!("{:?}", terminal.backend().buffer());

        assert_eq!(first, second);
    }

    #[test]
    fn renders_version_on_compact_header() {
        let backend = TestBackend::new(64, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = App::default();
        terminal.draw(|f| render(f, &app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let dump: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(dump.contains("agentic dev environment"));
        assert!(dump.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))));
    }

    #[test]
    fn renders_mcp_approval_overlay() {
        let backend = TestBackend::new(100, 28);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::default();
        app.push_mcp_approval(approval_prompt(9));

        terminal.draw(|f| render(f, &app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let dump: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(dump.contains("MCP Approval"));
        assert!(dump.contains("read_file"));
        assert!(dump.contains("Enter/Y approve"));
    }

    #[test]
    fn quit_confirmation_renders_session_summary() {
        let backend = TestBackend::new(120, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App {
            quit_confirmation_open: true,
            best_savings_pct: Some(75),
            ..App::default()
        };
        app.goals.push(GoalEntry {
            description: "done".into(),
            status: TaskStatus::Done {
                tokens_used: 250,
                wall_time_ms: 1,
            },
            state: Some(GlobalState {
                task_status: TaskStatus::Done {
                    tokens_used: 250,
                    wall_time_ms: 1,
                },
                goal_contract: None,
                handoff_packet: None,
                active_workers: Vec::new(),
                tokens_used: 250,
                tokens_budget: None,
                estimated_naive_tokens: 1000,
                checkpoints: Vec::new(),
            }),
            task_id: TaskId::new(),
            flight_log: Vec::new(),
            checkpoint_cursor: None,
        });

        terminal.draw(|f| render(f, &app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let dump: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(dump.contains("End Session"));
        assert!(dump.contains("Goals"));
        assert!(dump.contains("Tokens"));
        assert!(dump.contains("Efficiency"));
        assert!(dump.contains("phonton -r"));
    }

    #[test]
    fn detects_real_api_keys_but_not_goals() {
        // Provider-prefix keys must be caught.
        assert!(looks_like_api_key("sk-ant-FAKE_TEST_KEY_123456"));
        assert!(looks_like_api_key("AIzaFAKE_TEST_KEY_1234567890"));
        assert!(looks_like_api_key("sk-proj-FAKE_TEST_KEY_123456"));
        assert!(looks_like_api_key("xai-FAKE_TEST_KEY_123456"));
        assert!(looks_like_api_key("gsk_FAKE_TEST_KEY_123456"));
        assert!(looks_like_api_key("key_FAKE_TEST_KEY_123456"));

        // Plausible goals must NOT be caught.
        assert!(!looks_like_api_key("make a chess game"));
        assert!(!looks_like_api_key("refactor the parser"));
        assert!(!looks_like_api_key("a")); // too short, not key-shaped
        assert!(!looks_like_api_key("hello"));
        // Single-word names that aren't keys (no digits OR too short).
        assert!(!looks_like_api_key("README.md"));
        assert!(!looks_like_api_key("CamelCase"));
    }

    #[test]
    fn enter_with_api_key_redirects_to_settings() {
        let mut app = App::default();
        for c in "sk-ant-FAKE_TEST_KEY_123456".chars() {
            app.handle_key(key(c));
        }
        let intent = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(intent.is_none(), "must not queue an API key as a goal");
        assert_eq!(app.goals.len(), 0, "no goal should be queued");
        assert_eq!(app.mode, Mode::Settings, "should jump to Settings");
        assert!(
            app.settings
                .message
                .as_deref()
                .unwrap_or("")
                .contains("API key"),
            "user-facing toast should explain why"
        );
    }

    #[test]
    fn renders_handoff_packet_on_review_ready() {
        let backend = TestBackend::new(120, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::default();
        app.goals.push(GoalEntry::new("make chess".into()));
        app.apply_state(
            0,
            GlobalState {
                task_status: TaskStatus::Reviewing {
                    tokens_used: 240,
                    estimated_savings_tokens: 760,
                },
                goal_contract: None,
                handoff_packet: Some(HandoffPacket {
                    task_id: TaskId::new(),
                    goal: "make chess".into(),
                    headline: "Review ready: 1 file(s), 1 verified subtask(s)".into(),
                    changed_files: vec![phonton_types::ChangedFileSummary {
                        path: PathBuf::from("chess.py"),
                        added_lines: 42,
                        removed_lines: 0,
                        summary: "created chess scaffold".into(),
                    }],
                    generated_artifacts: Vec::new(),
                    diff_stats: phonton_types::DiffStats {
                        files_changed: 1,
                        added_lines: 42,
                        removed_lines: 0,
                    },
                    verification: phonton_types::VerifyReport {
                        passed: vec!["created chess scaffold passed syntax".into()],
                        findings: Vec::new(),
                        skipped: vec!["No explicit test layer was recorded.".into()],
                    },
                    run_commands: Vec::new(),
                    known_gaps: vec!["No run command was inferred yet.".into()],
                    review_actions: Vec::new(),
                    rollback_points: Vec::new(),
                    token_usage: TokenUsage::estimated(240),
                    influence: phonton_types::InfluenceSummary::default(),
                }),
                active_workers: Vec::new(),
                tokens_used: 240,
                tokens_budget: None,
                estimated_naive_tokens: 1000,
                checkpoints: Vec::new(),
            },
        );

        terminal.draw(|f| render(f, &app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let dump: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(dump.contains("Result"));
        assert!(dump.contains("Changed files"));
        assert!(dump.contains("chess.py"));
        assert!(dump.contains("Known gaps"));
    }

    #[test]
    fn trust_demo_explains_contract_verification_receipt_and_memory() {
        let demo = render_trust_demo();

        assert!(demo.contains("GoalContract"));
        assert!(demo.contains("Verification Caught"));
        assert!(demo.contains("Review Receipt"));
        assert!(demo.contains("Memory Prompt"));
        assert!(demo.contains("phonton plan"));
    }

    #[test]
    fn renders_failed_goal_reason() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::default();
        app.goals.push(GoalEntry::new("make chess".into()));
        app.apply_state(
            0,
            GlobalState {
                task_status: TaskStatus::Failed {
                    reason: "Cloudflare requires an Account ID".into(),
                    failed_subtask: None,
                },
                goal_contract: None,
                handoff_packet: None,
                active_workers: Vec::new(),
                tokens_used: 0,
                tokens_budget: None,
                estimated_naive_tokens: 35000,
                checkpoints: Vec::new(),
            },
        );

        terminal.draw(|f| render(f, &app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let dump: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(dump.contains("Failure"));
        assert!(dump.contains("Cloudflare requires an Account ID"));
    }

    #[test]
    fn renders_with_active_goal_and_savings() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::default();
        app.goals.push(GoalEntry::new("add function foo".into()));
        app.apply_state(
            0,
            GlobalState {
                task_status: TaskStatus::Running {
                    active_subtasks: Vec::new(),
                    completed: 1,
                    total: 2,
                },
                goal_contract: None,
                handoff_packet: None,
                active_workers: Vec::new(),
                tokens_used: 150,
                tokens_budget: None,
                estimated_naive_tokens: 500,
                checkpoints: Vec::new(),
            },
        );
        terminal.draw(|f| render(f, &app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let dump: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(dump.contains("add function foo"));
        // Rendered savings line shows "vs Σ <baseline>" — match the live wording.
        assert!(dump.contains("vs Σ 500") || dump.contains("baseline: 500"));
    }
}
