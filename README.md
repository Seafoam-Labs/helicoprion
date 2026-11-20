<p align="center">
  <img src="helicoprion-icon.png" alt="helicoprion icon" width="160">
</p>

<h1 align="center">helicoprion</h1>

<p align="center">
  A small, scheduler-focused CPU benchmark for Linux.
</p>

helicoprion detects the running scheduler, runs its built-in workloads and
compares the results in the terminal and in a self-contained HTML report with
charts. It recognises EEVDF, EEVDF with BORE and sched-ext BPF schedulers, and
it needs no external benchmark programs or downloaded assets.

How the comparisons are run and scored is explained in
[METHODOLOGY.md](METHODOLOGY.md).

## Install

### From crates.io

```sh
cargo install helicoprion
```

### From a package

Packages for Arch, Fedora and openSUSE are in [packages/](packages).

### From source

```sh
cargo build --release
```

The binary is `target/release/helicoprion` and links dynamically against
glibc. A static binary can be built for musl instead, although CI does not
test this yet:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

## Quick start

### Check the current scheduler

`info` shows the kernel, the CPU and the scheduler in charge. `run` measures
that scheduler and writes the results to a file.

```sh
helicoprion info
helicoprion run -o current.json
```

### Compare schedulers

Use `matrix` to compare schedulers. It switches between them through scxcli
and `scx.service`, visits each of them once per round in alternating order,
and restores the previous scheduler when it is done. Run it as root,
otherwise polkit asks for authorization at every switch.

The easiest way is to compare the installed schedulers that scxcli
recommends against the kernel's own scheduler:

```sh
sudo helicoprion matrix --recommended --simple
```

To pick the schedulers yourself, name each of them with `--sched`. The first
one is the baseline.

```sh
sudo helicoprion matrix --sched default --sched scx_lavd --sched scx_cosmos \
    --rounds 5 -o comparison/
```

Both run five rounds by default, and `--recommended` cannot be combined with
`--sched`. `--simple` adds a summary in plain words, and the HTML report in
the output directory still has all the data and charts.

`--suite` picks the workloads: `quick` (the default) hands a token between
pairs of threads as fast as it goes, `periodic` wakes a few threads on a
timer for a short piece of work, as a frame or an audio buffer would,
`bursts` wakes a whole group of threads at once, as a thread pool given a
batch would, `cgroups` runs two control groups of busy threads and checks
that the CPUs are split between them by their weights, `idle` lets the
machine go to sleep and has one thread wake another, and `all` runs all
five. The first two come idle and with every CPU kept busy, the bursts with
half as many threads as CPUs, as many and twice as many, the control groups
once with unequal thread counts and once with unequal weights, and the
wake-ups on a sleeping machine once from a thread that sleeps again and once
from one that goes on working.
`cgroups` needs root and cgroup v2 with the `cpu` controller.

`--periodic-period-us` and `--periodic-phase-us` change the period of the
timer-driven workload and place its wake-ups on the clock, to see whether a
result depends on where they fall, as it does where a wake-up waits for the
kernel's timer tick. The period is 1010 µs for that reason, and no longer
the 1 ms of 0.5.0 (see METHODOLOGY.md). Runs with
either are only compared with runs with the same.

`--sched-diag` also records what the kernel counted for the worker threads:
how long they waited for a CPU, how often they were preempted and how often
they changed CPU. These numbers are not scored. They help explain a
difference, and a session with them is only compared with other sessions
with them.

### Compare existing result files

`compare` reads the result files of earlier `run` or `matrix` sessions,
prints a table in the terminal and writes an HTML report that needs no
JavaScript. `--svg-dir` also saves every chart as an SVG file.

```sh
helicoprion compare default.json lavd.json -o report.html --svg-dir charts/
```

