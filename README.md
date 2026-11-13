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
| `wake-pingpong` | N/2 thread pairs pass a token through a private futex | per population: p50/p99/p99.9/max (µs); round trips/s; threads actually woken/s |
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
* Single-sample extremes (`*_max`) and informational metrics
  (`hog_work_per_s`, which only says how CPU time was split between the
  hogs and the ping/pong threads) are listed but never scored or charted.
* Changes of 2× or more are written as multiples ("34× worse"). The change
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
