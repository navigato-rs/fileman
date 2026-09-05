#[cfg(unix)]
use std::os::unix::ffi::OsStrExt as _;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::UNIX_EPOCH,
};

use std::sync::{Arc, Mutex};

use std::sync::atomic::{AtomicBool, Ordering};

use crate::core::{
    EntryLocation, IOResult, IOTask, PreviewContent, PreviewRequest, SearchCase, SearchEvent,
    SearchMode, SearchProgress, SearchRequest, SearchResult, copy_container_dir,
    copy_container_entry, copy_recursively, create_archive, format_container_listing,
    is_probably_text, is_text_name, is_text_path, read_container_directory,
};
use crate::sftp::SftpSession;

type SftpSessions = Arc<Mutex<std::collections::HashMap<String, Arc<Mutex<SftpSession>>>>>;
type RemoteDirSizeChannels = (
    mpsc::Sender<(String, String)>,
    mpsc::Receiver<(String, String, u64)>,
);

fn lock_or_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn with_sftp<F, R>(sessions: &SftpSessions, host: &str, f: F) -> Result<R, String>
where
    F: FnOnce(&crate::ssh::Conn) -> Result<R, String>,
{
    with_sftp_session(sessions, host, |s| f(&s.sftp))
}

fn with_sftp_session<F, R>(sessions: &SftpSessions, host: &str, f: F) -> Result<R, String>
where
    F: FnOnce(&SftpSession) -> Result<R, String>,
{
    let session_arc = lock_or_recover(sessions)
        .get(host)
        .cloned()
        .ok_or_else(|| format!("No SFTP session for host: {host}"))?;
    let locked = lock_or_recover(&session_arc);
    f(&locked)
}

const PREVIEW_CHUNK_BYTES: usize = 16 * 1024;

