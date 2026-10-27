param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 3.0
$PSNativeCommandUseErrorActionPreference = $false

if ([System.Environment]::OSVersion.Platform -ne [System.PlatformID]::Win32NT) {
  throw 'Native Windows updater tests require Windows.'
}
if (-not $env:RUNNER_TEMP -or $env:GITHUB_SHA -notmatch '^[0-9a-f]{40}$' -or
    -not $env:GITHUB_RUN_ID -or -not $env:GITHUB_RUN_ATTEMPT -or -not $env:GITHUB_REPOSITORY) {
  throw 'The isolated Actions test context is missing.'
}
# These tests execute untrusted fixture data. Signing and publication credentials
# belong only to the separate protected producer/publisher jobs.
foreach ($name in @(
  'AZURE_CLIENT_ID', 'AZURE_TENANT_ID', 'AZURE_SUBSCRIPTION_ID',
  'CLOUDFLARE_API_TOKEN', 'BWS_ACCESS_TOKEN', 'AGENT_UPDATES_BWS_ACCESS_TOKEN',
  'AGENT_UPDATE_SIGNING_KEY', 'AGENT_R2_ACCESS_KEY_ID', 'AGENT_R2_SECRET_ACCESS_KEY'
)) {
  if (-not [string]::IsNullOrEmpty([System.Environment]::GetEnvironmentVariable($name))) {
    throw 'A signing or publication credential is present in native tests.'
  }
}
foreach ($entry in Get-ChildItem Env:) {
  if ($entry.Name.StartsWith('MAPLE_WINDOWS_ARTIFACT_SIGNING_')) {
    throw 'Artifact signing configuration is present in native tests.'
  }
}

$repository = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
$component = Join-Path $repository 'apps/maple-agent'
$reportDirectory = Join-Path $env:RUNNER_TEMP 'maple-windows-updater-tests'
New-Item -ItemType Directory -Path $reportDirectory -Force | Out-Null
$receiptPath = Join-Path $reportDirectory 'receipt.json'
$receipt = [ordered]@{
  schema = 'maple-windows-updater-native-tests-v1'
  source = $env:GITHUB_SHA
  repository = $env:GITHUB_REPOSITORY
  run_id = $env:GITHUB_RUN_ID
  run_attempt = $env:GITHUB_RUN_ATTEMPT
  started_at = [DateTimeOffset]::UtcNow.ToString('o')
  result = 'running'
  platform = 'x86_64-pc-windows-msvc'
  native_component_tests = $false
  official_maple_signature_qualified = $false
  signed_installed_a_to_b_qualified = $false
  visible_gui_upgrade_qualified = $false
  paired_remote_upgrade_qualified = $false
}

function Save-Receipt {
  $receipt | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $receiptPath -Encoding utf8
}

function Invoke-CargoStep {
  param([string]$LogName, [string[]]$CargoArguments)
  $path = Join-Path $reportDirectory $LogName
  & cargo @CargoArguments *> $path
  $exitCode = $LASTEXITCODE
  if ($exitCode -ne 0) {
    Get-Content -LiteralPath $path -Tail 150
    throw "Native updater Cargo step failed with exit code $exitCode."
  }
  return $path
}

