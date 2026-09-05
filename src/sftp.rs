use std::{
    collections::HashMap,
    io::{self, Read, Write},
    path::Path,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::core::{DirEntry, EntryLocation};
use crate::ssh::{self, Conn, FileAttrs, OpenMode, RemoteFile};

pub struct SftpSession {
    /// The connection. Named `sftp` because that is what the app does with it;
    /// exec-based operations share the same connection.
    pub sftp: Arc<Conn>,
    pub host: String,
    /// Remote user's home directory (from `realpath(".")`), if resolved.
    pub home_dir: Option<String>,
}

impl SftpSession {
    /// False once the connection has failed, so the app can drop the session.
    pub fn is_alive(&self) -> bool {
        self.sftp.is_alive()
    }
}

type SessionMap = HashMap<String, Arc<Mutex<SftpSession>>>;

/// Shared SFTP session registry for global access (e.g. from archive streaming).
/// Initialized once at startup with the app's shared session map.
static SHARED_SESSIONS: OnceLock<Arc<Mutex<SessionMap>>> = OnceLock::new();

pub fn init_shared_registry(sessions: Arc<Mutex<SessionMap>>) {
    let _ = SHARED_SESSIONS.set(sessions);
}

pub fn get_session(host: &str) -> Option<Arc<Mutex<SftpSession>>> {
    SHARED_SESSIONS.get()?.lock().ok()?.get(host).cloned()
}

/// Synthetic path prefix used to encode remote archive locations as `PathBuf`,
/// so remote archives can flow through the existing container browsing path.
/// Format: `/.sftp-archive/<host><remote_abs_path>` — the remote path keeps
/// its leading `/` so `<host>` is the first path segment after the marker.
pub const SFTP_ARCHIVE_PREFIX: &str = "/.sftp-archive/";

pub fn encode_archive_path(host: &str, remote_path: &str) -> std::path::PathBuf {
    let trimmed = remote_path.trim_start_matches('/');
    std::path::PathBuf::from(format!("{SFTP_ARCHIVE_PREFIX}{host}/{trimmed}"))
}

pub fn decode_archive_path(path: &Path) -> Option<(String, String)> {
    let s = path.to_str()?;
    let rest = s.strip_prefix(SFTP_ARCHIVE_PREFIX)?;
    let slash = rest.find('/')?;
    let host = rest[..slash].to_string();
    let remote = format!("/{}", &rest[slash + 1..]);
    Some((host, remote))
}

pub struct SshHostConfig {
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_files: Vec<String>,
}

/// Parse `~/.ssh/config` for Host/Hostname/User/Port/IdentityFile.
pub fn parse_ssh_config(content: &str) -> HashMap<String, SshHostConfig> {
    let mut hosts: HashMap<String, SshHostConfig> = HashMap::new();
    let mut current_hosts: Vec<String> = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // Split on first whitespace or '='
        let (key, value) = if let Some(eq) = trimmed.find('=') {
            let (k, v) = trimmed.split_at(eq);
            (k.trim(), v[1..].trim())
        } else if let Some(sp) = trimmed.find(char::is_whitespace) {
            let (k, v) = trimmed.split_at(sp);
            (k.trim(), v.trim())
        } else {
            continue;
        };

        match key.to_ascii_lowercase().as_str() {
            "host" => {
                current_hosts.clear();
                for h in value.split_whitespace() {
                    if h.contains('*') || h.contains('?') {
                        continue;
                    }
                    current_hosts.push(h.to_string());
                    hosts.entry(h.to_string()).or_insert_with(|| SshHostConfig {
                        hostname: None,
                        user: None,
                        port: None,
                        identity_files: Vec::new(),
                    });
                }
            }
            "hostname" => {
                for h in &current_hosts {
                    if let Some(cfg) = hosts.get_mut(h) {
                        cfg.hostname = Some(value.to_string());
                    }
                }
            }
            "user" => {
                for h in &current_hosts {
                    if let Some(cfg) = hosts.get_mut(h) {
                        cfg.user = Some(value.to_string());
                    }
                }
            }
            "port" => {
                if let Ok(port) = value.parse::<u16>() {
                    for h in &current_hosts {
                        if let Some(cfg) = hosts.get_mut(h) {
                            cfg.port = Some(port);
                        }
                    }
                }
            }
            "identityfile" => {
                let expanded = expand_tilde(value);
                for h in &current_hosts {
                    if let Some(cfg) = hosts.get_mut(h) {
                        cfg.identity_files.push(expanded.clone());
                    }
                }
            }
            _ => {}
        }
    }
    hosts
}

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return format!("{}/{rest}", home.display());
        }
    }
    path.to_string()
}

