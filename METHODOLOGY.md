# Helicoprion methodology

This document explains how helicoprion runs a comparison, checks that every
run measured the scheduler it was meant to, what its workloads measure and
when the report calls a change clear. Installing, the quick start and the
result format are covered in the [README](README.md).

The statistics only cover the variation within one session. Two sessions can
differ in thermal state, scheduler options or background activity, and
helicoprion does not yet check how well results repeat from one session to
the next.

## Running a comparison with `matrix`

```sh
sudo helicoprion matrix --sched default --sched scx_lavd --sched scx_cosmos \
    --sched "scx_bpfland -s 5000" --rounds 5 -o lavd-vs-cosmos/
```

Run `matrix` as root, otherwise scxcli asks polkit for authorization at
every switch.

### Choosing the schedulers

`--sched` takes what scxcli takes: `default` for the kernel's own scheduler,
a scheduler or catalog profile id, or a scheduler with options quoted as one
argument. The first one is the baseline. Profile ids are looked up in
`scxcli list --json` up front, so helicoprion knows which scheduler and
options to expect after each switch.

`--recommended` instead compares every installed scheduler from
`scxcli list recommended` against the kernel's own scheduler. With the
default five rounds this takes about 11 minutes for five schedulers. The
list comes from the scx catalog, so the distribution or the administrator
decides what is in it. It cannot be combined with `--sched`.

### Rounds

Every round visits each scheduler once and runs every workload during the
visit. The order alternates between rounds (A B C, then C B A), so slow
drift such as the machine warming up is spread over all schedulers instead
of always hitting the one in the same position.

### Switching

`scxcli start` and `scxcli run` return once systemd has restarted
`scx.service`, which can be before the scheduler is attached. A switch only
counts as done when, for `--settle` seconds (3 by default), scxcli reports
the expected scheduler and options without a failure, the kernel has a BPF
scheduler attached whose ops name matches, such as `lavd` for `scx_lavd`,
and nothing changes, including the kernel's `enable_seq`. Use
`--allow-ops-mismatch` for a scheduler that names itself differently.

The session ends when a switch fails or does not settle within
`--switch-timeout` seconds (30 by default). An scxcli or systemctl call that
takes longer than `--command-timeout` seconds (20 by default) is killed, so
a hung scxcli cannot stall the session.

The options scxcli reports for the running scheduler are recorded with its
results. The session also ends when a scheduler comes back from a later
switch with other options than in its first visit: one result file stands
for one configuration.

### Restoring the previous state

At the end, after an error, or after a first Ctrl-C, which stops before the
next run or switch, helicoprion restores the scheduler and options that were
selected before, and whether `scx.service` was active and enabled. It then
checks both the configuration scxcli reports and the scheduler the kernel
has attached. A failed restore is reported before any other error, the
commands that restore everything by hand are printed when the session
starts, and a second Ctrl-C exits at once without restoring.

`matrix` refuses to start in a state it could not restore exactly: when
`SCX_SCHEDULER_OVERRIDE` or `SCX_FLAGS_OVERRIDE` is set, when the unit was
enabled with `systemctl enable --runtime` or is active without a configured
scheduler, when scxcli could not fully resolve its configuration, or when
the unit and the kernel disagree about the scheduler.

### Output

The results go to a new directory set with `--output`, named
`helicoprion-matrix-<time>` by default. Each scheduler gets a
`NN-<spec>.json` that is rewritten atomically after every visit, so a crash
loses at most the visit in progress, and `report.html` is written at the
end. `compare` reads these files like any other result files.

## Checking the scheduler of every run

| Condition | Reported as |
|---|---|
| No `/sys/kernel/sched_ext` | `EEVDF` (`CFS` before 6.6), "no sched_ext" |
| `/sys/kernel/sched_ext/state` is not `enabled` | `EEVDF`, "sched_ext available, inactive" |
| `state` is `enabled` and `root/ops` is set | `scx_<name>`, with the raw ops string and `enable_seq` |
| `/proc/sys/kernel/sched_bore` is 1 | `EEVDF+BORE` |

The name of the fair class is only inferred from the kernel version, so a
downstream patch set can make it wrong. BORE is detected directly.

