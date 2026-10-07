use std::{
    collections::HashSet,
    fs,
    io::{self, Read, Seek, Write},
    path::{self, Path},
};

use crate::core::{DirEntry, EntryLocation, format_size};

const ARCHIVE_READ_BUFFER: usize = 1024 * 1024;

/// Run `f` with a `Read + Seek` handle to the archive. For local paths this
/// opens a buffered file; for synthetic SFTP archive paths it locks the SFTP
/// session for the host and streams over SFTP.
pub fn with_seek_reader<R, F>(archive_path: &Path, f: F) -> io::Result<R>
where
    F: FnOnce(&mut (dyn ReadSeek + '_)) -> io::Result<R>,
{
    if let Some((host, remote_path)) = crate::sftp::decode_archive_path(archive_path) {
        let session = crate::sftp::get_session(&host)
            .ok_or_else(|| io::Error::other(format!("no active SFTP session for host {host}")))?;
        let locked = session
            .lock()
            .map_err(|_| io::Error::other("session mutex poisoned"))?;
        let file = crate::sftp::open_remote_reader(&locked.sftp, &remote_path)
            .map_err(io::Error::other)?;
        // Buffer the remote handle: the zip reader parses the central directory
        // with many tiny field-sized reads, and each unbuffered read is a
        // separate SFTP round-trip. Buffering collapses them into 1 MB fills.
        let mut reader = io::BufReader::with_capacity(ARCHIVE_READ_BUFFER, file);
        f(&mut reader)
    } else {
        let file = fs::File::open(archive_path)?;
        let mut reader = io::BufReader::with_capacity(ARCHIVE_READ_BUFFER, file);
        f(&mut reader)
    }
}

/// Sequential-read variant. For compressed tar variants this wraps the inner
/// reader with the decompressor.
pub fn with_reader<R, F>(archive_path: &Path, f: F) -> io::Result<R>
where
    F: FnOnce(Box<dyn Read + '_>) -> io::Result<R>,
{
    if let Some((host, remote_path)) = crate::sftp::decode_archive_path(archive_path) {
        let session = crate::sftp::get_session(&host)
            .ok_or_else(|| io::Error::other(format!("no active SFTP session for host {host}")))?;
        let locked = session
            .lock()
            .map_err(|_| io::Error::other("session mutex poisoned"))?;
        let file = crate::sftp::open_remote_reader(&locked.sftp, &remote_path)
            .map_err(io::Error::other)?;
        f(Box::new(file))
    } else {
        let file = fs::File::open(archive_path)?;
        let reader = io::BufReader::with_capacity(ARCHIVE_READ_BUFFER, file);
        f(Box::new(reader))
    }
}

pub trait ReadSeek: Read + Seek {}
impl<T: Read + Seek + ?Sized> ReadSeek for T {}

pub trait ContainerPlugin: Sync {
    fn kind(&self) -> ContainerKind;
    fn scheme(&self) -> &'static str;
    fn matches_path(&self, path: &Path) -> bool;
    fn read_dir(&self, archive_path: &Path, cwd: &str) -> anyhow::Result<Vec<DirEntry>>;
    fn read_bytes_prefix(
        &self,
        archive_path: &Path,
        inner_path: &str,
        max_bytes: usize,
    ) -> anyhow::Result<Vec<u8>>;
    fn read_metadata(
        &self,
        archive_path: &Path,
        inner_path: &str,
    ) -> anyhow::Result<Option<(u64, Option<u32>)>>;
}

struct ZipPlugin;
struct TarPlugin;
struct TarGzPlugin;
struct TarBz2Plugin;

static ZIP_PLUGIN: ZipPlugin = ZipPlugin;
static TAR_PLUGIN: TarPlugin = TarPlugin;
static TAR_GZ_PLUGIN: TarGzPlugin = TarGzPlugin;
static TAR_BZ2_PLUGIN: TarBz2Plugin = TarBz2Plugin;

fn container_plugins() -> &'static [&'static dyn ContainerPlugin] {
    static PLUGINS: [&dyn ContainerPlugin; 4] =
        [&ZIP_PLUGIN, &TAR_PLUGIN, &TAR_GZ_PLUGIN, &TAR_BZ2_PLUGIN];
    &PLUGINS
}

fn plugin_for_kind(kind: ContainerKind) -> &'static dyn ContainerPlugin {
    for plugin in container_plugins() {
        if plugin.kind() == kind {
            return *plugin;
        }
    }
    &ZIP_PLUGIN
}

pub fn container_display_path(
    kind: ContainerKind,
    archive_path: &Path,
    inner_path: &str,
) -> String {
    let _ = kind;
    let base = if let Some((host, remote)) = crate::sftp::decode_archive_path(archive_path) {
        format!("/sftp/{host}{remote}")
    } else {
        archive_path.to_string_lossy().to_string()
    };
    if inner_path.is_empty() {
        base
    } else {
        format!("{base}/{inner_path}")
    }
}

pub fn container_kind_from_path(path: &Path) -> Option<ContainerKind> {
    container_plugins()
        .iter()
        .find(|plugin| plugin.matches_path(path))
        .map(|plugin| plugin.kind())
}

pub fn is_container_path(p: &Path) -> bool {
    container_kind_from_path(p).is_some()
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContainerKind {
    Zip,
    Tar,
    TarGz,
    TarBz2,
}

pub fn copy_container_entry(
    kind: ContainerKind,
    archive_path: &Path,
    inner_path: &str,
    dst_dir: &Path,
    display_name: &str,
) -> io::Result<()> {
    match kind {
        ContainerKind::Zip => copy_zip_entry(archive_path, inner_path, dst_dir, display_name),
        ContainerKind::Tar => copy_tar_entry_plain(archive_path, inner_path, dst_dir, display_name),
        ContainerKind::TarGz => copy_tar_entry_gz(archive_path, inner_path, dst_dir, display_name),
        ContainerKind::TarBz2 => {
            copy_tar_entry_bz2(archive_path, inner_path, dst_dir, display_name)
        }
    }
}

pub fn copy_container_dir(
    kind: ContainerKind,
    archive_path: &Path,
    inner_path: &str,
    dst_dir: &Path,
    display_name: &str,
) -> io::Result<()> {
    let root = dst_dir.join(display_name);
    fs::create_dir_all(&root)?;
    match kind {
        ContainerKind::Zip => copy_zip_dir(archive_path, inner_path, &root),
        ContainerKind::Tar => copy_tar_dir_plain(archive_path, inner_path, &root),
        ContainerKind::TarGz => copy_tar_dir_gz(archive_path, inner_path, &root),
        ContainerKind::TarBz2 => copy_tar_dir_bz2(archive_path, inner_path, &root),
    }
}

fn safe_rel_path(rel: &str) -> Option<path::PathBuf> {
    let candidate = path::Path::new(rel);
    let mut out = path::PathBuf::new();
    for comp in candidate.components() {
        match comp {
            path::Component::Normal(part) => out.push(part),
            _ => return None,
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

fn copy_zip_entry(
    archive_path: &Path,
    inner_path: &str,
    dst_dir: &Path,
    display_name: &str,
) -> io::Result<()> {
    with_seek_reader(archive_path, |reader| {
        let mut zip = zip::ZipArchive::new(reader).map_err(io::Error::other)?;
        let normalized = inner_path.trim_start_matches('/');
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).map_err(io::Error::other)?;
            if entry.name() == normalized {
                let target = dst_dir.join(display_name);
                if entry.is_dir() {
                    fs::create_dir_all(&target)?;
                    return Ok(());
                }
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut out = fs::File::create(target)?;
                io::copy(&mut entry, &mut out)?;
                return Ok(());
            }
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Entry not found in zip: {}", inner_path),
        ))
    })
}

fn copy_zip_dir(archive_path: &Path, inner_path: &str, dst_root: &Path) -> io::Result<()> {
    with_seek_reader(archive_path, |reader| {
        let mut zip = zip::ZipArchive::new(reader).map_err(io::Error::other)?;
        let normalized = inner_path.trim_start_matches('/');
        let prefix = if normalized.is_empty() {
            String::new()
        } else {
            format!("{}/", normalized.trim_end_matches('/'))
        };

        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).map_err(io::Error::other)?;
            let name = entry.name();
            if !name.starts_with(&prefix) {
                continue;
            }
            let rel = &name[prefix.len()..];
            let Some(rel_path) = safe_rel_path(rel) else {
                continue;
            };
            let target = dst_root.join(rel_path);
            if entry.is_dir() {
                fs::create_dir_all(&target)?;
                continue;
            }
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut out = fs::File::create(target)?;
            io::copy(&mut entry, &mut out)?;
        }
        Ok(())
    })
}

