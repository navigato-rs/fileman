use std::path::{Path, PathBuf};

use fileman::{app_state, archive, core};

use crate::open_props_dialog;
use crate::{
    ContainerLoadMode, UiCache, active_window_rows, apply_panel_snapshot, cancel_search,
    load_container_directory_async, load_fs_directory_async, open_search, preview_find_next,
    preview_find_prev, preview_rebuild_matches, refresh_active_panel, refresh_fs_panels,
    start_search,
};

pub(crate) fn open_selected(app: &mut app_state::AppState) {
    let active = app.active_panel;
    open_selected_from_to(app, active, active);
}

pub(crate) fn open_selected_external(app: &mut app_state::AppState) {
    if !app.allow_external_open {
        return;
    }
    let entry = {
        let panel = app.get_active_panel();
        let browser = panel.browser();
        if browser.entries.is_empty() {
            return;
        }
        browser.entries[browser.selected_index].clone()
    };
    match entry.location {
        core::EntryLocation::Fs(path) => {
            if let Err(err) = open_with_default_app(&path) {
                eprintln!("{err}");
            }
        }
        core::EntryLocation::Remote { host, path } => {
            if entry.is_dir || entry.name == ".." {
                return;
            }
            let name = path.rsplit('/').next().unwrap_or(&entry.name).to_string();
            let tmp_dir = std::env::temp_dir().join("fileman_extract");
            if let Err(e) = std::fs::create_dir_all(&tmp_dir) {
                eprintln!("Failed to create temp dir: {e}");
                return;
            }
            let local_path = tmp_dir.join(&name);
            let _ = app.io_tx.send(core::IOTask::CopyRemoteToLocalAndOpen {
                host,
                remote_path: path,
                local_path,
            });
            app.io_in_flight = app.io_in_flight.saturating_add(1);
        }
        core::EntryLocation::Container {
            kind,
            archive_path,
            inner_path,
        } => {
            if entry.is_dir || entry.name == ".." {
                return;
            }
            let display_name = Path::new(&inner_path)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(&entry.name)
                .to_string();
            let tmp_dir = std::env::temp_dir().join("fileman_extract");
            if let Err(e) = std::fs::create_dir_all(&tmp_dir) {
                eprintln!("Failed to create temp dir: {e}");
                return;
            }
            let _ = app.io_tx.send(core::IOTask::CopyContainerAndOpen {
                kind,
                archive_path,
                inner_path,
                dst_dir: tmp_dir,
                display_name,
            });
            app.io_in_flight = app.io_in_flight.saturating_add(1);
        }
    }
}

/// Reveal the selected entry in the desktop environment's file manager, where
/// the native right-click context menu is available. Local entries only —
/// remote and in-archive entries have no path the file manager can show.
pub(crate) fn reveal_selected_in_file_manager(app: &mut app_state::AppState) {
    if !app.allow_external_open {
        return;
    }
    let entry = {
        let panel = app.get_active_panel();
        let browser = panel.browser();
        if browser.entries.is_empty() {
            return;
        }
        browser.entries[browser.selected_index].clone()
    };
    if let core::EntryLocation::Fs(path) = entry.location
        && let Err(err) = reveal_in_file_manager(&path)
    {
        eprintln!("{err}");
    }
}

fn open_with_default_app(path: &Path) -> anyhow::Result<()> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", path.to_string_lossy().as_ref()])
            .spawn()
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("Failed to open with default app: {e}"))?;
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(path)
            .spawn()
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("Failed to open with default app: {e}"))?;
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(path)
            .spawn()
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("Failed to open with default app: {e}"))?;
        return Ok(());
    }
    #[allow(unreachable_code)]
    Ok(())
}

/// Open the platform file manager focused on `path`, selecting the item so the
/// user can invoke the desktop's own context menu on it.
fn reveal_in_file_manager(path: &Path) -> anyhow::Result<()> {
    #[cfg(target_os = "windows")]
    {
        // explorer.exe returns a non-zero exit code even on success, so spawn
        // and don't inspect the status.
        std::process::Command::new("explorer")
            .arg(format!("/select,{}", path.display()))
            .spawn()
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("Failed to reveal in Explorer: {e}"))?;
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg("-R")
            .arg(path)
            .spawn()
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("Failed to reveal in Finder: {e}"))?;
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    {
        // Ask the file manager to show and select the item via the freedesktop
        // D-Bus interface (Nautilus, Dolphin, Nemo, …). Fall back to opening the
        // containing folder when no such service answers.
        let uri = path_to_file_uri(path);
        let shown = std::process::Command::new("dbus-send")
            .args([
                "--session",
                "--dest=org.freedesktop.FileManager1",
                "--type=method_call",
                "/org/freedesktop/FileManager1",
                "org.freedesktop.FileManager1.ShowItems",
                &format!("array:string:{uri}"),
                "string:",
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !shown {
            let dir = path.parent().unwrap_or(path);
            std::process::Command::new("xdg-open")
                .arg(dir)
                .spawn()
                .map(|_| ())
                .map_err(|e| anyhow::anyhow!("Failed to open containing folder: {e}"))?;
        }
        return Ok(());
    }
    #[allow(unreachable_code)]
    Ok(())
}

/// Percent-encode a filesystem path into a `file://` URI for D-Bus ShowItems.
#[cfg(target_os = "linux")]
fn path_to_file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for &b in path.to_string_lossy().as_bytes() {
        match b {
            b'/' => uri.push('/'),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                uri.push(b as char)
            }
            _ => uri.push_str(&format!("%{b:02X}")),
        }
    }
    uri
}

/// The active panel's selected entry, or `None` when the list is empty.
fn active_selected_entry(app: &app_state::AppState) -> Option<core::DirEntry> {
    let browser = app.get_active_panel().browser();
    browser.entries.get(browser.selected_index).cloned()
}

/// Show the desktop's own "properties" window for the selected local entry.
pub(crate) fn show_properties_of_selected(app: &mut app_state::AppState) {
    if !app.allow_external_open {
        return;
    }
    if let Some(core::DirEntry {
        location: core::EntryLocation::Fs(path),
        ..
    }) = active_selected_entry(app)
        && let Err(err) = show_properties(&path)
    {
        app.record_error("properties", err.to_string());
    }
}

/// Move the selected local entry to the desktop trash (recoverable), then
/// reload the panel so the row disappears.
pub(crate) fn trash_selected(app: &mut app_state::AppState) {
    let panel = app.active_panel;
    let Some(entry) = active_selected_entry(app) else {
        return;
    };
    if entry.name == ".." {
        return;
    }
    if let core::EntryLocation::Fs(path) = entry.location {
        match move_to_trash(&path) {
            Ok(()) => crate::reload_panel(app, panel),
            Err(err) => app.record_error("trash", err.to_string()),
        }
    }
}

/// Copy the paths of the marked-or-selected entries to the clipboard.
pub(crate) fn copy_selected_paths(app: &app_state::AppState, ctx: &egui::Context) {
    if let Some(text) = build_selected_paths(app) {
        ctx.copy_text(text);
    }
}

