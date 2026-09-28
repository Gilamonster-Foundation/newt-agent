#Requires -Version 7.0
<#
.SYNOPSIS
Prepare the Windows NUL device for ordinary AppContainer tools, once per boot.
.DESCRIPTION
Run explicitly from an elevated PowerShell. Only the DACL of literal \\.\NUL
is changed: add a non-inheriting GENERIC_READ | GENERIC_WRITE allow ACE for
ALL APPLICATION PACKAGES. Existing ACEs and other security components remain.
This is host preparation, never a Newt runtime or model tool.
.PARAMETER SelfTest
Test the ACL transformation in memory; do not open or change any device.
#>
[CmdletBinding()]
param([switch]$SelfTest)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'This host prerequisite requires Windows.' }
if (-not $SelfTest) {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    try {
        $principal = [Security.Principal.WindowsPrincipal]::new($identity)
        if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
            throw 'Run this script explicitly from an elevated PowerShell, once per boot. Newt will not elevate itself.'
        }
    } finally { $identity.Dispose() }
}

Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Security.AccessControl;
using System.Security.Principal;
using Microsoft.Win32.SafeHandles;

namespace Newt.WindowsHostPreparation {
    public static class NullDevice {
        private const uint DaclSecurityInformation = 4;
        private const int FileObject = 1;
        // FILE_GENERIC_READ | FILE_GENERIC_WRITE, the kernel's concrete GR|GW
        // mapping for IoFileObjectType (including NUL). No execute right.
        private const int ReadWrite = 0x0012019F;
        private static readonly SecurityIdentifier Packages = new SecurityIdentifier("S-1-15-2-1");

        [StructLayout(LayoutKind.Sequential)]
        private struct GenericMapping {
            public uint Read, Write, Execute, All;
        }
        [DllImport("advapi32.dll")]
        private static extern void MapGenericMask(ref uint mask, ref GenericMapping mapping);

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        private static extern SafeFileHandle CreateFileW(string path, uint access,
            uint sharing, IntPtr attributes, uint disposition, uint flags, IntPtr template);
        [DllImport("advapi32.dll")]
        private static extern uint GetSecurityInfo(SafeFileHandle handle, int type, uint information,
            out IntPtr owner, out IntPtr group, out IntPtr dacl, out IntPtr sacl, out IntPtr descriptor);
        [DllImport("advapi32.dll")]
        private static extern uint SetSecurityInfo(SafeFileHandle handle, int type, uint information,
            IntPtr owner, IntPtr group, byte[] dacl, IntPtr sacl);
        [DllImport("advapi32.dll")]
        private static extern uint GetSecurityDescriptorLength(IntPtr descriptor);
        [DllImport("kernel32.dll")]
        private static extern IntPtr LocalFree(IntPtr allocation);

        private static byte[] Bytes(GenericAcl acl) {
            byte[] bytes = new byte[acl.BinaryLength];
            acl.GetBinaryForm(bytes, 0);
            return bytes;
        }

        private static bool Equal(byte[] left, byte[] right) {
            if (left.Length != right.Length) return false;
            for (int i = 0; i < left.Length; i++) if (left[i] != right[i]) return false;
            return true;
        }

        private static int NormalizeMask(int mask) {
            uint mapped = unchecked((uint)mask);
            GenericMapping file = new GenericMapping {
                Read = 0x00120089, Write = 0x00120116,
                Execute = 0x001200A0, All = 0x001F01FF
            };
            MapGenericMask(ref mapped, ref file);
            return unchecked((int)mapped);
        }

        // NUL's kernel setter expands generic masks in existing ACEs as well.
        // Compare only cloned masks in their concrete form; every other byte
        // (including SID, flags, opaque fields, and ACE order) still must match.
        // Never use this normalized copy for the write or pre-write race check.
        private static byte[] ComparisonBytes(RawAcl acl) {
            RawAcl copy = new RawAcl(Bytes(acl), 0);
            for (int i = 0; i < copy.Count; i++) {
                KnownAce known = copy[i] as KnownAce;
                if (known != null) known.AccessMask = NormalizeMask(known.AccessMask);
            }
            return Bytes(copy);
        }