fn copy_tar_entry_gz(
    archive_path: &Path,
    inner_path: &str,
    dst_dir: &Path,
    display_name: &str,
) -> io::Result<()> {
    with_reader(archive_path, |reader| {
        let decoder = flate2::read::GzDecoder::new(reader);
        copy_tar_entry(decoder, inner_path, dst_dir, display_name)
    })
}

fn copy_tar_entry_plain(
    archive_path: &Path,
    inner_path: &str,
    dst_dir: &Path,
    display_name: &str,
) -> io::Result<()> {
    with_reader(archive_path, |reader| {
        copy_tar_entry(reader, inner_path, dst_dir, display_name)
    })
}

fn copy_tar_entry_bz2(
    archive_path: &Path,
    inner_path: &str,
    dst_dir: &Path,
    display_name: &str,
) -> io::Result<()> {
    with_reader(archive_path, |reader| {
        let decoder = bzip2::read::BzDecoder::new(reader);
        copy_tar_entry(decoder, inner_path, dst_dir, display_name)
    })
}

fn copy_tar_entry<R: Read>(
    reader: R,
    inner_path: &str,
    dst_dir: &Path,
    display_name: &str,
) -> io::Result<()> {
    let mut archive = tar::Archive::new(reader);
    let normalized = inner_path.trim_start_matches('/');
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;
        let name = normalize_archive_path(&path);
        if name == normalized {
            let target = dst_dir.join(display_name);
            if entry.header().entry_type().is_dir() {
                fs::create_dir_all(&target)?;
                return Ok(());
            }
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut out = fs::File::create(target)?;
            io::copy(&mut entry, &mut out)?;
            return Ok(());
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("Entry not found in tar: {}", inner_path),
    ))
}

fn copy_tar_dir_gz(archive_path: &Path, inner_path: &str, dst_root: &Path) -> io::Result<()> {
    with_reader(archive_path, |reader| {
        let decoder = flate2::read::GzDecoder::new(reader);
        copy_tar_dir(decoder, inner_path, dst_root)
    })
}

fn copy_tar_dir_plain(archive_path: &Path, inner_path: &str, dst_root: &Path) -> io::Result<()> {
    with_reader(archive_path, |reader| {
        copy_tar_dir(reader, inner_path, dst_root)
    })
}

fn copy_tar_dir_bz2(archive_path: &Path, inner_path: &str, dst_root: &Path) -> io::Result<()> {
    with_reader(archive_path, |reader| {
        let decoder = bzip2::read::BzDecoder::new(reader);
        copy_tar_dir(decoder, inner_path, dst_root)
    })
}

