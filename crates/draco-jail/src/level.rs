//! Achieved sandbox level — the posture the child actually established, reported
//! back to the supervisor so it can surface a `runtime.sandbox` trace step.
//!
//! Two families:
//!
//! * **`hardened`** — the OS sandbox (seccomp) is engaged. This is the Linux
//!   default. The description lists the layers that took effect, e.g.
//!   `"hardened: seccomp+netns+landlock"` or, when a best-effort layer was
//!   unavailable, `"hardened: seccomp+landlock (no netns: userns unavailable)"`.
//! * **`isolate`** — V8 runs with no host-capability bindings but no OS sandbox
//!   is applied. This is the macOS default (`"isolate: v8 no host bindings
//!   (macos)"`) and the Linux `--no-jail` case. It is a normal, supported outcome,
//!   not a degraded/dev-only one.
//!
//! The description string is carried to the supervisor inside a `Log` frame whose
//! `msg` is prefixed with [`LEVEL_LOG_PREFIX`] (using an existing frozen frame
//! field rather than changing the wire contract).

/// Prefix on the `Log` frame `msg` that carries the achieved sandbox level, so the
/// supervisor can distinguish it from an ordinary diagnostic log and strip it
/// before recording the `runtime.sandbox` trace detail.
pub const LEVEL_LOG_PREFIX: &str = "sandbox:";

/// Whether the best-effort network-namespace layer was established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetnsStatus {
    /// User + network namespace entered — the child is off-box at the netns layer.
    Engaged,
    /// Unprivileged user namespaces are unavailable on this host.
    UsernsUnavailable,
    /// Userns worked but `CLONE_NEWNET` did not.
    NetnsFailed,
}

/// How much of the best-effort Landlock filesystem lockdown the kernel enforced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LandlockStatus {
    /// Fully enforced.
    Enforced,
    /// Partially enforced (older ABI downgrade under BestEffort).
    Partial,
    /// Not available (kernel < 5.13).
    Unavailable,
}

/// The achieved sandbox posture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxLevel {
    /// OS sandbox engaged (seccomp on). `strict` records which seccomp model.
    Hardened {
        strict: bool,
        netns: NetnsStatus,
        landlock: LandlockStatus,
    },
    /// V8-only isolation, no OS sandbox. `note` explains why (platform / flag).
    Isolate { note: &'static str },
}

impl SandboxLevel {
    /// Construct a `hardened` level from the layer outcomes.
    pub fn hardened(strict: bool, netns: NetnsStatus, landlock: LandlockStatus) -> Self {
        SandboxLevel::Hardened {
            strict,
            netns,
            landlock,
        }
    }

    /// `isolate` level for the Linux `--no-jail` case.
    pub fn isolate_no_jail() -> Self {
        SandboxLevel::Isolate {
            note: "v8 no host bindings (--no-jail)",
        }
    }

    /// `isolate` level for a platform with no OS sandbox (macOS et al.).
    pub fn isolate_macos() -> Self {
        SandboxLevel::Isolate {
            note: "v8 no host bindings (macos)",
        }
    }

    /// One-line, log-safe description for the `runtime.sandbox` trace detail.
    pub fn describe(&self) -> String {
        match self {
            SandboxLevel::Isolate { note } => format!("isolate: {note}"),
            SandboxLevel::Hardened {
                strict,
                netns,
                landlock,
            } => {
                // Engaged layers, in order.
                let mut engaged = vec!["seccomp"];
                if *netns == NetnsStatus::Engaged {
                    engaged.push("netns");
                }
                if matches!(landlock, LandlockStatus::Enforced | LandlockStatus::Partial) {
                    engaged.push("landlock");
                }

                // Parenthetical notes for best-effort layers that did not engage.
                let mut notes: Vec<&str> = Vec::new();
                match netns {
                    NetnsStatus::Engaged => {}
                    NetnsStatus::UsernsUnavailable => notes.push("no netns: userns unavailable"),
                    NetnsStatus::NetnsFailed => notes.push("no netns: netns setup failed"),
                }
                if *landlock == LandlockStatus::Unavailable {
                    notes.push("no landlock: kernel <5.13");
                }

                let seccomp_kind = if *strict { " (strict allowlist)" } else { "" };
                let base = format!("hardened: {}{seccomp_kind}", engaged.join("+"));
                if notes.is_empty() {
                    base
                } else {
                    format!("{base} ({})", notes.join("; "))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardened_all_layers() {
        let l = SandboxLevel::hardened(false, NetnsStatus::Engaged, LandlockStatus::Enforced);
        assert_eq!(l.describe(), "hardened: seccomp+netns+landlock");
    }

    #[test]
    fn hardened_no_netns_notes_reason() {
        let l = SandboxLevel::hardened(
            false,
            NetnsStatus::UsernsUnavailable,
            LandlockStatus::Enforced,
        );
        assert_eq!(
            l.describe(),
            "hardened: seccomp+landlock (no netns: userns unavailable)"
        );
    }

    #[test]
    fn hardened_seccomp_only_notes_both_reasons() {
        let l = SandboxLevel::hardened(
            false,
            NetnsStatus::UsernsUnavailable,
            LandlockStatus::Unavailable,
        );
        assert_eq!(
            l.describe(),
            "hardened: seccomp (no netns: userns unavailable; no landlock: kernel <5.13)"
        );
    }

    #[test]
    fn hardened_strict_is_labelled() {
        let l = SandboxLevel::hardened(true, NetnsStatus::Engaged, LandlockStatus::Enforced);
        assert_eq!(
            l.describe(),
            "hardened: seccomp+netns+landlock (strict allowlist)"
        );
    }

    #[test]
    fn isolate_variants() {
        assert_eq!(
            SandboxLevel::isolate_macos().describe(),
            "isolate: v8 no host bindings (macos)"
        );
        assert_eq!(
            SandboxLevel::isolate_no_jail().describe(),
            "isolate: v8 no host bindings (--no-jail)"
        );
    }
}
