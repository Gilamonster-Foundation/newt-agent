#Requires -Version 7.0
# Internal fixed-operation host-preparation helper. Loading this file changes no ACL.
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'This host prerequisite requires Windows.' }

Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Security.AccessControl;
using System.Security.Principal;
using Microsoft.Win32.SafeHandles;

namespace Newt.WindowsHostPreparation {
    public static class AppContainerHostAcl {
        private const uint DaclSecurityInformation = 4;
        private const int FileObject = 1;
        private const int KernelObject = 6;
        private const int DirectoryQueryTraverse = 0x00020003;
        private const int LinkQuery = 0x00020001;
        // FILE_READ_EA | FILE_READ_ATTRIBUTES | READ_CONTROL | SYNCHRONIZE.
        // Neither FILE_READ_DATA nor FILE_WRITE_DATA; never fall back to GR.
        private const int MountMetadata = 0x00120088;
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

        [StructLayout(LayoutKind.Sequential)]
        private struct UnicodeString {
            public ushort Length, MaximumLength;
            public IntPtr Buffer;
        }
        [StructLayout(LayoutKind.Sequential)]
        private struct ObjectAttributes {
            public uint Length;
            public IntPtr RootDirectory, ObjectName;
            public uint Attributes;
            public IntPtr SecurityDescriptor, SecurityQualityOfService;
        }
        [DllImport("ntdll.dll")]
        private static extern int NtOpenDirectoryObject(out SafeFileHandle handle, uint access, ref ObjectAttributes attributes);
        [DllImport("ntdll.dll")]
        private static extern int NtOpenSymbolicLinkObject(out SafeFileHandle handle, uint access, ref ObjectAttributes attributes);
        [DllImport("ntdll.dll")]
        private static extern uint RtlNtStatusToDosError(int status);
        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern uint GetLogicalDrives();
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode)]
        private static extern uint GetDriveTypeW(string root);

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
        private static byte[] ComparisonBytes(RawAcl acl, bool fileMapping = true) {
            RawAcl copy = new RawAcl(Bytes(acl), 0);
            for (int i = 0; i < copy.Count; i++) {
                KnownAce known = copy[i] as KnownAce;
                if (known != null) known.AccessMask = ComparableMask(known.AccessMask, fileMapping);
            }
            return Bytes(copy);
        }

        private static int ComparableMask(int mask, bool fileMapping) {
            if (fileMapping) return NormalizeMask(mask);
            if ((unchecked((uint)mask) & 0xF0000000) != 0)
                throw new InvalidOperationException("Unexpected generic rights on an NT namespace ACL; refusing to guess an object-specific mapping.");
            return mask;
        }

        // Copy every existing ACE unchanged. Do not coalesce other grants for
        // this SID, replace the descriptor, or normalize an administrator's ACL.
        private static RawAcl Prepare(RawAcl original, out bool changed) {
            return Prepare(original, ReadWrite, true, out changed);
        }

        private static RawAcl Prepare(RawAcl original, int access, bool fileMapping, out bool changed) {
            if (original == null) throw new InvalidOperationException("Refusing an absent/null host object DACL; inspect host policy manually.");
            // Check every NT ACE before making any change, including other SIDs.
            ComparisonBytes(original, fileMapping);
            bool present = false;
            for (int i = 0; i < original.Count; i++) {
                KnownAce known = original[i] as KnownAce;
                if (known == null || !known.SecurityIdentifier.Equals(Packages)) continue;
                QualifiedAce qualified = original[i] as QualifiedAce;
                if (qualified == null || qualified.IsCallback ||
                    qualified.AceQualifier != AceQualifier.AccessAllowed) {
                    throw new InvalidOperationException("Conflicting host object ACE for ALL APPLICATION PACKAGES; inspect host policy manually.");
                }
                if (original[i].AceType == AceType.AccessAllowed &&
                    original[i].AceFlags == AceFlags.None && ComparableMask(known.AccessMask, fileMapping) == access) present = true;
            }
            changed = !present;
            RawAcl result = new RawAcl(Bytes(original), 0);
            if (changed) {
                int insertion = 0;
                while (insertion < result.Count && (result[insertion].AceFlags & AceFlags.Inherited) == 0) insertion++;
                result.InsertAce(insertion, new CommonAce(AceFlags.None, AceQualifier.AccessAllowed,
                    access, Packages, false, null));
            }
            return result;
        }

        private static RawSecurityDescriptor Read(SafeFileHandle handle, int objectType = FileObject) {
            IntPtr owner, group, dacl, sacl, descriptor;
            uint error = GetSecurityInfo(handle, objectType, DaclSecurityInformation,
                out owner, out group, out dacl, out sacl, out descriptor);
            if (error != 0) throw new Win32Exception((int)error, "Cannot read the host object DACL.");
            try {
                int length = checked((int)GetSecurityDescriptorLength(descriptor));
                if (length == 0) throw new InvalidOperationException("Host object returned an empty security descriptor.");
                byte[] bytes = new byte[length];
                Marshal.Copy(descriptor, bytes, 0, length);
                return new RawSecurityDescriptor(bytes, 0);
            } finally { LocalFree(descriptor); }
        }

        public static string Apply() {
            // READ_CONTROL | WRITE_DAC; no data, ownership, or SACL access.
            // GLOBALROOT bypasses any per-logon DOS alias named NUL.
            SafeFileHandle handle = CreateFileW(@"\\?\GLOBALROOT\Device\Null", 0x00060000,
                3, IntPtr.Zero, 3, 0, IntPtr.Zero);
            if (handle.IsInvalid) {
                int error = Marshal.GetLastWin32Error(); handle.Dispose();
                throw new Win32Exception(error, "Cannot open fixed \\Device\\Null for host preparation.");
            }
            using (PendingAcl update = new PendingAcl(handle, @"\Device\Null", FileObject, ReadWrite)) {
                update.Apply();
                if (!update.Changed) return "NUL: required non-inheriting GR|GW AppContainer ACE already present; no change.";
                return "NUL: added only non-inheriting GR|GW for ALL APPLICATION PACKAGES (S-1-15-2-1), until reboot.";
            }
        }

        // These private openers are reachable only from the fixed operation
        // below. No operator/model-provided path, SID, or access mask is accepted.
        private static SafeFileHandle OpenNamespace(string name, bool directory) {
            IntPtr text = Marshal.StringToHGlobalUni(name);
            IntPtr unicodePointer = IntPtr.Zero;
            try {
                UnicodeString unicode = new UnicodeString {
                    Length = checked((ushort)(name.Length * 2)),
                    MaximumLength = checked((ushort)((name.Length + 1) * 2)), Buffer = text
                };
                unicodePointer = Marshal.AllocHGlobal(Marshal.SizeOf<UnicodeString>());
                Marshal.StructureToPtr(unicode, unicodePointer, false);
                ObjectAttributes attributes = new ObjectAttributes {
                    Length = (uint)Marshal.SizeOf<ObjectAttributes>(),
                    ObjectName = unicodePointer, Attributes = 0x40 // OBJ_CASE_INSENSITIVE
                };
                SafeFileHandle handle;
                int status = directory
                    ? NtOpenDirectoryObject(out handle, 0x00060000, ref attributes)
                    : NtOpenSymbolicLinkObject(out handle, 0x00060000, ref attributes);
                if (status < 0) {
                    if (handle != null) handle.Dispose();
                    throw new Win32Exception((int)RtlNtStatusToDosError(status), "Cannot open fixed NT object " + name);
                }
                if (handle == null || handle.IsInvalid) {
                    if (handle != null) handle.Dispose();
                    throw new InvalidOperationException("NT object opener returned an invalid handle for " + name);
                }
                return handle;
            } finally {
                if (unicodePointer != IntPtr.Zero) Marshal.FreeHGlobal(unicodePointer);
                Marshal.FreeHGlobal(text);
            }
        }

        private sealed class PendingAcl : IDisposable {
            public readonly SafeFileHandle Handle;
            public readonly string Name;
            public readonly int ObjectType;
            public readonly bool FileMapping, Changed;
            public readonly RawSecurityDescriptor Before;
            public readonly RawAcl Updated;
            public bool WriteCompleted { get; private set; }
            public PendingAcl(SafeFileHandle handle, string name, int type, int access) {
                Handle = handle; Name = name; ObjectType = type; FileMapping = type == FileObject;
                try {
                    Before = Read(handle, type);
                    bool changed;
                    Updated = Prepare(Before.DiscretionaryAcl, access, FileMapping, out changed);
                    Changed = changed;
                } catch { Handle.Dispose(); throw; }
            }
            public void Dispose() { Handle.Dispose(); }
            public void Apply() {
                if (!Changed) return;
                RawSecurityDescriptor current = Read(Handle, ObjectType);
                if (current.ControlFlags != Before.ControlFlags || current.DiscretionaryAcl == null ||
                    !Equal(Bytes(Before.DiscretionaryAcl), Bytes(current.DiscretionaryAcl)))
                    throw new InvalidOperationException("DACL changed during preparation for " + Name + "; this object was not updated.");
                uint error = SetSecurityInfo(Handle, ObjectType, DaclSecurityInformation,
                    IntPtr.Zero, IntPtr.Zero, Bytes(Updated), IntPtr.Zero);
                if (error != 0) throw new Win32Exception((int)error, "Cannot apply DACL prerequisite to " + Name);
                WriteCompleted = true;
                RawSecurityDescriptor after = Read(Handle, ObjectType);
                if (after.DiscretionaryAcl == null ||
                    !Equal(ComparisonBytes(Updated, FileMapping), ComparisonBytes(after.DiscretionaryAcl, FileMapping)) ||
                    (Before.ControlFlags & ControlFlags.DiscretionaryAclProtected) !=
                    (after.ControlFlags & ControlFlags.DiscretionaryAclProtected))
                    throw new InvalidOperationException("DACL readback failed for " + Name + "; inspect host policy manually.");
            }
        }

        public static string[] ApplyDosPaths(bool includeDevice) {
            List<PendingAcl> pending = new List<PendingAcl>();
            try {
                // Preflight and hold every exact object before any DACL write.
                pending.Add(new PendingAcl(OpenNamespace(@"\GLOBAL??", true), @"\GLOBAL??",
                    KernelObject, DirectoryQueryTraverse));
                pending.Add(new PendingAcl(OpenNamespace(@"\GLOBAL??\MountPointManager", false), @"\GLOBAL??\MountPointManager",
                    KernelObject, LinkQuery));
                uint drives = GetLogicalDrives();
                if (drives == 0) throw new Win32Exception(Marshal.GetLastWin32Error(), "Cannot enumerate local drive letters.");
                int localDrives = 0;
                for (int index = 0; index < 26; index++) {
                    if ((drives & (1u << index)) == 0) continue;
                    char letter = (char)('A' + index);
                    uint type = GetDriveTypeW(letter + @":\");
                    // DRIVE_REMOVABLE / DRIVE_FIXED / DRIVE_RAMDISK only.
                    // No network, optical, unknown, or arbitrary namespace entries.
                    if (type != 2 && type != 3 && type != 6) continue;
                    string name = @"\GLOBAL??\" + letter + ":";
                    pending.Add(new PendingAcl(OpenNamespace(name, false), name, KernelObject, LinkQuery));
                    localDrives++;
                }
                if (localDrives == 0) throw new InvalidOperationException("No supported local drive links found; no ACLs changed.");
                if (includeDevice) {
                    // Bypass per-logon DOS aliases for the ACL edit itself.
                    const string device = @"\\?\GLOBALROOT\Device\MountPointManager";
                    SafeFileHandle handle = CreateFileW(device, 0x00060000, 3, IntPtr.Zero, 3, 0, IntPtr.Zero);
                    if (handle.IsInvalid) {
                        int error = Marshal.GetLastWin32Error(); handle.Dispose();
                        throw new Win32Exception(error, "Cannot open fixed MountPointManager device for metadata-only preparation.");
                    }
                    pending.Add(new PendingAcl(handle, @"\Device\MountPointManager", FileObject, MountMetadata));
                }
                bool changed = false;
                List<string> receipts = new List<string>();
                foreach (PendingAcl item in pending) {
                    item.Apply();
                    changed |= item.Changed;
                    receipts.Add("DOS paths: " + item.Name + (item.Changed ? " exact non-inheriting ACE added." : " already present; no change."));
                }
                receipts.Add(changed ? "DOS paths: bounded ACL preparation applied; validate with the confined probe."
                    : "DOS paths: required AppContainer ACEs already present; no change.");
                return receipts.ToArray();
            } catch (Exception error) {
                int written = 0;
                foreach (PendingAcl item in pending) if (item.WriteCompleted) written++;
                throw new InvalidOperationException("DOS-path preparation stopped; " + written +
                    " object DACL writes completed before the failure. No stale ACL rollback was attempted. " + error.Message, error);
            } finally { foreach (PendingAcl item in pending) item.Dispose(); }
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
            Require((MountMetadata & (0x1 | 0x2 | 0x20 | unchecked((int)0xF0000000))) == 0,
                "MountPointManager grant must not include read-data, write-data, execute, or generic bits");
            foreach (int access in new[] { DirectoryQueryTraverse, LinkQuery, MountMetadata }) {
                bool fileMapping = access == MountMetadata;
                RawAcl original = new RawSecurityDescriptor("D:(D;;0x2;;;BU)(A;;0x001F01FF;;;SY)(A;ID;0x80;;;WD)").DiscretionaryAcl;
                byte[] originalBytes = Bytes(original);
                RawAcl update = Prepare(original, access, fileMapping, out changed);
                CommonAce ace = update[2] as CommonAce;
                Require(changed && ace != null && ace.AccessMask == access && ace.AceFlags == AceFlags.None &&
                    ace.SecurityIdentifier.Equals(Packages), "DOS-path grant must be exact and non-inheriting");
                Require(Equal(Bytes(update), Bytes(Prepare(update, access, fileMapping, out changed))) && !changed,
                    "DOS-path exact grant must be idempotent");
                update.RemoveAce(2);
                Require(Equal(originalBytes, Bytes(update)) && Equal(originalBytes, Bytes(original)),
                    "DOS-path preparation must preserve every other ACE and input byte");
                RawAcl denied = new RawSecurityDescriptor("D:(D;;0x1;;;AC)").DiscretionaryAcl;
                refused = false;
                try { Prepare(denied, access, fileMapping, out changed); }
                catch (InvalidOperationException) { refused = true; }
                Require(refused, "DOS-path package deny must be retained and refused");
            }
            foreach (int access in new[] { DirectoryQueryTraverse, LinkQuery }) {
                RawAcl unexpected = new RawSecurityDescriptor("D:(A;;GR;;;SY)").DiscretionaryAcl;
                refused = false;
                try { Prepare(unexpected, access, false, out changed); }
                catch (InvalidOperationException) { refused = true; }
                Require(refused, "NT namespace generics must fail closed, including on other SIDs");
            }
            RawAcl genericRead = new RawSecurityDescriptor("D:(A;;GR;;;AC)").DiscretionaryAcl;
            RawAcl metadata = Prepare(genericRead, MountMetadata, true, out changed);
            Require(changed && metadata.Count == 2 && ((KnownAce)metadata[1]).AccessMask == MountMetadata,
                "broader existing GR must not count as the exact metadata-only device grant");
            return "NUL and DOS-path ACL self-tests passed (in-memory only; no device opened).";
        }
    }
}
'@

