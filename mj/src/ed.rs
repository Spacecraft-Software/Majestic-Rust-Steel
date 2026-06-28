// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! `mj ed` — the classic line editor (M4): the minimal editing surface that runs over any TTY, even a
//! dumb terminal or serial line, with no raw mode and no screen control. It reads commands from stdin
//! and writes to stdout, in the lineage of Unix `ed` (POSIX) / GNU `ed`.
//!
//! This is a deliberately *separate* model from the TUI: a line editor is line-oriented, so the buffer
//! is a `Vec<String>` of lines (not Stratum's rope) with one-level snapshot undo — the honest shape
//! for `ed`. Addresses (`.`, `$`, `N`, `/re/`, `?re?`, `+`/`-`, `,`, `;`, `%`, `'x`) select lines for
//! the commands `a i c d p n l = s g v G? m t j k r e f w q Q u P h H !`.
//!
//! Regex note: patterns use **extended** regular expressions (the `regex` crate / ERE), not historical
//! BRE — `(`, `)`, `{`, `}`, `|`, `+`, `?` are operators (escape them for literals). Replacements honor
//! `ed` conventions: `&` = whole match, `\1`–`\9` = groups, `\&` / `\\` literal.
//!
//! Deferred (noted): multi-command `g`/`v` bodies (v1 runs one of `p`/`n`/`l`/`d`/`s` per match),
//! newlines inside a replacement, and the `l` command's full unambiguous escaping (v1 marks line ends
//! with `$`).
//
// Rust guideline compliant 2026-05-18

use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};

use regex::{Captures, Regex};

/// Runs the `mj ed` line editor on an optional initial `path`, reading commands from stdin until `q`
/// (or end of input).
///
/// # Errors
/// Returns an [`io::Error`] only on an unrecoverable stdin/stdout failure; per-command problems (bad
/// address, no match, …) are reported in-band as `ed` does (`?`, with `h`/`H` for detail).
pub fn run(path: Option<&str>) -> io::Result<()> {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut out = io::stdout().lock();
    let mut ed = Ed::new();
    if let Some(path) = path {
        ed.filename = Some(path.to_owned());
        match std::fs::read_to_string(path) {
            Ok(text) => {
                ed.set_text(&text);
                writeln!(out, "{}", text.len())?;
            }
            // A missing file is normal: `ed file` on a new file starts empty, naming it.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => writeln!(out, "{path}: {error}")?,
        }
    }
    let mut command = String::new();
    loop {
        if let Some(prompt) = &ed.prompt {
            write!(out, "{prompt}")?;
            out.flush()?;
        }
        command.clear();
        if reader.read_line(&mut command)? == 0 {
            // End of input behaves like `q`, but a modified buffer warns once first (GNU `ed`).
            if ed.modified && !ed.warned {
                ed.warned = true;
                ed.fail("warning: buffer modified", &mut out)?;
                continue;
            }
            break;
        }
        let line = command.trim_end_matches(['\n', '\r']);
        match ed.execute(line, &mut reader, &mut out) {
            Ok(Flow::Continue) => {}
            Ok(Flow::Quit) => break,
            Err(message) => ed.fail(&message, &mut out)?,
        }
    }
    out.flush()
}

/// Whether the command loop should continue or exit.
enum Flow {
    Continue,
    Quit,
}

/// How a line is printed (`p` plain, `n` numbered, `l` with a visible end-of-line marker).
#[derive(Clone, Copy)]
enum PrintMode {
    Plain,
    Numbered,
    Unambiguous,
}

/// The line-editor state.
struct Ed {
    /// The buffer, one entry per line (line `N` is `lines[N - 1]`).
    lines: Vec<String>,
    /// The current line, 1-based (`0` only when the buffer is empty).
    current: usize,
    filename: Option<String>,
    modified: bool,
    /// One-level undo: the buffer + current line before the last modifying command (`u` toggles).
    undo: Option<(Vec<String>, usize)>,
    last_error: Option<String>,
    /// `H`: print the error message immediately, not just `?`.
    verbose_errors: bool,
    /// `P`: the command prompt, when enabled.
    prompt: Option<String>,
    /// The most recent regular expression, reused by an empty `//` / `s//`.
    last_pattern: Option<String>,
    /// Line marks set by `k` (`'x` addresses them).
    marks: BTreeMap<char, usize>,
    /// Whether a modified-buffer quit was already warned about (the second `q` then quits).
    warned: bool,
}

