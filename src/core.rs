use std::{
    fs,
    io::{self, Read},
    path::{self, Path},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::UNIX_EPOCH,
};

/// Shared transfer progress, updated atomically by worker threads and read by
/// the UI. One instance lives in AppState behind an Arc.
pub struct TransferProgress {
    /// Bytes transferred so far.
    pub bytes_done: AtomicU64,
    /// Total bytes expected (0 = unknown).
    pub bytes_total: AtomicU64,
    /// Items processed (e.g. files/dirs deleted); displayed when bytes are 0.
    pub items_done: AtomicU64,
    /// Name of the item currently being transferred — set by the IO worker
    /// at the start of each task, displayed in the progress modal. None
    /// when no task is in flight.
    pub current_name: std::sync::Mutex<Option<String>>,
}

impl TransferProgress {
    pub fn new() -> Self {
        Self {
            bytes_done: AtomicU64::new(0),
            bytes_total: AtomicU64::new(0),
            items_done: AtomicU64::new(0),
            current_name: std::sync::Mutex::new(None),
        }
    }

    pub fn reset(&self, total: u64) {
        self.bytes_done.store(0, Ordering::Relaxed);
        self.bytes_total.store(total, Ordering::Relaxed);
        self.items_done.store(0, Ordering::Relaxed);
    }

    pub fn add(&self, n: u64) {
        self.bytes_done.fetch_add(n, Ordering::Relaxed);
    }

    pub fn add_item(&self) {
        self.items_done.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> (u64, u64) {
        (
            self.bytes_done.load(Ordering::Relaxed),
            self.bytes_total.load(Ordering::Relaxed),
        )
    }

    pub fn set_current_name(&self, name: Option<String>) {
        if let Ok(mut g) = self.current_name.lock() {
            *g = name;
        }
    }

    pub fn current_name(&self) -> Option<String> {
        self.current_name.lock().ok().and_then(|g| g.clone())
    }
}

pub use crate::archive::{
    ContainerKind, container_display_path, container_kind_from_path, copy_container_dir,
    copy_container_entry, create_archive, format_container_listing, is_container_path,
    normalize_archive_path, read_container_bytes_prefix, read_container_directory,
    read_container_directory_with_progress, read_container_metadata,
};

#[derive(Clone)]
pub enum EntryLocation {
    Fs(path::PathBuf),
    Container {
        kind: ContainerKind,
        archive_path: path::PathBuf,
        inner_path: String, // no leading slash, '' means root
    },
    Remote {
        host: String,
        path: String, // absolute path on remote, e.g. "/home/user"
    },
}

impl EntryLocation {
    pub fn display_name(&self) -> String {
        match *self {
            EntryLocation::Fs(ref path) => path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("<unknown>")
                .to_string(),
            EntryLocation::Container { ref inner_path, .. } => inner_path
                .rsplit('/')
                .next()
                .unwrap_or("<unknown>")
                .to_string(),
            EntryLocation::Remote { ref path, .. } => {
                path.rsplit('/').next().unwrap_or("<unknown>").to_string()
            }
        }
    }
}

#[derive(Clone)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub link_target: Option<String>,
    pub location: EntryLocation,
    pub size: Option<u64>,
    pub modified: Option<u64>,
}

