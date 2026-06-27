// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The GNU Texinfo parser for the rendered [`Preview`](crate::Preview) (M4): a `.texi` source becomes
//! styled display lines.
//!
//! [`layout`] is a line-oriented Texinfo reader that drives a [`LineBuilder`] (the shared typesetter in
//! [`crate::preview`]): sectioning commands become headings (bold + a rule for `@chapter`/`@section`),
//! `@example`/`@verbatim`/`@lisp` become verbatim code blocks, `@itemize`/`@enumerate`/`@table` become
//! lists, `@menu` entries become links, and the inline `@`-commands (`@code`, `@emph`, `@strong`,
//! `@var`, `@file`, `@url`, …) are styled with the Steelbore palette. Comments (`@c`), index entries,
//! conditionals (`@iftex`/`@tex`/`@html`/`@ignore` are skipped; `@ifinfo`/`@ifnottex` render), and the
//! `@`-escapes (`@@`, `@{`, `@}`, `@*`) are handled.
//!
//! Deferred (rendered minimally / as text for now): `@def…` definition blocks (signature shown bold,
//! body indented), `@value` expansion + footnotes (`@multitable` renders as aligned columns) — clearly-marked follow-ups.
//
// Rust guideline compliant 2026-05-18

use penumbra::Style;

use crate::preview::{text_width, Line, LineBuilder, Palette};

/// Spaces of indent per block-nesting level.
const INDENT: usize = 2;

/// Lays a Texinfo `source` out into styled display lines wrapped to `width` columns, styled with
/// `palette`.
pub(crate) fn layout(source: &str, width: usize, palette: &Palette) -> Vec<Line> {
    let mut reader = Texinfo::new(width, palette);
    for raw in source.lines() {
        reader.line(raw);
    }
    reader.builder.finish()
}

/// A Texinfo block environment (`@example … @end example`, a list, …).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Block {
    /// `@example`/`@smallexample`/`@verbatim`/`@lisp` — verbatim code.
    Verbatim,
    /// `@itemize` — a bullet list.
    Itemize,
    /// `@enumerate` — a numbered list (carrying the next number).
    Enumerate(u64),
    /// `@table`/`@vtable`/`@ftable` — a description list (`@item` term is bold).
    Table,
    /// `@menu` — node menu (entries become links).
    Menu,
    /// `@quotation` — an indented, barred quote.
    Quotation,
    /// `@def…` — a definition (signature bold, body indented).
    Definition,
}

/// An `@multitable` collected as rows of plain-text cells (`@item`/`@headitem`, split on `@tab`).
#[derive(Default)]
struct Multitable {
    rows: Vec<Vec<String>>,
    /// How many leading rows are `@headitem` header rows (for bold + the separator rule).
    head_rows: usize,
}

/// A line-oriented Texinfo reader driving a [`LineBuilder`].
struct Texinfo<'a> {
    builder: LineBuilder<'a>,
    blocks: Vec<Block>,
    /// Environments whose content is skipped (`@iftex`, `@tex`, `@html`, `@ignore`, …), innermost last.
    skip: Vec<String>,
    /// The `@multitable` currently being collected (rows of cells), rendered at `@end multitable`.
    multitable: Option<Multitable>,
}

impl<'a> Texinfo<'a> {
    fn new(width: usize, palette: &'a Palette) -> Self {
        Self {
            builder: LineBuilder::new(width, palette),
            blocks: Vec::new(),
            skip: Vec::new(),
            multitable: None,
        }
    }

