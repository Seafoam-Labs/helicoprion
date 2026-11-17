//! Text and HTML for `sessions::Analysis`.

use std::fmt::Write as _;

use crate::compare::{UnclearReason, Verdict};
use crate::conditions::utc;
use crate::esc;
use crate::html::PAGE_CSS;
use crate::sessions::{Agreement, Analysis, Consensus, Coverage, Effect, NoiseAcross, Outcome};
use crate::svg::{fmt_delta, fmt_interval, fmt_value, pct};
use crate::theme::STYLE;

/// How the sessions agree on the scored metrics of one scheduler on one
/// workload: a count per kind of agreement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tally {
    pub scheduler: usize,
    pub workload: String,
    pub metrics: usize,
    /// Measured in every session and scored the same way in each: (as
    /// better, as worse).
    pub every: (usize, usize),
    /// Measured in every session, scored in some only, always the same way.
    pub some: usize,
    /// Measured in every session, never scored.
    pub never: usize,
    /// Scored as better in some sessions and as worse in others.
    pub opposite: Vec<String>,
    /// Not measured in every session, and not scored both ways where it
    /// was: whatever the sessions that have it say, it is not a statement
    /// about all of them.
    pub partly: usize,
}

pub fn tallies(a: &Analysis) -> Vec<Tally> {
    let mut out: Vec<Tally> = Vec::new();
    for e in &a.effects {
        let at = match out.iter().position(|t| t.scheduler == e.scheduler && t.workload == e.workload) {
            Some(at) => at,
            None => {
                out.push(Tally {
                    scheduler: e.scheduler,
                    workload: e.workload.clone(),
                    metrics: 0,
                    every: (0, 0),
                    some: 0,
                    never: 0,
                    opposite: Vec::new(),
                    partly: 0,
                });
                out.len() - 1
            }
        };
        let t = &mut out[at];
        t.metrics += 1;
        match e.agreement() {
            // A contradiction is one wherever it was measured.
            Agreement::Opposite { .. } => t.opposite.push(e.metric.clone()),
            _ if e.measured() < a.sessions.len() => t.partly += 1,
            Agreement::Every { better: true } => t.every.0 += 1,
            Agreement::Every { better: false } => t.every.1 += 1,
            Agreement::Some { .. } => t.some += 1,
            Agreement::Never => t.never += 1,
        }
    }
    out.sort_by_key(|t| t.scheduler);
    out
}

fn verdict_word(v: Option<Verdict>) -> &'static str {
    match v {
        Some(Verdict::Better) => "better",
        Some(Verdict::Worse) => "worse",
        Some(Verdict::Mixed) => "mixed",
        Some(Verdict::NoClearDifference) => "no clear difference",
        None => "not measured",
    }
}

fn direction(better: bool) -> &'static str {
    if better { "better" } else { "worse" }
}

/// "worse in all 3", "worse in 2 of 3", "better in 1, worse in 2".
///
/// Agreement is counted over the sessions that measured the metric. When
/// those are not all `total` sessions, the words say so: "better in all
/// 2" would read as every session where three of five have no such
/// measurement.
fn agreement_words(e: &Effect, total: usize) -> String {
    let n = e.measured();
    if n == total {
        return match e.agreement() {
            Agreement::Every { better } => format!("{} in all {n}", direction(better)),
            Agreement::Some { better, clear } => format!("{} in {clear} of {n}", direction(better)),
            Agreement::Opposite { better, worse } => format!("better in {better}, worse in {worse}"),
            Agreement::Never => format!("clear in none of {n}"),
        };
    }
    let measured = match n {
        1 => "the 1 measured session".to_string(),
        2 => "both measured sessions".to_string(),
        _ => format!("all {n} measured sessions"),
    };
    let of = format!("({n} of {total})");
    match e.agreement() {
        Agreement::Every { better } => format!("{} in {measured} {of}", direction(better)),
        Agreement::Some { better, clear } => {
            format!("{} in {clear} of {n} measured sessions {of}", direction(better))
        }
        Agreement::Opposite { better, worse } => {
            format!("better in {better}, worse in {worse}, measured in {n} of {total}")
        }
        Agreement::Never if n == 1 => format!("not clear in {measured} {of}"),
        Agreement::Never => format!("clear in none of {n} measured sessions {of}"),
    }
}

/// A few words for why a session does not score a change.
pub(crate) fn unclear_words(reason: UnclearReason) -> &'static str {
    match reason {
        UnclearReason::SingleSample => "single sample",
        UnclearReason::MinorPopulation => "minor",
        UnclearReason::UndefinedRounds => "zero baseline",
        UnclearReason::TooFewRounds => "few rounds",
        UnclearReason::IntervalIncludesNoChange => "incl. no change",
        UnclearReason::BelowThreshold => "below threshold",
        UnclearReason::Uneven(_) => "uneven",
        UnclearReason::Unstable => "unstable",
    }
}

/// The smallest and the largest estimate, each as the table would show it.
fn range_words(e: &Effect) -> Option<String> {
    let mut seen: Vec<&Outcome> = e.sessions.iter().flatten().collect();
    if seen.len() < 2 {
        return None;
    }
    seen.sort_by(|a, b| a.delta.median.total_cmp(&b.delta.median));
    Some(format!("{} … {}", fmt_delta(&seen[0].delta), fmt_delta(&seen[seen.len() - 1].delta)))
}

