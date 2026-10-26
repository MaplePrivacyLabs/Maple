param(
  [Parameter(Mandatory = $true, Position = 0)]
  [string]$File,
  [switch]$VerifyOnly,
  [switch]$Unsigned
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version 3.0
if (-not (Test-Path -LiteralPath $File -PathType Leaf)) {
  throw "Windows artifact to verify is missing."
}
if (-not $VerifyOnly) {
  if ($Unsigned) { throw "Unsigned artifacts cannot enter the signing path." }
  $signer = Join-Path $PSScriptRoot "../../../scripts/ci/windows-artifact-sign.ps1"
  & $signer -File $File
}
$signature = Get-AuthenticodeSignature -LiteralPath $File
if ($Unsigned) {
  if ($signature.Status -ne "NotSigned") { throw "Expected an unsigned PR artifact." }
} else {
  $expectedCn = $env:MAPLE_WINDOWS_AUTHENTICODE_EXPECTED_CN
  if ([string]::IsNullOrWhiteSpace($expectedCn)) { $expectedCn = "Mutiny Wallet Inc. dba OpenSecret" }
  if ($signature.Status -ne "Valid" -or $null -eq $signature.SignerCertificate) {
    throw "Windows artifact has no valid Authenticode signature."
  }
  $pattern = "(^|,\s*)CN=$([regex]::Escape($expectedCn))(\s*,|$)"
  if (-not [regex]::IsMatch($signature.SignerCertificate.Subject, $pattern)) {
    throw "Windows artifact signer differs from the expected Maple publisher."
  }
  if ($null -eq $signature.TimeStamperCertificate) { throw "Windows artifact has no timestamp signature." }
}
Write-Host ("verified-agent-windows-signature  {0}" -f (Split-Path -Leaf $File))
