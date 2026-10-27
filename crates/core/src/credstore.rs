//! User names and passwords for network folders, kept by the service.
//!
//! The file lives next to the service's config (`%ProgramData%\quickseek`),
//! encrypted with DPAPI under the service's own account (LocalSystem), so
//! only that account can read the passwords back; the file itself allows
//! only SYSTEM and Administrators. Passwords never go into the config,
//! the log, `status` or MCP output.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use serde::{Deserialize, Serialize};
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};

/// SYSTEM, Administrators and the creating account (the service), not inherited.
const FILE_SDDL: &str = "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;OW)";

/// A password: zeroed when dropped, never printed.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: String) -> Secret {
        Secret(s)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_empty() { "\"\"" } else { "\"***\"" })
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // SAFETY: zeros are valid UTF-8.
        unsafe { self.0.as_mut_vec().fill(0) };
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Login {
    pub user: String,
    pub password: Secret,
}

/// Logins by folder path (case-folded).
pub type Logins = BTreeMap<String, Login>;

pub fn key(path: &str) -> String {
    crate::folder::root_name(path).to_lowercase()
}

/// Where the service keeps them: beside `config`.
pub fn path_beside(config: &Path) -> PathBuf {
    config.with_file_name("credentials.bin")
}

pub fn load(path: &Path) -> Logins {
    let Ok(blob) = std::fs::read(path) else {
        return Logins::new();
    };
    match unprotect(&blob).and_then(|plain| {
        let r = serde_json::from_slice(&plain).map_err(io::Error::other);
        zero(plain);
        r
    }) {
        Ok(l) => l,
        Err(e) => {
            log::warn!(
                "{}: can't read the stored folder logins: {e}",
                path.display()
            );
            Logins::new()
        }
    }
}

pub fn save(path: &Path, logins: &Logins) -> io::Result<()> {
    if logins.is_empty() {
        return match std::fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let plain = serde_json::to_vec(logins).map_err(io::Error::other)?;
    let blob = protect(&plain);
    zero(plain);
    let blob = blob?;
    let tmp = path.with_extension("tmp");
    let mut f = create_private(&tmp)?;
    f.write_all(&blob)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)
}

fn zero(mut v: Vec<u8>) {
    v.fill(0);
}

/// Creates (or truncates) `path` with [`FILE_SDDL`] as its security.
fn create_private(path: &Path) -> io::Result<std::fs::File> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_ALWAYS, CreateFileW, FILE_ATTRIBUTE_NORMAL,
    };
    let _ = std::fs::remove_file(path); // CREATE_ALWAYS keeps an old file's security
    let sd = crate::ipc::Sd::new(FILE_SDDL)?;
    let sa = sd.attrs();
    let w = crate::ntfs::win::wide(&path.to_string_lossy());
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            GENERIC_WRITE,
            0,
            &sa,
            CREATE_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { std::fs::File::from_raw_handle(h) })
}

fn protect(plain: &[u8]) -> io::Result<Vec<u8>> {
    crypt(plain, true)
}

fn unprotect(blob: &[u8]) -> io::Result<Vec<u8>> {
    crypt(blob, false)
}

fn crypt(data: &[u8], encrypt: bool) -> io::Result<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    };
    let mut out = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: null_mut(),
    };
    let ok = unsafe {
        if encrypt {
            CryptProtectData(
                &input,
                null(),
                null(),
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        } else {
            CryptUnprotectData(
                &input,
                null_mut(),
                null(),
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        }
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let v = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
    unsafe {
        std::ptr::write_bytes(out.pbData, 0, out.cbData as usize);
        LocalFree(out.pbData as _);
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logins_round_trip_encrypted() {
        let dir = std::env::temp_dir().join(format!("quickseek-cred-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials.bin");
        let mut l = Logins::new();
        l.insert(
            key(r"\\nas\data"),
            Login {
                user: "nasuser".into(),
                password: Secret::new("correct horse".into()),
            },
        );
        save(&path, &l).unwrap();
        let raw = std::fs::read(&path).unwrap();
        assert!(
            !raw.windows(13).any(|w| w == b"correct horse"),
            "stored in the clear"
        );
        assert_eq!(load(&path), l);
        // Removing the last one removes the file.
        save(&path, &Logins::new()).unwrap();
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn secrets_dont_print() {
        let l = Login {
            user: "u".into(),
            password: Secret::new("hunter2".into()),
        };
        assert!(!format!("{l:?}").contains("hunter2"));
    }
}