impl Ed {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            current: 0,
            filename: None,
            modified: false,
            undo: None,
            last_error: None,
            verbose_errors: false,
            prompt: None,
            last_pattern: None,
            marks: BTreeMap::new(),
            warned: false,
        }
    }

    /// Replaces the buffer with `text` split into lines, leaving the current line at the last.
    fn set_text(&mut self, text: &str) {
        self.lines = if text.is_empty() {
            Vec::new()
        } else {
            text.strip_suffix('\n')
                .unwrap_or(text)
                .split('\n')
                .map(str::to_owned)
                .collect()
        };
        self.current = self.lines.len();
        self.marks.clear();
    }

    /// The last line number (`$`), `0` for an empty buffer.
    fn last(&self) -> i64 {
        i64::try_from(self.lines.len()).unwrap_or(i64::MAX)
    }

    /// The current line number (`.`) as a signed value, for address arithmetic.
    fn cur(&self) -> i64 {
        i64::try_from(self.current).unwrap_or(i64::MAX)
    }

    /// Reports an error: records it, prints `?`, and (under `H`) the message.
    fn fail(&mut self, message: &str, out: &mut dyn Write) -> io::Result<()> {
        self.last_error = Some(message.to_owned());
        writeln!(out, "?")?;
        if self.verbose_errors {
            writeln!(out, "{message}")?;
        }
        Ok(())
    }

    /// Saves the buffer for `u` before a modifying command.
    fn checkpoint(&mut self) {
        self.undo = Some((self.lines.clone(), self.current));
    }

    /// Parses and runs one command line.
    fn execute(
        &mut self,
        line: &str,
        reader: &mut dyn BufRead,
        out: &mut dyn Write,
    ) -> Result<Flow, String> {
        let mut lex = Lexer::new(line);
        let (start, end, ranged) = self.parse_range(&mut lex)?;
        let was_warned = self.warned;
        self.warned = false;
        let command = lex.bump();
        match command {
            // A bare address prints (and moves to) the addressed line.
            None => {
                let line = self.resolve(end.or(start).unwrap_or(self.cur() + 1))?;
                self.print_range(line, line, PrintMode::Plain, out)
                    .map_err(|e| e.to_string())?;
                Ok(Flow::Continue)
            }
            Some('p') => self.print_command(start, end, PrintMode::Plain, &mut lex, out),
            Some('n') => self.print_command(start, end, PrintMode::Numbered, &mut lex, out),
            Some('l') => self.print_command(start, end, PrintMode::Unambiguous, &mut lex, out),
            Some('a') => self.cmd_insert(end, false, reader),
            Some('i') => self.cmd_insert(end, true, reader),
            Some('c') => self.cmd_change(start, end, ranged, reader),
            Some('d') => self.cmd_delete(start, end, ranged),
            Some('=') => self.cmd_line_number(end, out),
            Some('s') => self.cmd_substitute(start, end, ranged, &mut lex, out),
            Some('g') => self.cmd_global(start, end, ranged, false, &mut lex, out),
            Some('v') => self.cmd_global(start, end, ranged, true, &mut lex, out),
            Some('m') => self.cmd_transfer(start, end, ranged, &mut lex, true),
            Some('t') => self.cmd_transfer(start, end, ranged, &mut lex, false),
            Some('j') => self.cmd_join(start, end, ranged),
            Some('k') => self.cmd_mark(end, &mut lex),
            Some('r') => self.cmd_read(end, &mut lex, out),
            Some('e') => self.cmd_edit(was_warned, &mut lex, out),
            Some('f') => self.cmd_filename(&mut lex, out),
            Some('w') => self.cmd_write(start, end, ranged, &mut lex, out),
            Some('q') => self.cmd_quit(was_warned, false),
            Some('Q') => Ok(Flow::Quit),
            Some('u') => self.cmd_undo(),
            Some('P') => {
                self.prompt = if self.prompt.is_some() {
                    None
                } else {
                    Some("*".to_owned())
                };
                Ok(Flow::Continue)
            }
            Some('h') => {
                if let Some(message) = self.last_error.clone() {
                    writeln!(out, "{message}").map_err(|e| e.to_string())?;
                }
                Ok(Flow::Continue)
            }
            Some('H') => {
                self.verbose_errors = !self.verbose_errors;
                if let (true, Some(message)) = (self.verbose_errors, self.last_error.clone()) {
                    writeln!(out, "{message}").map_err(|e| e.to_string())?;
                }
                Ok(Flow::Continue)
            }
            Some('!') => run_shell(&mut lex, out),
            Some(other) => Err(format!("unknown command `{other}`")),
        }
    }

    /// `q` (or `wq`'s tail): quits, warning once if the buffer is modified.
    fn cmd_quit(&mut self, was_warned: bool, forced: bool) -> Result<Flow, String> {
        if self.modified && !was_warned && !forced {
            self.warned = true;
            return Err("warning: buffer modified".to_owned());
        }
        Ok(Flow::Quit)
    }

    // --- Addressing -------------------------------------------------------------------------------

    /// Parses an optional address range, returning `(start, end, an explicit range was given)`.
    fn parse_range(&mut self, lex: &mut Lexer) -> Result<(Option<i64>, Option<i64>, bool), String> {
        if lex.eat('%') {
            return Ok((Some(1), Some(self.last()), true));
        }
        let first = self.parse_addr(lex)?;
        match lex.peek() {
            Some(separator @ (',' | ';')) => {
                lex.bump();
                if separator == ';' {
                    // `;` advances the current line so a search in the second address starts there.
                    if let Some(value) = first {
                        if let Ok(line) = self.resolve(value) {
                            self.current = line;
                        }
                    }
                }
                let second = self.parse_addr(lex)?;
                let default_start = if separator == ';' { self.cur() } else { 1 };
                Ok((
                    Some(first.unwrap_or(default_start)),
                    Some(second.unwrap_or(self.last())),
                    true,
                ))
            }
            _ => Ok((first, first, false)),
        }
    }

    /// Parses one address (a base optionally followed by `+`/`-` offsets), or `None` if absent.
    fn parse_addr(&self, lex: &mut Lexer) -> Result<Option<i64>, String> {
        let mut value: Option<i64> = match lex.peek() {
            Some('.') => {
                lex.bump();
                Some(self.cur())
            }
            Some('$') => {
                lex.bump();
                Some(self.last())
            }
            Some('0'..='9') => lex.number().map(|n| i64::try_from(n).unwrap_or(i64::MAX)),
            Some('\'') => {
                lex.bump();
                let mark = lex.bump().ok_or("missing mark letter")?;
                let line = *self.marks.get(&mark).ok_or("undefined mark")?;
                Some(i64::try_from(line).unwrap_or(i64::MAX))
            }
            Some(delim @ ('/' | '?')) => {
                lex.bump();
                let pattern = lex.take_delimited(delim);
                Some(i64::try_from(self.search(&pattern, delim == '/')?).unwrap_or(i64::MAX))
            }
            _ => None,
        };
        while let Some(sign @ ('+' | '-' | '^')) = lex.peek() {
            lex.bump();
            let step = lex
                .number()
                .map_or(1, |n| i64::try_from(n).unwrap_or(i64::MAX));
            let base = value.unwrap_or(self.cur());
            value = Some(if sign == '+' {
                base + step
            } else {
                base - step
            });
        }
        Ok(value)
    }

    /// Searches for `pattern` (empty reuses the last) forward or backward from the current line,
    /// wrapping around, returning the 1-based line number.
    fn search(&self, pattern: &str, forward: bool) -> Result<usize, String> {
        let source = if pattern.is_empty() {
            self.last_pattern.clone().ok_or("no previous pattern")?
        } else {
            pattern.to_owned()
        };
        let regex = Regex::new(&source).map_err(|e| format!("invalid pattern: {e}"))?;
        let count = self.lines.len();
        if count == 0 {
            return Err("empty buffer".to_owned());
        }
        // Walk the other lines in order, wrapping, starting just past the current line.
        for offset in 1..=count {
            let index = if forward {
                (self.current + offset - 1) % count
            } else {
                (self.current + count - offset - 1) % count
            };
            if regex.is_match(&self.lines[index]) {
                return Ok(index + 1);
            }
        }
        Err("no match".to_owned())
    }

    /// Validates a 1-based address against the buffer (`1..=$`), returning it as an index base.
    fn resolve(&self, address: i64) -> Result<usize, String> {
        usize::try_from(address)
            .ok()
            .filter(|&line| line >= 1 && line <= self.lines.len())
            .ok_or_else(|| "invalid address".to_owned())
    }

    /// Validates a destination address (`0..=$`, where `0` means "before the first line").
    fn resolve_dest(&self, address: i64) -> Result<usize, String> {
        usize::try_from(address)
            .ok()
            .filter(|&line| line <= self.lines.len())
            .ok_or_else(|| "invalid address".to_owned())
    }

    /// Resolves a command's `(start, end)` range, applying `default` when no address was given.
    fn range(
        &self,
        start: Option<i64>,
        end: Option<i64>,
        default: (i64, i64),
    ) -> Result<(usize, usize), String> {
        let start = self.resolve(start.unwrap_or(default.0))?;
        let end = self.resolve(end.unwrap_or(default.1))?;
        if start > end {
            return Err("addresses out of order".to_owned());
        }
        Ok((start, end))
    }

    // --- Printing ---------------------------------------------------------------------------------

    /// Runs `p`/`n`/`l` over the range (default: the current line).
    fn print_command(
        &mut self,
        start: Option<i64>,
        end: Option<i64>,
        mode: PrintMode,
        _lex: &mut Lexer,
        out: &mut dyn Write,
    ) -> Result<Flow, String> {
        let (start, end) = self.range(start, end, (self.cur(), self.cur()))?;
        self.print_range(start, end, mode, out)
            .map_err(|e| e.to_string())?;
        self.current = end;
        Ok(Flow::Continue)
    }

    /// Prints lines `start..=end` in `mode`.
    fn print_range(
        &self,
        start: usize,
        end: usize,
        mode: PrintMode,
        out: &mut dyn Write,
    ) -> io::Result<()> {
        for line in start..=end {
            let text = &self.lines[line - 1];
            match mode {
                PrintMode::Plain => writeln!(out, "{text}")?,
                PrintMode::Numbered => writeln!(out, "{line}\t{text}")?,
                PrintMode::Unambiguous => writeln!(out, "{text}$")?,
            }
        }
        Ok(())
    }

    /// `=` — prints the addressed line number (default `$`).
    fn cmd_line_number(&mut self, end: Option<i64>, out: &mut dyn Write) -> Result<Flow, String> {
        let line = end.unwrap_or(self.last());
        writeln!(out, "{line}").map_err(|e| e.to_string())?;
        Ok(Flow::Continue)
    }

    // --- Input commands ---------------------------------------------------------------------------

    /// `a`/`i` — reads input lines (until a sole `.`) and inserts them after (`a`) or before (`i`) the
    /// addressed line.
    fn cmd_insert(
        &mut self,
        addr: Option<i64>,
        before: bool,
        reader: &mut dyn BufRead,
    ) -> Result<Flow, String> {
        let target = addr.unwrap_or(self.cur());
        let at = self.resolve_dest(target)?;
        let at = if before { at.saturating_sub(1) } else { at };
        let input = read_input(reader).map_err(|e| e.to_string())?;
        if input.is_empty() {
            return Ok(Flow::Continue);
        }
        self.checkpoint();
        let added = input.len();
        self.splice(at, 0, input);
        self.current = at + added;
        self.modified = true;
        Ok(Flow::Continue)
    }

    /// `c` — replaces the range with freshly read input lines.
    fn cmd_change(
        &mut self,
        start: Option<i64>,
        end: Option<i64>,
        _ranged: bool,
        reader: &mut dyn BufRead,
    ) -> Result<Flow, String> {
        let (start, end) = self.range(start, end, (self.cur(), self.cur()))?;
        let input = read_input(reader).map_err(|e| e.to_string())?;
        self.checkpoint();
        let added = input.len();
        self.splice(start - 1, end - start + 1, input);
        self.current = if added == 0 {
            start.saturating_sub(1)
        } else {
            start - 1 + added
        };
        self.modified = true;
        Ok(Flow::Continue)
    }

    /// `d` — deletes the range.
    fn cmd_delete(
        &mut self,
        start: Option<i64>,
        end: Option<i64>,
        _ranged: bool,
    ) -> Result<Flow, String> {
        let (start, end) = self.range(start, end, (self.cur(), self.cur()))?;
        self.checkpoint();
        self.splice(start - 1, end - start + 1, Vec::new());
        self.current = start
            .min(self.lines.len())
            .max(usize::from(!self.lines.is_empty()));
        self.modified = true;
        Ok(Flow::Continue)
    }

    /// Replaces `count` lines at index `at` with `replacement` (the one buffer-mutating primitive).
    fn splice(&mut self, at: usize, count: usize, replacement: Vec<String>) {
        let end = (at + count).min(self.lines.len());
        self.lines.splice(at..end, replacement).for_each(drop);
    }

    // --- Substitute -------------------------------------------------------------------------------

    /// `s/re/repl/flags` — substitutes over the range.
    fn cmd_substitute(
        &mut self,
        start: Option<i64>,
        end: Option<i64>,
        _ranged: bool,
        lex: &mut Lexer,
        out: &mut dyn Write,
    ) -> Result<Flow, String> {
        let (start, end) = self.range(start, end, (self.cur(), self.cur()))?;
        let spec = self.parse_subst(lex)?;
        if !(start..=end).any(|line| spec.regex.is_match(&self.lines[line - 1])) {
            return Err("no match".to_owned());
        }
        self.checkpoint();
        let mut last = start;
        for line in start..=end {
            if spec.regex.is_match(&self.lines[line - 1]) {
                self.lines[line - 1] = substitute(&spec, &self.lines[line - 1]);
                last = line;
            }
        }
        self.current = last;
        self.modified = true;
        if spec.print {
            self.print_range(last, last, PrintMode::Plain, out)
                .map_err(|e| e.to_string())?;
        }
        Ok(Flow::Continue)
    }

    /// Parses the `s` command's `/re/repl/[g][N][p]` specification.
    fn parse_subst(&mut self, lex: &mut Lexer) -> Result<Subst, String> {
        let delim = lex.bump().ok_or("missing delimiter")?;
        if delim.is_alphanumeric() || delim == ' ' {
            return Err("invalid delimiter".to_owned());
        }
        let pattern = lex.take_delimited(delim);
        let replacement = lex.take_delimited(delim);
        let mut global = false;
        let mut nth = 0;
        let mut print = false;
        while let Some(flag) = lex.peek() {
            match flag {
                'g' => global = true,
                'p' => print = true,
                '0'..='9' => {
                    nth = lex.number().unwrap_or(0);
                    continue;
                }
                _ => break,
            }
            lex.bump();
        }
        let source = if pattern.is_empty() {
            self.last_pattern.clone().ok_or("no previous pattern")?
        } else {
            pattern
        };
        let regex = Regex::new(&source).map_err(|e| format!("invalid pattern: {e}"))?;
        self.last_pattern = Some(source);
        Ok(Subst {
            regex,
            replacement,
            global,
            nth,
            print,
        })
    }

    // --- Global -----------------------------------------------------------------------------------

    /// `g/re/cmd` (and `v` for the inverse) — runs `cmd` on each matching line. v1 supports a single
    /// `p`/`n`/`l`/`d`/`s` body.
    fn cmd_global(
        &mut self,
        start: Option<i64>,
        end: Option<i64>,
        _ranged: bool,
        invert: bool,
        lex: &mut Lexer,
        out: &mut dyn Write,
    ) -> Result<Flow, String> {
        let (start, end) = self.range(start, end, (1, self.last()))?;
        let delim = lex.bump().ok_or("missing delimiter")?;
        let pattern = lex.take_delimited(delim);
        let source = if pattern.is_empty() {
            self.last_pattern.clone().ok_or("no previous pattern")?
        } else {
            pattern
        };
        let regex = Regex::new(&source).map_err(|e| format!("invalid pattern: {e}"))?;
        self.last_pattern = Some(source);
        let targets: Vec<usize> = (start..=end)
            .filter(|&line| regex.is_match(&self.lines[line - 1]) != invert)
            .collect();
        let body = lex.rest();
        let body = body.trim();
        match body.chars().next().unwrap_or('p') {
            verb @ ('p' | 'n' | 'l') => {
                let mode = match verb {
                    'n' => PrintMode::Numbered,
                    'l' => PrintMode::Unambiguous,
                    _ => PrintMode::Plain,
                };
                for &line in &targets {
                    self.print_range(line, line, mode, out)
                        .map_err(|e| e.to_string())?;
                }
                if let Some(&last) = targets.last() {
                    self.current = last;
                }
            }
            'd' => {
                self.checkpoint();
                for &line in targets.iter().rev() {
                    self.splice(line - 1, 1, Vec::new());
                }
                if !targets.is_empty() {
                    self.modified = true;
                    self.current = self.lines.len().max(usize::from(!self.lines.is_empty()));
                }
            }
            's' => {
                let mut body_lex = Lexer::new(body);
                body_lex.bump(); // consume 's'
                let spec = self.parse_subst(&mut body_lex)?;
                self.checkpoint();
                let mut any = false;
                for &line in &targets {
                    if spec.regex.is_match(&self.lines[line - 1]) {
                        self.lines[line - 1] = substitute(&spec, &self.lines[line - 1]);
                        self.current = line;
                        any = true;
                    }
                }
                self.modified |= any;
            }
            other => {
                return Err(format!(
                    "unsupported global command `{other}` (use p/n/l/d/s)"
                ))
            }
        }
        Ok(Flow::Continue)
    }

    // --- Move / copy / join -----------------------------------------------------------------------

    /// `m`/`t` — moves (`m`) or copies (`t`) the range to after a destination address.
    fn cmd_transfer(
        &mut self,
        start: Option<i64>,
        end: Option<i64>,
        _ranged: bool,
        lex: &mut Lexer,
        moving: bool,
    ) -> Result<Flow, String> {
        let (start, end) = self.range(start, end, (self.cur(), self.cur()))?;
        let dest_addr = self.parse_addr(lex)?.ok_or("destination required")?;
        let dest = self.resolve_dest(dest_addr)?;
        if moving && dest >= start - 1 && dest <= end {
            return Err("invalid destination".to_owned());
        }
        self.checkpoint();
        let block: Vec<String> = self.lines[start - 1..end].to_vec();
        let count = block.len();
        if moving {
            self.splice(start - 1, count, Vec::new());
            // Account for removed lines preceding the destination.
            let adjusted = if dest >= start { dest - count } else { dest };
            self.splice(adjusted, 0, block);
            self.current = adjusted + count;
        } else {
            self.splice(dest, 0, block);
            self.current = dest + count;
        }
        self.modified = true;
        Ok(Flow::Continue)
    }

    /// `j` — joins the range into one line (default: the current line and the next).
    fn cmd_join(
        &mut self,
        start: Option<i64>,
        end: Option<i64>,
        _ranged: bool,
    ) -> Result<Flow, String> {
        let default_end = (self.cur() + 1).min(self.last());
        let (start, end) = self.range(start, end, (self.cur(), default_end))?;
        if start == end {
            return Ok(Flow::Continue); // nothing to join
        }
        self.checkpoint();
        let joined: String = self.lines[start - 1..end].concat();
        self.splice(start - 1, end - start + 1, vec![joined]);
        self.current = start;
        self.modified = true;
        Ok(Flow::Continue)
    }

    /// `k` — marks the addressed line with a letter.
    fn cmd_mark(&mut self, end: Option<i64>, lex: &mut Lexer) -> Result<Flow, String> {
        let line = self.resolve(end.unwrap_or(self.cur()))?;
        let mark = lex
            .bump()
            .filter(char::is_ascii_lowercase)
            .ok_or("expected a-z mark")?;
        self.marks.insert(mark, line);
        Ok(Flow::Continue)
    }

    // --- Files ------------------------------------------------------------------------------------

    /// `r [file]` — reads a file in after the addressed line (default end of buffer).
    fn cmd_read(
        &mut self,
        end: Option<i64>,
        lex: &mut Lexer,
        out: &mut dyn Write,
    ) -> Result<Flow, String> {
        let at = self.resolve_dest(end.unwrap_or(self.last()))?;
        let name = self.file_arg(lex)?;
        let text = std::fs::read_to_string(&name).map_err(|e| format!("{name}: {e}"))?;
        writeln!(out, "{}", text.len()).map_err(|e| e.to_string())?;
        let added: Vec<String> = if text.is_empty() {
            Vec::new()
        } else {
            text.strip_suffix('\n')
                .unwrap_or(&text)
                .split('\n')
                .map(str::to_owned)
                .collect()
        };
        self.checkpoint();
        let count = added.len();
        self.splice(at, 0, added);
        if count > 0 {
            self.current = at + count;
            self.modified = true;
        }
        Ok(Flow::Continue)
    }

    /// `e file` — replaces the buffer with a file (warns once if the buffer is modified).
    fn cmd_edit(
        &mut self,
        was_warned: bool,
        lex: &mut Lexer,
        out: &mut dyn Write,
    ) -> Result<Flow, String> {
        if self.modified && !was_warned {
            self.warned = true;
            return Err("warning: buffer modified".to_owned());
        }
        let name = self.file_arg(lex)?;
        let text = std::fs::read_to_string(&name).map_err(|e| format!("{name}: {e}"))?;
        writeln!(out, "{}", text.len()).map_err(|e| e.to_string())?;
        self.set_text(&text);
        self.filename = Some(name);
        self.modified = false;
        self.undo = None;
        Ok(Flow::Continue)
    }

    /// `f [file]` — prints or sets the current filename.
    fn cmd_filename(&mut self, lex: &mut Lexer, out: &mut dyn Write) -> Result<Flow, String> {
        let arg = lex.rest();
        let arg = arg.trim();
        if arg.is_empty() {
            match &self.filename {
                Some(name) => writeln!(out, "{name}").map_err(|e| e.to_string())?,
                None => return Err("no current filename".to_owned()),
            }
        } else {
            self.filename = Some(arg.to_owned());
        }
        Ok(Flow::Continue)
    }

    /// `w [file]` / `wq [file]` — writes the range (default whole buffer) to a file.
    fn cmd_write(
        &mut self,
        start: Option<i64>,
        end: Option<i64>,
        ranged: bool,
        lex: &mut Lexer,
        out: &mut dyn Write,
    ) -> Result<Flow, String> {
        let quit = lex.eat('q');
        let name = self.file_arg(lex)?;
        let (start, end) = if ranged || start.is_some() {
            self.range(start, end, (1, self.last()))?
        } else if self.lines.is_empty() {
            (1, 0) // empty buffer: write nothing
        } else {
            (1, self.lines.len())
        };
        let text = if start > end {
            String::new()
        } else {
            let mut text = self.lines[start - 1..end].join("\n");
            text.push('\n');
            text
        };
        std::fs::write(&name, &text).map_err(|e| format!("{name}: {e}"))?;
        writeln!(out, "{}", text.len()).map_err(|e| e.to_string())?;
        self.filename.get_or_insert(name);
        // A full-buffer write clears the modified flag.
        if start == 1 && end == self.lines.len() {
            self.modified = false;
        }
        if quit {
            return Ok(Flow::Quit);
        }
        Ok(Flow::Continue)
    }

    /// Resolves a command's filename argument, falling back to the current filename.
    fn file_arg(&self, lex: &mut Lexer) -> Result<String, String> {
        let arg = lex.rest();
        let arg = arg.trim();
        if arg.is_empty() {
            self.filename
                .clone()
                .ok_or_else(|| "no current filename".to_owned())
        } else {
            Ok(arg.to_owned())
        }
    }

    /// `u` — toggles the buffer with its pre-command snapshot.
    fn cmd_undo(&mut self) -> Result<Flow, String> {
        let snapshot = self.undo.take().ok_or("nothing to undo")?;
        let now = (self.lines.clone(), self.current);
        self.lines = snapshot.0;
        self.current = snapshot.1.min(self.lines.len());
        self.undo = Some(now);
        self.modified = true;
        Ok(Flow::Continue)
    }
}

