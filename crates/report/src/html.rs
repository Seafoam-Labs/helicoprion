//! Self-contained HTML report and a plain-text table for the terminal.

use std::fmt::Write as _;

use helicoprion_core::Better;

use crate::compare::{Comparison, Delta, MetricRow, PairedSpread, Verdict};
use crate::esc;
use crate::stats::MIN_RUNS_FOR_CI;
use crate::svg::{delta_chart, fmt_delta, fmt_interval, fmt_share, fmt_value, tail_chart};
use crate::theme::{STYLE, series_var};

pub(crate) const PAGE_CSS: &str = r#"
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
table + .cap { margin-top: 14px; }
.wide { overflow-x: auto; } .wide td, .wide th { white-space: nowrap; }
.wide td:first-child { white-space: normal; min-width: 180px; }
details.help { margin: 0 0 16px; font-size: 13px; }
.briefs { display: grid; gap: 12px; margin: -8px 0 20px; }
.brief { background: var(--surface); border: 1px solid var(--grid); border-radius: 8px; padding: 12px 16px; }
.brief h3 { margin: 0 0 8px; font-size: 16px; font-weight: 600; }
.brief h3 i { display: inline-block; width: 12px; height: 12px; border-radius: 3px; margin-right: 8px; }
.brief h4 { margin: 10px 0 4px; font-size: 14px; font-weight: 600; }
.brief h4:first-of-type { margin-top: 0; }
.brief ul { margin: 0; padding-left: 20px; } .brief li { margin: 2px 0; }
.brief .note { margin: 6px 0 0; padding-left: 8px; border-left: 3px solid var(--grid); font-size: 13px; color: var(--ink-2); }
details.full { margin-top: 24px; } details.full > summary { font-weight: 600; color: var(--ink-1); padding: 8px 0; }
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
                    "the per-round ratios between the two schedulers differ from one another by more than ±{uneven_threshold}%, and by more than the cautious end of the interval is away from no change"
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
        "<h2>Summary</h2><p class=cap>Which result is better than the baseline <strong>{base}</strong>, counting only clear changes.</p><details class=help><summary>How to read this</summary><p>For each scheduler and workload: how many of the compared metrics are clearly better or clearly worse than the baseline's. A change is clear when the table below colours it: its interval excludes zero, it is at least {me}%, and neither side is unstable or a minor population. A share of counted events with under 20 events on either side is not compared at all, and is not among the metrics counted. <em>Better</em> means some metrics are clearly better and none clearly worse; <em>worse</em> the reverse; <em>mixed</em> both; <em>no clear difference</em> neither, which is not proof that the two are equal. <em>Unclear</em> counts the rest; those marked <em>uneven</em> would be clear, but the change differs too much from round to round (tagged in the table below). Single-sample maxima and informational metrics are not counted. This is a count, not a score: metrics overlap (several describe the same latency distribution) and are not weighed against each other, and the result holds for these workloads on this machine only. Under <code>wake-pingpong/loaded</code>, fewer round trips can be the price of more work done by the CPU hogs (<code>hog_work_per_s</code>, informational): look at both before calling a scheduler worse. <code>woken_per_s</code>, the threads actually woken per second, is informational too: it follows the mix of round trips (fewer wake-ups where fewer round trips need a sleep), so it is listed and not counted.</p></details><table><tr><th>Scheduler</th><th class=l>Workload</th><th class=l>Result vs {base}</th><th>Better</th><th>Worse</th><th>Unclear</th><th class=l>Largest clear changes</th></tr>",
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
    report_with(c, false)
}

