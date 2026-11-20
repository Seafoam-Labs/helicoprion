//! The result card: one picture of a comparison, for places that show a
//! picture and not a page (a chat that previews images, a README).
//!
//! It holds what the plain view's headings hold and one number more:
//! for every scheduler and every workload, whether the scheduler did
//! better than the baseline, worse, both, or neither clearly, and next
//! to the mark the largest of the clear changes that way. The verdicts
//! are the plain view's own (`simple::briefs`) and the number is one of
//! the changes the verdict counts, so the card cannot say something the
//! report does not. The number is the most that was found and not what
//! is typical, and the card says so. What it leaves out is everything
//! that makes a verdict worth trusting: every other change, how certain
//! each is, and the warnings. It says so on its face and is no
//! substitute for the report.
//!
//! The card is an SVG without style sheets or scripts, in fixed colours
//! (a picture has no dark mode), sized as link previews are. With the
//! `png` feature it can be drawn into a PNG file.

use std::fmt::Write as _;

use crate::compare::{Comparison, Delta, Verdict};
use crate::esc;

pub const WIDTH: u32 = 1200;
pub const HEIGHT: u32 = 630;

const PAGE: &str = "#f9f9f7";
const SURFACE: &str = "#ffffff";
const INK: &str = "#0b0b0b";
const INK_2: &str = "#52514e";
const MUTED: &str = "#898781";
const GRID: &str = "#e1e0d9";
/// Fill and mark of a verdict's cell. The word in the cell is always in
/// ink: the colour is a second way to see it, never the only one.
const GOOD: (&str, &str) = ("#dcefdc", "#006300");
const BAD: (&str, &str) = ("#f9dede", "#d03b3b");
const BOTH: (&str, &str) = ("#fbedc9", "#a86f00");

const FONT: &str = "sans-serif";
const MARGIN: f64 = 48.0;
/// Width of the column with the schedulers' names.
const NAMES: f64 = 210.0;
/// Baselines of the two lines between the head and the grid.
pub(crate) const READING: f64 = 151.0;
pub(crate) const CAVEAT: f64 = 174.0;
/// Top of the grid's first row.
const GRID_TOP: f64 = 238.0;
/// Bottom of the last row at most: the legend goes below.
const GRID_BOTTOM: f64 = 548.0;
/// More rows or columns than fit are not drawn small: the card says how
/// many are missing.
const MAX_ROWS: usize = 7;
const MAX_COLUMNS: usize = 12;

/// `text` cut to `max` characters, with an ellipsis where it was cut.
fn fit(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let cut: String = text.chars().take(max.saturating_sub(1)).collect();
        format!("{}…", cut.trim_end())
    }
}

/// A workload's name in two short lines for a column's head: the kind
/// of workload, and which case of it.
fn column(workload: &str) -> (String, String) {
    let (kind, case) = workload.split_once('/').unwrap_or((workload, ""));
    let kind = match kind {
        "wake-pingpong" => "pingpong",
        "periodic-wake" => "periodic",
        "wake-burst" => "burst",
        "cgroup-fair" => "cgroup",
        "idle-wake" => "idle wake",
        other => other,
    };
    // The cases without a name of their own are the idle ones.
    let case = if case.is_empty() && matches!(kind, "pingpong" | "periodic") { "idle" } else { case };
    (fit(kind, 11), fit(case, 11))
}

fn word(v: Verdict) -> &'static str {
    match v {
        Verdict::Better => "better",
        Verdict::Worse => "worse",
        Verdict::Mixed => "mixed",
        // A word like the others: the card is passed around small
        // and without its legend, where a dash says nothing.
        Verdict::NoClearDifference => "unclear",
    }
}

/// The size of a change as the card writes it, next to a mark that
/// says which way: a multiple from 2× up, as the report gives it, and a
/// percentage below that, in whole percent from ten up. No sign and no
/// word; the mark and the cell's own word carry the direction.
fn size(d: &Delta) -> String {
    let times = match crate::svg::multiple(d.ratio) {
        Some(true) => d.ratio,
        Some(false) => 1.0 / d.ratio,
        None => {
            let pct = d.median.abs();
            return if pct >= 9.95 { format!("{pct:.0}%") } else { format!("{pct:.1}%") };
        }
    };
    // Past three digits the number is wider than it is worth.
    if times >= 999.5 { ">999×".into() } else { format!("{}×", crate::svg::times(times)) }
}