/// `!command` — runs a shell command, printing its output, then `!`.
fn run_shell(lex: &mut Lexer, out: &mut dyn Write) -> Result<Flow, String> {
    let command = lex.rest();
    let command = command.trim();
    if command.is_empty() {
        return Err("no command".to_owned());
    }
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .output()
        .map_err(|e| e.to_string())?;
    out.write_all(&output.stdout).map_err(|e| e.to_string())?;
    out.write_all(&output.stderr).map_err(|e| e.to_string())?;
    writeln!(out, "!").map_err(|e| e.to_string())?;
    Ok(Flow::Continue)
}

/// A parsed `s` specification.
struct Subst {
    regex: Regex,
    replacement: String,
    /// Replace every match on a line (`g`).
    global: bool,
    /// Replace only the Nth match (`0` = first/all per `global`).
    nth: usize,
    /// Print the last changed line afterwards (`p`).
    print: bool,
}

/// Applies a substitution to one line, honoring `&` / `\1`–`\9` in the replacement.
fn substitute(spec: &Subst, line: &str) -> String {
    let mut result = String::new();
    let mut last_end = 0;
    let mut count = 0;
    for caps in spec.regex.captures_iter(line) {
        let Some(whole) = caps.get(0) else { continue };
        count += 1;
        let replace = if spec.global {
            true
        } else if spec.nth > 0 {
            count == spec.nth
        } else {
            count == 1
        };
        if replace {
            result.push_str(&line[last_end..whole.start()]);
            expand_replacement(&spec.replacement, &caps, &mut result);
            last_end = whole.end();
            if !spec.global {
                break;
            }
        }
    }
    result.push_str(&line[last_end..]);
    result
}