pub(crate) fn home_dir() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    {
        std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .ok()
            .map(std::path::PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var("HOME").ok().map(std::path::PathBuf::from)
    }
}

/// Connect to an SSH host using config resolution. Offers ssh-agent keys first,
/// then key files from the config and the default locations.
pub fn connect(
    host: &str,
    ssh_config: &HashMap<String, SshHostConfig>,
) -> Result<SftpSession, String> {
    let config = ssh_config.get(host);
    let hostname = config
        .and_then(|c| c.hostname.as_deref())
        .unwrap_or(host)
        .to_string();
    let user = config
        .and_then(|c| c.user.as_deref())
        .map(|s| s.to_string())
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "root".to_string());
    let port = config.and_then(|c| c.port).unwrap_or(22);

    let mut identity_files: Vec<String> =
        config.map(|c| c.identity_files.clone()).unwrap_or_default();
    if let Some(home) = home_dir() {
        for default in &["id_ed25519", "id_rsa", "id_ecdsa"] {
            let path = home.join(".ssh").join(default);
            let path = path.to_string_lossy().into_owned();
            if !identity_files.contains(&path) {
                identity_files.push(path);
            }
        }
    }
    identity_files.retain(|p| Path::new(p).exists());

    let conn = ssh::connect(ssh::ConnectParams {
        host: host.to_string(),
        hostname,
        port,
        user,
        identity_files,
        use_agent: true,
    })
    .map_err(|e| e.message)?;

    let home_dir = conn.home_dir.clone();
    Ok(SftpSession {
        sftp: Arc::new(conn),
        host: host.to_string(),
        home_dir,
    })
}

/// List a remote directory, producing DirEntry items with EntryLocation::Remote.
/// Does not include ".." when path is "/".
pub fn read_directory(conn: &Conn, host: &str, path: &str) -> Result<Vec<DirEntry>, String> {
    let mut all = Vec::new();
    read_directory_streaming(conn, host, path, |entries| {
        all.extend(entries);
    })
    .map_err(|(msg, _)| msg)?;
    Ok(all)
}

/// Incrementally list a remote directory, calling `on_batch` for each batch of entries.
/// The first batch always contains the ".." entry (if applicable).
/// Entries within each batch are unsorted; the final sort is the caller's responsibility.
/// Returns `Err((message, is_connection_error))`.
/// `is_connection_error = true` means the SSH session is likely dead (timeout, disconnect).
/// `is_connection_error = false` means an SFTP-level error (permission denied, etc.)
pub fn read_directory_streaming(
    conn: &Conn,
    host: &str,
    path: &str,
    mut on_batch: impl FnMut(Vec<DirEntry>),
) -> Result<(), (String, bool)> {
    let remote_path = if path.is_empty() { "/" } else { path };
    let handle = conn
        .open_dir(remote_path)
        .map_err(|e| (e.message.clone(), e.fatal))?;

    // First batch: ".." entry if not at root
    if remote_path != "/" {
        let parent = parent_remote_path(remote_path);
        on_batch(vec![DirEntry {
            name: "..".to_string(),
            is_dir: true,
            is_symlink: false,
            link_target: None,
            location: EntryLocation::Remote {
                host: host.to_string(),
                path: parent,
            },
            size: None,
            modified: None,
        }]);
    }

    let result = (|| -> Result<(), (String, bool)> {
        // The server chooses the batch size; each round trip yields one batch.
        while let Some(items) = conn
            .read_dir(handle)
            .map_err(|e| (format!("readdir {remote_path}: {e}"), e.fatal))?
        {
            let mut batch = Vec::with_capacity(items.len());
            for item in items {
                if item.name.is_empty() || item.name == "." || item.name == ".." {
                    continue;
                }
                let inner_path = join_remote(remote_path, &item.name);
                let is_symlink = item.attrs.is_symlink();
                // A symlink's own attributes describe the link, so resolve the
                // target to decide whether it behaves as a directory.
                let (is_dir, size) = if is_symlink {
                    match conn.stat(&inner_path) {
                        Ok(t) => (t.is_dir(), if t.is_dir() { None } else { t.size }),
                        Err(_) => (false, item.attrs.size),
                    }
                } else {
                    let d = item.attrs.is_dir();
                    (d, if d { None } else { item.attrs.size })
                };
                let link_target = if is_symlink {
                    conn.readlink(&inner_path).ok()
                } else {
                    None
                };
                batch.push(DirEntry {
                    name: item.name,
                    is_dir,
                    is_symlink,
                    link_target,
                    location: EntryLocation::Remote {
                        host: host.to_string(),
                        path: inner_path,
                    },
                    size,
                    modified: item.attrs.mtime.map(u64::from),
                });
            }
            if !batch.is_empty() {
                on_batch(batch);
            }
        }
        Ok(())
    })();

    // Close even after a failure: the listing may just be partial.
    let _ = conn.close(handle);
    result
}