/// Font size of a size, and the room it takes: an estimate on the wide
/// side, from the widest of the fonts the card is likely to be drawn
/// in, since the card does not know which one it will be.
const SIZE_PX: u32 = 13;
fn size_w(s: &str) -> f64 {
    s.chars()
        .map(|c| match c {
            '.' => 4.2,
            '%' => 12.4,
            '×' | '>' => 10.9,
            _ => 8.3,
        })
        .sum()
}
/// A size and its direction: `true` for the better.
type Size = (bool, String);
/// The sizes of a cell, and whether two of them go one above the other.
#[derive(Default)]
struct Sizes {
    list: Vec<Size>,
    stacked: bool,
}
/// A mark and the size after it, and the space between two of them.
const MARK_W: f64 = 12.0;
const MARK_GAP: f64 = 5.0;
const PAIR_GAP: f64 = 10.0;

/// The sizes a cell shows next to its marks: for a verdict of better or
/// worse the largest clear change that way, for mixed both, and both or
/// neither: one of the two alone would read as the whole of it. None
/// for no clear difference, and none where the change is to nothing.
fn sizes(v: Verdict, largest: &[Option<Delta>; 2]) -> Vec<Size> {
    let of = |better: bool| largest[usize::from(!better)].as_ref().map(|d| (better, size(d)));
    match v {
        Verdict::Better => of(true).into_iter().collect(),
        Verdict::Worse => of(false).into_iter().collect(),
        Verdict::Mixed => match (of(true), of(false)) {
            (Some(b), Some(w)) => vec![b, w],
            _ => Vec::new(),
        },
        Verdict::NoClearDifference => Vec::new(),
    }
}

/// The room the marks of a cell take with these sizes next to them.
fn marks_w(sizes: &[Size]) -> f64 {
    sizes.iter().map(|(_, s)| MARK_W + MARK_GAP + size_w(s)).sum::<f64>()
        + PAIR_GAP * sizes.len().saturating_sub(1) as f64
}

fn text(out: &mut String, x: f64, y: f64, size: u32, fill: &str, extra: &str, content: &str) {
    let _ = write!(
        out,
        r#"<text x="{x:.1}" y="{y:.1}" font-family="{FONT}" font-size="{size}" fill="{fill}"{extra}>{}</text>"#,
        esc(content)
    );
}

/// The least height of a row in which two sizes go one above the other
/// with the word below them.
const STACK_H: f64 = 60.0;

/// Which sizes a card draws, of those its cells have (`sizes`, one
/// list per cell): whether the single ones are drawn, and how the two
/// of a mixed cell are, `Some(stacked)`, or `None` for not at all.
///
/// Sizes are drawn where every cell that has them has the room, and
/// nowhere otherwise: a card with numbers in some cells and not in
/// others, for want of room, would read as if the others had none. The
/// two of a mixed cell need more room than one and are judged apart, so
/// that a card with many columns keeps the single ones: side by side
/// where all of them fit so, one above the other where the rows are
/// high enough for that, and left out otherwise.
fn layout(cells: &[Vec<Size>], column_w: f64, row_h: f64) -> (bool, Option<bool>) {
    // The box is 6 narrower than the column, and 2 of air at either
    // end are enough with widths that are estimated on the wide side.
    let room = column_w - 6.0 - 4.0;
    let in_a_row = |s: &Vec<Size>| marks_w(s) <= room;
    let each = |s: &Vec<Size>| s.iter().all(|one| marks_w(std::slice::from_ref(one)) <= room);
    let single = cells.iter().filter(|s| s.len() == 1).all(in_a_row);
    let mut pairs = cells.iter().filter(|s| s.len() == 2);
    let pair = if pairs.clone().all(in_a_row) {
        Some(false)
    } else if row_h >= STACK_H && pairs.all(each) {
        Some(true)
    } else {
        None
    };
    (single, pair)
}

