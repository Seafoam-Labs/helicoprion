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

The same entry given twice is measured as two arms, `default` and
`default #2`: the same scheduler with the same options, visited separately
in every round like any other entry. Such an A/A pair differs in nothing but
when it ran, so what a report finds between its arms is what the method
finds where there is nothing to find. The second arm's result file names
the first as its twin (`twin_of`).

With a repeated entry the number of rounds must be even, and is 6 unless
`--rounds` says otherwise. The order of visits is reversed from round to
round, so over an even number each arm of a pair comes first as often as
second. Over five rounds one arm would lead three times, and an effect of
position would end up in the difference between the arms: if the later
visit of a round is 10% slower, two identical arms differ by a median of
10% over five rounds and by under 1% over six.

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

The same holds for where the benchmark itself runs. The control groups of
the process are read when the session starts and after every run, and a run
after which they differ is invalid, as is every later one: its limits and
weights are no longer those of the runs before. No workload moves the
benchmark: `cgroup-fair`, which needs threads in control groups of its own,
has them in a second process. If the benchmark is found elsewhere,
something outside it moved it.

## What is measured

The `quick` suite has two workloads, both with one thread pair for every two
CPUs. The `periodic` suite has the two timer-driven ones, `bursts` the three
sizes of a group woken at once, `cgroups` the two cases of a split between
control groups, `idle` the two cases of a wake-up on a sleeping machine, and
`all` runs all eleven. `quick` is the default and stays as
it is, so that its results remain comparable with earlier ones.

| Name | What it does | Metrics |
|---|---|---|
| `wake-pingpong` | Each pair passes a token back and forth through a private futex | p50, p99, p99.9 and max in µs for each population, round trips per second, and threads actually woken per second |
| `wake-pingpong/loaded` | The same, while one CPU hog thread per CPU keeps every CPU busy | The same, plus hog work per second |
| `periodic-wake` | A few threads wake on a timer every millisecond, work for 100 µs of CPU time and sleep again | p50, p99, p99.9 and max in µs of the wake-up latency and of the response time, and the share of periods that missed their deadline |
| `periodic-wake/loaded` | The same, while one CPU hog thread per CPU keeps every CPU busy | The same, plus hog work per second |
| `wake-burst/half` | One thread wakes a group of sleeping threads with one call every 2 ms, half as many as there are CPUs; each works for 200 µs of CPU time and sleeps again | p50, p99, p99.9 and max in µs of the time until a task runs, until the whole group runs and until the whole group is done, and the share of wake-ups that found the task still at work |
| `wake-burst/full` | The same with as many tasks as CPUs | The same |
| `wake-burst/double` | The same with twice as many tasks as CPUs | The same, with the quartiles of the time until a task runs in place of its median |
| `cgroup-fair/equal` | Two control groups of busy threads with the same `cpu.weight`, one with as many threads as CPUs and one with three times as many | How closely the split of CPU time follows the weights, the share of all CPU time that was used, the first group's share, and work per second |
| `cgroup-fair/weighted` | The same with as many threads as CPUs in both groups and three times the weight for the second | The same |
| `idle-wake/handoff` | Up to eight pairs of threads on an otherwise idle machine: every 20.3 ms one wakes the other and goes back to sleep | How long the woken thread takes to run, where it ran, and how deep the machine slept |
| `idle-wake/busy` | The same, with the waker working for 200 µs after its call | The same |

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

`rtt_blocked_slow_pct` is the share of the blocked round trips that took
about a millisecond or more, a hundred times a wake-up on an idle CPU and
about what waiting out another task's turn costs. The denominator is the
blocked round trips, not all of them. The threshold is approximate because
the count is taken from the histogram: every round trip of a millisecond or
more is counted, and with them those from 999.4 µs on, which share a bucket
with it. It is scored, and lower is better. It
exists because a percentile can sit on a step. With every CPU kept busy,
about one blocked round trip in a thousand takes some 1.4 ms under the
kernel's scheduler and the rest take microseconds, so `rtt_blocked_p99.9`
read 34 µs in one run and 1434 µs in the next of the same session, depending
on whether the slow ones were just under or just over a thousandth. Between
sessions that looked like a drifting baseline. The share says how many slow
round trips there were without a step to fall off, and is reported from
10000 blocked round trips on; `roundtrips_slow` is their number. The
percentile stays, and is unstable wherever the share is near 0.1%.

A share of counted events is only as good as the number of events behind
it, which the share does not show. In one comparison the kernel's scheduler
had 9 slow round trips in 33.6 million and two others had none: "100%
fewer", with an interval from 100% to 100% because every round agreed,
counted as a clear improvement, and enough to turn a workload that was
clearly worse in five other metrics into "better in some ways, worse in
others". Nine events are not a difference between schedulers. How this share
and the two others like it are compared is in "Shares of counted events"
below.

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

### Periodic wake-ups

`wake-pingpong` runs as fast as the scheduler lets it, so the amount and the
kind of work differ between schedulers. `periodic-wake` asks every scheduler
for the same thing at the same moments. Each task has a release every
period, at fixed instants that do not move when a period is late, and the
tasks' phases are spread over the period. There are up to four tasks, one
for every four CPUs.

For each period that runs, `wake` is the time from the release to the task
running again, and `response` the time from the release to the work being
done. The work is a fixed amount of the thread's own CPU time, so a task
preempted in the middle of it has a longer response, not more work. A
period is on time when its work is done before the next release. When it is
not, the releases that passed meanwhile are skipped, as a frame loop drops
frames instead of queueing them.

`deadline_miss_pct` is the share of all due periods that were late or
skipped, and it is the scored metric: 1.0% against 0.1% is ten times fewer
missed deadlines. `on_time_pct` is its complement and informational. As a
change of the share on time, 99.0% against 99.9% is under one percent, and
from 95.3% on not even a perfect 100% would reach a 5% threshold. A side
that missed no deadline has 0%. When that side is the baseline, the change
is undefined and the report says so instead of scoring it; such a
difference shows in the tail of `response`. The counters `periods_due`,
`periods_ran`, `periods_late` and `periods_skipped` give the numbers behind
both, and `task0_periods_missed`, `task1_periods_missed` and so on split the
late and skipped periods by task, in the order in which the tasks wake
within a period. The tasks differ in nothing but that instant, so misses
that are one task's and not the others' point at when it wakes. Which task
that is can change from run to run; the report sums the counters over the
runs, and the result file has them for each.

Each task sets its timer slack to 1 ns and reads it back. The default of
50 µs lets the kernel fire a timer that much late on purpose, which would
be measured as wake-up latency, and a run whose tasks did not get 1 ns is
invalid. The tasks report ready before one start is fixed for all; a run
whose tasks are not ready within 30 s, or in which a thread cannot be
created or fails, ends as invalid after every thread that was started has
been joined.