/// Joins a remote directory and an entry name, without doubling the separator.
fn join_remote(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else {
        format!("{}/{name}", dir.trim_end_matches('/'))
    }
}

/// Lists a directory's raw entries, skipping "." and "..".
fn list_dir(conn: &Conn, path: &str) -> Result<Vec<ssh::DirItem>, String> {
    let handle = conn.open_dir(path).map_err(|e| e.message)?;
    let mut all = Vec::new();
    let result = (|| -> Result<(), String> {
        while let Some(items) = conn
            .read_dir(handle)
            .map_err(|e| format!("readdir {path}: {e}"))?
        {
            all.extend(
                items
                    .into_iter()
                    .filter(|i| i.name != "." && i.name != ".." && !i.name.is_empty()),
            );
        }
        Ok(())
    })();
    let _ = conn.close(handle);
    result.map(|()| all)
}

/// Read an entire remote file into memory.
pub fn read_file_full(conn: &Conn, path: &str) -> Result<Vec<u8>, String> {
    read_file_full_progress(conn, path, None)
}

/// Read an entire remote file into memory with progress reporting.
pub fn read_file_full_progress(
    conn: &Conn,
    path: &str,
    progress: Option<&crate::core::TransferProgress>,
) -> Result<Vec<u8>, String> {
    let size = conn.stat(path).ok().and_then(|s| s.size);
    if let Some(p) = progress {
        p.reset(size.unwrap_or(0));
    }
    let handle = conn.open(path, OpenMode::Read).map_err(|e| e.message)?;
    let mut buf = Vec::new();
    let mut offset = 0u64;
    let result = (|| -> Result<(), String> {
        loop {
            let chunk = conn
                .read_at(handle, offset, ssh::CHUNK)
                .map_err(|e| format!("read {path}: {e}"))?;
            if chunk.is_empty() {
                return Ok(());
            }
            offset += chunk.len() as u64;
            if let Some(p) = progress {
                p.add(chunk.len() as u64);
            }
            buf.extend_from_slice(&chunk);
        }
    })();
    let _ = conn.close(handle);
    result.map(|()| buf)
}

