"""Noisy and pseudo-whispered copies of real clips (reference transcripts unchanged).

noise      MUSAN noise (CC BY 4.0) at SNR 20/10/5/0 dB
babble     5 other real speakers (LibriSpeech test) summed, at SNR 20/10/5/0 dB
competing  one other real speaker 6-15 dB below the target
whisper    synthetic: LPC envelope (order 16, 25 ms frames) excited by white noise (no F0), level lowered 20-30 dB

Stratified round-robin over sources and conditions. Writes <out>/real/aug/<cond>/<id>.wav and cases/real-aug.jsonl.

  python augment.py --out ~/Documents/echotype-models/bench-v1 --noise-dir real/musan/noise --n 1500 --whisper-n 300
"""

import argparse
import json
import random
from collections import defaultdict
from pathlib import Path

import numpy as np
import soundfile as sf
from scipy.signal import lfilter

SRC = ["earnings22", "voxpopuli", "ami", "edacc", "fleurs_en", "librispeech_clean", "librispeech_other", "peoples_speech"]


def rms(x):
    return float(np.sqrt(np.mean(x ** 2)) + 1e-9)


def segment(x, n, rng):
    if len(x) < n:
        x = np.tile(x, n // len(x) + 1)
    o = rng.randrange(0, len(x) - n + 1)
    return x[o : o + n]


def at_snr(speech, noise, snr):
    return speech + noise * (rms(speech) / rms(noise)) / (10 ** (snr / 20))


def lpc(frame, order):
    """Levinson-Durbin on the autocorrelation -> (a with a[0] = 1, prediction error)."""
    r = np.correlate(frame, frame, "full")[len(frame) - 1 : len(frame) + order]
    if r[0] <= 1e-10:
        return np.r_[1.0, np.zeros(order)], 0.0
    a, e = np.zeros(order + 1), r[0]
    a[0] = 1.0
    for i in range(1, order + 1):
        k = -(r[i] + np.dot(a[1:i], r[i - 1 : 0 : -1])) / e
        a[1:i] = a[1:i] + k * a[i - 1 : 0 : -1]
        a[i] = k
        e *= 1 - k * k
    return a, max(e, 0.0)


def pseudo_whisper(x, rng, order=16, n=400, hop=200):
    """Keep the spectral envelope, replace voicing with noise."""
    win = np.hanning(n + 1)[:-1]  # periodic Hann: overlap-add at hop n/2 sums to 1
    y = np.zeros(len(x) + n)
    noise = np.random.default_rng(rng.randrange(2**31)).standard_normal(len(x) + n)
    for s in range(0, len(x) - n, hop):
        f = x[s : s + n] * win
        a, e = lpc(f, order)
        g = np.sqrt(e / n)
        y[s : s + n] += lfilter([g], a, noise[s : s + n]) * win
    y = y[: len(x)]
    y *= rms(x) / rms(y)
    return y * 10 ** (-rng.uniform(20, 30) / 20)


def load_rows(out):
    rows = []
    for s in SRC:
        p = out / "cases" / f"real-{s}.jsonl"
        if p.exists():
            rows += [json.loads(l) for l in open(p)]
    return rows


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--noise-dir", type=Path, required=True)
    ap.add_argument("--n", type=int, default=1500)
    ap.add_argument("--whisper-n", type=int, default=300)
    ap.add_argument("--seed", type=int, default=0)
    a = ap.parse_args()
    rng = random.Random(a.seed)
    rows = [r for r in load_rows(a.out) if 2 <= r["duration_s"] <= 30]
    by = defaultdict(list)
    for r in rows:
        by[r["source"]].append(r)
    for v in by.values():
        rng.shuffle(v)
    srcs = sorted(by)
    noises = sorted(a.noise_dir.rglob("*.wav"))
    babble_pool = [r for r in rows if r["source"].startswith("librispeech")]
    conds = [("noise", s) for s in (20, 10, 5, 0)] + [("babble", s) for s in (20, 10, 5, 0)] + [("competing", None)] * 2
    plan, i = [], 0
    while len(plan) < a.n + a.whisper_n and any(by.values()):
        s = srcs[i % len(srcs)]
        i += 1
        if by[s]:
            plan.append(by[s].pop())
    out_rows = []
    for k, r in enumerate(plan):
        whisper = k >= a.n
        cond, snr = ("whisper", None) if whisper else conds[k % len(conds)]
        d = a.out / "real" / "aug" / cond
        d.mkdir(parents=True, exist_ok=True)
        cid = Path(r["wav"]).stem
        wav = d / f"{r['source']}__{cid}{'' if snr is None else f'_snr{snr}'}.wav"
        tags = [f"cond:{cond}"]
        if not wav.exists():
            x, sr = sf.read(r["wav"], dtype="float64")
            if cond == "noise":
                y = at_snr(x, segment(sf.read(rng.choice(noises), dtype="float64")[0], len(x), rng), snr)
            elif cond == "babble":
                talkers = [sf.read(b["wav"], dtype="float64")[0] for b in rng.sample(babble_pool, 5)]
                y = at_snr(x, sum(segment(t / rms(t), len(x), rng) for t in talkers), snr)
            elif cond == "competing":
                other = sf.read(rng.choice([b for b in rows if b["source"] != r["source"]])["wav"], dtype="float64")[0]
                snr = rng.uniform(6, 15)
                y = at_snr(x, segment(other, len(x), rng), snr)
            else:
                y = pseudo_whisper(x, rng)
            peak = np.max(np.abs(y))
            sf.write(wav, y / peak * 0.95 if peak > 0.99 else y, 16000, subtype="PCM_16")
        if snr is not None:
            tags.append(f"snr:{cond}:{round(snr) if cond != 'competing' else '6-15'}")
        out_rows.append({**r, "id": f"aug/{cond}/{r['id']}" + (f"_snr{snr}" if cond in ("noise", "babble") else ""), "source": r["source"],
                         "wav": str(wav), "tags_extra": tags, "synthetic": cond == "whisper", "orig_id": r["id"]})
    with open(a.out / "cases" / "real-aug.jsonl", "w") as f:
        for r in out_rows:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")
    print(f"{len(out_rows)} augmented clips: " + ", ".join(f"{c} {sum(t == f'cond:{c}' for r in out_rows for t in r['tags_extra'])}" for c in ("noise", "babble", "competing", "whisper")))


if __name__ == "__main__":
    main()
