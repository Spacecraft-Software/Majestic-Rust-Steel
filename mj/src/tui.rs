// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The interactive terminal front-end: a crossterm raw-mode loop driving the editor and an
//! optional integrated terminal panel.
//!
//! This is the only place that touches the real terminal. It enables raw mode + the alternate
//! screen (restored on drop, even on panic), reads crossterm events, and routes them through
//! an [`App`] that holds the [`Editor`] and a lazily-spawned [`PtyTerminal`]. The screen is laid
//! out per `UI.md`: the editor area on top, a labelled **terminal panel** docked along the
//! bottom (a tabbed divider in Steel Blue), and a one-row global status bar beneath it. `F12`
//! spawns/toggles focus between the editor and the terminal panel; while the panel is focused,
//! keys are encoded to bytes and written to the PTY. Each frame is presented through a Penumbra
//! [`Screen`]. The editor model itself is backend-agnostic and tested headless.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::cursor;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode as TermKey, KeyEvent,
    KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};

use keymaker::{KeyCode, KeyPress, Mods, Profile};

#[cfg(feature = "agent")]
use crate::agent_host::AgentHost;
use crate::agent_panel::{AgentPanel, AGENT_COLS};
use majestic_core::{
    Action, CodeActions, Completion, Editor, FileTree, Finder, HelpOverlay, Hover, InfoReader,
    Occurrence, ProfileSelector, Prompt, References, RenameEdit, Search, Session, SignatureHelp,
    Symbols, Workspace,
};
use majestic_lsp::{LspManager, LspOutcome, ServerHealth};
use majestic_term::PtyTerminal;
use penumbra::{Buffer, Rect, Screen, Style, Theme};

/// The `F12` key spawns/toggles the integrated terminal (reassignable once the Architect lands at M3).
const TERMINAL_TOGGLE: KeyCode = KeyCode::Function(12);

/// ``Ctrl+` `` drops down the Warp-style **Architect** terminal pane: the same shell as the bottom panel,
/// but from the top, with `Tab` flipping the input line into natural-language `Architect` mode.
const ARCHITECT_TERMINAL_TOGGLE: KeyPress = KeyPress::ctrl('`');

/// Default height of the Architect drop-down terminal in rows (header + shell grid + an NL input row).
const ARCHITECT_TERMINAL_ROWS: u16 = 12;

/// Default height of the bottom terminal panel in rows (UI.md §5: 8–15, resizable later).
const PANEL_ROWS: u16 = 10;

/// Editor rows kept visible above the panel; below this total the panel is hidden.
const MIN_EDITOR_ROWS: u16 = 3;

/// Width of the explorer sidebar in columns (UI.md §2: 20–35, resizable later).
const SIDEBAR_COLS: u16 = 28;

/// Editor columns kept usable beside the sidebar; below this the sidebar is not drawn.
const MIN_MAIN_COLS: u16 = 24;

/// Restores the terminal (cooked mode, main screen, visible cursor) when dropped.
pub(crate) struct TerminalGuard {
    /// Whether the Kitty keyboard protocol was enabled (so it is popped on teardown).
    keyboard_enhanced: bool,
}

impl TerminalGuard {
    pub(crate) fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            cursor::Hide
        )?;
        // Enable the Kitty keyboard protocol where the terminal supports it (PRD-01 §6.5). Without
        // it, legacy terminals collapse chords like `Ctrl+\`` to a NUL byte — indistinguishable from
        // `Ctrl+Space` — so the Architect-terminal toggle never matches. `DISAMBIGUATE_ESCAPE_CODES`
        // makes those chords distinct; `translate` already ignores the protocol's release events.
        // Graceful fallback: if the terminal does not support it, it stays off.
        let keyboard_enhanced = terminal::supports_keyboard_enhancement().unwrap_or(false);
        if keyboard_enhanced {
            execute!(
                io::stdout(),
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )?;
        }
        Ok(Self { keyboard_enhanced })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.keyboard_enhanced {
            let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
        }
        let _ = execute!(
            io::stdout(),
            cursor::Show,
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}

/// Which surface currently has keyboard focus.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Editor,
    Explorer,
    Terminal,
    Architect,
    ArchitectTerminal,
}

/// The `Ctrl+B` key toggles the explorer sidebar (VS Code convention).
const SIDEBAR_TOGGLE: KeyPress = KeyPress::ctrl('b');

/// The `Ctrl+P` key opens the fuzzy file finder (the command palette is `Ctrl+Shift+P`).
const FILE_FINDER: KeyPress = KeyPress::ctrl('p');

/// The `F1` key opens (and closes) the Oracle key-bindings help overlay.
const HELP_KEY: KeyPress = KeyPress::key(KeyCode::Function(1));

/// The `Ctrl+Space` key requests LSP completion at the cursor (the editor convention).
const COMPLETION_KEY: KeyPress = KeyPress::ctrl(' ');

/// The `F2` key requests LSP hover documentation at the cursor (the keyboard counterpart to mouse
/// hover; an F-key, like F1/F12, so it is safe to capture globally without shadowing an editing key).
const HOVER_KEY: KeyPress = KeyPress::key(KeyCode::Function(2));

/// The `Ctrl+F12` key requests LSP goto-definition at the cursor. Plain `F12` toggles the integrated
/// terminal (the visible status-bar affordance), so goto-definition takes the `Ctrl` chord — like its
/// `Shift+F12` find-references companion it is dispatched before the modifier-agnostic terminal toggle
/// so the chord opens the jump rather than toggling the panel.
const GOTO_DEF_KEY: KeyPress = KeyPress::new(Mods::CTRL, KeyCode::Function(12));

/// The `Shift+F12` key requests LSP find-references at the cursor (the universal "Find All
/// References" shortcut, the companion to `Ctrl+F12` goto-definition). Matched via
/// [`is_references_key`] (tolerant of how terminals report the chord), and dispatched before the
/// modifier-agnostic terminal toggle so `Shift+F12` opens the references popup rather than toggling.
const REFERENCES_KEY: KeyPress = KeyPress::new(Mods::SHIFT, KeyCode::Function(12));

/// The `Shift+F6` key starts an LSP rename of the symbol at the cursor (a common IDE binding;
/// plain `F2` is already taken by hover here). Matched via [`is_rename_key`], tolerant of how
/// terminals report the chord.
const RENAME_KEY: KeyPress = KeyPress::new(Mods::SHIFT, KeyCode::Function(6));

/// `F8` — jump the cursor to the next diagnostic (the common "go to next problem" key). `Shift+F8`
/// jumps to the previous one (matched via [`is_prev_diagnostic_key`], tolerant of the chord).
const NEXT_DIAGNOSTIC_KEY: KeyPress = KeyPress::key(KeyCode::Function(8));

/// How long the cursor / typing must settle before a debounced LSP request fires. Short enough that
/// the signature/highlight still feels live, long enough to coalesce a burst of cursor moves or
/// keystrokes into a single request (so holding an arrow key or typing fast never floods the server).
const LSP_DEBOUNCE: Duration = Duration::from_millis(120);

/// Which auto-issued LSP request is pending behind the debounce.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PendingKind {
    /// `textDocument/documentHighlight` (auto-issued as the cursor moves between identifiers).
    DocumentHighlight,
    /// `textDocument/signatureHelp` (auto-issued as `(`/`,`/`)` are typed in a call).
    SignatureHelp,
}

/// A debounced LSP request, fired once its deadline `at` passes (unless superseded first).
struct PendingLsp {
    kind: PendingKind,
    path: PathBuf,
    byte: usize,
    at: Instant,
}

/// Which way to step when navigating diagnostics (`F8` next / `Shift+F8` previous).
#[derive(Clone, Copy)]
enum Direction {
    Next,
    Prev,
}

/// What the modal prompt is collecting, resolved when it is confirmed.
enum PromptAction {
    /// Rename the symbol recorded at `(path, byte)` to the typed name (LSP `textDocument/rename`).
    Rename { path: PathBuf, byte: usize },
    /// Search-and-replace, stage 1: the typed text becomes the search term, then stage 2 opens.
    ReplaceSearch,
    /// Search-and-replace, stage 2: the typed text replaces every `search` occurrence in the buffer.
    ReplaceWith { search: String },
    /// Go to a line: the typed (1-based) line number moves the cursor to that line.
    GotoLine,
    /// Go to symbol in the project: the typed text queries `workspace/symbol`; matches open the picker.
    WorkspaceSymbol,
    /// Ask the Architect agent: the typed natural-language request starts an agent turn (`Ctrl+Shift+N`).
    #[cfg(feature = "agent")]
    AskAgent,
    /// Edit a pending agent edit's text before applying it (the Edit option of the approval card).
    #[cfg(feature = "agent")]
    EditApproval,
}

/// The running application: the editor workspace, an optional explorer sidebar, and an optional
/// integrated terminal. The front ends (the `mj` TTY loop and `mj-nova`'s GPU loop) own one of these
/// and drive it — rendering it into a cell [`penumbra::Buffer`] and feeding it `keymaker::KeyPress`es.
#[expect(
    missing_debug_implementations,
    reason = "App aggregates editor subsystems (PTY terminal, LSP client, highlighter) that do not \
              all implement Debug; it is internal editor state driven by a front end, not a public \
              data type"
)]
pub struct App {
    workspace: Workspace,
    explorer: Option<FileTree>,
    sidebar_visible: bool,
    terminal: Option<PtyTerminal>,
    /// Whether the Architect drop-down terminal pane is shown (over the top of the editor).
    architect_terminal_visible: bool,
    /// In the Architect terminal, whether the input line is in natural-language `Architect` mode (`Tab`) rather
    /// than feeding the shell. Always `false` in a build without the `agent` feature.
    architect_terminal_nl: bool,
    /// The in-progress natural-language line typed in the Architect terminal's NL mode.
    architect_terminal_input: String,
    finder: Option<Finder>,
    help: Option<HelpOverlay>,
    info: Option<InfoReader>,
    /// The first-run profile picker; `Some` until the user chooses (modal while open).
    selector: Option<ProfileSelector>,
    /// The LSP completion popup; `Some` while candidates are shown over the editor.
    completion: Option<Completion>,
    /// The byte offset where the in-progress identifier (the prefix being completed) starts; an
    /// accepted candidate replaces `completion_anchor..cursor`.
    completion_anchor: usize,
    /// The LSP hover popup; `Some` while documentation is shown over the editor.
    hover: Option<Hover>,
    /// The LSP find-references popup; `Some` while a symbol's use sites are shown over the editor.
    references: Option<References>,
    /// The LSP document-symbols picker; `Some` while the file's outline is shown over the editor.
    symbols: Option<Symbols>,
    /// The LSP signature-help popup; `Some` while the active call's signature is shown over the
    /// editor. Passive: it does not capture keys (you keep typing arguments under it).
    signature: Option<SignatureHelp>,
    /// The LSP code-actions menu; `Some` while the quick-fixes/refactors at the cursor are shown.
    code_actions: Option<CodeActions>,
    /// The modal rename input; `Some` while the user is typing the new name (captures every key).
    prompt: Option<Prompt>,
    /// The incremental in-buffer search (`find`); `Some` while the search line captures keys. It
    /// tints all matches and parks the cursor on the active one.
    search: Option<Search>,
    /// What the open [`Self::prompt`] is collecting input for (rename, or a stage of search/replace),
    /// applied when the prompt is confirmed. `Some` exactly when `prompt` is.
    prompt_action: Option<PromptAction>,
    /// The `(path, identifier byte-span)` the document-highlight tint currently tracks, so the host
    /// re-requests occurrences only when the cursor moves to a different identifier.
    highlight_anchor: Option<(PathBuf, std::ops::Range<usize>)>,
    /// A debounced auto-issued LSP request (document highlight / signature help) waiting for the
    /// cursor or typing to settle; superseded by a newer one, fired once its deadline passes.
    pending_lsp: Option<PendingLsp>,
    /// A debounced whole-document inlay-hint request (`path`, deadline), scheduled when the buffer
    /// changes. Separate from `pending_lsp` since inlay hints are edit-driven, not cursor-driven.
    pending_inlay: Option<(PathBuf, Instant)>,
    /// Language servers + document sync (diagnostics). Servers start lazily on first matching file.
    lsp: LspManager,
    /// The buffer revision last sent to a language server, keyed by path (so an unchanged buffer
    /// is not re-synced each frame).
    lsp_synced: HashMap<PathBuf, u64>,
    /// The path + buffer revision of an in-flight format request, recorded when `Shift+Alt+F` is
    /// pressed. A returned reformat is applied only while the buffer is still on this revision, so an
    /// edit made while the request was in flight is never clobbered by stale output.
    format_request: Option<(PathBuf, u64)>,
    /// The Architect agent sidebar (right of the editor). Hidden until toggled with `Ctrl+Shift+A`.
    agent: AgentPanel,
    /// The governed agent's host — config + in-flight turn. Present with the `agent` feature.
    #[cfg(feature = "agent")]
    agent_host: AgentHost,
    focus: Focus,
}

impl App {
    fn new(workspace: Workspace) -> Self {
        Self {
            workspace,
            explorer: None,
            sidebar_visible: false,
            terminal: None,
            architect_terminal_visible: false,
            architect_terminal_nl: false,
            architect_terminal_input: String::new(),
            finder: None,
            help: None,
            info: None,
            selector: None,
            completion: None,
            completion_anchor: 0,
            hover: None,
            references: None,
            symbols: None,
            signature: None,
            code_actions: None,
            prompt: None,
            search: None,
            prompt_action: None,
            highlight_anchor: None,
            pending_lsp: None,
            pending_inlay: None,
            lsp: LspManager::with_defaults(),
            lsp_synced: HashMap::new(),
            format_request: None,
            agent: AgentPanel::new(),
            #[cfg(feature = "agent")]
            agent_host: AgentHost::new(),
            focus: Focus::Editor,
        }
    }

