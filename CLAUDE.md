<!--
SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
SPDX-License-Identifier: CC-BY-SA-4.0
-->

# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

The active **Concept #1** build of **Majestic** — a TUI-first terminal, editor, and governed AI coding-agent (binary: `mj`), written in **Rust with an embedded Steel (Scheme)** extension layer. This is a Cargo workspace and the contract-carrier for the five-concept product.

- **Full spec & roadmap:** [`MAJESTIC.md`](./MAJESTIC.md) (architecture + M0–M4 milestones).
- **The 5 PRDs / product vision:** the sibling planning workspace [`../majestic/`](../majestic/) (`Majestic-PRD-01-Rust.md` is this build's contract).
- **Before editing any `.rs`:** load the `microsoft-rust-guidelines` skill (mandatory) and `spacecraft-rust-guidelines`.

## Build & run

```bash
cargo build                 # debug, whole workspace
cargo build --release       # optimized (LTO, single codegen-unit, stripped) — use for perf
cargo run -p majestic       # run the mj binary (package `majestic`, in mj/)
cargo run --release -p majestic
cargo test -p <crate> <name>   # single test, e.g. cargo test -p majestic-core buffer
```

**Toolchain:** stable only, **MSRV 1.90.0** (`rust-toolchain.toml`, pinned in `Cargo.toml`, separately CI-gated). No nightly features; `unsafe_code = "deny"` workspace-wide (PRD §6.1 — C1 has zero unsafe).

## Feature flags (highlighting / grammars)

Defined in `crates/majestic-core/Cargo.toml`; `mj/Cargo.toml` forwards them. The lean default keeps the binary under the §7 25 MB target:

- **default** = `grammars-common` (tree-sitter: rust/python/go/c/bash/json) + `syntect-highlighting` (syntect + `two-face`, bat's ~150-language `.sublime-syntax` set as the broad regex tier).
- `--features grammars-extra` (nix/scheme/elixir/erlang/powershell/typescript), `grammars-all`, or any single `lang-*`.
- `--no-default-features` → syntect-only, no compiled tree-sitter grammars.

## Quality gates (all CI-blocking — `.github/workflows/ci.yml`; run before pushing)

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace && cargo test --workspace
cargo deny check            # license + advisory policy (deny.toml)
reuse lint                  # SPDX on every file (see REUSE.toml)
# cargo audit runs in CI via rustsec/audit-check; run `cargo audit` locally too
```

`RUSTFLAGS="-D warnings"` in CI — the workspace must be warning-clean (`Cargo.toml [workspace.lints]`: clippy pedantic/perf/style/correctness + selected restriction lints, `missing_docs = warn`).

## Performance & binary-size harness (§7)

```bash
cargo run --release -p majestic-bench            # report p50/p99/max latencies
cargo run --release -p majestic-bench -- --check # gate: fails if a gated scenario breaches
                                                 # FRAME_BUDGET (16 ms) or COLD_START_BUDGET (50 ms)
```

Known breaches are tracked, not enforced (see the harness output). Debug `--check` reports but does not gate (debug is far slower than the budgets). CI also gates the `mj` release binary at the **40 MB ceiling** (25 MB is the target) via `stat -c%s target/release/mj`.

## Architecture mental model

**Engine crates** (reusable libraries; Concepts #3/#4 reuse these): `stratum` (rope, snapshots, anchors, undo tree, append-only journal) · `penumbra` (TTY framebuffer-diff renderer over crossterm/ratatui) · `nova` (wgpu GPU stub, M4) · `keymaker` (prefix-tree keymaps, profiles, runtime-rebindable) · `oracle` (live help/introspection) · `seraph` (AI guardrail/policy/audit, M3) · `architect` (agent loop, M3) · `morpheus` (foreground/background executors, event bus, deterministic test executor).

**Integration crates:** `majestic-core` (composes the engine into buffers/windows/modes/commands/sessions) · `majestic-term` (alacritty_terminal PTY widget) · `majestic-config` (Nickel manifest + Steel loader + safe mode) · `majestic-steel` (Steel embedding + extension API) · `majestic-cli` (SFRS CLI plumbing). **Binary:** `mj/` (thin; package `majestic`).

**Core data model** (`crates/majestic-core/src/`):
- `Document` (`buffer.rs`) — `Rc<RefCell<>>` shared text: rope + `UndoTree` (branching, persisted) + WAL journal + revision counter.
- `Buffer` (`buffer.rs`) — a *view* over a Document (cursor, selection anchor, goal column). Multiple Buffers share one Document (two-views-of-one-buffer; sibling cursors track edits via `stratum::Anchor`). Clamp-on-access guards a sibling shrinking the doc.
- `Editor` (`editor.rs`) — Buffer + keymap dispatcher + clipboard + viewport + highlighter state.
- `Workspace` (`workspace.rs`) — a **binary window tree**: `Node::Leaf(editor)` | `Split{dir, ratio, first, second}` (nested grids, resizable, one focus).
- Highlighting: structural tree-sitter tier (`syntax.rs`) + regex syntect tier (`syntect_hl.rs`), painted by a **background snapshot worker** (Morpheus) over immutable Rope snapshots (zero-locking).
- Info/Texinfo reader (`info.rs`) — multi-document GNU `.info` reader: `* Menu:` + inline `*note` xrefs, cross-file `(file)Node`; entry point `mj info [topic]`.

## Repo workflow & conventions

- **Signed commits are mandatory** (Standard §6.3): Ed25519 SSH signing (`commit.gpgsign=true`, `gpg.format=ssh`, key registered as a *Signing* key); every commit must show **Verified**. Rewrites preserve signatures.
- **DCO sign-off** on every commit (`git commit -s`) + **Conventional Commits** prefix (`feat:`/`fix:`/`docs:`/`refactor:`/`test:`/`chore:`/`perf:`/`build:`/`ci:`). See `CONTRIBUTING.md`.
- **New work → stacked feature branch → PR** (never direct to `main`). Currently stacked: `feat/document-view-split` → `feat/two-views` → `feat/two-views-anchors`.
- **REUSE/SPDX on every file:** code `GPL-3.0-or-later`, docs/designs `CC-BY-SA-4.0` (two inline tags or a `REUSE.toml` annotation; `reuse lint` must pass).
- **Next milestone: M2 (Engine)** — daemon/sessions (detach/attach), full 4-profile Keymaker (first-run selector + which-key), LSP client, manifest-pinned extension loading.
