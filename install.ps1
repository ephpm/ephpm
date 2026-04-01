# ePHPm installer for Windows — https://get.ephpm.dev
#
# Usage (PowerShell):
#   irm https://get.ephpm.dev/windows | iex
#   irm https://get.ephpm.dev/windows | iex -Args "--no-service"
#
# Options:
#   --no-service     Install binary only, skip Windows service setup
#   --no-config      Skip creating default config file
#   --uninstall      Remove ePHPm binary, service, and config
#
# Environment variables:
#   $env:EPHPM_VERSION      Specific version to install (default: latest)
#   $env:EPHPM_INSTALL_DIR  Install directory (default: C:\Program Files\ephpm)
#   $env:EPHPM_CONFIG_DIR   Config directory (default: C:\ProgramData\ephpm)
#   $env:EPHPM_DATA_DIR     Data directory (default: C:\ephpm\www)

param(
    [switch]$NoService,
    [switch]$NoConfig,
    [switch]$Uninstall
)

$ErrorActionPreference = "Stop"

# --- defaults ---
$GithubRepo = "ephpm/ephpm"
$InstallDir = if ($env:EPHPM_INSTALL_DIR) { $env:EPHPM_INSTALL_DIR } else { "$env:ProgramFiles\ephpm" }
$ConfigDir = if ($env:EPHPM_CONFIG_DIR) { $env:EPHPM_CONFIG_DIR } else { "$env:ProgramData\ephpm" }
$DataDir = if ($env:EPHPM_DATA_DIR) { $env:EPHPM_DATA_DIR } else { "C:\ephpm\www" }
$ServiceName = "ephpm"

# --- helpers ---
function Write-Info { param($Message) Write-Host "[INFO] " -ForegroundColor Blue -NoNewline; Write-Host $Message }
function Write-Ok { param($Message) Write-Host "[OK] " -ForegroundColor Green -NoNewline; Write-Host $Message }
function Write-Warn { param($Message) Write-Host "[WARN] " -ForegroundColor Yellow -NoNewline; Write-Host $Message }
function Write-Fatal { param($Message) Write-Host "[ERROR] " -ForegroundColor Red -NoNewline; Write-Host $Message; exit 1 }

