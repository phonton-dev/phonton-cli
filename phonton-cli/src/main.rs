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

mod art;
mod ask_context;
mod benchmark_cli;
mod config;
mod contract_preflight;
mod doctor;
mod extensions_cli;
mod index_cli;
mod local_goal_cli;
mod local_plan_approval;
mod local_tui;
mod mcp_cli;
mod memory_cli;
mod models_cli;
mod plan_preview;
mod prompt_buffer;
mod proof_cli;
mod record;
mod review;
mod serve_cli;
mod serve_desktop;
mod store_util;
mod tokens_cli;
mod trust;

pub(crate) use store_util::open_persistent_store;

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use base64::{engine::general_purpose, Engine as _};
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use phonton_diff::DiffApplier;
use phonton_extensions::{load_extensions, DiagnosticSeverity, ExtensionLoadOptions, ExtensionSet};
use phonton_mcp::{
    DenyByDefaultApprover, ExplicitApproveAll, McpApprovalDecision, McpApprovalRequest, McpApprover,
};
use phonton_orchestrator::{BudgetGuard, Orchestrator, WorkerDispatcher};
use phonton_planner::{decompose_with_memory, Goal};
use phonton_providers::{
    discover_models, pick_default_from_list, provider_for, select_best_working_model, Provider,
};
use phonton_sandbox::{ExecutionGuard, Sandbox};
use phonton_store::{Store, TaskRecord};
use phonton_types::{
    BudgetLimits, ContextManifest, CostReceipt, CoverageSummary, EventRecord, ExtensionId,
    GlobalState, HandoffPacket, MemoryRecord, ModelPricing, ModelTier, OrchestratorEvent,
    OrchestratorMessage, OutcomeLedger, PausedRunSnapshot, Permission, PermissionLedger,
    PlannerOutput, PromptArtifact, PromptArtifactRole, PromptAttachment, PromptAttachmentKind,
    ProviderConfig as ApiProviderConfig, ProviderKind, Subtask, SubtaskId, SubtaskResult,
    SubtaskStatus, TaskId, TaskStatus, TokenUsage,
};
use prompt_buffer::{PromptBuffer, SubmittedPrompt};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

// ---------------------------------------------------------------------------
// Visual identity: Ink & Photon (see art.rs)
// ---------------------------------------------------------------------------

// Ink and paper, flat signal colours; the logo spectrum lives in art.rs.
const ACCENT: Color = art::PHOTON;
const ACCENT_HI: Color = art::PAPER;
const PAPER: Color = art::PAPER;
const SUCCESS: Color = art::VERIFIED;
const WARN: Color = art::RUNNING;
const DANGER: Color = art::FAILED;
const MUTED: Color = art::MUTED;
const DIM: Color = art::DIM;
const RULE: Color = art::RULE;
const BG_PANEL: Color = art::PANEL;
const BG_DEEP: Color = art::INK;
/// Side-channel accent (ask mode, flight log): quiet paper, not a new hue.
const QUIET: Color = Color::Rgb(201, 198, 190);

const UI_TICK_MS: u64 = 80;
const LOGO_WIDTH_THRESHOLD: u16 = 72;
static NEXT_MCP_APPROVAL_ID: AtomicU64 = AtomicU64::new(1);

/// `[ text ]` tag in a flat signal colour.
fn tag(text: &str, color: Color) -> Span<'static> {
    Span::styled(
        format!("[{text}]"),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

/// What a handoff can honestly claim.
fn handoff_verdict(h: &HandoffPacket) -> art::Verdict {
    if !h.verification.findings.is_empty() {
        art::Verdict::Review
    } else if h.verification.passed.is_empty() {
        art::Verdict::Unverified
    } else if !h.verification.skipped.is_empty() {
        art::Verdict::Partial
    } else {
        art::Verdict::Verified
    }
}

/// True when the provider runs on this machine (Ollama, or a compatible
/// server on a loopback address).
fn provider_is_local(provider: &str, base_url: &str) -> bool {
    match provider {
        "ollama" => true,
        "custom" | "openai-compatible" => {
            let host = base_url
                .split("://")
                .nth(1)
                .unwrap_or(base_url)
                .split(['/', ':'])
                .next()
                .unwrap_or("");
            matches!(host, "localhost" | "127.0.0.1" | "[" | "::1") || host.ends_with(".localhost")
        }
        _ => false,
    }
}

/// Flat block bar, `width` cells, filled `filled_frac` of the way.
fn gradient_bar(filled_frac: f32, width: usize) -> Vec<Span<'static>> {
    art::gauge(filled_frac, width, ACCENT)
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
    /// Suspended to answer clarification questions.
    Clarify,
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
    /// When the goal was queued; drives the elapsed clock.
    pub started_at: std::time::Instant,
    /// When the goal reached a terminal or review state.
    pub finished_at: Option<std::time::Instant>,
    /// UI tick at which the receipt first appeared (count-up animation).
    pub receipt_tick: Option<usize>,
    /// True once this goal has been added to the run record.
    pub recorded: bool,
    /// Local harness records count only after the durable final receipt is saved.
    pub local_harness: bool,
    /// Route observed when this goal was dispatched; later settings do not change it.
    pub token_origin: record::TokenOrigin,
    /// Latest local-harness receipt when this goal ran on the local model.
    pub local: Option<Box<phonton_types::local_run::LocalRunReceipt>>,
    /// Result of applying the local candidate: `Ok(summary)` or `Err(why)`.
    pub applied: Option<Result<String, String>>,
}

/// What this machine brings to a run, shown in the side panel. Filled in
/// the background at startup; absent fields mean "not observed".
#[derive(Debug, Clone, Default)]
pub struct Machine {
    /// Calibrated and selected local model, if any.
    pub local_model: Option<String>,
    /// Edit protocol the calibration chose for it.
    pub protocol: Option<String>,
    /// Calibrated context window.
    pub context_tokens: Option<u32>,
    /// First GPU: name, free bytes, total bytes.
    pub gpu: Option<(String, u64, u64)>,
    /// Host RAM: free bytes, total bytes.
    pub ram: Option<(u64, u64)>,
    /// True once the local model selection has been read (hardware may
    /// still be pending); until then panels say "checking", not "missing".
    pub probed: bool,
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
    /// A local candidate is reviewed and not yet applied.
    fn can_apply(&self) -> bool {
        self.applied.is_none()
            && matches!(self.status, TaskStatus::Reviewing { .. })
            && self
                .local
                .as_ref()
                .is_some_and(|r| r.selected_candidate.is_some() && r.state == "review_ready")
    }

    fn new(description: String) -> Self {
        Self {
            description,
            status: TaskStatus::Queued,
            state: None,
            task_id: TaskId::new(),
            flight_log: Vec::new(),
            checkpoint_cursor: None,
            started_at: std::time::Instant::now(),
            finished_at: None,
            receipt_tick: None,
            recorded: false,
            local_harness: false,
            token_origin: record::TokenOrigin::Unknown,
            local: None,
            applied: None,
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
    /// Goal/task prompt buffer, including collapsed paste artifacts.
    pub goal_prompt: PromptBuffer,
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
    /// Local plans waiting for a per-goal approval.
    pub pending_local_plans: Vec<local_plan_approval::PendingLocalPlan>,
    /// True when the pending prompt artifact drawer is visible.
    pub prompt_artifacts_open: bool,
    /// Cursor into pending prompt artifacts.
    pub prompt_artifact_selected: usize,
    /// Interactive clarification goal index
    pub clarifying_goal_idx: Option<usize>,
    /// Active clarification question index
    pub clarifying_question_idx: usize,
    /// Answers gathered so far
    pub clarifying_answers: Vec<String>,
    /// Live typing buffer for answers
    pub clarifying_buffer: String,
    /// Caret position inside clarifying_buffer
    pub clarifying_cursor: usize,
    /// Set by the first Esc/Ctrl+C at top level; a second press within
    /// [`QUIT_CONFIRM_WINDOW`] quits. Prevents losing a session to one key.
    pub quit_armed_at: Option<std::time::Instant>,
    /// Session answer to "may verification run project code on this
    /// machine?". `None` until the first goal of the session asks.
    pub host_checks_approved: Option<bool>,
    /// Goal held back while the host-check question is on screen, plus
    /// whether it was submitted in Task mode.
    pub pending_host_goal: Option<(SubmittedPrompt, bool)>,
    /// Verified-run record; only finished receipts move it.
    pub record: record::Record,
    /// Local model and hardware observed at startup.
    pub machine: Machine,
    /// False when `PHONTON_REDUCED_MOTION` is set: art renders still.
    pub motion: bool,
    /// Finished runs not yet written to the record file (the event loop
    /// saves them; tests never touch the user's record).
    pub unsaved_runs: Vec<(record::Outcome, u64, record::TokenOrigin)>,
    /// False when the configured provider has no usable key. Goals are then
    /// routed to Settings instead of starting a run that can only fail.
    pub model_ready: bool,
}

/// How long a first Esc/Ctrl+C stays armed waiting for confirmation.
pub const QUIT_CONFIRM_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

impl App {
    pub fn new(cfg: &crate::config::Config) -> Self {
        Self {
            mode: Mode::Goal,
            goals: Vec::new(),
            selected: 0,
            goal_prompt: PromptBuffer::default(),
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
            pending_local_plans: Vec::new(),
            prompt_artifacts_open: false,
            prompt_artifact_selected: 0,
            clarifying_goal_idx: None,
            clarifying_question_idx: 0,
            clarifying_answers: Vec::new(),
            clarifying_buffer: String::new(),
            clarifying_cursor: 0,
            quit_armed_at: None,
            host_checks_approved: None,
            pending_host_goal: None,
            record: record::Record::default(),
            machine: Machine::default(),
            motion: std::env::var_os("PHONTON_REDUCED_MOTION").is_none(),
            unsaved_runs: Vec::new(),
            model_ready: true,
        }
    }

    /// Animation tick for art helpers; `None` when motion is reduced.
    fn tick(&self) -> Option<usize> {
        self.motion.then_some(self.spinner_frame)
    }

    /// True while a first quit press is waiting for confirmation.
    pub fn quit_armed(&self) -> bool {
        self.quit_armed_at
            .is_some_and(|at| at.elapsed() < QUIT_CONFIRM_WINDOW)
    }

    /// First press arms, second press (within the window) quits.
    fn request_quit(&mut self) -> Option<Intent> {
        if self.quit_armed() {
            self.should_quit = true;
            return Some(Intent::Quit);
        }
        self.quit_armed_at = Some(std::time::Instant::now());
        None
    }

    /// Answer the host-check question for this session, then queue the
    /// held goal. Esc puts the goal text back in the prompt instead.
    fn handle_host_checks_key(&mut self, key: KeyEvent) -> Option<Intent> {
        let approved = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => true,
            KeyCode::Char('n') | KeyCode::Char('N') => false,
            KeyCode::Esc => {
                if let Some((prompt, _)) = self.pending_host_goal.take() {
                    // ponytail: collapsed paste artifacts are not restored;
                    // only the visible text comes back.
                    self.goal_prompt.insert_text(&prompt.display_text);
                }
                return None;
            }
            _ => return None,
        };
        self.host_checks_approved = Some(approved);
        let (prompt, direct_task) = self.pending_host_goal.take()?;
        Some(self.queue_goal(prompt, direct_task))
    }

    fn queue_goal(&mut self, prompt: SubmittedPrompt, direct_task: bool) -> Intent {
        self.goals
            .insert(0, GoalEntry::new(prompt.display_text.clone()));
        self.selected = 0;
        if direct_task {
            Intent::QueueTask(prompt)
        } else {
            Intent::QueueGoal(prompt)
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
/// Provider implied by an unambiguous key prefix. Bare `sk-` is shared by
/// OpenAI, DeepSeek and others, so it is left to the user.
fn provider_for_key_prefix(key: &str) -> Option<&'static str> {
    [
        ("sk-ant-", "anthropic"),
        ("sk-or-", "openrouter"),
        ("sk-proj-", "openai"),
        ("AIza", "gemini"),
        ("xai-", "xai"),
        ("gsk_", "groq"),
        ("tgp_v1_", "together"),
    ]
    .iter()
    .find(|(prefix, _)| key.starts_with(prefix))
    .map(|(_, provider)| *provider)
}

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

fn contains_likely_secret(s: &str) -> bool {
    s.split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '`' | ',' | ';'))
        .any(looks_like_api_key)
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
                keys: Default::default(),
                allow_unverified_model: None,
            },
            budget: crate::config::BudgetConfig {
                max_tokens: None,
                max_usd_cents: None,
            },
            index: crate::config::IndexConfig::default(),
            permissions: crate::config::PermissionsConfig::default(),
            general: crate::config::GeneralConfig::default(),
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
        let tick = self.spinner_frame;
        if let Some(g) = self.goals.get_mut(index) {
            let settled = matches!(
                state.task_status,
                TaskStatus::Reviewing { .. } | TaskStatus::Done { .. } | TaskStatus::Failed { .. }
            );
            if settled && g.finished_at.is_none() {
                g.finished_at = Some(std::time::Instant::now());
            }
            if state.handoff_packet.is_some() && g.receipt_tick.is_none() {
                g.receipt_tick = Some(tick);
            }
            // Only runs that produced evidence move the record; a goal refused
            // before any model call (no RAM, no runtime) is not a lost run.
            let ran = state.handoff_packet.as_ref().is_some_and(|h| {
                !h.verification.passed.is_empty()
                    || !h.verification.findings.is_empty()
                    || !h.changed_files.is_empty()
                    || h.token_usage.input_tokens + h.token_usage.output_tokens > 0
            });
            if settled && ran && !g.recorded && !g.local_harness {
                g.recorded = true;
                let outcome = match (&state.task_status, &state.handoff_packet) {
                    (TaskStatus::Failed { .. }, _) => record::Outcome::Failed,
                    (_, Some(h)) if handoff_verdict(h) == art::Verdict::Verified => {
                        record::Outcome::Verified
                    }
                    _ => record::Outcome::Unverified,
                };
                self.record.add(outcome, state.tokens_used, g.token_origin);
                self.unsaved_runs
                    .push((outcome, state.tokens_used, g.token_origin));
            }
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
        if let Some(prompt) = self.pending_local_plans.first_mut() {
            let decision = prompt.key(key);
            return decision.map(|approved| {
                let prompt = self.pending_local_plans.remove(0);
                Intent::ResolveLocalPlan {
                    task_id: prompt.task_id,
                    approved,
                }
            });
        }
        if !self.pending_mcp_approvals.is_empty() {
            return self.handle_mcp_approval_key(key);
        }

        if self.pending_host_goal.is_some() {
            return self.handle_host_checks_key(key);
        }

        if self.prompt_artifacts_open {
            return self.handle_prompt_artifacts_key(key);
        }

        // Ctrl+Y applies a reviewed local candidate on the selected goal.
        if key.code == KeyCode::Char('y') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if let Some(g) = self.current_goal().filter(|g| g.can_apply()) {
                return Some(Intent::ApplyLocal(g.task_id));
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
                Mode::Ask
                    | Mode::Settings
                    | Mode::Memory
                    | Mode::History
                    | Mode::CommandPalette
                    | Mode::Clarify
            ) {
                if self.mode == Mode::Clarify {
                    self.clarifying_goal_idx = None;
                    self.clarifying_question_idx = 0;
                    self.clarifying_answers.clear();
                    self.clarifying_buffer.clear();
                    self.clarifying_cursor = 0;
                }
                self.mode = Mode::Goal;
                return None;
            }
            return self.request_quit();
        }

        // `?` toggles the help overlay anywhere it isn't legitimate text input.
        if matches!(key.code, KeyCode::Char('?'))
            && !matches!(
                self.mode,
                Mode::Ask
                    | Mode::Settings
                    | Mode::Memory
                    | Mode::History
                    | Mode::CommandPalette
                    | Mode::Clarify
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

        // Handle '/' as the command trigger (like gemini cli / slash commands)
        if matches!(key.code, KeyCode::Char('/'))
            && !matches!(
                self.mode,
                Mode::CommandPalette | Mode::Ask | Mode::Settings | Mode::Clarify
            )
            && self.goal_prompt.is_empty()
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
                Mode::Ask
                    | Mode::Settings
                    | Mode::Memory
                    | Mode::History
                    | Mode::CommandPalette
                    | Mode::Clarify
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
                    return self.request_quit();
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
                KeyCode::Char('v') | KeyCode::Char('V') => {
                    return Some(Intent::PasteClipboard);
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
            Mode::Clarify => self.handle_clarify_key(key),
        }
    }