#### The period and the kernel's tick

The period is 1010 µs, not 1 ms, for a reason that was measured.

The releases are instants on `CLOCK_MONOTONIC`. The kernel's timer tick, at
which a busy CPU looks whether the running task should give way, is timed by
the same clock. With a period that is a whole number of nominal ticks, such
as 1 ms on a kernel with a tick of 1 ms, the releases of a task can keep
their place against the ticks for a whole run, and have another one in the
next run, because a run starts whenever it starts. Where a scheduler lets a
woken task wait for the tick, how long it waits can then depend on that
place, and so can whether its work is done before the deadline. "Can": the
ticks are per CPU, may be offset against each other and are stopped on a CPU
with nothing to run, and schedulers differ in whether a wake-up waits for
one at all.

Version 0.5.0 used 1 ms, and under the kernel's own scheduler the missed
deadlines of `periodic-wake/loaded` came in two kinds of run, about 0.1-0.5%
and 2.6-4.6%, with nothing in between. With the releases of the first task
placed on the clock (16 CPUs, tick of 1 ms, four tasks 250 µs apart, medians
of `deadline_miss_pct`):

| | at a multiple of 1 ms | 150 µs after | period 1010 µs |
|---|---|---|---|
| kernel scheduler (EEVDF) | 5.15% | 0.38% | 1.83% |
| scx_cake | 0.05% | 0.07% | 0.17% |
| scx_cosmos | 0.24% | 0.17% | 0.18% |
| scx_eevdf | 18.4% | 18.0% | 16.9% |
| scx_lavd | 30.0% | 34.2% | 32.1% |

Runs with the same placement agreed with each other; the two kinds were one
scheduler at different placements. In the runs with many misses nearly all
of them were one task's, the one released at the multiple of 1 ms, and its
delayed wake-ups took 1 ms less the distance of its release from the
multiple: the time to the next one. The other schedulers hardly follow the
placement, so the difference to the baseline, the thing a comparison
reports, was between 2 and 20 times for one scheduler and 5 and 100 times
for another, by the moment the baseline's runs happened to start. Without
load the placement shows as well, for every scheduler: the slowest wake-ups
(`wake_p99`) were 35-86 µs with a release at the multiple and 21-29 µs
otherwise.

This is one machine and one kernel, and what was set is the place of the
releases on the clock: that the tick is what sits at the multiples is
inferred from the tick's length and from the delays, not observed. The
conclusion does not need more than that. A period that is no whole number of
ticks shifts against any grid of the tick's length and moves through the
phases within a run, which limits this kind of dependence. 1010 µs is a
multiple of none of the usual ticks, and returns to the same phase against
their nominal grid after:

| tick rate | periods | time |
|---|---|---|
| 1000 Hz | 100 | 0.101 s |
| 250 Hz | 400 | 0.404 s |
| 300 Hz | 1000 | 1.010 s |
| 100 Hz | 1000 | 1.010 s |

A run of five seconds goes through the phases about five times or more, so
it no longer hangs on the one it started with, and runs repeat. This does
not take the tick out of the measurement. What a scheduler loses to waiting
for it is still in the result: sampled over many phases instead of at one.
Of the four rates only 1000 Hz was measured; the others are arithmetic.

The tick rate is recorded with the system (`tick_hz`, worked out from the
resolution of the coarse clock), and two options are there to look into a
result:

- `--periodic-period-us N` sets another period; 1000 is what 0.5.0
  measured. A report says so when a period is a whole number of the ticks
  of the kernel it ran on.
- `--periodic-phase-us N` places the releases of the first task at N µs
  past every multiple of the period on that clock, the other tasks evenly
  after it. This sets where the wake-ups fall on the clock, which is all
  that can be set from here. Where the ticks fall is the kernel's business:
  `skew_tick` gives every CPU its own offset, a CPU with nothing to run has
  its tick stopped, and a task can wake on another CPU each time. So the
  same phase is the same instant on the clock, and the same instant within a
  tick only as far as the ticks of the CPUs involved are where they were.

Both are parameters of the workload (`period_us`, `phase_us`): runs with
different periods, or with and without a phase, are never compared, and so
results of `periodic-wake` from 0.5.0 are not compared with later ones. The
scoring and the definition of `deadline_miss_pct` are as they were.

### Wake-up bursts

The two workloads above wake one task at a time. `wake-burst` hands the
scheduler a whole group in the same instant, as a thread pool given a batch
or a build step fanned out to workers does: one thread calls `FUTEX_WAKE`
for every waiter of one futex, each woken task works for a fixed amount of
its own CPU time and sleeps on the futex again. Bursts are released at least
2 ms apart, each one a period after the one before it actually came, so that
every burst finds the tasks with the same time to finish and go back to
sleep. The three sizes follow the machine: half as many tasks as CPUs, where
the group fits with room to spare, as many, where it fits only if the
scheduler uses every CPU, and twice as many, where half of the group has to
wait.

The waker notes the time right before its call. From that instant, `start`
is the time to a woken task running, with one sample per task and burst,
`all_running` the time to the last task of the group running and `all_done`
the time to the last one having done its work, both with one sample per
burst. The kernel wakes the group one task after another inside the waker's
call, so `start` includes the time the waker needs to get to a task. With
twice as many tasks as CPUs, how long the second half waits is the
scheduler's choice between letting the first ones finish and slicing, and
`all_running` and `all_done` show the two sides of it.

With more tasks than CPUs the typical `start` is two things, a task that
finds a CPU and one that waits for it, and a median sits on the step between
them: under one scheduler, in the runs of one session, it came out anywhere
from 24 to 206 µs. `wake-burst/double` therefore has the lower and the upper
quartile of the start delay in its place, `start_p25` and `start_p75`. They
are quartiles of all the wake-ups together. The tasks are not sorted into
those that found a CPU and those that waited. Where about half of them wait,
the lower quartile lies among the ones that did not and the upper among the
ones that did, which is why the two are stable where the median is not.
Where the split is another one, they are still the quartiles and nothing
more. The median is still on the quantile curve.

A wake-up is only measured for a task that slept in `FUTEX_WAIT` when its
burst was released, and every such wake-up is measured. A task woken by the
last burst that gets a CPU only after the run is over still takes its time
and does its work, because leaving it out would drop exactly the slowest
wake-ups. For every burst released, each task counts which of three things
happened:

| Counter | What happened |
|---|---|
| `wakeups_ran` | The task was asleep and the burst woke it: one sample |
| `wakeups_overrun` | The task was still busy with an earlier burst, woken by it and waiting for a CPU or at its work. It is not woken again, finds the burst gone when it is done and waits for the next |
| `wakeups_not_asleep` | The task was done with its work but not asleep yet: the few instructions between the two, or the first burst coming before the task got to sleep |