# --- check admin ---
function Test-Admin {
    $identity = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
    return $identity.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

# --- uninstall ---
function Invoke-Uninstall {
    Write-Info "Uninstalling ePHPm..."

    # Stop and remove service
    $svc = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
    if ($svc) {
        if ($svc.Status -eq "Running") {
            Stop-Service -Name $ServiceName -Force
            Write-Ok "Stopped $ServiceName service"
        }
        sc.exe delete $ServiceName | Out-Null
        Write-Ok "Removed $ServiceName service"
    }

    # Remove binary
    $bin = Join-Path $InstallDir "ephpm.exe"
    if (Test-Path $bin) {
        Remove-Item $bin -Force
        Write-Ok "Removed $bin"
    }

    # Remove from PATH
    $machinePath = [Environment]::GetEnvironmentVariable("Path", "Machine")
    if ($machinePath -like "*$InstallDir*") {
        $newPath = ($machinePath -split ";" | Where-Object { $_ -ne $InstallDir }) -join ";"
        [Environment]::SetEnvironmentVariable("Path", $newPath, "Machine")
        Write-Ok "Removed $InstallDir from PATH"
    }

    # Remove install dir if empty
    if ((Test-Path $InstallDir) -and -not (Get-ChildItem $InstallDir)) {
        Remove-Item $InstallDir -Force
    }

    Write-Warn "Config directory $ConfigDir was NOT removed (contains your config)"
    Write-Warn "Data directory $DataDir was NOT removed (contains your sites)"
    Write-Ok "ePHPm uninstalled"
    exit 0
}

# --- resolve version ---
function Get-LatestVersion {
    if ($env:EPHPM_VERSION) {
        Write-Info "Using specified version: $env:EPHPM_VERSION"
        return $env:EPHPM_VERSION
    }

    Write-Info "Finding latest version..."
    try {
        $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$GithubRepo/releases/latest"
        $version = $release.tag_name -replace "^v", ""
        Write-Info "Latest version: $version"
        return $version
    }
    catch {
        Write-Fatal "Could not determine latest version. Set `$env:EPHPM_VERSION manually."
    }
}

# --- download ---
function Install-Binary {
    param($Version)

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

    $binaryName = "ephpm-windows-x86_64"
    $downloadUrl = "https://github.com/$GithubRepo/releases/download/v${Version}/${binaryName}.zip"
    $tmpDir = Join-Path $env:TEMP "ephpm-install-$(Get-Random)"
    $zipPath = Join-Path $tmpDir "ephpm.zip"

    New-Item -ItemType Directory -Force -Path $tmpDir | Out-Null

    Write-Info "Downloading ePHPm v${Version} for windows/x86_64..."
    Write-Info "URL: $downloadUrl"

    try {
        Invoke-WebRequest -Uri $downloadUrl -OutFile $zipPath -UseBasicParsing
    }
    catch {
        # Try raw .exe
        $downloadUrl = "https://github.com/$GithubRepo/releases/download/v${Version}/${binaryName}.exe"
        Write-Info "Trying: $downloadUrl"
        try {
            Invoke-WebRequest -Uri $downloadUrl -OutFile (Join-Path $tmpDir "ephpm.exe") -UseBasicParsing
        }
        catch {
            Remove-Item $tmpDir -Recurse -Force -ErrorAction SilentlyContinue
            Write-Fatal "Download failed. Check https://github.com/$GithubRepo/releases"
        }
    }

    # Extract if zip
    if (Test-Path $zipPath) {
        Expand-Archive -Path $zipPath -DestinationPath $tmpDir -Force
    }

    # Find the binary
    $ephpmExe = Get-ChildItem -Path $tmpDir -Filter "ephpm.exe" -Recurse | Select-Object -First 1
    if (-not $ephpmExe) {
        Remove-Item $tmpDir -Recurse -Force
        Write-Fatal "ephpm.exe not found in download"
    }

    Copy-Item $ephpmExe.FullName (Join-Path $InstallDir "ephpm.exe") -Force
    Remove-Item $tmpDir -Recurse -Force

    Write-Ok "Installed $(Join-Path $InstallDir 'ephpm.exe')"
}

# --- add to PATH ---
function Add-ToPath {
    $machinePath = [Environment]::GetEnvironmentVariable("Path", "Machine")
    if ($machinePath -notlike "*$InstallDir*") {
        [Environment]::SetEnvironmentVariable("Path", "$machinePath;$InstallDir", "Machine")
        $env:Path = "$env:Path;$InstallDir"
        Write-Ok "Added $InstallDir to system PATH"
    }
}

# --- create config ---
function New-DefaultConfig {
    if ($NoConfig) { return }

    New-Item -ItemType Directory -Force -Path $ConfigDir | Out-Null
    New-Item -ItemType Directory -Force -Path "$DataDir\html" | Out-Null
    New-Item -ItemType Directory -Force -Path "$DataDir\sites" | Out-Null

    $configPath = Join-Path $ConfigDir "ephpm.toml"
    if (Test-Path $configPath) {
        Write-Info "Config already exists at $configPath, skipping"
        return
    }

    $dataPathEscaped = $DataDir -replace "\\", "/"

    @"
# ePHPm configuration
# Full reference: https://github.com/ephpm/ephpm

[server]
listen = "0.0.0.0:8080"
document_root = "$dataPathEscaped/html"
# sites_dir = "$dataPathEscaped/sites"   # uncomment for virtual hosting

[php]
memory_limit = "128M"
max_execution_time = 30

# Uncomment for automatic HTTPS:
# [server.tls]
# acme_domains = ["example.com"]
# acme_email = "you@example.com"

# Uncomment for embedded SQLite database:
# [db.sqlite]
# path = "$dataPathEscaped/data/ephpm.db"
"@ | Set-Content $configPath -Encoding UTF8

    Write-Ok "Created $configPath"

    # Default index page
    $indexPath = Join-Path "$DataDir\html" "index.php"
    if (-not (Test-Path $indexPath)) {
        @"
<?php
echo "<h1>ePHPm is running!</h1>";
echo "<p>PHP " . PHP_VERSION . "</p>";
echo "<p>Server: Windows</p>";
echo "<p>Edit $($DataDir -replace '\\','\\')\html\index.php or configure your site.</p>";
"@ | Set-Content $indexPath -Encoding UTF8
        Write-Ok "Created default index.php"
    }
}

# --- windows service ---
function Install-Service {
    if ($NoService) { return }

    $binPath = Join-Path $InstallDir "ephpm.exe"
    $configPath = Join-Path $ConfigDir "ephpm.toml"

    # Check for existing service
    $svc = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
    if ($svc) {
        if ($svc.Status -eq "Running") {
            Stop-Service -Name $ServiceName -Force
        }
        sc.exe delete $ServiceName | Out-Null
        Start-Sleep -Seconds 1
    }

    # Create service using sc.exe
    $scArgs = "create $ServiceName binPath= `"$binPath --config $configPath`" start= auto DisplayName= `"ePHPm PHP Application Server`""
    sc.exe $scArgs.Split(" ") | Out-Null

    # Set description
    sc.exe description $ServiceName "ePHPm - Embedded PHP Manager. Single-binary PHP application server." | Out-Null

    # Set restart on failure
    sc.exe failure $ServiceName reset= 86400 actions= restart/5000/restart/10000/restart/30000 | Out-Null

    # Start the service
    Start-Service -Name $ServiceName
    Write-Ok "Created and started Windows service: $ServiceName"
}

# --- summary ---
function Write-Summary {
    param($Version)

    Write-Host ""
    Write-Host "--------------------------------------------" -ForegroundColor Green
    Write-Host "ePHPm v${Version} installed successfully" -ForegroundColor Green
    Write-Host "--------------------------------------------" -ForegroundColor Green
    Write-Host ""
    Write-Host "  Binary:    $(Join-Path $InstallDir 'ephpm.exe')"
    if (-not $NoConfig) {
        Write-Host "  Config:    $(Join-Path $ConfigDir 'ephpm.toml')"
        Write-Host "  Doc root:  $DataDir\html"
    }
    if (-not $NoService) {
        Write-Host "  Service:   Get-Service $ServiceName"
        Write-Host ""

        $ip = (Get-NetIPAddress -AddressFamily IPv4 | Where-Object { $_.InterfaceAlias -notlike "*Loopback*" } | Select-Object -First 1).IPAddress
        if ($ip) {
            Write-Host "  Your site is live at: http://${ip}:8080"
        }
    }
    Write-Host ""
    Write-Host "  Quick start:"
    Write-Host "    ephpm --config $(Join-Path $ConfigDir 'ephpm.toml')"
    Write-Host ""
    Write-Host "  Docs:      https://github.com/ephpm/ephpm"
    Write-Host ""

    if (-not $NoService) {
        Write-Warn "Note: Clustered SQLite (sqld) is not available on Windows."
        Write-Warn "Single-node SQLite and DB proxy work fully."
    }
}

# --- main ---
function Main {
    Write-Host ""
    Write-Host "ePHPm Installer for Windows" -ForegroundColor Green
    Write-Host ""

    if (-not (Test-Admin)) {
        if (-not $NoService) {
            Write-Fatal "Administrator privileges required. Right-click PowerShell and 'Run as Administrator', or use --no-service"
        }
        Write-Warn "Running without admin — installing to user directory"
        $script:InstallDir = Join-Path $env:LOCALAPPDATA "ephpm"
    }

    if ($Uninstall) {
        Invoke-Uninstall
        return
    }

    $version = Get-LatestVersion

    # Check existing
    $existing = Join-Path $InstallDir "ephpm.exe"
    if (Test-Path $existing) {
        $current = & $existing --version 2>&1 | ForEach-Object { ($_ -split " ")[1] }
        Write-Info "Existing installation found: v${current}"
        Write-Info "Upgrading to v${version}"
    }

    Install-Binary -Version $version
    Add-ToPath
    New-DefaultConfig
    Install-Service
    Write-Summary -Version $version
}

Main
