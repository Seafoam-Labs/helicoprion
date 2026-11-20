# Follow-up calibration: the "clear" rule as production applies it, both directions of
# unequal clumping, a candidate with no events, the phi allowance on large shares, and
# phi borrowed from a third arm. phi is re-estimated in every sample.
import numpy as np, sys, os, itertools
from scipy.stats import beta, chi2
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import sim
rng = sim.rng; REPS = sim.REPS; MIN_EVENTS = sim.MIN_EVENTS; MIN_EFFECT = sim.MIN_EFFECT

def phi_pair(cb, nb, cs, ns, extra=None):
    """max of the sides' own estimates (and of `extra`, a third arm's), floor 1, with the allowance."""
    est = [np.nan_to_num(sim.phi_hat(c, n), nan=1.0) for c, n in ((cb, nb), (cs, ns))]
    if extra is not None: est.append(np.nan_to_num(sim.phi_hat(*extra), nan=1.0))
    f = np.maximum(1.0, np.max(est, axis=0))
    df = cb.shape[1] - 1
    return np.maximum(1.0, f * df / chi2.ppf(0.2, df))

def interval(cb, nb, cs, ns, f):
    b, s = cb.sum(1) / f, cs.sum(1) / f
    Nb, Ns = nb.sum(1), ns.sum(1)
    with np.errstate(all="ignore"):
        lo = np.where(s > 0, beta.ppf(0.025, np.maximum(s, 1e-12), b + 1), 0.0)
        hi = np.where(b > 0, beta.ppf(0.975, s + 1, np.maximum(b, 1e-12)), 1.0)
        rr = lambda q: np.where(q >= 1, np.inf, q / np.maximum(1 - q, 1e-300) * Nb / Ns)
        est = np.where(cb.sum(1) > 0, (cs.sum(1) / Ns) / np.where(cb.sum(1) > 0, cb.sum(1) / Nb, 1), np.inf)
    return rr(lo), rr(hi), est

def verdicts(cb, cs, lo, hi, est, both):
    """Production's rule: enough events, a baseline with events (a change from none is not
    scored), a point estimate at least MIN_EFFECT from no change, an interval without no change.
    `both`: both sides need MIN_EVENTS, not one."""
    tb, ts = cb.sum(1), cs.sum(1)
    enough = (np.minimum(tb, ts) if both else np.maximum(tb, ts)) >= MIN_EVENTS
    ok = enough & (tb > 0) & (np.abs(1 - est) >= MIN_EFFECT)
    return ok & (hi < 1), ok & (lo > 1)        # better (fewer events), worse

def counts(rounds, lam, ratio, phi, drift, d=None):
    shape = (REPS, rounds)
    n = rng.uniform(0.7, 1.3, shape)
    if d is None: d = np.ones(shape)
    return sim.clumped(lam * ratio * n * d, phi), n * 1e6

def drift_of(rounds, drift):
    return np.exp(rng.normal(0, drift, (REPS, rounds))) if drift else np.ones((REPS, rounds))

PAIRS = ((1, 1), (10, 10), (300, 300), (300, 1), (1, 300), (10, 1), (1, 10), (300, 10), (10, 300))

def part1():
    print("== 1. clear as in production; rule 'one side >= 20' vs 'both sides >= 20' ==")
    print("rounds lam phi_b phi_s ratio | one side: better worse | both sides: better worse | candidate has no events")
    worst = {}
    for rounds, lam, (pb, ps), ratio in itertools.product((5, 12), (5, 100, 5000), PAIRS, (1.0, 0.1, 0.01, 3.0)):
        d = drift_of(rounds, 0.3)
        cb, nb = counts(rounds, lam, 1.0, pb, 0.3, d); cs, ns = counts(rounds, lam, ratio, ps, 0.3, d)
        lo, hi, est = interval(cb, nb, cs, ns, phi_pair(cb, nb, cs, ns))
        (b1, w1), (b2, w2) = verdicts(cb, cs, lo, hi, est, False), verdicts(cb, cs, lo, hi, est, True)
        zero = (cs.sum(1) == 0).mean()
        print(f"{rounds:5d} {lam:5d} {pb:5d} {ps:5d} {ratio:5.2f} | {b1.mean():.4f} {w1.mean():.4f} | {b2.mean():.4f} {w2.mean():.4f} | {zero:.3f}")
        if ratio == 1.0:
            kind = "same" if pb == ps else ("baseline clumpier" if pb > ps else "candidate clumpier")
            for name, v in (("one", (b1 | w1).mean()), ("both", (b2 | w2).mean())):
                worst[(kind, rounds, name)] = max(worst.get((kind, rounds, name), 0), v)
        else:
            right = (b1 if ratio < 1 else w1).mean(), (b2 if ratio < 1 else w2).mean()
            key = ("power", rounds, pb == ps)
            worst.setdefault(key, []).append(right)
    print("\nA/A, worst share of false clear:")
    for (kind, rounds, name), v in sorted(k for k in worst.items() if k[0][0] != "power"):
        print(f"  {kind:18s} {rounds:2d} rounds, {name:4s} side(s) >= 20: {v:.4f}")
    print("\nreal differences found (clear in the right direction), mean over scenarios:")
    for (_, rounds, same), v in sorted(k for k in worst.items() if k[0][0] == "power"):
        a = np.array(v)
        print(f"  {rounds:2d} rounds, {'same' if same else 'different'} clumping: one side {a[:,0].mean():.3f}, both sides {a[:,1].mean():.3f}")

