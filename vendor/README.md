# vendor/

`llama-cpp-rs/` (git-ignored) is a clone of <https://github.com/utilityai/llama-cpp-rs> whose `llama-cpp-sys-2/llama.cpp`
submodule is replaced by ggml-org/llama.cpp tag **b11535**, with one stubbed wrapper function
(`wrapper_common.cpp`, MTP speculative decoding, unused). Recreate it with `scripts/windows/setup-vendor.ps1` or `scripts/macos/setup-vendor.sh`.
Bumping llama.cpp = change the tag in both scripts, rebuild, re-run the ignored test in `medialake-llama`.
