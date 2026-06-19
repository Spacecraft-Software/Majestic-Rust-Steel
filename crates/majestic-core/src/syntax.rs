// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Tree-sitter syntax highlighting → theme-styled [`stratum::Span`] layers (PRD #1 §5.7, §6.9).
//!
//! A [`SyntaxHighlighter`] runs `tree_sitter_highlight` over a buffer's bytes and produces a
//! [`SpanLayer`] of [`HighlightKind`]s, each resolving to a [`Style`] from the active theme
//! (so highlighting stays inside the Steelbore palette, never bare hex). Languages are
//! selected by file extension. Highlighting is computed on the whole buffer for now;
//! incremental parsing on background snapshots (PRD §6.4/§6.9) is the optimization.

use std::fmt;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

use penumbra::{Rgb, Style, Theme};
use stratum::{Rope, Span, SpanLayer};
use tree_sitter::Language;
use tree_sitter_highlight::{Highlight, HighlightConfiguration, HighlightEvent, Highlighter};

/// A syntax category, resolved to a [`Style`] via the active theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HighlightKind {
    /// Keywords (`fn`, `let`, `if`, …).
    Keyword,
    /// Type names.
    Type,
    /// Function / method / macro names.
    Function,
    /// String and character literals.
    String,
    /// Comments.
    Comment,
    /// Numeric literals.
    Number,
    /// Constants and builtin values.
    Constant,
    /// Attributes / annotations.
    Attribute,
    /// Operators and punctuation.
    Punctuation,
    /// Variables and properties.
    Variable,
}

impl HighlightKind {
    /// Resolves this category to a concrete [`Style`] using `theme` tokens (UI.md §3).
    #[must_use]
    pub fn style(self, theme: &Theme) -> Style {
        match self {
            Self::Keyword | Self::Type | Self::Attribute => {
                Style::new(theme.accent, theme.background)
            }
            Self::Function => Style::new(theme.foreground, theme.background).bold(),
            Self::String => Style::new(theme.success, theme.background),
            Self::Comment => Style::new(dim(theme.foreground, theme.background), theme.background),
            Self::Number | Self::Constant => Style::new(theme.info, theme.background),
            Self::Punctuation | Self::Variable => theme.base_style(),
        }
    }
}

/// The highlight-capture vocabulary, mapping tree-sitter capture names to [`HighlightKind`].
///
/// `tree_sitter_highlight` matches a grammar's capture (e.g. `keyword.control`) to the
/// longest name here that is its prefix, and returns that name's index.
const CAPTURES: &[(&str, HighlightKind)] = &[
    ("attribute", HighlightKind::Attribute),
    ("comment", HighlightKind::Comment),
    ("constant", HighlightKind::Constant),
    ("constant.builtin", HighlightKind::Constant),
    ("constructor", HighlightKind::Function),
    ("function", HighlightKind::Function),
    ("function.macro", HighlightKind::Function),
    ("function.method", HighlightKind::Function),
    ("keyword", HighlightKind::Keyword),
    ("label", HighlightKind::Constant),
    ("number", HighlightKind::Number),
    ("operator", HighlightKind::Punctuation),
    ("property", HighlightKind::Variable),
    ("punctuation", HighlightKind::Punctuation),
    ("string", HighlightKind::String),
    ("type", HighlightKind::Type),
    ("type.builtin", HighlightKind::Type),
    ("variable", HighlightKind::Variable),
    ("variable.builtin", HighlightKind::Constant),
];

/// A configured tree-sitter highlighter for one language.
pub struct SyntaxHighlighter {
    highlighter: Highlighter,
    config: HighlightConfiguration,
}

