#Requires -Version 7.0
# Scratch probe (agent-bridle #408/#410): why does confined `git -C <repo> status`
# die with "Unable to read current working directory: Permission denied"?
# Each trial uses a fresh AppContainer name (fresh SID), grants that SID only the
# chosen subset of DOS-path namespace ACEs, then launches with --nul-device-ace.
param([Parameter(Mandatory)][string]$LauncherPath)
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Security.AccessControl;
using System.Security.Principal;
using System.Text;
using Microsoft.Win32.SafeHandles;
public static class P408 {
    [DllImport("userenv.dll", CharSet = CharSet.Unicode)]
    static extern int DeriveAppContainerSidFromAppContainerName(string n, out IntPtr sid);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern uint GetLongPathNameW(string s, StringBuilder l, uint n);
    [StructLayout(LayoutKind.Sequential)] struct US { public ushort L, M; public IntPtr B; }
    [StructLayout(LayoutKind.Sequential)] struct OA { public uint Len; public IntPtr Root, Name; public uint Attr; public IntPtr Sd, Qos; }
    [DllImport("ntdll.dll")] static extern int NtOpenDirectoryObject(out SafeFileHandle h, uint a, ref OA o);
    [DllImport("ntdll.dll")] static extern int NtOpenSymbolicLinkObject(out SafeFileHandle h, uint a, ref OA o);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern SafeFileHandle CreateFileW(string p, uint a, uint s, IntPtr sa, uint d, uint f, IntPtr t);
    [DllImport("advapi32.dll")] static extern uint GetSecurityInfo(SafeFileHandle h, int t, uint i, out IntPtr o, out IntPtr g, out IntPtr d, out IntPtr s, out IntPtr sd);
    [DllImport("advapi32.dll")] static extern uint SetSecurityInfo(SafeFileHandle h, int t, uint i, IntPtr o, IntPtr g, byte[] d, IntPtr s);
    [DllImport("advapi32.dll")] static extern uint GetSecurityDescriptorLength(IntPtr sd);

    public static string Sid(string name) {
        IntPtr p; int hr = DeriveAppContainerSidFromAppContainerName(name, out p);
        if (hr != 0) throw new Win32Exception(hr);
        return new SecurityIdentifier(p).Value;
    }
    public static string LongPath(string s) {
        var b = new StringBuilder(1024);
        if (GetLongPathNameW(s, b, 1024) == 0) throw new Win32Exception(Marshal.GetLastWin32Error());
        return b.ToString();
    }
    static SafeFileHandle OpenNs(string name, bool dir) {
        IntPtr text = Marshal.StringToHGlobalUni(name);
        var u = new US { L = (ushort)(name.Length * 2), M = (ushort)(name.Length * 2 + 2), B = text };
        IntPtr up = Marshal.AllocHGlobal(Marshal.SizeOf<US>()); Marshal.StructureToPtr(u, up, false);
        var o = new OA { Len = (uint)Marshal.SizeOf<OA>(), Name = up, Attr = 0x40 };
        SafeFileHandle h;
        int st = dir ? NtOpenDirectoryObject(out h, 0x00060000, ref o) : NtOpenSymbolicLinkObject(out h, 0x00060000, ref o);
        if (st < 0) throw new Exception("open " + name + " NTSTATUS=0x" + st.ToString("X8"));
        return h;
    }
    // kind: dir | link | dev ; adds one explicit non-inheriting allow ACE for sid.
    public static void Grant(string kind, string name, string sid, int mask) {
        SafeFileHandle h; int type;
        if (kind == "dev") { h = CreateFileW(@"\\?\GLOBALROOT" + name, 0x00060000, 3, IntPtr.Zero, 3, 0, IntPtr.Zero); type = 1;
            if (h.IsInvalid) throw new Win32Exception(Marshal.GetLastWin32Error(), "open " + name); }
        else { h = OpenNs(name, kind == "dir"); type = 6; }
        using (h) {
            IntPtr o, g, d, s, sd;
            uint e = GetSecurityInfo(h, type, 4, out o, out g, out d, out s, out sd);
            if (e != 0) throw new Win32Exception((int)e, "read DACL " + name);
            int len = (int)GetSecurityDescriptorLength(sd); var bytes = new byte[len]; Marshal.Copy(sd, bytes, 0, len);
            var acl = new RawSecurityDescriptor(bytes, 0).DiscretionaryAcl;
            acl.InsertAce(0, new CommonAce(AceFlags.None, AceQualifier.AccessAllowed, mask, new SecurityIdentifier(sid), false, null));
            var outb = new byte[acl.BinaryLength]; acl.GetBinaryForm(outb, 0);
            e = SetSecurityInfo(h, type, 4, IntPtr.Zero, IntPtr.Zero, outb, IntPtr.Zero);
            if (e != 0) throw new Win32Exception((int)e, "write DACL " + name);
        }
    }
}
'@

