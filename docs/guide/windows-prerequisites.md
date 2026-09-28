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
second opens only literal `\\.\NUL` through Windows handle APIs and adds one
non-inheriting read/write (`GR|GW`) allow ACE for `ALL APPLICATION PACKAGES`
(`S-1-15-2-1`). This is a host-wide grant to **normal AppContainers**, not just
Newt. It grants no execute access, no filesystem paths or network capabilities,
and no `ALL RESTRICTED APPLICATION PACKAGES` / LPAC grant. Existing ACEs, owner,
group and SACL are retained. An exact existing ACE makes a repeated run a no-op.
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
