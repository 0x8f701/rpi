<#
Release-dist build wrapper for the rpi workspace (PowerShell twin of
build-release.sh; same flag composition).

Cargo 1.88 rejects the unstable profile `trim-paths` key, so stable rustc
`--remap-path-prefix` flags are composed here and handed to cargo via
CARGO_ENCODED_RUSTFLAGS (unit-separator encoded; cargo 1.88 splits on
U+001F and prefers it over RUSTFLAGS). Incoming RUSTFLAGS, such as the
Windows matrix `-C target-feature=+crt-static`, is merged in first so
explicit caller flags keep working. The workspace, HOME, CARGO_HOME and
RUSTUP_HOME roots are remapped to fixed neutral virtual paths so release
binaries never embed the builder's absolute paths.

# Native C/C++ sources (rquickjs, tree-sitter) are compiled by the cc build
# scripts, which ignore rustc remapping and embed the absolute source path
# via __FILE__/__FILEW__ (C assert/_wassert) macros, so the same roots are
# also scrubbed for the C compiler through CFLAGS/CXXFLAGS: MSVC takes
# /d1trimfile:<prefix> (one flag per root, trailing separator required),
# GCC/Clang take -ffile-prefix-map/-fmacro-prefix-map. The cc crate (1.4.0
# in this workspace) documents that individual CFLAGS cannot contain spaces
# unless CC_SHELL_ESCAPED_FLAGS is set, in which case *FLAGS are parsed with
# POSIX-shell lexing (shlex::Shlex, like make/cmake): 'a "b c"' yields the
# two arguments a and b c. This helper therefore sets CC_SHELL_ESCAPED_FLAGS
# and appends every composed compiler flag as one POSIX single-quoted token
# ('...' keeps backslashes, spaces and metacharacters literal; a literal
# quote is written '\''), so /d1trimfile prefixes whose roots contain spaces
# survive cc-rs parsing as exactly one argument. Incoming CFLAGS/CXXFLAGS
# pass through verbatim ahead of the composed flags, so their quoting and
# grouping is interpreted by the same Shlex parser cc-rs applies to the whole
# string.
#
# cc-rs reads the live environment and concatenates CFLAGS + HOST_CFLAGS +
# TARGET_CFLAGS + CFLAGS_<target> (cc-1.4.0 src/lib.rs:4214-4228,4240-4259);
# rquickjs-sys 0.12.2 overwrites CFLAGS (build.rs:186-191) before cc-rs reads
# it, so the composed suffix is also exported through the scoped
# HOST_CFLAGS/TARGET_CFLAGS and HOST_CXXFLAGS/TARGET_CXXFLAGS. Those carry
# the suffix only - never the incoming flags - so crates that keep
# CFLAGS/CXXFLAGS do not see the caller's flags twice. An unquoted word-start
# '#' in incoming flags would start a shlex 2.x comment that consumes the
# rest of the string (including the appended suffix), so the pass-through
# guard rejects it while quoted and mid-token '#' stay literal.
#
# The remap list is ordered most-specific-first because rustc applies the
# first matching --remap-path-prefix. No workstation path is hardcoded; the
# roots are read from the environment at invocation time.

# Usage:
#   ./E2E.d/release/build-release.ps1 [cargo args...]
#   ./E2E.d/release/build-release.ps1 -DumpFlags  # print the composed roots
#       and CFLAGS/CXXFLAGS environment, then exit without invoking cargo
#       (used by tests/release-helper-static.sh)
#>
param([switch]$DumpFlags)
$ErrorActionPreference = 'Stop'

$repoRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..\..')).Path
$homeRoot = if ($HOME) { $HOME } else { '' }
if ($env:CARGO_HOME) {
    $cargoHomeRoot = $env:CARGO_HOME
} elseif ($homeRoot) {
    $cargoHomeRoot = Join-Path $homeRoot '.cargo'
} else {
    $cargoHomeRoot = ''
}
if ($env:RUSTUP_HOME) {
    $rustupHomeRoot = $env:RUSTUP_HOME
} elseif ($homeRoot) {
    $rustupHomeRoot = Join-Path $homeRoot '.rustup'
} else {
    $rustupHomeRoot = ''
}

$remapFlags = @()
if ($repoRoot) { $remapFlags += "--remap-path-prefix=$repoRoot=/pi-src" }
if ($cargoHomeRoot) { $remapFlags += "--remap-path-prefix=$cargoHomeRoot=/pi-cargo-home" }
if ($rustupHomeRoot) { $remapFlags += "--remap-path-prefix=$rustupHomeRoot=/pi-rustup-home" }
if ($homeRoot) { $remapFlags += "--remap-path-prefix=$homeRoot=/pi-home" }

