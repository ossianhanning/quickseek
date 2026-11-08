//! Signed updates, shared by the Windows service and the NAS agent.
//!
//! Every release carries `update.json` (the version and the SHA-256 and size
//! of each file) and `update.json.sig`, an Ed25519 signature of those exact
//! bytes made with the project's update key (kept as a release-workflow
//! secret). Builds carry the public half ([`PUBLIC_KEYS`]); an update is used
//! only if the manifest verifies against one of them, is newer than the
//! running build, and the downloaded file matches the manifest's hash and
//! size. TLS only gets the bytes here; trust comes from the signature.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Where releases are published.
pub const REPO: &str = "ossianhanning/quickseek";

/// Public update keys (Ed25519, raw 32 bytes). A manifest signed by any of
/// them is accepted; a new key is added here a release before the old one
/// stops being used.
pub const PUBLIC_KEYS: &[[u8; 32]] = &[include!("update_key.in")];

pub const MANIFEST: &str = "update.json";
pub const SIGNATURE: &str = "update.json.sig";
/// The product a manifest must name (a signed manifest of anything else is refused).
pub const PRODUCT: &str = "quickseek";

/// Largest file an update downloads (the installer is ~10 MB).
const MAX_FILE: usize = 256 << 20;
const MAX_MANIFEST: usize = 1 << 20;
const TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub product: String,
    pub version: String,
    pub files: Vec<FileEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    /// Hex.
    pub sha256: String,
    pub size: u64,
}

impl Manifest {
    pub fn file(&self, name: &str) -> Option<&FileEntry> {
        self.files.iter().find(|f| f.name == name)
    }

    /// The first file whose name starts with `prefix` and ends with `suffix`.
    pub fn find(&self, prefix: &str, suffix: &str) -> Option<&FileEntry> {
        self.files
            .iter()
            .find(|f| f.name.starts_with(prefix) && f.name.ends_with(suffix))
    }
}

/// Parses `manifest` if `sig` is a valid signature of it by one of `keys`.
pub fn verify_with(manifest: &[u8], sig: &[u8], keys: &[[u8; 32]]) -> Result<Manifest, String> {
    use ring::signature::{ED25519, UnparsedPublicKey};
    if !keys.iter().any(|k| {
        UnparsedPublicKey::new(&ED25519, k)
            .verify(manifest, sig)
            .is_ok()
    }) {
        return Err("the update's signature doesn't match the update key".into());
    }
    let m: Manifest =
        serde_json::from_slice(manifest).map_err(|e| format!("the update manifest: {e}"))?;
    if m.product != PRODUCT {
        return Err(format!("the update manifest is for {:?}", m.product));
    }
    if parse_version(&m.version).is_none() {
        return Err(format!("the update manifest's version {:?}", m.version));
    }
    Ok(m)
}

/// [`verify_with`] against the keys built in.
pub fn verify(manifest: &[u8], sig: &[u8]) -> Result<Manifest, String> {
    verify_with(manifest, sig, PUBLIC_KEYS)
}

/// `19.10.0` → (19, 10, 0); `None` unless it's three numbers.
pub fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = v.trim().trim_start_matches('v').split('.');
    let p = (
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
    );
    it.next().is_none().then_some(p)
}