fn copy_tar_dir<R: Read>(reader: R, inner_path: &str, dst_root: &Path) -> io::Result<()> {
    let mut archive = tar::Archive::new(reader);
    let normalized = inner_path.trim_start_matches('/');
    let prefix = if normalized.is_empty() {
        String::new()
    } else {
        format!("{}/", normalized.trim_end_matches('/'))
    };
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;
        let name = normalize_archive_path(&path);
        if !name.starts_with(&prefix) {
            continue;
        }
        let rel = &name[prefix.len()..];
        let Some(rel_path) = safe_rel_path(rel) else {
            continue;
        };
        let target = dst_root.join(rel_path);
        if entry.header().entry_type().is_dir() {
            fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = fs::File::create(target)?;
        io::copy(&mut entry, &mut out)?;
    }
    Ok(())
}

fn read_zip_directory(archive_path: &Path, cwd: &str) -> anyhow::Result<Vec<DirEntry>> {
    let (dirs, files) = with_seek_reader(archive_path, |reader| {
        let zip = zip::ZipArchive::new(reader).map_err(io::Error::other)?;
        let mut dirs: Vec<String> = Vec::new();
        let mut seen_dirs: HashSet<String> = HashSet::new();
        let mut files: Vec<String> = Vec::new();
        let mut seen_files: HashSet<String> = HashSet::new();

        let prefix = if cwd.is_empty() {
            "".to_string()
        } else {
            format!("{}/", cwd.trim_end_matches('/'))
        };

        // Names come straight from the in-memory central directory. Using
        // by_index() here would seek to each entry's local header — one SFTP
        // round-trip per entry — which is ruinous for a large remote archive.
        for name in zip.file_names() {
            if name.is_empty() || !name.starts_with(&prefix) {
                continue;
            }
            let rem = &name[prefix.len()..];
            if rem.is_empty() {
                continue;
            }
            if let Some(slash) = rem.find('/') {
                let dir = rem[..slash].to_string();
                if seen_dirs.insert(dir.clone()) {
                    dirs.push(dir);
                }
            } else if seen_files.insert(rem.to_string()) {
                files.push(rem.to_string());
            }
        }
        Ok((dirs, files))
    })?;

    let mut entries: Vec<DirEntry> = Vec::new();

    if !cwd.is_empty() {
        let parent = cwd
            .trim_end_matches('/')
            .rsplit_once('/')
            .map(|(p, _)| p.to_string())
            .unwrap_or_default();
        entries.push(DirEntry {
            name: "..".into(),
            is_dir: true,
            is_symlink: false,
            link_target: None,
            location: EntryLocation::Container {
                kind: ContainerKind::Zip,
                archive_path: archive_path.to_path_buf(),
                inner_path: parent,
            },
            size: None,
            modified: None,
        });
    } else {
        let parent = archive_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        entries.push(DirEntry {
            name: "..".into(),
            is_dir: true,
            is_symlink: false,
            link_target: None,
            location: EntryLocation::Fs(parent),
            size: None,
            modified: None,
        });
    }

    let dir_entries: Vec<DirEntry> = dirs
        .into_iter()
        .map(|d| DirEntry {
            name: d.clone(),
            is_dir: true,
            is_symlink: false,
            link_target: None,
            location: EntryLocation::Container {
                kind: ContainerKind::Zip,
                archive_path: archive_path.to_path_buf(),
                inner_path: if cwd.is_empty() {
                    d
                } else {
                    format!("{}/{}", cwd.trim_end_matches('/'), d)
                },
            },
            size: None,
            modified: None,
        })
        .collect();

    let file_entries: Vec<DirEntry> = files
        .into_iter()
        .map(|f| DirEntry {
            name: f.clone(),
            is_dir: false,
            is_symlink: false,
            link_target: None,
            location: EntryLocation::Container {
                kind: ContainerKind::Zip,
                archive_path: archive_path.to_path_buf(),
                inner_path: if cwd.is_empty() {
                    f
                } else {
                    format!("{}/{}", cwd.trim_end_matches('/'), f)
                },
            },
            size: None,
            modified: None,
        })
        .collect();
    entries.extend(dir_entries);
    entries.extend(file_entries);

    Ok(entries)
}

pub fn format_container_listing(entries: &[DirEntry], max_entries: usize) -> String {
    let mut out = String::new();
    out.push_str("Contents:\n");
    let mut count = 0usize;
    for entry in entries.iter() {
        if entry.name == ".." {
            continue;
        }
        if count >= max_entries {
            out.push_str(&format!(
                "… and {} more\n",
                entries.len().saturating_sub(count)
            ));
            break;
        }
        // Render straight from the already-loaded listing. Previously this
        // re-opened the whole archive once per entry to fetch a size — for a
        // large (especially remote) archive that meant reopening and scanning
        // the central directory hundreds of times, holding the SFTP session
        // lock throughout. The size shown here isn't worth that; use whatever
        // the listing already carries.
        let mut line = String::new();
        if let Some(size) = entry.size {
            line.push_str(&format!("{:>8} ", format_size(size)));
        } else {
            line.push_str("       - ");
        }
        line.push_str(&entry_display_name(entry));
        line.push('\n');
        out.push_str(&line);
        count += 1;
    }
    out
}

fn entry_display_name(entry: &DirEntry) -> String {
    if entry.is_dir {
        format!("{}/", entry.name)
    } else {
        entry.name.clone()
    }
}

fn read_zip_bytes_prefix(
    archive_path: &Path,
    inner_path: &str,
    max_bytes: usize,
) -> anyhow::Result<Vec<u8>> {
    let data = with_seek_reader(archive_path, |reader| {
        let mut zip = zip::ZipArchive::new(reader).map_err(io::Error::other)?;
        let normalized = inner_path.trim_start_matches('/');
        let mut found = None;
        for i in 0..zip.len() {
            let name = zip
                .by_index(i)
                .map_err(io::Error::other)?
                .name()
                .to_string();
            if name == normalized {
                found = Some(i);
                break;
            }
        }
        let mut data = Vec::new();
        if let Some(idx) = found {
            let mut zf = zip.by_index(idx).map_err(io::Error::other)?;
            zf.by_ref().take(max_bytes as u64).read_to_end(&mut data)?;
            Ok(data)
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("Entry not found in zip: {}", inner_path),
            ))
        }
    })?;
    Ok(data)
}