/// What few sessions can and cannot say. Five is where the warning
/// stops, as a practical floor; the words do not promise that five are
/// enough.
fn few_words(a: &Analysis) -> Option<String> {
    a.few().then(|| {
        format!(
            "{} sessions show what differed between them. They are too few for even a first idea of how often a \
             result would repeat; {} is a practical minimum for that, not a statistical guarantee.",
            a.sessions.len(),
            crate::sessions::SESSIONS_TO_GENERALISE
        )
    })
}

/// The terminal summary.
pub fn text(a: &Analysis) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{} sessions of one experiment, baseline {}:", a.sessions.len(), a.schedulers[0]);
    for (i, s) in a.sessions.iter().enumerate() {
        let _ = writeln!(out, "  {}. {}", i + 1, s.name());
    }
    let tallies = tallies(a);
    for (si, sched) in a.schedulers.iter().enumerate().skip(1) {
        let _ = writeln!(out, "\n{sched}");
        for v in a.verdicts.iter().filter(|v| v.scheduler == si) {
            let words: Vec<&str> = v.sessions.iter().map(|x| verdict_word(*x)).collect();
            let _ = writeln!(
                out,
                "  {}: {}",
                v.workload,
                if v.same() { format!("{} in every session", words[0]) } else { words.join(", ") }
            );
            let Some(t) = tallies.iter().find(|t| t.scheduler == si && t.workload == v.workload) else { continue };
            let mut parts = Vec::new();
            if t.every != (0, 0) {
                parts.push(format!(
                    "{} scored the same way in every session ({} better, {} worse)",
                    t.every.0 + t.every.1,
                    t.every.0,
                    t.every.1
                ));
            }
            if t.some > 0 {
                parts.push(format!("{} in some sessions only", t.some));
            }
            if !t.opposite.is_empty() {
                parts.push(format!("{} scored both ways ({})", t.opposite.len(), t.opposite.join(", ")));
            }
            if t.never > 0 {
                parts.push(format!("{} in none", t.never));
            }
            if t.partly > 0 {
                parts.push(format!("{} not measured in every session", t.partly));
            }
            let _ = writeln!(out, "    of {} measurements: {}", t.metrics, parts.join(", "));
        }
    }
    for (first, twin) in &a.pairs {
        let of: Vec<&NoiseAcross> = a.noise.iter().filter(|n| &n.twin == twin).collect();
        let _ = writeln!(out, "\nA/A: {twin} is {first} again");
        if of.is_empty() {
            let _ = writeln!(out, "  {}", inconclusive_words(a.sessions.len()));
            continue;
        }
        for line in coverage_words(a.coverage(twin)) {
            let _ = writeln!(out, "  {line}");
        }
        let cover = a.coverage(twin);
        let alarmed = of.iter().filter(|n| n.false_alarms() > 0).count();
        let sessions = (0..a.sessions.len())
            .filter(|i| of.iter().any(|n| n.sessions[*i].as_ref().is_some_and(|x| x.false_alarm())))
            .count();
        match a.silencing_min_effect(twin) {
            Some(silencing) => {
                let _ = writeln!(
                    out,
                    "  false alarms: {alarmed} of {} metrics, in {sessions} of {} measured sessions; a --min-effect \
                     above {}% would have scored none",
                    of.len(),
                    cover.measured,
                    crate::calibration::above(silencing)
                );
            }
            // No alarm is a finding only where something could be scored.
            None if cover.scorable > 0 => {
                let _ = writeln!(
                    out,
                    "  no false alarm in {} metrics over {} session{} that could score them",
                    of.len(),
                    cover.scorable,
                    if cover.scorable == 1 { "" } else { "s" }
                );
            }
            None => {
                let _ = writeln!(out, "  nothing could be scored, so nothing is known about false alarms");
            }
        }
        let largest = of
            .iter()
            .filter_map(|n| Some((*n, n.largest()?)))
            .max_by(|x, y| x.1.1.delta.median.abs().total_cmp(&y.1.1.delta.median.abs()));
        if let Some((n, (session, noise))) = largest {
            let _ = writeln!(
                out,
                "  largest change between the arms: {} · {} {} (session {})",
                n.workload,
                n.metric,
                fmt_delta(&noise.delta),
                session + 1
            );
        }
    }
    if let Some(few) = few_words(a) {
        let _ = writeln!(out, "\n{few}");
    }
    out
}

/// What the sessions could not say about a pair, if anything: sessions
/// without data, without an interval for any change, or not balanced.
fn coverage_words(c: Coverage) -> Vec<String> {
    let mut out = Vec::new();
    let s = |n: usize| if n == 1 { "" } else { "s" };
    if c.measured < c.sessions {
        let missing = c.sessions - c.measured;
        out.push(format!(
            "measured in {} of {} sessions; {missing} session{} without comparable data for this pair say{} nothing \
             about it",
            c.measured,
            c.sessions,
            s(missing),
            if missing == 1 { "s" } else { "" }
        ));
    }
    if c.scorable < c.measured {
        let unscorable = c.measured - c.scorable;
        out.push(format!(
            "{unscorable} measured session{} had too few rounds to score any change: no alarm there is no finding",
            s(unscorable)
        ));
    }
    if c.unbalanced > 0 {
        out.push(format!(
            "{} session{} did not balance the order of the arms: an effect of position is part of the change there",
            c.unbalanced,
            s(c.unbalanced)
        ));
    }
    out
}

/// For a pair that was run and has no measurement to compare in any
/// session.
fn inconclusive_words(sessions: usize) -> String {
    format!(
        "no comparable measurements in any of the {sessions} sessions: the calibration is inconclusive, which is \
         not the same as quiet"
    )
}