def part2():
    print("\n== 2. large shares with the allowance ==")
    print("rounds share ratio phi_b phi_s | cover | clear(one side)")
    worst = 1.0; aa = 0.0
    for rounds, share, (pb, ps), ratio in itertools.product((5, 12), (0.016, 0.26), ((1, 1), (5, 5), (30, 30), (30, 1), (1, 30), (5, 30), (30, 5)), (1.0, 0.5, 2.0)):
        shape = (REPS, rounds); d = np.exp(rng.normal(0, 0.1, shape))
        nb = np.round(rng.uniform(0.7, 1.3, shape) * 20_000).astype(int); ns = np.round(rng.uniform(0.7, 1.3, shape) * 20_000).astype(int)
        cb = sim.betabin(nb, np.clip(share * d, 0, 0.95), pb); cs = sim.betabin(ns, np.clip(share * ratio * d, 0, 0.95), ps)
        lo, hi, est = interval(cb, nb, cs, ns, phi_pair(cb, nb, cs, ns))
        cover = ((lo <= ratio) & (ratio <= hi)).mean()
        b, w = verdicts(cb, cs, lo, hi, est, False); cl = (b | w).mean()
        worst = min(worst, cover); aa = max(aa, cl) if ratio == 1.0 else aa
        print(f"{rounds:5d} {share:5.3f} {ratio:4.1f} {pb:4d} {ps:4d} | {cover:.3f} | {cl:.4f}")
    print(f"worst coverage {worst:.3f}; A/A worst false clear {aa:.4f}")

def part3():
    print("\n== 3. phi borrowed from a third arm C (lam_C = 5000 a run) ==")
    print("A/A between baseline and candidate; false clear under the rule 'one side >= 20'")
    print("rounds lam phi_b phi_s phi_C | pair only | with C")
    for rounds, lam, (pb, ps), pc in itertools.product((5, 12), (5, 100), ((300, 1), (1, 300), (1, 1)), (1, 300)):
        d = drift_of(rounds, 0.3)
        cb, nb = counts(rounds, lam, 1.0, pb, 0.3, d); cs, ns = counts(rounds, lam, 1.0, ps, 0.3, d)
        cc, nc = counts(rounds, 5000, 1.0, pc, 0.3, d)
        out = []
        for extra in (None, (cc, nc)):
            lo, hi, est = interval(cb, nb, cs, ns, phi_pair(cb, nb, cs, ns, extra))
            b, w = verdicts(cb, cs, lo, hi, est, False); out.append((b | w).mean())
        print(f"{rounds:5d} {lam:5d} {pb:5d} {ps:5d} {pc:5d} | {out[0]:.4f} | {out[1]:.4f}")
    print("\na real tenfold difference, same clumping (phi 1 and 1): found without C, with a quiet C, with a clumpy C")
    for rounds, lam in itertools.product((5, 12), (5, 100, 5000)):
        d = drift_of(rounds, 0.3)
        cb, nb = counts(rounds, lam, 1.0, 1, 0.3, d); cs, ns = counts(rounds, lam, 0.1, 1, 0.3, d)
        row = []
        for pc in (None, 1, 300):
            extra = None if pc is None else counts(rounds, 5000, 1.0, pc, 0.3, d)
            lo, hi, est = interval(cb, nb, cs, ns, phi_pair(cb, nb, cs, ns, extra))
            row.append(verdicts(cb, cs, lo, hi, est, False)[0].mean())
        print(f"{rounds:5d} {lam:5d} | {row[0]:.3f} | {row[1]:.3f} | {row[2]:.3f}")

def part4():
    print("\n== 4. coverage of the interval among pairs with 20 events on both sides ==")
    print("(cases in which under 5% of the samples are such pairs are left out)")
    print("rounds lam phi_b phi_s ratio | scored | cover")
    found = {}
    for rounds, lam, (pb, ps), ratio in itertools.product((5, 12), (5, 100, 5000), PAIRS, (1.0, 0.1, 0.01, 3.0)):
        d = drift_of(rounds, 0.3)
        cb, nb = counts(rounds, lam, 1.0, pb, 0.3, d); cs, ns = counts(rounds, lam, ratio, ps, 0.3, d)
        lo, hi, est = interval(cb, nb, cs, ns, phi_pair(cb, nb, cs, ns))
        both = np.minimum(cb.sum(1), cs.sum(1)) >= MIN_EVENTS
        if both.mean() < 0.05: continue
        cover = ((lo <= ratio) & (ratio <= hi))[both].mean()
        print(f"{rounds:5d} {lam:5d} {pb:5d} {ps:5d} {ratio:5.2f} | {both.mean():.3f} | {cover:.3f}")
        found.setdefault(("same" if pb == ps else "different", rounds), []).append(cover)
    total = 0
    for (kind, rounds), v in sorted(found.items()):
        total += len(v)
        print(f"  {kind:9s} clumping, {rounds:2d} rounds: {sum(c >= 0.94 for c in v)} of {len(v)} cases at 0.94 or more, worst {min(v):.3f}")
    print(f"  cases in all: {total}")

if __name__ == "__main__":
    for p in sys.argv[1:] or ("1", "2", "3", "4"):
        {"1": part1, "2": part2, "3": part3, "4": part4}[p]()