# Same roots for the C/C++ compiler (cc build scripts). /pathmap: is the C#
# compiler's PathMap option and cl.exe ignores it, so the previous MSVC remap
# was a silent no-op; cl.exe's mechanism for trimming __FILE__/__FILEW__
# (assert/_wassert records) and obj/PDB debug paths is the undocumented
# /d1trimfile:<prefix> (standard in Chromium et al.; also accepted by
# clang-cl). One flag per root, each trimming every recorded path beginning
# with that prefix; the trailing separator is required so C:\Users\alice\
# cannot match C:\Users\aliceX\. GCC/Clang take
# -ffile-prefix-map/-fmacro-prefix-map (file-prefix-map already implies
# macro-prefix-map on GCC 8+/Clang 10+; both are emitted explicitly for
# older toolchains).
$isWindowsHost = ([System.Environment]::OSVersion.Platform -eq [System.PlatformID]::Win32NT)

# Compiler selection, explicit for the hosted MSVC target: an explicit CC
# naming a cl-compatible driver - cl, cl.exe, clang-cl or clang-cl.exe,
# optionally prefixed by a toolchain path or an sccache-style wrapper
# ("sccache cl") and optionally followed by driver args ("clang-cl
# --target=...") - selects the MSVC /d1trimfile branch. With no CC, a
# Windows host defaults to MSVC (the case release.yml drives this script
# with); anything else (gcc, clang, MinGW cross compilers) takes the
# -ffile-prefix-map branch. The match is case-insensitive and
# boundary-anchored so `clang` or `myclang` are never mistaken for cl.
$isMsvc = $false
if ($env:CC) {
    $isMsvc = ($env:CC -match '(^|[\s\\/])cl(\.exe)?(\s|$)' -or $env:CC -match '(^|[\s\\/])clang-cl(\.exe)?(\s|$)')
} else {
    $isMsvc = $isWindowsHost
}
$remapRoots = @($repoRoot, $cargoHomeRoot, $rustupHomeRoot, $homeRoot)
$compilerRemapFlags = @()
if ($isMsvc) {
    foreach ($root in $remapRoots) {
        if ($root) { $compilerRemapFlags += "/d1trimfile:$($root.TrimEnd([char]'\'))\" }
    }
} else {
    if ($repoRoot) { $compilerRemapFlags += "-ffile-prefix-map=$repoRoot=/pi-src", "-fmacro-prefix-map=$repoRoot=/pi-src" }
    if ($cargoHomeRoot) { $compilerRemapFlags += "-ffile-prefix-map=$cargoHomeRoot=/pi-cargo-home", "-fmacro-prefix-map=$cargoHomeRoot=/pi-cargo-home" }
    if ($rustupHomeRoot) { $compilerRemapFlags += "-ffile-prefix-map=$rustupHomeRoot=/pi-rustup-home", "-fmacro-prefix-map=$rustupHomeRoot=/pi-rustup-home" }
    if ($homeRoot) { $compilerRemapFlags += "-ffile-prefix-map=$homeRoot=/pi-home", "-fmacro-prefix-map=$homeRoot=/pi-home" }
}

# RUSTFLAGS is space-separated by definition (cargo splits on whitespace);
# merge it ahead of the remap flags so caller flags keep working.
$incomingFlags = @()
if ($env:RUSTFLAGS) {
    $incomingFlags = @($env:RUSTFLAGS.Split(' ') | Where-Object { $_ -ne '' })
}
$allFlags = @($incomingFlags) + @($remapFlags)
$env:CARGO_ENCODED_RUSTFLAGS = ($allFlags -join [char]0x1F)

# The cc crate (1.4.0) parses CFLAGS/CXXFLAGS with shlex::Shlex (POSIX shell
# syntax) when CC_SHELL_ESCAPED_FLAGS is set; without it, individual flags
# cannot contain spaces (cc-1.4.0 src/lib.rs:69-92,4239-4256). Enable that
# mode: it is the only way a /d1trimfile prefix containing a space survives
# as one compiler argument. It is forced rather than inherited because the
# composed flags below require it; make/cmake-style quoting then applies to
# every consumer of CFLAGS/CXXFLAGS in the build.
$env:CC_SHELL_ESCAPED_FLAGS = '1'

# Serialize one flag as a POSIX single-quoted token: backslashes (Windows
# roots), spaces and metacharacters stay literal, and a literal quote is
# written '\'' per POSIX. Quoting every flag is harmless - the lexer strips
# the quotes and the compiler receives the bare token.
function ConvertTo-ShlexArg {
    param([string]$Arg)
    return "'" + $Arg.Replace("'", "'\''") + "'"
}

# Guard for the pass-through contract below: incoming CFLAGS/CXXFLAGS travel
# verbatim, and cc-rs's Shlex iteration STOPS or silently DROPS the rest of
# the string - including the composed /d1trimfile flags - on an unterminated
# quote, a trailing backslash, or an unquoted '#' at the start of a word
# (shlex 2.x treats that as a comment consuming the rest of the line,
# shlex-2.0.1/src/bytes.rs:138-146). Fail loudly instead of silently losing
# the path-trim flags. Quote, escape and word-boundary state are tracked so
# the check matches shlex's own parsing without re-tokenizing the input: a
# '#' begins a word only after the start of the text or after unquoted
# whitespace, so foo#bar and quoted '#'/'#' stay literal, and an escaped
# '#' or escaped whitespace keeps the word going.
function Test-ShlexComplete {
    param([string]$Text, [string]$VarName)
    if (-not $Text) { return }
    $state = 'plain'   # plain | single | double
    $atWordStart = $true
    for ($i = 0; $i -lt $Text.Length; $i++) {
        $ch = $Text[$i]
        if ($state -eq 'single') {
            if ($ch -eq "'") { $state = 'plain' }
        } elseif ($state -eq 'double') {
            if ($ch -eq '"') { $state = 'plain' }
            elseif ($ch -eq '\') {
                if ($i -eq $Text.Length - 1) {
                    throw "$VarName ends in an unescaped backslash; with CC_SHELL_ESCAPED_FLAGS=1 the cc crate's shlex parser would stop and drop the appended /d1trimfile flags"
                }
                $i++
            }
        } else {
            if ($ch -eq "'") { $state = 'single'; $atWordStart = $false }
            elseif ($ch -eq '"') { $state = 'double'; $atWordStart = $false }
            elseif ($ch -eq '\') {
                if ($i -eq $Text.Length - 1) {
                    throw "$VarName ends in an unescaped backslash; with CC_SHELL_ESCAPED_FLAGS=1 the cc crate's shlex parser would stop and drop the appended /d1trimfile flags"
                }
                $i++
                $atWordStart = $false
            }
            elseif ($ch -eq ' ' -or $ch -eq "`t" -or $ch -eq "`n") { $atWordStart = $true }
            elseif ($ch -eq '#') {
                if ($atWordStart) {
                    throw "$VarName contains an unquoted '#' at the start of a word; with CC_SHELL_ESCAPED_FLAGS=1 the cc crate's shlex parser treats it as a comment and would drop the appended /d1trimfile flags"
                }
                $atWordStart = $false
            }
            else { $atWordStart = $false }
        }
    }
    if ($state -ne 'plain') {
        throw "$VarName has an unterminated quote; with CC_SHELL_ESCAPED_FLAGS=1 the cc crate's shlex parser would stop and drop the appended /d1trimfile flags"
    }
}

# CFLAGS/CXXFLAGS are re-parsed by the cc crate from the environment string,
# so incoming flags are preserved verbatim (never space-split: their quoted
# grouping is interpreted by the same Shlex parser cc-rs applies) and the
# composed remap flags are appended, each single-quoted.
$quotedRemapFlags = @($compilerRemapFlags | ForEach-Object { ConvertTo-ShlexArg $_ })
$remapSuffix = ($quotedRemapFlags -join ' ')
if ($env:CFLAGS) {
    Test-ShlexComplete -Text $env:CFLAGS -VarName 'CFLAGS'
    $env:CFLAGS = $env:CFLAGS + ' ' + $remapSuffix
} else {
    $env:CFLAGS = $remapSuffix
}
if ($env:CXXFLAGS) {
    Test-ShlexComplete -Text $env:CXXFLAGS -VarName 'CXXFLAGS'
    $env:CXXFLAGS = $env:CXXFLAGS + ' ' + $remapSuffix
} else {
    $env:CXXFLAGS = $remapSuffix
}

# rquickjs-sys 0.12.2 overwrites CFLAGS (build.rs:186-191) before cc-rs reads
# it, and cc-rs concatenates CFLAGS + HOST_CFLAGS + TARGET_CFLAGS +
# CFLAGS_<target> (cc-1.4.0 src/lib.rs:4214-4228,4240-4259), so the composed
# suffix is also exported through the scoped HOST_/TARGET_ vars. They carry
# the suffix only - never the incoming flags - so a crate that keeps
# CFLAGS/CXXFLAGS does not see the caller's flags twice, while a crate that
# overwrites them still receives /d1trimfile. Setting both HOST_ and TARGET_
# covers hosted and cross builds (cc-rs reads the kind-matching scoped var).
$env:HOST_CFLAGS = $remapSuffix
$env:TARGET_CFLAGS = $remapSuffix
$env:HOST_CXXFLAGS = $remapSuffix
$env:TARGET_CXXFLAGS = $remapSuffix

# Test mode: print the computed environment so tests can re-parse it with
# POSIX shlex semantics and assert exact tokens; run no cargo.
if ($DumpFlags) {
    for ($i = 0; $i -lt $remapRoots.Count; $i++) {
        if ($remapRoots[$i]) { Write-Output "DUMPFLAGS_ROOT_$i=$($remapRoots[$i])" }
    }
    Write-Output "CC_SHELL_ESCAPED_FLAGS=$env:CC_SHELL_ESCAPED_FLAGS"
    Write-Output "CFLAGS=$env:CFLAGS"
    Write-Output "CXXFLAGS=$env:CXXFLAGS"
    Write-Output "HOST_CFLAGS=$env:HOST_CFLAGS"
    Write-Output "TARGET_CFLAGS=$env:TARGET_CFLAGS"
    Write-Output "HOST_CXXFLAGS=$env:HOST_CXXFLAGS"
    Write-Output "TARGET_CXXFLAGS=$env:TARGET_CXXFLAGS"
    exit 0
}

& cargo @args
exit $LASTEXITCODE