Save-Receipt
try {
  $identity = (& rustc -vV) -join "`n"
  if ($LASTEXITCODE -ne 0 -or
      $identity -notmatch '(?m)^release: 1\.98\.0$' -or
      $identity -notmatch '(?m)^host: x86_64-pc-windows-msvc$') {
    throw 'The Windows Rust toolchain differs from the repository pin.'
  }
  $receipt.rust_release = '1.98.0'

  # Use the OS's signed PowerShell executable as an unrelated-publisher fixture.
  # It is never copied over or modified, and is not a downloaded test executable.
  $unrelated = Join-Path $env:SystemRoot 'System32/WindowsPowerShell/v1.0/powershell.exe'
  $signature = Get-AuthenticodeSignature -LiteralPath $unrelated
  if ($signature.Status -ne 'Valid' -or $null -eq $signature.SignerCertificate -or
      $signature.SignerCertificate.Subject -notmatch '(^|,\s*)CN=Microsoft' -or
      $signature.SignerCertificate.Subject -match '(^|,\s*)CN=Mutiny Wallet Inc\. dba OpenSecret(,|$)') {
    throw 'The unrelated signed Windows fixture is unavailable or has another identity.'
  }
  $receipt.unrelated_os_signature_valid = $true
  $receipt.unrelated_os_signature_type = [string]$signature.SignatureType

  Push-Location $component
  try {
    $listPath = Invoke-CargoStep 'test-list.log' @(
      'test', '-p', 'maple-agent-updater', '--locked', '--lib',
      '--test', 'windows_bootstrap_native', '--', '--list', '--format', 'terse'
    )
    $testNames = @(Get-Content -LiteralPath $listPath | ForEach-Object {
      if ($_ -match '^([A-Za-z0-9_:]+): test$') { $Matches[1] }
    })
    # Requiring native cases prevents an accidental cfg/filter change from
    # turning this Windows job into an empty or host-independent-only success.
    $requiredPatterns = @(
      'installer::windows_bundle::authenticode::tests::native_verification_rejects_missing_malformed_and_nonfiles',
      'installer::windows_bundle::authenticode::tests::native_unsigned_test_executable_requires_the_explicit_dev_exception',
      'installer::windows_bundle::authenticode::tests::native_microsoft_publisher_is_rejected_even_for_unsigned_dev',
      'installer::windows_bundle::authenticode::tests::signer_requires_the_exact_compiled_publisher',
      'installer::windows_bundle::authenticode::tests::pe_certificate_directory_must_be_completely_absent',
      'installer::windows_bundle::authenticode::tests::purpose_provider_flags_pin_startup_offline_and_admission_revocation',
      'installer::windows_bundle::authenticode::tests::native_trust_configuration_keeps_startup_and_admission_separate',
      'installer::windows_bundle::authenticode::tests::catalog_presence_and_lookup_errors_never_enable_unsigned_dev',
      'installer::windows_bundle::authenticode::tests::native_catalog_lookup_distinguishes_unsigned_and_windows_signed_executables',
      'installer::windows_bundle::windows_fs::tests::refuses_nonlocal_and_ambiguous_windows_names',
      'installer::windows_bundle::windows_fs::tests::private_storage_round_trip_and_atomic_replacement',
      'installer::windows_bundle::windows_fs::tests::rejects_hard_linked_state_and_preserves_oversized_file',
      'installer::windows_bundle::windows_fs::tests::rejects_existing_foreign_write_acl_before_truncation',
      'installer::windows_bundle::windows_fs::tests::rejects_file_and_ancestor_reparse_points_when_symlink_privilege_available',
      'installer::windows_bundle::windows_fs::tests::native_mutation_guard_blocks_loader_until_released',
      'installer::windows_bundle::windows_fs::tests::native_shared_read_validation_accepts_mapped_image_and_mutation_guard',
      'installer::windows_bundle::windows_fs::tests::native_owner_allowlist_rejects_unprivileged_sids',
      'installer::windows_bundle::windows_fs::tests::native_default_owner_storage_and_named_temp_atomic_replace',
      'installer::windows_bundle::tests::exact_archive_round_trip_binds_all_nine_files_and_three_directories',
      'installer::windows_bundle::tests::rejected_archive_never_begins_extraction',
      'installer::windows_bundle::tests::descriptors_bind_profile_product_release_and_strict_json_fields',
      'installer::windows_bundle::tests::descriptor_comparison_rejects_changed_release_version_build_or_source',
      'installer::windows_bundle::tests::current_pointer_cannot_name_a_path_or_ambiguous_generation',
      'installer::windows_bundle::tests::pe_metadata_parser_reads_x64_maple_section_without_running_code',
      'installer::windows_bundle::tests::pe_metadata_parser_rejects_header_architecture_and_bounds_changes',
      'installer::windows_bundle::tests::pe_metadata_parser_rejects_duplicate_writeable_executable_or_overlapping_sections',
      'installer::windows_bundle::transaction_tests::native_transaction_commits_pointer_and_preserves_old_generation',
      'installer::windows_bundle::transaction_tests::native_transaction_interruption_before_pointer_keeps_previous',
      'installer::windows_bundle::transaction_tests::native_transaction_pointer_lock_failure_is_retryable',
      'installer::windows_bundle::transaction_tests::native_install_lock_rejects_concurrent_writers',
      'installer::windows_bundle::transaction_tests::native_public_install_lock_refuses_unadmitted_root_without_pointer_or_lock',
      'installer::windows_bundle::transaction_tests::native_pending_manual_intent_preserves_current_generation',
      'installer::windows_bundle::transaction_tests::native_completed_manual_intent_recovers_after_pointer_commit',
      'installer::windows_bundle::transaction_tests::native_live_matching_manual_intent_is_never_cleared',
      'installer::windows_bundle::transaction_tests::native_uninstall_owner_fences_until_nsis_finishes',
      'installer::windows_bundle::transaction_tests::native_partial_generation_removal_preserves_unknown_files',
      'installer::windows_bundle::transaction_tests::native_retained_guard_removes_owned_generation_without_partial_pointer_change',
      'installer::windows_bundle::transaction_tests::native_dead_uninstall_journal_allows_only_owned_partial_repair',
      'tests::downloads_and_reauthenticates_preparation_across_restart',
      'tests::rollback_clears_offer_without_lowering_persisted_high_watermark',
      'tests::resumes_only_exact_valid_ranges_and_final_digest',
      'tests::interrupted_transfer_preserves_a_resumable_verified_prefix',
      'tests::platform_download_restore_revalidate_and_commit_use_their_exact_feed',
      'tests::wrong_target_feed_cannot_advance_rollback_memory_or_start_a_download',
      'tests::commit_guard_excludes_competing_advancement_without_blocking',
      'tests::newer_process_verification_between_ready_and_commit_blocks_old_installation',
      'tests::successful_commit_result_survives_loss_of_state_record_after_replacement',
      'tests::commit_guard_retains_windows_state_lock_against_deletion',
      'native_reparse_fixtures_are_available',
      'windows_bootstrap::tests::windows_restart_identity_is_canonical_and_bounded',
      'windows_bootstrap::tests::windows_current_process_has_stable_creation_identity',
      'windows_bootstrap::tests::windows_bootstrap_only_reserves_exact_internal_entrypoints',
      'windows_bootstrap::tests::windows_forward_preserves_native_argv_and_exit_status'
    )
    foreach ($pattern in $requiredPatterns) {
      if (@($testNames | Where-Object { $_ -match "^$pattern$" }).Count -eq 0) {
        throw 'A required native Windows test group is missing.'
      }
    }
    $receipt.discovered_tests = $testNames.Count
    $runPath = Invoke-CargoStep 'native-tests.log' @(
      'test', '-p', 'maple-agent-updater', '--locked', '--no-fail-fast', '--lib',
      '--test', 'windows_bootstrap_native', '--', '--test-threads=1'
    )
    $summaries = @(Get-Content -LiteralPath $runPath | Where-Object { $_ -match '^test result:' })
    if ($summaries.Count -ne 2 -or @($summaries | Where-Object {
      $_ -notmatch '^test result: ok\. [1-9][0-9]* passed; 0 failed; 0 ignored;'
    }).Count -ne 0) {
      throw 'Both native test binaries must execute nonempty suites without skipped tests.'
    }
    $receipt.test_summaries = $summaries
    $receipt.native_component_tests = $true
    $receipt.result = 'pass'
    $receipt.completed_at = [DateTimeOffset]::UtcNow.ToString('o')
    Save-Receipt
    Get-Content -LiteralPath $runPath -Tail 12
  } finally {
    Pop-Location
  }
} catch {
  $receipt.result = 'fail'
  $receipt.completed_at = [DateTimeOffset]::UtcNow.ToString('o')
  Save-Receipt
  throw
}