    /// Reconciles the focused buffer with its language server and applies any published
    /// diagnostics. Cheap each frame: it only sends `didOpen`/`didChange` when the buffer's
    /// revision changed, and `poll` is non-blocking (servers start on a background thread).
    fn sync_lsp(&mut self) {
        let active = {
            let editor = self.workspace.active();
            editor
                .buffer()
                .path()
                .map(|path| (path, editor.buffer().revision()))
        };
        if let Some((path, revision)) = active {
            if self.lsp.handles(&path) && self.lsp_synced.get(&path).copied() != Some(revision) {
                let first = !self.lsp_synced.contains_key(&path);
                let text = self.workspace.active().buffer().text();
                let result = if first {
                    self.lsp.open(&path, &text)
                } else {
                    self.lsp.change(&path, &text)
                };
                if result.is_ok() {
                    self.lsp_synced.insert(path.clone(), revision);
                    // The buffer (re)synced — refresh its inlay hints, debounced against a burst of
                    // edits.
                    self.pending_inlay = Some((path, Instant::now() + LSP_DEBOUNCE));
                }
            }
        }
        for (path, diagnostics) in self.lsp.poll() {
            self.workspace.apply_diagnostics(&path, &diagnostics);
        }
        self.apply_lsp_outcomes();
        self.refresh_document_highlight();
        self.fire_due_lsp();
    }

    /// Re-requests the symbol occurrences to tint (LSP `documentHighlight`) when the cursor has moved
    /// to a different identifier since the last request, and clears the tint when it leaves one. Cheap
    /// each frame: it only issues a request (off-thread) on an actual identifier change, so holding an
    /// arrow key does not flood the server.
    fn refresh_document_highlight(&mut self) {
        // While searching, the buffer tint belongs to the search matches — don't fight over it.
        if self.search.is_some() {
            return;
        }
        // The (path, identifier byte-span) the cursor is on, when on a symbol in an LSP buffer.
        let target = if self.focus == Focus::Editor {
            let editor = self.workspace.active();
            editor.buffer().path().and_then(|path| {
                if !self.lsp.handles(&path) {
                    return None;
                }
                let cursor = editor.buffer().cursor();
                let text = editor.buffer().text();
                let span = identifier_start(&text, cursor)..identifier_end(&text, cursor);
                (span.start < span.end).then_some((path, span))
            })
        } else {
            None
        };

        if self.highlight_anchor == target {
            return; // same identifier (or still nothing) — leave the current tint as is
        }
        // The cursor moved to a different identifier (or off one): drop the old tint, then schedule
        // a (debounced) request for fresh occurrences when it is on a symbol.
        self.workspace.clear_active_occurrences();
        self.highlight_anchor.clone_from(&target);
        if let Some((path, span)) = target {
            self.debounce_lsp(PendingKind::DocumentHighlight, path, span.start);
        } else {
            // Left an identifier — drop any highlight request still waiting on the debounce.
            if matches!(
                self.pending_lsp.as_ref().map(|p| p.kind),
                Some(PendingKind::DocumentHighlight)
            ) {
                self.pending_lsp = None;
            }
        }
    }

    /// Schedules an auto-issued LSP request behind the debounce, replacing any pending one (so a
    /// burst of cursor moves or keystrokes coalesces to just the last position). Fired by
    /// [`Self::fire_due_lsp`] once [`LSP_DEBOUNCE`] elapses with no newer request.
    fn debounce_lsp(&mut self, kind: PendingKind, path: PathBuf, byte: usize) {
        self.pending_lsp = Some(PendingLsp {
            kind,
            path,
            byte,
            at: Instant::now() + LSP_DEBOUNCE,
        });
    }

    /// Issues a debounced LSP request whose deadline has passed (called each frame from `sync_lsp`).
    fn fire_due_lsp(&mut self) {
        let now = Instant::now();
        if self
            .pending_lsp
            .as_ref()
            .is_some_and(|pending| now >= pending.at)
        {
            if let Some(pending) = self.pending_lsp.take() {
                match pending.kind {
                    PendingKind::DocumentHighlight => self
                        .lsp
                        .request_document_highlight(&pending.path, pending.byte),
                    PendingKind::SignatureHelp => {
                        self.lsp.request_signature_help(&pending.path, pending.byte);
                    }
                }
            }
        }
        // The separate, edit-driven debounce for the whole-document refreshes (inlay hints + folds).
        if self
            .pending_inlay
            .as_ref()
            .is_some_and(|(_, at)| now >= *at)
        {
            if let Some((path, _)) = self.pending_inlay.take() {
                self.lsp.request_inlay_hints(&path);
                self.lsp.request_folding_ranges(&path);
            }
        }
    }

    /// How long until the next debounced LSP request (cursor-driven or inlay) is due, for capping the
    /// event-loop poll timeout so the loop wakes to fire it rather than waiting out a full idle tick.
    /// `None` when nothing is pending.
    fn lsp_debounce_timeout(&self) -> Option<Duration> {
        let now = Instant::now();
        let cursor = self.pending_lsp.as_ref().map(|pending| pending.at);
        let inlay = self.pending_inlay.as_ref().map(|(_, at)| *at);
        [cursor, inlay]
            .into_iter()
            .flatten()
            .min()
            .map(|at| at.saturating_duration_since(now))
    }

    /// Drains the interactive-request results (completion, hover, goto-definition, references,
    /// symbols, signature help) and opens/updates the matching cursor popup when the result is for
    /// the still-focused buffer. The cursor popups are mutually exclusive, so opening one closes the
    /// others. Split out of `sync_lsp` to keep each method within the line budget.
    fn apply_lsp_outcomes(&mut self) {
        // Open the matching popup when a result is for the buffer that still has focus.
        for outcome in self.lsp.poll_outcomes() {
            // Inlay hints apply to the buffer by path, regardless of focus (like diagnostics), so
            // they are not subject to the focused-pane gate below.
            if let LspOutcome::InlayHints { path, hints } = &outcome {
                self.workspace.apply_inlay_hints(path, hints);
                continue;
            }
            // A command's edits (workspace/applyEdit) carry their own paths — apply ungated too.
            if let LspOutcome::ApplyEdit { edits } = outcome {
                self.apply_workspace_edits(edits);
                continue;
            }
            // Foldable ranges apply to the buffer by path, regardless of focus (like inlay hints).
            if let LspOutcome::FoldingRanges { path, folds } = &outcome {
                self.workspace.apply_folds(path, folds);
                continue;
            }
            let active_path = self.workspace.active().buffer().path();
            let focused_match =
                self.focus == Focus::Editor && active_path.as_deref().is_some_and(|active| {
                    matches!(&outcome, LspOutcome::Completion { path, .. } | LspOutcome::Hover { path, .. } | LspOutcome::GotoDefinition { path, .. } | LspOutcome::References { path, .. } | LspOutcome::WorkspaceSymbols { path, .. } | LspOutcome::DocumentSymbols { path, .. } | LspOutcome::SignatureHelp { path, .. } | LspOutcome::Rename { path, .. } | LspOutcome::DocumentHighlight { path, .. } | LspOutcome::CodeActions { path, .. } | LspOutcome::Formatting { path, .. } if path == active)
                });
            if !focused_match {
                continue;
            }
            match outcome {
                LspOutcome::Completion { items, .. } => {
                    if !items.is_empty() {
                        self.close_cursor_popups();
                        self.completion = Some(Completion::new(items));
                    }
                }
                LspOutcome::Hover { text, .. } => {
                    if let Some(hover) =
                        text.map(|text| Hover::new(&text)).filter(|h| !h.is_empty())
                    {
                        self.close_cursor_popups();
                        self.hover = Some(hover);
                    }
                }
                LspOutcome::GotoDefinition { target, .. } => {
                    if let Some((target_path, position)) = target {
                        // Reveal the destination (reusing an open editor when possible), then land
                        // the cursor on the target position converted against that file's text.
                        if self.workspace.reveal_path(&target_path).is_ok() {
                            let text = self.workspace.active().buffer().text();
                            let byte = majestic_lsp::position_to_byte(
                                &text,
                                position.line,
                                position.character,
                            );
                            self.workspace.set_active_cursor(byte);
                            self.close_cursor_popups();
                        }
                    }
                }
                LspOutcome::References { references, .. } => {
                    let references = References::new(references);
                    if !references.is_empty() {
                        self.close_cursor_popups();
                        self.references = Some(references);
                    }
                }
                LspOutcome::WorkspaceSymbols { symbols, .. } => {
                    // Project-wide symbol matches reuse the references picker (cross-file jump).
                    let matches = References::new(symbols);
                    if !matches.is_empty() {
                        self.close_cursor_popups();
                        self.references = Some(matches);
                    }
                }
                LspOutcome::DocumentSymbols { symbols, .. } => {
                    let symbols = Symbols::new(symbols);
                    if !symbols.is_empty() {
                        self.close_cursor_popups();
                        self.symbols = Some(symbols);
                    }
                }
                LspOutcome::SignatureHelp { signature, .. } => {
                    // Passive popup: `Some` opens/updates it (closing the interactive popups), `None`
                    // (cursor left the call) closes it.
                    if signature.is_some() {
                        self.close_cursor_popups();
                    }
                    self.signature = signature;
                }
                LspOutcome::CodeActions { actions, .. } => {
                    let actions = CodeActions::new(actions);
                    if !actions.is_empty() {
                        self.close_cursor_popups();
                        self.code_actions = Some(actions);
                    }
                }
                LspOutcome::Rename { edits, .. } => {
                    self.apply_workspace_edits(edits);
                }
                LspOutcome::DocumentHighlight { occurrences, .. } => {
                    // Tint the occurrences — but only while the cursor is still on an identifier (a
                    // result arriving after it left one is dropped, so no stale tint lingers) and no
                    // search owns the buffer tint.
                    if self.highlight_anchor.is_some() && self.search.is_none() {
                        self.workspace.set_active_occurrences(occurrences);
                    }
                }
                LspOutcome::Formatting { path, formatted } => {
                    self.apply_formatting(&path, formatted);
                }
                // Applied (by path, ungated by focus) before the focus check above.
                LspOutcome::InlayHints { .. }
                | LspOutcome::ApplyEdit { .. }
                | LspOutcome::FoldingRanges { .. } => {}
            }
        }
    }

    /// `true` while a live shell exists (so the loop polls quickly for its output).
    fn terminal_running(&self) -> bool {
        self.terminal.as_ref().is_some_and(PtyTerminal::is_running)
    }

    /// Closes the panel and returns to the editor once the shell has exited.
    fn reap_dead_terminal(&mut self) {
        if self
            .terminal
            .as_ref()
            .is_some_and(|term| !term.is_running())
        {
            self.terminal = None;
            self.architect_terminal_visible = false;
            self.focus = Focus::Editor;
        }
    }

    fn should_quit(&self) -> bool {
        self.workspace.should_quit()
    }

    /// The editing workspace (so the daemon can snapshot it into a session on detach).
    pub(crate) fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    /// Draws the sidebar, the editor area, the bottom terminal panel (when present), and the
    /// status bar — the full UI.md layout.
    fn render(&mut self, surface: &mut Buffer, theme: &Theme) {
        self.sync_lsp(); // reconcile document sync + apply diagnostics before drawing
        surface.clear(theme.base_style());
        let (body, status) = surface.area().split_bottom(1);
        let main = self.render_sidebar(surface, body, theme);
        let main = self.render_agent_panel(surface, main, theme);

        // The region the editor was actually drawn into (none while the Info reader is showing),
        // used to anchor the completion popup at the cursor.
        let mut editor_rect: Option<Rect> = None;

        if let Some(info) = self.info.as_mut() {
            // The Info reader takes over the editor region (the sidebar + status bar remain).
            info.render(surface, main, theme);
        } else if self.architect_terminal_visible
            && self.terminal.is_some()
            && main.height > MIN_EDITOR_ROWS + 3
        {
            // The Architect terminal drops down across the top; the editor takes the rest.
            editor_rect = Some(self.render_architect_terminal(surface, main, theme));
        } else if self.terminal.is_some() && main.height > MIN_EDITOR_ROWS + 1 {
            let panel_rows = PANEL_ROWS.min(main.height - (MIN_EDITOR_ROWS + 1));
            let (editor_area, block) = main.split_bottom(panel_rows + 1);
            let (divider, panel_area) = block.split_top(1);

            // Keep the shell sized to its panel so its grid matches what we draw.
            if let Some(term) = self.terminal.as_mut() {
                if term.columns() != usize::from(panel_area.width)
                    || term.screen_lines() != usize::from(panel_area.height)
                {
                    term.resize(
                        usize::from(panel_area.width),
                        usize::from(panel_area.height),
                    );
                }
            }

            self.workspace
                .render(surface, editor_area, theme, self.focus == Focus::Editor);
            editor_rect = Some(editor_area);
            draw_panel_tab(surface, divider, theme, self.focus == Focus::Terminal);
            if let Some(term) = self.terminal.as_ref() {
                term.render_in(surface, panel_area, theme, self.focus == Focus::Terminal);
            }
        } else {
            self.workspace
                .render(surface, main, theme, self.focus == Focus::Editor);
            editor_rect = Some(main);
        }

        self.draw_status_bar(surface, status.y, theme);

        // which-key hint: while a prefix is in progress in the editor (Spacemacs SPC, Emacs C-x)
        // and no modal is open, list the keys that may come next over the editor area.
        if self.focus == Focus::Editor
            && self.finder.is_none()
            && self.help.is_none()
            && self.info.is_none()
            && self.selector.is_none()
        {
            if let Some(which_key) = self.workspace.which_key() {
                which_key.render(surface, main, theme);
            }
        }

        // Modal overlays are drawn last, over everything else.
        let area = surface.area();
        if let Some(finder) = self.finder.as_ref() {
            finder.render(surface, area, theme);
        }
        if let Some(help) = self.help.as_ref() {
            help.render(surface, area, theme);
        }
        if let Some(selector) = self.selector.as_ref() {
            selector.render(surface, area, theme);
        }
        if let Some(prompt) = self.prompt.as_ref() {
            prompt.render(surface, area, theme);
        }

        // The completion, hover, references, symbols, and signature popups are anchored at the cursor
        // within the editor region, above all else (they are mutually exclusive, so at most one shows).
        if let (Some(completion), Some(rect)) = (self.completion.as_ref(), editor_rect) {
            if let Some(cursor) = self.workspace.active_cursor_screen(rect) {
                completion.render(surface, rect, cursor, theme);
            }
        }
        if let (Some(hover), Some(rect)) = (self.hover.as_ref(), editor_rect) {
            if let Some(cursor) = self.workspace.active_cursor_screen(rect) {
                hover.render(surface, rect, cursor, theme);
            }
        }
        if let (Some(references), Some(rect)) = (self.references.as_ref(), editor_rect) {
            if let Some(cursor) = self.workspace.active_cursor_screen(rect) {
                references.render(surface, rect, cursor, theme);
            }
        }
        if let (Some(symbols), Some(rect)) = (self.symbols.as_ref(), editor_rect) {
            if let Some(cursor) = self.workspace.active_cursor_screen(rect) {
                symbols.render(surface, rect, cursor, theme);
            }
        }
        if let (Some(signature), Some(rect)) = (self.signature.as_ref(), editor_rect) {
            if let Some(cursor) = self.workspace.active_cursor_screen(rect) {
                signature.render(surface, rect, cursor, theme);
            }
        }
        if let (Some(menu), Some(rect)) = (self.code_actions.as_ref(), editor_rect) {
            if let Some(cursor) = self.workspace.active_cursor_screen(rect) {
                menu.render(surface, rect, cursor, theme);
            }
        }
    }

