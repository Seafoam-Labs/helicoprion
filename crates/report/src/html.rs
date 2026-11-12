//! Self-contained HTML report and a plain-text table for the terminal.

use std::fmt::Write as _;

use helicoprion_core::Better;

use crate::compare::{Comparison, Delta, MetricRow};
use crate::esc;
use crate::stats::MIN_RUNS_FOR_CI;
use crate::svg::{delta_chart, fmt_delta, fmt_value, pct, tail_chart};
use crate::theme::{STYLE, series_var};

const PAGE_CSS: &str = r#"
body.ck { margin: 0; background: var(--page); color: var(--ink-1);
  font: 14px/1.5 system-ui, -apple-system, "Segoe UI", sans-serif; font-variant-numeric: tabular-nums; }
main { max-width: 860px; margin: 0 auto; padding: 32px 20px 64px; }
h1 { font-size: 22px; margin: 0 0 4px; }
h2 { font-size: 16px; margin: 32px 0 10px; }
.sub { color: var(--ink-2); margin: 0 0 20px; }
.legend { display: flex; flex-wrap: wrap; gap: 6px 18px; margin: 0 0 12px; color: var(--ink-2); }
.legend span { display: inline-flex; align-items: center; gap: 6px; }
.legend i { width: 12px; height: 12px; border-radius: 3px; display: inline-block; }
figure { margin: 0 0 16px; overflow-x: auto; }
svg.ck { display: block; box-sizing: border-box; max-width: 100%; height: auto; border: 1px solid var(--ring); border-radius: 8px; }
table { border-collapse: collapse; width: 100%; background: var(--surface);
  border: 1px solid var(--ring); border-radius: 8px; overflow: hidden; font-size: 13px; }
th, td { padding: 6px 10px; text-align: right; border-bottom: 1px solid var(--grid); }
th:first-child, td:first-child, th.l, td.l { text-align: left; }
th { color: var(--ink-2); font-weight: 600; }
td .m, .m { color: var(--ink-muted); }
.good { color: var(--good); } .bad { color: var(--bad); }
.note { border-left: 3px solid var(--axis); padding: 6px 12px; background: var(--surface); margin: 6px 0; color: var(--ink-2); }
.warn { border-left: 3px solid var(--bad); padding: 6px 12px; background: var(--surface); margin: 6px 0; }
details { margin-top: 8px; } summary { cursor: pointer; color: var(--ink-2); }
footer { color: var(--ink-muted); margin-top: 40px; font-size: 12px; }
"#;

/// Text for a change cell: value, and a class that only marks a win/loss
/// when the bootstrap interval excludes zero.
/// Text for a change cell and its class. Coloured only when `highlight`:
/// notable (interval excludes zero, magnitude ≥ min effect) and stable.
fn delta_cell(d: &Delta, highlight: bool) -> (String, &'static str) {
    let cls = match () {
        _ if highlight && d.median > 0.0 => "good",
        _ if highlight && d.median < 0.0 => "bad",
        _ => "m",
    };
    let text = match d.ci {
        Some((lo, hi)) => format!("{} [{} … {}]", fmt_delta(d), pct(lo), pct(hi)),
        None => format!("{} (n&lt;{MIN_RUNS_FOR_CI})", fmt_delta(d)),
    };
    (text, cls)
}

fn row_suffix(r: &MetricRow) -> &'static str {
    match (r.better, r.extreme) {
        (Better::Neither, _) => "info",
        (Better::Lower, false) => "↓",
        (Better::Higher, false) => "↑",
        (Better::Lower, true) => "↓ single sample",
        (Better::Higher, true) => "↑ single sample",
    }
}

/// "1.3%" or "∞" (spread around a zero median).
fn half_spread(rel_iqr_pct: f64) -> String {
    if rel_iqr_pct.is_finite() { format!("{:.1}%", rel_iqr_pct / 2.0) } else { "∞".into() }
}

/// A row/cell combination whose change is coloured and starred.
fn highlighted(c: &Comparison, r: &MetricRow, i: usize, d: &Delta) -> bool {
    !r.extreme && !r.unstable_pair(c.baseline, i) && d.notable(c.opts.min_effect_pct)
}

