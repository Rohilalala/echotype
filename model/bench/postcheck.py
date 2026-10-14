"""Proposed app-side guards around the cleanup reply, simulated offline on scored runs (nothing here changes the app).

  empty     reply is empty but the transcript has a non-filler word  -> paste the raw transcript
  shrink    transcript >= 40 words and reply keeps < 75% of its words -> paste the raw transcript
  grow      reply has > 1.4x the transcript's words + 5               -> paste the raw transcript
  negation  translate on, transcript has nahi/mat/na/kabhi/नहीं/मत/ne...pas/nicht, reply has no English negation -> raw

A guard that fires turns a critical failure into an uncleaned paste (not critical) and costs exact match when it
fires on a good reply. Reports both, per guard and combined.

  python postcheck.py --scored runs/v1_mlx_adv.scored.jsonl
"""

import argparse
import json
import re

FILLERS = {"um", "umm", "uh", "uhh", "er", "erm", "ah", "hmm", "hm", "mm", "mhm", "like", "so", "matlab", "woh", "yaani", "haan", "mm-hmm", "hmm-mm"}
FILLER_PHRASES = re.compile(r"\b(you know|i mean)\b", re.I)
SRC_NEG = re.compile(r"(?<!\w)(nahi|nahin|mat|na|kabhi|नहीं|मत|ना|कभी|pas|nicht|kein|keine|no)(?!\w)", re.I)
EN_NEG = re.compile(r"(?<!\w)(not|no|never|nobody|nothing|none|neither|nor|without)(?!\w)|n't", re.I)


def words(s):
    return [w for w in re.findall(r"[\w'₹$€%.:@/-]+", (s or "").lower()) if w.strip(".-")]


def content_words(s):
    s = re.sub(r"\[[^\]]*\]|\([^)]*\)", " ", s or "")  # whisper tags: [BLANK_AUDIO], (silence)
    return [w for w in words(FILLER_PHRASES.sub(" ", s)) if w.strip(".") not in FILLERS and any(ch.isalnum() for ch in w)]


def guard(c, out):
    """-> name of the first guard that fires, or None."""
    t = c["transcript"]
    if not (out or "").strip() and content_words(t):
        return "empty"
    n_in, n_out = len(content_words(t)), len(words(out))
    if n_in >= 40 and n_out < 0.75 * n_in:
        return "shrink"
    if n_out > 1.4 * len(words(t)) + 5:
        return "grow"
    if c.get("translate") and SRC_NEG.search(t) and not EN_NEG.search(out or ""):
        return "negation"
    return None


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--scored", required=True)
    a = ap.parse_args()
    rows = [json.loads(l) for l in open(a.scored)]
    fired = {}
    for r in rows:
        g = guard(r, r.get("output"))
        if g:
            fired.setdefault(g, []).append(r)
    crit = sum(r["critical"] for r in rows)
    fixed = sum(r["critical"] for v in fired.values() for r in v)
    lost = sum(r["exact"] or r["harmless"] for v in fired.values() for r in v)
    print(f"{a.scored}: {len(rows)} cases, critical {crit} -> {crit - fixed} with all guards; good replies replaced by raw text: {lost}")
    for g, v in fired.items():
        print(f"  {g:9s} fires {len(v):4d}  fixes {sum(r['critical'] for r in v):3d} critical  replaces {sum(r['exact'] or r['harmless'] for r in v):3d} good replies  e.g. {[r['id'] for r in v[:6]]}")


def _selfcheck():
    assert guard({"transcript": "yes"}, "") == "empty" and guard({"transcript": "um, you know"}, "") is None
    assert guard({"transcript": "[BLANK_AUDIO]"}, "") is None and guard({"transcript": "(silence)"}, "") is None
    assert guard({"transcript": "mat bhejo", "translate": True}, "Send it.") == "negation"
    assert guard({"transcript": "mat bhejo", "translate": True}, "Don't send it.") is None
    print("postcheck self-check ok")


if __name__ == "__main__":
    import sys
    _selfcheck() if sys.argv[1:] == ["--selfcheck"] else main()
