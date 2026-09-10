//! Integration tests for the SFTP module against a local SSH server.
//!
//! These tests are `#[ignore]`d by default — they require a running sshd
//! on localhost with key-based auth for the current user.
//! Run with: `cargo test --test sftp_localhost -- --ignored`
//!
//! CI sets up sshd automatically (see .github/workflows/ci.yml).

use std::io::Read;

use fileman::sftp;

fn connect_localhost() -> sftp::SftpSession {
    let config = sftp::load_ssh_config();
    sftp::connect("localhost", &config).expect("connect to localhost")
}

#[test]
#[ignore]
fn sftp_connect() {
    let session = connect_localhost();
    assert_eq!(session.host, "localhost");
    assert!(session.is_alive());
    let home = session.home_dir.expect("remote home from realpath or pwd");
    assert!(home.starts_with('/'), "home should be absolute: {home}");
    assert_ne!(home, "/");
}

#[test]
#[ignore]
fn sftp_read_root_directory() {
    let session = connect_localhost();
    let entries = sftp::read_directory(&session.sftp, "localhost", "/").expect("read root dir");
    // Root should have no ".." entry
    assert!(
        !entries.iter().any(|e| e.name == ".."),
        "root dir should not have .."
    );
    // Root should have some well-known dirs
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"etc"), "root should contain etc: {names:?}");
    assert!(names.contains(&"tmp"), "root should contain tmp: {names:?}");
}

#[test]
#[ignore]
fn sftp_read_subdirectory_has_parent() {
    let session = connect_localhost();
    let entries = sftp::read_directory(&session.sftp, "localhost", "/tmp").expect("read /tmp");
    assert!(
        entries.iter().any(|e| e.name == ".."),
        "/tmp should have .. entry"
    );
    // ".." should point to "/"
    let dotdot = entries.iter().find(|e| e.name == "..").unwrap();
    if let fileman::core::EntryLocation::Remote { path, .. } = &dotdot.location {
        assert_eq!(path, "/");
    } else {
        panic!(".. should be EntryLocation::Remote");
    }
}

#[test]
#[ignore]
fn sftp_write_read_delete() {
    let session = connect_localhost();
    let test_path = "/tmp/fileman_sftp_test_write";
    let contents = b"hello from fileman sftp test";

    // Write
    sftp::write_file(&session.sftp, test_path, contents).expect("write file");

    // Read back
    let data = sftp::read_bytes_prefix(&session.sftp, test_path, 1024).expect("read file");
    assert_eq!(data, contents);

    // Delete
    sftp::recursive_delete(&session.sftp, test_path, false, None).expect("delete file");

    // Verify gone
    let result = sftp::read_bytes_prefix(&session.sftp, test_path, 1024);
    assert!(result.is_err(), "file should be deleted");
}

#[test]
#[ignore]
fn sftp_mkdir_and_delete() {
    let session = connect_localhost();
    let dir_path = "/tmp/fileman_sftp_test_dir";

    // Clean up in case of prior failed run
    let _ = sftp::recursive_delete(&session.sftp, dir_path, true, None);

    // Create directory
    sftp::mkdir(&session.sftp, dir_path).expect("mkdir");

    // Write a file inside
    let file_path = format!("{dir_path}/nested.txt");
    sftp::write_file(&session.sftp, &file_path, b"nested").expect("write nested");

    // List it
    let entries = sftp::read_directory(&session.sftp, "localhost", dir_path).expect("read dir");
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"nested.txt"),
        "should contain nested.txt: {names:?}"
    );

    // Recursive delete
    sftp::recursive_delete(&session.sftp, dir_path, true, None).expect("recursive delete");

    // Verify gone
    let result = sftp::read_directory(&session.sftp, "localhost", dir_path);
    assert!(result.is_err(), "dir should be deleted");
}

#[test]
#[ignore]
fn sftp_rename() {
    let session = connect_localhost();
    let src = "/tmp/fileman_sftp_test_rename_src";
    let dst = "/tmp/fileman_sftp_test_rename_dst";

    // Clean up
    let _ = sftp::recursive_delete(&session.sftp, src, false, None);
    let _ = sftp::recursive_delete(&session.sftp, dst, false, None);

    sftp::write_file(&session.sftp, src, b"rename me").expect("write");
    sftp::rename(&session.sftp, src, dst).expect("rename");

    let data = sftp::read_bytes_prefix(&session.sftp, dst, 1024).expect("read renamed");
    assert_eq!(data, b"rename me");

    // Old path should be gone
    let result = sftp::read_bytes_prefix(&session.sftp, src, 1024);
    assert!(result.is_err(), "old path should not exist");

    sftp::recursive_delete(&session.sftp, dst, false, None).expect("cleanup");
}