function Invoke-NewtAppContainerPreparation {
    param([Parameter(Mandatory)][ValidateSet('Null', 'DosPaths', 'NamespaceOnly', 'SelfTest')][string]$Operation)
    if ($Operation -eq 'SelfTest') {
        [Newt.WindowsHostPreparation.AppContainerHostAcl]::SelfTest()
        return
    }
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    try {
        $principal = [Security.Principal.WindowsPrincipal]::new($identity)
        if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
            throw 'Run this script explicitly from an elevated PowerShell, once per boot. Newt will not elevate itself.'
        }
    } finally { $identity.Dispose() }
    # Abandoned mutex acquisition gives ownership; the operation reads fresh ACLs.
    $mutex = [Threading.Mutex]::new($false, 'Global\Newt.AppContainerHostPreparation')
    $acquired = $false
    try {
        try { $acquired = $mutex.WaitOne([TimeSpan]::FromSeconds(30)) }
        catch [Threading.AbandonedMutexException] { $acquired = $true }
        if (-not $acquired) { throw 'Timed out waiting for AppContainer host preparation; no update applied.' }
        switch ($Operation) {
            'Null' { [Newt.WindowsHostPreparation.AppContainerHostAcl]::Apply() }
            'DosPaths' { [Newt.WindowsHostPreparation.AppContainerHostAcl]::ApplyDosPaths($true) }
            'NamespaceOnly' { [Newt.WindowsHostPreparation.AppContainerHostAcl]::ApplyDosPaths($false) }
        }
    } finally {
        if ($acquired) { $mutex.ReleaseMutex() }
        $mutex.Dispose()
    }
}
