"""Fetch real speech test clips (human transcripts) as 16 kHz mono WAV plus a manifest.

Only test/validation splits, only permissive licences (see SOURCES). Resumable: existing WAVs are kept.
Writes <out>/real/<source>/<id>.wav and <out>/cases/real-<source>.jsonl rows:
  {id, source, split, licence, url, speaker, accent, lang, punctuated, duration_s, reference_raw, wav}

  python fetch_real.py --out ~/Documents/echotype-models/bench-v1 --sources fleurs_en earnings22 ami
"""

import argparse
import hashlib
import io
import json
import random
import re
import tarfile
from collections import defaultdict
from pathlib import Path

import numpy as np
import pyarrow.parquet as pq
import soundfile as sf
import soxr
from huggingface_hub import hf_hub_download, list_repo_files

ESB = "hf-audio/esb-datasets-test-only"
ESB_REV = "refs/convert/parquet"
# name -> (repo, revision, parquet prefix or fleurs lang, split, licence, punctuated refs, how many clips (0 = all), lang)
SOURCES = {
    "librispeech_clean": (ESB, ESB_REV, "librispeech/test.clean/", "test.clean", "CC-BY-4.0", False, 750, "en"),
    "librispeech_other": (ESB, ESB_REV, "librispeech/test.other/", "test.other", "CC-BY-4.0", False, 750, "en"),
    "ami": (ESB, ESB_REV, "ami/test/", "test", "CC-BY-4.0", True, 1500, "en"),
    "earnings22": (ESB, ESB_REV, "earnings22/test/", "test", "CC-BY-SA-4.0", True, 0, "en"),
    "voxpopuli": (ESB, ESB_REV, "voxpopuli/test/", "test", "CC0-1.0", False, 0, "en"),
    "edacc": ("edinburghcstr/edacc", None, "data/test-", "test", "CC-BY-SA-4.0", True, 2500, "en"),
    "peoples_speech": ("MLCommons/peoples_speech", None, "test/test-00000-", "test", "CC-BY/CC-BY-SA (per-clip, see card)", False, 600, "en"),
    "fleurs_en": ("google/fleurs", None, "en_us", "test", "CC-BY-4.0", True, 0, "en"),
    "fleurs_hi": ("google/fleurs", None, "hi_in", "test", "CC-BY-4.0", True, 0, "hi"),
    "fleurs_es": ("google/fleurs", None, "es_419", "test", "CC-BY-4.0", True, 150, "es"),
    "fleurs_fr": ("google/fleurs", None, "fr_fr", "test", "CC-BY-4.0", True, 150, "fr"),
    "fleurs_de": ("google/fleurs", None, "de_de", "test", "CC-BY-4.0", True, 150, "de"),
}


def to16k(data, sr):
    if data.ndim > 1:
        data = data.mean(axis=1)
    if sr != 16000:
        data = soxr.resample(data, sr, 16000)
    return data.astype(np.float32)


def safe(s):
    """File-safe and unique: long ids (People's Speech paths) keep a hash so truncation can't collide."""
    t = re.sub(r"[^A-Za-z0-9_.-]+", "_", s)
    return t if len(t) <= 100 else t[:80] + "-" + hashlib.sha1(s.encode()).hexdigest()[:12]


def ami_dur(i):
    m = re.search(r"_(\d{7})_(\d{7})$", i)
    return (int(m.group(2)) - int(m.group(1))) / 100 if m else 0


def pick(rows, n, key, rng):
    """Stratified sample: round-robin over strata of key(row) so every stratum is represented."""
    if not n or n >= len(rows):
        return rows
    groups = defaultdict(list)
    for r in rows:
        groups[key(r)].append(r)
    for g in groups.values():
        rng.shuffle(g)
    out, ks = [], sorted(groups)
    while len(out) < n:
        for k in ks:
            if groups[k] and len(out) < n:
                out.append(groups[k].pop())
    return out


