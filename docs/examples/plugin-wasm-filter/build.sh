#!/bin/sh
# Assemble filter.wat -> filter.wasm with wasm-tools (or wabt's wat2wasm).
# Tervo intentionally does not parse WAT at runtime.
set -eu
cd "$(dirname "$0")"
if command -v wasm-tools >/dev/null 2>&1; then
    wasm-tools parse filter.wat -o filter.wasm
elif command -v wat2wasm >/dev/null 2>&1; then
    wat2wasm filter.wat -o filter.wasm
else
    echo "need wasm-tools (cargo install wasm-tools) or wat2wasm (wabt)" >&2
    exit 1
fi
ls -l filter.wasm