    /// Draws the explorer sidebar on the left (when shown and wide enough), returning the
    /// remaining region for the editor/terminal stack.
    fn render_sidebar(&mut self, surface: &mut Buffer, body: Rect, theme: &Theme) -> Rect {
        if !self.sidebar_visible {
            return body;
        }
        let focused = self.focus == Focus::Explorer;
        let Some(explorer) = self.explorer.as_mut() else {
            return body;
        };
        let sidebar_cols = SIDEBAR_COLS.min(body.width.saturating_sub(MIN_MAIN_COLS + 1));
        if sidebar_cols == 0 {
            return body; // too narrow to show the sidebar; keep the full main area
        }
        let (sidebar, rest) = body.split_left(sidebar_cols);
        let (divider, main) = rest.split_left(1);
        explorer.render(surface, sidebar, theme, focused);
        let rule = Style::new(theme.accent, theme.background); // Steel Blue
        for y in divider.y..divider.bottom() {
            surface.set_char(divider.x, y, '│', rule);
        }
        main
    }

    /// Draws the Architect agent sidebar on the right of `body` (when shown) and returns the region to
    /// its left for the editor + terminal. Mirrors [`Self::render_sidebar`] on the opposite edge.
    fn render_agent_panel(&self, surface: &mut Buffer, body: Rect, theme: &Theme) -> Rect {
        if !self.agent.is_visible() {
            return body;
        }
        let agent_cols = AGENT_COLS.min(body.width.saturating_sub(MIN_MAIN_COLS + 1));
        if agent_cols == 0 {
            return body; // too narrow to show the panel; keep the full editor area
        }
        let (rest, block) = body.split_left(body.width - agent_cols - 1);
        let (divider, panel) = block.split_left(1);
        let rule = Style::new(theme.accent, theme.background); // Steel Blue
        for y in divider.y..divider.bottom() {
            surface.set_char(divider.x, y, '│', rule);
        }
        self.agent
            .render(surface, panel, theme, self.focus == Focus::Architect);
        rest
    }

    /// Draws the Architect drop-down terminal across the top of `main` — a header row, the shell grid, and
    /// (in NL mode) a natural-language input row — with the editor below it. Returns the editor region.
    fn render_architect_terminal(&mut self, surface: &mut Buffer, main: Rect, theme: &Theme) -> Rect {
        let pane_rows = ARCHITECT_TERMINAL_ROWS.min(main.height - (MIN_EDITOR_ROWS + 1));
        let (block, editor_area) = main.split_top(pane_rows);
        let focused = self.focus == Focus::ArchitectTerminal;
        let (header, body) = block.split_top(1);
        // In NL mode the last row of the pane is the natural-language input line.
        let (term_area, input_area) = if self.architect_terminal_nl {
            let (term_area, input_area) = body.split_bottom(1);
            (term_area, Some(input_area))
        } else {
            (body, None)
        };

        // Keep the shell sized to its grid so what we draw matches the PTY.
        if let Some(term) = self.terminal.as_mut() {
            if term.columns() != usize::from(term_area.width)
                || term.screen_lines() != usize::from(term_area.height)
            {
                term.resize(usize::from(term_area.width), usize::from(term_area.height));
            }
        }

        draw_architect_terminal_header(
            surface,
            header,
            theme,
            focused,
            self.architect_terminal_nl,
            cfg!(feature = "agent"),
        );
        if let Some(term) = self.terminal.as_ref() {
            // The shell shows its cursor only when the pane is focused *and* in shell mode.
            term.render_in(surface, term_area, theme, focused && !self.architect_terminal_nl);
        }
        if let Some(input) = input_area {
            draw_architect_terminal_input(surface, input, theme, &self.architect_terminal_input, focused);
        }

        self.workspace
            .render(surface, editor_area, theme, self.focus == Focus::Editor);
        editor_area
    }

    /// Draws the global status bar: the editor's status line plus a focus/terminal hint.
    fn draw_status_bar(&self, surface: &mut Buffer, row: u16, theme: &Theme) {
        let style = Style::new(theme.background, theme.accent);
        for x in 0..surface.width() {
            surface.set_char(x, row, ' ', style);
        }
        // While searching, the search line replaces the normal status line on the left.
        let left = self
            .search_status_line()
            .unwrap_or_else(|| self.workspace.status_line());
        surface.set_str(0, row, &left, style);

        let hint = if self.focus == Focus::ArchitectTerminal {
            if cfg!(feature = "agent") {
                "[F1 help · Ctrl+` ⇄ EDITOR · Tab shell/NL · F12 terminal]"
            } else {
                "[F1 help · Ctrl+` ⇄ EDITOR · F12 terminal]"
            }
        } else if self.terminal.is_some() {
            if self.focus == Focus::Terminal {
                "[F1 help · F12 ⇄ EDITOR · Ctrl+` architect · Ctrl+B files]"
            } else {
                "[F1 help · F12 ⇄ TERMINAL · Ctrl+` architect · Ctrl+B files]"
            }
        } else {
            "[F1 help · F12 terminal · Ctrl+` architect · Ctrl+B files]"
        };
        // Right-aligned cluster: the active buffer's LSP server health (when one is configured),
        // then the key hint.
        let right = match self.active_lsp_status() {
            Some(lsp) => format!("{lsp}  {hint}"),
            None => hint.to_owned(),
        };
        if let Ok(len) = u16::try_from(right.chars().count()) {
            if len < surface.width() {
                surface.set_str(surface.width() - len - 1, row, &right, style);
            }
        }
    }

    /// The LSP server status for the active buffer (`"<server> ✓/…/✗"` for ready / starting /
    /// failed), or `None` when no server is configured for it — shown in the status bar.
    fn active_lsp_status(&self) -> Option<String> {
        let path = self.workspace.active().buffer().path()?;
        let (name, health) = self.lsp.server_health(&path)?;
        let glyph = match health {
            ServerHealth::Ready => '✓',
            ServerHealth::Starting => '…',
            ServerHealth::Failed => '✗',
        };
        Some(format!("{name} {glyph}"))
    }

    /// The search line shown in the status bar while a search is open: `search: <query> [i/N]` (or
    /// `(no matches)`), or `None` when no search is active.
    fn search_status_line(&self) -> Option<String> {
        let search = self.search.as_ref()?;
        let query = search.query();
        let line = if query.is_empty() {
            "search: ".to_owned()
        } else {
            match search.match_count() {
                0 => format!("search: {query}  (no matches)"),
                n => format!("search: {query}  [{}/{n}]", search.active_index()),
            }
        };
        Some(line)
    }

    /// Dispatches the editor feature keys — completion (`Ctrl+Space`), hover (`F2`), goto-definition
    /// (`Ctrl+F12`), find-references (`Shift+F12`), document symbols (`Ctrl+Shift+O`), rename
    /// (`Shift+F6`), code actions (`Ctrl+.`), format (`Shift+Alt+F`), and diagnostics (`F8`/`Shift+F8`).
    /// Returns `true` when `key` triggered one (the caller then returns). Each trigger is a no-op
    /// unless the editor is focused (the LSP ones also need a server handling the buffer).
    fn try_lsp_trigger(&mut self, key: KeyPress) -> bool {
        if key == COMPLETION_KEY {
            self.trigger_completion();
        } else if key == HOVER_KEY {
            self.trigger_hover();
        } else if key == GOTO_DEF_KEY {
            self.trigger_goto_definition();
        } else if is_references_key(key) {
            self.trigger_references();
        } else if is_document_symbols_key(key) {
            self.trigger_document_symbols();
        } else if is_rename_key(key) {
            self.trigger_rename();
        } else if is_code_action_key(key) {
            self.trigger_code_actions();
        } else if is_format_key(key) {
            self.trigger_formatting();
        } else if is_prev_diagnostic_key(key) {
            self.goto_diagnostic(Direction::Prev);
        } else if key == NEXT_DIAGNOSTIC_KEY {
            self.goto_diagnostic(Direction::Next);
        } else {
            return false;
        }
        true
    }

    /// Opens the command palette: Oracle's editor commands plus the host-level commands the App
    /// dispatches itself (config reload, the goto/replace/diagnostic/symbol actions).
    fn open_command_palette(&mut self) {
        let mut commands = oracle::command_names();
        commands.extend([
            "reload-config",
            "goto-type-definition",
            "goto-implementation",
            "next-diagnostic",
            "prev-diagnostic",
            "replace",
            "goto-line",
            "workspace-symbols",
            "toggle-fold",
        ]);
        self.finder = Some(Finder::commands(&commands));
    }

    fn handle_key(&mut self, key: KeyPress, columns: u16, lines: u16) -> io::Result<()> {
        // The first-run selector is modal and outranks everything: it captures every key until
        // the user has chosen a keybinding profile.
        if self.selector.is_some() {
            self.selector_key(key);
            return Ok(());
        }
        // The help overlay and the fuzzy finder are modal: while open they capture every key.
        if self.help.is_some() {
            self.help_key(key);
            return Ok(());
        }
        if self.finder.is_some() {
            self.finder_key(key);
            return Ok(());
        }
        if self.info.is_some() {
            self.info_key(key);
            return Ok(());
        }
        // The rename prompt is modal: while open it captures every key (the user is typing a name).
        if self.prompt.is_some() {
            self.prompt_key(key);
            return Ok(());
        }
        // The completion and hover popups are light modals: each captures its navigation/accept
        // keys while open, and any other key dismisses it and falls through to normal editing.
        if self.completion.is_some() && self.completion_popup_key(key) {
            return Ok(());
        }
        if self.hover.is_some() && self.hover_popup_key(key) {
            return Ok(());
        }
        if self.references.is_some() && self.references_popup_key(key) {
            return Ok(());
        }
        if self.symbols.is_some() && self.symbols_popup_key(key) {
            return Ok(());
        }
        if self.signature.is_some() && self.signature_popup_key(key) {
            return Ok(());
        }
        if self.code_actions.is_some() && self.code_actions_popup_key(key) {
            return Ok(());
        }
        // The incremental search line captures keys while open (printable chars extend the query);
        // a non-search key exits it and falls through.
        if self.search.is_some() && self.search_key(key) {
            return Ok(());
        }
        if key == HELP_KEY {
            self.help = Some(HelpOverlay::new(
                "Key Bindings (Esc to close)",
                &oracle::describe_bindings(&keymaker::cua()),
            ));
            return Ok(());
        }
        if is_command_palette(key) {
            self.open_command_palette();
            return Ok(());
        }
        if key == FILE_FINDER {
            let root = self.project_root();
            self.finder = Some(Finder::files(&root));
            return Ok(());
        }
        if self.try_lsp_trigger(key) {
            return Ok(());
        }
        if key.code == TERMINAL_TOGGLE {
            self.toggle_terminal(columns, lines);
            return Ok(());
        }
        if key == ARCHITECT_TERMINAL_TOGGLE {
            self.toggle_architect_terminal(columns, lines);
            return Ok(());
        }
        if key == SIDEBAR_TOGGLE {
            self.toggle_sidebar();
            return Ok(());
        }
        if self.try_agent_key(key) {
            return Ok(());
        }
        match self.focus {
            Focus::Architect => self.agent_key(key),
            Focus::Terminal => {
                if let Some(term) = self.terminal.as_mut() {
                    if let Some(bytes) = encode_key(key) {
                        term.write_input(&bytes)?;
                    }
                }
            }
            Focus::ArchitectTerminal => self.architect_terminal_key(key)?,
            Focus::Explorer => self.explorer_key(key),
            Focus::Editor => {
                self.workspace.handle_key(key);
                // The `find` command (Ctrl+F / Ctrl+S / `<leader> f f`) opens the search line.
                if self.workspace.take_search_request() {
                    self.open_search();
                }
                // Auto-trigger signature help after a typed `(` or `,` (refresh on each argument),
                // and after `)` (which makes the server report no call, closing the popup). The
                // request runs off-thread and is a no-op when no server handles the buffer.
                if let KeyCode::Char('(' | ',' | ')') = key.code {
                    if !key.mods.contains(Mods::CTRL) && !key.mods.contains(Mods::ALT) {
                        self.trigger_signature_help();
                    }
                }
            }
        }
        Ok(())
    }

