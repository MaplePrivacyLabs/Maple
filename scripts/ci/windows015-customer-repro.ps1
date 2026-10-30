# Temporary diagnostic branch only. Executes the exact shipped 0.0.15 bytes;
# does not build, sign, import certificates, repair ACLs, or supply account credentials.
param([switch]$Child, [string]$Control, [ValidateSet('normal-install','normal-launch','uninstall')][string]$Phase)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 3.0

Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Security.Principal;
using System.Text;
public static class Maple015Token {
  [StructLayout(LayoutKind.Sequential)] struct SIDATTR { public IntPtr Sid; public uint Attributes; }
  [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] struct STARTUPINFO {
    public uint cb; public string reserved, desktop, title;
    public uint x,y,xsize,ysize,xchars,ychars,fill,flags;
    public short show,reserved2; public IntPtr reservedptr,input,output,error;
  }
  [StructLayout(LayoutKind.Sequential)] struct PROCESSINFO { public IntPtr process,thread; public uint pid,tid; }
  [DllImport("advapi32.dll",SetLastError=true)] static extern bool OpenProcessToken(IntPtr p,uint access,out IntPtr token);
  [DllImport("advapi32.dll",SetLastError=true)] static extern bool GetTokenInformation(IntPtr token,int kind,IntPtr data,int length,out int needed);
  [DllImport("advapi32.dll",SetLastError=true)] static extern bool SetTokenInformation(IntPtr token,int kind,IntPtr data,int length);
  [DllImport("advapi32.dll",SetLastError=true)] static extern bool CreateRestrictedToken(IntPtr token,uint flags,uint count,[In]SIDATTR[] disable,uint privCount,IntPtr privs,uint restrictCount,IntPtr restrict,out IntPtr result);
  [DllImport("advapi32.dll",SetLastError=true,CharSet=CharSet.Unicode)] static extern bool ConvertStringSidToSid(string sid,out IntPtr result);
  [DllImport("advapi32.dll",SetLastError=true,CharSet=CharSet.Unicode)] static extern bool CreateProcessAsUser(IntPtr token,string app,StringBuilder cmd,IntPtr pa,IntPtr ta,bool inherit,uint flags,IntPtr env,string cwd,ref STARTUPINFO startup,out PROCESSINFO process);
  [DllImport("advapi32.dll",SetLastError=true,CharSet=CharSet.Unicode)] static extern bool CreateProcessWithTokenW(IntPtr token,uint logon,string app,StringBuilder cmd,uint flags,IntPtr env,string cwd,ref STARTUPINFO startup,out PROCESSINFO process);
  [DllImport("userenv.dll",SetLastError=true)] static extern bool CreateEnvironmentBlock(out IntPtr environment,IntPtr token,bool inherit);
  [DllImport("userenv.dll")] static extern bool DestroyEnvironmentBlock(IntPtr environment);
  [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
  [DllImport("kernel32.dll")] static extern IntPtr LocalFree(IntPtr allocation);
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
  public static uint RunRestricted(string app,string command,string directory,int timeoutSeconds) {
    IntPtr original=IntPtr.Zero,restricted=IntPtr.Zero,admins=IntPtr.Zero,medium=IntPtr.Zero,environment=IntPtr.Zero,user=IntPtr.Zero,label=IntPtr.Zero,owner=IntPtr.Zero;
    PROCESSINFO process=new PROCESSINFO();
    try {
      if(!OpenProcessToken(new IntPtr(-1),0xF01FF,out original)) throw new Win32Exception(Marshal.GetLastWin32Error());
      if(!ConvertStringSidToSid("S-1-5-32-544",out admins)) throw new Win32Exception(Marshal.GetLastWin32Error());
      if(!CreateRestrictedToken(original,1,1,new SIDATTR[]{new SIDATTR{Sid=admins}},0,IntPtr.Zero,0,IntPtr.Zero,out restricted)) throw new Win32Exception(Marshal.GetLastWin32Error());
      // A filtered token's default owner must be its user, not the now deny-only Administrators group.
      user=Info(original,1); owner=Marshal.AllocHGlobal(IntPtr.Size); Marshal.WriteIntPtr(owner,Marshal.ReadIntPtr(user));
      if(!SetTokenInformation(restricted,4,owner,IntPtr.Size)) throw new Win32Exception(Marshal.GetLastWin32Error());
      if(!ConvertStringSidToSid("S-1-16-8192",out medium)) throw new Win32Exception(Marshal.GetLastWin32Error());
      int labelSize=Marshal.SizeOf(typeof(SIDATTR))+12; // S-1-16-8192 has a 12-byte SID.
      label=Marshal.AllocHGlobal(labelSize); Marshal.StructureToPtr(new SIDATTR{Sid=medium,Attributes=0x20},label,false);
      if(!SetTokenInformation(restricted,25,label,labelSize)) throw new Win32Exception(Marshal.GetLastWin32Error());
      if(!CreateEnvironmentBlock(out environment,restricted,false)) throw new Win32Exception(Marshal.GetLastWin32Error());
      var startup=new STARTUPINFO(); startup.cb=(uint)Marshal.SizeOf(typeof(STARTUPINFO));
      const uint flags=0x08000400; // CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT
      bool ok=CreateProcessAsUser(restricted,app,new StringBuilder(command),IntPtr.Zero,IntPtr.Zero,false,flags,environment,directory,ref startup,out process);
      if(!ok) {
        // No product process exists on a failed creation; this is a bounded API capability fallback.
        ok=CreateProcessWithTokenW(restricted,0,app,new StringBuilder(command),flags,environment,directory,ref startup,out process);
      }
      if(!ok) throw new Win32Exception(Marshal.GetLastWin32Error());
      if(WaitForSingleObject(process.process,(uint)timeoutSeconds*1000)!=0) { TerminateProcess(process.process,124); throw new TimeoutException("restricted child exceeded its bound"); }
      uint code; if(!GetExitCodeProcess(process.process,out code)) throw new Win32Exception(Marshal.GetLastWin32Error()); return code;
    } finally {
      if(process.thread!=IntPtr.Zero)CloseHandle(process.thread); if(process.process!=IntPtr.Zero)CloseHandle(process.process);
      if(environment!=IntPtr.Zero)DestroyEnvironmentBlock(environment); if(label!=IntPtr.Zero)Marshal.FreeHGlobal(label); if(owner!=IntPtr.Zero)Marshal.FreeHGlobal(owner); if(user!=IntPtr.Zero)Marshal.FreeHGlobal(user);
      if(medium!=IntPtr.Zero)LocalFree(medium); if(admins!=IntPtr.Zero)LocalFree(admins); if(restricted!=IntPtr.Zero)CloseHandle(restricted); if(original!=IntPtr.Zero)CloseHandle(original);
    }
  }
}
'@

function Token-Facts {
    [ordered]@{ elevated = ([Maple015Token]::TokenInt(20) -ne 0); elevation_type = [Maple015Token]::TokenInt(18); admin_enabled = [Maple015Token]::AdminEnabled(); integrity_rid = [Maple015Token]::Integrity(); token_owner_matches_user = ([Security.Principal.WindowsIdentity]::GetCurrent().Owner.Value -eq [Maple015Token]::UserSid()); user_matches_parent = ([Maple015Token]::UserSid() -eq (Get-Content -Raw -LiteralPath (Join-Path $Control 'parent-sid.txt')).Trim()) }
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
function Fetch-Pinned([string]$Name, [long]$Size, [string]$Sha) {
    $target = Join-Path $Control $Name
    Invoke-WebRequest -Uri ("https://updates.maple.ai/agent/stable/releases/0.0.15/" + $Name) -Headers @{'X-Maple-Download-Probe'='1'; 'User-Agent'='Maple-Agent-Updater/1'} -MaximumRedirection 0 -TimeoutSec 120 -OutFile $target
    if ((Get-Item -LiteralPath $target).Length -ne $Size -or (Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash.ToLowerInvariant() -ne $Sha) { throw 'Pinned release download size or hash differs' }
    return $target
}

if ($Child) {
    $result = [ordered]@{ phase=$Phase; scope='Original 0.0.15 bytes; default local known folders; restricted same-SID token, not a Windows 10 UAC linked token'; token=$null; environment_limit=$null }
    $operation = 'child-token-validation'
    try {
        $result.token = Token-Facts
        if ($result.token.elevated -or $result.token.admin_enabled -or $result.token.integrity_rid -gt 8192 -or -not $result.token.user_matches_parent -or -not $result.token.token_owner_matches_user) { throw 'Requested ordinary token conditions were not established' }
        foreach ($name in @('XDG_DATA_HOME','XDG_CONFIG_HOME','XDG_CACHE_HOME')) { [Environment]::SetEnvironmentVariable($name,$null) }
        $local = [Environment]::GetFolderPath([Environment+SpecialFolder]::LocalApplicationData)
        if ([string]::IsNullOrWhiteSpace($local)) { throw 'LocalApplicationData known folder is unavailable' }
        $result.local_known_folder_matches_parent = ($local -eq (Get-Content -Raw -LiteralPath (Join-Path $Control 'parent-local.txt')).Trim())
        if (-not $result.local_known_folder_matches_parent) { throw 'Default local known folder differs across the same-user token change' }
        $installed = Join-Path $local 'Programs/Maple Work'
        $store = Join-Path $local 'maple-agent-prod/store-v1'
        $installer = Join-Path $Control 'maple-agent-prod-0.0.15-a40ff7b-windows-x86_64-setup.exe'
        if ($Phase -eq 'normal-install') {
            $operation = 'ordinary-checker-and-install'
            $temporary = Join-Path ([IO.Path]::GetTempPath()) ('Maple015Checker-' + [Guid]::NewGuid().ToString('N'))
            [void][IO.Directory]::CreateDirectory($temporary)
            $checker = Join-Path $temporary 'maple-installer-check.exe'
            Copy-Item -LiteralPath (Join-Path $Control 'payload/maple-agent-windows/maple-agent.exe') -Destination $checker
            $result.checker_acl = Acl-Facts $checker; $result.temp_acl = Acl-Facts $temporary
            $result.install_parent_acl = Acl-Facts (Split-Path -Parent $installed)
            $result.preflight = Invoke-Product $checker ('--internal-windows-installer-preflight "' + $installed + '"')
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
$Control = Join-Path $env:LOCALAPPDATA ('Maple015ReproControl-' + [Guid]::NewGuid().ToString('N'))
$result = [ordered]@{ schema='maple-windows015-customer-reproduction-v1'; platform=[Environment]::OSVersion.VersionString; product_source='a40ff7b66517b4c76c18c6ae8be97a335b8aff33'; product_version='0.0.15'; parent_token=$null; normal_install=$null; elevated_install=$null; normal_launch=$null; environment_limit=$null; scope='Disposable Windows Actions host. First ordinary ACP launch follows fresh elevated silent install; no prior elevated app/host. No GUI/UAC desktop or Windows10 qualification; no signing, publishing, certificate import or product ACL repair.' }
$defaultInstall = Join-Path $env:LOCALAPPDATA 'Programs/Maple Work'
$defaultData = Join-Path $env:LOCALAPPDATA 'maple-agent-prod'
$operation = 'disposable-profile-admission'
try {
    if (Test-Path -LiteralPath $defaultInstall) { throw 'Disposable runner already has the target installation; no test performed' }
    if (Test-Path -LiteralPath $defaultData) { throw 'Disposable runner already has the target data namespace; no test performed' }
    [void][IO.Directory]::CreateDirectory($Control)
    # Only this newly created probe-control directory gets an explicit fixture ACL.
    $security = [Security.AccessControl.DirectorySecurity]::new()
    $user = [Security.Principal.WindowsIdentity]::GetCurrent().User
    $security.SetOwner($user); $security.SetAccessRuleProtection($true,$false)
    foreach ($sid in @($user, [Security.Principal.SecurityIdentifier]::new('S-1-5-18'), [Security.Principal.SecurityIdentifier]::new('S-1-5-32-544'))) {
        $security.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($sid,'FullControl','ContainerInherit,ObjectInherit','None','Allow'))
    }
    Set-Acl -LiteralPath $Control -AclObject $security
    [Maple015Token]::UserSid() | Set-Content -LiteralPath (Join-Path $Control 'parent-sid.txt') -Encoding ascii
    $env:LOCALAPPDATA | Set-Content -LiteralPath (Join-Path $Control 'parent-local.txt') -Encoding utf8
    $result.parent_token = Token-Facts
    Copy-Item -LiteralPath $PSCommandPath -Destination (Join-Path $Control 'probe.ps1')
    $operation = 'fixed-release-download-and-signature'
    $installer = Fetch-Pinned 'maple-agent-prod-0.0.15-a40ff7b-windows-x86_64-setup.exe' 22430808 'e2b561394118ee7bca4746f6dc9bd1f5fcf972ca51ac3ab5aa729576d2cdb747'
    Verify-Signed $installer
    $archive = Fetch-Pinned 'maple-agent-prod-0.0.15-a40ff7b-windows-x86_64.tar.gz' 32145528 '62521021a02b0d746bf26d6c2954519f28a95509927cae1ef87bbf2ad1b0b056'
    $payload = Join-Path $Control 'payload'; [void][IO.Directory]::CreateDirectory($payload)
    & "$env:SystemRoot\System32\tar.exe" -xf $archive -C $payload
    if ($LASTEXITCODE -ne 0) { throw 'Pinned release archive extraction failed' }
    Verify-Signed (Join-Path $payload 'maple-agent-windows/maple-agent.exe')
    $shell = (Get-Process -Id $PID).Path
    function Run-Ordinary([string]$ChildPhase) {
        $command = '"' + $shell + '" -NoLogo -NoProfile -ExecutionPolicy Bypass -File "' + (Join-Path $Control 'probe.ps1') + '" -Child -Control "' + $Control + '" -Phase ' + $ChildPhase
        $code = [Maple015Token]::RunRestricted($shell,$command,$Control,240)
        $receipt = Join-Path $Control ($ChildPhase + '.json')
        if (-not (Test-Path -LiteralPath $receipt)) { throw ('Restricted child did not write its receipt; exit=' + $code) }
        $value = Get-Content -Raw -LiteralPath $receipt | ConvertFrom-Json
        $value | Add-Member -NotePropertyName child_exit_code -NotePropertyValue $code
        return $value
    }
    $operation = 'ordinary-token-and-default-install'
    $result.normal_install = Run-Ordinary 'normal-install'
    if ($result.normal_install.child_exit_code -ne 0) { throw 'Ordinary child could not establish the requested environment' }
    if ($result.normal_install.install.exit_code -eq 0) {
        $operation = 'ordinary-uninstall-before-fresh-elevated-case'
        $cleanup = Run-Ordinary 'uninstall'
        $result.normal_uninstall = $cleanup
        if ($cleanup.child_exit_code -ne 0 -or $cleanup.uninstall.exit_code -ne 0) { throw 'First case did not uninstall cleanly; second case is not fresh' }
        # The product deliberately retains its empty root. A synchronous Windows
        # uninstaller may also retain its own still-mapped PE. Remove only that
        # verified test-owned PE after exit, then an actually empty test-owned root.
        if (Test-Path -LiteralPath $defaultInstall) {
            $remaining = @(Get-ChildItem -LiteralPath $defaultInstall -Force)
            if (@($remaining | Where-Object { $_.Name -ne 'uninstall.exe' -or $_.PSIsContainer }).Count -ne 0) { throw 'Uninstall left owned or unknown files; no fresh elevated case' }
            if ($remaining.Count -eq 1) { Verify-Signed $remaining[0].FullName; Remove-Item -LiteralPath $remaining[0].FullName }
            Remove-Item -LiteralPath $defaultInstall
        }
    } elseif (Test-Path -LiteralPath $defaultInstall) { throw 'First case left an incomplete install; stop rather than disguise it as a fresh elevated case' }
    $operation = 'fresh-elevated-default-install'
    $result.elevated_install = Invoke-Product $installer '/S' 150
    if ($result.elevated_install.exit_code -eq 0) {
        $bootstrap = Join-Path $defaultInstall 'maple-agent.exe'; Verify-Signed $bootstrap
        # The user's reported ordinary launch is first; no elevated host may
        # pre-create the store or accidentally service this normal-token ACP.
        $operation = 'ordinary-acp-first-launch-after-elevated-install'
        $result.normal_launch = Run-Ordinary 'normal-launch'
        if ($result.normal_launch.child_exit_code -ne 0) { throw 'Ordinary first-launch child did not complete its requested environment' }
    }
} catch { $result.environment_limit = Error-Facts $_.Exception $operation }
finally {
    # Only processes whose exact executable lives in this initially absent,
    # probe-created installation and started during the probe are owned here.
    foreach ($process in @(Get-Process -Name 'maple-agent' -ErrorAction SilentlyContinue)) {
        try { if ($process.StartTime -ge $started -and $process.Path.StartsWith($defaultInstall + '\',[StringComparison]::OrdinalIgnoreCase)) { Stop-Process -Id $process.Id -Force } } catch {}
    }
    $result.elapsed_seconds = [math]::Round(((Get-Date)-$started).TotalSeconds,1)
    $result | ConvertTo-Json -Depth 10 | Write-Output
}
if ($null -ne $result.environment_limit) { exit 3 }
exit 0
