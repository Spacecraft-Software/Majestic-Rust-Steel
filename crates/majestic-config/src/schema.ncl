# SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
# SPDX-License-Identifier: GPL-3.0-or-later

# The Majestic configuration schema (Standard §5 hybrid manifest — declarative half).
#
# A record contract with defaults. The user manifest is merged onto this record
# (`schema & user`) so that every field falls back to a default when omitted and is
# type-checked by its field contract before the result is handed to Rust. Range/value
# validation beyond types is enforced on the Rust side (see `Config`).
{
  # Theme token set selecting the palette (only "steelbore" exists today).
  theme | String | default = "steelbore",

  # Keybinding profile: "cua" (default), later "vim" / "emacs" / "acme".
  keymap | String | default = "cua",

  # Indent width in columns. Clamped to a sane range when applied by the editor.
  tab_width | Number | default = 4,
}