pub fn start_io_worker(
    sftp_sessions: SftpSessions,
    transfer_progress: Arc<crate::core::TransferProgress>,
    wake: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    cancel_flag: Arc<AtomicBool>,
) -> (
    mpsc::Sender<IOTask>,
    mpsc::Receiver<IOResult>,
    mpsc::Sender<()>,
) {
    let (tx, rx) = mpsc::channel::<IOTask>();
    let (result_tx, result_rx) = mpsc::channel::<IOResult>();
    let (cancel_tx, cancel_rx) = mpsc::channel::<()>();
    thread::spawn(move || {
        let mut cancel_requested = false;
        while let Ok(task) = rx.recv() {
            while cancel_rx.try_recv().is_ok() {
                cancel_requested = true;
                cancel_flag.store(true, Ordering::Relaxed);
            }
            if cancel_requested {
                let _ = result_tx.send(IOResult::Completed);
                while let Ok(_dropped) = rx.try_recv() {
                    let _ = result_tx.send(IOResult::Completed);
                }
                cancel_requested = false;
                cancel_flag.store(false, Ordering::Relaxed);
                continue;
            }
            cancel_flag.store(false, Ordering::Relaxed);
            transfer_progress.reset(0);
            transfer_progress.set_current_name(Some(task.display_name()));
            // Default: refresh local Fs panels. Remote/silent ops override below.
            let mut io_result = IOResult::Completed;
            let task_clone = task.clone();
            match task {
                IOTask::Copy { src, dst_dir } => {
                    if let Err(e) = copy_recursively(&src, &dst_dir) {
                        if e.kind() == std::io::ErrorKind::PermissionDenied {
                            let msg = format!(
                                "Permission denied: copy {} → {}",
                                src.display(),
                                dst_dir.display()
                            );
                            eprintln!("{msg}");
                            io_result = IOResult::PermissionError {
                                message: msg,
                                task: task_clone,
                            };
                        } else {
                            let msg = format!("Copy error: {e}");
                            eprintln!("{msg}");
                            io_result = IOResult::Error(msg);
                        }
                    }
                }
                IOTask::CopyContainer {
                    kind,
                    archive_path,
                    inner_path,
                    dst_dir,
                    display_name,
                } => {
                    if let Err(e) = copy_container_entry(
                        kind,
                        &archive_path,
                        &inner_path,
                        &dst_dir,
                        &display_name,
                    ) {
                        let msg = format!("Copy container error: {e}");
                        eprintln!("{msg}");
                        io_result = IOResult::Error(msg);
                    }
                }
                IOTask::CopyContainerDir {
                    kind,
                    archive_path,
                    inner_path,
                    dst_dir,
                    display_name,
                } => {
                    if let Err(e) = copy_container_dir(
                        kind,
                        &archive_path,
                        &inner_path,
                        &dst_dir,
                        &display_name,
                    ) {
                        let msg = format!("Copy container dir error: {e}");
                        eprintln!("{msg}");
                        io_result = IOResult::Error(msg);
                    }
                }
                IOTask::CopyContainerToRemote {
                    kind,
                    archive_path,
                    inner_path,
                    host,
                    remote_dir,
                    display_name,
                    is_dir,
                } => {
                    // Extract into a temp dir, then upload to the remote host.
                    let tmp_dir = std::env::temp_dir().join("fileman_to_remote");
                    let extracted = tmp_dir.join(&display_name);
                    let result = (|| -> Result<(), String> {
                        std::fs::create_dir_all(&tmp_dir).map_err(|e| e.to_string())?;
                        if is_dir {
                            copy_container_dir(
                                kind,
                                &archive_path,
                                &inner_path,
                                &tmp_dir,
                                &display_name,
                            )
                            .map_err(|e| e.to_string())?;
                        } else {
                            copy_container_entry(
                                kind,
                                &archive_path,
                                &inner_path,
                                &tmp_dir,
                                &display_name,
                            )
                            .map_err(|e| e.to_string())?;
                        }
                        let session = lock_or_recover(&sftp_sessions)
                            .get(&host)
                            .cloned()
                            .ok_or_else(|| format!("no active SFTP session for host {host}"))?;
                        let locked = lock_or_recover(&session);
                        if is_dir {
                            let total = crate::sftp::count_bytes_local(&extracted);
                            transfer_progress.reset(total);
                            crate::sftp::copy_local_dir_to_remote_via_tar(
                                &extracted,
                                &locked.sftp,
                                &remote_dir,
                                &cancel_flag,
                                Some(&transfer_progress),
                            )
                        } else {
                            let remote_path = format!("{remote_dir}/{display_name}");
                            crate::sftp::copy_local_to_remote_progress(
                                &locked.sftp,
                                &extracted,
                                &remote_path,
                                Some(&cancel_flag),
                                Some(&transfer_progress),
                            )
                        }
                    })();
                    // Remove the extracted temp copy regardless of outcome.
                    let _ = if is_dir {
                        std::fs::remove_dir_all(&extracted)
                    } else {
                        std::fs::remove_file(&extracted)
                    };
                    io_result = match result {
                        Ok(()) => IOResult::CompletedRemote(host),
                        Err(e) => IOResult::ErrorRemote(host, format!("Copy to remote: {e}")),
                    };
                }
                IOTask::Move { src, dst_dir } => {
                    let target = dst_dir.join(
                        src.file_name()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "moved".to_string()),
                    );
                    if let Err(e) = std::fs::rename(&src, &target) {
                        if e.kind() == std::io::ErrorKind::PermissionDenied {
                            let msg = format!(
                                "Permission denied: move {} → {}",
                                src.display(),
                                dst_dir.display()
                            );
                            eprintln!("{msg}");
                            io_result = IOResult::PermissionError {
                                message: msg,
                                task: task_clone,
                            };
                        } else if let Err(copy_err) = copy_recursively(&src, &dst_dir) {
                            if copy_err.kind() == std::io::ErrorKind::PermissionDenied {
                                let msg = format!(
                                    "Permission denied: move {} → {}",
                                    src.display(),
                                    dst_dir.display()
                                );
                                eprintln!("{msg}");
                                io_result = IOResult::PermissionError {
                                    message: msg,
                                    task: task_clone,
                                };
                            } else {
                                let msg = format!("Move error: {copy_err}");
                                eprintln!("{msg}");
                                io_result = IOResult::Error(msg);
                            }
                        } else if let Err(remove_err) = match std::fs::symlink_metadata(&src) {
                            Ok(ref m) if m.is_dir() && !m.file_type().is_symlink() => {
                                std::fs::remove_dir_all(&src)
                            }
                            _ => std::fs::remove_file(&src),
                        } {
                            if remove_err.kind() == std::io::ErrorKind::PermissionDenied {
                                let msg =
                                    format!("Permission denied: move cleanup {}", src.display());
                                eprintln!("{msg}");
                                io_result = IOResult::PermissionError {
                                    message: msg,
                                    task: task_clone,
                                };
                            } else {
                                let msg = format!("Move cleanup error: {remove_err}");
                                eprintln!("{msg}");
                                io_result = IOResult::Error(msg);
                            }
                        }
                    }
                }
                IOTask::Delete { target } => {
                    let res = match std::fs::symlink_metadata(&target) {
                        Ok(ref m) if m.is_dir() && !m.file_type().is_symlink() => {
                            std::fs::remove_dir_all(&target)
                        }
                        _ => std::fs::remove_file(&target),
                    };
                    if let Err(e) = res {
                        if e.kind() == std::io::ErrorKind::PermissionDenied {
                            let msg = format!("Permission denied: {}", target.display());
                            eprintln!("{msg}");
                            io_result = IOResult::PermissionError {
                                message: msg,
                                task: task_clone,
                            };
                        } else {
                            let msg = format!("Delete error: {}\n{e}", target.to_string_lossy());
                            eprintln!("{msg}");
                            io_result = IOResult::Error(msg);
                        }
                    }
                }
                IOTask::Rename { src, new_name } => {
                    let target = src.with_file_name(&new_name);
                    // Refuse to rename onto an existing different entry, which
                    // std::fs::rename would silently replace. A no-op rename and
                    // a case-only rename on a case-insensitive filesystem (where
                    // target resolves to the same file as src) are allowed.
                    let onto_existing = target != src
                        && target.symlink_metadata().is_ok()
                        && target.canonicalize().ok() != src.canonicalize().ok();
                    if onto_existing {
                        let msg = format!("Already exists: {}", target.display());
                        eprintln!("{msg}");
                        io_result = IOResult::Error(msg);
                    } else if let Err(e) = std::fs::rename(&src, &target) {
                        if e.kind() == std::io::ErrorKind::PermissionDenied {
                            let msg = format!("Permission denied: rename {}", src.display());
                            eprintln!("{msg}");
                            io_result = IOResult::PermissionError {
                                message: msg,
                                task: task_clone,
                            };
                        } else {
                            let msg = format!("Rename error: {e}");
                            eprintln!("{msg}");
                            io_result = IOResult::Error(msg);
                        }
                    }
                }
                IOTask::WriteFile {
                    path,
                    contents,
                    exclusive,
                } => {
                    let write_result = (|| -> std::io::Result<()> {
                        use std::io::Write as _;
                        if exclusive {
                            // New file: create_new fails if the path exists,
                            // rather than truncating an existing file.
                            let mut f = std::fs::OpenOptions::new()
                                .write(true)
                                .create_new(true)
                                .open(&path)?;
                            f.write_all(&contents)?;
                            f.sync_all()?;
                        } else {
                            // Overwrite (editor save): write a sibling temp file,
                            // fsync it, then rename over the target so a crash or
                            // ENOSPC can never leave the original truncated.
                            let dir = path.parent().unwrap_or_else(|| Path::new("."));
                            let fname = path
                                .file_name()
                                .map(|s| s.to_string_lossy().into_owned())
                                .unwrap_or_else(|| "file".to_string());
                            let tmp =
                                dir.join(format!(".{fname}.fileman-tmp.{}", std::process::id()));
                            {
                                let mut f = std::fs::File::create(&tmp)?;
                                f.write_all(&contents)?;
                                f.sync_all()?;
                            }
                            if let Err(e) = std::fs::rename(&tmp, &path) {
                                let _ = std::fs::remove_file(&tmp);
                                return Err(e);
                            }
                        }
                        Ok(())
                    })();
                    if let Err(e) = write_result {
                        if e.kind() == std::io::ErrorKind::PermissionDenied {
                            let msg = format!("Permission denied: write {}", path.display());
                            eprintln!("{msg}");
                            io_result = IOResult::PermissionError {
                                message: msg,
                                task: task_clone,
                            };
                        } else if exclusive && e.kind() == std::io::ErrorKind::AlreadyExists {
                            let msg = format!("File already exists: {}", path.display());
                            eprintln!("{msg}");
                            io_result = IOResult::Error(msg);
                        } else {
                            let msg = format!("Write error: {e}");
                            eprintln!("{msg}");
                            io_result = IOResult::Error(msg);
                        }
                    }
                }
                IOTask::Mkdir { path } => {
                    if let Err(e) = std::fs::create_dir(&path) {
                        if e.kind() == std::io::ErrorKind::PermissionDenied {
                            let msg = format!("Permission denied: mkdir {}", path.display());
                            eprintln!("{msg}");
                            io_result = IOResult::PermissionError {
                                message: msg,
                                task: task_clone,
                            };
                        } else {
                            let msg = format!("Mkdir error: {e}");
                            eprintln!("{msg}");
                            io_result = IOResult::Error(msg);
                        }
                    }
                }
                IOTask::Pack {
                    sources,
                    archive_path,
                    kind,
                } => {
                    if let Err(e) = create_archive(&sources, &archive_path, kind) {
                        let msg = format!("Pack error: {e}");
                        eprintln!("{msg}");
                        io_result = IOResult::Error(msg);
                    }
                }
                #[cfg(unix)]
                IOTask::SetProps {
                    path,
                    mode,
                    uid,
                    gid,
                    recursive,
                } => {
                    let res = if recursive {
                        apply_props_recursive(&path, mode, uid, gid)
                    } else {
                        apply_props(&path, mode, uid, gid)
                    };
                    if let Err(e) = res {
                        if e.kind() == std::io::ErrorKind::PermissionDenied {
                            let msg = format!("Permission denied: set props on {}", path.display());
                            eprintln!("{msg}");
                            io_result = IOResult::PermissionError {
                                message: msg,
                                task: task_clone,
                            };
                        } else {
                            let msg = format!("Props error: {e}");
                            eprintln!("{msg}");
                            io_result = IOResult::Error(msg);
                        }
                    }
                }
                #[cfg(not(unix))]
                IOTask::SetProps { .. } => {
                    let msg = "SetProps is not supported on this platform".to_string();
                    eprintln!("{msg}");
                    io_result = IOResult::Error(msg);
                }
                IOTask::WriteRemoteFile {
                    host,
                    path,
                    contents,
                } => {
                    let mut err_msg = None;
                    if let Err(e) = with_sftp(&sftp_sessions, &host, |sftp| {
                        crate::sftp::write_file(sftp, &path, &contents)
                            .map_err(|e| format!("Remote write error: {e}"))
                    }) {
                        eprintln!("{e}");
                        err_msg = Some(e);
                    }
                    io_result = if let Some(msg) = err_msg {
                        IOResult::ErrorRemote(host, msg)
                    } else {
                        IOResult::CompletedRemote(host)
                    };
                }
                IOTask::CopyRemoteToLocal {
                    host,
                    remote_path,
                    dst_dir,
                    name,
                    is_dir,
                    delete_source_on_success,
                } => {
                    let mut err_msg: Option<String> = None;
                    if let Some(session) = lock_or_recover(&sftp_sessions).get(&host).cloned() {
                        let locked = lock_or_recover(&session);
                        let result = if is_dir {
                            let total =
                                crate::sftp::count_bytes_via_exec(&locked.sftp, &remote_path);
                            transfer_progress.reset(total);
                            crate::sftp::copy_remote_dir_to_local_via_tar(
                                &locked.sftp,
                                &remote_path,
                                &dst_dir,
                                &name,
                                &cancel_flag,
                                Some(&transfer_progress),
                            )
                        } else {
                            let local_path = dst_dir.join(&name);
                            crate::sftp::copy_remote_to_local_progress(
                                &locked.sftp,
                                &remote_path,
                                &local_path,
                                Some(&cancel_flag),
                                Some(&transfer_progress),
                            )
                        };
                        match result {
                            // Only a fully successful copy may delete the source
                            // (this is the move half). A cancel returns Err and
                            // therefore leaves the source untouched.
                            Ok(()) => {
                                if delete_source_on_success
                                    && let Err(e) = crate::sftp::recursive_delete(
                                        &locked.sftp,
                                        &remote_path,
                                        is_dir,
                                        Some(&transfer_progress),
                                    )
                                {
                                    let msg = format!(
                                        "Move {name}: copied but failed to remove source: {e}"
                                    );
                                    eprintln!("{msg}");
                                    err_msg = Some(msg);
                                }
                            }
                            Err(ref e) if e == "Cancelled" => {}
                            Err(e) => {
                                let msg = format!("Copy {name}: {e}");
                                eprintln!("{msg}");
                                err_msg = Some(msg);
                            }
                        }
                    } else {
                        let msg = format!("No SFTP session for host: {host}");
                        eprintln!("{msg}");
                        err_msg = Some(msg);
                    }
                    io_result = match err_msg {
                        Some(msg) => IOResult::Error(msg),
                        // A move refreshes both the local destination and the
                        // remote source; a plain copy refreshes only the local
                        // destination (the default `Completed`).
                        None if delete_source_on_success => IOResult::CompletedMoved(host),
                        None => IOResult::Completed,
                    };
                }
                IOTask::CopyLocalToRemote {
                    src,
                    host,
                    remote_dir,
                    is_dir,
                    delete_source_on_success,
                } => {
                    let mut err_msg: Option<String> = None;
                    if let Some(session) = lock_or_recover(&sftp_sessions).get(&host).cloned() {
                        let locked = lock_or_recover(&session);
                        let result = if is_dir {
                            let total = crate::sftp::count_bytes_local(&src);
                            transfer_progress.reset(total);
                            crate::sftp::copy_local_dir_to_remote_via_tar(
                                &src,
                                &locked.sftp,
                                &remote_dir,
                                &cancel_flag,
                                Some(&transfer_progress),
                            )
                        } else {
                            let name = src
                                .file_name()
                                .map(|s| s.to_string_lossy().to_string())
                                .unwrap_or_else(|| "file".to_string());
                            let remote_path = format!("{remote_dir}/{name}");
                            crate::sftp::copy_local_to_remote_progress(
                                &locked.sftp,
                                &src,
                                &remote_path,
                                Some(&cancel_flag),
                                Some(&transfer_progress),
                            )
                        };
                        match result {
                            // Only a fully successful upload may delete the local
                            // source (the move half). A cancel returns Err and
                            // therefore leaves the source untouched.
                            Ok(()) => {
                                if delete_source_on_success {
                                    let del = match std::fs::symlink_metadata(&src) {
                                        Ok(ref m) if m.is_dir() && !m.file_type().is_symlink() => {
                                            std::fs::remove_dir_all(&src)
                                        }
                                        _ => std::fs::remove_file(&src),
                                    };
                                    if let Err(e) = del {
                                        let msg = format!(
                                            "Move: uploaded but failed to remove source {}: {e}",
                                            src.display()
                                        );
                                        eprintln!("{msg}");
                                        err_msg = Some(msg);
                                    }
                                }
                            }
                            Err(ref e) if e == "Cancelled" => {}
                            Err(e) => {
                                let label = src
                                    .file_name()
                                    .map(|s| s.to_string_lossy().to_string())
                                    .unwrap_or_else(|| src.display().to_string());
                                let msg = format!("Copy {label}: {e}");
                                eprintln!("{msg}");
                                err_msg = Some(msg);
                            }
                        }
                    } else {
                        let msg = format!("No SFTP session for host: {host}");
                        eprintln!("{msg}");
                        err_msg = Some(msg);
                    }
                    io_result = match err_msg {
                        Some(msg) => IOResult::ErrorRemote(host, msg),
                        // A move refreshes both the remote destination and the
                        // local source; a plain copy refreshes only the remote
                        // destination.
                        None if delete_source_on_success => IOResult::CompletedMoved(host),
                        None => IOResult::CompletedRemote(host),
                    };
                }
                IOTask::DeleteRemote { host, items } => {
                    let mut err_msg = None;
                    if let Some(session) = lock_or_recover(&sftp_sessions).get(&host).cloned() {
                        let locked = lock_or_recover(&session);
                        for item in items.iter() {
                            if let Err(e) = crate::sftp::recursive_delete(
                                &locked.sftp,
                                item.0.as_str(),
                                item.1,
                                Some(&transfer_progress),
                            ) {
                                let msg = format!("Remote delete error: {e}");
                                eprintln!("{msg}");
                                err_msg = Some(msg);
                            }
                        }
                    } else {
                        let msg = format!("No SFTP session for host: {host}");
                        eprintln!("{msg}");
                        err_msg = Some(msg);
                    }
                    io_result = if let Some(msg) = err_msg {
                        IOResult::ErrorRemote(host, msg)
                    } else {
                        IOResult::CompletedRemote(host)
                    };
                }
                IOTask::RenameRemote {
                    host,
                    src,
                    new_name,
                } => {
                    let mut err_msg = None;
                    if let Some(session) = lock_or_recover(&sftp_sessions).get(&host).cloned() {
                        let locked = lock_or_recover(&session);
                        let parent = if let Some(pos) = src.rfind('/') {
                            &src[..pos]
                        } else {
                            ""
                        };
                        let dst = if parent.is_empty() {
                            format!("/{new_name}")
                        } else {
                            format!("{parent}/{new_name}")
                        };
                        if let Err(e) = crate::sftp::rename(&locked.sftp, &src, &dst) {
                            let msg = format!("Remote rename error: {e}");
                            eprintln!("{msg}");
                            err_msg = Some(msg);
                        }
                    } else {
                        let msg = format!("No SFTP session for host: {host}");
                        eprintln!("{msg}");
                        err_msg = Some(msg);
                    }
                    io_result = if let Some(msg) = err_msg {
                        IOResult::ErrorRemote(host, msg)
                    } else {
                        IOResult::CompletedRemote(host)
                    };
                }
                IOTask::MkdirRemote { host, path } => {
                    let mut err_msg = None;
                    if let Some(session) = lock_or_recover(&sftp_sessions).get(&host).cloned() {
                        let locked = lock_or_recover(&session);
                        if let Err(e) = crate::sftp::mkdir(&locked.sftp, &path) {
                            let msg = format!("Remote mkdir error: {e}");
                            eprintln!("{msg}");
                            err_msg = Some(msg);
                        }
                    } else {
                        let msg = format!("No SFTP session for host: {host}");
                        eprintln!("{msg}");
                        err_msg = Some(msg);
                    }
                    io_result = if let Some(msg) = err_msg {
                        IOResult::ErrorRemote(host, msg)
                    } else {
                        IOResult::CompletedRemote(host)
                    };
                }
                IOTask::CopyRemoteToLocalAndOpen {
                    host,
                    remote_path,
                    local_path,
                } => {
                    let mut err_msg: Option<String> = None;
                    if let Some(session) = lock_or_recover(&sftp_sessions).get(&host).cloned() {
                        let locked = lock_or_recover(&session);
                        match crate::sftp::copy_remote_to_local_progress(
                            &locked.sftp,
                            &remote_path,
                            &local_path,
                            Some(&cancel_flag),
                            Some(&transfer_progress),
                        ) {
                            Ok(()) => open_with_default_app_bg(&local_path),
                            Err(e) if e == "Cancelled" => {}
                            Err(e) => {
                                let msg = format!("Open {remote_path}: {e}");
                                eprintln!("{msg}");
                                err_msg = Some(msg);
                            }
                        }
                    } else {
                        let msg = format!("No SFTP session for host: {host}");
                        eprintln!("{msg}");
                        err_msg = Some(msg);
                    }
                    io_result = match err_msg {
                        Some(msg) => IOResult::Error(msg),
                        None => IOResult::CompletedSilent,
                    };
                }
                IOTask::CopyRemoteSameHost {
                    host,
                    src_path,
                    dst_dir,
                    name,
                } => {
                    let mut err_msg = None;
                    if let Some(session) = lock_or_recover(&sftp_sessions).get(&host).cloned() {
                        let locked = lock_or_recover(&session);
                        if let Err(e) = crate::sftp::recursive_copy_remote(
                            &locked.sftp,
                            &src_path,
                            &dst_dir,
                            &name,
                        ) {
                            let msg = format!("Remote copy error: {e}");
                            eprintln!("{msg}");
                            err_msg = Some(msg);
                        }
                    } else {
                        let msg = format!("No SFTP session for host: {host}");
                        eprintln!("{msg}");
                        err_msg = Some(msg);
                    }
                    io_result = if let Some(msg) = err_msg {
                        IOResult::ErrorRemote(host, msg)
                    } else {
                        IOResult::CompletedRemote(host)
                    };
                }
                IOTask::MoveRemoteSameHost {
                    host,
                    src_path,
                    dst_dir,
                    name,
                } => {
                    let mut err_msg = None;
                    if let Some(session) = lock_or_recover(&sftp_sessions).get(&host).cloned() {
                        let locked = lock_or_recover(&session);
                        let dst_path = format!("{}/{}", dst_dir.trim_end_matches('/'), name);
                        if let Err(e) = crate::sftp::rename(&locked.sftp, &src_path, &dst_path) {
                            let msg = format!("Remote move error: {e}");
                            eprintln!("{msg}");
                            err_msg = Some(msg);
                        }
                    } else {
                        let msg = format!("No SFTP session for host: {host}");
                        eprintln!("{msg}");
                        err_msg = Some(msg);
                    }
                    io_result = if let Some(msg) = err_msg {
                        IOResult::ErrorRemote(host, msg)
                    } else {
                        IOResult::CompletedRemote(host)
                    };
                }
                IOTask::CopyContainerAndOpen {
                    kind,
                    archive_path,
                    inner_path,
                    dst_dir,
                    display_name,
                } => {
                    match copy_container_entry(
                        kind,
                        &archive_path,
                        &inner_path,
                        &dst_dir,
                        &display_name,
                    ) {
                        Ok(()) => open_with_default_app_bg(&dst_dir.join(&display_name)),
                        Err(e) => eprintln!("Extract error: {e}"),
                    }
                    io_result = IOResult::CompletedSilent;
                }
                IOTask::CopyRemoteCrossHost {
                    src_host,
                    src_path,
                    dst_host,
                    dst_dir,
                    name,
                    is_dir: _,
                } => {
                    let sessions = lock_or_recover(&sftp_sessions);
                    let src_session = sessions.get(&src_host).cloned();
                    let dst_session = sessions.get(&dst_host).cloned();
                    drop(sessions);
                    let mut err_msg: Option<String> = None;
                    match (src_session, dst_session) {
                        (Some(src_arc), Some(dst_arc)) => {
                            let src_locked = lock_or_recover(&src_arc);
                            let dst_locked = lock_or_recover(&dst_arc);
                            transfer_progress.reset(0);
                            if let Err(e) = crate::sftp::copy_cross_host_via_tar(
                                &src_locked.sftp,
                                &src_path,
                                &dst_locked.sftp,
                                &dst_dir,
                                &name,
                                &cancel_flag,
                                Some(&transfer_progress),
                            ) && e != "Cancelled"
                            {
                                let msg = format!("Copy {name}: {e}");
                                eprintln!("{msg}");
                                err_msg = Some(msg);
                            }
                        }
                        (None, _) => {
                            let msg = format!("No SFTP session for host: {src_host}");
                            eprintln!("{msg}");
                            err_msg = Some(msg);
                        }
                        (_, None) => {
                            let msg = format!("No SFTP session for host: {dst_host}");
                            eprintln!("{msg}");
                            err_msg = Some(msg);
                        }
                    }
                    io_result = match err_msg {
                        Some(msg) => IOResult::ErrorRemote(dst_host, msg),
                        None => IOResult::CompletedRemote(dst_host),
                    };
                }
                IOTask::Elevated(inner) => match crate::elevate::execute_elevated(&inner) {
                    Ok(()) => {}
                    Err(msg) => {
                        eprintln!("{msg}");
                        io_result = IOResult::Error(msg);
                    }
                },
            }
            transfer_progress.set_current_name(None);
            let _ = result_tx.send(io_result);
            if let Some(ref wake) = wake {
                wake();
            }
        }
    });
    (tx, result_rx, cancel_tx)
}

