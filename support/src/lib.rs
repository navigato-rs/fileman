//! User-reviewed feedback. No sockets, HTTP client, timer or background worker.
//!
//! Reports contain typed classifications, never formatted errors or panic payloads.
//! Hosted delivery is intentionally absent until a TLS transport passes policy.

mod stack;
mod storage;
mod ui;

use std::{fmt::Write as _, path, sync, time};

pub use ui::show;

static SUPPORT: sync::OnceLock<sync::Mutex<State>> = sync::OnceLock::new();
static USAGE: sync::atomic::AtomicBool = sync::atomic::AtomicBool::new(false);
static CONSENT: sync::atomic::AtomicU64 = sync::atomic::AtomicU64::new(0);
static DIAGNOSTICS: sync::atomic::AtomicBool = sync::atomic::AtomicBool::new(false);

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum App {
    Fileman,
    Starcom,
}

impl App {
    pub fn slug(self) -> &'static str {
        match self {
            Self::Fileman => "fileman",
            Self::Starcom => "starcom",
        }
    }
}

#[derive(Clone, Copy)]
pub struct Info {
    pub app: App,
    pub version: &'static str,
    pub revision: Option<&'static str>,
    /// A delivery address, not an authentication secret. Visible in shipped binaries.
    pub private_email: Option<&'static str>,
}

impl Info {
    fn environment(self) -> String {
        format!(
            "{} {}\nbuild: {}\nOS: {}\narchitecture: {}",
            self.app.slug(),
            token(self.version),
            token(self.revision.unwrap_or("unknown")),
            std::env::consts::OS,
            std::env::consts::ARCH,
        )
    }

    pub fn issue_url(self) -> String {
        let base = format!(
            "https://github.com/navigato-rs/{}/issues/new",
            self.app.slug()
        );
        let url = format!(
            "{base}?template=feedback.yml&environment={}",
            encode(&self.environment())
        );
        if url.len() <= 1800 {
            url
        } else {
            format!("{base}/choose")
        }
    }

    pub fn email_url(self, body: &str) -> Option<String> {
        let email = self.private_email.filter(|email| valid_email(email))?;
        let url = format!(
            "mailto:{email}?subject={}&body={}",
            encode(&format!("{} private report", self.app.slug())),
            encode(body)
        );
        // Mail handlers have inconsistent length limits. Never truncate a report.
        (url.len() <= 1800).then_some(url)
    }
}

fn token(value: &str) -> &str {
    if !value.is_empty()
        && value.len() <= 80
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
    {
        value
    } else {
        "unknown"
    }
}

fn valid_email(value: &str) -> bool {
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    value.len() <= 254
        && !local.is_empty()
        && local.len() <= 64
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
        && local
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
        && domain.contains('.')
        && domain.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

fn encode(text: &str) -> String {
    let mut encoded = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            encoded.push(char::from(b));
        } else {
            let _ = write!(encoded, "%{b:02X}");
        }
    }
    encoded
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    Panic,
    GpuInitialization,
    SurfaceCreation,
    FrameWait,
    DesktopExit,
}

/// Closed vocabulary: these APIs cannot accept a path, command, host or error text.
#[derive(Clone, Copy)]
#[repr(usize)]
pub enum Feature {
    Remote,
    Archive,
    Editor,
    Search,
    Jump,
    Upload,
}

