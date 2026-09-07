use std::{
    fs,
    io::{self, Read as _, Write as _},
    path,
    sync::atomic,
    time,
};

use crate::{App, Payload, Preferences, Report};

pub(crate) const MAX_REPORTS: usize = 8;
const MAX_BYTES: u64 = 8192;
const MAX_AGE: time::Duration = time::Duration::from_secs(7 * 24 * 60 * 60);
static SEQUENCE: atomic::AtomicU64 = atomic::AtomicU64::new(0);

pub(crate) struct Storage {
    root: path::PathBuf,
    app: App,
    // One reporting owner per app. Other instances fail closed, never race pruning/consent.
    _lock: fs::File,
}

fn private_file(path: &path::Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)
}

fn read_bounded(path: &path::Path, limit: u64) -> io::Result<Vec<u8>> {
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let file = fs::File::open(path)?;
    if file.metadata()?.len() > limit {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(bytes)
}

impl Storage {
    pub(crate) fn open(root: path::PathBuf, app: App) -> io::Result<Self> {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
            builder.mode(0o700);
            builder.create(&root)?;
            if fs::symlink_metadata(&root)?.permissions().mode() & 0o077 != 0 {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
        }
        #[cfg(not(unix))]
        builder.create(&root)?;
        if !fs::symlink_metadata(&root)?.is_dir() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let lock_path = root.join("lock");
        let lock = match fs::symlink_metadata(&lock_path) {
            Ok(ref meta) if meta.is_file() => {
                fs::OpenOptions::new().write(true).open(&lock_path)?
            }
            Ok(_) => return Err(io::ErrorKind::InvalidData.into()),
            Err(ref error) if error.kind() == io::ErrorKind::NotFound => private_file(&lock_path)?,
            Err(error) => return Err(error),
        };
        lock.try_lock().map_err(io::Error::other)?;
        let storage = Self {
            root,
            app,
            _lock: lock,
        };
        storage.prune(MAX_REPORTS)?;
        storage.purge_disabled(storage.load_preferences())?;
        Ok(storage)
    }

    pub(crate) fn load_preferences(&self) -> Preferences {
        match read_bounded(&self.root.join("preferences.json"), 512) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or(Preferences {
                diagnostics: false,
                usage: false,
            }),
            Err(ref error) if error.kind() == io::ErrorKind::NotFound => Preferences::default(),
            Err(_) => Preferences {
                diagnostics: false,
                usage: false,
            },
        }
    }

    pub(crate) fn preferences(&self, preferences: Preferences) -> io::Result<()> {
        let bytes = serde_json::to_vec(&preferences).map_err(io::Error::other)?;
        let tmp = self.root.join(format!("preferences-{}.tmp", self.unique()));
        let result = (|| {
            let mut file = private_file(&tmp)?;
            file.write_all(&bytes)?;
            drop(file);
            fs::rename(&tmp, self.root.join("preferences.json"))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    fn unique(&self) -> String {
        let nanos = time::SystemTime::now()
            .duration_since(time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!(
            "{nanos:039}-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, atomic::Ordering::Relaxed)
        )
    }

    fn files(&self) -> io::Result<Vec<path::PathBuf>> {
        // A private, application-owned directory. Never walk outside it or follow links.
        let mut files = Vec::new();
        for (index, entry) in fs::read_dir(&self.root)?.take(65).enumerate() {
            if index == 64 {
                return Err(io::ErrorKind::InvalidData.into());
            }
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if (name.starts_with("failure-") || name.starts_with("usage-"))
                && name.ends_with(".json")
            {
                files.push(entry.path());
            }
        }
        files.sort_by_key(|path| {
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .split_once('-')
                .map(|(_, key)| key.to_owned())
                .unwrap_or_default()
        });
        Ok(files)
    }

    fn prune(&self, keep: usize) -> io::Result<()> {
        let now = time::SystemTime::now();
        let mut kept = 0;
        for path in self.files()?.iter().rev() {
            let meta = fs::symlink_metadata(path)?;
            let expired = meta
                .modified()
                .ok()
                .and_then(|t| now.duration_since(t).ok())
                .is_some_and(|age| age > MAX_AGE);
            if !meta.is_file() || meta.len() > MAX_BYTES || expired || kept >= keep {
                fs::remove_file(path)?;
            } else {
                kept += 1;
            }
        }
        Ok(())
    }

    pub(crate) fn purge_disabled(&self, preferences: Preferences) -> io::Result<()> {
        for path in self.files()? {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if (!preferences.diagnostics && name.starts_with("failure-"))
                || (!preferences.usage && name.starts_with("usage-"))
            {
                fs::remove_file(path)?;
            }
        }
        Ok(())
    }

    pub(crate) fn save(&self, report: &Report) -> io::Result<path::PathBuf> {
        if !report.valid(self.app) {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let bytes = serde_json::to_vec(report).map_err(io::Error::other)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(io::ErrorKind::InvalidData.into());
        }
        self.prune(MAX_REPORTS - 1)?;
        let prefix = match report.payload {
            Payload::Failure { .. } => "failure",
            Payload::Usage { .. } => "usage",
        };
        let path = self.root.join(format!("{prefix}-{}.json", self.unique()));
        let mut file = private_file(&path)?;
        if let Err(error) = file.write_all(&bytes) {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        Ok(path)
    }

    pub(crate) fn reports(&self) -> Vec<(path::PathBuf, Report)> {
        self.files()
            .unwrap_or_default()
            .into_iter()
            .rev()
            .take(MAX_REPORTS)
            .filter_map(|path| {
                let report: Report =
                    serde_json::from_slice(&read_bounded(&path, MAX_BYTES).ok()?).ok()?;
                report.valid(self.app).then_some((path, report))
            })
            .collect()
    }

    pub(crate) fn delete(&self, path: &path::Path) -> io::Result<()> {
        if path.parent() != Some(self.root.as_path()) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        fs::remove_file(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Failure, Info, Usage};
    const INFO: Info = Info {
        app: App::Fileman,
        version: "0.4.0",
        revision: None,
        private_email: None,
    };
    struct Temp(path::PathBuf);
    impl Temp {
        fn new() -> Self {
            let unique = SEQUENCE.fetch_add(1, atomic::Ordering::Relaxed);
            Self(
                std::env::temp_dir()
                    .join(format!("navigato-support-{}-{unique}", std::process::id())),
            )
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn diagnostic() -> Report {
        Report::new(
            INFO,
            Payload::Failure {
                kind: Failure::Panic,
                site: None,
            },
        )
    }
    #[test]
    fn reports_are_bounded_and_parseable() {
        let tmp = Temp::new();
        let store = Storage::open(tmp.0.clone(), App::Fileman).unwrap();
        for _ in 0..20 {
            store.save(&diagnostic()).unwrap();
        }
        assert_eq!(store.reports().len(), MAX_REPORTS);
        assert_eq!(store.files().unwrap().len(), MAX_REPORTS);
    }
    #[test]
    fn only_one_instance_can_own_reporting() {
        let tmp = Temp::new();
        let store = Storage::open(tmp.0.clone(), App::Fileman).unwrap();
        assert!(Storage::open(tmp.0.clone(), App::Fileman).is_err());
        drop(store);
        assert!(Storage::open(tmp.0.clone(), App::Fileman).is_ok());
    }
    #[test]
    fn crowded_directory_fails_closed_instead_of_ignoring_reports() {
        let tmp = Temp::new();
        let store = Storage::open(tmp.0.clone(), App::Fileman).unwrap();
        for index in 0..65 {
            fs::write(tmp.0.join(format!("other-{index}")), b"").unwrap();
        }
        assert!(store.save(&diagnostic()).is_err());
    }
    #[test]
    fn privacy_choices_are_independent_and_persisted() {
        let tmp = Temp::new();
        let store = Storage::open(tmp.0.clone(), App::Fileman).unwrap();
        assert!(!store.load_preferences().usage);
        store.save(&diagnostic()).unwrap();
        store
            .save(&Report::new(
                INFO,
                Payload::Usage {
                    counters: Usage::default(),
                },
            ))
            .unwrap();
        let prefs = Preferences {
            diagnostics: false,
            usage: true,
        };
        store.preferences(prefs).unwrap();
        store.purge_disabled(prefs).unwrap();
        assert!(!store.load_preferences().diagnostics);
        assert_eq!(store.reports().len(), 1);
        assert!(matches!(
            store.reports()[0].1.payload,
            Payload::Usage { .. }
        ));
        store
            .purge_disabled(Preferences {
                diagnostics: false,
                usage: false,
            })
            .unwrap();
        assert!(store.reports().is_empty());
    }
    #[test]
    fn corrupt_preferences_fail_closed() {
        let tmp = Temp::new();
        let store = Storage::open(tmp.0.clone(), App::Fileman).unwrap();
        fs::write(tmp.0.join("preferences.json"), b"not json").unwrap();
        let prefs = store.load_preferences();
        assert!(!prefs.diagnostics && !prefs.usage);
    }
    #[test]
    fn tampered_reports_and_oversize_files_are_not_exported() {
        let tmp = Temp::new();
        let store = Storage::open(tmp.0.clone(), App::Fileman).unwrap();
        let path = store.save(&diagnostic()).unwrap();
        let mut data = serde_json::to_value(diagnostic()).unwrap();
        data["payload"]["message"] = "CANARY secret/host/path".into();
        fs::write(path, serde_json::to_vec(&data).unwrap()).unwrap();
        fs::write(tmp.0.join("failure-big.json"), vec![b'x'; 8193]).unwrap();
        assert!(store.reports().is_empty());
    }
    #[test]
    fn disabling_diagnostics_prevents_writes() {
        let tmp = Temp::new();
        let store = Storage::open(tmp.0.clone(), App::Fileman).unwrap();
        let mut state = crate::State {
            info: INFO,
            storage: Some(store),
            preferences: Preferences {
                diagnostics: false,
                usage: false,
            },
            reports: Vec::new(),
            usage: Usage::default(),
            notice: None,
            selected: 0,
        };
        state.record(Failure::Panic, None);
        assert!(state.storage.as_ref().unwrap().files().unwrap().is_empty());
    }
    #[cfg(unix)]
    #[test]
    fn private_permissions_and_symlink_rejection() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let tmp = Temp::new();
        let store = Storage::open(tmp.0.clone(), App::Fileman).unwrap();
        let report = store.save(&diagnostic()).unwrap();
        assert_eq!(
            fs::metadata(&tmp.0).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&report).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let link = tmp.0.join("failure-link.json");
        symlink(&report, &link).unwrap();
        assert!(read_bounded(&link, MAX_BYTES).is_err());
        let root_link = Temp::new();
        symlink(&tmp.0, &root_link.0).unwrap();
        assert!(Storage::open(root_link.0.clone(), App::Fileman).is_err());
        fs::remove_file(&root_link.0).unwrap();
    }
}