    /// Routes a key to the open completion popup. Returns `true` when the key was consumed (the
    /// caller returns), `false` when the key dismissed the popup and should fall through to editing.
    fn completion_popup_key(&mut self, key: KeyPress) -> bool {
        match key.code {
            KeyCode::Up => {
                if let Some(completion) = self.completion.as_mut() {
                    completion.select_up();
                }
                true
            }
            KeyCode::Down => {
                if let Some(completion) = self.completion.as_mut() {
                    completion.select_down();
                }
                true
            }
            KeyCode::Enter | KeyCode::Tab => {
                self.accept_completion();
                true
            }
            KeyCode::Escape => {
                self.completion = None;
                true
            }
            _ => {
                self.completion = None;
                false
            }
        }
    }

    /// Routes a key to the open hover popup: ↑/↓ scroll it, `Esc` closes it, and any other key
    /// dismisses it and falls through (so pressing `F2` again re-requests hover at the new cursor).
    /// Returns `true` when the key was consumed, `false` when it should fall through to editing.
    fn hover_popup_key(&mut self, key: KeyPress) -> bool {
        match key.code {
            KeyCode::Up => {
                if let Some(hover) = self.hover.as_mut() {
                    hover.scroll_up();
                }
                true
            }
            KeyCode::Down => {
                if let Some(hover) = self.hover.as_mut() {
                    hover.scroll_down();
                }
                true
            }
            KeyCode::Escape => {
                self.hover = None;
                true
            }
            _ => {
                self.hover = None;
                false
            }
        }
    }

    /// Routes a key to the open references popup: ↑/↓ move the selection, `Enter` jumps to the
    /// selected use site, `Esc` closes it, and any other key dismisses it and falls through to
    /// editing. Returns `true` when the key was consumed, `false` when it should fall through.
    fn references_popup_key(&mut self, key: KeyPress) -> bool {
        match key.code {
            KeyCode::Up => {
                if let Some(references) = self.references.as_mut() {
                    references.select_up();
                }
                true
            }
            KeyCode::Down => {
                if let Some(references) = self.references.as_mut() {
                    references.select_down();
                }
                true
            }
            KeyCode::Enter => {
                self.jump_to_selected_reference();
                true
            }
            KeyCode::Escape => {
                self.references = None;
                true
            }
            _ => {
                self.references = None;
                false
            }
        }
    }

    /// Jumps to the selected reference and closes the popup: reveals its file (reusing an open editor
    /// when possible) and lands the cursor on the use site, converting its LSP position against that
    /// file's current text — the same jump path as goto-definition.
    fn jump_to_selected_reference(&mut self) {
        let Some(references) = self.references.take() else {
            return;
        };
        let Some(reference) = references.selected() else {
            return;
        };
        let (path, line, character) = (reference.path.clone(), reference.line, reference.character);
        if self.workspace.reveal_path(&path).is_ok() {
            let text = self.workspace.active().buffer().text();
            let byte = majestic_lsp::position_to_byte(&text, line, character);
            self.workspace.set_active_cursor(byte);
        }
    }

    /// Routes a key to the open symbols picker: ↑/↓ move the selection, `Enter` jumps to the selected
    /// definition, `Esc` closes it, and any other key dismisses it and falls through to editing.
    /// Returns `true` when the key was consumed, `false` when it should fall through.
    fn symbols_popup_key(&mut self, key: KeyPress) -> bool {
        match key.code {
            KeyCode::Up => {
                if let Some(symbols) = self.symbols.as_mut() {
                    symbols.select_up();
                }
                true
            }
            KeyCode::Down => {
                if let Some(symbols) = self.symbols.as_mut() {
                    symbols.select_down();
                }
                true
            }
            KeyCode::Enter => {
                self.jump_to_selected_symbol();
                true
            }
            KeyCode::Escape => {
                self.symbols = None;
                true
            }
            _ => {
                self.symbols = None;
                false
            }
        }
    }

    /// Jumps to the selected symbol and closes the picker. Document symbols are always in the file
    /// the request was issued for — still the focused buffer — so the cursor is landed there directly
    /// (the symbol's name position, converted against the buffer's current text).
    fn jump_to_selected_symbol(&mut self) {
        let Some(symbols) = self.symbols.take() else {
            return;
        };
        let Some(symbol) = symbols.selected() else {
            return;
        };
        let (line, character) = (symbol.line, symbol.character);
        let text = self.workspace.active().buffer().text();
        let byte = majestic_lsp::position_to_byte(&text, line, character);
        self.workspace.set_active_cursor(byte);
    }

    /// Routes a key to the open (passive) signature popup. It captures only `Esc` (to dismiss it);
    /// every other key falls through, so the user keeps typing arguments under the popup — which
    /// refreshes on the next `,` and closes on `)`. Returns `true` only when `Esc` was consumed.
    fn signature_popup_key(&mut self, key: KeyPress) -> bool {
        if key.code == KeyCode::Escape {
            self.signature = None;
            true
        } else {
            false
        }
    }

    /// Closes every cursor-anchored LSP popup (completion, hover, references, symbols, signature, code
    /// actions). They are mutually exclusive, so an opener calls this before showing its own.
    fn close_cursor_popups(&mut self) {
        self.completion = None;
        self.hover = None;
        self.references = None;
        self.symbols = None;
        self.signature = None;
        self.code_actions = None;
    }

    /// Routes a key to the open code-actions menu: ↑/↓ move the selection, `Enter` applies the
    /// selected action, `Esc` closes it, and any other key dismisses it and falls through to editing.
    /// Returns `true` when the key was consumed, `false` when it should fall through.
    fn code_actions_popup_key(&mut self, key: KeyPress) -> bool {
        match key.code {
            KeyCode::Up => {
                if let Some(menu) = self.code_actions.as_mut() {
                    menu.select_up();
                }
                true
            }
            KeyCode::Down => {
                if let Some(menu) = self.code_actions.as_mut() {
                    menu.select_down();
                }
                true
            }
            KeyCode::Enter => {
                self.apply_selected_code_action();
                true
            }
            KeyCode::Escape => {
                self.code_actions = None;
                true
            }
            _ => {
                self.code_actions = None;
                false
            }
        }
    }

    /// Applies the selected code action and closes the menu. An action with edits applies them
    /// directly (the shared `WorkspaceEdit` applier); an edit-less action runs its command, whose
    /// edits come back as a `workspace/applyEdit` request → an `ApplyEdit` outcome.
    fn apply_selected_code_action(&mut self) {
        let Some(menu) = self.code_actions.take() else {
            return;
        };
        let (edits, command) = match menu.selected() {
            Some(action) => (action.edits.clone(), action.command.clone()),
            None => return,
        };
        if !edits.is_empty() {
            self.apply_workspace_edits(edits);
        } else if let Some(command) = command {
            if let Some(path) = self.workspace.active().buffer().path() {
                self.lsp
                    .request_execute_command(&path, command.id, command.arguments);
            }
        }
    }

    /// Schedules an LSP signature-help request at the cursor behind the debounce; it fires (off the
    /// editor thread) once typing settles, and the popup opens/updates (or closes) in `sync_lsp` once
    /// the reply arrives. Auto-invoked after typing `(`/`,`/`)`. A no-op unless the editor is focused
    /// and a server handles the buffer.
    fn trigger_signature_help(&mut self) {
        if self.focus != Focus::Editor {
            return;
        }
        let editor = self.workspace.active();
        let Some(path) = editor.buffer().path() else {
            return;
        };
        if !self.lsp.handles(&path) {
            return;
        }
        let cursor = editor.buffer().cursor();
        self.debounce_lsp(PendingKind::SignatureHelp, path, cursor);
    }

    /// Requests LSP completion at the cursor: records where the in-progress identifier starts (so an
    /// accepted candidate replaces it, not the whole word), then asks the manager to fetch
    /// candidates off-thread. A no-op unless the editor is focused and a server handles the buffer;
    /// the popup opens later, in `sync_lsp`, once the result arrives.
    fn trigger_completion(&mut self) {
        if self.focus != Focus::Editor {
            return;
        }
        let editor = self.workspace.active();
        let Some(path) = editor.buffer().path() else {
            return;
        };
        if !self.lsp.handles(&path) {
            return;
        }
        let cursor = editor.buffer().cursor();
        let text = editor.buffer().text();
        self.completion_anchor = identifier_start(&text, cursor);
        self.lsp.request_completion(&path, cursor);
    }

    /// Requests LSP hover documentation at the cursor, off-thread; the popup opens later, in
    /// `sync_lsp`, once the reply arrives. A no-op unless the editor is focused and a server handles
    /// the buffer.
    fn trigger_hover(&mut self) {
        if self.focus != Focus::Editor {
            return;
        }
        let editor = self.workspace.active();
        let Some(path) = editor.buffer().path() else {
            return;
        };
        if !self.lsp.handles(&path) {
            return;
        }
        let cursor = editor.buffer().cursor();
        self.lsp.request_hover(&path, cursor);
    }

    /// Requests LSP goto-definition at the cursor, off-thread; the jump happens later, in
    /// `sync_lsp`, once the reply arrives. A no-op unless the editor is focused and a server handles
    /// the buffer.
    fn trigger_goto_definition(&mut self) {
        if self.focus != Focus::Editor {
            return;
        }
        let editor = self.workspace.active();
        let Some(path) = editor.buffer().path() else {
            return;
        };
        if !self.lsp.handles(&path) {
            return;
        }
        let cursor = editor.buffer().cursor();
        self.lsp.request_goto_definition(&path, cursor);
    }

    /// Jumps the cursor to the next/previous diagnostic in the active buffer (wrapping around), so
    /// `F8`/`Shift+F8` step through problems; the diagnostic's message then shows in the status line.
    /// Uses the diagnostics already in the buffer — no server round-trip — and is a no-op when the
    /// editor is not focused or the buffer has no diagnostics. Also the `next-/prev-diagnostic` palette
    /// commands.
    fn goto_diagnostic(&mut self, direction: Direction) {
        if self.focus != Focus::Editor {
            return;
        }
        let editor = self.workspace.active();
        let cursor = editor.buffer().cursor();
        let target = match direction {
            Direction::Next => editor.next_diagnostic(cursor),
            Direction::Prev => editor.prev_diagnostic(cursor),
        };
        if let Some(byte) = target {
            self.workspace.set_active_cursor(byte);
        }
    }

    /// Opens the incremental in-buffer search, anchored at the current cursor (restored on cancel).
    /// Triggered by the `find` command (Ctrl+F / Ctrl+S / `<leader> f f`, per profile).
    fn open_search(&mut self) {
        let cursor = self.workspace.active().buffer().cursor();
        self.search = Some(Search::new(cursor));
        self.apply_search();
    }

    /// Routes a key to the open search line: printable chars extend the query (jumping to the first
    /// match as you type), `Backspace` trims it, `Down`/`Up` step to the next/previous match, `Enter`
    /// accepts (exits at the current match), `Esc` cancels (restoring the cursor to where the search
    /// began). Any other key accepts and falls through to normal editing. Returns `true` when the key
    /// was consumed.
    fn search_key(&mut self, key: KeyPress) -> bool {
        if self.search.is_none() {
            return false;
        }
        match key.code {
            KeyCode::Escape => self.close_search(true),
            KeyCode::Enter => self.close_search(false),
            KeyCode::Down => self.search_step(Direction::Next),
            KeyCode::Up => self.search_step(Direction::Prev),
            KeyCode::Backspace => self.search_edit(None),
            KeyCode::Char(c) if !key.mods.contains(Mods::CTRL) && !key.mods.contains(Mods::ALT) => {
                self.search_edit(Some(c));
            }
            _ => {
                // A non-search key accepts at the current spot and is then handled normally.
                self.close_search(false);
                return false;
            }
        }
        true
    }

    /// Extends (`Some(c)`) or trims (`None`) the query, recomputing matches from the search origin
    /// and re-applying the tint + cursor jump.
    fn search_edit(&mut self, ch: Option<char>) {
        let text = self.workspace.active().buffer().text();
        if let Some(search) = self.search.as_mut() {
            let from = search.origin();
            match ch {
                Some(c) => search.push(c, &text, from),
                None => search.backspace(&text, from),
            }
        }
        self.apply_search();
    }

    /// Moves to the next/previous match and re-applies the tint + cursor jump.
    fn search_step(&mut self, direction: Direction) {
        if let Some(search) = self.search.as_mut() {
            match direction {
                Direction::Next => search.next(),
                Direction::Prev => search.prev(),
            }
        }
        self.apply_search();
    }

    /// Tints every match and parks the cursor on the active one (after each query/selection change).
    fn apply_search(&mut self) {
        let Some((occurrences, target)) = self.search.as_ref().map(|search| {
            let occurrences: Vec<Occurrence> = search
                .matches()
                .iter()
                .cloned()
                .map(|range| Occurrence::new(range, false))
                .collect();
            (occurrences, search.active_match().map(|m| m.start))
        }) else {
            return;
        };
        self.workspace.set_active_occurrences(occurrences);
        if let Some(byte) = target {
            self.workspace.set_active_cursor(byte);
        }
    }

    /// Closes the search, clearing the match tint. When `cancel` is set (Esc), the cursor returns to
    /// where the search started; otherwise it stays on the current match.
    fn close_search(&mut self, cancel: bool) {
        if let Some(search) = self.search.take() {
            self.workspace.clear_active_occurrences();
            if cancel {
                self.workspace.set_active_cursor(search.origin());
            }
        }
    }

    /// Requests LSP goto-type-definition at the cursor (jump to the declaration of the symbol's
    /// type), off-thread; the jump reuses the goto-definition path in `sync_lsp`. Invoked by the
    /// `goto-type-definition` palette command. A no-op unless the editor is focused and a server
    /// handles the buffer.
    fn trigger_type_definition(&mut self) {
        if self.focus != Focus::Editor {
            return;
        }
        let editor = self.workspace.active();
        let Some(path) = editor.buffer().path() else {
            return;
        };
        if !self.lsp.handles(&path) {
            return;
        }
        let cursor = editor.buffer().cursor();
        self.lsp.request_type_definition(&path, cursor);
    }

