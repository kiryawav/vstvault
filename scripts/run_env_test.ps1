param(
    [Parameter(Mandatory = $true)][string]$Name,
    [Parameter(Mandatory = $true)][string]$Installer,
    [string[]]$InstallerArgs = @(),
    [Parameter(Mandatory = $true)][string]$PluginInside,
    [string[]]$MustStayAbsent = @(),
    [string]$Vault = 'D:\PluginVault',
    [Parameter(Mandatory = $true)][string]$Out,
    [int]$Seconds = 1,
    [switch]$Timing
)

$root = Split-Path -Parent $PSScriptRoot
$rel = Join-Path $root 'target\release'
$report = Join-Path $root 'results\env_test.txt'
$csv = Join-Path $Vault 'env-bench.tmp.csv'
$cli = Join-Path $Vault 'tools\vault-cli.exe'
$bench = Join-Path $rel 'ipc-bench.exe'
$bridge = Join-Path $Out "$([IO.Path]::GetFileNameWithoutExtension($PluginInside)).vst3"
$environment = Join-Path $Vault "plugins\$Name"
$reportInside = "C:\Users\Public\vault-env-report.txt"
$reportReal = Join-Path $environment 'C\Users\Public\vault-env-report.txt'
function Note($line) { Add-Content $report $line; Write-Output $line }
function Absent { foreach ($path in $MustStayAbsent) { if (Test-Path -LiteralPath $path) { return $false } }; $true }

Note "--- $Name, $(Get-Date -Format 'yyyy-MM-dd HH:mm')"
if (Test-Path -LiteralPath $environment) { & $cli remove --name $Name | Out-Null }
& $cli disconnect *> $null
& $cli connect --out $Out | Out-Null
function Install-Step {
    & $cli create --name $Name | Out-Null
    & $cli pull --name $Name $Installer | Out-Null
    & $cli run --name $Name (Split-Path -Leaf $Installer) @InstallerArgs | Out-Null
}

Install-Step
Note "${Name}: install $(if ($LASTEXITCODE -eq 0 -and (Test-Path -LiteralPath $bridge)) { 'PASS' } else { "FAIL exit=$LASTEXITCODE" })"
Note "${Name}: system-clean-after-install $(if (Absent) { 'PASS' } else { 'FAIL' })"

$inside = @(
    @('--null-test', '--block', '64', '--seconds', $Seconds),
    @('--null-test', '--block', '256', '--seconds', $Seconds),
    @('--state-test'),
    @('--param-test', '--block', '256', '--seconds', $Seconds),
    @('--sidechain-test', '--block', '256', '--seconds', $Seconds)
)
foreach ($test in $inside) {
    & $cli run --name $Name $bench --plugin $bridge --compare-with $PluginInside @test --out $reportInside 2>$null | Out-Null
}
if (Test-Path -LiteralPath $reportReal) {
    Get-Content -LiteralPath $reportReal | ForEach-Object { Note $_ }
    Remove-Item -LiteralPath $reportReal
} else { Note "${Name}: FAIL no comparison results" }

& $bench --plugin $bridge --editor-test --seconds 1 --out $report *> $null
if ($LASTEXITCODE -ne 0) { Note "${Name}: FAIL editor test exit code $LASTEXITCODE" }

$before = @(Get-Process vault-host -ErrorAction SilentlyContinue | ForEach-Object { $_.Id })
foreach ($instances in $(if ($Timing) { 1, 8 } else { @() })) {
    foreach ($case in @(@('original inside', $true), @('bridge', $false))) {
        $benchArgs = @('--block', 128, '--instances', $instances, '--seconds', $Seconds, '--in-process', '--out', $csv)
        if ($case[1]) { & $cli run --name $Name $bench --plugin $PluginInside @benchArgs 2>$null | Out-Null } else { & $bench --plugin $bridge @benchArgs *> $null }
        Note "${Name}: timing $($case[0]) instances=$instances $(if ($LASTEXITCODE -eq 0) { 'PASS' } else { "FAIL exit=$LASTEXITCODE" })"
    }
}
if (Test-Path -LiteralPath $csv) { Add-Content (Join-Path $root 'results\env_bench.csv') (Get-Content -LiteralPath $csv); Remove-Item -LiteralPath $csv }
Note "${Name}: hosts-gone-after-runs $(if (@(Get-Process vault-host -ErrorAction SilentlyContinue | Where-Object { $before -notcontains $_.Id }).Count -eq 0) { 'PASS' } else { 'FAIL' })"
Note "${Name}: system-clean-after-use $(if (Absent) { 'PASS' } else { 'FAIL' })"

& $cli remove --name $Name | Out-Null
Note "${Name}: remove $(if ($LASTEXITCODE -eq 0 -and -not (Test-Path -LiteralPath $environment) -and -not (Test-Path -LiteralPath $bridge)) { 'PASS' } else { "FAIL exit=$LASTEXITCODE" })"
Install-Step
Note "${Name}: reinstall $(if ($LASTEXITCODE -eq 0 -and (Test-Path -LiteralPath $bridge)) { 'PASS' } else { "FAIL exit=$LASTEXITCODE" })"
