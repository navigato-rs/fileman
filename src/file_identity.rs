//! File identity checks before an overwrite. Paths alone miss hard links.

use std::{fs, io, path};

pub(super) fn same_file(source: &path::Path, destination: &path::Path) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let source = fs::metadata(source)?;
        let destination = match fs::metadata(destination) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        Ok(source.dev() == destination.dev() && source.ino() == destination.ino())
    }
    #[cfg(windows)]
    {
        Ok(Some(identity(source)?)
            == match identity(destination) {
                Ok(id) => Some(id),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let source = fs::canonicalize(source)?;
        match fs::canonicalize(destination) {
            Ok(destination) => Ok(source == destination),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}

#[cfg(windows)]
fn identity(path: &path::Path) -> io::Result<(u32, u32, u32)> {
    use std::os::windows::{fs::OpenOptionsExt as _, io::AsRawHandle as _};
    use windows_sys::Win32::Storage::FileSystem as win;

    // Query metadata without requiring read access to the file's contents.
    let file = fs::OpenOptions::new()
        .read(true)
        .access_mode(0)
        .custom_flags(win::FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    // SAFETY: this POD structure is initialized and writable; File owns a live
    // handle for the duration of the call.
    let mut info: win::BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { win::GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow,
    ))
}
