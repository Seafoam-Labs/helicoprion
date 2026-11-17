# Helicoprion methodology

This document explains how helicoprion switches schedulers, checks that each
run measured the scheduler it was meant to, what its workloads measure and
how it reads the results. Installing, the quick start and the result format
are covered in the [README](README.md).

The statistics only cover the variation within one session. Two sessions can
differ in thermal state, scheduler options or background activity even when
each of them is consistent on its own, and helicoprion does not yet check how
well results repeat from one session to the next.

## Comparing schedulers with `matrix`

```sh
sudo helicoprion matrix --sched default --sched scx_lavd --sched scx_cosmos \
    --sched "scx_bpfland -s 5000" --rounds 5 -o lavd-vs-cosmos/
```

### Choosing the schedulers

`--sched` takes what scxcli takes:

- `default`, the kernel's own scheduler (`scxcli start default`).
- A scheduler or a catalog profile id (`scxcli start ID`).
- A scheduler with options, quoted as one argument (`scxcli run SCHED ARGS`).

The first scheduler is the baseline of the report. Profile ids are looked up
in `scxcli list --json` before the session starts, so helicoprion knows which
scheduler and options to expect after each switch.

To let scxcli choose, use `--recommended` instead:

```sh
sudo helicoprion matrix --recommended
```

This compares every installed scheduler from `scxcli list recommended`
against the kernel's own scheduler over the default five rounds, which takes
about 11 minutes for five schedulers. The list comes from the scx catalog, so
the distribution or the administrator decides what is in it.
`--recommended` cannot be combined with `--sched`, but every other option
works with it.

### Interleaved rounds

Every round visits each scheduler once and runs every workload during the
visit. The order alternates between rounds (A B C, then C B A, then A B C
again), so slow drift such as the machine warming up or background activity
is spread across all schedulers instead of always hitting the one in the same
position.

### Switching and settling

`scxcli start` and `scxcli run` return once systemd has restarted
`scx.service`, which can be before the scheduler is attached. A switch only
counts as done when all of these hold for `--settle` seconds (3 by default):

- `scxcli get --json` reports the expected scheduler and no failure. For
  profiles and `run`, the options must match as well.
- The kernel has a BPF scheduler attached whose ops name matches, such as
  `lavd` for `scx_lavd`. Use `--allow-ops-mismatch` for a scheduler that
  names itself differently.
- Nothing changes in the meantime, including the kernel's `enable_seq`.

If a switch fails or does not settle within `--switch-timeout` seconds (30 by
default), the session ends.

An scxcli or systemctl call that has not finished within
`--command-timeout` seconds (20 by default) is killed, so a hung scxcli
cannot stall the session.

### Restoring the previous state

At the end of a session, and also after an error or Ctrl-C, helicoprion puts
back the scheduler and options that were selected before, and whether
`scx.service` was active and enabled. scxcli enables the unit whenever it
starts a scheduler, so helicoprion runs `systemctl enable` or
`systemctl disable` to restore that.

The restore is checked twice. The configuration must match a fresh
`scxcli get --json`, and the previous scheduler must be running and attached
with a matching ops name, or nothing must be attached if nothing was before.
The commands that restore everything by hand are printed when the session
starts. A failed restore is reported before any other error, so a later
problem such as failing to write the report cannot hide it.

`matrix` refuses to start when it could not restore the current state
exactly, which is the case when:

- `SCX_SCHEDULER_OVERRIDE` or `SCX_FLAGS_OVERRIDE` is set, since scxcli
  clears it.
- The unit was enabled with `systemctl enable --runtime`.
- The unit is active without a configured scheduler.
- scxcli could not fully resolve its configuration (`config_error`).
- The unit and the kernel disagree about the scheduler (`scheduler: null`).

Pressing Ctrl-C once stops before the next run or switch and restores the
previous state. Pressing it again exits at once without restoring.

Run `matrix` as root, otherwise scxcli asks polkit for authorization at every
switch.

### Output

The results go to a new directory, set with `--output` and named
`helicoprion-matrix-<time>` by default. An existing directory that is not
empty is refused. Each scheduler gets its own `NN-<spec>.json`, rewritten
atomically after every visit, so a crash loses at most the visit in progress.
`report.html` is written at the end. The files record the design of the
session and each run its visit and round, and `compare` reads them like any
other result files.

## Scheduler detection

