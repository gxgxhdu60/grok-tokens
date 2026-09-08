# grok-tokens installer for Windows PowerShell 5.1+
#
#   irm https://github.com/gxgxhdu60/grok-tokens/releases/latest/download/install.ps1 | iex
#
$ErrorActionPreference = "Stop"
try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
} catch {
}

$RepoSlug = if ($env:GROK_TOKENS_REPO) { $env:GROK_TOKENS_REPO } else { "gxgxhdu60/grok-tokens" }
$InstallDir = if ($env:GROK_TOKENS_INSTALL_DIR) {
    $env:GROK_TOKENS_INSTALL_DIR
} else {
    Join-Path $env:LOCALAPPDATA "grok-tokens"
}
$BinaryName = "grok-tokens.exe"
$Destination = Join-Path $InstallDir $BinaryName

Write-Host "Installing grok-tokens..."

$Architecture = $env:PROCESSOR_ARCHITECTURE
if ($Architecture -eq "ARM64") {
    throw "ARM64 Windows is not published yet. Use x64 (or WSL: curl .../install.sh | sh)."
}

$AssetName = "grok-tokens-x86_64-pc-windows-msvc.tar.gz"
$CandidateUrls = @(
    "https://github.com/$RepoSlug/releases/latest/download/$AssetName"
    "https://ghfast.top/https://github.com/$RepoSlug/releases/latest/download/$AssetName"
    "https://ghproxy.net/https://github.com/$RepoSlug/releases/latest/download/$AssetName"
    "https://mirror.ghproxy.com/https://github.com/$RepoSlug/releases/latest/download/$AssetName"
)

$TempRoot = Join-Path $env:TEMP ("grok-tokens-" + [guid]::NewGuid().ToString("n"))
New-Item -ItemType Directory -Path $TempRoot | Out-Null

try {
    $ArchivePath = Join-Path $TempRoot $AssetName
    $ExtractDir = Join-Path $TempRoot "extract"
    New-Item -ItemType Directory -Path $ExtractDir | Out-Null

    $Downloaded = $false
    foreach ($Url in $CandidateUrls) {
        Write-Host "Trying $Url ..."
        try {
            Invoke-WebRequest -Uri $Url -OutFile $ArchivePath -UseBasicParsing
            $Downloaded = $true
            break
        } catch {
            Write-Host "  failed: $($_.Exception.Message)"
        }
    }

    if (-not $Downloaded) {
        throw "Download failed. Open https://github.com/$RepoSlug/releases/latest and download the Windows tarball."
    }

    tar -xzf $ArchivePath -C $ExtractDir
    $ExtractedBinary = Get-ChildItem -Path $ExtractDir -Filter $BinaryName -Recurse | Select-Object -First 1
    if (-not $ExtractedBinary) {
        throw "Archive did not contain $BinaryName"
    }

    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    Copy-Item -Path $ExtractedBinary.FullName -Destination $Destination -Force

    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if (-not $UserPath) { $UserPath = "" }
    $PathParts = $UserPath -split ";" | Where-Object { $_ -ne "" }
    if ($PathParts -notcontains $InstallDir) {
        $UpdatedPath = if ($UserPath.Trim() -eq "") { $InstallDir } else { "$InstallDir;$UserPath" }
        [Environment]::SetEnvironmentVariable("Path", $UpdatedPath, "User")
        $env:Path = "$InstallDir;$env:Path"
        Write-Host "Added $InstallDir to the user PATH"
    }

    Write-Host "Installed: $Destination"
    & $Destination --version
    Write-Host "Open a new terminal, then run: grok-tokens daily"
} finally {
    Remove-Item -LiteralPath $TempRoot -Recurse -Force -ErrorAction SilentlyContinue
}