/// Read a prefix of a remote file for preview purposes.
pub fn read_bytes_prefix(conn: &Conn, path: &str, max_bytes: usize) -> Result<Vec<u8>, String> {
    let handle = conn.open(path, OpenMode::Read).map_err(|e| e.message)?;
    let mut buf = Vec::new();
    let result = (|| -> Result<(), String> {
        while buf.len() < max_bytes {
            let want = (max_bytes - buf.len()).min(ssh::CHUNK);
            let chunk = conn
                .read_at(handle, buf.len() as u64, want)
                .map_err(|e| format!("read {path}: {e}"))?;
            if chunk.is_empty() {
                break;
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(())
    })();
    let _ = conn.close(handle);
    buf.truncate(max_bytes);
    result.map(|()| buf)
}

/// Open a remote file as a seekable reader (for streaming preview and archives).
pub fn open_remote_reader(conn: &Arc<Conn>, path: &str) -> Result<RemoteFile, String> {
    RemoteFile::open(conn.clone(), path).map_err(|e| e.message)
}

/// Recursively delete a remote path (file or directory).
/// Reports each deleted item via `progress.add_item()` when provided.
pub fn recursive_delete(
    conn: &Conn,
    path: &str,
    is_dir: bool,
    progress: Option<&crate::core::TransferProgress>,
) -> Result<(), String> {
    if is_dir {
        for child in list_dir(conn, path)? {
            let child_path = join_remote(path, &child.name);
            recursive_delete(conn, &child_path, child.attrs.is_dir(), progress)?;
        }
        conn.rmdir(path).map_err(|e| e.message)?;
    } else {
        conn.remove(path).map_err(|e| e.message)?;
    }
    if let Some(p) = progress {
        p.add_item();
    }
    Ok(())
}

/// Write bytes to a remote file (create or overwrite).
pub fn write_file(conn: &Conn, path: &str, contents: &[u8]) -> Result<(), String> {
    let handle = conn.open(path, OpenMode::Write).map_err(|e| e.message)?;
    let result = (|| -> Result<(), String> {
        let mut offset = 0u64;
        for chunk in contents.chunks(ssh::CHUNK) {
            conn.write_at(handle, offset, chunk.to_vec())
                .map_err(|e| format!("write {path}: {e}"))?;
            offset += chunk.len() as u64;
        }
        Ok(())
    })();
    let closed = conn.close(handle).map_err(|e| format!("close {path}: {e}"));
    result.and(closed)
}

/// Create a remote directory.
pub fn mkdir(conn: &Conn, path: &str) -> Result<(), String> {
    conn.mkdir(path).map_err(|e| e.message)
}

/// Copy a file within the same remote host (read then write).
pub fn copy_remote_remote(conn: &Conn, src_path: &str, dst_path: &str) -> Result<(), String> {
    copy_remote(conn, src_path, dst_path)
}

/// Recursively copy a file or directory within the same remote host.
pub fn recursive_copy_remote(
    conn: &Conn,
    src_path: &str,
    dst_dir: &str,
    name: &str,
) -> Result<(), String> {
    let dst_path = join_remote(dst_dir, name);
    let stat = conn.stat(src_path).map_err(|e| e.message)?;
    if stat.is_dir() {
        conn.mkdir(&dst_path).map_err(|e| e.message)?;
        for child in list_dir(conn, src_path)? {
            let child_src = join_remote(src_path, &child.name);
            recursive_copy_remote(conn, &child_src, &dst_path, &child.name)?;
        }
    } else {
        copy_remote(conn, src_path, &dst_path)?;
    }
    Ok(())
}

/// A `Read` wrapper that tracks transferred bytes and checks a cancel flag.
struct TrackedReader<'a, R: Read> {
    inner: R,
    cancel: &'a AtomicBool,
    progress: Option<&'a crate::core::TransferProgress>,
}
impl<R: Read> Read for TrackedReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::other("Cancelled"));
        }
        let n = self.inner.read(buf)?;
        if let Some(p) = self.progress {
            p.add(n as u64);
        }
        Ok(n)
    }
}

/// A `Write` wrapper that tracks transferred bytes and checks a cancel flag.
struct TrackedWriter<'a, W: Write> {
    inner: W,
    cancel: &'a AtomicBool,
    progress: Option<&'a crate::core::TransferProgress>,
}
impl<W: Write> Write for TrackedWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::other("Cancelled"));
        }
        let n = self.inner.write(buf)?;
        if let Some(p) = self.progress {
            p.add(n as u64);
        }
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn is_cancel_err(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::Other && e.to_string() == "Cancelled"
}

