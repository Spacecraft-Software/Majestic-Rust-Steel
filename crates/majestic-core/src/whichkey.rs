// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The which-key hint overlay (PRD #1 §5.2.1).
//!
//! While a multi-key prefix is in progress (the Spacemacs `SPC` leader, an Emacs `C-x`, …), the
//! host shows this transient hint listing the keys that may come next and what each does. It is
//! built from [`Editor::which_key`](crate::Editor::which_key) and drawn near the bottom of the
//! editor area, the conventional which-key position.

use keymaker::{KeyCode, KeyPress, Mods};
use penumbra::{Buffer as Surface, Rect, Style, Theme};

use crate::finder::draw_box;

/// A transient list of the keys available after the in-progress prefix, with what each runs.
#[derive(Clone, Debug)]
pub struct WhichKey {
    rows: Vec<(KeyPress, String)>,
}

impl WhichKey {
    /// Creates a hint over `rows` of `(next key, label)` (the output of `Editor::which_key`).
    #[must_use]
    pub fn new(rows: Vec<(KeyPress, String)>) -> Self {
        Self { rows }
    }

    /// Whether there is nothing to show (no prefix in progress).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Draws the hint as a small box near the bottom of `area`.
    pub fn render(&self, surface: &mut Surface, area: Rect, theme: &Theme) {
        if self.rows.is_empty() || area.width < 12 || area.height < 4 {
            return;
        }
        let lines: Vec<String> = self
            .rows
            .iter()
            .map(|(key, label)| format!("  {:<7} {label}", key_label(*key)))
            .collect();
        let width = area.width.min(44);
        let height = (u16::try_from(lines.len()).unwrap_or(u16::MAX) + 2).min(area.height);
        // Anchor near the bottom (the conventional which-key position), horizontally centred.
        let box_area = Rect::new(
            area.x + (area.width - width) / 2,
            area.bottom().saturating_sub(height),
            width,
            height,
        );
        draw_box(surface, box_area, theme, "which-key");

        let inner = Rect::new(
            box_area.x + 1,
            box_area.y + 1,
            box_area.width - 2,
            box_area.height - 2,
        );
        let style = Style::new(theme.foreground, theme.background);
        for (row, line) in lines.iter().enumerate() {
            let Ok(row) = u16::try_from(row) else { break };
            if row >= inner.height {
                break;
            }
            surface.set_str(inner.x, inner.y + row, line, style);
        }
    }
}

/// Formats one keypress as a short which-key label (`SPC`, `f`, `C-s`, `Esc`).
fn key_label(key: KeyPress) -> String {
    let mut label = String::new();
    if key.mods.contains(Mods::CTRL) {
        label.push_str("C-");
    }
    if key.mods.contains(Mods::ALT) {
        label.push_str("M-");
    }
    if key.mods.contains(Mods::SHIFT) {
        label.push_str("S-");
    }
    let name = match key.code {
        KeyCode::Char(' ') => "SPC".to_owned(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "RET".to_owned(),
        KeyCode::Escape => "Esc".to_owned(),
        KeyCode::Tab => "TAB".to_owned(),
        KeyCode::Backspace => "DEL".to_owned(),
        KeyCode::Delete => "Del".to_owned(),
        KeyCode::Insert => "Ins".to_owned(),
        KeyCode::Left => "Left".to_owned(),
        KeyCode::Right => "Right".to_owned(),
        KeyCode::Up => "Up".to_owned(),
        KeyCode::Down => "Down".to_owned(),
        KeyCode::Home => "Home".to_owned(),
        KeyCode::End => "End".to_owned(),
        KeyCode::PageUp => "PgUp".to_owned(),
        KeyCode::PageDown => "PgDn".to_owned(),
        KeyCode::Function(number) => format!("F{number}"),
    };
    label.push_str(&name);
    label
}

#[cfg(test)]
mod tests {
    use super::{key_label, WhichKey};
    use keymaker::KeyPress;
    use penumbra::{Buffer as Surface, Theme};

    #[test]
    fn key_label_renders_space_as_spc() {
        assert_eq!(key_label(KeyPress::char(' ')), "SPC");
        assert_eq!(key_label(KeyPress::char('f')), "f");
        assert_eq!(key_label(KeyPress::ctrl('s')), "C-s");
    }

    #[test]
    fn empty_which_key_reports_empty() {
        assert!(WhichKey::new(vec![]).is_empty());
    }

    #[test]
    fn render_lists_the_continuation_labels() {
        let theme = Theme::steelbore();
        let mut surface = Surface::new(50, 12, theme.base_style());
        let area = surface.area();
        WhichKey::new(vec![
            (KeyPress::char('f'), "+prefix".to_owned()),
            (KeyPress::char(' '), "save".to_owned()),
        ])
        .render(&mut surface, area, &theme);
        let mut text = String::new();
        for y in 0..surface.height() {
            for x in 0..surface.width() {
                if let Some(cell) = surface.cell(x, y) {
                    text.push(cell.symbol);
                }
            }
        }
        assert!(text.contains("save"));
        assert!(text.contains("+prefix"));
        assert!(text.contains("SPC"));
    }
}
