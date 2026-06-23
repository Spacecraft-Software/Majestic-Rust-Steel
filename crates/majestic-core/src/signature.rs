// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The LSP signature-help popup (PRD #1 §6.9).
//!
//! A small, *passive* box that shows the signature of the call the cursor is inside, with the
//! parameter currently being typed highlighted — opened automatically as you type `(` and `,` and
//! refreshed on each argument. Unlike the completion/hover/references/symbols popups it captures no
//! navigation keys (you keep typing arguments underneath it); only `Esc` dismisses it, and it also
//! closes when the server reports no active call. Editor-facing only — the `lsp-types`
//! `SignatureHelp` payload is reduced to the active signature's label plus the byte range of the
//! active parameter before it reaches here, so the core carries no LSP dependency.

use penumbra::{Buffer as Surface, Rect, Style, Theme};

use crate::finder::draw_box;

/// A signature-help popup: the active signature's label and the byte range of its active parameter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignatureHelp {
    label: String,
    /// Byte range `[start, end)` of the active parameter within `label`, highlighted when drawn.
    /// `None` when the server gave no active parameter or it could not be located in the label.
    active: Option<(usize, usize)>,
}

/// Largest popup the box is allowed to grow to (rows of wrapped label, columns of width).
const MAX_ROWS: u16 = 8;
const MAX_WIDTH: u16 = 72;
const MIN_WIDTH: u16 = 14;

impl SignatureHelp {
    /// Builds a popup from the active signature's `label` and the byte range of the active parameter
    /// within it (or `None`).
    #[must_use]
    pub fn new(label: impl Into<String>, active: Option<(usize, usize)>) -> Self {
        Self {
            label: label.into(),
            active,
        }
    }

    /// Whether there is nothing to show (the host should not open an empty popup).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.label.trim().is_empty()
    }

    /// Draws the popup near `cursor` (an absolute screen position), clamped inside `area`. Prefers to
    /// open just *above* the cursor (so it does not cover the arguments being typed), flipping below
    /// when there is no room. The label wraps to the box width; the active parameter's characters are
    /// drawn inverted wherever they fall.
    pub fn render(&self, surface: &mut Surface, area: Rect, cursor: (u16, u16), theme: &Theme) {
        if self.is_empty() || area.width < MIN_WIDTH || area.height < 3 {
            return;
        }
        let chars: Vec<char> = self.label.chars().collect();
        let want_width = u16::try_from(chars.len() + 2).unwrap_or(MAX_WIDTH);
        let width = want_width.clamp(MIN_WIDTH, MAX_WIDTH).min(area.width);
        let inner_width = usize::from(width - 2);
        let rows_needed = chars.len().div_ceil(inner_width.max(1)).max(1);
        let rows = u16::try_from(rows_needed).unwrap_or(MAX_ROWS).min(MAX_ROWS);
        let height = rows + 2; // a bordered box adds a row top and bottom

        // Prefer above the cursor; flip below when there is no room above, else clamp to the top.
        let (cx, cy) = cursor;
        let x = cx.min(area.right().saturating_sub(width));
        let y = if cy >= area.y + height {
            cy - height
        } else if cy + 1 + height <= area.bottom() {
            cy + 1
        } else {
            area.y
        };
        let box_area = Rect::new(x, y, width, height);

        draw_box(surface, box_area, theme, "signature");
        let inner = Rect::new(
            box_area.x + 1,
            box_area.y + 1,
            box_area.width - 2,
            box_area.height - 2,
        );
        let fg = Style::new(theme.foreground, theme.background);
        let highlight = Style::new(theme.background, theme.accent);
        // Clear the inner area, then lay the label out char-by-char, wrapping at the box width and
        // inverting any char whose byte offset falls inside the active-parameter range.
        for ry in inner.y..inner.bottom() {
            for rx in inner.x..inner.right() {
                surface.set_char(rx, ry, ' ', fg);
            }
        }
        let mut byte = 0usize;
        let mut col = 0usize;
        let mut row = 0u16;
        for &ch in &chars {
            if col >= inner_width {
                col = 0;
                row += 1;
                if row >= inner.height {
                    break;
                }
            }
            let active = self
                .active
                .is_some_and(|(start, end)| byte >= start && byte < end);
            let style = if active { highlight } else { fg };
            let px = inner.x + u16::try_from(col).unwrap_or(0);
            surface.set_char(px, inner.y + row, ch, style);
            byte += ch.len_utf8();
            col += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SignatureHelp;

    #[test]
    fn blank_label_is_empty() {
        assert!(SignatureHelp::new("", None).is_empty());
        assert!(SignatureHelp::new("   ", None).is_empty());
        assert!(!SignatureHelp::new("fn f(x: i32)", Some((5, 11))).is_empty());
    }

    #[test]
    fn carries_label_and_active_range() {
        let help = SignatureHelp::new("write(buf, n)", Some((6, 9)));
        assert_eq!(
            help,
            SignatureHelp::new("write(buf, n)".to_owned(), Some((6, 9)))
        );
        assert_ne!(help, SignatureHelp::new("write(buf, n)", None));
    }
}
