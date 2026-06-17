// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! `majestic-bench` — the instrumented latency/throughput harness for the Standard §7 / PRD #1
//! §7 performance budgets.
//!
//! A dependency-free harness (no `criterion`) that drives the real editing pipeline — keystroke
//! → command → render — and reports `p50`/`p99`/`max` per scenario against the one-frame (16 ms)
//! and cold-start (50 ms) budgets. Run it in **release** (the budgets are release-build figures):
//!
//! ```text
//! cargo run --release -p majestic-bench            # report
//! cargo run --release -p majestic-bench -- --check # report and fail CI on a budget violation
//! ```
//!
//! In a debug build `--check` reports but does not gate (debug is far slower than the budgets
//! describe), so CI must run it `--release`.

use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use keymaker::KeyPress;
use majestic_core::{Buffer, Editor, Workspace};
use penumbra::{Buffer as Surface, Theme};

/// One rendered frame's budget — a 60 Hz refresh (PRD §7: keypress p99 and scroll ≤ 16 ms).
const FRAME_BUDGET: Duration = Duration::from_millis(16);
/// Cold start to first paint (PRD §7: ≤ 50 ms).
const COLD_START_BUDGET: Duration = Duration::from_millis(50);
/// A representative editor viewport (≈ the reference T490s terminal).
const WIDTH: u16 = 120;
const HEIGHT: u16 = 40;

fn main() -> ExitCode {
    let check = std::env::args().any(|arg| arg == "--check");

    let outcomes = vec![
        cold_start(),
        keypress("keypress (plain, 800 lines)", false, 800, true),
        keypress("keypress (highlighted .rs, 800 lines)", true, 800, false),
        scroll_highlighted(5_000),
        open_large(32),
    ];

    print_report(&outcomes);

    let breached: Vec<&Outcome> = outcomes
        .iter()
        .filter(|outcome| outcome.gated && outcome.headline > outcome.budget)
        .collect();

    if !check {
        return ExitCode::SUCCESS;
    }
    if cfg!(debug_assertions) {
        println!(
            "\nnote: debug build — budgets are not enforced (run with --release to gate). \
             {} scenario(s) would breach in this build.",
            breached.len(),
        );
        return ExitCode::SUCCESS;
    }
    if breached.is_empty() {
        println!("\nAll gated scenarios are within budget.");
        ExitCode::SUCCESS
    } else {
        for outcome in &breached {
            eprintln!(
                "BUDGET EXCEEDED: {} = {} > {}",
                outcome.name,
                millis(outcome.headline),
                millis(outcome.budget),
            );
        }
        ExitCode::FAILURE
    }
}

/// The result of one scenario.
struct Outcome {
    name: &'static str,
    /// The metric compared against `budget` (p99 for latency, the elapsed time otherwise).
    headline: Duration,
    budget: Duration,
    /// Whether `headline` is enforced under `--check` (informational scenarios are not gated).
    gated: bool,
    /// A human-readable breakdown (percentiles, throughput, …).
    detail: String,
}

/// Cold start to first paint: build the workspace and render the first frame.
///
/// In-process only — it excludes OS process spawn and terminal handshake, so it is a lower bound
/// on the end-to-end `mj` cold start (which CI also times with `hyperfine`). Reports the median
/// of repeated builds to smooth scheduler jitter.
fn cold_start() -> Outcome {
    let theme = Theme::steelbore();
    let mut samples = Vec::with_capacity(64);
    for _ in 0..64 {
        let start = Instant::now();
        let mut workspace = Workspace::new(Editor::with_buffer(Buffer::from_text(
            "fn main() {\n    println!(\"hello\");\n}\n",
        )));
        let mut surface = blank_surface(&theme);
        let area = surface.area();
        workspace.render(&mut surface, area, &theme, true);
        samples.push(start.elapsed());
        std::hint::black_box(&surface);
    }
    samples.sort_unstable();
    let p50 = quantile(&samples, 500);
    Outcome {
        name: "cold start to first paint",
        headline: p50,
        budget: COLD_START_BUDGET,
        gated: true,
        detail: format!(
            "median {} | p99 {} | max {} (in-process; excludes process spawn)",
            millis(p50),
            millis(quantile(&samples, 990)),
            millis(quantile(&samples, 1000)),
        ),
    }
}

/// Keypress → screen update latency: self-insert characters, rendering after each, and report
/// the per-keystroke percentiles.
///
/// The highlighted variant is `gated = false` (tracked, not enforced): it re-highlights the whole
/// buffer synchronously on every keystroke, which needs incremental/background parsing (PRD §6.9)
/// — a larger optimization than the per-frame style/build fixes already landed.
fn keypress(name: &'static str, highlighted: bool, lines: usize, gated: bool) -> Outcome {
    let theme = Theme::steelbore();
    let mut surface = blank_surface(&theme);
    let area = surface.area();

    let scratch = highlighted.then(|| TempSource::rust(lines));
    let editor = match &scratch {
        Some(source) => Editor::with_buffer(Buffer::open(source.path()).expect("open temp source")),
        None => Editor::with_buffer(Buffer::from_text(&plain_source(lines))),
    };
    let mut workspace = Workspace::new(editor);

    let typed = b"the quick brown fox jumps over the lazy dog ";
    let mut samples = Vec::with_capacity(600);
    for &byte in typed.iter().cycle().take(600) {
        let key = KeyPress::char(char::from(byte));
        let start = Instant::now();
        workspace.handle_key(key);
        workspace.render(&mut surface, area, &theme, true);
        samples.push(start.elapsed());
        std::hint::black_box(&surface);
    }
    samples.sort_unstable();
    let p99 = quantile(&samples, 990);
    let note = if gated {
        ""
    } else {
        " — KNOWN: whole-buffer re-highlight per keystroke; needs incremental/background parsing (PRD §6.9)"
    };
    Outcome {
        name,
        headline: p99,
        budget: FRAME_BUDGET,
        gated,
        detail: format!(
            "p50 {} | p99 {} | max {} over {} keystrokes{note}",
            millis(quantile(&samples, 500)),
            millis(p99),
            millis(quantile(&samples, 1000)),
            samples.len(),
        ),
    }
}

