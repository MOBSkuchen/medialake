# Recreates vendor/llama-cpp-rs (llama-cpp-rs + llama.cpp at a pinned tag). Needs git.
param([string]$LlamaTag = "b11535")
$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent
$dst = Join-Path $root "vendor\llama-cpp-rs"
if (Test-Path $dst) { Write-Host "vendor already present: $dst"; exit 0 }
git config --global core.longpaths true
git clone --depth 1 https://github.com/utilityai/llama-cpp-rs $dst
$sub = Join-Path $dst "llama-cpp-sys-2\llama.cpp"
if (Test-Path $sub) { Remove-Item -Recurse -Force $sub }
git clone --depth 1 --branch $LlamaTag https://github.com/ggml-org/llama.cpp $sub
Write-Host "NOTE: if the build fails in llama-cpp-sys-2/wrapper_common.cpp (common_speculative_process), make that function return LLAMA_RS_STATUS_EXCEPTION."