/// Whether version `a` is newer than `b` (unparseable is never newer).
pub fn newer(a: &str, b: &str) -> bool {
    match (parse_version(a), parse_version(b)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

pub fn sha256_hex(data: &[u8]) -> String {
    crate::pair::hex(ring::digest::digest(&ring::digest::SHA256, data).as_ref())
}

/// Whether `data` is the file the manifest describes.
pub fn check_file(data: &[u8], f: &FileEntry) -> Result<(), String> {
    if data.len() as u64 != f.size {
        return Err(format!(
            "{}: {} bytes, the manifest says {}",
            f.name,
            data.len(),
            f.size
        ));
    }
    if !sha256_hex(data).eq_ignore_ascii_case(&f.sha256) {
        return Err(format!(
            "{}: its SHA-256 doesn't match the manifest",
            f.name
        ));
    }
    Ok(())
}

/// Where release files come from: GitHub, or `QUICKSEEK_UPDATE_URL` (a
/// folder URL holding `update.json`, `update.json.sig` and the files; for
/// testing, plain http is allowed to loopback addresses).
fn base_override() -> Option<String> {
    std::env::var("QUICKSEEK_UPDATE_URL")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
}

fn manifest_url(name: &str) -> String {
    match base_override() {
        Some(b) => format!("{b}/{name}"),
        None => format!("https://github.com/{REPO}/releases/latest/download/{name}"),
    }
}

fn file_url(version: &str, name: &str) -> String {
    match base_override() {
        Some(b) => format!("{b}/{name}"),
        None => format!("https://github.com/{REPO}/releases/download/v{version}/{name}"),
    }
}

/// The newest release's manifest, verified: (manifest bytes, signature, parsed).
pub fn fetch_latest() -> Result<(Vec<u8>, Vec<u8>, Manifest), String> {
    let raw = get(&manifest_url(MANIFEST), MAX_MANIFEST)?;
    // The signature from that release's own folder: GitHub's `latest/download`
    // answers 504 for `.sig` files. The version read here only picks the
    // folder; nothing in the manifest counts until the signature verifies.
    #[derive(Deserialize)]
    struct Unverified {
        version: String,
    }
    let version = serde_json::from_slice::<Unverified>(&raw)
        .map_err(|e| format!("the update manifest: {e}"))?
        .version;
    if parse_version(&version).is_none() {
        return Err(format!("the update manifest's version {version:?}"));
    }
    let sig = get(&file_url(&version, SIGNATURE), 1024)
        .or_else(|_| get(&manifest_url(SIGNATURE), 1024))?;
    let m = verify(&raw, &sig)?;
    if m.version != version {
        return Err("the update manifest changed while it was read".into());
    }
    Ok((raw, sig, m))
}

/// Downloads one file of `m` and checks it against the manifest.
pub fn download(m: &Manifest, f: &FileEntry) -> Result<Vec<u8>, String> {
    if f.size as usize > MAX_FILE {
        return Err(format!("{}: too big for an update", f.name));
    }
    let data = get(&file_url(&m.version, &f.name), f.size as usize + 1)?;
    check_file(&data, f)?;
    Ok(data)
}

// ---- a small HTTPS GET -------------------------------------------------------------

struct Url {
    tls: bool,
    host: String,
    port: u16,
    path: String,
}

fn parse_url(u: &str) -> Result<Url, String> {
    let (tls, rest) = if let Some(r) = u.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = u.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(format!("not a web address: {u}"));
    };
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(']') || hostport.starts_with('[') => (
            h.to_string(),
            p.parse().map_err(|_| format!("bad port in {u}"))?,
        ),
        _ => (hostport.to_string(), if tls { 443 } else { 80 }),
    };
    if !tls && !matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]") {
        return Err(format!("{u}: updates are only fetched over https"));
    }
    Ok(Url {
        tls,
        host,
        port,
        path: path.to_string(),
    })
}

fn tls_config() -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth(),
    )
}

/// GETs `url`, following redirects; at most `max` bytes of body.
pub fn get(url: &str, max: usize) -> Result<Vec<u8>, String> {
    let mut url = url.to_string();
    for _ in 0..6 {
        match get_once(&url, max)? {
            Response::Body(b) => return Ok(b),
            Response::Redirect(to) => {
                url = if to.starts_with("http") {
                    to
                } else {
                    let u = parse_url(&url)?;
                    format!(
                        "{}://{}:{}{to}",
                        if u.tls { "https" } else { "http" },
                        u.host,
                        u.port
                    )
                };
            }
        }
    }
    Err(format!("{url}: too many redirects"))
}

enum Response {
    Body(Vec<u8>),
    Redirect(String),
}

