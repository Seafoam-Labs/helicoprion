//! A/A pairs: two arms of one `matrix` session that are the same
//! scheduler with the same options (`Design::twin_of`).
//!
//! Between such arms nothing differs but when each ran. A change the
//! report scores as clear there is a false alarm by construction, and
//! the size of the changes it finds is the noise of the method on this
//! machine, per metric: what `--min-effect` and `--unstable-spread` have
//! to be judged against, rather than guessed.
//!
//! This reports what an A/A pair shows. It changes no threshold.

use helicoprion_core::{Better, ResultFile};

use crate::compare::{Comparison, Delta, UnclearReason, session_key};
use crate::stats::median;

/// Two series of a comparison that are one scheduler, measured twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pair {
    /// Index of the arm that came first in the scheduler list.
    pub first: usize,
    /// Index of the arm that repeats it.
    pub twin: usize,
}

/// One scored metric of an A/A pair.
#[derive(Debug, Clone, PartialEq)]
pub struct Noise {
    pub workload: String,
    pub metric: String,
    /// The twin against the first arm, estimated and scored exactly as
    /// any candidate against a baseline.
    pub delta: Delta,
    /// Why the change is not scored; `None` means it is scored as clear,
    /// between two arms of the same scheduler: a false alarm.
    pub unclear: Option<UnclearReason>,
    /// The arm visited second in a round against the arm visited first,
    /// median over the rounds, in percent (positive: the later visit is
    /// better). The arms swap places from round to round, so this is the
    /// effect of position within a round, which the twin-against-first
    /// change averages out. `None` without two rounds to take it from.
    pub order: Option<Order>,
    /// Paired rounds in which the first arm was visited before its twin,
    /// and in which after it. Equal numbers balance an effect of
    /// position; otherwise it is part of `delta`.
    pub led: (usize, usize),
}

/// The effect of being visited later in a round.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Order {
    /// Median change of the later visit against the earlier, percent.
    pub median: f64,
    /// Rounds it was taken over, and in how many the later visit was
    /// better.
    pub rounds: usize,
    pub later_better: usize,
}

impl Noise {
    pub fn false_alarm(&self) -> bool {
        self.unclear.is_none()
    }

    /// Each arm came first in a round as often as the other.
    pub fn balanced(&self) -> bool {
        self.led.0 == self.led.1
    }
}

/// A `--min-effect` to name as one that lies above `change`: rounded up
/// at the digit shown, so that it does lie above.
pub fn above(change: f64) -> String {
    format!("{:.1}", (change * 10.0).ceil() / 10.0)
}

/// What one A/A pair shows.
#[derive(Debug, Clone)]
pub struct Calibration {
    pub pair: Pair,
    /// Labels of the two arms.
    pub first: String,
    pub twin: String,
    /// Every scored metric both arms have, in the report's order.
    pub metrics: Vec<Noise>,
}

impl Calibration {
    pub fn false_alarms(&self) -> impl Iterator<Item = &Noise> {
        self.metrics.iter().filter(|m| m.false_alarm())
    }

    /// The largest change among the false alarms, in percent: a
    /// `--min-effect` above it would have scored none of them. `None`
    /// when there are no false alarms.
    pub fn silencing_min_effect(&self) -> Option<f64> {
        self.false_alarms().map(|m| m.delta.median.abs()).reduce(f64::max)
    }

    /// The largest change the pair shows in any scored metric, scored or
    /// not, in percent.
    pub fn largest(&self) -> Option<&Noise> {
        self.metrics.iter().max_by(|a, b| a.delta.median.abs().total_cmp(&b.delta.median.abs()))
    }

    /// How often the first arm was visited before its twin and after it,
    /// when the two numbers differ for some metric: the session did not
    /// balance an effect of position (an odd number of rounds, or a
    /// session cut short).
    pub fn unbalanced(&self) -> Option<(usize, usize)> {
        self.metrics.iter().find(|m| !m.balanced()).map(|m| m.led)
    }

    /// For the report's warnings: that the pair is unbalanced.
    pub fn balance_warning(&self) -> Option<String> {
        let (before, after) = self.unbalanced()?;
        Some(format!(
            "A/A: {} was visited before {} in {before} rounds and after it in {after}. The order is not balanced, \
             so an effect of position within a round is part of the change between the two arms; run an even \
             number of rounds",
            self.first, self.twin
        ))
    }