/// Splits a remote path into its parent directory and final component.
fn split_remote(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(0) => ("/", &path[1..]),
        Some(pos) => (&path[..pos], &path[pos + 1..]),
        None => (".", path),
    }
}

/// Copy a remote directory tree to a local path.
/// Runs `tar cf -` on the remote via SSH exec, extracts locally with the Rust `tar` crate.
pub fn copy_remote_dir_to_local_via_tar(
    conn: &Arc<Conn>,
    src_path: &str,
    dst_dir: &std::path::Path,
    name: &str,
    cancel: &AtomicBool,
    progress: Option<&crate::core::TransferProgress>,
) -> Result<(), String> {
    let (src_parent, src_name) = split_remote(src_path);

    let src_cmd = format!(
        "tar cf - -C {} {}",
        sh_quote(src_parent),
        sh_quote(src_name)
    );
    let stream = conn
        .exec_stream(&src_cmd, ssh::Stdin::Closed)
        .map_err(|e| e.message)?;

    let reader = TrackedReader {
        inner: io::BufReader::with_capacity(1 << 20, stream),
        cancel,
        progress,
    };
    let mut archive = tar::Archive::new(reader);
    archive.unpack(dst_dir).map_err(|e| {
        if e.get_ref().is_some_and(|s| {
            is_cancel_err(
                s.downcast_ref::<io::Error>()
                    .unwrap_or(&io::Error::other("")),
            )
        }) {
            "Cancelled".to_string()
        } else {
            format!("tar extract: {e}")
        }
    })?;

    if name != src_name {
        std::fs::rename(dst_dir.join(src_name), dst_dir.join(name))
            .map_err(|e| format!("rename: {e}"))?;
    }
    Ok(())
}

/// Runs a command that should print nothing, treating any stderr as failure.
///
/// sunset surfaces a channel's exit status as a session-wide event that cannot
/// be tied back to one channel, so unlike the libssh2 version this reads the
/// command's stderr rather than its exit code.
fn exec_checked(conn: &Conn, cmd: &str, what: &str) -> Result<(), String> {
    let out = conn.exec(cmd).map_err(|e| format!("{what}: {e}"))?;
    check_exec(&out, what)
}

/// Judges a finished command by its exit status, and by stderr only when the
/// server did not send one.
///
/// Going by stderr alone would fail a copy over any warning the command
/// printed while still succeeding.
fn check_exec(out: &ssh::ExecOutput, what: &str) -> Result<(), String> {
    let tail = String::from_utf8_lossy(&out.stderr);
    let tail = tail.trim();
    match out.exit {
        Some(ref status) if out.failed() => {
            if tail.is_empty() {
                Err(format!("{what}: {status}"))
            } else {
                Err(format!("{what}: {status}: {tail}"))
            }
        }
        None if !tail.is_empty() => Err(format!("{what}: {tail}")),
        _ => Ok(()),
    }
}

/// Feeds a tar archive into a remote `tar x` and reports what it complained about.
///
/// `write_archive` writes the archive; closing the input afterwards is what
/// lets the remote tar finish, so it happens whether or not writing succeeded.
fn pipe_archive_to_tar(
    conn: &Arc<Conn>,
    dst_dir: &str,
    tar_flags: &str,
    write_archive: impl FnOnce(&mut ssh::ExecStream) -> io::Result<()>,
) -> Result<(), String> {
    let cmd = format!("tar {tar_flags} -C {}", sh_quote(dst_dir));
    let mut stream = conn
        .exec_stream(&cmd, ssh::Stdin::Piped)
        .map_err(|e| e.message)?;

    let written = write_archive(&mut stream);
    // Signals end of input, so the remote tar can finish and exit.
    stream.finish_input();
    let outcome = stream.wait();

    match written {
        Ok(()) => {}
        Err(e) if is_cancel_err(&e) => return Err("Cancelled".to_string()),
        Err(e) => return Err(format!("tar create: {e}")),
    }
    check_exec(
        &outcome.map_err(|e| format!("remote tar: {e}"))?,
        "remote tar",
    )
}