fn collect_tar_listing<R: Read>(
    reader: R,
    cwd: &str,
    mut on_progress: impl FnMut(usize),
) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    let mut archive = tar::Archive::new(reader);
    let mut dirs: Vec<String> = Vec::new();
    let mut seen_dirs: HashSet<String> = HashSet::new();
    let mut files: Vec<String> = Vec::new();
    let mut seen_files: HashSet<String> = HashSet::new();
    let mut seen = 0usize;
    const PROGRESS_INTERVAL: usize = 1000;
    let prefix = if cwd.is_empty() {
        "".to_string()
    } else {
        format!("{}/", cwd.trim_end_matches('/'))
    };
    for entry in archive.entries()? {
        let entry = entry?;
        let path = entry.path()?;
        let name = normalize_archive_path(&path);
        seen += 1;
        if seen.is_multiple_of(PROGRESS_INTERVAL) {
            on_progress(seen);
        }
        if name.is_empty() || !name.starts_with(&prefix) {
            continue;
        }
        let rem = &name[prefix.len()..];
        if rem.is_empty() {
            continue;
        }
        if let Some(slash) = rem.find('/') {
            let dir = rem[..slash].to_string();
            if seen_dirs.insert(dir.clone()) {
                dirs.push(dir);
            }
        } else if seen_files.insert(rem.to_string()) {
            files.push(rem.to_string());
        }
    }
    Ok((dirs, files))
}

fn read_tar_directory(archive_path: &Path, cwd: &str) -> anyhow::Result<Vec<DirEntry>> {
    let (dirs, files) = with_reader(archive_path, |reader| {
        collect_tar_listing(reader, cwd, |_| {}).map_err(io::Error::other)
    })?;

    let mut entries: Vec<DirEntry> = Vec::new();

    if !cwd.is_empty() {
        let parent = cwd
            .trim_end_matches('/')
            .rsplit_once('/')
            .map(|(p, _)| p.to_string())
            .unwrap_or_default();
        entries.push(DirEntry {
            name: "..".into(),
            is_dir: true,
            is_symlink: false,
            link_target: None,
            location: EntryLocation::Container {
                kind: ContainerKind::Tar,
                archive_path: archive_path.to_path_buf(),
                inner_path: parent,
            },
            size: None,
            modified: None,
        });
    } else {
        let parent = archive_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        entries.push(DirEntry {
            name: "..".into(),
            is_dir: true,
            is_symlink: false,
            link_target: None,
            location: EntryLocation::Fs(parent),
            size: None,
            modified: None,
        });
    }

    let dir_entries: Vec<DirEntry> = dirs
        .into_iter()
        .map(|d| DirEntry {
            name: d.clone(),
            is_dir: true,
            is_symlink: false,
            link_target: None,
            location: EntryLocation::Container {
                kind: ContainerKind::Tar,
                archive_path: archive_path.to_path_buf(),
                inner_path: if cwd.is_empty() {
                    d
                } else {
                    format!("{}/{}", cwd.trim_end_matches('/'), d)
                },
            },
            size: None,
            modified: None,
        })
        .collect();

    let file_entries: Vec<DirEntry> = files
        .into_iter()
        .map(|f| DirEntry {
            name: f.clone(),
            is_dir: false,
            is_symlink: false,
            link_target: None,
            location: EntryLocation::Container {
                kind: ContainerKind::Tar,
                archive_path: archive_path.to_path_buf(),
                inner_path: if cwd.is_empty() {
                    f
                } else {
                    format!("{}/{}", cwd.trim_end_matches('/'), f)
                },
            },
            size: None,
            modified: None,
        })
        .collect();
    entries.extend(dir_entries);
    entries.extend(file_entries);

    Ok(entries)
}

fn read_tar_gz_directory(archive_path: &Path, cwd: &str) -> anyhow::Result<Vec<DirEntry>> {
    let (dirs, files) = with_reader(archive_path, |reader| {
        let decoder = flate2::read::GzDecoder::new(reader);
        collect_tar_listing(decoder, cwd, |_| {}).map_err(io::Error::other)
    })?;

    let mut entries: Vec<DirEntry> = Vec::new();

    if !cwd.is_empty() {
        let parent = cwd
            .trim_end_matches('/')
            .rsplit_once('/')
            .map(|(p, _)| p.to_string())
            .unwrap_or_default();
        entries.push(DirEntry {
            name: "..".into(),
            is_dir: true,
            is_symlink: false,
            link_target: None,
            location: EntryLocation::Container {
                kind: ContainerKind::TarGz,
                archive_path: archive_path.to_path_buf(),
                inner_path: parent,
            },
            size: None,
            modified: None,
        });
    } else {
        let parent = archive_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        entries.push(DirEntry {
            name: "..".into(),
            is_dir: true,
            is_symlink: false,
            link_target: None,
            location: EntryLocation::Fs(parent),
            size: None,
            modified: None,
        });
    }

    let dir_entries: Vec<DirEntry> = dirs
        .into_iter()
        .map(|d| DirEntry {
            name: d.clone(),
            is_dir: true,
            is_symlink: false,
            link_target: None,
            location: EntryLocation::Container {
                kind: ContainerKind::TarGz,
                archive_path: archive_path.to_path_buf(),
                inner_path: if cwd.is_empty() {
                    d
                } else {
                    format!("{}/{}", cwd.trim_end_matches('/'), d)
                },
            },
            size: None,
            modified: None,
        })
        .collect();

    let file_entries: Vec<DirEntry> = files
        .into_iter()
        .map(|f| DirEntry {
            name: f.clone(),
            is_dir: false,
            is_symlink: false,
            link_target: None,
            location: EntryLocation::Container {
                kind: ContainerKind::TarGz,
                archive_path: archive_path.to_path_buf(),
                inner_path: if cwd.is_empty() {
                    f
                } else {
                    format!("{}/{}", cwd.trim_end_matches('/'), f)
                },
            },
            size: None,
            modified: None,
        })
        .collect();
    entries.extend(dir_entries);
    entries.extend(file_entries);

    Ok(entries)
}

