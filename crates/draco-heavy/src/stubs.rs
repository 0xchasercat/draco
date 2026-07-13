//! Box-dependent components intentionally deferred to later design slices.

use std::path::PathBuf;

/// STUB: per-slot Linux network namespace and tun2socks owner.
///
/// TODO(design build step 3): install fail-closed routes, proxy DNS, carry TCP
/// and UDP through the mutable pipe, and gate jobs on the TCP+QUIC leak probe.
#[derive(Debug)]
pub struct NetworkNamespace {
    pub slot_id: usize,
}

/// STUB: per-slot source-keyed SOCKS5 relay mapping.
///
/// TODO(design build step 3): implement CONNECT and UDP ASSOCIATE with atomic,
/// per-slot upstream swaps and blackhole-on-clear behavior.
#[derive(Debug)]
pub struct SocksRelay {
    pub slot_id: usize,
}

/// STUB: patchright worker supervising stock headed Chrome.
///
/// TODO(design build step 4): launch persistent channel=chrome profiles under
/// the discovered display/render command and expose mint IPC to the daemon.
#[derive(Debug)]
pub struct BrowserWorker {
    pub slot_id: usize,
    pub profile_dir: PathBuf,
}