# Exact objects and masks used by newt's Enable-AppContainerDosPaths.ps1.
$objects = [ordered]@{
    'GLOBAL??'        = @('dir',  '\GLOBAL??', 0x00020003)              # DIRECTORY_QUERY|TRAVERSE|READ_CONTROL
    'MPM-link'        = @('link', '\GLOBAL??\MountPointManager', 0x00020001) # SYMBOLIC_LINK_QUERY|READ_CONTROL
    'C-link'          = @('link', '\GLOBAL??\C:', 0x00020001)
    'D-link'          = @('link', '\GLOBAL??\D:', 0x00020001)
    'MPM-device'      = @('dev',  '\Device\MountPointManager', 0x00120088)  # READ_EA|READ_ATTRIBUTES|READ_CONTROL|SYNCHRONIZE
}

$launcher = (Resolve-Path -LiteralPath $LauncherPath).ProviderPath
$git = Join-Path ((& git --exec-path).Trim()) 'git.exe'
& $git --version
"TEMP=$env:TEMP"
"GetTempPath=$([IO.Path]::GetTempPath())"

function New-Repo($root) {
    New-Item -ItemType Directory -Force $root | Out-Null
    icacls.exe $root /setintegritylevel '(OI)(CI)Low' | Out-Null
    $env:GIT_CONFIG_NOSYSTEM = '1'; $env:GIT_CONFIG_GLOBAL = 'NUL'
    & $git init --quiet "$root\repo"
    return "$root\repo"
}
$tag = [Guid]::NewGuid().ToString('N').Substring(0, 8)
$shortRepo = New-Repo (Join-Path $env:TEMP "ab408-$tag")
$longRepo = [P408]::LongPath($shortRepo)
$cRepo = New-Repo "C:\ab408-$tag"
"shortRepo=$shortRepo"; "longRepo=$longRepo"; "cRepo=$cRepo"

$n = 0
function Trial($label, $repo, [string[]]$grants, [switch]$Trace) {
    $script:n++
    $name = "ab408-$tag-$($script:n)"
    $sid = [P408]::Sid($name)
    foreach ($g in $grants) { $o = $objects[$g]; [P408]::Grant($o[0], $o[1], $sid, $o[2]) }
    $env:GIT_CONFIG_NOSYSTEM = '1'; $env:GIT_CONFIG_SYSTEM = 'NUL'; $env:GIT_CONFIG_GLOBAL = 'NUL'
    $env:GIT_TERMINAL_PROMPT = '0'; $env:HOME = 'C:\Windows'
    $env:GIT_CONFIG_COUNT = '1'; $env:GIT_CONFIG_KEY_0 = 'safe.directory'; $env:GIT_CONFIG_VALUE_0 = '*'
    if ($Trace) { $env:GIT_TRACE_SETUP = '1'; $env:GIT_TRACE = '1' }
    Push-Location C:\Windows
    try { $out = & $launcher --name $name --nul-device-ace --fs-write $repo $git -C $repo status --porcelain 2>&1; $code = $LASTEXITCODE }
    finally { Pop-Location; Remove-Item Env:GIT_TRACE_SETUP, Env:GIT_TRACE -ErrorAction SilentlyContinue }
    "### [$label] repo=$repo grants=$($grants -join ',') EXIT=$code"
    $out | ForEach-Object { "    $_" }
}

# Round 2: narrowest MountPointManager device mask (per-SID, device ACE only).
foreach ($m in @(0x00100000, 0x00000080, 0x00100080, 0x00020000, 0x00000008, 0x00120088)) {
    $objects['MPM-device'] = @('dev', '\Device\MountPointManager', $m)
    Trial ("MPM-device mask 0x{0:X8}" -f $m) $shortRepo @('MPM-device')
}