fn read_tar_entry_prefix<R: Read>(
    reader: R,
    inner_path: &str,
    max_bytes: usize,
    kind_label: &str,
) -> io::Result<Vec<u8>> {
    let mut archive = tar::Archive::new(reader);
    let normalized = inner_path.trim_start_matches('/');
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;
        let name = normalize_archive_path(&path);
        if name == normalized {
            let mut data = Vec::new();
            entry
                .by_ref()
                .take(max_bytes as u64)
                .read_to_end(&mut data)?;
            return Ok(data);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("Entry not found in {kind_label}: {}", inner_path),
    ))
}

fn read_tar_bytes_prefix(
    archive_path: &Path,
    inner_path: &str,
    max_bytes: usize,
) -> anyhow::Result<Vec<u8>> {
    Ok(with_reader(archive_path, |reader| {
        read_tar_entry_prefix(reader, inner_path, max_bytes, "tar")
    })?)
}

fn read_tar_gz_bytes_prefix(
    archive_path: &Path,
    inner_path: &str,
    max_bytes: usize,
) -> anyhow::Result<Vec<u8>> {
    Ok(with_reader(archive_path, |reader| {
        let decoder = flate2::read::GzDecoder::new(reader);
        read_tar_entry_prefix(decoder, inner_path, max_bytes, "tar.gz")
    })?)
}

pub fn normalize_archive_path(path: &Path) -> String {
    use std::path::Component;
    let mut parts: Vec<String> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::Normal(seg) => {
                let s = seg.to_string_lossy();
                if !s.is_empty() {
                    parts.push(s.into_owned());
                }
            }
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
            Component::ParentDir => {
                parts.pop();
            }
        }
    }
    parts.join("/")
}

fn read_tar_bz2_directory_with_progress(
    archive_path: &Path,
    cwd: &str,
    progress: &mut dyn FnMut(usize),
) -> anyhow::Result<Vec<DirEntry>> {
    let (dirs, files) = with_reader(archive_path, |reader| {
        let decoder = bzip2::read::BzDecoder::new(reader);
        collect_tar_listing(decoder, cwd, &mut *progress).map_err(io::Error::other)
    })?;

    let mut entries: Vec<DirEntry> = Vec::new();

    if !cwd.is_empty() {
        let parent = cwd
            .trim_end_matches('/')
            .rsplit_once('/')
            .map(|(p, _)| p.to_string())
            .unwrap_or_default();
        entries.push(DirEntry {
            name: "..".into(),
            is_dir: true,
            is_symlink: false,
            link_target: None,
            location: EntryLocation::Container {
                kind: ContainerKind::TarBz2,
                archive_path: archive_path.to_path_buf(),
                inner_path: parent,
            },
            size: None,
            modified: None,
        });
    } else {
        let parent = archive_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        entries.push(DirEntry {
            name: "..".into(),
            is_dir: true,
            is_symlink: false,
            link_target: None,
            location: EntryLocation::Fs(parent),
            size: None,
            modified: None,
        });
    }

    let dir_entries: Vec<DirEntry> = dirs
        .into_iter()
        .map(|d| DirEntry {
            name: d.clone(),
            is_dir: true,
            is_symlink: false,
            link_target: None,
            location: EntryLocation::Container {
                kind: ContainerKind::TarBz2,
                archive_path: archive_path.to_path_buf(),
                inner_path: if cwd.is_empty() {
                    d
                } else {
                    format!("{}/{}", cwd.trim_end_matches('/'), d)
                },
            },
            size: None,
            modified: None,
        })
        .collect();

    let file_entries: Vec<DirEntry> = files
        .into_iter()
        .map(|f| DirEntry {
            name: f.clone(),
            is_dir: false,
            is_symlink: false,
            link_target: None,
            location: EntryLocation::Container {
                kind: ContainerKind::TarBz2,
                archive_path: archive_path.to_path_buf(),
                inner_path: if cwd.is_empty() {
                    f
                } else {
                    format!("{}/{}", cwd.trim_end_matches('/'), f)
                },
            },
            size: None,
            modified: None,
        })
        .collect();
    entries.extend(dir_entries);
    entries.extend(file_entries);

    Ok(entries)
}

fn read_tar_bz2_directory(archive_path: &Path, cwd: &str) -> anyhow::Result<Vec<DirEntry>> {
    read_tar_bz2_directory_with_progress(archive_path, cwd, &mut |_| {})
}

fn read_tar_bz2_bytes_prefix(
    archive_path: &Path,
    inner_path: &str,
    max_bytes: usize,
) -> anyhow::Result<Vec<u8>> {
    Ok(with_reader(archive_path, |reader| {
        let decoder = bzip2::read::BzDecoder::new(reader);
        read_tar_entry_prefix(decoder, inner_path, max_bytes, "tar.bz2")
    })?)
}

pub fn read_container_directory(
    kind: ContainerKind,
    archive_path: &Path,
    cwd: &str,
) -> anyhow::Result<Vec<DirEntry>> {
    plugin_for_kind(kind).read_dir(archive_path, cwd)
}

pub fn read_container_directory_with_progress(
    kind: ContainerKind,
    archive_path: &Path,
    cwd: &str,
    mut progress: impl FnMut(usize),
) -> anyhow::Result<Vec<DirEntry>> {
    match kind {
        ContainerKind::TarBz2 => {
            read_tar_bz2_directory_with_progress(archive_path, cwd, &mut progress)
        }
        _ => {
            let entries = read_container_directory(kind, archive_path, cwd)?;
            progress(entries.len());
            Ok(entries)
        }
    }
}