fn get_once(url: &str, max: usize) -> Result<Response, String> {
    let u = parse_url(url)?;
    let e = |e: std::io::Error| format!("{}: {e}", u.host);
    let addr = std::net::ToSocketAddrs::to_socket_addrs(&(u.host.trim_matches(['[', ']']), u.port))
        .map_err(e)?
        .next()
        .ok_or_else(|| format!("{}: no address", u.host))?;
    let sock = TcpStream::connect_timeout(&addr, TIMEOUT).map_err(e)?;
    sock.set_read_timeout(Some(TIMEOUT)).map_err(e)?;
    sock.set_write_timeout(Some(TIMEOUT)).map_err(e)?;
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: quickseek-updater/{}\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        u.path,
        u.host,
        env!("CARGO_PKG_VERSION")
    );
    let raw = if u.tls {
        let name = rustls::pki_types::ServerName::try_from(u.host.clone())
            .map_err(|_| format!("bad host name {}", u.host))?;
        let conn = rustls::ClientConnection::new(tls_config(), name)
            .map_err(|e| format!("{}: {e}", u.host))?;
        let mut s = rustls::StreamOwned::new(conn, sock);
        s.write_all(req.as_bytes()).map_err(e)?;
        read_all(&mut s, max)?
    } else {
        let mut s = sock;
        s.write_all(req.as_bytes()).map_err(e)?;
        read_all(&mut s, max)?
    };
    parse_response(&raw, url, max)
}

/// Whether `raw` holds a whole response (servers don't always close the
/// connection after one, `Connection: close` or not).
fn complete(raw: &[u8]) -> bool {
    let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
        return false;
    };
    let head = String::from_utf8_lossy(&raw[..end]).to_ascii_lowercase();
    let body = &raw[end + 4..];
    if head
        .split_whitespace()
        .nth(1)
        .is_some_and(|s| s.starts_with('3') || s == "204" || s == "304")
        && !head.contains("content-length:")
    {
        return true;
    }
    for l in head.split("\r\n") {
        if let Some(v) = l.strip_prefix("content-length:") {
            return v.trim().parse::<usize>().is_ok_and(|n| body.len() >= n);
        }
        if l.starts_with("transfer-encoding:") && l.contains("chunked") {
            return body.ends_with(b"0\r\n\r\n") && dechunk(body).is_ok();
        }
    }
    false
}

fn read_all(r: &mut impl Read, max: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if out.len() > max + 64 * 1024 {
                    return Err("the download is bigger than expected".into());
                }
                if complete(&out) {
                    break;
                }
            }
            // Servers that close without a TLS close_notify.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(format!("reading the download: {e}")),
        }
    }
    Ok(out)
}

fn parse_response(raw: &[u8], url: &str, max: usize) -> Result<Response, String> {
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| format!("{url}: no response"))?;
    let head = String::from_utf8_lossy(&raw[..end]);
    let body = &raw[end + 4..];
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("{url}: bad response"))?;
    let mut location = None;
    let mut chunked = false;
    let mut length = None;
    for l in lines {
        let Some((k, v)) = l.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        match k.as_str() {
            "location" => location = Some(v.to_string()),
            "transfer-encoding" => chunked = v.to_ascii_lowercase().contains("chunked"),
            "content-length" => length = v.parse::<usize>().ok(),
            _ => {}
        }
    }
    if (300..400).contains(&status) {
        return location
            .map(Response::Redirect)
            .ok_or_else(|| format!("{url}: redirect without a location"));
    }
    if status != 200 {
        return Err(if status == 404 {
            format!("{url}: not found (no release has it yet?)")
        } else {
            format!("{url}: HTTP {status}")
        });
    }
    let body = if chunked {
        dechunk(body)?
    } else {
        let b = match length {
            Some(n) if n <= body.len() => &body[..n],
            Some(_) => return Err(format!("{url}: the download was cut short")),
            None => body,
        };
        b.to_vec()
    };
    if body.len() > max {
        return Err(format!("{url}: bigger than expected"));
    }
    Ok(Response::Body(body))
}