/// "In short": the comparison in plain words (`simple::briefs`).
fn in_short_section(h: &mut String, c: &Comparison, simple: bool) {
    let briefs = crate::simple::briefs(c);
    if briefs.is_empty() {
        // The full report shows what there is. Folded away, it would leave
        // a page that does not say why there is no result.
        if simple {
            let _ = write!(h, "<h2>In short</h2><p class=sub>{}</p>", esc(&crate::simple::nothing_to_compare(c)));
        }
        return;
    }
    let _ = write!(
        h,
        "<h2>In short</h2><p class=sub>Compared with <strong>{}</strong>. {}</p><div class=briefs>",
        esc(&c.series[c.baseline].label),
        esc(&crate::simple::what(c))
    );
    // One card per scheduler, a part per workload.
    for (series, of) in crate::simple::by_scheduler(&briefs) {
        let s = &c.series[series];
        let _ = write!(
            h,
            r#"<div class=brief><h3><i style="background:{}"></i>{}</h3>"#,
            series_var(s.slot),
            esc(&s.label)
        );
        for b in of {
            let (_, cls) = verdict_word(b.verdict);
            let _ = write!(
                h,
                "<h4>{}: <span class={cls}>{}</span></h4><ul>",
                esc(&b.title),
                esc(crate::simple::verdict_phrase(b.verdict))
            );
            for p in &b.points {
                let _ = write!(h, "<li>{}</li>", esc(p));
            }
            h.push_str("</ul>");
            if let Some(note) = &b.note {
                let _ = write!(h, "<p class=note>{}</p>", esc(note));
            }
        }
        h.push_str("</div>");
    }
    h.push_str("</div>");
    if let Some(note) = crate::simple::at_least_note(&briefs) {
        let _ = write!(h, "<p class=cap>{}</p>", esc(note));
    }
}

/// The report; with `simple`, everything but the plain-words section is
/// folded into one collapsed block, for readers who only want the outcome.
/// The rest of what the files say about machine, session and scheduler
/// options, folded: a property per row, a file per column, only the
/// properties some file has.
fn system_details(h: &mut String, c: &Comparison) {
    type Get = fn(&helicoprion_core::ResultFile) -> Option<String>;
    let rows: [(&str, Get); 14] = [
        ("Kernel build", |f| f.system.kernel_build.clone()),
        ("Preemption", |f| f.system.preempt.clone()),
        ("Timer tick", |f| f.system.tick_hz.map(|hz| format!("{hz} Hz"))),
        ("Kernel parameters", |f| (!f.system.cmdline.is_empty()).then(|| f.system.cmdline.join(" "))),
        ("Topology", |f| {
            let t = f.system.topology.as_ref()?;
            let more = |n: Option<u32>, what: &str| n.map(|n| format!(", {n} {what}")).unwrap_or_default();
            Some(format!(
                "{} package(s), {} cores{}{}",
                t.packages,
                t.cores,
                more(t.l3_domains, "L3 domain(s)"),
                more(t.numa_nodes, "NUMA node(s)")
            ))
        }),
        ("CPUs the benchmark may use", |f| f.system.allowed_cpus.clone()),
        ("Isolated CPUs", |f| f.system.isolated_cpus.clone()),
        ("Cpufreq driver", |f| f.system.cpufreq_driver.clone()),
        ("Highest frequency (kHz)", |f| f.system.max_freq_khz.clone()),
        ("Boost", |f| f.system.boost.map(|b| if b { "on" } else { "off" }.to_string())),
        ("Power", |f| f.system.on_ac.map(|ac| if ac { "mains" } else { "battery" }.to_string())),
        ("Temperature from", |f| f.system.temp_sensor.clone()),
        ("Scheduler options", |f| {
            let o = f.design.as_ref()?.options.as_ref()?;
            Some(if o.is_empty() { "none".to_string() } else { o.join(" ") })
        }),
        ("Runs", |f| {
            let p = f.protocol.as_ref()?;
            let d = f.design.as_ref();
            let per = if d.is_some() { " per visit" } else { "" };
            let mut text =
                format!("{} × {} s{per}, {} warm-up, {} s pause", p.runs, p.duration_s, p.warmup, p.cooldown_s);
            if let Some(settle) = d.and_then(|d| d.settle_s) {
                let _ = write!(text, ", {settle} s settle");
            }
            if let Some(order) = d.and_then(|d| d.order.as_ref()) {
                let _ = write!(text, ", {order} order");
            }
            Some(text)
        }),
    ];
    let shown: Vec<(&str, Vec<Option<String>>)> = rows
        .iter()
        .map(|(name, get)| (*name, c.series.iter().map(|s| get(&s.file)).collect::<Vec<_>>()))
        .filter(|(_, values)| values.iter().any(Option::is_some))
        .collect();
    if shown.is_empty() {
        return;
    }
    h.push_str("<details><summary>More about the systems and how the runs were taken</summary><table><tr><th></th>");
    for s in &c.series {
        let _ = write!(h, "<th class=l>{}</th>", esc(&s.label));
    }
    h.push_str("</tr>");
    for (name, values) in shown {
        let _ = write!(h, "<tr><td>{name}</td>");
        // One cell across when every file says the same.
        if values.len() > 1 && values.iter().all(|v| v == &values[0]) {
            let _ = write!(h, "<td class=l colspan={}>{}</td>", values.len(), esc(values[0].as_deref().unwrap_or("")));
        } else {
            for v in values {
                match v {
                    Some(v) => {
                        let _ = write!(h, "<td class=l>{}</td>", esc(&v));
                    }
                    None => h.push_str("<td class=\"l m\">—</td>"),
                }
            }
        }
        h.push_str("</tr>");
    }
    h.push_str("</table></details>");
}

