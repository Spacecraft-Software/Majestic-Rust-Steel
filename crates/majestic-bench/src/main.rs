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
use majestic_core::{
    apply_hashline, tagged_read, Buffer, Editor, HashlineEdit, LineRef, Workspace,
};
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
        keypress("keypress (highlighted .rs, 800 lines)", true, 800, true),
        scroll_highlighted(5_000),
        open_large(32),
        edit_format(),
        nova_scene(),
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
/// The highlighted variant stays on budget because highlighting runs on a background worker
/// (PRD §6.4/§6.9): the keystroke only takes a cheap `Rope` snapshot and renders with the latest
/// finished spans, so re-highlighting never blocks the UI thread.
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

/// Nova's per-frame translation latency (M4.5, the GUI half of the §7 frame budget): `build_scene`
/// turning a full editor-sized cell buffer into the renderer-agnostic draw list.
///
/// The GUI frame is `App::render` (the keypress scenarios, shared with the TTY) + this translation +
/// the GPU submit. This isolates the Nova-specific CPU cost, which must stay a small fraction of the
/// 16 ms frame; a dense, nearly-all-glyph screen is its worst case.
fn nova_scene() -> Outcome {
    let theme = Theme::steelbore();
    let base = theme.base_style();
    let (cols, rows) = (120_u16, 40_u16);
    let mut buffer = penumbra::Buffer::new(cols, rows, base);
    let line = "    let scene = build_scene(&buffer, metrics); // representative editor content";
    for row in 0..rows {
        for (index, ch) in line.chars().enumerate() {
            let col = u16::try_from(index).unwrap_or(u16::MAX);
            if col < cols {
                buffer.set_char(col, row, ch, base);
            }
        }
    }
    let metrics = nova::CellMetrics::new(8.0, 16.0);

    let mut samples = Vec::with_capacity(500);
    for _ in 0..500 {
        let start = Instant::now();
        let scene = nova::build_scene(&buffer, metrics);
        samples.push(start.elapsed());
        std::hint::black_box(&scene);
    }
    samples.sort_unstable();
    let p99 = quantile(&samples, 990);
    Outcome {
        name: "nova build_scene (120×40 dense)",
        headline: p99,
        budget: FRAME_BUDGET,
        gated: true,
        detail: format!(
            "p50 {} | p99 {} | max {} over {} translations",
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
    // Wait for the background highlighter once so the scenario measures *highlighted* rendering;
    // scrolling does not edit the buffer, so no further re-highlight is triggered.
    workspace.active_mut().flush_highlights();

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

/// Edit-format robustness (PRD #1 M3 exit criterion): the hashline primitive must be **at least as
/// correct as** a `str_replace` baseline. The decisive case is a buffer full of *identical* lines —
/// the ambiguity a string match cannot resolve. Each intended edit targets one specific occurrence:
/// hashline cites that line by number + BLAKE3 tag and lands on it; `str_replace` (replace-first) can
/// only ever hit occurrence 0, so it clobbers the wrong line for every later target.
///
/// Informational latency, but the criterion itself is *gated* by the assertions below: the harness
/// fails (non-zero exit) if hashline ever misapplies or falls behind `str_replace`.
fn edit_format() -> Outcome {
    const GROUPS: usize = 64;
    // Interleave a unique line and an identical "    step();" so the ambiguous line recurs GROUPS
    // times at distinct positions (its 0-based index for group g is `2 * g + 1`).
    let mut lines = Vec::with_capacity(GROUPS * 2);
    for g in 0..GROUPS {
        lines.push(format!("uniq_{g}"));
        lines.push("    step();".to_owned());
    }
    let original = lines.join("\n");
    let target_index = |g: usize| 2 * g + 1;
    let replacement = |g: usize| format!("    step_{g}();");

    // hashline: cite the exact line + tag, so the intended occurrence is the one that changes.
    let start = Instant::now();
    let mut hashline_correct = 0usize;
    for g in 0..GROUPS {
        let mut buffer = Buffer::from_text(&original);
        let index = target_index(g);
        let edit = HashlineEdit::Replace {
            at: LineRef::new(index, tag_at(&buffer, index)),
            text: replacement(g),
        };
        if apply_hashline(&mut buffer, &[edit]).is_ok()
            && only_line_changed(&buffer, &original, index, &replacement(g))
        {
            hashline_correct += 1;
        }
    }
    let hashline_elapsed = start.elapsed();

    // str_replace baseline: replace the first textual occurrence — always occurrence 0.
    let start = Instant::now();
    let mut str_replace_correct = 0usize;
    for g in 0..GROUPS {
        let edited = original.replacen("    step();", &replacement(g), 1);
        let buffer = Buffer::from_text(&edited);
        if only_line_changed(&buffer, &original, target_index(g), &replacement(g)) {
            str_replace_correct += 1;
        }
    }
    let str_replace_elapsed = start.elapsed();

    // The gate: hashline applies every edit to the intended line, and is never less correct.
    assert!(
        hashline_correct == GROUPS,
        "hashline must apply every edit to the cited line ({hashline_correct}/{GROUPS})"
    );
    assert!(
        hashline_correct >= str_replace_correct,
        "M3 exit criterion: hashline ({hashline_correct}) must be at least as correct as str_replace ({str_replace_correct})"
    );

    Outcome {
        name: "edit format: hashline vs str_replace (disambiguation)",
        headline: hashline_elapsed,
        budget: FRAME_BUDGET,
        gated: false, // the gate is the correctness assertion above, not a latency budget
        detail: format!(
            "hashline {hashline_correct}/{GROUPS} correct ({}) | str_replace {str_replace_correct}/{GROUPS} correct ({}) — hashline disambiguates identical lines str_replace cannot",
            micros(hashline_elapsed),
            micros(str_replace_elapsed),
        ),
    }
}

/// The hashline tag `tagged_read` assigns to 0-based `index` of `buffer` (as the agent would cite it).
fn tag_at(buffer: &Buffer, index: usize) -> String {
    let read = tagged_read(buffer);
    let row = read.lines().nth(index).expect("line present");
    let after_colon = row.split_once(':').expect("colon").1;
    after_colon.split_once('│').expect("separator").0.to_owned()
}

/// Whether `buffer` differs from `original` in exactly one line — line `index`, now equal to `text`.
fn only_line_changed(buffer: &Buffer, original: &str, index: usize, text: &str) -> bool {
    let after = buffer.text();
    let before: Vec<&str> = original.lines().collect();
    let now: Vec<&str> = after.lines().collect();
    before.len() == now.len()
        && now.get(index) == Some(&text)
        && before
            .iter()
            .zip(&now)
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .all(|(line, _)| line == index)
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

/// Formats a duration as microseconds (for sub-millisecond comparisons).
fn micros(duration: Duration) -> String {
    format!("{:.1} µs", duration.as_secs_f64() * 1_000_000.0)
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
