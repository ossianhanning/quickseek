//! Document text through the IFilter Windows Search uses for a file type:
//! Word, Excel and PowerPoint documents with the filter Windows ships,
//! PDFs when a PDF reader has installed one. Used by [`crate::content`].
//!
//! Filters run in this process with the searching user's rights. They are
//! the same components Explorer's search loads, but a buggy third-party
//! filter can still crash the caller; [`crate::content::ContentOptions::ifilter`]
//! turns this off.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use windows::Win32::Storage::IndexServer::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CLSIDFromString, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, STGM_READ, STGM_SHARE_DENY_NONE};
use windows::Win32::UI::Shell::PropertiesSystem::IInitializeWithStream;
use windows::Win32::UI::Shell::SHCreateStreamOnFileEx;
use windows::core::{HSTRING, IUnknown, Interface, PWSTR};

use crate::content::Budget;

pub enum Extracted {
    Text(String),
    /// No filter is registered for this type.
    NoFilter,
    Failed,
    /// Cancelled or out of budget.
    Unchecked,
}

thread_local! {
    // Filters need COM on the calling thread; content threads live for the process.
    static COM: () = unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    };
}

/// The filter for `path`: LoadIFilter (filters that open files themselves),
/// else the type's registered filter initialised with a stream (Windows'
/// own PDF filter only takes streams).
fn load(path: &str) -> Option<IFilter> {
    let mut raw = std::ptr::null_mut();
    if unsafe { LoadIFilter(&HSTRING::from(path), None, &mut raw) }.is_ok() && !raw.is_null() {
        return unsafe { IUnknown::from_raw(raw) }.cast::<IFilter>().ok();
    }
    let ext = std::path::Path::new(path).extension()?.to_string_lossy().to_ascii_lowercase();
    let handler = reg_default(&format!(r".{ext}\PersistentHandler"))?;
    let clsid = reg_default(&format!(r"CLSID\{handler}\PersistentAddinsRegistered\{{89BCB740-6119-101A-BCB7-00DD010655AF}}"))?;
    unsafe {
        let clsid = CLSIDFromString(&HSTRING::from(clsid)).ok()?;
        let filter: IFilter = CoCreateInstance(&clsid, None, CLSCTX_INPROC_SERVER).ok()?;
        let init = filter.cast::<IInitializeWithStream>().ok()?;
        let stream = SHCreateStreamOnFileEx(&HSTRING::from(path), (STGM_READ | STGM_SHARE_DENY_NONE).0, 0, false, None).ok()?;
        init.Initialize(&stream, STGM_READ.0).ok()?;
        Some(filter)
    }
}

/// The default value of an `HKEY_CLASSES_ROOT` key.
fn reg_default(key: &str) -> Option<String> {
    use windows_sys::Win32::System::Registry::{HKEY_CLASSES_ROOT, RRF_RT_REG_SZ, RegGetValueW};
    let k = crate::ntfs::win::wide(key);
    let mut buf = [0u16; 128];
    let mut n = (buf.len() * 2) as u32;
    let r = unsafe { RegGetValueW(HKEY_CLASSES_ROOT, k.as_ptr(), std::ptr::null(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr() as _, &mut n) };
    (r == 0).then(|| String::from_utf16_lossy(&buf[..(n as usize / 2).saturating_sub(1)]))
}

/// The text of a document, up to about `max_bytes` of it.
pub fn extract(path: &str, max_bytes: u64, budget: &Budget, cancel: &AtomicBool) -> Extracted {
    COM.with(|_| ());
    // The filter reads the file itself: count the whole file.
    let len = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(_) => return Extracted::Failed,
    };
    if len > max_bytes {
        return Extracted::Failed;
    }
    if !budget.take(len) {
        return Extracted::Unchecked;
    }
    let Some(filter) = load(path) else { return Extracted::NoFilter };
    let init = IFILTER_INIT_CANON_PARAGRAPHS.0 | IFILTER_INIT_HARD_LINE_BREAKS.0 | IFILTER_INIT_CANON_SPACES.0 | IFILTER_INIT_APPLY_INDEX_ATTRIBUTES.0;
    let mut flags = 0u32;
    if unsafe { filter.Init(init as u32, &[], &mut flags) } < 0 {
        return Extracted::Failed;
    }
    let mut text = String::new();
    let mut buf = vec![0u16; 8192];
    'chunks: loop {
        if cancel.load(Relaxed) {
            return Extracted::Unchecked;
        }
        let mut stat = STAT_CHUNK::default();
        let hr = unsafe { filter.GetChunk(&mut stat) };
        if hr == FILTER_E_END_OF_CHUNKS.0 {
            break;
        }
        if hr < 0 {
            if hr == FILTER_E_EMBEDDING_UNAVAILABLE.0 || hr == FILTER_E_LINK_UNAVAILABLE.0 {
                continue;
            }
            break;
        }
        if stat.flags.0 & CHUNK_TEXT.0 == 0 {
            continue;
        }
        if stat.breakType != CHUNK_NO_BREAK && !text.is_empty() {
            text.push('\n');
        }
        loop {
            let mut n = buf.len() as u32;
            let hr = unsafe { filter.GetText(&mut n, PWSTR(buf.as_mut_ptr())) };
            if hr < 0 {
                break; // FILTER_E_NO_MORE_TEXT, or a broken chunk
            }
            text.push_str(&String::from_utf16_lossy(&buf[..(n as usize).min(buf.len())]));
            if text.len() as u64 > max_bytes {
                break 'chunks;
            }
            if hr == FILTER_S_LAST_TEXT.0 {
                break;
            }
        }
    }
    Extracted::Text(text)
}
