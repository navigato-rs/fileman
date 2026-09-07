//! An exclusively created staging directory, removed when its job finishes.

use std::{fs, io, path, sync::atomic};

pub(super) struct TempDir(path::PathBuf);

impl TempDir {
    pub(super) fn new() -> io::Result<Self> {
        static NEXT: atomic::AtomicU64 = atomic::AtomicU64::new(0);
        for _ in 0..128 {
            let id = NEXT.fetch_add(1, atomic::Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("fileman-upload-{}-{id}", std::process::id()));
            #[cfg_attr(not(unix), allow(unused_mut))]
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt as _;
                builder.mode(0o700);
            }
            match builder.create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "cannot allocate staging directory",
        ))
    }

    pub(super) fn path(&self) -> &path::Path {
        &self.0
    }

    pub(super) fn child(&self, name: &str) -> io::Result<path::PathBuf> {
        let name = path::Path::new(name);
        if name.file_name() != Some(name.as_os_str()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid staging filename",
            ));
        }
        Ok(self.0.join(name))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            log::debug!("staging cleanup failed: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_do_not_share_staging_and_cleanup_is_scoped() {
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        assert_ne!(first.path(), second.path());
        fs::write(first.child("data").unwrap(), b"first").unwrap();
        fs::write(second.child("data").unwrap(), b"second").unwrap();
        let path = first.path().to_owned();
        drop(first);
        assert!(!path.exists());
        assert_eq!(fs::read(second.child("data").unwrap()).unwrap(), b"second");
    }

    #[test]
    fn rejects_paths_in_place_of_a_filename() {
        let dir = TempDir::new().unwrap();
        for name in ["", ".", "..", "../escape", "nested/file", "/absolute"] {
            assert!(dir.child(name).is_err(), "accepted {name:?}");
        }
        assert!(dir.child("file with spaces").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn staging_is_private_on_unix() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = TempDir::new().unwrap();
        assert_eq!(
            fs::metadata(dir.path()).unwrap().permissions().mode() & 0o077,
            0
        );
    }
}
