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

  # Seraph agent-governance policy (PRD #1 §5.2.4) — declarative and FAIL-CLOSED. Every omitted field
  # takes its closed default: empty `network_allowlist` / `shell_allowlist` means no network and no
  # shell; `edits_need_approval` true means the agent can never apply an edit the user did not see.
  # The user widens these explicitly (e.g. allow-listing program names); extensions may only tighten.
  seraph
    | {
      network_allowlist | Array String | default = [],
      shell_allowlist | Array String | default = [],
      edits_need_approval | Bool | default = true,
      isolate_filesystem | Bool | default = false,
    }
    | default = {},

  # Architect agent provider (PRD #1 §5.2.3 / §9) — LOCAL-FIRST. `model` and `base_url` select the
  # OpenAI-compatible backend (a local Ollama server by default). The API key is deliberately NOT
  # here: it comes from the environment (`MAJESTIC_AGENT_KEY`), never the manifest (§9). The agent's
  # shell/network permissions live in the `seraph` policy above.
  agent
    | {
      model | String | default = "qwen2.5-coder",
      base_url | String | default = "http://localhost:11434/v1",
    }
    | default = {},
}
