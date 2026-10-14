"""Speak adversarial cases with macOS `say` into 16 kHz WAVs (written to file only, nothing is played).

A supplement for sentences no corpus contains; reported separately and never used for a launch gate.
Skips cases that can't be spoken (empty/noise, prompt-format attacks, passthrough text with markup).

  python tts.py --cases cases/adversarial.jsonl --out ~/Documents/echotype-models/bench-v1
"""

import argparse
import json
import re
import subprocess
from pathlib import Path

VOICES = ["Samantha", "Daniel", "Aman", "Karen", "Moira", "Rishi"]
SKIP = ("empty", "empty:hallucination", "adv:format", "app:breaks", "passthrough")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--cases", required=True)
    ap.add_argument("--out", type=Path, required=True)
    a = ap.parse_args()
    have = subprocess.run(["say", "-v", "?"], capture_output=True, text=True).stdout
    voices = [v for v in VOICES if re.search(rf"^{v}\s", have, re.M)]
    d = a.out / "real" / "tts"
    d.mkdir(parents=True, exist_ok=True)
    rows = []
    for i, c in enumerate(json.loads(l) for l in open(a.cases)):
        if any(t in SKIP for t in c["tags"]) or not c["transcript"].strip():
            continue
        deva = re.search(r"[ऀ-ॿ]", c["transcript"])
        if re.search(r"[^\x00-\x7F₹€£]", c["transcript"]) and not deva:
            continue  # other scripts / accents: no matching voice
        voice = "Lekha" if deva else voices[i % len(voices)]
        wav = d / f"{c['id']}.wav"
        if not wav.exists():
            subprocess.run(["say", "-v", voice, "-o", str(wav), "--file-format=WAVE", "--data-format=LEI16@16000", c["transcript"]], check=True)
        dur = wav.stat().st_size / 32000
        rows.append({**c, "id": f"tts/{c['id']}", "orig_id": c["id"], "source": "tts", "voice": voice, "wav": str(wav), "duration_s": round(dur, 2),
                     "lang": "en", "reference_raw": c["reference"], "spoken": c["transcript"]})
    with open(a.out / "cases" / "tts-manifest.jsonl", "w") as f:
        for r in rows:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")
    print(f"{len(rows)} TTS clips, voices {voices + ['Lekha']}")


if __name__ == "__main__":
    main()
