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

### Compare existing result files

`compare` reads the result files of earlier `run` or `matrix` sessions,
prints a table in the terminal and writes an HTML report that needs no
JavaScript. `--svg-dir` also saves every chart as an SVG file.

```sh
helicoprion compare default.json lavd.json -o report.html --svg-dir charts/
```

Separate `run` sessions can drift apart, for example as the machine warms up,
so prefer `matrix` when the comparison has to be fair.

## Result format

Each `run` writes one JSON result file, and `matrix` writes one per
scheduler. The current format is schema 4, and `compare` also reads schema 3.
The schema number goes up with every incompatible change.

A file records the tool version, the label, the system, the expected
scheduler, whether a scheduler restart can be detected
(`restart_detectable`), the design of the `matrix` session if there is one,
and the parameters and runs of each workload. Each run stores:

- `sched_before` and `sched_after`, the scheduler readings used to check that
  the run is valid.
- `metrics`, each with its value, its unit and which direction is better.
  These are what `compare` compares.
- `visit` and `round`, the scheduler visit and the round the run belongs to
  in a `matrix` session.
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
