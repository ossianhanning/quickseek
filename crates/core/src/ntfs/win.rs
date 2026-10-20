//! Thin, safe-ish wrappers over the handful of Win32 calls we need.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::ffi::c_void;
use std::io;
use std::ops::{Deref, DerefMut};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, GetLogicalDrives, GetVolumeInformationW, OPEN_EXISTING, ReadFile,
};
use windows_sys::Win32::System::IO::{DeviceIoControl, OVERLAPPED};

pub const ERROR_HANDLE_EOF: i32 = 38;
pub const ERROR_JOURNAL_DELETE_IN_PROGRESS: i32 = 1178;
pub const ERROR_JOURNAL_NOT_ACTIVE: i32 = 1179;
pub const ERROR_JOURNAL_ENTRY_DELETED: i32 = 1181;
pub const ERROR_ACCESS_DENIED: i32 = 5;

/// Owned Win32 handle.
pub struct Handle(pub HANDLE);

// Volume handles are used from one thread at a time, but moving them between
// threads is fine.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn create(path: &str, access: u32, flags: u32) -> io::Result<Handle> {
    let path = wide(path);
    let h = unsafe {
        CreateFileW(
            path.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            OPEN_EXISTING,
            flags,
            null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        Ok(Handle(h))
    }
}

/// Opens `\\.\C:` for raw reads / FSCTLs. Requires administrator rights.
pub fn open_volume(letter: char, flags: u32) -> io::Result<Handle> {
    create(&format!("\\\\.\\{letter}:"), GENERIC_READ, flags)
        .or_else(|_| create(&format!("\\\\.\\{letter}:"), GENERIC_READ | GENERIC_WRITE, flags))
}

/// Opens `\\.\PhysicalDriveN` for raw reads. Requires administrator rights.
pub fn open_physical_drive(n: u32, flags: u32) -> io::Result<Handle> {
    create(&format!(r"\\.\PhysicalDrive{n}"), GENERIC_READ, flags)
}

const IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: u32 = 0x0056_0000;

/// (disk number, byte offset on that disk) of a single-extent volume.
pub fn volume_disk_extent(h: &Handle) -> io::Result<(u32, u64)> {
    // VOLUME_DISK_EXTENTS with room for a few extents.
    let mut buf = [0u64; 1 + 3 * 4];
    unsafe { ioctl(h, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, null(), 0, buf.as_mut_ptr() as *mut _, size_of_val(&buf))? };
    let count = buf[0] as u32;
    if count != 1 {
        return Err(io::Error::other(format!("volume spans {count} disk extents")));
    }
    Ok((buf[1] as u32, buf[2]))
}

/// Opens the root directory of a volume; used as the "volume hint" for OpenFileById.
pub fn open_root_dir(letter: char) -> io::Result<Handle> {
    create(&format!("{letter}:\\"), 0, FILE_FLAG_BACKUP_SEMANTICS)
}

/// DeviceIoControl returning the number of output bytes.
///
/// # Safety
/// `inp`/`out` must point to buffers of the given sizes that match `code`.
pub unsafe fn ioctl(
    h: &Handle,
    code: u32,
    inp: *const c_void,
    in_len: usize,
    out: *mut c_void,
    out_len: usize,
) -> io::Result<u32> {
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            h.0,
            code,
            inp,
            in_len as u32,
            out,
            out_len as u32,
            &mut returned,
            null_mut(),
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(returned)
    }
}

/// Positional read on a synchronous handle (the OVERLAPPED only carries the offset).
pub fn read_at(h: &Handle, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
    ov.Anonymous.Anonymous.Offset = offset as u32;
    ov.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
    let mut read = 0u32;
    let ok = unsafe {
        ReadFile(
            h.0,
            buf.as_mut_ptr(),
            buf.len() as u32,
            &mut read,
            &mut ov,
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(read as usize)
    }
}

pub fn read_exact_at(h: &Handle, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
    while !buf.is_empty() {
        let n = read_at(h, offset, buf)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        offset += n as u64;
        buf = &mut buf[n..];
    }
    Ok(())
}

/// Page-aligned heap buffer, required for FILE_FLAG_NO_BUFFERING reads.
pub struct AlignedBuf {
    ptr: *mut u8,
    len: usize,
}

unsafe impl Send for AlignedBuf {}

impl AlignedBuf {
    pub const ALIGN: usize = 4096;

    pub fn new(len: usize) -> Self {
        let len = len.max(Self::ALIGN).next_multiple_of(Self::ALIGN);
        let ptr = unsafe { alloc_zeroed(Layout::from_size_align(len, Self::ALIGN).unwrap()) };
        assert!(!ptr.is_null(), "out of memory");
        AlignedBuf { ptr, len }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr, Layout::from_size_align(self.len, Self::ALIGN).unwrap()) };
    }
}

impl Deref for AlignedBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

#[derive(Debug, Clone)]
pub struct VolumeInfo {
    pub letter: char,
    pub fs: String,
    pub serial: u32,
    /// GetDriveTypeW: 2 removable, 3 fixed, 4 remote, 5 cdrom, 6 ramdisk.
    pub drive_type: u32,
}

