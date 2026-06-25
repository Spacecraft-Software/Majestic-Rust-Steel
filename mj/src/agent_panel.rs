// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The Architect agent sidebar — a scrolling conversation transcript and an input line (UI.md).
//!
//! This module owns the panel *surface*: visibility, focus-driven input editing, and rendering on
//! the right of the editor. A later change wires the input to a live `AgentSession`; until then a
//! submitted message is echoed with a placeholder note, so the panel and its layout/focus/keys can be
//! exercised on their own.

use keymaker::{KeyCode, KeyPress, Mods};
use penumbra::{Buffer, Rect, Style, Theme};

/// The panel's width in columns when shown (UI.md calls for 25–35; this includes its 1-col divider).
pub const AGENT_COLS: u16 = 36;

/// The input prompt and its column width.
const PROMPT: &str = "> ";
const PROMPT_COLS: u16 = 2;

/// Who authored a transcript line. (Agent and diff lines are only produced with the `agent` feature.)
#[derive(Clone, Copy)]
enum Speaker {
    User,
    /// An agent reply.
    #[cfg(feature = "agent")]
    Agent,
    /// A removed (old) line in a proposed edit's diff.
    #[cfg(feature = "agent")]
    DiffRemoved,
    /// An added (new) line in a proposed edit's diff.
    #[cfg(feature = "agent")]
    DiffAdded,
    System,
}

/// One line of the conversation transcript.
struct ChatLine {
    speaker: Speaker,
    text: String,
}

/// The Architect agent sidebar: a transcript plus an input line.
#[derive(Default)]
pub struct AgentPanel {
    visible: bool,
    lines: Vec<ChatLine>,
    input: String,
    /// Whether the last transcript line is an in-progress streamed agent reply that
    /// [`Self::stream_token`] should append to. Reset by any explicit push or [`Self::end_stream`].
    streaming: bool,
}

impl AgentPanel {
    /// A hidden, empty panel.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the panel is shown.
    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Toggles visibility, returning the new state.
    pub fn toggle(&mut self) -> bool {
        self.visible = !self.visible;
        self.visible
    }

    /// Appends a line authored by the user.
    pub fn push_user(&mut self, text: impl Into<String>) {
        self.streaming = false;
        self.lines.push(ChatLine {
            speaker: Speaker::User,
            text: text.into(),
        });
    }

    /// Appends a line authored by the agent.
    #[cfg(feature = "agent")]
    pub fn push_agent(&mut self, text: impl Into<String>) {
        self.streaming = false;
        self.lines.push(ChatLine {
            speaker: Speaker::Agent,
            text: text.into(),
        });
    }

    /// Appends a chunk of streamed assistant text: starts a new agent line on the first token of a
    /// reply, then grows that same line as more tokens arrive (until any other push or
    /// [`Self::end_stream`]).
    #[cfg(feature = "agent")]
    pub fn stream_token(&mut self, text: &str) {
        if self.streaming {
            if let Some(line) = self.lines.last_mut() {
                line.text.push_str(text);
                return;
            }
        }
        self.push_agent(text); // resets `streaming`, so re-arm it for subsequent tokens
        self.streaming = true;
    }

    /// Marks the current streamed reply complete, so the next token starts a fresh line.
    #[cfg(feature = "agent")]
    pub fn end_stream(&mut self) {
        self.streaming = false;
    }

    /// Appends a removed (old) line of a proposed edit's diff.
    #[cfg(feature = "agent")]
    pub fn push_diff_removed(&mut self, text: impl Into<String>) {
        self.streaming = false;
        self.lines.push(ChatLine {
            speaker: Speaker::DiffRemoved,
            text: text.into(),
        });
    }

    /// Appends an added (new) line of a proposed edit's diff.
    #[cfg(feature = "agent")]
    pub fn push_diff_added(&mut self, text: impl Into<String>) {
        self.streaming = false;
        self.lines.push(ChatLine {
            speaker: Speaker::DiffAdded,
            text: text.into(),
        });
    }

    /// Appends a system note (status, errors, placeholders).
    pub fn push_system(&mut self, text: impl Into<String>) {
        self.streaming = false;
        self.lines.push(ChatLine {
            speaker: Speaker::System,
            text: text.into(),
        });
    }

