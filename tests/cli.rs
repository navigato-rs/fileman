use std::process;

#[test]
fn updater_is_not_advertised_or_accepted() {
    let help = process::Command::new(env!("CARGO_BIN_EXE_fileman"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(help.status.success());
    assert!(!String::from_utf8_lossy(&help.stderr).contains("--update"));
    let update = process::Command::new(env!("CARGO_BIN_EXE_fileman"))
        .arg("--update")
        .output()
        .unwrap();
    assert!(!update.status.success());
    assert!(String::from_utf8_lossy(&update.stderr).contains("Unknown option: --update"));
}
