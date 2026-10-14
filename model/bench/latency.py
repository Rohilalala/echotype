"""Warm latency by input length, tokens/s, server memory (phys_footprint and its peak), and determinism.

Same request as the app (run.py). Buckets by transcript words; --per cases each, after --warmup requests.
Memory comes from `footprint -f bytes -p <server pid>` (phys_footprint_peak covers MLX's Metal buffers).

  python latency.py --url http://127.0.0.1:8899/v1 --pid 1234 --cases a.jsonl b.jsonl --label alone --out runs/latency.jsonl
  python latency.py ... --determinism 50 --label det          # the same 50 requests twice, outputs compared
"""

import argparse
import json
import random
import re
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from gen import system_prompt, user_message  # noqa: E402
from run import call  # noqa: E402

BUCKETS = [(1, 10), (11, 30), (31, 60), (61, 120), (121, 250), (251, 700)]


def footprint(pid):
    out = subprocess.run(["footprint", "-f", "bytes", "-p", str(pid)], capture_output=True, text=True).stdout
    get = lambda k: int(re.search(rf"{k}: (\d+) B", out).group(1)) / 2**30 if re.search(rf"{k}: (\d+) B", out) else None
    return get("phys_footprint"), get("phys_footprint_peak")


def body(c, model, reasoning_off):
    b = {"model": model, "temperature": 0, "stream": False,
         "messages": [{"role": "system", "content": system_prompt(c.get("dictionary", []), c.get("translate", False))},
                      {"role": "user", "content": user_message(c["transcript"])}]}
    if reasoning_off:
        b["reasoning_effort"] = "none"
    return b


def q(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(p * (len(xs) - 1)))] if xs else None


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--url", required=True)
    ap.add_argument("--model", default="default_model")
    ap.add_argument("--pid", type=int)
    ap.add_argument("--cases", nargs="+", required=True)
    ap.add_argument("--per", type=int, default=40)
    ap.add_argument("--warmup", type=int, default=5)
    ap.add_argument("--label", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--reasoning-off", action="store_true")
    ap.add_argument("--determinism", type=int, default=0)
    ap.add_argument("--seed", type=int, default=0)
    a = ap.parse_args()
    rng = random.Random(a.seed)
    cases = [json.loads(l) for p in a.cases for l in open(p) if l.strip()]
    cases = [c for c in cases if c.get("transcript", "").strip()]
    rng.shuffle(cases)
    res = {"label": a.label}
    if a.determinism:
        picked = cases[: a.determinism]
        runs = [[call(a.url, body(c, a.model, a.reasoning_off), 120)[0] for c in picked] for _ in range(2)]
        diff = [(c["id"], x, y) for c, x, y in zip(picked, *runs) if x != y]
        res.update(determinism_n=len(picked), determinism_identical=len(picked) - len(diff), determinism_diffs=diff[:10])
        print(f"determinism: {len(picked) - len(diff)}/{len(picked)} identical", flush=True)
    else:
        for c in cases[: a.warmup]:
            call(a.url, body(c, a.model, a.reasoning_off), 120)
        rows = []
        for lo, hi in BUCKETS:
            sel = [c for c in cases if lo <= len(c["transcript"].split()) <= hi][: a.per]
            for c in sel:
                out, ms, usage, fin = call(a.url, body(c, a.model, a.reasoning_off), 300)
                rows.append({"bucket": f"{lo}-{hi}", "words": len(c["transcript"].split()), "ms": ms, "ctok": usage.get("completion_tokens"), "ptok": usage.get("prompt_tokens"), "finish": fin})
        res["buckets"] = {}
        for lo, hi in BUCKETS:
            s = [r for r in rows if r["bucket"] == f"{lo}-{hi}"]
            if s:
                ms = [r["ms"] for r in s]
                tps = [r["ctok"] / (r["ms"] / 1000) for r in s if r["ctok"]]
                res["buckets"][f"{lo}-{hi}"] = {"n": len(s), "p50": q(ms, .5), "p95": q(ms, .95), "p99": q(ms, .99), "max": max(ms), "tok_s": q(tps, .5)}
        le60 = [r["ms"] for r in rows if r["words"] <= 60]
        res["le60"] = {"n": len(le60), "p50": q(le60, .5), "p95": q(le60, .95), "p99": q(le60, .99)}
        for k, v in res["buckets"].items():
            print(f"  {a.label} {k:>8} words n={v['n']:3d} p50 {v['p50']:6.0f} p95 {v['p95']:6.0f} p99 {v['p99']:6.0f} ms  {v['tok_s'] or 0:5.0f} tok/s", flush=True)
        print(f"  {a.label} <=60 words: p50 {res['le60']['p50']:.0f} p95 {res['le60']['p95']:.0f} p99 {res['le60']['p99']:.0f} ms", flush=True)
    if a.pid:
        res["footprint_gb"], res["footprint_peak_gb"] = footprint(a.pid)
        print(f"  server footprint {res['footprint_gb']:.2f} GB, peak {res['footprint_peak_gb']:.2f} GB", flush=True)
    with open(a.out, "a") as f:
        f.write(json.dumps(res, ensure_ascii=False) + "\n")


if __name__ == "__main__":
    main()