/// Expands a replacement template against `caps` into `out` (`&` = whole match, `\N` = group N).
fn expand_replacement(template: &str, caps: &Captures, out: &mut String) {
    let chars: Vec<char> = template.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            '&' => {
                out.push_str(caps.get(0).map_or("", |m| m.as_str()));
                index += 1;
            }
            '\\' => {
                index += 1;
                match chars.get(index) {
                    Some(digit @ '0'..='9') => {
                        let group = digit
                            .to_digit(10)
                            .and_then(|n| usize::try_from(n).ok())
                            .unwrap_or(0);
                        out.push_str(caps.get(group).map_or("", |m| m.as_str()));
                    }
                    Some(other) => out.push(*other),
                    None => out.push('\\'),
                }
                index += 1;
            }
            other => {
                out.push(other);
                index += 1;
            }
        }
    }
}

/// Reads input lines from `reader` until a line containing only `.` (or end of input).
fn read_input(reader: &mut dyn BufRead) -> io::Result<Vec<String>> {
    let mut lines = Vec::new();
    let mut buffer = String::new();
    loop {
        buffer.clear();
        if reader.read_line(&mut buffer)? == 0 {
            break;
        }
        let line = buffer.trim_end_matches(['\n', '\r']);
        if line == "." {
            break;
        }
        lines.push(line.to_owned());
    }
    Ok(lines)
}