    fn line(&mut self, raw: &str) {
        // Inside a skipped environment, only watch for its matching `@end`.
        if let Some(env) = self.skip.last() {
            if line_ends(raw, env) {
                self.skip.pop();
            }
            return;
        }
        // Inside a verbatim block, emit lines as-is (code style) until `@end`.
        if self.blocks.last() == Some(&Block::Verbatim) {
            if ends_a_verbatim(raw) {
                self.pop_block();
            } else {
                self.builder
                    .append(raw.trim_end(), self.builder.palette().code);
                self.builder.end_line();
            }
            return;
        }
        // Inside an @multitable, only @item/@headitem/@end multitable matter (cells split on @tab).
        if self.multitable.is_some() {
            self.multitable_line(raw.trim());
            return;
        }

        let trimmed = raw.trim();
        if trimmed.is_empty() {
            self.builder.blank();
            return;
        }
        if let Some(rest) = trimmed.strip_prefix('@') {
            let (word, arg) = split_command(rest);
            if self.line_command(word, arg) {
                return;
            }
            // Not a line command — a paragraph that opens with an inline command (`@code{…} …`).
        }
        if self.blocks.last() == Some(&Block::Menu) && trimmed.starts_with('*') {
            self.menu_entry(trimmed);
        } else {
            let style = self.builder.palette().body;
            self.render_inline(trimmed, style);
        }
    }

    /// Handles a leading `@word`; returns whether it was a recognized *line* command (vs. inline text).
    fn line_command(&mut self, word: &str, arg: &str) -> bool {
        if let Some(level) = heading_level(word) {
            self.heading(arg, level);
            return true;
        }
        match word {
            "c" | "comment" => {}
            "end" => self.end_named(arg),
            "multitable" => {
                self.builder.blank();
                self.multitable = Some(Multitable::default());
            }
            "example" | "smallexample" | "verbatim" | "lisp" | "smalllisp" | "display"
            | "format" => {
                self.push_block(Block::Verbatim);
            }
            "itemize" => self.push_block(Block::Itemize),
            "enumerate" => self.push_block(Block::Enumerate(1)),
            "table" | "vtable" | "ftable" => self.push_block(Block::Table),
            "menu" => self.push_block(Block::Menu),
            "quotation" | "smallquotation" => self.push_block(Block::Quotation),
            "item" | "itemx" => self.item(arg),
            "sp" => {
                self.builder.blank();
            }
            "center" | "noindent" | "indent" | "exdent" => {
                if !arg.is_empty() {
                    let style = self.builder.palette().body;
                    self.render_inline(arg, style);
                }
            }
            // Skipped environments (print/markup-only or ignored); render nothing until their `@end`.
            "iftex"
            | "tex"
            | "html"
            | "ifhtml"
            | "docbook"
            | "ifdocbook"
            | "xml"
            | "ifxml"
            | "ignore"
            | "ifnotinfo"
            | "titlepage"
            | "copying"
            | "direntry"
            | "documentdescription" => self.skip.push(word.to_owned()),
            _ if is_definition(word) => self.definition(arg),
            // Conditionals we honor (Info output): the start/end are no-ops, content flows through.
            "ifinfo" | "ifnottex" | "ifnothtml" | "ifnotdocbook" | "ifnotxml"
            | "ifnotplaintext" | "ifplaintext" => {}
            // Directives / metadata / index entries with no rendered output.
            _ if is_directive(word) => {}
            // Unknown leading `@word` — treat the line as inline text so content is never lost.
            _ => return false,
        }
        true
    }

    /// Renders a sectioning command's title as a heading (bold), with a rule for the top levels.
    fn heading(&mut self, title: &str, level: usize) {
        self.builder.blank();
        let style = self.builder.palette().heading;
        self.render_inline(title, style);
        self.builder.end_line();
        if level <= 2 {
            self.builder.rule();
        }
    }

    fn push_block(&mut self, block: Block) {
        self.builder.blank();
        self.blocks.push(block);
        self.reflow();
    }

    fn pop_block(&mut self) {
        self.builder.end_line();
        self.blocks.pop();
        self.reflow();
        self.builder.blank();
    }

    /// Handles `@end <env>` — pops the current block (well-formed input assumed).
    fn end_named(&mut self, env: &str) {
        if matches!(
            env,
            "ifinfo"
                | "ifnottex"
                | "ifnothtml"
                | "ifnotdocbook"
                | "ifnotxml"
                | "ifnotplaintext"
                | "ifplaintext"
        ) {
            return; // matched an honored conditional whose start was a no-op
        }
        if !self.blocks.is_empty() {
            self.pop_block();
        }
    }