pub fn read_container_bytes_prefix(
    kind: ContainerKind,
    archive_path: &Path,
    inner_path: &str,
    max_bytes: usize,
) -> anyhow::Result<Vec<u8>> {
    plugin_for_kind(kind).read_bytes_prefix(archive_path, inner_path, max_bytes)
}

pub fn read_container_metadata(
    kind: ContainerKind,
    archive_path: &Path,
    inner_path: &str,
) -> anyhow::Result<Option<(u64, Option<u32>)>> {
    plugin_for_kind(kind).read_metadata(archive_path, inner_path)
}

impl ContainerPlugin for ZipPlugin {
    fn kind(&self) -> ContainerKind {
        ContainerKind::Zip
    }

    fn scheme(&self) -> &'static str {
        "zip"
    }

    fn matches_path(&self, path: &Path) -> bool {
        matches!(
            path.extension()
                .and_then(|s| s.to_str())
                .map(|s| s.to_ascii_lowercase()),
            Some(ext) if ext == "zip"
        )
    }

    fn read_dir(&self, archive_path: &Path, cwd: &str) -> anyhow::Result<Vec<DirEntry>> {
        read_zip_directory(archive_path, cwd)
    }

    fn read_bytes_prefix(
        &self,
        archive_path: &Path,
        inner_path: &str,
        max_bytes: usize,
    ) -> anyhow::Result<Vec<u8>> {
        read_zip_bytes_prefix(archive_path, inner_path, max_bytes)
    }

    fn read_metadata(
        &self,
        archive_path: &Path,
        inner_path: &str,
    ) -> anyhow::Result<Option<(u64, Option<u32>)>> {
        Ok(with_seek_reader(archive_path, |reader| {
            let mut zip = zip::ZipArchive::new(reader).map_err(io::Error::other)?;
            let normalized = inner_path.trim_start_matches('/');
            for i in 0..zip.len() {
                let entry = zip.by_index(i).map_err(io::Error::other)?;
                if entry.name() == normalized {
                    let size = entry.size();
                    let mode = entry.unix_mode();
                    return Ok(Some((size, mode)));
                }
            }
            Ok(None)
        })?)
    }
}

impl ContainerPlugin for TarPlugin {
    fn kind(&self) -> ContainerKind {
        ContainerKind::Tar
    }

    fn scheme(&self) -> &'static str {
        "tar"
    }

    fn matches_path(&self, path: &Path) -> bool {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        name.ends_with(".tar")
    }

    fn read_dir(&self, archive_path: &Path, cwd: &str) -> anyhow::Result<Vec<DirEntry>> {
        read_tar_directory(archive_path, cwd)
    }

    fn read_bytes_prefix(
        &self,
        archive_path: &Path,
        inner_path: &str,
        max_bytes: usize,
    ) -> anyhow::Result<Vec<u8>> {
        read_tar_bytes_prefix(archive_path, inner_path, max_bytes)
    }

    fn read_metadata(
        &self,
        archive_path: &Path,
        inner_path: &str,
    ) -> anyhow::Result<Option<(u64, Option<u32>)>> {
        Ok(with_reader(archive_path, |reader| {
            tar_entry_meta(reader, inner_path)
        })?)
    }
}

fn tar_entry_meta<R: Read>(reader: R, inner_path: &str) -> io::Result<Option<(u64, Option<u32>)>> {
    let mut archive = tar::Archive::new(reader);
    let normalized = inner_path.trim_start_matches('/');
    for entry in archive.entries()? {
        let entry = entry?;
        let path = entry.path()?;
        let name = normalize_archive_path(&path);
        if name == normalized {
            let size = entry.size();
            let mode = entry.header().mode().ok();
            return Ok(Some((size, mode)));
        }
    }
    Ok(None)
}

impl ContainerPlugin for TarGzPlugin {
    fn kind(&self) -> ContainerKind {
        ContainerKind::TarGz
    }

    fn scheme(&self) -> &'static str {
        "tar.gz"
    }

    fn matches_path(&self, path: &Path) -> bool {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        name.ends_with(".tar.gz") || name.ends_with(".tgz")
    }

    fn read_dir(&self, archive_path: &Path, cwd: &str) -> anyhow::Result<Vec<DirEntry>> {
        read_tar_gz_directory(archive_path, cwd)
    }

    fn read_bytes_prefix(
        &self,
        archive_path: &Path,
        inner_path: &str,
        max_bytes: usize,
    ) -> anyhow::Result<Vec<u8>> {
        read_tar_gz_bytes_prefix(archive_path, inner_path, max_bytes)
    }

    fn read_metadata(
        &self,
        archive_path: &Path,
        inner_path: &str,
    ) -> anyhow::Result<Option<(u64, Option<u32>)>> {
        Ok(with_reader(archive_path, |reader| {
            let decoder = flate2::read::GzDecoder::new(reader);
            tar_entry_meta(decoder, inner_path)
        })?)
    }
}

pub fn create_archive(
    sources: &[path::PathBuf],
    archive_path: &Path,
    kind: ContainerKind,
) -> io::Result<()> {
    // Refuse to overwrite an existing file: packing uses File::create, which
    // would silently truncate whatever is already at archive_path (e.g. an
    // unrelated file the user named by mistake, or a previous archive).
    if archive_path.symlink_metadata().is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} already exists", archive_path.display()),
        ));
    }
    match kind {
        ContainerKind::Zip => create_zip_archive(sources, archive_path),
        ContainerKind::Tar => create_tar_archive(sources, archive_path),
        ContainerKind::TarGz => create_tar_gz_archive(sources, archive_path),
        ContainerKind::TarBz2 => create_tar_bz2_archive(sources, archive_path),
    }
}

