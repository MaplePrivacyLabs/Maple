# Temporary diagnostic branch only. Executes the exact shipped 0.0.15 bytes;
# does not build, sign, import certificates, repair ACLs, or supply Maple account credentials.
param([switch]$Child, [string]$Control, [ValidateSet('normal-install','elevated-install','normal-launch','uninstall')][string]$Phase)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 3.0

# The credential child's environment intentionally has no caller profile or
# temp variables. Resolve its loaded Windows profile before Add-Type/Roslyn
# needs a writable default temp directory. This creates no product state.
if ($Child) {
    try {
        $expected=(Get-Content -Raw -LiteralPath (Join-Path $Control 'expected-sid.txt')).Trim()
        if ([Security.Principal.WindowsIdentity]::GetCurrent().User.Value -ne $expected) { throw 'Credential child identity differs' }
        $env:LOCALAPPDATA=[Environment]::GetFolderPath([Environment+SpecialFolder]::LocalApplicationData)
        $env:APPDATA=[Environment]::GetFolderPath([Environment+SpecialFolder]::ApplicationData)
        $env:USERPROFILE=[Environment]::GetFolderPath([Environment+SpecialFolder]::UserProfile)
        if ([string]::IsNullOrWhiteSpace($env:LOCALAPPDATA) -or [string]::IsNullOrWhiteSpace($env:USERPROFILE)) { throw 'Credential profile known folders are unavailable' }
        $env:TEMP=Join-Path $env:LOCALAPPDATA 'Temp'; $env:TMP=$env:TEMP
        [void][IO.Directory]::CreateDirectory($env:TEMP)
    } catch {
        @{ phase=$Phase; token=$null; environment_limit=@{operation='credential-profile-bootstrap';error_type=$_.Exception.GetType().Name;win32_code=$null} } | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $Control ($Phase + '.json')) -Encoding utf8
        exit 3
    }
}

Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Security.Principal;
using System.Text;
public static class Maple015Token {
  [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] struct STARTUPINFO {
    public uint cb; public string reserved, desktop, title;
    public uint x,y,xsize,ysize,xchars,ychars,fill,flags;
    public short show,reserved2; public IntPtr reservedptr,input,output,error;
  }
  [StructLayout(LayoutKind.Sequential)] struct PROCESSINFO { public IntPtr process,thread; public uint pid,tid; }
  [DllImport("advapi32.dll",SetLastError=true)] static extern bool OpenProcessToken(IntPtr p,uint access,out IntPtr token);
  [DllImport("advapi32.dll",SetLastError=true)] static extern bool GetTokenInformation(IntPtr token,int kind,IntPtr data,int length,out int needed);
  [DllImport("advapi32.dll",SetLastError=true,CharSet=CharSet.Unicode)] static extern bool CreateProcessWithLogonW(string user,string domain,string password,uint logonFlags,string app,StringBuilder cmd,uint flags,IntPtr env,string cwd,ref STARTUPINFO startup,out PROCESSINFO process);
  [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
  [DllImport("kernel32.dll",SetLastError=true)] static extern uint WaitForSingleObject(IntPtr handle,uint milliseconds);
  [DllImport("kernel32.dll",SetLastError=true)] static extern bool GetExitCodeProcess(IntPtr process,out uint code);
  [DllImport("kernel32.dll",SetLastError=true)] static extern bool TerminateProcess(IntPtr process,uint code);
  static IntPtr Info(IntPtr token,int kind) {
    int size; GetTokenInformation(token,kind,IntPtr.Zero,0,out size);
    if(size<=0) throw new Win32Exception(Marshal.GetLastWin32Error());
    IntPtr result=Marshal.AllocHGlobal(size);
    if(!GetTokenInformation(token,kind,result,size,out size)) { Marshal.FreeHGlobal(result); throw new Win32Exception(Marshal.GetLastWin32Error()); }
    return result;
  }
  public static string UserSid() { return WindowsIdentity.GetCurrent().User.Value; }
  public static bool AdminEnabled() { return new WindowsPrincipal(WindowsIdentity.GetCurrent()).IsInRole(WindowsBuiltInRole.Administrator); }
  public static int TokenInt(int kind) {
    IntPtr token; if(!OpenProcessToken(new IntPtr(-1),8,out token)) throw new Win32Exception(Marshal.GetLastWin32Error());
    try { IntPtr data=Info(token,kind); try { return Marshal.ReadInt32(data); } finally { Marshal.FreeHGlobal(data); } } finally { CloseHandle(token); }
  }
  public static int Integrity() {
    IntPtr token; if(!OpenProcessToken(new IntPtr(-1),8,out token)) throw new Win32Exception(Marshal.GetLastWin32Error());
    try { IntPtr data=Info(token,25); try { var sid=new SecurityIdentifier(Marshal.ReadIntPtr(data)); var parts=sid.Value.Split('-'); return Int32.Parse(parts[parts.Length-1]); } finally { Marshal.FreeHGlobal(data); } } finally { CloseHandle(token); }
  }
  public static uint RunAsUser(string user,string password,string app,string command,string directory,string cleanEnvironment,int timeoutSeconds) {
    PROCESSINFO process=new PROCESSINFO();
    IntPtr environment=Marshal.StringToHGlobalUni(cleanEnvironment);
    try {
      var startup=new STARTUPINFO(); startup.cb=(uint)Marshal.SizeOf(typeof(STARTUPINFO));
      // Password is an in-memory argument to the Windows API only: no child
      // environment, command line, output or saved credential contains it.
      if(!CreateProcessWithLogonW(user,".",password,1,app,new StringBuilder(command),0x08000400,environment,directory,ref startup,out process)) throw new Win32Exception(Marshal.GetLastWin32Error());
      if(WaitForSingleObject(process.process,(uint)timeoutSeconds*1000)!=0) { TerminateProcess(process.process,124); throw new TimeoutException("credential child exceeded its bound"); }
      uint code; if(!GetExitCodeProcess(process.process,out code)) throw new Win32Exception(Marshal.GetLastWin32Error()); return code;
    } finally { if(process.thread!=IntPtr.Zero)CloseHandle(process.thread); if(process.process!=IntPtr.Zero)CloseHandle(process.process); Marshal.FreeHGlobal(environment); }
  }
}
'@

function Token-Facts {
    [ordered]@{ elevated = ([Maple015Token]::TokenInt(20) -ne 0); elevation_type = [Maple015Token]::TokenInt(18); admin_enabled = [Maple015Token]::AdminEnabled(); integrity_rid = [Maple015Token]::Integrity(); token_owner_matches_user = ([Security.Principal.WindowsIdentity]::GetCurrent().Owner.Value -eq [Maple015Token]::UserSid()); user_matches_expected = ([Maple015Token]::UserSid() -eq (Get-Content -Raw -LiteralPath (Join-Path $Control 'expected-sid.txt')).Trim()) }
}
function Acl-Facts([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return @{ exists = $false } }
    try {
        $acl = Get-Acl -LiteralPath $Path
        $sid = $acl.GetOwner([System.Security.Principal.SecurityIdentifier]).Value
        [ordered]@{ exists = $true; owner_matches_current = ($sid -eq [Maple015Token]::UserSid()); owner_is_admins = ($sid -eq 'S-1-5-32-544'); owner_is_system = ($sid -eq 'S-1-5-18'); dacl_protected = $acl.AreAccessRulesProtected; ace_count = @($acl.Access).Count }
    } catch { @{ exists = $true; acl_unavailable = $true; error_type = $_.Exception.GetType().Name } }
}
function Error-Facts([System.Exception]$Failure, [string]$Operation) {
    $native = $null; $inner = $Failure
    for ($i=0; $i -lt 5 -and $null -ne $inner; $i++) {
        if ($inner -is [ComponentModel.Win32Exception]) { $native = $inner.NativeErrorCode; break }
        $inner = $inner.InnerException
    }
    [ordered]@{ operation=$Operation; error_type=$Failure.GetType().Name; win32_code=$native }
}
function Invoke-Product([string]$Exe, [string]$Arguments, [int]$Seconds = 60) {
    $info = [System.Diagnostics.ProcessStartInfo]::new($Exe, $Arguments)
    $info.UseShellExecute = $false; $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true; $info.RedirectStandardError = $true; $info.RedirectStandardInput = $true
    $info.Environment.Clear()
    foreach ($name in @('SYSTEMROOT','WINDIR','COMSPEC','USERPROFILE','APPDATA','LOCALAPPDATA','TEMP','TMP','USERNAME','USERDOMAIN','PROGRAMDATA','PROGRAMFILES','PROGRAMFILES(X86)','COMMONPROGRAMFILES','COMMONPROGRAMFILES(X86)','NUMBER_OF_PROCESSORS','PROCESSOR_ARCHITECTURE')) {
        $value = [Environment]::GetEnvironmentVariable($name)
        if ($null -ne $value) { $info.Environment[$name] = $value }
    }
    $info.Environment['PATH'] = "$env:SystemRoot\System32;$env:SystemRoot"
    $info.Environment['MAPLE_DISABLE_UPDATE_CHECK'] = '1'
    $process = [System.Diagnostics.Process]::new(); $process.StartInfo = $info
    [void]$process.Start(); $process.StandardInput.Close()
    $stdout = $process.StandardOutput.ReadToEndAsync(); $stderr = $process.StandardError.ReadToEndAsync()
    $timedOut = -not $process.WaitForExit($Seconds * 1000)
    if ($timedOut) { $process.Kill($true); $process.WaitForExit() }
    $captureComplete = $stdout.Wait(5000) -and $stderr.Wait(5000)
    $text = if ($captureComplete) { $stdout.GetAwaiter().GetResult() + "`n" + $stderr.GetAwaiter().GetResult() } else { '' }
    $code = $process.ExitCode; $process.Dispose()
    $guard = @([regex]::Matches($text, 'unsafe Windows updater storage \(([a-z-]+)\)') | ForEach-Object { $_.Groups[1].Value } | Select-Object -Unique)
    [ordered]@{ exit_code = $code; exit_hex = ('0x{0:X8}' -f [BitConverter]::ToUInt32([BitConverter]::GetBytes([int]$code),0)); timed_out = $timedOut; capture_complete=$captureComplete; guard_reasons = $guard; needs_setup = $text.Contains('Maple needs local store setup. Open Maple to continue.'); needs_sign_in = $text.Contains('Maple ACP requires sign-in.'); unsafe_store_path = ($text.Contains('not private and safe') -or $text.Contains('not in a private, safe folder')); host_did_not_start = $text.Contains('Maple host did not start'); captured_bytes = [Text.Encoding]::UTF8.GetByteCount($text) }
}
function Verify-Signed([string]$Path) {
    $s = Get-AuthenticodeSignature -LiteralPath $Path
    if ($s.Status -ne 'Valid' -or $null -eq $s.SignerCertificate -or $null -eq $s.TimeStamperCertificate -or $s.SignerCertificate.Subject -notmatch '(^|,\s*)CN=Mutiny Wallet Inc\. dba OpenSecret(\s*,|$)') { throw 'Pinned release Authenticode verification failed' }
}
function Verify-ReleaseInstaller([string]$Path) {
    if ((Get-Item -LiteralPath $Path).Length -ne 22430808 -or (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() -ne 'e2b561394118ee7bca4746f6dc9bd1f5fcf972ca51ac3ab5aa729576d2cdb747') { throw 'Original installer bytes changed across privilege contexts' }
    Verify-Signed $Path
}
function Fetch-Pinned([string]$Name, [long]$Size, [string]$Sha) {
    $target = Join-Path $Control $Name
    Invoke-WebRequest -Uri ("https://updates.maple.ai/agent/stable/releases/0.0.15/" + $Name) -Headers @{'X-Maple-Download-Probe'='1'; 'User-Agent'='Maple-Agent-Updater/1'} -MaximumRedirection 0 -TimeoutSec 120 -OutFile $target
    if ((Get-Item -LiteralPath $target).Length -ne $Size -or (Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash.ToLowerInvariant() -ne $Sha) { throw 'Pinned release download size or hash differs' }
    return $target
}

if ($Child) {
    $result = [ordered]@{ phase=$Phase; scope='Original 0.0.15 bytes; default known folders; fresh credential logon of one disposable local user; group membership changes, not a Windows10 UAC linked token'; token=$null; environment_limit=$null }
    $operation = 'child-token-validation'
    try {
        $result.token = Token-Facts
        $expectAdmin = ($Phase -eq 'elevated-install')
        if (-not $result.token.user_matches_expected) { throw 'Credential child is not the exact disposable user' }
        if ($expectAdmin) {
            if (-not $result.token.elevated -or -not $result.token.admin_enabled) { throw 'Elevated-install child is not actually elevated' }
        } elseif ($result.token.elevated -or $result.token.admin_enabled -or $result.token.integrity_rid -gt 8192 -or -not $result.token.token_owner_matches_user) { throw 'Requested ordinary token conditions were not established' }
        foreach ($name in @('XDG_DATA_HOME','XDG_CONFIG_HOME','XDG_CACHE_HOME')) { [Environment]::SetEnvironmentVariable($name,$null) }
        $local = [Environment]::GetFolderPath([Environment+SpecialFolder]::LocalApplicationData)
        $profile = [Environment]::GetFolderPath([Environment+SpecialFolder]::UserProfile)
        $roaming = [Environment]::GetFolderPath([Environment+SpecialFolder]::ApplicationData)
        if ([string]::IsNullOrWhiteSpace($local) -or [string]::IsNullOrWhiteSpace($profile) -or [string]::IsNullOrWhiteSpace($roaming)) { throw 'Credential logon did not load the disposable profile known folders' }
        # Reconstruct the real account's standard environment from its loaded
        # Windows known folders, never a scratch/XDG namespace or parent's profile.
        $env:USERPROFILE=$profile; $env:APPDATA=$roaming; $env:LOCALAPPDATA=$local
        $identity=[Security.Principal.WindowsIdentity]::GetCurrent().Name.Split('\',2)
        if ($identity.Length -eq 2) { $env:USERDOMAIN=$identity[0]; $env:USERNAME=$identity[1] }
        $env:TEMP=Join-Path $local 'Temp'; $env:TMP=$env:TEMP
        [void][IO.Directory]::CreateDirectory($env:TEMP)
        $known = Join-Path $Control 'child-local.txt'
        if (Test-Path -LiteralPath $known) {
            $result.local_known_folder_consistent = ($local -eq (Get-Content -Raw -LiteralPath $known).Trim())
            if (-not $result.local_known_folder_consistent) { throw 'Known folder changed across the same disposable account logons' }
        } else { $local | Set-Content -LiteralPath $known -Encoding utf8; $result.local_known_folder_consistent=$true }
        $installed = Join-Path $local 'Programs/Maple Work'
        $store = Join-Path $local 'maple-agent-prod/store-v1'
        $installer = Join-Path $Control 'maple-agent-prod-0.0.15-a40ff7b-windows-x86_64-setup.exe'
        if ($Phase -eq 'normal-install') {
            $operation = 'ordinary-checker-and-install'
            if (Test-Path -LiteralPath $installed) { throw 'Fresh disposable user already has an installation' }
            if (Test-Path -LiteralPath (Join-Path $local 'maple-agent-prod')) { throw 'Fresh disposable user already has app data' }
            $temporary = Join-Path ([IO.Path]::GetTempPath()) ('Maple015Checker-' + [Guid]::NewGuid().ToString('N'))
            [void][IO.Directory]::CreateDirectory($temporary)
            $checker = Join-Path $temporary 'maple-installer-check.exe'
            [IO.File]::WriteAllBytes($checker,[IO.File]::ReadAllBytes((Join-Path $Control 'payload/maple-agent-windows/maple-agent.exe')))
            $result.checker_acl = Acl-Facts $checker; $result.temp_acl = Acl-Facts $temporary
            $result.install_parent_acl = Acl-Facts (Split-Path -Parent $installed)
            $result.preflight = Invoke-Product $checker ('--internal-windows-installer-preflight "' + $installed + '"')
            Verify-ReleaseInstaller $installer
            $result.install = Invoke-Product $installer '/S' 150
            $result.installed_acl = Acl-Facts $installed
        } elseif ($Phase -eq 'elevated-install') {
            $operation = 'elevated-default-install'
            if (Test-Path -LiteralPath $installed) { throw 'Fresh elevated case already has an installation' }
            if (Test-Path -LiteralPath (Join-Path $local 'maple-agent-prod')) { throw 'Fresh elevated case already has app data' }
            Verify-ReleaseInstaller $installer
            $result.install = Invoke-Product $installer '/S' 150
            $result.installed_acl = Acl-Facts $installed
        } elseif ($Phase -eq 'uninstall') {
            $operation = 'ordinary-uninstall'
            $uninstaller = Join-Path $installed 'uninstall.exe'
            Verify-Signed $uninstaller
            # _?= keeps the uninstaller synchronous instead of spawning its temp copy.
            $result.uninstall = Invoke-Product $uninstaller ('/S _?=' + $installed) 60
        } else {
            $operation = 'ordinary-acp-after-elevated-install'
            $bootstrap = Join-Path $installed 'maple-agent.exe'
            Verify-Signed $bootstrap
            $result.acp = Invoke-Product $bootstrap 'acp' 45
            $result.store_acl = Acl-Facts $store
            $result.db_acl = Acl-Facts (Join-Path $store 'db')
            $result.log_acl = Acl-Facts (Join-Path $store 'logs/maple-agent.log')
            $result.default_store_path_reached = Test-Path -LiteralPath $store
            $result.signed_out_or_setup_guard_reached = ($result.acp.exit_code -eq 1 -and ($result.acp.needs_setup -or $result.acp.needs_sign_in))
            $result.customer_unsafe_path_text_observed = $result.acp.unsafe_store_path
            $result.stop = Invoke-Product $bootstrap 'host stop' 20
        }
    } catch { $result.environment_limit = Error-Facts $_.Exception $operation }
    $result | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $Control ($Phase + '.json')) -Encoding utf8
    if ($null -ne $result.environment_limit) { exit 3 }; exit 0
}

$started = Get-Date
$Control = Join-Path $env:SystemRoot ('Temp/Maple015ReproControl-' + [Guid]::NewGuid().ToString('N'))
$result = [ordered]@{ schema='maple-windows015-customer-reproduction-v2'; platform=[Environment]::OSVersion.VersionString; product_source='a40ff7b66517b4c76c18c6ae8be97a335b8aff33'; product_version='0.0.15'; parent_token=$null; normal_install=$null; elevated_install=$null; normal_launch=$null; environment_limit=$null; account_removed=$false; scope='Disposable local user with real nonadmin token, temporary Admin group logon for silent install, membership removed before fresh ordinary ACP. No elevated app/host first. Windows2025, not Windows10/UAC/GUI qualification; no signing, publishing, certificate import or product ACL repair.' }
$defaultInstall=$null; $childLocal=$null; $probeUser=$null; $addedAdmin=$false; $password=$null
$operation = 'disposable-account-creation'
try {
    $name='MapleProbe' + [Guid]::NewGuid().ToString('N').Substring(0,10)
    $password='Aa1!' + [Convert]::ToBase64String([Security.Cryptography.RandomNumberGenerator]::GetBytes(32))
    $secure=ConvertTo-SecureString -String $password -AsPlainText -Force
    $probeUser=New-LocalUser -Name $name -Password $secure -Description 'Disposable Maple installer reproduction' -AccountNeverExpires
    $usersGroup=Get-LocalGroup -SID 'S-1-5-32-545'; $adminGroup=Get-LocalGroup -SID 'S-1-5-32-544'
    Add-LocalGroupMember -Group $usersGroup -Member $probeUser
    [void][IO.Directory]::CreateDirectory($Control)
    # Only the newly created probe-control directory gets an explicit fixture ACL.
    $security = [Security.AccessControl.DirectorySecurity]::new()
    $runnerSid = [Security.Principal.WindowsIdentity]::GetCurrent().User
    $security.SetOwner($runnerSid); $security.SetAccessRuleProtection($true,$false)
    foreach ($sid in @($runnerSid,$probeUser.SID,[Security.Principal.SecurityIdentifier]::new('S-1-5-18'),[Security.Principal.SecurityIdentifier]::new('S-1-5-32-544'))) {
        $security.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($sid,'FullControl','ContainerInherit,ObjectInherit','None','Allow'))
    }
    Set-Acl -LiteralPath $Control -AclObject $security
    $probeUser.SID.Value | Set-Content -LiteralPath (Join-Path $Control 'expected-sid.txt') -Encoding ascii
    $result.parent_token=Token-Facts
    Copy-Item -LiteralPath $PSCommandPath -Destination (Join-Path $Control 'probe.ps1')
    $operation='fixed-release-download-and-signature'
    $installer=Fetch-Pinned 'maple-agent-prod-0.0.15-a40ff7b-windows-x86_64-setup.exe' 22430808 'e2b561394118ee7bca4746f6dc9bd1f5fcf972ca51ac3ab5aa729576d2cdb747'
    Verify-ReleaseInstaller $installer
    $archive=Fetch-Pinned 'maple-agent-prod-0.0.15-a40ff7b-windows-x86_64.tar.gz' 32145528 '62521021a02b0d746bf26d6c2954519f28a95509927cae1ef87bbf2ad1b0b056'
    $payload=Join-Path $Control 'payload'; [void][IO.Directory]::CreateDirectory($payload)
    & "$env:SystemRoot\System32\tar.exe" -xf $archive -C $payload
    if ($LASTEXITCODE -ne 0) { throw 'Pinned release archive extraction failed' }
    Verify-Signed (Join-Path $payload 'maple-agent-windows/maple-agent.exe')
    $shell=(Get-Process -Id $PID).Path
    function Run-Account([string]$ChildPhase) {
        $command='"' + $shell + '" -NoLogo -NoProfile -ExecutionPolicy Bypass -File "' + (Join-Path $Control 'probe.ps1') + '" -Child -Control "' + $Control + '" -Phase ' + $ChildPhase
        # No Actions/runtime credentials or parent account/profile variables
        # cross into the new account. Its loaded known folders are resolved there.
        $environment=[Collections.Generic.SortedDictionary[string,string]]::new([StringComparer]::OrdinalIgnoreCase)
        foreach ($key in @('SYSTEMROOT','WINDIR','COMSPEC','SYSTEMDRIVE','COMPUTERNAME','OS','PATHEXT','PROGRAMDATA','PROGRAMFILES','PROGRAMFILES(X86)','COMMONPROGRAMFILES','COMMONPROGRAMFILES(X86)','NUMBER_OF_PROCESSORS','PROCESSOR_ARCHITECTURE')) {
            $value=[Environment]::GetEnvironmentVariable($key)
            if ($null -ne $value) { $environment[$key]=$value }
        }
        $environment['PATH']="$env:SystemRoot\System32;$env:SystemRoot"
        $pairs=@($environment.GetEnumerator() | ForEach-Object { $_.Key + '=' + $_.Value })
        $block=($pairs -join [char]0) + [char]0 + [char]0
        $code=[Maple015Token]::RunAsUser($probeUser.Name,$password,$shell,$command,$Control,$block,240)
        $receipt=Join-Path $Control ($ChildPhase + '.json')
        if (-not (Test-Path -LiteralPath $receipt)) {
            return [pscustomobject]@{
                phase=$ChildPhase; child_exit_code=$code; token=$null
                environment_limit=@{operation='credential-child-without-receipt';error_type='MissingReceipt';process_exit_code=$code}
            }
        }
        $value=Get-Content -Raw -LiteralPath $receipt | ConvertFrom-Json
        $value | Add-Member -NotePropertyName child_exit_code -NotePropertyValue $code
        return $value
    }
    $operation='ordinary-account-default-install'
    $result.normal_install=Run-Account 'normal-install'
    if ($result.normal_install.child_exit_code -ne 0 -or $null -ne $result.normal_install.environment_limit) { throw 'Ordinary account child did not establish the requested environment' }
    $childLocal=(Get-Content -Raw -LiteralPath (Join-Path $Control 'child-local.txt')).Trim()
    $defaultInstall=Join-Path $childLocal 'Programs/Maple Work'
    if ($result.normal_install.install.exit_code -eq 0) {
        $operation='ordinary-uninstall-before-fresh-elevated-case'
        $cleanup=Run-Account 'uninstall'; $result.normal_uninstall=$cleanup
        if ($cleanup.child_exit_code -ne 0 -or $null -ne $cleanup.environment_limit -or $cleanup.uninstall.exit_code -ne 0) { throw 'Ordinary installation did not uninstall cleanly' }
        if (Test-Path -LiteralPath $defaultInstall) {
            $remaining=@(Get-ChildItem -LiteralPath $defaultInstall -Force)
            if (@($remaining | Where-Object { $_.Name -ne 'uninstall.exe' -or $_.PSIsContainer }).Count -ne 0) { throw 'Uninstall left owned or unknown files; no fresh elevated case' }
            if ($remaining.Count -eq 1) { Verify-Signed $remaining[0].FullName; Remove-Item -LiteralPath $remaining[0].FullName }
            Remove-Item -LiteralPath $defaultInstall
        }
    } elseif (Test-Path -LiteralPath $defaultInstall) { throw 'Ordinary failure left an incomplete install; stop rather than hide it' }
    $operation='temporary-disposable-admin-membership'
    Add-LocalGroupMember -Group $adminGroup -Member $probeUser; $addedAdmin=$true
    $operation='fresh-elevated-account-install'
    $result.elevated_install=Run-Account 'elevated-install'
    Remove-LocalGroupMember -Group $adminGroup -Member $probeUser; $addedAdmin=$false
    if ($result.elevated_install.child_exit_code -ne 0 -or $null -ne $result.elevated_install.environment_limit) { throw 'Elevated child did not establish the requested environment' }
    if ($result.elevated_install.install.exit_code -eq 0) {
        $operation='ordinary-account-first-launch-after-elevated-install'
        $result.normal_launch=Run-Account 'normal-launch'
        if ($result.normal_launch.child_exit_code -ne 0 -or $null -ne $result.normal_launch.environment_limit) { throw 'Ordinary first-launch child did not complete its environment' }
    }
} catch { $result.environment_limit=Error-Facts $_.Exception $operation }
finally {
    # Full paths + start times, never a blanket process-name kill. This account,
    # its profile and these exact program paths did not exist before this probe.
    foreach ($process in @(Get-Process -ErrorAction SilentlyContinue)) {
        try {
            $ownedApp=($null -ne $defaultInstall -and $process.Path.StartsWith($defaultInstall + '\',[StringComparison]::OrdinalIgnoreCase))
            $ownedSetup=($process.Path -eq (Join-Path $Control 'maple-agent-prod-0.0.15-a40ff7b-windows-x86_64-setup.exe'))
            $ownedTemp=($null -ne $childLocal -and $process.Path.StartsWith((Join-Path $childLocal 'Temp') + '\',[StringComparison]::OrdinalIgnoreCase) -and $process.ProcessName -eq 'maple-installer-check')
            if ($process.StartTime -ge $started -and ($ownedApp -or $ownedSetup -or $ownedTemp)) { Stop-Process -Id $process.Id -Force }
        } catch {}
    }
    if ($null -ne $probeUser) {
        try { if ($addedAdmin) { Remove-LocalGroupMember -Group $adminGroup -Member $probeUser }; Remove-LocalUser -SID $probeUser.SID; $result.account_removed=$true } catch { $result.account_cleanup_error=Error-Facts $_.Exception 'disposable-account-cleanup' }
    }
    $password=$null
    $result.elapsed_seconds=[math]::Round(((Get-Date)-$started).TotalSeconds,1)
    $result | ConvertTo-Json -Depth 10 | Write-Output
}
if ($null -ne $result.environment_limit) { exit 3 }
exit 0