fn open_with_default_app_bg(path: &Path) {
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(path).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        // Use ShellExecuteW directly — no process spawning, no console flash.
        // `cmd /C start` spawns a console-subsystem process which causes a
        // brief terminal window to appear when the parent has no console.
        use std::os::windows::ffi::OsStrExt as _;
        #[link(name = "shell32")]
        unsafe extern "system" {
            fn ShellExecuteW(
                hwnd: *mut std::ffi::c_void,
                operation: *const u16,
                file: *const u16,
                parameters: *const u16,
                directory: *const u16,
                show_cmd: i32,
            ) -> isize;
        }
        let file: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let verb: Vec<u16> = std::ffi::OsStr::new("open")
            .encode_wide()
            .chain(Some(0))
            .collect();
        unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                file.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1, // SW_SHOWNORMAL
            );
        }
    }
}

#[cfg(unix)]
fn apply_props(path: &Path, mode: u32, uid: u32, gid: u32) -> std::io::Result<()> {
    let permissions = std::fs::Permissions::from_mode(mode);
    std::fs::set_permissions(path, permissions)?;
    // `OsStr::as_bytes()` is not NUL-terminated; passing its pointer straight to
    // chown() reads past the buffer and can operate on the wrong path. Build a
    // proper C string (which also rejects interior NULs).
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let res = unsafe { libc::chown(c_path.as_ptr(), uid, gid) };
    if res != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn apply_props_recursive(path: &Path, mode: u32, uid: u32, gid: u32) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    apply_props(path, mode, uid, gid)?;
    if meta.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            apply_props_recursive(&entry.path(), mode, uid, gid)?;
        }
    }
    Ok(())
}

