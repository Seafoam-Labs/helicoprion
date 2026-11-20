# Variants compared on the same samples: effective counts (max phi), the same with a
# small-sample allowance for phi, independent events (phi = 1), and a paired bootstrap
# of rounds on the pooled ratio. phi is re-estimated in every sample.
import numpy as np, sys, itertools
from scipy.stats import beta, chi2
import os; sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import sim
rng = sim.rng; REPS = sim.REPS; MIN_EVENTS = sim.MIN_EVENTS; MIN_EFFECT = sim.MIN_EFFECT

def eff(cb, nb, cs, ns, kind):
    pb, ps = sim.phi_hat(cb, nb), sim.phi_hat(cs, ns)
    f = np.fmax(1.0, np.fmax(np.nan_to_num(pb, nan=1.0), np.nan_to_num(ps, nan=1.0)))
    if kind == "naive": f = np.ones_like(f)
    if kind == "allow":                       # phi could be this large with 20% probability, given df
        df = cb.shape[1] - 1
        f = np.maximum(1.0, f * df / chi2.ppf(0.2, df))
    b, s = cb.sum(1) / f, cs.sum(1) / f
    Nb, Ns = nb.sum(1), ns.sum(1)
    with np.errstate(all="ignore"):
        lo = np.where(s > 0, beta.ppf(0.025, np.maximum(s, 1e-12), b + 1), 0.0)
        hi = np.where(b > 0, beta.ppf(0.975, s + 1, np.maximum(b, 1e-12)), 1.0)
    rr = lambda q: np.where(q >= 1, np.inf, q / np.maximum(1 - q, 1e-300) * Nb / Ns)
    return rr(lo), rr(hi)

def boot(cb, nb, cs, ns, B=200):
    reps, rounds = cb.shape
    lo = np.empty(reps); hi = np.empty(reps)
    for a in range(0, reps, 2000):
        sl = slice(a, a + 2000)
        idx = rng.integers(0, rounds, (cb[sl].shape[0], B, rounds))
        take = lambda x: np.take_along_axis(x[sl][:, None, :].repeat(B, 1), idx, 2).sum(2)
        b, s, Nb, Ns = take(cb), take(cs), take(nb), take(ns)
        with np.errstate(all="ignore"):
            r = np.where(b > 0, (s / Ns) / (b / Nb), np.where(s > 0, np.inf, np.nan))
        lo[sl] = np.nanquantile(r, 0.025, axis=1); hi[sl] = np.nanquantile(r, 0.975, axis=1)
    return lo, hi

def run(rounds, lam, ratio, pb, ps, drift):
    shape = (REPS, rounds)
    d = np.exp(rng.normal(0, drift, shape)) if drift else np.ones(shape)
    nb = rng.uniform(0.7, 1.3, shape); ns = rng.uniform(0.7, 1.3, shape)
    cb = sim.clumped(lam * nb * d, pb); cs = sim.clumped(lam * ratio * ns * d, ps)
    nb, ns = nb * 1e6, ns * 1e6
    out = {}
    enough = np.maximum(cb.sum(1), cs.sum(1)) >= MIN_EVENTS
    for kind in ("max", "allow", "naive", "boot"):
        with np.errstate(all="ignore"):
            lo, hi = boot(cb, nb, cs, ns) if kind == "boot" else eff(cb, nb, cs, ns, kind)
            cover = ((lo <= ratio) & (ratio <= hi)).mean()
            clear = (enough & ((hi < 1 - MIN_EFFECT) | (lo > 1 + MIN_EFFECT))).mean()
        out[kind] = (cover, clear)
    return out

rows = []
for rounds, lam, (pb, ps), drift, ratio in itertools.product(
        (5, 12), (5, 100, 5000), ((1, 1), (10, 10), (300, 300), (300, 1), (1, 300)), (0.0, 0.3), (1.0, 0.1, 0.01, 3.0)):
    rows.append(((rounds, lam, pb, ps, drift, ratio), run(rounds, lam, ratio, pb, ps, drift)))
kinds = ("max", "allow", "naive", "boot")
print("rounds lam phi_b phi_s drift ratio | coverage: " + " ".join(f"{k:>6s}" for k in kinds) + " | clear: " + " ".join(f"{k:>6s}" for k in kinds))
for (rounds, lam, pb, ps, drift, ratio), o in rows:
    print(f"{rounds:5d} {lam:5d} {pb:5d} {ps:5d} {drift:4.1f} {ratio:5.2f} | " + " ".join(f"{o[k][0]:6.3f}" for k in kinds) + " | " + " ".join(f"{o[k][1]:6.3f}" for k in kinds))
for name, pick in (("same clumping on both sides", lambda p: p[2] == p[3]), ("different clumping", lambda p: p[2] != p[3])):
    for rounds in (5, 12):
        sel = [(p, o) for p, o in rows if pick(p) and p[0] == rounds]
        print(f"\n{name}, {rounds} rounds: {len(sel)} scenarios")
        for k in kinds:
            cov = np.array([o[k][0] for _, o in sel]); aa = np.array([o[k][1] for p, o in sel if p[5] == 1.0])
            print(f"  {k:6s} coverage>=0.94 in {int((cov >= 0.94).sum()):2d}/{len(sel)}  worst {cov.min():.3f}  median {np.median(cov):.3f} | A/A clear: worst {aa.max():.3f} median {np.median(aa):.3f}")