| Condition | Reported as |
|---|---|
| No `/sys/kernel/sched_ext` | `EEVDF` (`CFS` before 6.6), "no sched_ext" |
| `/sys/kernel/sched_ext/state` is not `enabled` | `EEVDF`, "sched_ext available, inactive" |
| `state` is `enabled` and `root/ops` is set | `scx_<name>`, with the raw ops string and `enable_seq` |
| `/proc/sys/kernel/sched_bore` is 1 | `EEVDF+BORE` |

The name of the fair class, EEVDF or CFS, is only inferred from the kernel
version, so a downstream patch set can make it wrong. BORE is detected
directly.

The scheduler seen when `run` starts is recorded in the result file, and
every run must see exactly that scheduler right before and right after it.
Otherwise the run is marked invalid and left out of the statistics. This
catches another scheduler attached between runs, a crash or restart during a
run (the ops name or `enable_seq` changed) and a manual switch. Older
sched_ext kernels have no `enable_seq`, so a restart of the same scheduler
between two readings goes unnoticed there. The file then records
`restart_detectable: false`, and both `run` and `compare` warn about it.

## Workloads

The `quick` suite has two workloads. Both use one thread pair for every two
CPUs.

| Name | What it does | Metrics |
|---|---|---|
| `wake-pingpong` | Each pair passes a token back and forth through a private futex | p50, p99, p99.9 and max in µs for each population, round trips per second, and threads actually woken per second (informational) |
| `wake-pingpong/loaded` | The same, while one CPU hog thread per CPU keeps every CPU busy | The same, plus hog work per second |

### Blocked and fast round trips

A hand-off only involves a real sleep and wakeup when the receiver is already
waiting in `FUTEX_WAIT`. Under load many hand-offs are not like that, and
mixing both kinds in one distribution would make the fast ones look like
excellent wakeup latency. So every round trip goes into one of two
histograms:

- `rtt_blocked`, when the pinger slept in `FUTEX_WAIT` at least once. This is
  wakeup latency.
- `rtt_fast`, when the reply arrived before the pinger blocked. This is not
  wakeup latency.

A round trip whose wait was interrupted by a signal (`EINTR`) before any
completed sleep could belong to either, so it is counted as
`roundtrips_ambiguous` and kept out of both.

The diagnostics show the share of each kind, the two-sided round trips in
which both threads slept (`roundtrips_two_sided`), and how many waits really
slept and how many threads were really woken. A quantile is only reported
when at least ten samples lie beyond it, which means 20 samples for p50, 1000
for p99 and 10000 for p99.9, so a small population never shows a made-up
tail.

On an idle 2-CPU VM, about 97% of the round trips were blocked. Under load
only 45 to 65% were, and about 20% were two-sided.

### Same CPU and cross CPU

The ponger reads its CPU with `sched_getcpu` just before handing the token
back, and the pinger reads its own once it has the token again. A round trip
is same CPU when both readings name the same logical CPU, and cross CPU when
they differ. These are two readings at two moments, not a trace of the
wakeup, and either thread may move in between, so neither label proves a
local wakeup or an IPI. They are good at telling regimes apart though, such
as a pair sharing one CPU and a pair spread over two.

Blocked round trips get `rtt_blocked_same_cpu_*` and
`rtt_blocked_cross_cpu_*` metrics with p50 and p99, and their full histograms
are kept in the result file. The diagnostics show the same CPU share of
blocked and fast round trips, and `ping_cpu_changes`, which counts how often
the pinger was seen on a different CPU than after the previous round trip.
These are changes seen between readings, not a count of migrations. SMT
siblings and separate cores are not told apart yet.

On a 2-vCPU VM, idle round trips on the same CPU had a p50 of about 1.9 µs
and those across CPUs about 20 µs. Under load, whole runs flipped between
almost all cross CPU (p50 about 28 µs) and 80 to 90% same CPU (p50 about
2 µs).

Each reading costs one `sched_getcpu` call, about 3 ns through the vDSO, and
the CPU number travels in the cache line of the token. An A/A test on that VM
showed no difference beyond its noise of about 4%, although an effect of a
few percent on the fastest hand-offs cannot be ruled out. `--no-cpu-diag`
turns the readings off for `run` and `matrix`. Results with and without the
readings have different workload parameters, so they are never compared with
each other.

### Timing and failures

- Every worker reports that it is ready before the start time is fixed and
  the gate opens, so all pingers share the same start and deadline. If the
  workers are not ready within 30 s, the run is aborted.
