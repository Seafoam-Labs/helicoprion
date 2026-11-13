//! Self-contained HTML report and a plain-text table for the terminal.

use std::fmt::Write as _;

use helicoprion_core::Better;

use crate::compare::{Comparison, Delta, MetricRow, PairedSpread, Verdict};
use crate::esc;
use crate::stats::MIN_RUNS_FOR_CI;
use crate::svg::{delta_chart, fmt_delta, fmt_interval, fmt_share, fmt_value, tail_chart};
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
.cap { color: var(--ink-2); margin: 0 0 6px; }
details.help { margin: 0 0 16px; font-size: 13px; }
details.help summary { color: var(--ink-muted); }
details.help p { color: var(--ink-2); margin: 6px 0 0; padding-left: 12px; border-left: 3px solid var(--grid); }
footer { color: var(--ink-muted); margin-top: 40px; font-size: 12px; }
"#;

/// Text for a change cell and its class. Coloured only when `highlight`:
/// notable (interval excludes zero, magnitude ≥ min effect) and stable.
/// `uneven` is `Comparison::held_back_by_unevenness` for the cell.
fn delta_cell(
    d: &Delta,
    highlight: bool,
    uneven: Option<PairedSpread>,
    uneven_threshold: f64,
) -> (String, &'static str) {
    let cls = match () {
        _ if highlight && d.median > 0.0 => "good",
        _ if highlight && d.median < 0.0 => "bad",
        _ => "m",
    };
    let paired = d
        .paired_rounds
        .map(|n| format!(" <span class=m title=\"paired over {n} rounds\">⇄{n}</span>"))
        .unwrap_or_default();
    let text = match (d.ci, d.paired_rounds) {
        (_, None) if d.unmatched_rounds => {
            format!("{} (no matching rounds: descriptive comparison only)", fmt_delta(d))
        }
        (Some((lo, hi)), _) => format!("{} [{}]{paired}", fmt_delta(d), fmt_interval(d, lo, hi)),
        (None, Some(n)) => format!("{} (paired, {n} round(s): no interval){paired}", fmt_delta(d)),
        (None, None) => format!("{} (n&lt;{MIN_RUNS_FOR_CI}){paired}", fmt_delta(d)),
    };
    let uneven = uneven
        .map(|spread| {
            let what = match spread {
                PairedSpread::Relative(_) => format!(
                    "the per-round ratios between the two schedulers differ from one another by more than ±{uneven_threshold}%"
                ),
                PairedSpread::Points(_) => format!(
                    "in some round the ratio between the two schedulers is zero or not finite (typically a zero value); the per-round changes differ by more than ±{uneven_threshold} percentage points"
                ),
            };
            format!(" <span class=bad title=\"{what}: never highlighted or charted\">uneven {spread}</span>")
        })
        .unwrap_or_default();
    let undefined = match (d.undefined_rounds, d.paired_rounds) {
        (0, _) | (_, None) => String::new(),
        (n, Some(used)) => format!(
            " <span class=bad title=\"the baseline measured zero in {n} of the {all} shared rounds, where a percentage change is undefined; the estimate covers the other {used} only, so it is never highlighted or charted\">{n} of {all} rounds undefined</span>",
            all = n + used
        ),
    };
    (text + &uneven + &undefined, cls)
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
    c.clear(r, i, d)
}

fn verdict_word(v: Verdict) -> (&'static str, &'static str) {
    match v {
        Verdict::Better => ("better", "good"),
        Verdict::Worse => ("worse", "bad"),
        Verdict::Mixed => ("mixed", "m"),
        Verdict::NoClearDifference => ("no clear difference", "m"),
    }
}

