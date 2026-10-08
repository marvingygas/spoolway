#!/usr/bin/env python3
"""Append made-up ledger lines to the demo project's usage.jsonl, so the eval
tab has three weeks of runs, two versions of `impl`, one settled trial and a
few directory sessions to show.

Usage: fake-eval.py <project home>     e.g. ~/.spoolway/shop-ab12cd

The data is seeded, so every run of the script draws the same table. The
session ids point at no transcript, so `spoolway eval` has nothing to re-read
and leaves these lines exactly as written.
"""
import json
import random
import sys
import uuid
from datetime import datetime, timedelta, timezone
from pathlib import Path

home = Path(sys.argv[1]).expanduser()
repo = Path.home() / "demo" / "shop"
rng = random.Random(7)
now = datetime.now(timezone.utc)

HAIKU = "claude-haiku-5-5"
# USD per 1M tokens: input, output, cache read, cache write 5m, cache write 1h.
# The same rates setup.sh writes into the demo's config.toml.
PRICES = {HAIKU: (0.10, 0.50, 0.01, 0.125, 0.20)}

# Which agent and model each pipeline's agent steps run, in walk order. The
# `test` command step banks nothing, so it is left out.
STEPS = {
    "impl": [("implement", "claude", HAIKU), ("review", "claude", HAIKU), ("document", "claude", HAIKU)],
    "impl_fast": [("implement", "claude", HAIKU)],
    "impl_tdd": [("implement", "claude", HAIKU), ("review", "claude", HAIKU)],
}

# (task, group, pipeline, version, days ago). impl ran under 1.0 for the
# first half of the window and 1.1 since, so `by version` has two rows.
RUNS = [
    ("search-index", "search", "impl", "1.0", 20),
    ("search-typo", "search", "impl_fast", "1.0", 20),
    ("search-facets", "search", "impl", "1.0", 19),
    ("catalog-images", "catalog", "impl", "1.0", 18),
    ("catalog-sort", "catalog", "impl_fast", "1.0", 18),
    ("catalog-pagination", "catalog", "impl", "1.0", 17),
    ("audit-deps-1", "nightly", "impl_fast", "1.0", 17),
    ("audit-docs-1", "nightly", "impl_fast", "1.0", 17),
    ("login-rate-limit", "auth", "impl_tdd", "1.0", 16),
    ("login-lockout", "auth", "impl_tdd", "1.0", 15),
    ("password-reset", "auth", "impl", "1.0", 15),
    ("audit-deps-2", "nightly", "impl_fast", "1.0", 14),
    ("audit-docs-2", "nightly", "impl_fast", "1.0", 14),
    ("wishlist-add", "wishlist", "impl", "1.0", 13),
    ("wishlist-share", "wishlist", "impl", "1.0", 12),
    ("wishlist-empty-state", "wishlist", "impl_fast", "1.0", 12),
    ("bump-deps-1", "weekly-deps", "impl", "1.0", 11),
    ("orders-history", "orders", "impl", "1.1", 10),
    ("orders-cancel", "orders", "impl_tdd", "1.0", 9),
    ("orders-invoice-pdf", "orders", "impl", "1.1", 9),
    ("audit-deps-3", "nightly", "impl_fast", "1.0", 8),
    ("audit-docs-3", "nightly", "impl_fast", "1.0", 8),
    ("reviews-submit", "reviews", "impl", "1.1", 7),
    ("reviews-moderate", "reviews", "impl", "1.1", 7),
    ("reviews-stars", "reviews", "impl_fast", "1.0", 6),
    ("coupons-expiry", "coupons", "impl_tdd", "1.0", 6),
    ("coupons-single-use", "coupons", "impl_tdd", "1.0", 5),
    ("bump-deps-2", "weekly-deps", "impl", "1.1", 4),
    ("audit-deps-4", "nightly", "impl_fast", "1.0", 3),
    ("audit-docs-4", "nightly", "impl_fast", "1.0", 3),
    ("stock-badges", "inventory", "impl", "1.1", 2),
    ("stock-reserve", "inventory", "impl_tdd", "1.0", 2),
    ("stock-restock-mail", "inventory", "impl", "1.1", 1),
    ("audit-deps-5", "nightly", "impl_fast", "1.0", 1),
    ("audit-docs-5", "nightly", "impl_fast", "1.0", 1),
]

# One settled trial: the `shipping` group forked across impl and impl_tdd.
TRIAL = ("shipping", 4, [("shipping-rates", "shipping-quote")])