- Hog work is read right before the start time is fixed and again as soon as
  the main thread wakes at the deadline, and the hog rate uses exactly that
  interval. The real boundaries of the latency and hog windows are stored
  per run in `windows`, in seconds from the common start, so any
  misalignment can be seen instead of assumed away.
- If any worker fails, with an unexpected futex error or a panic, every
  waiting thread is woken and the run ends promptly and is marked invalid.

## Statistics and interpretation

### Runs and medians

`run` measures each workload `--runs` times (5 by default) after `--warmup`
runs that are thrown away (1 by default). In a `matrix` session each visit
has `--runs-per-visit` measured runs (1 by default) after its warm-up runs.
Reports use the median of the valid runs and the interquartile range.

### Change against the baseline

Every change is signed so that positive means better, whatever the metric.
Its interval is a 95% percentile bootstrap that resamples the runs of both
the baseline and the candidate, which needs at least 3 valid runs on each
side. A change is highlighted only when that interval excludes zero and the
change is at least `--min-effect` (5% by default). With a handful of runs
this is a rough guide, not a significance test, and there is no correction
for comparing many metrics at once.

The bootstrap only sees the spread between runs of one session. In an A/A
test, with EEVDF twice on the same machine, whole sessions shifted by about
3%, which highlighted seven metrics without the 5% threshold. Calibrate
`--min-effect` with an A/A test on your own hardware, or use `matrix`.

### Paired analysis in `matrix` sessions

Files from one interleaved session share a random `design.session_id`, and
older files without it are matched by start time and scheduler list. They
are compared round by round. The change is computed within each round, where
both schedulers ran under the same conditions, the estimate is the median of
those changes, and the interval is a bootstrap over whole rounds. Drift
between rounds then cancels out instead of widening the interval. With a
simulated drift of up to 60% in either direction and a constant 10%
difference, the paired
interval is about +8% to +12%, while the unpaired one on the same numbers
includes zero.

The runs of one session are never treated as independent samples. With
`--rounds 2 --runs-per-visit 5` there are ten runs but only two independent
rounds. With one or two shared rounds, the report gives the paired estimate
without an interval. With none, it gives the change of the medians, labelled
as a descriptive comparison that is not paired, without an interval and with
a warning. Neither is ever highlighted, so use at least 3 rounds.

In a paired comparison, stability is judged by how much the changes of the
single rounds disagree, not by the spread between runs, which in a `matrix`
session is mostly drift.

### Unstable cells

A cell whose spread between runs, taken as half the relative interquartile
range, is above `--unstable-spread` (10% by default) is marked unstable. It
is shown in the table but never highlighted or put on the change chart. A
typical case is p99.9 under load sitting at the edge of a rare plateau
around 1.6 ms, so it flips between 40 µs and 1.4 ms from run to run.

### Uneven paired changes

In a `matrix` session, a change is tagged uneven (`≠` in the terminal table)
when the ratio between the two schedulers varies too much from round to
round. The variation, half the interquartile range of the ratios on a
relative scale, must be above `--unstable-spread` percent and also above the
distance between no change and the cautious end of the 95% interval. A
change of 2.2 times worse with an
interval of 1.9 to 2.5 times and rounds that differ by 14% is scored,
because 1.9 times is 90% away from no change. A change of 16 times better
with an interval of 1.2 to 60 times and rounds that differ by 530% is not.

An uneven change is listed but neither highlighted nor charted, even when
its interval excludes zero, and the summary counts it among the unclear
ones. Without this rule, a large effect could be clear in one session and
unclear in the next whenever the baseline wobbled a little more.

A round without a usable ratio, typically because the candidate measured
zero, is not dropped. The changes of the rounds are then compared in
percentage points instead (`uneven ±x pp`). A round where the baseline
measured zero has no percentage change at all, so the estimate covers the
other rounds only, and the change is tagged as undefined in n of m rounds
(`?` in the terminal table) and not scored.

### Different mix

When a population makes up a very different part of the run on the two
sides, the report warns at the top. That is the case when its share of the
round trips differs by a factor of 2 or more, for example when blocked round
trips are 99.9% of one scheduler's round trips and 40% of the other's. The
latencies are still compared and scored, but they describe different parts
of the two runs, so read them together with `roundtrips_per_s`, which covers
all round trips.

### Minor populations

The metrics of one population, such as `rtt_blocked_*`, `rtt_fast_*` and the
same CPU and cross CPU split, carry that population's share of the run's
round trips. When the share is below `--min-share` (1% by default) on either
side, the cell is tagged minor (`°` in the terminal table). The change is
still listed but never highlighted or put on the change chart, and its curve
is dashed on the quantile charts.