impl SyntaxHighlighter {
    /// Whether `path`'s extension maps to a supported language (cheap; no construction).
    ///
    /// The recognised extensions follow the enabled grammar features (see `Cargo.toml`): an
    /// extension only counts when its language is compiled in.
    #[must_use]
    pub fn supports(path: &Path) -> bool {
        let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
            return false;
        };
        #[cfg(feature = "lang-rust")]
        if extension == "rs" {
            return true;
        }
        #[cfg(feature = "lang-python")]
        if extension == "py" {
            return true;
        }
        #[cfg(feature = "lang-go")]
        if extension == "go" {
            return true;
        }
        #[cfg(feature = "lang-c")]
        if matches!(extension, "c" | "h") {
            return true;
        }
        #[cfg(feature = "lang-bash")]
        if matches!(extension, "sh" | "bash") {
            return true;
        }
        #[cfg(feature = "lang-json")]
        if extension == "json" {
            return true;
        }
        #[cfg(feature = "lang-nix")]
        if extension == "nix" {
            return true;
        }
        #[cfg(feature = "lang-scheme")]
        if matches!(extension, "scm" | "ss") {
            return true;
        }
        #[cfg(feature = "lang-elixir")]
        if matches!(extension, "ex" | "exs") {
            return true;
        }
        #[cfg(feature = "lang-erlang")]
        if matches!(extension, "erl" | "hrl") {
            return true;
        }
        #[cfg(feature = "lang-powershell")]
        if matches!(extension, "ps1" | "psm1" | "psd1") {
            return true;
        }
        #[cfg(feature = "lang-typescript")]
        if matches!(extension, "ts" | "tsx") {
            return true;
        }
        false
    }

    /// Builds a highlighter for `path`'s file type, or `None` if unsupported.
    #[must_use]
    pub fn for_path(path: &Path) -> Option<Self> {
        let extension = path.extension()?.to_str()?;
        let (language, query): (Language, &str) = match extension {
            #[cfg(feature = "lang-rust")]
            "rs" => (
                tree_sitter_rust::LANGUAGE.into(),
                tree_sitter_rust::HIGHLIGHTS_QUERY,
            ),
            #[cfg(feature = "lang-python")]
            "py" => (
                tree_sitter_python::LANGUAGE.into(),
                tree_sitter_python::HIGHLIGHTS_QUERY,
            ),
            #[cfg(feature = "lang-go")]
            "go" => (
                tree_sitter_go::LANGUAGE.into(),
                tree_sitter_go::HIGHLIGHTS_QUERY,
            ),
            #[cfg(feature = "lang-c")]
            "c" | "h" => (
                tree_sitter_c::LANGUAGE.into(),
                tree_sitter_c::HIGHLIGHT_QUERY,
            ),
            #[cfg(feature = "lang-bash")]
            "sh" | "bash" => (
                tree_sitter_bash::LANGUAGE.into(),
                tree_sitter_bash::HIGHLIGHT_QUERY,
            ),
            #[cfg(feature = "lang-json")]
            "json" => (
                tree_sitter_json::LANGUAGE.into(),
                tree_sitter_json::HIGHLIGHTS_QUERY,
            ),
            #[cfg(feature = "lang-nix")]
            "nix" => (
                tree_sitter_nix::LANGUAGE.into(),
                tree_sitter_nix::HIGHLIGHTS_QUERY,
            ),
            // Generic Scheme grammar — covers GNU Guile (`.scm`/`.ss`).
            #[cfg(feature = "lang-scheme")]
            "scm" | "ss" => (
                tree_sitter_scheme::LANGUAGE.into(),
                tree_sitter_scheme::HIGHLIGHTS_QUERY,
            ),
            #[cfg(feature = "lang-elixir")]
            "ex" | "exs" => (
                tree_sitter_elixir::LANGUAGE.into(),
                tree_sitter_elixir::HIGHLIGHTS_QUERY,
            ),
            #[cfg(feature = "lang-erlang")]
            "erl" | "hrl" => (
                tree_sitter_erlang::LANGUAGE.into(),
                tree_sitter_erlang::HIGHLIGHTS_QUERY,
            ),
            #[cfg(feature = "lang-powershell")]
            "ps1" | "psm1" | "psd1" => (
                tree_sitter_powershell::LANGUAGE.into(),
                tree_sitter_powershell::HIGHLIGHTS_QUERY,
            ),
            #[cfg(feature = "lang-typescript")]
            "ts" => (
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
            ),
            #[cfg(feature = "lang-typescript")]
            "tsx" => (
                tree_sitter_typescript::LANGUAGE_TSX.into(),
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
            ),
            _ => return None,
        };
        Self::new(language, query)
    }

    fn new(language: Language, highlights_query: &str) -> Option<Self> {
        let mut config =
            HighlightConfiguration::new(language, "majestic", highlights_query, "", "").ok()?;
        let names: Vec<&str> = CAPTURES.iter().map(|(name, _)| *name).collect();
        config.configure(&names);
        Some(Self {
            highlighter: Highlighter::new(),
            config,
        })
    }

    /// Highlights `source`, returning a layer of styled spans (empty on parse error).
    #[must_use]
    pub fn highlight(&mut self, source: &[u8]) -> SpanLayer<HighlightKind> {
        let Ok(events) = self
            .highlighter
            .highlight(&self.config, source, None, |_| None)
        else {
            return SpanLayer::new();
        };

        // `tree_sitter_highlight` emits `Source` ranges in increasing start order, so the spans
        // are collected already sorted and bulk-loaded in O(n) (vs. O(n²) repeated inserts —
        // the hot path the §7 harness flagged).
        let mut spans = Vec::new();
        let mut stack: Vec<HighlightKind> = Vec::new();
        for event in events {
            match event {
                Ok(HighlightEvent::HighlightStart(Highlight(index))) => {
                    if let Some((_, kind)) = CAPTURES.get(index) {
                        stack.push(*kind);
                    }
                }
                Ok(HighlightEvent::HighlightEnd) => {
                    stack.pop();
                }
                Ok(HighlightEvent::Source { start, end }) => {
                    if end > start {
                        if let Some(&kind) = stack.last() {
                            spans.push(Span::with_offsets(start, end, kind));
                        }
                    }
                }
                Err(_) => break,
            }
        }
        SpanLayer::from_sorted(spans)
    }
}

