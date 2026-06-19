// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The first-run keybinding-profile selector (PRD #1 §5.2.1).
//!
//! On the very first launch (no manifest yet), the host shows this modal so the user picks a
//! keymap profile before editing. The choice is applied immediately and persisted to a minimal
//! Nickel manifest (via `majestic-config`) so later launches start in the chosen profile and the
//! prompt does not reappear.

use keymaker::Profile;
use penumbra::{Buffer as Surface, Rect, Style, Theme};

use crate::finder::draw_box;

/// One offered profile: the mnemonic key, the [`Profile`] it selects, and its menu label.
#[derive(Clone, Copy, Debug)]
struct Choice {
    /// The mnemonic key (lower-case) that selects this profile.
    key: char,
    /// The profile selected.
    profile: Profile,
    /// The menu line, with the mnemonic in brackets (e.g. `[E]macs — …`).
    label: &'static str,
}

/// The modal profile picker shown on first run. Stateless apart from its fixed option list — the
/// host maps a pressed key through [`ProfileSelector::choose`] and closes the modal.
#[derive(Clone, Debug)]
pub struct ProfileSelector {
    choices: Vec<Choice>,
}

impl ProfileSelector {
    /// Creates the selector offering every built-in profile.
    #[must_use]
    pub fn new() -> Self {
        Self {
            choices: vec![
                Choice {
                    key: 'c',
                    profile: Profile::Cua,
                    label: "[C]UA — modern shortcuts (Ctrl+C/X/V copy, cut, paste)",
                },
                Choice {
                    key: 'e',
                    profile: Profile::Emacs,
                    label: "[E]macs — C-/M- chords and the C-x prefix map",
                },
                Choice {
                    key: 'v',
                    profile: Profile::Vim,
                    label: "[V]im — modal editing (Normal / Insert / Visual)",
                },
                Choice {
                    key: 's',
                    profile: Profile::Spacemacs,
                    label: "[S]pacemacs — Vim modality with a SPC leader menu",
                },
            ],
        }
    }

    /// Maps a pressed character to a profile, case-insensitively. Returns `None` when the key
    /// matches no option (the host keeps the modal open).
    #[must_use]
    pub fn choose(&self, key: char) -> Option<Profile> {
        let key = key.to_ascii_lowercase();
        self.choices
            .iter()
            .find(|choice| choice.key == key)
            .map(|choice| choice.profile)
    }

    /// The lines shown inside the modal — a heading, the options, and the key hint.
    fn lines(&self) -> Vec<String> {
        let mut lines = vec![
            "Welcome to Majestic. Choose your keybindings:".to_owned(),
            String::new(),
        ];
        for choice in &self.choices {
            lines.push(format!("  {}", choice.label));
        }
        lines.push(String::new());
        lines.push("Press the bracketed letter.   Esc = CUA (default).".to_owned());
        lines
    }

    /// Draws the selector as a centred modal box over `area` (mirrors the help overlay style).
    pub fn render(&self, surface: &mut Surface, area: Rect, theme: &Theme) {
        if area.width < 3 || area.height < 3 {
            return;
        }
        let lines = self.lines();
        let width = area.width.min(64);
        let height = area
            .height
            .min(u16::try_from(lines.len()).unwrap_or(u16::MAX) + 2);
        let box_area = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        draw_box(surface, box_area, theme, "First-run setup");

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

impl Default for ProfileSelector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::ProfileSelector;
    use keymaker::Profile;
    use penumbra::{Buffer as Surface, Theme};

    #[test]
    fn choose_maps_letters_case_insensitively() {
        let selector = ProfileSelector::new();
        assert_eq!(selector.choose('c'), Some(Profile::Cua));
        assert_eq!(selector.choose('E'), Some(Profile::Emacs));
        assert_eq!(selector.choose('v'), Some(Profile::Vim));
        assert_eq!(selector.choose('S'), Some(Profile::Spacemacs));
        assert_eq!(selector.choose('z'), None);
    }

    #[test]
    fn render_draws_the_options_into_the_surface() {
        let theme = Theme::steelbore();
        let mut surface = Surface::new(70, 16, theme.base_style());
        let area = surface.area();
        ProfileSelector::new().render(&mut surface, area, &theme);
        // The rendered box contains the heading text somewhere on its rows.
        let mut found = false;
        for y in 0..surface.height() {
            let mut row = String::new();
            for x in 0..surface.width() {
                if let Some(cell) = surface.cell(x, y) {
                    row.push(cell.symbol);
                }
            }
            if row.contains("Choose your keybindings") {
                found = true;
            }
        }
        assert!(found, "selector heading should be rendered");
    }
}