pub fn start_preview_worker(
    wake: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    sftp_sessions: SftpSessions,
    transfer_progress: Arc<crate::core::TransferProgress>,
) -> (
    mpsc::Sender<PreviewRequest>,
    mpsc::Receiver<(u64, PreviewContent)>,
) {
    let (tx, rx) = mpsc::channel::<PreviewRequest>();
    let (result_tx, result_rx) = mpsc::channel::<(u64, PreviewContent)>();
    let current_id = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    thread::spawn(move || {
        while let Ok(request) = rx.recv() {
            current_id.store(
                preview_request_id(&request),
                std::sync::atomic::Ordering::Relaxed,
            );
            let current_id = std::sync::Arc::clone(&current_id);
            let wake = wake.clone();
            match request {
                PreviewRequest::Read {
                    id,
                    location,
                    max_bytes,
                } => {
                    let result_tx = result_tx.clone();
                    let sftp_sessions = sftp_sessions.clone();
                    let progress = transfer_progress.clone();
                    thread::spawn(move || match location {
                        EntryLocation::Fs(path) => {
                            if crate::core::is_video_path(&path) {
                                let _ = result_tx.send((id, PreviewContent::Text(video_preview(&path))));
                                return;
                            }
                            let force_text = is_text_path(&path);
                            let file = File::open(&path);
                            if let Ok(file) = file {
                                let reader = std::io::BufReader::new(file);
                                if let Err(err) = send_streaming_preview(
                                    &result_tx,
                                    &current_id,
                                    id,
                                    reader,
                                    max_bytes,
                                    force_text,
                                    wake.as_ref(),
                                    None,
                                ) {
                                    let _ = result_tx.send((
                                        id,
                                        PreviewContent::Text(format!("Failed to read file: {err}")),
                                    ));
                                }
                            } else if let Err(err) = file {
                                let _ = result_tx.send((
                                    id,
                                    PreviewContent::Text(format!("Failed to read file: {err}")),
                                ));
                            }
                        }
                        EntryLocation::Container {
                            kind,
                            archive_path,
                            inner_path,
                        } => {
                            let force_text = is_text_name(&inner_path);
                            if let Err(err) = stream_container_preview(
                                &result_tx,
                                &current_id,
                                id,
                                kind,
                                &archive_path,
                                &inner_path,
                                max_bytes,
                                force_text,
                                wake.as_ref(),
                                Some(&progress),
                            ) {
                                let _ = result_tx.send((
                                    id,
                                    PreviewContent::Text(format!(
                                        "Failed to read archive entry: {err}"
                                    )),
                                ));
                            }
                        }
                        EntryLocation::Remote { host, path } => {
                            let force_text = is_text_name(&path);
                            let session = lock_or_recover(&sftp_sessions).get(&host).cloned();
                            if let Some(session) = session {
                                let locked = lock_or_recover(&session);
                                match crate::sftp::open_remote_reader(&locked.sftp, &path) {
                                    Ok(reader) => {
                                        if let Err(err) = send_streaming_preview(
                                            &result_tx,
                                            &current_id,
                                            id,
                                            reader,
                                            max_bytes,
                                            force_text,
                                            wake.as_ref(),
                                            Some(&progress),
                                        ) {
                                            let _ = result_tx.send((
                                                id,
                                                PreviewContent::Text(format!(
                                                    "Failed to read remote file: {err}"
                                                )),
                                            ));
                                        }
                                    }
                                    Err(err) => {
                                        let _ = result_tx.send((
                                            id,
                                            PreviewContent::Text(format!(
                                                "Failed to open remote file: {err}"
                                            )),
                                        ));
                                    }
                                }
                            } else {
                                let _ = result_tx.send((
                                    id,
                                    PreviewContent::Text(format!(
                                        "No SFTP session for host: {host}"
                                    )),
                                ));
                            }
                        }
                    });
                }
                PreviewRequest::ListContainer {
                    id,
                    kind,
                    archive_path,
                    max_entries,
                } => {
                    let entries = match read_container_directory(kind, &archive_path, "") {
                        Ok(entries) => entries,
                        Err(e) => {
                            let content =
                                PreviewContent::Text(format!("Failed to read archive: {e}"));
                            let _ = result_tx.send((id, content));
                            continue;
                        }
                    };
                    let listing = format_container_listing(&entries, max_entries);
                    let _ = result_tx.send((id, PreviewContent::Text(listing)));
                }
            }
        }
    });
    (tx, result_rx)
}

