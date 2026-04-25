<#
.SYNOPSIS
    ecal-mcp installer for Windows.

.DESCRIPTION
    Resolves a release tag (defaults to "latest"), downloads the matching
    .zip + .sha256 from GitHub Releases, verifies the checksum, and installs
    ecal-mcp.exe into a prefix (default: %LOCALAPPDATA%\Programs\ecal-mcp).
    Adds the bin directory to the *user* PATH if it isn't already there.

.PARAMETER Version
    Release tag (e.g. v0.2.0). Defaults to "latest". Override via -Version
    or $env:ECAL_MCP_VERSION.

.PARAMETER Prefix
    Install prefix. The exe ends up in <Prefix>\bin\ecal-mcp.exe.
    Override via -Prefix or $env:ECAL_MCP_PREFIX.

.PARAMETER Repo
    GitHub repo "owner/name". Override via -Repo or $env:ECAL_MCP_REPO.

.EXAMPLE
    irm https://zpg6.github.io/ecal-mcp/install.ps1 | iex
#>

[CmdletBinding()]
param(
    [string]$Version = $(if ($env:ECAL_MCP_VERSION) { $env:ECAL_MCP_VERSION } else { 'latest' }),
    [string]$Prefix  = $(if ($env:ECAL_MCP_PREFIX)  { $env:ECAL_MCP_PREFIX  } else { Join-Path $env:LOCALAPPDATA 'Programs\ecal-mcp' }),
    [string]$Repo    = $(if ($env:ECAL_MCP_REPO)    { $env:ECAL_MCP_REPO    } else { 'zpg6/ecal-mcp' })
)

$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

function Die($msg) { Write-Error "ecal-mcp install: $msg"; exit 1 }

switch -Wildcard ($env:PROCESSOR_ARCHITECTURE) {
    'AMD64' { $target = 'x86_64-pc-windows-msvc' }
    'ARM64' { Die "Windows ARM64 prebuilts are not published yet; build from source (see README)." }
    default { Die "unsupported architecture: $($env:PROCESSOR_ARCHITECTURE)" }
}

if ($Version -eq 'latest') {
    $api = "https://api.github.com/repos/$Repo/releases/latest"
} else {
    $api = "https://api.github.com/repos/$Repo/releases/tags/$Version"
}

$headers = @{ 'User-Agent' = 'ecal-mcp-installer' }
try {
    $release = Invoke-RestMethod -Uri $api -Headers $headers
} catch {
    Die "could not query release info from $api ($_)"
}
$tag = $release.tag_name
if (-not $tag) { Die "release info missing tag_name" }

$ver  = $tag.TrimStart('v')
$stem = "ecal-mcp-$ver-$target"
$base = "https://github.com/$Repo/releases/download/$tag"

$tmp = New-Item -ItemType Directory -Path (Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid())) -Force
try {
    $zip      = Join-Path $tmp "$stem.zip"
    $shaFile  = Join-Path $tmp "$stem.zip.sha256"

    Write-Host "Downloading $stem.zip from $tag..."
    Invoke-WebRequest -Uri "$base/$stem.zip"        -OutFile $zip     -Headers $headers
    Invoke-WebRequest -Uri "$base/$stem.zip.sha256" -OutFile $shaFile -Headers $headers

    $expected = ((Get-Content $shaFile -Raw).Trim() -split '\s+')[0].ToLower()
    $actual   = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower()
    if ($expected -ne $actual) { Die "sha256 mismatch for $stem.zip (expected $expected, got $actual)" }

    Expand-Archive -Path $zip -DestinationPath $tmp -Force
    $exe = Join-Path $tmp "$stem\ecal-mcp.exe"
    if (-not (Test-Path $exe)) { Die "extracted archive missing $exe" }

    $binDir = Join-Path $Prefix 'bin'
    New-Item -ItemType Directory -Path $binDir -Force | Out-Null
    Copy-Item -Path $exe -Destination (Join-Path $binDir 'ecal-mcp.exe') -Force

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $parts = @()
    if ($userPath) { $parts = $userPath -split ';' | Where-Object { $_ -ne '' } }
    if ($parts -notcontains $binDir) {
        [Environment]::SetEnvironmentVariable('Path', (($parts + $binDir) -join ';'), 'User')
        Write-Host "Added $binDir to user PATH (open a new shell to pick it up)."
    }

    Write-Host "Installed ecal-mcp $tag -> $(Join-Path $binDir 'ecal-mcp.exe')"
    Write-Host "Note: ecal-mcp requires the eCAL v6 runtime."
    Write-Host "      See https://github.com/eclipse-ecal/ecal/releases (Windows installer)."
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