fn show_properties(path: &Path) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        // The file manager's own properties window, via freedesktop D-Bus.
        let uri = path_to_file_uri(path);
        std::process::Command::new("dbus-send")
            .args([
                "--session",
                "--dest=org.freedesktop.FileManager1",
                "--type=method_call",
                "/org/freedesktop/FileManager1",
                "org.freedesktop.FileManager1.ShowItemProperties",
                &format!("array:string:{uri}"),
                "string:",
            ])
            .spawn()
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("D-Bus ShowItemProperties failed: {e}"))?;
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "tell application \"Finder\"\nactivate\nopen information window of (POSIX file \"{}\" as alias)\nend tell",
            applescript_escape(path)
        );
        std::process::Command::new("osascript")
            .arg("-e")
            .arg(script)
            .spawn()
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("Finder Get Info failed: {e}"))?;
        return Ok(());
    }
    #[cfg(target_os = "windows")]
    {
        shell_properties_dialog(path)?;
        return Ok(());
    }
    #[allow(unreachable_code)]
    Ok(())
}

fn move_to_trash(path: &Path) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let status = std::process::Command::new("gio")
            .arg("trash")
            .arg(path)
            .status()
            .map_err(|e| anyhow::anyhow!("gio trash failed to start: {e}"))?;
        if !status.success() {
            anyhow::bail!("could not trash {}", path.display());
        }
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "tell application \"Finder\" to delete POSIX file \"{}\"",
            applescript_escape(path)
        );
        let status = std::process::Command::new("osascript")
            .arg("-e")
            .arg(script)
            .status()
            .map_err(|e| anyhow::anyhow!("osascript failed to start: {e}"))?;
        if !status.success() {
            anyhow::bail!("could not trash {}", path.display());
        }
        return Ok(());
    }
    #[cfg(target_os = "windows")]
    {
        // Send to the Recycle Bin via the VisualBasic FileSystem helper — no
        // extra crate, no FFI.
        let quoted = path.display().to_string().replace('\'', "''");
        let func = if path.is_dir() {
            "DeleteDirectory"
        } else {
            "DeleteFile"
        };
        let script = format!(
            "Add-Type -AssemblyName Microsoft.VisualBasic; \
             [Microsoft.VisualBasic.FileIO.FileSystem]::{func}('{quoted}','OnlyErrorDialogs','SendToRecycleBin')"
        );
        let status = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .status()
            .map_err(|e| anyhow::anyhow!("powershell failed to start: {e}"))?;
        if !status.success() {
            anyhow::bail!("could not trash {}", path.display());
        }
        return Ok(());
    }
    #[allow(unreachable_code)]
    Ok(())
}

/// Escape a path for embedding inside an AppleScript double-quoted string.
#[cfg(target_os = "macos")]
fn applescript_escape(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

/// Invoke the Windows shell "properties" dialog for `path`.
#[cfg(target_os = "windows")]
fn shell_properties_dialog(path: &Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use std::ptr;

    #[repr(C)]
    #[allow(non_snake_case, clippy::upper_case_acronyms)]
    struct SHELLEXECUTEINFOW {
        cbSize: u32,
        fMask: u32,
        hwnd: *mut std::ffi::c_void,
        lpVerb: *const u16,
        lpFile: *const u16,
        lpParameters: *const u16,
        lpDirectory: *const u16,
        nShow: i32,
        hInstApp: *mut std::ffi::c_void,
        lpIDList: *mut std::ffi::c_void,
        lpClass: *const u16,
        hkeyClass: *mut std::ffi::c_void,
        dwHotKey: u32,
        hIcon: *mut std::ffi::c_void,
        hProcess: *mut std::ffi::c_void,
    }

    unsafe extern "system" {
        fn ShellExecuteExW(pExecInfo: *mut SHELLEXECUTEINFOW) -> i32;
    }

    // The "properties" verb needs the id-list flag to open the dialog.
    const SEE_MASK_INVOKEIDLIST: u32 = 0x0000000C;
    const SW_SHOW: i32 = 5;

    let to_wide =
        |s: &std::ffi::OsStr| -> Vec<u16> { s.encode_wide().chain(std::iter::once(0)).collect() };
    let verb = to_wide(std::ffi::OsStr::new("properties"));
    let file = to_wide(path.as_os_str());

    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_INVOKEIDLIST,
        hwnd: ptr::null_mut(),
        lpVerb: verb.as_ptr(),
        lpFile: file.as_ptr(),
        lpParameters: ptr::null(),
        lpDirectory: ptr::null(),
        nShow: SW_SHOW,
        hInstApp: ptr::null_mut(),
        lpIDList: ptr::null_mut(),
        lpClass: ptr::null(),
        hkeyClass: ptr::null_mut(),
        dwHotKey: 0,
        hIcon: ptr::null_mut(),
        hProcess: ptr::null_mut(),
    };
    let ok = unsafe { ShellExecuteExW(&mut info) };
    if ok == 0 {
        anyhow::bail!("could not open properties for {}", path.display());
    }
    Ok(())
}

