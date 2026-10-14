"""Owner's real call (private, local only): mic track in 25 s chunks -> whisper -> v1, WER vs Wispr Flow's "(You)" lines.

  python owner_call.py chunk  --dir private/meeting-2026-10-14          # chunks + manifest
  python transcribe.py run    --manifest private/meeting-2026-10-14/chunks.jsonl
  python owner_call.py cases  --dir private/meeting-2026-10-14          # cases for run.py
  python owner_call.py score  --dir private/meeting-2026-10-14 --run private/meeting-2026-10-14/v1.jsonl
Nothing here leaves the private directory; the report quotes numbers only.
"""

import argparse
import json
import re
from pathlib import Path

import soundfile as sf

from asr_score import errors, norm
from transcribe import whisper_text


def you_lines(d):
    lines = [l.split(":", 1)[1].strip() for l in open(d / "wispr-flow-transcript.txt", encoding="utf-8") if re.match(r"^[^:]+\(You\):", l)]
    other = [l.split(":", 1)[1].strip() for l in open(d / "wispr-flow-transcript.txt", encoding="utf-8") if re.match(r"^Speaker \d+:", l)]
    return " ".join(lines), " ".join(other)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("cmd", choices=["chunk", "cases", "score"])
    ap.add_argument("--dir", type=Path, required=True)
    ap.add_argument("--run")
    ap.add_argument("--sec", type=float, default=25)
    a = ap.parse_args()
    d, man = a.dir, a.dir / "chunks.jsonl"
    if a.cmd == "chunk":
        x, sr = sf.read(d / "audio-mic.wav")
        n = int(a.sec * sr)
        (d / "chunks").mkdir(exist_ok=True)
        with open(man, "w") as f:
            for i in range(0, len(x), n):
                w = d / "chunks" / f"mic-{i // n:03d}.wav"
                sf.write(w, x[i : i + n], sr, subtype="PCM_16")
                f.write(json.dumps({"id": f"call/{w.stem}", "source": "owner_call", "wav": str(w), "duration_s": len(x[i : i + n]) / sr, "reference_raw": "", "lang": "en"}) + "\n")
        print(f"{man}: {len(x) / sr / 60:.1f} min in {a.sec:.0f} s chunks")
    elif a.cmd == "cases":
        rows = [json.loads(l) for l in open(man)]
        with open(d / "cases.jsonl", "w") as f:
            for r in rows:
                text, lang = whisper_text(r["wav"] + ".json")
                f.write(json.dumps({**r, "transcript": text, "whisper_lang": lang, "dictionary": [], "translate": False, "reference": ""}, ensure_ascii=False) + "\n")
        print(f"{len(rows)} chunk cases -> {d / 'cases.jsonl'}")
    else:
        cases = [json.loads(l) for l in open(d / "cases.jsonl")]
        out = {json.loads(l)["id"]: json.loads(l)["output"] or "" for l in open(a.run)}
        raw = " ".join(c["transcript"] for c in cases)
        cleaned = " ".join(out.get(c["id"], "") for c in cases)
        you, other = you_lines(d)
        res = {}
        for name, ref in [("you", you), ("all speakers", you + " " + other)]:
            nr = norm(ref, "en")
            er, nw = errors(nr, norm(raw, "en"))
            eo, _ = errors(nr, norm(cleaned, "en"))
            res[name] = {"ref_words": nw, "wer_raw": round(er / nw, 4), "wer_cleaned": round(eo / nw, 4)}
        res["words"] = {"raw": len(norm(raw, "en").split()), "cleaned": len(norm(cleaned, "en").split()), "chunks": len(cases),
                        "empty_cleaned_chunks": sum(not out.get(c["id"]) and bool(c["transcript"].strip()) for c in cases)}
        print(json.dumps(res, indent=1))
        json.dump(res, open(d / "score.json", "w"), indent=1)


if __name__ == "__main__":
    main()
