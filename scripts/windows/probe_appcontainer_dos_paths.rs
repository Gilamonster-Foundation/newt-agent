//! Standalone AppContainer path diagnostic; build with rustc, no dependencies.
//! No path or file contents are printed. See Test-AppContainerDosPaths.ps1.

use std::ffi::OsString;

struct Arguments {
    require_dos: bool,
    trace_dos: bool,
    allowed: OsString,
    denied: OsString,
}

fn arguments() -> Option<Arguments> {
    let mut args = std::env::args_os().skip(1);
    let require_dos = match args.next()?.to_str()? {
        "--report-only" => false,
        "--require-dos" => true,
        _ => return None,
    };
    let mut parsed = Arguments {
        require_dos,
        trace_dos: false,
        allowed: args.next()?,
        denied: args.next()?,
    };
    if let Some(option) = args.next() {
        if option != "--trace-dos" {
            return None;
        }
        parsed.trace_dos = true;
    }
    args.next().is_none().then_some(parsed)
}

fn main() {
    let Some(args) = arguments() else {
        eprintln!("arguments error=87"); // ERROR_INVALID_PARAMETER
        std::process::exit(87);
    };
    #[cfg(windows)]
    std::process::exit(i32::from(!windows::run(&args)));
    #[cfg(not(windows))]
    {
        let _ = (args.require_dos, args.trace_dos, args.allowed, args.denied);
        eprintln!("platform error=50"); // ERROR_NOT_SUPPORTED
        std::process::exit(50);
    }
}

