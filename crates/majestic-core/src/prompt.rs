// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! A minimal modal text-input prompt (PRD #1 §6.9).
//!
//! A centred, bordered single-line input box — the host opens it to read one line of text (e.g. the
//! new name for an LSP rename), feeds it keystrokes ([`Prompt::push`]/[`Prompt::backspace`]), and on
//! `Enter` reads [`Prompt::input`]. Editing is append/erase at the end only (no mid-line caret) — all
//! that the rename flow needs. Modal while open: the host routes every key to it.

use penumbra::{Buffer as Surface, Rect, Style, Theme};

use crate::finder::draw_box;

/// A single-line modal text input with a title.
#[derive(Clone, Debug)]
pub struct Prompt {
    title: String,
    input: String,
}

/// Preferred box width (clamped to the available area).
const WIDTH: u16 = 48;

impl Prompt {
    /// A prompt titled `title`, pre-filled with `initial` (e.g. the symbol's current name).
    #[must_use]
    pub fn new(title: impl Into<String>, initial: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            input: initial.into(),
        }
    }

    /// Appends `c` to the input.
    pub fn push(&mut self, c: char) {
        self.input.push(c);
    }

    /// Removes the last character of the input (no-op when empty).
    pub fn backspace(&mut self) {
        self.input.pop();
    }

    /// The current input text.
    #[must_use]
    pub fn input(&self) -> &str {
        &self.input
    }

    /// Draws the prompt as a centred, bordered box: the title on the border, the input plus a caret
    /// block on the single inner line (clipped to the box width).
    pub fn render(&self, surface: &mut Surface, area: Rect, theme: &Theme) {
        if area.width < 3 || area.height < 3 {
            return;
        }
        let width = WIDTH.min(area.width);
        let height = 3; // border top + one input line + border bottom
        let box_area = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        draw_box(surface, box_area, theme, &self.title);
        let inner = Rect::new(box_area.x + 1, box_area.y + 1, box_area.width - 2, 1);
        let fg = Style::new(theme.foreground, theme.background);
        for x in inner.x..inner.right() {
            surface.set_char(x, inner.y, ' ', fg);
        }
        let line = format!("{}\u{2588}", self.input); // trailing block = caret
        let shown: String = line.chars().take(usize::from(inner.width)).collect();
        surface.set_str(inner.x, inner.y, &shown, fg);
    }
}

#[cfg(test)]
mod tests {
    use super::Prompt;

    #[test]
    fn edits_the_input_at_the_end() {
        let mut prompt = Prompt::new("Rename", "old");
        assert_eq!(prompt.input(), "old");
        prompt.backspace();
        assert_eq!(prompt.input(), "ol");
        prompt.push('d');
        prompt.push('y');
        assert_eq!(prompt.input(), "oldy");
    }

    #[test]
    fn backspace_on_empty_is_a_no_op() {
        let mut prompt = Prompt::new("Rename", "");
        prompt.backspace();
        assert_eq!(prompt.input(), "");
    }
}
