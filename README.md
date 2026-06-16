<!--
SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
SPDX-License-Identifier: CC-BY-SA-4.0
-->

# Majestic

**One application — terminal, editor, and coding agent.**

Majestic is a TUI-first, programmable editing environment built in Rust. It serves as your
terminal, your editor, and your coding agent — with your keybindings, extensible from
inside itself while running, and with AI that can never touch a document or the outside
world without explicit, audited consent.

> *"Basically my own Emacs — memory-safe, concurrency-first, and TUI-native."*

This repository is **Concept #1 (Rust + Steel)** — the flagship implementation and carrier
of the cross-implementation Product Contract (PRD #1). It embeds **Steel** (a Scheme
written in Rust) as the live extension language and uses a hybrid **Nickel** manifest +
Steel config. Four sibling concepts (Pure Guile, RMS+Guile, BEAM+NIFs, Clojure/JVM) realize
the same contract on other substrates.

## Status

| Milestone | Status | Description |
|---|---|---|
| **M0 — Keel** | Scaffolding | Foundation: text core (Stratum), TTY renderer (Penumbra), CUA editing, crash-safe journal |
| M1 — Hull | Planned | Daily driver: integrated terminal, splits/tabs, tree-sitter, Nickel + Steel config, Oracle v1 |
| M2 — Engine | Planned | Daemon + sessions, full Keymaker (4 profiles), LSP |
| M3 — Bridge | Planned | Governed AI: Architect surface, Seraph guardrails, hashline edits |
| M4 — Fleet | Planned | GPU GUI (Nova), Markdown/notes, Magit-role Git, `mj ed` line editor |

The architecture and full milestone roadmap live in [`MAJESTIC.md`](./MAJESTIC.md).

## Build

```sh
cargo build              # build the workspace
cargo test               # run tests
cargo run -p majestic -- --version
```

> Scaffold stage: `mj` establishes the `--version` / `--help` surfaces and recognizes the
> planned command set; the editor engine lands across the M0 steps. Once published:
> `cargo install majestic` installs the `mj` binary.

## Configuration

Majestic uses a Doom-style split (lands in M1):

| Layer | File | Language | Role |
|---|---|---|---|
| Manifest | `~/.config/majestic/majestic.ncl` | Nickel | Declarative, validated: modules, keybinding profile, theme, Seraph policy |
| Config | `~/.config/majestic/config.scm` | Steel | Imperative, live-reloadable: hooks, custom commands |

An invalid manifest starts Majestic in **safe mode** — a config error never prevents
opening files.

## Project Posture

Spacecraft Software is a **personal hobby project**. Majestic is developed at hobby pace and
shaped around the maintainer's own use case, not a general audience.

- **No warranty, no liability.** See [`NOTICE.md`](./NOTICE.md).
- **Contributions are welcome but not guaranteed.** See [`CONTRIBUTING.md`](./CONTRIBUTING.md).
- **Forking is encouraged.** GPL-3.0-or-later is there for exactly that.

## License

- **Software** (code, manifests, tooling, themes): `GPL-3.0-or-later`.
- **Documents** (this README and the other `.md` files): `CC-BY-SA-4.0`.
- The project is [REUSE](https://reuse.software)-compliant: every file carries SPDX tags
  (inline or via [`REUSE.toml`](./REUSE.toml)); license texts live in [`LICENSES/`](./LICENSES/).
- Third-party work we build on is credited in [`CREDITS.md`](./CREDITS.md).

## Maintainer

**Mohamed Hammad** &lt;Mohamed.Hammad@SpacecraftSoftware.org&gt;
Copyright (C) 2026 Mohamed Hammad & Spacecraft Software | License: GPL-3.0-or-later
<https://Majestic.SpacecraftSoftware.org/>

---

*— Built by Spacecraft Software —*
