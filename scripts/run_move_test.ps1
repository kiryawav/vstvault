param(
    [string]$Vault = 'D:\PluginVault',
    [Parameter(Mandatory = $true)][string]$Name,
    [Parameter(Mandatory = $true)][string]$PluginInside,
    [string]$Letter = 'V:'
)

$root = Split-Path -Parent $PSScriptRoot
$bench = Join-Path $root 'target\release\ipc-bench.exe'
$report = Join-Path $root 'results\move_test.txt'
$bridgeName = "$([IO.Path]::GetFileNameWithoutExtension($PluginInside)).vst3"
$inside = 'C:\Users\Public\vault-move.txt'
$outside = Join-Path $env:TEMP 'vault-move-editor.txt'
$cyrillic = -join ([char[]](0x041C, 0x043E, 0x0439))
$copyParent = "D:\VaultMoveTest"
$copyOnD = Join-Path $copyParent "$cyrillic vault"
$copyOnC = Join-Path $root 'deploy\move-test'
$started = Get-Date
$failures = 0

function Note($line) { Add-Content $report $line -Encoding utf8; Write-Output $line }
function Result($case, $what, $ok, $detail = '') {
    if (-not $ok) { $script:failures++ }
    Note "${case}: $what $(if ($ok) { 'PASS' } else { 'FAIL' }) $detail"
}

function Snapshot {
    $folders = $env:TEMP, $env:APPDATA, $env:LOCALAPPDATA, $env:ProgramData, "$env:ProgramFiles\Common Files\VST3", "$env:USERPROFILE\Documents"
    foreach ($folder in $folders) { Get-ChildItem -LiteralPath $folder -Force -ErrorAction SilentlyContinue | ForEach-Object { $_.FullName } }
    $keys = 'HKCU:\Software', 'HKLM:\SOFTWARE', 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall', 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall', 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall'
    foreach ($key in $keys) { Get-ChildItem $key -ErrorAction SilentlyContinue | ForEach-Object { $_.Name } }
}

function Check-Location($case, $location) {
    $cli = Join-Path $location 'tools\vault-cli.exe'
    $environment = Join-Path $location "plugins\$Name"
    $resultFile = Join-Path $environment 'C\Users\Public\vault-move.txt'
    $bridge = Join-Path $location "bridges\$bridgeName"
    & $cli check *> $null
    Result $case 'check' ($LASTEXITCODE -eq 0)
    foreach ($test in @(@('--null-test', '--block', '256', '--seconds', '1'), @('--state-test'))) {
        & $cli run --name $Name $bench --plugin $bridge --compare-with $PluginInside @test --out $inside *> $null
    }
    $lines = @(Get-Content -LiteralPath $resultFile -ErrorAction SilentlyContinue)
    Result $case 'sound-and-state' ($lines.Count -eq 2 -and -not ($lines -cmatch 'FAIL')) ($lines -join ' | ')
    Remove-Item -LiteralPath $resultFile -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $outside -ErrorAction SilentlyContinue
    & $bench --plugin $bridge --editor-test --no-input --seconds 1 --out $outside *> $null
    $editor = @(Get-Content -LiteralPath $outside -ErrorAction SilentlyContinue)
    Result $case 'editor' ($editor.Count -gt 0 -and -not ($editor -cmatch 'FAIL'))
    Remove-Item -LiteralPath $outside -ErrorAction SilentlyContinue
}

Note "--- $Name, $(Get-Date -Format 'yyyy-MM-dd HH:mm')"
$cli = Join-Path $Vault 'tools\vault-cli.exe'
& $cli disconnect *> $null
$before = Snapshot

& $cli connect *> $null
Result 'original' 'connect' ($LASTEXITCODE -eq 0)
$manifest = Get-Content -LiteralPath (Join-Path $Vault "bridges\$bridgeName\Contents\Resources\vault-manifest.txt") -Encoding utf8 -ErrorAction SilentlyContinue
Result 'original' 'manifest-is-relative' (($manifest -contains 'host=..\tools\vault-host.exe') -and ($manifest -contains "environment=..\plugins\$Name"))
Check-Location 'original' $Vault

subst $Letter $Vault
Check-Location 'other-letter' $Letter
subst $Letter /d

New-Item -ItemType Directory -Force -Path $copyParent | Out-Null
Copy-Item -LiteralPath $Vault -Destination $copyOnD -Recurse
Check-Location 'other-folder' $copyOnD
Remove-Item -LiteralPath $copyOnD -Recurse -Force

Copy-Item -LiteralPath $Vault -Destination $copyOnC -Recurse
Check-Location 'on-c' $copyOnC
Remove-Item -LiteralPath $copyOnC -Recurse -Force

$realProfile = $env:USERPROFILE
$profilesFile = Join-Path $Vault "plugins\$Name\profiles.txt"
$knownProfiles = Get-Content -LiteralPath $profilesFile -Raw
$marker = "vault-move-marker-$PID.txt"
Set-Content -LiteralPath (Join-Path $Vault "plugins\$Name\C\Users\current\$marker") 'found'
$env:USERPROFILE = 'C:\Users\SomeoneElse'
Check-Location 'other-user' $Vault
$env:USERPROFILE = 'C:\Users\AThirdUser'
$read = & $cli run --name $Name "$env:SystemRoot\System32\cmd.exe" /c type "$realProfile\$marker" 2>$null
Result 'other-user' 'old-absolute-path' ("$read".Trim() -eq 'found') "read=[$read]"
$env:USERPROFILE = $realProfile
Remove-Item -LiteralPath (Join-Path $Vault "plugins\$Name\C\Users\current\$marker")
Set-Content -LiteralPath $profilesFile $knownProfiles -NoNewline

& $cli disconnect *> $null
Result 'after' 'disconnect' ($LASTEXITCODE -eq 0)
Result 'after' 'bridge-gone' (-not (Test-Path -LiteralPath (Join-Path $Vault "bridges\$bridgeName")))
Result 'after' 'no-hosts-left' (@(Get-Process vault-host -ErrorAction SilentlyContinue).Count -eq 0)
$after = Snapshot
$new = @(Compare-Object @($before) @($after) | Where-Object { $_.SideIndicator -eq '=>' -and $_.InputObject -notmatch '__PSScriptPolicyTest' } | ForEach-Object { $_.InputObject })
Result 'after' 'no-traces' ($new.Count -eq 0) ($new -join ' ; ')
Remove-Item -LiteralPath $copyParent -ErrorAction SilentlyContinue
Note "move test: $failures failures in $([int]((Get-Date) - $started).TotalSeconds) s"