/// "Which result is better": one row per candidate and workload.
fn summary_section(h: &mut String, c: &Comparison) {
    let summary = c.summary();
    if summary.is_empty() {
        return;
    }
    let base = esc(&c.series[c.baseline].label);
    let _ = write!(
        h,
        "<h2>Summary</h2><p class=cap>Which result is better than the baseline <strong>{base}</strong>, counting only clear changes.</p><details class=help><summary>How to read this</summary><p>For each scheduler and workload: how many of the compared metrics are clearly better or clearly worse than the baseline's. A change is clear when the table below colours it: its interval excludes zero, it is at least {me}%, and neither side is unstable or a minor population. <em>Better</em> means some metrics are clearly better and none clearly worse; <em>worse</em> the reverse; <em>mixed</em> both; <em>no clear difference</em> neither, which is not proof that the two are equal. <em>Unclear</em> counts the rest; those marked <em>uneven</em> would be clear, but the change differs too much from round to round (tagged in the table below). Single-sample maxima and informational metrics are not counted. This is a count, not a score: metrics overlap (several describe the same latency distribution) and are not weighed against each other, and the result holds for these workloads on this machine only. Under <code>wake-pingpong/loaded</code>, fewer round trips can be the price of more work done by the CPU hogs (<code>hog_work_per_s</code>, informational): look at both before calling a scheduler worse.</p></details><table><tr><th>Scheduler</th><th class=l>Workload</th><th class=l>Result vs {base}</th><th>Better</th><th>Worse</th><th>Unclear</th><th class=l>Largest clear changes</th></tr>",
        me = c.opts.min_effect_pct
    );
    for s in &summary {
        let (word, cls) = verdict_word(s.verdict());
        let top: Vec<String> = s
            .top
            .iter()
            .map(|(m, d)| {
                let cls = if d.median > 0.0 { "good" } else { "bad" };
                format!("{} <span class={cls}>{}</span>", esc(m), fmt_delta(d))
            })
            .collect();
        let _ = write!(
            h,
            "<tr><td>{}</td><td class=l>{}</td><td class=l><strong class={cls}>{word}</strong></td><td>{}</td><td>{}</td><td>{}</td><td class=l>{}</td></tr>",
            esc(&c.series[s.series].label),
            esc(&s.workload),
            s.better,
            s.worse,
            if s.uneven > 0 {
                format!("{} <span class=m>({} uneven)</span>", s.unclear(), s.uneven)
            } else {
                s.unclear().to_string()
            },
            if top.is_empty() { "<span class=m>—</span>".to_string() } else { top.join(", ") }
        );
    }
    h.push_str("</table>");
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

    summary_section(&mut h, c);

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
        let paired_note = if c.any_paired() {
            " <strong>Paired:</strong> files from one <code>matrix</code> session are compared round by round: the change is computed within each round (both schedulers measured under the same conditions), the bar is the median of those per-round changes and the interval resamples whole rounds, so drift between rounds cancels. Cells marked ⇄N were paired over N rounds. Runs of one session are never treated as independent samples: with fewer than 3 shared rounds there is a point estimate and no interval."
        } else {
            ""
        };
        let _ = write!(
            h,
            "<h2>Change vs baseline</h2><p class=cap>Each scored metric against the baseline: bars to the right are better, whiskers are 95% intervals, the shaded band is ±{me}% noise.</p><details class=help><summary>How to read this</summary><p>Bars: change between the medians of valid runs, positive = better for every metric; changes of 2× or more are labelled as multiples. Whiskers: 95% percentile-bootstrap interval, resampling the runs of both the baseline and the candidate (shown only with ≥{MIN_RUNS_FOR_CI} runs on each side). The shaded band is ±{me}%: the within-session bootstrap cannot see drift between sessions, so smaller changes are not highlighted (calibrate with an A/A run of one scheduler). Left off the chart, but listed in the table: single-sample extremes (max), informational metrics, cells whose run-to-run spread exceeds ±{us}% (marked unstable), and minor populations, i.e. metrics of a population holding under {ms}% of the run's round trips on either side (marked minor).{paired_note}</p></details><figure>{svg}</figure>",
            me = c.opts.min_effect_pct,
            us = c.opts.unstable_spread_pct,
            ms = c.opts.min_share_pct
        );
    }
    let tails: Vec<_> = c.distributions.iter().filter_map(|d| tail_chart(c, d)).collect();
    if !tails.is_empty() {
        let _ = write!(
            h,
            "<h2>Latency quantiles</h2><p class=cap>Latency at each percentile, one chart per population: a lower line is better.</p><details class=help><summary>How to read this</summary><p>One chart per population. For wake-pingpong, <code>rtt_blocked</code> are round trips in which the pinger really slept in FUTEX_WAIT (wakeup latency); <code>rtt_fast</code> are hand-offs that completed before it blocked, and are not wakeup latency. Each curve is the quantile function of the runs' log-linear histograms (bucket width ≤1.6% of value; min and max exact), median per quantile across valid runs, plotted only up to the quantile every run has enough samples for (≥10 beyond it). Series run with different workload parameters than the baseline are left out. A dashed curve is a minor population, holding under {ms}% of the run's round trips: its quantiles describe a handful of unusual events, not the run. Hover a point for its value.</p></details>",
            ms = c.opts.min_share_pct
        );
        for svg in tails {
            let _ = write!(h, "<figure>{svg}</figure>");
        }
    }

    // Table view: every number behind the charts.
    let _ = write!(
        h,
        "<h2>All metrics</h2><p class=cap>Every number behind the charts: median of valid runs ± spread, then the change vs baseline with its 95% interval. Only clear changes are coloured.</p><details class=help><summary>How to read this</summary><p>Median of valid runs, ± half the relative IQR, then the change vs baseline with its 95% bootstrap interval; a change of 2× or more and its interval are given as multiples. Coloured only when the interval excludes zero, the change is at least {me}%, the metric is not a single sample and neither side is unstable (spread above ±{us}%) or a minor population (under {ms}% of round trips: a large change there means the populations differ, not that the same events got faster or slower). <em>info</em> metrics have no better direction and are never scored. A change tagged <em>n of m rounds undefined</em> comes from a session where the baseline measured zero in some rounds: a percentage change does not exist there, the estimate covers the other rounds only, and it is not scored. In a paired comparison stability is judged on the per-round changes instead of the raw spread: a change tagged <em>uneven ±x%</em> is one where the ratio between the two schedulers differs from round to round by more than ±{us}% (half the interquartile range of the per-round ratios, on a relative scale, so a steady 26× counts as even; where a round has no usable ratio, typically a zero value, the per-round changes are compared in percentage points instead, <em>uneven ±x pp</em>), so it is not coloured or charted even when its interval excludes zero.</p></details><table><tr><th>Workload · metric</th>",
        me = c.opts.min_effect_pct,
        us = c.opts.unstable_spread_pct,
        ms = c.opts.min_share_pct
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
            if let Some(share) = cell.share.filter(|_| r.minor(i)) {
                let _ = write!(
                    td,
                    " <span class=bad title=\"this population is {s} of the run's round trips, under the {ms}% threshold: compared, never highlighted or charted\">minor {s}</span>",
                    s = esc(&fmt_share(share)),
                    ms = c.opts.min_share_pct
                );
            }
            if i != c.baseline {
                match r.improvement(c.baseline, i) {
                    Some(d) => {
                        let (text, cls) = delta_cell(
                            &d,
                            highlighted(c, r, i, &d),
                            c.held_back_by_unevenness(r, i, &d),
                            c.opts.unstable_spread_pct,
                        );
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
        h.push_str("<h2>Run diagnostics</h2><p class=cap>Counters summed over valid runs: what kind of round trips each scheduler produced.</p><details class=help><summary>How to read this</summary><p>For wake-pingpong: <code>roundtrips_blocked</code>/<code>roundtrips_fast</code> split round trips by whether the pinger slept; <code>roundtrips_ambiguous</code> had a wait interrupted by a signal (EINTR) without a completed sleep and are in neither histogram; <code>roundtrips_two_sided</code> are blocked round trips whose FUTEX_WAKE also woke a sleeping ponger; <code>*_waits_slept</code> are FUTEX_WAITs that really slept, <code>*_waits_eagain</code> returned at once because the token had already moved; <code>futex_woken</code> is the number of threads FUTEX_WAKE actually woke. Same-CPU means the CPU read by the ponger just before replying and the CPU read by the pinger just after receiving the reply are the same logical CPU; cross-CPU means they differ. These are two observations at two instants (either thread may move in between), useful to tell regimes apart, not proof of a local wakeup or an IPI. <code>ping_cpu_changes</code> counts observed CPU changes between round trips, not migrations. Blocked round trips are also split into <code>rtt_blocked_same_cpu</code> / <code>rtt_blocked_cross_cpu</code> metrics in the table above.</p></details>");
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
            for (num, den, label) in [
                ("roundtrips_blocked", "roundtrips", "blocked share (pinger slept)"),
                ("roundtrips_two_sided", "roundtrips", "two-sided share (both sides slept)"),
                ("roundtrips_blocked_same_cpu", "roundtrips_blocked", "same-CPU share of blocked round trips"),
                ("roundtrips_fast_same_cpu", "roundtrips_fast", "same-CPU share of fast round trips"),
                ("ping_cpu_changes", "roundtrips", "pinger seen on a new CPU, per round trip"),
                ("roundtrips_ambiguous", "roundtrips", "ambiguous share (EINTR, excluded)"),
            ] {
                if !keys.iter().any(|k| *k == num) {
                    continue;
                }
                let _ = write!(h, "<tr><td><strong>{label}</strong></td>");
                for d in &ds {
                    match (d.counters.get(num), d.counters.get(den)) {
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
        let mut row = vec![format!("{}·{} [{} {}]", r.workload, r.metric, r.unit, row_suffix(r))];
        for (i, cell) in r.cells.iter().enumerate() {
            row.push(match cell {
                None => "—".into(),
                Some(cell) => {
                    let mut s = fmt_value(cell.spread.median);
                    if cell.unstable {
                        s.push('~');
                    }
                    if r.minor(i) {
                        s.push('°');
                    }
                    if i != c.baseline {
                        match r.improvement(c.baseline, i) {
                            Some(d) => {
                                let mark = if highlighted(c, r, i, &d) {
                                    "*"
                                } else if d.undefined_rounds > 0 {
                                    "?"
                                } else if c.held_back_by_unevenness(r, i, &d).is_some() {
                                    "≠"
                                } else {
                                    ""
                                };
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
        "* interval excludes zero and |change| ≥ {}% (needs ≥3 valid runs per side)\n~ unstable: run-to-run spread above ±{}%, never starred\n° minor population: under {}% of the run's round trips; neither it nor a change against it is starred",
        c.opts.min_effect_pct, c.opts.unstable_spread_pct, c.opts.min_share_pct
    );
    if c.any_paired() {
        out.push_str(
            "  matrix session: changes are paired round by round (median of per-round changes, rounds resampled)\n",
        );
        let _ = writeln!(
            out,
            "≠ uneven: the per-round ratios differ from one another by more than ±{}% (in percentage points of the change where a round has no usable ratio), never starred\n? baseline measured zero in some shared rounds: the change is undefined there and those rounds are left out, never starred",
            c.opts.unstable_spread_pct
        );
    }
    let summary = c.summary();
    if !summary.is_empty() {
        out.push_str("\nSummary (clear changes only, i.e. the starred ones; a count, not a score):\n");
    }
    for s in &summary {
        let _ = writeln!(
            out,
            "  {} vs {} · {}: {} ({} better, {} worse, {} unclear{} of {} metrics)",
            c.series[s.series].label,
            c.series[c.baseline].label,
            s.workload,
            verdict_word(s.verdict()).0,
            s.better,
            s.worse,
            s.unclear(),
            if s.uneven > 0 { format!(", {} of them uneven across rounds", s.uneven) } else { String::new() },
            s.compared
        );
    }
    out
}
