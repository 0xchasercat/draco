//! Non-Linux degraded path.
//!
//! The seccomp / Landlock / namespace layers are Linux-only. On macOS (dev) and
//! any other platform we still want the crate to compile and the runtime to
//! *function*, so the child runs **un-jailed** with a loud warning. This is a
//! development affordance only; production extraction runs on Linux with the full
//! jail (canonical spec §7, "Platforms").

use draco_types::JailKind;

use crate::{payload, JailError, JailHandle};

/// Un-jailed supervisor spawn: this platform has no namespaces/seccomp/Landlock,
/// so no sandbox can be established.
///
/// We deliberately return an error rather than silently handing back an
/// unprotected [`JailHandle`]: a dev-platform caller must *explicitly* opt into
/// an un-jailed Tier 2 run, and it never happens by accident. The child-side
/// [`run_jail_child`] still runs the payload un-jailed (with a warning) when the
/// binary is entered as `draco __jail`, which is the intended macOS dev path.
pub fn spawn_jail() -> Result<JailHandle, JailError> {
    eprintln!(
        "draco-jail: WARNING — no seccomp/Landlock/netns support on this platform; the sandbox \
         cannot be established. Use Linux (kernel >= 5.13, unprivileged user namespaces) for the \
         real jail."
    );
    Err(JailError::new(
        JailKind::NamespaceSetup,
        "jail unsupported on this platform; caller must opt into an un-jailed Tier 2 run",
    ))
}

/// Un-jailed child entry: run the payload loop over fd 3 with no lockdown.
pub fn run_jail_child() -> ! {
    eprintln!(
        "draco-jail: WARNING — jailed child running UN-JAILED (no seccomp/Landlock/netns on this \
         platform)."
    );
    match payload::run_child_over_fd3() {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("draco-jail: child exiting on error: {e}");
            std::process::exit(1);
        }
    }
}
