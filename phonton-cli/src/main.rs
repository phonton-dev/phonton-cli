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

mod config;
mod doctor;
mod extensions_cli;
mod mcp_cli;
mod memory_cli;
mod plan_preview;
mod review;
mod trust;

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use phonton_diff::DiffApplier;
use phonton_extensions::{load_extensions, DiagnosticSeverity, ExtensionLoadOptions, ExtensionSet};
use phonton_mcp::{McpApprovalDecision, McpApprovalRequest, McpApprover};
use phonton_orchestrator::{BudgetGuard, Orchestrator, WorkerDispatcher};
use phonton_planner::{decompose_with_memory, Goal};
use phonton_providers::{
    discover_models, pick_default_from_list, provider_for, select_best_working_model, Provider,
};
use phonton_sandbox::{ExecutionGuard, Sandbox};
use phonton_store::{Store, TaskRecord};
use phonton_types::{
    BudgetLimits, CoverageSummary, DiffHunk, DiffLine, EventRecord, ExtensionId, GlobalState,
    MemoryRecord, ModelPricing, ModelTier, OrchestratorEvent, OrchestratorMessage, Permission,
    PlannerOutput, ProviderConfig as ApiProviderConfig, ProviderKind, Subtask, SubtaskId,
    SubtaskResult, SubtaskStatus, TaskId, TaskStatus, TokenUsage, VerifyLayer, VerifyResult,
};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

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

/// Four-stop animated logo palette: violet → pink → electric blue → cyan,
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
    // Two highlight waves traveling at different speeds give a richer shimmer
    // than a single pass, like light playing across a curved surface.
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
                let color = grad(LOGO_SHADOW, base_color, (body + glow).clamp(0.0, 0.9));
                Style::default().fg(color)
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
/// way through with the cyan→violet gradient.
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
            message: None,
            picker_open: false,
            picker: ModelPickerState::default(),
            model_ok: None,
        }
    }
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
    /// Goal-bar input buffer.
    pub goal_input: String,
    /// Ask-mode input buffer, preserved across mode toggles.
    pub ask_input: String,
    /// Most recent ask-mode answer, for display in the side panel.
    pub ask_answer: Option<String>,
    /// True while an ask-mode provider call is in flight; drives the
    /// thinking spinner in the Ask panel.
    pub ask_pending: bool,
    /// When `true`, the render loop exits on the next frame.
    pub should_quit: bool,
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
    /// Caret position (in chars) inside `goal_input`.
    pub goal_cursor: usize,
    /// Caret position (in chars) inside `ask_input`.
    pub ask_cursor: usize,
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
    /// Local index/Nexus status shown in the ambient system strip.
    pub nexus_status: NexusStatus,
    /// Path of the SQLite store backing this session.
    pub store_path: Option<std::path::PathBuf>,
    /// MCP approval requests awaiting an explicit user decision.
    pub pending_mcp_approvals: Vec<PendingMcpApproval>,
    /// Cursor into `pending_mcp_approvals` when more than one request is queued.
    pub mcp_approval_selected: usize,
}

impl App {
    pub fn new(cfg: &crate::config::Config) -> Self {
        Self {
            mode: Mode::Goal,
            goals: Vec::new(),
            selected: 0,
            goal_input: String::new(),
            ask_input: String::new(),
            ask_answer: None,
            ask_pending: false,
            should_quit: false,
            spinner_frame: 0,
            flight_log_open: false,
            settings: SettingsState::new(cfg),
            palette_input: String::new(),
            palette_selected: 0,
            prev_mode: Mode::Goal,
            goal_cursor: 0,
            ask_cursor: 0,
            best_savings_pct: None,
            new_best_ticks: 0,
            help_open: false,
            flight_log_scroll: None,
            memory_records: Vec::new(),
            history_records: Vec::new(),
            nexus_status: NexusStatus::default(),
            store_path: None,
            pending_mcp_approvals: Vec::new(),
            mcp_approval_selected: 0,
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
}

/// Insert a character at a char-index position in `s`. Returns the new
/// caret position (one past the inserted char).
fn insert_char_at(s: &mut String, char_idx: usize, c: char) -> usize {
    let byte_idx = s
        .char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len());
    s.insert(byte_idx, c);
    char_idx + 1
}

/// Delete the character immediately before the caret. Returns the new caret.
fn delete_char_before(s: &mut String, char_idx: usize) -> usize {
    if char_idx == 0 {
        return 0;
    }
    let new_idx = char_idx - 1;
    let start = s.char_indices().nth(new_idx).map(|(b, _)| b).unwrap_or(0);
    let end = s
        .char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len());
    s.replace_range(start..end, "");
    new_idx
}

