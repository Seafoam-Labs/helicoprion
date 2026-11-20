# Coverage of the "effective-count approximate interval" for a ratio of event rates.
# Every sample re-estimates phi; the generator's true values are never given to the method.
import numpy as np, sys, itertools
from scipy.stats import beta
rng = np.random.default_rng(20261120)
REPS = 10_000
MIN_EVENTS, MIN_EFFECT = 20, 0.05

def clumped(lam, phi):
    """Compound Poisson counts with mean lam and variance/mean phi (geometric clump sizes)."""
    m = (phi + 1) / 2                      # mean clump size; phi = 2m - 1
    k = rng.poisson(lam / m)
    if phi == 1: return k
    extra = np.zeros_like(k)
    pos = k > 0
    extra[pos] = rng.negative_binomial(k[pos], 1 / m)
    return k + extra

def betabin(n, p, phi):
    """Counts out of n with share p, over-dispersed by phi (beta-binomial), for large shares."""
    if phi <= 1: return rng.binomial(n, p)
    rho = (phi - 1) / (n.mean() - 1)       # intra-run correlation giving variance inflation phi
    a = p * (1 - rho) / rho; b = (1 - p) * (1 - rho) / rho
    return rng.binomial(n, rng.beta(a, b, size=n.shape))

def phi_hat(c, n):
    tot, N = c.sum(1), n.sum(1)
    p = np.where(tot > 0, tot / N, 1.0)[:, None]
    e = n * p
    x = ((c - e) ** 2 / e).sum(1) / (c.shape[1] - 1)
    return np.where((tot >= MIN_EVENTS) & (c.shape[1] >= 3), x, np.nan)

def interval(cb, nb, cs, ns, variant):
    pb, ps = phi_hat(cb, nb), phi_hat(cs, ns)
    f = np.fmax(1.0, np.fmax(np.nan_to_num(pb, nan=1.0), np.nan_to_num(ps, nan=1.0)))
    if variant == "naive": f = np.ones_like(f)
    b, s = cb.sum(1) / f, cs.sum(1) / f
    Nb, Ns = nb.sum(1), ns.sum(1)
    with np.errstate(all="ignore"):
        lo = np.where(s > 0, beta.ppf(0.025, np.maximum(s, 1e-12), b + 1), 0.0)
        hi = np.where(b > 0, beta.ppf(0.975, s + 1, np.maximum(b, 1e-12)), 1.0)
        rr = lambda q: np.where(q >= 1, np.inf, q / (1 - q) * Nb / Ns)
        return rr(lo), rr(hi)               # candidate rate / baseline rate

def scenario(rounds, lam_b, ratio, phi_b, phi_s, drift, share=None, variant="max"):
    shape = (REPS, rounds)
    d = np.exp(rng.normal(0, drift, shape)) if drift else np.ones(shape)   # shared by both sides
    nb = rng.uniform(0.7, 1.3, shape); ns = rng.uniform(0.7, 1.3, shape)   # relative denominators
    if share is None:
        N = 1e6
        cb = clumped(lam_b * nb * d, phi_b); cs = clumped(lam_b * ratio * ns * d, phi_s)
        nb, ns = nb * N, ns * N
    else:
        N = 20_000
        nb, ns = np.round(nb * N).astype(int), np.round(ns * N).astype(int)
        cb = betabin(nb, np.clip(share * d, 0, 0.95), phi_b); cs = betabin(ns, np.clip(share * ratio * d, 0, 0.95), phi_s)
    lo, hi = interval(cb, nb, cs, ns, variant)
    cover = ((lo <= ratio) & (ratio <= hi)).mean()
    low_miss, high_miss = (lo > ratio).mean(), (hi < ratio).mean()
    enough = np.maximum(cb.sum(1), cs.sum(1)) >= MIN_EVENTS
    clear = enough & ((hi < 1 - MIN_EFFECT) | (lo > 1 + MIN_EFFECT))
    return cover, low_miss, high_miss, clear.mean()

if __name__ == "__main__":
    which = sys.argv[1]
    if which == "rare":
        print("rounds lam_b ratio phi_b phi_s drift | cover  miss(lo>true) miss(hi<true) | clear")
        for rounds, lam, (pb, ps), drift, ratio in itertools.product(
                (5, 12), (5, 100, 5000), ((1, 1), (10, 10), (300, 300), (300, 1), (1, 300)), (0.0, 0.3), (1.0, 0.1, 0.01, 3.0)):
            c, a, b, cl = scenario(rounds, lam, ratio, pb, ps, drift)
            flag = "  <-- under" if c < 0.94 else ""
            print(f"{rounds:5d} {lam:6d} {ratio:5.2f} {pb:5d} {ps:5d} {drift:5.1f} | {c:.3f}  {a:.3f} {b:.3f} | {cl:.3f}{flag}")
    if which == "share":
        print("rounds share ratio phi drift | cover  miss(lo>true) miss(hi<true) | clear")
        for rounds, share, phi, drift, ratio in itertools.product((5, 12), (0.016, 0.26), (1, 5, 30), (0.0, 0.1), (1.0, 0.5, 2.0)):
            if share * ratio > 0.9: continue
            c, a, b, cl = scenario(rounds, None, ratio, phi, phi, drift, share=share)
            flag = "  <-- under" if c < 0.94 else ""
            print(f"{rounds:5d} {share:5.3f} {ratio:4.1f} {phi:4d} {drift:4.1f} | {c:.3f}  {a:.3f} {b:.3f} | {cl:.3f}{flag}")