fn open_selected_from_to(
    app: &mut app_state::AppState,
    source: core::ActivePanel,
    target: core::ActivePanel,
) {
    let (selected_entry, current_path, container_cwd) = {
        let panel = app.panel(source);
        let browser = panel.browser();
        if browser.entries.is_empty() {
            return;
        }
        let entry = browser.entries[browser.selected_index].clone();
        let current_path = browser.current_path.clone();
        let app_state::BrowserState {
            browser_mode: ref mode,
            ..
        } = *browser;
        let container_cwd = match mode {
            core::BrowserMode::Container { cwd, .. } => Some(cwd.clone()),
            _ => None,
        };
        (entry, current_path, container_cwd)
    };
    let container_root = app.panel(source).browser().container_root.clone();

    app.store_selection_memory_for(source);
    app.push_history(target);

    match selected_entry.location.clone() {
        core::EntryLocation::Fs(path) => {
            if selected_entry.is_dir {
                let prefer_name = if selected_entry.name == ".." {
                    current_path
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                } else {
                    None
                };
                load_fs_directory_async(app, path.clone(), target, prefer_name);

                if selected_entry.name != ".."
                    && let Some(name) = app.fs_last_selected_name.get(&path).cloned()
                {
                    app.select_entry_by_name(target, &name);
                }
            } else if let Some(kind) = core::container_kind_from_path(&path) {
                load_container_directory_async(
                    app,
                    kind,
                    path.clone(),
                    "".to_string(),
                    None,
                    target,
                    None,
                    ContainerLoadMode::UseCache,
                    None,
                );
            }
        }
        core::EntryLocation::Container {
            kind,
            archive_path,
            inner_path,
        } => {
            if selected_entry.is_dir {
                let prefer_name = if selected_entry.name == ".." {
                    container_cwd.as_ref().and_then(|cwd| {
                        cwd.trim_end_matches('/')
                            .rsplit('/')
                            .next()
                            .map(|s| s.to_string())
                    })
                } else {
                    None
                };
                let root_hint = container_root.clone();
                let mut effective_cwd = inner_path.clone();
                if let Some(ref root) = root_hint {
                    let root_prefix = format!("{}/", root.trim_end_matches('/'));
                    if effective_cwd == *root {
                        effective_cwd.clear();
                    } else if effective_cwd.starts_with(&root_prefix) {
                        effective_cwd = effective_cwd[root_prefix.len()..].to_string();
                    }
                }
                load_container_directory_async(
                    app,
                    kind,
                    archive_path.clone(),
                    effective_cwd,
                    root_hint,
                    target,
                    prefer_name,
                    ContainerLoadMode::UseCache,
                    None,
                );

                if selected_entry.name != ".."
                    && let Some(name) = app
                        .container_last_selected_name
                        .get(&(archive_path.clone(), inner_path.clone(), kind))
                        .cloned()
                {
                    app.select_entry_by_name(target, &name);
                }
            }
        }
        core::EntryLocation::Remote { host, path } => {
            if selected_entry.is_dir {
                let prefer_name = if selected_entry.name == ".." {
                    // Extract last component of current remote path
                    let browser = app.panel(source).browser();
                    if let core::BrowserMode::Remote {
                        path: ref cur_path, ..
                    } = browser.browser_mode
                    {
                        cur_path
                            .trim_end_matches('/')
                            .rsplit('/')
                            .next()
                            .map(|s| s.to_string())
                    } else {
                        None
                    }
                } else {
                    None
                };
                crate::load_sftp_directory_async(app, &host, &path, target, prefer_name);
            } else if let Some(kind) = core::container_kind_from_path(std::path::Path::new(&path)) {
                // Navigate into the remote archive as a container, using a
                // synthetic archive path so the whole container stack can work
                // unchanged. The archive reader streams via SFTP.
                let synthetic = fileman::sftp::encode_archive_path(&host, &path);
                let return_dir = path
                    .trim_end_matches('/')
                    .rsplit_once('/')
                    .map(|(parent, _)| parent.to_string())
                    .unwrap_or_else(|| "/".to_string());
                load_container_directory_async(
                    app,
                    kind,
                    synthetic,
                    "".to_string(),
                    None,
                    target,
                    None,
                    ContainerLoadMode::UseCache,
                    Some((host, return_dir)),
                );
            }
        }
    }

    while let Ok((path, size)) = app.dir_size_rx.try_recv() {
        app.dir_size_pending.remove(&path);
        app.dir_sizes.insert(path.clone(), size);
        for side in [core::ActivePanel::Left, core::ActivePanel::Right] {
            let panel = app.panel_mut(side);
            let browser = panel.browser_mut();
            for entry in &mut browser.entries {
                if entry.is_dir
                    && let core::EntryLocation::Fs(p) = &entry.location
                    && *p == path
                {
                    entry.size = Some(size);
                }
            }
        }
    }
}