impl fmt::Debug for SyntaxHighlighter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyntaxHighlighter").finish_non_exhaustive()
    }
}

/// Blends `fg` halfway toward `bg` to produce a dimmed color (for comments).
fn dim(fg: Rgb, bg: Rgb) -> Rgb {
    Rgb::new(
        fg.r.midpoint(bg.r),
        fg.g.midpoint(bg.g),
        fg.b.midpoint(bg.b),
    )
}

/// The hybrid highlighter: tree-sitter for its structural core, syntect for the broad regex tier.
///
/// Routing precedence: a file goes to **tree-sitter** when a grammar is compiled in for it
/// (structural parsing, §6.9), otherwise to **syntect** (broad coverage from `.sublime-syntax`
/// data), otherwise nothing. Both engines yield the same `SpanLayer<HighlightKind>`, so the
/// background worker is engine-agnostic.
enum Engine {
    // Boxed: a `SyntaxHighlighter` (tree-sitter config + highlighter) dwarfs the syntect handle.
    TreeSitter(Box<SyntaxHighlighter>),
    #[cfg(feature = "syntect-highlighting")]
    Syntect(crate::syntect_hl::SyntectHighlighter),
}

impl Engine {
    /// Whether either tier can highlight `path`.
    fn supports(path: &Path) -> bool {
        if SyntaxHighlighter::supports(path) {
            return true;
        }
        #[cfg(feature = "syntect-highlighting")]
        if crate::syntect_hl::SyntectHighlighter::supports(path) {
            return true;
        }
        false
    }

    /// Builds the highlighter for `path`, preferring the structural tree-sitter tier.
    fn for_path(path: &Path) -> Option<Self> {
        if let Some(highlighter) = SyntaxHighlighter::for_path(path) {
            return Some(Self::TreeSitter(Box::new(highlighter)));
        }
        #[cfg(feature = "syntect-highlighting")]
        if let Some(highlighter) = crate::syntect_hl::SyntectHighlighter::for_path(path) {
            return Some(Self::Syntect(highlighter));
        }
        None
    }

    fn highlight(&mut self, source: &[u8]) -> SpanLayer<HighlightKind> {
        match self {
            Self::TreeSitter(highlighter) => highlighter.highlight(source),
            #[cfg(feature = "syntect-highlighting")]
            Self::Syntect(highlighter) => highlighter.highlight(source),
        }
    }
}

/// A finished highlight result, tagged with the buffer revision it was computed from.
pub(crate) struct Highlighted {
    /// The buffer revision the snapshot reflected.
    pub revision: u64,
    /// The styled span layer for that revision.
    pub layer: SpanLayer<HighlightKind>,
}

/// A request to highlight one buffer snapshot. The snapshot is a cheap [`Rope`] clone (an `Arc`
/// bump), so the UI thread does no text copying — the worker materializes the text itself.
struct Request {
    revision: u64,
    snapshot: Rope,
}