#[derive(Clone, Copy)]
#[repr(usize)]
pub enum Metric {
    Frame,
    DirectoryLocal,
    DirectoryRemote,
    Connection,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Usage {
    features: [bool; 6],
    /// Millisecond buckets: <=1, <=5, <=16, <=50, <=200, <=1000, >1000.
    histograms: [[u64; 7]; 4],
}

impl Usage {
    fn observe(&mut self, metric: Metric, duration: time::Duration) {
        let bucket =
            [1, 5, 16, 50, 200, 1000].partition_point(|bound| duration.as_millis() > *bound);
        let count = &mut self.histograms[metric as usize][bucket];
        *count = count.saturating_add(1);
    }
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Preferences {
    diagnostics: bool,
    #[serde(default)]
    backtraces: bool,
    usage: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            diagnostics: true,
            backtraces: false,
            usage: false,
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Report {
    schema: u8,
    app: App,
    version: String,
    revision: String,
    os: String,
    arch: String,
    payload: Payload,
    #[serde(default)]
    sentry_consent: bool,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Payload {
    Failure {
        kind: Failure,
        site: Option<Site>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trace: Option<stack::Trace>,
    },
    Usage {
        counters: Usage,
    },
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Site {
    file: String,
    line: u32,
    column: u32,
}

impl Site {
    fn new(location: &std::panic::Location<'_>) -> Option<Self> {
        // Do not record absolute compiler paths (which may name the builder).
        let file = location.file();
        if file.starts_with("src/") && safe_source(file) {
            Some(Self {
                file: file.into(),
                line: location.line(),
                column: location.column(),
            })
        } else {
            None
        }
    }
}

fn safe_source(file: &str) -> bool {
    file.len() <= 160
        && file.starts_with("src/")
        && file.ends_with(".rs")
        && !file.contains("..")
        && file
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/_-.".contains(&b))
}

impl Report {
    fn new(info: Info, payload: Payload) -> Self {
        Self {
            schema: 2,
            app: info.app,
            version: token(info.version).into(),
            revision: token(info.revision.unwrap_or("unknown")).into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            payload,
            sentry_consent: false,
        }
    }

    fn valid(&self, app: App) -> bool {
        matches!(self.schema, 1 | 2)
            && self.app == app
            && token(&self.version) == self.version
            && token(&self.revision) == self.revision
            && matches!(self.os.as_str(), "linux" | "macos" | "windows")
            && matches!(
                self.arch.as_str(),
                "x86" | "x86_64" | "aarch64" | "arm" | "riscv64"
            )
            && match self.payload {
                Payload::Failure {
                    ref site,
                    ref trace,
                    ..
                } => {
                    site.as_ref().is_none_or(|site| safe_source(&site.file))
                        && trace
                            .as_ref()
                            .is_none_or(|trace| self.schema == 2 && trace.valid())
                }
                _ => true,
            }
    }

    fn text(&self) -> String {
        // This is also the exact reviewed content copied or added to a mail draft.
        serde_json::to_string_pretty(self).unwrap_or_default()
    }
}

struct State {
    info: Info,
    storage: Option<storage::Storage>,
    preferences: Preferences,
    usage: Usage,
    reports: Vec<(path::PathBuf, Report)>,
    notice: Option<&'static str>,
    selected: usize,
    sentry_review: Option<path::PathBuf>,
}

impl State {
    fn save_preferences(&mut self, preferences: Preferences) {
        // Serialize with recorders. A finished disable cannot be followed by a late write.
        if self.preferences.usage != preferences.usage {
            CONSENT.fetch_add(1, sync::atomic::Ordering::AcqRel);
        }
        let remove_stacks = self.preferences.backtraces && !preferences.backtraces;
        self.preferences = preferences;
        DIAGNOSTICS.store(preferences.diagnostics, sync::atomic::Ordering::Release);
        USAGE.store(preferences.usage, sync::atomic::Ordering::Release);
        if !preferences.usage {
            self.usage = Usage::default();
        }
        if let Some(ref storage) = self.storage {
            let result = storage.preferences(preferences).and_then(|()| {
                storage.purge_disabled(Preferences {
                    diagnostics: preferences.diagnostics && !remove_stacks,
                    ..preferences
                })
            });
            if result.is_err() {
                self.notice =
                    Some("Could not persist privacy settings or remove every local report.");
            }
            self.reports.retain(|(_, report)| match report.payload {
                Payload::Failure { .. } => preferences.diagnostics && !remove_stacks,
                Payload::Usage { .. } => preferences.usage,
            });
        }
    }

    fn record(&mut self, kind: Failure, site: Option<Site>) {
        if !self.preferences.diagnostics {
            return;
        }
        let trace = self
            .preferences
            .backtraces
            .then(stack::Trace::capture)
            .flatten();
        let report = Report::new(self.info, Payload::Failure { kind, site, trace });
        if let Some(ref storage) = self.storage {
            match storage.save(&report) {
                Ok(path) => {
                    self.reports.insert(0, (path, report));
                    self.reports.truncate(storage::MAX_REPORTS);
                }
                Err(_) => self.notice = Some("Could not save a local diagnostic report."),
            }
        }
    }
}

/// Call once, before opening the desktop. Drop after workers/window teardown.
/// Debug builds, demo and replay should pass `None` (or not initialize support).
/// An unavailable state directory fails closed: no counters or failure recording.
pub fn init(info: Info, directory: Option<path::PathBuf>) -> Session {
    let storage = directory.and_then(|path| storage::Storage::open(path, info.app).ok());
    let preferences = storage
        .as_ref()
        .map(storage::Storage::load_preferences)
        .unwrap_or(Preferences {
            diagnostics: false,
            backtraces: false,
            usage: false,
        });
    let reports = storage
        .as_ref()
        .map(storage::Storage::reports)
        .unwrap_or_default();
    let state = State {
        info,
        storage,
        preferences,
        reports,
        usage: Usage::default(),
        notice: None,
        selected: 0,
        sentry_review: None,
    };
    if SUPPORT.set(sync::Mutex::new(state)).is_ok() {
        DIAGNOSTICS.store(preferences.diagnostics, sync::atomic::Ordering::Release);
        USAGE.store(preferences.usage, sync::atomic::Ordering::Release);
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if DIAGNOSTICS.load(sync::atomic::Ordering::Acquire)
                && let Some(support) = SUPPORT.get()
                && let Ok(mut state) = support.try_lock()
            {
                // Never format `info` or inspect `payload()`. Hooks must not block
                // on a lock held by a panicking thread or attempt network I/O.
                state.record(Failure::Panic, info.location().and_then(Site::new));
            }
            previous(info);
        }));
        Session { owner: true }
    } else {
        Session { owner: false }
    }
}

pub fn state_directory(app: App) -> Option<path::PathBuf> {
    fn absolute(name: &str) -> Option<path::PathBuf> {
        std::env::var_os(name)
            .map(path::PathBuf::from)
            .filter(|p| p.is_absolute())
    }
    let root = if cfg!(windows) {
        absolute("LOCALAPPDATA")
    } else if cfg!(target_os = "macos") {
        absolute("HOME").map(|p| p.join("Library/Application Support"))
    } else {
        absolute("XDG_STATE_HOME").or_else(|| absolute("HOME").map(|p| p.join(".local/state")))
    };
    root.map(|p| p.join("navigato").join(app.slug()).join("support"))
}

pub struct Session {
    owner: bool,
}
impl Drop for Session {
    fn drop(&mut self) {
        if !self.owner || std::thread::panicking() {
            return;
        }
        if let Some(support) = SUPPORT.get()
            && let Ok(state) = support.lock()
            && state.preferences.usage
            && let Some(ref storage) = state.storage
        {
            let _ = storage.save(&Report::new(
                state.info,
                Payload::Usage {
                    counters: state.usage.clone(),
                },
            ));
        }
    }
}

pub fn failure(kind: Failure) {
    if DIAGNOSTICS.load(sync::atomic::Ordering::Acquire)
        && let Some(support) = SUPPORT.get()
        && let Ok(mut state) = support.try_lock()
    {
        state.record(kind, None);
    }
}

pub fn feature(feature: Feature) {
    let generation = CONSENT.load(sync::atomic::Ordering::Acquire);
    if USAGE.load(sync::atomic::Ordering::Acquire)
        && let Some(support) = SUPPORT.get()
        && let Ok(mut state) = support.try_lock()
        && state.preferences.usage
        && generation == CONSENT.load(sync::atomic::Ordering::Acquire)
    {
        state.usage.features[feature as usize] = true;
    }
}

pub fn timer(metric: Metric) -> Option<Timer> {
    let generation = CONSENT.load(sync::atomic::Ordering::Acquire);
    USAGE.load(sync::atomic::Ordering::Acquire).then(|| Timer {
        metric,
        start: time::Instant::now(),
        generation,
    })
}

pub struct Timer {
    metric: Metric,
    start: time::Instant,
    generation: u64,
}
impl Drop for Timer {
    fn drop(&mut self) {
        if self.generation == CONSENT.load(sync::atomic::Ordering::Acquire)
            && USAGE.load(sync::atomic::Ordering::Acquire)
            && let Some(support) = SUPPORT.get()
            && let Ok(mut state) = support.try_lock()
            && state.preferences.usage
            && self.generation == CONSENT.load(sync::atomic::Ordering::Acquire)
        {
            state.usage.observe(self.metric, self.start.elapsed());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const INFO: Info = Info {
        app: App::Fileman,
        version: "0.4.0",
        revision: Some("abc123"),
        private_email: Some("private+bugs@example.test"),
    };

    #[test]
    fn issue_url_is_bounded_and_does_not_request_privileged_labels() {
        let url = INFO.issue_url();
        assert!(url.starts_with(
            "https://github.com/navigato-rs/fileman/issues/new?template=feedback.yml&environment="
        ));
        assert!(!url.contains("labels="));
        assert!(url.len() < 1800);
        assert!(url.contains("%0A"));
        assert!(!url.contains("private"));
    }
    #[test]
    fn encoding_is_byte_based_and_never_introduces_headers() {
        assert_eq!(encode("a &?#+é\r\n"), "a%20%26%3F%23%2B%C3%A9%0D%0A");
    }
    #[test]
    fn recipients_cannot_add_headers_or_other_recipients() {
        for address in [
            "",
            "x@y",
            "a@example.test?bcc=thief",
            "a@example.test\r\nBcc:x",
            "a@example.test,b@example.test",
            "a@@b.test",
            "a@-b.test",
            "a..b@example.test",
        ] {
            assert!(!valid_email(address), "{address:?}");
        }
        assert!(valid_email("private+bugs@example.test"));
    }
    #[test]
    fn oversized_email_is_not_silently_truncated() {
        assert!(
            INFO.email_url("reviewed report")
                .unwrap()
                .contains("subject=fileman%20private%20report")
        );
        assert!(INFO.email_url(&"x".repeat(2000)).is_none());
        assert!(
            Info {
                private_email: None,
                ..INFO
            }
            .email_url("hello")
            .is_none()
        );
    }
    #[test]
    fn metadata_cannot_inject_paths_or_content() {
        let info = Info {
            version: "/home/CANARY/secret",
            revision: Some("host=CANARY\n"),
            ..INFO
        };
        let report = Report::new(
            info,
            Payload::Failure {
                kind: Failure::Panic,
                site: None,
                trace: None,
            },
        );
        assert!(!report.text().contains("CANARY"));
        assert!(report.valid(App::Fileman));
        assert!(!report.valid(App::Starcom));
        assert!(!safe_source("src/../../home/CANARY.rs"));
        assert!(!safe_source("/home/CANARY/src/main.rs"));
    }
    #[test]
    fn usage_has_closed_fields_and_saturating_buckets() {
        let mut usage = Usage::default();
        for ms in [0, 1, 2, 5, 6, 16, 17, 50, 51, 200, 201, 1000, 1001] {
            usage.observe(Metric::Frame, time::Duration::from_millis(ms));
        }
        assert_eq!(usage.histograms[0], [2, 2, 2, 2, 2, 2, 1]);
        usage.histograms[0][0] = u64::MAX;
        usage.observe(Metric::Frame, time::Duration::ZERO);
        assert_eq!(usage.histograms[0][0], u64::MAX);
        let mut value =
            serde_json::to_value(Report::new(INFO, Payload::Usage { counters: usage })).unwrap();
        value["hostname"] = "CANARY".into();
        assert!(serde_json::from_value::<Report>(value).is_err());
    }
    #[test]
    fn panic_and_consent_are_content_free() {
        if let Some(root) = std::env::var_os("NAVIGATO_SUPPORT_TEST_CHILD") {
            let root = path::PathBuf::from(root);
            let guard = init(INFO, Some(root.clone()));
            let _ = std::panic::catch_unwind(|| {
                panic!("CANARY user@privatehost /secret/file clipboard token")
            });
            let mutex = SUPPORT.get().unwrap();
            {
                let mut state = mutex.lock().unwrap();
                assert_eq!(state.reports.len(), 1);
                assert!(!state.reports[0].1.text().contains("CANARY"));
                assert!(matches!(
                    state.reports[0].1.payload,
                    Payload::Failure { trace: None, .. }
                ));
                state.save_preferences(Preferences {
                    diagnostics: true,
                    backtraces: true,
                    usage: false,
                });
                state.record(Failure::Panic, None);
                assert!(matches!(
                    state.reports[0].1.payload,
                    Payload::Failure { trace: Some(_), .. }
                ));
                assert!(!state.reports[0].1.text().contains("CANARY"));
                state.save_preferences(Preferences {
                    diagnostics: true,
                    backtraces: false,
                    usage: false,
                });
                assert!(state.reports.is_empty());
                assert!(state.storage.as_ref().unwrap().reports().is_empty());
                state.record(Failure::Panic, None);
                assert!(matches!(
                    state.reports[0].1.payload,
                    Payload::Failure { trace: None, .. }
                ));
                state.save_preferences(Preferences {
                    diagnostics: false,
                    backtraces: false,
                    usage: true,
                });
            }
            let old_timer = timer(Metric::Frame);
            {
                let mut state = mutex.lock().unwrap();
                state.save_preferences(Preferences {
                    diagnostics: false,
                    backtraces: false,
                    usage: false,
                });
                state.save_preferences(Preferences {
                    diagnostics: false,
                    backtraces: false,
                    usage: true,
                });
            }
            drop(old_timer);
            assert_eq!(mutex.lock().unwrap().usage.histograms, [[0; 7]; 4]);
            feature(Feature::Search);
            drop(timer(Metric::Frame));
            {
                let mut state = mutex.lock().unwrap();
                assert!(state.usage.features[Feature::Search as usize]);
                assert_eq!(state.usage.histograms[0].iter().sum::<u64>(), 1);
                state.save_preferences(Preferences {
                    diagnostics: false,
                    backtraces: false,
                    usage: false,
                });
            }
            assert!(timer(Metric::Frame).is_none());
            let _ = std::panic::catch_unwind(|| panic!("SECOND_CANARY"));
            drop(guard);
            let state = mutex.lock().unwrap();
            assert!(state.storage.as_ref().unwrap().reports().is_empty());
            return;
        }
        let root =
            std::env::temp_dir().join(format!("navigato-support-process-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::panic_and_consent_are_content_free",
                "--nocapture",
            ])
            .env("NAVIGATO_SUPPORT_TEST_CHILD", &root)
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&root);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
