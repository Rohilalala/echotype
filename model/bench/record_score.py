"""Owner recording kit: inbox audio -> 16 kHz WAV + manifest (prepare), whisper output -> cases (cases), results (report).

Files: inbox/<id>-<quiet|noisy|whisper>.<ext> (one sentence) or inbox/<id1>_<id2>-<cond>.<ext> (rows id1..id2 in
order, split on pauses >= 1.2 s; if the pause count doesn't match, scored as one dictation).
Hinglish rows ("both") become two cases: translate off and on.

  python record_score.py prepare --kit ~/Documents/echotype-models/bench-v1/record
  python record_score.py cases   --kit ...      (after transcribe.py run --manifest record/manifest.jsonl)
  python record_score.py report  --kit ... --run record/run.jsonl
"""

import argparse
import csv
import json
import re
import subprocess
from pathlib import Path

import numpy as np
import soundfile as sf

from asr_score import errors, norm
from transcribe import whisper_text

NAME = re.compile(r"^(r\d+)(?:_(r\d+))?-(quiet|noisy|whisper)$")


def script(kit):
    rows = list(csv.DictReader(open(kit / "script.tsv", encoding="utf-8"), delimiter="\t"))
    return {r["id"]: r for r in rows}, [r["id"] for r in rows]


def split_pauses(x, sr, k, min_gap=1.2):
    """k segments split at the k-1 longest pauses >= min_gap (30 ms frames, energy < 10% of the 95th percentile)."""
    f = int(0.03 * sr)
    e = np.array([np.sqrt(np.mean(x[i : i + f] ** 2)) for i in range(0, len(x) - f, f)])
    quiet = e < 0.1 * np.percentile(e, 95)
    gaps, s = [], None
    for i, q in enumerate(np.r_[quiet, False]):
        if q and s is None:
            s = i
        elif not q and s is not None:
            if (i - s) * 0.03 >= min_gap and s > 0:
                gaps.append((i - s, (s + i) // 2 * f))
            s = None
    if len(gaps) < k - 1:
        return None
    cuts = sorted(c for _, c in sorted(gaps, reverse=True)[: k - 1])
    return [x[a:b] for a, b in zip([0] + cuts, cuts + [len(x)])]


def prepare(kit):
    sc, order = script(kit)
    wav_dir = kit / "wav"
    wav_dir.mkdir(exist_ok=True)
    man = []
    for p in sorted((kit / "inbox").iterdir()):
        m = NAME.match(p.stem)
        if not m or p.suffix.lower() not in (".m4a", ".wav", ".mp3", ".aiff", ".aif", ".caf"):
            continue
        a, b, cond = m.groups()
        w = wav_dir / f"{p.stem}.wav"
        ids = order[order.index(a) : order.index(b) + 1] if b else [a]
        if not w.exists() or p.stat().st_mtime > w.stat().st_mtime:  # new or re-recorded
            for f in [w, Path(f"{w}.json")] + [wav_dir / f"{i}-{cond}.wav{e}" for i in ids for e in ("", ".json")]:
                f.unlink(missing_ok=True)  # stale conversion, segments and whisper output
            subprocess.run(["ffmpeg", "-nostdin", "-loglevel", "error", "-y", "-i", str(p), "-ac", "1", "-ar", "16000", str(w)], check=True)
        x, sr = sf.read(w)
        parts = split_pauses(x, sr, len(ids)) if len(ids) > 1 else [x]
        if parts:
            for rid, seg in zip(ids, parts):
                sw = w if len(ids) == 1 else wav_dir / f"{rid}-{cond}.wav"
                if len(ids) > 1:
                    sf.write(sw, seg, sr, subtype="PCM_16")
                man.append({"id": f"rec/{rid}-{cond}", "rows": [rid], "cond": cond, "wav": str(sw), "duration_s": len(seg) / sr, "lang": "en", "source": f"owner_{cond}"})
        else:
            man.append({"id": f"rec/{a}_{b}-{cond}", "rows": ids, "cond": cond, "wav": str(w), "duration_s": len(x) / sr, "lang": "en", "source": f"owner_{cond}"})
    with open(kit / "manifest.jsonl", "w") as f:
        for r in man:
            f.write(json.dumps(r) + "\n")
    print(f"{len(man)} recordings -> {kit / 'manifest.jsonl'}")


def cases(kit):
    sc, _ = script(kit)
    out = []
    for r in (json.loads(l) for l in open(kit / "manifest.jsonl")):
        if not Path(r["wav"] + ".json").exists():
            continue
        text, lang = whisper_text(r["wav"] + ".json")
        rows = [sc[i] for i in r["rows"]]
        dct = [w.strip() for row in rows for w in row["dictionary"].split(",") if w.strip()]
        modes = [False, True] if any(row["translate"] == "both" for row in rows) else [False]
        for tr in modes:
            ref = " ".join(row["expected"].split(" || ")[1 if tr and row["translate"] == "both" else 0] for row in rows).replace("\\n", "\n")
            out.append({**r, "id": r["id"] + ("+tr" if tr else ""), "transcript": text, "whisper_lang": lang, "dictionary": dct, "translate": tr,
                        "reference": ref, "reference_raw": ref, "tags": [f"cond:{r['cond']}"] + [f"kind:{row['kind']}" for row in rows] + (["translate"] if tr else [])})
    with open(kit / "cases.jsonl", "w") as f:
        for c in out:
            f.write(json.dumps(c, ensure_ascii=False) + "\n")
    print(f"{len(out)} cases -> {kit / 'cases.jsonl'}")


def report(kit, run):
    cs = [json.loads(l) for l in open(kit / "cases.jsonl")]
    out = {json.loads(l)["id"]: json.loads(l).get("output") or "" for l in open(run)}
    lines = ["Echotype v1 on your own recordings", ""]
    for cond in ("quiet", "noisy", "whisper"):
        # Hinglish rows are left out of WER: whisper may write them in Devanagari, the references are romanized
        s = [c for c in cs if c["cond"] == cond and not c["translate"] and c["id"] in out and "kind:hinglish" not in c["tags"]]
        if not s:
            continue
        er = eo = n = 0
        for c in s:
            nr = norm(c["reference"], "en")
            e1, k = errors(nr, norm(c["transcript"], "en"))
            e2, _ = errors(nr, norm(out[c["id"]], "en"))
            er, eo, n = er + e1, eo + e2, n + k
        exact = sum(out[c["id"]] == c["reference"] for c in s)
        lines.append(f"{cond:8s} {len(s):3d} sentences  WER raw whisper {100 * er / max(n, 1):5.1f}%  cleaned {100 * eo / max(n, 1):5.1f}%  exact {exact}/{len(s)}")
    lines += ["(WER and exact match leave out the Hinglish rows; they are listed below for a look.)",
              "", "Every sentence that is not exactly right (check the meaning by eye):", ""]
    for c in cs:
        o = out.get(c["id"])
        if o is not None and o != c["reference"]:
            lines.append(f"{c['id']}\n  whisper:  {c['transcript']}\n  cleaned:  {o}\n  expected: {c['reference']}\n")
    (kit / "RESULTS.txt").write_text("\n".join(lines))
    print("\n".join(lines[:6]))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("cmd", choices=["prepare", "cases", "report"])
    ap.add_argument("--kit", type=Path, required=True)
    ap.add_argument("--run")
    a = ap.parse_args()
    {"prepare": lambda: prepare(a.kit), "cases": lambda: cases(a.kit), "report": lambda: report(a.kit, a.run)}[a.cmd]()


if __name__ == "__main__":
    main()