/// What the A/A pairs found, session by session.
fn noise_section(h: &mut String, a: &Analysis) {
    if a.pairs.is_empty() {
        return;
    }
    let n = a.sessions.len();
    h.push_str("<h2>A/A across sessions</h2><p class=cap>Two arms of one scheduler, session by session: how large a difference the method finds where there is none.</p><details class=help><summary>How to read this</summary><p>A cell is the repeated arm against its first arm in that session, estimated and scored like any candidate against a baseline. It is red where the session scores it as a clear change, which between two arms of one scheduler is a false alarm. <em>False alarms</em> counts those sessions. <em>Largest</em> is the largest change in any session, scored or not: the noise this metric has shown on this machine. A threshold for the metric that sits below it has already been crossed without any difference between schedulers. <em>Later against earlier</em> is the effect of position within a round (the arm visited second against the arm visited first), smallest and largest over the sessions; a range that stays on one side is a position effect that repeats. These are observations of the noise, as many as there are sessions. They do not set a threshold.</p></details>");
    for (first, twin) in &a.pairs {
        let of: Vec<&NoiseAcross> = a.noise.iter().filter(|x| &x.twin == twin).collect();
        if of.is_empty() {
            let _ = write!(
                h,
                "<p class=cap><strong>{} is {} again</strong>: {}.</p>",
                esc(twin),
                esc(first),
                inconclusive_words(n)
            );
            continue;
        }
        let alarmed = of.iter().filter(|x| x.false_alarms() > 0).count();
        let mut line = format!(
            "<strong>{} is {} again</strong>: {alarmed} of {} scored metrics raised a false alarm in some session",
            esc(twin),
            esc(first),
            of.len()
        );
        if let Some(silencing) = a.silencing_min_effect(twin) {
            let _ = write!(
                line,
                "; a --min-effect above {}% would have scored none of them",
                crate::calibration::above(silencing)
            );
        }
        for words in coverage_words(a.coverage(twin)) {
            let _ = write!(line, ". <strong>Note</strong>: {}", esc(&words));
        }
        let _ = write!(h, "<p class=cap>{line}.</p><div class=wide><table><tr><th class=l>Workload · metric</th>");
        for i in 0..n {
            let _ = write!(h, "<th>{}</th>", i + 1);
        }
        h.push_str("<th class=l>False alarms</th><th>Largest</th><th class=l>Later against earlier</th></tr>");
        for x in of {
            let _ = write!(h, "<tr><td>{} · <code>{}</code></td>", esc(&x.workload), esc(&x.metric));
            for s in &x.sessions {
                match s {
                    None => h.push_str("<td class=m>—</td>"),
                    Some(s) => {
                        let title = s
                            .delta
                            .ci
                            .map(|(lo, hi)| {
                                format!(" title=\"95% interval: {}\"", esc(&fmt_interval(&s.delta, lo, hi)))
                            })
                            .unwrap_or_default();
                        let class = if s.false_alarm() { "bad" } else { "m" };
                        let _ = write!(h, "<td class={class}{title}>{}</td>", fmt_delta(&s.delta));
                    }
                }
            }
            let alarms = x.false_alarms();
            let alarms = if alarms == 0 {
                format!("<span class=m>none in {}</span>", x.measured())
            } else {
                format!("<strong>{alarms} of {}</strong>", x.measured())
            };
            let largest = x.largest().map_or_else(|| "—".to_string(), |(_, s)| fmt_delta(&s.delta));
            let orders: Vec<f64> = x.sessions.iter().flatten().filter_map(|s| Some(s.order?.median)).collect();
            let order = match (orders.iter().copied().reduce(f64::min), orders.iter().copied().reduce(f64::max)) {
                (Some(lo), Some(hi)) if orders.len() >= 2 => format!("{} … {}", pct(lo), pct(hi)),
                (Some(only), _) => pct(only),
                _ => "—".to_string(),
            };
            let _ = write!(h, "<td class=l>{alarms}</td><td>{largest}</td><td class=l>{order}</td></tr>");
        }
        h.push_str("</table></div>");
    }
}