/// A background worker that highlights buffer snapshots off the UI thread (PRD §6.4 snapshot
/// ping-pong, §6.9).
///
/// The editor sends `(revision, snapshot)` over a channel and polls results; the worker owns the
/// tree-sitter highlighter and coalesces superseded requests (only the newest pending snapshot is
/// parsed — older ones are dropped, which is the cancellation signal). One thread per highlighted
/// buffer; a shared Morpheus pool is a later refinement.
#[derive(Debug)]
pub(crate) struct HighlightWorker {
    requests: Option<Sender<Request>>,
    results: Receiver<Highlighted>,
    handle: Option<JoinHandle<()>>,
}

impl HighlightWorker {
    /// Spawns a worker for `path`'s language, or `None` if the file type is unsupported.
    #[must_use]
    pub(crate) fn for_path(path: &Path) -> Option<Self> {
        if !Engine::supports(path) {
            return None;
        }
        let path = path.to_path_buf();
        let (request_tx, request_rx) = mpsc::channel::<Request>();
        let (result_tx, result_rx) = mpsc::channel::<Highlighted>();
        let handle = std::thread::Builder::new()
            .name("majestic-highlight".to_owned())
            .spawn(move || run_worker(&path, &request_rx, &result_tx))
            .ok()?;
        Some(Self {
            requests: Some(request_tx),
            results: result_rx,
            handle: Some(handle),
        })
    }

    /// Queues a snapshot to highlight in the background. Non-blocking; never waits on the worker.
    pub(crate) fn request(&self, revision: u64, snapshot: Rope) {
        if let Some(requests) = &self.requests {
            let _ = requests.send(Request { revision, snapshot });
        }
    }

    /// Returns the most recent finished result, discarding any older ones (non-blocking).
    pub(crate) fn poll(&self) -> Option<Highlighted> {
        let mut latest = None;
        while let Ok(done) = self.results.try_recv() {
            latest = Some(done);
        }
        latest
    }

    /// Blocks until a result for `revision` (or newer) arrives, or the worker stops. Used by
    /// deterministic, non-interactive rendering (tests, the perf harness).
    pub(crate) fn wait_for(&self, revision: u64) -> Option<Highlighted> {
        while let Ok(done) = self.results.recv() {
            if done.revision >= revision {
                return Some(done);
            }
        }
        None
    }
}

