#!/usr/bin/env bash
# Vendors a corpus into seed/, for scanning into a real storage with
# `make seed`. Run by the Makefile; nothing in the Rust build fetches anything.
#
# The default corpus is rust-lang/rfcs: 639 self-contained Markdown documents,
# ~1.3M words, heavy on fenced code, tables and nested lists — which is what
# `memory add` has to take apart. Deliberately *not* rust-lang/book, whose
# source has 707 mdBook `{{#rustdoc_include}}` directives where the code should
# be, so scanning it would file placeholder lines as Rust.
#
# Cloned rather than downloaded as a tarball because codeload.github.com answers
# unauthenticated tarball requests with 429 often enough to make a build target
# that depends on it useless. `--depth 1 --filter=blob:none --sparse` fetches
# only the newest revision of only the one directory that is wanted.
set -euo pipefail

REPO="${1:?usage: fetch-seed.sh REPO DEST PATH}"
DEST="${2:?usage: fetch-seed.sh REPO DEST PATH}"
SUBDIR="${3:?usage: fetch-seed.sh REPO DEST PATH}"

if [[ -d "$DEST/.git" ]]; then
    echo "  have  $DEST"
    git -C "$DEST" fetch --depth 1 origin HEAD
    git -C "$DEST" reset --hard FETCH_HEAD
else
    echo "  get   $REPO"
    rm -rf "$DEST"
    git clone --depth 1 --filter=blob:none --sparse "$REPO" "$DEST"
    git -C "$DEST" sparse-checkout set "$SUBDIR"
fi

count=$(find "$DEST/$SUBDIR" -name '*.md' | wc -l)
if [[ "$count" -eq 0 ]]; then
    echo "FAIL: no .md files under $DEST/$SUBDIR" >&2
    exit 1
fi
echo "ready: $DEST/$SUBDIR ($count documents, $(du -sh "$DEST/$SUBDIR" | cut -f1))"
