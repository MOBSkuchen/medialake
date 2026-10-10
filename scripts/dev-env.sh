# source this: toolchain paths for building (libclang for bindgen, CMake from VS 2022, per-user .NET SDK)
export LIBCLANG_PATH="$LOCALAPPDATA/Programs/llvm/clang+llvm-23.1.3-x86_64-pc-windows-msvc/bin"
export PATH="/c/Program Files/Microsoft Visual Studio/2022/Community/Common7/IDE/CommonExtensions/Microsoft/CMake/CMake/bin:$LOCALAPPDATA/Microsoft/dotnet:$PATH"
export DOTNET_ROOT="$LOCALAPPDATA/Microsoft/dotnet"
export DOTNET_CLI_TELEMETRY_OPTOUT=1