pub(crate) fn handle_keyboard(
    ctx: &egui::Context,
    input: &egui::InputState,
    app: &mut app_state::AppState,
    cache: &mut UiCache,
) {
    let in_edit = matches!(
        app.panel(app.active_panel).mode,
        app_state::PanelMode::Edit(_)
    );
    let in_preview = matches!(
        app.panel(app.active_panel).mode,
        app_state::PanelMode::Preview(_)
    );
    // The Settings modal owns the keyboard while open: return before any
    // shortcut is consumed so egui's widgets receive typing, Tab (field
    // navigation), and Enter, and no panel action fires on the panel beneath
    // it. Escape (close) is handled by the settings view itself.
    if app.settings_open() {
        return;
    }
    // The search bar is a focused egui TextEdit; its select-all/copy/cut keys
    // must reach it instead of triggering panel actions (Pack/Copy/Delete).
    let search_typing = app.search_ui == app_state::SearchUiState::Open;
    // In edit mode, don't consume Ctrl+letter shortcuts that egui's TextEdit
    // needs (copy, paste, cut, select-all, undo, redo, etc.).
    let ctrl_h = !in_edit && ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::H));
    let ctrl_p = !in_edit && ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::P));
    let ctrl_e = !in_edit && ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::E));
    let ctrl_n = !in_edit && ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::N));
    let ctrl_c = !in_edit
        && !search_typing
        && ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::C));
    let ctrl_a = if in_preview || in_edit || search_typing {
        // Don't consume: egui's widgets handle Ctrl+A (select-all) natively.
        false
    } else {
        ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::A))
    };
    let ctrl_m = !in_edit && ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::M));
    let ctrl_d = !in_edit && ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::D));
    let ctrl_g = ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::G));
    let ctrl_x = !in_edit
        && !search_typing
        && ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::X));
    let ctrl_i = !in_edit && ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::I));
    let ctrl_shift_m = !in_edit
        && ctx.input_mut(|i| {
            i.consume_key(
                egui::Modifiers::CTRL.plus(egui::Modifiers::SHIFT),
                egui::Key::M,
            )
        });
    let ctrl_shift_c = !in_edit
        && ctx.input_mut(|i| {
            i.consume_key(
                egui::Modifiers::CTRL.plus(egui::Modifiers::SHIFT),
                egui::Key::C,
            )
        });
    let ctrl_shift_o = !in_edit
        && ctx.input_mut(|i| {
            i.consume_key(
                egui::Modifiers::CTRL.plus(egui::Modifiers::SHIFT),
                egui::Key::O,
            )
        });
    let ctrl_comma =
        !in_edit && ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::Comma));
    let f2 = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F2));

    let f1 = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F1));
    // Bare F1 only — `consume_key(NONE, …)` matches any modifiers, so guard
    // against Ctrl/Shift/Alt+F1 doing anything (Ctrl+H is the modified path).
    if (f1 && input.modifiers.is_none()) || ctrl_h {
        app.toggle_help();
        ctx.request_repaint();
        return;
    }
    // While IO is in flight (modal visible): only Escape is processed.
    if app.io_in_flight > 0 {
        if input.key_pressed(egui::Key::Escape) {
            app.request_io_cancel();
            ctx.request_repaint();
        }
        return;
    }
    if matches!(app.modal, Some(app_state::Modal::Elevation { .. })) {
        if input.key_pressed(egui::Key::Enter) {
            if let Some((_, task)) = app.take_elevation() {
                app.enqueue_io(crate::core::IOTask::Elevated(Box::new(task)));
            }
            ctx.request_repaint();
        } else if input.key_pressed(egui::Key::Escape) {
            app.close_modal();
            ctx.request_repaint();
        }
        return;
    }
    if app.error_message().is_some() {
        if input.key_pressed(egui::Key::Escape) || input.key_pressed(egui::Key::Enter) {
            app.close_modal();
            ctx.request_repaint();
        }
        return;
    }
    if app.props_dialog().is_some() {
        if input.key_pressed(egui::Key::Escape) {
            app.close_modal();
            ctx.request_repaint();
        }
        return;
    }
    if app.quick_jump().is_some() {
        if input.key_pressed(egui::Key::Escape) {
            app.close_quick_jump();
            ctx.request_repaint();
        } else if input.key_pressed(egui::Key::ArrowDown) {
            if let Some(qj) = app.quick_jump_mut()
                && qj.selected + 1 < qj.filtered.len()
            {
                qj.selected += 1;
            }
            ctx.request_repaint();
        } else if input.key_pressed(egui::Key::ArrowUp) {
            if let Some(qj) = app.quick_jump_mut() {
                qj.selected = qj.selected.saturating_sub(1);
            }
            ctx.request_repaint();
        } else if input.key_pressed(egui::Key::Enter) {
            let result = app.quick_jump().and_then(|qj| {
                if !qj.filtered.is_empty() {
                    let entry = &qj.entries[qj.filtered[qj.selected]];
                    Some(crate::ui::quick_jump::QuickJumpResult {
                        path: entry.path.clone(),
                        category: entry.category,
                    })
                } else if !qj.input.is_empty() {
                    let expanded = if qj.input.starts_with("~/") || qj.input == "~" {
                        std::env::var("HOME")
                            .map(|h| qj.input.replacen('~', &h, 1))
                            .unwrap_or_else(|_| qj.input.clone())
                    } else {
                        qj.input.clone()
                    };
                    Some(crate::ui::quick_jump::QuickJumpResult {
                        path: std::path::PathBuf::from(expanded),
                        category: app_state::QuickJumpCategory::Home,
                    })
                } else {
                    None
                }
            });
            let active = app.active_panel;
            app.close_quick_jump();
            if let Some(result) = result {
                crate::navigate_quick_jump(app, result, active);
            }
            ctx.request_repaint();
        }
        return;
    }
    if app.help_panel_side().is_some() {
        if input.key_pressed(egui::Key::Escape) || input.key_pressed(egui::Key::Enter) {
            app.toggle_help();
            ctx.request_repaint();
        }
        return;
    }
    if app.pending_op().is_some() {
        if input.key_pressed(egui::Key::Enter) {
            confirm_pending_op(app);
        }
        if input.key_pressed(egui::Key::Escape) {
            app.clear_pending_op();
        }
        ctx.request_repaint();
        return;
    }
    if handle_inline_rename(app, input) {
        ctx.request_repaint();
        return;
    }
    if ctrl_g {
        app.open_quick_jump();
        ctx.request_repaint();
        return;
    }
    if let app_state::PanelMode::Edit(ref mut edit) = app.panel_mut(app.active_panel).mode {
        let enter = input.key_pressed(egui::Key::Enter);
        let escape = input.key_pressed(egui::Key::Escape);
        let ctrl_s = ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::S));
        let mut refresh_after = false;
        let mut save_payload: Option<(PathBuf, Vec<u8>)> = None;
        let mut close_editor = false;
        let mut remote_size_update: Option<(String, String, u64)> = None;
        // Tasks are collected here and enqueued through app.enqueue_io() after
        // the `edit` borrow of `app` ends, so io_in_flight/io_batch_total stay
        // accurate (a raw io_tx.send would leave the counter one short and make
        // a later batch's progress/cancel-reset fire early).
        let mut pending_io_tasks: Vec<core::IOTask> = Vec::new();
        if edit.confirm_discard {
            if enter {
                close_editor = true;
            } else if escape {
                edit.confirm_discard = false;
            }
            ctx.request_repaint();
            if close_editor {
                let return_focus = edit.return_focus;
                let panel = app.panel_mut(app.active_panel);
                panel.mode = app_state::PanelMode::Browser;
                app.active_panel = return_focus;
            }
            return;
        }
        if !input.events.is_empty() {
            ctx.request_repaint();
        }
        // Only a successfully-loaded buffer may be saved: a still-loading buffer
        // is empty (would truncate the file) and a failed one holds an error
        // message, not the file's contents.
        if ctrl_s && edit.load == app_state::EditLoad::Loaded {
            if let Some(path) = edit.path.clone() {
                // Restore CRLF line endings if the original file used them.
                let save_text = if edit.crlf {
                    edit.text.replace('\n', "\r\n")
                } else {
                    edit.text.clone()
                };
                let contents = save_text.as_bytes().to_vec();
                // Check if this is a remote file (synthetic /sftp/host/path)
                let path_str = path.to_string_lossy();
                if let Some(rest) = path_str.strip_prefix("/sftp/") {
                    // Parse host and remote path
                    if let Some(slash) = rest.find('/') {
                        let host = rest[..slash].to_string();
                        let remote_path = rest[slash..].to_string();
                        let size = contents.len() as u64;
                        remote_size_update = Some((host.clone(), remote_path.clone(), size));
                        save_payload = None; // Don't use the local write path
                        edit.dirty = false;
                        edit.confirm_discard = false;
                        // Don't refresh the whole panel — just update file size in place
                        close_editor = true;
                        pending_io_tasks.push(core::IOTask::WriteRemoteFile {
                            host,
                            path: remote_path,
                            contents,
                        });
                    }
                } else {
                    save_payload = Some((path, contents));
                    edit.dirty = false;
                    edit.confirm_discard = false;
                    refresh_after = true;
                    close_editor = true;
                }
            }
            ctx.request_repaint();
            if let Some((path, contents)) = save_payload {
                pending_io_tasks.push(core::IOTask::WriteFile {
                    path,
                    contents,
                    exclusive: false,
                });
            }
            if close_editor {
                let return_focus = edit.return_focus;
                let panel = app.panel_mut(app.active_panel);
                panel.mode = app_state::PanelMode::Browser;
                app.active_panel = return_focus;
            }
            // `edit` is no longer borrowed here; enqueue through app so the
            // in-flight counter is maintained.
            for task in pending_io_tasks {
                app.enqueue_io(task);
            }
            if let Some((ref host, ref rpath, size)) = remote_size_update {
                for side in [core::ActivePanel::Left, core::ActivePanel::Right] {
                    for entry in &mut app.panel_mut(side).browser_mut().entries {
                        if let core::EntryLocation::Remote {
                            host: ref h,
                            path: ref p,
                        } = entry.location
                            && h == host
                            && p == rpath
                        {
                            entry.size = Some(size);
                        }
                    }
                }
            }
            if refresh_after {
                refresh_active_panel(app);
            }
            return;
        }
        if escape {
            if edit.dirty {
                edit.confirm_discard = true;
            } else {
                close_editor = true;
            }
            ctx.request_repaint();
            if close_editor {
                let return_focus = edit.return_focus;
                let panel = app.panel_mut(app.active_panel);
                panel.mode = app_state::PanelMode::Browser;
                app.active_panel = return_focus;
            }
            return;
        }
        return;
    }
    if app.search_ui == app_state::SearchUiState::Open {
        if input.key_pressed(egui::Key::Escape) {
            cancel_search(app);
            app.search_ui = app_state::SearchUiState::Closed;
            ctx.request_repaint();
            return;
        }
        let ctrl_enter = ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::Enter));
        if ctrl_enter {
            start_search(app);
            ctx.request_repaint();
        }
    }
    {
        let alt_enter = ctx.input_mut(|i| i.consume_key(egui::Modifiers::ALT, egui::Key::Enter));
        if alt_enter {
            open_props_dialog(app);
            ctx.request_repaint();
            return;
        }
    }
    // Preview find bar: handle Escape (close), Enter (next), Shift+Enter (prev) before
    // the global Shift+Enter handler steals the event.
    if let app_state::PanelMode::Preview(ref mut preview) = app.panel_mut(app.active_panel).mode
        && preview.find_open
    {
        if input.key_pressed(egui::Key::Escape) {
            preview.find_open = false;
            ctx.request_repaint();
            return;
        }
        let shift_enter =
            ctx.input_mut(|i| i.consume_key(egui::Modifiers::SHIFT, egui::Key::Enter));
        if shift_enter {
            preview_find_prev(app);
            ctx.request_repaint();
            return;
        }
    }
    let shift_enter = ctx.input_mut(|i| i.consume_key(egui::Modifiers::SHIFT, egui::Key::Enter));
    if shift_enter {
        open_selected_external(app);
        ctx.request_repaint();
        return;
    }
    if ctrl_shift_o {
        reveal_selected_in_file_manager(app);
        ctx.request_repaint();
        return;
    }
    if input.key_pressed(egui::Key::Escape) && app.preview_panel_side().is_none() {
        let panel = app.get_active_panel();
        let browser = panel.browser();
        if matches!(browser.browser_mode, core::BrowserMode::Search { .. }) {
            cancel_search(app);
            if let Some(snapshot) = app.pop_history_back(app.active_panel) {
                apply_panel_snapshot(app, app.active_panel, snapshot);
            }
            ctx.request_repaint();
            return;
        }
    }
    let window_rows = active_window_rows(app, cache);
    let tab_pressed = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Tab));
    if tab_pressed {
        if app.props_dialog().is_none() {
            app.switch_panel();
        }
        ctx.request_repaint();
    }
    // Tab management — only in Browser mode
    let active_is_browser_mode =
        matches!(app.get_active_panel().mode, app_state::PanelMode::Browser);
    let ctrl_shift_tab = ctx.input_mut(|i| {
        i.consume_key(
            egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
            egui::Key::Tab,
        )
    });
    if ctrl_shift_tab && active_is_browser_mode {
        app.get_active_panel_mut().prev_tab();
        refresh_active_panel(app);
        ctx.request_repaint();
    }
    let ctrl_tab = ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::Tab));
    if ctrl_tab && active_is_browser_mode {
        app.get_active_panel_mut().next_tab();
        refresh_active_panel(app);
        ctx.request_repaint();
    }
    let ctrl_t = ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::T));
    if ctrl_t && active_is_browser_mode {
        app.get_active_panel_mut().new_tab();
        refresh_active_panel(app);
        ctx.request_repaint();
    }
    let ctrl_w = ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::W));
    if ctrl_w && active_is_browser_mode {
        app.get_active_panel_mut().close_tab();
        ctx.request_repaint();
    }
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::U)) {
        app.swap_panels();
        ctx.request_repaint();
    }
    let ctrl_pgup = ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::PageUp));
    let backspace = input.key_pressed(egui::Key::Backspace);
    let typing_in_ui = ctx.egui_wants_keyboard_input();
    if ctrl_pgup || (backspace && !typing_in_ui) {
        open_parent(app, window_rows);
    }
    let ctrl_pgdn = ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::PageDown));
    if ctrl_pgdn {
        open_selected(app);
    }
    let ctrl_r = ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::R));
    // Bare F2 only (see the F1 note above); Ctrl+R is the modified path.
    if ctrl_r || (f2 && input.modifiers.is_none()) {
        refresh_active_panel(app);
    }
    let space = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Space));
    if space {
        let selected = {
            let panel = app.get_active_panel();
            let browser = panel.browser();
            browser.entries.get(browser.selected_index).cloned()
        };
        if let Some(entry) = selected
            && entry.is_dir
        {
            match entry.location {
                core::EntryLocation::Fs(path) => {
                    if !app.dir_size_pending.contains(&path) {
                        app.dir_size_pending.insert(path.clone());
                        let _ = app.dir_size_tx.send(path);
                    }
                }
                core::EntryLocation::Remote { host, path } => {
                    let key = (host.clone(), path.clone());
                    if !app.remote_dir_size_pending.contains(&key) {
                        app.remote_dir_size_pending.insert(key);
                        let _ = app.remote_dir_size_tx.send((host, path));
                    }
                }
                core::EntryLocation::Container { .. } => {}
            }
        }
    }
    let ctrl_left = ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::ArrowLeft));
    if ctrl_left && app.active_panel == core::ActivePanel::Right {
        open_selected_from_to(app, core::ActivePanel::Right, core::ActivePanel::Left);
    }
    let ctrl_right = ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::ArrowRight));
    if ctrl_right && app.active_panel == core::ActivePanel::Left {
        open_selected_from_to(app, core::ActivePanel::Left, core::ActivePanel::Right);
    }
    let alt_left = ctx.input_mut(|i| i.consume_key(egui::Modifiers::ALT, egui::Key::ArrowLeft));
    if alt_left && let Some(snapshot) = app.pop_history_back(app.active_panel) {
        apply_panel_snapshot(app, app.active_panel, snapshot);
    }
    let alt_right = ctx.input_mut(|i| i.consume_key(egui::Modifiers::ALT, egui::Key::ArrowRight));
    if alt_right && let Some(snapshot) = app.pop_history_forward(app.active_panel) {
        apply_panel_snapshot(app, app.active_panel, snapshot);
    }
    // Shift+Alt+F7 must be consumed before Alt+F7, because egui's
    // consume_key(ALT, ...) matches regardless of the Shift modifier.
    let shift_alt_f7 = ctx
        .input_mut(|i| i.consume_key(egui::Modifiers::ALT | egui::Modifiers::SHIFT, egui::Key::F7));
    if shift_alt_f7 {
        open_search(app, core::SearchMode::Content);
    }
    let alt_f7 = ctx.input_mut(|i| i.consume_key(egui::Modifiers::ALT, egui::Key::F7));
    if alt_f7 {
        open_search(app, core::SearchMode::Name);
    }
    // Only plain Enter navigates here. `input` is a frozen snapshot and
    // key_pressed ignores modifiers, so without these guards this block also
    // fired for Ctrl+Enter (start search — already handled above, would run
    // twice), Shift+Enter (open-external), and Enter in preview mode (find-next
    // below) — each a double action off one keypress.
    let plain_enter = input.key_pressed(egui::Key::Enter)
        && !input.modifiers.ctrl
        && !input.modifiers.alt
        && !input.modifiers.shift;
    if !in_preview && plain_enter {
        if app.search_ui == app_state::SearchUiState::Open {
            if matches!(
                app.get_active_panel().browser().browser_mode,
                core::BrowserMode::Search { .. }
            ) {
                // Fall through to open selected result below.
            } else {
                start_search(app);
                app.search_ui = app_state::SearchUiState::Closed;
                ctx.request_repaint();
                // Don't fall through — search just started, no results to navigate yet.
            }
        } else if matches!(
            app.get_active_panel().browser().browser_mode,
            core::BrowserMode::Search { .. }
        ) {
            app.push_history(app.active_panel);
            let panel = app.get_active_panel();
            let browser = panel.browser();
            let entry = browser.entries.get(browser.selected_index).cloned();
            if let Some(entry) = entry {
                match entry.location {
                    core::EntryLocation::Fs(path) => {
                        if entry.is_dir {
                            load_fs_directory_async(app, path, app.active_panel, None);
                        } else if let Some(parent) = path.parent() {
                            let name = path
                                .file_name()
                                .and_then(|s| s.to_str())
                                .map(|s| s.to_string());
                            load_fs_directory_async(
                                app,
                                parent.to_path_buf(),
                                app.active_panel,
                                name,
                            );
                        }
                    }
                    core::EntryLocation::Remote { host, path } => {
                        if entry.is_dir {
                            crate::load_sftp_directory_async(
                                app,
                                &host,
                                &path,
                                app.active_panel,
                                None,
                            );
                        } else {
                            let slash = path.rfind('/').unwrap_or(0);
                            let parent = if slash == 0 { "/" } else { &path[..slash] };
                            let name = path[slash + 1..].to_string();
                            crate::load_sftp_directory_async(
                                app,
                                &host,
                                parent,
                                app.active_panel,
                                Some(name),
                            );
                        }
                    }
                    core::EntryLocation::Container { .. } => {}
                }
            }
            app.search_ui = app_state::SearchUiState::Closed;
        } else if app.theme_picker_open() {
            app.apply_selected_theme();
        } else {
            open_selected(app);
        }
    }
    if let app_state::PanelMode::Preview(ref mut preview) = app.panel_mut(app.active_panel).mode {
        let line = preview.line_height.max(16.0);
        let page = preview.page_height.max(200.0);
        let mut consumed = false;
        let can_scroll = preview.can_scroll;
        let max = preview.max_scroll;
        if can_scroll && input.key_pressed(egui::Key::ArrowDown) {
            preview.scroll = (preview.scroll + line).min(max);
            consumed = true;
        }
        if can_scroll && input.key_pressed(egui::Key::ArrowUp) {
            preview.scroll = (preview.scroll - line).max(0.0);
            consumed = true;
        }
        if can_scroll && input.key_pressed(egui::Key::PageDown) {
            preview.scroll = (preview.scroll + page).min(max);
            consumed = true;
        }
        if can_scroll && input.key_pressed(egui::Key::PageUp) {
            preview.scroll = (preview.scroll - page).max(0.0);
            consumed = true;
        }
        if can_scroll && input.key_pressed(egui::Key::Home) {
            preview.scroll = 0.0;
            consumed = true;
        }
        if can_scroll && input.key_pressed(egui::Key::End) {
            preview.scroll = max;
            consumed = true;
        }
        let find_open = preview.find_open;
        let enter = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
        // Drop the `preview` borrow before calling preview_find_next (needs full &mut app).
        let _ = preview;
        if find_open && enter {
            preview_find_next(app);
            consumed = true;
        }
        if consumed {
            ctx.request_repaint();
            return;
        }
    }
    let ctrl_f = ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::F));
    if ctrl_f {
        if let app_state::PanelMode::Preview(ref mut preview) = app.panel_mut(app.active_panel).mode
        {
            preview.find_open = true;
            preview.find_focus = true;
        } else {
            open_search(app, core::SearchMode::Name);
        }
        ctx.request_repaint();
    }
    // Rebuild matches every frame when find bar is open (picks up query edits)
    if let app_state::PanelMode::Preview(ref mut preview) = app.panel_mut(app.active_panel).mode
        && preview.find_open
    {
        let prev_count = preview.find_matches.len();
        preview_rebuild_matches(preview);
        if preview.find_matches.len() != prev_count {
            ctx.request_repaint();
        }
    }
    let app_state::PanelState {
        mode: active_mode, ..
    } = app.panel(app.active_panel);
    let active_is_browser = matches!(active_mode, app_state::PanelMode::Browser);
    // Whether an arrow key was pressed together with Shift, read from the
    // event's own modifiers rather than the aggregate InputState.modifiers, so
    // it holds both in the live app and under the replay harness (which sets
    // per-event modifiers but leaves the aggregate at NONE).
    let arrow_shift = |key: egui::Key| {
        input.events.iter().any(|e| {
            matches!(
                e,
                egui::Event::Key { key: k, modifiers, pressed: true, .. }
                if *k == key && modifiers.shift
            )
        })
    };
    if input.key_pressed(egui::Key::ArrowDown)
        && active_is_browser
        && !arrow_shift(egui::Key::ArrowDown)
    {
        if app.theme_picker_open() {
            app.select_next_theme();
        } else {
            let browser = app.get_active_panel().browser();
            if browser.selected_index + 1 < browser.entries.len() {
                app.select_entry(browser.selected_index + 1, window_rows);
            }
        }
    }
    if input.key_pressed(egui::Key::ArrowUp)
        && active_is_browser
        && !arrow_shift(egui::Key::ArrowUp)
    {
        if app.theme_picker_open() {
            app.select_prev_theme();
        } else {
            let browser = app.get_active_panel().browser();
            if browser.selected_index > 0 {
                app.select_entry(browser.selected_index - 1, window_rows);
            }
        }
    }
    // Shift+Up / Shift+Down extend the marked range while moving the cursor:
    // toggle the current row's mark (skipping ".."), then step in that direction.
    let shift_down = arrow_shift(egui::Key::ArrowDown);
    let shift_up = arrow_shift(egui::Key::ArrowUp);
    if active_is_browser && !app.theme_picker_open() && (shift_down || shift_up) {
        let down = shift_down;
        let (mark_name, next_index) = {
            let browser = app.get_active_panel().browser();
            let idx = browser.selected_index;
            let len = browser.entries.len();
            let mark_name = browser
                .entries
                .get(idx)
                .filter(|e| e.name != "..")
                .map(|e| e.name.clone());
            let next_index = if down {
                (idx + 1 < len).then_some(idx + 1)
            } else {
                (idx > 0).then_some(idx - 1)
            };
            (mark_name, next_index)
        };
        if let Some(name) = mark_name {
            let browser = app.get_active_panel_mut().browser_mut();
            if !browser.marked.remove(&name) {
                browser.marked.insert(name);
            }
        }
        if let Some(next) = next_index {
            app.select_entry(next, window_rows);
        }
    }
    if (input.key_pressed(egui::Key::Insert) || ctrl_i) && active_is_browser {
        let browser = app.get_active_panel_mut().browser_mut();
        let idx = browser.selected_index;
        if idx < browser.entries.len() && browser.entries[idx].name != ".." {
            let name = browser.entries[idx].name.clone();
            if !browser.marked.remove(&name) {
                browser.marked.insert(name);
            }
            if idx + 1 < browser.entries.len() {
                browser.selected_index = idx + 1;
            }
        }
    }
    // Plain PageUp/PageDown page the row cursor. Exclude Ctrl, which is the
    // parent-folder / open-selected binding consumed above — key_pressed
    // ignores modifiers, so without this the cursor also jumped on Ctrl+PgUp/Dn.
    if input.key_pressed(egui::Key::PageUp) && !input.modifiers.ctrl && active_is_browser {
        let browser = app.get_active_panel().browser();
        let new_index = browser.selected_index.saturating_sub(window_rows);
        app.select_entry(new_index, window_rows);
    }
    if input.key_pressed(egui::Key::PageDown) && !input.modifiers.ctrl && active_is_browser {
        let browser = app.get_active_panel().browser();
        let len = browser.entries.len();
        let mut new_index = browser.selected_index.saturating_add(window_rows);
        if len > 0 && new_index >= len {
            new_index = len - 1;
        }
        app.select_entry(new_index, window_rows);
    }
    if input.key_pressed(egui::Key::Home) && active_is_browser {
        app.select_entry(0, window_rows);
    }
    if input.key_pressed(egui::Key::End) && active_is_browser {
        let browser = app.get_active_panel().browser();
        if !browser.entries.is_empty() {
            app.select_entry(browser.entries.len() - 1, window_rows);
        }
    }
    if input.key_pressed(egui::Key::F3) || ctrl_p {
        app.toggle_preview();
    }
    if input.key_pressed(egui::Key::Escape) {
        if app.theme_picker_open() {
            app.close_theme_picker();
        } else {
            app.clear_preview();
        }
    }
    let other_panel_preview = app
        .preview_panel_side()
        .is_some_and(|side| side != app.active_panel);
    let alt_f5 = ctx.input_mut(|i| i.consume_key(egui::Modifiers::ALT, egui::Key::F5));
    if (alt_f5 || ctrl_a) && !other_panel_preview {
        app.prepare_pack_selected();
        ctx.request_repaint();
    }
    if (input.key_pressed(egui::Key::F5) || ctrl_c) && !other_panel_preview {
        app.prepare_copy_selected();
        ctx.request_repaint();
    }
    if ctrl_shift_c && let Some(text) = build_selected_paths(app) {
        ctx.copy_text(text);
        ctx.request_repaint();
    }
    if ctrl_comma {
        crate::ui::settings::open(app);
        ctx.request_repaint();
    }
    // Modified F-key variants must be consumed before bare variants,
    // because egui's consume_key(NONE, ...) matches regardless of modifiers.
    let shift_f4 = ctx.input_mut(|i| i.consume_key(egui::Modifiers::SHIFT, egui::Key::F4));
    if shift_f4 || ctrl_n {
        app.start_inline_new_file();
        ctx.request_repaint();
    }
    let f4 = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F4));
    if f4 || ctrl_e {
        app.prepare_edit_selected();
        ctx.request_repaint();
    }
    let shift_f6 = ctx.input_mut(|i| i.consume_key(egui::Modifiers::SHIFT, egui::Key::F6));
    if shift_f6 || ctrl_shift_m {
        app.prepare_rename_selected();
        ctx.request_repaint();
    }
    let f6 = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F6));
    if (f6 || ctrl_m) && !other_panel_preview {
        app.prepare_move_selected();
        ctx.request_repaint();
    }
    let f7 = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F7));
    if f7 || ctrl_d {
        app.start_inline_new_dir();
        ctx.request_repaint();
    }
    if input.key_pressed(egui::Key::F9) {
        app.switch_theme();
        ctx.request_repaint();
    }
    if input.key_pressed(egui::Key::F10) {
        app.open_theme_picker();
        ctx.request_repaint();
    }
    if input.key_pressed(egui::Key::F8) || ctrl_x {
        app.prepare_delete_selected();
        ctx.request_repaint();
    }
}