pub enum DirBatch {
    Append(Vec<DirEntry>),
    Replace(Vec<DirEntry>),
    ContainerRoot(Option<String>),
    Loading,
    Progress { loaded: usize, total: Option<usize> },
    Error(String),
    ConnectionError(String),
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ActivePanel {
    Left,
    Right,
}

#[derive(Clone)]
pub enum BrowserMode {
    Fs,
    Container {
        kind: ContainerKind,
        archive_path: path::PathBuf,
        cwd: String,
        root: Option<String>,
    },
    Search {
        root: path::PathBuf,
        query: String,
        mode: SearchMode,
        case: SearchCase,
    },
    Remote {
        host: String,
        path: String,
    },
}

pub enum PreviewContent {
    Text(String),
    Binary(Vec<u8>),
    TextChunk { text: String, done: bool },
    BinaryChunk { data: Vec<u8>, done: bool },
    Image(ImageLocation),
}

#[derive(Clone)]
pub enum ImageLocation {
    Fs(Arc<Path>),
    Container {
        kind: ContainerKind,
        archive_path: path::PathBuf,
        inner_path: String,
    },
    Remote {
        host: String,
        path: String,
    },
}

pub enum PreviewRequest {
    Read {
        id: u64,
        location: EntryLocation,
        max_bytes: Option<usize>,
    },
    ListContainer {
        id: u64,
        kind: ContainerKind,
        archive_path: path::PathBuf,
        max_entries: usize,
    },
}

#[derive(Clone)]
pub enum IOTask {
    Copy {
        src: path::PathBuf,
        dst_dir: path::PathBuf,
    },
    CopyContainer {
        kind: ContainerKind,
        archive_path: path::PathBuf,
        inner_path: String,
        dst_dir: path::PathBuf,
        display_name: String,
    },
    CopyContainerDir {
        kind: ContainerKind,
        archive_path: path::PathBuf,
        inner_path: String,
        dst_dir: path::PathBuf,
        display_name: String,
    },
    Move {
        src: path::PathBuf,
        dst_dir: path::PathBuf,
    },
    Delete {
        target: path::PathBuf,
    },
    Rename {
        src: path::PathBuf,
        new_name: String,
    },
    WriteFile {
        path: path::PathBuf,
        contents: Vec<u8>,
        /// When true (creating a new file), fail if the path already exists
        /// instead of truncating it. When false (editor save), overwrite the
        /// existing file atomically.
        exclusive: bool,
    },
    Mkdir {
        path: path::PathBuf,
    },
    SetProps {
        path: path::PathBuf,
        mode: u32,
        uid: u32,
        gid: u32,
        recursive: bool,
    },
    Pack {
        sources: Vec<path::PathBuf>,
        archive_path: path::PathBuf,
        kind: crate::archive::ContainerKind,
    },
    WriteRemoteFile {
        host: String,
        path: String,
        contents: Vec<u8>,
    },
    CopyRemoteToLocal {
        host: String,
        remote_path: String,
        dst_dir: path::PathBuf,
        name: String,
        is_dir: bool,
        /// When true (a cross-location Move), delete the remote source after the
        /// copy has verifiably succeeded. The delete never runs on copy failure.
        delete_source_on_success: bool,
    },
    CopyLocalToRemote {
        src: path::PathBuf,
        host: String,
        remote_dir: String,
        is_dir: bool,
        /// When true (a cross-location Move), delete the local source after the
        /// copy has verifiably succeeded. The delete never runs on copy failure.
        delete_source_on_success: bool,
    },
    DeleteRemote {
        host: String,
        /// `(path, is_dir)` pairs — batched so the progress counter isn't reset between items.
        items: Vec<(String, bool)>,
    },
    RenameRemote {
        host: String,
        src: String,
        new_name: String,
    },
    MkdirRemote {
        host: String,
        path: String,
    },
    CopyRemoteToLocalAndOpen {
        host: String,
        remote_path: String,
        local_path: path::PathBuf,
    },
    CopyRemoteSameHost {
        host: String,
        src_path: String,
        dst_dir: String,
        name: String,
    },
    MoveRemoteSameHost {
        host: String,
        src_path: String,
        dst_dir: String,
        name: String,
    },
    CopyContainerAndOpen {
        kind: crate::archive::ContainerKind,
        archive_path: path::PathBuf,
        inner_path: String,
        dst_dir: path::PathBuf,
        display_name: String,
    },
    CopyRemoteCrossHost {
        src_host: String,
        src_path: String,
        dst_host: String,
        dst_dir: String,
        name: String,
        is_dir: bool,
    },
    /// Extract an entry from an archive and upload it to a remote directory.
    CopyContainerToRemote {
        kind: crate::archive::ContainerKind,
        archive_path: path::PathBuf,
        inner_path: String,
        host: String,
        remote_dir: String,
        display_name: String,
        is_dir: bool,
    },
    /// Re-run the inner task with OS-level privilege elevation.
    Elevated(Box<IOTask>),
}

impl IOTask {
    /// Best-effort filename for display in the progress modal. Walks into
    /// `Elevated` so a wrapped task still shows its target. Returns "…" for
    /// tasks where no single name is meaningful (Pack, batched delete with
    /// multiple items).
    pub fn display_name(&self) -> String {
        fn fs_name(p: &path::Path) -> String {
            p.file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.to_string_lossy().into_owned())
        }
        fn remote_name(p: &str) -> String {
            p.rsplit('/').next().unwrap_or(p).to_string()
        }
        match *self {
            IOTask::Copy { ref src, .. } => fs_name(src),
            IOTask::Move { ref src, .. } => fs_name(src),
            IOTask::Delete { ref target } => fs_name(target),
            IOTask::Rename { ref src, .. } => fs_name(src),
            IOTask::WriteFile { ref path, .. } => fs_name(path),
            IOTask::Mkdir { ref path } => fs_name(path),
            IOTask::SetProps { ref path, .. } => fs_name(path),
            IOTask::CopyContainer {
                ref display_name, ..
            }
            | IOTask::CopyContainerDir {
                ref display_name, ..
            }
            | IOTask::CopyContainerAndOpen {
                ref display_name, ..
            }
            | IOTask::CopyContainerToRemote {
                ref display_name, ..
            } => display_name.clone(),
            IOTask::Pack {
                ref archive_path, ..
            } => fs_name(archive_path),
            IOTask::WriteRemoteFile { ref path, .. } => remote_name(path),
            IOTask::CopyRemoteToLocal { ref name, .. } => name.clone(),
            IOTask::CopyLocalToRemote { ref src, .. } => fs_name(src),
            IOTask::DeleteRemote { ref items, .. } => {
                if items.len() == 1 {
                    remote_name(&items[0].0)
                } else {
                    format!("{} items", items.len())
                }
            }
            IOTask::RenameRemote { ref src, .. } => remote_name(src),
            IOTask::MkdirRemote { ref path, .. } => remote_name(path),
            IOTask::CopyRemoteToLocalAndOpen {
                ref remote_path, ..
            } => remote_name(remote_path),
            IOTask::CopyRemoteSameHost { ref name, .. }
            | IOTask::MoveRemoteSameHost { ref name, .. } => name.clone(),
            IOTask::CopyRemoteCrossHost { ref name, .. } => name.clone(),
            IOTask::Elevated(ref inner) => inner.display_name(),
        }
    }
}