fn create_zip_archive(sources: &[path::PathBuf], archive_path: &Path) -> io::Result<()> {
    let file = fs::File::create(archive_path)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for src in sources {
        add_path_to_zip(&mut zip, src, "", options)?;
    }
    zip.finish().map_err(io::Error::other)?;
    Ok(())
}

fn add_path_to_zip<W: Write + io::Seek>(
    zip: &mut zip::ZipWriter<W>,
    path: &Path,
    prefix: &str,
    options: zip::write::SimpleFileOptions,
) -> io::Result<()> {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("file");
    let archive_name = if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    };
    if path.is_dir() {
        let dir_name = format!("{archive_name}/");
        zip.add_directory(&dir_name, options)
            .map_err(io::Error::other)?;
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            add_path_to_zip(zip, &entry.path(), &archive_name, options)?;
        }
    } else {
        zip.start_file(&archive_name, options)
            .map_err(io::Error::other)?;
        let mut file = fs::File::open(path)?;
        io::copy(&mut file, zip)?;
    }
    Ok(())
}

fn create_tar_archive(sources: &[path::PathBuf], archive_path: &Path) -> io::Result<()> {
    let file = fs::File::create(archive_path)?;
    let mut builder = tar::Builder::new(file);
    for src in sources {
        append_path_to_tar(&mut builder, src)?;
    }
    builder.finish()?;
    Ok(())
}

fn create_tar_gz_archive(sources: &[path::PathBuf], archive_path: &Path) -> io::Result<()> {
    let file = fs::File::create(archive_path)?;
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    for src in sources {
        append_path_to_tar(&mut builder, src)?;
    }
    builder.into_inner()?.finish()?;
    Ok(())
}

fn create_tar_bz2_archive(sources: &[path::PathBuf], archive_path: &Path) -> io::Result<()> {
    let file = fs::File::create(archive_path)?;
    let encoder = bzip2::write::BzEncoder::new(file, bzip2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    for src in sources {
        append_path_to_tar(&mut builder, src)?;
    }
    builder.into_inner()?.finish()?;
    Ok(())
}

fn append_path_to_tar<W: Write>(builder: &mut tar::Builder<W>, path: &Path) -> io::Result<()> {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("file");
    if path.is_dir() {
        builder.append_dir_all(name, path)?;
    } else {
        let mut file = fs::File::open(path)?;
        builder.append_file(name, &mut file)?;
    }
    Ok(())
}

/// Write a `tar` byte stream to `archive_path`.
///
/// Zip is rebuilt from an uncompressed tar, so the extension matches the
/// bytes. Tar, tar.gz, and tar.bz2 are already the finished archive.
/// An existing path is left untouched. A failed write removes the partial file.
pub fn pack_from_tar_stream<R: Read>(
    stream: R,
    archive_path: &Path,
    kind: ContainerKind,
) -> io::Result<()> {
    if archive_path.symlink_metadata().is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} already exists", archive_path.display()),
        ));
    }
    let written = match kind {
        ContainerKind::Zip => zip_from_tar(stream, archive_path),
        ContainerKind::Tar | ContainerKind::TarGz | ContainerKind::TarBz2 => {
            copy_stream_to_file(stream, archive_path)
        }
    };
    if written.is_err() {
        let _ = fs::remove_file(archive_path);
    }
    written
}

fn copy_stream_to_file<R: Read>(mut stream: R, archive_path: &Path) -> io::Result<()> {
    let mut file = fs::File::create(archive_path)?;
    io::copy(&mut stream, &mut file)?;
    Ok(())
}

fn zip_from_tar<R: Read>(reader: R, archive_path: &Path) -> io::Result<()> {
    let file = fs::File::create(archive_path)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = {
            let raw = entry.path()?;
            let Some(name) = zip_entry_name(raw.as_ref()) else {
                continue;
            };
            name
        };
        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            zip.add_directory(format!("{name}/"), options)
                .map_err(io::Error::other)?;
        } else if entry_type.is_file() {
            zip.start_file(name, options).map_err(io::Error::other)?;
            io::copy(&mut entry, &mut zip)?;
        }
    }
    zip.finish().map_err(io::Error::other)?;
    Ok(())
}

