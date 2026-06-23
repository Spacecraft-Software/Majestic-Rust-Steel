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

  # Manifest-pinned extensions (PRD #1 §5.5 / §6.7): a dictionary keyed by extension name,
  # each pinned to an exact `version` (Doom/straight.el-style reproducibility). `enabled`
  # defaults true; `source` optionally overrides where the Steel file is found (otherwise
  # `<config-dir>/extensions/<name>.scm`). The loader runs each enabled extension's Steel
  # file and verifies it declares the pinned version via `(majestic-provides! …)`.
  extensions
    | { _ : { version | String, enabled | Bool | default = true, source | String | optional } }
    | default = {},
}