impl Drop for HighlightWorker {
    fn drop(&mut self) {
        // Dropping the request sender ends the worker's `recv` loop; then join the thread.
        self.requests = None;
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// The worker thread body: own a highlighter, then highlight the newest pending snapshot in a
/// loop, streaming results back until the editor (and its request sender) is gone.
fn run_worker(path: &Path, requests: &Receiver<Request>, results: &Sender<Highlighted>) {
    let Some(mut highlighter) = Engine::for_path(path) else {
        return; // unsupported despite `supports` — exit; the editor simply gets no highlights
    };
    while let Ok(request) = requests.recv() {
        // Coalesce: skip snapshots already superseded by a newer pending one (cancellation).
        let mut latest = request;
        while let Ok(newer) = requests.try_recv() {
            latest = newer;
        }
        let layer = highlighter.highlight(latest.snapshot.to_string().as_bytes());
        if results
            .send(Highlighted {
                revision: latest.revision,
                layer,
            })
            .is_err()
        {
            break; // the editor is gone
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Engine, HighlightKind, SyntaxHighlighter};
    use std::path::Path;

    #[cfg(all(feature = "lang-rust", feature = "syntect-highlighting"))]
    #[test]
    fn routing_prefers_tree_sitter_then_falls_back_to_syntect() {
        // Rust has a tree-sitter grammar (structural tier) — it must win.
        assert!(matches!(
            Engine::for_path(Path::new("x.rs")),
            Some(Engine::TreeSitter(_))
        ));
        // Ruby has no tree-sitter grammar wired but is in syntect's default set — broad tier.
        assert!(matches!(
            Engine::for_path(Path::new("x.rb")),
            Some(Engine::Syntect(_))
        ));
        // A language neither tier knows is unsupported.
        assert!(Engine::for_path(Path::new("x.unknownext")).is_none());
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn highlights_rust_keywords_and_strings() {
        let mut highlighter = SyntaxHighlighter::for_path(Path::new("x.rs")).unwrap();
        let source = b"fn main() {\n    let s = \"hi\";\n}\n";
        let layer = highlighter.highlight(source);

        // "fn" at bytes 0..2 is a keyword.
        let fn_kind = layer.spans_in(0..2).map(|span| span.value).next().unwrap();
        assert_eq!(fn_kind, HighlightKind::Keyword);

        // Somewhere there is a string span and a keyword `let`.
        let kinds: Vec<HighlightKind> = layer.iter().map(|span| span.value).collect();
        assert!(
            kinds.contains(&HighlightKind::String),
            "expected a string span"
        );
        assert!(
            kinds
                .iter()
                .filter(|k| **k == HighlightKind::Keyword)
                .count()
                >= 2,
            "expected `fn` and `let` keywords",
        );
    }

    #[test]
    fn unknown_extension_has_no_highlighter() {
        assert!(SyntaxHighlighter::for_path(Path::new("notes.xyz")).is_none());
        assert!(SyntaxHighlighter::for_path(Path::new("noext")).is_none());
    }

    #[test]
    #[allow(
        clippy::vec_init_then_push,
        reason = "cases are pushed under #[cfg]; a vec! literal cannot gate elements"
    )]
    fn highlights_each_supported_language() {
        // Each *enabled* grammar builds and produces spans for a representative snippet. Cases are
        // gated by feature so the test adapts to whatever grammar set the build compiled in.
        let mut cases: Vec<(&str, &[u8])> = Vec::new();
        #[cfg(feature = "lang-rust")]
        cases.push(("x.rs", b"fn main() {}\n" as &[u8]));
        #[cfg(feature = "lang-python")]
        cases.push(("x.py", b"def f():\n    return 1\n" as &[u8]));
        #[cfg(feature = "lang-go")]
        cases.push(("x.go", b"package main\nfunc main() {}\n" as &[u8]));
        #[cfg(feature = "lang-c")]
        cases.push(("x.c", b"int main(void) { return 0; }\n" as &[u8]));
        #[cfg(feature = "lang-bash")]
        cases.push(("x.sh", b"echo hi\nif true; then ls; fi\n" as &[u8]));
        #[cfg(feature = "lang-json")]
        cases.push(("x.json", b"{\"key\": true, \"n\": 1}\n" as &[u8]));
        #[cfg(feature = "lang-nix")]
        cases.push(("x.nix", b"{ a = 1; b = \"x\"; }\n" as &[u8]));
        #[cfg(feature = "lang-scheme")]
        cases.push(("x.scm", b"(define (square x) (* x x))\n" as &[u8]));
        #[cfg(feature = "lang-elixir")]
        cases.push(("x.ex", b"defmodule M do\n  def f, do: 1\nend\n" as &[u8]));
        #[cfg(feature = "lang-erlang")]
        cases.push(("x.erl", b"-module(m).\nf() -> 1.\n" as &[u8]));
        #[cfg(feature = "lang-powershell")]
        cases.push(("x.ps1", b"function Get-X { param($a) $a }\n" as &[u8]));
        #[cfg(feature = "lang-typescript")]
        cases.push(("x.ts", b"const x: number = 1;\n" as &[u8]));
        #[cfg(feature = "lang-typescript")]
        cases.push((
            "x.tsx",
            b"const x: number = 1;\nfunction f() { return x; }\n" as &[u8],
        ));

        let mut failed = Vec::new();
        for (name, source) in &cases {
            let ok = SyntaxHighlighter::for_path(Path::new(name))
                .is_some_and(|mut highlighter| !highlighter.highlight(source).is_empty());
            if !ok {
                failed.push(*name);
            }
        }
        assert!(failed.is_empty(), "no highlight spans for: {failed:?}");
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn background_worker_highlights_a_snapshot() {
        use super::HighlightWorker;
        use stratum::Rope;
        let worker = HighlightWorker::for_path(Path::new("x.rs")).unwrap();
        worker.request(7, Rope::from("fn main() {}\n"));
        let done = worker.wait_for(7).expect("worker should deliver a result");
        assert_eq!(done.revision, 7);
        assert!(!done.layer.is_empty(), "expected highlight spans");
    }

    #[test]
    fn background_worker_is_none_for_unsupported() {
        use super::HighlightWorker;
        assert!(HighlightWorker::for_path(Path::new("notes.txt")).is_none());
    }
}
