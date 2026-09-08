//! Isolated synthetic panic for symbol validation; never reads real app state.
use std::{fs, io::Write as _, path};

#[inline(never)]
fn synthetic_panic() {
    panic!("NAVIGATO_TEST_CANARY: not reportable content");
}

fn main() -> std::io::Result<()> {
    let root = path::PathBuf::from(std::env::args_os().nth(1).expect("new report directory"));
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(&root)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options
        .open(root.join("preferences.json"))?
        .write_all(br#"{"diagnostics":true,"backtraces":true,"usage":false}"#)?;
    let _session = navigato_support::init(
        navigato_support::Info {
            app: navigato_support::App::Fileman,
            version: "0.0.0-sentry-probe",
            revision: option_env!("GITHUB_SHA"),
            private_email: None,
        },
        Some(root),
    );
    synthetic_panic();
    Ok(())
}