/// Copy a local directory tree to a remote path.
/// Creates the tar archive with the Rust `tar` crate, extracts it remotely
/// via `tar xf -`.
pub fn copy_local_dir_to_remote_via_tar(
    src_path: &std::path::Path,
    conn: &Arc<Conn>,
    dst_dir: &str,
    cancel: &AtomicBool,
    progress: Option<&crate::core::TransferProgress>,
) -> Result<(), String> {
    let src_name = src_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("dir");

    pipe_archive_to_tar(conn, dst_dir, "xf -", |stream| {
        let mut writer = TrackedWriter {
            inner: io::BufWriter::with_capacity(1 << 20, stream),
            cancel,
            progress,
        };
        let mut ar = tar::Builder::new(&mut writer);
        ar.append_dir_all(src_name, src_path)?;
        ar.finish()?;
        drop(ar);
        writer.flush()
    })
}

/// Shell-quote a string with single quotes, escaping any internal single quotes.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Return the total byte size of a remote path via SSH exec.
/// Far faster than recursive SFTP readdir for large trees (one round-trip vs O(dirs)).
///
/// Strategy (in order):
///   1. `du -sb`  — Linux/GNU coreutils: exact bytes.
///   2. `du -sk`  — macOS/BSD POSIX du: 1 KiB blocks → multiply by 1024.
///   3. Return 0  — Windows SSH or other exotic remote; progress bar shows animated form.
pub fn count_bytes_via_exec(conn: &Conn, path: &str) -> u64 {
    let quoted = sh_quote(path);
    for (cmd, scale) in [
        (format!("du -sb {quoted} 2>/dev/null"), 1u64),
        (format!("du -sk {quoted} 2>/dev/null"), 1024u64),
    ] {
        let Ok(out) = conn.exec(&cmd) else { continue };
        // du output: "12345\t/path/name\n" — first token is the numeric value.
        // A failed du prints nothing (stderr is redirected away), so a parse
        // failure is what selects the next form.
        if let Some(n) = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .next()
            .and_then(|s| s.parse::<u64>().ok())
        {
            return n * scale;
        }
    }
    0
}

/// Return the total byte size of all regular files under `path` on the local filesystem.
pub fn count_bytes_local(path: &std::path::Path) -> u64 {
    match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => std::fs::read_dir(path)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .map(|e| count_bytes_local(&e.path()))
                    .sum()
            })
            .unwrap_or(0),
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

/// Copy a file or directory tree between two different remote hosts using a single
/// `tar cf -` → relay → `tar xf -` stream.  This avoids per-file SFTP round-trips.
pub fn copy_cross_host_via_tar(
    src_conn: &Arc<Conn>,
    src_path: &str,
    dst_conn: &Arc<Conn>,
    dst_dir: &str,
    name: &str,
    cancel: &AtomicBool,
    progress: Option<&crate::core::TransferProgress>,
) -> Result<(), String> {
    let (src_parent, src_name) = split_remote(src_path);

    // Use gzip compression to reduce bandwidth — data transits two SSH
    // connections (source → local → destination).
    let src_cmd = format!(
        "tar czf - -C {} {}",
        sh_quote(src_parent),
        sh_quote(src_name)
    );
    let mut src = src_conn
        .exec_stream(&src_cmd, ssh::Stdin::Closed)
        .map_err(|e| e.message)?;

    // Relay the compressed stream straight into the destination's tar.
    pipe_archive_to_tar(dst_conn, dst_dir, "xzf -", |stream| {
        let mut out = io::BufWriter::with_capacity(1 << 20, stream);
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(io::Error::other("Cancelled"));
            }
            match src.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    out.write_all(&buf[..n])?;
                    if let Some(p) = progress {
                        p.add(n as u64);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        out.flush()
    })?;

    // Rename on destination if the target name differs from the source name.
    if name != src_name {
        let mv_cmd = format!(
            "mv {} {}",
            sh_quote(&join_remote(dst_dir, src_name)),
            sh_quote(&join_remote(dst_dir, name)),
        );
        exec_checked(dst_conn, &mv_cmd, "mv")?;
    }

    Ok(())
}

