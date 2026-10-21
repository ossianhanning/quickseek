//! USN change journal: how the index stays current without rescanning.
//!
//! NTFS appends a record for every create/delete/rename/modify to
//! `$Extend\$UsnJrnl`. We remember the journal id and the next USN alongside
//! the saved index, so on startup we only replay what happened while we were
//! not running, and afterwards we block in FSCTL_READ_USN_JOURNAL
//! (`BytesToWaitFor = 1`) so the kernel wakes us the moment something changes.

use std::io;

use windows_sys::Win32::System::Ioctl::{
    FSCTL_CREATE_USN_JOURNAL, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL,
};

use super::record::u64le;
use super::usn::{UsnRecord, parse_records};
use super::win::{self, Handle};

#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
pub struct UsnJournalDataV0 {
    pub journal_id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
    pub lowest_valid_usn: i64,
    pub max_usn: i64,
    pub maximum_size: u64,
    pub allocation_delta: u64,
}

#[repr(C)]
struct ReadUsnJournalDataV1 {
    start_usn: i64,
    reason_mask: u32,
    return_only_on_close: u32,
    timeout: u64,
    bytes_to_wait_for: u64,
    journal_id: u64,
    min_major_version: u16,
    max_major_version: u16,
}

#[repr(C)]
struct CreateUsnJournalData {
    maximum_size: u64,
    allocation_delta: u64,
}

#[derive(Debug)]
pub enum JournalError {
    /// Our saved position was overwritten (journal wrapped) or the journal was
    /// recreated: the index must be rebuilt from a fresh scan.
    Gap,
    Io(io::Error),
}

impl From<io::Error> for JournalError {
    fn from(e: io::Error) -> Self {
        match e.raw_os_error() {
            Some(win::ERROR_JOURNAL_ENTRY_DELETED)
            | Some(win::ERROR_JOURNAL_NOT_ACTIVE)
            | Some(win::ERROR_JOURNAL_DELETE_IN_PROGRESS) => JournalError::Gap,
            _ => JournalError::Io(e),
        }
    }
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JournalError::Gap => write!(f, "USN journal position lost"),
            JournalError::Io(e) => write!(f, "{e}"),
        }
    }
}

pub fn query(h: &Handle) -> io::Result<UsnJournalDataV0> {
    let mut d = UsnJournalDataV0::default();
    unsafe {
        win::ioctl(
            h,
            FSCTL_QUERY_USN_JOURNAL,
            std::ptr::null(),
            0,
            &mut d as *mut _ as *mut _,
            size_of::<UsnJournalDataV0>(),
        )?;
    }
    Ok(d)
}

/// Queries the journal, creating it (32 MiB, like Windows' default) if the
/// volume has none.
pub fn query_or_create(h: &Handle) -> io::Result<UsnJournalDataV0> {
    match query(h) {
        Ok(d) => Ok(d),
        Err(e) if e.raw_os_error() == Some(win::ERROR_JOURNAL_NOT_ACTIVE) => {
            log::warn!("volume has no USN journal; creating one");
            let c = CreateUsnJournalData {
                maximum_size: 32 << 20,
                allocation_delta: 8 << 20,
            };
            unsafe {
                win::ioctl(
                    h,
                    FSCTL_CREATE_USN_JOURNAL,
                    &c as *const _ as *const _,
                    size_of::<CreateUsnJournalData>(),
                    std::ptr::null_mut(),
                    0,
                )?;
            }
            query(h)
        }
        Err(e) => Err(e),
    }
}

pub struct Journal {
    h: Handle,
    pub journal_id: u64,
    pub next_usn: i64,
    buf: Vec<u64>,
}

impl Journal {
    /// Opens the journal on `letter` positioned at `start_usn` (from a saved
    /// index), or at its current end when `None`.
    pub fn open(
        letter: char,
        journal_id: Option<u64>,
        start_usn: Option<i64>,
    ) -> Result<Self, JournalError> {
        let h = win::open_volume(letter, 0)?;
        let d = query_or_create(&h)?;
        if let Some(id) = journal_id
            && id != d.journal_id
        {
            return Err(JournalError::Gap);
        }
        let next_usn = match start_usn {
            Some(u) if u < d.first_usn || u < d.lowest_valid_usn => return Err(JournalError::Gap),
            Some(u) => u,
            None => d.next_usn,
        };
        Ok(Journal {
            h,
            journal_id: d.journal_id,
            next_usn,
            buf: vec![0u64; 1 << 16],
        })
    }

    /// Reads the next batch of records. With `wait`, blocks until at least one
    /// record exists. Returns an empty batch when caught up (non-waiting).
    pub fn read(&mut self, wait: bool) -> Result<Vec<UsnRecord>, JournalError> {
        let q = ReadUsnJournalDataV1 {
            start_usn: self.next_usn,
            reason_mask: 0xFFFF_FFFF,
            return_only_on_close: 0,
            timeout: 0,
            bytes_to_wait_for: if wait { 1 } else { 0 },
            journal_id: self.journal_id,
            min_major_version: 2,
            max_major_version: 3,
        };
        let n = unsafe {
            win::ioctl(
                &self.h,
                FSCTL_READ_USN_JOURNAL,
                &q as *const _ as *const _,
                size_of::<ReadUsnJournalDataV1>(),
                self.buf.as_mut_ptr() as *mut _,
                self.buf.len() * 8,
            )?
        } as usize;
        let bytes: &[u8] = bytemuck::cast_slice(&self.buf);
        let mut out = Vec::new();
        if n >= 8 {
            self.next_usn = u64le(bytes, 0) as i64;
            parse_records(&bytes[8..n], |r| out.push(r));
        }
        Ok(out)
    }

    /// Reads everything up to the current end of the journal.
    pub fn drain(&mut self) -> Result<Vec<UsnRecord>, JournalError> {
        let mut all = Vec::new();
        loop {
            let before = self.next_usn;
            let batch = self.read(false)?;
            let empty = batch.is_empty();
            all.extend(batch);
            if empty || self.next_usn == before {
                return Ok(all);
            }
        }
    }
}
