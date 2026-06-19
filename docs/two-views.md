<!--
SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
SPDX-License-Identifier: CC-BY-SA-4.0
Document class: design note (Steelbore Standard §4.1.1 — documents default to CC-BY-SA-4.0)
-->

# Design: Two Views of One Buffer

**Status:** approved; implemented in chunks. **Concept:** C1 (Rust + Steel).
**Maintained by** Mohamed Hammad — <https://Majestic.SpacecraftSoftware.org/>

## 1. Goal

Show one document in two (or more) panes with **independent cursor and scroll**, sharing
**text, undo history, and the crash-safe journal**. Editing in one view is visible in the other;
each view scrolls and places its cursor independently.

## 2. The blocker

Today `Buffer` (`crates/majestic-core/src/buffer.rs`) conflates *document* state with *view*
state:

| Document state (should be shared) | View state (should be per-pane) |
|-----------------------------------|---------------------------------|
| `history: UndoTree` (text + undo) | `cursor: usize` |
| `path`, `journal`, `journal_error`, `recovered` | `selection_anchor: Option<usize>` |
| `dirty`, `revision` | `goal_column: Option<usize>` |

Two panes therefore can only share *everything* (one `Editor` → one cursor + one viewport) or
*nothing* (separate buffers → separate text). Additionally, `Buffer::rope() -> &Rope` hands out a
borrow that cannot survive shared interior mutability.

## 3. Data model

```
Document  (shared: Rc<RefCell<Document>>)          // the file
    history: UndoTree   path: Option<PathBuf>
    journal / journal_error / recovered   dirty   revision
    fn rope(&self) -> Rope            // cheap Arc clone, NOT &Rope
    fn apply_edit(&mut self, range, text)           // history.record + journal + dirty + revision
    fn undo(&mut self) -> bool        fn redo(&mut self) -> bool
    fn open(path) -> io::Result<Document>           fn save(&mut self) -> io::Result<()>

Buffer  (per-view handle)                           // the view
    doc: Rc<RefCell<Document>>
    cursor: usize   selection_anchor: Option<usize>   goal_column: Option<usize>
    fn view(&self) -> Buffer          // clone the Rc; fresh cursor = 0, no selection
```

- `Editor` is unchanged in shape — it holds a `Buffer` view plus its own `viewport_top/left` and
  highlighter. `Editor::view()` wraps `buffer.view()` with a fresh viewport + highlighter.
- **Two views = two `Editor`s whose `Buffer`s share one `Document` `Rc`.**
- `Buffer`'s public API is unchanged **except `rope()` now returns an owned `Rope`** (an `Arc`
  bump). Call sites that bind `let rope = self.buffer.rope();` already work; the background
  highlighter already takes an owned `Rope` snapshot.

## 4. Sharing mechanism

`Rc<RefCell<Document>>`. The UI is single-threaded; the only thing crossing to a highlight thread
is a `Rope` (which is `Send`) snapshot, never the `Editor`. `Editor`/`Buffer`/`Workspace` thus
become `!Send` — verified acceptable today (the sole spawned thread is the PTY). `#![deny(unsafe_code)]`
is preserved (`Rc`/`RefCell` are safe). Discipline: never hold a `borrow()` across a call that
re-borrows; each `Buffer` method takes one short borrow.

## 5. Cross-view cursor validity — two phases

- **Phase 1 (Chunk 1–2): byte-offset cursor + clamp-on-access.** Each view keeps its own
  `cursor: usize`; on read/motion it snaps to `≤ len` on a `char` boundary. Robust (never panics).
  A sibling's cursor stays valid but does not *semantically* track an edit made elsewhere.
- **Phase 2 (Chunk 3, follow-up): promote cursors to `stratum::Anchor`.** After an edit, map every
  other view's cursor across the returned `Edit` via `Anchor`/`Bias` (Stratum already implements
  edit-mapping in `anchor.rs`), so cursors follow edits made in other views.

## 6. Split UX

Make splitting show the **same buffer as a second view** by default (Emacs `C-x 2`/`C-x 3`),
replacing the current "reuse a hidden buffer" behavior. `Ctrl+\`/`Alt+\` → second view of the
current buffer; `Alt+←/→` still re-points a pane at another buffer. `dirty` is document-level, so
both tabs of one file share the indicator.

## 7. Implementation chunks (each a PR)

1. **Extract `Document` from `Buffer`.** Pure structural refactor: move document state behind
   `Rc<RefCell<Document>>`; `rope()` returns owned; adapt `editor.rs`/`syntax.rs`. **No behavior
   change** (still one view per buffer). Every existing test stays green — buffer unit tests, the
   cross-process **SIGKILL** recovery test, journal recovery, undo/redo, multibyte motion.
2. **`view()` + split-as-second-view.** Add `Buffer::view()`/`Editor::view()` and the Workspace
   command; new tests (edit in A appears in B; independent cursor; independent scroll).
3. **Anchor-based sibling cursors** (Phase 2).

## 8. Test strategy

- Chunk 1 gate: full `buffer.rs` suite (incl. SIGKILL/journal/undo), editor + syntax suites,
  `clippy -D warnings`, `fmt`, `audit`, `deny`, `reuse lint`, and `majestic-bench --check`
  (the `rope()` clone is an `Arc` bump and the `RefCell` borrow is trivial, so no §7 regression).
- Chunk 2: `two_views_share_edits`, `two_views_independent_cursor`, `two_views_independent_scroll`.

## 9. Risks

- **`RefCell` double-borrow** → audited short-borrow discipline per method.
- **Shared `revision`** → each view's highlighter re-requests on a bump (two highlight threads per
  shared document); a shared highlight cache is a later optimization, not required.
- **`!Send`** → acceptable today; documented so a future threaded use is a conscious choice.