/// When each scheduler was measured and what the machine was doing then,
/// session by session: a table of rounds for every `matrix` session, and
/// one table with a row per file for files taken with `run`.
fn conditions_section(h: &mut String, c: &Comparison) {
    use crate::conditions::{Session, Visit, clock, conditions, utc};
    let sessions = conditions(&c.series);
    if sessions.is_empty() {
        return;
    }
    let range = |r: (f64, f64), digits: usize| {
        if format!("{:.digits$}", r.0) == format!("{:.digits$}", r.1) {
            format!("{:.digits$}", r.0)
        } else {
            format!("{:.digits$}–{:.digits$}", r.0, r.1)
        }
    };
    let overall = |s: &Session| {
        let mut parts = vec![format!("{} from the first run to the last", clock(s.span_s))];
        if let Some(t) = s.temp_after_c {
            parts.push(format!("{} °C after a visit's hottest run", range(t, 0)));
        }
        if let Some(f) = s.freq_ghz {
            parts.push(format!("{} GHz right after the runs", range(f, 2)));
        }
        if let Some(l) = s.load_max {
            parts.push(format!("load average at most {l:.2}"));
        }
        parts.join(", ")
    };
    let state = |v: &Visit| {
        let mut parts = Vec::new();
        if let Some((lo, hi)) = v.temp_c {
            parts.push(format!("{lo:.0}→{hi:.0} °C"));
        }
        if let Some(f) = v.freq_ghz {
            parts.push(format!("{f:.2} GHz"));
        }
        if let Some(l) = v.load {
            parts.push(format!("load {l:.2}"));
        }
        parts
    };
    let apart = if sessions.len() > 1 {
        format!(
            " The files come from {} sessions; each is shown on its own, with its own clock, since a round of one has nothing to do with the same round of another.",
            sessions.len()
        )
    } else {
        String::new()
    };
    let _ = write!(
        h,
        "<h2>Conditions</h2><p class=cap>When each scheduler was measured, and the state of the machine then.{apart}</p><details class=help><summary>How to read this</summary><p>Read right before and right after every run, outside the measurement. For a <code>matrix</code> session a cell is one scheduler's visit in a round: the time its first run started, counted from the session's first run; the CPU temperature from the coolest reading before a run to the hottest after one; the mean frequency of the CPUs right after the runs (before a run they idle, which says little); and the highest 1-minute load average. That average includes the benchmark's own threads of the last minute: after a workload that keeps many threads busy, such as cgroup-fair, it stays near their number well into the workloads that follow, and in a suite with such a workload it says little about what else ran on the machine. Visits of one round that differ much were not measured under the same conditions; a round that differs from the others is where drift between rounds shows. Files taken with <code>run</code> are sessions of their own and get a row each, with the time they started. Nothing here is scored: the tables are for judging whether the machine was the same machine throughout.</p></details>"
    );
    for s in sessions.iter().filter(|s| s.interleaved) {
        let _ = write!(
            h,
            "<p class=cap><strong>Session started {}</strong>: {}.</p><table><tr><th></th>",
            utc(s.started_at),
            overall(s)
        );
        for i in &s.series {
            let _ = write!(h, "<th>{}</th>", esc(&c.series[*i].label));
        }
        h.push_str("</tr>");
        for row in &s.rows {
            let name = row.round.map_or_else(|| "all runs".to_string(), |r| format!("round {}", r + 1));
            let _ = write!(h, "<tr><td>{name}</td>");
            for v in &row.visits {
                match v {
                    Some(v) => {
                        let _ = write!(
                            h,
                            "<td><span class=m>+{}</span><br>{}</td>",
                            clock(v.start_s),
                            state(v).join("<br>")
                        );
                    }
                    None => h.push_str("<td class=m>—</td>"),
                }
            }
            h.push_str("</tr>");
        }
        h.push_str("</table>");
    }
    let separate: Vec<&Session> = sessions.iter().filter(|s| !s.interleaved).collect();
    if !separate.is_empty() {
        h.push_str("<p class=cap><strong>Runs taken separately</strong>: every file is a session of its own.</p><table><tr><th>File</th><th>Started</th><th>Took</th><th>Machine</th></tr>");
        for s in separate {
            // One file, all its runs: a single visit.
            let v = s.rows.iter().flat_map(|r| r.visits.iter().flatten()).next();
            let _ = write!(
                h,
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                esc(&c.series[s.series[0]].label),
                utc(s.started_at),
                clock(s.span_s),
                v.map(|v| state(v).join(" · ")).unwrap_or_default()
            );
        }
        h.push_str("</table>");
    }
}