    fn handle_clarify_key(&mut self, key: KeyEvent) -> Option<Intent> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            return self.request_quit();
        }

        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Goal;
                self.clarifying_goal_idx = None;
                self.clarifying_question_idx = 0;
                self.clarifying_answers.clear();
                self.clarifying_buffer.clear();
                self.clarifying_cursor = 0;
                None
            }
            KeyCode::Enter => {
                let ans = self.clarifying_buffer.trim().to_string();
                if ans.is_empty() {
                    return None;
                }
                self.clarifying_answers.push(ans);
                self.clarifying_buffer.clear();
                self.clarifying_cursor = 0;

                let goal_idx = self.clarifying_goal_idx.unwrap_or(self.selected);
                let questions = if let Some(g) = self.goals.get(goal_idx) {
                    if let Some(state) = &g.state {
                        if let Some(contract) = &state.goal_contract {
                            contract.clarification_questions.clone()
                        } else {
                            Vec::new()
                        }
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                };

                if self.clarifying_question_idx + 1 < questions.len() {
                    self.clarifying_question_idx += 1;
                } else {
                    // All answered! Rerun planning.
                    let goal_idx = self.clarifying_goal_idx.unwrap_or(self.selected);
                    if let Some(g) = self.goals.get_mut(goal_idx) {
                        let original_desc = g.description.clone();
                        let mut refined = original_desc;
                        refined.push_str("\n\nRefined requirements:");
                        for (q, a) in questions.iter().zip(&self.clarifying_answers) {
                            refined.push_str(&format!("\n- Q: {}\n  A: {}", q, a));
                        }
                        g.description = refined.clone();

                        // Clear the failed status and previous checkpoints/states so it runs clean
                        g.status = TaskStatus::Queued;
                        g.state = None;
                        g.checkpoint_cursor = None;
                        g.flight_log.clear();

                        let prompt = SubmittedPrompt {
                            description: refined.clone(),
                            display_text: refined,
                            prompt_artifacts: Vec::new(),
                        };

                        // We reset clarify state
                        self.mode = Mode::Goal;
                        self.clarifying_goal_idx = None;
                        self.clarifying_question_idx = 0;
                        self.clarifying_answers.clear();
                        self.clarifying_buffer.clear();
                        self.clarifying_cursor = 0;

                        // Rerun the goal!
                        return Some(Intent::QueueGoal(prompt));
                    }
                    self.mode = Mode::Goal;
                }
                None
            }
            KeyCode::Backspace => {
                self.clarifying_cursor =
                    delete_char_before(&mut self.clarifying_buffer, self.clarifying_cursor);
                None
            }
            KeyCode::Left => {
                self.clarifying_cursor = self.clarifying_cursor.saturating_sub(1);
                None
            }
            KeyCode::Right => {
                if self.clarifying_cursor < self.clarifying_buffer.chars().count() {
                    self.clarifying_cursor += 1;
                }
                None
            }
            KeyCode::Char(c) => {
                self.clarifying_cursor =
                    insert_char_at(&mut self.clarifying_buffer, self.clarifying_cursor, c);
                None
            }
            _ => None,
        }
    }

    fn handle_mcp_approval_key(&mut self, key: KeyEvent) -> Option<Intent> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            return self.request_quit();
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

    pub fn handle_paste(&mut self, text: String) -> Option<Intent> {
        if text.trim().is_empty() {
            return None;
        }

        if self.mode == Mode::Settings {
            self.paste_into_settings(&text);
            return None;
        }

        match self.mode {
            Mode::Goal | Mode::Task => {
                if looks_like_api_key(text.trim()) {
                    self.help_open = false;
                    self.settings.message = Some(
                        "That looked like an API key - open Settings (/settings) and paste it into the API Key field, not the Goal bar."
                            .into(),
                    );
                    self.mode = Mode::Settings;
                    return None;
                }
                if contains_likely_secret(&text) {
                    self.goal_prompt.set_notice(
                        "Pasted text looked like it contains a secret; it was not attached.",
                    );
                    return None;
                }
                self.goal_prompt.handle_paste(&text);
                self.prompt_artifact_selected =
                    self.goal_prompt.artifacts().len().saturating_sub(1);
            }
            Mode::Ask => {
                for c in prompt_buffer::normalize_paste(&text).chars() {
                    self.ask_cursor = insert_char_at(&mut self.ask_input, self.ask_cursor, c);
                }
            }
            Mode::CommandPalette => {
                self.palette_input.push_str(
                    prompt_buffer::normalize_paste(&text)
                        .lines()
                        .next()
                        .unwrap_or(""),
                );
                self.palette_selected = 0;
            }
            Mode::Settings => {}
            Mode::Memory | Mode::History => {}
            Mode::Clarify => {
                for c in prompt_buffer::normalize_paste(&text).chars() {
                    self.clarifying_cursor =
                        insert_char_at(&mut self.clarifying_buffer, self.clarifying_cursor, c);
                }
            }
        }
        None
    }

    fn paste_into_settings(&mut self, text: &str) {
        let value = prompt_buffer::normalize_paste(text);
        let value = value.trim_end_matches('\n');
        match self.settings.active_field {
            SettingsField::Provider => self.settings.provider.push_str(value),
            SettingsField::Model => {
                self.settings.model.push_str(value);
                self.settings.model_ok = None;
            }
            SettingsField::ApiKey => self.settings.api_key.push_str(value.trim()),
            SettingsField::AccountId => self.settings.account_id.push_str(value.trim()),
            SettingsField::BaseUrl => self.settings.base_url.push_str(value.trim()),
            SettingsField::MaxTokens => self.settings.max_tokens.push_str(
                &value
                    .chars()
                    .filter(|c| c.is_ascii_digit())
                    .collect::<String>(),
            ),
            SettingsField::MaxUsdCents => self.settings.max_usd_cents.push_str(
                &value
                    .chars()
                    .filter(|c| c.is_ascii_digit())
                    .collect::<String>(),
            ),
        }
        self.settings.message = None;
    }

    fn handle_prompt_artifacts_key(&mut self, key: KeyEvent) -> Option<Intent> {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => {
                self.prompt_artifacts_open = false;
                None
            }
            KeyCode::Up => {
                self.prompt_artifact_selected = self.prompt_artifact_selected.saturating_sub(1);
                None
            }
            KeyCode::Down => {
                if self.prompt_artifact_selected + 1 < self.goal_prompt.artifacts().len() {
                    self.prompt_artifact_selected += 1;
                }
                None
            }
            KeyCode::Delete | KeyCode::Backspace => {
                self.goal_prompt
                    .remove_artifact(self.prompt_artifact_selected);
                self.prompt_artifact_selected = self
                    .prompt_artifact_selected
                    .min(self.goal_prompt.artifacts().len().saturating_sub(1));
                if self.goal_prompt.artifacts().is_empty() {
                    self.prompt_artifacts_open = false;
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
            "Paste Clipboard",
            "Artifacts",
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
                    "Paste Clipboard" => {
                        self.mode = self.prev_mode;
                        return Some(Intent::PasteClipboard);
                    }
                    "Artifacts" => {
                        self.prompt_artifacts_open = true;
                        self.mode = self.prev_mode;
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
        // Intercept 'c' or 'C' when prompt is empty to start clarification if questions exist.
        if matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
            && !key.modifiers.contains(KeyModifiers::CONTROL)
            && self.goal_prompt.is_empty()
        {
            if let Some(g) = self.goals.get(self.selected) {
                if let Some(state) = &g.state {
                    if let Some(contract) = &state.goal_contract {
                        if !contract.clarification_questions.is_empty() {
                            self.mode = Mode::Clarify;
                            self.clarifying_goal_idx = Some(self.selected);
                            self.clarifying_question_idx = 0;
                            self.clarifying_answers.clear();
                            self.clarifying_buffer.clear();
                            self.clarifying_cursor = 0;
                            return None;
                        }
                    }
                }
            }
        }

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
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('w') => {
                    self.goal_prompt.delete_word_before();
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
                _ => {}
            }
        }
        match key.code {
            KeyCode::Enter => {
                let prompt = self.goal_prompt.submit()?;
                // Guard against the user pasting an API key into the
                // goal bar (it has happened — see issue with AIza... in
                // the wild). Keys would otherwise be sent verbatim to
                // the LLM as the user prompt, which is both unhelpful
                // and a credential leak. Detect, refuse, and surface a
                // clear redirect to Settings instead.
                if looks_like_api_key(prompt.description.trim()) {
                    let pasted = prompt.description.trim().to_string();
                    self.help_open = false;
                    if let Some(provider) = provider_for_key_prefix(&pasted) {
                        self.settings.provider = provider.into();
                        self.settings.model.clear();
                    }
                    self.settings.api_key = pasted;
                    self.settings.message = Some(format!(
                        "That looked like an API key, so it went here instead of to a model. \
                         Provider: {}. Check it, then Enter to save.",
                        self.settings.provider
                    ));
                    self.mode = Mode::Settings;
                    return None;
                }
                if !self.model_ready {
                    self.goal_prompt.insert_text(&prompt.display_text);
                    self.help_open = false;
                    self.settings.message = Some(
                        "Add a model first: paste an API key into the Key field and press \
                         Enter, or quit and run `phonton models setup` to run on this machine."
                            .into(),
                    );
                    self.mode = Mode::Settings;
                    return None;
                }
                let direct_task = self.mode == Mode::Task;
                if self.host_checks_approved.is_none() {
                    self.pending_host_goal = Some((prompt, direct_task));
                    return None;
                }
                Some(self.queue_goal(prompt, direct_task))
            }
            KeyCode::Backspace => {
                self.goal_prompt.delete_char_before();
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
                self.selected = self.selected.saturating_sub(1);
                None
            }
            KeyCode::Down => {
                if self.selected + 1 < self.goals.len() {
                    self.selected += 1;
                }
                None
            }
            KeyCode::Char(c) => {
                self.goal_prompt.insert_char(c);
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
    QueueGoal(SubmittedPrompt),
    /// User submitted a direct single subtask. Skips planner decomposition.
    QueueTask(SubmittedPrompt),
    /// User submitted an ask-mode question. Isolated from goal context.
    Ask(String),
    /// User approved or denied a pending MCP approval prompt.
    ResolveMcpApproval {
        approval_id: u64,
        approved: bool,
    },
    ResolveLocalPlan {
        task_id: TaskId,
        approved: bool,
    },
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
    /// Import the OS clipboard into the active prompt field.
    PasteClipboard,
    /// User accepted the workspace-trust prompt — proceed into the TUI.
    AcceptTrust,
    /// User declined trust. Caller should exit cleanly.
    DeclineTrust,
    /// Apply the selected goal's reviewed local candidate.
    ApplyLocal(TaskId),
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
        return "  est. $—  |  est. saved — vs frontier  |  tokens: —".into();
    };
    if s.cost_receipt.frontier_equivalent_usd_micros > 0 {
        let pct = s
            .cost_receipt
            .saved_percent()
            .map(|p| format!("{p}%"))
            .unwrap_or_else(|| "—".into());
        return format!(
            "  est. {}  |  est. saved {} vs frontier  |  {} tok",
            format_usd_micros(s.cost_receipt.actual_usd_micros),
            pct,
            s.tokens_used
        );
    }
    let pct = savings_pct(s);
    let pct_txt = pct.map(|p| format!("{p}%")).unwrap_or_else(|| "—".into());
    format!(
        "  {} tok  |  saved {} vs naive  |  Σ baseline: {}",
        s.tokens_used, pct_txt, s.estimated_naive_tokens
    )
}

fn format_usd_micros(micros: u64) -> String {
    format!("${:.3}", micros as f64 / 1_000_000.0)
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
            "  est. $—  |  est. saved — vs frontier  |  tokens: —",
            Style::default().fg(MUTED),
        ));
    };
    if s.cost_receipt.frontier_equivalent_usd_micros > 0 {
        let pct = s.cost_receipt.saved_percent();
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
        return Line::from(vec![
            Span::styled(
                format!(
                    "  est. {}",
                    format_usd_micros(s.cost_receipt.actual_usd_micros)
                ),
                Style::default().fg(ACCENT_HI).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  est. saved ", Style::default().fg(MUTED)),
            Span::styled(pct_txt, pct_style),
            Span::styled(" vs frontier  ", Style::default().fg(MUTED)),
            Span::styled(format!("{} tok", s.tokens_used), Style::default().fg(MUTED)),
        ]);
    }
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
    let want_full_logo =
        app.goals.is_empty() && area.width >= LOGO_WIDTH_THRESHOLD && area.height >= 24;
    let splash_h: u16 = if want_full_logo {
        art::LOGO_ROWS + 1
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
    if app.prompt_artifacts_open {
        render_prompt_artifacts_drawer(frame, area, app);
    }
    if !app.pending_mcp_approvals.is_empty() {
        render_mcp_approval(frame, area, app);
    }
    if app.pending_host_goal.is_some() {
        render_host_checks_prompt(frame, area);
    }
    if let Some(prompt) = app.pending_local_plans.first() {
        render_local_plan(frame, area, prompt);
    }
}

/// Once-per-session question before the first goal: verification runs the
/// project's own build and test commands, which is not sandboxed.
fn render_host_checks_prompt(frame: &mut Frame, area: Rect) {
    let w = 72.min(area.width.saturating_sub(4));
    let h = 11.min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    frame.render_widget(Clear, popup);
    let key = Style::default().fg(ACCENT_HI).add_modifier(Modifier::BOLD);
    let muted = Style::default().fg(MUTED);
    let lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "  Verify runs your project's own checks (build, tests)",
            Style::default().fg(PAPER),
        )),
        Line::from(Span::styled(
            "  on this machine with your user permissions.",
            Style::default().fg(PAPER),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "  No isolation backend is configured. Without approval, Phonton",
            muted,
        )),
        Line::from(Span::styled(
            "  still plans and writes diffs, but cannot mark them verified.",
            muted,
        )),
        Line::raw(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("Y", key),
            Span::styled(" allow for this session   ", muted),
            Span::styled("N", key),
            Span::styled(" run without checks   ", muted),
            Span::styled("Esc", key),
            Span::styled(" edit goal", muted),
        ]),
    ];
    let block = Block::default()
        .title(Span::styled(" Run checks on this machine? ", key))
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(BG_DEEP));
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

/// Centred modal listing every keybinding in one place. Toggled by `?`,
/// dismissed with `?` again or `Esc`. Drawn last so it always sits on top.
fn render_help(frame: &mut Frame, area: Rect) {
    let rows: &[(&str, &str)] = &[
        ("Enter", "submit goal / question"),
        ("/", "open the command palette"),
        ("@", "attach a file, folder, symbol or MCP server"),
        ("?", "toggle this help"),
        ("Ctrl+V", "paste clipboard as text or artifact"),
        ("Ctrl+;", "toggle the Ask side panel"),
        ("Shift+L", "toggle the Flight Log"),
        ("Ctrl+D", "delete the selected goal"),
        ("Ctrl+U / Ctrl+K", "clear before/after cursor"),
        ("Ctrl+W", "delete the previous word in the input"),
        ("↑ / ↓", "move selection in Goals (or palette)"),
        ("← / →", "move caret in the input bar"),
        ("Home / End", "jump to start/end of the input"),
        ("Ctrl+↑↓", "move the checkpoint cursor"),
        ("r", "rollback to the highlighted checkpoint (input empty)"),
        ("Ctrl+C", "quit (press twice)"),
        ("Esc", "close overlay / cancel / quit (press twice)"),
    ];

    // Fit the modal to the longest description so wrapping never bites.
    // Row = 2 indent + padded key column + 3 gap + description.
    let key_col = rows
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(8);
    let longest = rows
        .iter()
        .map(|(_, v)| 2 + key_col + 3 + v.chars().count())
        .max()
        .unwrap_or(40);
    let popup_w = (longest as u16 + 4)
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
            Span::styled((*v).to_string(), Style::default().fg(PAPER)),
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

fn render_prompt_artifacts_drawer(frame: &mut Frame, area: Rect, app: &App) {
    let popup_w = 72u16.min(area.width.saturating_sub(2)).max(32);
    let popup_h = 18u16.min(area.height.saturating_sub(2)).max(8);
    let popup_area = Rect {
        x: area.x + (area.width.saturating_sub(popup_w)) / 2,
        y: area.y + (area.height.saturating_sub(popup_h)) / 2,
        width: popup_w,
        height: popup_h,
    };

    frame.render_widget(Clear, popup_area);
    let block = Block::default()
        .title(Span::styled(
            " Prompt Artifacts ",
            Style::default().fg(ACCENT_HI).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(BG_DEEP));
    frame.render_widget(block.clone(), popup_area);
    let inner = block.inner(popup_area);

    let artifacts = app.goal_prompt.artifacts();
    if artifacts.is_empty() {
        frame.render_widget(
            Paragraph::new("No pending prompt artifacts.")
                .style(Style::default().fg(MUTED).bg(BG_DEEP))
                .alignment(Alignment::Center),
            inner,
        );
        return;
    }

    let selected = app
        .prompt_artifact_selected
        .min(artifacts.len().saturating_sub(1));
    let mut lines = Vec::new();
    for (idx, artifact) in artifacts.iter().enumerate() {
        let style = if idx == selected {
            Style::default()
                .fg(BG_DEEP)
                .bg(ACCENT_HI)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(PAPER).bg(BG_DEEP)
        };
        lines.push(Line::from(Span::styled(
            format!("{} {}", idx + 1, artifact.label),
            style,
        )));
        lines.push(Line::from(Span::styled(
            format!("   {}", artifact_preview(artifact, inner.width as usize)),
            Style::default().fg(MUTED).bg(BG_DEEP),
        )));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Backspace/Delete removes selected - Esc closes",
        Style::default().fg(MUTED).bg(BG_DEEP),
    )));

    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(BG_DEEP))
            .wrap(Wrap { trim: true }),
        inner,
    );
}

fn artifact_preview(artifact: &PromptArtifact, width: usize) -> String {
    let preview = artifact
        .text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    short(preview, width.saturating_sub(4).max(12))
}

/// Scrollable exact scope, model and commands; Y is the only approval key.
fn render_local_plan(
    frame: &mut Frame,
    area: Rect,
    prompt: &local_plan_approval::PendingLocalPlan,
) {
    let width = area.width.saturating_sub(4).clamp(1, 108);
    let height = area.height.saturating_sub(2).clamp(1, 34);
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(" Review local plan ")
        .title_bottom(" Y approve plan · N/Esc cancel · ↑↓/PgUp/PgDn scroll ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(BG_DEEP).fg(PAPER));
    let inner = block.inner(popup);
    let lines: Vec<Line> = prompt
        .wrapped_lines(inner.width.max(1) as usize)
        .into_iter()
        .map(Line::raw)
        .collect();
    let max_scroll = lines
        .len()
        .saturating_sub(inner.height as usize)
        .min(u16::MAX as usize) as u16;
    frame.render_widget(block, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((prompt.scroll.min(max_scroll), 0))
            .style(Style::default().bg(BG_DEEP).fg(PAPER)),
        inner,
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
                Style::default().fg(PAPER).add_modifier(Modifier::BOLD),
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
            Span::styled(line, Style::default().fg(PAPER)),
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
    if area.height > art::LOGO_ROWS && area.width >= LOGO_WIDTH_THRESHOLD {
        let mut lines: Vec<Line> = Vec::with_capacity(art::LOGO_ROWS as usize + 1);
        lines.push(Line::raw(""));
        lines.extend(art::logo(app.tick()));
        let p = Paragraph::new(lines)
            .alignment(Alignment::Center)
            .style(Style::default().bg(BG_DEEP));
        frame.render_widget(p, area);
    } else {
        // Compact one-line header: wordmark, version, where the model runs.
        let local = provider_is_local(&app.settings.provider, &app.settings.base_url);
        let spans = vec![
            Span::styled(
                "φ ",
                Style::default()
                    .fg(art::spectrum(0.35))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "phonton",
                Style::default().fg(PAPER).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                concat!(" ", env!("CARGO_PKG_VERSION")),
                Style::default().fg(DIM),
            ),
            Span::styled("  ·  ", Style::default().fg(RULE)),
            Span::styled(
                if local { "local" } else { "cloud" },
                Style::default().fg(if local { SUCCESS } else { WARN }),
            ),
            Span::styled(
                format!(" · {}", display_model(app)),
                Style::default().fg(MUTED),
            ),
        ];
        let p = Paragraph::new(Line::from(spans))
            .alignment(Alignment::Center)
            .style(Style::default().bg(BG_DEEP));
        frame.render_widget(p, area);
    }
}

/// Model label for headers: the configured model, else the calibrated local
/// model, else the provider default.
fn display_model(app: &App) -> String {
    if !app.settings.model.trim().is_empty() {
        app.settings.model.clone()
    } else if let Some(m) = app
        .machine
        .local_model
        .as_ref()
        .filter(|_| provider_is_local(&app.settings.provider, &app.settings.base_url))
    {
        m.clone()
    } else {
        default_model_for(&app.settings.provider)
    }
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    let key = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
    let txt = Style::default().fg(MUTED);
    let dim = Style::default().fg(DIM);
    let sep = Span::styled("   ", dim);

    if let Some(goal) = app.goals.get(app.selected) {
        if matches!(goal.status, TaskStatus::Paused { .. }) {
            let hint = Line::from(vec![
                Span::styled("paused", Style::default().fg(Color::Rgb(255, 200, 0))),
                Span::styled(
                    format!("  resume: phonton goal --resume {}", goal.task_id),
                    txt,
                ),
            ]);
            frame.render_widget(Paragraph::new(hint).alignment(Alignment::Center), area);
            return;
        }
    }

    if app.quit_armed() {
        let hint = Line::from(vec![
            Span::styled("Esc", key),
            Span::styled(" or ", txt),
            Span::styled("Ctrl+C", key),
            Span::styled(" again to quit", txt),
            Span::styled("  ·  any other key keeps working", dim),
        ]);
        frame.render_widget(Paragraph::new(hint).alignment(Alignment::Center), area);
        return;
    }

    // Hints in priority order; the last one (quit/close) is always kept and
    // lower-priority hints drop off the tail when the terminal is narrow.
    let can_apply = app.current_goal().is_some_and(GoalEntry::can_apply);
    let hints: &[(&str, &str)] = match app.mode {
        Mode::Goal | Mode::Task if can_apply => &[
            ("Ctrl+Y", "apply"),
            ("Enter", "run"),
            ("/", "commands"),
            ("?", "help"),
            ("Shift+L", "log"),
            ("Esc", "quit"),
        ],
        Mode::Goal | Mode::Task => &[
            ("Enter", "run"),
            ("/", "commands"),
            ("?", "help"),
            ("@", "attach"),
            ("Ctrl+;", "ask"),
            ("Shift+L", "log"),
            ("Ctrl+V", "paste"),
            ("Ctrl+D", "delete"),
            ("Esc", "quit"),
        ],
        Mode::Ask => &[
            ("Enter", "send"),
            ("Ctrl+;", "close ask"),
            ("Esc", "cancel"),
        ],
        Mode::Settings => &[
            ("Enter", "save"),
            ("Tab", "next field"),
            ("←→", "cycle provider"),
            ("Esc", "cancel"),
        ],
        Mode::Memory | Mode::History => &[("/", "commands"), ("Esc", "back to goals")],
        Mode::CommandPalette => &[
            ("type", "filter"),
            ("↑↓", "select"),
            ("Enter", "run"),
            ("Esc", "close"),
        ],
        Mode::Clarify => &[("Enter", "submit answer"), ("Esc", "cancel clarification")],
    };
    let spans = fit_footer_hints(hints, area.width as usize, key, txt, sep);

    let p = Paragraph::new(Line::from(spans)).alignment(Alignment::Center);
    frame.render_widget(p, area);
}

/// Lay out `(key, label)` footer hints, dropping lower-priority hints from the
/// middle of the list until the row fits `width`. The final hint is kept.
fn fit_footer_hints(
    hints: &[(&str, &str)],
    width: usize,
    key: Style,
    txt: Style,
    sep: Span<'static>,
) -> Vec<Span<'static>> {
    let cost = |h: &[(&str, &str)]| -> usize {
        h.iter()
            .map(|(k, l)| k.chars().count() + 1 + l.chars().count())
            .sum::<usize>()
            + h.len().saturating_sub(1) * sep.content.chars().count()
    };
    let mut kept: Vec<(&str, &str)> = hints.to_vec();
    while kept.len() > 2 && cost(&kept) > width {
        kept.remove(kept.len() - 2);
    }
    let mut spans = Vec::new();
    for (i, (k, l)) in kept.iter().enumerate() {
        if i > 0 {
            spans.push(sep.clone());
        }
        spans.push(Span::styled((*k).to_string(), key));
        spans.push(Span::styled(format!(" {l}"), txt));
    }
    spans
}

fn render_palette(frame: &mut Frame, area: Rect, app: &App) {
    let all_options = vec![
        "Goal Mode",
        "Task Mode",
        "Ask Mode",
        "Memory",
        "History",
        "Settings",
        "Paste Clipboard",
        "Artifacts",
        "Toggle Log",
        "Help",
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
            Span::styled("◆ ", Style::default().fg(QUIET)),
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
                ("▍", Style::default().fg(PAPER).add_modifier(Modifier::BOLD))
            } else {
                (" ", Style::default().fg(MUTED))
            };
            let mut spans = vec![
                Span::styled(marker, Style::default().fg(ACCENT)),
                Span::raw(" "),
            ];
            spans.extend(status_tag_spans(&g.status, app.spinner_frame));
            // One photon per concurrently active worker, capped at 5.
            let active_count = g
                .state
                .as_ref()
                .map(|s| s.active_workers.len())
                .unwrap_or(0);
            if active_count > 1 {
                spans.push(Span::raw(" "));
                let visible = active_count.min(5);
                for i in 0..visible {
                    spans.push(Span::styled(
                        art::spinner(app.spinner_frame.wrapping_add(i * 3)).to_string(),
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
            let used: usize = spans.iter().map(|s| s.width()).sum();
            let text_w = (area.width as usize).saturating_sub(used + 3).max(12);
            spans.push(Span::styled(short(&g.description, text_w), base_style));
            ListItem::new(Line::from(spans))
        })
        .collect();
    let goals_focused = matches!(app.mode, Mode::Goal | Mode::Task);
    let block = Block::default()
        .title(Span::styled(
            if app.goals.is_empty() {
                " runs ".to_string()
            } else {
                format!(" runs · {} ", app.goals.len())
            },
            Style::default().fg(PAPER).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if goals_focused { DIM } else { RULE }))
        .style(Style::default().bg(BG_PANEL));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(9)])
        .split(area);

    if app.goals.is_empty() {
        // Empty board: the φ, drawn from the logo's own pixels.
        let inner = block.inner(chunks[0]);
        frame.render_widget(block, chunks[0]);
        let mut lines: Vec<Line> = Vec::new();
        let hint = [
            Line::from(Span::styled("No goals yet.", Style::default().fg(MUTED))),
            Line::from(Span::styled("Type one below ↓", Style::default().fg(DIM))),
        ];
        let phi_h = art::PHI_HEIGHT;
        if inner.width >= art::PHI_WIDTH && inner.height > phi_h {
            // Centre the φ and as much of the hint as fits under it.
            let room = (inner.height - phi_h - 1).min(2) as usize;
            let pad = inner.height.saturating_sub(phi_h + 1 + room as u16) / 2;
            lines.extend((0..pad).map(|_| Line::raw("")));
            lines.extend(art::phi(app.tick()));
            lines.push(Line::raw(""));
            lines.extend(hint.into_iter().take(room));
        } else {
            lines.push(Line::raw(""));
            lines.extend(hint);
        }
        frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), inner);
    } else {
        frame.render_widget(List::new(items).block(block), chunks[0]);
    }

    render_machine(frame, chunks[1], app);
}

