"""Batched whisper-cli transcription with Echotype's local STT settings, then case files for the cleanup run.

Settings mirror stt::run_local: greedy (best_of 1), temperature fallback on, flash attention, language auto,
no initial prompt (no dictionary), no timestamps. The text is the joined segments through stt::clean_text
(drop [..] tags, collapse whitespace). Resumable: a WAV with a <wav>.json next to it is done.

  python transcribe.py run --manifest cases/real-*.jsonl --model ~/.cache/whisper/ggml-large-v3-turbo-q5_0.bin
  python transcribe.py collect --manifest cases/real-*.jsonl --out cases/asr-real.jsonl
"""

import argparse
import json
import subprocess
import time
from pathlib import Path


def clean_text(s):
    """Port of stt::clean_text."""
    out, depth = [], 0
    for ch in s:
        if ch == "[":
            depth += 1
        elif ch == "]" and depth > 0:
            depth -= 1
        elif depth == 0:
            out.append(ch)
    return " ".join("".join(out).split())


def whisper_text(js):
    d = json.load(open(js, encoding="utf-8", errors="replace"))
    return clean_text("".join(seg["text"] for seg in d.get("transcription", []))), (d.get("result") or {}).get("language")


def rows(manifests):
    return [json.loads(l) for m in manifests for l in open(m) if l.strip()]


def run(a):
    todo = [r for r in rows(a.manifest) if not Path(r["wav"] + ".json").exists()]
    if a.limit:
        todo = todo[: a.limit]
    total = sum(r["duration_s"] for r in todo)
    print(f"{len(todo)} clips to transcribe, {total / 3600:.2f} h audio", flush=True)
    done_s, t_all = 0.0, time.perf_counter()
    for i in range(0, len(todo), a.batch):
        b = todo[i : i + a.batch]
        while a.ram_check and subprocess.run([a.ram_check], capture_output=True).returncode:
            print("  paused: low RAM or memory pressure", flush=True)
            time.sleep(30)
        cmd = [a.cli, "-m", str(a.model), "-l", a.language, "-bs", "1", "-bo", "1", "-nt", "-np", "-oj", "-t", str(a.threads)]
        for r in b:
            cmd += ["-f", r["wav"]]
        t0 = time.perf_counter()
        p = subprocess.run(cmd, capture_output=True)  # bytes: whisper can print invalid UTF-8 mid-character
        dt = time.perf_counter() - t0
        aud = sum(r["duration_s"] for r in b)
        done_s += aud
        missing = [r["wav"] for r in b if not Path(r["wav"] + ".json").exists()]
        el = time.perf_counter() - t_all
        print(f"  batch {i // a.batch}: {len(b)} clips {aud:.0f} s audio in {dt:.1f} s ({aud / dt:.1f}x realtime) rc={p.returncode} missing={len(missing)}"
              f" | {done_s / 3600:.2f}/{total / 3600:.2f} h, eta {(total - done_s) / max(done_s / el, 1e-9) / 60:.0f} min", flush=True)
        if p.returncode and missing:
            print(p.stderr[-2000:].decode("utf-8", "replace"), flush=True)


def collect(a):
    out = []
    for r in rows(a.manifest):
        js = Path(r["wav"] + ".json")
        if not js.exists():
            continue
        text, lang = whisper_text(js)
        out.append({**r, "transcript": text, "whisper_lang": lang, "dictionary": r.get("dictionary", []), "translate": r.get("translate", False),
                    "reference": r["reference_raw"], "tags": r.get("tags", []) + [f"src:{r['source']}", f"lang:{r['lang']}"]
                    + ([f"accent:{r['accent']}"] if r.get("accent") else []) + r.get("tags_extra", [])})
    with open(a.out, "w") as f:
        for r in out:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")
    print(f"{len(out)} transcribed clips -> {a.out}")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run")
    r.add_argument("--manifest", nargs="+", required=True)
    r.add_argument("--model", type=Path, default=Path.home() / ".cache/whisper/ggml-large-v3-turbo-q5_0.bin")
    r.add_argument("--cli", default="whisper-cli")
    r.add_argument("--language", default="auto")
    r.add_argument("--batch", type=int, default=100)
    r.add_argument("--threads", type=int, default=4)
    r.add_argument("--limit", type=int, default=0)
    r.add_argument("--ram-check", help="script that exits non-zero when the machine is short of memory; checked before each batch")
    c = sub.add_parser("collect")
    c.add_argument("--manifest", nargs="+", required=True)
    c.add_argument("--out", required=True)
    a = ap.parse_args()
    run(a) if a.cmd == "run" else collect(a)


if __name__ == "__main__":
    main()
