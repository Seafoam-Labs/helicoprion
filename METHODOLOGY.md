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
weights are no longer those of the runs before. `cgroup-fair` moves the
process for its own runs and puts it back, and this is what notices if that
ever fails.

## What is measured

The `quick` suite has two workloads, both with one thread pair for every two
CPUs. The `periodic` suite has the two timer-driven ones, `bursts` the three
sizes of a group woken at once, `cgroups` the two cases of a split between
control groups, and `all` runs all nine. `quick` is the default and stays as
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
both.

Each task sets its timer slack to 1 ns and reads it back. The default of
50 µs lets the kernel fire a timer that much late on purpose, which would
be measured as wake-up latency, and a run whose tasks did not get 1 ns is
invalid. The tasks report ready before one start is fixed for all; a run
whose tasks are not ready within 30 s, or in which a thread cannot be
created or fails, ends as invalid after every thread that was started has
been joined.

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
root of the hierarchy with two threaded children, gives them their weights,
moves its own process into the new group and each thread into its child.
Afterwards the process goes back to the group it came from and the three
groups are removed, however the run ends. Moving the benchmark's own process
changes where everything after it is measured, so none of this is taken on
trust. A run is invalid, whatever it measured, when the move changes the
CPUs the process may use, which happens when the group it came from has a
cpuset that the new one has not, when the process cannot be moved back, when
it is not in the group it started in afterwards, or when one of the groups
cannot be removed. The threads run for half a second before the window
starts, so that the scheduler has spread them and the split has settled.

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
comparison. So are the kernel build, the preemption model, the kernel
parameters, the CPU topology, the CPUs the benchmark may run on, isolated CPUs, the cpufreq driver, boost, the
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
  here.

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
preemption model, kernel parameters, CPU model, topology, usable and allowed
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