pub const DRIVE_REMOVABLE: u32 = 2;
pub const DRIVE_FIXED: u32 = 3;

/// 128-bit file id of a volume's root directory (FileIdInfo).
pub fn root_file_id(letter: char) -> io::Result<u128> {
    use windows_sys::Win32::Storage::FileSystem::{FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx};
    let h = open_root_dir(letter)?;
    let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandleEx(h.0, FileIdInfo, &mut info as *mut _ as *mut _, size_of::<FILE_ID_INFO>() as u32) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(u128::from_le_bytes(info.FileId.Identifier))
}

/// Bit mask of present drive letters (bit 0 = A:).
pub fn logical_drives() -> u32 {
    unsafe { GetLogicalDrives() }
}

/// All lettered volumes, with their filesystem name.
pub fn list_volumes() -> Vec<VolumeInfo> {
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root = wide(&format!("{letter}:\\"));
        let mut fs = [0u16; 32];
        let mut serial = 0u32;
        let ok = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                null_mut(),
                0,
                &mut serial,
                null_mut(),
                null_mut(),
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        };
        if ok != 0 {
            let n = fs.iter().position(|&c| c == 0).unwrap_or(fs.len());
            let drive_type = unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(root.as_ptr()) };
            out.push(VolumeInfo {
                letter,
                fs: String::from_utf16_lossy(&fs[..n]),
                serial,
                drive_type,
            });
        }
    }
    out
}

pub fn is_elevated() -> bool {
    // Cheapest reliable check: try to open a volume handle.
    list_volumes()
        .iter()
        .find(|v| v.fs == "NTFS")
        .map(|v| open_volume(v.letter, 0).is_ok())
        .unwrap_or(false)
}

/// An impersonation token (a pipe client's identity), used by the service to
/// reach network shares with the user's credentials instead of LocalSystem's.
pub struct Token(HANDLE);

unsafe impl Send for Token {}
unsafe impl Sync for Token {}

impl Drop for Token {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

impl Token {
    /// Captures the identity of the client connected to `pipe`.
    pub fn from_pipe_client(pipe: &std::fs::File) -> Option<Token> {
        use std::os::windows::io::AsRawHandle;
        let pipe = pipe.as_raw_handle();
        use windows_sys::Win32::Security::{
            DuplicateTokenEx, RevertToSelf, SecurityImpersonation, TOKEN_DUPLICATE, TOKEN_IMPERSONATE, TOKEN_QUERY,
            TokenImpersonation,
        };
        use windows_sys::Win32::System::Pipes::ImpersonateNamedPipeClient;
        use windows_sys::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};
        unsafe {
            if ImpersonateNamedPipeClient(pipe) == 0 {
                return None;
            }
            let mut t: HANDLE = null_mut();
            let ok = OpenThreadToken(GetCurrentThread(), TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_IMPERSONATE, 1, &mut t);
            RevertToSelf();
            if ok == 0 {
                return None;
            }
            let mut dup: HANDLE = null_mut();
            let ok = DuplicateTokenEx(t, TOKEN_QUERY | TOKEN_IMPERSONATE | TOKEN_DUPLICATE, null(), SecurityImpersonation, TokenImpersonation, &mut dup);
            CloseHandle(t);
            (ok != 0).then_some(Token(dup))
        }
    }

    /// The current thread acts as this user until [`Token::revert`].
    pub fn impersonate(&self) -> bool {
        unsafe { windows_sys::Win32::System::Threading::SetThreadToken(null(), self.0) != 0 }
    }

    pub fn revert() {
        unsafe { windows_sys::Win32::Security::RevertToSelf() };
    }
}

/// Mapped drive paths (`Z:\data`) only exist in the user's session; turn them
/// into UNC (`\server\share\data`) so the service can reach them.
pub fn universal_path(path: &str) -> String {
    use windows_sys::Win32::NetworkManagement::WNet::{UNIVERSAL_NAME_INFO_LEVEL, UNIVERSAL_NAME_INFOW, WNetGetUniversalNameW};
    let w = wide(path);
    let mut buf = vec![0u64; 512];
    let mut size = (buf.len() * 8) as u32;
    let rc = unsafe { WNetGetUniversalNameW(w.as_ptr(), UNIVERSAL_NAME_INFO_LEVEL, buf.as_mut_ptr() as *mut _, &mut size) };
    if rc != 0 {
        return path.to_string();
    }
    let info = unsafe { &*(buf.as_ptr() as *const UNIVERSAL_NAME_INFOW) };
    let p = info.lpUniversalName;
    if p.is_null() {
        return path.to_string();
    }
    let mut n = 0;
    unsafe {
        while *p.add(n) != 0 {
            n += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
    }
}

/// UNC paths (`\server\share`) — indexed as network folders.
pub fn is_network_path(path: &str) -> bool {
    path.starts_with(r"\\") && !path.starts_with(r"\\?\") && !path.starts_with(r"\\.\")
}
