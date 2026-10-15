"""Stratified random sample of real-speech outputs for hand review, and the critical rate (Wilson 95% CI) from the verdicts.

  python sample_review.py pick  --scored runs/v1_mlx_real.scored.jsonl --per 30 --out cases/review_real.jsonl
  python sample_review.py rate  --review cases/review_real.jsonl --judge cases/review_real_judge.jsonl
Verdicts: {"id", "verdict": "critical"|"minor"|"ok", "cls", "note"}; judged against the raw whisper text the model saw
(cleaning must keep its meaning) with the human reference for context.
"""

import argparse
import json
import random
from collections import defaultdict

from score import wilson


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("cmd", choices=["pick", "rate"])
    ap.add_argument("--scored", nargs="+")
    ap.add_argument("--per", type=int, default=30)
    ap.add_argument("--out")
    ap.add_argument("--review")
    ap.add_argument("--judge")
    ap.add_argument("--seed", type=int, default=1)
    a = ap.parse_args()
    if a.cmd == "pick":
        rng, by = random.Random(a.seed), defaultdict(list)
        for p in a.scored:
            for l in open(p):
                r = json.loads(l)
                if r["raw"].strip():
                    by[r["source"]].append(r)
        pick = []
        for s in sorted(by):
            pick += rng.sample(by[s], min(a.per, len(by[s])))
        with open(a.out, "w") as f:
            for r in pick:
                f.write(json.dumps({k: r[k] for k in ("id", "source", "reference", "raw", "output", "flags")}, ensure_ascii=False) + "\n")
        print(f"{len(pick)} clips from {len(by)} sources -> {a.out}")
    else:
        rows = [json.loads(l) for l in open(a.review)]
        j = {x["id"]: x for x in (json.loads(l) for l in open(a.judge))}
        n, k = sum(r["id"] in j for r in rows), sum(j.get(r["id"], {}).get("verdict") == "critical" for r in rows)
        p, lo, hi = wilson(k, n)
        print(f"hand-judged {n}/{len(rows)}: critical {k} = {100 * p:.1f}% (95% CI {100 * lo:.1f}-{100 * hi:.1f})")
        for s in sorted({r["source"] for r in rows}):
            sub = [r for r in rows if r["source"] == s and r["id"] in j]
            print(f"  {s:20s} {sum(j[r['id']]['verdict'] == 'critical' for r in sub)}/{len(sub)} critical, {sum(j[r['id']]['verdict'] == 'minor' for r in sub)} minor")


if __name__ == "__main__":
    main()
