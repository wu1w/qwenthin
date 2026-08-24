#!/bin/sh
set -eu
ROOT="$(CDPATH= cd -- "$(dirname "$0")/../../.." && pwd)"
cd "$ROOT"

cargo build --release -p q38-cli

# llvm-lib / lld-link live in keg-only Homebrew formulas on macOS.
if [ -d /opt/homebrew/opt/llvm/bin ]; then
  PATH="/opt/homebrew/opt/llvm/bin:${PATH}"
fi
if [ -d /opt/homebrew/opt/lld/bin ]; then
  PATH="/opt/homebrew/opt/lld/bin:${PATH}"
fi
export PATH

# Windows sidecar: MSVC via cargo-xwin, else mingw-gnu if the linker exists.
if command -v cargo-xwin >/dev/null 2>&1 || cargo xwin --help >/dev/null 2>&1; then
  rustup target add x86_64-pc-windows-msvc >/dev/null
  cargo xwin build --release -p q38-cli --target x86_64-pc-windows-msvc
elif command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1; then
  rustup target add x86_64-pc-windows-gnu >/dev/null
  cargo build --release -p q38-cli --target x86_64-pc-windows-gnu
else
  echo "skip Windows q38.exe (install cargo-xwin or mingw-w64 to cross-compile)" >&2
fi
