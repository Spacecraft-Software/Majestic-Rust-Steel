<!--
SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
SPDX-License-Identifier: CC-BY-SA-4.0
Document class: specification/roadmap (Steelbore Standard §4.1.1 — documents default to CC-BY-SA-4.0)
-->

# Majestic — Concept #1 (Rust + Steel): Understanding & Bootstrap Roadmap

| Field | Value |
|---|---|
| **Document** | Founding understanding + roadmap for the Opus clean-room build of Concept #1 |
| **Project** | Majestic (binary: `mj`) — Spacecraft Software |
| **Concept** | C1 — Pure Rust + Steel (PRD #1, the Product-Contract carrier) |
| **Approach** | Fresh clean-room, authored from the PRDs (not derived from any prior bootstrap) |
| **Date** | 2026-06-16 |
| **Author / Maintainer** | Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org> |
| **Project URL** | https://Majestic.SpacecraftSoftware.org/ |
| **Software license (project)** | GPL-3.0-or-later (Steelbore Standard §4.1 — local app, not network-facing) |
| **This document's license** | CC-BY-SA-4.0 (§4.1.1) |
| **Governing standard** | The Steelbore Standard v1.18; PRD #1 v1.0.0 (Approved for implementation) |
| **Status** | Understanding established; implementation deferred to a separate approved phase |

> **Scope of this document.** This is a *no-code* deliverable. It proves end-to-end
> understanding of Majestic and lays out an executable, contract-anchored build order for
> the fresh C1 implementation. No Cargo workspace, crates, posture files, or Rust are
> created here — those are gated behind a later approval (see §8, Out of Scope).

---

## 0. Orientation — one product, five concepts, this is #1

Majestic is **one application** that is simultaneously its owner's *terminal, editor, and
coding agent*, TTY-first, in the spirit of "basically my own Emacs." It is specified once
as an invariant **Product Contract** (PRD #1 §5) and then realized as **five** substrate
implementations that compete in a deliberate bake-off:

| Concept | Repo | Lineage | Extension lang | Engine code | Signature strength |
|---|---|---|---|---|---|
| **C1 — Rust + Steel** | `majestic` | Zed / msedit | Steel (Scheme) | native crates | smallest, fastest |
| C2 — Pure Guile | `majestic-guile` | Emacs / Schemacs | Guile (native) | pure Scheme | purest live image |
| C3 — RMS + Guile | `majestic-rms` | xi / Xile / Zed | Guile (client) | **C1 crates** | strongest isolation |
| C4 — BEAM + NIFs | `majestic-beam` | Erlang / OTP | Elixir | **C1 crates as NIFs** | broadest recovery |
| C5 — Clojure/JVM | `majestic-clj` | Clojure / nREPL | Clojure | JVM-native | atomic multi-file edit |

**Why C1 is the right first build:** it carries the Product Contract; its engine crates
(`stratum`, `penumbra`, `seraph`, `majestic-term`, `morpheus`, `majestic-config`) are
*reused outright* by C3 and *wrapped as NIFs* by C4. Hardening C1 heals three of the five
concepts. C1 also has the most aggressive performance budgets (§3), making it the
reference others are measured against.

The daily-driver bar is deliberately early — **reliable CUA editing + an integrated
terminal at the end of M1.** AI (Architect/Seraph) is deliberately late (M3), after the
buffer model has hardened.

---

## 1. What Majestic *is* — the INVARIANT Product Contract (PRD #1 §5)

An implementation conforms to "Majestic" iff it satisfies every clause below. This is the
shared spine across all five concepts; C1 implements it in Rust + Steel.

### 1.1 Three interfaces, in priority order
- **TUI (Penumbra) — primary, *is* the product.** Full-screen terminal UI working on the
  bare Linux console, every mainstream emulator, and over SSH (`emacs -nw`/`msedit` class).
- **CLI line-editor mode (M4)** — `ed`-style non-visual surface for agentic/scripted use,
  governed by the Spacecraft Dual-Mode CLI Standard (SFRS): `--json`, structured errors,
  agent env-var detection.
- **GUI (Nova, M4)** — wgpu-rendered; same document model, keymaps, config. In GUI mode the
  embedded terminal makes Majestic a standalone terminal emulator.

### 1.2 Subsystem contracts
- **Keymaker** — persistent **prefix-tree keymaps**, rebindable at runtime without
  disturbing in-flight dispatch (every rebind yields a new keymap value structurally
  sharing with the old). First-run profile selector `[E]macs / [V]im / [C]UA / [S]pacemacs`;
  layered resolution `buffer-local → minor-mode → major-mode → global`. CUA is the M0
  default; Vim is modal; Spacemacs is Evil + `SPC` leader with which-key hints. Satisfies
  Standard §8 (CUA + Vim) by construction.
- **Oracle** — live help & introspection (`describe-key/-function/-variable/-mode/-bindings`,
  `apropos`) reading the **same live registries** the running image uses (never stale).
  Help buffers hyperlink into the running Steel image. **Docstring-at-registration is
  mandatory — undocumented registration is a CI lint failure.**
- **Architect (M3)** — Warp-like NL terminal surface: run internal/OS commands, converse
  with a governed AI that proposes edits, drive the extension REPL, live-hack Majestic.
  Quake drop-down (`F12` / `Ctrl+~` / `Super+~`). Context-aware (buffers, cwd, project,
  config/API) subject to Seraph. Provider-agnostic, local-first.
- **Seraph (M3) — the mandatory AI guardrail.** *Invariant: no agent-proposed side effect
  reaches a document or the outside world without passing through Seraph; there is no
  bypass path.* Edit approval as a unified diff (`Apply / Edit / Reject`, default ON); tool
  sandboxing; rate limiting; prompt sanitization; **append-only, hash-chained, UTC-Z,
  local-only audit log**; a `agent-stop-all` **kill switch reachable from any state in every
  profile**. **Seraph policy is declarative-only — defined in the Nickel manifest, never in
  Turing-complete extension code; extensions may only *tighten* it.** Fails **closed**.
- **Hashline (§5.2.5) — the agent edit primitive.** Agent reads return every line as
  `LINE:TAG│text`, where `TAG` is **BLAKE3 over the line's normalized bytes truncated to a
  2-char base32 tag** (widened only for in-file collisions; **agent-facing only**, never in
  the human UI). Agents edit *by reference* (`replace 2:f1 …`, `insert-after 3:0e …`,
  `delete 2:f1`, ranges) rather than re-emitting whitespace-perfect text. **Stale-tag
  rejection is a Seraph pre-approval gate** — if any referenced tag no longer matches the
  live buffer, the edit is rejected *before the diff renders*, closing the
  read→propose TOCTOU window by construction. `str_replace` and whole-file rewrite remain
  as **demoted fallbacks** (the reverse of current-gen harnesses).

### 1.3 Core editing contract
- **Stratum** — persistent/CoW text structure: O(log n) edits, cheap immutable snapshots,
  position **anchors** that survive edits, interval-tagged **spans** (highlight,
  diagnostics, marks).
- **Undo** — branching undo *tree*, persisted across sessions.
- **Crash safety** — append-only edit journal; Emacs-style recovery on restart after
  abnormal exit. Acceptance: induced `SIGKILL` mid-edit loses **at most the journal-flush
  window (default ≤ 1 s of keystrokes)**.
- **Encodings** — UTF-8 native; correct grapheme-cluster cursor motion & width; large files
  (≥ 1 GB) open without blocking the UI.
- **Windowing** — splits (H/V), tabs, window tree (Zellij-grade ergonomics).
- **Sessions** — named sessions; daemon mode (M2) for detach/attach across TTYs and instant
  client startup; state survives detach and restart.

### 1.4 Configuration contract (hybrid, Doom-style split)
| Layer | File | Language | Role |
|---|---|---|---|
| **Manifest** | `~/.config/majestic/majestic.ncl` | **Nickel** | Declarative, contract-validated: enabled modules (pinned), keybinding profile, theme, **Seraph policy (only home)**, daemon opts. |
| **Config** | `~/.config/majestic/config.scm` | **Steel** | Imperative, live-reloadable: hooks, custom commands, mode tweaks, personal functions. |

Invalid manifest ⇒ **safe mode** (last-known-good or built-in defaults), violation
reported; *a config error must never prevent opening files.* `mj config check` validates
both layers and exits non-zero on violation (CI-friendly).

### 1.5 Extension, theming & iconography contracts
- **Extension** — language-neutral public API (buffer ops, keymap ops, hooks, commands, UI
  primitives, Oracle registration) so plugin *logic* ports across concepts. Hooks (min):
  file open/save pre/post, mode activation, buffer create/kill, focus change, before/after
  command dispatch. Live REPL; user-level redefinition takes effect without restart.
- **Theming (§9/§9.1)** — default named **`Steelbore`** theme exposing exactly six tokens:
  `background #000027` (Void Navy, mandatory), `foreground #D98E32`, `accent #4B7EB0`,
  `success #50FA7B`, `error #FF5C5C`, `info #8BE9FD`. **No bare hex literals in UI logic;**
  all color goes through theme tokens. WCAG 2.1 AA verified per shipped pairing.
- **Iconography (§5.6.1)** — icons are **semantic tokens** (`icon :folder-open`,
  `icon :git-branch`, …), never literal glyphs. One vocabulary → three targets: **TUI
  default** plain Unicode (renders on the bare console), **TUI optional** Nerd Font PUA
  glyphs (only when `majestic.ncl` declares a Nerd Font), **GUI/Nova (M4)** Material Symbols
  **vendored in-tree, never CDN-fetched** (Apache-2.0; `CREDITS.md`).

---

## 2. Concept #1 architecture — Rust + Steel (PRD #1 §6)

### 2.1 Toolchain & governing skills
- **Rust, stable toolchain only** (no nightly). MSRV pinned in `Cargo.toml`, CI-enforced.
- **All Rust governed by `microsoft-rust-guidelines` (mandatory gateway) + Spacecraft Rust
  guidelines.** `#![deny(unsafe_code)]` workspace-wide; any exception is a documented,
  reviewed, narrowly-scoped module.
- **Steel** (Scheme written in Rust) is the embedded extension language → toolchain stays
  100% Rust. **Nickel** via `nickel-lang-core`.
- Lint gates (all CI-blocking): `cargo clippy -D warnings`, `cargo fmt --check`,
  `cargo audit`, `cargo-deny` (license + advisory), `reuse lint`.

### 2.2 Workspace layout (embeddable cores, Alacritty-style)
```
majestic/                       # Cargo workspace — GPL-3.0-or-later, REUSE-clean
├── crates/
│   ├── stratum/                # Text core: rope, snapshots, anchors, spans, undo tree, journal
│   ├── penumbra/               # TTY renderer: crossterm + ratatui, framebuffer diff
│   ├── nova/                   # GPU renderer: wgpu + cosmic-text (M4; stub until then)
│   ├── keymaker/               # Persistent prefix-tree keymaps, profiles, dispatch
│   ├── oracle/                 # Introspection registries + help rendering
│   ├── seraph/                 # Policy engine, diff-approval, sandbox broker, audit log
│   ├── architect/              # Agent loop, provider abstraction, quake surface (M3)
│   ├── morpheus/               # Executors (fg/bg), task, event bus, deterministic test executor
│   ├── majestic-term/          # Terminal widget wrapping alacritty_terminal
│   ├── majestic-config/        # Nickel manifest schema + Steel config loader, safe mode
│   ├── majestic-steel/         # Steel runtime embedding + extension API surface
│   ├── majestic-core/          # Buffers, windows, modes, commands, sessions (composes above)
│   └── majestic-cli/           # SFRS CLI plumbing (--json, errors, agent detection)
├── mj/                         # The thin binary crate
├── extensions/                 # First-party Steel extensions (shipped, pinned)
├── themes/steelbore.*          # §9.1 named theme (canonical)
├── i18n/                       # Localization (msedit-style single source)
├── LICENSES/  README.md  NOTICE.md  CONTRIBUTING.md  CREDITS.md  REUSE.toml
```
`stratum`, `penumbra`, `keymaker`, `majestic-term` are designed as **reusable library
crates** (Loran/Ferrocast/Caliper may embed them; C3/C4 reuse them) and follow semver
independently of the `mj` binary after 1.0.

### 2.3 Stratum — the text core
- **Structure:** CoW rope over a **SumTree-style B-tree** (Zed lineage) — chunks with
  summaries (bytes, chars, lines, UTF-16 units) → O(log n) edit/index/coordinate
  conversion.
- **Snapshots:** `snapshot()` is an `Arc` bump — immutable, `Send`, shareable with
  background threads with **zero locking**. All search/highlight/LSP-sync/agent-read run
  against snapshots, never the live buffer.
- **Anchors:** left/right-bias positions surviving concurrent edits (marker semantics).
- **Spans:** interval-keyed metadata layers (syntax captures, diagnostics, selections,
  Seraph pending-diff regions) in the same summarized-tree discipline.
- **Undo:** branching tree; nodes reference edit-journal offsets; persisted per file;
  `undo`/`redo`/`undo-tree-visualize`.
- **Journal:** append-only WAL; fsync policy configurable (default ≤ 1 s or 64 ops);
  recovery replays journal onto last save.
- **Hashline tags computed here**, directly from chunk summaries; tagged edits resolve
  through the anchor machinery (positions that already survive concurrent edits).
- Penumbra MAY use `memchr` SIMD for cold-path line scans where profiling justifies (the §3
  benchmarking rule applies).

### 2.4 Morpheus — concurrency model (Zed-pattern, TUI-adapted)
Implements Standard §3.2 (concurrency designed-in):
- **Two executors:** a `ForegroundExecutor` (UI thread — input dispatch, state mutation,
  frame composition; *the main thread is holy*) and a `BackgroundExecutor` (work-stealing
  pool sized to cores) on `smol`/`async-task`. **No tokio in the editor core.**
- **`Task<T>` drop-cancellation:** dropping a task cancels it; owners hold handles so
  closing a view/buffer cancels its in-flight work; `.detach()` is the explicit escape.
- **Snapshot ping-pong:** heavy work gets a Stratum snapshot on a background thread, streams
  results back over bounded channels (drop-receiver = cancellation).
- **Event bus:** subsystem events drained at one point per frame — run-to-completion, no
  reentrant mutation.
- **Deterministic test executor:** seedable single-thread executor for property-testing
  concurrent code; failing seeds reproduce. **Shipping a concurrency feature without
  deterministic tests is a review blocker.**
- Locks: `parking_lot` only; *clone the Arc on the foreground, lock only on the background.*

### 2.5 Penumbra — TTY renderer (the primary interface)
- **Stack:** `crossterm` (raw mode, input decode, capability queries) + `ratatui` widgets,
  rendered through a **cell framebuffer with frame diffing** (msedit discipline): draw the
  whole logical frame immediate-mode, diff vs. previous, emit minimal VT — efficient over
  SSH by construction.
- Kitty keyboard protocol where available (graceful fallback); mouse; bracketed paste; OSC
  52 clipboard.
- **Renderer parity rule:** Penumbra and Nova must produce logically identical layouts for
  the same document state (shared layout layer; divergence is a bug class).
- **UI layout** (from `UI.md`, VS Code / Antigravity translated to TTY, all Steelbore
  palette, Steel-Blue box-drawing borders): Activity bar (3–4 cols) → Explorer sidebar
  (20–35) → Editor area (tabs + gutter + syntax-highlighted content, splits) → Architect
  sidebar (25–35, toggleable, `Ctrl+Shift+A`) → bottom panel (Terminal | Problems | Output,
  8–15 rows) → status bar (git branch, language, Ln/Col, encoding, agent status). Command
  palette `Ctrl+Shift+P`. Target 120–160 cols × 40–60 rows, responsive.

### 2.6 majestic-term, majestic-steel, daemon, IDE, CLI
- **majestic-term (M1):** embeds **`alacritty_terminal`** (Apache-2.0; `CREDITS.md`) — PTY
  spawn, VT parsing, grid state; surfaced as a buffer-like widget in the window tree; copy
  mode; scrollback (default 10 000); OSC 7 cwd tracking. Default shell `$SHELL`
  (**Nushell/Ion first-class, verified in CI**). In Nova it becomes a standalone GPU term.
- **majestic-steel (M1→M3):** Steel VM in-process; API as Steel modules `(majestic buffer)`,
  `(majestic keymap)`, `(majestic hook)`, `(majestic command)`, `(majestic ui)`,
  `(majestic oracle)`. Registration requires docstrings (Oracle reads the live env).
  Live-hacking REPL (`M-:` and the Architect). **Containment: a Steel error never crashes
  the editor** (fault-isolated, surfaced in a diagnostics buffer). **Seraph boundary:**
  agent-invoked Steel runs with a *restricted effect table*; direct user REPL is
  unrestricted (*the user is sovereign; the agent is not*).
- **Daemon & sessions (M2):** `mj --daemon` headless server over a Unix socket (0700);
  attach ≤ 10 ms; multiple mirrored clients (Zellij-style detach/move-across-TTYs); session
  resurrection. **Local-only — no TCP listener in v1** (keeps GPL-vs-AGPL & PFA trivial).
- **IDE (M2):** LSP client (Helix-grade), tree-sitter incremental parsing on background
  snapshots, fuzzy finder + file tree as built-in extensions.
- **CLI (SFRS):** `mj [FILE...]`; subcommands noun-verb, dual-mode, `--json`:
  `mj config check`, `mj session list|attach|kill`, `mj describe key|function <name>`,
  `--version`/`--help` with §13.2 attribution. Agent env-var detection (`AI_AGENT`,
  `CLAUDECODE`, …) switches to machine-friendly output. `mj ed` line mode at M4.

---

## 3. Non-negotiable guardrails (Steelbore Standard + PRD #1 §7)

### 3.1 Priority hierarchy (Standard §3 — a higher number may never compromise a lower one)
1. **Stability.** Memory safety (Rust) is the primary lever; *plus* robust error handling
   (**no `unwrap`/`expect`/`panic!` on fallible paths** — lint-enforced), fault tolerance /
   graceful degradation, crash-safe persistence, and test-verified stability (unit +
   integration + property + fuzz) gating CI.
2. **Performance** — multi-core, multi-thread concurrency **designed in from the start**;
   benchmarks **mandatory** before/after optimization; documented serial fallbacks where
   concurrency would hurt.
3. **Hardened Security** — sandboxing/privilege separation for anything touching network or
   agent-requested actions; PQC readiness (ML-KEM-768 / ML-DSA-65 migration paths) for any
   future crypto surface; `cargo-audit` before any third-party crate.
> **Cardinal Rule:** any optimization that weakens stability (incl. memory safety) or
> security is rejected — no exceptions.

### 3.2 Performance & stability targets (PRD #1 §7; reference T490s, release build)
| Target | Budget | Verification |
|---|---|---|
| Cold start → first paint (TUI, no daemon) | ≤ 50 ms | CI hyperfine, regression-gated |
| Daemon attach → interactive | ≤ 10 ms | CI benchmark |
| Keypress → screen update, p99 | ≤ 16 ms (one frame) | latency harness |
| Open 1 GB file | UI interactive < 100 ms; no main-thread block | integration test |
| Sustained typing in 100 MB file w/ tree-sitter | no dropped input, p99 ≤ 16 ms | property/latency test |
| Scroll full-page in highlighted buffer | ≤ 16 ms/frame | benchmark |
| Crash data loss (SIGKILL mid-edit) | ≤ 1 s of keystrokes | kill-test in CI |
| Memory, idle, 10 buffers + terminal | ≤ 150 MB RSS | CI check |
| Binary size (`mj`, stripped, release) | ≤ 25 MB target / 40 MB ceiling | CI check |

Stability gates: zero clippy warnings; lint-enforced no-panic-on-fallible; fuzzing on
Stratum edit ops, VT input path, Nickel/Steel loaders; deterministic-executor property
tests for every concurrent subsystem; `cargo audit`/`cargo-deny` clean.

### 3.3 Compliance surface (Standard §4/§5/§6/§7/§9/§12/§13)
- **§4 Licensing/REUSE:** GPL-3.0-or-later; **two-tag SPDX header on every file**
  (`SPDX-FileCopyrightText` + `SPDX-License-Identifier`) or `.license`/`REUSE.toml`;
  `LICENSES/` with verbatim texts (incl. upstream per §4.2); `reuse lint` CI-clean.
- **§5 Posture:** Personal/Hobby (default); ship `README.md` (with Posture section),
  `NOTICE.md`, `CONTRIBUTING.md`, `LICENSES/` from `/spacecraft-software/license/` templates.
- **§6.1 POSIX** CLI/system surface; **§6.3 signed & verified commits** — every commit
  Ed25519-SSH-signed by the `Mohamed.Hammad@SpacecraftSoftware.org` signing key, "Verified"
  on GitHub; programmatic/assistant commits included; rewrites preserve signatures.
- **§7 PFA:** zero telemetry (not even opt-in), minimal permissions requested lazily, local
  storage by default.
- **§9 Steelbore theme** (no bare hex); **§10** FOSS fonts only — Share Tech Mono (chrome) +
  Inconsolata (body/code) in Nova; **§11** WCAG 2.1 AA.
- **§12 Time:** ISO 8601 / 24-hour / **UTC Z** everywhere (logs, journal, audit, sessions);
  Rust uses **`jiff`** (serialize `…Z` strings; never `NaiveDateTime`/local in output).
- **§13 Attribution** in `--version`/`--help`/README/About; **`CREDITS.md`** for
  substantial third-party work — triggers at minimum: `alacritty_terminal`, Steel, Nickel,
  tree-sitter, `blake3` (hashline), `cosmic-text` (Nova), Material Symbols (vendored, Nova).

---

## 4. Bootstrap roadmap — milestone ladder mapped to crate build order

The M0–M4 ladder, exit criteria, and **daily-driver line at end of M1** are the PRD #1 §8
invariant capability ladder. Below, each milestone is sequenced into a concrete,
dependency-ordered build order for a fresh implementation.

### M0 — Keel (foundation; boring on purpose)
*Goal:* reliable single-window CUA editing of UTF-8 files of any size, crash-safe.
**Build order:**
1. **Scaffold:** Cargo workspace (14 crates per §2.2 + `mj`), MSRV pin, `#![deny(unsafe_code)]`;
   posture files + `LICENSES/` + `REUSE.toml`; CI gates (clippy/fmt/audit/deny/reuse + the §3.2
   benchmark + kill-test harness); `themes/steelbore.*`; signed-commit tooling.
2. **`stratum`:** rope → snapshots → anchors → spans → branching undo tree → WAL journal +
   recovery. Property tests + fuzz on edit ops from day one.
3. **`morpheus`:** fg/bg executors + bounded channels + event bus + **deterministic seedable
   test executor** (gates every later concurrent feature).
4. **`penumbra`:** cell framebuffer + frame diff + crossterm input decode; single window.
5. **`keymaker`:** prefix-tree keymaps + **CUA profile only**.
6. **`majestic-core`** (compose) + **`majestic-cli`** (minimal SFRS plumbing) → **`mj FILE`**
   opens/edits/saves. (`majestic-cli`'s `--json`/subcommand surface grows M1→M4.)
**Exit:** all §3.2 stability gates pass; SIGKILL test passes; maintainer dogfoods for one
week without data loss.

### M1 — Hull (the daily-driver release) ← **daily-driver line**
`majestic-term` (alacritty_terminal; Nushell/Ion CI; vttest core) · splits/tabs/window tree
· tree-sitter highlighting (initial set: Rust, Go, C, C++, Bash, Nu, Zig, PowerShell, DOS
batch, Windows Registry, `.ini`, Markdown) · fuzzy finder + file tree · `majestic-config`
(Nickel manifest + Steel `config.scm` + **safe mode**) · `majestic-steel` v1 · **Oracle v1**
(`describe-key/-function/-bindings`, `apropos`).
**Exit:** maintainer replaces editor *and* multiplexer for daily work; terminal passes
vttest core; Nushell + Ion verified.

### M2 — Engine
Daemon + sessions (detach/attach, resurrection) · full **Keymaker** (all 4 profiles,
first-run selector, live switching, which-key for Spacemacs) · **LSP client** · remaining
grammars · manifest-pinned extension loading.
**Exit:** profile switch under load loses no keystrokes; LSP parity vs Helix on
rust-analyzer; session survives daemon restart.

### M3 — Bridge (AI, governed)
**Architect** (quake drop-down, NL command line, agent loop) · **Seraph** in full
(diff-approval, sandboxed tools, rate limits, audit log, kill switch, **Nickel-only
policy**) · provider layer (local-first: Ollama / mistral.rs / OpenAI-compatible; BYOK cloud
opt-in) · **hashline** edit tool as default with stale-tag pre-approval gate.
**Exit:** red-team suite shows **no Seraph bypass (incl. via Steel)**; audit log
reconstructs every agent side effect; `agent-stop-all` ≤ 100 ms; hashline stale-tag
rejection verified; edit-format benchmark shows **hashline ≥ `str_replace`** across
providers.

### M4 — Fleet
**Nova** (wgpu GUI; renderer-parity tests; vendored Material Symbols) · Markdown/GFM + notes
layer · Magit-role Git (status/stage/commit/log/diff) · **`mj ed`** line-editor (hashline as
primary edit interface).
**Exit:** GUI passes the same latency harness; Penumbra↔Nova parity suite green.

*Post-1.0 (recorded direction):* Windows Tier-3, Redox exploration, collaborative-editing
investigation, PRDs #2–#5 conformance runs.

---

## 5. Key risks & decisions to front-load (PRD #1 §13)

| Risk | Mitigation (decide early) |
|---|---|
| **Steel maturity** vs Emacs-grade extensibility | Wrap Steel behind the `majestic-steel` API boundary (swap-capable); M1 needs only config+hooks; full surface matures M2–M3; upstream contributions. |
| **Scope gravity** ("Emacs OS" pull) | Milestone exit criteria are contractual; daily-driver line at M1; the §10 non-goals list is enforced in review. |
| **ratatui/crossterm latency** vs 16 ms p99 | Framebuffer-diff layer owns the hot path; §3.2 benchmarking decides any replacement — measure, don't assume. |
| **Seraph bypass** via extension/prompt injection | Effect-table containment for agent-invoked Steel; fail-closed; red-team suite is an M3 *exit criterion*. |
| **Terminal correctness long tail** | Embed `alacritty_terminal` rather than build; vttest + esctest in CI. |
| Single-maintainer / hobby pace | §5 Personal posture is explicit; reproducible builds + this doc keep the project resumable. |
**Standing rule:** no concurrent subsystem merges without deterministic-executor tests;
MSRV stable-only; benchmark before/after every optimization.

---

## 6. Reference-corpus map (study guide — *learn from, don't copy*)

`/spacecraft-software/majestic/` holds ~50 vendored lineage repos. Clean-room means reading
them for *ideas and pitfalls*, then writing original Rust. Primary correspondences:

| Subsystem | Study | Why |
|---|---|---|
| `stratum`, `morpheus` | **zed** | SumTree rope, snapshots, dual-executor `Task<T>` pattern (the direct lineage). |
| `penumbra` | **edit** (msedit), **ratatui**, **rio/wezterm/alacritty** | Framebuffer-diff discipline; VT emission; widget patterns. |
| `majestic-term` | **alacritty** (`alacritty_terminal`), **cosmic-term** | The embedded VT engine (actually depended on, Apache-2.0). |
| `keymaker` | **doomemacs**, **spacemacs**, **magit** | Profile binding sets; which-key; Evil/`SPC` leader; layered resolution. |
| `oracle` | **doomemacs**, GNU **gnu_zile/mg/nano** | `describe-*` ergonomics; live introspection. |
| `nova` (M4) | **cosmic-text**, **cosmic-edit**, **zed** | GPU text shaping/rendering; FOSS font stack. |
| `architect` | **rig**, **mistral.rs**, **goose/cline/opencode/codex/claude-code/warp/waveterm** | Provider abstraction; agent-loop & tool-use UX; quake/NL terminal. |
| `seraph` + **hashline** | **oh-my-pi** | Edit-format benchmark fixtures; `str_replace`/`apply_patch` failure modes hashline fixes. |
| C3 cross-ref (not C1) | **xile** | Guile front-end / Rust back-end split — informs PRD #3, not this build. |
| AI-agent infra patterns | **agents, agent-orchestrator, crewAI, claude-task-master, anda** | Orchestration/task patterns for the M3 agent loop. |

---

## 7. Conformance gate (PRD #1 §15 + Standard §14) — standing audit instrument

Every milestone is measured against this cross-walk. (✓ = addressed in this roadmap; the
build phase must keep each green.)

| Contract clause | Where it lands |
|---|---|
| Three interfaces present/scheduled; TUI primary | Penumbra M0/M1; CLI/Nova M4 (§1.1, §2.5, M4) |
| Keymaker: 4 profiles, first-run selector, layered live rebinding | CUA M0 → full M2 (§1.2, M2) |
| Oracle: 6 describe/apropos read live registries; docstring lint | Oracle v1 M1; lint from M1 (§1.2, §2.6) |
| Architect: quake, context-aware, provider-agnostic, local-first | M3 (§1.2, M3) |
| Seraph: invariant under red-team; Nickel-only policy; fail-closed; kill switch everywhere | M3 exit (§1.2, M3) |
| Hashline: tagged-line default; stale-tag pre-approval gate; demoted fallbacks | tags in `stratum` M0; gate M3 (§1.2, §2.3) |
| Stratum: snapshots, anchors, branching persisted undo, journal recovery | M0 (§1.3, §2.3) |
| Config: Nickel manifest + Steel config; safe-mode startup | M1 (§1.4, §2.6) |
| Steelbore theme default; no bare hex; WCAG AA; semantic iconography | theme M0; icons per target (§1.5, §3.3) |
| Language/format set; ISO 8601 / UTC Z throughout | grammars M1/M2; `jiff` from M0 (§2.6, §3.3) |
| Standard §14 (license, REUSE, posture, PFA, CUA+Vim, attribution, signed commits) | scaffold M0 (§3.3) |
| §7-equivalent perf/stability table CI-gated | M0 CI (§3.2) |
| Daily-driver bar: CUA editing + integrated terminal | end of M1 (M1) |

---

## 8. Out of scope (deferred to a separate, approved code phase)

No Cargo workspace, crate code, posture/REUSE/CI files, theme module, or any `.rs` are
created by this document. When the build phase is approved:
1. **Load `microsoft-rust-guidelines` (mandatory gateway) + `spacecraft-rust-guidelines`
   before writing any Rust.**
2. Begin at **M0 step 1 (scaffold)** in §4; derive posture files from
   `/spacecraft-software/license/`.
3. Keep every commit signed/verified (§6.3) and `reuse lint`-clean from the first commit.
4. Treat §7's cross-walk as the running audit gate.

---

*Maintained by Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>*
*Copyright (C) 2026 Mohamed Hammad & Spacecraft Software | Document license: CC-BY-SA-4.0*
*https://Majestic.SpacecraftSoftware.org/*

*— Built by Spacecraft Software —*
