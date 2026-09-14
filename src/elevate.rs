use std::path::Path;

use crate::core::IOTask;

/// Execute a command with elevated privileges using the platform-native mechanism.
///
/// - Linux: `pkexec`
/// - macOS: `osascript` with `do shell script ... with administrator privileges`
/// - Windows: `ShellExecuteExW` with `"runas"` verb
pub fn run_elevated(program: &str, args: &[&str]) -> Result<(), String> {
    run_elevated_platform(program, args)
}

#[cfg(target_os = "linux")]
fn run_elevated_platform(program: &str, args: &[&str]) -> Result<(), String> {
    let status = std::process::Command::new("pkexec")
        .arg(program)
        .args(args)
        .status()
        .map_err(|e| format!("Failed to launch pkexec: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        match status.code() {
            Some(126) => Err("Elevation cancelled by user".to_string()),
            Some(code) => Err(format!("Elevated command failed (exit code {code})")),
            None => Err("Elevated command was killed by signal".to_string()),
        }
    }
}

#[cfg(target_os = "macos")]
fn run_elevated_platform(program: &str, args: &[&str]) -> Result<(), String> {
    let mut cmd_parts = vec![shell_escape(program)];
    for arg in args {
        cmd_parts.push(shell_escape(arg));
    }
    let cmd = cmd_parts.join(" ");
    // Escape for AppleScript string literal (backslash and double-quote).
    let escaped = cmd.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        "do shell script \"{}\" with administrator privileges",
        escaped
    );
    let status = std::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .status()
        .map_err(|e| format!("Failed to launch osascript: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        match status.code() {
            Some(-128) => Err("Elevation cancelled by user".to_string()),
            Some(code) => Err(format!("Elevated command failed (exit code {code})")),
            None => Err("Elevated command was killed by signal".to_string()),
        }
    }
}

#[cfg(target_os = "macos")]
fn shell_escape(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '/' || c == '.' || c == '-' || c == '_')
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

