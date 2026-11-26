param(
    [string]$Version = "",
    [string]$InstallDir = ""
)

$ErrorActionPreference = "Stop"
try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
} catch {}
$Repo = "Cukkoo12/ctxproof"
$Target = "x86_64-pc-windows-msvc"

if (-not $Version) { $Version = $env:CTXPROOF_VERSION }
if (-not $InstallDir) { $InstallDir = $env:CTXPROOF_INSTALL_DIR }

if ([Environment]::Is64BitOperatingSystem -eq $false) {
    throw "Unsupported 32-bit Windows host. Install from source via: cargo install --git https://github.com/$Repo --locked"
}

if (-not $InstallDir) {
    $existing = Get-Command ctxproof -ErrorAction SilentlyContinue
    if ($existing -and $existing.Source -and ($existing.Source -like "*\ctxproof.exe")) {
        $InstallDir = Split-Path -Parent $existing.Source
    } elseif (Test-Path (Join-Path $env:USERPROFILE ".cargo\bin\ctxproof.exe")) {
        $InstallDir = Join-Path $env:USERPROFILE ".cargo\bin"
    } else {
        $InstallDir = Join-Path $env:LOCALAPPDATA "ctxproof\bin"
    }
}

if (-not $Version) {
    $req = [System.Net.HttpWebRequest]::Create("https://github.com/$Repo/releases/latest")
    $req.Method = "HEAD"
    $req.AllowAutoRedirect = $false
    $req.UserAgent = "ctxproof-installer"
    $resp = $req.GetResponse()
    try {
        $location = $resp.Headers["Location"]
    } finally {
        $resp.Close()
    }
    if (-not $location -or ($location -notmatch "/tag/(v[^/]+)$")) {
        throw "Could not resolve latest release tag from GitHub."
    }
    $Version = $Matches[1]
}

$Archive = "ctxproof-$Version-$Target.zip"
$BaseUrl = "https://github.com/$Repo/releases/download/$Version"
$TmpDir = Join-Path ([System.IO.Path]::GetTempPath()) ("ctxproof-install-" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $TmpDir | Out-Null

try {
    $ZipPath = Join-Path $TmpDir $Archive
    $ShaPath = Join-Path $TmpDir "$Archive.sha256"

    Write-Host "Downloading ctxproof $Version ($Target)..."
    Invoke-WebRequest -Uri "$BaseUrl/$Archive" -OutFile $ZipPath -UseBasicParsing
    Invoke-WebRequest -Uri "$BaseUrl/$Archive.sha256" -OutFile $ShaPath -UseBasicParsing

    $ExpectedSha = ((Get-Content $ShaPath -Raw).Trim() -split '\s+')[0].ToLowerInvariant()
    $ActualSha = (Get-FileHash $ZipPath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($ExpectedSha -ne $ActualSha) {
        throw "SHA256 verification failed for $Archive (expected $ExpectedSha, got $ActualSha)"
    }

    $ExtractDir = Join-Path $TmpDir "extract"
    Expand-Archive -Path $ZipPath -DestinationPath $ExtractDir -Force
    $ExtractedExe = Join-Path $ExtractDir "ctxproof.exe"
    if (-not (Test-Path $ExtractedExe)) {
        throw "Archive did not contain ctxproof.exe"
    }

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    $DstExe = Join-Path $InstallDir "ctxproof.exe"
    $StagedExe = Join-Path $InstallDir "ctxproof.new.exe"
    Copy-Item $ExtractedExe $StagedExe -Force

    # Swap atomically so active MCP processes holding ctxproof.exe do not fail with OS lock error 5.
    if (Test-Path $DstExe) {
        $OldExe = Join-Path $InstallDir ("ctxproof.old." + [DateTimeOffset]::UtcNow.ToUnixTimeSeconds() + ".exe")
        [System.IO.File]::Move($DstExe, $OldExe)
    }
    [System.IO.File]::Move($StagedExe, $DstExe)

    Get-ChildItem $InstallDir -Filter "ctxproof.old.*.exe" -ErrorAction SilentlyContinue | ForEach-Object {
        Remove-Item $_.FullName -Force -ErrorAction SilentlyContinue
    }

    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $PathEntries = if ($UserPath) { $UserPath -split ';' | Where-Object { $_ } } else { @() }
    if ($PathEntries -notcontains $InstallDir) {
        $UpdatedPath = (@($PathEntries) + $InstallDir) -join ';'
        [Environment]::SetEnvironmentVariable("Path", $UpdatedPath, "User")
        $env:PATH = "$InstallDir;$env:PATH"
        Write-Host "Added $InstallDir to your User PATH (reopen terminal if not detected immediately)."
    }

    Write-Host "Installed $(& $DstExe --version) to $DstExe"
    Write-Host "Run 'ctxproof init' inside any repository to configure .ctxproof.toml and MCP rules."
} finally {
    Remove-Item $TmpDir -Recurse -Force -ErrorAction SilentlyContinue
}