    /// Requests LSP goto-implementation at the cursor (jump to a trait method's `impl`, etc.),
    /// off-thread; the jump reuses the goto-definition path in `sync_lsp`. Invoked by the
    /// `goto-implementation` palette command. A no-op unless the editor is focused and a server
    /// handles the buffer.
    fn trigger_implementation(&mut self) {
        if self.focus != Focus::Editor {
            return;
        }
        let editor = self.workspace.active();
        let Some(path) = editor.buffer().path() else {
            return;
        };
        if !self.lsp.handles(&path) {
            return;
        }
        let cursor = editor.buffer().cursor();
        self.lsp.request_implementation(&path, cursor);
    }

    /// Requests LSP find-references at the cursor, off-thread; the popup opens later, in `sync_lsp`,
    /// once the reply arrives. A no-op unless the editor is focused and a server handles the buffer.
    fn trigger_references(&mut self) {
        if self.focus != Focus::Editor {
            return;
        }
        let editor = self.workspace.active();
        let Some(path) = editor.buffer().path() else {
            return;
        };
        if !self.lsp.handles(&path) {
            return;
        }
        let cursor = editor.buffer().cursor();
        self.lsp.request_references(&path, cursor);
    }

    /// Requests the LSP document-symbol outline for the focused buffer, off-thread; the picker opens
    /// later, in `sync_lsp`, once the reply arrives. Whole-document, so unlike the cursor-based
    /// requests it sends no cursor. A no-op unless the editor is focused and a server handles the
    /// buffer.
    fn trigger_document_symbols(&mut self) {
        if self.focus != Focus::Editor {
            return;
        }
        let editor = self.workspace.active();
        let Some(path) = editor.buffer().path() else {
            return;
        };
        if !self.lsp.handles(&path) {
            return;
        }
        self.lsp.request_document_symbols(&path);
    }

    /// Requests the LSP code actions at the cursor, off-thread; the menu opens later, in `sync_lsp`,
    /// once the reply arrives. A no-op unless the editor is focused and a server handles the buffer.
    fn trigger_code_actions(&mut self) {
        if self.focus != Focus::Editor {
            return;
        }
        let editor = self.workspace.active();
        let Some(path) = editor.buffer().path() else {
            return;
        };
        if !self.lsp.handles(&path) {
            return;
        }
        let cursor = editor.buffer().cursor();
        self.lsp.request_code_action(&path, cursor);
    }

    /// Starts an LSP rename: opens the modal prompt pre-filled with the identifier under the cursor
    /// and records where to rename. The request is issued later, on confirm. A no-op unless the
    /// editor is focused and a server handles the buffer.
    fn trigger_rename(&mut self) {
        if self.focus != Focus::Editor {
            return;
        }
        let editor = self.workspace.active();
        let Some(path) = editor.buffer().path() else {
            return;
        };
        if !self.lsp.handles(&path) {
            return;
        }
        let cursor = editor.buffer().cursor();
        let text = editor.buffer().text();
        let name = text
            .get(identifier_start(&text, cursor)..identifier_end(&text, cursor))
            .unwrap_or_default();
        self.prompt = Some(Prompt::new("Rename symbol to", name));
        self.prompt_action = Some(PromptAction::Rename { path, byte: cursor });
    }

    /// Routes a key to the open rename prompt: a character extends the name, `Backspace` erases,
    /// `Enter` confirms (issuing the request), and `Esc` cancels.
    fn prompt_key(&mut self, key: KeyPress) {
        match key.code {
            KeyCode::Char(c) if !key.mods.contains(Mods::CTRL) && !key.mods.contains(Mods::ALT) => {
                if let Some(prompt) = self.prompt.as_mut() {
                    prompt.push(c);
                }
            }
            KeyCode::Backspace => {
                if let Some(prompt) = self.prompt.as_mut() {
                    prompt.backspace();
                }
            }
            KeyCode::Enter => self.confirm_prompt(),
            KeyCode::Escape => {
                self.prompt = None;
                self.prompt_action = None;
            }
            _ => {}
        }
    }

    /// Confirms the modal prompt, dispatching on what it was collecting and closing it (a blank entry
    /// cancels). Rename issues `textDocument/rename`; search-and-replace's first stage records the
    /// search term and opens the second stage, whose entry replaces every occurrence.
    fn confirm_prompt(&mut self) {
        // `take` clears both fields; missing either (shouldn't happen) just closes the prompt.
        let (Some(prompt), Some(action)) = (self.prompt.take(), self.prompt_action.take()) else {
            return;
        };
        let input = prompt.input().to_owned();
        match action {
            PromptAction::Rename { path, byte } => {
                let new_name = input.trim().to_owned();
                if !new_name.is_empty() {
                    self.lsp.request_rename(&path, byte, new_name);
                }
            }
            PromptAction::ReplaceSearch => {
                // Stage 1 → stage 2: a non-empty search term opens the "Replace with" prompt.
                if !input.is_empty() {
                    self.prompt = Some(Prompt::new("Replace with", ""));
                    self.prompt_action = Some(PromptAction::ReplaceWith { search: input });
                }
            }
            PromptAction::ReplaceWith { search } => self.replace_all(&search, &input),
            PromptAction::GotoLine => self.goto_line(&input),
            PromptAction::WorkspaceSymbol => {
                if let Some(path) = self.workspace.active().buffer().path() {
                    self.lsp.request_workspace_symbols(&path, input);
                }
            }
            #[cfg(feature = "agent")]
            PromptAction::AskAgent => self.ask_agent(&input),
            #[cfg(feature = "agent")]
            PromptAction::EditApproval => self.agent_host.answer_modified(&mut self.agent, &input),
        }
    }

    /// Replaces every (case-sensitive) occurrence of `search` in the active buffer with `replacement`,
    /// back-to-front so earlier byte offsets stay valid, and reports the count. A no-op on an empty
    /// search term.
    fn replace_all(&mut self, search: &str, replacement: &str) {
        if search.is_empty() {
            return;
        }
        let text = self.workspace.active().buffer().text();
        let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
        let mut start = 0;
        while let Some(offset) = text[start..].find(search) {
            let at = start + offset;
            let end = at + search.len();
            ranges.push(at..end);
            start = end;
        }
        let count = ranges.len();
        for range in ranges.into_iter().rev() {
            self.workspace.replace_active(range, replacement);
        }
        let plural = if count == 1 { "" } else { "s" };
        self.workspace
            .set_status(format!("Replaced {count} occurrence{plural}"));
    }

    /// Starts search-and-replace: opens the "Search for" prompt (its entry then opens "Replace with").
    /// Invoked by the `replace` palette command. A no-op unless the editor is focused.
    fn trigger_replace(&mut self) {
        if self.focus != Focus::Editor {
            return;
        }
        self.prompt = Some(Prompt::new("Search for", ""));
        self.prompt_action = Some(PromptAction::ReplaceSearch);
    }

    /// Starts go-to-line: opens the "Go to line" prompt. Invoked by the `goto-line` palette command.
    /// A no-op unless the editor is focused.
    fn trigger_goto_line(&mut self) {
        if self.focus != Focus::Editor {
            return;
        }
        self.prompt = Some(Prompt::new("Go to line", ""));
        self.prompt_action = Some(PromptAction::GotoLine);
    }

    /// Starts go-to-symbol-in-project: opens the "Go to symbol" prompt, whose entry queries
    /// `workspace/symbol` and shows the matches in the picker. Invoked by the `workspace-symbols`
    /// palette command. A no-op unless the editor is focused and a server handles the active buffer.
    fn trigger_workspace_symbols(&mut self) {
        if self.focus != Focus::Editor {
            return;
        }
        let Some(path) = self.workspace.active().buffer().path() else {
            return;
        };
        if !self.lsp.handles(&path) {
            return;
        }
        self.prompt = Some(Prompt::new("Go to symbol", ""));
        self.prompt_action = Some(PromptAction::WorkspaceSymbol);
    }

    /// Moves the cursor to the start of (1-based) line `input`, clamped to the buffer's last line. A
    /// non-numeric entry reports an error and does nothing.
    fn goto_line(&mut self, input: &str) {
        let Ok(line) = input.trim().parse::<usize>() else {
            self.workspace.set_status(format!(
                "Go to line: `{}` is not a line number",
                input.trim()
            ));
            return;
        };
        if line == 0 {
            return;
        }
        let text = self.workspace.active().buffer().text();
        let line_count = text.lines().count().max(1);
        let zero_based = u32::try_from(line.min(line_count) - 1).unwrap_or(u32::MAX);
        let byte = majestic_lsp::position_to_byte(&text, zero_based, 0);
        self.workspace.set_active_cursor(byte);
    }

    /// Applies a server-provided `WorkspaceEdit` (already reduced to positional `RenameEdit`s): groups
    /// the edits by file, and for each file reveals it (reusing an open editor when possible, so an
    /// unsaved buffer is edited in place) and applies its edits back-to-front (so earlier byte offsets
    /// stay valid). Focus returns to where it started. Shared by rename and code actions.
    fn apply_workspace_edits(&mut self, edits: Vec<RenameEdit>) {
        if edits.is_empty() {
            return;
        }
        let origin = self.workspace.active().buffer().path();
        let mut by_path: HashMap<PathBuf, Vec<RenameEdit>> = HashMap::new();
        for edit in edits {
            by_path.entry(edit.path.clone()).or_default().push(edit);
        }
        for (path, file_edits) in by_path {
            if self.workspace.reveal_path(&path).is_err() {
                continue;
            }
            let text = self.workspace.active().buffer().text();
            // Resolve every edit to a byte range against this file's text, then splice back-to-front.
            let mut spans: Vec<(usize, usize, String)> = file_edits
                .into_iter()
                .map(|edit| {
                    let start = majestic_lsp::position_to_byte(
                        &text,
                        edit.start_line,
                        edit.start_character,
                    );
                    let end =
                        majestic_lsp::position_to_byte(&text, edit.end_line, edit.end_character)
                            .max(start);
                    (start, end, edit.new_text)
                })
                .collect();
            spans.sort_by_key(|span| std::cmp::Reverse(span.0));
            for (start, end, new_text) in spans {
                self.workspace.replace_active(start..end, &new_text);
            }
        }
        // Return to the buffer the rename was triggered from.
        if let Some(path) = origin {
            let _ = self.workspace.reveal_path(&path);
        }
    }

    /// Requests a whole-document LSP reformat, off-thread; the result is applied later, in
    /// `sync_lsp`, once the reply arrives — but only if the buffer is still on the revision recorded
    /// here, so an edit made while formatting was in flight is never clobbered by stale output. A
    /// no-op unless the editor is focused and a server handles the buffer.
    fn trigger_formatting(&mut self) {
        if self.focus != Focus::Editor {
            return;
        }
        let editor = self.workspace.active();
        let Some(path) = editor.buffer().path() else {
            return;
        };
        if !self.lsp.handles(&path) {
            return;
        }
        self.format_request = Some((path.clone(), editor.buffer().revision()));
        self.lsp.request_formatting(&path);
    }

    /// Applies a completed reformat to the focused buffer — but only if `formatted` is `Some` and the
    /// buffer is still the one, and on the revision, that was formatted (otherwise the result is
    /// stale and dropped, never overwriting newer edits). The whole document is replaced as one
    /// undoable edit and the cursor is re-seated at its old byte offset (clamped to the new text),
    /// since formatting usually only adjusts surrounding whitespace.
    fn apply_formatting(&mut self, path: &Path, formatted: Option<String>) {
        let Some((requested_path, revision)) = self.format_request.take() else {
            return;
        };
        let Some(formatted) = formatted else {
            return;
        };
        let (current_path, current_revision, cursor, len) = {
            let buffer = self.workspace.active().buffer();
            (
                buffer.path(),
                buffer.revision(),
                buffer.cursor(),
                buffer.text().len(),
            )
        };
        if requested_path.as_path() != path
            || current_path.as_deref() != Some(path)
            || current_revision != revision
        {
            return;
        }
        self.workspace.replace_active(0..len, &formatted);
        self.workspace
            .set_active_cursor(cursor.min(formatted.len()));
        self.completion = None;
        self.hover = None;
    }

    /// Inserts the selected candidate over the typed identifier prefix and closes the popup.
    fn accept_completion(&mut self) {
        let Some(completion) = self.completion.take() else {
            return;
        };
        let Some(item) = completion.selected() else {
            return;
        };
        let cursor = self.workspace.active().buffer().cursor();
        let start = self.completion_anchor.min(cursor);
        self.workspace
            .replace_active(start..cursor, &item.insert_text);
    }

    /// Routes a key to the explorer: arrow navigation, `Enter` to open/expand, `Esc` to leave.
    fn explorer_key(&mut self, key: KeyPress) {
        if key.code == KeyCode::Escape {
            self.focus = Focus::Editor;
            return;
        }
        let opened = if let Some(explorer) = self.explorer.as_mut() {
            match key.code {
                KeyCode::Up => {
                    explorer.select_up();
                    None
                }
                KeyCode::Down => {
                    explorer.select_down();
                    None
                }
                KeyCode::Enter => explorer.activate(),
                _ => None,
            }
        } else {
            self.focus = Focus::Editor;
            None
        };
        if let Some(path) = opened {
            self.open_path(&path);
        }
    }

    /// Opens `path` as a new buffer in the workspace and moves focus to the editor.
    fn open_path(&mut self, path: &Path) {
        // GNU Info documents open in the built-in reader (M1 §5.7) rather than the text editor.
        if path
            .extension()
            .is_some_and(|extension| extension == "info")
        {
            if let Ok(reader) = InfoReader::open(path) {
                self.info = Some(reader);
                self.focus = Focus::Editor;
                return;
            }
        }
        if let Ok(buffer) = majestic_core::Buffer::open(path) {
            self.workspace.open(Editor::with_buffer(buffer));
            self.focus = Focus::Editor;
        }
        // A failed open keeps focus on the explorer; surfaced errors arrive with the minibuffer.
    }

