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

use penumbra::{Rgb, Style, Theme};
use stratum::{Span, SpanLayer};
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
    /// Builds a highlighter for `path`'s file type, or `None` if unsupported.
    #[must_use]
    pub fn for_path(path: &Path) -> Option<Self> {
        let extension = path.extension()?.to_str()?;
        let (language, query): (Language, &str) = match extension {
            "rs" => (
                tree_sitter_rust::LANGUAGE.into(),
                tree_sitter_rust::HIGHLIGHTS_QUERY,
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

#[cfg(test)]
mod tests {
    use super::{HighlightKind, SyntaxHighlighter};
    use std::path::Path;

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
}