    /// Recomputes the builder's indent + quote depth from the block stack.
    fn reflow(&mut self) {
        let mut indent = 0;
        let mut quotes = 0;
        for block in &self.blocks {
            match block {
                Block::Itemize
                | Block::Enumerate(_)
                | Block::Table
                | Block::Definition
                | Block::Verbatim => indent += INDENT,
                Block::Quotation => quotes += 1,
                Block::Menu => {}
            }
        }
        self.builder.set_indent(indent);
        self.builder.set_quote_depth(quotes);
    }

    /// Handles `@item` inside the current list/table, or as plain text outside one.
    fn item(&mut self, arg: &str) {
        self.builder.end_line();
        // Reset the indent to the block-nesting value (a previous `@item` left it at its hanging
        // indent), so this item's marker lands at the list's left edge, not the previous item's.
        self.reflow();
        let base = self.builder.indent().saturating_sub(INDENT);
        let palette = self.builder.palette();
        match self.blocks.last_mut() {
            Some(Block::Enumerate(number)) => {
                let marker = format!("{number}. ");
                *number += 1;
                self.set_marker(base, &marker, palette.body);
                self.builder.set_indent(base + text_width(&marker));
                if !arg.is_empty() {
                    let style = self.builder.palette().body;
                    self.render_inline(arg, style);
                }
            }
            Some(Block::Table) => {
                // A description term: bold, at the list's left edge; the body wraps under it.
                self.set_marker(base, "", palette.body);
                self.builder.set_indent(base);
                let style = self.builder.palette().heading;
                self.render_inline(arg, style);
                self.builder.end_line();
                self.builder.set_indent(base + INDENT);
            }
            _ => {
                self.set_marker(base, "• ", palette.body);
                self.builder.set_indent(base + INDENT);
                if !arg.is_empty() {
                    let style = self.builder.palette().body;
                    self.render_inline(arg, style);
                }
            }
        }
    }

    /// Sets a one-shot line prefix of `base` spaces followed by `marker` in `style`.
    fn set_marker(&mut self, base: usize, marker: &str, style: Style) {
        let mut prefix: Line = Vec::new();
        if base > 0 {
            prefix.push((" ".repeat(base), self.builder.palette().body));
        }
        if !marker.is_empty() {
            prefix.push((marker.to_owned(), style));
        }
        self.builder.set_marker(prefix);
    }

    /// Renders a `@def…` definition: the signature line bold, the body indented until `@end`.
    fn definition(&mut self, signature: &str) {
        self.builder.blank();
        self.blocks.push(Block::Definition);
        self.reflow();
        // The signature sits at the definition's left edge (one level out from its body).
        let base = self.builder.indent().saturating_sub(INDENT);
        self.set_marker(base, "", self.builder.palette().body);
        self.builder.set_indent(base);
        let style = self.builder.palette().heading;
        self.render_inline(signature, style);
        self.builder.end_line();
        self.builder.set_indent(base + INDENT);
    }

    /// Renders a `@menu` entry line (`* Name:: description` / `* Name: (file)node.`) — name as a link.
    fn menu_entry(&mut self, line: &str) {
        let entry = line.trim_start_matches('*').trim_start();
        let name = entry.split("::").next().unwrap_or(entry);
        let name = name.split(':').next().unwrap_or(name).trim();
        self.builder.append("• ", self.builder.palette().body);
        self.builder.append(name, self.builder.palette().link);
        self.builder.end_line();
    }

