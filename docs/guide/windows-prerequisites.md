# Windows host prerequisites

Confined native commands require the trusted `agent-bridle-aclaunch.exe`
AppContainer launcher. Install it with
`cargo install agent-bridle-aclaunch --version 0.7.10 --locked` and keep the
launcher beside `newt.exe` in the same installation directory. An arbitrary
`PATH` entry is not trusted.

## Prepare the null device once per boot

Windows resets the `\Device\Null` security descriptor on reboot. Its default
permissions can prevent AppContainer processes from opening `NUL`. Native Git
opens this device during startup, even when its standard streams are already
connected; a denial can therefore prevent ordinary status and staging commands.

From the repository root, run these commands explicitly in **PowerShell 7 opened
as Administrator**, after each boot and before starting confined tools:

```powershell
./scripts/windows/Enable-AppContainerNull.ps1 -SelfTest
./scripts/windows/Enable-AppContainerNull.ps1
```

The first command checks synthetic ACLs in memory without opening a device. The
second opens only `\\?\GLOBALROOT\Device\Null` through Windows handle APIs and adds one
non-inheriting read/write (`GR|GW`) allow ACE for `ALL APPLICATION PACKAGES`
(`S-1-15-2-1`). This is a host-wide grant to **normal AppContainers**, not just
Newt. It grants no execute access, no filesystem paths or network capabilities,
and no `ALL RESTRICTED APPLICATION PACKAGES` / LPAC grant. Existing ACEs, owner,
group and SACL are retained. An exact existing ACE makes a repeated run a no-op.
The fixed global device path prevents a per-logon DOS alias from redirecting the
ACL edit.
The script writes the concrete Windows file-mask equivalent of `GR|GW`
(`0x0012019F`). Readback accepts Windows' generic-to-concrete mask mapping, while
still comparing every other ACE field and the original order. Equivalent
generic and concrete existing entries are both no-ops; extra rights do not
count as an exact match.
Conflicting package deny entries, unreadable policy and failed verification
cause an error rather than replacing host policy.

A named mutex serializes script invocations with a bounded wait. Other host
security administration must be coordinated separately; Windows does not offer
an atomic compare-and-swap for a DACL. The script rereads before writing and
verifies the resulting DACL. It never restores a stale full descriptor.

This is an explicit operator or installation prerequisite. Newt does not run
this script, request elevation, or expose it as a model tool. An administrator
may arrange the explicit boot-time setup under the machine's own management
policy. CI performs it visibly before ordinary confined tests; actual native
Git effects and filesystem/network denial controls still have to pass.

The boundary follows the separate elevated host-preparation approach described
by [Microsoft MXC](https://github.com/microsoft/mxc/blob/main/docs/host-prep.md#prepare-null-device).
The device-handle mechanism is also used by
[libuv's AppContainer test launcher](https://github.com/libuv/libuv/blob/v1.x/test/appcontainer.c#L196).
Newt adds only the stated DACL entry; it does not adopt MXC's complete replacement
descriptor or libuv's other test-launcher grants.
The comparison accounts for the NUL mask mapping documented in
[MXC's kernel descriptor implementation](https://github.com/microsoft/mxc/blob/715942607f210c300667ba01605b564ae7253c98/src/host/wxc_host_prep/src/null_device/sd.rs#L232).

## Verify confined DOS-path lookup

Git for Windows normalizes its working directory using
`GetFinalPathNameByHandleW` with DOS-volume output. AppContainer can deny that
lookup even when the directory itself is granted. Microsoft's
[MXC issue #694](https://github.com/microsoft/mxc/issues/694) identifies NT object
namespace and MountPointManager permissions as a cause. This section describes
a bounded compatibility setting. Each host must pass the confined probe;
installing the prerequisite alone is not evidence that path lookup works.

The separate elevated script accepts no path, SID, or access-mask arguments:

```powershell
./scripts/windows/Enable-AppContainerDosPaths.ps1 -SelfTest
./scripts/windows/Enable-AppContainerDosPaths.ps1 -NamespaceOnly
# Run the confined diagnostic to observe namespace-only behavior.
./scripts/windows/Enable-AppContainerDosPaths.ps1
```

It adds exact, non-inheriting allow entries for normal AppContainers
(`S-1-15-2-1`) only:

| Fixed object | Added rights |
| --- | --- |
| NT directory `\GLOBAL??` | Query, traverse, read control (`0x00020003`) |
| NT symbolic link `\GLOBAL??\MountPointManager` | Query, read control (`0x00020001`) |
| Existing local drive-letter links in `\GLOBAL??` | Query, read control (`0x00020001`) |
| Device `\Device\MountPointManager` | Read extended attributes, read attributes, read control, synchronize (`0x00120088`) |

Drive letters come from Windows' logical-drive inventory. Only removable,
fixed, and RAM-disk drives are included; network, optical and unknown types are
excluded. Newly attached drives require another explicit run. Device ACL edits
use `\\?\GLOBALROOT\Device\MountPointManager`, bypassing per-logon DOS aliases.
`-NamespaceOnly` does not open or change the device.

This exposes global DOS-device names and mount metadata to normal AppContainers.
It does not grant filesystem ancestor listing, file contents, namespace
creation/deletion, device data read/write, generic execute, network access, or
LPAC access. In particular, it does **not** grant MountPointManager
`FILE_GENERIC_READ`: that includes `FILE_READ_DATA` and admits state-changing
IOCTLs. In the
[pinned Windows Driver Kit header](https://github.com/microsoft/wdkmetadata/blob/fbb3a13785073a707c6171bffbd5522438031732/generation/WDK/IdlHeaders/km/mountmgr.h),
the public `FILE_ANY_ACCESS` operations query points, DOS volume paths or
automount state; the listed state-changing operations require read and/or write
data access. The script has no fallback that adds those rights. These documented
access checks bound the proposal; actual API compatibility still requires the
confined probe.

The scripts share the NUL helper's DACL preservation, exact-match no-op checks,
readback verification and bounded mutex. NT namespace ACLs containing unexpected
generic masks fail closed rather than borrowing the file-object mapping. All
handles and proposed ACLs are prepared before writing. A later write failure
can leave earlier bounded entries installed; the error reports completed DACL
writes. No stale full-descriptor rollback is attempted. Re-running is idempotent.
Existing broader administrator grants are preserved, not silently removed.

CI runs the confined probe before setup, after namespace-only setup, and after
metadata-only setup. The final probe must prove DOS-path lookup, an allowed file
read, a denied outside-file read, a successful fixed MountPointManager
`0x00120088` open, and denial of a `GENERIC_READ` open on that same device
without issuing a state-changing IOCTL. A repeated apply
must report no change. The three ordinary native Git effect tests remain
required. Until those controls pass, this setting is not evidence that
confined native Git works on the target Windows host.
