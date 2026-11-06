# Installs the latest dalzzack release on Windows.
#
#   irm https://raw.githubusercontent.com/gosuda/dalzzack/main/scripts/install.ps1 | iex
#
# Environment:
#   DALZZACK_INSTALL_DIR     Where to put the binary
#                            (default: %LOCALAPPDATA%\Programs\dalzzack).
#   DALZZACK_NO_MODIFY_PATH  Set to 1 to leave your user PATH alone.
#   DALZZACK_VERSION         The release tag to install, such as v1.1.0-rc.2
#                            (default: the latest release).
#
# Rules for this file:
# - It runs through `iex`, inside the session of the user. `exit` would close
#   that window, so report errors with `throw`. Preferences set outside the
#   script block would stay in that session.
# - Keep it ASCII. Windows PowerShell 5.1 reads it with the wrong encoding
#   otherwise.

& {
    $ErrorActionPreference = 'Stop'
    # Invoke-WebRequest on 5.1 is very slow while it draws a progress bar.
    $ProgressPreference = 'SilentlyContinue'
    # 5.1 can default to a TLS version that GitHub refuses.
    [Net.ServicePointManager]::SecurityProtocol =
        [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    if ($env:OS -ne 'Windows_NT') {
        throw 'This script is for Windows. On macOS, use install.sh.'
    }

    $asset = 'dalzzack-windows-x64.zip'
    $baseUrl = 'https://github.com/gosuda/dalzzack/releases/latest/download'
    if ($env:DALZZACK_VERSION) {
        $baseUrl = "https://github.com/gosuda/dalzzack/releases/download/$env:DALZZACK_VERSION"
    }

    $dir = $env:DALZZACK_INSTALL_DIR
    if (-not $dir) { $dir = Join-Path $env:LOCALAPPDATA 'Programs\dalzzack' }
    $dest = Join-Path $dir 'dalzzack.exe'
    $existing = Get-Command dalzzack -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1

    $tmp = Join-Path ([IO.Path]::GetTempPath()) "dalzzack-$([guid]::NewGuid())"
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        $zip = Join-Path $tmp $asset
        Write-Host "Downloading $asset"
        Invoke-WebRequest -UseBasicParsing -Uri "$baseUrl/$asset" -OutFile $zip
        Invoke-WebRequest -UseBasicParsing -Uri "$baseUrl/$asset.sha256" -OutFile "$zip.sha256"

        $expected = ((Get-Content "$zip.sha256" -Raw) -split '\s+')[0].ToLower()
        $actual = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
        if ($expected -ne $actual) {
            throw "Checksum mismatch for ${asset}: expected $expected, got $actual"
        }

        $unpacked = Join-Path $tmp 'unpacked'
        Expand-Archive -Path $zip -DestinationPath $unpacked
        New-Item -ItemType Directory -Path $dir -Force | Out-Null

        # Windows can rename a running exe, but it cannot overwrite one. A daemon
        # keeps running from the old file until you stop it.
        if (Test-Path $dest) {
            $old = "$dest.old"
            Remove-Item $old -Force -ErrorAction SilentlyContinue
            try {
                Move-Item $dest $old -Force
            } catch {
                throw "Cannot replace $dest. Stop the running daemons (dalzzack list, then dalzzack stop --id <id>), then run this again."
            }
        }
        Move-Item (Join-Path $unpacked 'dalzzack.exe') $dest
    } finally {
        Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }

    $version = & $dest --version
    if ($LASTEXITCODE -ne 0) { throw "$dest does not run" }
    Write-Host "Installed $version to $dest"

    if ($env:DALZZACK_NO_MODIFY_PATH -ne '1') {
        # Use the registry, not [Environment]::GetEnvironmentVariable: that call
        # expands %VAR% entries, and writing its result back would freeze them.
        $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
        try {
            $raw = $key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
            if (($raw -split ';') -notcontains $dir) {
                $key.SetValue('Path', "$($raw.TrimEnd(';'));$dir".TrimStart(';'), [Microsoft.Win32.RegistryValueKind]::ExpandString)
                # The registry write does not tell running programs. Setting and
                # clearing a variable through this API broadcasts the change.
                [Environment]::SetEnvironmentVariable('DALZZACK_PATH_REFRESH', '1', 'User')
                [Environment]::SetEnvironmentVariable('DALZZACK_PATH_REFRESH', [NullString]::Value, 'User')
                Write-Host "Added $dir to your user PATH"
            }
        } finally {
            $key.Close()
        }
        if (($env:Path -split ';') -notcontains $dir) { $env:Path = "$env:Path;$dir" }
    }

    if ($existing -and $existing.Source -ne $dest) {
        Write-Warning "Another dalzzack comes first on PATH at $($existing.Source). Delete it, or it hides this install."
    }

    Write-Host ''
    Write-Host 'Next, open a new terminal and give your agents the usage guide:'
    Write-Host '  dalzzack install all'
    Write-Host ''
    Write-Host 'To update later:'
    Write-Host '  dalzzack upgrade'
}
