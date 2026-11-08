//! Makes and signs update manifests (see `src/update.rs`).
//!
//!   release_sign genkey <key.pk8>                    a new Ed25519 key (PKCS#8); prints the public key
//!   release_sign pubkey <key.pk8>                    the public key, as `update_key.in` wants it
//!   release_sign manifest <version> <file>...        update.json for these files (to stdout)
//!   release_sign sign <key.pk8> <update.json>        writes update.json.sig
//!   release_sign verify <update.json> [<sig>]        checks it against the keys built in
//!
//! The release workflow passes the key as base64 in `UPDATE_SIGNING_KEY`;
//! `sign -` reads it from there instead of a file.

use quickseek_remote::update::{self, FileEntry, Manifest};
use ring::signature::{Ed25519KeyPair, KeyPair};

fn main() {
    if let Err(e) = run() {
        eprintln!("release_sign: {e}");
        std::process::exit(1);
    }
}

fn key_bytes(path: &str) -> Result<Vec<u8>, String> {
    if path == "-" {
        let b64 =
            std::env::var("UPDATE_SIGNING_KEY").map_err(|_| "UPDATE_SIGNING_KEY isn't set")?;
        return unbase64(b64.trim()).ok_or_else(|| "UPDATE_SIGNING_KEY isn't base64".into());
    }
    std::fs::read(path).map_err(|e| format!("{path}: {e}"))
}

fn load(path: &str) -> Result<Ed25519KeyPair, String> {
    Ed25519KeyPair::from_pkcs8(&key_bytes(path)?)
        .map_err(|e| format!("{path}: not an Ed25519 key ({e})"))
}

fn show_public(k: &Ed25519KeyPair) -> String {
    let bytes: Vec<String> = k
        .public_key()
        .as_ref()
        .iter()
        .map(|b| format!("0x{b:02x}"))
        .collect();
    format!("[{}]", bytes.join(", "))
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let a = |i: usize| {
        args.get(i)
            .map(String::as_str)
            .ok_or("missing argument (see the source for usage)")
    };
    match a(0)? {
        "genkey" => {
            let out = a(1)?;
            if std::path::Path::new(out).exists() {
                return Err(format!("{out} exists; not overwriting a key"));
            }
            let pk8 = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
                .map_err(|e| e.to_string())?;
            std::fs::write(out, pk8.as_ref()).map_err(|e| format!("{out}: {e}"))?;
            println!("{}", show_public(&load(out)?));
        }
        "pubkey" => println!("{}", show_public(&load(a(1)?)?)),
        "base64" => println!("{}", base64(&key_bytes(a(1)?)?)),
        "manifest" => {
            let version = a(1)?;
            if update::parse_version(version).is_none() {
                return Err(format!("{version}: not a version like 19.10.0"));
            }
            let mut files = Vec::new();
            for p in &args[2..] {
                let data = std::fs::read(p).map_err(|e| format!("{p}: {e}"))?;
                let name = std::path::Path::new(p)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                files.push(FileEntry {
                    name,
                    sha256: update::sha256_hex(&data),
                    size: data.len() as u64,
                });
            }
            let m = Manifest {
                product: update::PRODUCT.into(),
                version: version.trim_start_matches('v').into(),
                files,
            };
            println!("{}", serde_json::to_string_pretty(&m).unwrap());
        }
        "sign" => {
            let k = load(a(1)?)?;
            let path = a(2)?;
            let data = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
            let sig = k.sign(&data);
            std::fs::write(format!("{path}.sig"), sig.as_ref()).map_err(|e| e.to_string())?;
            // Refuse to finish with a key the builds don't trust.
            update::verify(&data, sig.as_ref()).map_err(|e| {
                format!("signed, but the result doesn't verify with the built-in keys: {e}")
            })?;
            println!("wrote {path}.sig");
        }
        "verify" => {
            let path = a(1)?;
            let data = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
            let sig_path = args.get(2).cloned().unwrap_or(format!("{path}.sig"));
            let sig = std::fs::read(&sig_path).map_err(|e| format!("{sig_path}: {e}"))?;
            let m = update::verify(&data, &sig)?;
            println!("ok: {} {} ({} files)", m.product, m.version, m.files.len());
        }
        c => return Err(format!("unknown command {c}")),
    }
    Ok(())
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64(d: &[u8]) -> String {
    let mut s = String::new();
    for c in d.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                s.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

fn unbase64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for ch in s.bytes().filter(|b| !b.is_ascii_whitespace()) {
        if ch == b'=' {
            break;
        }
        acc = acc << 6 | B64.iter().position(|&x| x == ch)? as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}
