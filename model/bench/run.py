"""Send eval cases to an OpenAI-compatible server exactly as Echotype's app does (app/src-tauri/src/cleanup.rs).

  request:  POST {url}/chat/completions {"model", "temperature": 0, "stream": false, "messages": [system, user]}
            (+ "reasoning_effort": "none" with --reasoning-off, like the ollama / lmstudio providers)
  reply:    parse_reply: content null -> "", text after the last </think>, trimmed

--prompt finetuned  the short trained prompt (gen.system_prompt), what the "echotype" provider sends
--prompt app        the long cleanup_prompt.txt prompt (eval.app_system_prompt), what every other provider sends
--pipeline model    the transcript goes to the model as-is (the model under test)
--pipeline app      mirror of cleanup::after_stt for dictation: has_speech gate, spoken "new line"/"new paragraph"
                    -> breaks before the model, and an empty reply means "no speech" only for the echotype provider

Resumable: rows already in --out (by id) are skipped. Extra request fields: --extra '{"max_tokens": 2048}'.
Quick fixes (section 7 of the bench brief, none of them in the app today):
  --scale-max-tokens   send max_tokens = 64 + 4 x words (mlx_lm.server otherwise stops at 512 tokens)
  --chunk-words N      clean inputs longer than N words in sentence-aligned chunks of <= N words, joined by spaces
  --system-extra TEXT  one more line after the trained instruction

  python run.py --url http://127.0.0.1:8899/v1 --model default_model --data cases/adversarial.jsonl --out runs/v1_adv.jsonl
"""

import argparse
import json
import re
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from eval import app_system_prompt  # noqa: E402
from gen import system_prompt, user_message  # noqa: E402


def has_speech(text):
    return any(ch.isalnum() for ch in text)


def spoken_breaks(text):
    """Port of cleanup::spoken_breaks."""
    t, out, i = text, [], 0
    low = t.lower()

    def boundary(j):
        return j < 0 or j >= len(t) or not t[j].isalnum()

    while i < len(t):
        hit = next(((p, br) for p, br in (("new paragraph", "\n\n"), ("new line", "\n")) if low.startswith(p, i) and boundary(i - 1) and boundary(i + len(p))), None)
        if not hit:
            out.append(t[i])
            i += 1
            continue
        s = "".join(out).rstrip(" \t")
        out = [s, hit[1]]
        i += len(hit[0])
        while i < len(t) and t[i] in ".,;:!?":
            i += 1
        while i < len(t) and t[i] in " \t":
            i += 1
    return "".join(out)


def chunks(text, n):
    """Sentence-aligned pieces of <= n words (a sentence longer than n is split on words)."""
    out, cur = [], []
    for sent in re.split(r"(?<=[.?!])\s+", text):
        w = sent.split()
        while len(w) > n:
            if cur:
                out.append(" ".join(cur))
                cur = []
            out.append(" ".join(w[:n]))
            w = w[n:]
        if cur and len(cur) + len(w) > n:
            out.append(" ".join(cur))
            cur = []
        cur += w
    return out + ([" ".join(cur)] if cur else [])


def call(url, body, timeout):
    req = urllib.request.Request(url + "/chat/completions", json.dumps(body).encode(), {"Content-Type": "application/json"})
    t0 = time.perf_counter()
    with urllib.request.urlopen(req, timeout=timeout) as r:
        reply = json.load(r)
    ms = (time.perf_counter() - t0) * 1000
    msg = reply["choices"][0]["message"]
    c = msg.get("content") or ""
    c = c.rsplit("</think>", 1)[-1].strip()
    return c, ms, reply.get("usage") or {}, reply["choices"][0].get("finish_reason")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--url", default="http://127.0.0.1:8899/v1")
    ap.add_argument("--model", default="default_model")
    ap.add_argument("--data", nargs="+", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--prompt", choices=["finetuned", "app"], default="finetuned")
    ap.add_argument("--pipeline", choices=["model", "app"], default="model")
    ap.add_argument("--provider", default="echotype", help="app pipeline: empty reply = no speech only for echotype")
    ap.add_argument("--reasoning-off", action="store_true")
    ap.add_argument("--extra", type=json.loads, default={}, help="extra request fields (JSON)")
    ap.add_argument("--input-field", default="transcript", help="case field to send (e.g. whisper_raw for audio cases)")
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--timeout", type=float, default=120)
    ap.add_argument("--scale-max-tokens", action="store_true")
    ap.add_argument("--chunk-words", type=int, default=0)
    ap.add_argument("--system-extra", default="")
    a = ap.parse_args()

    out = Path(a.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    done = {json.loads(l)["id"] for l in open(out)} if out.exists() else set()
    cases = [json.loads(l) for p in a.data for l in open(p) if l.strip()]
    if a.limit:
        cases = cases[: a.limit]
    todo = [c for c in cases if c["id"] not in done]
    print(f"{len(cases)} cases, {len(todo)} to run -> {out}", flush=True)
    with open(out, "a") as f:
        for k, c in enumerate(todo):
            text = c[a.input_field]
            sysp = system_prompt(c.get("dictionary", []), c.get("translate", False)) if a.prompt == "finetuned" else app_system_prompt(c.get("dictionary", []), c.get("translate", False))
            row = {"id": c["id"], "input": text}
            if a.pipeline == "app":
                if not has_speech(text):
                    f.write(json.dumps({**row, "output": "", "skipped": "no_speech", "ms": 0}, ensure_ascii=False) + "\n")
                    continue
                text = spoken_breaks(text)
            if a.system_extra:
                first, _, rest = sysp.partition("\n")
                sysp = first + "\n" + a.system_extra + ("\n" + rest if rest else "")
            pieces = chunks(text, a.chunk_words) if a.chunk_words and len(text.split()) > a.chunk_words else [text]
            outs, ms, usage, fin, err = [], 0.0, {}, None, None
            for piece in pieces:
                body = {"model": a.model, "temperature": 0, "stream": False,
                        "messages": [{"role": "system", "content": sysp}, {"role": "user", "content": user_message(piece)}], **a.extra}
                if a.scale_max_tokens:
                    body["max_tokens"] = 64 + 4 * len(piece.split())
                if a.reasoning_off:
                    body["reasoning_effort"] = "none"
                try:
                    o, t, u, fin = call(a.url, body, a.timeout)
                    outs.append(o)
                    ms += t
                    for k, v in u.items():
                        if isinstance(v, int):
                            usage[k] = usage.get(k, 0) + v
                except (urllib.error.URLError, TimeoutError, KeyError, json.JSONDecodeError) as e:
                    err = f"{type(e).__name__}: {e}"
                    break
            c_out = None if err else " ".join(o for o in outs if o)
            if err:
                ms = None
            if a.pipeline == "app":
                if err:
                    c_out = text  # cleanup failure pastes the raw transcript
                elif c_out == "" and a.provider != "echotype":
                    c_out = text
            row.update(output=c_out, ms=ms, pieces=len(pieces), prompt_tokens=usage.get("prompt_tokens"), completion_tokens=usage.get("completion_tokens"), finish=fin, error=err)
            f.write(json.dumps(row, ensure_ascii=False) + "\n")
            f.flush()
            if k % 100 == 0:
                print(f"  {k}/{len(todo)} {ms and round(ms)} ms", flush=True)


if __name__ == "__main__":
    main()