    /// Collects an `@multitable` row (`@item`/`@headitem`, cells split on `@tab`); renders at `@end
    /// multitable`. Other lines (including multi-line cell continuations) are ignored in v1.
    fn multitable_line(&mut self, trimmed: &str) {
        let Some(rest) = trimmed.strip_prefix('@') else {
            return;
        };
        let (word, arg) = split_command(rest);
        match word {
            "item" | "headitem" => {
                let cells: Vec<String> = arg
                    .split("@tab")
                    .map(|cell| cell.trim().to_owned())
                    .collect();
                if let Some(table) = self.multitable.as_mut() {
                    if word == "headitem" {
                        table.head_rows += 1;
                    }
                    table.rows.push(cells);
                }
            }
            "end" if arg == "multitable" => {
                if let Some(table) = self.multitable.take() {
                    crate::preview::render_table(&mut self.builder, &table.rows, table.head_rows);
                }
            }
            _ => {}
        }
    }

    /// Renders inline text, styling `@`-commands and resolving `@`-escapes onto `base`.
    fn render_inline(&mut self, text: &str, base: Style) {
        let chars: Vec<char> = text.chars().collect();
        let mut plain = String::new();
        let mut i = 0;
        while i < chars.len() {
            let ch = chars[i];
            if ch != '@' {
                plain.push(ch);
                i += 1;
                continue;
            }
            // Escapes and one-character commands keep accumulating into `plain` (so `a@@b` stays one
            // run, not `a` + `@b` with a spurious gap); only a real command flushes the run first.
            match chars.get(i + 1).copied() {
                // Escapes (`@@`/`@{`/`@}`) and end-of-sentence marks (`@.`/`@!`/`@?`) emit the character.
                Some(literal @ ('@' | '{' | '}' | '.' | '!' | '?')) => {
                    plain.push(literal);
                    i += 2;
                    continue;
                }
                // `@ `, `@<tab>`, `@<newline>` are a single space.
                Some(' ' | '\t' | '\n') => {
                    plain.push(' ');
                    i += 2;
                    continue;
                }
                // `@:`, `@-`, `@/` produce no output.
                Some(':' | '-' | '/') => {
                    i += 2;
                    continue;
                }
                // `@*` is a forced line break.
                Some('*') => {
                    self.flush_plain(&mut plain, base);
                    self.builder.end_line();
                    i += 2;
                    continue;
                }
                None => break,
                Some(_) => {} // a `@command` — handled below
            }
            self.flush_plain(&mut plain, base);
            i += 1; // past '@'
            let start = i;
            while i < chars.len() && chars[i].is_alphanumeric() {
                i += 1;
            }
            let command: String = chars[start..i].iter().collect();
            if chars.get(i) == Some(&'{') {
                i += 1;
                let arg_start = i;
                let mut depth = 1;
                while i < chars.len() && depth > 0 {
                    match chars[i] {
                        '{' => depth += 1,
                        '}' => depth -= 1,
                        _ => {}
                    }
                    if depth > 0 {
                        i += 1;
                    }
                }
                let arg: String = chars[arg_start..i].iter().collect();
                if i < chars.len() {
                    i += 1; // past the closing brace
                }
                self.brace_command(&command, &arg, base);
            } else {
                // A no-brace command mid-line — fall back to its name as text.
                self.builder.push_span(&command, base);
            }
        }
        self.flush_plain(&mut plain, base);
    }

    /// Styles a `@command{arg}` (recursing into `arg` so commands nest).
    fn brace_command(&mut self, command: &str, arg: &str, base: Style) {
        let palette = self.builder.palette();
        match command {
            "code" | "samp" | "file" | "command" | "option" | "env" | "kbd" | "key" | "verb"
            | "t" | "sc" | "indicateurl" | "math" => self.builder.push_span(arg, palette.code),
            "emph" | "i" | "var" | "dfn" | "cite" | "slanted" => {
                let mut style = base;
                style.attrs.italic = true;
                self.render_inline(arg, style);
            }
            "strong" | "b" => {
                let mut style = base;
                style.attrs.bold = true;
                self.render_inline(arg, style);
            }
            "email" | "url" | "uref" | "ref" | "xref" | "pxref" => {
                // The display text is the last comma-field for url/uref/email, else the node name.
                let text = arg.split(',').next_back().unwrap_or(arg).trim();
                self.builder.push_span(text, palette.link);
            }
            "w" | "asis" | "r" | "footnote" | "dmn" => self.render_inline(arg, base),
            "value" | "anchor" | "today" => {} // no @set tracking / not rendered in v1
            _ => {
                if let Some(glyph) = symbol(command) {
                    self.builder.push_span(glyph, base);
                } else {
                    // Unknown command — keep its content as text rather than dropping it.
                    self.render_inline(arg, base);
                }
            }
        }
    }

