<#
Scans a built release binary for forbidden builder-path prefixes (PowerShell
twin of path-leak-check.sh; UTF-8 and UTF-16LE scans).

The release gate (release.yml) and local release-dist builds run this after
building: it reports, per category, how many binary byte runs match a
forbidden absolute prefix (HOME, workspace, CARGO_HOME, RUSTUP_HOME) and
fails when any count is nonzero. Matching strings are never printed, so the
check doubles as a privacy-safe leak gate. rustc embeds remapped source
paths as UTF-8 strings (file!/panic locations); the UTF-16LE scan additionally
covers wide-string path forms in PE binaries.

Usage:
  ./E2E.d/release/path-leak-check.ps1 BINARY KEY=PREFIX [KEY=PREFIX...]
#>
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [string]$Binary,
    [Parameter(Mandatory = $true, Position = 1, ValueFromRemainingArguments = $true)]
    [string[]]$PrefixSpecs
)
$ErrorActionPreference = 'Stop'

if (-not (Test-Path -LiteralPath $Binary -PathType Leaf)) {
    throw "path-leak-check: binary not found: $Binary"
}
# Latin1 maps every byte to a distinct character, so byte runs (UTF-8 text
# and NUL-interleaved UTF-16LE text) survive verbatim for substring search.
$text = [System.Text.Encoding]::Latin1.GetString(
    [System.IO.File]::ReadAllBytes((Resolve-Path -LiteralPath $Binary))
)

$total = 0
$violations = 0
foreach ($spec in $PrefixSpecs) {
    $eq = $spec.IndexOf('=')
    if ($eq -le 0 -or $eq -eq $spec.Length - 1) {
        throw "path-leak-check: expected KEY=PREFIX, got: $spec"
    }
    $key = $spec.Substring(0, $eq)
    $prefix = $spec.Substring($eq + 1)
    $count = [regex]::Matches($text, [regex]::Escape($prefix)).Count
    # PE files can also carry the prefix as a NUL-interleaved UTF-16LE string.
    $widePrefix = ($prefix.ToCharArray() -join [char]0)
    $count += [regex]::Matches($text, [regex]::Escape($widePrefix)).Count
    Write-Output ("{0}: {1}" -f $key, $count)
    $total += $count
    if ($count -ne 0) {
        $violations += 1
    }
}
if ($violations -ne 0) {
    throw "path-leak-check: $total forbidden builder-path occurrence(s) in $Binary (see counts above)"
}
Write-Output 'no builder path leakage detected'
