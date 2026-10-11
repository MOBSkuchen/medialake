# source this (macOS): toolchain for building. Needs Xcode (swift, codesign, pkgbuild) and `brew install cmake`.
# bindgen normally finds the libclang that ships with Xcode; a Homebrew LLVM is used when present.
export MACOSX_DEPLOYMENT_TARGET=14.0
[ -x /opt/homebrew/bin/brew ] && export PATH="/opt/homebrew/bin:$PATH"
if command -v brew >/dev/null 2>&1; then
    export PATH="$(brew --prefix)/bin:$PATH"
    if [ -d "$(brew --prefix)/opt/llvm/lib" ]; then export LIBCLANG_PATH="$(brew --prefix)/opt/llvm/lib"; fi
fi
