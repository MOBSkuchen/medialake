#!/usr/bin/env bash
# Recreates vendor/llama-cpp-rs (llama-cpp-rs + llama.cpp at a pinned tag). Needs git.
#   scripts/macos/setup-vendor.sh [llama-tag]
set -euo pipefail
llama_tag="${1:-b11535}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
dst="$root/vendor/llama-cpp-rs"
if [ -d "$dst" ]; then echo "vendor already present: $dst"; exit 0; fi
git clone --depth 1 https://github.com/utilityai/llama-cpp-rs "$dst"
sub="$dst/llama-cpp-sys-2/llama.cpp"
rm -rf "$sub"
git clone --depth 1 --branch "$llama_tag" https://github.com/ggml-org/llama.cpp "$sub"
echo "NOTE: if the build fails in llama-cpp-sys-2/wrapper_common.cpp (common_speculative_process), make that function return LLAMA_RS_STATUS_EXCEPTION."
