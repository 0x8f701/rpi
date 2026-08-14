#!/bin/sh
# Focused checks for the Windows release helper
# (E2E.d/release/build-release.ps1) and its gate (path-leak-check.ps1), in
# the style of tests/install-sh-static.sh. No build is performed; the checks
# assert the source contracts that keep MSVC release builds free of builder
# paths:
#
#   build-release.ps1
#     - /pathmap: (the C# compiler's PathMap; cl.exe ignores it) is gone
#     - cl.exe scrubs __FILEW__ (assert/_wassert records) via one
#       /d1trimfile:<prefix> per root, trailing separator appended so
#       C:\Users\alice\ cannot match C:\Users\aliceX\
#     - CC_SHELL_ESCAPED_FLAGS=1 is set and every composed C/C++ flag is
#       serialized as one POSIX single-quoted token, so /d1trimfile prefixes
#       whose roots contain spaces survive cc-rs shlex parsing as exactly one
#       argument (cc-1.4.0 documents that individual CFLAGS cannot contain
#       spaces unless CC_SHELL_ESCAPED_FLAGS is set)
#     - incoming CFLAGS/CXXFLAGS pass through verbatim (never space-split,
#       which would lose quoted grouping) ahead of the composed flags; a
#       shlex quote/backslash guard makes malformed incoming flags fail
#       loudly instead of silently dropping the composed /d1trimfile flags
#     - the composed suffix is also exported through the scoped
#       HOST_CFLAGS/TARGET_CFLAGS/HOST_CXXFLAGS/TARGET_CXXFLAGS (suffix
#       only, never incoming+suffix): rquickjs-sys 0.12.2 overwrites CFLAGS
#       before cc-rs reads it, and cc-rs concatenates CFLAGS + HOST_CFLAGS +
#       TARGET_CFLAGS + CFLAGS_<target>
#     - an unquoted word-start '#' in incoming CFLAGS/CXXFLAGS (a shlex 2.x
#       comment that would silently drop the composed flags) fails loudly;
#       foo#bar and quoted '#' stay literal with the suffix intact
#     - cl/cl.exe/clang-cl/clang-cl.exe (optionally wrapper-prefixed) select
#       the MSVC branch; the GCC/Clang branch is unchanged
#     - no NDEBUG: C/C++ assertions stay active (paths are trimmed, not
#       disabled)
#     - incoming RUSTFLAGS preserved; rustc --remap-path-prefix and
#       CARGO_ENCODED_RUSTFLAGS unchanged
#   path-leak-check.ps1
#     - still fails on any forbidden builder-path occurrence
#     - still scans NUL-interleaved UTF-16LE (wide __FILEW__) strings
#
# When pwsh is available the helper's -DumpFlags test mode is exercised with
# synthetic roots containing spaces, the computed CFLAGS/CXXFLAGS are
# re-parsed with POSIX shlex semantics (python3's shlex when present, the
# POSIX shell itself otherwise), and the exact token lists are asserted;
# without pwsh only the guarded source-contract checks above run and no
# runtime proof is claimed.
set -eu
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"
PS1="$REPO_ROOT/E2E.d/release/build-release.ps1"
GATE_PS1="$REPO_ROOT/E2E.d/release/path-leak-check.ps1"
ROOT="$(mktemp -d)"
trap 'rm -rf "$ROOT"' EXIT HUP INT TERM

# --- build-release.ps1: MSVC C/C++ path scrubbing --------------------------

# The ineffective /pathmap: flag construction must be gone (the explanatory
# comment may keep mentioning the option).
if grep -Fq '+= "/pathmap:' "$PS1"; then
    printf 'release-helper-static: /pathmap: (cl.exe ignores it) flag still composed in %s\n' "$PS1" >&2
    exit 1
fi