def parquet_source(name, spec, out, rng):
    repo, rev, prefix, split, lic, punct, n, lang = spec
    files = [f for f in list_repo_files(repo, repo_type="dataset", revision=rev) if f.startswith(prefix) and f.endswith(".parquet")]
    meta = []
    for f in files:
        p = hf_hub_download(repo, f, repo_type="dataset", revision=rev)
        cols = [c for c in pq.read_schema(p).names if c != "audio"]
        for i, r in enumerate(pq.read_table(p, columns=cols).to_pylist()):
            meta.append({"_file": p, "_row": i, **r})
    print(f"{name}: {len(meta)} rows in {len(files)} files", flush=True)
    meta = [m for m in meta if (m.get("text") or "").strip()]
    if name == "ami":  # strata by duration so short backchannels don't dominate
        key = lambda m: min(int(ami_dur(m["id"]) // 2), 5)
    elif name == "edacc":
        key = lambda m: m.get("accent") or "?"
    else:
        key = lambda m: 0
    chosen = pick(meta, n, key, rng)
    by_file = defaultdict(list)
    for m in chosen:
        by_file[m["_file"]].append(m)
    rows, d = [], out / "real" / name
    d.mkdir(parents=True, exist_ok=True)
    for p, ms in by_file.items():
        audio = pq.read_table(p, columns=["audio"]).column("audio")
        for m in ms:
            cid = safe(m.get("id") or f"{Path(p).stem}-{m['_row']}")
            wav = d / f"{cid}.wav"
            if not wav.exists():
                a = audio[m["_row"]].as_py()
                data, sr = sf.read(io.BytesIO(a["bytes"]))
                sf.write(wav, to16k(data, sr), 16000, subtype="PCM_16")
            rows.append({"id": f"{name}/{cid}", "source": name, "split": split, "licence": lic, "url": f"https://huggingface.co/datasets/{repo}",
                         "speaker": m.get("speaker"), "accent": m.get("accent") or m.get("l1"), "lang": lang, "punctuated": punct,
                         "duration_s": round(sf.info(wav).duration, 2), "reference_raw": m["text"].strip(), "wav": str(wav)})
    return rows


def fleurs_source(name, spec, out, rng):
    repo, _, lang_dir, split, lic, punct, n, lang = spec
    tsv = hf_hub_download(repo, f"data/{lang_dir}/{split}.tsv", repo_type="dataset")
    meta = []
    for line in open(tsv, encoding="utf-8"):
        c = line.rstrip("\n").split("\t")
        meta.append({"sid": c[0], "file": c[1], "raw": c[2], "gender": c[-1]})
    chosen = {m["file"]: m for m in pick(meta, n, lambda m: m["gender"], rng)}
    d = out / "real" / name
    d.mkdir(parents=True, exist_ok=True)
    tar = hf_hub_download(repo, f"data/{lang_dir}/audio/{split}.tar.gz", repo_type="dataset")
    rows = []
    with tarfile.open(tar) as t:
        for mem in t:
            base = Path(mem.name).name
            if base not in chosen:
                continue
            m, wav = chosen[base], d / f"{Path(base).stem}.wav"
            if not wav.exists():
                data, sr = sf.read(io.BytesIO(t.extractfile(mem).read()))
                sf.write(wav, to16k(data, sr), 16000, subtype="PCM_16")
            rows.append({"id": f"{name}/{wav.stem}", "source": name, "split": split, "licence": lic, "url": f"https://huggingface.co/datasets/{repo}",
                         "speaker": None, "accent": None, "gender": m["gender"], "sentence_id": m["sid"], "lang": lang, "punctuated": punct,
                         "duration_s": round(sf.info(wav).duration, 2), "reference_raw": m["raw"], "wav": str(wav)})
    return rows


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--sources", nargs="+", default=list(SOURCES), choices=list(SOURCES))
    ap.add_argument("--seed", type=int, default=0)
    a = ap.parse_args()
    (a.out / "cases").mkdir(parents=True, exist_ok=True)
    for name in a.sources:
        man = a.out / "cases" / f"real-{name}.jsonl"
        if man.exists():
            print(f"{name}: done already ({man})", flush=True)
            continue
        spec, rng = SOURCES[name], random.Random(f"{a.seed}-{name}")
        rows = (fleurs_source if spec[0] == "google/fleurs" else parquet_source)(name, spec, a.out, rng)
        tmp = man.with_suffix(".tmp")
        with open(tmp, "w") as f:
            for r in rows:
                f.write(json.dumps(r, ensure_ascii=False) + "\n")
        tmp.rename(man)
        print(f"{name}: {len(rows)} clips, {sum(r['duration_s'] for r in rows) / 3600:.2f} h", flush=True)


if __name__ == "__main__":
    main()
