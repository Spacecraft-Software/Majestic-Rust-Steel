// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Incremental in-buffer search (the `find` command).
//!
//! [`Search`] holds the query, every (non-overlapping, case-sensitive) match in the buffer, and
//! which match is active. The host edits the query as the user types, jumps the cursor to the active
//! match, tints all matches, and shows `query [i/N]` in the status line. Regex and case-folding are
//! deliberate follow-ups; v1 is plain substring search.

use std::ops::Range;

/// Incremental in-buffer search state.
#[derive(Clone, Debug)]
pub struct Search {
    /// The current search query.
    query: String,
    /// Every match of `query` in the buffer, in document order (byte ranges).
    matches: Vec<Range<usize>>,
    /// Index into `matches` of the active (cursor) match.
    active: usize,
    /// The cursor byte when the search opened — restored if the search is cancelled.
    origin: usize,
}

impl Search {
    /// Opens a search anchored at `origin` (the cursor when it started, restored on cancel).
    #[must_use]
    pub fn new(origin: usize) -> Self {
        Self {
            query: String::new(),
            matches: Vec::new(),
            active: 0,
            origin,
        }
    }

    /// The current query text.
    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    /// The cursor byte the search started from (the cancel target).
    #[must_use]
    pub fn origin(&self) -> usize {
        self.origin
    }

    /// Every match, in document order.
    #[must_use]
    pub fn matches(&self) -> &[Range<usize>] {
        &self.matches
    }

    /// How many matches the query has.
    #[must_use]
    pub fn match_count(&self) -> usize {
        self.matches.len()
    }

    /// The active match's byte range, or `None` when the query is empty or matches nothing.
    #[must_use]
    pub fn active_match(&self) -> Option<Range<usize>> {
        self.matches.get(self.active).cloned()
    }

    /// The 1-based index of the active match (for an `i/N` display), or `0` when there are none.
    #[must_use]
    pub fn active_index(&self) -> usize {
        if self.matches.is_empty() {
            0
        } else {
            self.active + 1
        }
    }

    /// Appends `c` to the query and recomputes against `text`, selecting the first match at/after
    /// `from` so the search jumps forward as you type.
    pub fn push(&mut self, c: char, text: &str, from: usize) {
        self.query.push(c);
        self.recompute(text, from);
    }

    /// Removes the last query character and recomputes (a no-op on an empty query).
    pub fn backspace(&mut self, text: &str, from: usize) {
        self.query.pop();
        self.recompute(text, from);
    }

    /// Recomputes all (non-overlapping) matches of the query in `text`; the active match becomes the
    /// first one starting at/after `from`, wrapping to the first when none are later. An empty query
    /// clears the matches.
    ///
    /// **Smart-case:** matching folds ASCII case when the query is all-lowercase (so `foo` finds
    /// `Foo`/`FOO`), and is exact when the query contains any uppercase letter (so `Foo` finds only
    /// `Foo`). Non-ASCII letters are matched exactly either way (full Unicode case-folding is a
    /// follow-up). Only char-boundary-aligned matches are kept, so a window never splits a glyph.
    pub fn recompute(&mut self, text: &str, from: usize) {
        self.matches.clear();
        self.active = 0;
        if self.query.is_empty() {
            return;
        }
        let fold = !self.query.chars().any(char::is_uppercase);
        let query = self.query.as_bytes();
        let bytes = text.as_bytes();
        let mut i = 0;
        while i + query.len() <= bytes.len() {
            let window = &bytes[i..i + query.len()];
            let hit = if fold {
                window.eq_ignore_ascii_case(query)
            } else {
                window == query
            };
            if hit && text.is_char_boundary(i) && text.is_char_boundary(i + query.len()) {
                self.matches.push(i..i + query.len());
                i += query.len(); // non-overlapping
            } else {
                i += 1;
            }
        }
        // Land on the first match at/after the cursor, else wrap to the first.
        self.active = self
            .matches
            .iter()
            .position(|m| m.start >= from)
            .unwrap_or(0);
    }