    /// Pushes accumulated plain text (if any) and clears the buffer.
    fn flush_plain(&mut self, plain: &mut String, base: Style) {
        if !plain.is_empty() {
            self.builder.push_span(plain, base);
            plain.clear();
        }
    }
}

/// The 1-based heading level for a sectioning command, or `None` if `word` isn't one.
fn heading_level(word: &str) -> Option<usize> {
    let level = match word {
        "top" | "chapter" | "unnumbered" | "appendix" | "majorheading" | "chapheading"
        | "centerchap" => 1,
        "section" | "unnumberedsec" | "appendixsec" | "appendixsection" | "heading" => 2,
        "subsection" | "unnumberedsubsec" | "appendixsubsec" | "subheading" => 3,
        "subsubsection" | "unnumberedsubsubsec" | "appendixsubsubsec" | "subsubheading" => 4,
        _ => return None,
    };
    Some(level)
}

/// Whether `word` begins a `@def…` definition block (but not the index-defining directives).
fn is_definition(word: &str) -> bool {
    word.starts_with("def")
        && !matches!(word, "defindex" | "defcodeindex" | "definfoenclose")
        && !word.ends_with('x') // @deffnx etc. are continuations, treated as plain text
}

/// Whether `word` is a directive / metadata / index command with no rendered output.
fn is_directive(word: &str) -> bool {
    matches!(
        word,
        "node"
            | "anchor"
            | "setfilename"
            | "settitle"
            | "set"
            | "clear"
            | "include"
            | "dircategory"
            | "documentencoding"
            | "documentlanguage"
            | "syncodeindex"
            | "synindex"
            | "paragraphindent"
            | "firstparagraphindent"
            | "exampleindent"
            | "finalout"
            | "bye"
            | "contents"
            | "shortcontents"
            | "summarycontents"
            | "insertcopying"
            | "printindex"
            | "listoffloats"
            | "page"
            | "need"
            | "vskip"
            | "title"
            | "subtitle"
            | "author"
            | "shorttitlepage"
            | "cindex"
            | "findex"
            | "vindex"
            | "kindex"
            | "pindex"
            | "tindex"
            | "kbdinputstyle"
            | "allowcodebreaks"
            | "frenchspacing"
            | "codequoteundirected"
            | "codequotebacktick"
            | "defindex"
            | "defcodeindex"
            | "definfoenclose"
            | "headings"
            | "setchapternewpage"
            | "everyheading"
            | "everyfooting"
    )
}

/// The replacement glyph for a no-argument symbol command (`@dots{}`, `@copyright{}`, …).
fn symbol(command: &str) -> Option<&'static str> {
    let glyph = match command {
        "dots" => "…",
        "enddots" => "….",
        "copyright" => "©",
        "registeredsymbol" => "®",
        "bullet" => "•",
        "minus" => "−",
        "result" => "⇒",
        "expansion" | "arrow" => "→",
        "error" => "error→",
        "point" => "∗",
        "equiv" => "≡",
        "tie" | "comma" => ",",
        "TeX" => "TeX",
        "LaTeX" => "LaTeX",
        _ => return None,
    };
    Some(glyph)
}

/// Splits a line's leading `@word` from the remaining argument (after a single separating space).
fn split_command(rest: &str) -> (&str, &str) {
    let end = rest
        .find(|c: char| !c.is_alphanumeric())
        .unwrap_or(rest.len());
    let (word, tail) = rest.split_at(end);
    (word, tail.trim_start())
}

/// Whether `raw` is `@end <env>` for the given environment.
fn line_ends(raw: &str, env: &str) -> bool {
    raw.trim()
        .strip_prefix("@end")
        .is_some_and(|rest| rest.trim() == env)
}

