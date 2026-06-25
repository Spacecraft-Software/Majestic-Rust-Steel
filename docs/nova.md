<!--
SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
SPDX-License-Identifier: CC-BY-SA-4.0
Document class: design note (Steelbore Standard §4.1.1 — documents default to CC-BY-SA-4.0)
-->

# Nova — the GPU renderer (M4)

Nova is Majestic's **M4** deliverable: a `wgpu` + `cosmic-text` graphical front end that
draws the *same editor* the TTY already draws, on a GPU-accelerated window. It is the
**secondary** interface — Majestic is TUI-first; Nova is the GUI for users who want one,
and (with the embedded `majestic-term` widget) it doubles as a standalone GPU terminal
emulator (PRD-01 §6.6).

This note is the architecture and the chunk ladder for building it. It is the spine M4
hangs from; each chunk below is a separate signed PR with green gates, the same rhythm
M0–M3 used.

## 1. The parity contract (the whole idea)

PRD-01 §6.5 states the rule Nova exists to honour:

> **Renderer parity rule:** Penumbra and Nova must produce *logically identical layouts*
> for the same document state (shared layout layer; divergence is a bug class).

The **shared layout layer is the `penumbra::Buffer`** — a `width × height` grid of
`Cell { symbol: char, style: Style }`, where `Style { fg, bg, attrs }` and
`attrs ∈ {bold, underline, reverse}`. The editor (`mj`'s `App::render`) draws the whole
frame into a `Buffer` immediate-mode; **Penumbra diffs that buffer and emits VT**, and
**Nova reads the same buffer and emits GPU draw calls.** Same buffer in → same logical
picture out. Anything Nova shows that Penumbra wouldn't (or vice versa) for an identical
`Buffer` is a parity bug, caught by the parity suite (§7, M4.5).

So Nova adds **no layout logic**. It is a *backend* for an already-laid-out frame. This
is what keeps the two renderers honest and is why the first chunk is a pure, testable
translation of `Buffer` → GPU primitives, with the windowing/GPU plumbing layered on top.

```
        App::render(&mut Buffer, &Theme)        ← shared layout layer (no renderer here)
                      │
        ┌─────────────┴──────────────┐
        ▼                            ▼
   penumbra::render              nova::Scene  (this crate)
   (diff → VT bytes)             (cells → quads + glyph runs)
        ▼                            ▼
     a TTY                      wgpu surface  (M4.2) + cosmic-text glyphs (M4.3)
```

## 2. The `Scene` — Nova's renderer-agnostic intermediate

`build_scene(buffer, metrics) -> Scene` is the parity core (chunk M4.1, this PR): a
**pure function**, no GPU, fully unit-testable. It walks the `Buffer` row-major and emits

- one **background quad** per visible cell (a pixel rect filled with the cell's `bg`), and
- one **glyph placement** per non-blank cell (the `symbol`, at the cell's pixel origin, in
  the cell's `fg`, carrying `bold`/`underline`).

with the editor's existing cell semantics preserved exactly:

- `attrs.reverse` swaps `fg`/`bg` (so a reversed cell's quad is the *fg* colour and its
  glyph the *bg* colour — matching how Penumbra renders reverse on a TTY);
- a **double-width** glyph (`penumbra::char_width(symbol) == 2`) emits a 2-cell-wide quad,
  and its **continuation cell** (sentinel `'\0'`) is skipped — the wide quad already
  covers it;
- blank cells (`' '`) emit a background quad but **no** glyph (nothing to rasterise).

`CellMetrics { width, height }` (pixels per cell) is a *parameter* in M4.1; M4.3 derives it
from the chosen monospace font's `cosmic-text` metrics so the grid is pixel-exact.

The GPU pipeline (M4.2) consumes a `Scene`: quads → an instanced-rect pass, glyphs → the
`cosmic-text` atlas pass. Keeping `Scene` a plain data type (not wgpu types) is what makes
the parity suite able to assert Nova's interpretation **without a GPU or a window**.

## 3. Stack

| Layer | Choice | Notes |
|-------|--------|-------|
| Window + input | `winit` 0.30 (`ApplicationHandler`) | the standard Rust windowing layer; its keyboard events map to `keymaker::KeyPress` (M4.4). |
| GPU | `wgpu` (current) | cross-backend (Vulkan/Metal/DX12/GL); the **safe** `Instance::create_surface(&Window)` path (wgpu ≥ 0.19) needs **no `unsafe`** — see §5. |
| Text | `cosmic-text` | shaping + `swash` rasterisation into a glyph atlas; PRD-01 dep list. |
| Icons | **Material Symbols** variable font | Apache-2.0, **vendored in-tree, never fetched at runtime** (§7 PFA); `CREDITS.md` (M4.6). |
| async glue | `pollster` | block on `wgpu`'s async adapter/device request on the one init path. |

## 4. Binary budget & build cost (non-negotiable constraints)

Two hard constraints shape the crate layout:

1. **`mj` must stay ≤ 25 MB (§7).** `wgpu` + `winit` + `cosmic-text` are many MB. Therefore
   **Nova is a *separate binary* (`mj-nova`), never linked into `mj`.** The TTY daily
   driver is unaffected; the GUI is a distinct deliverable with its own (larger) footprint.
   Both reuse the same engine crates and the same `App` (via the lib split, M4.4).

2. **Everyday gates must stay cheap.** The gate suite runs `cargo clippy/test --workspace`,
   which builds every member — so once `nova` pulls `wgpu`, *every* gate would rebuild it
   (minutes + GBs, the ENOSPC trap CLAUDE.md warns about). So the heavy deps live behind a
   **default-off `gpu` feature** (the `http-provider` pattern): `--workspace` builds the
   cheap `gpu`-off crate (the pure `Scene` model + tests), and the GPU path is built
   deliberately with **`cargo clippy/build -p nova --features gpu`** when that code changes.
   The `Scene` model (M4.1) carries no heavy deps and is always in the cheap gate.

## 5. No `unsafe` (§6.1, workspace-wide `deny(unsafe_code)`)

C1 has no `unsafe` exceptions. The two places GPU code historically reached for it:

- **Surface creation.** `wgpu ≥ 0.19`'s `Instance::create_surface(impl Into<SurfaceTarget>)`
  is **safe** for a borrowed/owned `Window` (it goes through `raw-window-handle`'s safe
  `HasWindowHandle`/`HasDisplayHandle`). We hold the window in an `Arc<Window>` and hand it
  to `create_surface` — no `unsafe` block. (If a pinned version ever required the old unsafe
  path, that is a blocker, not a carve-out.)
- **GPU buffer casts.** `bytemuck` (`Pod`/`Zeroable` derives) gives `&[Vertex] → &[u8]`
  with the `unsafe` confined inside `bytemuck`, exactly as `blake3`/`nix`/`prlimit` keep
  their syscalls outside our crates elsewhere in the tree.

## 6. Colour space

Cell colours are sRGB (the Steelbore §9 palette). `Scene` stores quad/glyph colour as
sRGB-normalised `[f32; 4]` (`r/255, g/255, b/255, 1.0`); the **surface format choice**
(and any sRGB↔linear conversion) is made in M4.2 where the surface is configured, not baked
into the parity model. The Steelbore background (Void Navy `#000027`) is the clear colour.

## 7. Chunk ladder

Each is one PR; gates green; signed/Verified.

| Chunk | Scope | Heavy deps? | Verifiable by |
|-------|-------|-------------|---------------|
| **M4.1** | **`Scene` frame model** — `build_scene(Buffer, CellMetrics) → Scene` (quads + glyph runs), reverse/double-width/blank handling, colour normalisation. **This PR.** | no | unit tests (cheap `--workspace` gate) |
| M4.2 | `gpu` feature: `winit` window + `wgpu` surface/device, render loop, clear to Void Navy, draw the `Scene`'s quads (instanced rects). `mj-nova` binary. | yes (`gpu`) | `-p nova --features gpu` build/clippy; user runs `mj-nova` → sees the cell-background layout |
| M4.3 | `cosmic-text` glyph atlas; render the `Scene`'s glyph runs; derive `CellMetrics` from font metrics. | yes | as above → text renders |
| M4.4 | Split `mj`'s `App` into a shared lib (`majestic` lib + `mj` TTY bin); `mj-nova` drives `App::render` into a `Buffer` each frame + maps `winit` keys → `KeyPress`. | — | `mj` unchanged + `mj-nova` is interactive |
| M4.5 | **Parity suite** (same `Buffer` → Nova `Scene` vs Penumbra emission, logically identical) + **GUI latency harness** (the §7 budgets, GUI path). | — | the two M4 exit criteria |
| M4.6 | Material Symbols vendored icon font + the `icon :x` semantic-token path in Nova; `CREDITS.md`. | yes | icons render |

Adjacent M4 tracks from the PRD (not part of Nova's renderer, sequenced separately):
**Markdown/GFM mode + notes layer**, **`mj ed` line-editor mode** (hashline as the primary
edit interface), **Magit-role Git extension**, session/theme polish.

## 8. Exit criteria (PRD-01 §M4)

- **GUI passes the same latency harness** — the §7 budgets (≈16 ms frame / ≈50 ms cold
  start) measured on the Nova path (`majestic-bench` gains a GUI scenario in M4.5).
- **Penumbra ↔ Nova parity suite green** — for a representative set of editor states, the
  `Buffer` Nova interprets and the `Buffer` Penumbra emits are the same, and Nova's `Scene`
  faithfully reflects it (no cell dropped, mis-coloured, or mis-placed).
