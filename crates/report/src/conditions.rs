//! What the machine was doing while the runs were taken, from the readings
//! around every run: when each scheduler was measured, how warm the CPU
//! was, how fast it ran and how busy the machine was otherwise.
//!
//! This only lays the readings out. Whether a difference between two
//! visits is enough to explain a difference between two results is not
//! judged here.

use helicoprion_core::{Reading, ResultFile, RunResult};

use crate::compare::{Series, SessionKey, session_key};

/// One scheduler's runs in one round (or all runs of a file taken with
/// `run`).
#[derive(Debug, Clone, PartialEq)]
pub struct Visit {
    /// First and last reading, in seconds from the session's first.
    pub start_s: f64,
    pub end_s: f64,
    /// Lowest temperature before a run and highest after one, °C.
    pub temp_c: Option<(f64, f64)>,
    /// Mean CPU frequency right after the runs, GHz.
    pub freq_ghz: Option<f64>,
    /// Highest 1-minute load average.
    pub load: Option<f64>,
    pub runs: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// Round of a `matrix` session; `None` for a file taken with `run`.
    pub round: Option<u32>,
    /// One per series of the session, `None` where it has no readings in
    /// this round.
    pub visits: Vec<Option<Visit>>,
}

/// The readings of one session: the files of one `matrix` session that
/// are in the comparison, or one file taken with `run`. Never more: two
/// sessions both have a round 1, hours or days apart, and laying those
/// side by side would describe a session that did not happen.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    /// Indices into the comparison's series, in its order.
    pub series: Vec<usize>,
    /// From `matrix`; otherwise a single file taken with `run`.
    pub interleaved: bool,
    /// Unix time of the first reading.
    pub started_at: f64,
    pub rows: Vec<Row>,
    /// From the first reading to the last, seconds.
    pub span_s: f64,
    /// Lowest and highest of the visits' values.
    pub temp_after_c: Option<(f64, f64)>,
    pub freq_ghz: Option<(f64, f64)>,
    pub load_max: Option<f64>,
}

fn readings(r: &RunResult) -> impl Iterator<Item = &Reading> {
    r.before.iter().chain(r.after.iter())
}

fn runs(f: &ResultFile) -> impl Iterator<Item = &RunResult> {
    f.workloads.iter().flat_map(|w| w.runs.iter())
}

fn span(values: impl Iterator<Item = f64>) -> Option<(f64, f64)> {
    values.fold(None, |acc, v| match acc {
        None => Some((v, v)),
        Some((lo, hi)) => Some((lo.min(v), hi.max(v))),
    })
}

fn visit(f: &ResultFile, round: Option<u32>, t0: f64) -> Option<Visit> {
    let rs: Vec<&RunResult> = runs(f).filter(|r| r.round == round && readings(r).next().is_some()).collect();
    let (start, end) = span(rs.iter().flat_map(|r| readings(r)).map(|x| x.at))?;
    let before = span(rs.iter().filter_map(|r| r.before.as_ref()?.temp_c));
    let after = span(rs.iter().filter_map(|r| r.after.as_ref()?.temp_c));
    let freq: Vec<f64> = rs.iter().filter_map(|r| Some(r.after.as_ref()?.freq_mhz?.mean)).collect();
    Some(Visit {
        start_s: start - t0,
        end_s: end - t0,
        // With only one side read, that side stands for both.
        temp_c: match (before, after) {
            (Some(b), Some(a)) => Some((b.0, a.1)),
            (Some(x), None) | (None, Some(x)) => Some(x),
            (None, None) => None,
        },
        freq_ghz: (!freq.is_empty()).then(|| freq.iter().sum::<f64>() / freq.len() as f64 / 1e3),
        load: span(rs.iter().flat_map(|r| readings(r)).filter_map(|x| x.loadavg_1m)).map(|s| s.1),
        runs: rs.len(),
    })
}

