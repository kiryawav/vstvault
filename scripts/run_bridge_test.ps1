param(
    [Parameter(Mandatory = $true)][string[]]$Plugins,
    [int]$Seconds = 10
)

$root = Split-Path -Parent $PSScriptRoot
$rel = Join-Path $root 'target\release'
$results = Join-Path $root 'results'
$report = Join-Path $results 'bridge_test.txt'
$csv = Join-Path $results 'bridge_bench.csv'
$deploy = Join-Path $root 'deploy\real'
$vst = 'C:\Program Files\Common Files\VST3'
New-Item -ItemType Directory -Force -Path $results | Out-Null

foreach ($plugin in $Plugins) {
    $original = if ([IO.Path]::IsPathRooted($plugin)) { $plugin } else { Join-Path $vst $plugin }
    $name = [IO.Path]::GetFileNameWithoutExtension($original)
    $bridge = Join-Path $deploy "$name.vst3"

    & "$rel\vault-cli.exe" bridge --plugin $original --out $deploy | Out-Null
    if ($LASTEXITCODE -ne 0) { Add-Content $report "${name}: FAIL could not create a bridge"; continue }

    $identity = { param($path) (& "$rel\vst3-inspect.exe" $path 2>$null) | Select-String -Pattern 'Audio Module Class' -Context 0,1 | ForEach-Object { $_.ToString() } }
    $same = (& $identity $original) -join '|' -ceq ((& $identity $bridge) -join '|')
    Add-Content $report "${name}: identity $(if ($same) { 'PASS' } else { 'FAIL' })"

    foreach ($block in 64, 256) {
        & "$rel\ipc-bench.exe" --plugin $bridge --compare-with $original --null-test --block $block --seconds $Seconds --out $report *> $null
    }
    & "$rel\ipc-bench.exe" --plugin $bridge --compare-with $original --state-test --out $report *> $null
    & "$rel\ipc-bench.exe" --plugin $bridge --compare-with $original --param-test --block 256 --seconds $Seconds --out $report *> $null
    & "$rel\ipc-bench.exe" --plugin $bridge --compare-with $original --sidechain-test --block 256 --seconds $Seconds --out $report *> $null
    & "$rel\ipc-bench.exe" --plugin $bridge --editor-test --seconds 1 --out $report *> $null
    if ($LASTEXITCODE -ne 0) { Add-Content $report "${name}: editor test exit code $LASTEXITCODE" }

    foreach ($instances in 1, 8) {
        foreach ($target in @($original, $bridge)) {
            $benchArgs = @('--plugin', $target, '--in-process', '--block', 128, '--instances', $instances, '--seconds', $Seconds, '--out', $csv)
            if (-not (Test-Path -LiteralPath $csv)) { $benchArgs += '--csv-header' }
            & "$rel\ipc-bench.exe" @benchArgs *> $null
            if ($LASTEXITCODE -ne 0) { Add-Content $report "${name}: timing run failed for $target with $instances instances" }
        }
    }

    $hostsBefore = @(Get-Process vault-host -ErrorAction SilentlyContinue | ForEach-Object { $_.Id })
    $run = Start-Process -FilePath "$rel\ipc-bench.exe" -ArgumentList @('--plugin', "`"$bridge`"", '--in-process', '--block', 128, '--seconds', 6) -PassThru -WindowStyle Hidden -RedirectStandardOutput (Join-Path $env:TEMP 'vault-kill-test.txt')
    $null = $run.Handle
    Start-Sleep -Seconds 3
    $victim = Get-Process vault-host -ErrorAction SilentlyContinue | Where-Object { $hostsBefore -notcontains $_.Id } | Select-Object -First 1
    if ($victim) { Stop-Process -Id $victim.Id -Force }
    $finished = $run.WaitForExit(30000)
    $code = if ($finished) { $run.ExitCode } else { 'hung' }
    $passed = $victim -and ($code -eq 0)
    Add-Content $report "${name}: host-killed $(if ($passed) { 'PASS' } else { 'FAIL' }) killed=$([bool]$victim) bench_exit=$code"
}