fn open_parent(app: &mut app_state::AppState, window_rows: usize) {
    let panel = app.get_active_panel();
    let browser = panel.browser();
    let parent_index = browser.entries.iter().position(|e| e.name == "..");
    let Some(idx) = parent_index else { return };
    if browser.selected_index != idx {
        app.select_entry(idx, window_rows);
    }
    open_selected(app);
}

pub(crate) fn confirm_pending_op(app: &mut app_state::AppState) {
    if let Some(op) = app.take_pending_op() {
        if let app_state::PendingOp::Delete { targets } = &op
            && let Some(first) = targets.first()
        {
            let panel = app.get_active_panel();
            let browser = panel.browser();
            let mut next_name: Option<String> = None;
            if !browser.entries.is_empty() {
                let mut next_idx = browser.selected_index.saturating_add(1);
                while next_idx < browser.entries.len() {
                    let candidate = &browser.entries[next_idx].name;
                    if candidate != ".." {
                        next_name = Some(candidate.clone());
                        break;
                    }
                    next_idx += 1;
                }
                if next_name.is_none() {
                    let mut prev_idx = browser.selected_index.saturating_sub(1);
                    while prev_idx < browser.entries.len() {
                        let candidate = &browser.entries[prev_idx].name;
                        if candidate != ".." {
                            next_name = Some(candidate.clone());
                            break;
                        }
                        if prev_idx == 0 {
                            break;
                        }
                        prev_idx -= 1;
                    }
                }
            }
            let parent = first.parent().unwrap_or_else(|| std::path::Path::new("."));
            if let Some(next_name) = next_name {
                // Move the cursor onto the surviving neighbor now so it stays
                // put across the async delete and the refresh that follows —
                // the post-delete reload restores selection by name, which works
                // for remote and container listings too (fs_last_selected_name
                // is keyed by local path and doesn't cover those).
                let browser = app.get_active_panel_mut().browser_mut();
                if let Some(idx) = browser.entries.iter().position(|e| e.name == next_name) {
                    browser.selected_index = idx;
                }
                app.fs_last_selected_name
                    .insert(parent.to_path_buf(), next_name);
            } else {
                app.fs_last_selected_name.remove(parent);
            }
        }
        if let app_state::PendingOp::Rename { src } = &op {
            let name = app.rename_input.clone().unwrap_or_default();
            if name.is_empty()
                || name == "."
                || name == ".."
                || name.contains('/')
                || name.contains('\\')
            {
                app.clear_pending_op();
                return;
            }
            if let Some(current) = src.file_name().and_then(|n| n.to_str())
                && current == name
            {
                app.clear_pending_op();
                return;
            }
            app.store_selection_memory_for(app.active_panel);
            app.fs_last_selected_name.insert(
                src.parent()
                    .unwrap_or_else(|| std::path::Path::new("."))
                    .to_path_buf(),
                name,
            );
        }
        if let app_state::PendingOp::Pack { .. } = &op {
            let name = app.rename_input.clone().unwrap_or_default();
            if name.is_empty()
                || name == "."
                || name == ".."
                || name.contains('/')
                || name.contains('\\')
            {
                app.clear_pending_op();
                return;
            }
            if archive::container_kind_from_path(std::path::Path::new(&name)).is_none() {
                app.clear_pending_op();
                return;
            }
        }
        app.enqueue_pending_op(&op);
        match op {
            app_state::PendingOp::Copy { .. } => {
                // Only refresh the destination (other) panel — source is unchanged
                let dst = match app.active_panel {
                    core::ActivePanel::Left => core::ActivePanel::Right,
                    core::ActivePanel::Right => core::ActivePanel::Left,
                };
                crate::reload_panel(app, dst);
            }
            app_state::PendingOp::Move { .. } => {
                // Store next-neighbor name so cursor stays near the moved item
                store_neighbor_selection(app);
                refresh_fs_panels(app);
            }
            app_state::PendingOp::Rename { .. } => refresh_fs_panels(app),
            app_state::PendingOp::Delete { .. } => {
                // Cursor was already moved onto the neighbor above; the reload
                // restores it by name.
                refresh_active_panel(app);
            }
            app_state::PendingOp::Pack { .. } => refresh_active_panel(app),
        }
    }
}