/// The self-contained HTML report.
pub fn html(a: &Analysis) -> String {
    let mut h = String::new();
    let n = a.sessions.len();
    let base = &a.schedulers[0];
    let _ = write!(
        h,
        r#"<!DOCTYPE html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>helicoprion — {n} sessions</title><style>{style}{PAGE_CSS}</style></head><body class="ck"><main><h1>helicoprion sessions</h1><p class="sub">{n} sessions of one experiment, baseline: <strong>{b}</strong></p>"#,
        b = esc(base),
        style = *STYLE
    );
    if let Some(few) = few_words(a) {
        let _ = write!(h, r#"<div class="warn">⚠ {}</div>"#, esc(&few));
    }
    for (i, u) in a.unverified.iter().enumerate().filter(|(_, u)| !u.is_empty()) {
        let _ = write!(
            h,
            r#"<div class="note">Session {}: not recorded, so not compared with session 1: {}.</div>"#,
            i + 1,
            esc(&u.join("; "))
        );
    }

    h.push_str("<h2>Sessions</h2><p class=cap>Each session is analysed on its own, as <code>compare</code> would, and the answers are laid side by side. Nothing is pooled.</p><details class=help><summary>How to read this</summary><p>A <code>matrix</code> session compares schedulers round by round, and its intervals come from resampling its own rounds. They cannot see what changes from one session to the next. Here every session is one observation. The sessions were checked to repeat one experiment: the same schedulers in the same order with the same options, the same workloads and parameters, the same protocol, the same machine, kernel and tool version; a difference in any of these stops the analysis. What a file does not record is listed above as not compared. Sessions are numbered oldest first.</p></details><table><tr><th></th><th class=l>Started</th><th class=l>Files from</th><th>Rounds</th></tr>");
    for (i, s) in a.sessions.iter().enumerate() {
        let rounds = s.files[0].design.as_ref().map_or(0, |d| d.rounds);
        let _ = write!(
            h,
            "<tr><td>{}</td><td class=l>{}</td><td class=l>{}</td><td>{rounds}</td></tr>",
            i + 1,
            utc(s.files[0].started_at as f64),
            esc(&s.source)
        );
    }
    h.push_str("</table>");

    let head = |h: &mut String, first: &str| {
        let _ = write!(h, "<div class=wide><table><tr><th class=l>{first}</th>");
        for i in 0..n {
            let _ = write!(h, "<th>{}</th>", i + 1);
        }
    };

    h.push_str("<h2>Verdicts</h2><p class=cap>What each session concluded about each scheduler: the result <code>compare</code> gives for that session alone.</p>");
    head(&mut h, "Scheduler · workload");
    h.push_str("<th class=l>Across sessions</th></tr>");
    for v in &a.verdicts {
        let _ = write!(h, "<tr><td>{} · {}</td>", esc(&a.schedulers[v.scheduler]), esc(&v.workload));
        for x in &v.sessions {
            let class = match x {
                Some(Verdict::Better) => "good",
                Some(Verdict::Worse) => "bad",
                _ => "m",
            };
            let _ = write!(h, "<td class={class}>{}</td>", verdict_word(*x));
        }
        let across = match v.consensus() {
            Consensus::Same(_) => "the same".to_string(),
            Consensus::Differs => "<strong>differs</strong>".to_string(),
            // Not "the same", and not a contradiction either.
            Consensus::Incomplete { agreed: None, measured } if measured >= 2 => {
                "not measured in every session; <strong>differs</strong> where it was".to_string()
            }
            Consensus::Incomplete { measured, .. } => {
                format!("not measured in every session ({measured} of {n})")
            }
        };
        let _ = write!(h, "<td class=l>{across}</td></tr>");
    }
    h.push_str("</table></div>");

    noise_section(&mut h, a);

    let _ = write!(
        h,
        "<h2>Effects against {b}</h2><p class=cap>Every scored metric, session by session: the session's own estimate, coloured where that session scores it.</p><details class=help><summary>How to read this</summary><p>A cell is one session's change against the baseline, positive or “better” meaning better for every metric, with that session's own paired estimate and scoring; hover it for the interval. A coloured cell is a change that session scores as clear. A grey one is not scored there, with the reason below it (its interval includes no change, the change is below the threshold, a minor population, too few rounds, uneven rounds, unstable runs, a baseline of zero), and does not count towards agreement whatever its sign. <em>Across sessions</em> counts sessions: scored the same way in all of them, in some of them only, as better in some and as worse in others, or in none. <em>Range</em> is the smallest and the largest estimate. A change scored the same way everywhere with a narrow range repeats in these sessions; one scored both ways does not describe the schedulers, and the levels below usually show which side moved. No interval is computed across sessions and nothing is averaged: the rounds of a session are not independent samples of how sessions differ.</p></details>",
        b = esc(base)
    );
    for (si, sched) in a.schedulers.iter().enumerate().skip(1) {
        let _ = write!(h, "<details open><summary>{}</summary>", esc(sched));
        head(&mut h, "Workload · metric");
        h.push_str("<th class=l>Across sessions</th><th class=l>Range</th></tr>");
        for e in a.effects.iter().filter(|e| e.scheduler == si) {
            let _ = write!(h, "<tr><td>{} · <code>{}</code></td>", esc(&e.workload), esc(&e.metric));
            for o in &e.sessions {
                match o {
                    None => h.push_str("<td class=m>—</td>"),
                    Some(o) => {
                        let title = o
                            .delta
                            .ci
                            .map(|(lo, hi)| {
                                format!(" title=\"95% interval: {}\"", esc(&fmt_interval(&o.delta, lo, hi)))
                            })
                            .unwrap_or_default();
                        match o.unclear {
                            None => {
                                let class = if o.delta.median > 0.0 { "good" } else { "bad" };
                                let _ = write!(h, "<td class={class}{title}>{}</td>", fmt_delta(&o.delta));
                            }
                            Some(reason) => {
                                let _ = write!(
                                    h,
                                    "<td class=m{title}>{}<br>{}</td>",
                                    fmt_delta(&o.delta),
                                    unclear_words(reason)
                                );
                            }
                        }
                    }
                }
            }
            let words = agreement_words(e, n);
            let agreement = match e.agreement() {
                Agreement::Opposite { .. } => format!("<strong>{words}</strong>"),
                Agreement::Never => format!("<span class=m>{words}</span>"),
                _ => words,
            };
            let _ = write!(
                h,
                "<td class=l>{agreement}</td><td class=l>{}</td></tr>",
                range_words(e).unwrap_or_else(|| "—".into())
            );
        }
        h.push_str("</table></div></details>");
    }

    h.push_str("<h2>Levels</h2><p class=cap>The measured values themselves, session by session: how stable each result is, apart from any comparison.</p><details class=help><summary>How to read this</summary><p>A cell is the session's median over its valid runs. <em>Largest / smallest</em> divides the largest session median by the smallest: 1.00× means the sessions measured the same. Read it next to the effects: when a scheduler's own values hardly move and the baseline's do, a disagreement between sessions is the baseline's. Every metric is listed, the baseline's, single-sample extremes and informational ones too.</p></details>");
    for (si, sched) in a.schedulers.iter().enumerate() {
        let _ = write!(
            h,
            "<details{}><summary>{}{}</summary>",
            if si == 0 { " open" } else { "" },
            esc(sched),
            if si == 0 { " (baseline)" } else { "" }
        );
        head(&mut h, "Workload · metric");
        h.push_str("<th>Largest / smallest</th></tr>");
        for l in a.levels.iter().filter(|l| l.scheduler == si) {
            let _ = write!(
                h,
                "<tr><td>{} · <code>{}</code> <span class=m>({})</span></td>",
                esc(&l.workload),
                esc(&l.metric),
                esc(&l.unit)
            );
            for v in &l.sessions {
                match v {
                    Some(v) => {
                        let _ = write!(h, "<td>{}</td>", fmt_value(*v));
                    }
                    None => h.push_str("<td class=m>—</td>"),
                }
            }
            match l.factor() {
                Some(f) => {
                    let _ = write!(h, "<td>{f:.2}×</td></tr>");
                }
                None => h.push_str("<td class=m>—</td></tr>"),
            }
        }
        h.push_str("</table></div></details>");
    }

    let _ = write!(
        h,
        "<footer>Generated by helicoprion {}. Each session's own report has its charts, conditions and diagnostics.</footer></main></body></html>",
        env!("CARGO_PKG_VERSION")
    );
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::Options;
    use crate::sessions::analyse;
    use crate::sessions::tests::session;

    /// Three sessions: lavd 1.5× slower every time; cosmos at a constant
    /// 120 µs against a baseline of 100, 100 and 150 µs.
    fn analysis() -> Analysis {
        let mut files = Vec::new();
        for (k, base) in [100.0, 100.0, 150.0].into_iter().enumerate() {
            let value = move |spec: &str, round: u32| {
                (1.0 + 0.02 * f64::from(round))
                    * match spec {
                        "default" => base,
                        "scx_lavd" => base * 1.5,
                        _ => 120.0,
                    }
            };
            files.extend(session(
                &format!("s{k}"),
                1000 * (k as u64 + 1),
                &["default", "scx_lavd", "scx_cosmos"],
                4,
                value,
            ));
        }
        analyse(files, Options::default()).unwrap()
    }

    #[test]
    fn tallies_count_kinds_of_agreement() {
        let t = tallies(&analysis());
        assert_eq!(t.len(), 2);
        assert_eq!((t[0].scheduler, t[0].metrics, t[0].every, t[0].opposite.len()), (1, 2, (0, 2), 0));
        assert_eq!((t[1].scheduler, t[1].every, t[1].some, t[1].never), (2, (0, 0), 0, 0));
        assert_eq!(t[1].opposite, ["ops", "p99"]);
    }

    #[test]
    fn the_terminal_summary_says_what_repeated() {
        let text = text(&analysis());
        let expected = "3 sessions of one experiment, baseline default:
  1. 1970-01-01 00:16 UTC (s0/00-default.json)
  2. 1970-01-01 00:33 UTC (s1/00-default.json)
  3. 1970-01-01 00:50 UTC (s2/00-default.json)

scx_lavd
  w: worse in every session
    of 2 measurements: 2 scored the same way in every session (0 better, 2 worse)

scx_cosmos
  w: worse, worse, better
    of 2 measurements: 2 scored both ways (ops, p99)

3 sessions show what differed between them. They are too few for even a first idea of how often a result would repeat; 5 is a practical minimum for that, not a statistical guarantee.
";
        assert_eq!(text, expected);
    }

    #[test]
    fn the_report_lays_sessions_side_by_side() {
        let a = analysis();
        let html = html(&a);
        assert!(html.contains("<title>helicoprion — 3 sessions</title>"));
        assert!(html.contains("⚠ 3 sessions show what differed between them."));
        // Verdicts: the same for lavd, different for cosmos.
        assert!(html.contains(
            "<tr><td>scx_lavd · w</td><td class=bad>worse</td><td class=bad>worse</td><td class=bad>worse</td><td class=l>the same</td></tr>"
        ));
        assert!(html.contains(
            "<tr><td>scx_cosmos · w</td><td class=bad>worse</td><td class=bad>worse</td><td class=good>better</td><td class=l><strong>differs</strong></td></tr>"
        ));
        // Effects: each session's own estimate and scoring.
        let row = |sched: &str, metric: &str| {
            let from = html.find(&format!("<summary>{sched}</summary>")).unwrap();
            let at = from + html[from..].find(&format!("<code>{metric}</code>")).unwrap();
            html[at..at + html[at..].find("</tr>").unwrap()].to_string()
        };
        let lavd = row("scx_lavd", "p99");
        assert_eq!(lavd.matches("<td class=bad title=\"95% interval:").count(), 3, "{lavd}");
        assert!(lavd.contains("<td class=l>worse in all 3</td>"), "{lavd}");
        let cosmos = row("scx_cosmos", "p99");
        assert_eq!((cosmos.matches("<td class=bad").count(), cosmos.matches("<td class=good").count()), (2, 1));
        assert!(cosmos.contains("<td class=l><strong>better in 1, worse in 2</strong></td>"), "{cosmos}");
        assert!(cosmos.contains("<td class=l>\u{2212}20.0% … +20.0%</td>"), "{cosmos}");
        // Levels: the baseline moved, cosmos did not.
        let from = html.find("<h2>Levels</h2>").unwrap();
        let levels = &html[from..];
        assert!(levels.contains("<summary>default (baseline)</summary>"));
        let base = &levels[levels.find("<code>p99</code>").unwrap()..];
        assert!(base[..base.find("</tr>").unwrap()].ends_with("<td>1.50×</td>"), "{}", &base[..300]);
        let cosmos = &levels[levels.find("<summary>scx_cosmos</summary>").unwrap()..];
        let cosmos = &cosmos[cosmos.find("<code>p99</code>").unwrap()..];
        assert!(cosmos[..cosmos.find("</tr>").unwrap()].ends_with("<td>1.00×</td>"));

        // Five sessions are not flagged as few; unverified fields are listed.
        let mut files = Vec::new();
        for k in 0..5u64 {
            let mut s = session(&format!("s{k}"), 1000 * (k + 1), &["default", "scx_lavd"], 3, |spec, _| {
                if spec == "default" { 100.0 } else { 150.0 }
            });
            if k == 3 {
                s[1].1.design.as_mut().unwrap().options = None;
            }
            files.extend(s);
        }
        let a = analyse(files, Options::default()).unwrap();
        let (text, html) = (text(&a), super::html(&a));
        assert!(!text.contains("too few") && !html.contains("class=\"warn\""));
        assert!(html.contains("Session 4: not recorded, so not compared with session 1: scx_lavd: scheduler options."));
        assert!(html.contains("<td class=l>worse in all 5</td>"));
    }

    /// A scheduler without valid runs in one session: its verdict is not
    /// "the same in every session", and its metrics are not counted as
    /// scored the same way in every session.
    #[test]
    fn what_was_not_measured_everywhere_is_not_called_agreement() {
        let scheds = ["default", "scx_lavd", "scx_cosmos"];
        let mut files = Vec::new();
        for k in 0..5u64 {
            let mut s = session(&format!("s{k}"), 1000 * (k + 1), &scheds, 4, |spec, _| match spec {
                "scx_lavd" => 50.0,
                "scx_cosmos" => 150.0,
                _ => 100.0,
            });
            // lavd has valid runs in two sessions of five.
            if k >= 2 {
                s[1].1.workloads[0].runs.iter_mut().for_each(|r| r.valid = false);
            }
            files.extend(s);
        }
        let a = analyse(files, Options::default()).unwrap();
        let t = tallies(&a);
        assert_eq!((t[0].scheduler, t[0].metrics, t[0].every, t[0].partly), (1, 2, (0, 0), 2));
        assert_eq!((t[1].scheduler, t[1].every, t[1].partly), (2, (0, 2), 0));

        let text = text(&a);
        assert!(text.contains(
            "scx_lavd\n  w: better, better, not measured, not measured, not measured\n    of 2 measurements: 2 not measured in every session\n"
        ), "{text}");
        assert!(!text.contains("better in every session"), "{text}");
        assert!(text.contains("scx_cosmos\n  w: worse in every session\n    of 2 measurements: 2 scored the same way in every session (0 better, 2 worse)\n"));

        let html = html(&a);
        let row = &html[html.find("<tr><td>scx_lavd · w</td>").unwrap()..];
        let row = &row[..row.find("</tr>").unwrap()];
        assert_eq!(row.matches("<td class=m>not measured</td>").count(), 3, "{row}");
        assert!(row.ends_with("<td class=l>not measured in every session (2 of 5)</td>"), "{row}");
        assert!(html.contains("<td class=l>better in both measured sessions (2 of 5)</td>"));
        assert!(html.contains("<tr><td>scx_cosmos · w</td>") && html.contains("<td class=l>the same</td>"));

        // Where it was measured, the sessions disagree: said as such, and
        // still not as a statement about every session.
        let v = crate::sessions::Verdicts {
            scheduler: 1,
            workload: "w".into(),
            sessions: vec![Some(Verdict::Better), None, Some(Verdict::Worse)],
        };
        assert_eq!(v.consensus(), Consensus::Incomplete { agreed: None, measured: 2 });
    }

    /// "In all 2" must not read as "in every session" when only two of
    /// five sessions have the measurement.
    #[test]
    fn agreement_says_how_many_sessions_measured() {
        use crate::compare::Delta;
        use helicoprion_core::Better;
        let outcome = |change: f64, unclear: Option<UnclearReason>| {
            Some(Outcome {
                delta: Delta {
                    median: change,
                    ci: Some((change - 1.0, change + 1.0)),
                    ratio: 1.0 + change / 100.0,
                    paired_rounds: Some(5),
                    unmatched_rounds: false,
                    paired_spread: None,
                    undefined_rounds: 0,
                    better: Better::Higher,
                },
                unclear,
            })
        };
        let words = |sessions: Vec<Option<Outcome>>| {
            let total = sessions.len();
            agreement_words(&Effect { scheduler: 1, workload: "w".into(), metric: "m".into(), sessions }, total)
        };
        let (b, w) = (outcome(20.0, None), outcome(-20.0, None));
        let u = outcome(1.0, Some(UnclearReason::BelowThreshold));
        // Measured everywhere: as before.
        assert_eq!(words(vec![b, b, b]), "better in all 3");
        assert_eq!(words(vec![w, u, w]), "worse in 2 of 3");
        assert_eq!(words(vec![b, w, w]), "better in 1, worse in 2");
        assert_eq!(words(vec![u, u]), "clear in none of 2");
        // Measured in some sessions only.
        assert_eq!(words(vec![b, None, b, None, None]), "better in both measured sessions (2 of 5)");
        assert_eq!(words(vec![w, w, None, w, None]), "worse in all 3 measured sessions (3 of 5)");
        assert_eq!(words(vec![None, w, None]), "worse in the 1 measured session (1 of 3)");
        assert_eq!(words(vec![w, u, None, w, None]), "worse in 2 of 3 measured sessions (3 of 5)");
        assert_eq!(words(vec![b, None, w]), "better in 1, worse in 1, measured in 2 of 3");
        assert_eq!(words(vec![u, None, None]), "not clear in the 1 measured session (1 of 3)");
        assert_eq!(words(vec![u, None, u]), "clear in none of 2 measured sessions (2 of 3)");
    }

    /// Three A/A sessions of the kernel's scheduler. In the first and the
    /// third, the arm visited second in a round is 10% slower: an effect
    /// of position. In the second, the repeated arm is 8% slower wherever
    /// it stands: one false alarm.
    fn aa() -> Analysis {
        use crate::calibration::tests::aa_session;
        let mut files = Vec::new();
        for k in 0..3u64 {
            files.extend(aa_session(
                &format!("aa{k}"),
                1000 * (k + 1),
                &["default", "default #2"],
                6,
                move |label, _, pos| match (k, pos, label) {
                    (1, _, "default") => 100.0,
                    (1, _, _) => 108.0,
                    (_, 0, _) => 100.0,
                    _ => 110.0,
                },
            ));
        }
        analyse(files, Options::default()).unwrap()
    }

    #[test]
    fn aa_pairs_are_laid_side_by_side_across_sessions() {
        let a = aa();
        assert_eq!(a.noise.len(), 2, "p99 and ops");
        let p99 = a.noise.iter().find(|n| n.metric == "p99").unwrap();
        assert_eq!((p99.first.as_str(), p99.twin.as_str()), ("default", "default #2"));
        assert_eq!((p99.measured(), p99.false_alarms()), (3, 1));
        let (session, largest) = p99.largest().unwrap();
        assert_eq!(session, 1);
        assert!((largest.delta.median + 8.0).abs() < 0.5, "{:?}", largest.delta);
        // The position effect is there in the first and the third session,
        // and no false alarm comes of it.
        for k in [0, 2] {
            let n = p99.sessions[k].as_ref().unwrap();
            assert!((n.order.unwrap().median + 10.0).abs() < 0.5 && !n.false_alarm(), "{n:?}");
        }
        let silencing = a.silencing_min_effect("default #2").unwrap();
        assert!((7.0..8.5).contains(&silencing), "{silencing}");
        assert_eq!(a.silencing_min_effect("default"), None);

        let text = text(&a);
        assert!(text.contains(
            "\nA/A: default #2 is default again\n  false alarms: 2 of 2 metrics, in 1 of 3 measured sessions; a --min-effect above 8.0% would have scored none\n  largest change between the arms: w · p99 \u{2212}8.0% (session 2)\n"
        ), "{text}");

        let html = html(&a);
        let section =
            &html[html.find("<h2>A/A across sessions</h2>").unwrap()..html.find("<h2>Effects against").unwrap()];
        assert!(section.contains(
            "<strong>default #2 is default again</strong>: 2 of 2 scored metrics raised a false alarm in some session; a --min-effect above 8.0% would have scored none of them."
        ), "{section}");
        let row = &section[section.find("<code>p99</code>").unwrap()..];
        let row = &row[..row.find("</tr>").unwrap()];
        // Scored in the second session only, and red only there.
        assert_eq!((row.matches("<td class=bad").count(), row.matches("<td class=m").count()), (1, 2), "{row}");
        assert!(row.contains("<td class=l><strong>1 of 3</strong></td><td>\u{2212}8.0%</td>"), "{row}");
        assert!(row.ends_with("<td class=l>\u{2212}10.0% … \u{2212}0.3%</td>"), "{row}");

        // Sessions without a repeated arm have no such section or lines.
        let plain = analysis();
        assert!(plain.noise.is_empty());
        assert!(!super::html(&plain).contains("A/A across sessions") && !super::text(&plain).contains("A/A"));
    }

    /// No false alarm in any session is said as such.
    #[test]
    fn quiet_aa_sessions_are_reported_as_quiet() {
        use crate::calibration::tests::aa_session;
        let mut files = Vec::new();
        for k in 0..2u64 {
            files.extend(aa_session(&format!("aa{k}"), 1000 * (k + 1), &["default", "default #2"], 6, |_, r, _| {
                100.0 + f64::from(r)
            }));
        }
        let a = analyse(files, Options::default()).unwrap();
        assert!(
            text(&a).contains(
                "A/A: default #2 is default again\n  no false alarm in 2 metrics over 2 sessions that could score them\n"
            ),
            "{}",
            text(&a)
        );
        let html = html(&a);
        assert!(html.contains("0 of 2 scored metrics raised a false alarm in some session."));
        assert!(html.contains("<span class=m>none in 2</span>"));
    }

    /// Five A/A sessions, three of them without valid runs of the
    /// repeated arm. "No false alarm" must not read as five sessions
    /// that found none; nor may sessions too short to score anything.
    #[test]
    fn sessions_without_data_do_not_confirm_a_quiet_calibration() {
        use crate::calibration::tests::aa_session;
        let sessions = |rounds: u32, broken: &[u64]| {
            let mut files = Vec::new();
            for k in 0..5u64 {
                let mut s =
                    aa_session(&format!("aa{k}"), 1000 * (k + 1), &["default", "default #2"], rounds, |_, r, _| {
                        100.0 + f64::from(r)
                    });
                if broken.contains(&k) {
                    s[1].1.workloads[0].runs.iter_mut().for_each(|r| r.valid = false);
                }
                files.extend(s);
            }
            analyse(files, Options::default()).unwrap()
        };

        let a = sessions(6, &[2, 3, 4]);
        assert_eq!(a.coverage("default #2"), Coverage { sessions: 5, measured: 2, scorable: 2, unbalanced: 0 });
        let said = text(&a);
        assert!(said.contains(
            "A/A: default #2 is default again\n  measured in 2 of 5 sessions; 3 sessions without comparable data for this pair say nothing about it\n  no false alarm in 2 metrics over 2 sessions that could score them\n"
        ), "{said}");
        assert!(!said.contains("over 5 sessions"), "{said}");
        let page = html(&a);
        assert!(
            page.contains("<strong>Note</strong>: measured in 2 of 5 sessions; 3 sessions without comparable data")
        );
        assert!(page.contains("<span class=m>none in 2</span>"));

        // Two rounds are too few for an interval: nothing could be scored,
        // which is not the same as nothing found.
        let a = sessions(2, &[]);
        assert_eq!(a.coverage("default #2"), Coverage { sessions: 5, measured: 5, scorable: 0, unbalanced: 0 });
        let said = text(&a);
        assert!(said.contains(
            "  5 measured sessions had too few rounds to score any change: no alarm there is no finding\n  nothing could be scored, so nothing is known about false alarms\n"
        ), "{said}");
        assert!(!said.contains("no false alarm in"), "{said}");

        // An odd number of rounds: measured and scorable, and not balanced.
        let a = sessions(5, &[]);
        assert_eq!(a.coverage("default #2").unbalanced, 5);
        assert!(text(&a).contains("5 sessions did not balance the order of the arms"));
        // All is well: no such lines.
        let a = sessions(6, &[]);
        assert_eq!(a.coverage("default #2"), Coverage { sessions: 5, measured: 5, scorable: 5, unbalanced: 0 });
        let said = text(&a);
        assert!(said.contains("A/A: default #2 is default again\n  no false alarm in 2 metrics over 5 sessions that could score them\n"), "{said}");
    }

    /// A session counts as able to score only when some change there has
    /// an interval. A minor population is given as the reason a change is
    /// unscored before its missing interval is, and must not make a
    /// two-round session look scorable.
    #[test]
    fn a_session_without_any_interval_is_not_scorable() {
        use crate::calibration::tests::aa_session;
        let mut files = Vec::new();
        for k in 0..2u64 {
            let mut s = aa_session(&format!("aa{k}"), 1000 * (k + 1), &["default", "default #2"], 2, |_, r, _| {
                100.0 + f64::from(r)
            });
            for (_, f) in s.iter_mut() {
                for r in &mut f.workloads[0].runs {
                    for m in r.metrics.values_mut() {
                        m.share = Some(0.001);
                    }
                }
            }
            files.extend(s);
        }
        let a = analyse(files, Options::default()).unwrap();
        let all: Vec<_> = a.noise.iter().flat_map(|n| n.sessions.iter().flatten()).collect();
        assert_eq!(all.len(), 4);
        assert!(all.iter().all(|n| n.unclear == Some(UnclearReason::MinorPopulation) && n.delta.ci.is_none()));
        assert_eq!(a.coverage("default #2"), Coverage { sessions: 2, measured: 2, scorable: 0, unbalanced: 0 });
        let said = text(&a);
        assert!(said.contains("nothing could be scored, so nothing is known about false alarms"), "{said}");
        assert!(!said.contains("no false alarm in"), "{said}");
    }

    /// Five A/A sessions in which every run of the repeated arm is
    /// invalid. The pair was run; that nothing came of it is said, rather
    /// than the calibration vanishing from the report.
    #[test]
    fn a_pair_without_any_measurement_is_reported_as_inconclusive() {
        use crate::calibration::tests::aa_session;
        let mut files = Vec::new();
        for k in 0..5u64 {
            let mut s = aa_session(&format!("aa{k}"), 1000 * (k + 1), &["default", "default #2"], 6, |_, r, _| {
                100.0 + f64::from(r)
            });
            s[1].1.workloads[0].runs.iter_mut().for_each(|r| r.valid = false);
            files.extend(s);
        }
        let a = analyse(files, Options::default()).unwrap();
        assert_eq!(a.pairs, [("default".to_string(), "default #2".to_string())]);
        assert!(a.noise.is_empty());
        assert_eq!(a.coverage("default #2"), Coverage { sessions: 5, measured: 0, scorable: 0, unbalanced: 0 });
        let said = text(&a);
        assert!(said.contains(
            "\nA/A: default #2 is default again\n  no comparable measurements in any of the 5 sessions: the calibration is inconclusive, which is not the same as quiet\n"
        ), "{said}");
        assert!(!said.contains("no false alarm"), "{said}");
        let page = html(&a);
        assert!(page.contains("<h2>A/A across sessions</h2>"));
        assert!(page.contains(
            "<p class=cap><strong>default #2 is default again</strong>: no comparable measurements in any of the 5 sessions: the calibration is inconclusive, which is not the same as quiet.</p>"
        ));

        // Sessions without a repeated arm have no pairs at all.
        assert!(analysis().pairs.is_empty());
    }
}
