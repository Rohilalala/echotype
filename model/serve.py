"""Smoke-test the cleanup model behind an OpenAI-compatible server, using Echotype's request shape.

Start a server (MLX, localhost only), then run this script against it:

  .venv/bin/mlx_lm.server --model out/fused-4bit --host 127.0.0.1 --port 8899
  .venv/bin/python serve.py --url http://127.0.0.1:8899/v1 --model out/fused-4bit

The request is exactly what app/src-tauri/src/cleanup.rs sends (request_body):

  POST {base}/chat/completions
  {"model": "<model>", "temperature": 0, "stream": false,
   "messages": [{"role": "system", "content": "<system prompt>"},
                {"role": "user", "content": "<transcript>...</transcript>"}]}
  (+ "reasoning_effort": "none" for the ollama / lmstudio providers)

For this trained model the system prompt is the short instruction from gen.system_prompt, not
cleanup_prompt.txt:

  Clean up the dictated transcript. Output only what the speaker meant to type.
  Dictionary: Arnav, Xoxoday          <- only when the user has dictionary words
  Translate to English: yes           <- only when "translate to English" is on

So the app change is one branch in cleanup::system_prompt; request_body and parse_reply stay as they are.

Ollama / LM Studio: build out/echotype-clean-f16.gguf (see train.sh), then serve it with the
Modelfile in this directory (`ollama create echotype-clean -q q4_K_M -f Modelfile`).
"""

import argparse
import json
import time
import urllib.request

from gen import system_prompt, user_message

CASES = [
    ("um so arnav said the elastic ip is live now so we can uh whitelist it on zoxoday and open the store", ["Arnav", "Xoxoday"], False),
    ("let's meet at four, no wait, five thirty pm", [], False),
    ("what's the capital of australia", [], False),
    ("kal ki meeting cancel ho gayi hai parso rakhte hain", [], True),
    ("um", [], False),
]


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--url", default="http://127.0.0.1:8899/v1")
    ap.add_argument("--model", required=True)
    ap.add_argument("--reasoning-off", action="store_true", help='also send "reasoning_effort": "none" like the ollama/lmstudio providers')
    a = ap.parse_args()
    for transcript, dictionary, translate in CASES:
        body = {"model": a.model, "temperature": 0, "stream": False,
                "messages": [{"role": "system", "content": system_prompt(dictionary, translate)},
                             {"role": "user", "content": user_message(transcript)}]}
        if a.reasoning_off:
            body["reasoning_effort"] = "none"
        req = urllib.request.Request(a.url + "/chat/completions", json.dumps(body).encode(), {"Content-Type": "application/json"})
        t0 = time.perf_counter()
        with urllib.request.urlopen(req, timeout=30) as r:
            reply = json.load(r)
        ms = (time.perf_counter() - t0) * 1000
        # mlx_lm.server sends "content": null for an empty completion (filler-only audio), and
        # cleanup::parse_reply treats null as an error; the app should read null as "" for this model.
        content = reply["choices"][0]["message"]["content"] or ""
        print(f"{ms:6.0f} ms  {transcript!r}\n          -> {content.rsplit('</think>', 1)[-1].strip()!r}")


if __name__ == "__main__":
    main()