Every run must see the scheduler recorded at the start right before and
right after it. Otherwise the run is marked invalid and left out of the
statistics, which catches a different scheduler attached between runs, a
crash or restart during a run and a manual switch. Kernels without
`enable_seq` cannot show a restart of the same scheduler, so the result file
then records `restart_detectable: false`, and both `run` and `compare` warn
about it.

## What is measured

The `quick` suite has two workloads, both with one thread pair for every two
CPUs.

| Name | What it does | Metrics |
|---|---|---|
| `wake-pingpong` | Each pair passes a token back and forth through a private futex | p50, p99, p99.9 and max in µs for each population, round trips per second, and threads actually woken per second |
| `wake-pingpong/loaded` | The same, while one CPU hog thread per CPU keeps every CPU busy | The same, plus hog work per second |

### Blocked and fast round trips

A round trip only measures a wakeup when the pinger really slept in
`FUTEX_WAIT`. Under load many replies arrive before that, and mixing both
kinds would make fast hand-offs look like excellent wakeup latency. So every
round trip goes into one of two histograms, `rtt_blocked` when the pinger
slept at least once and `rtt_fast` when the reply arrived first. A round trip
whose wait was interrupted by a signal before any sleep is counted as
`roundtrips_ambiguous` and kept out of both.

The diagnostics show the share of each kind, the round trips in which both
threads slept (`roundtrips_two_sided`), and how many waits really slept and
how many threads were really woken. A quantile is only reported when at
least ten samples lie beyond it, which means 20 samples for p50, 1000 for
p99 and 10000 for p99.9, so a small population never shows a made-up tail.

### Same CPU and cross CPU

The ponger reads its CPU with `sched_getcpu` just before handing the token
back, and the pinger reads its own once it has the token again. Blocked
round trips where both readings name the same CPU get
`rtt_blocked_same_cpu_*` metrics, and the others `rtt_blocked_cross_cpu_*`,
both with p50 and p99. Either thread may move between the two readings, so
they do not prove a local wakeup or an IPI, but they tell regimes apart,
such as a pair sharing one CPU and a pair spread over two.
`ping_cpu_changes` counts how often the pinger was seen on another CPU than
after the previous round trip. SMT siblings and separate cores are not told
apart yet.

A reading costs about 3 ns, and an A/A test showed no difference beyond its
noise of about 4%. `--no-cpu-diag` turns the readings off for `run` and
`matrix`. Results with and without them have different workload parameters
and are never compared with each other.

### Timing

All workers report that they are ready before one start time and deadline
are fixed for every pinger, and a run whose workers are not ready within
30 s is aborted. Hog work is read right at the start and again at the
deadline, and the real boundaries of each measurement are stored in
`windows`. If any worker fails, every waiting thread is woken and the run
ends as invalid.

## Reading the results

### Change against the baseline

Each workload has `--runs` measured runs (5 by default) after `--warmup`
runs that are thrown away (1 by default), or `--runs-per-visit` runs per
visit (1 by default) in a `matrix` session. Reports use the median of the
valid runs and the interquartile range, and every change is signed so that
positive means better.

The interval of a change is a 95% percentile bootstrap over the runs of both
sides. Files from one `matrix` session, recognised by their
`design.session_id`, are compared round by round instead: the change is
computed within each round, where both schedulers ran under the same
conditions, the estimate is the median of those changes, and the bootstrap
resamples whole rounds, so drift between rounds cancels out instead of
widening the interval. Runs of one session are never treated as independent
samples, so `--rounds 2 --runs-per-visit 5` still has only two rounds. With
a handful of runs or rounds this is a rough guide, not a significance test,
and there is no correction for comparing many metrics at once.

### When a change is clear

A change is clear, coloured in the HTML report and starred (`*`) in the
terminal table, only when none of these rules holds it back. They are
checked in this order. A change that is held back is still listed, and
extremes, minor populations, uneven and unstable changes are also kept off
the change chart.

| Held back when | Option | Mark |
|---|---|---|
| The metric is a single-sample extreme (`*_max`) | | |
| The population is less than a set share of the round trips on either side | `--min-share` (1%) | `°` |
| The baseline measured zero in some rounds | | `?` |
| There are fewer than 3 rounds, or 3 valid runs per side outside `matrix` | `--rounds`, `--runs` | |
| The 95% interval includes no change | | |
| The change is smaller than the threshold | `--min-effect` (5%) | |
| In `matrix`, the change varies too much between rounds | `--unstable-spread` (10%) | `≠` |
| Outside `matrix`, the runs of either side spread too much | `--unstable-spread` (10%) | `~` |