# One /d1trimfile:<prefix> per root, built with a trailing separator so a
# root cannot match a sibling prefix (C:\Users\alice\ vs C:\Users\aliceX\).
cat > "$ROOT/d1trimfile-flag.txt" <<'EOF'
if ($root) { $compilerRemapFlags += "/d1trimfile:$($root.TrimEnd([char]'\'))\" }
EOF
grep -Fqf "$ROOT/d1trimfile-flag.txt" "$PS1"

# Release C assertions stay enabled: no NDEBUG define is composed.
if grep -Fq 'NDEBUG' "$PS1"; then
    printf 'release-helper-static: NDEBUG would disable C assertions; build-release.ps1 must trim paths instead\n' >&2
    exit 1
fi

# --- build-release.ps1: shell-escaped flag contract (cc-rs Shlex) -----------

# CC_SHELL_ESCAPED_FLAGS=1 must be forced: without it the cc crate
# (cc-1.4.0 src/lib.rs:69-92,4239-4256) splits CFLAGS/CXXFLAGS on
# whitespace and individual flags cannot contain spaces.
grep -Fq '$env:CC_SHELL_ESCAPED_FLAGS = '\''1'\''' "$PS1"

# Incoming CFLAGS/CXXFLAGS must never be space-split first (that would lose
# quoted grouping); they pass through verbatim under the same shlex contract.
if grep -Fq '$env:CFLAGS.Split(' "$PS1" || grep -Fq '$env:CXXFLAGS.Split(' "$PS1"; then
    printf 'release-helper-static: incoming CFLAGS/CXXFLAGS are still space-split, which loses quoted grouping under CC_SHELL_ESCAPED_FLAGS\n' >&2
    exit 1
fi
grep -Fq '$env:CFLAGS = $env:CFLAGS + ' "$PS1"
grep -Fq '$env:CXXFLAGS = $env:CXXFLAGS + ' "$PS1"

# Every composed flag is serialized as one POSIX single-quoted token, and a
# shlex quote/backslash guard fails loudly on malformed incoming flags
# (cc-rs's Shlex iteration stops on those errors and would silently drop the
# appended /d1trimfile flags).
grep -Fq 'function ConvertTo-ShlexArg' "$PS1"
grep -Fq '$Arg.Replace' "$PS1"
grep -Fq 'function Test-ShlexComplete' "$PS1"

# rquickjs-sys 0.12.2 overwrites CFLAGS (build.rs:186-191) before cc-rs reads
# it, and cc-rs concatenates CFLAGS + HOST_CFLAGS + TARGET_CFLAGS +
# CFLAGS_<target> (cc-1.4.0 src/lib.rs:4214-4228,4240-4259), so the composed
# remap suffix must also be exported through the scoped HOST_/TARGET_ vars.
# They are assigned the suffix only - never incoming+suffix - or cc-rs would
# pass the caller's flags twice for every crate that keeps CFLAGS/CXXFLAGS.
grep -Fq '$env:HOST_CFLAGS = $remapSuffix' "$PS1"
grep -Fq '$env:TARGET_CFLAGS = $remapSuffix' "$PS1"
grep -Fq '$env:HOST_CXXFLAGS = $remapSuffix' "$PS1"
grep -Fq '$env:TARGET_CXXFLAGS = $remapSuffix' "$PS1"

# shlex 2.x starts a comment at an unquoted '#' that begins a word
# (shlex-2.0.1/src/bytes.rs:138-146), consuming the rest of the line and
# silently dropping the appended /d1trimfile flags; Test-ShlexComplete must
# reject it while keeping foo#bar, '#...' and "#..." literal.
grep -Fq '$atWordStart' "$PS1"
grep -Fq 'treats it as a comment' "$PS1"

# --- build-release.ps1: compiler selection and Rust remap preserved ---------

# cl/cl.exe/clang-cl/clang-cl.exe (optionally prefixed by a toolchain path or
# an sccache-style wrapper, optionally followed by driver args) select the
# MSVC branch; the match is boundary-anchored so clang/myclang are not
# mistaken for cl.
grep -Fq '$isMsvc = ($env:CC -match' "$PS1"
grep -Fq '(^|[\s\\/])cl(\.exe)?(\s|$)' "$PS1"
grep -Fq '(^|[\s\\/])clang-cl(\.exe)?(\s|$)' "$PS1"

cat > "$ROOT/rust-remap.txt" <<'EOF'
$remapFlags += "--remap-path-prefix=$repoRoot=/pi-src"
$allFlags = @($incomingFlags) + @($remapFlags)
$env:CARGO_ENCODED_RUSTFLAGS = ($allFlags -join [char]0x1F)
EOF
grep -Fqf "$ROOT/rust-remap.txt" "$PS1"

# GCC/Clang branch untouched.
grep -Fq -- '-ffile-prefix-map=$repoRoot=/pi-src' "$PS1"

# --- path-leak-check.ps1: strict gate intact --------------------------------

# Fails when any category count is nonzero; matching strings are never printed.
grep -Fq 'throw "path-leak-check: $total forbidden builder-path occurrence(s) in $Binary (see counts above)"' "$GATE_PS1"
# Wide-string (UTF-16LE, __FILEW__) scan still present.
grep -Fq '$widePrefix = ($prefix.ToCharArray() -join [char]0)' "$GATE_PS1"

# --- PowerShell syntax + behavioral gates (when pwsh is available) ----------

behavioral=0
if command -v pwsh >/dev/null 2>&1; then
    BUILDER_PS1="$PS1" GATE_PS1="$GATE_PS1" pwsh -NoProfile -Command '$errors = $null; foreach ($file in @($env:BUILDER_PS1, $env:GATE_PS1)) { [System.Management.Automation.Language.Parser]::ParseFile((Resolve-Path $file), [ref]$null, [ref]$errors) | Out-Null; if ($errors.Count -ne 0) { $errors | ForEach-Object { Write-Error $_ }; exit 1 } }'

    # Re-parse a serialized flag string with POSIX shlex semantics (the cc
    # crate's shlex 2.x is a POSIX-shell lexer). python3's shlex is the
    # documented reference implementation; the POSIX shell itself is the
    # fallback (shlex 2.x documents compatibility with bash/dash/ash/mksh).
    # The strings are produced by the helper under test from fixed test
    # values, so no untrusted input reaches the eval fallback.
    if command -v python3 >/dev/null 2>&1; then
        tokenize() {
            printf '%s\n' "$1" | python3 -c 'import shlex, sys; [print(t) for t in shlex.split(sys.stdin.read())]'
        }
    else
        tokenize() {
            printf '%s\n' "$1" | sh -c 'IFS= read -r line; eval "set -- $line"; for a do printf "%s\n" "$a"; done'
        }
    fi

    # Run the helper in test mode with synthetic roots containing spaces and
    # incoming flags whose quoted grouping must survive the round trip.
    # The fake Windows $HOME makes pwsh itself create a <cwd>/C:\... directory
    # (PSReadLine/telemetry caches), so pwsh runs from inside the temp dir.
    DUMP="$ROOT/dump.txt"
    if ! ( cd "$ROOT" && HOME='C:\Users\Alice Smith' USERPROFILE='C:\Users\Alice Smith' \
        CARGO_HOME='C:\Cargo Cache\home' RUSTUP_HOME='C:\Rust Up\home' \
        CC='cl.exe' CFLAGS='-O2 -DFOO="bar baz"' CXXFLAGS='/arch:AVX2 -D_WIN32_WINNT=0x0A00' \
        pwsh -NoProfile -File "$PS1" -DumpFlags > "$DUMP" ); then
        printf 'release-helper-static: pwsh -DumpFlags run failed (see output above)\n' >&2
        exit 1
    fi

    escaped="$(sed -n 's/^CC_SHELL_ESCAPED_FLAGS=//p' "$DUMP")"
    if [ "$escaped" != '1' ]; then
        printf 'release-helper-static: CC_SHELL_ESCAPED_FLAGS was not exported as 1 by -DumpFlags\n' >&2
        exit 1
    fi

    # Roots the helper actually used, in remap order (repoRoot, CARGO_HOME,
    # RUSTUP_HOME, HOME), skipping empty ones.
    sed -n 's/^DUMPFLAGS_ROOT_[0-9]*=//p' "$DUMP" > "$ROOT/roots.txt"
    if [ ! -s "$ROOT/roots.txt" ]; then
        printf 'release-helper-static: -DumpFlags reported no remap roots\n' >&2
        exit 1
    fi

    # Expected tokens: incoming flags re-parsed under the same shlex contract,
    # then exactly one /d1trimfile:<root>\ per root, in remap order. The
    # remap-only suffix is the expected value of every compiler channel.
    while IFS= read -r root; do
        printf '/d1trimfile:%s\\\n' "$root"
    done < "$ROOT/roots.txt" > "$ROOT/expected-remap.tokens"
    {
        printf '%s\n' '-O2' '-DFOO=bar baz'
        cat "$ROOT/expected-remap.tokens"
    } > "$ROOT/expected-cflags.tokens"
    {
        printf '%s\n' '/arch:AVX2' '-D_WIN32_WINNT=0x0A00'
        cat "$ROOT/expected-remap.tokens"
    } > "$ROOT/expected-cxxflags.tokens"

    cflags="$(sed -n 's/^CFLAGS=//p' "$DUMP")"
    cxxflags="$(sed -n 's/^CXXFLAGS=//p' "$DUMP")"
    tokenize "$cflags" > "$ROOT/cflags.tokens"
    tokenize "$cxxflags" > "$ROOT/cxxflags.tokens"

    if ! diff -u "$ROOT/expected-cflags.tokens" "$ROOT/cflags.tokens"; then
        printf 'release-helper-static: CFLAGS did not re-parse to the exact expected tokens (diff above): one /d1trimfile:<root>\\ per root with trailing separator required\n' >&2
        exit 1
    fi
    if ! diff -u "$ROOT/expected-cxxflags.tokens" "$ROOT/cxxflags.tokens"; then
        printf 'release-helper-static: CXXFLAGS did not re-parse to the exact expected tokens (diff above)\n' >&2
        exit 1
    fi

    # The scoped HOST_/TARGET_ vars must each tokenize to exactly the remap
    # tokens (suffix only): rquickjs-sys overwrites CFLAGS, and cc-rs
    # concatenates CFLAGS + HOST_CFLAGS + TARGET_CFLAGS + CFLAGS_<target>, so
    # /d1trimfile must survive through the scoped channel without duplicating
    # the incoming flags for crates that keep CFLAGS/CXXFLAGS.
    for var in HOST_CFLAGS TARGET_CFLAGS HOST_CXXFLAGS TARGET_CXXFLAGS; do
        val="$(sed -n "s/^$var=//p" "$DUMP")"
        tokenize "$val" > "$ROOT/$var.tokens"
        if ! diff -u "$ROOT/expected-remap.tokens" "$ROOT/$var.tokens"; then
            printf 'release-helper-static: %s did not re-parse to exactly the remap tokens (diff above): scoped vars must carry the /d1trimfile suffix only\n' "$var" >&2
            exit 1
        fi
    done

    # Explicit invariants on top of the exact-list diff: exactly one
    # /d1trimfile token per root, each with the trailing separator, and at
    # least one /d1trimfile token whose root contains a space survives as a
    # single argument.
    root_count="$(wc -l < "$ROOT/roots.txt")"
    trimfile_count="$(grep -c '^/d1trimfile:' "$ROOT/cflags.tokens" || true)"
    trimfile_count="${trimfile_count:-0}"
    if [ "$trimfile_count" -ne "$root_count" ]; then
        printf 'release-helper-static: expected %s /d1trimfile tokens, found %s\n' "$root_count" "$trimfile_count" >&2
        exit 1
    fi
    if grep '^/d1trimfile:' "$ROOT/cflags.tokens" | grep -v '\\$' | grep -q .; then
        printf 'release-helper-static: a /d1trimfile token lacks the required trailing separator\n' >&2
        exit 1
    fi
    if ! grep -q '^/d1trimfile:.* .*\\$' "$ROOT/cflags.tokens"; then
        printf 'release-helper-static: no /d1trimfile token contained a space; the synthetic spaced roots were not exercised\n' >&2
        exit 1
    fi

    # The shlex guard must fail loudly on incoming flags that would stop
    # cc-rs's Shlex mid-string and silently drop the composed /d1trimfile
    # flags (trailing unescaped backslash and unterminated quote).
    if ( cd "$ROOT" && HOME='C:\Users\Alice Smith' CARGO_HOME='C:\Cargo Cache\home' RUSTUP_HOME='C:\Rust Up\home' CC='cl.exe' CFLAGS='-DFOO=C:\' pwsh -NoProfile -File "$PS1" -DumpFlags >/dev/null 2>&1 ); then
        printf 'release-helper-static: guard did not reject incoming CFLAGS ending in an unescaped backslash\n' >&2
        exit 1
    fi
    if ( cd "$ROOT" && HOME='C:\Users\Alice Smith' CARGO_HOME='C:\Cargo Cache\home' RUSTUP_HOME='C:\Rust Up\home' CC='cl.exe' CFLAGS='-DFOO="unterminated' pwsh -NoProfile -File "$PS1" -DumpFlags >/dev/null 2>&1 ); then
        printf 'release-helper-static: guard did not reject incoming CFLAGS with an unterminated quote\n' >&2
        exit 1
    fi

    # shlex 2.x treats an unquoted word-start '#' as a comment consuming the
    # rest of the line (bytes.rs:138-146), silently dropping the composed
    # /d1trimfile flags; the guard must fail loudly. The rejection is
    # asserted by the helper's exit status - python3's shlex.split with
    # comments=False is not a valid oracle for this drop.
    if ( cd "$ROOT" && HOME='C:\Users\Alice Smith' CARGO_HOME='C:\Cargo Cache\home' RUSTUP_HOME='C:\Rust Up\home' CC='cl.exe' CFLAGS='-O2 # note' pwsh -NoProfile -File "$PS1" -DumpFlags >/dev/null 2>&1 ); then
        printf 'release-helper-static: guard did not reject incoming CFLAGS with an unquoted word-start # (shlex comment)\n' >&2
        exit 1
    fi

    # Quoted and mid-token '#' are literal for shlex and must pass through
    # with the /d1trimfile suffix intact.
    if ! ( cd "$ROOT" && HOME='C:\Users\Alice Smith' CARGO_HOME='C:\Cargo Cache\home' RUSTUP_HOME='C:\Rust Up\home' CC='cl.exe' CFLAGS='-DFOO="bar # baz"' pwsh -NoProfile -File "$PS1" -DumpFlags > "$ROOT/quoted-hash.txt" ); then
        printf 'release-helper-static: guard rejected incoming CFLAGS with a double-quoted #\n' >&2
        exit 1
    fi
    quoted_cflags="$(sed -n 's/^CFLAGS=//p' "$ROOT/quoted-hash.txt")"
    {
        printf '%s\n' '-DFOO=bar # baz'
        cat "$ROOT/expected-remap.tokens"
    } > "$ROOT/expected-quoted.tokens"
    tokenize "$quoted_cflags" > "$ROOT/quoted.tokens"
    if ! diff -u "$ROOT/expected-quoted.tokens" "$ROOT/quoted.tokens"; then
        printf 'release-helper-static: CFLAGS with a quoted # did not keep the exact tokens incl. the /d1trimfile suffix (diff above)\n' >&2
        exit 1
    fi

    if ! ( cd "$ROOT" && HOME='C:\Users\Alice Smith' CARGO_HOME='C:\Cargo Cache\home' RUSTUP_HOME='C:\Rust Up\home' CC='cl.exe' CFLAGS='-O2#note' pwsh -NoProfile -File "$PS1" -DumpFlags > "$ROOT/midhash.txt" ); then
        printf 'release-helper-static: guard rejected incoming CFLAGS with a mid-token #\n' >&2
        exit 1
    fi
    mid_cflags="$(sed -n 's/^CFLAGS=//p' "$ROOT/midhash.txt")"
    {
        printf '%s\n' '-O2#note'
        cat "$ROOT/expected-remap.tokens"
    } > "$ROOT/expected-midhash.tokens"
    tokenize "$mid_cflags" > "$ROOT/midhash.tokens"
    if ! diff -u "$ROOT/expected-midhash.tokens" "$ROOT/midhash.tokens"; then
        printf 'release-helper-static: CFLAGS with a mid-token # did not keep the exact tokens incl. the /d1trimfile suffix (diff above)\n' >&2
        exit 1
    fi

    # Compiler selection: clang-cl selects the MSVC /d1trimfile branch; a
    # GCC-style cross compiler takes the -ffile-prefix-map branch and never
    # emits /d1trimfile.
    if ! ( cd "$ROOT" && HOME='C:\Users\Alice Smith' CARGO_HOME='C:\Cargo Cache\home' RUSTUP_HOME='C:\Rust Up\home' CC='clang-cl' pwsh -NoProfile -File "$PS1" -DumpFlags 2>/dev/null | grep -q '/d1trimfile:' ); then
        printf 'release-helper-static: CC=clang-cl did not select the /d1trimfile branch\n' >&2
        exit 1
    fi
    GCC_DUMP="$ROOT/gcc-dump.txt"
    if ! ( cd "$ROOT" && HOME='C:\Users\Alice Smith' CARGO_HOME='C:\Cargo Cache\home' RUSTUP_HOME='C:\Rust Up\home' CC='x86_64-w64-mingw32-gcc' pwsh -NoProfile -File "$PS1" -DumpFlags > "$GCC_DUMP" ); then
        printf 'release-helper-static: pwsh -DumpFlags (CC=gcc-style) failed\n' >&2
        exit 1
    fi
    if ! grep -q -- '-ffile-prefix-map=' "$GCC_DUMP"; then
        printf 'release-helper-static: GCC-style CC did not emit -ffile-prefix-map flags\n' >&2
        exit 1
    fi
    if grep -q '/d1trimfile:' "$GCC_DUMP"; then
        printf 'release-helper-static: GCC-style CC must not emit /d1trimfile flags\n' >&2
        exit 1
    fi

    behavioral=1
else
    printf 'release-helper-static: pwsh not found; runtime token checks skipped (static source-contract checks only, no runtime proof claimed)\n' >&2
fi

if [ "$behavioral" -eq 1 ]; then
    printf 'release-helper static + behavioral checks passed (CC_SHELL_ESCAPED_FLAGS=1; exact shlex token round trip for CFLAGS/CXXFLAGS incl. spaced roots, one /d1trimfile:<root>\\ per root; scoped HOST_/TARGET_* vars carry the remap tokens only; guard rejects trailing backslash, unterminated quote and unquoted word-start # while quoted/mid-token # pass with suffix intact; cl/clang-cl vs GCC selection; no /pathmap:, no NDEBUG; incoming flags and Rust remap preserved; gate still throws on any leak and scans UTF-16LE)\n'
else
    printf 'release-helper static checks passed (per-root /d1trimfile: with trailing separator, CC_SHELL_ESCAPED_FLAGS=1 single-quoted serialization, no space-split of incoming CFLAGS/CXXFLAGS, scoped HOST_/TARGET_* vars set to the suffix only, shlex guard present incl. word-start #, cl/clang-cl selection anchored, no /pathmap:, no NDEBUG, incoming flags and Rust remap preserved; gate still throws on any leak and scans UTF-16LE; pwsh unavailable so no runtime proof)\n'
fi