/// Count the total byte size of a remote path (file or directory tree).
pub fn count_bytes_remote(conn: &Conn, path: &str) -> u64 {
    match conn.stat(path) {
        Ok(stat) if stat.is_dir() => list_dir(conn, path)
            .unwrap_or_default()
            .into_iter()
            .map(|child| count_bytes_remote(conn, &join_remote(path, &child.name)))
            .sum(),
        Ok(stat) => stat.size.unwrap_or(0),
        Err(_) => 0,
    }
}

/// Rename a remote file or directory.
pub fn rename(conn: &Conn, src: &str, dst: &str) -> Result<(), String> {
    conn.rename(src, dst).map_err(|e| e.message)
}

/// Copy a remote file to a local path.
pub fn copy_remote_to_local(
    conn: &Conn,
    remote_path: &str,
    local_dst: &Path,
) -> Result<(), String> {
    copy_remote_to_local_progress(conn, remote_path, local_dst, None, None)
}

pub fn copy_remote_to_local_progress(
    conn: &Conn,
    remote_path: &str,
    local_dst: &Path,
    cancel: Option<&AtomicBool>,
    progress: Option<&crate::core::TransferProgress>,
) -> Result<(), String> {
    let expected_size = conn.stat(remote_path).ok().and_then(|s| s.size);
    if let Some(p) = progress {
        p.reset(expected_size.unwrap_or(0));
    }
    // The copy confirmation prompt already warned about (and got consent for)
    // replacing any existing destination, so write straight to it. On
    // error/cancel the partial file is removed.
    let inner = || -> Result<(), String> {
        let handle = conn
            .open(remote_path, OpenMode::Read)
            .map_err(|e| e.message)?;
        let copied = (|| -> Result<u64, String> {
            let mut local_file = std::fs::File::create(local_dst)
                .map_err(|e| format!("create local {}: {e}", local_dst.display()))?;
            let mut written: u64 = 0;
            loop {
                if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                    return Err("Cancelled".to_string());
                }
                let chunk = conn
                    .read_at(handle, written, ssh::CHUNK)
                    .map_err(|e| format!("read remote: {e}"))?;
                if chunk.is_empty() {
                    break;
                }
                local_file
                    .write_all(&chunk)
                    .map_err(|e| format!("write local: {e}"))?;
                written += chunk.len() as u64;
                if let Some(p) = progress {
                    p.add(chunk.len() as u64);
                }
            }
            // Flush before the size check below.
            local_file
                .sync_all()
                .map_err(|e| format!("sync local: {e}"))?;
            Ok(written)
        })();
        let _ = conn.close(handle);
        let written = copied?;
        if let Some(expected) = expected_size
            && written != expected
        {
            return Err(format!(
                "size mismatch: wrote {written} bytes, expected {expected}"
            ));
        }
        Ok(())
    };
    match inner() {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(local_dst);
            Err(e)
        }
    }
}

/// Copy a local file to a remote path.
pub fn copy_local_to_remote(
    conn: &Conn,
    local_src: &Path,
    remote_path: &str,
) -> Result<(), String> {
    copy_local_to_remote_progress(conn, local_src, remote_path, None, None)
}