    /// For the report's warnings: the false alarms of this pair, if any.
    pub fn warning(&self) -> Option<String> {
        let mut alarms: Vec<&Noise> = self.false_alarms().collect();
        let silencing = self.silencing_min_effect()?;
        alarms.sort_by(|a, b| b.delta.median.abs().total_cmp(&a.delta.median.abs()));
        let named: Vec<String> = alarms
            .iter()
            .take(3)
            .map(|m| format!("{} · {} {}", m.workload, m.metric, crate::svg::fmt_delta(&m.delta)))
            .collect();
        Some(format!(
            "A/A: {} is {} again, measured as a separate arm, and {} of {} scored metrics differ clearly between the \
             two ({}{}). That is not the scheduler: the method can produce a change of this size without one. Read \
             changes of this size between other schedulers of this session with caution and repeat them in \
             another session; a --min-effect above {}% would have scored none of these",
            self.twin,
            self.first,
            alarms.len(),
            self.metrics.len(),
            named.join(", "),
            if alarms.len() > named.len() { ", …" } else { "" },
            above(silencing)
        ))
    }
}

/// The A/A pairs among the series of `c`: a series whose file names
/// another series of the same session as its twin.
pub fn pairs(c: &Comparison) -> Vec<Pair> {
    let spec = |f: &ResultFile| f.design.as_ref().map(|d| d.spec.clone());
    c.series
        .iter()
        .enumerate()
        .filter_map(|(twin, s)| {
            let of = s.file.design.as_ref()?.twin_of.as_ref()?;
            let key = session_key(&s.file)?;
            let first = c
                .series
                .iter()
                .position(|o| session_key(&o.file).as_ref() == Some(&key) && spec(&o.file).as_ref() == Some(of))?;
            (first != twin).then_some(Pair { first, twin })
        })
        .collect()
}

/// The position of each round's visit of `f` on `workload` in the
/// session: the lowest visit number among that round's runs.
fn visit_of(f: &ResultFile, workload: &str, round: u32) -> Option<u32> {
    f.workloads
        .iter()
        .find(|w| w.name == workload)?
        .runs
        .iter()
        .filter(|r| r.round == Some(round))
        .filter_map(|r| r.visit)
        .min()
}

fn change(better: Better, base: f64, v: f64) -> Option<f64> {
    if base == 0.0 {
        return None;
    }
    match better {
        Better::Lower => Some((base - v) / base * 100.0),
        Better::Higher => Some((v - base) / base * 100.0),
        Better::Neither => None,
    }
}