/// Side panel: what this machine brings to a run, and the run record.
fn render_machine(frame: &mut Frame, area: Rect, app: &App) {
    let label = |s: &str| Span::styled(format!(" {s:<7}"), Style::default().fg(MUTED));
    let value = |s: String| Span::styled(s, Style::default().fg(PAPER));
    let gib = |b: u64| b as f64 / 1_073_741_824.0;
    let local = provider_is_local(&app.settings.provider, &app.settings.base_url);
    let width = area.width.saturating_sub(11) as usize;

    let mut lines = vec![Line::from(vec![
        label("where"),
        Span::styled(
            if local { "this machine" } else { "cloud" },
            Style::default()
                .fg(if local { SUCCESS } else { WARN })
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" · {}", app.settings.provider),
            Style::default().fg(DIM),
        ),
    ])];
    lines.push(Line::from(vec![
        label("model"),
        value(short(&display_model(app), width)),
    ]));
    if local {
        let calibrated = match (&app.machine.local_model, app.machine.context_tokens) {
            (Some(_), Some(ctx)) => format!(
                "{} · {} ctx",
                app.machine.protocol.as_deref().unwrap_or("calibrated"),
                art::thousands(ctx as u64)
            ),
            _ if !app.machine.probed => "checking…".to_string(),
            _ => "not calibrated · phonton models".to_string(),
        };
        lines.push(Line::from(vec![
            label("edits"),
            Span::styled(short(&calibrated, width), Style::default().fg(MUTED)),
        ]));
    }
    match &app.machine.gpu {
        Some((name, free, total)) if *total > 0 => {
            let mut spans = vec![label("vram")];
            let bar = 8.min(width.saturating_sub(12));
            spans.extend(art::gauge(1.0 - *free as f32 / *total as f32, bar, MUTED));
            spans.push(value(format!(
                " {:.1}/{:.1}G",
                gib(total.saturating_sub(*free)),
                gib(*total)
            )));
            lines.push(Line::from(spans));
            lines.push(Line::from(vec![
                label(""),
                Span::styled(short(name, width), Style::default().fg(DIM)),
            ]));
        }
        _ => {
            if let Some((free, total)) = app.machine.ram {
                lines.push(Line::from(vec![
                    label("ram"),
                    value(format!("{:.1} GiB free of {:.1}", gib(free), gib(total))),
                ]));
            }
        }
    }
    let r = &app.record;
    lines.push(Line::from(vec![
        label("record"),
        Span::styled(
            format!("{} verified", art::thousands(r.verified_runs)),
            Style::default().fg(if r.verified_runs > 0 { SUCCESS } else { MUTED }),
        ),
        Span::styled(
            format!(" · streak {}", r.streak),
            Style::default().fg(if r.streak > 0 { PAPER } else { DIM }),
        ),
    ]));
    lines.push(Line::from(vec![
        label(""),
        Span::styled(
            format!("{} tokens kept local", art::thousands(r.local_tokens)),
            Style::default().fg(DIM),
        ),
    ]));

    let p = Paragraph::new(lines)
        .style(Style::default().bg(BG_PANEL))
        .block(
            Block::default()
                .title(Span::styled(
                    concat!(" machine · v", env!("CARGO_PKG_VERSION"), " "),
                    Style::default().fg(PAPER).add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(RULE))
                .style(Style::default().bg(BG_PANEL)),
        );
    frame.render_widget(p, area);
}

fn render_centre(frame: &mut Frame, area: Rect, app: &App) {
    let has_active = app
        .current_goal()
        .and_then(|g| g.state.as_ref())
        .is_some_and(|s| !s.active_workers.is_empty());
    let pulse_color = if has_active {
        art::spectrum(((app.spinner_frame / 3) % 20) as f32 / 19.0)
    } else {
        MUTED
    };
    let block = Block::default()
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                if has_active {
                    art::spinner(app.spinner_frame).to_string()
                } else {
                    "○".to_string()
                },
                Style::default().fg(pulse_color),
            ),
            Span::styled(
                if app.current_goal().is_some() {
                    " run "
                } else {
                    " welcome "
                },
                Style::default().fg(PAPER).add_modifier(Modifier::BOLD),
            ),
        ]))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if has_active { DIM } else { RULE }))
        .style(Style::default().bg(BG_PANEL));

    let Some(g) = app.current_goal() else {
        let head = Style::default().fg(PAPER).add_modifier(Modifier::BOLD);
        let muted = Style::default().fg(MUTED);
        let dim = Style::default().fg(DIM);
        let key = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
        let item = |k: &'static str, body: &'static str| {
            Line::from(vec![
                Span::styled("  › ", Style::default().fg(ACCENT)),
                Span::styled(format!("{k:<24}"), Style::default().fg(PAPER)),
                Span::styled(body, muted),
            ])
        };
        let local = provider_is_local(&app.settings.provider, &app.settings.base_url);
        let local_line = match (&app.machine.local_model, local) {
            (None, true) if !app.machine.probed => Line::from(vec![
                Span::styled(
                    format!("  {} ", art::spinner(app.spinner_frame)),
                    Style::default().fg(ACCENT),
                ),
                Span::styled("Checking the local model on this machine…", muted),
            ]),
            (Some(model), true) => Line::from(vec![
                Span::styled("  ● ", Style::default().fg(SUCCESS)),
                Span::styled(
                    model.clone(),
                    Style::default().fg(PAPER).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" runs on this machine", muted),
                Span::styled(
                    app.machine
                        .context_tokens
                        .map(|c| format!(" · {} ctx calibrated", art::thousands(c as u64)))
                        .unwrap_or_default(),
                    dim,
                ),
            ]),
            (None, true) => Line::from(vec![
                Span::styled("  ○ ", Style::default().fg(WARN)),
                Span::styled("No calibrated local model yet. ", muted),
                Span::styled("phonton models", key),
                Span::styled(" picks one for your GPU.", muted),
            ]),
            (_, false) if !app.model_ready => Line::from(vec![
                Span::styled("  ○ ", Style::default().fg(WARN)),
                Span::styled("No model yet. Paste an API key, or run ", muted),
                Span::styled("phonton models setup", key),
                Span::styled(" to use this machine.", muted),
            ]),
            (_, false) => Line::from(vec![
                Span::styled("  ● ", Style::default().fg(SUCCESS)),
                Span::styled(
                    format!("{} ", app.settings.provider),
                    Style::default().fg(PAPER).add_modifier(Modifier::BOLD),
                ),
                Span::styled("ready. ", muted),
                Span::styled("phonton models", key),
                Span::styled(" adds a model on this machine.", muted),
            ]),
        };
        let inner_w = area.width.saturating_sub(2);
        let lines = vec![
            Line::raw(""),
            Line::from(vec![
                Span::styled("  phonton", head),
                Span::styled(" — the local-first ADE that proves its work.", muted),
            ]),
            Line::raw(""),
            {
                let mut track = vec![Span::raw("  ")];
                track.extend(art::loop_track(art::Track::Idle, app.tick(), inner_w).spans);
                Line::from(track)
            },
            Line::raw(""),
            local_line,
            Line::raw(""),
            Line::from(Span::styled("  Every run gets", head)),
            item("a plan first", "files, checks and budget before any edit"),
            item("verified diffs", "your build and tests gate review"),
            item("a receipt", "tokens, time, cost, what left the machine"),
            Line::raw(""),
            Line::from(Span::styled("  Try", head)),
            item("Fix the failing test in", "@tests/…"),
            item("Add input validation to", "@src/…"),
            item("Explain this repo", "Ctrl+; asks without editing"),
        ];
        let p = Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false });
        frame.render_widget(p, area);
        return;
    };

    let mut show_clarify = false;
    let mut questions = Vec::new();
    if let Some(state) = &g.state {
        if let Some(contract) = &state.goal_contract {
            if !contract.clarification_questions.is_empty() {
                show_clarify = true;
                questions = contract.clarification_questions.clone();
            }
        }
    }

    if show_clarify {
        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(Line::from(vec![
            Span::styled(
                "goal: ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(g.description.clone()),
        ]));
        lines.push(Line::raw(""));

        if app.mode == Mode::Clarify {
            lines.push(Line::from(Span::styled(
                "📝 Interactive Clarification Questionnaire",
                Style::default().fg(ACCENT_HI).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::raw(""));

            // Render previous answered questions
            for (idx, (q, a)) in questions.iter().zip(&app.clarifying_answers).enumerate() {
                lines.push(Line::from(vec![
                    Span::styled(format!("  Q{}: ", idx + 1), Style::default().fg(MUTED)),
                    Span::styled(q.clone(), Style::default().fg(MUTED)),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("  Answer: ", Style::default().fg(SUCCESS)),
                    Span::styled(a.clone(), Style::default().fg(SUCCESS)),
                ]));
                lines.push(Line::raw(""));
            }

            // Render current question
            let q_idx = app.clarifying_question_idx;
            if q_idx < questions.len() {
                lines.push(Line::from(vec![Span::styled(
                    format!("  Question {} of {}:", q_idx + 1, questions.len()),
                    Style::default().fg(WARN).add_modifier(Modifier::BOLD),
                )]));
                lines.push(Line::from(vec![
                    Span::styled(
                        "  ▸ ",
                        Style::default().fg(ACCENT_HI).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        questions[q_idx].clone(),
                        Style::default().fg(PAPER).add_modifier(Modifier::BOLD),
                    ),
                ]));
                lines.push(Line::raw(""));
                lines.push(Line::from(vec![
                    Span::styled("  Your Answer: ", Style::default().fg(ACCENT_HI)),
                    Span::styled(app.clarifying_buffer.clone(), Style::default().fg(PAPER)),
                    Span::styled("█", Style::default().fg(pulse_color)),
                ]));
            }
        } else {
            lines.push(Line::from(Span::styled(
                "⚠️  Requirements Clarification Required",
                Style::default().fg(WARN).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(Span::styled(
                format!(
                    "  Goal confidence is too low ({}%) due to under-specified requirements.",
                    g.state
                        .as_ref()
                        .and_then(|s| s.goal_contract.as_ref())
                        .map(|c| c.confidence_percent)
                        .unwrap_or(0)
                ),
                Style::default().fg(PAPER),
            )));
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "  Phonton needs clarification on the following questions:",
                Style::default().fg(ACCENT),
            )));
            for (idx, q) in questions.iter().enumerate() {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("    {}. ", idx + 1),
                        Style::default().fg(ACCENT_HI).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(q.clone(), Style::default().fg(PAPER)),
                ]));
            }
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "  [ Press 'C' to start answering clarification questions & proceed ]",
                Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
            )));
        }

        let p = Paragraph::new(lines).wrap(Wrap { trim: true }).block(block);
        frame.render_widget(p, area);
        return;
    }

    let inner_w = area.width.saturating_sub(2);
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("goal  ", Style::default().fg(MUTED)),
        Span::styled(
            g.description.clone(),
            Style::default().fg(PAPER).add_modifier(Modifier::BOLD),
        ),
    ]));
    let mut track = art::loop_track(goal_track(g), app.tick(), inner_w).spans;
    track.push(Span::styled(
        format!("   {}", fmt_elapsed(g)),
        Style::default().fg(DIM),
    ));
    lines.push(Line::from(track));
    lines.push(Line::raw(""));

    if let Some(state) = &g.state {
        for w in &state.active_workers {
            // Worker descriptions can carry a "Prior context from memory"
            // preamble for the model; show the user the task itself.
            let task =
                phonton_types::task_description_without_prior_context(&w.subtask_description);
            let mut spans = vec![
                Span::styled(
                    format!("  {} ", art::spinner(app.spinner_frame)),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    short(task.lines().next().unwrap_or(""), 52),
                    Style::default().fg(PAPER),
                ),
                Span::styled(
                    format!(
                        "  {} · {} tok",
                        if w.model_name.is_empty() {
                            w.model_tier.to_string()
                        } else {
                            w.model_name.clone()
                        },
                        art::thousands(w.tokens_used)
                    ),
                    Style::default().fg(DIM),
                ),
            ];
            if w.is_thinking {
                spans.push(Span::styled(
                    "  thinking…",
                    Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
                ));
            }
            lines.push(Line::from(spans));
        }
        if !state.active_workers.is_empty() {
            lines.push(Line::raw(""));
        }
        if state.handoff_packet.is_none() {
            append_local_attempts(&mut lines, g);
        }
        // Receipt-first: once a handoff exists it leads the pane.
        if let Some(handoff) = &state.handoff_packet {
            let width = (inner_w as usize).saturating_sub(1).clamp(40, 66);
            lines.extend(receipt_lines(app, g, state, handoff, width));
            append_local_review(&mut lines, g);
            append_handoff_lines(&mut lines, handoff);
            lines.push(Line::raw(""));
        } else if let TaskStatus::Failed { reason, .. } = &g.status {
            lines.push(Line::from(vec![
                Span::styled(
                    "✗ ",
                    Style::default().fg(DANGER).add_modifier(Modifier::BOLD),
                ),
                Span::styled(reason.clone(), Style::default().fg(PAPER)),
            ]));
            lines.push(Line::from(Span::styled(
                if g.recorded {
                    "  Streak reset. The flight log (Shift+L) has every event."
                } else {
                    "  Run record unchanged. The flight log (Shift+L) has every event."
                },
                Style::default().fg(DIM),
            )));
            lines.push(Line::raw(""));
        }
        if state.estimated_naive_tokens > 0 {
            lines.push(render_savings_line_styled(
                Some(state),
                app.best_savings_pct,
                app.new_best_ticks,
            ));
        }
        if let Some(label) = execution_mode_label(g) {
            lines.push(Line::from(vec![
                Span::styled("execution: ", Style::default().fg(MUTED)),
                Span::styled(label, Style::default().fg(WARN)),
            ]));
        }

        // Checkpoint history is inspectable; legacy reset-based rollback is
        // disabled until it can leave unrelated user work intact.
        if !state.checkpoints.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                format!(
                    "Checkpoints ({} — history only; legacy rollback disabled):",
                    state.checkpoints.len()
                ),
                Style::default().fg(PAPER).add_modifier(Modifier::BOLD),
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
                    format!("    #{:>2}  ", cp.seq)
                };
                lines.push(Line::from(vec![
                    Span::styled(marker, marker_style),
                    Span::styled(format!("{}  ", oid_short), Style::default().fg(MUTED)),
                    Span::raw(short(&cp.message, 50)),
                ]));
            }
        }
    } else {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{} ", art::spinner(app.spinner_frame)),
                Style::default().fg(ACCENT),
            ),
            Span::styled(
                "Planning — reading the workspace and drafting the goal contract…",
                Style::default().fg(MUTED),
            ),
        ]));
    }

    let p = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(block);
    frame.render_widget(p, area);
}

/// Attempts so far in a running local goal: what each tried and how its
/// checks ended, so a 30-second run is not a blank screen.
fn append_local_attempts(lines: &mut Vec<Line<'static>>, g: &GoalEntry) {
    let Some(receipt) = &g.local else { return };
    if receipt.candidates.is_empty() && receipt.baseline_checks.is_empty() {
        return;
    }
    lines.push(Line::from(Span::styled(
        "Attempts",
        Style::default().fg(PAPER).add_modifier(Modifier::BOLD),
    )));
    if !receipt.baseline_checks.is_empty() {
        let failing = receipt
            .baseline_checks
            .iter()
            .filter(|c| c.status == phonton_types::local::CheckStatus::Failed)
            .count();
        lines.push(Line::from(vec![
            Span::styled("  ○ baseline  ", Style::default().fg(MUTED)),
            Span::styled(
                if failing > 0 {
                    "checks fail on the unchanged source, as expected".to_string()
                } else {
                    "checks pass on the unchanged source".to_string()
                },
                Style::default().fg(DIM),
            ),
        ]));
    }
    for c in &receipt.candidates {
        let passed = !c.checks.is_empty()
            && c.checks
                .iter()
                .all(|k| k.status == phonton_types::local::CheckStatus::Passed);
        let (mark, color) = if c.rejection.is_none() && passed {
            ("✓", SUCCESS)
        } else if c.rejection.is_some() {
            ("✗", DANGER)
        } else {
            ("·", MUTED)
        };
        let why = local_tui::check_failures(c, 1)
            .into_iter()
            .find(|l| l.starts_with("not ok"))
            .or_else(|| c.rejection.clone())
            .unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {mark} candidate {}  ", c.number),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(short(&c.approach, 34), Style::default().fg(PAPER)),
            Span::styled(
                format!(
                    "  {:.1} s · {} tok",
                    c.elapsed_ms as f64 / 1000.0,
                    c.output_tokens.unwrap_or(0)
                ),
                Style::default().fg(DIM),
            ),
        ]));
        if !why.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("      {}", short(&why, 70)),
                Style::default().fg(if mark == "✗" { DANGER } else { MUTED }),
            )));
        }
    }
    lines.push(Line::raw(""));
}

/// The reviewed local candidate's diff and how to apply it.
fn append_local_review(lines: &mut Vec<Line<'static>>, g: &GoalEntry) {
    let Some(receipt) = &g.local else { return };
    let selected = receipt
        .selected_candidate
        .and_then(|n| receipt.candidates.iter().find(|c| c.number == n));
    // Show the most informative attempt: the selected one, else the last
    // with a diff; explain failure from the last attempt whose checks failed.
    let Some(candidate) = selected
        .or_else(|| receipt.candidates.iter().rev().find(|c| !c.diff.is_empty()))
        .or(receipt.candidates.last())
    else {
        return;
    };
    let failed_attempt = selected.or_else(|| {
        receipt.candidates.iter().rev().find(|c| {
            c.checks
                .iter()
                .any(|k| k.status == phonton_types::local::CheckStatus::Failed)
        })
    });
    lines.push(Line::raw(""));
    match &g.applied {
        None if selected.is_none() => lines.push(Line::from(vec![
            Span::styled(
                "✗ ",
                Style::default().fg(DANGER).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                match &g.status {
                    TaskStatus::Failed { reason, .. } => {
                        format!("{reason}. Your files are unchanged. ")
                    }
                    _ => "Nothing to apply. Your files are unchanged. ".to_string(),
                },
                Style::default().fg(PAPER),
            ),
            Span::styled(
                format!("Evidence: phonton goal --local show {}", receipt.id),
                Style::default().fg(DIM),
            ),
        ])),
        None if receipt.state != "review_ready" => lines.push(Line::from(vec![
            Span::styled("○ ", Style::default().fg(WARN).add_modifier(Modifier::BOLD)),
            Span::styled(
                "Not appliable: only a candidate whose checks passed can land. ",
                Style::default().fg(PAPER),
            ),
            Span::styled(
                format!("Evidence: phonton goal --local show {}", receipt.id),
                Style::default().fg(DIM),
            ),
        ])),
        None => lines.push(Line::from(vec![
            Span::styled(
                "Ctrl+Y",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " apply to the working tree · originals kept for rollback",
                Style::default().fg(MUTED),
            ),
        ])),
        Some(Ok(done)) => lines.push(Line::from(vec![
            Span::styled(
                "✓ ",
                Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
            ),
            Span::styled(done.clone(), Style::default().fg(PAPER)),
        ])),
        Some(Err(why)) => lines.push(Line::from(vec![
            Span::styled(
                "✗ ",
                Style::default().fg(DANGER).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("Not applied: {why}"), Style::default().fg(PAPER)),
        ])),
    }
    lines.push(Line::raw(""));
    let failures = failed_attempt
        .map(|c| local_tui::check_failures(c, 8))
        .unwrap_or_default();
    if let (false, Some(attempt)) = (failures.is_empty(), failed_attempt) {
        lines.push(Line::from(Span::styled(
            format!("Why candidate {} failed", attempt.number),
            Style::default().fg(PAPER).add_modifier(Modifier::BOLD),
        )));
        for line in failures {
            lines.push(Line::from(Span::styled(
                format!("  {line}"),
                Style::default().fg(if line.starts_with("not ok") {
                    DANGER
                } else {
                    MUTED
                }),
            )));
        }
        lines.push(Line::raw(""));
    }
    lines.push(Line::from(Span::styled(
        if selected.is_some() {
            format!("Diff · candidate {}", candidate.number)
        } else {
            format!("Diff · candidate {} · not applied", candidate.number)
        },
        Style::default().fg(PAPER).add_modifier(Modifier::BOLD),
    )));
    let diff: Vec<&str> = candidate.diff.lines().collect();
    for line in diff.iter().take(40) {
        let color = if line.starts_with("+++") || line.starts_with("---") {
            DIM
        } else if line.starts_with('+') {
            SUCCESS
        } else if line.starts_with('-') {
            DANGER
        } else if line.starts_with("@@") {
            ACCENT
        } else {
            MUTED
        };
        lines.push(Line::from(Span::styled(
            format!("  {line}"),
            Style::default().fg(color),
        )));
    }
    if diff.len() > 40 {
        lines.push(Line::from(Span::styled(
            format!(
                "  … {} more lines · phonton goal --local show {}",
                diff.len() - 40,
                receipt.id
            ),
            Style::default().fg(DIM),
        )));
    }
}

