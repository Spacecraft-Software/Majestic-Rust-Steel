// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! A small subsequence fuzzy matcher for the finder/command palette (UI.md global UX).
//!
//! [`fuzzy_match`] scores a candidate against a query: the query must appear as a
//! case-insensitive subsequence, and the score rewards consecutive runs and matches at word
//! boundaries (after `/`, `_`, `-`, `.`, space) so that `mn` ranks `main.rs` above `human`.
//! [`fuzzy_rank`] applies it across a list and returns the surviving indices, best first.

/// Scores `candidate` against `query`, or `None` when `query` is not a subsequence of it.
///
/// Higher is better. An empty query matches everything with score `0`. Matching is greedy
/// (first occurrence of each query character), which is fast and good enough for a picker.
#[must_use]
pub fn fuzzy_match(query: &str, candidate: &str) -> Option<i32> {
    let query: Vec<char> = query.chars().collect();
    if query.is_empty() {
        return Some(0);
    }
    let mut matched = 0usize;
    let mut score = 0i32;
    let mut prev_matched = false;
    let mut prev: Option<char> = None;
    for (index, c) in candidate.chars().enumerate() {
        if matched < query.len() && c.eq_ignore_ascii_case(&query[matched]) {
            let mut bonus = 1;
            if prev_matched {
                bonus += 10; // consecutive run — the strongest signal
            }
            if index == 0 || prev.is_some_and(is_boundary) {
                bonus += 5; // start of a word / path segment
            }
            score += bonus;
            matched += 1;
            prev_matched = true;
        } else {
            prev_matched = false;
        }
        prev = Some(c);
    }
    (matched == query.len()).then_some(score)
}

/// Ranks `candidates` by descending fuzzy score against `query`, dropping non-matches.
///
/// Ties keep the original input order (stable). Returns the surviving indices into `candidates`.
#[must_use]
pub fn fuzzy_rank<S: AsRef<str>>(query: &str, candidates: &[S]) -> Vec<usize> {
    let mut scored: Vec<(usize, i32)> = candidates
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| {
            fuzzy_match(query, candidate.as_ref()).map(|score| (index, score))
        })
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    scored.into_iter().map(|(index, _)| index).collect()
}

/// Whether `c` separates words/path segments, so the next character begins a new one.
fn is_boundary(c: char) -> bool {
    matches!(c, '/' | '\\' | '_' | '-' | '.' | ' ')
}

#[cfg(test)]
mod tests {
    use super::{fuzzy_match, fuzzy_rank};

    #[test]
    fn matches_subsequences_case_insensitively() {
        assert!(fuzzy_match("fb", "foobar").is_some());
        assert!(fuzzy_match("FB", "foobar").is_some());
        assert!(fuzzy_match("fzf", "fuzzy_finder").is_some());
        assert!(fuzzy_match("xyz", "foobar").is_none());
        // An empty query matches anything.
        assert_eq!(fuzzy_match("", "anything"), Some(0));
    }

    #[test]
    fn consecutive_and_boundary_matches_score_higher() {
        // Consecutive run beats a scattered match.
        assert!(fuzzy_match("foo", "foobar") > fuzzy_match("foo", "f_o_o_x"));
        // A match at a path-segment boundary beats one mid-word.
        assert!(fuzzy_match("m", "src/main.rs") > fuzzy_match("m", "human"));
    }

    #[test]
    fn rank_orders_best_first_and_drops_non_matches() {
        let candidates = ["human", "src/main.rs", "readme.md", "mod.rs"];
        let ranked = fuzzy_rank("mn", &candidates);
        // `main` matches with `m` on a path boundary, outranking the mid-word `m..n` in `human`.
        // `readme.md`/`mod.rs` have no `n` after their `m`, so they are dropped.
        assert_eq!(ranked, vec![1, 0]);
    }

    #[test]
    fn empty_query_keeps_input_order() {
        let candidates = ["a", "b", "c"];
        assert_eq!(fuzzy_rank("", &candidates), vec![0, 1, 2]);
    }
}