/// Store the neighboring entry name so the cursor lands nearby after a
/// destructive operation (move/delete) removes the current selection.
fn store_neighbor_selection(app: &mut app_state::AppState) {
    let panel = app.get_active_panel();
    let browser = panel.browser();
    if browser.entries.is_empty() {
        return;
    }
    let idx = browser.selected_index;
    // Prefer the next entry, fall back to the previous one
    let neighbor = browser
        .entries
        .get(idx + 1)
        .or_else(|| {
            if idx > 0 {
                browser.entries.get(idx - 1)
            } else {
                None
            }
        })
        .filter(|e| e.name != "..")
        .map(|e| e.name.clone());
    if let Some(name) = neighbor {
        let dir = browser.current_path.clone();
        app.fs_last_selected_name.insert(dir, name);
    }
}

fn handle_inline_rename(app: &mut app_state::AppState, input: &egui::InputState) -> bool {
    let enter = input.key_pressed(egui::Key::Enter);
    let escape = input.key_pressed(egui::Key::Escape);
    let (action, next_selection, handled) = {
        let panel = app.get_active_panel_mut();
        let browser = panel.browser_mut();
        let Some(_rename) = browser.inline_rename.as_ref() else {
            return false;
        };
        if !enter && !escape {
            return true;
        }
        let rename = browser.inline_rename.take().unwrap();
        if escape {
            if !matches!(rename.kind, app_state::InlineEditKind::Rename)
                && rename.index < browser.entries.len()
            {
                browser.entries.remove(rename.index);
                if browser.selected_index >= browser.entries.len() && !browser.entries.is_empty() {
                    browser.selected_index = browser.entries.len() - 1;
                }
            }
            return true;
        }
        let new_name = rename.text.trim();
        if new_name.is_empty()
            || new_name == "."
            || new_name == ".."
            || new_name.contains('/')
            || new_name.contains('\\')
        {
            return true;
        }
        let mut action: Option<fileman::core::IOTask> = None;
        let mut next_selection: Option<(PathBuf, String)> = None;
        match rename.kind {
            app_state::InlineEditKind::NewFile => {
                if let core::BrowserMode::Remote { ref host, ref path } = browser.browser_mode {
                    let host = host.clone();
                    let base_path = path.clone();
                    let remote_path = format!("{}/{}", base_path.trim_end_matches('/'), new_name);
                    if rename.index < browser.entries.len() {
                        browser.entries[rename.index].name = new_name.to_string();
                        browser.entries[rename.index].location = core::EntryLocation::Remote {
                            host: host.clone(),
                            path: remote_path.clone(),
                        };
                    }
                    action = Some(fileman::core::IOTask::WriteRemoteFile {
                        host,
                        path: remote_path,
                        contents: Vec::new(),
                    });
                } else {
                    let dir = browser.current_path.clone();
                    let path = dir.join(new_name);
                    action = Some(fileman::core::IOTask::WriteFile {
                        path: path.clone(),
                        contents: Vec::new(),
                        exclusive: true,
                    });
                    next_selection = Some((dir, new_name.to_string()));
                    if rename.index < browser.entries.len() {
                        browser.entries[rename.index].name = new_name.to_string();
                        browser.entries[rename.index].location = core::EntryLocation::Fs(path);
                    }
                }
            }
            app_state::InlineEditKind::NewDir => {
                if let core::BrowserMode::Remote { ref host, ref path } = browser.browser_mode {
                    let host = host.clone();
                    let base_path = path.clone();
                    let remote_path = format!("{}/{}", base_path.trim_end_matches('/'), new_name);
                    if rename.index < browser.entries.len() {
                        browser.entries[rename.index].name = new_name.to_string();
                        browser.entries[rename.index].location = core::EntryLocation::Remote {
                            host: host.clone(),
                            path: remote_path.clone(),
                        };
                    }
                    action = Some(fileman::core::IOTask::MkdirRemote {
                        host,
                        path: remote_path,
                    });
                } else {
                    let dir = browser.current_path.clone();
                    let path = dir.join(new_name);
                    action = Some(fileman::core::IOTask::Mkdir { path: path.clone() });
                    next_selection = Some((dir, new_name.to_string()));
                    if rename.index < browser.entries.len() {
                        browser.entries[rename.index].name = new_name.to_string();
                        browser.entries[rename.index].location = core::EntryLocation::Fs(path);
                    }
                }
            }
            app_state::InlineEditKind::Rename => {
                if rename.index < browser.entries.len() {
                    let entry = &browser.entries[rename.index];
                    match &entry.location {
                        core::EntryLocation::Fs(path) => {
                            let current = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                            if current != new_name {
                                action = Some(fileman::core::IOTask::Rename {
                                    src: path.clone(),
                                    new_name: new_name.to_string(),
                                });
                                next_selection = Some((
                                    path.parent()
                                        .unwrap_or_else(|| std::path::Path::new("."))
                                        .to_path_buf(),
                                    new_name.to_string(),
                                ));
                            }
                        }
                        core::EntryLocation::Remote { host, path } => {
                            let current = path.rsplit('/').next().unwrap_or("");
                            if current != new_name {
                                action = Some(fileman::core::IOTask::RenameRemote {
                                    host: host.clone(),
                                    src: path.clone(),
                                    new_name: new_name.to_string(),
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        (action, next_selection, true)
    };
    if !handled {
        return false;
    }
    if let Some(task) = action {
        let _ = app.io_tx.send(task);
        app.io_in_flight = app.io_in_flight.saturating_add(1);
        if let Some((dir, name)) = next_selection {
            app.fs_last_selected_name.insert(dir, name);
        }
    }
    true
}

/// Build the clipboard text for "copy path" — all marked entries (one per line)
/// or just the current selection if nothing is marked. `..` is always skipped.
pub(crate) fn build_selected_paths(app: &app_state::AppState) -> Option<String> {
    let panel = app.panel(app.active_panel);
    let browser = panel.browser();
    let format_entry = |entry: &core::DirEntry| -> Option<String> {
        if entry.name == ".." {
            return None;
        }
        match &entry.location {
            core::EntryLocation::Fs(path) => Some(path.to_string_lossy().into_owned()),
            core::EntryLocation::Remote { host, path } => Some(format!("{host}:{path}")),
            core::EntryLocation::Container {
                kind,
                archive_path,
                inner_path,
            } => Some(core::container_display_path(
                *kind,
                archive_path,
                inner_path,
            )),
        }
    };
    let paths: Vec<String> = if browser.marked.is_empty() {
        browser
            .entries
            .get(browser.selected_index)
            .and_then(format_entry)
            .into_iter()
            .collect()
    } else {
        browser
            .entries
            .iter()
            .filter(|e| browser.marked.contains(&e.name))
            .filter_map(format_entry)
            .collect()
    };
    if paths.is_empty() {
        return None;
    }
    Some(paths.join("\n"))
}