    /// `Ctrl+B`: open+focus the sidebar, focus it if already shown, or hide it when focused.
    fn toggle_sidebar(&mut self) {
        if !self.sidebar_visible {
            if let Some(explorer) = self.explorer.as_mut() {
                // Re-opening: rescan the tree and git status so decorations are current.
                explorer.refresh();
            } else {
                let root =
                    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
                self.explorer = Some(FileTree::new(root));
            }
            self.sidebar_visible = true;
            self.focus = Focus::Explorer;
        } else if self.focus == Focus::Explorer {
            self.sidebar_visible = false;
            self.focus = Focus::Editor;
        } else {
            self.focus = Focus::Explorer;
        }
    }

    /// Dispatches the agent control keys: toggle the panel (`Ctrl+Shift+A`), stop the agent
    /// (`Ctrl+Shift+K`), or open the "Ask Architect" prompt (`Ctrl+Shift+N`). Returns `true` when
    /// `key` triggered one (the caller then returns). Kept out of `handle_key` for its line budget.
    fn try_agent_key(&mut self, key: KeyPress) -> bool {
        if is_agent_toggle(key) {
            self.toggle_agent();
            return true;
        }
        if is_agent_stop(key) {
            self.stop_agent();
            return true;
        }
        #[cfg(feature = "agent")]
        if is_agent_prompt(key) {
            self.open_agent_prompt();
            return true;
        }
        false
    }

    /// Toggles the Architect agent sidebar (`Ctrl+Shift+A`): shows and focuses it, hides it when it is
    /// already focused, or just focuses it when shown but unfocused — mirroring the explorer toggle.
    fn toggle_agent(&mut self) {
        if !self.agent.is_visible() {
            self.agent.toggle();
            self.focus = Focus::Architect;
        } else if self.focus == Focus::Architect {
            self.agent.toggle();
            self.focus = Focus::Editor;
        } else {
            self.focus = Focus::Architect;
        }
    }

    /// Opens the "Ask Architect" minibuffer (`Ctrl+Shift+N`): a one-line natural-language request
    /// that starts an agent turn from anywhere, without first focusing the sidebar.
    #[cfg(feature = "agent")]
    fn open_agent_prompt(&mut self) {
        self.prompt = Some(Prompt::new("Ask Architect", ""));
        self.prompt_action = Some(PromptAction::AskAgent);
    }

    /// Starts an agent turn from the Ask-Architect prompt: shows and focuses the panel and submits `message`,
    /// so the streaming reply (and any approval prompt) lands in the now-visible sidebar.
    #[cfg(feature = "agent")]
    fn ask_agent(&mut self, message: &str) {
        let message = message.trim().to_owned();
        if message.is_empty() {
            return;
        }
        if !self.agent.is_visible() {
            self.agent.toggle();
        }
        self.focus = Focus::Architect;
        self.agent.push_user(message.clone());
        self.agent_host.start_turn(&mut self.agent, &message);
    }

    /// Routes a key to the agent panel while it is focused. While an approval is pending, `y` approves
    /// and `n`/`Esc` rejects; otherwise `Esc` returns focus to the editor and a submitted message is
    /// sent to the agent.
    fn agent_key(&mut self, key: KeyPress) {
        #[cfg(feature = "agent")]
        if self.agent_host.has_pending_approval() {
            match key.code {
                KeyCode::Char('y' | 'Y') => self.agent_host.answer_approval(&mut self.agent, true),
                KeyCode::Char('n' | 'N') | KeyCode::Escape => {
                    self.agent_host.answer_approval(&mut self.agent, false);
                }
                KeyCode::Char('e' | 'E') => self.start_edit_approval(),
                _ => {}
            }
            return;
        }
        if key.code == KeyCode::Escape {
            self.focus = Focus::Editor;
            return;
        }
        if let Some(message) = self.agent.handle_key(key) {
            self.agent.push_user(message.clone());
            self.submit_agent_message(&message);
        }
    }

    /// Hands a submitted panel message to the agent (starts a governed turn).
    #[cfg(feature = "agent")]
    fn submit_agent_message(&mut self, message: &str) {
        self.agent_host.start_turn(&mut self.agent, message);
    }

    /// Opens the minibuffer to edit a pending edit's proposed text (the Edit option of the approval
    /// card). A no-op when the pending change is not a single, editable edit.
    #[cfg(feature = "agent")]
    fn start_edit_approval(&mut self) {
        if let Some(text) = self.agent_host.pending_edit_text() {
            self.prompt = Some(Prompt::new("Edit the change", text));
            self.prompt_action = Some(PromptAction::EditApproval);
        }
    }

    /// Without the `agent` feature there is no backend: note it in the panel.
    #[cfg(not(feature = "agent"))]
    fn submit_agent_message(&mut self, message: &str) {
        let _ = message;
        self.agent
            .push_system("this build has no agent backend (built without the `agent` feature)");
    }

    /// Engages the running agent's kill switch (`agent-stop-all`, `Ctrl+Shift+K`).
    #[cfg(feature = "agent")]
    fn stop_agent(&mut self) {
        self.agent_host.stop(&mut self.agent);
    }

    /// No agent to stop without the feature.
    #[cfg(not(feature = "agent"))]
    #[expect(
        clippy::unused_self,
        reason = "uniform no-op mirroring the agent-enabled method"
    )]
    fn stop_agent(&mut self) {}

    /// Whether an agent turn is in flight (so the frame loop polls more responsively while it runs).
    #[cfg(feature = "agent")]
    fn agent_running(&self) -> bool {
        self.agent_host.is_running()
    }

    /// No turn can run without the feature.
    #[cfg(not(feature = "agent"))]
    #[expect(
        clippy::unused_self,
        reason = "uniform no-op mirroring the agent-enabled method"
    )]
    fn agent_running(&self) -> bool {
        false
    }

    /// Services the agent worker against the active buffer each frame (no-op without the feature).
    #[cfg(feature = "agent")]
    fn poll_agent(&mut self) {
        let buffer = self.workspace.active_mut().buffer_mut();
        // Split borrow: `agent` (panel) and `agent_host` are distinct fields from `workspace`.
        self.agent_host.poll(&mut self.agent, buffer);
    }

    /// No worker to service without the feature.
    #[cfg(not(feature = "agent"))]
    #[expect(
        clippy::unused_self,
        reason = "uniform no-op mirroring the agent-enabled method"
    )]
    fn poll_agent(&mut self) {}

    /// The directory the fuzzy file finder searches: the explorer root, else the working dir.
    fn project_root(&self) -> PathBuf {
        self.explorer.as_ref().map_or_else(
            || std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            |explorer| explorer.root().to_path_buf(),
        )
    }

    /// Re-runs the hybrid configuration — the Nickel manifest, its pinned extensions, and
    /// `config.scm` — in the running editor without a restart (PRD #1 §5.5: live-reloadable config),
    /// reporting the outcome in the status bar. Invoked by the `reload-config` palette command.
    fn reload_config(&mut self) {
        let notices = crate::apply_config(&mut self.workspace);
        let status = if notices.is_empty() {
            "config reloaded".to_owned()
        } else {
            format!("config reloaded with issues: {}", notices.join("; "))
        };
        self.workspace.set_status(status);
    }

    /// Routes a key to the open finder modal: type to filter, arrows to move, Enter/Esc to
    /// accept/cancel.
    fn finder_key(&mut self, key: KeyPress) {
        match key.code {
            KeyCode::Escape => self.finder = None,
            KeyCode::Enter => self.finder_accept(),
            KeyCode::Up => {
                if let Some(finder) = self.finder.as_mut() {
                    finder.select_up();
                }
            }
            KeyCode::Down => {
                if let Some(finder) = self.finder.as_mut() {
                    finder.select_down();
                }
            }
            KeyCode::Backspace => {
                if let Some(finder) = self.finder.as_mut() {
                    finder.backspace();
                }
            }
            KeyCode::Char(c)
                if !key.mods.contains(Mods::CTRL)
                    && !key.mods.contains(Mods::ALT)
                    && !key.mods.contains(Mods::SUPER) =>
            {
                if let Some(finder) = self.finder.as_mut() {
                    finder.push(c);
                }
            }
            _ => {}
        }
    }

    /// Performs the selected finder action (open a file / run a command) and closes the modal.
    fn finder_accept(&mut self) {
        let Some(action) = self.finder.as_ref().and_then(Finder::accept).cloned() else {
            self.finder = None;
            return;
        };
        self.finder = None;
        match action {
            Action::OpenFile(path) => self.open_path(&path),
            Action::RunCommand(name) if name == "reload-config" => self.reload_config(),
            Action::RunCommand(name) if name == "goto-type-definition" => {
                self.trigger_type_definition();
            }
            Action::RunCommand(name) if name == "goto-implementation" => {
                self.trigger_implementation();
            }
            Action::RunCommand(name) if name == "next-diagnostic" => {
                self.goto_diagnostic(Direction::Next);
            }
            Action::RunCommand(name) if name == "prev-diagnostic" => {
                self.goto_diagnostic(Direction::Prev);
            }
            Action::RunCommand(name) if name == "replace" => self.trigger_replace(),
            Action::RunCommand(name) if name == "goto-line" => self.trigger_goto_line(),
            Action::RunCommand(name) if name == "workspace-symbols" => {
                self.trigger_workspace_symbols();
            }
            Action::RunCommand(name) if name == "toggle-fold" => {
                self.workspace.toggle_active_fold();
            }
            Action::RunCommand(name) => self.workspace.execute(&name),
        }
        // A command may have requested the search line (e.g. `find` chosen from the palette).
        if self.workspace.take_search_request() {
            self.open_search();
        }
    }

    /// Routes a key to the open Info reader: `n`/`p`/`u` navigate, Enter follows the selected
    /// menu entry, `l` goes back, arrows/Page scroll and move the menu selection, `q`/Esc closes.
    fn info_key(&mut self, key: KeyPress) {
        if key.code == KeyCode::Escape || key == KeyPress::char('q') {
            self.info = None;
            return;
        }
        let Some(info) = self.info.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Char('n') => info.next(),
            KeyCode::Char('p') => info.prev(),
            KeyCode::Char('u') => info.up(),
            KeyCode::Char('l') => info.back(),
            KeyCode::Enter => info.enter(),
            KeyCode::Up => info.select_up(),
            KeyCode::Down => info.select_down(),
            KeyCode::PageUp => info.scroll_up(10),
            KeyCode::PageDown | KeyCode::Char(' ') => info.scroll_down(10),
            _ => {}
        }
    }

    /// Routes a key to the first-run selector: a mnemonic letter chooses that profile, `Esc`
    /// accepts the CUA default. The choice is applied live and persisted so the prompt does not
    /// reappear; a persistence failure is non-fatal (it just re-prompts next launch).
    fn selector_key(&mut self, key: KeyPress) {
        let Some(selector) = self.selector.as_ref() else {
            return;
        };
        let chosen = match key.code {
            KeyCode::Escape => Some(Profile::Cua),
            KeyCode::Char(ch) => selector.choose(ch),
            _ => None,
        };
        let Some(profile) = chosen else {
            return; // an unrecognized key keeps the modal open
        };
        self.selector = None;
        self.workspace.set_profile(profile);
        match majestic_config::write_keymap(profile.name()) {
            Ok(path) => self.workspace.set_status(format!(
                "keybindings: {} — saved to {}",
                profile.name(),
                path.display()
            )),
            Err(error) => self.workspace.set_status(format!(
                "keybindings: {} (not saved: {error})",
                profile.name()
            )),
        }
    }

    /// Routes a key to the open help overlay: arrows/Page scroll, Esc or F1 close.
    fn help_key(&mut self, key: KeyPress) {
        if key.code == KeyCode::Escape || key == HELP_KEY {
            self.help = None;
            return;
        }
        if let Some(help) = self.help.as_mut() {
            match key.code {
                KeyCode::Up => help.scroll_up(1),
                KeyCode::Down => help.scroll_down(1),
                KeyCode::PageUp => help.scroll_up(10),
                KeyCode::PageDown => help.scroll_down(10),
                _ => {}
            }
        }
    }

    fn paste(&mut self, text: &str) -> io::Result<()> {
        if self.focus == Focus::Terminal {
            if let Some(term) = self.terminal.as_mut() {
                return term.write_input(text.as_bytes());
            }
        }
        self.workspace.insert_text(text);
        Ok(())
    }

    /// `F12`: spawn the shell on first use (focusing it), otherwise flip focus between the
    /// terminal panel and the editor — the shell session survives toggling.
    fn toggle_terminal(&mut self, columns: u16, lines: u16) {
        if self.terminal_running() {
            self.focus = if self.focus == Focus::Terminal {
                Focus::Editor
            } else {
                Focus::Terminal
            };
            return;
        }
        // No live shell: (re)spawn one and focus the panel. It is resized to the panel on render.
        self.terminal = PtyTerminal::spawn(usize::from(columns), usize::from(lines)).ok();
        self.focus = if self.terminal.is_some() {
            Focus::Terminal
        } else {
            Focus::Editor
        };
    }

    /// Toggles the Warp-style Architect drop-down terminal (``Ctrl+` ``): (re)spawns a shell if needed, shows
    /// the pane across the top, and focuses it. Toggling again hides it and returns focus to the editor.
    fn toggle_architect_terminal(&mut self, columns: u16, lines: u16) {
        if self.architect_terminal_visible {
            self.architect_terminal_visible = false;
            if self.focus == Focus::ArchitectTerminal {
                self.focus = Focus::Editor;
            }
            return;
        }
        if self.terminal.is_none() {
            self.terminal = PtyTerminal::spawn(usize::from(columns), usize::from(lines)).ok();
        }
        if self.terminal.is_some() {
            self.architect_terminal_visible = true;
            self.architect_terminal_nl = false;
            self.focus = Focus::ArchitectTerminal;
        }
    }

    /// Routes a key to the focused Architect terminal. In shell mode every key is forwarded to the PTY (like the
    /// bottom terminal); `Tab` flips into natural-language mode (agent builds only). Closing is ``Ctrl+` ``.
    fn architect_terminal_key(&mut self, key: KeyPress) -> io::Result<()> {
        #[cfg(feature = "agent")]
        if self.architect_terminal_nl {
            self.architect_terminal_nl_key(key);
            return Ok(());
        }
        #[cfg(feature = "agent")]
        if key.code == KeyCode::Tab {
            // Flip the input line into natural-language mode: the next Enter asks the Architect.
            self.architect_terminal_nl = true;
            self.architect_terminal_input.clear();
            return Ok(());
        }
        if let Some(term) = self.terminal.as_mut() {
            if let Some(bytes) = encode_key(key) {
                term.write_input(&bytes)?;
            }
        }
        Ok(())
    }

    /// Edits the Architect terminal's natural-language line. `Tab`/`Esc` cancel back to the shell; `Enter` hands
    /// the line to the governed agent (the streamed reply and any approval land in the Architect sidebar).
    #[cfg(feature = "agent")]
    fn architect_terminal_nl_key(&mut self, key: KeyPress) {
        match key.code {
            KeyCode::Tab | KeyCode::Escape => {
                self.architect_terminal_nl = false;
                self.architect_terminal_input.clear();
            }
            KeyCode::Enter => {
                let line = self.architect_terminal_input.trim().to_owned();
                self.architect_terminal_input.clear();
                self.architect_terminal_nl = false;
                if !line.is_empty() {
                    self.ask_agent(&line);
                }
            }
            KeyCode::Backspace => {
                self.architect_terminal_input.pop();
            }
            KeyCode::Char(c) if !key.mods.contains(Mods::CTRL) && !key.mods.contains(Mods::ALT) => {
                self.architect_terminal_input.push(c);
            }
            _ => {}
        }
    }
}