/// What the A/A pairs of the session show: two arms of one scheduler,
/// where every clear change is a false alarm.
fn aa_section(h: &mut String, c: &Comparison) {
    let calibrations = crate::calibration::calibrate(c);
    if calibrations.is_empty() {
        return;
    }
    let _ = write!(
        h,
        "<h2>A/A check</h2><p class=cap>The same scheduler measured as two arms: what the method finds where there is nothing to find.</p><details class=help><summary>How to read this</summary><p>An arm that repeats another is the same scheduler with the same options, visited separately in every round. Nothing differs between the two but when each ran, so their difference is the noise of the measurement on this machine, metric by metric. <em>Change</em> is the repeated arm against the first, estimated and scored like any candidate against a baseline, with the thresholds of this report (±{me}% and the others). A change scored as clear is a false alarm: the method can produce a change of that size without any difference between schedulers, so read such changes between other schedulers of this session with caution and repeat them in another session. <em>Later against earlier</em> is the effect of position: within each round, the arm visited second against the arm visited first, median over the rounds. The arms swap places every round, so over an even number of rounds an effect of position cancels in <em>Change</em> and shows here, for example a machine that is warmer for whichever arm comes second. One session is one observation: several A/A sessions, laid side by side with <code>sessions</code>, show how large these numbers get.</p></details>",
        me = c.opts.min_effect_pct
    );
    for cal in &calibrations {
        let alarms = cal.false_alarms().count();
        let mut line = format!(
            "<strong>{} is {} again</strong>: {} of {} scored metrics differ clearly between the two",
            esc(&cal.twin),
            esc(&cal.first),
            alarms,
            cal.metrics.len()
        );
        if let Some(silencing) = cal.silencing_min_effect() {
            let _ = write!(
                line,
                ", which is not the scheduler; a --min-effect above {}% would have scored none",
                crate::calibration::above(silencing)
            );
        }
        if let Some(largest) = cal.largest() {
            let _ = write!(
                line,
                ". Largest change: {} · {} {}",
                esc(&largest.workload),
                esc(&largest.metric),
                fmt_delta(&largest.delta)
            );
        }
        if let Some((before, after)) = cal.unbalanced() {
            let _ = write!(
                line,
                ". <strong>Not balanced</strong>: {} was visited before {} in {before} rounds and after it in {after}, so an effect of position is part of the change",
                esc(&cal.first),
                esc(&cal.twin)
            );
        }
        // A session that is balanced can have metrics that are not.
        let uneven = cal.unbalanced_metrics();
        if let Some(named) = crate::calibration::named_uneven(&uneven) {
            let _ = write!(
                line,
                ". <strong>Not balanced for {}</strong>, which both arms have in some rounds only: {}; an effect of position is part of {} change, and of no other",
                if uneven.len() == 1 { "one metric".to_string() } else { format!("{} metrics", uneven.len()) },
                esc(&named),
                if uneven.len() == 1 { "its" } else { "their" }
            );
        }
        let _ = write!(
            h,
            "<p class=cap>{line}.</p><div class=wide><table><tr><th class=l>Workload · metric</th><th>Change</th><th class=l>Scored</th><th>Later against earlier</th></tr>"
        );
        for m in &cal.metrics {
            let interval = m
                .delta
                .ci
                .map(|(lo, hi)| format!(" <span class=m>[{}]</span>", fmt_interval(&m.delta, lo, hi)))
                .unwrap_or_default();
            let scored = match m.unclear {
                None => "<span class=bad>clear: a false alarm</span>".to_string(),
                Some(reason) => format!("<span class=m>{}</span>", crate::sessions_report::unclear_words(reason)),
            };
            let order = m.order.map_or_else(
                || "<span class=m>—</span>".to_string(),
                |o| {
                    format!(
                        "{} <span class=m>(better in {} of {} rounds)</span>",
                        crate::svg::pct(o.median),
                        o.later_better,
                        o.rounds
                    )
                },
            );
            let _ = write!(
                h,
                "<tr><td>{} · <code>{}</code></td><td>{}{interval}</td><td class=l>{scored}</td><td>{order}</td></tr>",
                esc(&m.workload),
                esc(&m.metric),
                fmt_delta(&m.delta)
            );
        }
        h.push_str("</table></div>");
    }
}

