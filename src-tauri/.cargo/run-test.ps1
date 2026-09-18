# cargo test/run "runner" for x86_64-pc-windows-msvc (see .cargo/config.toml).
#
# The lib unit-test binary (tauri_python_sidecar_lib-*.exe) links
# tauri-plugin-dialog, which imports comctl32 v6's TaskDialogIndirect, but
# carries no embedded manifest.
# The loader then binds against System32 comctl32 v5.82 (no such export) and the
# process dies at load with STATUS_ENTRYPOINT_NOT_FOUND before any test runs.
#
# This runner embeds a Common-Controls v6 manifest into the target exe *only if
# it lacks one*, then executes it. The shipped app.exe already carries a full
# manifest (from tauri's resource.lib), so it is detected and left untouched --
# a manual `cargo run` still launches the real app unmodified. See docs/comctl32-test-runner.md.
#
# cargo invokes: run-test.ps1 <exe> [test-args...]

$ErrorActionPreference = 'Stop'

$exe  = $args[0]
$rest = if ($args.Count -gt 1) { $args[1..($args.Count - 1)] } else { @() }

$manifest = Join-Path $PSScriptRoot 'comctl-v6.xml'
$mtCache  = Join-Path ([System.IO.Path]::GetTempPath()) 'tauri-python-sidecar-mt-path.txt'

function Resolve-Mt {
    if ((Test-Path $mtCache) -and (Test-Path (Get-Content $mtCache -Raw).Trim())) {
        return (Get-Content $mtCache -Raw).Trim()
    }
    $binRoot = 'C:\Program Files (x86)\Windows Kits\10\bin'
    if (-not (Test-Path $binRoot)) { return $null }
    $mt = Get-ChildItem $binRoot -Directory -ErrorAction SilentlyContinue |
        Sort-Object Name -Descending |
        ForEach-Object { Join-Path $_.FullName 'x64\mt.exe' } |
        Where-Object { Test-Path $_ } |
        Select-Object -First 1
    if ($mt) { Set-Content -Path $mtCache -Value $mt -NoNewline }
    return $mt
}

function Has-Manifest($path, $mt) {
    # mt.exe extracts RT_MANIFEST id 1; nonzero exit => no manifest present.
    $tmp = [System.IO.Path]::GetTempFileName()
    try {
        & $mt -nologo -inputresource:"$path;#1" -out:"$tmp" 2>$null | Out-Null
        return ($LASTEXITCODE -eq 0)
    } finally {
        Remove-Item $tmp -ErrorAction SilentlyContinue
    }
}

$mt = Resolve-Mt
if ($null -eq $mt) {
    Write-Warning "run-test.ps1: mt.exe not found; running '$exe' unpatched (may crash at load with STATUS_ENTRYPOINT_NOT_FOUND). Install the Windows 10/11 SDK."
} elseif (-not (Has-Manifest $exe $mt)) {
    & $mt -nologo -manifest "$manifest" -outputresource:"$exe;#1" | Out-Null
    if ($LASTEXITCODE -ne 0) {
        Write-Warning "run-test.ps1: mt.exe failed to embed manifest into '$exe' (exit $LASTEXITCODE)."
    }
}

& $exe @rest
exit $LASTEXITCODE
