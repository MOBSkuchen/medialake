# vendor/

`llama-cpp-rs/` (git-ignored) is a clone of <https://github.com/utilityai/llama-cpp-rs> whose `llama-cpp-sys-2/llama.cpp`
submodule is replaced by ggml-org/llama.cpp tag **b11535**, with one stubbed wrapper function
(`wrapper_common.cpp`, MTP speculative decoding, unused). Recreate it with `scripts/setup-vendor.ps1`.
Bumping llama.cpp = change the tag in that script, rebuild, re-run the ignored test in `searchnow-llama`.
