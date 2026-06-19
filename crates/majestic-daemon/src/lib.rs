// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Majestic daemon — a headless session server over a local Unix socket (PRD #1 §6.8).
//!
//! `mj --daemon` runs a [`Daemon`] that owns a [`Session`](majestic_core::Session); `mj` (or
//! `mj attach`) connects over a Unix domain socket under a `0700` runtime directory. The daemon is
//! **local-only** — no TCP listener in v1, which keeps the GPL-vs-AGPL classification and the
//! privacy story trivial. Sessions are resurrected from disk on boot, so they survive a restart.
//!
//! This module is the protocol and session-owning core. The codec ([`read_frame`]/[`write_frame`])
//! is transport-agnostic; the Unix-socket serve loop and the client live alongside it.
//!
//! Part of [Majestic](https://Majestic.SpacecraftSoftware.org/) — Concept #1 (Rust + Steel).

use std::path::PathBuf;

mod daemon;
mod protocol;

pub use daemon::Daemon;
pub use protocol::{read_frame, write_frame, DaemonStatus, Request, Response};

/// The daemon's Unix socket path: `$XDG_RUNTIME_DIR/majestic/daemon.sock`, else a path under the
/// system temp directory. The serve loop creates the parent directory with `0700` permissions.
#[must_use]
pub fn socket_path() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
    base.join("majestic").join("daemon.sock")
}

#[cfg(test)]
mod tests {
    use super::socket_path;

    #[test]
    fn socket_path_lives_under_a_majestic_directory() {
        let path = socket_path();
        assert_eq!(path.file_name().unwrap(), "daemon.sock");
        assert_eq!(
            path.parent().unwrap().file_name().unwrap(),
            "majestic",
            "the socket sits in a per-app directory the server can chmod 0700"
        );
    }
}
