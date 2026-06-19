// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The broad "regex tier" of Majestic's hybrid highlighter (PRD §5.7): a [`syntect`]-backed
//! highlighter that covers the long tail of languages from Sublime-Text `.sublime-syntax`
//! definitions — compact *data*, not one compiled C grammar per language.
//!
//! syntect supplies *scopes*; theming stays Steelbore (§10). Each token's scope stack maps to a
//! [`HighlightKind`] (which resolves to a theme token), so no `.tmTheme` and no bare hex enter
//! the pipeline. The pure-Rust `regex-fancy` backend keeps the engine free of C (memory-safety
//! first, §3.1). Highlighting runs on the background snapshot worker like the tree-sitter tier,
//! so keypress latency is unaffected (§7).

use std::path::Path;
use std::sync::LazyLock;

use stratum::{Span, SpanLayer};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

use crate::syntax::HighlightKind;

/// The shared syntax set: bat's extended `.sublime-syntax` collection (~150 languages) via
/// `two-face`. Built once, lazily, on first use — off the UI thread.
static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_newlines);

/// A syntect-backed highlighter bound to one language.
pub(crate) struct SyntectHighlighter {
    syntax: &'static SyntaxReference,
}

impl SyntectHighlighter {
    /// Whether syntect recognizes `path`'s file type (by extension).
    #[must_use]
    pub(crate) fn supports(path: &Path) -> bool {
        syntax_for(path).is_some()
    }

    /// Builds a highlighter for `path`, or `None` if syntect has no syntax for it.
    #[must_use]
    pub(crate) fn for_path(path: &Path) -> Option<Self> {
        Some(Self {
            syntax: syntax_for(path)?,
        })
    }

    /// Highlights `source` into a layer of [`HighlightKind`] spans (line-based regex scopes).
    #[must_use]
    pub(crate) fn highlight(&mut self, source: &[u8]) -> SpanLayer<HighlightKind> {
        let text = String::from_utf8_lossy(source);
        let mut parse = ParseState::new(self.syntax);
        let mut stack = ScopeStack::new();
        let mut spans = Vec::new();
        let mut base = 0usize;
        for line in LinesWithEndings::from(&text) {
            let Ok(ops) = parse.parse_line(line, &SYNTAXES) else {
                break; // a malformed line ends highlighting; the rest renders unstyled
            };
            let mut last = 0usize;
            for (offset, op) in ops {
                // The text up to this op carries the scope stack as it stands *before* the op.
                push_span(&mut spans, &stack, base + last, base + offset);
                let _ = stack.apply(&op);
                last = offset;
            }
            push_span(&mut spans, &stack, base + last, base + line.len());
            base += line.len();
        }
        // Spans are produced left-to-right, so the layer bulk-loads in O(n).
        SpanLayer::from_sorted(spans)
    }
}

/// Resolves `path`'s extension to a bundled syntax (`'static` — `SYNTAXES` is a `static`).
///
/// syntect's catch-all "Plain Text" syntax produces no scopes, so it is treated as *unsupported*
/// — a plain-text file gets no idle background worker.
fn syntax_for(path: &Path) -> Option<&'static SyntaxReference> {
    let extension = path.extension()?.to_str()?;
    let syntax = SYNTAXES.find_syntax_by_extension(extension)?;
    (syntax.name != "Plain Text").then_some(syntax)
}

/// Appends a span for `[start, end)` when the range is non-empty and its scope maps to a kind.
fn push_span(spans: &mut Vec<Span<HighlightKind>>, stack: &ScopeStack, start: usize, end: usize) {
    if end <= start {
        return;
    }
    if let Some(kind) = stack_to_kind(stack.as_slice()) {
        spans.push(Span::with_offsets(start, end, kind));
    }
}

