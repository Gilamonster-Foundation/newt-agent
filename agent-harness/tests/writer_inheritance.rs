//! Grounds writer ownership in a real inherited open file description.
//!
//! This executable deliberately has no libtest harness: it starts no threads,
//! so a forked child can drop a Session without inheriting another thread's
//! allocator locks. Read/write barriers establish every ownership transition;
//! socket timeouts only bound a broken fixture. No sleeps decide the result.

#[cfg(unix)]
mod unix {
    use agent_harness::{Error, Session, SessionConfig};
    use std::{
        io::{Read, Write},
        os::unix::{io::AsRawFd, net::UnixStream},
        time::Duration,
    };

    unsafe extern "C" {
        fn fork() -> i32;
        fn read(fd: i32, buffer: *mut u8, count: usize) -> isize;
        fn write(fd: i32, buffer: *const u8, count: usize) -> isize;
        fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
        fn _exit(status: i32) -> !;
    }

    struct Child {
        socket: UnixStream,
        pid: Option<i32>,
    }

    impl Child {
        fn fork(action: impl FnOnce() -> bool) -> (Self, bool) {
            let (parent, child) = UnixStream::pair().unwrap();
            for socket in [&parent, &child] {
                socket
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
            }
            // SAFETY: this harness-free executable is single-threaded. The
            // child may therefore drop owned Rust values before using only
            // raw POSIX I/O and _exit; no other thread holds an allocator lock.
            let pid = unsafe { fork() };
            assert!(pid >= 0, "fork failed");
            if pid == 0 {
                drop(parent);
                let ready = u8::from(action());
                let fd = child.as_raw_fd();
                // SAFETY: buffers remain valid for each one-byte syscall; fd
                // is owned by child. _exit avoids running inherited test state.
                unsafe {
                    if write(fd, &ready, 1) != 1 {
                        _exit(2);
                    }
                    let mut command = 0;
                    let valid = read(fd, &mut command, 1) == 1 && command == b'X';
                    _exit(if valid { 0 } else { 3 });
                }
            }
            drop(child);
            let mut process = Self {
                socket: parent,
                pid: Some(pid),
            };
            let mut ready = [0];
            process.socket.read_exact(&mut ready).unwrap();
            (process, ready[0] == 1)
        }

        fn finish(mut self) {
            self.socket.write_all(b"X").unwrap();
            let pid = self.pid.take().unwrap();
            let mut status = 0;
            // SAFETY: wait only for our own live child, writing a valid status.
            assert_eq!(unsafe { waitpid(pid, &mut status, 0) }, pid);
            assert_eq!(status, 0, "writer fixture child failed");
        }
    }

    impl Drop for Child {
        fn drop(&mut self) {
            if let Some(pid) = self.pid.take() {
                let _ = self.socket.write_all(b"X");
                let mut status = 0;
                // SAFETY: reap our child even if the parent assertion unwinds.
                let _ = unsafe { waitpid(pid, &mut status, 0) };
            }
        }
    }

    fn child_drop_cannot_unlock_the_live_parent() {
        let directory = tempfile::tempdir().unwrap();
        let mut owner = Some(Session::open(directory.path(), SessionConfig::default()).unwrap());
        let head = owner.as_ref().unwrap().head();
        let locator = owner.as_ref().unwrap().checkpoint_path().unwrap();
        let (child, refused) = Child::fork(|| {
            let mut inherited = owner.take().unwrap();
            let refused = matches!(inherited.ensure_writer(), Err(Error::Conflict(_)));
            drop(inherited);
            refused
        });
        let competitor = Session::restore(directory.path(), head, "local-session");
        let unchanged = std::fs::read_to_string(locator).unwrap().trim() == head.to_string();
        child.finish();
        assert!(
            refused,
            "an inherited Session must reject execution ownership"
        );
        assert!(
            matches!(competitor, Err(Error::Conflict(_))),
            "dropping the inherited Session must not unlock the live parent's lease"
        );
        assert!(unchanged, "a child drop must not admit a competing writer");
        drop(owner.take());
        assert!(Session::restore(directory.path(), head, "local-session").is_ok());
    }

    fn owner_drop_releases_despite_a_live_inherited_descriptor() {
        let directory = tempfile::tempdir().unwrap();
        let owner = Session::open(directory.path(), SessionConfig::default()).unwrap();
        let head = owner.head();
        let (child, held) = Child::fork(|| true);
        assert!(
            held,
            "the child must hold its inherited descriptor before release"
        );
        assert!(matches!(
            Session::restore(directory.path(), head, "local-session"),
            Err(Error::Conflict(_))
        ));
        drop(owner);
        // Capture the result while the forked child is still waiting. Releasing
        // the child before restoring would make this test vacuously pass.
        let restored = Session::restore(directory.path(), head, "local-session");
        child.finish();
        assert!(
            restored.is_ok(),
            "owner drop must release its lease despite the inherited descriptor: {:?}",
            restored.err()
        );
    }

    pub(super) fn run() {
        child_drop_cannot_unlock_the_live_parent();
        println!("test child_drop_cannot_unlock_the_live_parent ... ok");
        owner_drop_releases_despite_a_live_inherited_descriptor();
        println!("test owner_drop_releases_despite_a_live_inherited_descriptor ... ok");
    }
}

fn main() {
    #[cfg(unix)]
    unix::run();
    #[cfg(not(unix))]
    println!("writer inheritance tests require Unix fork; not exercised on this platform");
}
