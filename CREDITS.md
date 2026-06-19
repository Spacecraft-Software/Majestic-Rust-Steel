<!--
SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
SPDX-License-Identifier: CC-BY-SA-4.0
-->

# Credits

Majestic stands on prior art and (as milestones land) third-party software. This file is
the human-readable counterpart to the mechanical SPDX/REUSE metadata (Standard §13.3): who,
what, and how their work shaped Majestic. SPDX headers and `LICENSES/` cover the legal
mechanics; this file covers the narrative.

## Guidelines & standards applied

| Name | Author(s) | License | Scope |
|---|---|---|---|
| Microsoft Pragmatic Rust Guidelines | Microsoft Corporation | MIT | Governs all Rust in this repo (style, safety, API design) via the `microsoft-rust-guidelines` skill. |
| The Steelbore Standard | Mohamed Hammad / Spacecraft Software | CC-BY-SA-4.0 | The umbrella compliance standard (licensing, palette, posture, signing, dates). |

## Design lineage (prior art whose insights shaped the architecture)

These are **not** dependencies — Majestic is written clean-room — but their published
designs and ideas directly informed it:

| Work | Influence on Majestic |
|---|---|
| **Zed** (Zed Industries) | SumTree rope, immutable snapshots, the dual foreground/background executor pattern (Stratum, Morpheus). |
| **GNU Emacs / Doom Emacs / Spacemacs** | The live-image, infinitely-hackable editor ideal; keybinding-profile design; the Doom `init.el`/`config.el` config split (Keymaker, the Nickel+Steel split). |
| **msedit / `edit`** | Cell-framebuffer diff rendering discipline; the single `sys` platform seam (Penumbra). |
| **xi-editor / Xile** | Front-end/back-end and snapshot-cache thinking (informs Concept #3; recorded here for the portfolio). |
| **oh-my-pi** | Edit-format benchmark fixtures and the `str_replace`/`apply_patch` failure analysis behind the **hashline** primitive. |

## Integrated dependencies (substantially built upon, §13.3)

| Name | Author(s) | License | Source | Scope |
|---|---|---|---|---|
| `alacritty_terminal` | Alacritty contributors (Christian Dürr et al.) | Apache-2.0 | <https://github.com/alacritty/alacritty> | The VT engine and cell grid embedded by `majestic-term` (the integrated terminal) — escape parsing, the terminal grid, and scrollback. Integrated M1. |
| `tree-sitter` + `tree-sitter-highlight` | Max Brunsfeld and tree-sitter contributors | MIT | <https://github.com/tree-sitter/tree-sitter> | The incremental parser and highlight engine behind `majestic-core`'s `SyntaxHighlighter` — parses a buffer and emits the capture events Majestic maps onto theme-styled span layers. Integrated M1. |
| `tree-sitter-rust` (grammar) | tree-sitter contributors | MIT | <https://github.com/tree-sitter/tree-sitter-rust> | The Rust grammar and highlight query driving `.rs` syntax highlighting. Integrated M1. |
| `tree-sitter-{python,go,c,bash,json}` (grammars) | tree-sitter contributors | MIT | <https://github.com/tree-sitter> | Additional language grammars + highlight queries driving `.py`/`.go`/`.c`/`.h`/`.sh`/`.json` highlighting. Integrated M1. |
| `tree-sitter-{nix,scheme,elixir,erlang,powershell,typescript}` (grammars) | respective grammar authors | MIT / Apache-2.0 | <https://github.com/tree-sitter> & community | Grammars + highlight queries for `.nix`, `.scm`/`.ss` (Guile/Scheme), `.ex`/`.exs`, `.erl`/`.hrl`, `.ps1`/`.psm1`/`.psd1`, `.ts`/`.tsx`. Integrated M1. |
| `syntect` | Tristan Hume & contributors | MIT | <https://github.com/trishume/syntect> | The broad "regex tier" of the hybrid highlighter (PRD §5.7): parses `.sublime-syntax` definitions on the pure-Rust `regex-fancy` backend. Integrated M1. |
| `two-face` (+ bat's `.sublime-syntax` set) | 314eter & contributors; syntax authors via bat | MIT OR Apache-2.0 (crate); bundled syntaxes are mixed upstream licenses | <https://github.com/CosmicHorrorDev/two-face> | Supplies bat's extended ~150-language syntax set to syntect, giving the broad tier its reach as data (no compiled grammar per language). Per-syntax upstream licenses are enumerated by `two_face::acknowledgement` (§4.2). Integrated M1. |
| `nickel-lang-core` | Nickel contributors (Tweag) | MIT | <https://github.com/tweag/nickel> | The Nickel language evaluator behind `majestic-config` — evaluates the user manifest (merged onto a schema contract) and deserializes it into the typed settings. Integrated M1. Its tree carries weak-copyleft/unmaintained transitives accepted per `deny.toml` (see `license/ALLOWED_LICENSES.md`) and raises the workspace MSRV to 1.90. |
| `steel-core` (Steel) | Matthew Paras and Steel contributors | Apache-2.0 / MIT | <https://github.com/mattwparas/steel> | The embedded Scheme VM behind `majestic-steel` — runs the user's `config.scm` with the fault-isolated `(majestic …)` API. Integrated M1. Clean tree (no new licenses/advisories; MSRV ≤ 1.90). |

The `crossterm` crate (MIT, <https://github.com/crossterm-rs/crossterm>) provides the `mj`
binary's terminal raw mode, input decoding, and screen control. It is a routine dependency
surfaced mechanically via Cargo, noted here only for transparency.

## Planned upstream dependencies (itemized as each is integrated)

The scaffold is dependency-free. Each crate below is added in the milestone noted, only
after `cargo audit`, with its SPDX metadata recorded and (where §13.3 applies) its entry
filled in here:

| Dependency | License | Milestone | Role |
|---|---|---|---|
| `blake3` | CC0-1.0 / Apache-2.0 | M0 | Hashline line tags. |
| `cosmic-text` | MIT | M4 | GPU text shaping (Nova). |
| Material Symbols (icon font) | Apache-2.0 | M4 | GUI iconography, vendored in-tree (no runtime fetch). |

---

*— Built by Spacecraft Software —*