    /// Handles a key while the panel is focused. Returns the submitted message on Enter (when the
    /// input is non-empty); otherwise edits the input in place and returns `None`.
    pub fn handle_key(&mut self, key: KeyPress) -> Option<String> {
        match key.code {
            KeyCode::Enter => {
                let message = self.input.trim().to_owned();
                if message.is_empty() {
                    return None;
                }
                self.input.clear();
                return Some(message);
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            // A printable character (no Ctrl/Alt) extends the input — mirrors the search input line.
            KeyCode::Char(c) if !key.mods.contains(Mods::CTRL) && !key.mods.contains(Mods::ALT) => {
                self.input.push(c);
            }
            _ => {}
        }
        None
    }

    /// Renders the panel into `area`: a header row, the transcript (newest at the bottom), a divider,
    /// and the input row.
    pub fn render(&self, surface: &mut Buffer, area: Rect, theme: &Theme, focused: bool) {
        surface.fill(area, theme.base_style());
        if area.height == 0 || area.width == 0 {
            return;
        }

        // Header: inverted (Void Navy on Steel Blue) when focused, accent-on-base otherwise.
        let header_style = if focused {
            Style::new(theme.background, theme.accent).bold()
        } else {
            Style::new(theme.accent, theme.background).bold()
        };
        for x in area.x..area.right() {
            surface.set_char(x, area.y, ' ', header_style);
        }
        surface.set_str(area.x, area.y, " Architect", header_style);

        // The input occupies the bottom row; a divider sits just above it.
        let input_row = area.bottom().saturating_sub(1);
        let divider_row = input_row.saturating_sub(1);
        let rule = Style::new(theme.accent, theme.background);
        for x in area.x..area.right() {
            surface.set_char(x, divider_row, '─', rule);
        }

        // Transcript region: rows between the header and the divider, showing the newest tail.
        let top = area.y + 1;
        if divider_row > top {
            let rows = self.display_rows(area.width, theme);
            let visible = usize::from(divider_row - top);
            let start = rows.len().saturating_sub(visible);
            for (y, (text, style)) in (top..divider_row).zip(&rows[start..]) {
                surface.set_str(area.x, y, text, *style);
            }
        }

        // Input line: a prompt then the tail of the input that fits, with a cursor when focused.
        surface.set_str(area.x, input_row, PROMPT, rule);
        let input_x = area.x + PROMPT_COLS;
        let avail = usize::from(area.width.saturating_sub(PROMPT_COLS));
        let shown = tail_chars(&self.input, avail);
        let input_style = if focused {
            theme.base_style().bold()
        } else {
            theme.base_style()
        };
        surface.set_str(input_x, input_row, &shown, input_style);
        if focused {
            let used = u16::try_from(shown.chars().count()).unwrap_or(0);
            let cursor_x = input_x + used;
            if cursor_x < area.right() {
                surface.set_char(cursor_x, input_row, '▏', input_style);
            }
        }
    }

    /// Wraps the transcript into `(text, style)` display rows for a panel `width` columns wide.
    fn display_rows(&self, width: u16, theme: &Theme) -> Vec<(String, Style)> {
        let width = usize::from(width).max(1);
        let mut rows = Vec::new();
        for line in &self.lines {
            let (marker, style) = match line.speaker {
                Speaker::User => ("you ", Style::new(theme.info, theme.background).bold()),
                #[cfg(feature = "agent")]
                Speaker::Agent => ("ai  ", theme.base_style()),
                #[cfg(feature = "agent")]
                Speaker::DiffRemoved => ("- ", Style::new(theme.error, theme.background)),
                #[cfg(feature = "agent")]
                Speaker::DiffAdded => ("+ ", Style::new(theme.success, theme.background)),
                Speaker::System => ("    ", Style::new(theme.accent, theme.background)),
            };
            let body: Vec<char> = format!("{marker}{}", line.text).chars().collect();
            if body.is_empty() {
                rows.push((String::new(), style));
                continue;
            }
            for chunk in body.chunks(width) {
                rows.push((chunk.iter().collect(), style));
            }
        }
        rows
    }
}

/// The last `max` characters of `text` (so the caret end of a long input stays visible).
fn tail_chars(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_owned();
    }
    text.chars().skip(count - max).collect()
}

#[cfg(test)]
mod tests {
    use super::AgentPanel;
    use keymaker::{KeyCode, KeyPress, Mods};

    fn typed(c: char) -> KeyPress {
        KeyPress::new(Mods::NONE, KeyCode::Char(c))
    }

    #[test]
    fn typing_extends_the_input_and_enter_submits() {
        let mut panel = AgentPanel::new();
        for c in "hello".chars() {
            assert_eq!(panel.handle_key(typed(c)), None);
        }
        // Backspace edits in place.
        assert_eq!(
            panel.handle_key(KeyPress::new(Mods::NONE, KeyCode::Backspace)),
            None
        );
        // Enter submits the trimmed input and clears it.
        let submitted = panel.handle_key(KeyPress::new(Mods::NONE, KeyCode::Enter));
        assert_eq!(submitted.as_deref(), Some("hell"));
        // A second Enter on empty input submits nothing.
        assert_eq!(
            panel.handle_key(KeyPress::new(Mods::NONE, KeyCode::Enter)),
            None
        );
    }

    #[test]
    fn toggle_flips_visibility() {
        let mut panel = AgentPanel::new();
        assert!(!panel.is_visible());
        assert!(panel.toggle());
        assert!(panel.is_visible());
        assert!(!panel.toggle());
    }

    #[test]
    fn ctrl_chars_do_not_enter_the_input() {
        let mut panel = AgentPanel::new();
        panel.handle_key(KeyPress::new(Mods::CTRL, KeyCode::Char('a')));
        // Nothing was typed, so Enter submits nothing.
        assert_eq!(
            panel.handle_key(KeyPress::new(Mods::NONE, KeyCode::Enter)),
            None
        );
    }
}