What tells the second kind from the third is the time. A burst released
before the task finished its work found it busy, and one released after
found it done, whenever the task gets to look. A task that loses its CPU
right after its work is therefore not taken for one that was still working.

The three add up to `wakeups_due`, bursts times tasks, for every task, and a
run in which they do not is invalid. `overrun_pct` is the share of the
second kind. It is scored like `deadline_miss_pct`, with the same handling
of zero, so that a scheduler which lets tasks overrun is not judged only on
the bursts that went well. The third kind is counted and shown but not
scored, since it is not a way of being late. `all_running` and `all_done`
exist only for bursts in which every task took part (`bursts_complete`).
`tasks_found_asleep` is what the wake calls returned, and equals
`wakeups_ran`.

The waker runs with a timer slack of 1 ns, set and read back as in
`periodic-wake`, and gets the slack it had back afterwards. Each burst is
due one period after the burst before it was released, not at a fixed
instant. A waker that comes late to a fixed grid releases its burst close to
the next one, and then tasks overrun because of the waker. That is what the
first runs on real hardware showed: on a machine that is idle between
bursts the waker is the first thing to wake up, and it came half a
millisecond late at the median and more than a millisecond in one release of
a hundred, under every scheduler. Spaced from the release, a late burst only
makes the gap longer.

One reading of the clock is the release. It is the time the tasks measure
from, it gives how late the release was against when it was due, and the
next burst is due a period after it. The lateness is recorded as
`release_late_p50`, `release_late_p99` and `release_late_max`, with its
distribution in the results. From that reading to the return of the wake
call is `wake_call_p50`, `wake_call_p99` and `wake_call_max`. The waker can
lose its CPU in that interval. If it does before the call has woken the
tasks, that time is in every `start` of the burst without being the tasks'
wake-up; if it does after, the tasks ran on time and only `wake_call` is
long. A large `wake_call` therefore says that the waker's side may have
added to the start delays observed, and does not by itself say which of
them. A long gap before the burst is published also moves the line between
`wakeups_overrun` and `wakeups_not_asleep`, which goes by the reading. All
of these describe the load and are informational.

A late waker releases fewer bursts in the same time: `bursts` of
`bursts_planned`, which is what a waker that is never late would release.
The workload regulates itself, and the price is that two schedulers are not
necessarily asked for the same amount of work. One under which the waker
runs later gets fewer bursts, with longer rests between them. With bursts on
a fixed grid the median lateness in `wake-burst/full` was 367 µs under the
kernel's scheduler and 3.5 to 71 µs under the sched_ext ones in the same
session, so the difference can be large. Each burst is still the same
trial, and the latencies per burst stand, but the rate belongs to the
result and has to be read with it. `bursts_per_s` records it,
informational, and the report puts the rates of the two sides next to each
other. How far they may differ before a comparison is no longer one is not
known yet, and no limit is set until A/A sessions with the new spacing show
how much the rate varies by itself. Only a run that got fewer than half of
the planned bursts is invalid outright: at half the rate the machine sleeps
twice as long between bursts, and that is no longer the workload.

Between two bursts the whole machine has nothing to do, and a machine with
nothing to do goes to sleep, the deeper the longer it expects to. Coming
back takes time that has nothing to do with the scheduler. On the machine
the first sessions ran on, the waker came 0.7 ms late at the median to a
burst due after 2 ms, and a woken task started after 46 µs. With the deep
idle states ruled out the waker was 12 µs late and the task started after
4 µs, so most of `start` had been the processor waking up. How deep the
machine sleeps depends on what the scheduler did a moment before, so the
waker was late by different amounts under different schedulers, and they
got anywhere from 378 to 498 bursts a second.

Where the process may ask for it, which takes root as `matrix` has, and
the machine says what to ask for, a run of `wake-burst` therefore holds a
request for shallow idle states for its length. This is the kernel's PM QoS
interface, `/dev/cpu_dma_latency`: while the file is held open with a bound
in microseconds written to it, no CPU enters an idle state that takes longer
to leave, and the request ends when the file is closed, also if the process
dies. What is in force is read back, and a run in which the bound is not in
force is invalid.

The bound is the exit latency of the shallowest idle state the machine has
in use: the smallest one above zero among the states cpuidle lists that are
not switched off. That keeps a halted CPU and rules out the deeper states.
It is not zero, which would keep every CPU spinning in the idle loop, and it
is not guessed: a bound of one microsecond on a machine whose first state
takes two would leave only the idle loop. Where the states cannot be read,
the request is not made.

This takes the time to leave a deep idle state out of the waker's timing
and out of the tasks' start delays, where it was a large source of variation
between schedulers, and makes their comparison more representative. It does
not by itself make the waker punctual or the number of bursts equal: the
scheduler, interrupts and preemption still delay the waker, and
`bursts_per_s` and `release_late` show by how much.

A run with the request has `idle: shallow` and the bound, `idle_latency_us`,
among its parameters. The bound is a property of the run, the same in each
of its repetitions, so it is a parameter and not a counter, and the report
shows a workload's parameters with its diagnostics. Runs with different
bounds are different experiments. A run that may not ask, as `run` without
root, or that cannot read the idle states, sleeps as deeply as the machine
likes and is never compared with one that asked.

What is given up is waking from deep idle. It is real, and where a
scheduler prefers a CPU that sleeps lightly it is also a decision of the
scheduler. It belongs in a workload made for it and not in one about placing
a group of tasks, where it made the schedulers' loads unequal and hid their
own share of the delay. The other workloads never let the whole machine fall
asleep and are left as they are.

The start and the handling of failures are those of `periodic-wake`: all
tasks report ready before the first burst, and a run that cannot start or
in which a thread fails ends as invalid after every thread has been joined.

### Control groups

The workloads above ask how soon a task runs. `cgroup-fair` asks who gets
the CPUs when there are not enough of them, and by which rule. The kernel
tells a desktop session from a build, or one service from another, as
control groups, and `cpu.weight` says how the CPUs are to be split between
them. A scheduler can follow that, split by group and ignore the weights,
or know nothing of groups and treat every thread alike. Two cases tell
these apart:

| Case | Weights | Threads | By the weights | Every thread alike |
|---|---|---|---|---|
| `cgroup-fair/equal` | 100 : 100 | CPUs : 3 × CPUs | 50% : 50% | 25% : 75% |
| `cgroup-fair/weighted` | 100 : 300 | CPUs : CPUs | 25% : 75% | 50% : 50% |

A scheduler that does not schedule groups at all fails the first case. One
that schedules groups and ignores their weights passes the first and fails
the second. Every thread is busy all the time and each group has at least
as many threads as there are CPUs, so each group could use the whole
machine and the split is the scheduler's alone.