pub enum IOResult {
    /// Refresh all local (Fs) panels — default for local ops.
    Completed,
    /// Refresh only remote panels browsing this host.
    CompletedRemote(String),
    /// A cross-location move completed — refresh both local panels and the
    /// remote host, since one side is the source and the other the destination.
    CompletedMoved(String),
    /// No panel refresh needed (open-only / read-only ops).
    CompletedSilent,
    /// Operation failed — display the error to the user and still refresh.
    Error(String),
    /// Remote operation failed — display error and refresh that host.
    ErrorRemote(String, String),
    /// Local operation failed with a permission error — offer elevation retry.
    PermissionError { message: String, task: IOTask },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    Name,
    Date,
    Size,
    Raw,
}

pub struct EditLoadRequest {
    pub id: u64,
    pub path: path::PathBuf,
    pub remote: Option<(String, String)>, // (host, remote_path)
}

pub struct EditLoadResult {
    pub id: u64,
    pub path: path::PathBuf,
    pub text: String,
    /// Whether the original file used CRLF line endings.
    pub crlf: bool,
    /// True when the load failed or the file was refused (binary, unreadable,
    /// no session). The buffer holds an explanatory message rather than file
    /// contents, so saving must be blocked to avoid overwriting the file.
    pub failed: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    Name,
    Content,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SearchCase {
    Sensitive,
    Insensitive,
}

pub struct SearchRequest {
    pub id: u64,
    pub root: path::PathBuf,
    pub needle: String,
    pub case: SearchCase,
    pub mode: SearchMode,
    /// Set for remote searches: (host, remote_root_path).
    pub remote: Option<(String, String)>,
}

#[derive(Clone)]
pub struct SearchResult {
    pub path: path::PathBuf,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub modified: Option<u64>,
    /// For remote searches: the actual remote path (e.g. `/home/user/file.rs`).
    /// `path` in that case is the synthetic `/sftp/<host>/…` path used for display.
    pub remote_path: Option<String>,
}

#[derive(Clone, Copy)]
pub struct SearchProgress {
    pub scanned: usize,
    pub matched: usize,
}

pub enum SearchEvent {
    Match { id: u64, result: SearchResult },
    Progress { id: u64, progress: SearchProgress },
    Done { id: u64, progress: SearchProgress },
    Error { id: u64, message: String },
}

/// Whether two paths refer to the same filesystem location. Uses canonical
/// paths when both exist; if the destination does not exist yet (the common
/// case for a copy target), it compares the canonical source against the
/// canonical destination directory joined with the final component, and falls
/// back to a lexical comparison when canonicalization is unavailable.
fn is_same_path(src: &Path, dest: &Path) -> bool {
    if let (Ok(a), Ok(b)) = (src.canonicalize(), dest.canonicalize()) {
        return a == b;
    }
    if let (Ok(canon_src), Some(dest_parent), Some(dest_name)) =
        (src.canonicalize(), dest.parent(), dest.file_name())
    {
        if let Ok(canon_dest_parent) = dest_parent.canonicalize() {
            return canon_dest_parent.join(dest_name) == canon_src;
        }
    }
    src == dest
}

pub fn copy_recursively(src: &Path, dst_dir: &Path) -> io::Result<()> {
    let src_name = src
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "source has no file name"))?;

    let dest = dst_dir.join(src_name);

    // Prevent copying a file or directory onto itself. When both panels show
    // the same directory, `dest` resolves to `src`; without this guard
    // `fs::copy` would open `src` and then truncate the very same inode to
    // zero bytes before copying, irreversibly destroying the file (and, for a
    // directory, every file in the tree via the recursion below).
    if is_same_path(src, &dest) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source and destination are the same file",
        ));
    }

    // Prevent copying a directory into itself (infinite recursion / disk fill).
    if src.is_dir() {
        if let (Ok(canon_src), Ok(canon_dst)) = (src.canonicalize(), dst_dir.canonicalize()) {
            if canon_dst.starts_with(&canon_src) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "cannot copy a directory into itself",
                ));
            }
        }
    }

    let meta = fs::symlink_metadata(src)?;

    if meta.file_type().is_symlink() {
        #[cfg(unix)]
        {
            let target = fs::read_link(src)?;
            std::os::unix::fs::symlink(&target, &dest)?;
        }
        #[cfg(not(unix))]
        {
            // On Windows, copy the target rather than recreating the link.
            fs::copy(src, &dest)?;
        }
    } else if meta.is_dir() {
        fs::create_dir_all(&dest)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_recursively(&entry.path(), &dest)?;
        }
    } else {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, dest)?;
    }
    Ok(())
}

