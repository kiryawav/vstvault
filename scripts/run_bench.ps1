param([int]$Seconds = 15)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$bench = Join-Path $root 'target\release\ipc-bench.exe'
$resultDir = Join-Path $root 'results'
New-Item -ItemType Directory -Force -Path $resultDir | Out-Null
$csv = Join-Path $resultDir 'ipc_bench.csv'

$logical = (Get-CimInstance Win32_Processor | Measure-Object NumberOfLogicalProcessors -Sum).Sum

$runs = @(
    @{ block = 128; instances = 0; spin = 0;   load = 0 },
    @{ block = 64;  instances = 1; spin = 0;   load = 0 },
    @{ block = 128; instances = 1; spin = 0;   load = 0 },
    @{ block = 256; instances = 1; spin = 0;   load = 0 },
    @{ block = 512; instances = 1; spin = 0;   load = 0 },
    @{ block = 128; instances = 1; spin = 20;  load = 0 },
    @{ block = 128; instances = 1; spin = 100; load = 0 },
    @{ block = 128; instances = 8;  spin = 0;  load = 0 },
    @{ block = 128; instances = 32; spin = 0;  load = 0 },
    @{ block = 128; instances = 32; spin = 100; load = 0 },
    @{ block = 64;  instances = 32; spin = 0;   load = 0 },
    @{ block = 64;  instances = 32; spin = 100; load = 0 },
    @{ block = 128; instances = 1;  spin = 0;  load = $logical },
    @{ block = 128; instances = 8;  spin = 0;  load = $logical },
    @{ block = 128; instances = 32; spin = 0;  load = $logical },
    @{ block = 64;  instances = 8;  spin = 0;  load = $logical }
)

$lines = [System.Collections.Generic.List[string]]::new()
$first = $true
foreach ($r in $runs) {
    $benchArgs = @('--block', $r.block, '--instances', $r.instances, '--spin-us', $r.spin, '--load', $r.load, '--seconds', $Seconds)
    if ($first) { $benchArgs += '--csv-header' }
    $output = & $bench @benchArgs
    if ($LASTEXITCODE -ne 0) { Write-Warning "exit code $LASTEXITCODE for: $($benchArgs -join ' ')" }
    foreach ($line in $output) { $lines.Add($line); Write-Output $line }
    $first = $false
}
$lines | Set-Content -Encoding utf8 -LiteralPath $csv
Write-Output "Saved: $csv"
