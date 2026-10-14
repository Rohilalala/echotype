"""Evaluate a cleanup model on JSONL cases {transcript, dictionary, translate, reference, tags, must?, mustnot?}.

Modes:
  finetuned  short instruction from gen.system_prompt (what the trained model expects)
  prompted   the app's full cleanup prompt (app/src-tauri/src/cleanup_prompt.txt), rendered like cleanup.rs;
             translate=yes is passed as a custom-instructions line, since the app has no cleanup flag for it
  none       output = transcript (the "no cleanup" baseline)

Metrics: exact match, normalized WER vs reference, must/mustnot checks, fillers left, empty handling,
list formatting, latency (mean/p95 ms per item) and peak MLX memory.

Usage:
  python eval.py --mode none --data eval/handwritten.jsonl
  python eval.py --mode prompted --model mlx-community/Qwen2.5-1.5B-Instruct-4bit --data eval/handwritten.jsonl --out out/res/base_hw.jsonl
  python eval.py --mode finetuned --model out/fused-4bit --data eval/handwritten.jsonl out/data/heldout.jsonl --out out/res/ft.jsonl
"""

import argparse
import json
import re
import statistics
import time
from pathlib import Path

from gen import system_prompt, user_message

APP_PROMPT = Path(__file__).resolve().parent.parent / "app/src-tauri/src/cleanup_prompt.txt"
FILLER = re.compile(r"\b(um+|uh+|er|uhh|umm)\b", re.I)
LIST_LINE = re.compile(r"^(\d+\. |- )", re.M)


def app_system_prompt(dictionary, translate):
    """Mirror of cleanup::system_prompt in app/src-tauri/src/cleanup.rs (style = default)."""
    words = [w.strip() for w in dictionary if w.strip()]
    d = f"\nSpell these names and terms exactly as written: {', '.join(words)}." if words else ""
    custom = "\nTranslate the cleaned text to English." if translate else ""
    return APP_PROMPT.read_text().replace("{{DICTIONARY}}", d).replace("{{STYLE}}", "").replace("{{CUSTOM}}", custom).rstrip()


def norm_words(s):
    s = s.lower().replace("\n", " ")
    return [t for t in (w.strip(",.?!:;\"'()") for w in s.split()) if t]


def wer(ref, hyp):
    r, h = norm_words(ref), norm_words(hyp)
    if not r:
        return 0.0 if not h else 1.0
    prev = list(range(len(h) + 1))
    for i, rw in enumerate(r, 1):
        cur = [i] + [0] * len(h)
        for j, hw in enumerate(h, 1):
            cur[j] = min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + (rw != hw))
        prev = cur
    return prev[-1] / len(r)


def checks(c, out):
    """Per-case booleans; None = not applicable."""
    ref = c["reference"]
    res = {"exact": out == ref}
    if "must" in c or "mustnot" in c:
        bad = [m for m in c.get("mustnot", []) if re.search(rf"(?<!\w){re.escape(m.strip())}(?!\w)" if m.strip().isalnum() else re.escape(m), out, re.I)]
        res["must"] = all(m in out for m in c.get("must", [])) and not bad
    res["no_filler"] = None if FILLER.search(ref) or not FILLER.search(c["transcript"]) else not FILLER.search(out)
    res["empty_ok"] = (out == "") if ref == "" else None
    res["list_ok"] = (len(LIST_LINE.findall(out)) >= 2) if len(LIST_LINE.findall(ref)) >= 2 else None
    return res


def load_cases(paths, limit):
    cases = []
    for p in paths:
        rows = [json.loads(l) for l in open(p) if l.strip()]
        cases += rows[:limit] if limit else rows
    return cases


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--mode", choices=["finetuned", "prompted", "none"], required=True)
    ap.add_argument("--model")
    ap.add_argument("--adapter", help="LoRA adapter dir (unfused)")
    ap.add_argument("--data", nargs="+", required=True)
    ap.add_argument("--limit", type=int, default=0, help="first N cases per file")
    ap.add_argument("--out", help="write per-case outputs (JSONL) here")
    a = ap.parse_args()

    cases = load_cases(a.data, a.limit)
    lat, peak = [], None
    if a.mode != "none":
        import mlx.core as mx
        from mlx_lm import generate, load

        model, tok = load(a.model, adapter_path=a.adapter)
        mx.reset_peak_memory()
    for c in cases:
        if a.mode == "none":
            c["output"] = c["transcript"]
            continue
        sysp = system_prompt(c["dictionary"], c["translate"]) if a.mode == "finetuned" else app_system_prompt(c["dictionary"], c["translate"])
        msgs = [{"role": "system", "content": sysp}, {"role": "user", "content": user_message(c["transcript"])}]
        prompt = tok.apply_chat_template(msgs, add_generation_prompt=True)
        t0 = time.perf_counter()
        out = generate(model, tok, prompt, max_tokens=64 + 3 * len(tok.encode(c["transcript"])))
        lat.append((time.perf_counter() - t0) * 1000)
        c["output"] = out.rsplit("</think>", 1)[-1].strip()
    if a.mode != "none":
        peak = mx.get_peak_memory() / 2**30

    rows = []
    for c in cases:
        c["checks"] = checks(c, c["output"])
        c["wer"] = wer(c["reference"], c["output"])
        rows.append(c)
    if a.out:
        Path(a.out).parent.mkdir(parents=True, exist_ok=True)
        with open(a.out, "w") as f:
            for c in rows:
                f.write(json.dumps(c, ensure_ascii=False) + "\n")

    def rate(key, sub=rows):
        v = [c["checks"][key] for c in sub if c["checks"].get(key) is not None]
        return f"{100 * sum(v) / len(v):.1f}% (n={len(v)})" if v else "-"

    print(f"mode={a.mode} model={a.model} cases={len(rows)}")
    print(f"  exact {rate('exact')}  WER {100 * statistics.mean(c['wer'] for c in rows):.1f}%  must/mustnot {rate('must')}")
    print(f"  fillers removed {rate('no_filler')}  empty ok {rate('empty_ok')}  list ok {rate('list_ok')}")
    if lat:
        s = sorted(lat)
        print(f"  latency mean {statistics.mean(lat):.0f} ms  p95 {s[int(0.95 * (len(s) - 1))]:.0f} ms  peak mem {peak:.2f} GB")
    tags = sorted({t for c in rows for t in c.get("tags", [])})
    for t in tags:
        sub = [c for c in rows if t in c.get("tags", [])]
        print(f"  [{t}] n={len(sub)} exact {rate('exact', sub)} WER {100 * statistics.mean(c['wer'] for c in sub):.1f}% checks {rate('must', sub)}")


if __name__ == "__main__":
    main()
