"""Score cleanup on real speech: raw whisper vs cleaned output against human references.

Words (all clips): WER after the Open ASR Leaderboard normalizers (whisper EnglishTextNormalizer for English,
BasicTextNormalizer otherwise), corpus-level per slice, with a paired bootstrap 95% CI of cleaned - raw.
Written form (punctuated, cased references): punctuation F1 (. , ?) and first-letter casing on aligned words,
number tokens kept, exact match. Fillers left (um/uh), repeated-word false starts. Rule flags for the hand
review: cleaned much shorter / longer than raw, empty, language switched (langid).

  python asr_score.py --cases cases/asr-real.jsonl --run runs/v1_real.jsonl --out runs/v1_real.scored.jsonl
"""

import argparse
import difflib
import json
import random
import re
import statistics
import sys
from collections import defaultdict
from pathlib import Path

import jiwer
import langid
from whisper_normalizer.basic import BasicTextNormalizer
from whisper_normalizer.english import EnglishTextNormalizer

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from eval import FILLER  # noqa: E402

EN, BASIC = EnglishTextNormalizer(), BasicTextNormalizer()
MARKS = ".,?"
REPEAT = re.compile(r"\b(\w+)( \1\b)+", re.I)


def norm(s, lang):
    return (EN if lang == "en" else BASIC)(s or "").strip()


def errors(ref, hyp):
    """(word errors, ref words) after normalization; empty ref counts hyp words as insertions."""
    r, h = ref.split(), hyp.split()
    if not r:
        return len(h), 0
    o = jiwer.process_words(ref, hyp if h else "")
    return o.substitutions + o.deletions + o.insertions, len(r)


def words_marks(s):
    """[(word lowercased, following mark or '', starts with capital)] for written-form alignment."""
    out = []
    for t in (s or "").split():
        core = t.strip("\"'()“”‘’*-")
        mark = core[-1] if core and core[-1] in MARKS else ""
        w = core.rstrip(".,?!:;")
        if w:
            out.append((w.lower(), mark, w[:1].isupper()))
    return out


def written(ref, hyp):
    """Aligned punctuation counts per mark, casing matches, over equal word runs."""
    R, H = words_marks(ref), words_marks(hyp)
    sm = difflib.SequenceMatcher(a=[w for w, _, _ in R], b=[w for w, _, _ in H], autojunk=False)
    c = {m: [0, 0, 0] for m in MARKS}  # tp, fp, fn
    case_ok = case_n = 0
    for blk in sm.get_matching_blocks():
        for k in range(blk.size):
            (_, rm, rc), (_, hm, hc) = R[blk.a + k], H[blk.b + k]
            for m in MARKS:
                c[m][0] += rm == m and hm == m
                c[m][1] += hm == m and rm != m
                c[m][2] += rm == m and hm != m
            case_n += 1
            case_ok += rc == hc
    return c, case_ok, case_n


def nums(s):
    return [re.sub(r"[,.]$", "", t) for t in re.findall(r"[$€£₹]?\d[\d,.:]*%?", s or "")]


def f1(tp, fp, fn):
    return 2 * tp / (2 * tp + fp + fn) if tp else 0.0


def boot(rows, key_r, key_h, n=1000, seed=0):
    """Paired bootstrap 95% CI of corpus WER(hyp) - WER(raw) over clips."""
    rng = random.Random(seed)
    diffs = []
    for _ in range(n):
        s = [rows[rng.randrange(len(rows))] for _ in rows]
        ref = sum(r["ref_words"] for r in s) or 1
        diffs.append((sum(r[key_h] for r in s) - sum(r[key_r] for r in s)) / ref)
    diffs.sort()
    return diffs[int(0.025 * n)], diffs[int(0.975 * n)]