fn session(series: &[Series], members: Vec<usize>, interleaved: bool) -> Option<Session> {
    let all = || members.iter().flat_map(|i| runs(&series[*i].file));
    let (t0, t1) = span(all().flat_map(readings).map(|x| x.at))?;
    let mut rounds: Vec<Option<u32>> = all().filter(|r| readings(r).next().is_some()).map(|r| r.round).collect();
    rounds.sort();
    rounds.dedup();
    let rows: Vec<Row> = rounds
        .into_iter()
        .map(|round| Row { round, visits: members.iter().map(|i| visit(&series[*i].file, round, t0)).collect() })
        .collect();
    let visits = || rows.iter().flat_map(|r| r.visits.iter().flatten());
    Some(Session {
        interleaved,
        started_at: t0,
        span_s: t1 - t0,
        temp_after_c: span(visits().filter_map(|v| Some(v.temp_c?.1))),
        freq_ghz: span(visits().filter_map(|v| v.freq_ghz)),
        load_max: span(visits().filter_map(|v| v.load)).map(|s| s.1),
        rows,
        series: members,
    })
}

/// The sessions behind the series, in the order of their first file.
/// Files of one `matrix` session (`compare::session_key`) form one; every
/// other file is a session of its own. Sessions without any reading
/// (files written before readings were recorded) are left out.
pub fn conditions(series: &[Series]) -> Vec<Session> {
    let keys: Vec<Option<SessionKey>> = series.iter().map(|s| session_key(&s.file)).collect();
    let mut groups: Vec<(Vec<usize>, bool)> = Vec::new();
    for (i, key) in keys.iter().enumerate() {
        match key {
            Some(k) => match groups.iter_mut().find(|(m, il)| *il && keys[m[0]].as_ref() == Some(k)) {
                Some((members, _)) => members.push(i),
                None => groups.push((vec![i], true)),
            },
            None => groups.push((vec![i], false)),
        }
    }
    groups.into_iter().filter_map(|(members, interleaved)| session(series, members, interleaved)).collect()
}

/// `2026-11-16 19:40 UTC` for a Unix time. The files record no time zone,
/// and a report read elsewhere should not guess one.
pub fn utc(unix: f64) -> String {
    let secs = unix.max(0.0) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let (d, m) = (doy - (153 * mp + 2) / 5 + 1, if mp < 10 { mp + 3 } else { mp - 9 });
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02} UTC", rem / 3600, rem / 60 % 60)
}