/// Draws the terminal panel's tabbed divider row: a Steel Blue rule with a `TERMINAL` tab,
/// highlighted (reverse) when the panel holds focus (UI.md §5 bottom-panel tab bar).
fn draw_panel_tab(surface: &mut Buffer, area: Rect, theme: &Theme, focused: bool) {
    if area.is_empty() {
        return;
    }
    let rule = Style::new(theme.accent, theme.background); // Steel Blue box-drawing on Void Navy
    for x in area.x..area.right() {
        surface.set_char(x, area.y, '─', rule);
    }
    let label_style = if focused {
        Style::new(theme.background, theme.accent) // active tab: Void Navy on Steel Blue
    } else {
        rule
    };
    surface.set_str(area.x.saturating_add(1), area.y, " TERMINAL ", label_style);
}

/// Draws the Architect terminal's header rule with a tab that names the current input mode and its
/// toggle: `ARCHITECT · shell (Tab: NL)` while feeding the shell, `ARCHITECT · NL prompt (Tab: shell)`
/// while typing a natural-language request. Without the `agent` feature (`nl_available = false`) it is
/// a plain shell, so the tab is just `ARCHITECT · shell`. Reverse-highlighted when the pane is focused.
fn draw_architect_terminal_header(
    surface: &mut Buffer,
    area: Rect,
    theme: &Theme,
    focused: bool,
    nl_mode: bool,
    nl_available: bool,
) {
    if area.is_empty() {
        return;
    }
    let rule = Style::new(theme.accent, theme.background); // Steel Blue on Void Navy
    for x in area.x..area.right() {
        surface.set_char(x, area.y, '─', rule);
    }
    let label_style = if focused {
        Style::new(theme.background, theme.accent) // active tab: Void Navy on Steel Blue
    } else {
        rule
    };
    let tab = if !nl_available {
        " ARCHITECT · shell "
    } else if nl_mode {
        " ARCHITECT · NL prompt (Tab: shell) "
    } else {
        " ARCHITECT · shell (Tab: NL) "
    };
    surface.set_str(area.x.saturating_add(1), area.y, tab, label_style);
}

/// Draws the Architect terminal's natural-language input row: a `›` prompt and the typed text, truncated to the
/// row. Drawn only while the pane is in NL mode.
fn draw_architect_terminal_input(surface: &mut Buffer, area: Rect, theme: &Theme, input: &str, focused: bool) {
    if area.is_empty() {
        return;
    }
    let text_style = if focused {
        theme.base_style()
    } else {
        Style::new(theme.accent, theme.background)
    };
    for x in area.x..area.right() {
        surface.set_char(x, area.y, ' ', text_style);
    }
    surface.set_str(area.x, area.y, "› ", Style::new(theme.info, theme.background));
    let avail = usize::from(area.width.saturating_sub(2));
    let shown: String = input.chars().take(avail).collect();
    surface.set_str(area.x.saturating_add(2), area.y, &shown, text_style);
}

/// A headless editor session the daemon drives over a socket.
///
/// It owns the full [`App`] and renders into an off-screen [`Buffer`] — the exact frame `run` would
/// have drawn locally. The daemon mirrors that frame to every attached client, diffing it against
/// each client's own front buffer, so a late joiner gets a full repaint while the others get only
/// the delta (see [`SessionHost::render_frame`]). The whole editor UI (sidebar, panels, overlays)
/// is reused without touching a real terminal; only input and output go to the wire.
pub(crate) struct SessionHost {
    app: App,
    /// The latest rendered frame, mirrored to every attached client.
    frame: Buffer,
    theme: Theme,
    cols: u16,
    rows: u16,
}

impl SessionHost {
    /// Creates a host over `workspace` at the given client terminal size.
    pub(crate) fn new(workspace: Workspace, cols: u16, rows: u16) -> Self {
        let theme = Theme::steelbore();
        let (cols, rows) = (cols.max(1), rows.max(1));
        let frame = Buffer::new(cols, rows, theme.base_style());
        Self {
            app: App::new(workspace),
            frame,
            theme,
            cols,
            rows,
        }
    }

    /// Resizes the off-screen surface to a (new) mirrored size (the smallest attached terminal).
    ///
    /// A no-op when the size is unchanged, so redundant renegotiation does not resize the frame and
    /// thereby force a needless full repaint on clients whose own size did not change.
    pub(crate) fn resize(&mut self, cols: u16, rows: u16) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.frame.resize(cols, rows, self.theme.base_style());
    }

    /// Feeds one key; returns whether the editor asked to quit.
    ///
    /// # Errors
    /// Propagates an I/O error from the integrated terminal panel.
    pub(crate) fn input(&mut self, key: KeyPress) -> io::Result<bool> {
        self.app.handle_key(key, self.cols, self.rows)?;
        Ok(self.app.should_quit())
    }

    /// Renders the current state into the shared frame and returns it. The daemon diffs this against
    /// each client's front buffer (via [`penumbra::render`]) to produce that client's byte stream —
    /// a full repaint for a freshly attached client (its front buffer is empty), a minimal delta for
    /// the rest.
    pub(crate) fn render_frame(&mut self) -> &Buffer {
        self.app.reap_dead_terminal();
        self.app.render(&mut self.frame, &self.theme);
        &self.frame
    }

    /// Snapshots the current layout/open-files/cursors into a [`Session`] (persisted on detach).
    pub(crate) fn to_session(&self) -> Session {
        self.app.workspace().to_session()
    }

    /// Whether the integrated terminal is streaming output — the daemon ticks faster while it is,
    /// so a shell's output keeps painting for an attached client (matches `run`'s cadence).
    pub(crate) fn terminal_running(&self) -> bool {
        self.app.terminal_running()
    }
}

/// Runs the editor + terminal interactive loop until a quit command is issued.
///
/// When `persist_session` is set, the workspace layout is saved to the session file on exit so a
/// later plain `mj` reopens it (the transient `mj info` view passes `false`).
///
/// # Errors
/// Returns any terminal I/O error from setup, reading events, or rendering.
pub(crate) fn run(
    workspace: Workspace,
    initial_info: Option<PathBuf>,
    first_run: bool,
    persist_session: bool,
) -> io::Result<()> {
    let _guard = TerminalGuard::enter()?;
    let theme = Theme::steelbore();
    let (columns, lines) = terminal::size()?;
    let mut screen = Screen::new(columns, lines, theme.base_style());
    let mut out = io::stdout();
    let mut app = App::new(workspace);
    if first_run {
        // No manifest yet: ask the user to pick a keybinding profile before editing.
        app.selector = Some(ProfileSelector::new());
    }
    if let Some(path) = initial_info {
        // A launch-time Info path (an `.info` argument, or `mj info`) opens the reader directly —
        // including the extension-less `dir` directory file.
        if let Ok(reader) = InfoReader::open(&path) {
            app.info = Some(reader);
        }
    }

    loop {
        app.reap_dead_terminal();
        app.poll_agent(); // service the agent worker's buffer/approval requests + completion
        app.render(screen.back_mut(), &theme);
        screen.present(&mut out)?;
        out.flush()?;

        // Poll quickly while a shell streams output or an agent turn is running (even when the editor
        // is focused, so tool requests are serviced promptly); idle longer when only editing.
        let mut timeout = if app.terminal_running() || app.agent_running() {
            Duration::from_millis(16)
        } else {
            Duration::from_millis(200)
        };
        // …but never sleep past a pending debounced LSP request, so it fires at its deadline rather
        // than on the next idle tick.
        if let Some(due) = app.lsp_debounce_timeout() {
            timeout = timeout.min(due);
        }
        if event::poll(timeout)? {
            let (columns, lines) = (screen.front().width(), screen.front().height());
            match event::read()? {
                Event::Key(key) => {
                    if let Some(press) = translate(key) {
                        app.handle_key(press, columns, lines)?;
                    }
                }
                Event::Resize(columns, lines) => {
                    screen.resize(columns, lines, theme.base_style());
                    // The terminal is re-sized to its panel on the next render.
                }
                Event::Paste(text) => app.paste(&text)?,
                _ => {}
            }
        }

        if app.should_quit() {
            break;
        }
    }

    // Persist the layout/open-files/cursors so the next plain `mj` resumes here. A save failure
    // (e.g. no writable state dir) must not turn a clean quit into an error.
    if persist_session {
        let _ = app.workspace.to_session().save();
    }
    Ok(())
}

/// The byte offset where the identifier ending at `cursor` begins: `cursor` minus the trailing run
/// of identifier characters (alphanumeric or `_`). A completion replaces `start..cursor`, so an
/// empty run (cursor not after an identifier) yields `cursor` itself — the candidate is inserted.
fn identifier_start(text: &str, cursor: usize) -> usize {
    let end = cursor.min(text.len());
    text[..end]
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
        .last()
        .map_or(end, |(index, _)| index)
}

/// The byte offset where the identifier containing `cursor` ends: `cursor` plus the leading run of
/// identifier characters (alphanumeric or `_`) at and after it. Paired with [`identifier_start`] it
/// bounds the whole identifier under the cursor (used to pre-fill the rename prompt with its name).
fn identifier_end(text: &str, cursor: usize) -> usize {
    let start = cursor.min(text.len());
    let run: usize = text[start..]
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .map(char::len_utf8)
        .sum();
    start + run
}

/// Whether `key` is `Ctrl+Shift+P` (the command palette), tolerant of the terminal reporting
/// the letter as either case.
fn is_command_palette(key: KeyPress) -> bool {
    key.mods.contains(Mods::CTRL)
        && key.mods.contains(Mods::SHIFT)
        && matches!(key.code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&'p'))
}

/// Whether `key` is the agent-sidebar toggle (`Ctrl+Shift+A`).
fn is_agent_toggle(key: KeyPress) -> bool {
    key.mods.contains(Mods::CTRL)
        && key.mods.contains(Mods::SHIFT)
        && matches!(key.code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&'a'))
}

/// Whether `key` is the agent-stop-all panic key (`Ctrl+Shift+K`) — engages the running agent's
/// kill switch from any focus, the user's stop button (PRD #1 §5.2.4).
fn is_agent_stop(key: KeyPress) -> bool {
    key.mods.contains(Mods::CTRL)
        && key.mods.contains(Mods::SHIFT)
        && matches!(key.code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&'k'))
}

/// Whether `key` is the "Ask Architect" NL-prompt key (`Ctrl+Shift+N`).
#[cfg(feature = "agent")]
fn is_agent_prompt(key: KeyPress) -> bool {
    key.mods.contains(Mods::CTRL)
        && key.mods.contains(Mods::SHIFT)
        && matches!(key.code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&'n'))
}

/// Whether `key` is `Shift+Alt+F` (reformat the document — the universal "Format Document"
/// shortcut). Tolerant of how terminals report the chord: some send the `Shift` modifier, others
/// just upper-case the letter. The `Shift` requirement is deliberate, so this never shadows `Alt+f`
/// (the Emacs "forward-word" editing key); `Ctrl` must be absent for the same reason.
fn is_format_key(key: KeyPress) -> bool {
    key.mods.contains(Mods::ALT)
        && !key.mods.contains(Mods::CTRL)
        && matches!(
            key.code,
            KeyCode::Char(c)
                if c.eq_ignore_ascii_case(&'f')
                    && (key.mods.contains(Mods::SHIFT) || c.is_ascii_uppercase())
        )
}

/// Whether `key` is `Shift+F12` (find all references — the companion to `F12` goto-definition).
/// Requires `Shift` and forbids `Ctrl`/`Alt`, so it is distinct from plain `F12` (goto-definition)
/// and is dispatched before the modifier-agnostic terminal toggle. Tolerant of terminals that add
/// other flags to the chord, matching on the `Shift`+function-key shape via [`REFERENCES_KEY`].
fn is_references_key(key: KeyPress) -> bool {
    key.mods.contains(Mods::SHIFT)
        && !key.mods.contains(Mods::CTRL)
        && !key.mods.contains(Mods::ALT)
        && key.code == REFERENCES_KEY.code
}

/// Whether `key` is `Shift+F6` (start an LSP rename). Requires `Shift`, forbids `Ctrl`/`Alt`,
/// tolerant of extra flags the terminal may add — matched via [`RENAME_KEY`]'s code.
fn is_rename_key(key: KeyPress) -> bool {
    key.mods.contains(Mods::SHIFT)
        && !key.mods.contains(Mods::CTRL)
        && !key.mods.contains(Mods::ALT)
        && key.code == RENAME_KEY.code
}

