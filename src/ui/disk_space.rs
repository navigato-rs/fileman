//! Demand-driven free-space cache. No filesystem calls on the drawing thread.

use std::{collections, path, sync, thread, time};

type Wake = sync::Arc<dyn Fn() + Send + Sync>;
const TTL: time::Duration = time::Duration::from_secs(5);
const MAX_ENTRIES: usize = 64;
const MAX_PENDING: usize = 8;

struct Entry {
    value: Option<u64>,
    sampled: Option<time::Instant>,
    used: time::Instant,
    pending: bool,
}

impl Entry {
    fn fresh(&self, now: time::Instant) -> bool {
        self.sampled
            .is_some_and(|at| now.saturating_duration_since(at) < TTL)
    }
}

type Entries = collections::HashMap<path::PathBuf, Entry>;

struct Request {
    path: path::PathBuf,
    wake: Option<Wake>,
}

struct Cache {
    entries: sync::Arc<sync::Mutex<Entries>>,
    sender: sync::mpsc::SyncSender<Request>,
}

impl Cache {
    fn new(query: fn(&path::Path) -> Option<u64>) -> Self {
        let entries = sync::Arc::new(sync::Mutex::new(Entries::new()));
        let (sender, receiver) = sync::mpsc::sync_channel::<Request>(MAX_PENDING);
        let shared = sync::Arc::clone(&entries);
        if let Err(error) = thread::Builder::new()
            .name("free-space".into())
            .spawn(move || {
                while let Ok(request) = receiver.recv() {
                    let value = query(&request.path);
                    {
                        let mut entries = shared.lock().unwrap_or_else(|error| error.into_inner());
                        if let Some(entry) = entries.get_mut(&request.path) {
                            entry.value = value;
                            entry.sampled = Some(time::Instant::now());
                            entry.pending = false;
                        }
                    }
                    if let Some(wake) = request.wake {
                        wake();
                    }
                }
            })
        {
            log::warn!("cannot start free-space worker: {error}");
        }
        Self { entries, sender }
    }

    fn get(&self, path: &path::Path, wake: Option<Wake>) -> Option<u64> {
        let now = time::Instant::now();
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !entries.contains_key(path) && entries.len() >= MAX_ENTRIES {
            let oldest = entries
                .iter()
                .filter(|(_, entry)| !entry.pending)
                .min_by_key(|(_, entry)| entry.used)
                .map(|(path, _)| path.clone());
            entries.remove(&oldest?);
        }
        let entry = entries.entry(path.to_owned()).or_insert(Entry {
            value: None,
            sampled: None,
            used: now,
            pending: false,
        });
        entry.used = now;
        if !entry.pending && !entry.fresh(now) {
            entry.pending = true;
            if self
                .sender
                .try_send(Request {
                    path: path.to_owned(),
                    wake,
                })
                .is_err()
            {
                entry.pending = false;
            }
        }
        entry.value
    }
}

pub(super) fn get(path: &path::Path, wake: Option<Wake>) -> Option<u64> {
    static CACHE: sync::LazyLock<Cache> = sync::LazyLock::new(|| Cache::new(free_space_bytes));
    CACHE.get(path, wake)
}

#[cfg(unix)]
fn free_space_bytes(path: &std::path::Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt as _;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return None;
    }
    // statvfs widths vary by platform; multiply via u128 to avoid overflow.
    let bavail = u128::from(stat.f_bavail);
    let frsize = u128::from(stat.f_frsize);
    Some((bavail * frsize).min(u128::from(u64::MAX)) as u64)
}

#[cfg(windows)]
fn free_space_bytes(path: &std::path::Path) -> Option<u64> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt as _;

    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(
            lpDirectoryName: *const u16,
            lpFreeBytesAvailableToCaller: *mut u64,
            lpTotalNumberOfBytes: *mut u64,
            lpTotalNumberOfFreeBytes: *mut u64,
        ) -> i32;
    }

    let wide: Vec<u16> = OsStr::new(path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut free_available: u64 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok != 0 { Some(free_available) } else { None }
}

#[cfg(not(any(unix, windows)))]
fn free_space_bytes(_path: &std::path::Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_queries_are_cached_too() {
        let now = time::Instant::now();
        let entry = Entry {
            value: None,
            sampled: Some(now),
            used: now,
            pending: false,
        };
        assert!(entry.fresh(now));
        assert!(!entry.fresh(now + TTL));
    }

    #[test]
    fn worker_results_are_cached_and_wake_the_ui() {
        static RUNS: sync::atomic::AtomicUsize = sync::atomic::AtomicUsize::new(0);
        fn query(_: &path::Path) -> Option<u64> {
            RUNS.fetch_add(1, sync::atomic::Ordering::Relaxed);
            Some(123)
        }
        let cache = Cache::new(query);
        let (sender, receiver) = sync::mpsc::channel();
        let wake: Wake = sync::Arc::new(move || {
            let _ = sender.send(());
        });
        let path = path::Path::new("synthetic-path");
        assert_eq!(cache.get(path, Some(wake)), None);
        receiver.recv_timeout(time::Duration::from_secs(5)).unwrap();
        for _ in 0..100 {
            assert_eq!(cache.get(path, None), Some(123));
        }
        assert_eq!(RUNS.load(sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn cached_entries_are_bounded() {
        let (sender, _receiver) = sync::mpsc::sync_channel(0);
        let cache = Cache {
            entries: Default::default(),
            sender,
        };
        for i in 0..MAX_ENTRIES * 2 {
            assert_eq!(
                cache.get(&path::PathBuf::from(format!("path-{i}")), None),
                None
            );
        }
        assert_eq!(cache.entries.lock().unwrap().len(), MAX_ENTRIES);
        assert!(
            cache
                .entries
                .lock()
                .unwrap()
                .values()
                .all(|entry| !entry.pending)
        );
    }
}