#[cfg(target_os = "windows")]
fn run_elevated_platform(program: &str, args: &[&str]) -> Result<(), String> {
    use std::ffi::OsStr;
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
        fn WaitForSingleObject(hHandle: *mut std::ffi::c_void, dwMilliseconds: u32) -> u32;
        fn GetExitCodeProcess(hProcess: *mut std::ffi::c_void, lpExitCode: *mut u32) -> i32;
        fn CloseHandle(hObject: *mut std::ffi::c_void) -> i32;
    }

    const SEE_MASK_NOCLOSEPROCESS: u32 = 0x00000040;
    const SW_HIDE: i32 = 0;
    const INFINITE: u32 = 0xFFFFFFFF;

    fn to_wide(s: &str) -> Vec<u16> {
        OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let verb = to_wide("runas");
    let file = to_wide(program);
    let params_str = args
        .iter()
        .map(|a| {
            if a.contains(' ') || a.contains('"') {
                format!("\"{}\"", a.replace('"', "\\\""))
            } else {
                a.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    let params = to_wide(&params_str);

    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        hwnd: ptr::null_mut(),
        lpVerb: verb.as_ptr(),
        lpFile: file.as_ptr(),
        lpParameters: params.as_ptr(),
        lpDirectory: ptr::null(),
        nShow: SW_HIDE,
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
        return Err("Elevation cancelled or failed".to_string());
    }
    if info.hProcess.is_null() {
        return Ok(());
    }

    unsafe {
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut exit_code: u32 = 1;
        GetExitCodeProcess(info.hProcess, &mut exit_code);
        CloseHandle(info.hProcess);
        if exit_code == 0 {
            Ok(())
        } else {
            Err(format!("Elevated command failed (exit code {exit_code})"))
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn run_elevated_platform(_program: &str, _args: &[&str]) -> Result<(), String> {
    Err("Privilege elevation is not supported on this platform".to_string())
}

/// Check whether the platform elevation tool is available.
pub fn elevation_available() -> bool {
    elevation_available_platform()
}

#[cfg(target_os = "linux")]
fn elevation_available_platform() -> bool {
    std::process::Command::new("which")
        .arg("pkexec")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn elevation_available_platform() -> bool {
    // osascript is always available on macOS
    true
}

#[cfg(target_os = "windows")]
fn elevation_available_platform() -> bool {
    // ShellExecuteExW is always available
    true
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn elevation_available_platform() -> bool {
    false
}

/// Execute an IOTask with elevated privileges by translating it to shell commands.
pub fn execute_elevated(task: &IOTask) -> Result<(), String> {
    match *task {
        IOTask::Delete { ref target } => elevated_delete(target),
        IOTask::Copy {
            ref src,
            ref dst_dir,
            ref dest_name,
        } => elevated_copy(src, dst_dir, dest_name),
        IOTask::Move {
            ref src,
            ref dst_dir,
            ref dest_name,
        } => elevated_move(src, dst_dir, dest_name),
        IOTask::Rename {
            ref src,
            ref new_name,
        } => {
            let target = src.with_file_name(new_name);
            elevated_rename(src, &target)
        }
        IOTask::Mkdir { ref path } => elevated_mkdir(path),
        IOTask::WriteFile {
            ref path,
            ref contents,
            ..
        } => elevated_write_file(path, contents),
        #[cfg(unix)]
        IOTask::SetProps {
            ref path,
            mode,
            uid,
            gid,
            recursive,
        } => elevated_set_props(path, mode, uid, gid, recursive),
        _ => Err("This operation does not support elevation".to_string()),
    }
}

// --- Platform-branching helpers ---

#[cfg(unix)]
fn elevated_delete(target: &Path) -> Result<(), String> {
    run_elevated("rm", &["-rf", &target.to_string_lossy()])
}

#[cfg(windows)]
fn elevated_delete(target: &Path) -> Result<(), String> {
    let path_str = target.to_string_lossy();
    if target.is_dir() {
        run_elevated("cmd", &["/C", "rmdir", "/s", "/q", &path_str])
    } else {
        run_elevated("cmd", &["/C", "del", "/f", "/q", &path_str])
    }
}

#[cfg(unix)]
fn elevated_copy(src: &Path, dst_dir: &Path, dest_name: &str) -> Result<(), String> {
    let src_str = src.to_string_lossy();
    let dst_str = dst_dir.join(dest_name).to_string_lossy().into_owned();
    run_elevated("cp", &["-r", &src_str, &dst_str])
}

#[cfg(windows)]
fn elevated_copy(src: &Path, dst_dir: &Path, dest_name: &str) -> Result<(), String> {
    let dst = dst_dir.join(dest_name);
    let src_str = src.to_string_lossy();
    let dst_str = dst.to_string_lossy();
    if src.is_dir() {
        run_elevated(
            "cmd",
            &["/C", "xcopy", "/e", "/i", "/y", &src_str, &dst_str],
        )
    } else {
        run_elevated("cmd", &["/C", "copy", "/y", &src_str, &dst_str])
    }
}

#[cfg(unix)]
fn elevated_move(src: &Path, dst_dir: &Path, dest_name: &str) -> Result<(), String> {
    let src_str = src.to_string_lossy();
    let dst_str = dst_dir.join(dest_name).to_string_lossy().into_owned();
    run_elevated("mv", &[&src_str, &dst_str])
}

#[cfg(windows)]
fn elevated_move(src: &Path, dst_dir: &Path, dest_name: &str) -> Result<(), String> {
    let src_str = src.to_string_lossy();
    let dst_str = dst_dir.join(dest_name).to_string_lossy().into_owned();
    run_elevated("cmd", &["/C", "move", "/y", &src_str, &dst_str])
}

#[cfg(unix)]
fn elevated_rename(old: &Path, new: &Path) -> Result<(), String> {
    let old_str = old.to_string_lossy();
    let new_str = new.to_string_lossy();
    run_elevated("mv", &[&old_str, &new_str])
}

#[cfg(windows)]
fn elevated_rename(old: &Path, new: &Path) -> Result<(), String> {
    let old_str = old.to_string_lossy();
    let new_name = new.file_name().unwrap_or_default().to_string_lossy();
    run_elevated("cmd", &["/C", "ren", &old_str, &new_name])
}

#[cfg(unix)]
fn elevated_mkdir(path: &Path) -> Result<(), String> {
    run_elevated("mkdir", &["-p", &path.to_string_lossy()])
}

#[cfg(windows)]
fn elevated_mkdir(path: &Path) -> Result<(), String> {
    run_elevated("cmd", &["/C", "mkdir", &path.to_string_lossy()])
}

fn elevated_write_file(path: &Path, contents: &[u8]) -> Result<(), String> {
    // Write to a temp file, then elevated-copy over the target.
    let tmp_dir = std::env::temp_dir();
    let tmp_name = format!("fileman_elev_{}", std::process::id());
    let tmp_path = tmp_dir.join(&tmp_name);

    // Write with restrictive permissions on Unix.
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp_path)
            .map_err(|e| format!("Failed to create temp file: {e}"))?;
        f.write_all(contents)
            .map_err(|e| format!("Failed to write temp file: {e}"))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&tmp_path, contents)
            .map_err(|e| format!("Failed to create temp file: {e}"))?;
    }

    let result = elevated_copy_file(&tmp_path, path);
    let _ = std::fs::remove_file(&tmp_path);
    result
}

#[cfg(unix)]
fn elevated_copy_file(src: &Path, dst: &Path) -> Result<(), String> {
    run_elevated("cp", &[&src.to_string_lossy(), &dst.to_string_lossy()])
}

#[cfg(windows)]
fn elevated_copy_file(src: &Path, dst: &Path) -> Result<(), String> {
    run_elevated(
        "cmd",
        &[
            "/C",
            "copy",
            "/y",
            &src.to_string_lossy(),
            &dst.to_string_lossy(),
        ],
    )
}

#[cfg(unix)]
fn elevated_set_props(
    path: &Path,
    mode: u32,
    uid: u32,
    gid: u32,
    recursive: bool,
) -> Result<(), String> {
    let path_str = path.to_string_lossy();
    let mode_str = format!("{:o}", mode);
    let owner_str = format!("{uid}:{gid}");

    let r_flag: &[&str] = if recursive { &["-R"] } else { &[] };

    let mut chmod_args: Vec<&str> = r_flag.to_vec();
    chmod_args.push(&mode_str);
    chmod_args.push(&path_str);
    run_elevated("chmod", &chmod_args)?;

    let mut chown_args: Vec<&str> = r_flag.to_vec();
    chown_args.push(&owner_str);
    chown_args.push(&path_str);
    run_elevated("chown", &chown_args)?;

    Ok(())
}