pub fn copy_local_to_remote_progress(
    conn: &Conn,
    local_src: &Path,
    remote_path: &str,
    cancel: Option<&AtomicBool>,
    progress: Option<&crate::core::TransferProgress>,
) -> Result<(), String> {
    let expected_size = std::fs::metadata(local_src).map(|m| m.len()).ok();
    if let Some(p) = progress {
        p.reset(expected_size.unwrap_or(0));
    }
    // Inner closure pattern (mirror of copy_remote_to_local_progress): any
    // Err triggers cleanup of the partial remote artifact before the error
    // propagates to the worker.
    let inner = || -> Result<(), String> {
        let mut local_file = std::fs::File::open(local_src)
            .map_err(|e| format!("open local {}: {e}", local_src.display()))?;
        let handle = conn
            .open(remote_path, OpenMode::Write)
            .map_err(|e| format!("create remote {remote_path}: {e}"))?;
        let sent = (|| -> Result<u64, String> {
            let mut buf = vec![0u8; ssh::CHUNK];
            let mut written: u64 = 0;
            loop {
                if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                    return Err("Cancelled".to_string());
                }
                match local_file.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        conn.write_at(handle, written, buf[..n].to_vec())
                            .map_err(|e| format!("write remote: {e}"))?;
                        written += n as u64;
                        if let Some(p) = progress {
                            p.add(n as u64);
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(format!("read local: {e}")),
                }
            }
            Ok(written)
        })();
        // Close so the server flushes before the size is checked.
        conn.close(handle)
            .map_err(|e| format!("close remote {remote_path}: {e}"))?;
        let written = sent?;
        if let Some(expected) = expected_size {
            if written != expected {
                return Err(format!(
                    "size mismatch: read {written} bytes, expected {expected}"
                ));
            }
            // Cross-check the destination — guards against a server-side
            // short-write that the SFTP layer didn't already surface as
            // a write error.
            let actual = conn
                .stat(remote_path)
                .map_err(|e| format!("stat remote after copy: {e}"))?
                .size
                .unwrap_or(0);
            if actual != expected {
                return Err(format!(
                    "size mismatch on remote: {actual} bytes, expected {expected}"
                ));
            }
        }
        Ok(())
    };
    match inner() {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = conn.remove(remote_path);
            Err(e)
        }
    }
}

/// Copy a remote file to another path on the same host.
pub fn copy_remote(conn: &Conn, src: &str, dst: &str) -> Result<(), String> {
    let src_h = conn
        .open(src, OpenMode::Read)
        .map_err(|e| format!("open {src}: {e}"))?;
    let dst_h = match conn.open(dst, OpenMode::Write) {
        Ok(h) => h,
        Err(e) => {
            let _ = conn.close(src_h);
            return Err(format!("create {dst}: {e}"));
        }
    };
    let result = (|| -> Result<(), String> {
        let mut offset = 0u64;
        loop {
            let chunk = conn
                .read_at(src_h, offset, ssh::CHUNK)
                .map_err(|e| format!("read {src}: {e}"))?;
            if chunk.is_empty() {
                return Ok(());
            }
            let n = chunk.len() as u64;
            conn.write_at(dst_h, offset, chunk)
                .map_err(|e| format!("write {dst}: {e}"))?;
            offset += n;
        }
    })();
    let _ = conn.close(src_h);
    let closed = conn.close(dst_h).map_err(|e| format!("close {dst}: {e}"));
    result.and(closed)
}

/// Copies a file's permission bits, so an executable stays executable.
pub fn copy_permissions(conn: &Conn, attrs: FileAttrs, dst: &str) -> Result<(), String> {
    match attrs.permissions {
        Some(mode) => conn
            .set_stat(
                dst,
                FileAttrs {
                    permissions: Some(mode & 0o7777),
                    ..FileAttrs::default()
                },
            )
            .map_err(|e| format!("chmod {dst}: {e}")),
        None => Ok(()),
    }
}

fn parent_remote_path(path: &str) -> String {
    if path == "/" || path.is_empty() {
        return "/".to_string();
    }
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) => "/".to_string(),
        Some(pos) => trimmed[..pos].to_string(),
        None => "/".to_string(),
    }
}

/// Parse SSH hosts from ~/.ssh/config (cross-platform).
pub fn discover_ssh_hosts() -> Vec<String> {
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return Vec::new(),
    };
    let config_path = std::path::Path::new(&home).join(".ssh/config");
    let content = match std::fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut hosts = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Host ") {
            for host in rest.split_whitespace() {
                if !host.contains('*') && !host.contains('?') {
                    hosts.push(host.to_string());
                }
            }
        }
    }
    hosts
}

/// Load and parse the SSH config file once.
pub fn load_ssh_config() -> HashMap<String, SshHostConfig> {
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return HashMap::new(),
    };
    let config_path = std::path::Path::new(&home).join(".ssh/config");
    match std::fs::read_to_string(&config_path) {
        Ok(content) => parse_ssh_config(&content),
        Err(_) => HashMap::new(),
    }
}
