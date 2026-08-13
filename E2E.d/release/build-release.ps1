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
# via __FILE__-style macros, so the same roots are also remapped for the C
# compiler through CFLAGS/CXXFLAGS: MSVC takes /pathmap:from=to, GCC/Clang
# take -ffile-prefix-map/-fmacro-prefix-map. Incoming CFLAGS/CXXFLAGS are
# preserved ahead of the remap flags.
#
# The remap list is ordered most-specific-first because rustc applies the
# first matching --remap-path-prefix. No workstation path is hardcoded; the
# roots are read from the environment at invocation time.

# Usage:
#   ./E2E.d/release/build-release.ps1 [cargo args...]
#>
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

# Same roots for the C/C++ compiler (cc build scripts). MSVC takes
# /pathmap:from=to; GCC/Clang take -ffile-prefix-map/-fmacro-prefix-map
# (file-prefix-map already implies macro-prefix-map on GCC 8+/Clang 10+;
# both are emitted explicitly for older toolchains).
$isWindows = ([System.Environment]::OSVersion.Platform -eq [System.PlatformID]::Win32NT)
$isMsvc = $false
if ($env:CC) {
    $isMsvc = ($env:CC -match 'cl(\.exe)?$' -or $env:CC -match 'clang-cl')
} else {
    $isMsvc = $isWindows
}
$compilerRemapFlags = @()
if ($isMsvc) {
    if ($repoRoot) { $compilerRemapFlags += "/pathmap:$repoRoot=/pi-src" }
    if ($cargoHomeRoot) { $compilerRemapFlags += "/pathmap:$cargoHomeRoot=/pi-cargo-home" }
    if ($rustupHomeRoot) { $compilerRemapFlags += "/pathmap:$rustupHomeRoot=/pi-rustup-home" }
    if ($homeRoot) { $compilerRemapFlags += "/pathmap:$homeRoot=/pi-home" }
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

# CFLAGS/CXXFLAGS are space-separated (the cc crate's format); preserve any
# incoming flags ahead of the compiler remap flags.
$incomingCflags = @()
if ($env:CFLAGS) {
    $incomingCflags = @($env:CFLAGS.Split(' ') | Where-Object { $_ -ne '' })
}
$incomingCxxflags = @()
if ($env:CXXFLAGS) {
    $incomingCxxflags = @($env:CXXFLAGS.Split(' ') | Where-Object { $_ -ne '' })
}
$env:CFLAGS = ((@($incomingCflags) + @($compilerRemapFlags)) -join ' ')
$env:CXXFLAGS = ((@($incomingCxxflags) + @($compilerRemapFlags)) -join ' ')

& cargo @args
exit $LASTEXITCODE
