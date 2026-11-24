//! Directory listing that cannot take the process down.
//!
//! `std::fs::read_dir` aborts the process when closing the directory fails,
//! and the WebAssembly host in VS Code reports exactly that for some
//! directories (its virtual root among them). On wasm the listing therefore
//! goes through `fd_readdir` directly and a failing close is ignored; natively
//! it is the standard library's.

use std::io;
use std::path::Path;

/// One entry of a directory, as the file system reports it (symbolic links
/// are not followed, as with `std::fs::DirEntry::file_type`).
pub(crate) struct DirEntry {
    pub(crate) name: String,
    pub(crate) is_dir: bool,
    pub(crate) is_file: bool,
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn read_dir(path: &Path) -> io::Result<Vec<DirEntry>> {
    Ok(std::fs::read_dir(path)?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let file_type = entry.file_type().ok()?;
            Some(DirEntry {
                name: entry.file_name().to_str()?.to_string(),
                is_dir: file_type.is_dir(),
                is_file: file_type.is_file(),
            })
        })
        .collect())
}

#[cfg(target_family = "wasm")]
pub(crate) fn read_dir(path: &Path) -> io::Result<Vec<DirEntry>> {
    use std::ffi::CString;
    use std::os::wasi::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    // SAFETY: `c_path` is a valid NUL-terminated string.
    let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let entries = list(fd as wasi::Fd);
    // The host may refuse to close a directory it opened; nothing depends on
    // that succeeding, so the error is dropped instead of ending the process.
    // SAFETY: `fd` came from `open` above and is closed exactly once.
    let _ = unsafe { libc::close(fd) };
    entries
}

#[cfg(target_family = "wasm")]
fn list(fd: wasi::Fd) -> io::Result<Vec<DirEntry>> {
    const DIRENT_SIZE: usize = std::mem::size_of::<wasi::Dirent>();
    let mut entries = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut cookie: wasi::Dircookie = 0;
    loop {
        // SAFETY: `buf` is a valid writable buffer of `buf.len()` bytes.
        let used = unsafe { wasi::fd_readdir(fd, buf.as_mut_ptr(), buf.len(), cookie) }
            .map_err(|err| io::Error::from_raw_os_error(err.raw() as i32))?;
        let mut offset = 0;
        let mut progressed = false;
        while offset + DIRENT_SIZE <= used {
            // SAFETY: a complete `Dirent` header lies at `offset`; it is read
            // unaligned because entries are packed back to back.
            let dirent: wasi::Dirent =
                unsafe { (buf.as_ptr().add(offset) as *const wasi::Dirent).read_unaligned() };
            let name_start = offset + DIRENT_SIZE;
            let name_end = name_start + dirent.d_namlen as usize;
            if name_end > used {
                // The name was cut off at the end of the buffer; the next
                // read starts at this entry again.
                break;
            }
            if let Ok(name) = std::str::from_utf8(&buf[name_start..name_end]) {
                if name != "." && name != ".." {
                    entries.push(DirEntry {
                        name: name.to_string(),
                        is_dir: dirent.d_type == wasi::FILETYPE_DIRECTORY,
                        is_file: dirent.d_type == wasi::FILETYPE_REGULAR_FILE,
                    });
                }
            }
            cookie = dirent.d_next;
            offset = name_end;
            progressed = true;
        }
        if used < buf.len() {
            // The host had nothing more to report.
            return Ok(entries);
        }
        if !progressed {
            // One name larger than the whole buffer: make room and retry.
            let larger = buf.len() * 2;
            buf.resize(larger, 0);
        }
    }
}
