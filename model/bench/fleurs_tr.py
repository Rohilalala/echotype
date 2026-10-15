"""Translate-on cases for non-English FLEURS clips: same audio, reference = the en_us transcription of the same sentence id.

  python fleurs_tr.py --asr cases/asr-real.jsonl --out cases/asr-fleurs-tr.jsonl
"""

import argparse
import json

ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
ap.add_argument("--asr", required=True)
ap.add_argument("--out", required=True)
a = ap.parse_args()
rows = [json.loads(l) for l in open(a.asr)]
en = {r["sentence_id"]: r["reference_raw"] for r in rows if r["source"] == "fleurs_en"}
out = [{**r, "id": r["id"] + "+tr", "translate": True, "reference": en[r["sentence_id"]], "reference_raw": en[r["sentence_id"]], "lang": "en",
        "source": r["source"] + "+tr", "tags": r["tags"] + ["translate"]} for r in rows if r["source"].startswith("fleurs_") and r["source"] != "fleurs_en" and r.get("sentence_id") in en]
with open(a.out, "w") as f:
    for r in out:
        f.write(json.dumps(r, ensure_ascii=False) + "\n")
print(f"{len(out)} translate-on FLEURS cases with English references -> {a.out}")
