#!/bin/sh
# End-to-end pipeline for the Echotype cleanup model. Run from model/. Heavy outputs land in out/ (gitignored).
# Each step checks free RAM first: this runs on a shared 24 GB Mac.
set -eu
PY=.venv/bin
BASE=Qwen/Qwen2.5-1.5B-Instruct

ram_ok() {  # free + inactive + speculative pages, in GB; refuse to load a model under 6 GB
  vm_stat | awk '/Pages free/ {f=$3} /Pages inactive/ {i=$3} /Pages speculative/ {s=$3} END {g=(f+i+s)*16384/2^30; printf "available %.1f GB\n", g; exit (g < 6)}'
}

[ -d .venv ] || { uv venv --python 3.12 .venv && uv pip install --python .venv/bin/python -r pyproject.toml; }

$PY/python gen.py --out out/data                                  # 30k train / 1k valid / 500 held-out
ram_ok && $PY/mlx_lm.lora -c lora.yaml 2>&1 | tee out/train.log      # LoRA, 35-56 min on an M5 Pro (shared machine)
ram_ok && $PY/mlx_lm.fuse --model $BASE --adapter-path out/adapters --save-path out/fused          # bf16, HF layout
$PY/mlx_lm.convert --hf-path out/fused --mlx-path out/fused-4bit -q --q-bits 4 --q-group-size 64   # what mlx_lm.server serves

# GGUF for Ollama / LM Studio (mlx_lm.fuse --export-gguf only handles llama/mistral, so use llama.cpp's converter).
# ponytail: f16 GGUF only; Ollama quantizes at create time (ollama create -q q4_K_M), add llama-quantize if LM Studio needs Q4.
if [ ! -d out/llama.cpp ]; then
  git clone --depth 1 --filter=blob:none --sparse https://github.com/ggml-org/llama.cpp out/llama.cpp
  git -C out/llama.cpp sparse-checkout set gguf-py conversion
fi
uv run --no-project --python 3.12 --with ./out/llama.cpp/gguf-py --with torch --with transformers --with sentencepiece --with numpy \
  python out/llama.cpp/convert_hf_to_gguf.py out/fused --outtype f16 --outfile out/echotype-clean-f16.gguf

ram_ok && $PY/python eval.py --mode none --data eval/handwritten.jsonl out/data/heldout.jsonl
ram_ok && $PY/python eval.py --mode prompted --model mlx-community/Qwen2.5-1.5B-Instruct-4bit --data eval/handwritten.jsonl out/data/heldout.jsonl --out out/res/base.jsonl
ram_ok && $PY/python eval.py --mode finetuned --model out/fused-4bit --data eval/handwritten.jsonl out/data/heldout.jsonl --out out/res/ft.jsonl