What is measured is the CPU time each thread got during the window, read
from the threads' own CPU clocks by the measuring thread at both ends of
it, and summed per group.

- `weight_accuracy_pct` is how close the ratio of the two groups' CPU time
  is to the ratio of their weights, as a percentage of it. 100% is exact,
  and being off by a factor of three is 33% whichever group got too much,
  which is what both cases give under the wrong rule. It is scored, and
  higher is better. It is a ratio and not a difference of shares so that a
  relative change means something: an error of 0.1 against 0.3 percentage
  points would be a threefold difference that nobody cares about.
- `cpu_used_pct` is the CPU time of both groups as a share of what the
  machine has. With more busy threads than CPUs anything short of all of it
  is CPUs left idle. It is scored, and higher is better.
- `share_a_pct` is the first group's share of the CPU time used. It is
  informational and comes with `expected_share_a_pct`, the share by the
  weights, and `flat_share_a_pct`, the share by the number of threads, so
  that the report can say which rule the split followed.
- `work_per_s` is the integer mixing done by all threads, informational.

The workload needs cgroup v2 with the `cpu` controller available to the
children of the root, and the right to create groups there, which `matrix`
has as root. Where that is missing the run is invalid and says what was
missing; nothing is measured without the groups.

The threads of one process can only be in different groups in a threaded
subtree. For the run the benchmark creates `helicoprion-<pid>` under the
root of the hierarchy with two threaded children and gives them their
weights, and removes the three groups afterwards, however the run ends.

The threads are not the benchmark's own. Threads can only be put into the
children of a group their process is in, and moving a process moves all of
its threads: the benchmark would change control groups for the run, and
everything after it would rest on it having been put back, into a group
that may by then refuse it. Up to 0.6.0 it did that and checked it. Now the
benchmark starts itself a second time, and that process moves into the new
group, starts the threads, measures them and hands its result back as one
line of text. It does not move back: it ends, and every thread with it,
whatever went wrong inside. The benchmark stays where it was started.

A run is invalid, whatever it measured, when that process gives no result
(it could not be started, was killed, or ended without one; the reason and
the end of what it wrote are in the result file), when it wrote a result
and then did not end as it should, when its move changed the CPUs it may
use, which happens when the group the benchmark was started in has a cpuset
that the new one has not, or when one of the groups cannot be removed. The
process ends itself when the benchmark dies and after a time that no run
takes, so busy threads on every CPU are not left behind; before it moves or
starts anything it checks that the benchmark is still its parent, because a
parent that died before the process asked to be told is not reported. A
benchmark that is killed outright cannot remove the groups, and leaves
three empty ones under the root (`helicoprion-<pid>` with `a` and `b`)
until they are removed by hand or the machine restarts. What
is measured is what it was: the same threads, groups, weights and clocks,
read by the process the threads belong to. The threads run for half a
second before the window starts, so that the scheduler has spread them and
the split has settled.

The number of CPUs the workload is sized for has to be the number the
threads can run on, or there is nothing to split: four threads sized for two
CPUs get a CPU each on sixteen, and a scheduler that follows the weights to
the letter gives both groups half. That happens when the count was taken
under a CPU quota, which the group the benchmark was started in can have and
the new one under the root has not. A run whose CPU count differs from the
number of CPUs in its affinity mask is refused before anything is moved, and
one in which the threads used more than 105% of the CPU time that many CPUs
have is invalid. Both reject some set-ups that could be measured with more
work, which is better than reporting that a scheduler ignores weights it
follows.

### Wake-ups on a sleeping machine

The workloads above keep the machine awake, and `wake-burst` asks the kernel
outright to stay out of deep idle states, because the time to leave one
swamped what it measures. `idle-wake` measures that time, as far as it is
the scheduler's doing. A CPU that has been idle for a few milliseconds is in
a state that takes tens or hundreds of microseconds to leave, and a
scheduler chooses whether a wake-up pays for it: the woken task can run on
the CPU of the thread that wakes it, which is awake, on the CPU it last ran
on, which is asleep, or on any other.

There are up to eight pairs of threads, one for every two CPUs. The waker of
a pair sleeps on a timer for 20.3 ms, reads the clock and wakes its partner
with one `FUTEX_WAKE`; the partner reads the clock as soon as it runs and
goes back to sleep. `wake` is the time between the two readings, and its
quantiles are scored, lower is better. When the waker itself wakes up late
is not measured, only what happens from its call on. The pairs' phases are
spread over the period, and the period is no whole number of the usual timer
ticks, for the reason given under periodic wake-ups.

The long period is what lets the machine sleep, and it makes samples scarce:
246 wake-ups from each pair in a run of five seconds. The quantiles follow
the rule of every workload, and a p99 needs a thousand timed wake-ups; the
rule is not relaxed for this one. So what a run can say depends on the
machine and on the length of the run:

| CPUs | pairs | wake-ups in 5 s | what a 5 s run has |
|---|---|---|---|
| 16 or more | 8 | 1968 | typical delay and p99 |
| 10 to 15 | 5 to 7 | 1230 to 1722 | typical delay and p99, with less room for late wakers |
| 8 or 9 | 4 | 984 | typical delay only |
| 2 to 7 | 1 to 3 | 246 to 738 | typical delay only |

A machine in the lower rows gets the tail from a longer run (`--duration`):
six seconds with four pairs, twenty-one with one. With eight pairs a wake-up
comes every 2.5 ms somewhere on the machine, which leaves each task's own
CPU idle for 20 ms and the machine as a whole for less; how deep it slept is
what `idle_deepest_pct` reports. The period is not shortened to get more
samples, since that would wake the machine the workload is about.

The two cases differ in what the waker does next. In `idle-wake/handoff` it
goes back to sleep at once, as a thread that hands a piece of work on: its
CPU is free a moment later, and running the woken task there costs little.
In `idle-wake/busy` it works for 200 µs after the call, as a thread that
hands out work and goes on with its own: running the woken task on that CPU
means taking it from the waker or waiting, and sending it elsewhere means
waking a CPU up.

Four things are shown and not scored, because they say how a scheduler got
to its delay and not whether the delay is good:

- `timer_late` is how long after the instant its timer was set for a waker
  read the clock, for every wake-up it sent. It is the other wake-up of each
  round, by a timer and not by a thread, on a machine just as asleep. On the
  machine these workloads were tried on, a thread woken by another ran about
  36 µs later under every scheduler, while the timer-woken waker of
  `wake-burst` had been late by several hundred: the larger part of waking
  up from idle may lie here, and `wake` does not see it. It is not scored
  because it holds the timer's own lateness, the time to leave the idle
  state, the scheduler's time to run the waker and a few instructions of the
  workload, and how much of that a scheduler decides is not known. It is a
  population of its own, with its own distribution in the report. Its
  quantiles are not to be added to those of `wake`: the slowest of one need
  not come with the slowest of the other, and the sum of two p99 is no p99
  of anything. The time from a timer to the task running would have to be
  taken for each wake-up, which is not done.