/// A verdict's cell: a tinted box, a mark and the word, and next to the
/// mark the sizes given, each after a mark of its own direction: side
/// by side, or, `stacked`, one above the other where a narrow cell has
/// the height for it.
fn cell(out: &mut String, x: f64, y: f64, w: f64, h: f64, v: Verdict, sizes: &Sizes) {
    let (sizes, stacked) = (&sizes.list[..], sizes.stacked);
    let (fill, stroke, mark) = match v {
        Verdict::Better => (GOOD.0, GOOD.0, Some(GOOD.1)),
        Verdict::Worse => (BAD.0, BAD.0, Some(BAD.1)),
        Verdict::Mixed => (BOTH.0, BOTH.0, Some(BOTH.1)),
        Verdict::NoClearDifference => (SURFACE, GRID, None),
    };
    let _ = write!(
        out,
        r#"<rect x="{:.1}" y="{:.1}" width="{:.1}" height="{:.1}" rx="6" fill="{fill}" stroke="{stroke}"/>"#,
        x + 3.0,
        y + 3.0,
        w - 6.0,
        h - 6.0
    );
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    // The mark above the word: up for better, down for worse, both for
    // mixed. Drawn, so that it does not depend on a font having it.
    let triangle = |out: &mut String, cx: f64, top: f64, up: bool, colour: &str| {
        let (tip, base) = if up { (top, top + 8.0) } else { (top + 8.0, top) };
        let _ = write!(
            out,
            r#"<path d="M{:.1} {tip:.1} L{:.1} {base:.1} L{:.1} {base:.1} Z" fill="{colour}"/>"#,
            cx,
            cx - 6.0,
            cx + 6.0
        );
    };
    match (v, mark) {
        // With sizes: mark, size, and the same again for the other
        // direction, the row of them centred. The size is written in
        // the ink of the text, not in the mark's colour.
        (_, Some(colour)) if !sizes.is_empty() => {
            let one = |out: &mut String, at: f64, top: f64, better: bool, size: &str| {
                triangle(out, at + MARK_W / 2.0, top, better, colour);
                text(out, at + MARK_W + MARK_GAP, top + 9.5, SIZE_PX, INK, "", size);
            };
            if stacked {
                // The marks one under the other, the pair centred by
                // its wider line.
                let at = cx - sizes.iter().map(|s| marks_w(std::slice::from_ref(s))).fold(0.0, f64::max) / 2.0;
                for (line, (better, size)) in sizes.iter().enumerate() {
                    one(out, at, cy - 24.0 + 15.0 * line as f64, *better, size);
                }
            } else {
                let mut at = cx - marks_w(sizes) / 2.0;
                for (better, size) in sizes {
                    one(out, at, cy - 15.0, *better, size);
                    at += MARK_W + MARK_GAP + size_w(size) + PAIR_GAP;
                }
            }
        }
        (Verdict::Better, Some(colour)) => triangle(out, cx, cy - 15.0, true, colour),
        (Verdict::Worse, Some(colour)) => triangle(out, cx, cy - 15.0, false, colour),
        (Verdict::Mixed, Some(colour)) => {
            triangle(out, cx - 8.0, cy - 15.0, true, colour);
            triangle(out, cx + 8.0, cy - 15.0, false, colour);
        }
        _ => {}
    }
    let (baseline, ink) = match mark {
        Some(_) if stacked && !sizes.is_empty() => (cy + 19.0, INK),
        Some(_) => (cy + 12.0, INK),
        None => (cy + 5.0, INK_2),
    };
    text(out, cx, baseline, 14, ink, r#" text-anchor="middle""#, word(v));
}

/// The baseline in a sentence: which scheduler every cell is measured
/// against. A session calls the kernel's own scheduler "default", which
/// says nothing to someone who sees the card alone, and a label can be
/// anything; the scheduler recorded in the result says what ran.
fn baseline(label: &str, scheduler: &helicoprion_core::Scheduler) -> String {
    let what = scheduler.label();
    let kind = if scheduler.is_ext() { "a sched_ext scheduler" } else { "the scheduler built into the kernel" };
    if what.eq_ignore_ascii_case(label) {
        format!("The baseline is {}, {kind}.", fit(&what, 40))
    } else {
        format!("The baseline, {}, is {}: {kind}.", fit(label, 28), fit(&what, 28))
    }
}

/// What the card does not hold, in a line: what did not fit, and what
/// is in the report only. Both, where both apply: that rows are missing
/// is no reason to stop saying that there are warnings.
fn caveat(left_out: &[String], warnings: usize) -> String {
    let report = match warnings {
        0 => "Every change and its certainty are in the report.".to_string(),
        n => format!("Every change, its certainty and {n} warning{} are in the report.", if n == 1 { "" } else { "s" }),
    };
    if left_out.is_empty() { report } else { format!("Not shown: {}. {report}", left_out.join(" and ")) }
}

/// The card for `c`, or `None` where there is nothing to compare: a
/// single result, or none that the plain view has a verdict for.
pub fn svg(c: &Comparison) -> Option<String> {
    let briefs = crate::simple::briefs(c);
    let mut workloads: Vec<&str> = Vec::new();
    let mut rows: Vec<usize> = Vec::new();
    for b in &briefs {
        if !workloads.contains(&b.workload.as_str()) {
            workloads.push(&b.workload);
        }
        if !rows.contains(&b.series) {
            rows.push(b.series);
        }
    }
    if workloads.is_empty() || rows.is_empty() {
        return None;
    }
    let (more_rows, more_columns) = (rows.len().saturating_sub(MAX_ROWS), workloads.len().saturating_sub(MAX_COLUMNS));
    rows.truncate(MAX_ROWS);
    workloads.truncate(MAX_COLUMNS);

    let base = &c.series[c.baseline];
    let w = f64::from(WIDTH);
    let mut out = String::new();
    let _ = write!(
        out,
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {WIDTH} {HEIGHT}" width="{WIDTH}" height="{HEIGHT}" role="img" aria-label="{}"><rect width="{WIDTH}" height="{HEIGHT}" fill="{PAGE}"/>"#,
        esc(&format!("Schedulers compared with {}: {}", base.label, baseline(&base.label, &base.file.scheduler)))
    );

    // Head: what this is, against what, on what. The baseline is named
    // as the session named it and said for what it is: "default" alone
    // does not tell a reader which scheduler every cell is against.
    text(
        &mut out,
        MARGIN,
        70.0,
        36,
        INK,
        r#" font-weight="bold""#,
        // Short enough to end before the name at the right, in bold.
        &format!("Schedulers compared with {}", fit(&base.label, 22)),
    );
    text(&mut out, w - MARGIN, 70.0, 20, MUTED, r#" text-anchor="end""#, "helicoprion");
    text(&mut out, MARGIN, 102.0, 20, INK_2, "", &baseline(&base.label, &base.file.scheduler));
    let sys = &base.file.system;
    let machine = [
        sys.cpu_model.as_deref().map(|m| fit(m, 44)),
        Some(format!("{} CPUs", sys.usable_cpus)),
        Some(format!("kernel {}", fit(&sys.kernel, 26))),
    ];
    text(&mut out, MARGIN, 128.0, 16, MUTED, "", &machine.into_iter().flatten().collect::<Vec<_>>().join("  ·  "));

    // The grid: a column per workload, a row per scheduler.
    let column_w = ((w - 2.0 * MARGIN - NAMES) / workloads.len() as f64).min(120.0);
    let row_h = ((GRID_BOTTOM - GRID_TOP) / rows.len() as f64).min(78.0);
    let left = MARGIN + NAMES;
    for (k, workload) in workloads.iter().enumerate() {
        let cx = left + column_w * (k as f64 + 0.5);
        let (kind, case) = column(workload);
        text(&mut out, cx, GRID_TOP - 32.0, 14, INK_2, r#" text-anchor="middle""#, &kind);
        text(&mut out, cx, GRID_TOP - 14.0, 14, INK, r#" text-anchor="middle""#, &case);
    }
    // One rule for the whole card on which sizes it has room for.
    let shown = |b: &&crate::simple::Brief| rows.contains(&b.series) && workloads.contains(&b.workload.as_str());
    let all: Vec<Vec<Size>> = briefs.iter().filter(shown).map(|b| sizes(b.verdict, &b.largest)).collect();
    let (single, pair) = layout(&all, column_w, row_h);
    let mut numbers = false;
    for (j, series) in rows.iter().enumerate() {
        let y = GRID_TOP + row_h * j as f64;
        text(&mut out, MARGIN, y + row_h / 2.0 + 7.0, 21, INK, "", &fit(&c.series[*series].label, 17));
        for (k, workload) in workloads.iter().enumerate() {
            // No verdict where the two were not compared: the cell
            // stays empty, which is not "no clear difference".
            if let Some(b) = briefs.iter().find(|b| b.series == *series && b.workload == *workload) {
                let mut sizes = sizes(b.verdict, &b.largest);
                if !(if sizes.len() == 2 { pair.is_some() } else { single }) {
                    sizes.clear();
                }
                numbers |= !sizes.is_empty();
                let sizes = Sizes { stacked: sizes.len() == 2 && pair == Some(true), list: sizes };
                cell(&mut out, left + column_w * k as f64, y, column_w, row_h, b.verdict, &sizes);
            }
        }
    }

    // Foot: how to read it, and what it is not.
    let mut x = MARGIN;
    for (v, means) in [
        (Verdict::Better, "better than the baseline"),
        (Verdict::Worse, "worse"),
        (Verdict::Mixed, "better in some ways, worse in others"),
        (Verdict::NoClearDifference, "no clear difference"),
    ] {
        cell(&mut out, x, 566.0, 76.0, 46.0, v, &Sizes::default());
        text(&mut out, x + 84.0, 594.0, 15, INK_2, "", means);
        x += 84.0 + means.chars().count() as f64 * 7.6 + 26.0;
    }
    let mut left_out = Vec::new();
    if more_rows > 0 {
        left_out.push(format!("{more_rows} more scheduler{}", if more_rows == 1 { "" } else { "s" }));
    }
    if more_columns > 0 {
        left_out.push(format!("{more_columns} more workload{}", if more_columns == 1 { "" } else { "s" }));
    }
    // How to read the grid, and what is not on the card: a line each,
    // both from the left. Side by side they ran into each other once
    // the second had rows or columns to name as missing.
    // What a number is, where the card has any: the most, not the
    // typical, which is the one thing about it a reader has to know.
    let reading = if numbers {
        "Rows: schedulers  ·  Columns: workloads  ·  Numbers: the largest clear change in that direction"
    } else {
        "Rows: schedulers  ·  Columns: workloads  ·  Compared with the baseline"
    };
    text(&mut out, MARGIN, READING, 15, MUTED, "", reading);
    text(&mut out, MARGIN, CAVEAT, 15, MUTED, "", &caveat(&left_out, c.warnings.len()));
    out.push_str("</svg>");
    Some(out)
}

/// Draw a card's SVG into a PNG file's bytes.
///
/// The text needs a font of the machine this runs on; which one is
/// taken is the first of a short list of common ones, or whatever the
/// machine has. Without any font the card would be boxes without words,
/// so that is an error.
#[cfg(feature = "png")]
pub fn png(svg: &str) -> Result<Vec<u8>, String> {
    use resvg::{tiny_skia, usvg};

    let mut options = usvg::Options::default();
    let fonts = options.fontdb_mut();
    fonts.load_system_fonts();
    let has = |family: &str| {
        fonts.faces().any(|face| face.families.iter().any(|(name, _)| name.eq_ignore_ascii_case(family)))
    };
    const PREFERRED: [&str; 7] =
        ["Inter", "Noto Sans", "DejaVu Sans", "Liberation Sans", "Cantarell", "Roboto", "Arial"];
    let family = PREFERRED
        .into_iter()
        .find(|family| has(family))
        .map(str::to_string)
        .or_else(|| fonts.faces().find_map(|face| face.families.first().map(|(name, _)| name.clone())))
        .ok_or("no font was found on this machine to write the card's text with")?;
    fonts.set_sans_serif_family(family);

    let tree = usvg::Tree::from_str(svg, &options).map_err(|e| format!("reading the card's SVG: {e}"))?;
    let size = tree.size().to_int_size();
    let mut pixmap = tiny_skia::Pixmap::new(size.width(), size.height()).ok_or("the card has no size to draw it at")?;
    resvg::render(&tree, tiny_skia::Transform::default(), &mut pixmap.as_mut());
    pixmap.encode_png().map_err(|e| format!("writing the card as PNG: {e}"))
}

/// Whether this build can draw the card as a PNG file.
pub const PNG: bool = cfg!(feature = "png");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_names_are_cut_and_say_so() {
        assert_eq!(fit("scx_cosmos", 17), "scx_cosmos");
        assert_eq!(fit("exactly seventeen", 17), "exactly seventeen");
        assert_eq!(fit("a scheduler with a long label", 17), "a scheduler with…");
        // By characters, not bytes, and without a space before the dots.
        assert_eq!(fit("żółć żółć żółć", 6), "żółć…");
        assert_eq!(fit("", 5), "");
    }

    #[test]
    fn columns_are_headed_by_kind_and_case() {
        let head = |w: &str| {
            let (kind, case) = column(w);
            format!("{kind}|{case}")
        };
        assert_eq!(head("wake-pingpong"), "pingpong|idle");
        assert_eq!(head("wake-pingpong/loaded"), "pingpong|loaded");
        assert_eq!(head("periodic-wake"), "periodic|idle");
        assert_eq!(head("wake-burst/double"), "burst|double");
        assert_eq!(head("cgroup-fair/weighted"), "cgroup|weighted");
        assert_eq!(head("idle-wake/handoff"), "idle wake|handoff");
        // One the card has no short name for keeps its own, cut to fit.
        assert_eq!(head("something-else"), "something-…|");
        assert_eq!(head("new/case"), "new|case");
    }

    #[test]
    fn the_baseline_is_said_for_what_it_is() {
        use helicoprion_core::Scheduler;
        let kernel = |bore| Scheduler::Fair { class: "EEVDF".into(), scx_capable: true, bore };
        let ext = Scheduler::Ext { name: "lavd".into(), ops: "lavd_1.1.4".into(), enable_seq: Some(3) };
        // What a matrix session calls it, and what it is.
        assert_eq!(
            baseline("default", &kernel(None)),
            "The baseline, default, is EEVDF: the scheduler built into the kernel."
        );
        assert_eq!(
            baseline("default", &kernel(Some(true))),
            "The baseline, default, is EEVDF+BORE: the scheduler built into the kernel."
        );
        // A label that is the scheduler's name is not said twice.
        assert_eq!(baseline("scx_lavd", &ext), "The baseline is scx_lavd, a sched_ext scheduler.");
        assert_eq!(baseline("eevdf", &kernel(None)), "The baseline is EEVDF, the scheduler built into the kernel.");
        // One that is something else is kept, next to what ran.
        assert_eq!(
            baseline("before the patch", &ext),
            "The baseline, before the patch, is scx_lavd: a sched_ext scheduler."
        );
    }

    #[test]
    fn what_is_left_out_is_said_with_the_warnings() {
        assert_eq!(caveat(&[], 0), "Every change and its certainty are in the report.");
        assert_eq!(caveat(&[], 1), "Every change, its certainty and 1 warning are in the report.");
        assert_eq!(caveat(&[], 13), "Every change, its certainty and 13 warnings are in the report.");
        let more = ["2 more workloads".to_string()];
        assert_eq!(
            caveat(&more, 13),
            "Not shown: 2 more workloads. Every change, its certainty and 13 warnings are in the report."
        );
        let both = ["1 more scheduler".to_string(), "2 more workloads".to_string()];
        assert_eq!(
            caveat(&both, 0),
            "Not shown: 1 more scheduler and 2 more workloads. Every change and its certainty are in the report."
        );
    }

    fn delta(median: f64, ratio: f64) -> Delta {
        Delta {
            median,
            ci: None,
            ratio,
            paired_rounds: None,
            unmatched_rounds: false,
            paired_spread: None,
            undefined_rounds: 0,
            better: helicoprion_core::Better::Lower,
        }
    }

    #[test]
    fn a_size_is_written_short_and_as_the_report_gives_it() {
        // A percentage under 2×, whole from ten up, without a sign.
        assert_eq!(size(&delta(-31.3, 0.76)), "31%");
        assert_eq!(size(&delta(26.2, 1.35)), "26%");
        assert_eq!(size(&delta(-5.34, 0.95)), "5.3%");
        assert_eq!(size(&delta(9.96, 1.11)), "10%");
        assert_eq!(size(&delta(-99.6, 0.501)), "100%");
        // A multiple from 2× either way, where the report says "N×
        // better" or "N× worse".
        assert_eq!(size(&delta(50.0, 2.0)), "2.0×");
        assert_eq!(size(&delta(-190.0, 1.0 / 2.9)), "2.9×");
        assert_eq!(size(&delta(98.4, 63.2)), "63×");
        assert_eq!(size(&delta(-33_670.0, 1.0 / 337.7)), "338×");
        assert_eq!(size(&delta(99.97, 3377.0)), ">999×");
    }

    #[test]
    fn a_cell_has_the_sizes_its_verdict_is_about() {
        let (up, down) = (Some(delta(98.4, 63.0)), Some(delta(-190.0, 1.0 / 2.9)));
        let both = [up, down];
        let said = |v, largest: &[Option<Delta>; 2]| {
            sizes(v, largest)
                .into_iter()
                .map(|(better, s)| format!("{}{s}", if better { '+' } else { '-' }))
                .collect::<Vec<_>>()
        };
        assert_eq!(said(Verdict::Better, &both), ["+63×"]);
        assert_eq!(said(Verdict::Worse, &both), ["-2.9×"]);
        assert_eq!(said(Verdict::Mixed, &both), ["+63×", "-2.9×"]);
        assert!(said(Verdict::NoClearDifference, &both).is_empty());
        // A change to nothing has no size, and the verdict stands
        // without one. Of a mixed cell both or neither: one alone
        // would be read as all there is.
        assert!(said(Verdict::Better, &[None, down]).is_empty());
        assert!(said(Verdict::Mixed, &[None, down]).is_empty());
        assert!(said(Verdict::Mixed, &[up, None]).is_empty());
    }

    #[test]
    fn sizes_are_drawn_where_all_of_them_have_room() {
        let one = |s: &str| vec![(false, s.to_string())];
        let two = |a: &str, b: &str| vec![(true, a.to_string()), (false, b.to_string())];
        let cells = [one("31%"), two("63×", "2.9×"), Vec::new()];
        // Wide columns: everything, the two of a mixed cell in a row.
        assert_eq!(layout(&cells, 120.0, 78.0), (true, Some(false)));
        // Narrow and high: one above the other.
        assert_eq!(layout(&cells, 81.0, 78.0), (true, Some(true)));
        assert_eq!(layout(&cells, 81.0, 62.0), (true, Some(true)));
        // Narrow and low, as with seven schedulers: the mixed cells go
        // without, and the single ones stay.
        assert_eq!(layout(&cells, 81.0, 44.0), (true, None));
        // One pair too wide for a row decides for all of them.
        let wide = [two("63×", "2.9×"), two(">999×", ">999×")];
        assert_eq!(layout(&wide, 120.0, 78.0), (true, Some(true)));
        assert_eq!(layout(&wide, 120.0, 44.0), (true, None));
        // The narrowest column the card draws holds any single size.
        let narrowest = (f64::from(WIDTH) - 2.0 * MARGIN - NAMES) / MAX_COLUMNS as f64;
        assert!(layout(&[one(">999×"), one("100%"), one("5.3%")], narrowest, 44.0).0);
        // Nothing to draw is nothing to refuse.
        assert_eq!(layout(&[], 81.0, 44.0), (true, Some(false)));
    }

    /// A PNG file of the card's size, where this build draws pictures
    /// and the machine has a font to write with.
    #[cfg(feature = "png")]
    #[test]
    fn the_card_is_drawn_at_its_size() {
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {WIDTH} {HEIGHT}" width="{WIDTH}" height="{HEIGHT}"><text x="10" y="40" font-family="{FONT}" font-size="20">card</text></svg>"#
        );
        let bytes = match png(&svg) {
            Ok(bytes) => bytes,
            Err(why) => {
                assert!(why.contains("no font was found"), "{why}");
                return;
            }
        };
        assert_eq!(bytes[..8], [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        // Width and height open the first chunk, big-endian.
        let be = |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
        assert_eq!((be(16), be(20)), (WIDTH, HEIGHT));
        assert!(png("not an svg").unwrap_err().starts_with("reading the card's SVG"));
    }
}