fn preview_request_id(request: &PreviewRequest) -> u64 {
    match *request {
        PreviewRequest::Read { id, .. } => id,
        PreviewRequest::ListContainer { id, .. } => id,
    }
}

pub fn start_dir_size_worker(
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
) -> (mpsc::Sender<PathBuf>, mpsc::Receiver<(PathBuf, u64)>) {
    let (tx, rx) = mpsc::channel::<PathBuf>();
    let (result_tx, result_rx) = mpsc::channel::<(PathBuf, u64)>();
    let max_concurrent = 4usize;
    thread::spawn(move || {
        let mut active: Vec<thread::JoinHandle<()>> = Vec::new();
        while let Ok(path) = rx.recv() {
            active.retain(|h| !h.is_finished());
            if active.len() >= max_concurrent {
                let _ = active.remove(0).join();
            }
            let result_tx = result_tx.clone();
            let wake = wake.clone();
            active.push(thread::spawn(move || {
                let size = compute_dir_size(&path);
                let _ = result_tx.send((path, size));
                if let Some(ref w) = wake {
                    w();
                }
            }));
        }
        for handle in active {
            let _ = handle.join();
        }
    });
    (tx, result_rx)
}

fn is_preview_current(current_id: &std::sync::atomic::AtomicU64, id: u64) -> bool {
    current_id.load(std::sync::atomic::Ordering::Relaxed) == id
}

