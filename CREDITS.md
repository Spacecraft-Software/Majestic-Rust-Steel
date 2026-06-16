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

## Planned upstream dependencies (itemized as each is integrated)

The scaffold is dependency-free. Each crate below is added in the milestone noted, only
after `cargo audit`, with its SPDX metadata recorded and (where §13.3 applies) its entry
filled in here:

| Dependency | License | Milestone | Role |
|---|---|---|---|
| `alacritty_terminal` | Apache-2.0 | M1 | Integrated terminal VT engine (majestic-term). |
| Steel | Apache-2.0 / MIT | M1 | Embedded Scheme extension runtime (majestic-steel). |
| Nickel (`nickel-lang-core`) | MIT | M1 | Manifest evaluation (majestic-config). |
| tree-sitter (+ grammars) | MIT | M1/M2 | Incremental parsing / highlighting. |
| `blake3` | CC0-1.0 / Apache-2.0 | M0 | Hashline line tags. |
| `cosmic-text` | MIT | M4 | GPU text shaping (Nova). |
| Material Symbols (icon font) | Apache-2.0 | M4 | GUI iconography, vendored in-tree (no runtime fetch). |

---

*— Built by Spacecraft Software —*