- `same_cpu_pct` is the share of the measured wake-ups that ran the task on
  the CPU its waker was on when it called, `prev_cpu_pct` the share that ran
  it on the CPU it had last run on. Both are readings of the CPU at two
  instants.
- `idle_pct` and `idle_deepest_pct` are the share of the CPUs' time that
  the kernel's idle accounting (`cpuidle/state*/time`) has as idle, and as
  spent in the state that takes longest to leave, read before the first
  wake-up and after the last. They are the check that a run is what its
  name says: where the second is small, the machine did not go to sleep
  (something else kept it busy, or a request for shallow idle was in force),
  and the wake-ups were not from deep idle. They are absent where the kernel
  reports no idle states, as in most virtual machines; such results compare
  schedulers on a machine that cannot sleep deeply. They cover every CPU
  that reports idle states, also ones the benchmark may not run on: under a
  cpuset they say how deep the machine slept, not the CPUs of the tasks. The
  kernel adds a stay in an idle state to its count when the stay ends, so a
  stay that began before the window is counted whole and one still going on
  is not counted at all. The shares are rough for that reason and have come
  out up to two points over 100%: they say that a machine slept, not for
  exactly how long, and are no measure of energy.

The wake-ups are due at fixed instants, `wakes_planned` of them in the
window. A waker that comes back from its timer a period late skips what it
missed, and one that comes back after the end of the window sends nothing
for it, so that nothing is measured outside the window. `wakes_sent` and
`wakes_per_s` say what was sent. A scheduler under which the wakers run late
was asked for less, as with the bursts of `wake-burst`: the rate is shown
for every scheduler and no limit is drawn.

A wake-up is timed when the task was asleep and was woken by that call
alone. `wakes_not_asleep` found the task not asleep yet and `wakes_overrun`
were seen by the task only together with the next one; both are counted and
left out, and with 20 ms between wake-ups both should be zero. The waker
writes the time of a wake-up under that wake-up's number, and the task takes
it only if that number is still the latest when it has read it: a waker that
is already announcing the next wake-up cannot hand its time to this one. A
time that would lie after the task's own reading of the clock is not used
either (`wakes_mistimed`, which should be zero), instead of being counted as
a delay of nothing. Which idle
state a CPU was in at the instant of a wake-up is not known to the workload,
so a delay cannot be put down to a state: the shares above are of the whole
window.

How deep a machine sleeps, and how long it takes to wake, is a property of
the hardware and of the kernel's idle governor first. The workload compares
schedulers on one machine in one session; its delays say little about
another machine.

#### Initial hardware observations

What the workload found the first time it ran on real hardware, written down
because it is not what it was built to find. One machine: a Ryzen 7 8845HS
with 16 logical CPUs, kernel 7.3.4 with a tick of 1000 Hz, the kernel's
scheduler and four sched_ext schedulers, three sessions of five rounds each
on one day. All 150 runs were valid, every one sent 1971 or 1970 of its 1971
wake-ups, and none was left out as not asleep, overrun or mistimed. The
kernel had the CPUs in their deepest idle state 95-99% of the time in most
runs, so the machine did sleep with eight pairs at work. The ranges below
are of the medians of a scheduler in a session unless they say otherwise;
single runs go further out.

- **`timer_late`.** The wakers typically ran 610-690 µs after their timers
  (545-710 µs over all single runs) and 1.4-1.8 ms at the 99th percentile,
  under every scheduler alike:
  the schedulers' ranges overlap, and only the largest values of one of them
  stand out. In the two schedulers looked at, about two thirds of the values
  lay between 0.5 and 0.9 ms and under a tenth below 0.1 ms. This is the
  larger delay by far, and it is not to be read as the time to leave an
  idle state: it also holds the timer's own lateness and the time to run the
  waker.
- **`wake`.** The woken task ran 36-38 µs after its waker's reading of the
  clock at the median, the same to half a microsecond from session to
  session, and the schedulers differ by 2.5 µs at most. At the 99th
  percentile they are further apart, 57-77 µs for three of them and 76-116 µs
  for two, and one has largest values of up to 3.7 ms where the others stay
  at about 0.3 ms or below. The verdicts repeated: the two with the longer tails came out
  worse than the kernel's scheduler in 11 of 12 comparisons, the other two
  without a clear difference in 10 of 12.
- **Where the task ran.** On its waker's CPU in 0.0-0.4% of the wake-ups,
  and in no single run in more than 0.8%, under every scheduler and in both
  cases. On the CPU it had last run on in
  96-99% under four schedulers and 86-93% under one. These are two different
  things: nearly never the waker's CPU, and not always the previous one. The
  choice the workload was built around, the waker's awake CPU against a
  sleeping one, was not made by any of the five.
- **The two cases.** The typical delay is the same in `handoff` and `busy`.
  The tails are not quite: under the kernel's scheduler the 99th percentile
  of `wake` was 57-65 µs in `busy` and 68-77 µs in `handoff`, `timer_late`
  had a longer tail in `busy` under all, and one scheduler returned the task
  to its previous CPU less often. The verdicts of the two cases differed in
  3 of 12 pairs. `busy` adds little here and is kept: one machine is not
  grounds to remove it.

One possible explanation of the two delays is that the timer-driven waker
pays much of the cost of the first exit from deep idle before it issues
`FUTEX_WAKE`. The measured `wake` would then mostly describe how the target
task is scheduled afterwards, and not the first transition out of deep
idle. This remains a hypothesis: the workload does not record the idle state
of the specific CPU at the moment of each wake-up.

As a comparison of schedulers this is mostly a negative result, and a
useful one: on this machine the dominant delay observed on a sleeping
machine is the waker's lateness for its timer, which the schedulers share,
and not the stretch from `FUTEX_WAKE` to the task running, where they differ
by a few microseconds and in their tails. Nothing was added to the scoring
on the strength of it. The limits are plain: one platform, one day, no
second processor to check against, and three sessions are a first look at
repeatability, not a calibration of false alarms.

### Scheduler statistics (`--sched-diag`)

The workloads measure what a task experiences, a latency or a share. With
`--sched-diag`, `run` and `matrix` also record what the kernel counted for
every worker thread while it worked, which says something about how the
scheduler got there. A long tail with tasks that waited on a run queue is
another thing than one with tasks that were preempted in the middle of their
work, or one with tasks that were moved from CPU to CPU.