/// Delete the word (and trailing whitespace) immediately before the caret.
fn delete_word_before(s: &mut String, char_idx: usize) -> usize {
    let chars: Vec<char> = s.chars().collect();
    let mut i = char_idx.min(chars.len());
    while i > 0 && chars[i - 1].is_whitespace() {
        i -= 1;
    }
    while i > 0 && !chars[i - 1].is_whitespace() {
        i -= 1;
    }
    let start = s.char_indices().nth(i).map(|(b, _)| b).unwrap_or(0);
    let end = s
        .char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len());
    s.replace_range(start..end, "");
    i
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
    }

    /// Append a flight-log event to the goal at `index`.
    pub fn apply_event(&mut self, index: usize, event: EventRecord) {
        if let Some(g) = self.goals.get_mut(index) {
            g.flight_log.push(event);
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
        if !self.pending_mcp_approvals.is_empty() {
            return self.handle_mcp_approval_key(key);
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
            self.should_quit = true;
            return Some(Intent::Quit);
        }

        // `?` toggles the help overlay anywhere it isn't legitimate text input.
        if matches!(key.code, KeyCode::Char('?'))
            && !matches!(
                self.mode,
                Mode::Ask | Mode::Settings | Mode::Memory | Mode::History | Mode::CommandPalette
            )
            && self.goal_input.is_empty()
        {
            self.help_open = !self.help_open;
            return None;
        }
        // While help is up, swallow keystrokes so they don't leak into the
        // input buffer behind it. Esc handled above.
        if self.help_open {
            return None;
        }

        // Handle '/' as the command trigger (like gemini cli / slash commands)
        if matches!(key.code, KeyCode::Char('/'))
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
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    self.should_quit = true;
                    return Some(Intent::Quit);
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
            Mode::Memory | Mode::History => None,
            Mode::CommandPalette => self.handle_palette_key(key),
        }
    }

    fn handle_mcp_approval_key(&mut self, key: KeyEvent) -> Option<Intent> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            self.should_quit = true;
            return Some(Intent::Quit);
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
        let all_options = vec![
            "Goal Mode",
            "Task Mode",
            "Ask Mode",
            "Memory",
            "History",
            "Settings",
            "Toggle Log",
            "Help",
            "Delete Selected Goal",
            "Clear History",
            "Quit",
        ];

        let filtered_options: Vec<&str> = if self.palette_input.is_empty() {
            all_options.clone()
        } else {
            all_options
                .iter()
                .filter(|opt| {
                    opt.to_lowercase()
                        .contains(&self.palette_input.to_lowercase())
                })
                .copied()
                .collect()
        };

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
                match selected {
                    "Goal Mode" => {
                        self.mode = Mode::Goal;
                    }
                    "Task Mode" => {
                        self.mode = Mode::Task;
                    }
                    "Ask Mode" => {
                        self.mode = Mode::Ask;
                    }
                    "Memory" => {
                        self.mode = Mode::Memory;
                        return Some(Intent::OpenMemory);
                    }
                    "History" => {
                        self.mode = Mode::History;
                        return Some(Intent::OpenHistory);
                    }
                    "Settings" => {
                        self.mode = Mode::Settings;
                    }
                    "Toggle Log" => {
                        self.flight_log_open = !self.flight_log_open;
                        self.mode = self.prev_mode;
                    }
                    "Help" => {
                        self.help_open = true;
                        self.mode = self.prev_mode;
                    }
                    "Delete Selected Goal" => {
                        self.delete_selected_goal();
                        self.mode = self.prev_mode;
                    }
                    "Clear History" => {
                        self.goals.clear();
                        self.selected = 0;
                        self.mode = Mode::Goal;
                    }
                    "Quit" => {
                        self.should_quit = true;
                        return Some(Intent::Quit);
                    }
                    _ => {}
                }
                None
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
        // Ctrl+W deletes the word before the caret.
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('w')) {
            self.goal_cursor = delete_word_before(&mut self.goal_input, self.goal_cursor);
            return None;
        }
        match key.code {
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.goal_input);
                self.goal_cursor = 0;
                if text.trim().is_empty() {
                    return None;
                }
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
                self.goals.insert(0, GoalEntry::new(text.clone()));
                self.selected = 0;
                if self.mode == Mode::Task {
                    Some(Intent::QueueTask(text))
                } else {
                    Some(Intent::QueueGoal(text))
                }
            }
            KeyCode::Backspace => {
                self.goal_cursor = delete_char_before(&mut self.goal_input, self.goal_cursor);
                None
            }
            KeyCode::Left => {
                self.goal_cursor = self.goal_cursor.saturating_sub(1);
                None
            }
            KeyCode::Right => {
                if self.goal_cursor < char_count(&self.goal_input) {
                    self.goal_cursor += 1;
                }
                None
            }
            KeyCode::Home => {
                self.goal_cursor = 0;
                None
            }
            KeyCode::End => {
                self.goal_cursor = char_count(&self.goal_input);
                None
            }
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                None
            }
            KeyCode::Down => {
                if self.selected + 1 < self.goals.len() {
                    self.selected += 1;
                }
                None
            }
            KeyCode::Char('r') if self.goal_input.is_empty() => {
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
                self.goal_cursor = insert_char_at(&mut self.goal_input, self.goal_cursor, 'r');
                None
            }
            KeyCode::Char(c) => {
                self.goal_cursor = insert_char_at(&mut self.goal_input, self.goal_cursor, c);
                None
            }
            _ => None,
        }
    }

    fn handle_ask_key(&mut self, key: KeyEvent) -> Option<Intent> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('w')) {
            self.ask_cursor = delete_word_before(&mut self.ask_input, self.ask_cursor);
            return None;
        }
        match key.code {
            KeyCode::Enter => {
                let q = std::mem::take(&mut self.ask_input);
                self.ask_cursor = 0;
                if q.trim().is_empty() {
                    return None;
                }
                Some(Intent::Ask(q))
            }
            KeyCode::Backspace => {
                self.ask_cursor = delete_char_before(&mut self.ask_input, self.ask_cursor);
                None
            }
            KeyCode::Left => {
                self.ask_cursor = self.ask_cursor.saturating_sub(1);
                None
            }
            KeyCode::Right => {
                if self.ask_cursor < char_count(&self.ask_input) {
                    self.ask_cursor += 1;
                }
                None
            }
            KeyCode::Home => {
                self.ask_cursor = 0;
                None
            }
            KeyCode::End => {
                self.ask_cursor = char_count(&self.ask_input);
                None
            }
            KeyCode::Char(c) => {
                self.ask_cursor = insert_char_at(&mut self.ask_input, self.ask_cursor, c);
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
    /// User triggered a rollback to a specific checkpoint seq for the
    /// currently selected goal.
    Rollback { goal_index: usize, to_seq: u32 },
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
    // Once a goal is queued, collapse the giant ASCII logo down to a slim
    // one-line header so the work area gets the screen real estate.
    let want_full_logo = app.goals.is_empty() && area.width >= LOGO_WIDTH_THRESHOLD;
    let splash_h: u16 = if want_full_logo {
        LOGO.len() as u16 + 1
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
    render_footer(frame, footer_row, app);

    if app.mode == Mode::Settings {
        render_settings(frame, area, app);
    }
    if app.mode == Mode::CommandPalette {
        render_palette(frame, area, app);
    }
    if app.help_open {
        render_help(frame, area);
    }
    if !app.pending_mcp_approvals.is_empty() {
        render_mcp_approval(frame, area, app);
    }
}

/// Centred modal listing every keybinding in one place. Toggled by `?`,
/// dismissed with `?` again or `Esc`. Drawn last so it always sits on top.
fn render_help(frame: &mut Frame, area: Rect) {
    let rows: &[(&str, &str)] = &[
        ("Enter", "submit goal / question"),
        ("/", "open the command palette"),
        ("?", "toggle this help"),
        ("Ctrl+;", "toggle the Ask side panel"),
        ("Shift+L", "toggle the Flight Log"),
        ("Ctrl+D", "delete the selected goal"),
        ("Ctrl+W", "delete the previous word in the input"),
        ("↑ / ↓", "move selection in Goals (or palette)"),
        ("← / →", "move caret in the input bar"),
        ("Home / End", "jump to start/end of the input"),
        ("Ctrl+↑↓", "move the checkpoint cursor"),
        ("r", "rollback to the highlighted checkpoint (input empty)"),
        ("Ctrl+C", "quit immediately"),
        ("Esc", "close overlay / cancel / quit"),
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
    // Slowly drifting phase so the logo gets a deliberate scanline shimmer
    // without turning the whole splash into a fast flashing surface.
    let phase = (app.spinner_frame as f32) * LOGO_SHIMMER_SPEED;
    if area.height > LOGO.len() as u16 && area.width >= LOGO_WIDTH_THRESHOLD {
        let mut lines: Vec<Line> = Vec::with_capacity(LOGO.len() + 1);
        lines.push(Line::raw(""));
        lines.extend(
            LOGO.iter()
                .enumerate()
                .map(|(row_idx, row)| logo_line(row, phase, row_idx)),
        );
        let p = Paragraph::new(lines)
            .alignment(Alignment::Center)
            .style(Style::default().bg(BG_DEEP));
        frame.render_widget(p, area);
    } else {
        // Compact one-line header - gradient "phonton" + dim subtitle.
        let mut spans = gradient_line("✦ phonton", phase * 0.8, true).spans;
        spans.push(Span::styled("  ── ", Style::default().fg(DIM)));
        spans.push(Span::styled(
            "agentic dev environment",
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
            Span::styled("/", key),
            Span::styled(" commands  ", txt),
            sep.clone(),
            Span::styled("?", key),
            Span::styled(" help  ", txt),
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
            Span::styled(" quit", txt),
        ],
        Mode::Ask => vec![
            Span::styled("Enter", key),
            Span::styled(" send  ", txt),
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
    let all_options = vec![
        "Goal Mode",
        "Task Mode",
        "Ask Mode",
        "Memory",
        "History",
        "Settings",
        "Toggle Log",
        "Delete Selected Goal",
        "Clear History",
        "Quit",
    ];

    let filtered_options: Vec<&str> = if app.palette_input.is_empty() {
        all_options.clone()
    } else {
        all_options
            .iter()
            .filter(|opt| {
                opt.to_lowercase()
                    .contains(&app.palette_input.to_lowercase())
            })
            .copied()
            .collect()
    };

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

    let popup_w = 40;
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
        .map(|(i, &opt)| {
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
            ListItem::new(Line::from(format!("{marker}{opt}"))).style(style)
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
            spans.extend(status_tag_spans(&g.status, app.spinner_frame));
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
            spans.push(Span::styled(short(&g.description, 40), base_style));
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
        .constraints([Constraint::Min(1), Constraint::Length(7)])
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

    let block = Block::default()
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
        let lines = vec![
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
                    "    /",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled("       command palette", muted),
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
                Span::styled("     quit", muted),
            ]),
        ];
        let p = Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false });
        frame.render_widget(p, area);
        return;
    };

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            "goal: ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::raw(g.description.clone()),
    ]));
    lines.push(Line::raw(""));

    if let Some(state) = &g.state {
        for w in &state.active_workers {
            let mut spans = status_tag_spans(&w.status_as_task(), app.spinner_frame);
            spans.push(Span::raw(" "));
            spans.push(Span::raw(short(&w.subtask_description, 50)));
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

        // Checkpoint picker — one line per landed subtask, newest last.
        // Marked with a "↶ N" tag for "Rollback to step N" — the actual
        // rollback is dispatched through the orchestrator's control
        // channel (see `OrchestratorMessage::RollbackRequest`); this
        // panel only surfaces the picker.
        if !state.checkpoints.is_empty() {
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

    let p = Paragraph::new(lines).wrap(Wrap { trim: true }).block(block);
    frame.render_widget(p, area);
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
    }

    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }).block(block),
        area,
    );
}