/// `1:05` or `2:03:07`.
pub fn clock(seconds: f64) -> String {
    let s = seconds.max(0.0).round() as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helicoprion_core::{Freq, Scheduler, SystemInfo, WorkloadResult};

    fn reading(at: f64, temp: f64, mhz: f64, load: f64) -> Reading {
        Reading {
            at,
            loadavg_1m: Some(load),
            temp_c: Some(temp),
            freq_mhz: Some(Freq { min: mhz - 500.0, mean: mhz, max: mhz + 500.0 }),
        }
    }

    fn series(label: &str, runs: Vec<RunResult>) -> Series {
        Series {
            label: label.into(),
            slot: 0,
            file: ResultFile {
                schema: helicoprion_core::SCHEMA_VERSION,
                tool_version: "test".into(),
                label: label.into(),
                started_at: 0,
                system: SystemInfo::default(),
                scheduler: Scheduler::Fair { class: "EEVDF".into(), scx_capable: false, bore: None },
                restart_detectable: true,
                design: None,
                protocol: None,
                workloads: vec![WorkloadResult {
                    name: "w".into(),
                    description: String::new(),
                    params: serde_json::Value::Null,
                    runs,
                }],
            },
        }
    }

    fn run(round: Option<u32>, before: Option<Reading>, after: Option<Reading>) -> RunResult {
        RunResult { round, before, after, ..Default::default() }
    }

    /// A file of a `matrix` session with the given id.
    fn of_session(label: &str, id: &str, runs: Vec<RunResult>) -> Series {
        let mut s = series(label, runs);
        s.file.design = Some(helicoprion_core::Design {
            kind: "interleaved".into(),
            session_id: Some(id.into()),
            ..Default::default()
        });
        s
    }

    #[test]
    fn readings_are_laid_out_by_round_and_series() {
        let t = 1_800_000_000.0;
        let a = of_session(
            "a",
            "one",
            vec![
                run(Some(0), Some(reading(t, 50.0, 3000.0, 0.2)), Some(reading(t + 5.0, 70.0, 4400.0, 0.4))),
                run(Some(0), Some(reading(t + 6.0, 62.0, 3100.0, 0.5)), Some(reading(t + 11.0, 74.0, 4200.0, 0.9))),
                run(Some(1), Some(reading(t + 60.0, 66.0, 3000.0, 1.5)), Some(reading(t + 65.0, 81.0, 3900.0, 1.6))),
            ],
        );
        // The second scheduler was cut short: nothing in round 1.
        let b = of_session(
            "b",
            "one",
            vec![run(Some(0), Some(reading(t + 20.0, 60.0, 3000.0, 0.8)), Some(reading(t + 25.0, 77.0, 4300.0, 1.0)))],
        );
        let sessions = conditions(&[a, b]);
        assert_eq!(sessions.len(), 1);
        let c = &sessions[0];
        assert!(c.interleaved);
        assert_eq!((c.series.clone(), c.started_at), (vec![0, 1], t));
        assert_eq!(c.rows.iter().map(|r| r.round).collect::<Vec<_>>(), [Some(0), Some(1)]);
        let v = c.rows[0].visits[0].as_ref().unwrap();
        // Coolest before, hottest after; frequency after the runs only
        // (before a run the CPUs idle); the highest load.
        assert_eq!((v.start_s, v.end_s, v.runs), (0.0, 11.0, 2));
        assert_eq!(v.temp_c, Some((50.0, 74.0)));
        assert!((v.freq_ghz.unwrap() - 4.3).abs() < 1e-9);
        assert_eq!(v.load, Some(0.9));
        assert_eq!(c.rows[0].visits[1].as_ref().unwrap().start_s, 20.0);
        assert_eq!(c.rows[1].visits[1], None);
        assert_eq!(c.span_s, 65.0);
        assert_eq!(c.temp_after_c, Some((74.0, 81.0)));
        let (lo, hi) = c.freq_ghz.unwrap();
        assert!((lo - 3.9).abs() < 1e-9 && (hi - 4.3).abs() < 1e-9);
        assert_eq!(c.load_max, Some(1.6));
    }

    /// Two sessions both have a round 1. Their visits were hours apart
    /// and must not share a row, a clock or a summary; nor may files taken
    /// with `run` on different days read as one session.
    #[test]
    fn sessions_are_never_merged() {
        let day = 86_400.0;
        let t = 1_800_000_000.0;
        let one = |at: f64, temp: f64| {
            vec![
                run(Some(0), Some(reading(at, temp, 3000.0, 0.2)), Some(reading(at + 5.0, temp + 10.0, 4000.0, 0.3))),
                run(
                    Some(1),
                    Some(reading(at + 60.0, temp, 3000.0, 0.2)),
                    Some(reading(at + 65.0, temp + 12.0, 4000.0, 0.3)),
                ),
            ]
        };
        let files = [
            of_session("default", "monday", one(t, 50.0)),
            of_session("lavd", "monday", one(t + 20.0, 52.0)),
            // The same rounds, the same labels even, another session.
            of_session("default #2", "tuesday", one(t + day, 70.0)),
            of_session("lavd #2", "tuesday", one(t + day + 20.0, 72.0)),
        ];
        let sessions = conditions(&files);
        assert_eq!(sessions.iter().map(|s| s.series.clone()).collect::<Vec<_>>(), [vec![0, 1], vec![2, 3]]);
        for (s, start) in sessions.iter().zip([t, t + day]) {
            assert_eq!(s.started_at, start);
            // Its own clock: 85 s, not a day and 85 s.
            assert_eq!(s.span_s, 85.0);
            assert_eq!(s.rows.len(), 2);
            assert!(s.rows.iter().all(|r| r.visits.len() == 2 && r.visits.iter().all(Option::is_some)));
            assert_eq!(s.rows[1].visits[1].as_ref().unwrap().start_s, 80.0);
        }
        assert_eq!(sessions[0].temp_after_c, Some((60.0, 64.0)));
        assert_eq!(sessions[1].temp_after_c, Some((80.0, 84.0)));

        // The order of the files does not matter, only their session.
        let shuffled = [
            of_session("default", "monday", one(t, 50.0)),
            of_session("default #2", "tuesday", one(t + day, 70.0)),
            of_session("lavd", "monday", one(t + 20.0, 52.0)),
        ];
        let sessions = conditions(&shuffled);
        assert_eq!(sessions.iter().map(|s| s.series.clone()).collect::<Vec<_>>(), [vec![0, 2], vec![1]]);

        // Files written before the session id: start time and scheduler
        // list tell sessions apart, as for pairing.
        let legacy = |label: &str, started_at: u64, at: f64| {
            let mut s = of_session(label, "", one(at, 50.0));
            let d = s.file.design.as_mut().unwrap();
            d.session_id = None;
            d.schedulers = vec!["default".into(), "lavd".into()];
            s.file.started_at = started_at;
            s
        };
        let sessions = conditions(&[legacy("a", 100, t), legacy("b", 100, t + 20.0), legacy("c", 200, t + day)]);
        assert_eq!(sessions.iter().map(|s| s.series.clone()).collect::<Vec<_>>(), [vec![0, 1], vec![2]]);

        // Plain runs: every file is a session of its own, each with its
        // own start and length.
        let plain = |label: &str, at: f64| {
            series(
                label,
                vec![run(None, Some(reading(at, 50.0, 3000.0, 0.2)), Some(reading(at + 5.0, 60.0, 4000.0, 0.3)))],
            )
        };
        let sessions = conditions(&[plain("monday", t), plain("tuesday", t + day), plain("old", t)]);
        assert_eq!(sessions.len(), 3);
        assert!(sessions.iter().all(|s| !s.interleaved && s.series.len() == 1 && s.span_s == 5.0));
        assert_eq!(sessions[1].started_at, t + day);
        // And a matrix file next to a plain one are two sessions.
        let sessions = conditions(&[of_session("m", "monday", one(t, 50.0)), plain("p", t + 30.0)]);
        assert_eq!(
            sessions.iter().map(|s| (s.series.clone(), s.interleaved)).collect::<Vec<_>>(),
            [(vec![0], true), (vec![1], false)]
        );
    }

    #[test]
    fn missing_readings_are_not_made_up() {
        // Files from before readings were recorded.
        assert!(conditions(&[series("old", vec![run(None, None, None), run(None, None, None)])]).is_empty());
        // A machine without a temperature sensor or cpufreq: times and load.
        let bare = |at: f64| Reading { at, loadavg_1m: Some(0.3), temp_c: None, freq_mhz: None };
        let c = conditions(&[series("bare", vec![run(None, Some(bare(100.0)), Some(bare(105.0)))])]);
        let v = c[0].rows[0].visits[0].as_ref().unwrap();
        assert_eq!((c[0].rows[0].round, v.temp_c, v.freq_ghz, v.load), (None, None, None, Some(0.3)));
        assert_eq!((c[0].temp_after_c, c[0].freq_ghz), (None, None));
        // An old file next to a new one in one session: a cell without a
        // visit. As separate runs: only the new one is a session to show.
        let c = conditions(&[
            of_session("old", "s", vec![run(Some(0), None, None)]),
            of_session("new", "s", vec![run(Some(0), Some(bare(1.0)), Some(bare(2.0)))]),
        ]);
        assert!(c.len() == 1 && c[0].rows[0].visits[0].is_none() && c[0].rows[0].visits[1].is_some());
        let c = conditions(&[
            series("old", vec![run(None, None, None)]),
            series("new", vec![run(None, Some(bare(1.0)), Some(bare(2.0)))]),
        ]);
        assert_eq!(c.iter().map(|s| s.series.clone()).collect::<Vec<_>>(), [vec![1]]);
    }

    #[test]
    fn utc_dates() {
        assert_eq!(utc(0.0), "1970-01-01 00:00 UTC");
        assert_eq!(utc(951_782_400.0), "2000-02-29 00:00 UTC");
        assert_eq!(utc(1_794_873_600.0 + 19.0 * 3600.0 + 40.0 * 60.0 + 59.0), "2026-11-17 19:40 UTC");
        assert_eq!(utc(4_102_444_799.0), "2099-12-31 23:59 UTC");
    }

    #[test]
    fn clock_format() {
        assert_eq!(clock(0.0), "0:00");
        assert_eq!(clock(65.4), "1:05");
        assert_eq!(clock(7387.0), "2:03:07");
    }
}