def score(cases, run):
    rows = []
    for c in cases:
        r = run.get(c["id"])
        if r is None or r.get("output") is None:
            continue
        lang, raw, out, ref = c.get("lang", "en"), c["transcript"], r["output"], c["reference_raw"]
        nr, nraw, nout = norm(ref, lang), norm(raw, lang), norm(out, lang)
        e_raw, n_ref = errors(nr, nraw)
        e_out, _ = errors(nr, nout)
        wr, wo = len(nraw.split()), len(nout.split())
        flags = []
        if raw.strip() and not out.strip() and FILLER.sub("", raw).strip(" ,.?!"):
            flags.append("empty")
        if wr >= 4 and wo < 0.7 * wr:
            flags.append("shorter")
        if wr >= 4 and wo > 1.3 * wr + 2:
            flags.append("longer")
        if wr >= 4 and out.strip():
            lr, lo = langid.classify(raw)[0], langid.classify(out)[0]
            if lang != "en" and lr == lang and lo == "en":
                flags.append("translated")
        row = {"id": c["id"], "source": c["source"], "lang": lang, "accent": c.get("accent"), "tags": c.get("tags", []), "duration_s": c["duration_s"],
               "reference": ref, "raw": raw, "output": out, "ms": r.get("ms"), "finish": r.get("finish"), "ref_words": n_ref, "err_raw": e_raw, "err_out": e_out,
               "wer_raw": e_raw / n_ref if n_ref else None, "wer_out": e_out / n_ref if n_ref else None, "flags": flags,
               "fill_raw": len(FILLER.findall(raw)), "fill_out": len(FILLER.findall(out)),
               "rep_raw": len(REPEAT.findall(raw)), "rep_out": len(REPEAT.findall(out))}
        if c.get("punctuated"):
            row["w_raw"], row["w_out"] = written(ref, raw), written(ref, out)
            rn = nums(ref)
            row["num_ref"], row["num_raw"], row["num_out"] = len(rn), sum(x in nums(raw) for x in rn), sum(x in nums(out) for x in rn)
            row["exact_raw"] = " ".join(raw.split()) == " ".join(ref.split())
            row["exact_out"] = " ".join(out.split()) == " ".join(ref.split())
        rows.append(row)
    return rows


def summary(rows, by):
    groups = defaultdict(list)
    for r in rows:
        for k in by(r):
            groups[k].append(r)
    lines = ["| slice | clips | hours | WER raw | WER cleaned | Δ (95% CI) | clips worse | worse >5 pts | flags |", "|---|---|---|---|---|---|---|---|---|"]
    for k in sorted(groups, key=lambda x: (x != "ALL", x)):
        s = groups[k]
        ref = sum(r["ref_words"] for r in s) or 1
        wr, wo = sum(r["err_raw"] for r in s) / ref, sum(r["err_out"] for r in s) / ref
        lo, hi = boot(s, "err_raw", "err_out") if len(s) > 1 else (0, 0)
        scored = [r for r in s if r["ref_words"]]
        worse = sum(r["err_out"] > r["err_raw"] for r in scored)
        worse5 = sum((r["err_out"] - r["err_raw"]) / r["ref_words"] > 0.05 for r in scored)
        fl = sum(bool(r["flags"]) for r in s)
        lines.append(f"| {k} | {len(s)} | {sum(r['duration_s'] for r in s) / 3600:.2f} | {100 * wr:.2f}% | {100 * wo:.2f}% | {100 * (wo - wr):+.2f} ({100 * lo:+.2f}, {100 * hi:+.2f}) | "
                     f"{100 * worse / max(len(scored), 1):.1f}% | {100 * worse5 / max(len(scored), 1):.1f}% | {fl} |")
    return "\n".join(lines)