        // Copy every existing ACE unchanged. Do not coalesce other grants for
        // this SID, replace the descriptor, or normalize an administrator's ACL.
        private static RawAcl Prepare(RawAcl original, out bool changed) {
            if (original == null) throw new InvalidOperationException("Refusing an absent/null NUL DACL; inspect host policy manually.");
            bool present = false;
            for (int i = 0; i < original.Count; i++) {
                KnownAce known = original[i] as KnownAce;
                if (known == null || !known.SecurityIdentifier.Equals(Packages)) continue;
                QualifiedAce qualified = original[i] as QualifiedAce;
                if (qualified == null || qualified.IsCallback ||
                    qualified.AceQualifier != AceQualifier.AccessAllowed) {
                    throw new InvalidOperationException("Conflicting NUL ACE for ALL APPLICATION PACKAGES; inspect host policy manually.");
                }
                if (original[i].AceType == AceType.AccessAllowed &&
                    original[i].AceFlags == AceFlags.None && NormalizeMask(known.AccessMask) == ReadWrite) present = true;
            }
            changed = !present;
            RawAcl result = new RawAcl(Bytes(original), 0);
            if (changed) {
                int insertion = 0;
                while (insertion < result.Count && (result[insertion].AceFlags & AceFlags.Inherited) == 0) insertion++;
                result.InsertAce(insertion, new CommonAce(AceFlags.None, AceQualifier.AccessAllowed,
                    ReadWrite, Packages, false, null));
            }
            return result;
        }

        private static RawSecurityDescriptor Read(SafeFileHandle handle) {
            IntPtr owner, group, dacl, sacl, descriptor;
            uint error = GetSecurityInfo(handle, FileObject, DaclSecurityInformation,
                out owner, out group, out dacl, out sacl, out descriptor);
            if (error != 0) throw new Win32Exception((int)error, "Cannot read the NUL DACL.");
            try {
                int length = checked((int)GetSecurityDescriptorLength(descriptor));
                if (length == 0) throw new InvalidOperationException("NUL returned an empty security descriptor.");
                byte[] bytes = new byte[length];
                Marshal.Copy(descriptor, bytes, 0, length);
                return new RawSecurityDescriptor(bytes, 0);
            } finally { LocalFree(descriptor); }
        }

        public static string Apply() {
            // READ_CONTROL | WRITE_DAC; no data, ownership, or SACL access.
            using (SafeFileHandle handle = CreateFileW(@"\\.\NUL", 0x00060000,
                3, IntPtr.Zero, 3, 0, IntPtr.Zero)) {
                if (handle.IsInvalid) throw new Win32Exception(Marshal.GetLastWin32Error(), "Cannot open literal \\\\.\\NUL for host preparation.");
                RawSecurityDescriptor before = Read(handle);
                bool changed;
                RawAcl updated = Prepare(before.DiscretionaryAcl, out changed);
                if (!changed) return "NUL: required non-inheriting GR|GW AppContainer ACE already present; no change.";
                // Detect intervening changes before writing; administrators must
                // still serialize host ACL administration (Win32 has no DACL CAS).
                RawSecurityDescriptor current = Read(handle);
                if (current.ControlFlags != before.ControlFlags || current.DiscretionaryAcl == null ||
                    !Equal(Bytes(before.DiscretionaryAcl), Bytes(current.DiscretionaryAcl))) {
                    throw new InvalidOperationException("NUL DACL changed during preparation; no update applied. Retry with host ACL administration serialized.");
                }
                uint error = SetSecurityInfo(handle, FileObject, DaclSecurityInformation,
                    IntPtr.Zero, IntPtr.Zero, Bytes(updated), IntPtr.Zero);
                if (error != 0) throw new Win32Exception((int)error, "Cannot apply the NUL DACL prerequisite.");
                RawSecurityDescriptor after = Read(handle);
                if (after.DiscretionaryAcl == null ||
                    !Equal(ComparisonBytes(updated), ComparisonBytes(after.DiscretionaryAcl)) ||
                    (before.ControlFlags & ControlFlags.DiscretionaryAclProtected) !=
                    (after.ControlFlags & ControlFlags.DiscretionaryAclProtected)) {
                    throw new InvalidOperationException("NUL DACL readback differs from the requested update; host preparation is not verified. Inspect host policy manually.");
                }
                return "NUL: added only non-inheriting GR|GW for ALL APPLICATION PACKAGES (S-1-15-2-1), until reboot.";
            }
        }

        private static void Require(bool condition, string message) {
            if (!condition) throw new InvalidOperationException("ACL self-test: " + message);
        }

