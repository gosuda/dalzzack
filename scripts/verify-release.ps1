# Checks a published release on Windows: install, guides, browser, and uninstall.
#
#   pwsh -File scripts/verify-release.ps1 <tag>
#
# Home, temp, and the install folder are temporary, so the real install, the
# agent files, and running daemons stay as they are. Two things are real:
# - The user Path gets one entry, and uninstall removes it.
# - The Windows data folders. dalzzack finds them through the Known Folder API,
#   not through the environment, so this script does not use --purge: that would
#   delete the real browser logins. CI tests --purge on a throw-away runner.
#
# Each check prints PASS or FAIL. The last line is ALL PASSED <tag>, and the
# exit status is 0 only then.

param([Parameter(Mandatory)][string]$Tag)

$ErrorActionPreference = 'Stop'

function Pass($message) { Write-Host "PASS: $message" }

function Fail($message) {
    Write-Host "FAIL: $message"
    exit 1
}

function Get-UserPath {
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment')
    try {
        $key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames).TrimEnd(';')
    } finally {
        $key.Close()
    }
}

$version = ($Tag -replace '^v', '') -replace '-.*$', ''
$id = 'verify-release'
$root = Join-Path ([IO.Path]::GetTempPath()) "dalzzack-verify-$([guid]::NewGuid())"
$home_ = Join-Path $root 'home'
$dir = Join-Path $root 'dalzzack'
$exe = Join-Path $dir 'dalzzack.exe'
$claude = Join-Path $home_ '.claude\CLAUDE.md'
$codex = Join-Path $home_ '.codex'
$pathBefore = Get-UserPath
New-Item -ItemType Directory -Force -Path (Split-Path $claude), (Join-Path $root 'temp') | Out-Null

$env:HOME = $home_
$env:USERPROFILE = $home_
$env:CODEX_HOME = $codex
$env:TEMP = Join-Path $root 'temp'
$env:TMP = $env:TEMP
$env:DALZZACK_OUTPUT = 'full'
$env:DALZZACK_INSTALL_DIR = $dir
$env:DALZZACK_VERSION = $Tag
Remove-Item Env:DALZZACK_NO_MODIFY_PATH -ErrorAction SilentlyContinue

Set-Content -Path $claude -Value "# My notes`n" -NoNewline -Encoding utf8NoBOM

try {
    $script = Invoke-RestMethod "https://raw.githubusercontent.com/gosuda/dalzzack/$Tag/scripts/install.ps1"
    Invoke-Expression $script *> $null
    if (-not (Test-Path $exe)) { Fail "install.ps1 installs $Tag" }
    Pass "install.ps1 installs $Tag and checks its hash"

    if ((& $exe --version) -ne "dalzzack $version") { Fail "--version prints dalzzack $version" }
    Pass "--version prints dalzzack $version"

    if ((Get-UserPath) -notlike "*$dir*") { Fail 'install.ps1 adds the user Path entry' }
    Pass 'install.ps1 adds the user Path entry'

    & $exe install all *> $null
    $text = Get-Content $claude -Raw
    if ($text -notlike '*dalzzack-begin*') { Fail 'install all writes CLAUDE.md' }
    if ((Get-Content (Join-Path $codex 'AGENTS.md') -Raw) -notlike '*dalzzack-begin*') { Fail 'install all writes the Codex guide' }
    if ($text -notlike '*# My notes*') { Fail 'install all keeps the existing text' }
    Pass 'install all writes both guides and keeps the existing text'

    & $exe start --id $id --private-profile --ttl 5m *> $null
    & $exe navigate -i $id --timeout-ms 3000 --url 'data:text/html,%3Ch1%3Edalzzack%20verify%3C/h1%3E' *> $null
    if ((& $exe get-text -i $id | Out-String) -notlike '*dalzzack verify*') { Fail 'get-text reads the page' }
    Pass 'start, navigate, and get-text work'

    & $exe start --id "$id-kill" --private-profile --ttl 5m *> $null
    $killed = & $exe kill --id "$id-kill" | Out-String
    if ($killed -notmatch '"status":\s*"killed"') {
        Write-Host $killed
        Fail 'kill ends a real daemon'
    }
    Pass 'kill ends a real daemon'

    $files = $claude, (Join-Path $codex 'AGENTS.md'), $exe
    $before = ($files | ForEach-Object { (Get-FileHash $_).Hash }) -join ' '
    $plan = & $exe uninstall | Out-String
    if ($plan -notmatch '"status":\s*"planned"') { Fail 'uninstall alone reports planned' }
    $after = ($files | ForEach-Object { (Get-FileHash $_).Hash }) -join ' '
    if ($before -ne $after) { Fail 'uninstall alone changes no file' }
    if ((Get-UserPath) -notlike "*$dir*") { Fail 'uninstall alone keeps the user Path entry' }
    if ((& $exe get-text -i $id | Out-String) -notlike '*dalzzack verify*') { Fail 'uninstall alone keeps the daemon' }
    Pass 'uninstall alone reports planned and changes nothing'

    $done = & $exe uninstall --yes | Out-String
    if ($done -notmatch '"status":\s*"completed"') {
        Write-Host $done
        Fail 'uninstall --yes completes'
    }
    Pass 'uninstall --yes completes'

    for ($i = 0; $i -lt 40 -and (Test-Path $dir); $i++) { Start-Sleep -Milliseconds 250 }
    if (Test-Path $dir) { Fail 'the binary and its folder are gone' }
    Pass 'the binary and its folder are gone'

    if ((Get-UserPath) -cne $pathBefore) { Fail 'the user Path matches its original value' }
    Pass 'the user Path matches its original value'

    if ((Get-Content $claude -Raw) -cne "# My notes`n") { Fail 'CLAUDE.md matches its original text' }
    Pass 'CLAUDE.md matches its original text'

    if (Test-Path (Join-Path $codex 'AGENTS.md')) { Fail 'the Codex guide is gone' }
    Pass 'the Codex guide is gone'

    Write-Host "ALL PASSED $Tag"
} finally {
    if (Test-Path $exe) { & $exe kill --id $id *> $null }
    Remove-Item $root -Recurse -Force -ErrorAction SilentlyContinue
    # A failed run must not leave the temporary folder on the real user Path.
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
    try {
        $entries = @($key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames) -split ';')
        $kept = @($entries -ne $dir)
        if ($kept.Count -ne $entries.Count) {
            $key.SetValue('Path', ($kept -join ';'), [Microsoft.Win32.RegistryValueKind]::ExpandString)
        }
    } finally {
        $key.Close()
    }
}
