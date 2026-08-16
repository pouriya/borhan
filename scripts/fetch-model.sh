#!/usr/bin/env bash
# Vendors the embedding model into models/. Nothing in the Rust build talks to
# HuggingFace (model2vec-rs is built with `local-only`), so this is the only
# place the model is fetched — run it once after cloning.
set -euo pipefail

MODEL="${MODEL:-potion-base-8M}"
DEST="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/models/$MODEL"
BASE="https://huggingface.co/minishlab/$MODEL/resolve/main"

mkdir -p "$DEST"
for f in config.json tokenizer.json model.safetensors; do
    if [[ -s "$DEST/$f" ]]; then
        echo "  have  $f"
    else
        echo "  get   $f"
        curl -sSL --fail -o "$DEST/$f.part" "$BASE/$f"
        mv "$DEST/$f.part" "$DEST/$f"
    fi
done

echo "ready: $DEST ($(du -sh "$DEST" | cut -f1))"
