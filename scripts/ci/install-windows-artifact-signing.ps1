$ErrorActionPreference = "Stop"
Set-StrictMode -Version 3.0

$moduleName = "ArtifactSigning"
$moduleVersion = "0.1.8"
$moduleSha256 = "3221344b8c627915d3870f23e80816f31a5d8c2bae1d7c0cdd6c9652f6c4e089"
# The gallery API redirects to its CDN; try both, since either front end has
# failed alone. Every download must match the pinned SHA-256.
$moduleUrls = @(
  "https://www.powershellgallery.com/api/v2/package/$moduleName/$moduleVersion",
  "https://cdn.powershellgallery.com/packages/$($moduleName.ToLowerInvariant()).$moduleVersion.nupkg"
)
$attempts = 4

if ([string]::IsNullOrWhiteSpace($env:RUNNER_TEMP)) {
  $baseTemp = [System.IO.Path]::GetTempPath()
} else {
  $baseTemp = $env:RUNNER_TEMP
}

$moduleRoot = Join-Path $baseTemp "maple-powershell-modules"
$moduleDir = Join-Path $moduleRoot "$moduleName/$moduleVersion"
$downloadDir = Join-Path $baseTemp "maple-powershell-downloads"
$packagePath = Join-Path $downloadDir "$moduleName.$moduleVersion.nupkg"

New-Item -ItemType Directory -Force -Path $downloadDir | Out-Null
New-Item -ItemType Directory -Force -Path $moduleRoot | Out-Null

$verified = $false
for ($attempt = 1; $attempt -le $attempts -and -not $verified; $attempt++) {
  foreach ($moduleUrl in $moduleUrls) {
    Remove-Item -LiteralPath $packagePath -Force -ErrorAction SilentlyContinue
    try {
      Invoke-WebRequest -Uri $moduleUrl -OutFile $packagePath -TimeoutSec 120
    } catch {
      Write-Warning "Attempt ${attempt}/${attempts}: $moduleUrl failed: $($_.Exception.Message)"
      continue
    }
    $actualSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $packagePath).Hash.ToLowerInvariant()
    if ($actualSha256 -eq $moduleSha256) {
      $verified = $true
      break
    }
    Write-Warning "Attempt ${attempt}/${attempts}: $moduleUrl served SHA-256 $actualSha256, expected $moduleSha256."
  }
  if (-not $verified -and $attempt -lt $attempts) {
    Start-Sleep -Seconds (15 * $attempt)
  }
}
if (-not $verified) {
  throw "$moduleName $moduleVersion with SHA-256 $moduleSha256 could not be downloaded from $($moduleUrls -join ' or ')."
}

if (Test-Path -LiteralPath $moduleDir) {
  Remove-Item -LiteralPath $moduleDir -Recurse -Force
}
New-Item -ItemType Directory -Force -Path $moduleDir | Out-Null
Expand-Archive -LiteralPath $packagePath -DestinationPath $moduleDir -Force

$manifestPath = Join-Path $moduleDir "$moduleName.psd1"
Import-Module -Name $manifestPath -Force -ErrorAction Stop
$loadedModule = Get-Module $moduleName | Where-Object { $_.Version -eq [version]$moduleVersion } | Select-Object -First 1
if (-not $loadedModule) {
  throw "$moduleName $moduleVersion was not loaded from $manifestPath."
}
if (-not (Get-Command Invoke-ArtifactSigning -ErrorAction SilentlyContinue)) {
  throw "Invoke-ArtifactSigning was not exported by $manifestPath."
}

if (-not [string]::IsNullOrWhiteSpace($env:GITHUB_ENV)) {
  "MAPLE_WINDOWS_ARTIFACT_SIGNING_MODULE_ROOT=$moduleRoot" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
  if ([string]::IsNullOrWhiteSpace($env:PSModulePath)) {
    "PSModulePath=$moduleRoot" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
  } else {
    "PSModulePath=$moduleRoot;$env:PSModulePath" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
  }
}

Write-Host "$moduleName $moduleVersion installed with verified SHA-256 $moduleSha256"
