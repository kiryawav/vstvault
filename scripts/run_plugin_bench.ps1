param(
    [Parameter(Mandatory = $true)][string]$Plugin,
    [int]$Seconds = 15
)

$root = Split-Path -Parent $PSScriptRoot
$bench = Join-Path $root 'target\release\ipc-bench.exe'
$resultDir = Join-Path $root 'results'
New-Item -ItemType Directory -Force -Path $resultDir | Out-Null
$nullFile = Join-Path $resultDir 'plugin_null_test.txt'
$csv = Join-Path $resultDir 'plugin_bench.csv'

foreach ($block in 64, 256) {
    & $bench --plugin $Plugin --null-test --block $block --seconds $Seconds --out $nullFile
    if ($LASTEXITCODE -ne 0) { Write-Warning "null test exit code $LASTEXITCODE at block $block" }
}

$runs = @(
    @{ block = 128; instances = 1;  mode = 'in-process' },
    @{ block = 128; instances = 1;  mode = 'hosted' },
    @{ block = 64;  instances = 1;  mode = 'in-process' },
    @{ block = 64;  instances = 1;  mode = 'hosted' },
    @{ block = 128; instances = 16; mode = 'in-process' },
    @{ block = 128; instances = 16; mode = 'hosted' }
)
foreach ($r in $runs) {
    $benchArgs = @('--plugin', $Plugin, '--block', $r.block, '--instances', $r.instances, '--seconds', $Seconds, '--out', $csv)
    if ($r.mode -eq 'in-process') { $benchArgs += '--in-process' }
    if (-not (Test-Path -LiteralPath $csv)) { $benchArgs += '--csv-header' }
    & $bench @benchArgs
    if ($LASTEXITCODE -ne 0) { Write-Warning "exit code $LASTEXITCODE for: $($benchArgs -join ' ')" }
}