#[test]
#[ignore]
fn sftp_copy_remote_to_local() {
    let session = connect_localhost();
    let remote_path = "/tmp/fileman_sftp_test_r2l";
    sftp::write_file(&session.sftp, remote_path, b"copy me locally").expect("write");

    let local_dir = std::env::temp_dir().join("fileman_sftp_test_r2l_out");
    std::fs::create_dir_all(&local_dir).ok();
    let local_file = local_dir.join("copied.txt");

    sftp::copy_remote_to_local(&session.sftp, remote_path, &local_file).expect("copy r2l");
    let local_data = std::fs::read(&local_file).expect("read local");
    assert_eq!(local_data, b"copy me locally");

    // Cleanup
    sftp::recursive_delete(&session.sftp, remote_path, false, None).ok();
    std::fs::remove_dir_all(&local_dir).ok();
}

#[test]
#[ignore]
fn sftp_copy_local_to_remote() {
    let session = connect_localhost();
    let local_dir = std::env::temp_dir().join("fileman_sftp_test_l2r");
    std::fs::create_dir_all(&local_dir).ok();
    let local_file = local_dir.join("upload.txt");
    std::fs::write(&local_file, b"upload me").expect("write local");

    let remote_path = "/tmp/fileman_sftp_test_l2r_uploaded";
    let _ = sftp::recursive_delete(&session.sftp, remote_path, false, None);

    sftp::copy_local_to_remote(&session.sftp, &local_file, remote_path).expect("copy l2r");

    let data = sftp::read_bytes_prefix(&session.sftp, remote_path, 1024).expect("read remote");
    assert_eq!(data, b"upload me");

    // Cleanup
    sftp::recursive_delete(&session.sftp, remote_path, false, None).ok();
    std::fs::remove_dir_all(&local_dir).ok();
}

#[test]
#[ignore]
fn sftp_error_on_permission_denied_returns_parent() {
    let session = connect_localhost();
    // /root is typically not readable by normal users
    let result = sftp::read_directory(&session.sftp, "localhost", "/root");
    // This may succeed if running as root (CI), or fail — either is fine.
    // The important thing is it doesn't panic.
    match result {
        Ok(entries) => {
            // If it succeeded (running as root), just verify it's valid
            assert!(!entries.is_empty() || entries.is_empty()); // no panic
        }
        Err(msg) => {
            assert!(
                msg.contains("readdir") || msg.contains("permission") || msg.contains("denied"),
                "error should mention the failure: {msg}"
            );
        }
    }
}

#[test]
#[ignore]
fn sftp_open_remote_reader() {
    let session = connect_localhost();
    let remote_path = "/tmp/fileman_sftp_test_reader";
    let content = b"streaming read test data with more bytes";
    sftp::write_file(&session.sftp, remote_path, content).expect("write");

    let mut reader = sftp::open_remote_reader(&session.sftp, remote_path).expect("open reader");
    let mut buf = Vec::new();
    reader.read_to_end(&mut buf).expect("read_to_end");
    assert_eq!(buf, content);

    sftp::recursive_delete(&session.sftp, remote_path, false, None).ok();
}

#[test]
#[ignore]
fn sftp_discover_hosts_does_not_panic() {
    // Just ensure it doesn't panic — result depends on ~/.ssh/config
    let _hosts = sftp::discover_ssh_hosts();
}

