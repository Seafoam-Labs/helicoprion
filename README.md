<p align="center">
  <img src="helicoprion-icon.png" alt="helicoprion icon" width="160">
</p>

<h1 align="center">helicoprion</h1>

<p align="center">
  A small, scheduler-focused CPU benchmark for Linux.
</p>

It detects the running scheduler (EEVDF, EEVDF+BORE, or a sched-ext BPF
scheduler), runs built-in workloads, and compares results in the terminal and
in a self-contained HTML report with charts.

No external benchmark programs or downloaded assets are needed. A release
build produces one dynamically linked (glibc) binary; a static build is also
possible (see below).

For the experimental design, scheduler switching, workload definitions,
statistics, and limitations, see [METHODOLOGY.md](METHODOLOGY.md).

## Layout

```
crates/core       scheduler detection, system info, result types
crates/workloads  Workload trait, runner, built-in workloads
crates/report     statistics, SVG charts, HTML report
crates/cli        the helicoprion binary
```

Adding a workload: implement `helicoprion_workloads::Workload` and register it
in `suite()`.

## Install

From crates.io:

```sh
cargo install helicoprion
```

Packages for Arch, Fedora and openSUSE are in [packages/](packages).

## Quick start

Or build the binary from a checkout, and inspect the active scheduler:

```sh
cargo build --release
B=target/release/helicoprion
$B info
$B run -o current.json
```

For a scheduler comparison, **prefer `matrix`**. It switches schedulers through
`scxcli`/`scx.service`, interleaves visits across rounds, and attempts to
restore the previous scheduler and service state afterward. Run it as root
because switching schedulers otherwise prompts for polkit authorization.

```sh
# Installed schedulers from the scx recommended catalog, plus kernel default:
sudo "$B" matrix --recommended --simple

# Or choose the candidates explicitly (first scheduler = baseline):
sudo "$B" matrix --sched default --sched scx_lavd --sched scx_cosmos \
    --rounds 5 -o comparison/
```

`--recommended` uses installed entries from `scxcli list recommended` and
runs five rounds by default. It cannot be combined with `--sched`.
`--simple` adds a plain-language summary with reasons for inconclusive
results; the HTML report still contains the complete data and charts.

To compare **existing** result files without running a new matrix session:

```sh
$B compare default.json lavd.json -o report.html --svg-dir charts/
```

`compare` prints a terminal table and writes a standalone, light/dark-aware
HTML report (no JavaScript). `--svg-dir` also exports individual SVG charts.
Separate `run` sessions may drift independently; use `matrix` for controlled
comparisons. For exact switching, restore, pairing and scoring behavior, see
[METHODOLOGY.md](METHODOLOGY.md).

## Static build

Not verified in CI yet:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
file target/x86_64-unknown-linux-musl/release/helicoprion   # expect "statically linked"
```

## Result format

Each `run` writes a JSON result file; `matrix` writes one JSON file per
scheduler. The current format is `schema: 4` (`compare` also reads schema 3).
Files record the tool version, label, system metadata, expected scheduler,
`restart_detectable`, optional matrix `design`, and each workload's parameters
and runs. Each run stores:

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

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