def written_summary(rows, by):
    groups = defaultdict(list)
    for r in rows:
        if "w_raw" in r:
            for k in by(r):
                groups[k].append(r)
    lines = ["| slice | clips | punct F1 raw → cleaned (. , ?) | casing raw → cleaned | numbers kept raw → cleaned | exact raw → cleaned |", "|---|---|---|---|---|---|"]
    for k in sorted(groups, key=lambda x: (x != "ALL", x)):
        s = groups[k]
        cell = []
        for key in ("w_raw", "w_out"):
            f = [f1(*[sum(r[key][0][m][i] for r in s) for i in range(3)]) for m in MARKS]
            cell.append("/".join(f"{x:.2f}" for x in f))
        case = [sum(r[k2][1] for r in s) / max(sum(r[k2][2] for r in s), 1) for k2 in ("w_raw", "w_out")]
        nref = sum(r["num_ref"] for r in s) or 1
        lines.append(f"| {k} | {len(s)} | {cell[0]} → {cell[1]} | {100 * case[0]:.1f}% → {100 * case[1]:.1f}% | "
                     f"{100 * sum(r['num_raw'] for r in s) / nref:.1f}% → {100 * sum(r['num_out'] for r in s) / nref:.1f}% | "
                     f"{100 * statistics.mean(r['exact_raw'] for r in s):.1f}% → {100 * statistics.mean(r['exact_out'] for r in s):.1f}% |")
    return "\n".join(lines)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--cases", nargs="+", required=True)
    ap.add_argument("--run", required=True)
    ap.add_argument("--out")
    ap.add_argument("--worst", type=int, default=15)
    a = ap.parse_args()
    cases = [json.loads(l) for p in a.cases for l in open(p) if l.strip()]
    run = {}
    for l in open(a.run):
        r = json.loads(l)
        run[r["id"]] = r
    rows = score(cases, run)
    if a.out:
        with open(a.out, "w") as f:
            for r in rows:
                f.write(json.dumps(r, ensure_ascii=False) + "\n")
    print(f"# {Path(a.run).stem}: {len(rows)} clips, {sum(r['duration_s'] for r in rows) / 3600:.1f} h\n")
    print("## Words by source\n" + summary(rows, lambda r: ["ALL", r["source"]]))
    acc = [r for r in rows if r["accent"] and r["source"] == "edacc"]
    if acc:
        print("\n## EdAcc by accent\n" + summary(acc, lambda r: [r["accent"]]))
    tagged = [r for r in rows if any(t.startswith(("noise:", "snr:", "whisper:", "cond:")) for t in r["tags"])]
    if tagged:
        print("\n## By condition\n" + summary(tagged, lambda r: [t for t in r["tags"] if t.startswith(("noise:", "snr:", "whisper:", "cond:"))]))
    print("\n## By duration\n" + summary(rows, lambda r: [f"dur:{'<2s' if r['duration_s'] < 2 else '2-5s' if r['duration_s'] < 5 else '5-15s' if r['duration_s'] < 15 else '15s+'}"]))
    print("\n## Written form (punctuated references)\n" + written_summary(rows, lambda r: ["ALL", r["source"]]))
    print("\n## Fillers and repeats (counts in raw → cleaned)")
    for src in sorted({r["source"] for r in rows}):
        s = [r for r in rows if r["source"] == src]
        print(f"- {src}: um/uh {sum(r['fill_raw'] for r in s)} → {sum(r['fill_out'] for r in s)}; repeated words {sum(r['rep_raw'] for r in s)} → {sum(r['rep_out'] for r in s)}; "
              f"rule flags {dict((f, sum(f in r['flags'] for r in s)) for f in ('empty', 'shorter', 'longer', 'translated'))}")
    worst = sorted((r for r in rows if r["ref_words"]), key=lambda r: (r["err_out"] - r["err_raw"]) / r["ref_words"], reverse=True)[: a.worst]
    print("\n## Worst clips (cleaned − raw WER)")
    for r in worst:
        print(f"- {r['id']} Δ {100 * (r['err_out'] - r['err_raw']) / r['ref_words']:+.0f} pts\n  ref: {r['reference'][:200]}\n  raw: {r['raw'][:200]}\n  out: {r['output'][:200]}")


if __name__ == "__main__":
    main()
