//! The Git metadata reads a confined build is given: the objects bound at
//! session bootstrap or verified adoption, as agent-bridle held read roots.
//! The one `git_hardening` function that binds that type, in a child module
//! that a single `cfg` can gate (#3160).
use super::*;

/// Build metadata reads retain the objects acquired at session bootstrap or
/// verified adoption. Linux installs rules from these held descriptors.
/// Seatbelt remains path-only: verify before preparing the request, but a
/// replacement after that check can redirect its pathname-based profile.
/// No descriptor-isolation claim is made for Seatbelt; Windows gets no extra
/// metadata authority until equivalent held-object support is available. A build
/// without newt-core's `confine` feature has no executor to hold them: none (#3160).
pub(crate) fn build_git_reads(
    workspace: &Path,
) -> (Vec<String>, Vec<crate::sandbox_seam::HeldReadRoot>) {
    #[cfg(all(any(target_os = "linux", target_os = "macos"), feature = "confine"))]
    {
        let checked = || -> Option<_> {
            verified_identity(workspace)?;
            let identity = cached_identity(workspace)?;
            identity.held.verify().ok()?;
            let mut paths = Vec::new();
            #[allow(unused_mut)]
            let mut handles = Vec::new();
            for (path, root) in [
                (&identity.held.common_dir, &identity.held.common_root),
                (&identity.held.git_dir, &identity.held.git_root),
            ] {
                let path = path_to_string(path);
                // Constructor checks the path/fd pairing again; no second open.
                let held = agent_bridle::HeldReadRoot::bind(
                    path.clone(),
                    root.as_fd().try_clone_to_owned().ok()?,
                )
                .ok()?;
                paths.push(path);
                #[cfg(target_os = "linux")]
                handles.push(held);
                #[cfg(target_os = "macos")]
                let _ = held;
            }
            Some((paths, handles))
        };
        checked().unwrap_or_default()
    }
    #[cfg(not(all(any(target_os = "linux", target_os = "macos"), feature = "confine")))]
    {
        let _ = workspace;
        (Vec::new(), Vec::new())
    }
}