pub fn report(c: &Comparison) -> String {
    let mut h = String::new();
    let base = &c.series[c.baseline];
    let _ = write!(
        h,
        r#"<!DOCTYPE html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>helicoprion — {b}</title><style>{style}{PAGE_CSS}</style></head><body class="ck"><main><h1>helicoprion report</h1><p class="sub">{n} result file(s), baseline: <strong>{b}</strong></p>"#,
        b = esc(&base.label),
        n = c.series.len(),
        style = *STYLE
    );

    // Legend (identity never colour-only: label text always present).
    h.push_str(r#"<div class="legend">"#);
    for s in &c.series {
        let _ = write!(
            h,
            r#"<span><i style="background:{}"></i>{}{}</span>"#,
            series_var(s.slot),
            esc(&s.label),
            if s.slot == base.slot { " (baseline)" } else { "" }
        );
    }
    h.push_str("</div>");

    for w in &c.warnings {
        let _ = write!(h, r#"<div class="warn">⚠ {}</div>"#, esc(w));
    }
    for n in &c.env_notes {
        let _ = write!(h, r#"<div class="note">{}</div>"#, esc(n));
    }

    h.push_str("<h2>Systems</h2><table><tr><th>Label</th><th class=l>Scheduler</th><th class=l>Kernel</th><th class=l>CPU</th><th>CPUs</th><th class=l>Governor / EPP</th></tr>");
    for s in &c.series {
        let sys = &s.file.system;
        let _ = write!(
            h,
            "<tr><td>{}</td><td class=l>{}</td><td class=l>{}</td><td class=l>{}</td><td>{}</td><td class=l>{} / {}</td></tr>",
            esc(&s.label),
            esc(&s.file.scheduler.to_string()),
            esc(&sys.kernel),
            esc(sys.cpu_model.as_deref().unwrap_or("?")),
            sys.usable_cpus,
            esc(sys.governor.as_deref().unwrap_or("?")),
            esc(sys.epp.as_deref().unwrap_or("?")),
        );
    }
    h.push_str("</table>");

    if let Some(svg) = delta_chart(c) {
        let _ = write!(
            h,
            "<h2>Change vs baseline</h2><p class=sub>Bars: change between the medians of valid runs, positive = better for every metric; changes of 2× or more are labelled as multiples. Whiskers: 95% percentile-bootstrap interval, resampling the runs of both the baseline and the candidate (shown only with ≥{MIN_RUNS_FOR_CI} runs on each side). The shaded band is ±{me}%: the within-session bootstrap cannot see drift between sessions, so smaller changes are not highlighted (calibrate with an A/A run of one scheduler). Left off the chart, but listed in the table: single-sample extremes (max), informational metrics, and cells whose run-to-run spread exceeds ±{us}% (marked unstable).</p><figure>{svg}</figure>",
            me = c.opts.min_effect_pct,
            us = c.opts.unstable_spread_pct
        );
    }
    let tails: Vec<_> = c.distributions.iter().filter_map(|d| tail_chart(c, d)).collect();
    if !tails.is_empty() {
        h.push_str("<h2>Latency quantiles</h2><p class=sub>One chart per population. For wake-pingpong, <code>rtt_blocked</code> are round trips in which the pinger really slept in FUTEX_WAIT (wakeup latency); <code>rtt_fast</code> are hand-offs that completed before it blocked, and are not wakeup latency. Each curve is the quantile function of the runs' log-linear histograms (bucket width ≤1.6% of value; min and max exact), median per quantile across valid runs, plotted only up to the quantile every run has enough samples for (≥10 beyond it). Series run with different workload parameters than the baseline are left out. Hover a point for its value.</p>");
        for svg in tails {
            let _ = write!(h, "<figure>{svg}</figure>");
        }
    }

    // Table view: every number behind the charts.
    let _ = write!(
        h,
        "<h2>All metrics</h2><p class=sub>Median of valid runs, ± half the relative IQR, then the change vs baseline with its 95% bootstrap interval. Coloured only when the interval excludes zero, the change is at least {me}%, the metric is not a single sample and neither side is unstable (spread above ±{us}%). <em>info</em> metrics have no better direction and are never scored.</p><table><tr><th>Workload · metric</th>",
        me = c.opts.min_effect_pct,
        us = c.opts.unstable_spread_pct
    );
    for s in &c.series {
        let _ = write!(h, "<th>{}</th>", esc(&s.label));
    }
    h.push_str("</tr>");
    for r in &c.rows {
        let _ = write!(
            h,
            "<tr><td>{} · {} <span class=m>({} {})</span></td>",
            esc(&r.workload),
            esc(&r.metric),
            esc(&r.unit),
            row_suffix(r)
        );
        for (i, cell) in r.cells.iter().enumerate() {
            let Some(cell) = cell else {
                h.push_str("<td class=m>—</td>");
                continue;
            };
            let mut td = format!(
                "{} <span class=m>±{}</span>",
                fmt_value(cell.spread.median),
                half_spread(cell.spread.rel_iqr_pct())
            );
            if cell.unstable {
                let _ = write!(
                    td,
                    " <span class=bad title=\"run-to-run spread above ±{}%\">unstable</span>",
                    c.opts.unstable_spread_pct
                );
            }
            if i != c.baseline {
                match r.improvement(c.baseline, i) {
                    Some(d) => {
                        let (text, cls) = delta_cell(&d, highlighted(c, r, i, &d));
                        let _ = write!(td, "<br><span class={cls}>{text}</span>");
                    }
                    None if !cell.comparable => td.push_str("<br><span class=m>params differ</span>"),
                    None => {}
                }
            }
            let _ = write!(h, "<td>{td}</td>");
        }
        h.push_str("</tr>");
    }
    h.push_str("</table>");

    if !c.diagnostics.is_empty() {
        h.push_str("<h2>Run diagnostics</h2><p class=sub>Counters summed over valid runs. For wake-pingpong: <code>roundtrips_blocked</code>/<code>roundtrips_fast</code> split round trips by whether the pinger slept; <code>roundtrips_ambiguous</code> had a wait interrupted by a signal (EINTR) without a completed sleep and are in neither histogram; <code>roundtrips_two_sided</code> are blocked round trips whose FUTEX_WAKE also woke a sleeping ponger; <code>*_waits_slept</code> are FUTEX_WAITs that really slept, <code>*_waits_eagain</code> returned at once because the token had already moved; <code>futex_woken</code> is the number of threads FUTEX_WAKE actually woke.</p>");
        let mut workloads: Vec<&str> = Vec::new();
        for d in &c.diagnostics {
            if !workloads.contains(&d.workload.as_str()) {
                workloads.push(&d.workload);
            }
        }
        for w in workloads {
            let ds: Vec<_> = c.diagnostics.iter().filter(|d| d.workload == w).collect();
            let mut keys: Vec<&String> = ds.iter().flat_map(|d| d.counters.keys()).collect();
            keys.sort();
            keys.dedup();
            let _ = write!(h, "<details open><summary>{}</summary><table><tr><th>Counter</th>", esc(w));
            for d in &ds {
                let _ = write!(
                    h,
                    "<th>{} <span class=m>({} runs)</span></th>",
                    esc(&c.series[d.series].label),
                    d.valid_runs
                );
            }
            h.push_str("</tr>");
            for (num, label) in [
                ("roundtrips_blocked", "blocked share (pinger slept)"),
                ("roundtrips_two_sided", "two-sided share (both sides slept)"),
                ("roundtrips_ambiguous", "ambiguous share (EINTR, excluded)"),
            ] {
                if !keys.iter().any(|k| *k == num) {
                    continue;
                }
                let _ = write!(h, "<tr><td><strong>{label}</strong></td>");
                for d in &ds {
                    match (d.counters.get(num), d.counters.get("roundtrips")) {
                        (Some(s), Some(t)) if *t > 0 => {
                            let _ = write!(h, "<td><strong>{:.1}%</strong></td>", *s as f64 / *t as f64 * 100.0);
                        }
                        _ => h.push_str("<td class=m>—</td>"),
                    }
                }
                h.push_str("</tr>");
            }
            for k in keys {
                let _ = write!(h, "<tr><td><code>{}</code></td>", esc(k));
                for d in &ds {
                    match d.counters.get(k) {
                        Some(v) => {
                            let _ = write!(h, "<td>{v}</td>");
                        }
                        None => h.push_str("<td class=m>—</td>"),
                    }
                }
                h.push_str("</tr>");
            }
            h.push_str("</table></details>");
        }
    }

    let _ = write!(
        h,
        "<footer>Generated by helicoprion {}. ± after a value is half the relative interquartile range of its runs.</footer></main></body></html>",
        env!("CARGO_PKG_VERSION")
    );
    h
}

/// Compact text table for the terminal.
pub fn text_table(c: &Comparison) -> String {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut head = vec!["metric".to_string()];
    head.extend(c.series.iter().map(|s| s.label.clone()));
    rows.push(head);
    for r in &c.rows {
        let mut row = vec![format!("{}·{} [{}{}]", r.workload, r.metric, r.unit, row_suffix(r))];
        for (i, cell) in r.cells.iter().enumerate() {
            row.push(match cell {
                None => "—".into(),
                Some(cell) => {
                    let mut s = fmt_value(cell.spread.median);
                    if cell.unstable {
                        s.push('~');
                    }
                    if i != c.baseline {
                        match r.improvement(c.baseline, i) {
                            Some(d) => {
                                let mark = if highlighted(c, r, i, &d) { "*" } else { "" };
                                s.push_str(&format!(" ({}{mark})", fmt_delta(&d)));
                            }
                            None if !cell.comparable => s.push_str(" (params differ)"),
                            None => {}
                        }
                    }
                    s
                }
            });
        }
        rows.push(row);
    }
    let cols = rows[0].len();
    let widths: Vec<usize> = (0..cols).map(|k| rows.iter().map(|r| r[k].chars().count()).max().unwrap_or(0)).collect();
    let mut out = String::new();
    for (n, r) in rows.iter().enumerate() {
        for (k, cell) in r.iter().enumerate() {
            let pad = widths[k] - cell.chars().count();
            if k == 0 {
                out.push_str(cell);
                out.push_str(&" ".repeat(pad));
            } else {
                out.push_str("  ");
                out.push_str(&" ".repeat(pad));
                out.push_str(cell);
            }
        }
        out.push('\n');
        if n == 0 {
            out.push_str(&"─".repeat(widths.iter().sum::<usize>() + 2 * (cols - 1)));
            out.push('\n');
        }
    }
    let _ = writeln!(
        out,
        "* interval excludes zero and |change| ≥ {}% (needs ≥3 valid runs per side)\n~ unstable: run-to-run spread above ±{}%, never starred",
        c.opts.min_effect_pct, c.opts.unstable_spread_pct
    );
    out
}