fn dechunk(mut b: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let nl = b
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or("bad chunked response")?;
        let size_s = String::from_utf8_lossy(&b[..nl]);
        let size = usize::from_str_radix(size_s.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| "bad chunk size")?;
        b = &b[nl + 2..];
        if size == 0 {
            return Ok(out);
        }
        if b.len() < size + 2 {
            return Err("the download was cut short".into());
        }
        out.extend_from_slice(&b[..size]);
        b = &b[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    fn key() -> Ed25519KeyPair {
        let rng = ring::rand::SystemRandom::new();
        let pk8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        Ed25519KeyPair::from_pkcs8(pk8.as_ref()).unwrap()
    }

    fn manifest(v: &str) -> Vec<u8> {
        serde_json::to_vec(&Manifest {
            product: PRODUCT.into(),
            version: v.into(),
            files: vec![FileEntry {
                name: "a.exe".into(),
                sha256: sha256_hex(b"hello"),
                size: 5,
            }],
        })
        .unwrap()
    }

    #[test]
    fn only_signed_manifests_are_accepted() {
        let k = key();
        let pubk: [u8; 32] = k.public_key().as_ref().try_into().unwrap();
        let m = manifest("20.1.0");
        let sig = k.sign(&m);
        let got = verify_with(&m, sig.as_ref(), &[pubk]).unwrap();
        assert_eq!(got.version, "20.1.0");
        // Another key, a changed byte, a truncated signature: refused.
        let other: [u8; 32] = key().public_key().as_ref().try_into().unwrap();
        assert!(verify_with(&m, sig.as_ref(), &[other]).is_err());
        let mut bad = m.clone();
        *bad.last_mut().unwrap() = b' ';
        assert!(verify_with(&bad, sig.as_ref(), &[pubk]).is_err());
        assert!(verify_with(&m, &sig.as_ref()[..63], &[pubk]).is_err());
        // A signed manifest of another product.
        let other_product = br#"{"product":"x","version":"1.0.0","files":[]}"#;
        assert!(verify_with(other_product, k.sign(other_product).as_ref(), &[pubk]).is_err());
    }

    #[test]
    fn files_are_checked_against_the_manifest() {
        let m: Manifest = serde_json::from_slice(&manifest("1.0.0")).unwrap();
        let f = m.file("a.exe").unwrap();
        assert!(check_file(b"hello", f).is_ok());
        assert!(check_file(b"hellO", f).is_err());
        assert!(check_file(b"hello!", f).is_err());
    }

    #[test]
    fn versions_compare_numerically() {
        assert!(newer("19.10.0", "19.9.0"));
        assert!(newer("v20.0.0", "19.99.99"));
        assert!(!newer("19.9.0", "19.9.0"));
        assert!(!newer("19.9", "1.0.0"));
        assert!(!newer("garbage", "1.0.0"));
    }

    /// Over the internet: the newest release's manifest, verified with the
    /// built-in key.
    #[test]
    #[ignore = "network"]
    fn the_latest_release_verifies() {
        let (_, _, m) = fetch_latest().unwrap();
        assert!(parse_version(&m.version).is_some());
        assert!(m.find("quickseek-setup-", "-x64.exe").is_some());
    }

    /// Over the internet: a release file through GitHub's redirect to its CDN.
    #[test]
    #[ignore = "network"]
    fn github_release_files_download() {
        let url =
            format!("https://github.com/{REPO}/releases/download/v19.9.0/quickseek-19.9.0.mcpb");
        let b = get(&url, 2 << 20).unwrap();
        assert_eq!(b.len(), 971_247);
        // A file a release doesn't have.
        assert!(
            get(
                &format!("https://github.com/{REPO}/releases/download/v19.9.0/nope"),
                1024
            )
            .is_err()
        );
    }

    #[test]
    fn responses_are_parsed() {
        let r = parse_response(
            b"HTTP/1.1 302 Found\r\nLocation: https://x/y\r\n\r\n",
            "u",
            10,
        )
        .unwrap();
        assert!(matches!(r, Response::Redirect(l) if l == "https://x/y"));
        let r = parse_response(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n",
            "u",
            10,
        )
        .unwrap();
        assert!(matches!(r, Response::Body(b) if b == b"abcde"));
        let r = parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi", "u", 10).unwrap();
        assert!(matches!(r, Response::Body(b) if b == b"hi"));
        assert!(parse_response(b"HTTP/1.1 404 Not Found\r\n\r\n", "u", 10).is_err());
        assert!(parse_url("http://example.com/x").is_err());
        assert!(parse_url("http://127.0.0.1:8080/x").is_ok());
    }
}