/// Where a goal sits on the ADE loop.
fn goal_track(g: &GoalEntry) -> art::Track {
    let verifying = g.flight_log.iter().rev().take(6).any(|r| {
        matches!(
            r.event,
            OrchestratorEvent::VerifyPass { .. }
                | OrchestratorEvent::VerifyFail { .. }
                | OrchestratorEvent::RepairPlanned { .. }
                | OrchestratorEvent::VerifyEscalated { .. }
        )
    });
    match (&g.local, &g.status) {
        (Some(local), TaskStatus::Running { .. }) => {
            return art::Track::Active(local_tui::stage(local));
        }
        (Some(local), TaskStatus::Reviewing { .. }) if local.state != "review_ready" => {
            return art::Track::Failed(3);
        }
        _ => {}
    }
    match &g.status {
        TaskStatus::Queued => art::Track::Active(0),
        TaskStatus::Planning => art::Track::Active(1),
        TaskStatus::Running { .. } | TaskStatus::Paused { .. } => {
            art::Track::Active(if verifying { 3 } else { 2 })
        }
        TaskStatus::Reviewing { .. } => art::Track::Active(4),
        TaskStatus::Done { .. } => art::Track::Complete,
        TaskStatus::Failed { .. } => art::Track::Failed(if verifying { 3 } else { 2 }),
        TaskStatus::Rejected => art::Track::Failed(4),
    }
}

/// `12.4 s` or `3m 05s` since the goal was queued (frozen once settled).
fn fmt_elapsed(g: &GoalEntry) -> String {
    let d = g
        .finished_at
        .unwrap_or_else(std::time::Instant::now)
        .saturating_duration_since(g.started_at);
    let secs = d.as_secs_f64();
    if secs < 60.0 {
        format!("{secs:.1} s")
    } else {
        format!("{}m {:02}s", d.as_secs() / 60, d.as_secs() % 60)
    }
}

fn receipt_cost(g: &GoalEntry, cost: &CostReceipt) -> String {
    let origin = g
        .local
        .as_ref()
        .map(|r| r.runtime_origin.into())
        .unwrap_or(g.token_origin);
    match origin {
        record::TokenOrigin::ManagedLocal => "$0.00 · managed local runtime".into(),
        record::TokenOrigin::Hosted if cost.pricing_known => format!(
            "${:.4} · hosted provider",
            cost.actual_usd_micros as f64 / 1e6
        ),
        record::TokenOrigin::Hosted => "unpriced · hosted provider".into(),
        record::TokenOrigin::Unknown => "unpriced · runtime origin unknown".into(),
    }
}

/// The receipt card: counts up, then stamps what the evidence supports.
fn receipt_lines(
    app: &App,
    g: &GoalEntry,
    state: &GlobalState,
    h: &HandoffPacket,
    width: usize,
) -> Vec<Line<'static>> {
    let since = g
        .receipt_tick
        .filter(|_| app.motion)
        .map(|t| app.spinner_frame.wrapping_sub(t));
    let verdict = if matches!(g.status, TaskStatus::Failed { .. }) {
        art::Verdict::Failed
    } else {
        handoff_verdict(h)
    };
    let inner = width.saturating_sub(4);
    let n = |v: u64| art::thousands(art::count_up(v, since));
    let num = |s: String| Span::styled(s, Style::default().fg(PAPER).add_modifier(Modifier::BOLD));
    let usage = &h.token_usage;
    let (tin, tout) = if usage.input_tokens + usage.output_tokens > 0 {
        (usage.input_tokens, usage.output_tokens)
    } else {
        (state.tokens_used, 0)
    };
    let mut body = vec![
        Line::from(Span::styled(
            short(&h.headline, inner),
            Style::default().fg(PAPER),
        )),
        Line::raw(""),
        art::leader(
            "files",
            vec![
                num(h.diff_stats.files_changed.to_string()),
                Span::styled(
                    format!("  +{}", h.diff_stats.added_lines),
                    Style::default().fg(SUCCESS),
                ),
                Span::styled(
                    format!(" −{}", h.diff_stats.removed_lines),
                    Style::default().fg(DANGER),
                ),
            ],
            inner,
        ),
        art::leader(
            "checks",
            vec![
                num(format!("{} passed", h.verification.passed.len())),
                Span::styled(
                    if h.verification.findings.is_empty() {
                        String::new()
                    } else {
                        format!(" · {} findings", h.verification.findings.len())
                    },
                    Style::default().fg(WARN),
                ),
            ],
            inner,
        ),
        art::leader("tokens in", vec![num(n(tin))], inner),
        art::leader("tokens out", vec![num(n(tout))], inner),
        art::leader("time", vec![num(fmt_elapsed(g))], inner),
    ];
    body.push(art::leader(
        "API cost",
        vec![num(receipt_cost(g, &h.cost_receipt))],
        inner,
    ));
    if g.recorded {
        let streak = app.record.streak;
        body.push(art::leader(
            "streak",
            vec![Span::styled(
                if verdict == art::Verdict::Verified {
                    format!("{streak} verified in a row")
                } else {
                    "reset · needs passing tests".to_string()
                },
                Style::default().fg(if verdict == art::Verdict::Verified {
                    SUCCESS
                } else {
                    DIM
                }),
            )],
            inner,
        ));
    }
    art::boxed(
        "receipt",
        Some(art::stamp(verdict, since)),
        body,
        width,
        RULE,
    )
}

fn execution_mode_label(goal: &GoalEntry) -> Option<&'static str> {
    let mut local = false;
    let mut provider = false;
    for record in &goal.flight_log {
        if let OrchestratorEvent::SubtaskReviewReady { model_name, .. } = &record.event {
            if model_name.contains("stub") {
                local = true;
            } else if !model_name.is_empty() {
                provider = true;
            }
        }
    }
    match (local, provider) {
        (true, true) => Some("mixed (stub + provider)"),
        (true, false) => Some("stub — not a provider token-efficiency claim"),
        (false, true) => Some("provider"),
        (false, false) => None,
    }
}

