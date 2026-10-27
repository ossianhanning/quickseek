//! The protocol between `quickseek-agent` (on a NAS) and the Quickseek
//! service on Windows. See `docs/design/nas-agent.md`.
//!
//! * [`wire`]: messages and framing (a `u32` length, then the body), the
//!   same conventions as the service's named-pipe IPC.
//! * [`tls`]: TLS 1.3 with the agent's self-signed certificate pinned by
//!   SHA-256 fingerprint on the PC.
//! * [`client`]: the PC's side of hello, pairing and authentication.
//! * [`pair`]: pairing codes, the proofs that bind a code to the certificate,
//!   and tokens.

pub mod client;
pub mod pair;
pub mod tls;
pub mod wire;

pub use rustls;

/// TCP port the agent listens on by default.
pub const DEFAULT_PORT: u16 = 48623;
/// Newest protocol version this build speaks.
pub const PROTOCOL: u8 = 1;
/// Oldest protocol version this build speaks.
pub const MIN_PROTOCOL: u8 = 1;

/// Root entry id (like NTFS record 5).
pub const ROOT_ID: u32 = 5;
/// First id given to an entry below the root (like NTFS's first user record).
pub const FIRST_ID: u32 = 24;

pub const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
pub const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// 100 ns intervals between 1601-01-01 and 1970-01-01.
pub const FILETIME_UNIX_EPOCH: i64 = 116_444_736_000_000_000;

/// Unix seconds + nanoseconds as a Windows FILETIME.
pub fn filetime(secs: i64, nanos: u32) -> i64 {
    secs.saturating_mul(10_000_000)
        .saturating_add(nanos as i64 / 100)
        .saturating_add(FILETIME_UNIX_EPOCH)
}

/// A `SystemTime` as a FILETIME (0 if unknown).
pub fn filetime_of(t: std::io::Result<std::time::SystemTime>) -> i64 {
    let Ok(t) = t else { return 0 };
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => filetime(d.as_secs() as i64, d.subsec_nanos()),
        Err(e) => {
            let d = e.duration();
            filetime(-(d.as_secs() as i64), 0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_epoch() {
        assert_eq!(filetime(0, 0), FILETIME_UNIX_EPOCH);
        assert_eq!(filetime(1, 500), FILETIME_UNIX_EPOCH + 10_000_005);
    }
}