fn render_history(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .title(Span::styled(
            " History ",
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
    if app.history_records.is_empty() {
        lines.push(Line::from(Span::styled(
            "No persisted task history yet.",
            Style::default().fg(MUTED),
        )));
    } else {
        for row in app.history_records.iter().take(20) {
            let status = task_status_label(&row.status);
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{status:<9} "),
                    Style::default().fg(ACCENT_HI).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{} tok  ", row.total_tokens),
                    Style::default().fg(MUTED),
                ),
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
    } else if status.get("Running").is_some() {
        "running"
    } else if status.get("Paused").is_some() {
        "paused"
    } else {
        "task"
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
        for l in ans.lines() {
            lines.push(Line::raw(l.to_string()));
        }
    } else {
        lines.push(Line::from(Span::styled(
            "(no answer yet)",
            Style::default().fg(MUTED),
        )));
    }
    let p = Paragraph::new(lines).wrap(Wrap { trim: true }).block(
        Block::default()
            .title(Span::styled(
                " Ask ",
                Style::default().fg(VIOLET).add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::default().fg(VIOLET)),
    );
    frame.render_widget(p, area);
}

fn render_input(frame: &mut Frame, area: Rect, app: &App) {
    let (icon, mode_label, buf, cursor) = match app.mode {
        Mode::Goal => ("›", " GOAL ", &app.goal_input, app.goal_cursor),
        Mode::Task => ("›", " TASK ", &app.goal_input, app.goal_cursor),
        Mode::Ask => ("?", " ASK ", &app.ask_input, app.ask_cursor),
        Mode::Settings => ("⚙", " SETTINGS ", &app.goal_input, app.goal_cursor),
        Mode::Memory => ("M", " MEMORY ", &app.goal_input, app.goal_cursor),
        Mode::History => ("H", " HISTORY ", &app.goal_input, app.goal_cursor),
        Mode::CommandPalette => ("/", " COMMAND ", &app.goal_input, app.goal_cursor),
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

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(BG_DEEP));

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

    // Horizontal scroll so the drawn caret is always visible inside the input slot.
    let input_width = row[0].width.saturating_sub(prompt_prefix_w) as usize;
    let show_drawn_caret = input_width > 0
        && !matches!(
            app.mode,
            Mode::Settings | Mode::Memory | Mode::History | Mode::CommandPalette
        );
    let visible_width = input_width.saturating_sub(usize::from(show_drawn_caret));
    let total_chars = char_count(buf);
    let cursor_clamped = cursor.min(total_chars);
    let scroll = if show_drawn_caret {
        cursor_clamped.saturating_sub(visible_width)
    } else {
        cursor_clamped.saturating_sub(input_width.saturating_sub(1))
    };
    let visible_chars: Vec<char> = buf.chars().skip(scroll).take(visible_width).collect();
    let caret_offset = cursor_clamped
        .saturating_sub(scroll)
        .min(visible_chars.len());
    let before_caret: String = visible_chars.iter().take(caret_offset).collect();
    let after_caret: String = visible_chars.iter().skip(caret_offset).collect();

    let mut prompt_spans = vec![
        Span::styled(
            prompt_prefix,
            Style::default()
                .fg(border_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(before_caret, Style::default().fg(Color::White)),
    ];
    if show_drawn_caret {
        prompt_spans.push(Span::styled(
            "▌",
            Style::default()
                .fg(border_color)
                .add_modifier(Modifier::BOLD),
        ));
    }
    prompt_spans.push(Span::styled(after_caret, Style::default().fg(Color::White)));

    let prompt = Paragraph::new(Line::from(prompt_spans)).style(Style::default().bg(BG_DEEP));
    frame.render_widget(prompt, row[0]);

    let badge = Paragraph::new(Line::from(Span::styled(mode_label, mode_style)))
        .alignment(Alignment::Right)
        .style(Style::default().bg(BG_DEEP));
    frame.render_widget(badge, row[1]);

    // The native terminal cursor is hidden while Phonton runs. A drawn caret
    // avoids terminal-controlled blinking and keeps the input bar visually
    // stable during the animated splash.
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

fn short(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    } else {
        s.to_string()
    }
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
    StateUpdate(usize, GlobalState),
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
            eprintln!(
                "phonton: semantic index unavailable ({e}); continuing without indexed context"
            );
            None
        }
        Err(_) => {
            eprintln!(
                "phonton: semantic index timed out after {SEMANTIC_INDEX_TIMEOUT_SECS}s; continuing without indexed context"
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
        .ok_or_else(|| format!("unknown provider `{name}`"))?;
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
         phonton [SUBCOMMAND]\n\
         \n\
         SUBCOMMANDS:\n  \
         (none)            Launch the interactive TUI (default)\n  \
         ask <question>    One-shot Q&A using the configured provider\n  \
         doctor            Check config, store, trust, git, cargo, and Nexus\n  \
         extensions        Inspect loaded steering, skills, MCP, and profiles\n  \
         skills            Inspect loaded skills\n  \
         steering          Inspect loaded steering rules\n  \
         mcp               List configured MCP servers and explicitly call tools\n  \
         plan <goal>       Preview the task DAG without changing files\n  \
         review [task-id]  Show verified diff review payloads\n  \
         memory            List, edit, delete, and pin persistent memory\n  \
         config path       Print the resolved config file path\n  \
         config edit       Open the config in $EDITOR (or notepad on Windows)\n  \
         config show       Dump the resolved config as TOML\n  \
         version           Print version and exit\n  \
         help              Print this help and exit\n\
         \n\
         FLAGS:\n  \
         -h, --help        Same as `help`\n  \
         -V, --version     Same as `version`\n\
         \n\
         CONFIG:\n  \
         Settings live in ~/.phonton/config.toml. Override the provider key with\n  \
         ANTHROPIC_API_KEY, OPENAI_API_KEY, TOGETHER_API_KEY, etc.\n\
         \n\
         DOCTOR:\n  \
         phonton doctor [--json] [--provider]\n\
         \n\
         PLAN PREVIEW:\n  \
         phonton plan [--json] [--no-memory] [--no-tests] <goal>\n\
         \n\
         REVIEW:\n  \
         phonton review [--json] [latest|<task-id>]\n  \
         phonton review approve [--json] [latest|<task-id>]\n  \
         phonton review reject [--json] [latest|<task-id>]\n  \
         phonton review rollback [--json] [latest|<task-id>] <seq>\n\
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

/// Handle CLI subcommands that exit before the TUI launches.
/// Returns `Ok(true)` if a subcommand was handled (caller should exit),
/// `Ok(false)` if the TUI should launch normally.
async fn handle_cli_args() -> Result<bool> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        return Ok(false);
    }
    match args[0].as_str() {
        "-h" | "--help" | "help" => {
            print_help();
            Ok(true)
        }
        "-V" | "--version" | "version" => {
            print_version();
            Ok(true)
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
            Ok(true)
        }
        "doctor" => {
            let working_dir =
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let code = doctor::run(&working_dir, &args[1..]).await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(true)
        }
        "extensions" => {
            let working_dir =
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let code = extensions_cli::run(&working_dir, &args[1..]).await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(true)
        }
        "skills" => {
            let working_dir =
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let code = extensions_cli::run_skills(&working_dir, &args[1..]).await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(true)
        }
        "steering" => {
            let working_dir =
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let code = extensions_cli::run_steering(&working_dir, &args[1..]).await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(true)
        }
        "mcp" => {
            let working_dir =
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let code = mcp_cli::run(&working_dir, &args[1..]).await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(true)
        }
        "plan" => {
            let code = plan_preview::run(&args[1..]).await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(true)
        }
        "review" => {
            let code = review::run(&args[1..]).await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(true)
        }
        "memory" => {
            let code = memory_cli::run(&args[1..]).await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(true)
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
                    Ok(true)
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
    if handle_cli_args().await? {
        return Ok(());
    }
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

    let sandbox = Arc::new(Sandbox::new(working_dir.clone(), "phonton-cli".to_string()));
    let controls: ControlRegistry = Arc::new(std::sync::Mutex::new(HashMap::new()));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, crossterm::cursor::Hide)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let (evt_tx, mut evt_rx) = mpsc::channel::<LoopEvent>(64);
    spawn_input_task(evt_tx.clone());

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

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        crossterm::cursor::Show,
    )?;
    terminal.show_cursor()?;
    result
}

fn spawn_input_task(tx: mpsc::Sender<LoopEvent>) {
    std::thread::spawn(move || loop {
        // Poll on a modest cadence. This keeps input responsive while
        // preventing the splash animation from feeling like it is flashing
        // on every frame.
        if event::poll(Duration::from_millis(UI_TICK_MS)).unwrap_or(false) {
            if let Ok(Event::Key(k)) = event::read() {
                // IMPORTANT: Filter for 'Press' events only. Windows and some
                // modern terminal emulators send 'Release' events too. If we
                // handle both, the user sees "double input" (e.g. 'ww') and
                // the TUI flickers because we redraw twice.
                if k.kind != event::KeyEventKind::Release
                    && tx.blocking_send(LoopEvent::Key(k)).is_err()
                {
                    break;
                }
            }
        } else if tx.blocking_send(LoopEvent::Tick).is_err() {
            break;
        }
    });
}

async fn run_app<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    rx: &mut mpsc::Receiver<LoopEvent>,
    tx: mpsc::Sender<LoopEvent>,
    store: Arc<std::sync::Mutex<Store>>,
    ask_provider: Option<Arc<dyn Provider>>,
    sandbox: Arc<Sandbox>,
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
            LoopEvent::Key(k) => {
                if let Some(intent) = app.handle_key(k) {
                    match intent {
                        Intent::Quit => {
                            deny_pending_mcp_approvals(&mut approval_replies);
                            break;
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
                            cfg.provider.name = app.settings.provider.clone();
                            cfg.provider.model = if app.settings.model.is_empty() {
                                None
                            } else {
                                Some(app.settings.model.clone())
                            };
                            cfg.provider.api_key = if app.settings.api_key.is_empty() {
                                None
                            } else {
                                Some(app.settings.api_key.clone())
                            };
                            cfg.provider.account_id = if app.settings.account_id.is_empty() {
                                None
                            } else {
                                Some(app.settings.account_id.clone())
                            };
                            cfg.provider.base_url = if app.settings.base_url.is_empty() {
                                None
                            } else {
                                Some(app.settings.base_url.clone())
                            };
                            spawn_goal(
                                0,
                                task_id,
                                text,
                                direct_task,
                                &tx,
                                &store,
                                &sandbox,
                                &controls,
                                &cfg,
                                &working_dir,
                            )
                            .await;
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
                            cfg.provider.name = app.settings.provider.clone();
                            cfg.provider.model = if app.settings.model.is_empty() {
                                None
                            } else {
                                Some(app.settings.model.clone())
                            };
                            cfg.provider.api_key = if app.settings.api_key.is_empty() {
                                None
                            } else {
                                Some(app.settings.api_key.clone())
                            };
                            cfg.provider.base_url = if app.settings.base_url.is_empty() {
                                None
                            } else {
                                Some(app.settings.base_url.clone())
                            };
                            cfg.budget.max_tokens = app.settings.max_tokens.parse().ok();
                            cfg.budget.max_usd_cents = app.settings.max_usd_cents.parse().ok();

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
                                Ok(rows) => app.history_records = rows,
                                Err(e) => {
                                    app.settings.message = Some(format!("History load failed: {e}"))
                                }
                            },
                            Err(_) => {
                                app.settings.message =
                                    Some("History load failed: store lock poisoned".into())
                            }
                        },
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
            LoopEvent::StateUpdate(idx, state) => app.apply_state(idx, state),
            LoopEvent::FlightEvent(idx, ev) => app.apply_event(idx, ev),
            LoopEvent::AskAnswer(a) => {
                app.ask_pending = false;
                app.ask_answer = Some(a);
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

fn single_task_plan(description: String) -> PlannerOutput {
    let subtask = Subtask {
        id: SubtaskId::new(),
        description,
        model_tier: ModelTier::Standard,
        dependencies: Vec::new(),
        status: SubtaskStatus::Queued,
    };

    PlannerOutput {
        subtasks: vec![subtask],
        estimated_total_tokens: 1_200,
        naive_baseline_tokens: 4_000,
        coverage_summary: CoverageSummary::default(),
    }
}

async fn spawn_goal(
    goal_index: usize,
    task_id: TaskId,
    text: String,
    direct_task: bool,
    tx: &mpsc::Sender<LoopEvent>,
    store: &Arc<std::sync::Mutex<Store>>,
    sandbox: &Arc<Sandbox>,
    controls: &ControlRegistry,
    cfg: &config::Config,
    working_dir: &std::path::PathBuf,
) {
    if let Ok(g) = store.lock() {
        let _ = g.upsert_task(task_id, &text, &TaskStatus::Planning, 0);
    }
    let memory_store = phonton_memory::MemoryStore::new(Arc::clone(store)).await;

    let plan_result = if direct_task {
        Ok(single_task_plan(text.clone()))
    } else {
        let store_guard = match store.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let result = decompose_with_memory(&Goal::new(text.clone()), &store_guard, None).await;
        drop(store_guard);
        result
    };
    let mut plan = match plan_result {
        Ok(p) => p,
        Err(_) => return,
    };

    let (state_tx, mut state_rx) = watch::channel(GlobalState {
        task_status: TaskStatus::Planning,
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

    let extension_set = load_extensions(&ExtensionLoadOptions::for_workspace(working_dir));
    apply_extension_context_to_plan(&mut plan, &extension_set);
    publish_extension_events(task_id, &extension_set, &event_tx);

    let naive = plan.naive_baseline_tokens;
    let semantic_context = build_semantic_context(working_dir).await;
    let mcp_runtime = if extension_set.mcp_servers.is_empty() {
        None
    } else {
        let approver = Arc::new(TuiMcpApprover::new(goal_index, tx.clone()));
        Some(Arc::new(
            phonton_mcp::McpRuntime::new(
                extension_set.mcp_servers.clone(),
                ExecutionGuard::new(working_dir.clone()),
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

            let factory = move |tier: phonton_types::ModelTier| {
                let model = configured_model
                    .clone()
                    .unwrap_or_else(|| phonton_providers::model_for_tier(&provider_name, tier));
                let provider_cfg = make_api_provider_config(
                    &provider_name,
                    api_key.clone(),
                    model,
                    account_id.clone(),
                    base_url.clone(),
                )
                .expect("unknown provider config");
                provider_for(provider_cfg)
            };

            let guard = ExecutionGuard::new(working_dir.clone());
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
    let diff_applier = DiffApplier::open(working_dir)
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
            }
            if tx_updates
                .send(LoopEvent::StateUpdate(goal_index, s))
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
    fn typing_a_goal_appends_to_buffer() {
        let mut app = App::default();
        for c in "add fn foo".chars() {
            assert!(app.handle_key(key(c)).is_none());
        }
        assert_eq!(app.goal_input, "add fn foo");
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
        assert_eq!(app.goal_input, "");
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
    fn esc_from_ask_returns_to_goal_without_quitting() {
        let mut app = App::default();
        app.handle_key(ctrl(';'));
        assert_eq!(app.mode, Mode::Ask);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Goal);
        assert!(!app.should_quit);
    }

    #[test]
    fn esc_from_goal_quits() {
        let mut app = App::default();
        let r = app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(r, Some(Intent::Quit));
        assert!(app.should_quit);
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
    fn savings_line_shows_percent_when_baseline_known() {
        let s = GlobalState {
            task_status: TaskStatus::Queued,
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
    fn renders_without_panicking_on_empty_state() {
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = App::default();
        terminal.draw(|f| render(f, &app)).unwrap();
    }

    #[test]
    fn input_bar_renders_steady_drawn_caret() {
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::default();
        app.goal_input = "ship 0.3.1".to_string();
        app.goal_cursor = char_count(&app.goal_input);

        terminal.draw(|f| render(f, &app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let dump: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(dump.contains("ship 0.3.1▌"));
    }

    #[test]
    fn splash_logo_is_compact_and_shadowed() {
        let max_width = LOGO.iter().map(|row| char_count(row)).max().unwrap_or(0);
        assert!(max_width <= LOGO_WIDTH_THRESHOLD as usize);
        assert!(max_width < 64, "logo should stay compact for the splash");
        assert!(
            LOGO.iter().any(|row| row.contains('░')),
            "logo should carry its own pixel shadow layer"
        );
        assert!(
            LOGO.iter().any(|row| row.contains("██████╗")),
            "logo should use the ANSI Shadow terminal mark"
        );
    }

    #[test]
    fn renders_new_logo_on_wide_splash() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = App::default();
        terminal.draw(|f| render(f, &app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let dump: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(dump.contains("█████"));
        assert!(dump.contains("██████╗"));
        assert!(dump.contains("░▒▓"));
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