/// Archive member name using `/`. `..` and absolute paths are refused.
fn zip_entry_name(path: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for comp in path.components() {
        match comp {
            path::Component::Normal(part) => parts.push(part.to_str()?.to_string()),
            path::Component::CurDir => {}
            _ => return None,
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

impl ContainerPlugin for TarBz2Plugin {
    fn kind(&self) -> ContainerKind {
        ContainerKind::TarBz2
    }

    fn scheme(&self) -> &'static str {
        "tar.bz2"
    }

    fn matches_path(&self, path: &Path) -> bool {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        name.ends_with(".tar.bz2") || name.ends_with(".tbz") || name.ends_with(".tbz2")
    }

    fn read_dir(&self, archive_path: &Path, cwd: &str) -> anyhow::Result<Vec<DirEntry>> {
        read_tar_bz2_directory(archive_path, cwd)
    }

    fn read_bytes_prefix(
        &self,
        archive_path: &Path,
        inner_path: &str,
        max_bytes: usize,
    ) -> anyhow::Result<Vec<u8>> {
        read_tar_bz2_bytes_prefix(archive_path, inner_path, max_bytes)
    }

    fn read_metadata(
        &self,
        archive_path: &Path,
        inner_path: &str,
    ) -> anyhow::Result<Option<(u64, Option<u32>)>> {
        Ok(with_reader(archive_path, |reader| {
            let decoder = bzip2::read::BzDecoder::new(reader);
            tar_entry_meta(decoder, inner_path)
        })?)
    }
}

#[cfg(test)]
mod traversal_tests {
    use super::{normalize_archive_path, safe_rel_path};
    use std::path::Path;

    #[test]
    fn safe_rel_path_rejects_traversal_and_absolute() {
        // Parent-dir, absolute, and root-escaping paths must be refused so an
        // archive entry can never write outside the destination directory.
        assert!(safe_rel_path("../etc/passwd").is_none());
        assert!(safe_rel_path("a/../../b").is_none());
        assert!(safe_rel_path("/etc/passwd").is_none());
        assert!(safe_rel_path("..").is_none());
        assert!(safe_rel_path("").is_none());
    }

    #[test]
    fn safe_rel_path_accepts_normal_relative() {
        assert_eq!(
            safe_rel_path("a/b/c.txt"),
            Some(Path::new("a/b/c.txt").to_path_buf())
        );
        assert_eq!(
            safe_rel_path("file.txt"),
            Some(Path::new("file.txt").to_path_buf())
        );
    }

    #[test]
    fn normalize_archive_path_cannot_escape() {
        // Leading slashes are stripped and .. can never pop above the root.
        assert_eq!(
            normalize_archive_path(Path::new("/etc/passwd")),
            "etc/passwd"
        );
        assert_eq!(normalize_archive_path(Path::new("a/../../b")), "b");
        assert_eq!(normalize_archive_path(Path::new("../../x")), "x");
        assert_eq!(normalize_archive_path(Path::new("a/./b")), "a/b");
        assert_eq!(normalize_archive_path(Path::new("dir/sub/f")), "dir/sub/f");
    }
}

#[cfg(test)]
mod pack_stream_tests {
    use super::{ContainerKind, pack_from_tar_stream};
    use std::{fs, io, path::PathBuf};

    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("fileman-pack-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir(&path).unwrap();
            TmpDir(path)
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct FailRead;
    impl io::Read for FailRead {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("boom"))
        }
    }

    fn raw_entry(name: &str, body: &[u8]) -> Vec<u8> {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        header.set_size(body.len() as u64);
        let bytes = name.as_bytes();
        header.as_mut_bytes()[..bytes.len()].copy_from_slice(bytes);
        header.set_cksum();
        let mut out = header.as_bytes().to_vec();
        out.extend_from_slice(body);
        let pad = (512 - (body.len() % 512)) % 512;
        out.extend(std::iter::repeat_n(0u8, pad));
        out
    }

    fn sample_tar() -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let mut note = tar::Header::new_gnu();
        note.set_entry_type(tar::EntryType::Regular);
        note.set_mode(0o644);
        note.set_size(5);
        builder
            .append_data(&mut note, "note.txt", &b"hello"[..])
            .unwrap();
        let mut dir = tar::Header::new_gnu();
        dir.set_entry_type(tar::EntryType::Directory);
        dir.set_mode(0o755);
        dir.set_size(0);
        builder.append_data(&mut dir, "sub", io::empty()).unwrap();
        let mut inner = tar::Header::new_gnu();
        inner.set_entry_type(tar::EntryType::Regular);
        inner.set_mode(0o644);
        inner.set_size(6);
        builder
            .append_data(&mut inner, "sub/inner.txt", &b"nested"[..])
            .unwrap();
        let mut link = tar::Header::new_gnu();
        link.set_entry_type(tar::EntryType::Symlink);
        link.set_size(0);
        link.set_link_name("note.txt").unwrap();
        builder.append_data(&mut link, "link", io::empty()).unwrap();
        let mut bytes = builder.into_inner().unwrap();
        let end = bytes.split_off(bytes.len() - 1024);
        bytes.extend(raw_entry("../escape.txt", b"nope"));
        bytes.extend(raw_entry("/tmp/abs.txt", b"nope"));
        bytes.extend(end);
        bytes
    }

    #[test]
    fn tar_stream_becomes_a_zip_and_a_raw_tar_is_copied() {
        let dir = TmpDir::new();
        let tar_bytes = sample_tar();
        let zip_path = dir.0.join("out.zip");
        pack_from_tar_stream(
            io::Cursor::new(tar_bytes.clone()),
            &zip_path,
            ContainerKind::Zip,
        )
        .unwrap();
        let file = fs::File::open(&zip_path).unwrap();
        let mut zip = zip::ZipArchive::new(file).unwrap();
        let names: Vec<String> = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .collect();
        assert!(names.iter().any(|name| name == "note.txt"));
        assert!(names.iter().any(|name| name == "sub/"));
        assert!(names.iter().any(|name| name == "sub/inner.txt"));
        assert!(names.iter().all(|name| !name.contains("escape")));
        assert!(names.iter().all(|name| !name.contains("abs")));
        assert!(names.iter().all(|name| name != "link"));
        let mut note = zip.by_name("note.txt").unwrap();
        let mut body = Vec::new();
        io::copy(&mut note, &mut body).unwrap();
        assert_eq!(body, b"hello");
        drop(note);
        let mut inner = zip.by_name("sub/inner.txt").unwrap();
        body.clear();
        io::copy(&mut inner, &mut body).unwrap();
        assert_eq!(body, b"nested");

        let again = pack_from_tar_stream(
            io::Cursor::new(tar_bytes.clone()),
            &zip_path,
            ContainerKind::Zip,
        )
        .unwrap_err();
        assert_eq!(again.kind(), io::ErrorKind::AlreadyExists);
        assert!(zip_path.symlink_metadata().is_ok());

        let tar_path = dir.0.join("out.tar");
        pack_from_tar_stream(
            io::Cursor::new(tar_bytes.clone()),
            &tar_path,
            ContainerKind::Tar,
        )
        .unwrap();
        assert_eq!(fs::read(&tar_path).unwrap(), tar_bytes);

        let partial = dir.0.join("partial.zip");
        let err = pack_from_tar_stream(FailRead, &partial, ContainerKind::Zip).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);
        assert!(partial.symlink_metadata().is_err());
    }
}