def tokens_for(step):
    """A plausible Haiku lane's token counts and peak context."""
    heavy = step == "implement"
    out = rng.randint(6_000, 24_000) if heavy else rng.randint(2_500, 12_000)
    cache_read = rng.randint(600_000, 2_800_000) if heavy else rng.randint(250_000, 1_500_000)
    ctx = rng.randint(55_000, 150_000) if heavy else rng.randint(35_000, 95_000)
    return {"input": rng.randint(20, 120), "output": out, "cache_read": cache_read,
            "cache_write_5m": 0, "cache_write_1h": rng.randint(25_000, 90_000), "reasoning": 0}, ctx


def cost(model, t):
    if model not in PRICES:
        return 0.0
    i, o, cr, cw5, cw1 = PRICES[model]
    return round((t["input"] * i + t["output"] * o + t["cache_read"] * cr
                  + t["cache_write_5m"] * cw5 + t["cache_write_1h"] * cw1) / 1e6, 6)


def lane(ts, task, group, step, pipeline, version, agent, model, rnd, outcome, run, **extra):
    t, ctx = tokens_for(step)
    turns = max(3, t["output"] // 900)
    line = {
        "ts": ts.isoformat(), "task": task, "plan": group, "step": step,
        "pipeline": pipeline, "agent": agent, "kind": agent, "model": model,
        "session": str(uuid.UUID(int=rng.getrandbits(128), version=4)), "round": rnd,
        "wall_s": rng.randint(120, 600) if step == "implement" else rng.randint(60, 300),
        "turns": turns, "tokens": t, "cost_usd": cost(model, t), "ctx_peak": ctx,
        "pipeline_version": version, "outcome": outcome, "run": run,
    }
    if outcome == "block":
        line["blocked"] = True
    line.update(extra)
    return line


def walk(task, group, pipeline, version, start, extra=None):
    """Every lane one run banks, with a review failure or a block now and then."""
    extra = extra or {}
    run = "r%016x" % rng.getrandbits(64)
    # impl 1.1 is the better version: fewer review failures.
    fail_rate = {"impl": 0.35 if version == "1.0" else 0.12, "impl_tdd": 0.2}.get(pipeline, 0.0)
    block_rate = {"impl_fast": 0.15, "impl": 0.05 if version == "1.0" else 0.0}.get(pipeline, 0.03)
    ts, out, rounds = start, [], {}
    steps = STEPS[pipeline]
    i = 0
    while i < len(steps):
        step, agent, model = steps[i]
        rounds[step] = rounds.get(step, 0) + 1
        ts += timedelta(minutes=rng.randint(4, 18))
        if step == "implement" and rng.random() < block_rate:
            out.append(lane(ts, task, group, step, pipeline, version, agent, model,
                            rounds[step], "block", run, **extra))
            return out
        if step == "review" and rounds[step] == 1 and rng.random() < fail_rate:
            out.append(lane(ts, task, group, step, pipeline, version, agent, model,
                            1, "fail", run, **extra))
            i = 0
            continue
        out.append(lane(ts, task, group, step, pipeline, version, agent, model,
                        rounds[step], "pass", run, **extra))
        i += 1
    return out


lines = []
for task, group, pipeline, version, days in RUNS:
    start = (now - timedelta(days=days)).replace(hour=rng.randint(7, 20), minute=rng.randint(0, 59))
    lines += walk(task, group, pipeline, version, start)

trial_group, days, chain = TRIAL
trial = "t%016x" % rng.getrandbits(64)
start = (now - timedelta(days=days)).replace(hour=10, minute=12)
for pipeline in ("impl", "impl_tdd"):
    arm_group = f"{trial_group}-{pipeline}"
    for base in chain[0]:
        task = f"{base}-{1 if pipeline == 'impl' else 2}"
        version = "1.1" if pipeline == "impl" else "1.0"
        lines += walk(task, arm_group, pipeline, version, start,
                      {"trial": trial, "trial_group": trial_group})

# Sessions a person ran by hand in the checkout: the directory table.
for days in (12, 6, 2):
    t, ctx = tokens_for("implement")
    ts = (now - timedelta(days=days)).replace(hour=rng.randint(8, 18))
    lines.append({
        "ts": ts.isoformat(), "pipeline": "", "agent": "", "kind": "claude", "model": HAIKU,
        "session": str(uuid.UUID(int=rng.getrandbits(128), version=4)), "round": 0,
        "wall_s": rng.randint(900, 3600), "turns": t["output"] // 900, "tokens": t, "cost_usd": cost(HAIKU, t),
        "ctx_peak": ctx, "pipeline_version": "", "dir": str(repo),
    })

lines.sort(key=lambda l: l["ts"])
home.mkdir(parents=True, exist_ok=True)
with open(home / "usage.jsonl", "a") as f:
    for line in lines:
        f.write(json.dumps(line) + "\n")
print(f"appended {len(lines)} ledger lines to {home / 'usage.jsonl'}")