The quantiles of such a population describe a handful of unusual events, so
even a large change there says that the populations differ, not that the
same events got slower or faster. `--min-share 0` turns this off.

### Metrics that are never scored

Single-sample extremes (`*_max`) and informational metrics are listed but
never scored or charted. The informational ones are `hog_work_per_s`, which
only shows how CPU time was split between the hogs and the ping-pong
threads, and `woken_per_s`, the rate of real wakeups. `woken_per_s` follows
the mix of round trips. A scheduler under which most replies arrive before
the pinger sleeps wakes fewer threads while completing more round trips, and
is not worse for it.

### Summary

The report opens with one line per scheduler and workload that says better,
worse, mixed or no clear difference against the baseline. It gives the
number of compared metrics that are clearly better, clearly worse or
unclear, and the largest clear changes. Clear means exactly what the table
colours and what the terminal table marks with a star. It is a count, not a
score. Metrics overlap and are not weighed against each other, and under
load fewer round trips can be the price of more hog work. In the HTML
report, the explanations of each section are folded under "How to read
this", so they stay apart from the results.

### How changes are written

Changes of 2 times or more are written as multiples, such as `34× worse`.
So is either end of an interval that is that far from no change, with its
direction, because a percentage hides it. For a latency,
`−2819.1% … +97.2%` reads as `29× worse … 36× better`, an interval that
includes no change. For an interval that excludes no change, the end nearer
to no change is cut off rather than rounded, so 4.87 times reads as `4.8×`,
in the sense of "at least". The change axis of the charts is capped at 100%
in either direction, and larger changes are drawn clipped.

### What `compare` checks

`compare` refuses to compute a change, and leaves the series out of the
latency charts, when the workload parameters such as `pairs` or `hogs`
differ from the baseline's. It also refuses when the unit or direction of a
metric differs from the baseline's, or from the first file that has the
metric when the baseline lacks it. Differences in CPU, CPU count, SMT,
governor, EPP, platform profile, kernel and tool version are listed as
notes, and a different kernel never blocks a comparison.

Governor and EPP are read from every cpufreq policy, and values that differ
between policies are recorded as, for example,
`mixed: performance×8, powersave×8`. A run warns when the 1-minute load
average is above 25% of the CPU count.

### Getting stable numbers

Fix the governor and EPP, stop background jobs, keep the same kernel build
and do several runs per scheduler.

## Plain-language results (`--simple`)

Add `--simple` to `matrix` or `compare` to get the result in plain words:

```sh
sudo helicoprion matrix --recommended --simple
```

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
scheduler. With `--simple`, the charts, tables and reading notes are folded
under "Full report".

The scoring is the same as in the full report. A line only says "more" or
"longer" where the table colours the change, and "no reliable difference"
otherwise, however far apart the two numbers look. When a result is held
back, the line names the first reason that applies, checked in this order: a
minor population with too few relevant wakeups, a baseline of zero in some
rounds, too few paired rounds or runs, an interval that includes no change, a
change below the reporting threshold, and too much instability. "No clear difference"
means that the run could not tell the two apart, not that they are equal.

When a scored estimate is far above the cautious end of its interval, the
line also gives that end, cut off rather than rounded, as in
`54× shorter, at least 1.49×`. This is one end of an estimated 95% interval,
not a guaranteed minimum, and the report explains the phrase wherever it is
used.

When a change is held back only because its size varied too much between
rounds, the line still gives the direction the interval supports and why it
is not scored, as in
`fewer; the effect varied too much between rounds to score (estimated effect, 95% interval: 2.0× to 2.9×)`.
That interval describes the uncertainty of the estimate, not the range
across rounds. Neither the verdict nor the count includes such a change.
Background CPU work is informational and marked "observed, not scored".

When the schedulers produce different mixes of round trips, as warned in the
full report, the affected part explains the difference below its metrics,
for example "Note: Only 11% of round trips needed a wake-up, versus 99% for
default. The wake-up delays here do not describe the same mix of round
trips." Each part gets at most one note, about the blocking share when it differs
and otherwise about the population whose share changed the most. The note
describes a difference in behaviour, not a proven cause of a delay.

If the full report has warnings, or the results come from different setups
such as another kernel, CPU or governor, `--simple` says so above the folded
section and in the terminal.