`--png card.png` writes the result card: one picture with a cell for every
scheduler and workload that says whether the scheduler did better than the
baseline, worse, both, or neither clearly. It is for places that preview a
picture and not a page, such as a chat, and it leaves out how large and how
certain a difference is, so it goes with the report and not instead of it.
`matrix` writes it as `card.png` next to `report.html`. Drawing it needs a
font on the machine. A build without pictures (`cargo install helicoprion
--no-default-features`) is smaller and leaves the card out; `--svg-dir`
writes the same card as `card.svg` in any build.

Separate `run` sessions can drift apart, for example as the machine warms up,
so prefer `matrix` when the comparison has to be fair.

### Check whether a result repeats

One `matrix` session says how the schedulers differed that time. Run the
same `matrix` command again, on another day or after a reboot, and
`sessions` lays the sessions side by side: what each concluded, which
changes were scored the same way every time, and how much the measured
values themselves moved.

```sh
helicoprion sessions monday/ tuesday/ friday/ -o sessions.html
```

It refuses sessions that do not repeat one experiment, and says what
differs. See [METHODOLOGY.md](METHODOLOGY.md#comparing-sessions).

### Check the method itself

Give `matrix` the same scheduler twice and it measures it as two arms.
Nothing differs between them but when each ran, so whatever the report
finds there is the noise of the measurement on your machine:

```sh
sudo helicoprion matrix --sched default --sched default -o aa/
```

The report's A/A check lists what differed and flags every change it would
have scored as clear. Several such sessions, laid side by side with
`sessions`, show how large that noise gets for each metric. See
[METHODOLOGY.md](METHODOLOGY.md#aa-comparing-a-scheduler-with-itself).

## Result format

Each `run` writes one JSON result file, and `matrix` writes one per
scheduler. The current format is schema 4, and `compare` also reads schema 3.
The schema number goes up with every incompatible change.

A file records the tool version, the label, the system, the expected
scheduler, whether a scheduler restart can be detected
(`restart_detectable`), the design of the `matrix` session if there is one,
and the parameters and runs of each workload. `protocol` holds how the runs
were taken (suite, measured and warm-up runs, run length, pause), and the
design of a `matrix` session its settle time, the order of visits and the
options the scheduler ran with: what another session has to repeat to be a
repetition of this one. `system` describes the machine: kernel release and
build, preemption model, timer tick rate, the kernel parameters that affect scheduling,
timing or CPU power management (only those; a command line also holds disk
identifiers, which do not belong in a file that gets shared), CPU model and
topology, SMT, the CPUs the benchmark may run on, isolated CPUs, cpufreq driver, governor, EPP, boost, power
source and platform profile. `helicoprion info` prints the same. Each run
stores:

- `sched_before` and `sched_after`, the scheduler readings used to check that
  the run is valid.
- `metrics`, each with its value, its unit and which direction is better.
  These are what `compare` compares.
- `visit` and `round`, the scheduler visit and the round the run belongs to
  in a `matrix` session.
- `before` and `after`, the machine's state right before and right after
  the run: the time, the 1-minute load average, the CPU temperature (from
  the sensor named in `system.temp_sensor`) and the frequency the CPUs run
  at (lowest and highest of the cpufreq policies, and the mean over their
  CPUs). They say when
  each run happened and let a result be checked for drift. Values the
  machine does not offer are left out.
- `counters`, diagnostics that are not compared.
- `windows`, the real start and end of each measurement, relative to the
  common start.
- `latency_histograms`, the full latency distribution of each population.
  Buckets are exact below 64 ns and at most 1.6% of their value wide above
  that, and only the filled ones are stored, as `[lower, upper, count]` with
  inclusive bounds. Raw samples are not kept.
- `quantile_curves`, the quantiles of each population derived from its
  histogram, when it has enough samples. The latency quantile charts plot
  these.

The full definition is in `crates/core/src/result.rs`.

## Project layout

```
crates/core       scheduler detection, system info, result types
crates/workloads  Workload trait, runner, built-in workloads
crates/report     statistics, SVG charts, HTML report
crates/cli        the helicoprion binary
```

To add a workload, implement `helicoprion_workloads::Workload` and register
it in `suite()`.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