pub fn read_fs_directory(path: &path::Path) -> anyhow::Result<Vec<DirEntry>> {
    let mut entries = Vec::new();

    let read_dir = fs::read_dir(path)?;
    let mut dir_entries = Vec::new();

    for entry in read_dir {
        let entry = entry?;
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy().to_string();

        let file_type = entry.file_type()?;
        let is_symlink = file_type.is_symlink();
        // DirEntry::metadata() uses lstat (no follow); fs::metadata follows symlinks
        let metadata = if is_symlink {
            fs::metadata(entry.path()).ok()
        } else {
            entry.metadata().ok()
        };
        let is_dir = if is_symlink {
            metadata.as_ref().map(|m| m.is_dir()).unwrap_or(false)
        } else {
            file_type.is_dir()
        };
        let size = if is_dir {
            None
        } else {
            metadata.as_ref().map(|m| m.len())
        };
        let modified = metadata
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        dir_entries.push(DirEntry {
            name: file_name,
            is_dir,
            is_symlink,
            link_target: None,
            location: EntryLocation::Fs(entry.path()),
            size,
            modified,
        });
    }

    dir_entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));

    if path.parent().is_some() {
        entries.push(DirEntry {
            name: "..".to_string(),
            is_dir: true,
            is_symlink: false,
            link_target: None,
            location: EntryLocation::Fs(path.parent().unwrap().to_path_buf()),
            size: None,
            modified: None,
        });
    }

    entries.extend(dir_entries);

    Ok(entries)
}

