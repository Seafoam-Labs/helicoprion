# helicoprion

A small, scheduler-focused CPU benchmark for Linux. It tells you which
scheduler is in charge (EEVDF, EEVDF+BORE, or a sched-ext BPF scheduler),
runs a series of self-contained workloads, and turns several result files
into a comparison report with charts.

Everything is built in: no external benchmark programs, no downloaded
assets. `cargo build --release` gives a single, dynamically linked (glibc)
binary; for a static one see [Static build](#static-build).

## Quick start

```sh
cargo build --release
B=target/release/helicoprion

$B info                                   # which scheduler is running right now
$B run -o eevdf.json                      # on EEVDF
scx_lavd &                                # attach a sched-ext scheduler
$B run -o lavd.json                       # label defaults to "scx_lavd"
$B compare eevdf.json lavd.json -o report.html --svg-dir charts/
```

`compare` prints a table to the terminal and writes a self-contained HTML
report (light/dark aware, no JavaScript). With `--svg-dir` each chart is
also saved as a standalone SVG.

For comparing schedulers, prefer `matrix` (below): separate `run` sessions
differ by a few percent on their own, which the per-session statistics
cannot see.

## Interleaved sessions with scxcli (`matrix`)

```sh
sudo helicoprion matrix --sched default --sched scx_lavd --sched scx_cosmos \
    --sched "scx_bpfland -s 5000" --rounds 5 -o lavd-vs-cosmos/
```

Not sure which schedulers to compare? Let scxcli choose:

```sh
sudo helicoprion matrix --recommended
```

This takes every scheduler `scxcli list recommended` offers that is
installed, with the kernel's own scheduler as the baseline, and runs the
default 5 rounds (about 11 minutes for five schedulers). The list comes from
the scx catalog, so a distribution or an admin decides what is in it.
`--recommended` cannot be combined with `--sched`; all other options apply.

Add `--simple` for a result in plain words (also for `compare`):

```
In short, compared with scx_pandemonium:

scx_cake
  Idle system: WORSE
    • Round trips completed: no reliable difference; the interval includes no change (1.25M → 1.26M per second)
    • Typical wake-up delay: no reliable difference; the interval includes no change (6.14 → 6.11 µs)
    • Slowest wake-ups (1 in 1000): 75% longer (11.5 → 20.1 µs)
    • Of 11 measurements, 0 clearly better and 2 clearly worse
  Busy system (every CPU kept busy): BETTER
    • Round trips completed: 2.5× more (216.5k → 542.4k per second)
    • Typical wake-up delay: 2.6× shorter (36.1 → 14.0 µs)
    • Slowest wake-ups (1 in 1000): 29× shorter (1335 → 45.6 µs)
    • Background CPU work: 26% lower (observed, not scored)
    • Of 12 measurements, 11 clearly better and 0 clearly worse
```

The HTML report always opens with this "In short" section, one card per
scheduler; with `--simple`
the charts, tables and reading notes are folded under "Full report". The
scoring is the same as in the full report: a line says "more" or "longer"
only where the table colours the change, and "no reliable difference"
otherwise, however far apart the two numbers look, with the one reason the
scoring went by: the interval includes no change, the difference is below
the reporting threshold, too few paired rounds (or runs), too few relevant
wake-ups (a minor population), results too unstable to compare, a baseline
of zero in some rounds. A change that fails several tests names the first
that applies, in a fixed order; a verdict of "no clear
difference" means the run could not tell the two apart, not that they are
equal. Where the estimate of a scored change is far above the cautious end of
its interval, the line adds that end, cut off rather than rounded: "54×
shorter, at least 1.49×". That is an end of an estimated 95% interval, not a
guaranteed minimum, and the report says so wherever it uses the phrase. Where a change is held back only because its size differed
from round to round, the line still gives the direction, which the interval
supports, and says why it is not scored: "fewer; the effect varied too much
between rounds to score (estimated effect, 95% interval: 2.0× to 2.9×)".
The interval is the uncertainty of the estimate, not the range of the
rounds. The verdict and the tally leave such a change out, as the full
report does. Background CPU work is informational and marked "observed, not
scored". Where the two schedulers produced a different mix of round trips
(the full report's warning), the part it concerns says so under its points:
"Note: Only 11% of round trips needed a wake-up, versus 99% for default.
The wake-up delays here do not describe the same mix of round trips." What
follows is said about the numbers shown, which are the delays of all round
trips that needed a wake-up: if only the share of round trips without a
wake-up differs, the note asks to read throughput and delays together; if
only the split between one CPU and two differs, it says that the delays
shown cover both kinds, in different proportions. A part gets one note:
about round trips that needed a wake-up whenever their share differs,
otherwise about the kind whose share of all round trips moved the most. It
names a difference in behaviour, not the cause of a delay. If the full report has warnings, or the results
were not all taken on the same setup (kernel, CPUs, governor), `--simple`
says so above the folded part and in the terminal. So the whole benchmark
for someone who just wants an answer is
`sudo helicoprion matrix --recommended --simple`.

Every round visits each scheduler once, in alternating order (A B C, C B A,
A B C, …), and runs every workload during each visit, so slow drift
(thermal state, background activity) hits every scheduler alike. Switching
goes through [scxcli](https://github.com/sched-ext/scx/tree/main/tools/scxcli)
and scx.service:

* `--sched` takes what scxcli takes: `default` (the kernel's scheduler,
  `scxcli start default`), a scheduler or catalog profile id
  (`scxcli start ID`), or a scheduler with options in one argument
  (`scxcli run SCHED ARGS…`). The first one is the report's baseline.
  Profile ids are looked up in `scxcli list --json` up front, so the
  scheduler and options they stand for can be checked after the switch.
* `scxcli start`/`run` return once systemd restarted the unit, not once the
  scheduler is attached. A switch has settled only when `scxcli get --json`
  reports the expected process (and options, for profiles and `run`) with no
  failure, the kernel shows a BPF scheduler whose ops name matches it
  (`scx_lavd` attaches as `lavd`; `--allow-ops-mismatch` for a scheduler
  that names itself differently), and that state, `enable_seq` included,
  holds for `--settle` seconds (default 3). A failure or a switch that does
  not settle within `--switch-timeout` ends the session.
* Every scxcli/systemctl call runs in its own process group (a terminal
  Ctrl-C reaches only helicoprion) under one deadline,
  `--command-timeout` seconds (default 20), for the process to exit *and*
  its output pipes to close; past it the process group is killed. That
  also covers a call that exits but leaves a child holding its output open
  (a child that left the group with setsid is abandoned, not waited for).
  The switch deadline covers these calls too, so a hung scxcli cannot stall
  the session.
* The state from before the session is restored at the end, after an error
  and after Ctrl-C: the selection (scheduler and options), whether
  scx.service was active, and whether it was enabled (scxcli enables the unit
  when it starts a scheduler, so helicoprion runs `systemctl enable|disable
  scx.service` to put that back). The result is verified twice: the
  configuration against a fresh `scxcli get --json`, and the running state,
  i.e. the previous scheduler (with its options) running and attached with a
  matching ops name, or nothing attached, with no failure. The commands that
  would restore it by hand are printed at the start. A failed restore is
  reported before anything else, so a later error (writing the report)
  cannot hide it.
* `matrix` refuses to start in states it could not restore exactly: an
  `SCX_SCHEDULER_OVERRIDE`/`SCX_FLAGS_OVERRIDE` (scxcli clears it), a
  `systemctl enable --runtime` enablement, an active unit with no
  configured scheduler, an environment scxcli could not fully resolve
  (`config_error`), or a unit and kernel that disagree (`scheduler: null`).
* Ctrl-C: the first press stops before the next run (or switch) and
  restores; a second press terminates at once, without restoring.
* Run as root: as a user, scxcli asks polkit at every switch.
* Output goes to a new directory (`--output`, default
  `helicoprion-matrix-<time>`; an existing non-empty one is refused). Each
  scheduler gets `NN-<spec>.json`, rewritten atomically after every visit,
  so a crash loses at most the visit in progress; `report.html` is written
  at the end. `design` records the session and each run its `visit` and
  `round`. `compare` works on these files as on any others.

## Scheduler detection

| Condition | Reported as |
|---|---|
| no `/sys/kernel/sched_ext` | `EEVDF` (`CFS` before 6.6)*, "no sched_ext" |
| `/sys/kernel/sched_ext/state` ≠ `enabled` | `EEVDF`, "sched_ext available, inactive" |
| `state` = `enabled`, `root/ops` set | `scx_<name>` with the raw ops string and `enable_seq` |
| `/proc/sys/kernel/sched_bore` = 1 | `EEVDF+BORE` |

\* For the fair class the name is only *inferred* from the kernel version;
a downstream patch set can make it wrong. BORE is detected positively.

The scheduler seen at `run` start is recorded in the result file, and
**every** run must see exactly that scheduler both immediately before and
after it. Otherwise the run is marked invalid and excluded from statistics:
a different scheduler attached between runs, a crash/restart during a run
(ops or `enable_seq` changed), or a manual swap. Without `enable_seq`
(older sched_ext kernels) a restart of the *same* scheduler between two
readings is undetectable; the file records `restart_detectable: false`
and both `run` and `compare` warn about it.

## Workloads (`--suite quick`)

| Name | What it does | Metrics |
|---|---|---|
| `wake-pingpong` | N/2 thread pairs pass a token through a private futex | per population: p50/p99/p99.9/max (µs); round trips/s; threads actually woken/s (informational) |
| `wake-pingpong/loaded` | the same, while N CPU-hog threads keep every CPU busy | as above + hog work/s |

### Two populations: `rtt_blocked` and `rtt_fast`

A hand-off is a real sleep + wakeup only when the receiver was already in
`FUTEX_WAIT`. Under load many are not, and mixing both into one
distribution makes fast hand-offs look like excellent wakeup latency. Every
round trip is therefore classified and recorded in its own histogram:

* `rtt_blocked`: the pinger slept in `FUTEX_WAIT` at least once (wakeup latency);
* `rtt_fast`: the reply arrived before the pinger blocked (not wakeup latency).

A round trip in which a wait was interrupted by a signal (EINTR) without
any completed sleep could belong to either population; it is counted as
`roundtrips_ambiguous` and kept out of both histograms.

Diagnostics show the share of each, `roundtrips_two_sided` (the pinger slept
*and* its `FUTEX_WAKE` woke a sleeping ponger), waits that really slept vs
returned `EAGAIN`, and threads `FUTEX_WAKE` actually woke. A quantile is
reported only with at least ten samples beyond it (p50 ≥ 20, p99 ≥ 1000,
p99.9 ≥ 10000), so a small population never produces a fake tail.

On an idle 2-CPU VM ~97% of round trips were blocked; under load only
~45–65%, with ~20% two-sided.

### Same CPU or cross CPU

The ponger reads its CPU (`sched_getcpu`) just before handing the token
back; the pinger reads its own once it has the token again. *Same CPU*
means those two readings name the same logical CPU, *cross CPU* that they
differ. They are two observations at two instants, not a trace of the
wakeup: either thread may move in between, so neither label proves a local
wakeup or an IPI. What they do well is tell regimes apart, such as a pair
sharing one CPU versus spread out. Blocked round trips get
`rtt_blocked_same_cpu_*` and `rtt_blocked_cross_cpu_*` metrics (p50/p99;
full histograms in the result, no extra charts); diagnostics show the
same-CPU share of blocked and fast round trips and `ping_cpu_changes`, how
often the pinger was seen on a different CPU than after the previous round
trip (observed changes between samples, not a migration count). SMT
siblings and separate cores are not told apart yet.

On a 2-vCPU VM, idle round trips with equal readings had p50 ≈ 1.9 µs vs
≈ 20 µs with different ones, and under load whole runs flipped between
~100% different (p50 ≈ 28 µs) and 80–90% equal (p50 ≈ 2 µs).

Cost: one `sched_getcpu` (≈3 ns, vDSO) inside each timed round trip, on the
ponger's side; `reply_cpu` shares the token's cache line, so no extra cache
line moves between CPUs. An interleaved A/A on that VM (8 sessions with,
8 without) showed no difference beyond its noise (±4% idle; loaded runs are
bimodal there), which cannot rule out effects of a few percent on fast
hand-offs of ~0.5 µs. `--no-cpu-diag` (for `run` and `matrix`) turns it
off; the workload parameters then match results recorded before the option
existed, and differ from runs with diagnostics, so the two are never
compared with each other.

### Timing and failure handling

* Every worker acknowledges readiness; only then is one start instant fixed
  and the gate opened, so all pingers share the same start and deadline.
  Workers that are not ready within 30 s abort the run.
* Hog work is snapshotted immediately before the start instant is fixed and
  again as soon as the main thread wakes at the deadline, and the rate uses
  exactly that interval. The real boundaries of both the latency and the hog
  window are stored per run in `windows` (seconds relative to the common
  start), so any misalignment is visible rather than assumed away.
* Any worker failure (unexpected futex error, or a panic caught at the
  worker boundary) triggers a global abort: every token is set to ABORT and
  every waiter on the gate, the ready counter and the tokens is woken. The
  run ends promptly and is invalid. Fault-injection tests cover a ponger
  error, a ponger panic and a pinger panic during a 60 s run.
* Hogs and ping/pong threads are owned by RAII guards; spawn errors and
  early returns stop and join everything already started.

## Methodology

* `--runs` measured runs (default 5) after `--warmup` discarded runs (default 1).
* Reports use the **median** across valid runs and the interquartile range.
* Change vs baseline is normalised so **positive = better** for every metric.
  Its interval is a 95% percentile bootstrap that resamples the runs of
  *both* the baseline and the candidate (needs ≥3 valid runs per side).
  A change is highlighted only when that interval excludes zero **and** its
  magnitude reaches `--min-effect` (default 5%); with a handful of runs this
  is a rough guide, not a significance test. The bootstrap only sees
  run-to-run spread *within* a session: an A/A test (EEVDF twice on the
  same machine) showed whole sessions shifting by ~3%, which starred seven
  metrics without the threshold. Calibrate `--min-effect` with an A/A run on
  your hardware, or use `matrix`. There is no multiple-comparison
  correction.
* **Paired analysis for `matrix`.** Files from one interleaved session (same
  random `design.session_id`; files written before that field existed fall
  back to start time and scheduler list) are compared round by round: the change is
  computed within each round, where both schedulers ran under the same
  conditions, the estimate is the median of those per-round changes, and
  the interval is a bootstrap over whole rounds. Runs of one session are
  never treated as independent samples (with `--rounds 2 --runs-per-visit
  5` there are ten runs but only two independent rounds): with one or two
  shared rounds the report gives the paired point estimate without an
  interval, and with none the change of the medians, labelled as a
  descriptive comparison (not paired) without an interval, plus a warning;
  neither is ever highlighted. Use at least 3 rounds. Drift between rounds then cancels instead of
  widening the interval; with simulated ±60% drift and a constant 10%
  difference, the paired interval is about [+8%, +12%] while the unpaired
  one on the same numbers straddles zero. Stability of a paired comparison
  is judged by how much the per-round changes disagree, not by the raw
  run-to-run spread, which in a `matrix` session is mostly that drift.
* Cells whose run-to-run spread (± half the relative IQR) exceeds
  `--unstable-spread` (default 10%) are marked unstable: shown in the table,
  never highlighted, kept off the change chart. Typical case: p99.9 under
  load sitting on the edge of a ~1.6 ms plateau that holds ~0.1% of the
  samples, flipping between 40 µs and 1.4 ms from run to run.
* **Uneven paired changes.** In a matrix session stability is judged on the
  per-round results, not the raw spread. When the ratio between the two
  schedulers differs from round to round by more than `--unstable-spread`
  percent (half the interquartile range of the per-round ratios, on a
  relative scale, so a steady "26× better" counts as even), and by more
  than the effect itself, the change is tagged *uneven ±x%* (`≠` in the
  terminal table): listed, but not highlighted or charted even if its
  interval excludes zero. "More than the effect itself" means more than the
  cautious end of the 95% interval is away from no change: "2.2× worse,
  interval 1.9× to 2.5×" with rounds ±14% apart is scored (1.9× is 90%
  away), "16× better, interval 1.2× to 60×" with rounds ±530% apart is not.
  Without this, a large effect read "clear" in one session and "unclear" in
  the next whenever the baseline wobbled a few percent more. The summary
  counts these separately among the unclear ones. A round without a usable
  ratio (typically the candidate measured zero) is not dropped: the
  per-round changes are then compared in percentage points (*uneven ±x
  pp*). A round where the baseline measured zero has no percentage change
  at all; the estimate then covers the other rounds only, the change is
  tagged *n of m rounds undefined* (`?` in the terminal table) and is not
  scored.
* **Different mix.** When a population is a very different part of the
  run on the two sides (its share of round trips differs by 2× or more,
  e.g. blocked round trips are 99.9% of one scheduler's and 40% of the
  other's), the report warns at the top. The latencies are still compared
  and scored, but they describe different parts of the two runs: read them
  together with `roundtrips_per_s`, which covers all round trips.
* **Summary.** The report opens with one line per scheduler and workload:
  *better*, *worse*, *mixed* or *no clear difference* against the baseline,
  with the number of compared metrics that are clearly better, clearly
  worse or unclear, and the largest clear changes. "Clear" is exactly what
  the table colours (and the terminal table stars). It is a count, not a
  score: metrics overlap and are not weighed against each other, and under
  load fewer round trips can be the price of more hog work. In the HTML
  report the explanations of each section are folded under "How to read
  this", so results and help text stay apart.
* **Minor populations.** Metrics of one population of events (`rtt_blocked_*`,
  `rtt_fast_*`, the same-/cross-CPU split) carry that population's share of
  the run's round trips. When it is below `--min-share` (default 1%) on
  either side, the cell is tagged *minor* (`°` in the terminal table): the
  change is still listed, but never highlighted or put on the change
  chart; on the quantile charts its curve is dashed. Its quantiles
  then describe a handful of unusual events, so a "78× worse" or "11×
  better" there says that the populations differ (e.g. a scheduler under
  which almost every round trip blocks, leaving 0.05% fast hand-offs), not
  that the same events got slower or faster. Results written before the
  share was recorded get it from the `roundtrips_*` counters, so they
  re-render the same way. `--min-share 0` turns this off.
* Single-sample extremes (`*_max`) and informational metrics are listed but
  never scored or charted. Informational are `hog_work_per_s`, which only
  says how CPU time was split between the hogs and the ping/pong threads,
  and `woken_per_s`, the rate of real wake-ups: it follows the mix of round
  trips, so a scheduler under which most replies arrive before the pinger
  sleeps wakes fewer threads while completing more round trips, and is not
  worse for it. Results written when `woken_per_s` was still scored are read
  the same way.
* Changes of 2× or more are written as multiples ("34× worse"). So is any
  end of an interval that far from no change, with its direction, since a
  percentage hides it: "−2819.1% … +97.2%" for a latency is "29× worse …
  36× better", an interval that includes no change. The change
  axis is capped at ±100%; larger changes are drawn clipped.
* `compare` refuses to compute a change, and leaves the series out of the
  latency charts, when the workload parameters (`pairs`, `hogs`, …) differ
  from the baseline's. It also refuses when a metric's unit or direction
  differs from the baseline's (or, when the baseline lacks the metric, from
  the first file that has it). It lists environment
  differences (CPU, CPU count, SMT, governor, EPP, platform profile, kernel,
  tool version) as notes. A different kernel never blocks a comparison.
* Governor and EPP are read from every cpufreq policy; non-uniform values
  are recorded as `mixed: performance×8, powersave×8`.
* The run warns when the 1-minute load average is above 25% of the CPU count.

For stable numbers: fixed governor/EPP, no background jobs, same kernel
build, and several runs per scheduler.

## Result format

One JSON file per session (`schema: 4`; `compare` also reads 3): tool version, label, system
metadata, the expected scheduler and `restart_detectable`, `design` (for
`matrix` sessions), and per-workload `params` and runs. Each run stores:

* `sched_before` / `sched_after` — the readings the validity check used;
* `metrics` — value, unit, and direction (compared across files);
* `visit`, `round` — in a `matrix` session, which scheduler visit and round
  the run belongs to;
* `counters` — diagnostics (not compared);
* `windows` — actual measurement boundaries relative to the common start;
* `latency_histograms` — per population, the full distribution in a log-linear encoding
  (exact below 64 ns, then 64 sub-buckets per power of two, so bucket width
  ≤1.6% of the value; min/max exact), stored sparse as `[lower, upper,
  count]` with inclusive bounds. Raw samples are not kept;
* `quantile_curves` — per population, the quantile function derived from
  that histogram (only where the sample count supports it). The "latency
  quantiles" charts plot exactly this.

See `crates/core/src/result.rs`. `schema` is bumped on incompatible changes.

## Layout

```
crates/core       scheduler detection, system info, result types
crates/workloads  Workload trait, runner, built-in workloads
crates/report     statistics, SVG charts, HTML report
crates/cli        the helicoprion binary
```

Adding a workload: implement `helicoprion_workloads::Workload` and register it
in `suite()`.

## Static build

Not verified in CI yet:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
file target/x86_64-unknown-linux-musl/release/helicoprion   # expect "statically linked"
```

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