/// Maps a syntect scope stack to a [`HighlightKind`]. Comments and strings dominate their whole
/// region; otherwise the most specific (top-most) scope decides.
fn stack_to_kind(scopes: &[Scope]) -> Option<HighlightKind> {
    let mut chosen = None;
    for scope in scopes {
        let text = scope.build_string();
        if text.starts_with("comment") {
            return Some(HighlightKind::Comment);
        }
        if text.starts_with("string") {
            return Some(HighlightKind::String);
        }
        if let Some(kind) = scope_to_kind(&text) {
            chosen = Some(kind); // keep the most specific (last) non-comment/string mapping
        }
    }
    chosen
}

/// Maps one TextMate/Sublime scope string (e.g. `keyword.control.rust`) to a [`HighlightKind`].
/// Comment and string scopes are handled by [`stack_to_kind`] and are absent here.
///
/// Prefixes are ordered most-specific first; the first match wins.
fn scope_to_kind(scope: &str) -> Option<HighlightKind> {
    use HighlightKind::{
        Attribute, Constant, Function, Keyword, Number, Punctuation, Type, Variable,
    };
    const TABLE: &[(&str, HighlightKind)] = &[
        ("constant.numeric", Number),
        ("constant", Constant),
        ("entity.name.function", Function),
        ("entity.other.attribute-name", Attribute),
        ("entity.name.tag", Keyword),
        ("entity", Type),
        ("support.function", Function),
        ("support.type", Type),
        ("support.class", Type),
        ("support.constant", Constant),
        ("support", Variable),
        ("variable.function", Function),
        ("variable", Variable),
        ("storage.type", Type),
        ("storage", Keyword),
        ("keyword", Keyword),
        ("punctuation", Punctuation),
    ];
    TABLE
        .iter()
        .find(|(prefix, _)| scope.starts_with(prefix))
        .map(|&(_, kind)| kind)
}

#[cfg(test)]
mod tests {
    use super::{scope_to_kind, SyntectHighlighter};
    use crate::syntax::HighlightKind;
    use std::path::Path;

    #[test]
    fn maps_textmate_scopes_to_kinds() {
        assert_eq!(
            scope_to_kind("keyword.control.rust"),
            Some(HighlightKind::Keyword)
        );
        assert_eq!(
            scope_to_kind("constant.numeric.integer"),
            Some(HighlightKind::Number)
        );
        assert_eq!(
            scope_to_kind("entity.name.function.python"),
            Some(HighlightKind::Function)
        );
        assert_eq!(
            scope_to_kind("variable.other"),
            Some(HighlightKind::Variable)
        );
        assert_eq!(scope_to_kind("source.rust"), None);
    }

    #[test]
    fn highlights_a_python_snippet() {
        // Python is in syntect's bundled default set — no tree-sitter grammar involved here.
        let mut highlighter =
            SyntectHighlighter::for_path(Path::new("x.py")).expect("syntect has Python");
        let layer = highlighter.highlight(b"def f():\n    return 1  # done\n");
        let kinds: Vec<HighlightKind> = layer.iter().map(|span| span.value).collect();
        assert!(
            kinds.contains(&HighlightKind::Keyword),
            "def/return are keywords"
        );
        assert!(kinds.contains(&HighlightKind::Comment), "trailing comment");
        assert!(!layer.is_empty());
    }

    #[test]
    fn unknown_extension_is_unsupported() {
        assert!(!SyntectHighlighter::supports(Path::new("notes.unknownext")));
    }

    #[test]
    fn covers_languages_beyond_the_tree_sitter_core() {
        // A sampling from bat's extended set that the tree-sitter tier does not wire — including
        // Lua and Emacs Lisp (`.el`, via the bundled "Lisp" syntax).
        for ext in [
            "nix", "swift", "kt", "dart", "toml", "zig", "asm", "adb", "lua", "el",
        ] {
            assert!(
                SyntectHighlighter::supports(Path::new(&format!("x.{ext}"))),
                ".{ext} should be covered by the syntect tier"
            );
        }
    }
}