Each worker thread reads its own counters from `/proc/thread-self` when its
work begins and when it ends, and the differences are summed over the
threads. No tracing and no rights are needed.

| Source | What it gives |
|---|---|
| `schedstat` | Time on a CPU, time runnable and waiting for one, and the number of times the thread was given a CPU |
| `status` | Voluntary switches, where the thread slept or yielded, and involuntary ones, where the CPU was taken from it |
| `sched` | `se.nr_migrations`, where the kernel provides the file |

From these come five metrics, all informational:

| Metric | Meaning |
|---|---|
| `sched_wait_pct` | Share of the time the threads wanted a CPU that they spent waiting for one |
| `sched_slice_us` | Average time on a CPU per stint |
| `sched_preempt_pct` | Share of the switches that took the CPU away from a thread |
| `sched_switches_per_s` | Switches per second, all threads together |
| `sched_migrations_per_s` | Changes of CPU per second, all threads together |

The sums behind them are in the counters `sched_threads`, `sched_run_ms`,
`sched_wait_ms`, `sched_runnable_ms`, `sched_slices`, `sched_switches`,
`sched_voluntary`, `sched_involuntary` and `sched_migrations`. Migrations
are only reported when every thread has them. If a thread cannot read its
`schedstat` or `status`, the run is invalid, since the statistics were asked
for. So is a run after which no thread has any time on a CPU in its
`schedstat`: a kernel built without scheduler statistics
(`CONFIG_SCHED_INFO`) still has the file, and it holds zeros.

None of this is scored. The counters describe a mechanism, and no mechanism
is better as such: fewer migrations are good where they keep caches warm and
bad where they leave a task waiting behind another one. They are there to
explain a difference the scored metrics show, not to make one.

The readings are taken by the threads that are measured, at these points:

| Workload | Threads | From | To |
|---|---|---|---|
| `wake-pingpong` | Both threads of every pair | Before the thread reports ready | After its last round trip |
| `periodic-wake` | The tasks | After the start is fixed, before the first release | After the last period |
| `wake-burst` | The tasks, not the waker | Before the task reports ready | When the task leaves |
| `cgroup-fair` | The threads of both groups | When the threads start working | When they stop |
| `idle-wake` | Wakers and woken tasks | Wakers at their first wake-up, tasks when they start waiting | Wakers after their last wake-up, tasks when told to stop |

Nothing is read inside what is timed, but the readings are not free and
they add a little that is not the workload: the wait at the start gate in
`wake-pingpong`, the half second before the window in `cgroup-fair`. The
option is therefore off by default, and a run with it has the workload
parameter `sched_diag`, so that it is never compared with a run without it.
The CPU hogs of the loaded variants are not included.

A `FUTEX_WAIT` that reports sleeping does not necessarily imply a context
switch: when the wake-up comes after the thread was queued and before it
left its CPU, the wait returns the same way and the thread never stopped
running. In `wake-pingpong` the voluntary switches can therefore be fewer
than the waits that slept. Their difference is not a count of hand-offs that
needed no scheduling, though. The two counters do not correspond one to one:
the statistics cover both threads of a pair and the wait at the start gate,
and a thread gives the CPU up for other reasons than these waits.

The statistics are sums over a whole run. A high `sched_wait_pct` next to a
long tail fits tasks waiting on a run queue, and does not show that the
slowest wake-ups of the run were the ones that waited.

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

### Shares of counted events

Three scored metrics are events out of opportunities, and both numbers are
counters of the run:

| Metric | Events | Out of |
|---|---|---|
| `rtt_blocked_slow_pct` | `roundtrips_slow` | `roundtrips_blocked` |
| `deadline_miss_pct` | `periods_late` + `periods_skipped` | `periods_due` |
| `overrun_pct` | `wakeups_overrun` | `wakeups_due` |

They are compared from those counters and in no other way. The rules above
about unstable sides and uneven rounds are not applied to them; what they
are for is in the interval described here.

**What is shown and estimated.** The value of a cell is the share over all
valid runs together, their events out of their opportunities, with the two
counts next to it. The median of the runs' own shares, which the other
metrics show, is zero for three runs of five with events and hides them.
The change is the ratio of the two sides' shares over what is compared:
the rounds both sides have in a session, every valid run otherwise. A run
with more opportunities weighs more in it.

**What one observation is.** In a session it is a round, with the counters
of its runs summed, and not a run: rounds are what a session repeats, and
two runs of one round are not two samples of it. Two rounds of two runs are
two observations and give no interval, as two rounds give none for any
other metric; five rounds of two runs are five. Results that are not one
session have no rounds in common, and every valid run is an observation.

**Events are not independent.** In one session the kernel's scheduler had
3613, 4569, 3394, 5844 and 5756 slow round trips in five runs of
`wake-pingpong/loaded`: about 700 times the variation independent events at
that share would have. `scx_cosmos` on the same workload had 62, 68, 74, 67
and 47: 1.6 times. Missed deadlines come in groups by construction: a
period that ends late, and the periods skipped for it. How many are
skipped depends on the scheduler: one after each late period under the
kernel's scheduler in the sessions seen (798 late, 818 skipped), three to
four under `scx_lavd` on an idle machine (24 late, 85 skipped). An interval
that takes every event for evidence of its own is too narrow by a large
factor, and by a different one for every scheduler.

**The interval** is the *effective-count approximate interval*. For each
side, the dispersion is how much the counts of its observations vary against
what independent events at its share would (Pearson's statistic over its degrees
of freedom). The larger of the two sides' is taken, not under 1, and
enlarged for having been estimated from few observations: to the value it
would exceed with a probability of 20% (times 2.4 with five of them, 1.6
with twelve). Both sides' counts are divided by it, and the interval is that of
a binomial split of so many events between the two sides. Only the two
sides compared enter: the result for a pair does not depend on which other
schedulers are in the report. The dispersion holds whatever makes runs
differ, clumps of events and a rate that changes from run to run, and
grows with the count. It is not a clump size, and is not carried from one
side to another.

**Twenty events on both sides.** The share is compared only where each side
has at least 20 events over the runs compared; for missed deadlines, 20
late periods, since the skipped ones follow from them. With fewer on either
side the report gives the counts where the change would stand, as in "9 /
33.6M → 0 / 26.2M; too few events to score", and the metric is left out of
every verdict, the summary's counts, the change chart and the card. That
covers a change to none, from none, and to or from a handful.

Both sides, because a side with few events cannot show how its events vary,
and its zeros are then read as the zeros of independent events. If its
events come rarely and many at a time, "none against thousands" is what most
comparisons of two sides with the same share look like. An earlier rule
asked for 20 events on one side only, reasoning that none where the other
has many is what an improvement looks like. Simulation of two equal sides
showed what that costs:

| The side with events that come many at a time | 20 on one side | 20 on both |
|---|---|---|
| neither (same on both) | 0.4% called different | 0.1% |
| the baseline | 4.1% | 1.7% |
| the candidate | 55% | 1.9% |

(Five rounds; the worst of 9 combinations of dispersion from 1 to 300 and 3
rates. With twelve rounds: 0.5%, 3.2%, 70% against 0.3%, 1.8%, 2.0%.) No
events seen is worth reporting, and is not proof of a share of zero. "Too
few events" is not "no clear difference" either: the first is a pair that
was not admitted to a comparison, the second a pair that was and did not
settle it.

The price is that real differences are found less often: in the same
simulations, about 40% of them with five rounds where a rule of one side
found about 55%. The loss is almost all in pairs where the better side has
under 20 events, which is where the false ones came from.

**What the interval was checked against.** It is not exact, and its
coverage is what simulation found, under the models simulated: events in
clumps of geometric size (dispersion 1, 10 and 300, the same and different
on the two sides), 5, 100 and 5000 events a run, ratios of 1, 0.1, 0.01 and
3, a drift shared by both sides of a round, opportunities that differ by
±30% between runs, 5 and 12 rounds of one run, 10 000 samples a case, the
dispersion estimated anew in each. Among pairs with 20 events on both sides
the nominal 95% interval was to hold the true ratio in at least 94% of
samples in every case that has enough such pairs to judge it by (at least
5% of its samples), and did: in each of about 170 cases, the worst at
96.7% to 96.9% in the runs made. Which cases reach those 5%, and the last
digit of every number here, depend on the samples drawn, which differ with
the version of NumPy even at one seed; what is fixed is the criterion.

For large shares (1.6% and 26%, beta-binomial, dispersion up to 30 and
different on the two sides) the worst of 84 cases was 98.6%, with at most
0.9% of equal pairs called different. The scripts are in `calibration/` and
print these numbers; `calibration/README.md` says which script gives which,
and with which versions they were run.

What that does not show: other shapes of clumps or of drift, dispersion
above 300, and real hardware, where a comparison of a
scheduler with itself (below) is the check. Twenty events are not twenty
independent events, and the coverage is conditional on the pair having been
admitted.

**On hardware.** Two sessions of a scheduler against itself, six rounds of
all workloads on a Ryzen 7 8845HS, one of the kernel's scheduler and one of
`scx_lavd`:

| Scheduler | Workload · share | Events, arm → twin | Change |
|---|---|---|---|
| EEVDF | `wake-pingpong/loaded` · slow round trips | 29 056 → 27 398 | +6.4% [2.5× worse … 2.8× better] |
| EEVDF | `wake-pingpong` · slow round trips | 68 → 34 | +49.9% [4.3× worse … 31× better] |
| EEVDF | `periodic-wake/loaded` · missed deadlines | 1866 → 2050 | −9.9% [−30.8% … +7.7%] |
| `scx_lavd` | `wake-pingpong` · slow round trips | 1332 → 1243 | +7.6% [−9.1% … +21.8%] |
| `scx_lavd` | `wake-pingpong/loaded` · slow round trips | 34 → 26 | +23.6% [−71.8% … 3.0× better] |
| `scx_lavd` | `periodic-wake/loaded` · missed deadlines | 29 584 → 31 187 | −5.4% [−29.4% … +14.1%] |
| `scx_lavd` | `wake-burst/double` · overruns | 43 → 64 | −48.8% [21× worse … 6.6× better] |

None of the seven is clear, with estimates of up to a half. Seven more
pairs had under 20 events on a side and were not compared: none against
none twice, 0 → 1, 14 → 9, 3 → 13, 19 → 7, and 67 → 42 missed
deadlines of which 14 and 10 were late periods. The method this replaced
called the 1866 → 2050 of the first session clear, at −11.2%. With three
older sessions of the kind that makes 19 pairs compared and none called
different. That agrees with the 2% at most of the simulation and is far too
few to measure it.

**Old results.** A result file without the counters, or with a run that
lacks them, is not compared for these metrics: the report says "no event
counters in this result; not scored". The runs' own shares are not compared
in their place, which would make two reports that describe one method use
two. One session whose two sides share no round says "no rounds in common;
not scored".

**Tried and not used.** A bootstrap of rounds on the sums has no width
where a side has none in every round, and held the true ratio in under 94%
of samples in every case simulated. Taking the events for independent
called up to 88% of equal pairs different even where the two sides clump
alike. Borrowing the dispersion of a
third scheduler of the session made a pair's result depend on that
scheduler and turned nearly every verdict into "unclear": a real tenfold
difference found in 98% of samples was found in 0.7% with a third arm of
independent events and many of them. A change from a baseline without
events is not scored: its one-sided bound did not hold up.

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
comparison. So are the kernel build, the preemption model, the timer tick
rate, the kernel parameters, the CPU topology, the CPUs the benchmark may run on, isolated CPUs, the cpufreq driver, boost, the
power source and how the runs were taken, where both files record them. A
run warns when the 1-minute load average is above 25% of the CPU count. For stable numbers, fix the governor and EPP, stop background
jobs, keep the same kernel build and do several runs per scheduler.

The report's Conditions tables show when each scheduler was measured and the
state of the machine then: per round and scheduler, the start time, the CPU
temperature from before the runs to after them, the CPU frequency right
after the runs and the load average. Every `matrix` session has its own
table and its own clock, and a file taken with `run` is a session of its
own: round 1 of one session is never laid next to round 1 of another. These
are read right before and after every run, outside the measurement. The load
average is the 1-minute one and includes the benchmark's own threads of the
last minute. After a workload that keeps many threads busy it stays near
their number well into the workloads that follow: in the first sessions with
`cgroup-fair` it read 12 to 21 before the first workload of every visit,
left there by the visit before. In a suite with such a workload it says
little about what else ran on the machine. Nothing in the table is scored.
It is there to judge whether the machine stayed the same throughout: visits
of one round that differ much were not measured under the same conditions,
and a round unlike the others is where drift between rounds shows.

When a population's share of the round trips differs by a factor of 2 or
more between the two sides, for example 99.9% blocked round trips against
40%, the report warns at the top. Its latencies are still scored, but they
describe different parts of the two runs, so read them together with
`roundtrips_per_s`, which covers all round trips.

## A/A: comparing a scheduler with itself

```sh
sudo helicoprion matrix --sched default --sched default -o aa/
```

Between two arms of the same scheduler nothing differs but when each ran.
What the report finds there is the noise of the method on this machine,
which is what `--min-effect` and the other thresholds have to be judged
against. An A/A pair can also stand next to other schedulers in one
session.

The report's A/A check lists every scored metric of the pair:

- **Change**: the repeated arm against the first, estimated and scored like
  any candidate against a baseline. A change scored as clear is a false
  alarm, and it is also raised as a warning: the method can produce a change
  of that size without any difference between schedulers, so changes of
  that size between other schedulers of the session should be read with
  caution and repeated in another session. The report names the
  `--min-effect` above which none of them would have been scored.
- **Later against earlier**: within each round, the arm visited second
  against the arm visited first, as the median over the rounds. The arms
  swap places every round, so over an even number of rounds an effect of
  position, such as a machine that
  is warmer for whichever arm comes second, cancels in the change and shows
  here. That is so for a metric both arms have in every round. One that is
  given only from a number of samples on can be missing from some rounds of
  an arm, and over the rounds both have it the order need not be even: two
  and one in a session that is three and three. The report says so of that
  metric, and does not call the session unbalanced for it.

One A/A session is one observation of the noise. Lay several side by side
with `sessions` before settling on a threshold. Nothing here changes a
threshold by itself.

## Comparing sessions

```sh
helicoprion sessions monday/ tuesday/ friday/ -o sessions.html
```

The intervals of a `matrix` session come from resampling its own rounds.
They cannot see what changes between sessions: the state the machine booted
into, or what else ran that evening. `sessions` takes the directories or
result files of two or more `matrix` sessions and treats each session as
one observation.

### The sessions must repeat one experiment

Before anything is computed, every session is checked against the oldest
one. A difference stops the analysis and is named: the schedulers and their
order, a scheduler without results, each scheduler's build (the kernel's ops
string) and options, the workloads and their parameters, the protocol
(suite, runs per visit, warm-up, run length, pause, settle time, order of
visits), the tool version, and the machine (kernel release and build,
preemption model, timer tick rate where both record it, kernel parameters,
CPU model, topology, usable and allowed
CPUs, SMT, governor, EPP, cpufreq driver, boost, power source, platform
profile, THP). There is no option to override this.

What a file does not record cannot be compared. It counts as neither equal
nor different and is listed as not compared, so scheduler options that were
not recorded are never taken for "no options". Sessions written before the
protocol and the fuller description of the machine were recorded can still
be laid side by side, with those fields listed.

### What is shown

Every session is analysed on its own, exactly as `compare` would, and the
answers are laid side by side. Nothing is pooled or averaged, and there is
no interval across sessions and no reproducibility score: the rounds of a
session are not independent samples of how sessions differ.

- **Verdicts**: what each session concluded about each scheduler on each
  workload.
- **Effects**: each scored metric against the baseline, session by session,
  with that session's own estimate and scoring. Across sessions it is then
  scored the same way in all of them, in some only, as better in some and as
  worse in others, or in none, with the smallest and largest estimate. A
  change that a session does not score does not count, whatever its sign.
- **A/A across sessions**, when the sessions have a repeated arm: each
  scored metric of the pair, session by session, with the sessions that
  scored it as a clear change (false alarms), the largest change in any
  session, and the range of the position effect. The largest change is the
  noise that metric has shown on this machine; a threshold below it has
  already been crossed without any difference between schedulers. The report
  says how many sessions measured the pair at all, in how many of them a
  confidence interval was available and how many did not balance the order
  of the arms. A session without data, or without an interval for any
  metric, found no false alarm and confirms nothing. That intervals were
  available is all that is claimed: an interval is needed for a change to
  be scored and is not enough for it.
- **Levels**: the measured values themselves, session by session, with the
  largest session median over the smallest. This is the stability of the
  result apart from any comparison. When a scheduler's own values hardly
  move and the baseline's do, a disagreement between sessions is the
  baseline's.

Agreement is counted over the sessions that measured a metric, and the
report says when those are not all of them ("better in both measured
sessions (2 of 5)"). Such a metric is not counted among those scored the
same way in every session, and a scheduler without a verdict in some
session is never reported as having the same verdict in every session.

Fewer than five sessions are flagged: they show what differed between them
and are too few for even a first idea of how often a result would repeat.
Five is a practical floor for that, not a statistical guarantee: a report
lists dozens of metrics that move together, and agreement across a handful
of sessions on one of them proves little by itself.

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

### The result card

`compare --png FILE` writes a picture of 1200 × 630 pixels, and `matrix`
writes it as `card.png` next to its report: a row per scheduler, a column
per workload, and in every cell the verdict of the plain view for that
pair, as a word, a mark and a colour. The verdicts are taken from the plain
view, not worked out again, so the card cannot say something the report
does not. Its head says what everything is measured against: the baseline
by the name the session gave it and by the scheduler its result records, as
in "The baseline, default, is EEVDF: the scheduler built into the kernel",
since a card is seen without the session around it and "default" alone says
nothing. A cell is empty where the two were not compared, which is not the
same as "unclear", the word for no clear difference.

Next to its mark a cell gives one size: the largest of the clear changes in
the verdict's direction, "31%" next to the mark of a cell that says worse,
and for a mixed cell the largest each way, as in "63×" and "2.9×". It is
one of the changes the verdict counts and is written as the report writes
it, a multiple from 2× up and a percentage below, without the sign and
without the metric's name, for which a cell has no room. It is the most
that was found and not what is typical of the workload, and the card says
so in its head: a cell with "63×" can stand for one metric of a dozen. A
share of counted events that went from some to none is not among those
changes: with events on one side only it is not compared at all ("Shares of
counted events"), so it is not in the verdict and cannot be the size next
to it. Should any other clear change be one to nothing, which reads "100%"
whatever it was before, it is passed over for the next largest. A mixed
cell shows both of its sizes or neither, since one of them alone would be
read as all there is. Sizes are drawn where the card has room for them in every cell
that has any: on a card with many workloads the two of a mixed cell go one
above the other, and with many schedulers as well they are left out, for
all mixed cells of that card alike.

A row that is the baseline's own scheduler measured again, as a session
with a repeated scheduler has, says so under its name: "the baseline again:
noise". A cell of such a row that reads better or worse is what the method
finds where there is nothing to find, which the report says in its A/A
section and a card passed around alone has to say itself. Where every row
is one, the card is headed as what it is, "scx_lavd compared with itself",
an A/A check, and not as a comparison of schedulers. The cells are not
changed for it: they show what was found.

The card is for places that show a picture and not a page. It holds one
size per direction, no intervals and no warnings, and those are what make
a verdict worth trusting; it says on its face how many warnings the report
has, and it is not a result to quote without the report.

Drawing it needs a font installed on the machine for the text, and the `png`
feature, which is on by default and adds an SVG renderer that nothing else
uses. A build with `--no-default-features` leaves it out: `--png` is refused
there, `matrix` writes no card, and `--svg-dir` writes the same card as
`card.svg`.
