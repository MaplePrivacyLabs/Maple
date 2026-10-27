param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 3.0
$PSNativeCommandUseErrorActionPreference = $false

if ([System.Environment]::OSVersion.Platform -ne [System.PlatformID]::Win32NT) {
  throw 'Application typechecking requires native Windows.'
}
if (-not $env:RUNNER_TEMP -or $env:GITHUB_SHA -notmatch '^[0-9a-f]{40}$' -or
    -not $env:GITHUB_RUN_ID -or -not $env:GITHUB_RUN_ATTEMPT -or -not $env:GITHUB_REPOSITORY) {
  throw 'The isolated Actions typecheck context is missing.'
}
$repository = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
$report = Join-Path $env:RUNNER_TEMP 'maple-windows-app-typecheck'
New-Item -ItemType Directory -Path $report -Force | Out-Null
$receiptPath = Join-Path $report 'receipt.json'
$logPath = Join-Path $report 'cargo-check.log'
$receipt = [ordered]@{
  schema = 'maple-windows-app-typecheck-v1'
  source = $env:GITHUB_SHA
  repository = $env:GITHUB_REPOSITORY
  run_id = $env:GITHUB_RUN_ID
  run_attempt = $env:GITHUB_RUN_ATTEMPT
  started_at = [DateTimeOffset]::UtcNow.ToString('o')
  platform = 'x86_64-pc-windows-msvc'
  package = 'maple-agent-app'
  all_targets = $true
  result = 'running'
  app_all_targets_typechecked = $false
  runtime_tests_executed = $false
  app_code_generated = $false
  release_package_qualified = $false
}
$receipt | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $receiptPath -Encoding utf8

Push-Location (Join-Path $repository 'apps/maple-agent')
try {
  $identity = (& rustc -vV) -join "`n"
  if ($LASTEXITCODE -ne 0 -or
      $identity -notmatch '(?m)^release: 1\.98\.0$' -or
      $identity -notmatch '(?m)^host: x86_64-pc-windows-msvc$') {
    throw 'The Windows typecheck toolchain differs from the repository pin.'
  }
  $receipt.rust_release = '1.98.0'
  $arguments = @('check', '-p', 'maple-agent-app', '--all-targets', '--locked')
  & cargo @arguments *> $logPath
  $receipt.cargo_exit_code = $LASTEXITCODE
  if ($receipt.cargo_exit_code -ne 0) {
    Get-Content -LiteralPath $logPath -Tail 100
    throw 'Windows app all-targets typechecking failed.'
  }
  $receipt.app_all_targets_typechecked = $true
  $receipt.result = 'pass'
  Get-Content -LiteralPath $logPath -Tail 10
} catch {
  $receipt.result = 'fail'
  throw
} finally {
  $receipt.completed_at = [DateTimeOffset]::UtcNow.ToString('o')
  $receipt | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $receiptPath -Encoding utf8
  Pop-Location
}