pub fn report_with(c: &Comparison, simple: bool) -> String {
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

    // Everything from here to the footer is the full report.
    let full_from = h.len();
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
    system_details(&mut h, c);
    conditions_section(&mut h, c);
    aa_section(&mut h, c);

    if let Some(svg) = delta_chart(c) {
        let paired_note = if c.any_paired() {
            " <strong>Paired:</strong> files from one <code>matrix</code> session are compared round by round: the change is computed within each round (both schedulers measured under the same conditions), the bar is the median of those per-round changes and the interval resamples whole rounds, so drift between rounds cancels. Cells marked ⇄N were paired over N rounds. Runs of one session are never treated as independent samples: with fewer than 3 shared rounds there is a point estimate and no interval. For paired changes the stability rule above is applied to the rounds, not to the raw spread of a cell: a change is left off the chart (and tagged <em>uneven</em> in the table) when its per-round ratios differ by more than ±{us}% and by more than the effect itself. An effect large enough that even the cautious end of its interval is farther from no change than that spread outweighs it and is charted."
                .replace("{us}", &c.opts.unstable_spread_pct.to_string())
        } else {
            String::new()
        };
        let _ = write!(
            h,
            "<h2>Change vs baseline</h2><p class=cap>Each scored metric against the baseline: bars to the right are better, whiskers are 95% intervals, the shaded band is ±{me}% noise.</p><details class=help><summary>How to read this</summary><p>Bars: change between the medians of valid runs, positive = better for every metric; changes of 2× or more are labelled as multiples. For a share of counted events (<code>rtt_blocked_slow_pct</code>, <code>deadline_miss_pct</code>, <code>overrun_pct</code>) the bar is the change between the shares over all runs compared, and its whisker an approximate interval worked out from the counts (the effective-count interval, described under the table of all metrics); such a share is on the chart only where both sides have at least 20 events. Whiskers otherwise: 95% percentile-bootstrap interval, resampling the runs of both the baseline and the candidate (shown only with ≥{MIN_RUNS_FOR_CI} runs on each side). The shaded band is ±{me}%: the within-session bootstrap cannot see drift between sessions, so smaller changes are not highlighted (calibrate with an A/A run of one scheduler). Left off the chart, but listed in the table: single-sample extremes (max), informational metrics, cells whose run-to-run spread exceeds ±{us}% (marked unstable), and minor populations, i.e. metrics of a population holding under {ms}% of the run's round trips on either side (marked minor).{paired_note}</p></details><figure>{svg}</figure>",
            me = c.opts.min_effect_pct,
            us = c.opts.unstable_spread_pct,
            ms = c.opts.min_share_pct
        );
    }
    let tails: Vec<_> = c.distributions.iter().filter_map(|d| tail_chart(c, d)).collect();
    if !tails.is_empty() {
        let _ = write!(
            h,
            "<h2>Latency quantiles</h2><p class=cap>Latency at each percentile, one chart per population: a lower line is better.</p><details class=help><summary>How to read this</summary><p>One chart per population. For wake-pingpong, <code>rtt_blocked</code> are round trips in which the pinger really slept in FUTEX_WAIT (wakeup latency); <code>rtt_fast</code> are hand-offs that completed before it blocked, and are not wakeup latency. For periodic-wake, <code>wake</code> is the time from a period's release to the task running again and <code>response</code> the time from the release to its work being done. For wake-burst, all from the instant right before the call that wakes the group: <code>start</code> is the time to a woken task running (one sample per task and burst), <code>all_running</code> to the last task of the group running and <code>all_done</code> to the last one having done its work (one sample per burst, and only for bursts in which every task took part). Each curve is the quantile function of the runs' log-linear histograms (bucket width ≤1.6% of value; min and max exact), median per quantile across valid runs, plotted only up to the quantile every run has enough samples for (≥10 beyond it). Series run with different workload parameters than the baseline are left out. For wake-pingpong, whose populations are parts of the round trips, a dashed curve is a minor population, holding under {ms}% of the run's round trips: its quantiles describe a handful of unusual events, not the run. Hover a point for its value.</p></details>",
            ms = c.opts.min_share_pct
        );
        for svg in tails {
            let _ = write!(h, "<figure>{svg}</figure>");
        }
    }

    // Table view: every number behind the charts.
    let _ = write!(
        h,
        "<h2>All metrics</h2><p class=cap>Every number behind the charts: for most metrics the median of valid runs ± spread, for shares of counted events the share over all runs with its counts; then the change vs baseline with its 95% interval. Only clear changes are coloured.</p><details class=help><summary>How to read this</summary><p>For most metrics: median of valid runs, ± half the relative IQR, then the change vs baseline with its 95% bootstrap interval. Shares of counted events are worked out from their counts instead, with an approximate interval of another kind, as said below. For either, a change of 2× or more and its interval are given as multiples, and so is any end of an interval that is 2× or more away from no change, with its direction: <em>29× worse … 36× better</em> is an interval that includes no change. Of an interval that excludes no change, the end nearer to it is cut off at the digits shown, not rounded, so the least the change is estimated to be never reads larger than it is. Coloured only when the interval excludes zero, the change is at least {me}%, the metric is not a single sample and neither side is unstable (spread above ±{us}%) or a minor population (under {ms}% of round trips: a large change there means the populations differ, not that the same events got faster or slower). <em>info</em> metrics have no better direction and are never scored. A share of counted events (<code>rtt_blocked_slow_pct</code>, <code>deadline_miss_pct</code>, <code>overrun_pct</code>) is shown as the share over all valid runs, with its events and what they are out of in place of a spread; its change and interval are worked out from those counts and from how much they differ from run to run (an approximate interval, not an exact one), and the rules on unstable sides and uneven rounds do not apply to it. It is compared only where both sides have at least 20 events: a change to none, from none or to a handful is shown as counts with <em>too few events to score</em>, which is not the same as no clear difference. A change tagged <em>n of m rounds undefined</em> comes from a session where the baseline measured zero in some rounds: a percentage change does not exist there, the estimate covers the other rounds only, and it is not scored. In a paired comparison stability is judged on the per-round changes instead of the raw spread: a change tagged <em>uneven ±x%</em> is one where the ratio between the two schedulers differs from round to round by more than ±{us}% (half the interquartile range of the per-round ratios, on a relative scale, so a steady 26× counts as even; where a round has no usable ratio, typically a zero value, the per-round changes are compared in percentage points instead, <em>uneven ±x pp</em>) and by more than the effect itself, and the change is not coloured or charted even when its interval excludes zero. More than the effect itself means more than the cautious end of the interval is away from no change: <em>2.2× worse</em> with an interval of 1.9× to 2.5× and rounds ±14% apart is scored, <em>16× better</em> with an interval of 1.2× to 60× and rounds ±530% apart is not.</p></details><table><tr><th>Workload · metric</th>",
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
            // A share of counted events: the share over all runs, and
            // the counts it is made of. How its runs differ is in the
            // interval of its change, not next to its value.
            let mut td = match cell.counted() {
                Some((events, of)) => format!(
                    "{} <span class=m title=\"events out of opportunities, over all valid runs: the value is their share\">{events} / {}</span>",
                    fmt_value(cell.shown()),
                    fmt_value(of as f64)
                ),
                None => format!(
                    "{} <span class=m>±{}</span>",
                    fmt_value(cell.spread.median),
                    half_spread(cell.spread.rel_iqr_pct())
                ),
            };
            if cell.unstable && cell.pooled.is_none() {
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
                // Too few events to judge the share by: the counts
                // stand where the change would, and no percentage or
                // interval is shown that could be read as one. Asked
                // before the change is, since from a baseline of zero
                // there is no change and the counts are still due.
                if let Some(note) = c.share_note(r, i) {
                    let _ = write!(
                        td,
                        "<br><span class=m title=\"a share of counted events is compared from its counters, over the runs compared, and only where both sides have at least {} events: no change is given for this one\">{}</span>",
                        crate::compare::MIN_EVENTS,
                        esc(&note)
                    );
                } else {
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
            }
            let _ = write!(h, "<td>{td}</td>");
        }
        h.push_str("</tr>");
    }
    h.push_str("</table>");

    if !c.diagnostics.is_empty() {
        h.push_str("<h2>Run diagnostics</h2><p class=cap>Counters summed over valid runs: what each scheduler's runs consisted of. Parameters are what a workload was set up with, the same in every run, and are not summed.</p><details class=help><summary>How to read this</summary><p>For wake-pingpong: <code>roundtrips_blocked</code>/<code>roundtrips_fast</code> split round trips by whether the pinger slept; <code>roundtrips_ambiguous</code> had a wait interrupted by a signal (EINTR) without a completed sleep and are in neither histogram; <code>roundtrips_two_sided</code> are blocked round trips whose FUTEX_WAKE also woke a sleeping ponger; <code>*_waits_slept</code> are FUTEX_WAITs that really slept, <code>*_waits_eagain</code> returned at once because the token had already moved; <code>futex_woken</code> is the number of threads FUTEX_WAKE actually woke. Same-CPU means the CPU read by the ponger just before replying and the CPU read by the pinger just after receiving the reply are the same logical CPU; cross-CPU means they differ. These are two observations at two instants (either thread may move in between), useful to tell regimes apart, not proof of a local wakeup or an IPI. <code>ping_cpu_changes</code> counts observed CPU changes between round trips, not migrations. For periodic-wake: <code>periods_due</code> is fixed by the clock (tasks × run length ÷ period); <code>periods_ran</code> were started, <code>periods_late</code> of them finished their work after the next release, and <code>periods_skipped</code> were never started because an earlier period was still at work when they came due; <code>task0_periods_missed</code>, <code>task1_periods_missed</code> and so on are the late and skipped periods of each task, in the order in which the tasks wake within a period: the tasks differ in nothing but that instant. For wake-burst: <code>bursts</code> were released, of the <code>bursts_planned</code> a waker that is never late would release: each burst is due a period after the one before it came, so a late waker releases fewer, and a scheduler under which the waker runs later is asked for less work: compare <code>bursts_per_s</code> of the schedulers before comparing anything else (a run with under half of the planned bursts is invalid); <code>wakeups_due</code> is bursts × tasks, and each is one of three: <code>wakeups_ran</code> found the task asleep, woke it and were measured, <code>wakeups_overrun</code> found it still busy with an earlier burst (woken by it and waiting for a CPU, or at its work), so that it was not woken and sat the burst out, and <code>wakeups_not_asleep</code> found it done with its work but not asleep yet, which is not scored (a burst released before the task finished its work found it busy, one released after found it done, whenever the task got to look); <code>bursts_complete</code> are the bursts in which every task took part, the only ones with a time for the whole group; <code>tasks_found_asleep</code> is what the wake calls returned. The <code>release_late</code> metrics say how late the waker was for the bursts it released, against a period after the burst before. Where the parameters have <code>idle=shallow</code>, the run kept the machine out of idle states that take longer than <code>idle_latency_us</code> microseconds to leave. That takes the time to leave a deep idle state out of the waker's timing and of the start delays, where it was a large source of variation between schedulers; it does not by itself make the waker punctual or the number of bursts equal, which <code>bursts_per_s</code> and <code>release_late</code> show. Runs with and without it, or with different bounds, are not compared. The <code>wake_call</code> metrics are the time from the instant the tasks measure from to the wake call returning. The waker can lose its CPU in between, so a large value says that the waker's side may have added to the start delays observed; it does not by itself say which of them, since the waker may also have been delayed after the tasks were already running. For cgroup-fair: <code>cpu_ms_a</code> and <code>cpu_ms_b</code> are the CPU time the threads of the two control groups got in the measured windows, in milliseconds, read from the threads' own CPU clocks; their ratio against the ratio of the groups' weights is <code>weight_accuracy_pct</code>, where 100% is exact and 33% is what a split by number of threads gives in both cases of the suite. For idle-wake: <code>wakes_planned</code> is fixed by the clock (pairs × run length ÷ period), and <code>wakes_sent</code> of them were sent: a waker that comes back from its timer a period late skips what it missed, and one that comes back after the end of the window sends nothing for it, so a scheduler under which the wakers run late was asked for less: compare <code>wakes_per_s</code> of the schedulers before comparing their delays; <code>wakes_measured</code> found the task asleep and woke it alone, and only those are timed; <code>wakes_not_asleep</code> found it not asleep yet and <code>wakes_overrun</code> were seen by the task only together with the next one, and neither has a time of its own; <code>wakes_mistimed</code> had a time that lay after the task's own reading of the clock and were left out, and should be zero; <code>wakes_same_cpu</code> ran the task on the CPU its waker was on when it called and <code>wakes_prev_cpu</code> on the CPU the task had last run on, both read at two instants and not proof of what the kernel did in between. The <code>timer_late</code> metrics are how long after the instant its timer was set for a waker read the clock, for the wake-ups it sent: the waker's own wake-up, by a timer, which holds the timer's lateness, the time to leave the idle state and the time to run the waker; they are not scored, and their quantiles are not to be added to those of <code>wake</code>, since the slowest of one need not come with the slowest of the other. <code>idle_pct</code> and <code>idle_deepest_pct</code> in the table above are the share of the CPUs' time that the kernel counted as idle, and as spent in the idle state that takes longest to leave: where the second is small the machine did not go to sleep, and the wake-ups measured were not from deep idle. The kernel counts a stay in an idle state when it ends, so the shares are rough and can come out a little over 100%. With <code>--sched-diag</code>, for any workload: the <code>sched_*</code> counters are what the kernel counted for the worker threads while they worked, summed over <code>sched_threads</code> threads: <code>sched_run_ms</code> on a CPU, <code>sched_wait_ms</code> runnable and waiting for one (together <code>sched_runnable_ms</code>), <code>sched_slices</code> stints on a CPU, <code>sched_voluntary</code> switches where the thread slept or yielded and <code>sched_involuntary</code> ones where the CPU was taken from it (together <code>sched_switches</code>), and <code>sched_migrations</code> changes of CPU. They describe how a scheduler works, not how well: none of them is scored. For wake-pingpong, blocked round trips are also split into <code>rtt_blocked_same_cpu</code> / <code>rtt_blocked_cross_cpu</code> metrics in the table above.</p></details>");
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
            let _ = write!(h, "<details open><summary>{}</summary>", esc(w));
            // What the workload was set up with: a property of the
            // runs, said once, and not a counter to add up over them.
            let same = ds.windows(2).all(|d| d[0].params == d[1].params);
            if same && !ds[0].params.is_empty() {
                let _ = write!(h, "<p class=sub>Parameters: {}</p>", esc(&ds[0].params));
            } else if !same {
                h.push_str("<p class=sub>Parameters differ:");
                for d in &ds {
                    let _ = write!(h, " <strong>{}</strong>: {};", esc(&c.series[d.series].label), esc(&d.params));
                }
                h.push_str("</p>");
            }
            h.push_str("<table><tr><th>Counter</th>");
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
                ("periods_ran", "periods_due", "periods that ran, of those due"),
                ("periods_late", "periods_ran", "late share of the periods that ran"),
                ("periods_skipped", "periods_due", "skipped share of the periods due"),
                ("wakeups_ran", "wakeups_due", "wake-ups measured, of those due"),
                ("wakeups_overrun", "wakeups_due", "wake-ups lost because the task was still busy"),
                ("wakeups_not_asleep", "wakeups_due", "wake-ups that found the task done but not asleep yet"),
                ("bursts_complete", "bursts", "bursts in which every task took part"),
                ("bursts", "bursts_planned", "bursts released, of those a punctual waker would release"),
                ("sched_wait_ms", "sched_runnable_ms", "time the threads wanted a CPU and waited for one"),
                ("sched_involuntary", "sched_switches", "switches that took the CPU away from a thread"),
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

    let full = h.split_off(full_from);
    in_short_section(&mut h, c, simple);
    if simple {
        if let Some(caution) = crate::simple::caution(c) {
            let _ = write!(h, r#"<div class="warn">⚠ {}</div>"#, esc(&caution));
        }
        if let Some(environment) = crate::simple::environment(c) {
            let _ = write!(h, r#"<div class="note">{}</div>"#, esc(&environment));
        }
        let _ = write!(
            h,
            "<details class=full><summary>Full report: charts, every measurement, notes on how to read them</summary>{full}</details>"
        );
    } else {
        h.push_str(&full);
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
                    let mut s = fmt_value(cell.shown());
                    if cell.unstable && cell.pooled.is_none() {
                        s.push('~');
                    }
                    if r.minor(i) {
                        s.push('°');
                    }
                    // The counts in place of a change, as in the HTML
                    // table and in the same words, whether or not there
                    // is a change to take their place.
                    if let Some(counts) = c.share_note(r, i) {
                        s.push_str(&format!(" ({counts})"));
                    } else if i != c.baseline {
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
            "≠ uneven: the per-round ratios differ from one another by more than ±{}% and by more than the effect's cautious end (in percentage points of the change where a round has no usable ratio), never starred\n? baseline measured zero in some shared rounds: the change is undefined there and those rounds are left out, never starred",
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