/// Whether `key` is `Ctrl+.` (show code actions / quick fixes — the universal "Quick Fix" shortcut).
/// Requires `Ctrl`, forbids `Shift`/`Alt`.
fn is_code_action_key(key: KeyPress) -> bool {
    key.mods.contains(Mods::CTRL)
        && !key.mods.contains(Mods::SHIFT)
        && !key.mods.contains(Mods::ALT)
        && key.code == KeyCode::Char('.')
}

/// Whether `key` is `Shift+F8` (jump to the *previous* diagnostic). Requires `Shift`, forbids
/// `Ctrl`/`Alt`; tolerant of extra flags the terminal may add — matched via [`NEXT_DIAGNOSTIC_KEY`]'s
/// code.
fn is_prev_diagnostic_key(key: KeyPress) -> bool {
    key.mods.contains(Mods::SHIFT)
        && !key.mods.contains(Mods::CTRL)
        && !key.mods.contains(Mods::ALT)
        && key.code == NEXT_DIAGNOSTIC_KEY.code
}

/// Whether `key` is `Ctrl+Shift+O` (go to symbol in file — opens the document-symbols picker).
/// Tolerant of the terminal reporting the letter as either case, mirroring [`is_command_palette`]
/// (`Ctrl+Shift+P`).
fn is_document_symbols_key(key: KeyPress) -> bool {
    key.mods.contains(Mods::CTRL)
        && key.mods.contains(Mods::SHIFT)
        && matches!(key.code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&'o'))
}

/// Translates a crossterm key event into a Keymaker [`KeyPress`], if it maps to one.
pub(crate) fn translate(key: KeyEvent) -> Option<KeyPress> {
    if key.kind == KeyEventKind::Release {
        return None; // ignore key-release events (Kitty protocol)
    }

    let mut mods = Mods::NONE;
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        mods |= Mods::CTRL;
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        mods |= Mods::ALT;
    }
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        mods |= Mods::SHIFT;
    }
    if key.modifiers.contains(KeyModifiers::SUPER) {
        mods |= Mods::SUPER;
    }

    let code = match key.code {
        TermKey::Char(c) => KeyCode::Char(c),
        TermKey::Enter => KeyCode::Enter,
        TermKey::Esc => KeyCode::Escape,
        TermKey::Tab => KeyCode::Tab,
        TermKey::Backspace => KeyCode::Backspace,
        TermKey::Delete => KeyCode::Delete,
        TermKey::Insert => KeyCode::Insert,
        TermKey::Left => KeyCode::Left,
        TermKey::Right => KeyCode::Right,
        TermKey::Up => KeyCode::Up,
        TermKey::Down => KeyCode::Down,
        TermKey::Home => KeyCode::Home,
        TermKey::End => KeyCode::End,
        TermKey::PageUp => KeyCode::PageUp,
        TermKey::PageDown => KeyCode::PageDown,
        TermKey::F(n) => KeyCode::Function(n),
        _ => return None,
    };
    Some(KeyPress::new(mods, code))
}

/// Encodes a [`KeyPress`] into the bytes a terminal child expects, if any.
fn encode_key(key: KeyPress) -> Option<Vec<u8>> {
    let ctrl = key.mods.contains(Mods::CTRL);
    let alt = key.mods.contains(Mods::ALT);
    let mut out = Vec::new();

    match key.code {
        KeyCode::Char(c) => {
            if alt {
                out.push(0x1b); // Alt = ESC prefix
            }
            if ctrl {
                match u8::try_from(c) {
                    Ok(byte) => out.push(byte & 0x1f), // Ctrl maps to a control byte
                    Err(_) => push_char(&mut out, c),
                }
            } else {
                push_char(&mut out, c);
            }
        }
        KeyCode::Enter => out.push(b'\r'),
        KeyCode::Tab => out.push(b'\t'),
        KeyCode::Backspace => out.push(0x7f),
        KeyCode::Escape => out.push(0x1b),
        KeyCode::Left => out.extend_from_slice(b"\x1b[D"),
        KeyCode::Right => out.extend_from_slice(b"\x1b[C"),
        KeyCode::Up => out.extend_from_slice(b"\x1b[A"),
        KeyCode::Down => out.extend_from_slice(b"\x1b[B"),
        KeyCode::Home => out.extend_from_slice(b"\x1b[H"),
        KeyCode::End => out.extend_from_slice(b"\x1b[F"),
        KeyCode::PageUp => out.extend_from_slice(b"\x1b[5~"),
        KeyCode::PageDown => out.extend_from_slice(b"\x1b[6~"),
        KeyCode::Delete => out.extend_from_slice(b"\x1b[3~"),
        KeyCode::Insert => out.extend_from_slice(b"\x1b[2~"),
        KeyCode::Function(_) => return None,
    }

    (!out.is_empty()).then_some(out)
}

/// Appends the UTF-8 encoding of `c` to `out`.
fn push_char(out: &mut Vec<u8>, c: char) {
    let mut buf = [0u8; 4];
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
}

#[cfg(test)]
mod tests {
    use super::{encode_key, App, GOTO_DEF_KEY, TERMINAL_TOGGLE};
    use keymaker::{KeyCode, KeyPress, Mods};
    use majestic_core::{Editor, Workspace};
    use penumbra::{Buffer, Theme};

    #[test]
    fn renders_workspace_with_tab_bar_and_global_status_bar() {
        // With no terminal: row 0 is the workspace tab bar, the editor body sits below it, and
        // the last row is the global status bar (Steelbore accent background).
        let theme = Theme::steelbore();
        let mut editor = Editor::new();
        editor.handle_key(KeyPress::char('h'));
        editor.handle_key(KeyPress::char('i'));

        let mut app = App::new(Workspace::new(editor));
        let mut surface = Buffer::new(60, 6, theme.base_style());
        app.render(&mut surface, &theme);

        // Row 0 is the tab bar; the scratch buffer is listed there.
        let tabs: String = (0..surface.width())
            .filter_map(|x| surface.cell(x, 0).map(|c| c.symbol))
            .collect();
        assert!(
            tabs.contains("scratch"),
            "tab bar lists the buffer: {tabs:?}"
        );
        // Editor content is drawn just below the tab bar, past the 3-column line-number gutter.
        assert_eq!(surface.cell(3, 1).unwrap().symbol, 'h');
        assert_eq!(surface.cell(4, 1).unwrap().symbol, 'i');
        // The bottom row is the status bar: accent background, end-anchored F12 hint present.
        let status_row = surface.height() - 1;
        assert_eq!(surface.cell(0, status_row).unwrap().style.bg, theme.accent);
        let tail: String = (0..surface.width())
            .filter_map(|x| surface.cell(x, status_row).map(|c| c.symbol))
            .collect();
        assert!(
            tail.contains("F12"),
            "status bar shows the terminal hint: {tail:?}"
        );
    }

    #[test]
    fn dot_info_opens_in_the_reader_and_q_closes_it() {
        let dir = std::env::temp_dir().join(format!("mj-info-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("sample.info");
        std::fs::write(
            &path,
            "\u{001f}\nFile: sample.info,  Node: Top,  Up: (dir)\n\nHello, info world.\n",
        )
        .unwrap();

        let mut app = App::new(Workspace::new(Editor::new()));
        app.open_path(&path);
        assert!(app.info.is_some(), ".info opens in the Info reader");

        // The reader renders its body into the editor region.
        let theme = Theme::steelbore();
        let mut surface = Buffer::new(60, 6, theme.base_style());
        app.render(&mut surface, &theme);
        let mut text = String::new();
        for y in 0..surface.height() {
            for x in 0..surface.width() {
                if let Some(cell) = surface.cell(x, y) {
                    text.push(cell.symbol);
                }
            }
        }
        assert!(text.contains("Hello, info world."), "node body is shown");

        app.handle_key(KeyPress::char('q'), 60, 6).unwrap();
        assert!(app.info.is_none(), "q closes the reader");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn printable_keys_encode_to_utf8() {
        assert_eq!(encode_key(KeyPress::char('a')), Some(b"a".to_vec()));
        assert_eq!(encode_key(KeyPress::char('A')), Some(b"A".to_vec()));
        assert_eq!(
            encode_key(KeyPress::char('é')),
            Some("é".as_bytes().to_vec())
        );
    }

    #[test]
    fn control_and_named_keys() {
        assert_eq!(encode_key(KeyPress::ctrl('c')), Some(vec![3])); // Ctrl+C -> ETX
        assert_eq!(encode_key(KeyPress::key(KeyCode::Enter)), Some(vec![b'\r']));
        assert_eq!(
            encode_key(KeyPress::key(KeyCode::Backspace)),
            Some(vec![0x7f])
        );
        assert_eq!(
            encode_key(KeyPress::key(KeyCode::Left)),
            Some(b"\x1b[D".to_vec())
        );
    }

    #[test]
    fn alt_prefixes_escape() {
        assert_eq!(
            encode_key(KeyPress::new(Mods::ALT, KeyCode::Char('x'))),
            Some(vec![0x1b, b'x'])
        );
    }

    #[test]
    fn function_keys_are_not_encoded() {
        assert_eq!(encode_key(KeyPress::key(KeyCode::Function(5))), None);
        assert_eq!(TERMINAL_TOGGLE, KeyCode::Function(12));
    }

    #[test]
    fn plain_f12_toggles_the_terminal_not_goto_definition() {
        // Regression: F12 was double-bound — goto-definition (then plain F12) shadowed the terminal
        // toggle in `try_lsp_trigger`, so the advertised "F12 terminal" did nothing. goto-definition
        // now takes the Ctrl chord, leaving plain F12 for the modifier-agnostic terminal toggle.
        let f12 = KeyPress::key(KeyCode::Function(12));
        assert_eq!(
            f12.code, TERMINAL_TOGGLE,
            "plain F12 is the terminal toggle code"
        );
        assert_ne!(
            f12, GOTO_DEF_KEY,
            "plain F12 must not be the goto-definition key"
        );
        assert!(
            GOTO_DEF_KEY.mods.contains(Mods::CTRL),
            "goto-definition now requires Ctrl so it does not shadow the terminal toggle"
        );
    }

    #[test]
    fn architect_terminal_toggle_is_ctrl_backtick() {
        // The Architect drop-down terminal is bound to Ctrl+` (the classic drop-down-terminal key, as in VS Code).
        assert_eq!(super::ARCHITECT_TERMINAL_TOGGLE, KeyPress::ctrl('`'));
    }

    #[test]
    fn ctrl_backtick_translates_to_the_architect_terminal_toggle() {
        // Once the Kitty keyboard protocol is on (TerminalGuard), the terminal reports Ctrl+` as
        // `Char('`') + CONTROL` (instead of collapsing it to NUL/Ctrl+Space); `translate` must turn
        // that into the toggle key. This is the routing the protocol-enable made reachable.
        use crossterm::event::{KeyCode as TermKey, KeyEvent, KeyModifiers};
        let event = KeyEvent::new(TermKey::Char('`'), KeyModifiers::CONTROL);
        assert_eq!(super::translate(event), Some(super::ARCHITECT_TERMINAL_TOGGLE));
    }

    #[cfg(feature = "agent")]
    #[test]
    fn architect_terminal_nl_edits_the_line_then_tab_returns_to_the_shell() {
        // In the Architect terminal's natural-language mode, typing extends the line, Backspace trims it, and
        // Tab cancels back to feeding the shell (clearing the line). No PTY or agent turn involved.
        let mut app = App::new(Workspace::new(Editor::new()));
        app.architect_terminal_nl = true;
        for c in "fix it".chars() {
            app.architect_terminal_nl_key(KeyPress::char(c));
        }
        assert_eq!(app.architect_terminal_input, "fix it");
        app.architect_terminal_nl_key(KeyPress::key(KeyCode::Backspace));
        assert_eq!(app.architect_terminal_input, "fix i");
        app.architect_terminal_nl_key(KeyPress::key(KeyCode::Tab));
        assert!(!app.architect_terminal_nl, "Tab returns to shell input");
        assert!(app.architect_terminal_input.is_empty(), "the NL line is cleared");
    }

    #[test]
    fn replace_flow_replaces_every_occurrence_in_the_buffer() {
        let mut editor = Editor::new();
        for c in "foo bar foo".chars() {
            editor.handle_key(KeyPress::char(c));
        }
        let mut app = App::new(Workspace::new(editor));

        // The two-stage prompt: "Search for" foo → "Replace with" baz.
        app.trigger_replace();
        for c in "foo".chars() {
            app.prompt_key(KeyPress::char(c));
        }
        app.prompt_key(KeyPress::key(KeyCode::Enter)); // stage 1 → opens stage 2
        for c in "baz".chars() {
            app.prompt_key(KeyPress::char(c));
        }
        app.prompt_key(KeyPress::key(KeyCode::Enter)); // applies the replacement

        assert_eq!(app.workspace.active().buffer().text(), "baz bar baz");
        assert!(app.prompt.is_none(), "the prompt closes after replacing");
    }

    #[test]
    fn goto_line_jumps_to_the_line_start_and_clamps() {
        // Lines start at bytes 0 ("one"), 4 ("two"), 8 ("three"), 14 ("four").
        let editor = Editor::with_buffer(majestic_core::Buffer::from_text("one\ntwo\nthree\nfour"));
        let mut app = App::new(Workspace::new(editor));

        app.trigger_goto_line();
        app.prompt_key(KeyPress::char('3'));
        app.prompt_key(KeyPress::key(KeyCode::Enter));
        assert_eq!(app.workspace.active().buffer().cursor(), 8); // start of "three"

        // A line past the end clamps to the last line.
        app.trigger_goto_line();
        for c in "999".chars() {
            app.prompt_key(KeyPress::char(c));
        }
        app.prompt_key(KeyPress::key(KeyCode::Enter));
        assert_eq!(app.workspace.active().buffer().cursor(), 14); // start of "four"
    }
}