#[allow(clippy::too_many_arguments)]
fn send_streaming_preview<R: Read>(
    tx: &mpsc::Sender<(u64, PreviewContent)>,
    current_id: &std::sync::atomic::AtomicU64,
    id: u64,
    mut reader: R,
    max_bytes: Option<usize>,
    force_text: bool,
    wake: Option<&std::sync::Arc<dyn Fn() + Send + Sync>>,
    progress: Option<&crate::core::TransferProgress>,
) -> Result<(), std::io::Error> {
    let mut remaining = max_bytes.unwrap_or(usize::MAX);
    let mut buf = vec![0u8; PREVIEW_CHUNK_BYTES];
    let mut decided = force_text;
    let mut is_text = force_text;
    let mut bom_stripped = false;
    let mut sent_any = false;
    // Text decoder chosen after the first chunk. UTF-8 by default; if the
    // first chunk isn't valid UTF-8, chardetng picks a legacy encoding
    // (CP1251, CP1252, Shift_JIS, …) and encoding_rs decodes incrementally.
    let mut decoder: Option<encoding_rs::Decoder> = None;

    while remaining > 0 {
        if !is_preview_current(current_id, id) {
            return Ok(());
        }
        let to_read = buf.len().min(remaining);
        let read = reader.read(&mut buf[..to_read])?;
        if read == 0 {
            break;
        }
        remaining = remaining.saturating_sub(read);
        if let Some(p) = progress {
            p.add(read as u64);
        }
        let chunk = &buf[..read];
        if !decided {
            is_text = is_probably_text(chunk);
            decided = true;
        }
        if is_text {
            // Strip UTF-8 BOM from the first chunk
            let chunk = if !bom_stripped {
                bom_stripped = true;
                chunk.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(chunk)
            } else {
                chunk
            };
            let dec = decoder.get_or_insert_with(|| {
                let encoding = if std::str::from_utf8(chunk).is_ok() {
                    encoding_rs::UTF_8
                } else {
                    let mut det = chardetng::EncodingDetector::new();
                    det.feed(chunk, remaining == 0);
                    det.guess(None, true)
                };
                encoding.new_decoder_without_bom_handling()
            });
            let last = remaining == 0;
            // A single input byte can expand to up to 3 UTF-8 output bytes
            // (Cyrillic in CP1251, CJK in various single-byte encodings, …).
            // Reserving max_utf8_buffer_length avoids OutputFull truncation.
            let cap = dec
                .max_utf8_buffer_length(chunk.len())
                .unwrap_or(chunk.len() * 3);
            let mut text = String::with_capacity(cap);
            let _ = dec.decode_to_string(chunk, &mut text, last);
            let _ = tx.send((id, PreviewContent::TextChunk { text, done: last }));
        } else {
            let _ = tx.send((
                id,
                PreviewContent::BinaryChunk {
                    data: chunk.to_vec(),
                    done: remaining == 0,
                },
            ));
        }
        sent_any = true;
        if let Some(wake) = wake {
            wake();
        }
    }

    // For 0-byte files: no chunks were sent, so send an empty text to resolve the preview.
    if !sent_any {
        let _ = tx.send((
            id,
            PreviewContent::TextChunk {
                text: String::new(),
                done: true,
            },
        ));
        if let Some(wake) = wake {
            wake();
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
/// Metadata-only preview for a local video file: size, plus duration and
/// resolution for MP4/MOV containers (other formats show size only).
fn video_preview(path: &Path) -> String {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("<video>");
    let mut lines = vec![format!("Video: {name}")];
    if let Ok(size) = std::fs::metadata(path).map(|m| m.len()) {
        lines.push(format!("Size: {}", crate::core::format_size(size)));
    }
    match File::open(path).and_then(|mut f| crate::core::read_mp4_info(&mut f)) {
        Ok(info) => {
            if let Some(secs) = info.duration_secs {
                lines.push(format!("Duration: {}", fmt_duration(secs)));
            }
            if let (Some(w), Some(h)) = (info.width, info.height) {
                lines.push(format!("Resolution: {w}×{h}"));
            }
            if info.duration_secs.is_none() && info.width.is_none() {
                lines.push(String::new());
                lines.push("(No embedded metadata for this container format.)".to_string());
            }
        }
        Err(e) => lines.push(format!("(Could not read metadata: {e})")),
    }
    lines.join("\n")
}

fn fmt_duration(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[allow(clippy::too_many_arguments)]
fn stream_container_preview(
    tx: &mpsc::Sender<(u64, PreviewContent)>,
    current_id: &std::sync::atomic::AtomicU64,
    id: u64,
    kind: crate::core::ContainerKind,
    archive_path: &Path,
    inner_path: &str,
    max_bytes: Option<usize>,
    force_text: bool,
    wake: Option<&std::sync::Arc<dyn Fn() + Send + Sync>>,
    progress: Option<&crate::core::TransferProgress>,
) -> Result<(), String> {
    let normalized = inner_path.trim_start_matches('/');
    // Read through with_seek_reader so archives on a remote host (whose path is
    // synthetic, not a real local file) open over SFTP instead of failing with
    // ENOENT from a plain File::open.
    crate::archive::with_seek_reader(archive_path, |reader| {
        use std::io::SeekFrom;
        if let Some(p) = progress {
            let total = reader.seek(SeekFrom::End(0)).unwrap_or(0);
            reader.seek(SeekFrom::Start(0))?;
            p.reset(total);
        }
        let to_io = std::io::Error::other;
        match kind {
            crate::core::ContainerKind::Zip => {
                let mut zip = zip::ZipArchive::new(reader).map_err(to_io)?;
                for i in 0..zip.len() {
                    if !is_preview_current(current_id, id) {
                        return Ok(());
                    }
                    let entry = zip.by_index(i).map_err(to_io)?;
                    if entry.name() == normalized {
                        return send_streaming_preview(
                            tx, current_id, id, entry, max_bytes, force_text, wake, progress,
                        );
                    }
                }
            }
            crate::core::ContainerKind::Tar => {
                let mut archive = tar::Archive::new(reader);
                for entry in archive.entries()? {
                    if !is_preview_current(current_id, id) {
                        return Ok(());
                    }
                    let mut entry = entry?;
                    let path = entry.path()?;
                    let name = crate::core::normalize_archive_path(&path);
                    if name == normalized {
                        return send_streaming_preview(
                            tx, current_id, id, &mut entry, max_bytes, force_text, wake, progress,
                        );
                    }
                }
            }
            crate::core::ContainerKind::TarGz => {
                let decoder = flate2::read::GzDecoder::new(reader);
                let mut archive = tar::Archive::new(decoder);
                for entry in archive.entries()? {
                    if !is_preview_current(current_id, id) {
                        return Ok(());
                    }
                    let mut entry = entry?;
                    let path = entry.path()?;
                    let name = crate::core::normalize_archive_path(&path);
                    if name == normalized {
                        return send_streaming_preview(
                            tx, current_id, id, &mut entry, max_bytes, force_text, wake, progress,
                        );
                    }
                }
            }
            crate::core::ContainerKind::TarBz2 => {
                let decoder = bzip2::read::BzDecoder::new(reader);
                let mut archive = tar::Archive::new(decoder);
                for entry in archive.entries()? {
                    if !is_preview_current(current_id, id) {
                        return Ok(());
                    }
                    let mut entry = entry?;
                    let path = entry.path()?;
                    let name = crate::core::normalize_archive_path(&path);
                    if name == normalized {
                        return send_streaming_preview(
                            tx, current_id, id, &mut entry, max_bytes, force_text, wake, progress,
                        );
                    }
                }
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Entry not found in archive: {inner_path}"),
        ))
    })
    .map_err(|e| e.to_string())
}

pub fn start_search_worker(
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
    sftp_sessions: SftpSessions,
) -> (mpsc::Sender<SearchRequest>, mpsc::Receiver<SearchEvent>) {
    let (tx, rx) = mpsc::channel::<SearchRequest>();
    let (result_tx, result_rx) = mpsc::channel::<SearchEvent>();
    thread::spawn(move || {
        let mut pending: Option<SearchRequest> = None;
        'worker: loop {
            let request = match pending.take() {
                Some(request) => request,
                None => match rx.recv() {
                    Ok(request) => request,
                    Err(_) => break,
                },
            };
            if let Some((ref host, ref remote_root)) = request.remote {
                pending = run_remote_search(
                    &request,
                    host,
                    remote_root,
                    &sftp_sessions,
                    &result_tx,
                    &rx,
                    &wake,
                );
                continue;
            }
            let mut progress = SearchProgress {
                scanned: 0,
                matched: 0,
            };
            let mut stack = vec![request.root.clone()];
            let mut needle = request.needle.clone();
            if request.case == SearchCase::Insensitive {
                needle = needle.to_ascii_lowercase();
            }
            let use_wildcard = needle.contains('*') || needle.contains('?');
            let mut tick = 0usize;

            loop {
                if let Ok(new_request) = rx.try_recv() {
                    pending = Some(new_request);
                    continue 'worker;
                }
                let dir = match stack.pop() {
                    Some(dir) => dir,
                    None => {
                        let _ = result_tx.send(SearchEvent::Done {
                            id: request.id,
                            progress,
                        });
                        if let Some(ref wake) = wake {
                            wake();
                        }
                        continue 'worker;
                    }
                };
                let read_dir = match std::fs::read_dir(&dir) {
                    Ok(rd) => rd,
                    Err(_) => continue,
                };
                for entry in read_dir.flatten() {
                    tick = tick.wrapping_add(1);
                    if tick.is_multiple_of(256) {
                        if let Ok(new_request) = rx.try_recv() {
                            pending = Some(new_request);
                            continue 'worker;
                        }
                        let _ = result_tx.send(SearchEvent::Progress {
                            id: request.id,
                            progress,
                        });
                        if let Some(ref wake) = wake {
                            wake();
                        }
                    }
                    progress.scanned = progress.scanned.saturating_add(1);
                    let path = entry.path();
                    let file_type = match entry.file_type() {
                        Ok(ft) => ft,
                        Err(_) => continue,
                    };
                    let metadata = entry.metadata().ok();
                    let modified = metadata
                        .as_ref()
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_secs());
                    match request.mode {
                        SearchMode::Name => {
                            let name = entry.file_name().to_string_lossy().to_string();
                            let haystack = if request.case == SearchCase::Insensitive {
                                name.to_ascii_lowercase()
                            } else {
                                name.clone()
                            };
                            let matched = if use_wildcard {
                                wildcard_match(&haystack, &needle)
                            } else {
                                haystack.contains(&needle)
                            };
                            if matched {
                                let size = if file_type.is_file() {
                                    metadata.as_ref().map(|m| m.len())
                                } else {
                                    None
                                };
                                let _ = result_tx.send(SearchEvent::Match {
                                    id: request.id,
                                    result: SearchResult {
                                        path: path.clone(),
                                        is_dir: file_type.is_dir(),
                                        size,
                                        modified,
                                        remote_path: None,
                                    },
                                });
                                progress.matched = progress.matched.saturating_add(1);
                            }
                            if file_type.is_dir() {
                                stack.push(path);
                            }
                        }
                        SearchMode::Content => {
                            if file_type.is_dir() {
                                stack.push(path);
                                continue;
                            }
                            if !file_type.is_file() {
                                continue;
                            }
                            if file_contains(&path, &needle, request.case).unwrap_or(false) {
                                let size = metadata.as_ref().map(|m| m.len());
                                let _ = result_tx.send(SearchEvent::Match {
                                    id: request.id,
                                    result: SearchResult {
                                        path: path.clone(),
                                        is_dir: false,
                                        size,
                                        modified,
                                        remote_path: None,
                                    },
                                });
                                progress.matched = progress.matched.saturating_add(1);
                            }
                        }
                    }
                }
            }
        }
    });
    (tx, result_rx)
}

fn file_contains(path: &PathBuf, needle: &str, case: SearchCase) -> std::io::Result<bool> {
    if needle.is_empty() {
        return Ok(false);
    }
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut carry: Vec<u8> = Vec::new();
    let needle_bytes = needle.as_bytes();
    let needle_lower = if case == SearchCase::Insensitive {
        Some(needle.to_ascii_lowercase().into_bytes())
    } else {
        None
    };
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        let mut window = Vec::with_capacity(carry.len() + read);
        if !carry.is_empty() {
            window.extend_from_slice(&carry);
        }
        window.extend_from_slice(&buf[..read]);

        let found = if let Some(needle_lower) = needle_lower.as_ref() {
            let mut lowered = window.clone();
            for byte in &mut lowered {
                *byte = byte.to_ascii_lowercase();
            }
            memchr::memmem::find(&lowered, needle_lower).is_some()
        } else {
            memchr::memmem::find(&window, needle_bytes).is_some()
        };
        if found {
            return Ok(true);
        }

        let keep = needle_bytes.len().saturating_sub(1);
        if keep > 0 {
            if window.len() >= keep {
                carry = window[window.len() - keep..].to_vec();
            } else {
                carry = window;
            }
        } else {
            carry.clear();
        }
    }
    Ok(false)
}

fn wildcard_match(text: &str, pattern: &str) -> bool {
    let mut t = 0usize;
    let mut p = 0usize;
    let mut star_idx: Option<usize> = None;
    let mut match_idx = 0usize;
    let text_bytes = text.as_bytes();
    let pat_bytes = pattern.as_bytes();

    while t < text_bytes.len() {
        if p < pat_bytes.len() && (pat_bytes[p] == b'?' || pat_bytes[p] == text_bytes[t]) {
            p += 1;
            t += 1;
        } else if p < pat_bytes.len() && pat_bytes[p] == b'*' {
            star_idx = Some(p);
            match_idx = t;
            p += 1;
        } else if let Some(star) = star_idx {
            p = star + 1;
            match_idx += 1;
            t = match_idx;
        } else {
            return false;
        }
    }
    while p < pat_bytes.len() && pat_bytes[p] == b'*' {
        p += 1;
    }
    p == pat_bytes.len()
}

/// Shell-quote `s` with single quotes, escaping any embedded single quotes.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Build the SSH command for remote search.
fn build_remote_search_cmd(root: &str, needle: &str, case: SearchCase, mode: SearchMode) -> String {
    let quoted_root = sh_quote(root);
    match mode {
        SearchMode::Name => {
            let use_wildcard = needle.contains('*') || needle.contains('?');
            let pattern = if use_wildcard {
                needle.to_string()
            } else {
                format!("*{needle}*")
            };
            let flag = if case == SearchCase::Insensitive {
                "-iname"
            } else {
                "-name"
            };
            let pattern_q = sh_quote(&pattern);
            // GNU / busybox find: -printf with %y (type char) and %p (path)
            format!("find {quoted_root} {flag} {pattern_q} -printf '%y\\t%p\\n' 2>/dev/null")
        }
        SearchMode::Content => {
            let case_flag = if case == SearchCase::Insensitive {
                "i"
            } else {
                ""
            };
            let needle_q = sh_quote(needle);
            // -F = fixed string (not regex), -I = skip binaries, -l = filenames only, -r = recursive
            format!("grep -rI{case_flag}l -F {needle_q} {quoted_root} 2>/dev/null")
        }
    }
}

/// Parse one output line from the remote search command.
/// Returns (is_dir, remote_path).
fn parse_remote_search_line(line: &str, mode: SearchMode) -> (bool, &str) {
    match mode {
        SearchMode::Content => (false, line),
        SearchMode::Name => {
            // Expected format: "<type_char>\t<path>"
            if let Some(rest) = line.strip_prefix("d\t") {
                (true, rest)
            } else if let Some(rest) = line
                .strip_prefix("f\t")
                .or_else(|| line.strip_prefix("l\t"))
            {
                (false, rest)
            } else {
                // Fallback: no type prefix (non-GNU find)
                (false, line)
            }
        }
    }
}

/// Run a remote search (find / grep) over SSH, streaming results back.
/// Returns `Some(new_request)` if a new search request arrived (cancellation), `None` on completion.
fn run_remote_search(
    request: &SearchRequest,
    host: &str,
    remote_root: &str,
    sftp_sessions: &SftpSessions,
    result_tx: &mpsc::Sender<SearchEvent>,
    cancel_rx: &mpsc::Receiver<SearchRequest>,
    wake: &Option<Arc<dyn Fn() + Send + Sync>>,
) -> Option<SearchRequest> {
    use std::io::BufRead as _;

    let emit_done = |progress: SearchProgress| {
        let _ = result_tx.send(SearchEvent::Done {
            id: request.id,
            progress,
        });
        if let Some(w) = wake.as_ref() {
            w();
        }
    };

    let session_arc = match lock_or_recover(sftp_sessions).get(host).cloned() {
        Some(arc) => arc,
        None => {
            let _ = result_tx.send(SearchEvent::Error {
                id: request.id,
                message: format!("No SFTP session for host: {host}"),
            });
            emit_done(SearchProgress {
                scanned: 0,
                matched: 0,
            });
            return None;
        }
    };

    let cmd = build_remote_search_cmd(remote_root, &request.needle, request.case, request.mode);
    let locked = lock_or_recover(&session_arc);

    // Streamed rather than captured: a search over a big tree can print for a
    // long time, and results should appear as they arrive.
    let mut channel = match locked.sftp.exec_stream(&cmd, crate::ssh::Stdin::Closed) {
        Ok(ch) => ch,
        Err(e) => {
            let _ = result_tx.send(SearchEvent::Error {
                id: request.id,
                message: format!("SSH channel: {e}"),
            });
            emit_done(SearchProgress {
                scanned: 0,
                matched: 0,
            });
            return None;
        }
    };

    let mut progress = SearchProgress {
        scanned: 0,
        matched: 0,
    };
    let mut tick = 0usize;
    let reader = std::io::BufReader::new(&mut channel);

    for line_result in reader.lines() {
        tick = tick.wrapping_add(1);
        if tick.is_multiple_of(32) {
            if let Ok(new_req) = cancel_rx.try_recv() {
                // Dropping the reader/channel sends SIGPIPE to the remote process.
                return Some(new_req);
            }
            let _ = result_tx.send(SearchEvent::Progress {
                id: request.id,
                progress,
            });
            if let Some(w) = wake.as_ref() {
                w();
            }
        }

        let line = match line_result {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.is_empty() {
            continue;
        }

        let (is_dir, remote_path) = parse_remote_search_line(&line, request.mode);
        progress.scanned = progress.scanned.saturating_add(1);
        progress.matched = progress.matched.saturating_add(1);

        let synthetic = std::path::PathBuf::from(format!("/sftp/{host}{remote_path}"));
        let _ = result_tx.send(SearchEvent::Match {
            id: request.id,
            result: SearchResult {
                path: synthetic,
                is_dir,
                size: None,
                modified: None,
                remote_path: Some(remote_path.to_string()),
            },
        });
        if let Some(w) = wake.as_ref() {
            w();
        }
    }

    emit_done(progress);
    None
}

/// Worker that computes remote directory sizes via `du` over SSH exec.
/// Receives `(host, remote_path)` pairs, sends `(host, remote_path, byte_count)` back.
pub fn start_remote_dir_size_worker(
    sftp_sessions: SftpSessions,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
) -> RemoteDirSizeChannels {
    let (tx, rx) = mpsc::channel::<(String, String)>();
    let (result_tx, result_rx) = mpsc::channel::<(String, String, u64)>();
    let max_concurrent = 4usize;
    thread::spawn(move || {
        let mut active: Vec<thread::JoinHandle<()>> = Vec::new();
        while let Ok((host, path)) = rx.recv() {
            active.retain(|h| !h.is_finished());
            if active.len() >= max_concurrent {
                let _ = active.remove(0).join();
            }
            let sessions = sftp_sessions.clone();
            let result_tx = result_tx.clone();
            let wake = wake.clone();
            active.push(thread::spawn(move || {
                let size = {
                    let session_arc = lock_or_recover(&sessions).get(&host).cloned();
                    if let Some(arc) = session_arc {
                        let locked = lock_or_recover(&arc);
                        crate::sftp::count_bytes_via_exec(&locked.sftp, &path)
                    } else {
                        0
                    }
                };
                let _ = result_tx.send((host, path, size));
                if let Some(ref w) = wake {
                    w();
                }
            }));
        }
        for handle in active {
            let _ = handle.join();
        }
    });
    (tx, result_rx)
}

fn compute_dir_size(root: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read_dir = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file()
                && let Ok(meta) = entry.metadata()
            {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}
