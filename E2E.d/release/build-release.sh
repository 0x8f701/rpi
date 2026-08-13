#!/usr/bin/env bash
set -euo pipefail
# Native C/C++ sources (rquickjs, tree-sitter) are compiled by the cc build
# scripts, which ignore rustc remapping and embed the absolute source path
# via __FILE__-style macros, so the same roots are also remapped for the C
# compiler through CFLAGS/CXXFLAGS (-ffile-prefix-map/-fmacro-prefix-map).
# Incoming CFLAGS/CXXFLAGS are preserved ahead of the remap flags.
#
# The remap list is ordered most-specific-first because rustc applies the
# first matching --remap-path-prefix. No workstation path is hardcoded; the
# roots are read from the environment at invocation time. Remap flags do not
# contain spaces; rustc flags travel as unit-separated arguments and the C
# flags as a space-separated string (the cc crate's own format), so paths
# with spaces would survive as well.
#
# Usage:
#   E2E.d/release/build-release.sh [cargo args...]
#
# Example (release workflow / local release-dist build):
#   E2E.d/release/build-release.sh +1.88.0 build --package pi-cli --bin rpi \
#     --profile release-dist --locked [--target <triple>]
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(CDPATH= cd -- "$SCRIPT_DIR/../.." && pwd -P)"

home_root="${HOME:-}"
cargo_home_root="${CARGO_HOME:-${home_root:+$home_root/.cargo}}"
rustup_home_root="${RUSTUP_HOME:-${home_root:+$home_root/.rustup}}"

remap_flags=()
[ -n "$REPO_ROOT" ] && remap_flags+=("--remap-path-prefix=$REPO_ROOT=/pi-src")
[ -n "$cargo_home_root" ] && remap_flags+=("--remap-path-prefix=$cargo_home_root=/pi-cargo-home")
[ -n "$rustup_home_root" ] && remap_flags+=("--remap-path-prefix=$rustup_home_root=/pi-rustup-home")
[ -n "$home_root" ] && remap_flags+=("--remap-path-prefix=$home_root=/pi-home")

# Same roots for the C/C++ compiler (cc build scripts). -ffile-prefix-map
# already implies -fmacro-prefix-map on GCC 8+/Clang 10+; both are emitted
# explicitly for older toolchains.
compiler_remap_flags=()
[ -n "$REPO_ROOT" ] && compiler_remap_flags+=("-ffile-prefix-map=$REPO_ROOT=/pi-src" "-fmacro-prefix-map=$REPO_ROOT=/pi-src")
[ -n "$cargo_home_root" ] && compiler_remap_flags+=("-ffile-prefix-map=$cargo_home_root=/pi-cargo-home" "-fmacro-prefix-map=$cargo_home_root=/pi-cargo-home")
[ -n "$rustup_home_root" ] && compiler_remap_flags+=("-ffile-prefix-map=$rustup_home_root=/pi-rustup-home" "-fmacro-prefix-map=$rustup_home_root=/pi-rustup-home")
[ -n "$home_root" ] && compiler_remap_flags+=("-ffile-prefix-map=$home_root=/pi-home" "-fmacro-prefix-map=$home_root=/pi-home")

# RUSTFLAGS is space-separated by definition (cargo splits on whitespace);
# merge it ahead of the remap flags so caller flags win the same way they did
# before CARGO_ENCODED_RUSTFLAGS took precedence.
incoming_flags=()
if [ -n "${RUSTFLAGS:-}" ]; then
    read -r -a incoming_flags <<< "$RUSTFLAGS"
fi

all_flags=(${incoming_flags[@]+"${incoming_flags[@]}"} ${remap_flags[@]+"${remap_flags[@]}"})
encoded=""
for flag in "${all_flags[@]}"; do
    if [ -n "$encoded" ]; then
        encoded+=$'\x1f'
    fi
    encoded+="$flag"
done
export CARGO_ENCODED_RUSTFLAGS="$encoded"

# CFLAGS/CXXFLAGS are space-separated (the cc crate's format); preserve any
# incoming flags ahead of the compiler remap flags.
incoming_cflags=()
if [ -n "${CFLAGS:-}" ]; then
    read -r -a incoming_cflags <<< "$CFLAGS"
fi
incoming_cxxflags=()
if [ -n "${CXXFLAGS:-}" ]; then
    read -r -a incoming_cxxflags <<< "$CXXFLAGS"
fi
all_cflags=(${incoming_cflags[@]+"${incoming_cflags[@]}"} ${compiler_remap_flags[@]+"${compiler_remap_flags[@]}"})
export CFLAGS="${all_cflags[*]}"
all_cxxflags=(${incoming_cxxflags[@]+"${incoming_cxxflags[@]}"} ${compiler_remap_flags[@]+"${compiler_remap_flags[@]}"})
export CXXFLAGS="${all_cxxflags[*]}"

exec cargo "$@"