    /// Advances to the next match, wrapping. A no-op when there are none.
    pub fn next(&mut self) {
        if !self.matches.is_empty() {
            self.active = (self.active + 1) % self.matches.len();
        }
    }

    /// Steps to the previous match, wrapping. A no-op when there are none.
    pub fn prev(&mut self) {
        if !self.matches.is_empty() {
            self.active = (self.active + self.matches.len() - 1) % self.matches.len();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Search;

    #[test]
    fn incremental_query_finds_and_advances_matches() {
        let text = "foo bar foo baz foo";
        let mut search = Search::new(0);
        // Typing builds the query and finds every occurrence (positions 0, 8, 16).
        search.push('f', text, 0);
        search.push('o', text, 0);
        search.push('o', text, 0);
        assert_eq!(search.query(), "foo");
        assert_eq!(search.match_count(), 3);
        assert_eq!(search.active_match(), Some(0..3));
        assert_eq!(search.active_index(), 1);

        // Next/prev cycle through them and wrap.
        search.next();
        assert_eq!(search.active_match(), Some(8..11));
        search.next();
        assert_eq!(search.active_match(), Some(16..19));
        search.next();
        assert_eq!(search.active_match(), Some(0..3)); // wrapped
        search.prev();
        assert_eq!(search.active_match(), Some(16..19)); // wrapped back
    }

    #[test]
    fn search_lands_on_the_first_match_at_or_after_the_cursor() {
        let text = "foo bar foo baz foo";
        let mut search = Search::new(10); // cursor past the first `foo`
        search.push('f', text, 10);
        search.push('o', text, 10);
        search.push('o', text, 10);
        // The first match at/after byte 10 is the one at 16.
        assert_eq!(search.active_match(), Some(16..19));
    }

    #[test]
    fn backspace_recomputes_and_empty_query_clears() {
        let text = "alpha beta alps";
        let mut search = Search::new(0);
        for c in "alp".chars() {
            search.push(c, text, 0);
        }
        assert_eq!(search.match_count(), 2); // "alp" in alpha and alps
        for c in "ha".chars() {
            search.push(c, text, 0);
        }
        assert_eq!(search.query(), "alpha");
        assert_eq!(search.match_count(), 1);
        // Backspacing back to "alp" restores both matches; emptying clears everything.
        search.backspace(text, 0);
        search.backspace(text, 0);
        assert_eq!(search.query(), "alp");
        assert_eq!(search.match_count(), 2);
        for _ in 0..3 {
            search.backspace(text, 0);
        }
        assert!(search.query().is_empty());
        assert_eq!(search.match_count(), 0);
        assert_eq!(search.active_match(), None);
    }

    #[test]
    fn multibyte_text_does_not_panic_and_finds_matches() {
        let text = "café au café"; // 'é' is two bytes
        let mut search = Search::new(0);
        for c in "café".chars() {
            search.push(c, text, 0);
        }
        assert_eq!(search.match_count(), 2);
        assert_eq!(search.active_index(), 1);
    }

    #[test]
    fn smart_case_folds_an_all_lowercase_query() {
        let text = "Foo foo FOO";
        let mut search = Search::new(0);
        for c in "foo".chars() {
            search.push(c, text, 0);
        }
        // All-lowercase query is case-insensitive — every casing matches.
        assert_eq!(search.match_count(), 3);
    }

    #[test]
    fn an_uppercase_in_the_query_makes_it_case_sensitive() {
        let text = "Foo foo FOO";
        let mut search = Search::new(0);
        for c in "Foo".chars() {
            search.push(c, text, 0);
        }
        // The uppercase `F` forces an exact match — only `Foo` qualifies.
        assert_eq!(search.match_count(), 1);
        assert_eq!(search.active_match(), Some(0..3));
    }
}
