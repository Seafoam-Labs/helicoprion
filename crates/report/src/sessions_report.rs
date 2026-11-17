//! Text and HTML for `sessions::Analysis`.

use std::fmt::Write as _;

use crate::compare::{UnclearReason, Verdict};
use crate::conditions::utc;
use crate::esc;
use crate::html::PAGE_CSS;
use crate::sessions::{Agreement, Analysis, Consensus, Effect, Outcome};
use crate::svg::{fmt_delta, fmt_interval, fmt_value};
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
fn unclear_words(reason: UnclearReason) -> &'static str {
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
    if let Some(few) = few_words(a) {
        let _ = writeln!(out, "\n{few}");
    }
    out
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
}
