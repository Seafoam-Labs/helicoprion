//! Hand-written SVG charts. Each chart is standalone (own `<style>`), works
//! inline in HTML, follows the OS light/dark preference in browsers, falls
//! back to light colours elsewhere, and carries `<title>` tooltips on marks.

use std::fmt::Write as _;

use crate::compare::{Comparison, Distribution, MetricRow};
use crate::esc;
use crate::theme::{STYLE, fill, paint, series_token, stroke};

const FONT: f64 = 12.0;
/// Rough average glyph width for 12px system-ui; used only for layout.
const CHAR_W: f64 = 6.6;
/// The change axis never extends past ±this many percent; larger values are
/// drawn clipped and labelled with their true number.
const DELTA_CAP: f64 = 100.0;

/// Text/line attribute bundles.
struct Ink {
    t1: String,
    t2: String,
    tm: String,
    grid: String,
    axis: String,
    whisker: String,
    surface_stroke: String,
}

impl Ink {
    fn new() -> Self {
        Ink {
            t1: fill("ink-1"),
            t2: fill("ink-2"),
            tm: fill("ink-muted"),
            grid: format!(r#"{} stroke-width="1""#, stroke("grid")),
            axis: format!(r#"{} stroke-width="1""#, stroke("axis")),
            whisker: format!(r#"{} stroke-width="1.5""#, stroke("ink-2")),
            surface_stroke: stroke("surface"),
        }
    }
}

fn open(w: f64, h: f64, title: &str) -> String {
    format!(
        r#"<svg class="ck" xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" width="{w}" height="{h}" role="img" aria-label="{t}"><style>{style}</style><rect width="{w}" height="{h}" rx="8" {bg}/>"#,
        t = esc(title),
        style = *STYLE,
        bg = fill("surface")
    )
}

fn text_w(s: &str) -> f64 {
    s.chars().count() as f64 * CHAR_W
}

/// A change as a reader would say it: a multiple when the medians differ by
/// 2× or more ("34× worse"), a signed percentage otherwise.
pub fn fmt_delta(d: &crate::compare::Delta) -> String {
    let r = d.ratio;
    if r.is_finite() && r >= 2.0 {
        format!("{}× better", times(r))
    } else if r > 0.0 && r <= 0.5 {
        format!("{}× worse", times(1.0 / r))
    } else {
        // Includes a zero median on one side, where no multiple is meaningful.
        pct(d.median)
    }
}

fn times(x: f64) -> String {
    if x >= 10.0 { format!("{x:.0}") } else { format!("{x:.1}") }
}

/// Signed percent with a real minus sign.
pub fn pct(v: f64) -> String {
    if v.abs() < 0.05 {
        "0.0%".into()
    } else if v > 0.0 {
        format!("+{v:.1}%")
    } else {
        format!("\u{2212}{:.1}%", -v)
    }
}

pub fn fmt_value(v: f64) -> String {
    let a = v.abs();
    if a >= 1e6 {
        format!("{:.2}M", v / 1e6)
    } else if a >= 1e4 {
        format!("{:.1}k", v / 1e3)
    } else if a >= 100.0 {
        format!("{v:.0}")
    } else if a >= 10.0 {
        format!("{v:.1}")
    } else {
        format!("{v:.2}")
    }
}

/// Axis tick: no trailing zeros (`2`, `0.5`, `10k`, `+50%`).
fn tick(v: f64) -> String {
    if v.abs() >= 1e4 {
        format!("{}k", v / 1e3)
    } else {
        let s = format!("{v:.3}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

fn tick_pct(v: f64) -> String {
    match v.partial_cmp(&0.0) {
        Some(std::cmp::Ordering::Greater) => format!("+{}%", tick(v)),
        Some(std::cmp::Ordering::Less) => format!("\u{2212}{}%", tick(-v)),
        _ => "0".into(),
    }
}

/// Smallest "nice" step (1/2/5·10^k) giving at most `max_ticks` intervals over `span`.
fn nice_step(span: f64, max_ticks: usize) -> f64 {
    let raw = span / max_ticks as f64;
    let mag = 10f64.powf(raw.log10().floor());
    [1.0, 2.0, 5.0, 10.0].iter().map(|m| m * mag).find(|s| *s >= raw).unwrap_or(10.0 * mag)
}

/// Horizontal bar from x0 to x1 (either direction); outer end rounded unless `square`.
fn hbar(x0: f64, x1: f64, y: f64, h: f64, square: bool) -> String {
    let len = (x1 - x0).abs();
    let r = if square { 0.0 } else { 4f64.min(len).min(h / 2.0) };
    let dir = if x1 >= x0 { 1.0 } else { -1.0 };
    let xe = x1 - dir * r;
    let sweep = u8::from(dir > 0.0);
    format!(
        "M{x0:.1},{y:.1} H{xe:.1} A{r},{r} 0 0 {sweep} {x1:.1},{y1:.1} V{y2:.1} A{r},{r} 0 0 {sweep} {xe:.1},{y3:.1} H{x0:.1} Z",
        y1 = y + r,
        y2 = y + h - r,
        y3 = y + h
    )
}

fn row_label(r: &MetricRow) -> String {
    format!("{} · {}", r.workload, r.metric)
}

/// Bar chart: change vs baseline per metric, positive = better.
/// `None` when there is nothing to compare against.
pub fn delta_chart(c: &Comparison) -> Option<String> {
    let cands: Vec<usize> = (0..c.series.len()).filter(|i| *i != c.baseline).collect();
    let rows: Vec<&MetricRow> = c.rows.iter().filter(|r| cands.iter().any(|i| r.chartable(c.baseline, *i))).collect();
    if cands.is_empty() || rows.is_empty() {
        return None;
    }
    let ink = Ink::new();

    let bar_h = 12.0;
    let bar_gap = 2.0;
    let group_pad = 16.0;
    let group_h = cands.len() as f64 * (bar_h + bar_gap) - bar_gap;
    let label_w = rows.iter().map(|r| text_w(&row_label(r))).fold(0.0, f64::max) + 16.0;

    let mut extent: f64 = 5.0;
    for r in &rows {
        for i in &cands {
            if let Some(d) = r.improvement(c.baseline, *i).filter(|_| r.chartable(c.baseline, *i)) {
                let (lo, hi) = d.ci.unwrap_or((d.median, d.median));
                extent = extent.max(d.median.abs()).max(lo.abs()).max(hi.abs());
            }
        }
    }
    let extent = extent.min(DELTA_CAP);
    let step = nice_step(extent * 2.0, 8);
    let extent = (extent / step).ceil() * step;

    // Room on both sides of the plot for value labels.
    let label_room = 64.0;
    let width = (16.0 + label_w + 2.0 * label_room + 420.0).max(760.0);
    let top = 40.0;
    let plot_x0 = 16.0 + label_w + label_room;
    let plot_x1 = width - label_room;
    let plot_h = rows.len() as f64 * (group_h + group_pad) + group_pad;
    let height = top + plot_h + 44.0;
    let x = |v: f64| plot_x0 + (v.clamp(-extent, extent) + extent) / (2.0 * extent) * (plot_x1 - plot_x0);

    let base = &c.series[c.baseline].label;
    let mut s = open(width, height, &format!("Change vs {base}"));
    let _ = write!(
        s,
        r#"<text x="16" y="24" {} font-size="14" font-weight="600">Change vs baseline ({})</text>"#,
        ink.t1,
        esc(base)
    );

    // Noise band: changes inside ±min_effect are not highlighted anywhere.
    let band = c.opts.min_effect_pct.min(extent);
    if band > 0.0 {
        let _ = write!(
            s,
            r#"<rect x="{bx:.1}" y="{top}" width="{bw:.1}" height="{plot_h:.1}" {f} opacity="0.55"><title>noise band ±{band}%: smaller changes are not highlighted</title></rect>"#,
            bx = x(-band),
            bw = x(band) - x(-band),
            f = fill("grid")
        );
    }

    // Grid + tick labels.
    let n_ticks = (2.0 * extent / step).round() as i64;
    for k in 0..=n_ticks {
        let v = -extent + k as f64 * step;
        let gx = x(v);
        let line = if v.abs() < 1e-9 { &ink.axis } else { &ink.grid };
        let _ = write!(
            s,
            r#"<line x1="{gx:.1}" x2="{gx:.1}" y1="{top}" y2="{y2:.1}" {line}/><text x="{gx:.1}" y="{ty:.1}" {tm} font-size="{FONT}" text-anchor="middle">{t}</text>"#,
            y2 = top + plot_h,
            ty = top + plot_h + 16.0,
            tm = ink.tm,
            t = if v.abs() < 1e-9 { "0".into() } else { tick_pct(v) }
        );
    }
    let _ = write!(
        s,
        r#"<text x="{plot_x0:.1}" y="{y:.1}" {tm} font-size="{FONT}">← worse</text><text x="{plot_x1:.1}" y="{y:.1}" {tm} font-size="{FONT}" text-anchor="end">better →</text>"#,
        y = top + plot_h + 34.0,
        tm = ink.tm
    );

    let mut y = top + group_pad;
    for r in &rows {
        let _ = write!(
            s,
            r#"<text x="16" y="{ly:.1}" {t2} font-size="{FONT}" dominant-baseline="middle">{t}</text>"#,
            ly = y + group_h / 2.0,
            t2 = ink.t2,
            t = esc(&row_label(r))
        );
        for (k, i) in cands.iter().enumerate() {
            let by = y + k as f64 * (bar_h + bar_gap);
            if !r.chartable(c.baseline, *i) {
                continue;
            }
            let Some(d) = r.improvement(c.baseline, *i) else { continue };
            let m = d.median;
            let (lo, hi) = d.ci.unwrap_or((m, m));
            let ser = &c.series[*i];
            let cell = r.cells[*i].as_ref().expect("improvement implies cell");
            let base_cell = r.cells[c.baseline].as_ref().expect("improvement implies baseline");
            let clipped = m.abs() > extent;
            let interval = match (d.ci, d.paired_rounds) {
                (_, None) if d.unmatched_rounds => "no matching rounds: descriptive comparison only".into(),
                (Some((lo, hi)), Some(n)) => format!("paired over {n} rounds, 95% CI {} … {}", pct(lo), pct(hi)),
                (Some((lo, hi)), None) => format!("95% CI {} … {}", pct(lo), pct(hi)),
                (None, _) => "too few runs for an interval".into(),
            };
            let tip = format!(
                "{}: {} {} — {} (median {} vs {} {}; {interval}; {} vs {} valid runs)",
                ser.label,
                r.workload,
                r.metric,
                fmt_delta(&d),
                fmt_value(cell.spread.median),
                fmt_value(base_cell.spread.median),
                r.unit,
                cell.spread.n,
                base_cell.spread.n
            );
            let _ = write!(
                s,
                r#"<path d="{d}" {p}><title>{t}</title></path>"#,
                d = hbar(x(0.0), x(m), by, bar_h, clipped),
                p = paint("bar", Some(&series_token(ser.slot)), None),
                t = esc(&tip)
            );
            if clipped {
                // Break marker: the bar continues past the axis.
                let bx = x(m) - m.signum() * 8.0;
                let _ = write!(
                    s,
                    r#"<line x1="{a:.1}" x2="{b:.1}" y1="{y0:.1}" y2="{y1:.1}" {ss} stroke-width="2"/>"#,
                    a = bx - 3.0,
                    b = bx + 3.0,
                    y0 = by + bar_h,
                    y1 = by,
                    ss = ink.surface_stroke
                );
            }
            if (hi - lo).abs() > 1e-9 {
                let wy = by + bar_h / 2.0;
                let (a, b) = (x(lo), x(hi));
                let wk = &ink.whisker;
                let _ = write!(s, r#"<line {wk} x1="{a:.1}" x2="{b:.1}" y1="{wy:.1}" y2="{wy:.1}"/>"#);
                for (cx, end) in [(a, lo), (b, hi)] {
                    // Caps only where the whisker really ends inside the axis.
                    if end.abs() <= extent {
                        let _ = write!(
                            s,
                            r#"<line {wk} x1="{cx:.1}" x2="{cx:.1}" y1="{c0:.1}" y2="{c1:.1}"/>"#,
                            c0 = wy - 3.0,
                            c1 = wy + 3.0
                        );
                    }
                }
            }
            // Value label just past the outer end (bar or whisker).
            let (tx, anchor) = if m >= 0.0 { (x(m.max(hi)) + 6.0, "start") } else { (x(m.min(lo)) - 6.0, "end") };
            let _ = write!(
                s,
                r#"<text x="{tx:.1}" y="{ty:.1}" {t2} font-size="11" text-anchor="{anchor}" dominant-baseline="middle">{t}</text>"#,
                ty = by + bar_h / 2.0,
                t2 = ink.t2,
                t = fmt_delta(&d)
            );
        }
        y += group_h + group_pad;
    }

    s.push_str("</svg>");
    Some(s)
}

/// "Nines" transform for the percentile axis: 50% -> 0.30, 99.9% -> 3.
fn nines(q: f64) -> f64 {
    -(1.0 - q).log10()
}

/// Latency tail chart for one workload: percentile (x, nines scale) vs value (y, log).
pub fn tail_chart(c: &Comparison, d: &Distribution) -> Option<String> {
    let q_min = 0.5;
    let curves: Vec<(usize, Vec<(f64, f64)>)> = d
        .curves
        .iter()
        .enumerate()
        .filter_map(|(i, cv)| {
            let pts: Vec<_> = cv.as_ref()?.iter().copied().filter(|(q, v)| *q >= q_min && *v > 0.0).collect();
            (pts.len() >= 2).then_some((i, pts))
        })
        .collect();
    if curves.is_empty() {
        return None;
    }
    let ink = Ink::new();

    let (vmin, vmax) = curves
        .iter()
        .flat_map(|(_, p)| p.iter().map(|(_, v)| *v))
        .fold((f64::MAX, f64::MIN), |(a, b), v| (a.min(v), b.max(v)));
    let lmin = vmin.log10().floor();
    let lmax = vmax.log10().ceil().max(lmin + 1.0);

    let width = 820.0;
    let direct = curves.len() <= 4;
    let right =
        if direct { curves.iter().map(|(i, _)| text_w(&c.series[*i].label)).fold(0.0, f64::max) + 40.0 } else { 24.0 };
    let (x0, x1, top, bottom) = (64.0, width - right, 44.0, 300.0);
    let height = bottom + 48.0;
    let xq0 = nines(q_min);
    let xq1 = nines(0.9999);
    let x = |q: f64| x0 + (nines(q) - xq0) / (xq1 - xq0) * (x1 - x0);
    let y = |v: f64| bottom - (v.log10() - lmin) / (lmax - lmin) * (bottom - top);

    let title = format!("{} · {} — latency quantiles ({})", d.workload, d.population, d.unit);
    let mut s = open(width, height, &title);
    let _ = write!(s, r#"<text x="16" y="24" {} font-size="14" font-weight="600">{}</text>"#, ink.t1, esc(&title));

    // Y grid at 1/2/5 per decade.
    let mut decade = lmin;
    while decade <= lmax {
        for m in [1.0, 2.0, 5.0] {
            let v = m * 10f64.powf(decade);
            if v.log10() > lmax + 1e-9 {
                break;
            }
            let gy = y(v);
            let line = if decade == lmin && m == 1.0 { &ink.axis } else { &ink.grid };
            let _ = write!(
                s,
                r#"<line x1="{x0}" x2="{x1:.1}" y1="{gy:.1}" y2="{gy:.1}" {line}/><text x="{tx}" y="{gy:.1}" {tm} font-size="{FONT}" text-anchor="end" dominant-baseline="middle">{t}</text>"#,
                tx = x0 - 8.0,
                tm = ink.tm,
                t = tick(v)
            );
        }
        decade += 1.0;
    }
    // X ticks.
    let ticks = [(0.5, "p50"), (0.9, "p90"), (0.99, "p99"), (0.999, "p99.9"), (0.9999, "p99.99")];
    for (q, t) in ticks {
        let gx = x(q);
        let _ = write!(
            s,
            r#"<line x1="{gx:.1}" x2="{gx:.1}" y1="{top}" y2="{bottom}" {grid}/><text x="{gx:.1}" y="{ty}" {tm} font-size="{FONT}" text-anchor="middle">{t}</text>"#,
            ty = bottom + 18.0,
            grid = ink.grid,
            tm = ink.tm
        );
    }
    let _ = write!(
        s,
        r#"<text x="{cx:.1}" y="{ty}" {tm} font-size="{FONT}" text-anchor="middle">percentile (log tail) — lower line is better</text>"#,
        cx = (x0 + x1) / 2.0,
        ty = bottom + 38.0,
        tm = ink.tm
    );

    // Lines, then hover targets on top.
    let mut ends: Vec<(f64, usize)> = Vec::new();
    for (i, pts) in &curves {
        let ser = &c.series[*i];
        let path: String = pts
            .iter()
            .enumerate()
            .map(|(k, (q, v))| format!("{}{:.1},{:.1}", if k == 0 { "M" } else { " L" }, x(*q), y(*v)))
            .collect();
        let _ = write!(
            s,
            r#"<path d="{path}" fill="none" {st} stroke-width="2" stroke-linejoin="round" stroke-linecap="round"/>"#,
            st = stroke(&series_token(ser.slot))
        );
        if let Some((_, v)) = pts.last() {
            ends.push((y(*v), *i));
        }
    }
    for (i, pts) in &curves {
        let ser = &c.series[*i];
        for (tq, tname) in ticks {
            let Some((q, v)) = pts.iter().find(|(q, _)| (*q - tq).abs() < 1e-9) else { continue };
            let _ = write!(
                s,
                r#"<circle class="hit" cx="{cx:.1}" cy="{cy:.1}" r="9" fill="transparent"><title>{t}</title></circle><circle cx="{cx:.1}" cy="{cy:.1}" r="4" {p} stroke-width="2" opacity="0"/>"#,
                cx = x(*q),
                cy = y(*v),
                p = paint("dot", Some(&series_token(ser.slot)), Some("surface")),
                t = esc(&format!("{}: {tname} = {} {}", ser.label, fmt_value(*v), d.unit))
            );
        }
    }

    // Direct labels at line ends, nudged apart.
    if direct {
        ends.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut last = f64::MIN;
        for (ey, i) in &mut ends {
            *ey = ey.max(last + 15.0);
            last = *ey;
            let ser = &c.series[*i];
            let _ = write!(
                s,
                r#"<line x1="{a:.1}" x2="{b:.1}" y1="{ey:.1}" y2="{ey:.1}" {st} stroke-width="2" stroke-linecap="round"/><text x="{tx:.1}" y="{ey:.1}" {t2} font-size="{FONT}" dominant-baseline="middle">{t}</text>"#,
                a = x1 + 6.0,
                b = x1 + 16.0,
                tx = x1 + 22.0,
                st = stroke(&series_token(ser.slot)),
                t2 = ink.t2,
                t = esc(&ser.label)
            );
        }
    }

    s.push_str("</svg>");
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_format() {
        assert_eq!(pct(12.34), "+12.3%");
        assert_eq!(pct(-3.0), "\u{2212}3.0%");
        assert_eq!(pct(0.01), "0.0%");
    }

    #[test]
    fn deltas_switch_to_multiples() {
        use crate::compare::Delta;
        let d = |median, ratio| Delta {
            median,
            ci: None,
            ratio,
            paired_rounds: None,
            unmatched_rounds: false,
            paired_spread_pp: None,
        };
        assert_eq!(fmt_delta(&d(-3298.6, 1.0 / 33.986)), "34× worse");
        assert_eq!(fmt_delta(&d(89.6, 9.6)), "9.6× better");
        assert_eq!(fmt_delta(&d(-31.4, 1.0 / 1.314)), "\u{2212}31.4%");
        assert_eq!(fmt_delta(&d(50.0, 2.0)), "2.0× better");
    }

    #[test]
    fn steps_are_nice() {
        assert_eq!(nice_step(20.0, 8), 5.0);
        assert_eq!(nice_step(10.0, 8), 2.0);
        assert_eq!(nice_step(160.0, 8), 20.0);
    }

    #[test]
    fn ticks() {
        assert_eq!(tick(2.0), "2");
        assert_eq!(tick(0.5), "0.5");
        assert_eq!(tick(20000.0), "20k");
        assert_eq!(tick_pct(-50.0), "\u{2212}50%");
        assert_eq!(tick_pct(2.5), "+2.5%");
    }

    #[test]
    fn nines_axis() {
        assert!((nines(0.999) - 3.0).abs() < 1e-9);
    }
}
