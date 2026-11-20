# Calibration of the interval for shares of counted events

The scripts behind "Shares of counted events" in `METHODOLOGY.md`. They
simulate two sides whose events come in clumps and at a rate that drifts
from round to round, apply the method to every sample as the report does,
with the dispersion estimated anew each time, and count how often the 95%
interval holds the true ratio and how often two equal sides are called
different. They are not part of the build and nothing runs them.

They need Python 3 with NumPy and SciPy. The seed is fixed, so a run of a
whole script prints the same numbers again on the same versions of those;
a part run on its own draws different samples, and so does another version
of NumPy, whose generators do not give the same stream for the same seed.
The numbers then differ in the third digit, and a case that sits at a
threshold can fall on the other side of it.

| Script | What it prints | Time |
|---|---|---|
| `sim3.py` | What the method is as shipped (the larger dispersion of the two sides with the allowance for few runs): equal sides called different under the rule of 20 events on one side and on both, in both directions of unequal clumping (part 1); large shares (part 2); the dispersion borrowed from a third arm, which was not adopted (part 3); coverage among pairs with 20 events on both sides (part 4). `python3 sim3.py` runs all, `python3 sim3.py 4` one part. | 20 s |
| `sim2.py` | The first round: four ways to an interval on the same samples (the larger dispersion, the same with the allowance, independent events, a bootstrap of rounds on the sums), by whether the two sides clump alike. | 6 min |
| `sim.py` | The generators and the first version of the interval, used by the other two. `python3 sim.py rare` and `python3 sim.py share` print its own tables, without the allowance. | 10 s |

The numbers in `METHODOLOGY.md` come from `sim3.py` run whole: the table of
equal sides called different and the share of real differences found from
part 1, the large shares from part 2, the third arm from part 3, the
coverage from part 4. The bootstrap and the independent events of
"Tried and not used" are from `sim2.py`.

## What is to be reproduced

The criterion, not the digits: among pairs with 20 events on both sides
the interval holds the true ratio in at least 94% of the samples of every
case of part 4, where a case is counted if at least 5% of its samples are
such pairs; and two equal sides are called different in a few percent of
samples at most under the rule of 20 events on both sides (part 1), against
more than half under the rule of one side where the candidate's events
come many at a time.

Part 4, `python3 sim3.py` run whole, in the two environments it has been
run in:

| Python | NumPy | SciPy | Cases | At 94% or more | Worst |
|---|---|---|---|---|---|
| 3.13.18 | 2.5.3 | 1.18.1 | 171 | 171 | 96.9% |
| 3.13.5 | 2.3.5 | 1.17.0 | 172 | 172 | 96.7% |

One case has close to 5% of its samples with 20 events on both sides and is
counted in one environment and not in the other.

`crates/report/src/shares.rs` is the method in the report. Its test
`the_interval_is_the_calibrated_one` holds it to the numbers `phi_pair` and
`interval` of `sim3.py` give for the same counts.

What the simulation is not: other shapes of clumps or of drift than the
ones in `sim.py`, dispersion above 300, or a machine. It has one
observation a round, which is what the report gives the method: the runs
of a round are summed before it sees them. A session of a scheduler against itself is the check on hardware.