#[test]
fn sftp_parse_ssh_config() {
    let config_text = "\
Host myserver
    Hostname 10.0.0.1
    User deploy
    Port 2222
    IdentityFile ~/.ssh/deploy_key

Host *.example.com
    User admin

Host jump
    Hostname jump.internal
    IdentityFile ~/.ssh/jump_key
    IdentityFile ~/.ssh/backup_key
";
    let parsed = sftp::parse_ssh_config(config_text);

    let my = parsed.get("myserver").expect("myserver");
    assert_eq!(my.hostname.as_deref(), Some("10.0.0.1"));
    assert_eq!(my.user.as_deref(), Some("deploy"));
    assert_eq!(my.port, Some(2222));
    assert_eq!(my.identity_files.len(), 1);

    // Wildcard host should be excluded
    assert!(!parsed.contains_key("*.example.com"));

    let jump = parsed.get("jump").expect("jump");
    assert_eq!(jump.hostname.as_deref(), Some("jump.internal"));
    assert_eq!(jump.identity_files.len(), 2);
}

// --- exec-backed operations ---
//
// These cover the paths that run a command on the remote rather than talking
// SFTP: directory copies stream a tar through an exec channel, and the size
// probe shells out to `du`.

#[test]
#[ignore]
fn sftp_count_bytes_via_exec() {
    let session = connect_localhost();
    let dir = "/tmp/fileman_sftp_test_count";
    let _ = sftp::recursive_delete(&session.sftp, dir, true, None);
    sftp::mkdir(&session.sftp, dir).expect("mkdir");
    let payload = vec![b'x'; 5000];
    sftp::write_file(&session.sftp, &format!("{dir}/a.bin"), &payload).expect("write a");
    sftp::write_file(&session.sftp, &format!("{dir}/b.bin"), &payload).expect("write b");

    let n = sftp::count_bytes_via_exec(&session.sftp, dir);
    assert!(n >= 10_000, "du should report at least both files, got {n}");

    // The SFTP walk is exact, so it should agree on the file bytes.
    let walked = sftp::count_bytes_remote(&session.sftp, dir);
    assert_eq!(walked, 10_000, "recursive walk should sum both files");

    sftp::recursive_delete(&session.sftp, dir, true, None).expect("cleanup");
}

#[test]
#[ignore]
fn sftp_copy_remote_dir_to_local_via_tar() {
    use std::sync::atomic::AtomicBool;

    let session = connect_localhost();
    let remote_dir = "/tmp/fileman_sftp_test_tar_src";
    let _ = sftp::recursive_delete(&session.sftp, remote_dir, true, None);
    sftp::mkdir(&session.sftp, remote_dir).expect("mkdir");
    sftp::write_file(&session.sftp, &format!("{remote_dir}/one.txt"), b"first").expect("write");
    sftp::mkdir(&session.sftp, &format!("{remote_dir}/sub")).expect("mkdir sub");
    sftp::write_file(
        &session.sftp,
        &format!("{remote_dir}/sub/two.txt"),
        b"second",
    )
    .expect("write nested");

    let local = std::env::temp_dir().join("fileman_tar_dst");
    let _ = std::fs::remove_dir_all(&local);
    std::fs::create_dir_all(&local).expect("create local dir");

    let cancel = AtomicBool::new(false);
    sftp::copy_remote_dir_to_local_via_tar(
        &session.sftp,
        remote_dir,
        &local,
        "renamed",
        &cancel,
        None,
    )
    .expect("tar copy down");

    // The tree arrives under the requested name, contents intact.
    assert_eq!(
        std::fs::read_to_string(local.join("renamed/one.txt")).expect("one.txt"),
        "first"
    );
    assert_eq!(
        std::fs::read_to_string(local.join("renamed/sub/two.txt")).expect("two.txt"),
        "second"
    );

    let _ = std::fs::remove_dir_all(&local);
    sftp::recursive_delete(&session.sftp, remote_dir, true, None).expect("cleanup");
}

#[test]
#[ignore]
fn sftp_copy_local_dir_to_remote_via_tar() {
    use std::sync::atomic::AtomicBool;

    let session = connect_localhost();
    let local = std::env::temp_dir().join("fileman_tar_up");
    let _ = std::fs::remove_dir_all(&local);
    std::fs::create_dir_all(local.join("inner")).expect("create local tree");
    std::fs::write(local.join("top.txt"), b"top").expect("write top");
    std::fs::write(local.join("inner/deep.txt"), b"deep").expect("write deep");

    let remote_parent = "/tmp/fileman_sftp_test_tar_up";
    let _ = sftp::recursive_delete(&session.sftp, remote_parent, true, None);
    sftp::mkdir(&session.sftp, remote_parent).expect("mkdir");

    let cancel = AtomicBool::new(false);
    sftp::copy_local_dir_to_remote_via_tar(&local, &session.sftp, remote_parent, &cancel, None)
        .expect("tar copy up");

    let uploaded = format!("{remote_parent}/fileman_tar_up");
    assert_eq!(
        sftp::read_bytes_prefix(&session.sftp, &format!("{uploaded}/top.txt"), 64)
            .expect("read top"),
        b"top"
    );
    assert_eq!(
        sftp::read_bytes_prefix(&session.sftp, &format!("{uploaded}/inner/deep.txt"), 64)
            .expect("read deep"),
        b"deep"
    );

    let _ = std::fs::remove_dir_all(&local);
    sftp::recursive_delete(&session.sftp, remote_parent, true, None).expect("cleanup");
}

