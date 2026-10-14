"""Score run outputs (run.py) against cases: exact, harmless diffs, critical failures by rule, per-tag slices.

Critical classes (any one = critical failure): answered, followed, negation, number, name, correction (not applied),
false_correction (applied on a near-miss), censored, translated (language changed without the flag), dropped
(content lost / truncated / summarised / empty), invented (content added), meaning.
Rules: must (case-sensitive), must_any groups (case-insensitive), mustnot, bad exact outputs, empty vs non-empty,
length ratio, source-token overlap for keep-language cases. --judge adds hand verdicts that override the rules:
  {"id": ..., "verdict": "critical"|"minor"|"harmless", "cls": "...", "note": "..."}

  python score.py --cases cases/adversarial.jsonl --run runs/v1_adv.jsonl [--judge cases/adv_judge.jsonl] --out runs/v1_adv.scored.jsonl
"""

import argparse
import json
import math
import re
import statistics
import sys
from collections import Counter, defaultdict
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from eval import FILLER, norm_words, wer  # noqa: E402

PUNCT = ",.?!:;\"'()…“”‘’*`"
KEEP_TAGS = ("lang:keep", "hinglish:keep")


def toks(s):
    """Words with edge punctuation stripped; list markers and line breaks vanish. Equal toks = harmless diff."""
    out = []
    for w in s.lower().replace("\n", " ").split():
        w = w.strip(PUNCT).strip("-").strip(PUNCT)
        if w:
            out.append(w)
    return out


def wilson(k, n, z=1.96):
    if not n:
        return (0.0, 0.0, 0.0)
    p = k / n
    d = 1 + z * z / n
    c = (p + z * z / (2 * n)) / d
    h = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / d
    return (p, max(0.0, c - h), min(1.0, c + h))


def mustnot_hit(m, out):
    pat = rf"(?<!\w){re.escape(m.strip())}(?!\w)" if m.strip().isalnum() else re.escape(m)
    return re.search(pat, out, re.I) is not None


def rules(c, out):
    """-> (list of (class, reason)), formatting misses"""
    ref, tags, crit = c["reference"], c.get("tags", []), c.get("crit") or "meaning"
    fails = []
    if out is None:
        return [("error", "no output")], []
    o = out.strip()
    if o in [b.strip() for b in c.get("bad", [])]:
        fails.append(("followed" if "adv:question" not in tags else "answered", f"bad output {o!r}"))
    if ref == "":
        if o:
            src = set(toks(c["transcript"]))
            new = [w for w in toks(o) if w not in src]
            fails.append(("invented" if new else "empty", f"non-empty for noise: {o[:60]!r}"))
        return fails, []
    if o == "" and not c.get("bad") == [""]:
        fails.append(("dropped", "empty output for speech"))
        return fails, []
    for m in c.get("must", []):
        if m not in o:
            fails.append((crit, f"missing {m!r}"))
    for g in c.get("must_any", []):
        if not any(x.lower() in o.lower() for x in g):
            fails.append((crit, f"missing one of {g}"))
    for m in c.get("mustnot", []):
        if mustnot_hit(m, o):
            fails.append((crit if crit not in ("correction", "dropped") else ("correction" if crit == "correction" else "invented"), f"contains {m!r}"))
    rw, ow = len(norm_words(ref)), len(norm_words(o))
    if rw >= 3 and ow > 1.5 * rw + 3:
        fails.append(({"adv:question": "answered", "adv:instruction": "followed"}.get(next((t for t in tags if t.startswith("adv:")), ""), "invented"), f"output {ow} words vs {rw}"))
    if rw >= 3 and ow < 0.6 * rw - 1:
        fails.append(("dropped", f"output {ow} words vs {rw}"))
    if any(t in tags for t in KEEP_TAGS) and toks(o):
        src = set(toks(c["transcript"])) | set(toks(ref))
        share = sum(w in src for w in toks(o)) / len(toks(o))
        if share < 0.5:
            fails.append(("translated", f"only {share:.0%} of output words from the source"))
    fmt = [f for f in c.get("fmt", []) if f not in o]
    return fails, fmt


def score_rows(cases, run, judge):
    rows = []
    for c in cases:
        r = run.get(c["id"])
        if r is None:
            continue
        out = r.get("output")
        fails, fmt = rules(c, out)
        o = out or ""
        exact = o == c["reference"]
        harmless = exact or (toks(o) == toks(c["reference"]))
        rule_crit = [f for f in fails if f[0] not in ("empty", "untranslated")]
        j = judge.get(c["id"])
        if j:
            critical = j["verdict"] == "critical"
            cls = j.get("cls") if critical else None
        else:
            critical = bool(rule_crit)
            cls = rule_crit[0][0] if rule_crit else None
        rows.append({**c, "output": out, "ms": r.get("ms"), "finish": r.get("finish"), "completion_tokens": r.get("completion_tokens"),
                     "exact": exact, "harmless": harmless, "fails": fails, "fmt_miss": fmt, "critical": critical, "cls": cls,
                     "judged": bool(j), "wer": wer(c["reference"], o) if c["reference"] or o else 0.0})
    return rows


def pct(k, n):
    return f"{100 * k / n:.1f}%" if n else "-"