pub fn format_preview_info(kind: &str, location: &EntryLocation) -> String {
    match *location {
        EntryLocation::Fs(ref path) => format!("{kind}\n{}", path.to_string_lossy()),
        EntryLocation::Container {
            kind: container_kind,
            ref archive_path,
            ref inner_path,
        } => {
            let display = container_display_path(container_kind, archive_path, inner_path);
            format!("{kind}\n{display}")
        }
        EntryLocation::Remote { ref host, ref path } => format!("{kind}\n{host}:{path}"),
    }
}

pub fn is_image_path(p: &Path) -> bool {
    matches!(
        p.extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase()),
        Some(ext) if matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "tga" | "hdr" | "dds")
    )
}

pub fn is_image_name(name: &str) -> bool {
    is_image_path(Path::new(name))
}

pub fn is_audio_path(p: &Path) -> bool {
    matches!(
        p.extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase()),
        Some(ext)
            if matches!(
                ext.as_str(),
                "mp3" | "wav" | "flac" | "ogg" | "opus" | "m4a" | "aac" | "alac"
                    | "aiff" | "wma"
            )
    )
}

pub fn is_audio_name(name: &str) -> bool {
    is_audio_path(Path::new(name))
}

pub fn is_video_path(p: &Path) -> bool {
    matches!(
        p.extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase()),
        Some(ext)
            if matches!(
                ext.as_str(),
                "mp4"
                    | "m4v"
                    | "mkv"
                    | "avi"
                    | "mov"
                    | "webm"
                    | "mpg"
                    | "mpeg"
                    | "flv"
                    | "wmv"
            )
    )
}

pub fn is_video_name(name: &str) -> bool {
    is_video_path(Path::new(name))
}

pub fn is_media_name(name: &str) -> bool {
    is_image_name(name) || is_audio_name(name) || is_video_name(name)
}

pub fn is_text_path(p: &Path) -> bool {
    matches!(
        p.extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase()),
        Some(ext)
            if matches!(
                ext.as_str(),
                "txt"
                    | "md"
                    | "json"
                    | "toml"
                    | "yaml"
                    | "yml"
                    | "rs"
                    | "log"
                    | "ini"
                    | "csv"
                    | "nix"
            )
    )
}

pub fn is_text_name(name: &str) -> bool {
    is_text_path(Path::new(name))
}

pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1}G", b / GB)
    } else if b >= MB {
        format!("{:.1}M", b / MB)
    } else if b >= KB {
        format!("{:.1}K", b / KB)
    } else {
        format!("{}B", bytes)
    }
}

/// Format a Unix timestamp (seconds since epoch) as a compact date/time string.
/// Returns "YYYY-MM-DD HH:MM" for compactness.
pub fn format_date(epoch_secs: u64) -> String {
    // Convert epoch seconds to calendar date/time using basic arithmetic.
    // This handles leap years correctly.
    let secs = epoch_secs as i64;
    let days_since_epoch = secs.div_euclid(86400);
    let time_of_day = secs.rem_euclid(86400) as u64;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;

    // Civil date from days since 1970-01-01 (algorithm from Howard Hinnant)
    let z = days_since_epoch + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097) as u64; // day of era [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02} {hours:02}:{minutes:02}")
}