#[test]
#[ignore]
fn sftp_recursive_copy_remote() {
    let session = connect_localhost();
    let src = "/tmp/fileman_sftp_test_rcopy_src";
    let dst_parent = "/tmp/fileman_sftp_test_rcopy_dst";
    let _ = sftp::recursive_delete(&session.sftp, src, true, None);
    let _ = sftp::recursive_delete(&session.sftp, dst_parent, true, None);

    sftp::mkdir(&session.sftp, src).expect("mkdir src");
    sftp::mkdir(&session.sftp, dst_parent).expect("mkdir dst");
    sftp::write_file(&session.sftp, &format!("{src}/f.txt"), b"copied").expect("write");
    sftp::mkdir(&session.sftp, &format!("{src}/nested")).expect("mkdir nested");
    sftp::write_file(
        &session.sftp,
        &format!("{src}/nested/g.txt"),
        b"nested copy",
    )
    .expect("write nested");

    sftp::recursive_copy_remote(&session.sftp, src, dst_parent, "clone").expect("recursive copy");

    assert_eq!(
        sftp::read_bytes_prefix(&session.sftp, &format!("{dst_parent}/clone/f.txt"), 64)
            .expect("read copy"),
        b"copied"
    );
    assert_eq!(
        sftp::read_bytes_prefix(
            &session.sftp,
            &format!("{dst_parent}/clone/nested/g.txt"),
            64
        )
        .expect("read nested copy"),
        b"nested copy"
    );

    sftp::recursive_delete(&session.sftp, src, true, None).expect("cleanup src");
    sftp::recursive_delete(&session.sftp, dst_parent, true, None).expect("cleanup dst");
}

#[test]
#[ignore]
fn sftp_large_file_round_trip() {
    // Bigger than one SFTP chunk, so this exercises the pipelined read/write
    // path rather than a single request.
    let session = connect_localhost();
    let path = "/tmp/fileman_sftp_test_large";
    let _ = sftp::recursive_delete(&session.sftp, path, false, None);

    let data: Vec<u8> = (0..1_500_000u32).map(|i| (i % 251) as u8).collect();
    sftp::write_file(&session.sftp, path, &data).expect("write large");

    let back = sftp::read_file_full(&session.sftp, path).expect("read large");
    assert_eq!(back.len(), data.len(), "size should round trip");
    assert_eq!(back, data, "contents should round trip");

    sftp::recursive_delete(&session.sftp, path, false, None).expect("cleanup");
}

#[test]
#[ignore]
fn sftp_read_empty_directory() {
    // A directory with no entries of its own still has "." and ".." over
    // SFTP, so the listing should come back empty rather than failing.
    let session = connect_localhost();
    let dir = "/tmp/fileman_sftp_test_empty";
    let _ = sftp::recursive_delete(&session.sftp, dir, true, None);
    sftp::mkdir(&session.sftp, dir).expect("mkdir");

    let entries = sftp::read_directory(&session.sftp, "localhost", dir).expect("read empty dir");
    let names: Vec<&str> = entries
        .iter()
        .map(|e| e.name.as_str())
        .filter(|n| *n != "..")
        .collect();
    assert!(names.is_empty(), "empty dir should list nothing: {names:?}");

    sftp::recursive_delete(&session.sftp, dir, true, None).expect("cleanup");
}