        public static string SelfTest() {
            RawAcl before = new RawSecurityDescriptor("D:(D;;GW;;;BU)(A;;GA;;;SY)(A;ID;GR;;;WD)").DiscretionaryAcl;
            byte[] untouched = Bytes(before);
            bool changed;
            RawAcl after = Prepare(before, out changed);
            Require(changed && after.Count == before.Count + 1, "must add exactly one ACE");
            Require(Equal(untouched, Bytes(before)), "must not mutate input");
            CommonAce added = after[2] as CommonAce;
            Require(added != null && added.AceFlags == AceFlags.None && added.AccessMask == ReadWrite &&
                added.SecurityIdentifier.Equals(Packages), "must grant only explicit GR|GW to the normal package SID");
            RawAcl removed = new RawAcl(Bytes(after), 0);
            removed.RemoveAce(2);
            Require(Equal(untouched, Bytes(removed)), "must preserve all other ACEs and their order");
            Require(Equal(Bytes(after), Bytes(Prepare(after, out changed))) && !changed, "must be idempotent");
            Require(NormalizeMask(unchecked((int)0xC0000000)) == ReadWrite &&
                NormalizeMask(unchecked((int)0xA0000000)) == 0x001200A9 &&
                NormalizeMask(0x10000000) == 0x001F01FF,
                "generic masks must use the kernel file mapping");
            foreach (string sddl in new[] { "D:(A;;GRGW;;;AC)", "D:(A;;0x0012019F;;;AC)" }) {
                RawAcl existing = new RawSecurityDescriptor(sddl).DiscretionaryAcl;
                Require(Equal(Bytes(existing), Bytes(Prepare(existing, out changed))) && !changed,
                    "equivalent generic/concrete GR|GW must be idempotent without rewriting");
            }
            RawAcl mapped = new RawSecurityDescriptor("D:(D;;0x00120116;;;BU)(A;;0x001F01FF;;;SY)(A;ID;0x00120089;;;WD)").DiscretionaryAcl;
            Require(Equal(ComparisonBytes(before), ComparisonBytes(mapped)), "kernel-mapped existing ACEs must compare equal");
            Require(Equal(untouched, Bytes(before)), "comparison must not normalize the original ACL");
            RawAcl modified = new RawAcl(Bytes(mapped), 0);
            ((KnownAce)modified[0]).AccessMask |= 0x20;
            Require(!Equal(ComparisonBytes(before), ComparisonBytes(modified)), "additional rights must not compare equal");
            modified = new RawAcl(Bytes(mapped), 0);
            modified[0].AceFlags |= AceFlags.Inherited;
            Require(!Equal(ComparisonBytes(before), ComparisonBytes(modified)), "different ACE flags must not compare equal");
            modified = new RawAcl(Bytes(mapped), 0);
            ((KnownAce)modified[0]).SecurityIdentifier = Packages;
            Require(!Equal(ComparisonBytes(before), ComparisonBytes(modified)), "different SID must not compare equal");
            modified = new RawAcl(Bytes(mapped), 0);
            GenericAce first = modified[0];
            modified.RemoveAce(0);
            modified.InsertAce(1, first);
            Require(!Equal(ComparisonBytes(before), ComparisonBytes(modified)), "different ACE order must not compare equal");
            foreach (string sddl in new[] { "D:(A;;GRGWGX;;;AC)", "D:(A;;0x001201BF;;;AC)" }) {
                RawAcl extra = new RawSecurityDescriptor(sddl).DiscretionaryAcl;
                Require(Prepare(extra, out changed).Count == 2 && changed,
                    "an existing extra execute right is not the exact prerequisite ACE");
            }
            RawAcl broader = new RawSecurityDescriptor("D:(A;;GA;;;AC)").DiscretionaryAcl;
            RawAcl preserved = Prepare(broader, out changed);
            Require(changed && preserved.Count == 2 && ((KnownAce)preserved[0]).AccessMask == ((KnownAce)broader[0]).AccessMask,
                "must not replace an existing administrator grant");
            RawAcl inherited = new RawSecurityDescriptor("D:(A;ID;GRGW;;;AC)").DiscretionaryAcl;
            Require(Prepare(inherited, out changed).Count == 2 && changed, "inherited ACE must not satisfy explicit prerequisite");
            bool refused = false;
            try { Prepare(new RawSecurityDescriptor("D:(D;;GW;;;AC)").DiscretionaryAcl, out changed); }
            catch (InvalidOperationException) { refused = true; }
            Require(refused, "must refuse a conflicting package deny");
            refused = false;
            try { Prepare(null, out changed); }
            catch (InvalidOperationException) { refused = true; }
            Require(refused, "must refuse a null DACL");
            return "NUL ACL self-tests passed (in-memory only; no device opened).";
        }
    }
}
'@

if ($SelfTest) {
    [Newt.WindowsHostPreparation.NullDevice]::SelfTest()
} else {
    # Coordinate this script's invocations across sessions. An abandoned mutex
    # grants ownership; Apply always reads fresh state rather than a saved ACL.
    $mutex = [Threading.Mutex]::new($false, 'Global\Newt.AppContainerNull.HostPreparation')
    $acquired = $false
    try {
        try { $acquired = $mutex.WaitOne([TimeSpan]::FromSeconds(30)) }
        catch [Threading.AbandonedMutexException] { $acquired = $true }
        if (-not $acquired) { throw 'Timed out waiting for NUL host preparation; no update applied.' }
        [Newt.WindowsHostPreparation.NullDevice]::Apply()
    } finally {
        if ($acquired) { $mutex.ReleaseMutex() }
        $mutex.Dispose()
    }
}