fn append_handoff_lines(lines: &mut Vec<Line<'static>>, handoff: &HandoffPacket) {
    if !handoff.changed_files.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "Changed files",
            Style::default().fg(PAPER).add_modifier(Modifier::BOLD),
        )));
        for file in handoff.changed_files.iter().take(6) {
            lines.push(Line::from(vec![
                Span::styled("  - ", Style::default().fg(ACCENT_HI)),
                Span::styled(
                    short(&file.path.display().to_string(), 44),
                    Style::default().fg(PAPER).add_modifier(Modifier::BOLD),
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
            Style::default().fg(PAPER).add_modifier(Modifier::BOLD),
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
        Style::default().fg(PAPER).add_modifier(Modifier::BOLD),
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
                Span::styled(command.command.join(" "), Style::default().fg(PAPER)),
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
            Style::default().fg(QUIET).add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Line::from(Span::styled(
            scroll_hint,
            Style::default().fg(MUTED),
        )))
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(QUIET));

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
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{status:<9} "),
                    Style::default().fg(ACCENT_HI).add_modifier(Modifier::BOLD),
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
        E::ContextSelected { .. } => (QUIET, "context"),
        E::ExtensionLoaded { .. } => (ACCENT, "ext-loaded"),
        E::ExtensionSkipped { .. } => (WARN, "ext-skipped"),
        E::ExtensionConflict { .. } => (WARN, "ext-conflict"),
        E::SteeringApplied { .. } => (QUIET, "steering"),
        E::SkillApplied { .. } => (QUIET, "skill"),
        E::McpServerAvailable { .. } => (ACCENT, "mcp-server"),
        E::McpToolRequested { .. } => (WARN, "mcp-request"),
        E::McpToolApproved { .. } => (SUCCESS, "mcp-approve"),
        E::McpToolDenied { .. } => (DANGER, "mcp-denied"),
        E::McpToolCompleted { .. } => (SUCCESS, "mcp-done"),
        E::McpCapabilitiesDiscovered { .. } => (ACCENT, "mcp-cap"),
        E::SubtaskCompleted { .. } => (SUCCESS, "subtask-done"),
        E::SubtaskReviewReady { .. } => (SUCCESS, "review-ready"),
        E::SubtaskFailed { .. } => (DANGER, "subtask-fail"),
        E::VerifyPass { .. } => (SUCCESS, "verify-pass"),
        E::VerifyFail { .. } => (WARN, "verify-fail"),
        E::RepairPlanned { .. } => (WARN, "repair"),
        E::VerifyEscalated { .. } => (WARN, "escalate"),
        E::TokenMilestone { .. } => (MUTED, "tokens"),
        E::Thinking { .. } => (QUIET, "thinking"),
        E::CheckpointCreated { .. } => (SUCCESS, "checkpoint"),
        E::RollbackPerformed { .. } => (WARN, "rollback"),
        E::ReviewDecision { .. } => (ACCENT, "review"),
        E::VerifyBrowserCheckPass { .. } => (SUCCESS, "browser-pass"),
        E::VerifyBrowserCheckFail { .. } => (WARN, "browser-fail"),
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
            Style::default().fg(QUIET).add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
    ];
    if app.ask_pending {
        let frame_ch = art::spinner(app.spinner_frame);
        lines.push(Line::from(vec![
            Span::styled(
                format!("{frame_ch} "),
                Style::default().fg(QUIET).add_modifier(Modifier::BOLD),
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
                Style::default().fg(QUIET).add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::default().fg(QUIET)),
    );
    frame.render_widget(p, area);
}

fn render_input(frame: &mut Frame, area: Rect, app: &App) {
    let (icon, mode_label, buf, cursor, artifacts, notice) = match app.mode {
        Mode::Goal => (
            "›",
            " GOAL ",
            app.goal_prompt.text(),
            app.goal_prompt.cursor(),
            app.goal_prompt.artifacts(),
            app.goal_prompt.notice(),
        ),
        Mode::Task => (
            "›",
            " TASK ",
            app.goal_prompt.text(),
            app.goal_prompt.cursor(),
            app.goal_prompt.artifacts(),
            app.goal_prompt.notice(),
        ),
        Mode::Ask => (
            "?",
            " ASK ",
            app.ask_input.as_str(),
            app.ask_cursor,
            &[][..],
            None,
        ),
        Mode::Settings => ("⚙", " SETTINGS ", "", 0, &[][..], None),
        Mode::Memory => ("M", " MEMORY ", "", 0, &[][..], None),
        Mode::History => ("H", " HISTORY ", "", 0, &[][..], None),
        Mode::CommandPalette => (
            "/",
            " COMMAND ",
            app.palette_input.as_str(),
            char_count(&app.palette_input),
            &[][..],
            None,
        ),
        Mode::Clarify => (
            "?",
            " CLARIFY ",
            app.clarifying_buffer.as_str(),
            app.clarifying_cursor,
            &[][..],
            None,
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
            .bg(QUIET)
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
        Mode::Clarify => Style::default()
            .bg(WARN)
            .fg(BG_PANEL)
            .add_modifier(Modifier::BOLD),
    };

    let border_color = match app.mode {
        Mode::Ask => QUIET,
        Mode::Task | Mode::Clarify => WARN,
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

    let input_width = row[0].width.saturating_sub(prompt_prefix_w) as usize;
    let mut chip_spans: Vec<Span<'static>> = Vec::new();
    let mut chip_width = 0usize;
    for (idx, artifact) in artifacts.iter().enumerate() {
        let label = if idx < 2 {
            artifact.label.clone()
        } else {
            format!("+{} artifacts", artifacts.len() - idx)
        };
        let needed = label.chars().count() + 1;
        if chip_width + needed >= input_width.saturating_sub(8) {
            let hidden = artifacts.len().saturating_sub(idx);
            let label = format!("+{hidden} artifacts");
            chip_width += label.chars().count() + 1;
            chip_spans.push(Span::styled(
                label,
                Style::default()
                    .fg(BG_DEEP)
                    .bg(QUIET)
                    .add_modifier(Modifier::BOLD),
            ));
            chip_spans.push(Span::raw(" "));
            break;
        }
        chip_width += needed;
        chip_spans.push(Span::styled(
            label,
            Style::default()
                .fg(BG_DEEP)
                .bg(ACCENT_HI)
                .add_modifier(Modifier::BOLD),
        ));
        chip_spans.push(Span::raw(" "));
    }

    // Horizontal scroll so the caret is always visible inside the text slot.
    let input_width = input_width.saturating_sub(chip_width);
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
    prompt_spans.extend(chip_spans);
    if visible.is_empty() {
        if let Some(notice) = notice {
            prompt_spans.push(Span::styled(
                short(notice, input_width),
                Style::default().fg(DANGER),
            ));
        } else {
            prompt_spans.push(Span::styled(visible, Style::default().fg(PAPER)));
        }
    } else {
        prompt_spans.push(Span::styled(visible, Style::default().fg(PAPER)));
    }

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
        let cx = row[0].x + prompt_prefix_w + chip_width as u16 + (cursor_clamped - scroll) as u16;
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
        TaskStatus::Queued => vec![tag("queued", DIM)],
        TaskStatus::Planning => vec![tag(
            &format!("{} plan", art::spinner(spinner_frame)),
            ACCENT,
        )],
        TaskStatus::Running {
            completed, total, ..
        } => vec![tag(
            &format!("{} run {completed}/{total}", art::spinner(spinner_frame)),
            WARN,
        )],
        TaskStatus::Reviewing { .. } => vec![tag("review", ACCENT)],
        TaskStatus::Done { .. } => vec![tag("✓ done", SUCCESS)],
        TaskStatus::Failed { .. } => vec![tag("✗ fail", DANGER)],
        TaskStatus::Paused {
            limit,
            observed,
            ceiling,
        } => vec![tag(&format!("paused — {limit} {observed}/{ceiling}"), WARN)],
        TaskStatus::Rejected => vec![Span::styled(
            "[rej]",
            Style::default().fg(DIM).add_modifier(Modifier::CROSSED_OUT),
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

// ---------------------------------------------------------------------------
// Stub dispatcher — the CLI drives the orchestrator without a real provider
// ---------------------------------------------------------------------------

/// Fail-closed dispatcher used when no API key is configured.
///
/// Real goals must not report verified success from a stub diff. The CLI
/// still constructs this type so the orchestrator path stays wired; dispatch
/// returns an error the user can act on (`phonton doctor --provider`).
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
        _subtask: Subtask,
        _prior_errors: Vec<String>,
        _attempt: u8,
        _msg_tx: Option<tokio::sync::mpsc::Sender<OrchestratorMessage>>,
    ) -> Result<SubtaskResult> {
        anyhow::bail!(
            "no provider API key configured. Run `phonton doctor --provider` and save a key before running a goal. Stub output is not a verified success."
        );
    }
}

// ---------------------------------------------------------------------------
// Event loop
// ---------------------------------------------------------------------------

/// Incoming event for the event loop — either a user key or an async
/// state update the driver received on one of the watch channels.
enum LoopEvent {
    Key(KeyEvent),
    Paste(String),
    ClipboardPaste(Result<String, String>),
    /// Snapshot for the goal with this task id (indices shift as goals queue).
    StateUpdate(TaskId, Box<GlobalState>),
    AskAnswer(String),
    LocalFinished(TaskId, Box<phonton_types::local_run::LocalRunReceipt>),
    LocalPlanRequested {
        prompt: local_plan_approval::PendingLocalPlan,
        reply_tx: oneshot::Sender<bool>,
    },
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
    FlightEvent(TaskId, EventRecord),
    /// Local model and hardware observed by the startup probe.
    Machine(Box<Machine>),
    /// Latest local-harness receipt for a goal running on the local model.
    Local(TaskId, Box<phonton_types::local_run::LocalRunReceipt>),
    /// Outcome of applying a local candidate to the working tree.
    LocalApplied(TaskId, Result<String, String>),
    Tick,
}

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
    phonton_extensions::phonton_home().map(|h| h.join("store.sqlite3"))
}

fn read_clipboard_text() -> Result<String, String> {
    #[cfg(windows)]
    {
        clipboard_win::get_clipboard_string().map_err(|e| e.to_string())
    }
    #[cfg(not(windows))]
    {
        Err("clipboard import is currently implemented for Windows builds".into())
    }
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
    cfg: &config::IndexConfig,
) -> Option<Arc<phonton_worker::SemanticContext>> {
    build_semantic_context_with_warnings(root, cfg, true).await
}

async fn build_semantic_context_with_warnings(
    root: &std::path::Path,
    cfg: &config::IndexConfig,
    emit_warnings: bool,
) -> Option<Arc<phonton_worker::SemanticContext>> {
    let root = root.to_path_buf();
    let index_cfg = cfg.clone();
    let build = async move {
        let retriever: Arc<dyn phonton_index::CodeRetriever> = if index_cfg.backend == "qdrant" {
            let embedder = phonton_index::Embedder::new_for_workspace(&root)?;
            let url = index_cfg
                .qdrant_url
                .clone()
                .unwrap_or_else(|| "http://127.0.0.1:6333".into());
            let collection = index_cfg
                .qdrant_collection
                .clone()
                .unwrap_or_else(|| "phonton-code".into());
            Arc::new(phonton_index::QdrantCodeRetriever::new(
                phonton_index::QdrantConfig::new(url, collection),
                embedder,
            ))
        } else {
            let embedder = phonton_index::Embedder::new_for_workspace(&root)?;
            let index = match phonton_index::discover_nexus_config(&root) {
                Ok(Some(cfg)) => {
                    phonton_index::index_workspace_with_nexus_using_embedder(&root, &cfg, &embedder)
                        .await
                }
                Ok(None) => phonton_index::index_workspace_using_embedder(&root, &embedder).await,
                Err(e) => Err(e),
            }?;
            Arc::new(phonton_index::LocalCodeRetriever::new(embedder, index))
        };
        anyhow::Ok(Arc::new(phonton_worker::SemanticContext { retriever }))
    };

    match tokio::time::timeout(Duration::from_secs(SEMANTIC_INDEX_TIMEOUT_SECS), build).await {
        Ok(Ok(ctx)) => Some(ctx),
        Ok(Err(e)) => {
            if emit_warnings {
                eprintln!(
                    "phonton: semantic index unavailable ({e}); continuing without indexed context"
                );
            }
            None
        }
        Err(_) => {
            if emit_warnings {
                eprintln!(
                    "phonton: semantic index timed out after {SEMANTIC_INDEX_TIMEOUT_SECS}s; continuing without indexed context"
                );
            }
            None
        }
    }
}

/// Load a provider for ask-mode (stateless Q&A) using the config file or
/// env vars. Returns `None` when no key is available.
pub(crate) fn load_ask_provider(cfg: &config::Config) -> Option<Arc<dyn Provider>> {
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
            Span::styled("⚙ ", Style::default().fg(QUIET)),
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
        let spinner = art::spinner(app.spinner_frame);
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
        .border_style(Style::default().fg(QUIET))
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
                Style::default().fg(PAPER)
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
        "openai" => "gpt-6-luna".into(),
        "openrouter" => "openai/gpt-6-luna".into(),
        // `gemini-flash-latest` is an always-current alias that points at
        // whichever flash model is generally available on free-tier keys.
        // `gemini-2.5-flash` exists on most keys but the alias avoids
        // surprises when Google rotates the GA model. The Gemini provider
        // also auto-routes to a working model on 404 (see
        // `phonton-providers::GeminiProvider`).
        "gemini" => "gemini-flash-latest".into(),
        "agentrouter" => "claude-sonnet-4-5".into(),
        "cloudflare" => "@cf/moonshotai/kimi-k2.6".into(),
        // The model Phonton installed and calibrated, not a guess that may
        // not be pulled.
        "ollama" => models_cli::settings()
            .ok()
            .and_then(|settings| settings.active_model)
            .unwrap_or_else(|| "llama3.2:3b".into()),
        "deepseek" => "deepseek-flash".into(),
        "xai" | "grok" => "grok-build-0.1".into(),
        "groq" => "openai/gpt-oss-120b".into(),
        "together" => "deepseek-ai/DeepSeek-V4.1-Flash".into(),
        _ => "unknown".into(),
    }
}

/// Cheap/local use the configured model when set. Standard and frontier use
/// per-tier ids so escalation is not the same model as cheap.
fn model_for_dispatch(provider: &str, configured: Option<&str>, tier: ModelTier) -> String {
    // Keyless providers (Ollama, custom/OpenAI-compatible endpoints) serve
    // whatever the user installed; there is no tier ladder to escalate to,
    // so every tier uses the configured model.
    if !provider_requires_key(provider) {
        if let Some(model) = configured.filter(|s| !s.trim().is_empty()) {
            return model.to_string();
        }
    }
    match tier {
        ModelTier::Local | ModelTier::Cheap => configured
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| phonton_providers::model_for_tier(provider, tier)),
        ModelTier::Standard | ModelTier::Frontier => {
            phonton_providers::model_for_tier(provider, tier)
        }
    }
}

/// Print the `phonton --help` text. Plain stdout — runs before the TUI
/// touches the terminal, so it composes with shell pipes / `less`.
fn print_help() {
    println!(
        "phonton — local-first ADE: goal → plan → edit → verify → review → remember\n\
         \n\
         QUICK START:\n  \
         phonton doctor           check provider key, git, and local tools\n  \
         phonton                  open the TUI and type a goal\n  \
         phonton models setup qwen2.5-coder:3b\n                           \
         run on a local model instead of a cloud key\n  \
         phonton why-tokens       see where the last goal spent tokens\n\
         \n\
         USAGE:\n  \
         phonton [SUBCOMMAND]\n\
         \n\
         SUBCOMMANDS:\n  \
         (none)            Launch the interactive TUI (default)\n  \
         ask <question>    One-shot Q&A using the configured provider\n  \
         benchmark         Export benchmark evidence from the latest run\n  \
         doctor            Check provider key, store, trust, and the project's toolchains\n  \
         extensions        Inspect loaded steering, skills, MCP, and profiles\n  \
         skills            Inspect loaded skills\n  \
         steering          Inspect loaded steering rules\n  \
         goal <goal>       Run a noninteractive goal through plan/edit/verify/review\n  \
         serve             JSON-RPC sidecar API for Phonton Desktop\n  \
         mcp               List configured MCP servers and explicitly call tools\n  \
         plan <goal>       Preview the task DAG without changing files\n  \
         review [task-id]  Show verified diff review payloads\n  \
         why-tokens        Per-subtask token usage for the latest goal\n  \
         record            Verified runs, streak, and tokens kept local\n  \
         proof export      Export typed proof evidence for audit\n  \
         memory            List, edit, delete, and pin persistent memory\n  \
         models            Detect hardware, install and calibrate local coding models\n  \
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
         Settings live in ~/.phonton/config.toml; PHONTON_CONFIG_PATH selects\n  \
         another config file for this process. Provider keys may use\n  \
         ANTHROPIC_API_KEY, OPENAI_API_KEY, DEEPSEEK_API_KEY, etc.\n\
         \n\
         DOCTOR:\n  \
         phonton doctor [--json] [--provider]\n\
         \n\
         GOAL:\n  \
         phonton goal --local [--plan] <goal> [--repo <path>] [--files a,b]\n  \
         phonton goal --local <goal> [--check <JSON-array>] [--yes] [--allow-host-checks]\n  \
         phonton goal --local --reviewed-plan <path> --sha256 <hash> --yes [--allow-host-checks]\n  \
         phonton goal --local --request <path> [--allow-host-checks]\n  \
         phonton goal --local apply RUN_ID --yes\n  \
         phonton goal --local rollback RUN_ID --yes\n  \
         phonton goal --local list\n  \
         phonton goal --local show RUN_ID\n  \
         phonton goal [--prompt-file <path>|--stdin|<goal>] [--json] [--yes]\n  \
         phonton goal [--allow-host-checks] [--timeout-seconds <n>] [--task]\n\
         \n\
         BENCHMARK:\n  \
         phonton benchmark export --latest --format json\n\
         \n\
         PLAN PREVIEW:\n  \
         phonton plan [--json] [--no-memory] [--no-tests] <goal>\n\
         \n\
         REVIEW:\n  \
         phonton review [--json] [latest|<task-id>]\n  \
         phonton review approve [--json] [latest|<task-id>]\n  \
         phonton review reject [--json] [latest|<task-id>]\n  \
         phonton review rollback [--json] [latest|<task-id>] <seq>  (disabled: unsafe legacy reset)\n\
         \n\
         MCP:\n  \
         phonton mcp list [--json]\n  \
         phonton mcp capabilities <server-id> [--json] [--yes]\n  \
         phonton mcp tools <server-id> [--json] [--yes]\n  \
         phonton mcp call <server-id> <tool-name> [json-args] [--json] [--yes]\n\
         \n\
         EXTENSIONS:\n  \
         phonton extensions list [--json]\n  \
         phonton extensions doctor [--json]\n  \
         phonton extensions skills [--json]\n  \
         phonton extensions steering [--json]\n  \
         phonton extensions mcp [--json]\n  \
         phonton extensions profiles [--json]\n  \
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
                        let cfg = config::load()?;
                        config::save(&cfg)?;
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
                    let cfg = config::load()?;
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
        "models" => {
            let code = models_cli::run(&args[1..]).await?;
            if code != 0 {
                std::process::exit(code);
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
        "goal" => {
            let code = if args.get(1).is_some_and(|arg| arg == "--local") {
                // The local run future holds candidate-search state. Keep it
                // off the main thread's stack, including for plan previews.
                Box::pin(local_goal_cli::run(&args[2..])).await?
            } else {
                run_headless_goal(&args[1..]).await?
            };
            if code != 0 {
                std::process::exit(code);
            }
            Ok(true)
        }
        "index" => {
            let code = index_cli::run(&args[1..]).await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(true)
        }
        "benchmark" => {
            let code = benchmark_cli::run(&args[1..]).await?;
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
        "why-tokens" | "tokens" => {
            let code = tokens_cli::run(&args[1..]).await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(true)
        }
        "record" => {
            record::run(&args[1..])?;
            Ok(true)
        }
        "proof" => {
            let code = proof_cli::run(&args[1..]).await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(true)
        }
        "serve" => {
            let code = serve_cli::run(&args[1..]).await?;
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
            let cfg = config::load()?;
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

/// Hooks for the desktop/serve JSON-RPC sidecar.
#[derive(Debug, Default)]
pub(crate) struct HeadlessGoalHooks {
    pub fixed_task_id: Option<TaskId>,
    pub state_tx: Option<watch::Sender<GlobalState>>,
    pub event_tx: Option<broadcast::Sender<EventRecord>>,
    pub skip_trust_prompt: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HeadlessGoalOptions {
    goal_text: String,
    display_text: String,
    json: bool,
    yes: bool,
    host_checks_approved: bool,
    direct_task: bool,
    timeout_seconds: u64,
    resume_task_id: Option<TaskId>,
}

#[derive(Debug, Clone)]
pub(crate) struct HeadlessGoalResult {
    pub task_id: TaskId,
    pub final_state: GlobalState,
    pub exit_code: i32,
}

fn print_goal_help() {
    println!(
        "Usage:\n  phonton goal [--prompt-file <path>|--stdin|<goal>] [--json] [--yes]\n  phonton goal [--allow-host-checks] [--timeout-seconds <n>] [--task]\n  phonton goal --resume <task-id> [--allow-host-checks]\n\nRuns a noninteractive goal through Phonton's goal -> plan -> edit -> verify -> review loop.\nHost checks execute repository code and require explicit --allow-host-checks on each invocation.
With a local provider and a calibrated model, goals run through the local harness; see phonton goal --local --help."
    );
}

fn apply_budget_pricing(guard: BudgetGuard, cfg: &config::Config) -> BudgetGuard {
    let provider = cfg.provider.name.as_str();
    let model = cfg
        .provider
        .model
        .clone()
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| default_model_for(provider));
    let kind = match provider {
        "anthropic" => ProviderKind::Anthropic,
        "openai" => ProviderKind::OpenAI,
        "ollama" => ProviderKind::Ollama,
        "cloudflare" => ProviderKind::Cloudflare,
        "deepseek" | "openai-compatible" | "xai" | "groq" | "together" | "custom" => {
            ProviderKind::OpenAiCompatible
        }
        _ => ProviderKind::OpenAiCompatible,
    };
    if provider == "ollama" {
        return guard.with_price(
            ProviderKind::Ollama,
            &model,
            ModelPricing {
                input_usd_micros_per_mtok: 0,
                output_usd_micros_per_mtok: 0,
            },
        );
    }
    if provider == "cloudflare" && model == "@cf/moonshotai/kimi-k2.6" {
        return guard.with_price(
            ProviderKind::Cloudflare,
            &model,
            ModelPricing {
                input_usd_micros_per_mtok: 950_000,
                output_usd_micros_per_mtok: 4_000_000,
            },
        );
    }
    let official_deepseek_endpoint = match cfg.provider.base_url.as_deref() {
        None => provider == "deepseek",
        Some(url) => matches!(
            url.trim_end_matches('/'),
            "https://api.deepseek.com" | "https://api.deepseek.com/v1"
        ),
    };
    if matches!(provider, "deepseek" | "openai-compatible") && official_deepseek_endpoint {
        // Published peak cache-miss prices are conservative for the
        // discounted off-peak and cache-hit periods. Register every tier the
        // dispatcher can select, not only the configured cheap model.
        let flash = ModelPricing {
            input_usd_micros_per_mtok: 300_000,
            output_usd_micros_per_mtok: 1_200_000,
        };
        let pro = ModelPricing {
            input_usd_micros_per_mtok: 1_320_000,
            output_usd_micros_per_mtok: 3_960_000,
        };
        let mut guard = guard;
        for id in [
            "deepseek-flash",
            "deepseek-v4-flash",
            "deepseek-v4-flash-vision-exp",
        ] {
            guard = guard.with_price(kind, id, flash);
        }
        guard = guard.with_price(kind, "deepseek-v4-pro", pro);
        return guard;
    }
    guard
}

fn parse_headless_goal_options(args: &[String]) -> Result<HeadlessGoalOptions> {
    let mut json = false;
    let mut yes = false;
    let mut host_checks_approved = false;
    let mut direct_task = false;
    let mut timeout_seconds = 900;
    let mut prompt_file: Option<PathBuf> = None;
    let mut read_stdin = false;
    let mut resume_task_id: Option<TaskId> = None;
    let mut positionals = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--resume" => {
                i += 1;
                let raw = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--resume requires a task id"))?;
                let uuid = uuid::Uuid::parse_str(raw)
                    .map_err(|_| anyhow::anyhow!("--resume task id must be a UUID"))?;
                resume_task_id = Some(TaskId(uuid));
            }
            "--json" => json = true,
            "--yes" | "-y" => yes = true,
            "--allow-host-checks" => host_checks_approved = true,
            "--task" => direct_task = true,
            "--stdin" => read_stdin = true,
            "--prompt-file" => {
                i += 1;
                let path = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--prompt-file requires a path"))?;
                prompt_file = Some(PathBuf::from(path));
            }
            "--timeout-seconds" => {
                i += 1;
                let raw = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--timeout-seconds requires a value"))?;
                timeout_seconds = raw
                    .parse::<u64>()
                    .map_err(|_| anyhow::anyhow!("--timeout-seconds must be a positive integer"))?;
                if timeout_seconds == 0 {
                    return Err(anyhow::anyhow!(
                        "--timeout-seconds must be a positive integer"
                    ));
                }
            }
            "--permission-mode" => {
                i += 1;
                args.get(i)
                    .ok_or_else(|| anyhow::anyhow!("--permission-mode requires a value"))?;
                return Err(anyhow::anyhow!(
                    "--permission-mode is unsupported for headless goals; use --allow-host-checks to explicitly approve repository checks"
                ));
            }
            "--" => {
                positionals.extend(args[i + 1..].iter().cloned());
                break;
            }
            other if other.starts_with('-') => {
                return Err(anyhow::anyhow!("unknown phonton goal flag `{other}`"));
            }
            other => positionals.push(other.to_string()),
        }
        i += 1;
    }

    if resume_task_id.is_some() && (prompt_file.is_some() || read_stdin || !positionals.is_empty())
    {
        return Err(anyhow::anyhow!(
            "`--resume` cannot be combined with a new goal prompt"
        ));
    }

    let source_count = usize::from(prompt_file.is_some())
        + usize::from(read_stdin)
        + usize::from(!positionals.is_empty());
    if resume_task_id.is_none() && source_count > 1 {
        return Err(anyhow::anyhow!(
            "choose only one goal source: --prompt-file, --stdin, or positional text"
        ));
    }

    let goal_text = if resume_task_id.is_some() {
        String::new()
    } else if let Some(path) = prompt_file {
        std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", path.display()))?
    } else if read_stdin {
        let mut text = String::new();
        use std::io::Read as _;
        io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| anyhow::anyhow!("failed to read stdin: {e}"))?;
        text
    } else {
        positionals.join(" ")
    };

    let goal_text = goal_text.trim().to_string();
    if resume_task_id.is_none() && goal_text.is_empty() {
        return Err(anyhow::anyhow!("goal text is empty"));
    }

    let display_text = if resume_task_id.is_some() {
        "resume paused goal".into()
    } else {
        summarize_goal_display(&goal_text)
    };
    Ok(HeadlessGoalOptions {
        goal_text,
        display_text,
        json,
        yes,
        host_checks_approved,
        direct_task,
        timeout_seconds,
        resume_task_id,
    })
}

fn summarize_goal_display(text: &str) -> String {
    let first_line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .unwrap_or("goal");
    short(first_line, 96)
}

fn ensure_resume_workspace(saved_working_dir: &str, current_working_dir: &Path) -> Result<()> {
    let saved = std::fs::canonicalize(saved_working_dir)
        .map_err(|e| anyhow::anyhow!("cannot resolve the paused goal workspace: {e}"))?;
    let current = std::fs::canonicalize(current_working_dir)
        .map_err(|e| anyhow::anyhow!("cannot resolve the current workspace: {e}"))?;
    if saved != current {
        return Err(anyhow::anyhow!(
            "paused goal belongs to {}; resume it from that workspace",
            saved.display()
        ));
    }
    Ok(())
}

async fn run_headless_goal(args: &[String]) -> Result<i32> {
    if args.is_empty() {
        print_goal_help();
        return Ok(2);
    }
    if matches!(args[0].as_str(), "-h" | "--help" | "help") {
        print_goal_help();
        return Ok(0);
    }

    let opts = match parse_headless_goal_options(args) {
        Ok(opts) => opts,
        Err(e) => {
            eprintln!("phonton goal: {e}");
            print_goal_help();
            return Ok(2);
        }
    };

    let provider = config::load()?.provider.name;
    if !config::KNOWN_PROVIDERS.contains(&provider.as_str()) && provider != "grok" {
        eprintln!(
            "phonton goal: unknown provider `{provider}` in {}. Use one of: {}.",
            config::config_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "config.toml".into()),
            config::KNOWN_PROVIDERS.join(", ")
        );
        return Ok(2);
    }
    if !opts.json && !opts.direct_task && opts.resume_task_id.is_none() {
        let cfg = config::load()?;
        let base_url = cfg.provider.base_url.clone().unwrap_or_default();
        let configured = cfg.provider.model.clone().unwrap_or_default();
        if provider_is_local(&cfg.provider.name, &base_url) {
            start_managed_runtime_if_needed().await;
        }
        let calibrated = provider_is_local(&cfg.provider.name, &base_url)
            && match local_goal_cli::current_model_selection().await {
                Ok(Some(selection)) => {
                    configured.is_empty() || configured.eq_ignore_ascii_case(&selection.model)
                }
                _ => false,
            };
        if calibrated {
            // Same routing as the TUI: a calibrated local model runs through
            // the local harness built for small models.
            let mut local_args = vec![opts.goal_text.clone()];
            if opts.yes {
                local_args.push("--yes".into());
            }
            if opts.host_checks_approved {
                local_args.push("--allow-host-checks".into());
            }
            return Box::pin(local_goal_cli::run(&local_args)).await;
        }
    }
    let result = execute_headless_goal(opts, HeadlessGoalHooks::default()).await?;
    debug_assert_eq!(
        result.exit_code == 0,
        headless_goal_succeeded(&result.final_state.task_status)
    );
    Ok(result.exit_code)
}

pub(crate) async fn execute_headless_goal(
    opts: HeadlessGoalOptions,
    hooks: HeadlessGoalHooks,
) -> Result<HeadlessGoalResult> {
    let cfg = config::load()?;
    let working_dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let workspace_trusted = if opts.yes {
        true
    } else if hooks.skip_trust_prompt {
        trust::is_trusted(&working_dir)
    } else {
        trust::prompt_if_needed(&working_dir)?
    };
    if !workspace_trusted {
        return Ok(HeadlessGoalResult {
            task_id: hooks.fixed_task_id.unwrap_or_default(),
            final_state: GlobalState {
                task_status: TaskStatus::Failed {
                    reason: "trust prompt declined".into(),
                    failed_subtask: None,
                },
                goal_contract: None,
                plan_graph: None,
                index_backend: None,
                handoff_packet: None,
                active_workers: Vec::new(),
                tokens_used: 0,
                tokens_budget: None,
                estimated_naive_tokens: 0,
                checkpoints: Vec::new(),
                resume_checkpoint: None,
                cost_receipt: CostReceipt::default(),
            },
            exit_code: 1,
        });
    }

    let store = Arc::new(std::sync::Mutex::new(open_persistent_store()?));
    let sandbox = Arc::new(Sandbox::new(
        working_dir.clone(),
        "phonton-cli-headless".to_string(),
    ));

    let resume_snapshot = if let Some(resume_id) = opts.resume_task_id {
        let paused = store
            .lock()
            .ok()
            .and_then(|g| g.load_paused_run(resume_id).ok().flatten());
        match paused {
            Some(snapshot) => {
                ensure_resume_workspace(&snapshot.working_dir, &working_dir)?;
                Some(snapshot)
            }
            None => {
                return print_headless_failure(
                    opts.json,
                    resume_id,
                    &opts.display_text,
                    "no paused run found for that task id",
                    None,
                    Some(cfg.index.backend.clone()),
                    &store,
                );
            }
        }
    } else {
        None
    };

    let task_id = resume_snapshot
        .as_ref()
        .map(|s| s.task_id)
        .or(hooks.fixed_task_id)
        .unwrap_or_else(TaskId::new);
    let resume_from = resume_snapshot.as_ref().map(|s| s.resume.clone());
    let goal_text_for_run = resume_snapshot
        .as_ref()
        .map(|s| s.goal_text.clone())
        .unwrap_or_else(|| opts.goal_text.clone());

    let prompt = SubmittedPrompt {
        description: goal_text_for_run.clone(),
        display_text: opts.display_text.clone(),
        prompt_artifacts: Vec::new(),
    };
    let attachments = prepare_prompt_attachments(&prompt, &working_dir);

    if let Ok(g) = store.lock() {
        let _ = g.upsert_task(task_id, &opts.display_text, &TaskStatus::Planning, 0);
    }

    let memory_store = phonton_memory::MemoryStore::new(Arc::clone(&store)).await;
    let prompt_artifacts = prompt.prompt_artifacts.clone();
    let plan_result = if let Some(snapshot) = resume_snapshot {
        Ok(snapshot.planner_output)
    } else if opts.direct_task {
        Ok(single_task_plan(
            prompt.description.clone(),
            attachments.clone(),
            prompt_artifacts,
        ))
    } else {
        let store_guard = match store.lock() {
            Ok(g) => g,
            Err(_) => {
                return print_headless_failure(
                    opts.json,
                    task_id,
                    &opts.display_text,
                    "persistent store lock was poisoned",
                    None,
                    Some(cfg.index.backend.clone()),
                    &store,
                );
            }
        };
        let goal = Goal::new(prompt.description.clone())
            .with_attachments(attachments.clone())
            .with_prompt_artifacts(prompt_artifacts);
        let result = decompose_with_memory(&goal, &store_guard, load_ask_provider(&cfg)).await;
        drop(store_guard);
        result
    };

    let mut plan = match plan_result {
        Ok(plan) => plan,
        Err(e) => {
            return print_headless_failure(
                opts.json,
                task_id,
                &opts.display_text,
                &format!("planning failed: {e}"),
                None,
                Some(cfg.index.backend.clone()),
                &store,
            );
        }
    };
    if resume_from.is_none() {
        contract_preflight::apply_workspace_preflight(&mut plan, &working_dir);
    }

    let initial_state = GlobalState {
        task_status: TaskStatus::Planning,
        goal_contract: plan.goal_contract.clone(),
        plan_graph: Some(plan.plan_graph.clone()),
        index_backend: Some(cfg.index.backend.clone()),
        handoff_packet: None,
        active_workers: Vec::new(),
        tokens_used: 0,
        tokens_budget: None,
        estimated_naive_tokens: plan.naive_baseline_tokens,
        checkpoints: Vec::new(),
        resume_checkpoint: None,
        cost_receipt: CostReceipt::default(),
    };
    let (state_tx, _state_rx) = match hooks.state_tx {
        Some(tx) => {
            let _ = tx.send(initial_state.clone());
            (tx.clone(), tx.subscribe())
        }
        None => watch::channel(initial_state),
    };

    let (event_tx, _) = match hooks.event_tx {
        Some(tx) => (tx.clone(), tx.subscribe()),
        None => broadcast::channel::<EventRecord>(1024),
    };
    let mut event_rx_store = event_tx.subscribe();
    let store_for_events = Arc::clone(&store);
    let (event_writer_stop, mut event_writer_stop_rx) = tokio::sync::oneshot::channel::<()>();
    let event_writer = tokio::spawn(async move {
        let persist = |rec: EventRecord| {
            let store = Arc::clone(&store_for_events);
            async move {
                let _ = tokio::task::spawn_blocking(move || {
                    if let Ok(g) = store.lock() {
                        let _ = g.append_event(&rec);
                    }
                })
                .await;
            }
        };
        loop {
            tokio::select! {
                event = event_rx_store.recv() => match event {
                    Ok(rec) => persist(rec).await,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                _ = &mut event_writer_stop_rx => {
                    loop {
                        match event_rx_store.try_recv() {
                            Ok(rec) => persist(rec).await,
                            Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                            Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => break,
                        }
                    }
                    break;
                }
            }
        }
    });

    let extension_set = load_extensions(&ExtensionLoadOptions::for_workspace(&working_dir));
    apply_extension_context_to_plan(&mut plan, &extension_set);
    publish_extension_events(task_id, &extension_set, &event_tx);

    let naive = plan.naive_baseline_tokens;
    let semantic_context =
        build_semantic_context_with_warnings(&working_dir, &cfg.index, !opts.json).await;
    let mcp_runtime = if extension_set.mcp_servers.is_empty() {
        None
    } else {
        let approver: Arc<dyn McpApprover> = if opts.yes {
            Arc::new(ExplicitApproveAll)
        } else {
            Arc::new(DenyByDefaultApprover)
        };
        Some(Arc::new(
            phonton_mcp::McpRuntime::new(
                extension_set.mcp_servers.clone(),
                ExecutionGuard::new(working_dir.clone()),
            )
            .with_approver(approver)
            .with_event_sink(task_id, event_tx.clone()),
        ))
    };

    let verification_execution = if opts.host_checks_approved {
        phonton_types::verification::VerificationExecution::HostApproved
    } else {
        phonton_types::verification::VerificationExecution::RequireIsolation
    };
    let dispatcher: Arc<dyn WorkerDispatcher> =
        if let Some(api_key) = provider_key_for_run(&cfg.provider) {
            let provider_name = cfg.provider.name.clone();
            let account_id = cfg.provider.account_id.clone();
            let base_url = cfg.provider.base_url.clone();
            let configured_model = cfg.provider.model.clone();

            let factory = move |tier: phonton_types::ModelTier| {
                let model = model_for_dispatch(&provider_name, configured_model.as_deref(), tier);
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
            let mut dispatcher =
                phonton_worker::dispatcher::RealDispatcher::new(factory, guard, sandbox.clone())
                    .with_task_id(task_id)
                    .with_memory(memory_store.clone())
                    .with_verification_execution(verification_execution);
            if let Some(ctx) = semantic_context.clone() {
                dispatcher = dispatcher.with_semantic_context(ctx);
            }
            if let Some(runtime) = mcp_runtime.clone() {
                dispatcher = dispatcher.with_mcp_runtime(runtime);
            }
            Arc::new(dispatcher)
        } else {
            Arc::new(StubDispatcher::new(sandbox.clone()))
        };

    let diff_applier = DiffApplier::open(&working_dir)
        .ok()
        .map(|d| Arc::new(std::sync::Mutex::new(d)));

    let limits = BudgetLimits {
        max_tokens: cfg.budget.max_tokens,
        max_usd_micros: cfg.budget.max_usd_micros(),
    };
    let budget_guard = apply_budget_pricing(BudgetGuard::new(limits), &cfg);

    let mut orchestrator = Orchestrator::new(dispatcher)
        .with_verification_execution(verification_execution)
        .with_naive_baseline(naive)
        .with_budget_guard(budget_guard)
        .with_working_dir(working_dir.clone())
        .with_index_backend(cfg.index.backend.clone())
        .with_memory(memory_store)
        .with_event_sink(task_id, opts.display_text.clone(), event_tx.clone());
    if let Some(diff_applier) = diff_applier {
        orchestrator = orchestrator.with_diff_applier(diff_applier);
    }

    let run = orchestrator.run_task(plan.clone(), state_tx.clone(), resume_from);
    let run_result = tokio::time::timeout(Duration::from_secs(opts.timeout_seconds), run).await;
    let final_state = match run_result {
        Ok(Ok(_)) => state_tx.borrow().clone(),
        Ok(Err(e)) => mark_headless_goal_failed(&state_tx, format!("orchestrator failed: {e}")),
        Err(_) => mark_headless_goal_failed(
            &state_tx,
            format!("timed out after {} seconds", opts.timeout_seconds),
        ),
    };

    if let Ok(g) = store.lock() {
        let _ = g.upsert_task(
            task_id,
            &opts.display_text,
            &final_state.task_status,
            final_state.tokens_used,
        );
        if let Some(ledger) = outcome_ledger_from_state(task_id, &final_state) {
            let _ = g.upsert_outcome_ledger(&ledger);
        }
        if let (Some(resume), TaskStatus::Paused { .. }) =
            (&final_state.resume_checkpoint, &final_state.task_status)
        {
            let snapshot = PausedRunSnapshot {
                task_id,
                goal_text: goal_text_for_run.clone(),
                working_dir: working_dir.display().to_string(),
                planner_output: plan.clone(),
                resume: resume.clone(),
            };
            let _ = g.upsert_paused_run(&snapshot);
        } else if matches!(
            final_state.task_status,
            TaskStatus::Reviewing { .. } | TaskStatus::Done { .. }
        ) {
            let _ = g.delete_paused_run(task_id);
        }
    }

    drop(event_tx);
    let _ = event_writer_stop.send(());
    let _ = tokio::time::timeout(Duration::from_secs(5), event_writer).await;

    if opts.json {
        print_headless_goal_json(task_id, &final_state)?;
    } else {
        for line in headless_summary_lines(task_id, &final_state) {
            println!("{line}");
        }
    }

    let exit_code = if headless_goal_succeeded(&final_state.task_status) {
        0
    } else {
        1
    };
    Ok(HeadlessGoalResult {
        task_id,
        final_state,
        exit_code,
    })
}

fn print_headless_failure(
    json: bool,
    task_id: TaskId,
    display_text: &str,
    reason: &str,
    plan_graph: Option<phonton_types::PlanGraph>,
    index_backend: Option<String>,
    store: &Arc<std::sync::Mutex<Store>>,
) -> Result<HeadlessGoalResult> {
    let state = GlobalState {
        task_status: TaskStatus::Failed {
            reason: reason.to_string(),
            failed_subtask: None,
        },
        goal_contract: None,
        plan_graph,
        index_backend,
        handoff_packet: Some(failed_handoff_packet(task_id, display_text, reason, 0)),
        active_workers: Vec::new(),
        tokens_used: 0,
        tokens_budget: None,
        estimated_naive_tokens: 0,
        checkpoints: Vec::new(),
        resume_checkpoint: None,
        cost_receipt: CostReceipt::default(),
    };
    if let Ok(g) = store.lock() {
        let _ = g.upsert_task(task_id, display_text, &state.task_status, 0);
        if let Some(ledger) = outcome_ledger_from_state(task_id, &state) {
            let _ = g.upsert_outcome_ledger(&ledger);
        }
    }
    if json {
        print_headless_goal_json(task_id, &state)?;
    } else {
        eprintln!("phonton goal: {reason}");
    }
    Ok(HeadlessGoalResult {
        task_id,
        final_state: state,
        exit_code: 1,
    })
}

fn failed_handoff_packet(
    task_id: TaskId,
    display_text: &str,
    reason: &str,
    tokens_used: u64,
) -> HandoffPacket {
    HandoffPacket {
        schema_version: phonton_types::HANDOFF_PACKET_SCHEMA_VERSION.to_string(),
        task_id,
        goal: display_text.to_string(),
        headline: format!("Task failed before review: {}", short(reason, 120)),
        changed_files: Vec::new(),
        generated_artifacts: Vec::new(),
        diff_stats: phonton_types::DiffStats::default(),
        verification: phonton_types::VerifyReport {
            passed: Vec::new(),
            findings: vec![reason.to_string()],
            skipped: vec!["No verification layer completed before this failure.".into()],
        },
        run_commands: Vec::new(),
        known_gaps: vec![
            "No changed files were recorded for this run.".into(),
            "Review the failure reason before retrying.".into(),
        ],
        review_actions: vec![phonton_types::ReviewAction {
            label: "Inspect failure".into(),
            description: "Open the transcript and flight log for the failed run.".into(),
        }],
        rollback_points: Vec::new(),
        token_usage: TokenUsage::estimated(tokens_used),
        influence: phonton_types::InfluenceSummary::default(),
        screenshot_path: None,
        rendering_summary: None,
        cost_receipt: CostReceipt::default(),
    }
}

fn mark_headless_goal_failed(state_tx: &watch::Sender<GlobalState>, reason: String) -> GlobalState {
    let mut state = state_tx.borrow().clone();
    state.task_status = TaskStatus::Failed {
        reason,
        failed_subtask: None,
    };
    state.active_workers.clear();
    let _ = state_tx.send(state.clone());
    state
}

fn print_headless_goal_json(task_id: TaskId, state: &GlobalState) -> Result<()> {
    let doc = serde_json::json!({
        "task_id": task_id,
        "status": &state.task_status,
        "tokens_used": state.tokens_used,
        "estimated_naive_tokens": state.estimated_naive_tokens,
        "index_backend": &state.index_backend,
        "plan_graph": &state.plan_graph,
        "handoff_packet": &state.handoff_packet,
        "cost_receipt": &state.cost_receipt,
    });
    println!("{}", serde_json::to_string_pretty(&doc)?);
    Ok(())
}

async fn fail_spawned_goal(
    tx: &mpsc::Sender<LoopEvent>,
    store: &Arc<std::sync::Mutex<Store>>,
    task_id: TaskId,
    display_text: &str,
    reason: String,
    index_backend: Option<String>,
) {
    let state = GlobalState {
        task_status: TaskStatus::Failed {
            reason: reason.clone(),
            failed_subtask: None,
        },
        goal_contract: None,
        plan_graph: None,
        index_backend,
        handoff_packet: Some(failed_handoff_packet(task_id, display_text, &reason, 0)),
        active_workers: Vec::new(),
        tokens_used: 0,
        tokens_budget: None,
        estimated_naive_tokens: 0,
        checkpoints: Vec::new(),
        resume_checkpoint: None,
        cost_receipt: CostReceipt::default(),
    };
    if let Ok(g) = store.lock() {
        let _ = g.upsert_task(task_id, display_text, &state.task_status, 0);
        if let Some(ledger) = outcome_ledger_from_state(task_id, &state) {
            let _ = g.upsert_outcome_ledger(&ledger);
        }
    }
    let _ = tx
        .send(LoopEvent::StateUpdate(task_id, Box::new(state)))
        .await;
}

fn headless_goal_succeeded(status: &TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Reviewing { .. } | TaskStatus::Done { .. }
    )
}

/// Human-readable receipt for `phonton goal` without `--json`: what happened,
/// what changed, what was checked, what it cost, and what to do next.
fn headless_summary_lines(task_id: TaskId, state: &GlobalState) -> Vec<String> {
    let mut out = vec![format!(
        "phonton goal: {} ({task_id})",
        headless_status_label(&state.task_status)
    )];
    let packet = state.handoff_packet.as_ref();
    if let TaskStatus::Failed { reason, .. } = &state.task_status {
        out.push(format!("  reason: {reason}"));
    }
    if let Some(p) = packet {
        for f in &p.changed_files {
            out.push(format!(
                "  {}  +{} -{}",
                f.path.display(),
                f.added_lines,
                f.removed_lines
            ));
        }
        for passed in &p.verification.passed {
            out.push(format!("  ✓ {passed}"));
        }
        for finding in p.verification.findings.iter().take(3) {
            out.push(format!("  ! {finding}"));
        }
    }
    if state.tokens_used > 0 {
        let cost = if state.cost_receipt.pricing_known {
            format!(
                ", est. {}",
                format_usd_micros(state.cost_receipt.actual_usd_micros)
            )
        } else {
            String::new()
        };
        out.push(format!("  tokens: {}{cost}", state.tokens_used));
    }
    match &state.task_status {
        TaskStatus::Reviewing { .. } => out.push(
            "  next: phonton review latest, then phonton review approve latest (or reject latest)"
                .into(),
        ),
        TaskStatus::Paused { .. } => out.push(format!("  next: phonton goal --resume {task_id}")),
        TaskStatus::Failed { .. } => {
            out.push("  next: phonton doctor, or phonton review latest for details".into())
        }
        _ => {}
    }
    out
}

fn headless_status_label(status: &TaskStatus) -> &'static str {
    match status {
        TaskStatus::Queued => "queued",
        TaskStatus::Planning => "planning",
        TaskStatus::Running { .. } => "running",
        TaskStatus::Reviewing { .. } => "review-ready",
        TaskStatus::Done { .. } => "done",
        TaskStatus::Failed { .. } => "failed",
        TaskStatus::Paused { .. } => "paused",
        TaskStatus::Rejected => "rejected",
    }
}

fn main() -> Result<()> {
    // Local-harness and orchestrator futures are deep; unoptimized builds
    // overflow Windows' 1 MiB main-thread and 2 MiB worker stacks.
    const STACK: usize = 16 * 1024 * 1024;
    std::thread::Builder::new()
        .name("phonton-main".into())
        .stack_size(STACK)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_stack_size(STACK)
                .build()?
                .block_on(run_main())
        })?
        .join()
        .unwrap_or_else(|_| std::process::exit(101))
}

async fn run_main() -> Result<()> {
    if handle_cli_args().await? {
        return Ok(());
    }
    // Load configuration first so the rest of startup can use it.
    let mut cfg = config::load()?;

    // Start background auto-update check
    let pending_update = Arc::new(std::sync::Mutex::new(None));
    let pending_update_clone = pending_update.clone();
    let enable_update = cfg.general.enable_auto_update;
    tokio::spawn(async move {
        if !enable_update {
            return;
        }
        if std::env::var("CI").is_ok() || std::env::var("INTEGRATION_TESTS").is_ok() {
            return;
        }
        // Wait a short moment to let main TUI startup finish cleanly without contention
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
        {
            Ok(c) => c,
            Err(_) => return,
        };

        let res = match client
            .get("https://registry.npmjs.org/phonton-cli/latest")
            .header("User-Agent", "phonton-cli-auto-update")
            .send()
            .await
        {
            Ok(r) => r,
            Err(_) => return,
        };

        #[derive(serde::Deserialize)]
        struct NpmLatest {
            version: String,
        }

        let latest: NpmLatest = match res.json().await {
            Ok(j) => j,
            Err(_) => return,
        };

        let current = env!("CARGO_PKG_VERSION");
        if is_newer_version(&latest.version, current) {
            if let Ok(mut guard) = pending_update_clone.lock() {
                *guard = Some(latest.version);
            }
        }
    });

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
    app.model_ready = provider_key_for_run(&cfg.provider).is_some();
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

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    execute!(stdout, SetCursorStyle::SteadyBar)?;
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
        cfg,
        working_dir,
    )
    .await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        LeaveAlternateScreen,
        crossterm::cursor::Show,
        SetCursorStyle::DefaultUserShape,
    )?;
    terminal.show_cursor()?;

    // Never install software behind the user's back: say an update exists
    // and how to get it. Installs may come from npm, cargo or a script.
    if let Ok(guard) = pending_update.lock() {
        if let Some(ref version) = *guard {
            println!(
                "phonton {version} is available (you have {}). Update: npm install -g phonton-cli@latest",
                env!("CARGO_PKG_VERSION")
            );
        }
    }

    result
}

fn spawn_input_task(tx: mpsc::Sender<LoopEvent>) {
    std::thread::spawn(move || loop {
        // Poll on a modest cadence. This keeps input responsive while
        // preventing the splash animation and terminal cursor from feeling
        // like they are flashing on every frame.
        if event::poll(Duration::from_millis(UI_TICK_MS)).unwrap_or(false) {
            if let Ok(event) = event::read() {
                let should_break = match event {
                    // IMPORTANT: Filter for 'Press' events only. Windows and some
                    // modern terminal emulators send 'Release' events too. If we
                    // handle both, the user sees "double input" (e.g. 'ww') and
                    // the TUI flickers because we redraw twice.
                    Event::Key(k) if k.kind != event::KeyEventKind::Release => {
                        tx.blocking_send(LoopEvent::Key(k)).is_err()
                    }
                    Event::Paste(text) => tx.blocking_send(LoopEvent::Paste(text)).is_err(),
                    _ => false,
                };
                if should_break {
                    break;
                }
            }
        } else if tx.blocking_send(LoopEvent::Tick).is_err() {
            break;
        }
    });
}

/// Bring up the installed managed runtime before a local goal, telling the
/// user why the goal pauses. A failure is reported and the goal continues,
/// so its own error explains what is missing.
pub(crate) async fn start_managed_runtime_if_needed() {
    match models_cli::ensure_managed_runtime().await {
        Ok(true) => eprintln!("Started the local model runtime."),
        Ok(false) => {}
        Err(error) => eprintln!(
            "Local model runtime is not running and could not be started: {error}. Run `phonton models setup`."
        ),
    }
}

/// Selected local model (from calibration state) and hardware headroom.
async fn probe_selection() -> Machine {
    let mut machine = Machine {
        probed: true,
        ..Machine::default()
    };
    if let Ok(cfg) = config::load() {
        let base_url = cfg.provider.base_url.clone().unwrap_or_default();
        if provider_is_local(&cfg.provider.name, &base_url) {
            let _ = models_cli::ensure_managed_runtime().await;
        }
    }
    if let Ok(Some(selection)) = local_goal_cli::current_model_selection().await {
        machine.local_model = Some(selection.model);
        machine.context_tokens = Some(selection.context_tokens);
        machine.protocol = selection.protocol.map(|p| {
            match p {
                phonton_types::local::EditProtocol::SearchReplace => "search/replace",
                phonton_types::local::EditProtocol::UnifiedDiff => "unified diff",
            }
            .to_string()
        });
    }
    machine
}

/// Add hardware headroom to a probed selection (nvidia-smi and CIM are slow).
async fn probe_hardware(mut machine: Machine) -> Machine {
    let hw = phonton_local::hardware::detect().await;
    machine.gpu = hw
        .gpus
        .first()
        .map(|g| (g.name.clone(), g.available_bytes, g.total_bytes));
    machine.ram = hw.ram_available_bytes.zip(hw.ram_total_bytes);
    machine
}

async fn run_app<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    rx: &mut mpsc::Receiver<LoopEvent>,
    tx: mpsc::Sender<LoopEvent>,
    store: Arc<std::sync::Mutex<Store>>,
    ask_provider: Option<Arc<dyn Provider>>,
    sandbox: Arc<Sandbox>,
    mut cfg: config::Config,
    working_dir: std::path::PathBuf,
) -> Result<()> {
    // Mutable so Save Settings can swap in a freshly-built provider after
    // the user changes the API key / model / provider in the TUI. Without
    // this the Ask side panel would keep using the original credentials
    // until the user restarted the CLI.
    let mut ask_provider = ask_provider;
    let mut approval_replies: HashMap<u64, oneshot::Sender<McpApprovalDecision>> = HashMap::new();
    let mut local_plan_replies: HashMap<TaskId, oneshot::Sender<bool>> = HashMap::new();
    app.record = record::load();
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            let machine = probe_selection().await;
            let _ = tx.send(LoopEvent::Machine(Box::new(machine.clone()))).await;
            let _ = tx
                .send(LoopEvent::Machine(Box::new(probe_hardware(machine).await)))
                .await;
        });
    }
    loop {
        let size = terminal.size()?;
        for prompt in &mut app.pending_local_plans {
            prompt.clamp_scroll(Rect::new(0, 0, size.width, size.height));
        }
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
                            local_plan_replies.clear();
                            break;
                        }
                        Intent::QueueGoal(prompt) | Intent::QueueTask(prompt) => {
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
                            if provider_is_local(&app.settings.provider, &app.settings.base_url)
                                && app.machine.local_model.is_some()
                            {
                                // Calibrated local model: run through the
                                // search/replace harness it was measured on.
                                if let Some(g) = app.goals.first_mut() {
                                    g.local_harness = true;
                                }
                                spawn_local_goal(
                                    task_id,
                                    prompt.description.clone(),
                                    tx.clone(),
                                    working_dir.clone(),
                                    app.host_checks_approved.unwrap_or(false),
                                );
                            } else {
                                if let Some(g) = app.goals.first_mut() {
                                    g.token_origin = if provider_is_local(
                                        &app.settings.provider,
                                        &app.settings.base_url,
                                    ) {
                                        record::TokenOrigin::Unknown
                                    } else {
                                        record::TokenOrigin::Hosted
                                    };
                                }
                                spawn_goal(
                                    0,
                                    task_id,
                                    prompt,
                                    direct_task,
                                    &tx,
                                    &store,
                                    &sandbox,
                                    &cfg,
                                    &working_dir,
                                    app.host_checks_approved.unwrap_or(false),
                                )
                                .await;
                            }
                        }
                        Intent::ApplyLocal(id) => {
                            let receipt = app
                                .goals
                                .iter()
                                .find(|g| g.task_id == id)
                                .and_then(|g| g.local.clone());
                            if let Some(receipt) = receipt {
                                let tx = tx.clone();
                                let repo = working_dir.clone();
                                tokio::spawn(async move {
                                    let result = local_goal_cli::apply_selected(&receipt, &repo)
                                        .await
                                        .map(|applied| {
                                            format!(
                                                "Applied candidate {} · original files kept for rollback",
                                                applied.candidate_number
                                            )
                                        })
                                        .map_err(|e| e.to_string());
                                    let _ = tx.send(LoopEvent::LocalApplied(id, result)).await;
                                });
                            }
                        }
                        Intent::ResolveLocalPlan { task_id, approved } => {
                            if let Some(reply) = local_plan_replies.remove(&task_id) {
                                let _ = reply.send(
                                    approved && app.goals.iter().any(|g| g.task_id == task_id),
                                );
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
                            app.model_ready = provider_key_for_run(&cfg.provider).is_some();

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
                                    keys: Default::default(),
                                    allow_unverified_model: None,
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
                                    keys: Default::default(),
                                    allow_unverified_model: None,
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
                                        keys: Default::default(),
                                        allow_unverified_model: None,
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
                        Intent::PasteClipboard => {
                            let tx2 = tx.clone();
                            tokio::task::spawn_blocking(move || {
                                let result = read_clipboard_text();
                                let _ = tx2.blocking_send(LoopEvent::ClipboardPaste(result));
                            });
                        }
                        Intent::AcceptTrust | Intent::DeclineTrust => {
                            // Trust prompt is handled before the TUI loop
                            // starts; if we ever see one in here it is
                            // a no-op.
                        }
                        Intent::Ask(q) => {
                            let tx2 = tx.clone();
                            let provider = ask_provider.clone();
                            let workspace_root = working_dir.clone();
                            let current_goal = app.current_goal().map(|g| g.description.clone());
                            app.ask_pending = true;
                            app.ask_answer = None;
                            tokio::spawn(async move {
                                let report = ask_context::build_ask_context(
                                    ask_context::AskContextRequest {
                                        question: &q,
                                        workspace_root: &workspace_root,
                                        attachments: &[],
                                        current_goal: current_goal.as_deref(),
                                        diagnostics: &[],
                                        max_tokens: ask_context::ASK_CONTEXT_TARGET_TOKENS,
                                    },
                                );
                                let a = match provider {
                                    Some(p) => match p
                                        .call(ask_context::ASK_SYSTEM_PROMPT, &report.prompt, &[])
                                        .await
                                    {
                                        Ok(resp) => {
                                            if report.selected_paths.is_empty() {
                                                resp.content
                                            } else {
                                                format!(
                                                    "{}\n\n---\ncontext: {} ({} tok est.)",
                                                    resp.content,
                                                    report.summary,
                                                    report.context_tokens
                                                )
                                            }
                                        }
                                        Err(e) => format!("ask failed: {e}"),
                                    },
                                    None => "Configure a provider in ~/.phonton/config.toml \
                                        (phonton doctor --provider) to enable Ask mode."
                                        .to_string(),
                                };
                                let _ = tx2.send(LoopEvent::AskAnswer(a)).await;
                            });
                        }
                    }
                }
            }
            LoopEvent::Paste(text) => {
                if app.pending_local_plans.is_empty() {
                    app.handle_paste(text);
                }
            }
            LoopEvent::ClipboardPaste(result) => match result {
                Ok(text) => {
                    if app.pending_local_plans.is_empty() {
                        app.handle_paste(text);
                    }
                }
                Err(msg) => {
                    app.goal_prompt
                        .set_notice(format!("Clipboard unavailable: {msg}"));
                }
            },
            LoopEvent::StateUpdate(id, state) => {
                if let Some(idx) = app.goals.iter().position(|g| g.task_id == id) {
                    app.apply_state(idx, *state);
                }
                for (outcome, tokens, local) in std::mem::take(&mut app.unsaved_runs) {
                    app.record = record::add_run(outcome, tokens, local);
                }
            }
            LoopEvent::Machine(machine) => app.machine = *machine,
            LoopEvent::Local(id, receipt) => {
                if let Some(g) = app.goals.iter_mut().find(|g| g.task_id == id) {
                    g.local = Some(receipt);
                }
            }
            LoopEvent::LocalFinished(id, receipt) => {
                if let Some(g) = app.goals.iter_mut().find(|g| g.task_id == id) {
                    if !g.recorded {
                        match record::record_local_receipt(&receipt) {
                            Ok(Some(record)) => {
                                app.record = record;
                                g.recorded = true;
                            }
                            Ok(None) => {}
                            Err(error) => {
                                eprintln!("Receipt retained; run record was not updated: {error}")
                            }
                        }
                    }
                    g.local = Some(receipt);
                }
            }
            LoopEvent::LocalApplied(id, result) => {
                if let Some(g) = app.goals.iter_mut().find(|g| g.task_id == id) {
                    if result.is_ok() {
                        let tokens_used = g.state.as_ref().map(|s| s.tokens_used).unwrap_or(0);
                        let wall_time_ms = g
                            .finished_at
                            .unwrap_or_else(std::time::Instant::now)
                            .saturating_duration_since(g.started_at)
                            .as_millis() as u64;
                        g.status = TaskStatus::Done {
                            tokens_used,
                            wall_time_ms,
                        };
                        if let Some(state) = g.state.as_mut() {
                            state.task_status = g.status.clone();
                        }
                    }
                    g.applied = Some(result);
                }
            }
            LoopEvent::FlightEvent(id, ev) => {
                if let Some(idx) = app.goals.iter().position(|g| g.task_id == id) {
                    app.apply_event(idx, ev);
                }
            }
            LoopEvent::AskAnswer(a) => {
                app.ask_pending = false;
                app.ask_answer = Some(a);
            }
            LoopEvent::LocalPlanRequested { prompt, reply_tx } => {
                if app.goals.iter().any(|g| g.task_id == prompt.task_id)
                    && !local_plan_replies.contains_key(&prompt.task_id)
                {
                    local_plan_replies.insert(prompt.task_id, reply_tx);
                    app.pending_local_plans.push(prompt);
                } else {
                    let _ = reply_tx.send(false);
                }
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
            local_plan_replies.clear();
            break;
        }
    }
    Ok(())
}