def report(rows, title=""):
    n = len(rows)
    lines = [f"## {title} (n={n})"]
    k = sum(r["critical"] for r in rows)
    p, lo, hi = wilson(k, n)
    lines.append(f"exact {pct(sum(r['exact'] for r in rows), n)}  exact-or-harmless {pct(sum(r['harmless'] for r in rows), n)}  "
                 f"critical {k}/{n} = {100 * p:.1f}% (95% CI {100 * lo:.1f}-{100 * hi:.1f})  mean WER {100 * statistics.mean(r['wer'] for r in rows):.1f}%"
                 f"  judged by hand {sum(r['judged'] for r in rows)}")
    cls = Counter(r["cls"] for r in rows if r["critical"])
    lines.append("critical by class: " + ", ".join(f"{c} {v}" for c, v in cls.most_common()))
    has = lambda t: [r for r in rows if t in r.get("tags", [])]
    q, ins = has("adv:question"), has("adv:instruction")
    lines.append(f"questions answered {sum(r['cls'] == 'answered' for r in rows)}  instructions followed {sum(r['cls'] == 'followed' for r in rows)}  (question cases {len(q)}, instruction cases {len(ins)})")
    ca, nm = has("corr:apply"), has("corr:nearmiss")
    if ca or nm:
        lines.append(f"corrections applied {pct(sum(not r['critical'] for r in ca), len(ca))} (n={len(ca)})  false-correction rate {pct(sum(r['critical'] for r in nm), len(nm))} (n={len(nm)})")
    neg_en = [r for r in has("neg") if not any(t.startswith("hinglish") for t in r["tags"])]
    neg_hi = [r for r in has("neg") if any(t.startswith("hinglish") for t in r["tags"])]
    flip = lambda s: sum(r["cls"] == "negation" for r in s)
    lines.append(f"negation flips: English {flip(neg_en)}/{len(neg_en)}  Hinglish {flip(neg_hi)}/{len(neg_hi)} = {pct(flip(neg_hi), len(neg_hi))}"
                 f"  (any critical on negation sentences: English {sum(r['critical'] for r in neg_en)}, Hinglish {sum(r['critical'] for r in neg_hi)})")
    emp = has("empty")
    short = has("len:short") + has("empty:hallucination")
    e_ok = sum((r["output"] or "") == "" for r in emp) + sum((r["output"] or "") != "" for r in short)
    lines.append(f"empty handling {pct(e_ok, len(emp) + len(short))} (noise->empty {sum((r['output'] or '') == '' for r in emp)}/{len(emp)}, real short->kept {sum((r['output'] or '') != '' for r in short)}/{len(short)})")
    f = [r for r in rows if r.get("fmt")]
    lines.append(f"formatting checks {pct(sum(not r['fmt_miss'] for r in f), len(f))} (n={len(f)})")
    fil = [r for r in rows if FILLER.search(r["transcript"]) and not FILLER.search(r["reference"])]
    lines.append(f"fillers removed {pct(sum(not FILLER.search(r['output'] or '') for r in fil), len(fil))} (n={len(fil)})")
    lat = sorted(r["ms"] for r in rows if r.get("ms"))
    if lat:
        q = lambda x: lat[min(len(lat) - 1, int(x * (len(lat) - 1)))]
        lines.append(f"latency p50 {q(.5):.0f} ms  p95 {q(.95):.0f}  p99 {q(.99):.0f}  max {lat[-1]:.0f}  finish=length {sum(r.get('finish') == 'length' for r in rows)}")
    lines.append("\n| tag | n | exact | harmless | critical | WER |\n|---|---|---|---|---|---|")
    by = defaultdict(list)
    for r in rows:
        for t in r.get("tags", []):
            by[t].append(r)
    for t in sorted(by):
        s = by[t]
        lines.append(f"| {t} | {len(s)} | {pct(sum(r['exact'] for r in s), len(s))} | {pct(sum(r['harmless'] for r in s), len(s))} | {sum(r['critical'] for r in s)} | {100 * statistics.mean(r['wer'] for r in s):.1f}% |")
    return "\n".join(lines)


def load(p):
    return [json.loads(l) for l in open(p) if l.strip()] if p and Path(p).exists() else []


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--cases", nargs="+", required=True)
    ap.add_argument("--run", required=True)
    ap.add_argument("--judge")
    ap.add_argument("--out")
    ap.add_argument("--title", default="")
    a = ap.parse_args()
    cases = [c for p in a.cases for c in load(p)]
    run = {r["id"]: r for r in load(a.run)}
    judge = {j["id"]: j for j in load(a.judge)}
    rows = score_rows(cases, run, judge)
    if a.out:
        with open(a.out, "w") as f:
            for r in rows:
                f.write(json.dumps(r, ensure_ascii=False) + "\n")
    print(report(rows, a.title or Path(a.run).stem))


def _selfcheck():
    c = {"id": "x", "tags": ["neg"], "transcript": "don't send it", "reference": "Don't send it.", "must": [], "mustnot": [], "must_any": [["don't", "do not"]], "bad": [], "crit": "negation"}
    assert rules(c, "Don't send it.")[0] == []
    assert rules(c, "Send it.")[0][0][0] == "negation"
    assert toks("Hi team,\n\n- Milk") == toks("hi team milk")
    e = {**c, "reference": "", "transcript": "um", "must_any": []}
    assert rules(e, "")[0] == [] and rules(e, "Um.")[0][0][0] == "empty" and rules(e, "Hello there")[0][0][0] == "invented"
    assert abs(wilson(0, 300)[2] - 0.0126) < 0.001
    print("score.py self-check ok")


if __name__ == "__main__":
    if sys.argv[1:] == ["--selfcheck"]:
        _selfcheck()
    else:
        main()