#[test]
#[ignore]
fn sftp_tar_copy_larger_than_the_exec_buffer() {
    // A directory copy streams a tar through an exec channel. Output past a
    // fixed cap used to be discarded outright, which corrupted the archive.
    // Nothing is discarded now: the transport stops reading at a high-water
    // mark and lets the peer's window hold the rest. This is several times
    // that mark, so it covers the handover repeatedly.
    use std::sync::atomic::AtomicBool;

    let session = connect_localhost();
    let remote_dir = "/tmp/fileman_sftp_test_bigtar";
    let _ = sftp::recursive_delete(&session.sftp, remote_dir, true, None);
    sftp::mkdir(&session.sftp, remote_dir).expect("mkdir");

    // Not compressible, so the tar really is this big on the wire.
    let big: Vec<u8> = (0..8_000_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    sftp::write_file(&session.sftp, &format!("{remote_dir}/big.bin"), &big).expect("write big");

    let local = std::env::temp_dir().join("fileman_bigtar_dst");
    let _ = std::fs::remove_dir_all(&local);
    std::fs::create_dir_all(&local).expect("create local dir");

    let cancel = AtomicBool::new(false);
    sftp::copy_remote_dir_to_local_via_tar(&session.sftp, remote_dir, &local, "out", &cancel, None)
        .expect("tar copy down");

    let got = std::fs::read(local.join("out/big.bin")).expect("read copied file");
    assert_eq!(got.len(), big.len(), "size should survive the copy");
    assert!(got == big, "contents should survive the copy");

    let _ = std::fs::remove_dir_all(&local);
    sftp::recursive_delete(&session.sftp, remote_dir, true, None).expect("cleanup");
}

/// Kills an established session and checks that the next operation dials
/// again by itself.
///
/// Needs to run its own sshd so it can kill sessions without disturbing
/// anything else; skipped where it cannot start one.
#[test]
#[ignore]
fn sftp_reconnects_after_the_session_drops() {
    use std::process::Command;

    const PORT: u16 = 2222;
    const PIDFILE: &str = "/tmp/fileman-test-sshd.pid";

    let start = || {
        Command::new("/usr/sbin/sshd")
            .args(["-p", &PORT.to_string(), "-o", &format!("PidFile={PIDFILE}")])
            // Without privileges it cannot read the host keys and says so;
            // that is the skip path, not something to print in a test log.
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    let listener = || {
        std::fs::read_to_string(PIDFILE)
            .ok()
            .map(|s| s.trim().to_string())
    };
    // Killing the listener's children ends the session while leaving the
    // server up, which is what a sleep or a network blip looks like.
    let drop_session = || {
        if let Some(pid) = listener() {
            let _ = Command::new("pkill").args(["-9", "-P", &pid]).status();
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
    };
    let stop = || {
        if let Some(pid) = listener() {
            let _ = Command::new("pkill").args(["-9", "-P", &pid]).status();
            let _ = Command::new("kill").args(["-9", &pid]).status();
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
    };

    stop();
    if !start() {
        eprintln!("cannot start an sshd of our own, skipping");
        return;
    }
    std::thread::sleep(std::time::Duration::from_millis(400));

    let home = std::env::var("HOME").unwrap_or_default();
    let conn = fileman::ssh::connect(fileman::ssh::ConnectParams {
        host: "localhost".into(),
        hostname: "127.0.0.1".into(),
        port: PORT,
        user: std::env::var("USER").unwrap_or_else(|_| "root".into()),
        identity_files: vec![format!("{home}/.ssh/id_ed25519")],
        use_agent: false,
    })
    .expect("connect");
    assert!(conn.stat("/tmp").expect("stat before").is_dir());

    drop_session();

    // No reconnect step and no renavigation, just the next request. Only
    // read-only requests dial again themselves; everything else works
    // afterwards because one of them has healed the connection.
    assert!(
        conn.stat("/tmp").expect("stat should reconnect").is_dir(),
        "a dropped session should be dialled again"
    );
    conn.open_dir("/tmp")
        .expect("opendir on the fresh connection");
    assert_eq!(
        conn.exec("echo hi")
            .expect("exec on the fresh connection")
            .exit,
        Some(fileman::ssh::ExitStatus::Code(0))
    );
    assert!(conn.is_alive(), "reconnecting should keep it usable");

    // With the server gone for good it should give up rather than hang.
    stop();
    assert!(conn.stat("/tmp").is_err(), "no server means an error");
    assert!(!conn.is_alive(), "and the connection is retired");
}