- A minor population describes a handful of unusual events, so even a large
  change there only says that the populations differ, not that the same
  events got slower or faster. Its curve is dashed on the quantile charts,
  and `--min-share 0` turns the rule off.
- The threshold covers what the bootstrap cannot see, the drift between
  sessions. In an A/A test with EEVDF twice on the same machine, whole
  sessions shifted by about 3%, so calibrate `--min-effect` with an A/A test
  on your own hardware, or use `matrix`.
- In `matrix`, the spread between runs is mostly drift, so the rule looks at
  how much the per-round ratios disagree, measured as half their
  interquartile range. That only holds a change back when it is also larger
  than the distance between no change and the cautious end of the interval.
  A change of 2.2 times worse with an interval of 1.9 to 2.5 times is
  therefore scored even with rounds 14% apart, because 1.9 times is 90% away
  from no change. A round without a usable ratio, typically because the
  candidate measured zero, is compared in percentage points instead.
- Outside `matrix`, the spread is half the relative interquartile range of
  the runs. A typical case is p99.9 under load at the edge of a rare plateau
  around 1.6 ms, flipping between 40 µs and 1.4 ms from run to run.
- `hog_work_per_s` and `woken_per_s` are informational and never scored. The
  first only shows how CPU time was split between the hogs and the ping-pong
  threads, and the second drops when more replies arrive before the pinger
  sleeps, which is not worse.

### Summary and wording

The report opens with one line per scheduler and workload that says better,
worse, mixed or no clear difference, with the number of clearly better,
clearly worse and unclear metrics and the largest clear changes. It is a
count, not a score. Metrics overlap and are not weighed against each other,
and under load fewer round trips can be the price of more hog work.

Changes of 2 times or more are written as multiples, such as `34× worse`,
and so are interval ends that far from no change, since a percentage such
as `−2819.1%` hides it. The end of an interval nearer to no change is cut
off rather than rounded, so 4.87 times reads `4.8×`. The change axis of the
charts is capped at 100% in either direction.

### Checks and warnings

`compare` refuses to compute a change, and leaves the series out of the
latency charts, when the workload parameters such as `pairs` or `hogs`
differ from the baseline's, or when the unit or direction of a metric does.
Differences in CPU, CPU count, SMT, governor, EPP, platform profile, kernel
and tool version are listed as notes, and a different kernel never blocks a
comparison. A run warns when the 1-minute load average is above 25% of the
CPU count. For stable numbers, fix the governor and EPP, stop background
jobs, keep the same kernel build and do several runs per scheduler.

When a population's share of the round trips differs by a factor of 2 or
more between the two sides, for example 99.9% blocked round trips against
40%, the report warns at the top. Its latencies are still scored, but they
describe different parts of the two runs, so read them together with
`roundtrips_per_s`, which covers all round trips.

## Plain-language results (`--simple`)

Add `--simple` to `matrix` or `compare` to get the result in plain words.
The HTML report always opens with an "In short" section like the one below,
one card per scheduler, and `--simple` folds the charts, tables and reading
notes under "Full report". In the terminal it looks like this:

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

The scoring is the same as in the full report. A line only says "more" or
"longer" where the table colours the change. Otherwise it says "no reliable
difference", however far apart the two numbers look, and names the first
rule from the table above that held the change back. "No clear difference"
means that the run could not tell the two apart, not that they are equal.

- When a scored estimate is far above the cautious end of its interval, the
  line also gives that end, as in `54× shorter, at least 1.49×`. This is one
  end of an estimated 95% interval, not a guaranteed minimum.
- A change held back only because its rounds were uneven still shows the
  direction its interval supports, but neither the verdict nor the count
  includes it.
- When the schedulers produce different mixes of round trips, the affected
  part gets a note below its metrics, such as "Note: Only 11% of round trips
  needed a wake-up, versus 99% for default." It describes a difference in
  behaviour, not a proven cause of a delay.
- Warnings of the full report, and results from different setups, are also
  mentioned above the folded section and in the terminal.