/// Whether `raw` is an `@end` for any verbatim-style environment.
fn ends_a_verbatim(raw: &str) -> bool {
    matches!(
        raw.trim().strip_prefix("@end").map(str::trim),
        Some("example" | "smallexample" | "verbatim" | "lisp" | "smalllisp" | "display" | "format")
    )
}

#[cfg(test)]
mod tests {
    use super::layout;
    use crate::preview::Palette;
    use penumbra::Theme;

    fn rendered(texi: &str, width: usize) -> Vec<String> {
        let palette = Palette::new(&Theme::steelbore());
        layout(texi, width, &palette)
            .iter()
            .map(|line| {
                line.iter()
                    .map(|(text, _)| text.as_str())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn a_section_becomes_a_heading_with_a_rule() {
        let lines = rendered("@section Introduction\n", 30);
        assert_eq!(lines[0], "Introduction");
        assert!(
            lines[1].chars().all(|c| c == '─'),
            "@section is underlined by a rule"
        );
    }

    #[test]
    fn inline_commands_and_escapes_resolve() {
        // @code{...} keeps its text; @@ becomes a literal @; @strong{...} renders its text.
        let lines = rendered("Use @code{mj} and @strong{F7}; mail a@@b.\n", 60);
        let joined = lines.join(" ");
        assert!(
            joined.contains("Use mj and F7"),
            "inline commands keep their text: {joined:?}"
        );
        assert!(
            joined.contains("a@b"),
            "@@ resolves to a literal @: {joined:?}"
        );
    }

    #[test]
    fn itemize_and_enumerate_render_markers() {
        let bullets = rendered("@itemize\n@item one\n@item two\n@end itemize\n", 30);
        assert_eq!(bullets, ["• one", "• two"]);
        let numbered = rendered("@enumerate\n@item a\n@item b\n@end enumerate\n", 30);
        assert_eq!(numbered, ["1. a", "2. b"]);
    }

    #[test]
    fn a_multitable_renders_aligned_columns() {
        let texi = "@multitable @columnfractions .5 .5\n@headitem Key @tab Action\n\
                    @item F7 @tab Preview\n@end multitable\n";
        let lines = rendered(texi, 40);
        assert_eq!(lines[0], "Key │ Action", "@headitem is the header row");
        assert!(lines[1].contains('┼'), "a rule under the header");
        assert_eq!(
            lines[2], "F7  │ Preview",
            "@item cells align under the header"
        );
    }

    #[test]
    fn an_inline_command_before_punctuation_has_no_spurious_space() {
        // The space-preserving layout: `@code{F7}.` renders as `F7.`, not `F7 .`.
        assert_eq!(rendered("Press @code{F7}.", 40), ["Press F7."]);
    }

    #[test]
    fn an_example_block_is_verbatim_and_inset() {
        let lines = rendered("@example\nlet x = 1;\n@end example\n", 40);
        assert!(
            lines.iter().any(|line| line == "  let x = 1;"),
            "@example lines are inset and verbatim: {lines:?}"
        );
    }

    #[test]
    fn comments_and_skipped_conditionals_produce_nothing() {
        let lines = rendered(
            "@c a comment\n@iftex\nprinted only\n@end iftex\nVisible.\n",
            40,
        );
        let joined = lines.join("");
        assert!(!joined.contains("comment"), "@c lines are dropped");
        assert!(
            !joined.contains("printed only"),
            "@iftex content is skipped"
        );
        assert!(joined.contains("Visible."), "ordinary text renders");
    }

    #[test]
    fn a_code_command_is_styled_in_the_code_token() {
        let palette = Palette::new(&Theme::steelbore());
        let lines = layout("@code{mj}", 20, &palette);
        let span = lines[0]
            .iter()
            .find(|(text, _)| text == "mj")
            .expect("the @code text");
        assert_eq!(
            span.1.fg,
            Theme::steelbore().accent,
            "@code uses the Steel Blue token"
        );
    }
}
