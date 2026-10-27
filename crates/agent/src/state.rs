//! What the agent keeps in its config folder: its certificate and key, the
//! paired PCs (token hashes only) and the current pairing code.

use std::path::{Path, PathBuf};

use anyhow::Context;
use quickseek_remote::pair;
use serde::{Deserialize, Serialize};

/// A pairing code stays valid this long.
pub const CODE_MINUTES: i64 = 15;
/// Wrong proofs before a code is burnt.
pub const CODE_TRIES: u32 = 5;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Client {
    pub name: String,
    /// SHA-256 of the token, hex.
    pub token_sha256: String,
    /// Unix seconds.
    pub paired: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Code {
    pub code: String,
    /// Unix seconds.
    pub expires: i64,
    pub tries: u32,
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

pub struct Store {
    pub dir: PathBuf,
}

/// Writes a file only the agent's user can read, atomically.
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(tmp, path)
}

impl Store {
    pub fn new(dir: &Path) -> anyhow::Result<Store> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Store {
            dir: dir.to_path_buf(),
        })
    }

    pub fn history_dir(&self) -> PathBuf {
        self.dir.join("history")
    }

    /// The certificate and PKCS#8 key (DER), made on first use.
    pub fn identity(&self) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
        let (c, k) = (self.dir.join("cert.der"), self.dir.join("key.der"));
        if let (Ok(cert), Ok(key)) = (std::fs::read(&c), std::fs::read(&k)) {
            return Ok((cert, key));
        }
        let mut params =
            rcgen::CertificateParams::new(vec![quickseek_remote::tls::SERVER_NAME.to_string()])?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "Quickseek agent");
        // Pinned, never expires in practice: 100 years.
        params.not_after = rcgen::date_time_ymd(2125, 1, 1);
        let key = rcgen::KeyPair::generate()?;
        let cert = params.self_signed(&key)?;
        let (cert, key) = (cert.der().to_vec(), key.serialize_der());
        write_private(&k, &key)?;
        write_private(&c, &cert)?;
        log::info!("made a new certificate in {}", self.dir.display());
        Ok((cert, key))
    }

    pub fn fingerprint(&self) -> anyhow::Result<[u8; 32]> {
        Ok(quickseek_remote::tls::fingerprint(&self.identity()?.0))
    }

    pub fn clients(&self) -> Vec<Client> {
        std::fs::read(self.dir.join("clients.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn save_clients(&self, list: &[Client]) -> anyhow::Result<()> {
        write_private(
            &self.dir.join("clients.json"),
            serde_json::to_string_pretty(list)?.as_bytes(),
        )?;
        Ok(())
    }

    /// The paired PC with this token, if any.
    pub fn client_for(&self, token: &[u8; 32]) -> Option<Client> {
        let h = pair::hex(&pair::sha256(token));
        self.clients()
            .into_iter()
            .find(|c| pair::same(c.token_sha256.as_bytes(), h.as_bytes()))
    }

    /// Adds a paired PC (replacing one of the same name); returns its token.
    pub fn add_client(&self, name: &str) -> anyhow::Result<[u8; 32]> {
        let token: [u8; 32] = pair::random();
        let mut list = self.clients();
        list.retain(|c| c.name != name);
        list.push(Client {
            name: name.to_string(),
            token_sha256: pair::hex(&pair::sha256(&token)),
            paired: now(),
        });
        self.save_clients(&list)?;
        Ok(token)
    }

    pub fn revoke(&self, name: &str) -> anyhow::Result<bool> {
        let mut list = self.clients();
        let n = list.len();
        list.retain(|c| c.name != name);
        self.save_clients(&list)?;
        Ok(list.len() != n)
    }

    pub fn code(&self) -> Option<Code> {
        let c: Code =
            serde_json::from_slice(&std::fs::read(self.dir.join("pairing.json")).ok()?).ok()?;
        (c.expires > now() && c.tries < CODE_TRIES).then_some(c)
    }

    /// Makes a new pairing code (replacing any other).
    pub fn new_code(&self) -> anyhow::Result<Code> {
        let c = Code {
            code: pair::new_code(),
            expires: now() + CODE_MINUTES * 60,
            tries: 0,
        };
        write_private(
            &self.dir.join("pairing.json"),
            serde_json::to_string(&c)?.as_bytes(),
        )?;
        Ok(c)
    }

    /// Checks a pairing proof against the current code. A wrong one counts a
    /// try; the right one uses the code up. Returns the code on success.
    pub fn check_pairing(
        &self,
        proof: &[u8; 32],
        fp: &[u8; 32],
        nonce: &[u8; 16],
    ) -> Result<String, String> {
        let path = self.dir.join("pairing.json");
        let Some(mut c) = self.code() else {
            return Err("no pairing code is active: run `quickseek-agent pair` on the NAS".into());
        };
        if pair::same(&pair::pc_proof(&c.code, fp, nonce), proof) {
            let _ = std::fs::remove_file(&path);
            return Ok(c.code);
        }
        c.tries += 1;
        let _ = write_private(
            &path,
            serde_json::to_string(&c).unwrap_or_default().as_bytes(),
        );
        Err(if c.tries >= CODE_TRIES {
            "wrong pairing code; that code is now used up, make a new one".into()
        } else {
            "wrong pairing code".into()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(tag: &str) -> Store {
        let d = std::env::temp_dir().join(format!("qs-agent-store-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        Store::new(&d).unwrap()
    }

    #[test]
    fn identity_is_stable() {
        let s = store("id");
        let a = s.fingerprint().unwrap();
        assert_eq!(a, s.fingerprint().unwrap());
        std::fs::remove_dir_all(&s.dir).ok();
    }

    #[test]
    fn pairing_codes_burn_after_tries() {
        let s = store("code");
        let fp = [1u8; 32];
        let nonce = [2u8; 16];
        assert!(s.check_pairing(&[0; 32], &fp, &nonce).is_err());
        let c = s.new_code().unwrap();
        for _ in 0..CODE_TRIES {
            assert!(s.check_pairing(&[0; 32], &fp, &nonce).is_err());
        }
        // Even the right proof fails now.
        let right = pair::pc_proof(&c.code, &fp, &nonce);
        assert!(s.check_pairing(&right, &fp, &nonce).is_err());
        let c = s.new_code().unwrap();
        let right = pair::pc_proof(&c.code, &fp, &nonce);
        assert_eq!(s.check_pairing(&right, &fp, &nonce), Ok(c.code));
        // Used up.
        assert!(s.check_pairing(&right, &fp, &nonce).is_err());
        std::fs::remove_dir_all(&s.dir).ok();
    }

    #[test]
    fn clients_and_tokens() {
        let s = store("clients");
        let t = s.add_client("PC").unwrap();
        assert_eq!(s.client_for(&t).unwrap().name, "PC");
        assert!(s.client_for(&[0; 32]).is_none());
        let t2 = s.add_client("PC").unwrap();
        assert!(
            s.client_for(&t).is_none(),
            "re-pairing replaces the old token"
        );
        assert!(s.client_for(&t2).is_some());
        assert!(s.revoke("PC").unwrap());
        assert!(s.client_for(&t2).is_none());
        std::fs::remove_dir_all(&s.dir).ok();
    }
}
