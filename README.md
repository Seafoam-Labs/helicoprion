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
  A change is highlighted only when that interval excludes zero; with a
  handful of runs this is a rough guide, not a significance test. There is
  no multiple-comparison correction: with ~20 metrics, about one may be
  highlighted by chance even between two runs of the same scheduler.
* The change axis is capped at ±100%; larger changes are drawn clipped and
  labelled with the real value.
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

One JSON file per session (`schema: 3`): tool version, label, system
metadata, the expected scheduler and `restart_detectable`, and per-workload
`params` and runs. Each run stores:

* `sched_before` / `sched_after` — the readings the validity check used;
* `metrics` — value, unit, and direction (compared across files);
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