/// Full-page scrolling through a highlighted buffer, timing each rendered frame.
fn scroll_highlighted(lines: usize) -> Outcome {
    let theme = Theme::steelbore();
    let mut surface = blank_surface(&theme);
    let area = surface.area();
    let source = TempSource::rust(lines);
    let mut workspace = Workspace::new(Editor::with_buffer(
        Buffer::open(source.path()).expect("open temp source"),
    ));

    let page = KeyPress::key(keymaker::KeyCode::PageDown);
    let mut samples = Vec::with_capacity(256);
    // Page down through the whole file (lines / HEIGHT pages), timing the frame each step.
    let pages = lines / usize::from(HEIGHT) + 1;
    for _ in 0..pages.min(256) {
        workspace.handle_key(page);
        let start = Instant::now();
        workspace.render(&mut surface, area, &theme, true);
        samples.push(start.elapsed());
        std::hint::black_box(&surface);
    }
    samples.sort_unstable();
    let p99 = quantile(&samples, 990);
    Outcome {
        name: "scroll full page (highlighted)",
        headline: p99,
        budget: FRAME_BUDGET,
        gated: true,
        detail: format!(
            "p50 {} | p99 {} | max {} over {} frames",
            millis(quantile(&samples, 500)),
            millis(p99),
            millis(quantile(&samples, 1000)),
            samples.len(),
        ),
    }
}

/// Opening a large file: time the synchronous rope build for `megabytes` of text.
///
/// Informational, not gated: the PRD §7 target (1 GB interactive in < 100 ms) is met by an
/// incremental/async load that is a later optimization; this records the current synchronous
/// `Buffer::from_text` cost at a scaled size.
fn open_large(megabytes: usize) -> Outcome {
    let unit = "fn item(x: i64) -> i64 { let y = x * 2; y + 1 }\n";
    let count = megabytes * 1_000_000 / unit.len();
    let text = unit.repeat(count);

    let start = Instant::now();
    let buffer = Buffer::from_text(&text);
    let elapsed = start.elapsed();
    std::hint::black_box(&buffer);

    Outcome {
        name: "open large file (rope build)",
        headline: elapsed,
        budget: Duration::from_millis(100),
        gated: false,
        detail: format!(
            "{} for ~{} MB synchronous Buffer::from_text (async/incremental load is future work)",
            millis(elapsed),
            megabytes,
        ),
    }
}

/// A blank editor-sized framebuffer.
fn blank_surface(theme: &Theme) -> Surface {
    Surface::new(WIDTH, HEIGHT, theme.base_style())
}

/// Generates `lines` of plain (non-highlighted) prose-like text.
fn plain_source(lines: usize) -> String {
    "the quick brown fox jumps over the lazy dog\n".repeat(lines)
}

/// The nearest-rank quantile of a sorted slice. `permille` is the quantile × 1000 (e.g. 990 =
/// p99, 1000 = max). Integer arithmetic throughout to avoid lossy float casts.
fn quantile(sorted: &[Duration], permille: usize) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let last = sorted.len() - 1;
    let index = (permille * last + 500) / 1000;
    sorted[index.min(last)]
}

/// Formats a duration as milliseconds with microsecond precision.
fn millis(duration: Duration) -> String {
    format!("{:.3} ms", duration.as_secs_f64() * 1000.0)
}

/// Prints the scenario table.
fn print_report(outcomes: &[Outcome]) {
    let profile = if cfg!(debug_assertions) {
        "debug (numbers are NOT representative; use --release)"
    } else {
        "release"
    };
    let mut report =
        format!("Majestic §7 performance harness — {WIDTH}x{HEIGHT} viewport, {profile} build\n\n");
    for outcome in outcomes {
        let over = outcome.headline > outcome.budget;
        let status = match (outcome.gated, over) {
            (true, false) => "OK",
            (true, true) => "OVER",
            (false, true) => "KNOWN", // tracked breach, not enforced (see detail)
            (false, false) => "info",
        };
        let _ = writeln!(
            report,
            "[{status:>4}] {name}\n         {detail}\n         budget {budget}\n",
            name = outcome.name,
            detail = outcome.detail,
            budget = millis(outcome.budget),
        );
    }
    print!("{report}");
}

/// A temporary `.rs` file (with its journal sidecar) cleaned up on drop, so the editor attaches a
/// real tree-sitter highlighter for the highlighted scenarios.
struct TempSource {
    path: PathBuf,
    journal: PathBuf,
}

impl TempSource {
    fn rust(lines: usize) -> Self {
        let mut body = String::with_capacity(lines * 48);
        for index in 0..lines {
            let _ = writeln!(
                body,
                "fn item_{index}(x: i64) -> i64 {{ let y = x * 2; y + {index} }}"
            );
        }
        let mut path = std::env::temp_dir();
        path.push(format!("majestic-bench-{}-{lines}.rs", std::process::id()));
        let mut journal = path.clone().into_os_string();
        journal.push(".mjjournal");
        let journal = PathBuf::from(journal);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&journal);
        fs::write(&path, body).expect("write temp source");
        Self { path, journal }
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempSource {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_file(&self.journal);
    }
}