/// A tiny character cursor over a command line, for address and command parsing.
struct Lexer {
    chars: Vec<char>,
    pos: usize,
}

impl Lexer {
    fn new(line: &str) -> Self {
        Self {
            chars: line.chars().collect(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.chars.get(self.pos).copied();
        if ch.is_some() {
            self.pos += 1;
        }
        ch
    }

    /// Consumes `ch` if it is next, reporting whether it was.
    fn eat(&mut self, ch: char) -> bool {
        if self.peek() == Some(ch) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Parses a run of decimal digits, or `None` if the next character is not a digit.
    fn number(&mut self) -> Option<usize> {
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.pos == start {
            return None;
        }
        self.chars[start..self.pos]
            .iter()
            .collect::<String>()
            .parse()
            .ok()
    }

    /// Reads up to (and consuming) the next unescaped `delim`, or end of line. `\<delim>` yields a
    /// literal delimiter; every other backslash escape is kept verbatim for the regex engine.
    fn take_delimited(&mut self, delim: char) -> String {
        let mut text = String::new();
        while let Some(ch) = self.peek() {
            if ch == delim {
                self.pos += 1;
                break;
            }
            if ch == '\\' {
                self.pos += 1;
                match self.peek() {
                    Some(next) if next == delim => {
                        text.push(delim);
                        self.pos += 1;
                    }
                    Some(next) => {
                        text.push('\\');
                        text.push(next);
                        self.pos += 1;
                    }
                    None => text.push('\\'),
                }
            } else {
                text.push(ch);
                self.pos += 1;
            }
        }
        text
    }

    /// Returns the rest of the line.
    fn rest(&mut self) -> String {
        let rest: String = self.chars[self.pos..].iter().collect();
        self.pos = self.chars.len();
        rest
    }
}

#[cfg(test)]
mod tests {
    use super::Ed;
    use std::io::{BufRead, Cursor};

    /// Runs `commands` (with any `a`/`i`/`c` input inline) against a fresh editor, returning the
    /// stdout the commands produced.
    fn session(setup: &[&str], commands: &str) -> (Ed, String) {
        let mut ed = Ed::new();
        if !setup.is_empty() {
            ed.set_text(&format!("{}\n", setup.join("\n")));
        }
        let mut reader = Cursor::new(commands.as_bytes().to_vec());
        let mut out: Vec<u8> = Vec::new();
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap() == 0 {
                break;
            }
            let trimmed = line.trim_end_matches('\n').to_owned();
            if ed.execute(&trimmed, &mut reader, &mut out).is_err() {
                out.extend_from_slice(b"?\n");
            }
        }
        (ed, String::from_utf8(out).unwrap())
    }

    #[test]
    fn append_then_print_numbered() {
        let (ed, out) = session(&[], "a\nhello\nworld\n.\n1,$n\n");
        assert_eq!(ed.lines, ["hello", "world"]);
        assert_eq!(out, "1\thello\n2\tworld\n");
    }

    #[test]
    fn delete_and_undo_restores() {
        let (ed, _) = session(&["one", "two", "three"], "2d\nu\n");
        assert_eq!(
            ed.lines,
            ["one", "two", "three"],
            "u restores the deleted line"
        );
    }

    #[test]
    fn substitute_with_group_backref() {
        let (ed, out) = session(&["foo bar"], "s/(\\w+) (\\w+)/\\2 \\1/p\n");
        assert_eq!(ed.lines, ["bar foo"]);
        assert_eq!(out, "bar foo\n");
    }

    #[test]
    fn substitute_global_replaces_all() {
        let (ed, _) = session(&["a-a-a"], "s/a/X/g\n");
        assert_eq!(ed.lines, ["X-X-X"]);
    }

    #[test]
    fn address_dollar_and_search() {
        let (ed, out) = session(&["alpha", "beta", "gamma"], "/gamma/\n=\n");
        assert_eq!(
            out, "gamma\n3\n",
            "/re/ moves to the match; = prints its number"
        );
        assert_eq!(ed.current, 3);
    }

    #[test]
    fn global_print_matching_lines() {
        let (_, out) = session(&["cat", "dog", "cart"], "g/ca/p\n");
        assert_eq!(out, "cat\ncart\n");
    }

    #[test]
    fn global_delete_matching_lines() {
        let (ed, _) = session(&["keep", "drop x", "keep", "drop y"], "g/drop/d\n");
        assert_eq!(ed.lines, ["keep", "keep"]);
    }

    #[test]
    fn move_and_copy() {
        let (moved, _) = session(&["1", "2", "3"], "1m$\n");
        assert_eq!(moved.lines, ["2", "3", "1"], "m moves line 1 to the end");
        let (copied, _) = session(&["1", "2", "3"], "1t$\n");
        assert_eq!(
            copied.lines,
            ["1", "2", "3", "1"],
            "t copies line 1 to the end"
        );
    }

    #[test]
    fn join_range() {
        let (ed, _) = session(&["a", "b", "c"], "1,3j\n");
        assert_eq!(ed.lines, ["abc"]);
    }

    #[test]
    fn change_replaces_range_with_input() {
        let (ed, _) = session(&["x", "y", "z"], "2c\nY1\nY2\n.\n");
        assert_eq!(ed.lines, ["x", "Y1", "Y2", "z"]);
    }
}