pub fn format_mode(mode: u32) -> String {
    let file_type = if mode & 0o40000 != 0 {
        'd'
    } else if mode & 0o120000 != 0 {
        'l'
    } else {
        '-'
    };
    let mut out = String::with_capacity(10);
    out.push(file_type);
    let perms = [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'),
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'),
    ];
    for (mask, ch) in perms {
        if mode & mask != 0 {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    out
}

pub fn read_text_preview(path: &Path, max_bytes: usize) -> anyhow::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut buf = Vec::new();
    file.by_ref().take(max_bytes as u64).read_to_end(&mut buf)?;
    Ok(decode_text_bytes(&buf))
}

/// Decode a byte slice as text. Prefers UTF-8; falls back to chardetng
/// detection for legacy encodings (CP1251, CP1252, Shift_JIS, GBK, …).
pub fn decode_text_bytes(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    let mut det = chardetng::EncodingDetector::new();
    det.feed(bytes, true);
    let encoding = det.guess(None, true);
    let (decoded, _, _) = encoding.decode(bytes);
    decoded.into_owned()
}

pub fn read_bytes_prefix(path: &Path, max_bytes: usize) -> anyhow::Result<Vec<u8>> {
    let mut file = fs::File::open(path)?;
    let mut buf = Vec::new();
    file.by_ref().take(max_bytes as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

pub fn hexdump(bytes: &[u8]) -> String {
    hexdump_with_width(bytes, 16)
}

pub fn hexdump_with_width(bytes: &[u8], width: usize) -> String {
    let width = width.clamp(4, 32);
    let mut out = String::new();
    let mut offset = 0usize;
    for chunk in bytes.chunks(width) {
        out.push_str(&format!("{:08x}: ", offset));
        for i in 0..width {
            if i < chunk.len() {
                out.push_str(&format!("{:02x} ", chunk[i]));
            } else {
                out.push_str("   ");
            }
            if i == (width / 2).saturating_sub(1) {
                out.push(' ');
            }
        }
        out.push(' ');
        for &b in chunk {
            let ch = if (0x20..=0x7e).contains(&b) {
                b as char
            } else {
                '.'
            };
            out.push(ch);
        }
        out.push('\n');
        offset += width;
    }
    out
}

pub fn is_probably_text(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    if bytes.contains(&0) {
        return false;
    }
    // Strip UTF-8 BOM if present
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    // Valid UTF-8 is almost certainly text
    if std::str::from_utf8(bytes).is_ok() {
        return true;
    }
    // Fall back to printable ASCII ratio for non-UTF-8 encodings
    let mut printable = 0usize;
    for &b in bytes {
        match b {
            0x09 | 0x0A | 0x0D => printable += 1,
            0x20..=0x7E => printable += 1,
            0x80..=0xFF => printable += 1, // high bytes (Latin-1, etc.)
            _ => {}
        }
    }
    let ratio = printable as f32 / bytes.len().max(1) as f32;
    ratio > 0.85
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A self-cleaning unique temp directory built with std only (no dev-deps).
    struct TmpDir(path::PathBuf);
    impl TmpDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let mut p = std::env::temp_dir();
            p.push(format!("fileman-test-{tag}-{}-{n}", std::process::id()));
            fs::create_dir_all(&p).unwrap();
            TmpDir(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn copy_into_same_directory_is_rejected_and_preserves_file() {
        let dir = TmpDir::new("same-dir-copy");
        let file = dir.path().join("notes.txt");
        fs::write(&file, b"important contents").unwrap();

        // Copying a file into the directory it already lives in must be
        // rejected rather than truncating the source to zero bytes.
        let err = copy_recursively(&file, dir.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

        let after = fs::read(&file).unwrap();
        assert_eq!(after, b"important contents", "source file must be intact");
    }

    #[test]
    fn copy_into_same_directory_is_rejected_for_directory() {
        let dir = TmpDir::new("same-dir-copy-dir");
        let sub = dir.path().join("data");
        fs::create_dir_all(&sub).unwrap();
        let inner = sub.join("file.bin");
        fs::write(&inner, b"payload").unwrap();

        let err = copy_recursively(&sub, dir.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(fs::read(&inner).unwrap(), b"payload");
    }

    #[test]
    fn copy_into_different_directory_succeeds() {
        let src_dir = TmpDir::new("copy-src");
        let dst_dir = TmpDir::new("copy-dst");
        let file = src_dir.path().join("doc.txt");
        fs::write(&file, b"hello").unwrap();

        copy_recursively(&file, dst_dir.path()).unwrap();
        assert_eq!(fs::read(dst_dir.path().join("doc.txt")).unwrap(), b"hello");
        // Original must remain.
        assert_eq!(fs::read(&file).unwrap(), b"hello");
    }
}