#[cfg(windows)]
mod windows {
    use super::Arguments;
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};

    type Handle = *mut c_void;
    const INVALID_HANDLE: Handle = -1_isize as Handle;
    const MARKER: &[u8] = b"newt-appcontainer-workspace-marker";

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentDirectoryW(length: u32, buffer: *mut u16) -> u32;
        fn CreateFileW(
            path: *const u16,
            access: u32,
            sharing: u32,
            attributes: *const c_void,
            disposition: u32,
            flags: u32,
            template: Handle,
        ) -> Handle;
        fn GetFinalPathNameByHandleW(
            handle: Handle,
            buffer: *mut u16,
            size: u32,
            flags: u32,
        ) -> u32;
        fn GetLongPathNameW(path: *const u16, buffer: *mut u16, size: u32) -> u32;
        fn QueryDosDeviceW(name: *const u16, buffer: *mut u16, size: u32) -> u32;
        fn GetVolumePathNamesForVolumeNameW(
            volume: *const u16,
            buffer: *mut u16,
            size: u32,
            returned: *mut u32,
        ) -> i32;
        fn GetLastError() -> u32;
        fn CloseHandle(handle: Handle) -> i32;
    }

    struct OwnedHandle(Handle);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: constructed only from a successful, uniquely owned open.
            unsafe { CloseHandle(self.0) };
        }
    }

    fn report<T>(api: &str, result: &Result<T, u32>) -> bool {
        match result {
            Ok(_) => println!("{api} success"),
            Err(code) => println!("{api} error={code}"),
        }
        result.is_ok()
    }

    fn wide_path(call: impl FnOnce(*mut u16, u32) -> u32) -> Result<Vec<u16>, u32> {
        // Includes the terminator for the maximum Win32 extended path length.
        let mut buffer = vec![0; 32_768];
        let length = call(buffer.as_mut_ptr(), buffer.len() as u32);
        if length == 0 {
            // SAFETY: capture the calling thread's error before another API.
            return Err(unsafe { GetLastError() });
        }
        if length as usize >= buffer.len() {
            return Err(122); // ERROR_INSUFFICIENT_BUFFER; never inspect partial data.
        }
        buffer.truncate(length as usize + 1);
        Ok(buffer)
    }

    // Supplemental read-only queries, AFTER every original observation. Never
    // substitute a later successful lookup for the original DOS gate result.
    fn trace_dos_mapping(handle: &OwnedHandle) {
        let guid = wide_path(|buffer, size| unsafe {
            GetFinalPathNameByHandleW(handle.0, buffer, size, 1)
        });
        report("GetFinalPathNameByHandleW(GUID)", &guid);
        let opened = wide_path(|buffer, size| unsafe {
            GetFinalPathNameByHandleW(handle.0, buffer, size, 8)
        });
        report("GetFinalPathNameByHandleW(DOS, opened)", &opened);
        for drive in ['C', 'D'] {
            let name = [drive as u16, ':' as u16, 0];
            let lookup =
                wide_path(|buffer, size| unsafe { QueryDosDeviceW(name.as_ptr(), buffer, size) });
            report(&format!("QueryDosDeviceW({drive}:)"), &lookup);
        }
        let devices = wide_path(|buffer, size| unsafe { QueryDosDeviceW(null(), buffer, size) });
        report("QueryDosDeviceW(enumerate)", &devices);
        if let Ok(guid) = guid {
            if let Some(end) = guid.iter().position(|value| *value == '}' as u16) {
                if guid.get(end + 1) == Some(&('\\' as u16)) {
                    let mut volume = guid[..end + 2].to_vec();
                    volume.push(0);
                    let mut paths = vec![0; 32_768];
                    let mut returned = 0;
                    // SAFETY: volume is terminated and both output pointers are valid.
                    let ok = unsafe {
                        GetVolumePathNamesForVolumeNameW(
                            volume.as_ptr(),
                            paths.as_mut_ptr(),
                            paths.len() as u32,
                            &mut returned,
                        )
                    };
                    let result = if ok == 0 {
                        Err(unsafe { GetLastError() })
                    } else {
                        Ok(())
                    };
                    report("GetVolumePathNamesForVolumeNameW(cwd volume)", &result);
                }
            }
        }
        let after = wide_path(|buffer, size| unsafe {
            GetFinalPathNameByHandleW(handle.0, buffer, size, 0)
        });
        report("GetFinalPathNameByHandleW(DOS, after queries)", &after);
    }

    pub(super) fn run(args: &Arguments) -> bool {
        // SAFETY: wide_path supplies the stated writable UTF-16 capacity.
        let cwd = wide_path(|buffer, size| unsafe { GetCurrentDirectoryW(size, buffer) });
        report("GetCurrentDirectoryW", &cwd);
        let Ok(cwd) = cwd else { return false };
        let cwd_drive = cwd
            .first()
            .and_then(|value| char::from_u32(u32::from(*value)))
            .unwrap_or('?')
            .to_ascii_uppercase();
        let workspace_drive = args
            .allowed
            .to_string_lossy()
            .chars()
            .next()
            .unwrap_or('?')
            .to_ascii_uppercase();
        println!("cwd-drive={cwd_drive} workspace-drive={workspace_drive}");
        if cwd_drive != workspace_drive {
            eprintln!("cwd-workspace-drive mismatch");
            return false;
        }
        // SAFETY: cwd is NUL-terminated; no security/template pointers are used.
        // Zero desired access still permits querying a directory's final name.
        let handle = unsafe { CreateFileW(cwd.as_ptr(), 0, 7, null(), 3, 0x0200_0000, null_mut()) };
        let opened = if handle == INVALID_HANDLE {
            // SAFETY: this immediately follows the failed CreateFileW call.
            Err(unsafe { GetLastError() })
        } else {
            Ok(OwnedHandle(handle))
        };
        report("CreateFileW(cwd)", &opened);
        let Ok(handle) = opened else { return false };
        // SAFETY: handle remains open and wide_path supplies each output buffer.
        let dos = wide_path(|buffer, size| unsafe {
            GetFinalPathNameByHandleW(handle.0, buffer, size, 0)
        });
        let dos_ok = report("GetFinalPathNameByHandleW(DOS)", &dos);
        let nt = wide_path(|buffer, size| unsafe {
            GetFinalPathNameByHandleW(handle.0, buffer, size, 2)
        });
        let nt_ok = report("GetFinalPathNameByHandleW(NT)", &nt);
        // Diagnostic fallback only: parent traversal requirements can differ.
        let long =
            wide_path(|buffer, size| unsafe { GetLongPathNameW(cwd.as_ptr(), buffer, size) });
        report("GetLongPathNameW", &long);

        let allowed =
            std::fs::read(&args.allowed).map_err(|error| error.raw_os_error().unwrap_or(31) as u32);
        let read_ok = report("fs::read(workspace-marker)", &allowed);
        let contents = allowed
            .as_ref()
            .map(|bytes| bytes.as_slice() == MARKER)
            .unwrap_or(false);
        report(
            "workspace-marker-contents",
            &contents.then_some(()).ok_or(13),
        );
        let denied = std::fs::File::open(&args.denied)
            .map_err(|error| error.raw_os_error().unwrap_or(31) as u32);
        report("File::open(sibling-marker)", &denied);
        let denial_ok = matches!(denied, Err(5)); // ERROR_ACCESS_DENIED, not absence.
        let manager: Vec<u16> = r"\\?\GLOBALROOT\Device\MountPointManager"
            .encode_utf16()
            .chain([0])
            .collect();
        let open_manager = |access| {
            // SAFETY: fixed NUL-terminated name; no IOCTL is ever sent.
            let raw = unsafe { CreateFileW(manager.as_ptr(), access, 7, null(), 3, 0, null_mut()) };
            if raw == INVALID_HANDLE {
                // SAFETY: capture the failed open's error before any other API.
                Err(unsafe { GetLastError() })
            } else {
                Ok(OwnedHandle(raw))
            }
        };
        // Metadata rights only: READ_CONTROL, SYNCHRONIZE, READ_EA, READ_ATTRIBUTES.
        let metadata = open_manager(0x0012_0088);
        let metadata_ok = report("CreateFileW(mount-manager-metadata)", &metadata);
        let manager_read = open_manager(0x8000_0000); // GENERIC_READ includes data access.
        report("CreateFileW(mount-manager-read)", &manager_read);
        let manager_denied = matches!(manager_read, Err(5));
        if args.trace_dos {
            trace_dos_mapping(&handle);
        }
        nt_ok
            && read_ok
            && contents
            && denial_ok
            && manager_denied
            && (!args.require_dos || (dos_ok && metadata_ok))
    }
}
