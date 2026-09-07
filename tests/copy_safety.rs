use std::{fs, io, path, sync::atomic};

struct Temp(path::PathBuf);

impl Temp {
    fn new() -> Self {
        static NEXT: atomic::AtomicU64 = atomic::AtomicU64::new(0);
        loop {
            let id = NEXT.fetch_add(1, atomic::Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("fileman-copy-safety-{}-{id}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create test directory: {error}"),
            }
        }
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn copying_to_a_hard_link_preserves_both_names() {
    let dir = Temp::new();
    let source = dir.0.join("note");
    let destination = dir.0.join("out");
    fs::create_dir(&destination).unwrap();
    fs::write(&source, b"keep these bytes").unwrap();
    let alias = destination.join("note");
    fs::hard_link(&source, &alias).unwrap();

    let error = fileman::core::copy_recursively(&source, &destination).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(fs::read(&source).unwrap(), b"keep these bytes");
    assert_eq!(fs::read(&alias).unwrap(), b"keep these bytes");
}

#[test]
fn ordinary_overwrite_still_works() {
    let dir = Temp::new();
    let source = dir.0.join("note");
    let destination = dir.0.join("out");
    fs::create_dir(&destination).unwrap();
    fs::write(&source, b"new bytes").unwrap();
    fs::write(destination.join("note"), b"old bytes").unwrap();

    fileman::core::copy_recursively(&source, &destination).unwrap();
    assert_eq!(fs::read(&source).unwrap(), b"new bytes");
    assert_eq!(fs::read(destination.join("note")).unwrap(), b"new bytes");
}

#[cfg(unix)]
#[test]
fn copying_a_dangling_symlink_still_works() {
    let dir = Temp::new();
    let source = dir.0.join("link");
    let destination = dir.0.join("out");
    fs::create_dir(&destination).unwrap();
    std::os::unix::fs::symlink("absent", &source).unwrap();

    fileman::core::copy_recursively(&source, &destination).unwrap();
    assert_eq!(
        fs::read_link(destination.join("link")).unwrap(),
        path::Path::new("absent")
    );
}

#[cfg(unix)]
#[test]
fn copying_to_a_symlink_to_a_hard_link_preserves_source() {
    let dir = Temp::new();
    let source = dir.0.join("note");
    let destination = dir.0.join("out");
    fs::create_dir(&destination).unwrap();
    fs::write(&source, b"keep these bytes").unwrap();
    fs::hard_link(&source, dir.0.join("alias")).unwrap();
    std::os::unix::fs::symlink("../alias", destination.join("note")).unwrap();

    let error = fileman::core::copy_recursively(&source, &destination).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(fs::read(&source).unwrap(), b"keep these bytes");
}