/// Analyse every A/A pair of `c`. Each pair is compared on its own, the
/// first arm as the baseline, with the options of `c`: the scoring a
/// reader would get from `compare first.json twin.json`.
pub fn calibrate(c: &Comparison) -> Vec<Calibration> {
    pairs(c)
        .into_iter()
        .map(|pair| {
            let (a, b) = (&c.series[pair.first], &c.series[pair.twin]);
            let two = Comparison::build_core(vec![a.file.clone(), b.file.clone()], 0, c.opts);
            let metrics = two
                .rows
                .iter()
                .filter(|r| !r.extreme && r.better != Better::Neither)
                .filter_map(|r| {
                    let delta = r.improvement(0, 1)?;
                    let (ca, cb) = (r.cells[0].as_ref()?, r.cells[1].as_ref()?);
                    // Later visit against earlier, round by round, and
                    // which arm led how often.
                    let mut led = (0, 0);
                    let per_round: Vec<f64> = ca
                        .rounds
                        .iter()
                        .filter_map(|(round, va)| {
                            let vb = cb.rounds.get(round)?;
                            let (ta, tb) =
                                (visit_of(&a.file, &r.workload, *round)?, visit_of(&b.file, &r.workload, *round)?);
                            if ta < tb {
                                led.0 += 1;
                                change(r.better, *va, *vb)
                            } else {
                                led.1 += 1;
                                change(r.better, *vb, *va)
                            }
                        })
                        .collect();
                    let order = (per_round.len() >= 2)
                        .then(|| {
                            Some(Order {
                                median: median(&per_round)?,
                                rounds: per_round.len(),
                                later_better: per_round.iter().filter(|x| **x > 0.0).count(),
                            })
                        })
                        .flatten();
                    Some(Noise {
                        workload: r.workload.clone(),
                        metric: r.metric.clone(),
                        unclear: two.unclear_reason(r, 1, &delta),
                        delta,
                        order,
                        led,
                    })
                })
                .collect();
            Calibration { pair, first: a.label.clone(), twin: b.label.clone(), metrics }
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::compare::Options;
    use crate::sessions::tests::session;

    /// A session as `matrix` writes it for `scheds`, where an entry that
    /// repeats an earlier one is its twin (`default`, `default #2`). The
    /// order of visits alternates from round to round, and `value(label,
    /// round, position in the round)` is the latency measured.
    pub(crate) fn aa_session(
        id: &str,
        started_at: u64,
        scheds: &[&str],
        rounds: u32,
        value: impl Fn(&str, u32, usize) -> f64,
    ) -> Vec<(String, ResultFile)> {
        let n = scheds.len();
        let position = |i: usize, round: u32| if round.is_multiple_of(2) { i } else { n - 1 - i };
        let index = |label: &str| scheds.iter().position(|s| *s == label).unwrap();
        let mut files =
            session(id, started_at, scheds, rounds, |label, round| value(label, round, position(index(label), round)));
        for (i, (_, f)) in files.iter_mut().enumerate() {
            let label = scheds[i];
            if let Some((first, _)) = label.split_once(" #") {
                f.design.as_mut().unwrap().twin_of = Some(first.to_string());
                // The twin is the scheduler of its first arm.
                f.scheduler =
                    helicoprion_core::Scheduler::Fair { class: "EEVDF".into(), scx_capable: true, bore: None };
                f.design.as_mut().unwrap().options = None;
            }
            for r in &mut f.workloads[0].runs {
                let round = r.round.unwrap();
                r.visit = Some(round * n as u32 + position(i, round) as u32);
            }
        }
        files
    }

    fn silencing_of(cal: &Calibration) -> f64 {
        cal.silencing_min_effect().unwrap()
    }

    fn only(files: Vec<(String, ResultFile)>) -> Vec<ResultFile> {
        files.into_iter().map(|(_, f)| f).collect()
    }

    fn calibration(value: impl Fn(&str, u32, usize) -> f64) -> Calibration {
        let files = only(aa_session("aa", 1000, &["default", "default #2"], 6, value));
        let c = Comparison::build_with(files, 0, Options::default());
        let mut all = calibrate(&c);
        assert_eq!(all.len(), 1);
        all.remove(0)
    }

    #[test]
    fn twins_are_found_within_their_session() {
        let scheds = ["default", "scx_lavd", "default #2", "scx_lavd #2"];
        let mut files = only(aa_session("one", 1000, &scheds, 3, |_, _, _| 100.0));
        // In a real session the twin of a BPF scheduler is that scheduler.
        files[3].scheduler = files[1].scheduler.clone();
        let c = Comparison::build(files.clone(), 0);
        assert_eq!(pairs(&c), [Pair { first: 0, twin: 2 }, Pair { first: 1, twin: 3 }]);
        let cal = calibrate(&c);
        assert_eq!((cal[1].first.as_str(), cal[1].twin.as_str()), ("scx_lavd", "scx_lavd #2"));

        // The order of the files does not matter.
        let mut shuffled = files.clone();
        shuffled.swap(0, 2);
        assert_eq!(pairs(&Comparison::build(shuffled, 0)), [Pair { first: 2, twin: 0 }, Pair { first: 1, twin: 3 }]);

        // A twin without its first arm in the comparison is no pair, and
        // neither is an arm of the same name from another session.
        assert!(pairs(&Comparison::build(vec![files[1].clone(), files[2].clone()], 0)).is_empty());
        let mut other = only(aa_session("two", 2000, &scheds, 3, |_, _, _| 100.0));
        let c = Comparison::build(vec![other.remove(0), files[2].clone()], 0);
        assert!(pairs(&c).is_empty());
        // Sessions without repeated arms have none.
        let plain = only(session("p", 1000, &["default", "scx_lavd"], 3, |_, _| 100.0));
        assert!(calibrate(&Comparison::build(plain, 0)).is_empty());
    }

    #[test]
    fn two_arms_that_measure_the_same_raise_no_alarm() {
        // Rounds drift by up to 30%; within a round the arms agree.
        let cal = calibration(|_, round, _| 100.0 * (1.0 + 0.06 * f64::from(round)));
        assert_eq!(cal.metrics.len(), 2, "p99 and ops");
        assert_eq!(cal.false_alarms().count(), 0);
        assert_eq!((cal.silencing_min_effect(), cal.warning()), (None, None));
        assert!(cal.largest().unwrap().delta.median.abs() < 0.5);
        for m in &cal.metrics {
            let o = m.order.unwrap();
            assert!(o.rounds == 6 && o.median.abs() < 0.5, "{o:?}");
        }
    }

    /// Whichever arm is visited second in a round is 10% slower: the
    /// machine, not the scheduler. The arms swap places every round, so
    /// the twin is slower in one round and faster in the next: no clear
    /// change between the arms, and an order effect of 10% in every round.
    #[test]
    fn an_effect_of_position_is_told_from_a_difference_between_arms() {
        let cal = calibration(|_, _, position| if position == 0 { 100.0 } else { 110.0 });
        assert_eq!(cal.false_alarms().count(), 0, "{:?}", cal.metrics);
        let p99 = cal.metrics.iter().find(|m| m.metric == "p99").unwrap();
        let o = p99.order.unwrap();
        // A latency 10% longer is a change of −10%.
        assert!((o.median + 10.0).abs() < 0.5, "{o:?}");
        assert_eq!((o.rounds, o.later_better), (6, 0));
        // The rate moves the other way by the same factor: also worse.
        let ops = cal.metrics.iter().find(|m| m.metric == "ops").unwrap().order.unwrap();
        assert!(ops.median < -8.0 && ops.later_better == 0, "{ops:?}");
    }

    /// Five rounds do not balance the order: the first arm leads three
    /// times. With a later visit that is 10% slower, two identical arms
    /// then differ by 10%; over six rounds they do not. The scoring is the
    /// same in both; the report says which session is unbalanced.
    #[test]
    fn an_odd_number_of_rounds_leaves_the_position_effect_in_the_change() {
        let position = |_: &str, _: u32, position: usize| if position == 0 { 100.0 } else { 110.0 };
        let of = |rounds: u32| {
            let files = only(aa_session("aa", 1000, &["default", "default #2"], rounds, position));
            let c = Comparison::build(files, 0);
            (calibrate(&c).remove(0), c.warnings)
        };

        let (five, warnings) = of(5);
        let p99 = five.metrics.iter().find(|m| m.metric == "p99").unwrap();
        assert_eq!(p99.led, (3, 2));
        assert!(!p99.balanced());
        // −10, +9.1, −10, +9.1, −10: the median is the position effect.
        assert!((p99.delta.median + 10.0).abs() < 0.5, "{:?}", p99.delta);
        assert_eq!(five.unbalanced(), Some((3, 2)));
        assert_eq!(
            warnings,
            [
                "A/A: default was visited before default #2 in 3 rounds and after it in 2. The order is not balanced, so \
              an effect of position within a round is part of the change between the two arms; run an even number \
              of rounds"
            ]
        );
        let html = crate::html::report(&Comparison::build(
            only(aa_session("aa", 1000, &["default", "default #2"], 5, position)),
            0,
        ));
        assert!(html.contains(
            "<strong>Not balanced</strong>: default was visited before default #2 in 3 rounds and after it in 2"
        ));

        let (six, warnings) = of(6);
        let p99 = six.metrics.iter().find(|m| m.metric == "p99").unwrap();
        assert_eq!((p99.led, p99.balanced()), ((3, 3), true));
        assert!(p99.delta.median.abs() < 1.0, "{:?}", p99.delta);
        assert_eq!((six.unbalanced(), warnings.len()), (None, 0));
        // The position effect itself reads the same either way.
        for cal in [&five, &six] {
            let o = cal.metrics.iter().find(|m| m.metric == "p99").unwrap().order.unwrap();
            assert!((o.median + 10.0).abs() < 0.5 && o.later_better == 0, "{o:?}");
        }
    }

    /// The twin is 8% slower in every round, wherever it stands in the
    /// round: scored as a clear change, between two arms of one
    /// scheduler. A false alarm, with the threshold that would silence it.
    #[test]
    fn a_clear_change_between_twins_is_a_false_alarm() {
        let cal = calibration(|label, _, _| if label == "default" { 100.0 } else { 108.0 });
        assert_eq!(cal.false_alarms().count(), 2);
        let w = cal.warning().unwrap();
        assert!(w.starts_with(
            "A/A: default #2 is default again, measured as a separate arm, and 2 of 2 scored metrics differ clearly \
             between the two (w · p99 \u{2212}8.0%, w · ops \u{2212}7.4%). That is not the scheduler: the method can \
             produce a change of this size without one."
        ), "{w}");
        assert!(w.contains("with caution and repeat them in another session"), "{w}");
        // The threshold named does lie above every false alarm.
        let named: f64 = w.rsplit("above ").next().unwrap().split('%').next().unwrap().parse().unwrap();
        assert!(named >= silencing_of(&cal) && named - silencing_of(&cal) < 0.1, "{w}");
        // The comparison carries it among its warnings.
        let files =
            only(aa_session(
                "aa",
                1000,
                &["default", "default #2"],
                6,
                |label, _, _| {
                    if label == "default" { 100.0 } else { 108.0 }
                },
            ));
        assert_eq!(Comparison::build(files, 0).warnings, [w]);
        let p99 = cal.metrics.iter().find(|m| m.metric == "p99").unwrap();
        assert!(p99.false_alarm() && p99.unclear.is_none());
        assert!((p99.delta.median + 8.0).abs() < 0.5 && p99.delta.paired_rounds == Some(6));
        let silencing = cal.silencing_min_effect().unwrap();
        assert!((7.0..8.5).contains(&silencing), "{silencing}");
        assert_eq!(cal.largest().unwrap().metric, "p99");
        // With that threshold the same data raises none.
        let files =
            only(aa_session(
                "aa",
                1000,
                &["default", "default #2"],
                6,
                |label, _, _| {
                    if label == "default" { 100.0 } else { 108.0 }
                },
            ));
        let opts = Options { min_effect_pct: silencing + 0.1, ..Options::default() };
        let cal = &calibrate(&Comparison::build_with(files, 0, opts))[0];
        assert_eq!(cal.false_alarms().count(), 0);
        assert!(cal.metrics.iter().all(|m| m.unclear == Some(UnclearReason::BelowThreshold)));
        // No position effect: it does not matter which arm went first.
        assert_eq!(cal.metrics[0].order.unwrap().later_better, 3);
    }

    /// A pair that is not the comparison's baseline is still scored
    /// against its own first arm, not against the baseline.
    #[test]
    fn a_pair_is_scored_against_its_own_first_arm() {
        let scheds = ["default", "scx_lavd", "scx_lavd #2"];
        let mut files = only(aa_session("s", 1000, &scheds, 6, |label, _, _| match label {
            "default" => 100.0,
            "scx_lavd" => 200.0,
            _ => 230.0,
        }));
        files[2].scheduler = files[1].scheduler.clone();
        let cal = &calibrate(&Comparison::build(files, 0))[0];
        assert_eq!((cal.pair, cal.first.as_str()), (Pair { first: 1, twin: 2 }, "scx_lavd"));
        // 230 against 200, not against 100.
        let p99 = cal.metrics.iter().find(|m| m.metric == "p99").unwrap();
        assert!((p99.delta.median + 15.0).abs() < 0.5, "{:?}", p99.delta);
        assert!(p99.false_alarm());
    }

    /// The report shows the pair, and the plain words say that the
    /// "candidate" is the baseline again.
    #[test]
    fn the_report_shows_what_an_aa_pair_found() {
        let build = |value: fn(&str, u32, usize) -> f64| {
            Comparison::build(only(aa_session("aa", 1000, &["default", "default #2"], 6, value)), 0)
        };
        // A false alarm.
        let c = build(|label, _, _| if label == "default" { 100.0 } else { 108.0 });
        let html = crate::html::report(&c);
        assert!(html.contains("<h2>A/A check</h2>"));
        assert!(html.contains(
            "<strong>default #2 is default again</strong>: 2 of 2 scored metrics differ clearly between the two, which \
             is not the scheduler; a --min-effect above 8.0% would have scored none. Largest change: w · p99 \u{2212}8.0%."
        ));
        assert_eq!(html.matches("<span class=bad>clear: a false alarm</span>").count(), 2);
        assert!(html.contains("⚠ A/A: default #2 is default again"));
        let text = crate::simple::text(&c);
        assert!(
            text.contains(
                "Note: This is default again, measured as a separate arm: a difference here is not the scheduler"
            ),
            "{text}"
        );

        // Position only: nothing scored, and the order effect in its column.
        let c = build(|_, _, position| if position == 0 { 100.0 } else { 110.0 });
        let html = crate::html::report(&c);
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        assert!(html.contains("<strong>default #2 is default again</strong>: 0 of 2 scored metrics differ clearly between the two. Largest change:"));
        assert!(!html.contains("a false alarm</span>"));
        assert!(html.contains("\u{2212}10.0% <span class=m>(better in 0 of 6 rounds)</span>"), "order effect");

        // No repeated arm, no section and no note.
        let c = Comparison::build(only(session("p", 1000, &["default", "scx_lavd"], 3, |_, _| 100.0)), 0);
        assert!(!crate::html::report(&c).contains("A/A check"));
        assert!(!crate::simple::text(&c).contains("again"));
    }
}