const MAX_TEXT_ATTACHMENT_BYTES: u64 = 64 * 1024;
const MAX_IMAGE_ATTACHMENT_BYTES: u64 = 5 * 1024 * 1024;

fn single_task_plan(
    description: String,
    attachments: Vec<PromptAttachment>,
    prompt_artifacts: Vec<PromptArtifact>,
) -> PlannerOutput {
    let goal_contract = Goal::new(description.clone())
        .with_attachments(attachments.clone())
        .with_prompt_artifacts(prompt_artifacts.clone())
        .contract();
    let subtask = Subtask {
        id: SubtaskId::new(),
        description,
        model_tier: ModelTier::Standard,
        dependencies: Vec::new(),
        attachments,
        prompt_artifacts,
        status: SubtaskStatus::Queued,
    };

    PlannerOutput {
        subtasks: vec![subtask],
        estimated_total_tokens: 1_200,
        naive_baseline_tokens: 4_000,
        coverage_summary: CoverageSummary::default(),
        goal_contract: Some(goal_contract),
        plan_graph: Default::default(),
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

fn prepare_prompt_attachments(
    prompt: &SubmittedPrompt,
    working_dir: &Path,
) -> Vec<PromptAttachment> {
    let mut text = prompt.description.clone();
    for artifact in &prompt.prompt_artifacts {
        text.push('\n');
        text.push_str(&artifact.text);
    }
    prepare_goal_attachments(&text, working_dir)
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

    for raw in extract_path_mentions(text) {
        if !mentions.iter().any(|mention| mention == &raw) {
            mentions.push(raw);
        }
    }

    mentions
}

fn extract_path_mentions(text: &str) -> Vec<String> {
    let mut mentions = Vec::new();
    for raw in text.split_whitespace() {
        let cleaned = raw
            .trim_matches(|c: char| {
                matches!(
                    c,
                    '`' | '"'
                        | '\''
                        | ','
                        | ';'
                        | ':'
                        | '.'
                        | '!'
                        | '?'
                        | '('
                        | ')'
                        | '['
                        | ']'
                        | '{'
                        | '}'
                        | '<'
                        | '>'
                )
            })
            .replace('\\', "/");
        if looks_like_file_path(&cleaned) && !mentions.iter().any(|mention| mention == &cleaned) {
            mentions.push(cleaned);
        }
    }
    mentions
}

fn looks_like_file_path(raw: &str) -> bool {
    if raw.is_empty() || raw.contains("://") {
        return false;
    }
    let Some(extension) = Path::new(raw).extension().and_then(|ext| ext.to_str()) else {
        return false;
    };
    let extension = extension.to_ascii_lowercase();
    matches!(
        extension.as_str(),
        "rs" | "py"
            | "js"
            | "jsx"
            | "ts"
            | "tsx"
            | "json"
            | "toml"
            | "md"
            | "css"
            | "html"
            | "yml"
            | "yaml"
    )
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
    let context_manifest = ContextManifest {
        plan_graph: state.plan_graph.clone(),
        index_backend: state.index_backend.clone(),
        ..ContextManifest::default()
    };
    let permission_ledger = PermissionLedger::default();
    let summaries = phonton_types::OutcomeSummaries::from_evidence(
        state.goal_contract.as_ref(),
        &context_manifest,
        &permission_ledger,
        &handoff.verification,
        Some(&handoff),
    );
    Some(OutcomeLedger {
        task_id,
        goal_contract: state.goal_contract.clone(),
        context_manifest,
        permission_ledger,
        verify_report: handoff.verification.clone(),
        summaries,
        handoff: Some(handoff),
    })
}

/// Run a goal on the calibrated local model through the local harness. The
/// working tree changes only when the user applies the reviewed candidate.
fn spawn_local_goal(
    task_id: TaskId,
    goal: String,
    tx: mpsc::Sender<LoopEvent>,
    repository: std::path::PathBuf,
    host_approved: bool,
) {
    tokio::spawn(async move {
        let worker = SubtaskId::new();
        let planning = GlobalState {
            task_status: TaskStatus::Planning,
            goal_contract: None,
            plan_graph: None,
            index_backend: None,
            handoff_packet: None,
            active_workers: Vec::new(),
            tokens_used: 0,
            tokens_budget: None,
            estimated_naive_tokens: 0,
            checkpoints: Vec::new(),
            resume_checkpoint: None,
            cost_receipt: CostReceipt::default(),
        };
        let _ = tx
            .send(LoopEvent::StateUpdate(task_id, Box::new(planning.clone())))
            .await;
        let progress = |receipt: &phonton_types::local_run::LocalRunReceipt| {
            let state = local_tui::global_state(receipt, task_id, worker);
            let _ = tx.try_send(LoopEvent::StateUpdate(task_id, Box::new(state)));
            let _ = tx.try_send(LoopEvent::Local(task_id, Box::new(receipt.clone())));
        };
        let plan_tx = tx.clone();
        let on_plan = move |reviewed: phonton_types::local_run::ReviewedLocalPlan| async move {
            let prompt = local_plan_approval::PendingLocalPlan::new(task_id, &reviewed);
            let (reply_tx, reply_rx) = oneshot::channel();
            if plan_tx
                .send(LoopEvent::LocalPlanRequested { prompt, reply_tx })
                .await
                .is_err()
            {
                return false;
            }
            reply_rx.await.unwrap_or(false)
        };
        let result =
            local_goal_cli::run_goal(goal, repository, host_approved, on_plan, progress).await;
        match result {
            Ok(receipt) => {
                let state = local_tui::global_state(&receipt, task_id, worker);
                let _ = tx
                    .send(LoopEvent::LocalFinished(task_id, Box::new(receipt)))
                    .await;
                let _ = tx
                    .send(LoopEvent::StateUpdate(task_id, Box::new(state)))
                    .await;
            }
            Err(error) => {
                let mut reason = error.to_string();
                if reason.contains("11434") || reason.to_lowercase().contains("connect") {
                    reason.push_str(
                        " · Is the local runtime running? `phonton models setup` starts it.",
                    );
                }
                let mut failed = planning;
                failed.task_status = TaskStatus::Failed {
                    reason,
                    failed_subtask: None,
                };
                let _ = tx
                    .send(LoopEvent::StateUpdate(task_id, Box::new(failed)))
                    .await;
            }
        }
    });
}

async fn spawn_goal(
    goal_index: usize,
    task_id: TaskId,
    prompt: SubmittedPrompt,
    direct_task: bool,
    tx: &mpsc::Sender<LoopEvent>,
    store: &Arc<std::sync::Mutex<Store>>,
    sandbox: &Arc<Sandbox>,
    cfg: &config::Config,
    working_dir: &std::path::PathBuf,
    host_checks_approved: bool,
) {
    let verification_execution = if host_checks_approved {
        phonton_types::verification::VerificationExecution::HostApproved
    } else {
        phonton_types::verification::VerificationExecution::RequireIsolation
    };
    let attachments = prepare_prompt_attachments(&prompt, working_dir);
    let display_text = prompt.display_text.clone();
    let prompt_artifacts = prompt.prompt_artifacts.clone();
    let has_main_artifact = prompt_artifacts
        .iter()
        .any(|artifact| artifact.role == PromptArtifactRole::MainRequest);
    let text = if has_main_artifact {
        display_text.clone()
    } else {
        prompt.description.clone()
    };
    if let Ok(g) = store.lock() {
        let _ = g.upsert_task(task_id, &display_text, &TaskStatus::Planning, 0);
    }
    let memory_store = phonton_memory::MemoryStore::new(Arc::clone(store)).await;

    let plan_result = if direct_task {
        Ok(single_task_plan(
            text.clone(),
            attachments.clone(),
            prompt_artifacts.clone(),
        ))
    } else {
        let store_guard = match store.lock() {
            Ok(g) => g,
            Err(_) => {
                fail_spawned_goal(
                    tx,
                    store,
                    task_id,
                    &display_text,
                    "persistent store lock was poisoned".into(),
                    Some(cfg.index.backend.clone()),
                )
                .await;
                return;
            }
        };
        let goal = Goal::new(text.clone())
            .with_attachments(attachments.clone())
            .with_prompt_artifacts(prompt_artifacts.clone());
        let result = decompose_with_memory(&goal, &store_guard, load_ask_provider(cfg)).await;
        drop(store_guard);
        result
    };
    let mut plan = match plan_result {
        Ok(p) => p,
        Err(e) => {
            fail_spawned_goal(
                tx,
                store,
                task_id,
                &display_text,
                format!("planning failed: {e}"),
                Some(cfg.index.backend.clone()),
            )
            .await;
            return;
        }
    };
    contract_preflight::apply_workspace_preflight(&mut plan, working_dir);

    let (state_tx, mut state_rx) = watch::channel(GlobalState {
        task_status: TaskStatus::Planning,
        goal_contract: plan.goal_contract.clone(),
        plan_graph: Some(plan.plan_graph.clone()),
        index_backend: Some(cfg.index.backend.clone()),
        handoff_packet: None,
        active_workers: Vec::new(),
        tokens_used: 0,
        tokens_budget: None,
        estimated_naive_tokens: plan.naive_baseline_tokens,
        checkpoints: Vec::new(),
        resume_checkpoint: None,
        cost_receipt: CostReceipt::default(),
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
    let semantic_context = build_semantic_context(working_dir, &cfg.index).await;
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
            // CRITICAL: honour a configured cheap/local model, but escalate on
            // per-tier ids so Standard/Frontier are not the same model as Cheap.
            let configured_model = cfg.provider.model.clone();

            let factory = move |tier: phonton_types::ModelTier| {
                let model = model_for_dispatch(&provider_name, configured_model.as_deref(), tier);
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
                    .with_memory(memory_store.clone())
                    .with_verification_execution(verification_execution);
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

    let limits = BudgetLimits {
        max_tokens: cfg.budget.max_tokens,
        max_usd_micros: cfg.budget.max_usd_micros(),
    };
    let budget_guard = apply_budget_pricing(BudgetGuard::new(limits), cfg);

    let mut orch = Orchestrator::new(dispatcher)
        .with_verification_execution(verification_execution)
        .with_naive_baseline(naive)
        .with_budget_guard(budget_guard)
        .with_working_dir(working_dir.clone())
        .with_index_backend(cfg.index.backend.clone())
        .with_memory(memory_store)
        .with_event_sink(task_id, display_text.clone(), event_tx);
    if let Some(da) = diff_applier {
        orch = orch.with_diff_applier(da);
    }

    // Drive the orchestrator and forward every `GlobalState` update.
    let tx_updates = tx.clone();
    let store_for_states = store.clone();
    let goal_text_for_states = display_text.clone();
    let plan_for_pause = plan.clone();
    let working_dir_for_pause = working_dir.clone();
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
                if let (Some(resume), TaskStatus::Paused { .. }) =
                    (&s.resume_checkpoint, &s.task_status)
                {
                    let snapshot = PausedRunSnapshot {
                        task_id,
                        goal_text: goal_text_for_states.clone(),
                        working_dir: working_dir_for_pause.display().to_string(),
                        planner_output: plan_for_pause.clone(),
                        resume: resume.clone(),
                    };
                    let _ = g.upsert_paused_run(&snapshot);
                } else if matches!(
                    s.task_status,
                    TaskStatus::Reviewing { .. } | TaskStatus::Done { .. }
                ) {
                    let _ = g.delete_paused_run(task_id);
                }
            }
            if tx_updates
                .send(LoopEvent::StateUpdate(task_id, Box::new(s)))
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
                        .send(LoopEvent::FlightEvent(task_id, rec))
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
        let _ = orch.run_task(plan, state_tx, None).await;
    });
}

fn apply_extension_context_to_plan(plan: &mut PlannerOutput, extension_set: &ExtensionSet) {
    let preamble = extension_set.render_prompt_preamble();
    if preamble.is_empty() {
        return;
    }

    for subtask in &mut plan.subtasks {
        subtask.description = format!(
            "{preamble}{}{}",
            phonton_types::PRIOR_CONTEXT_TASK_SEPARATOR,
            subtask.description
        );
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

fn is_newer_version(latest: &str, current: &str) -> bool {
    let parse = |v: &str| -> Option<(u32, u32, u32)> {
        let clean = v.trim_start_matches('v');
        let parts: Vec<&str> = clean.split('.').collect();
        if parts.len() >= 3 {
            let major = parts[0].parse().ok()?;
            let minor = parts[1].parse().ok()?;
            let patch = parts[2].parse().ok()?;
            Some((major, minor, patch))
        } else {
            None
        }
    };
    if let (Some(l), Some(c)) = (parse(latest), parse(current)) {
        l > c
    } else {
        false
    }
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

    #[test]
    fn deepseek_cli_default_uses_current_flash_id() {
        assert_eq!(default_model_for("deepseek"), "deepseek-flash");
    }

    #[test]
    fn deepseek_budget_prices_flash_and_frontier_escalation() {
        let mut cfg = config::Config::default();
        cfg.provider.name = "deepseek".into();
        let mut guard = apply_budget_pricing(
            BudgetGuard::new(BudgetLimits {
                max_tokens: None,
                max_usd_micros: Some(1_000_000),
            }),
            &cfg,
        );
        assert_eq!(
            guard
                .estimate(
                    ProviderKind::OpenAiCompatible,
                    "deepseek-flash",
                    TokenUsage {
                        input_tokens: 1_000_000,
                        ..TokenUsage::default()
                    },
                )
                .total_usd_micros,
            300_000
        );
        assert_eq!(
            guard
                .estimate(
                    ProviderKind::OpenAiCompatible,
                    "deepseek-v4-pro",
                    TokenUsage {
                        output_tokens: 1_000_000,
                        ..TokenUsage::default()
                    },
                )
                .total_usd_micros,
            3_960_000
        );
        assert_eq!(
            guard
                .estimate(
                    ProviderKind::OpenAiCompatible,
                    "deepseek-v4-flash",
                    TokenUsage {
                        input_tokens: 1_000_000,
                        ..TokenUsage::default()
                    },
                )
                .total_usd_micros,
            300_000
        );
        assert!(matches!(
            guard.charge(
                ProviderKind::OpenAiCompatible,
                "deepseek-flash",
                1_000_000,
                0
            ),
            phonton_types::BudgetDecision::Ok
        ));
        assert!(matches!(
            guard.charge(ProviderKind::OpenAiCompatible, "deepseek-v4-pro", 1_000_000, 0),
            phonton_types::BudgetDecision::Pause { limit, .. } if limit == "usd"
        ));
    }

    #[test]
    fn deepseek_prices_do_not_apply_to_custom_or_lookalike_endpoints() {
        let mut cfg = config::Config::default();
        cfg.provider.name = "deepseek".into();
        cfg.provider.base_url = Some("https://proxy.example/v1".into());
        let guard = apply_budget_pricing(BudgetGuard::new(BudgetLimits::default()), &cfg);
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            ..TokenUsage::default()
        };
        assert!(
            !guard
                .estimate(ProviderKind::OpenAiCompatible, "deepseek-flash", usage)
                .pricing_known
        );

        cfg.provider.name = "openai-compatible".into();
        cfg.provider.base_url = Some("https://notdeepseek.example/v1".into());
        let guard = apply_budget_pricing(BudgetGuard::new(BudgetLimits::default()), &cfg);
        assert!(
            !guard
                .estimate(ProviderKind::OpenAiCompatible, "deepseek-flash", usage)
                .pricing_known
        );
    }

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
            keys: Default::default(),
            allow_unverified_model: None,
        };
        assert_eq!(provider_key_for_run(&ollama).as_deref(), Some(""));

        let custom = config::ProviderConfig {
            name: "openai-compatible".into(),
            api_key: None,
            model: Some("local-model".into()),
            account_id: None,
            base_url: Some("http://localhost:1234/v1".into()),
            keys: Default::default(),
            allow_unverified_model: None,
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
        assert_eq!(app.goal_prompt.text(), "add fn foo");
    }

    #[test]
    fn enter_queues_a_goal_and_clears_input() {
        let mut app = App {
            host_checks_approved: Some(false),
            ..App::default()
        };
        for c in "hello".chars() {
            app.handle_key(key(c));
        }
        let intent = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let Some(Intent::QueueGoal(prompt)) = intent else {
            panic!("expected queued prompt, got {intent:?}");
        };
        assert_eq!(prompt.description, "hello");
        assert_eq!(app.goals.len(), 1);
        assert_eq!(app.goal_prompt.text(), "");
    }

    #[test]
    fn enter_in_task_mode_emits_direct_task_intent() {
        let mut app = App {
            mode: Mode::Task,
            host_checks_approved: Some(false),
            ..App::default()
        };
        for c in "write one focused test".chars() {
            app.handle_key(key(c));
        }
        let intent = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let Some(Intent::QueueTask(prompt)) = intent else {
            panic!("expected queued task prompt, got {intent:?}");
        };
        assert_eq!(prompt.description, "write one focused test");
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
    fn ctrl_v_requests_clipboard_paste() {
        let mut app = App::default();
        let intent = app.handle_key(ctrl('v'));
        assert_eq!(intent, Some(Intent::PasteClipboard));
    }

    #[test]
    fn multiline_paste_creates_artifact_without_queueing() {
        let mut app = App::default();
        let intent = app.handle_paste("do x\ndo y\ndo z".into());

        assert!(intent.is_none());
        assert_eq!(app.goal_prompt.text(), "");
        assert_eq!(app.goal_prompt.artifacts().len(), 1);
        assert_eq!(
            app.goal_prompt.artifacts()[0].label,
            "[paste: 3 lines, 14 chars]"
        );
        assert_eq!(app.goals.len(), 0);
    }

    #[test]
    fn enter_after_multiline_paste_queues_one_goal() {
        let mut app = App {
            host_checks_approved: Some(false),
            ..App::default()
        };
        assert!(app.handle_paste("do x\ndo y\ndo z".into()).is_none());

        let intent = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let Some(Intent::QueueGoal(prompt)) = intent else {
            panic!("expected one queued prompt, got {intent:?}");
        };
        assert_eq!(prompt.description, "do x\ndo y\ndo z");
        assert_eq!(prompt.display_text, "paste: do x");
        assert_eq!(prompt.prompt_artifacts.len(), 1);
        assert_eq!(app.goals.len(), 1);
        assert_eq!(app.goals[0].description, "paste: do x");
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
    fn goal_mentions_attach_backtick_paths() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let src = temp.path().join("src");
        std::fs::create_dir(&src)?;
        std::fs::write(src.join("config.js"), "module.exports = { port: 3000 };\n")?;

        let attachments =
            prepare_goal_attachments("Fix `src/config.js` without changing tests.", temp.path());

        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].path, PathBuf::from("src/config.js"));
        assert_eq!(attachments[0].kind, PromptAttachmentKind::Text);
        assert!(attachments[0]
            .text
            .as_deref()
            .unwrap_or("")
            .contains("3000"));
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
    fn headless_goal_parser_accepts_benchmark_flags() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let prompt_path = temp.path().join("prompt.md");
        std::fs::write(&prompt_path, "Fix the fixture bug\n\nUse tests.")?;
        let args = vec![
            "--prompt-file".to_string(),
            prompt_path.display().to_string(),
            "--yes".to_string(),
            "--timeout-seconds".to_string(),
            "900".to_string(),
            "--json".to_string(),
        ];

        let opts = parse_headless_goal_options(&args)?;

        assert_eq!(opts.goal_text, "Fix the fixture bug\n\nUse tests.");
        assert_eq!(opts.display_text, "Fix the fixture bug");
        assert!(opts.yes);
        assert!(!opts.host_checks_approved);
        assert!(opts.json);
        assert!(!opts.direct_task);
        assert_eq!(opts.timeout_seconds, 900);
        Ok(())
    }

    #[test]
    fn headless_goal_host_checks_require_separate_explicit_flag() -> Result<()> {
        let default = parse_headless_goal_options(&["fix bug".into(), "--yes".into()])?;
        assert!(!default.host_checks_approved);

        let approved =
            parse_headless_goal_options(&["fix bug".into(), "--allow-host-checks".into()])?;
        assert!(approved.host_checks_approved);
        assert!(!approved.yes);

        let resumed =
            parse_headless_goal_options(&["--resume".into(), uuid::Uuid::new_v4().to_string()])?;
        assert!(!resumed.host_checks_approved);

        let err = parse_headless_goal_options(&[
            "fix bug".into(),
            "--permission-mode".into(),
            "full-access".into(),
        ])
        .expect_err("legacy mode must not silently approve host checks");
        assert!(err.to_string().contains("--allow-host-checks"));
        Ok(())
    }

    #[test]
    fn headless_resume_requires_original_workspace_with_or_without_host_approval() -> Result<()> {
        let root = tempfile::tempdir()?;
        let original = root.path().join("original");
        let other = root.path().join("other");
        std::fs::create_dir_all(&original)?;
        std::fs::create_dir_all(&other)?;
        let task_id = uuid::Uuid::new_v4().to_string();

        for approve_host in [false, true] {
            let mut args = vec!["--resume".to_string(), task_id.clone()];
            if approve_host {
                args.push("--allow-host-checks".into());
            }
            let opts = parse_headless_goal_options(&args)?;
            assert_eq!(opts.host_checks_approved, approve_host);
            ensure_resume_workspace(&original.display().to_string(), &original)?;
            let error = ensure_resume_workspace(&original.display().to_string(), &other)
                .expect_err("a paused plan must not run in another repository");
            assert!(error.to_string().contains("resume it from that workspace"));
        }
        Ok(())
    }

    #[test]
    fn headless_summary_explains_failure_and_next_step() {
        let task_id = TaskId::new();
        let reason = "provider returned 401 Unauthorized";
        let state = GlobalState {
            task_status: TaskStatus::Failed {
                reason: reason.into(),
                failed_subtask: None,
            },
            goal_contract: None,
            plan_graph: None,
            index_backend: None,
            handoff_packet: Some(failed_handoff_packet(task_id, "fix it", reason, 0)),
            active_workers: Vec::new(),
            tokens_used: 0,
            tokens_budget: None,
            estimated_naive_tokens: 0,
            checkpoints: Vec::new(),
            resume_checkpoint: None,
            cost_receipt: CostReceipt::default(),
        };
        let text = headless_summary_lines(task_id, &state).join("\n");
        assert!(text.starts_with("phonton goal: failed"), "{text}");
        assert!(text.contains(reason), "{text}");
        assert!(text.contains("next: phonton doctor"), "{text}");
    }

    #[test]
    fn headless_goal_parser_rejects_multiple_goal_sources() {
        let args = vec![
            "--prompt-file".to_string(),
            "prompt.md".to_string(),
            "also positional".to_string(),
        ];

        let err = parse_headless_goal_options(&args)
            .expect_err("prompt-file and positional text should conflict");
        assert!(err.to_string().contains("choose only one goal source"));
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
    async fn worker_mcp_approval_does_not_grant_verification_execution() -> Result<()> {
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
            prompt_artifacts: Vec::new(),
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
            matches!(result.status, SubtaskStatus::Failed { .. }),
            "MCP approval must not certify a diff without execution authority, got {:?}",
            result.status
        );
        assert!(
            matches!(
                result.verify_result,
                phonton_types::VerifyResult::Unavailable { .. }
            ),
            "MCP approval is separate from project verification, got {:?}",
            result.verify_result
        );
        assert!(
            result.diff_hunks.is_empty(),
            "unverified output must not reach apply"
        );
        assert!(!temp.path().join("src/mcp_fixture.rs").exists());
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
    fn esc_from_goal_quits_after_confirmation() {
        let mut app = App::default();
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.handle_key(esc), None);
        assert!(!app.should_quit);
        assert!(app.quit_armed());
        assert_eq!(app.handle_key(esc), Some(Intent::Quit));
        assert!(app.should_quit);
    }

    #[test]
    fn local_providers_use_the_configured_model_on_every_tier() {
        for tier in [ModelTier::Cheap, ModelTier::Standard, ModelTier::Frontier] {
            assert_eq!(
                model_for_dispatch("ollama", Some("qwen2.5-coder:7b"), tier),
                "qwen2.5-coder:7b"
            );
        }
        // Keyed providers still escalate along their tier ladder.
        assert_ne!(
            model_for_dispatch(
                "anthropic",
                Some("claude-haiku-4-5-20251001"),
                ModelTier::Frontier
            ),
            "claude-haiku-4-5-20251001"
        );
    }

    #[test]
    fn first_goal_asks_for_host_checks_once() {
        let mut app = App::default();
        app.goal_prompt.insert_text("fix the parser");
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.handle_key(enter), None);
        assert!(app.pending_host_goal.is_some());
        assert!(app.goals.is_empty());
        let r = app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(matches!(r, Some(Intent::QueueGoal(_))));
        assert_eq!(app.host_checks_approved, Some(true));
        assert_eq!(app.goals.len(), 1);

        app.goal_prompt.insert_text("second goal");
        assert!(matches!(app.handle_key(enter), Some(Intent::QueueGoal(_))));
    }

    #[test]
    fn esc_on_host_checks_prompt_restores_goal_text() {
        let mut app = App::default();
        app.goal_prompt.insert_text("fix the parser");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            None
        );
        assert!(app.pending_host_goal.is_none());
        assert_eq!(app.host_checks_approved, None);
        assert_eq!(app.goal_prompt.text(), "fix the parser");
        assert!(!app.should_quit);
    }

    #[test]
    fn ctrl_c_needs_confirmation_too() {
        let mut app = App::default();
        assert_eq!(app.handle_key(ctrl('c')), None);
        assert_eq!(app.handle_key(ctrl('c')), Some(Intent::Quit));
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
            goal_contract: None,
            plan_graph: None,
            index_backend: None,
            handoff_packet: None,
            active_workers: Vec::new(),
            tokens_used: 200,
            tokens_budget: None,
            estimated_naive_tokens: 1000,
            checkpoints: Vec::new(),
            resume_checkpoint: None,
            cost_receipt: CostReceipt::default(),
        };
        let line = render_savings_line(Some(&s));
        assert!(line.contains("200"));
        assert!(line.contains("1000"));
        assert!(line.contains("80%"));
    }

    #[test]
    fn savings_line_shows_frontier_when_receipt_present() {
        let s = GlobalState {
            task_status: TaskStatus::Queued,
            goal_contract: None,
            plan_graph: None,
            index_backend: None,
            handoff_packet: None,
            active_workers: Vec::new(),
            tokens_used: 200,
            tokens_budget: None,
            estimated_naive_tokens: 1000,
            checkpoints: Vec::new(),
            resume_checkpoint: None,
            cost_receipt: CostReceipt {
                actual_usd_micros: 220,
                frontier_equivalent_usd_micros: 15_000,
                saved_usd_micros: 14_780,
                pricing_known: true,
                route: Vec::new(),
            },
        };
        let line = render_savings_line(Some(&s));
        assert!(line.contains("frontier"));
        assert!(line.contains("99%"));
    }

    #[test]
    fn savings_line_handles_missing_state() {
        let line = render_savings_line(None);
        assert!(line.contains("frontier"));
    }

    #[test]
    fn footer_hints_fit_width_and_keep_quit() {
        let hints = [
            ("Enter", "run"),
            ("/", "commands"),
            ("?", "help"),
            ("Esc", "quit"),
        ];
        let sep = Span::raw("  ·  ");
        let wide = fit_footer_hints(&hints, 200, Style::default(), Style::default(), sep.clone());
        let wide_text: String = wide.iter().map(|s| s.content.to_string()).collect();
        assert!(wide_text.contains("commands") && wide_text.ends_with("Esc quit"));
        let narrow = fit_footer_hints(&hints, 24, Style::default(), Style::default(), sep);
        let narrow_text: String = narrow.iter().map(|s| s.content.to_string()).collect();
        assert!(narrow_text.chars().count() <= 24, "{narrow_text}");
        assert!(narrow_text.starts_with("Enter run") && narrow_text.ends_with("Esc quit"));
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
        let max_width = art::LOGO
            .iter()
            .map(|row| char_count(row))
            .max()
            .unwrap_or(0);
        assert!(max_width <= LOGO_WIDTH_THRESHOLD as usize);
        assert!(
            art::LOGO[0].contains("██████╗"),
            "logo should use the standard ANSI Shadow wordmark"
        );
        assert_eq!(art::logo(None).len(), art::LOGO_ROWS as usize);
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
        assert_eq!(app.settings.api_key, "sk-ant-FAKE_TEST_KEY_123456");
        assert_eq!(app.settings.provider, "anthropic");
    }

    #[test]
    fn goal_without_a_model_opens_settings_and_keeps_the_text() {
        let mut app = App {
            model_ready: false,
            host_checks_approved: Some(true),
            ..App::default()
        };
        for c in "fix the failing test".chars() {
            app.handle_key(key(c));
        }
        let intent = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(intent.is_none());
        assert!(app.goals.is_empty());
        assert_eq!(app.mode, Mode::Settings);
        assert_eq!(app.goal_prompt.text(), "fix the failing test");
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
                plan_graph: None,
                index_backend: None,
                handoff_packet: Some(HandoffPacket {
                    schema_version: phonton_types::HANDOFF_PACKET_SCHEMA_VERSION.to_string(),
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
                    screenshot_path: None,
                    rendering_summary: None,
                    cost_receipt: CostReceipt::default(),
                }),
                active_workers: Vec::new(),
                tokens_used: 240,
                tokens_budget: None,
                estimated_naive_tokens: 1000,
                checkpoints: Vec::new(),
                resume_checkpoint: None,
                cost_receipt: CostReceipt::default(),
            },
        );

        terminal.draw(|f| render(f, &app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let dump: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(dump.contains("receipt"));
        assert!(dump.contains("Changed files"));
        assert!(dump.contains("chess.py"));
        assert!(dump.contains("Known gaps"));
        // Syntax passed but no test ran: the receipt must not claim VERIFIED,
        // and the run does not extend the streak.
        assert!(!dump.contains("✓ VERIFIED"));
        assert_eq!(app.record.streak, 0);
        assert_eq!(app.unsaved_runs.len(), 1);
        // Intermediate terminal local progress must not count before the
        // durable LocalFinished event. Later settings cannot relabel a route.
        let state = app.goals[0].state.clone().unwrap();
        app.goals[0].recorded = false;
        app.goals[0].local_harness = true;
        app.record = record::Record::default();
        app.unsaved_runs.clear();
        app.apply_state(0, state.clone());
        assert_eq!(app.record.runs, 0);
        assert!(app.unsaved_runs.is_empty());
        app.goals[0].local_harness = false;
        app.goals[0].token_origin = record::TokenOrigin::Hosted;
        app.settings.provider = "ollama".into();
        app.apply_state(0, state);
        assert_eq!(app.record.cloud_tokens, 240);
        assert_eq!(app.record.local_tokens, 0);
    }

    #[test]
    fn receipt_cost_uses_saved_origin_not_current_provider_settings() {
        let mut g = GoalEntry::new("old run".into());
        let cost = CostReceipt::default();
        g.token_origin = record::TokenOrigin::Hosted;
        assert_eq!(receipt_cost(&g, &cost), "unpriced · hosted provider");
        g.token_origin = record::TokenOrigin::Unknown;
        assert_eq!(receipt_cost(&g, &cost), "unpriced · runtime origin unknown");
        g.token_origin = record::TokenOrigin::ManagedLocal;
        assert_eq!(receipt_cost(&g, &cost), "$0.00 · managed local runtime");
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
                plan_graph: None,
                index_backend: None,
                handoff_packet: None,
                active_workers: Vec::new(),
                tokens_used: 150,
                tokens_budget: None,
                estimated_naive_tokens: 500,
                checkpoints: Vec::new(),
                resume_checkpoint: None,
                cost_receipt: CostReceipt::default(),
            },
        );
        terminal.draw(|f| render(f, &app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let dump: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(dump.contains("add function foo"));
        assert!(dump.contains("vs Σ 500") || dump.contains("baseline: 500"));
    }

    #[test]
    fn interactive_clarification_loop_works() {
        let mut app = App::default();
        let mut g = GoalEntry::new("make chess".into());

        // Mock a state with a goal contract that has clarification questions
        let contract = phonton_types::GoalContract {
            goal: "make chess".into(),
            task_class: phonton_types::TaskClass::CoreLogic,
            intent: None,
            confidence_percent: 50,
            acceptance_criteria: vec![],
            acceptance_slices: Vec::new(),
            expected_artifacts: vec![],
            likely_files: vec![],
            verify_plan: vec![],
            run_plan: vec![],
            quality_floor: phonton_types::QualityFloor { criteria: vec![] },
            clarification_questions: vec![
                "What board theme?".to_string(),
                "Enable AI opponent?".to_string(),
            ],
            assumptions: vec![],
            token_policy: Default::default(),
        };

        let state = GlobalState {
            task_status: TaskStatus::Failed {
                reason: "Under-specified requirements".to_string(),
                failed_subtask: None,
            },
            goal_contract: Some(contract),
            plan_graph: None,
            index_backend: None,
            handoff_packet: None,
            active_workers: vec![],
            tokens_used: 100,
            tokens_budget: None,
            estimated_naive_tokens: 400,
            checkpoints: vec![],
            resume_checkpoint: None,
            cost_receipt: CostReceipt::default(),
        };

        g.state = Some(state);
        g.status = TaskStatus::Failed {
            reason: "Under-specified requirements".to_string(),
            failed_subtask: None,
        };

        app.goals.push(g);
        app.selected = 0;
        app.mode = Mode::Goal;

        // Press 'c' to enter Mode::Clarify
        let intent1 = app.handle_key(key('c'));
        assert!(intent1.is_none());
        assert_eq!(app.mode, Mode::Clarify);
        assert_eq!(app.clarifying_goal_idx, Some(0));
        assert_eq!(app.clarifying_question_idx, 0);

        // Type answer to first question: "glass"
        for c in "glass".chars() {
            app.handle_key(key(c));
        }
        assert_eq!(app.clarifying_buffer, "glass");

        // Press Enter to submit first answer
        let intent2 = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(intent2.is_none());
        assert_eq!(app.clarifying_question_idx, 1);
        assert_eq!(app.clarifying_answers, vec!["glass".to_string()]);
        assert_eq!(app.clarifying_buffer, "");

        // Type answer to second question: "no"
        for c in "no".chars() {
            app.handle_key(key(c));
        }

        // Press Enter to submit final answer and trigger re-queue
        let intent3 = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(intent3.is_some());

        if let Some(Intent::QueueGoal(submitted)) = intent3 {
            assert!(submitted.description.contains("make chess"));
            assert!(submitted.description.contains("Refined requirements:"));
            assert!(submitted
                .description
                .contains("- Q: What board theme?\n  A: glass"));
            assert!(submitted
                .description
                .contains("- Q: Enable AI opponent?\n  A: no"));

            // Check that the original goal entry description was updated
            let updated_goal = &app.goals[0];
            assert!(updated_goal.description.contains("Refined requirements:"));
            assert_eq!(updated_goal.status, TaskStatus::Queued);
            assert!(updated_goal.state.is_none());
            assert!(updated_goal.flight_log.is_empty());
        } else {
            panic!("Expected Intent::QueueGoal");
        }

        // Check app state reset
        assert_eq!(app.mode, Mode::Goal);
        assert_eq!(app.clarifying_goal_idx, None);
        assert_eq!(app.clarifying_question_idx, 0);
        assert!(app.clarifying_answers.is_empty());
        assert!(app.clarifying_buffer.is_empty());
    }
}

#[cfg(test)]
mod tui_screen_tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn screen(app: &App, w: u16, h: u16) -> Vec<String> {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| render(f, app)).unwrap();
        let buf = t.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
    }

    #[test]
    fn idle_screen_pitches_the_loop_and_footer_fits() {
        let rows = screen(&App::default(), 120, 36);
        let all = rows.join("\n");
        assert!(all.contains("· goal"), "{all}");
        assert!(all.contains("· remember"), "{all}");
        assert!(all.contains("No goals yet."));
        assert!(all.contains("machine"), "{all}");
        let footer = rows.last().unwrap().trim_end();
        assert!(footer.contains("Esc quit"), "footer clipped: {footer}");
    }

    #[test]
    fn help_overlay_rows_are_not_clipped() {
        let app = App {
            help_open: true,
            ..Default::default()
        };
        let all = screen(&app, 120, 40).join("\n");
        assert!(all.contains("rollback to the highlighted checkpoint (input empty)"));
    }

    #[test]
    fn narrow_footer_still_offers_quit() {
        let rows = screen(&App::default(), 70, 30);
        assert!(rows.last().unwrap().contains("Esc quit"));
    }
}
