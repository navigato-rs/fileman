use std::{
    collections::{HashMap, HashSet},
    path,
    sync::{Arc, Mutex, mpsc},
    time::Instant,
};

use crate::core::{
    ActivePanel, BrowserMode, ContainerKind, DirBatch, DirEntry, EditLoadRequest, EditLoadResult,
    EntryLocation, IOResult, IOTask, ImageLocation, PreviewContent, PreviewRequest, SearchCase,
    SearchMode, SearchResult, SortMode, container_display_path, container_kind_from_path,
    format_preview_info, is_image_name, is_image_path, is_text_name, is_text_path,
};
use crate::theme::Theme;

/// Determine the syntax-highlighting extension for a path.
/// For files with known compound names (e.g. `CMakeLists.txt`), returns the
/// semantic extension. Falls back to the file extension, or the full filename
/// (lowercased) for extensionless files.
pub fn syntax_ext_for_path(path: &std::path::Path) -> Option<String> {
    let filename = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let lower = filename.to_ascii_lowercase();
    // Compound filenames that need special routing
    match lower.as_str() {
        "cmakelists.txt" => return Some("cmake".to_string()),
        _ => {}
    }
    path.extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .or(Some(lower))
}

pub struct PanelState {
    pub tabs: Vec<BrowserState>,
    pub active_tab: usize,
    pub mode: PanelMode,
}

impl PanelState {
    pub fn browser(&self) -> &BrowserState {
        &self.tabs[self.active_tab]
    }

    pub fn browser_mut(&mut self) -> &mut BrowserState {
        &mut self.tabs[self.active_tab]
    }

    pub fn new_tab(&mut self) {
        let current = &self.tabs[self.active_tab];
        let new_browser = BrowserState {
            browser_mode: current.browser_mode.clone(),
            current_path: current.current_path.clone(),
            selected_index: 0,
            entries: Vec::new(),
            load: LoadState::Idle,
            progress_override: None,
            prefer_select_name: None,
            top_index: 0,
            container_root: current.container_root.clone(),
            dir_token: 0,
            history_back: Vec::new(),
            history_forward: Vec::new(),
            inline_rename: None,
            sort_mode: current.sort_mode,
            sort_desc: current.sort_desc,
            watching_archive: None,
            index_last_seen: 0,
            marked: std::collections::HashSet::new(),
            parent_cache: Vec::new(),
        };
        let new_idx = self.active_tab + 1;
        self.tabs.insert(new_idx, new_browser);
        self.active_tab = new_idx;
    }

    pub fn close_tab(&mut self) {
        if self.tabs.len() <= 1 {
            return;
        }
        self.tabs.remove(self.active_tab);
        if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
        }
    }

    pub fn next_tab(&mut self) {
        if self.tabs.len() > 1 {
            self.active_tab = (self.active_tab + 1) % self.tabs.len();
        }
    }

    pub fn prev_tab(&mut self) {
        if self.tabs.len() > 1 {
            self.active_tab = (self.active_tab + self.tabs.len() - 1) % self.tabs.len();
        }
    }
}

pub struct FileProps {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub file_type: String,
    pub is_dir: bool,
    pub user_label: String,
    pub group_label: String,
    pub size: Option<u64>,
    pub modified: Option<u64>,
}

pub struct FilePropsEdit {
    pub mode: u32,
    pub user: String,
    pub group: String,
}

pub struct PropsDialog {
    pub target: path::PathBuf,
    pub original: FileProps,
    pub current: FilePropsEdit,
    pub error: Option<String>,
}

pub enum PanelMode {
    Browser,
    Preview(PreviewState),
    Edit(EditState),
    Help(HelpState),
}

pub struct BrowserState {
    pub browser_mode: BrowserMode,
    pub current_path: path::PathBuf, // For Fs mode: real fs path. For Container: archive path.
    pub selected_index: usize,
    pub entries: Vec<DirEntry>,
    pub load: LoadState,
    /// Header progress overlay independent of `load` — used for activity
    /// surfaced in the panel header that isn't directory loading (e.g. live
    /// search progress). Cleared when the activity finishes.
    pub progress_override: Option<(usize, Option<usize>)>,
    pub prefer_select_name: Option<String>,
    pub top_index: usize,
    pub container_root: Option<String>,
    pub dir_token: u64,
    pub history_back: Vec<PanelSnapshot>,
    pub history_forward: Vec<PanelSnapshot>,
    pub inline_rename: Option<InlineRename>,
    pub sort_mode: SortMode,
    pub sort_desc: bool,
    pub watching_archive: Option<path::PathBuf>,
    pub index_last_seen: usize,
    pub marked: std::collections::HashSet<String>,
    /// Stack of cached parent directory listings. Pushed when descending into
    /// a child directory, popped when ascending back. Each entry may still
    /// carry an active `entries_rx` so its async loading continues.
    pub parent_cache: Vec<DirListingCache>,
}

/// Async-load state for a directory listing.
///
/// Replaces the previous trio of `loading: bool`, `loading_progress: Option<...>`,
/// `entries_rx: Option<Receiver>` with a single value whose invariants are
/// structural — a receiver only exists while the load is in-flight, progress
/// only meaningful while loading, and a failed terminal state is explicit.
#[derive(Default)]
pub enum LoadState {
    #[default]
    Idle,
    Loading {
        rx: mpsc::Receiver<DirBatch>,
        progress: Option<(usize, Option<usize>)>,
        /// The `dir_token` of the BrowserState at the moment this load was
        /// started. If `BrowserState.dir_token` advances while this load is
        /// still in-flight, the load is stale and its batches must be
        /// discarded before they corrupt the new directory.
        token: u64,
    },
    Failed(String),
}

impl LoadState {
    pub fn is_loading(&self) -> bool {
        matches!(self, LoadState::Loading { .. })
    }

    pub fn progress(&self) -> Option<(usize, Option<usize>)> {
        match *self {
            LoadState::Loading { progress, .. } => progress,
            _ => None,
        }
    }

    pub fn token(&self) -> Option<u64> {
        match *self {
            LoadState::Loading { token, .. } => Some(token),
            _ => None,
        }
    }

    /// Stamp this load with a new generation token. Used when a cached load
    /// is re-attached to a fresh navigation so subsequent batches are not
    /// discarded as stale.
    pub fn retag(&mut self, new_token: u64) {
        if let LoadState::Loading { ref mut token, .. } = *self {
            *token = new_token;
        }
    }

    pub fn set_progress(&mut self, loaded: usize, total: Option<usize>) {
        if let LoadState::Loading {
            ref mut progress, ..
        } = *self
        {
            *progress = Some((loaded, total));
        }
    }

    /// Begin a fresh load using the given receiver, tagged with the
    /// originating dir_token. Clears any prior progress.
    pub fn start(rx: mpsc::Receiver<DirBatch>, token: u64) -> Self {
        LoadState::Loading {
            rx,
            progress: None,
            token,
        }
    }

    pub fn finish(&mut self) {
        *self = LoadState::Idle;
    }

    /// Drain up to `max` batches from the in-flight receiver. Returns the
    /// drained batches and `true` if the channel disconnected (load complete).
    /// State remains Loading throughout — caller is responsible for calling
    /// `finish()` on termination and applying any batch side-effects.
    pub fn drain_batches(&mut self, max: usize) -> (Vec<DirBatch>, bool) {
        let mut batches = Vec::new();
        let mut terminated = false;
        if let LoadState::Loading { ref mut rx, .. } = *self {
            for _ in 0..max {
                match rx.try_recv() {
                    Ok(b) => batches.push(b),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        terminated = true;
                        break;
                    }
                }
            }
        }
        (batches, terminated)
    }
}

/// Cap on the in-memory error log surfaced in the Help screen.
pub const ERROR_LOG_CAP: usize = 100;

pub struct ErrorLogEntry {
    pub when: Instant,
    pub source: String,
    pub message: String,
}

pub enum InlineEditKind {
    Rename,
    NewFile,
    NewDir,
}

pub struct InlineRename {
    pub index: usize,
    pub text: String,
    pub kind: InlineEditKind,
    pub focus: bool,
}

pub struct QuickJumpEntry {
    pub label: String,
    pub path: path::PathBuf,
    pub category: QuickJumpCategory,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum QuickJumpCategory {
    /// Remote root or home (only shown when on a remote host).
    Remote,
    Home,
    Mount,
    Ssh,
}

pub struct QuickJumpState {
    pub input: String,
    pub entries: Vec<QuickJumpEntry>,
    pub filtered: Vec<usize>,
    pub selected: usize,
    pub focus_input: bool,
}

pub struct ArchiveFullIndex {
    pub entries: Vec<(String, bool, Option<u64>)>,
    pub root: Option<String>,
    pub complete: bool,
    /// Set when the loader thread hit an I/O error (permission denied,
    /// corrupt archive, …). Callers should evict the cached index and
    /// re-attempt indexing rather than reuse the empty entries.
    pub failed: bool,
}

pub struct ContainerDirCache {
    pub entries: Vec<DirEntry>,
    pub load: LoadState,
    pub selected_index: usize,
    pub top_index: usize,
    pub root: Option<String>,
}

/// Cached parent directory listing. Kept on a per-panel stack so that
/// async loading of ancestor directories continues while the user browses
/// deeper into the tree.
pub struct DirListingCache {
    pub current_path: path::PathBuf,
    pub entries: Vec<DirEntry>,
    pub selected_index: usize,
    pub top_index: usize,
    pub load: LoadState,
    pub sort_mode: SortMode,
    pub sort_desc: bool,
    /// Modification time of the directory when the listing was captured.
    /// Used to detect stale caches when navigating back.
    pub dir_mtime: Option<std::time::SystemTime>,
}

pub struct PreviewState {
    pub content: Option<PreviewContent>,
    pub key: Option<String>,
    pub ext: Option<String>,
    pub scroll: f32,
    pub line_height: f32,
    pub page_height: f32,
    pub max_scroll: f32,
    pub can_scroll: bool,
    pub find_open: bool,
    pub find_query: String,
    pub find_query_built: String,
    pub find_matches: Vec<usize>,
    pub find_match_num: usize,
    pub find_focus: bool,
    pub request_id: u64,
    pub wrap: bool,
    pub bytes_per_row: usize,
    pub bytes_per_row_auto: bool,
    pub loading_since: Option<Instant>,
    /// Image zoom: 0.0 = fit-to-panel, >0 = percentage (1.0 = 100%).
    pub image_zoom: f32,
    /// Image pan offset (x, y) in pixels, used when zoomed image exceeds panel.
    pub image_pan: [f32; 2],
}

/// State of the editor's background file load. The three cases are mutually
/// exclusive — there is no "loading and failed" — so they live in one field
/// rather than two booleans.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EditLoad {
    /// The read is in flight; the buffer is empty and must not be saved.
    Loading,
    /// The file loaded successfully; the buffer holds its contents.
    Loaded,
    /// The load failed or the file was refused (binary/unreadable); the buffer
    /// holds an error message, so saving is blocked to avoid clobbering it.
    Failed,
}

pub struct EditState {
    pub path: Option<path::PathBuf>,
    pub text: String,
    pub ext: Option<String>,
    pub load: EditLoad,
    pub dirty: bool,
    pub confirm_discard: bool,
    pub return_focus: ActivePanel,
    pub highlight_key: Option<String>,
    pub highlight_hash: u64,
    pub highlight_wrap_width: f32,
    pub highlight_dirty_at: Option<Instant>,
    pub request_id: u64,
    pub wrap: bool,
    /// Whether the original file used CRLF line endings (restore on save).
    pub crlf: bool,
    /// Pending forced selection range (char indices), consumed on next frame.
    pub force_select: Option<(usize, usize)>,
}

pub struct HelpState {
    pub return_focus: ActivePanel,
}
#[derive(Clone)]
pub struct PanelSnapshot {
    pub mode: BrowserMode,
    pub current_path: path::PathBuf,
    pub selected_name: Option<String>,
}

#[cfg(target_os = "linux")]
fn unescape_proc_mount(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let (a, b, c) = (bytes[i + 1], bytes[i + 2], bytes[i + 3]);
            if a.is_ascii_digit() && b.is_ascii_digit() && c.is_ascii_digit() {
                let v = (a - b'0') * 64 + (b - b'0') * 8 + (c - b'0');
                out.push(v as char);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn history_key(snapshot: &PanelSnapshot) -> String {
    match snapshot.mode {
        BrowserMode::Fs => format!("fs:{}", snapshot.current_path.to_string_lossy()),
        BrowserMode::Container {
            kind,
            ref archive_path,
            ref cwd,
            ref root,
        } => format!(
            "container:{}:{}:{}:{}",
            match kind {
                ContainerKind::Zip => "zip",
                ContainerKind::Tar => "tar",
                ContainerKind::TarGz => "tar.gz",
                ContainerKind::TarBz2 => "tar.bz2",
            },
            archive_path.to_string_lossy(),
            cwd,
            root.as_deref().unwrap_or_default()
        ),
        BrowserMode::Remote { ref host, ref path } => format!("remote:{host}:{path}"),
        BrowserMode::Search {
            ref root,
            ref query,
            mode,
            case,
        } => format!(
            "search:{}:{}:{}:{}",
            root.to_string_lossy(),
            query,
            match mode {
                SearchMode::Name => "name",
                SearchMode::Content => "content",
            },
            match case {
                SearchCase::Sensitive => "s",
                SearchCase::Insensitive => "i",
            }
        ),
    }
}

/// The single overlay dialog that can be showing at any moment. Holding the
/// modal state in one field (rather than a scatter of `Option`/`bool` fields)
/// makes it impossible by construction to have two dialogs open at once, and
/// gives one place to check with `any_modal_open`.
///
/// The busy/progress overlay is intentionally *not* here — it is driven by the
/// `io_in_flight` counter (it needs the count for "file X of N"), and the
/// editor's discard prompt lives in `EditState` since it is scoped to a panel.
pub enum Modal {
    /// Copy/Move/Delete/Rename/Pack confirmation.
    Confirm(PendingOp),
    /// File properties dialog.
    Props(PropsDialog),
    /// Settings editor (working copy applied on save, dropped on cancel).
    Settings(crate::settings::Settings),
    /// Quick-jump path/bookmark picker.
    QuickJump(QuickJumpState),
    /// Permission error offering an elevated retry of the failed task.
    Elevation {
        message: String,
        task: crate::core::IOTask,
    },
    /// A fatal/operation error message.
    Error(String),
    /// Shown while an SFTP connection is being established (holds the host).
    Connecting(String),
    /// External-theme picker.
    ThemePicker,
}

pub struct AppState {
    pub left_panel: PanelState,
    pub right_panel: PanelState,
    pub active_panel: ActivePanel,
    pub preview_return_focus: Option<ActivePanel>,
    pub allow_external_open: bool,
    pub wake: Option<Arc<dyn Fn() + Send + Sync>>,
    pub preview_tx: mpsc::Sender<PreviewRequest>,
    pub preview_rx: mpsc::Receiver<(u64, PreviewContent)>,
    pub preview_request_id: u64,
    pub io_tx: mpsc::Sender<IOTask>,
    pub io_rx: mpsc::Receiver<IOResult>,
    pub io_cancel_tx: mpsc::Sender<()>,
    pub io_cancel_flag: Arc<std::sync::atomic::AtomicBool>,
    pub io_in_flight: usize,
    /// Total tasks enqueued for the current batch — peak of io_in_flight
    /// since the queue last emptied. Used by the progress modal to show
    /// "file X of N". Reset to 0 when io_in_flight reaches 0.
    pub io_batch_total: usize,
    pub io_cancel_requested: bool,
    /// Human verb for the in-flight batch ("Copying", "Moving", …), shown as
    /// the progress modal's title.
    pub io_verb: &'static str,
    /// Shared transfer progress for IO/preview/edit workers.
    pub transfer_progress: Arc<crate::core::TransferProgress>,
    pub dir_size_tx: mpsc::Sender<path::PathBuf>,
    pub dir_size_rx: mpsc::Receiver<(path::PathBuf, u64)>,
    pub dir_sizes: HashMap<path::PathBuf, u64>,
    pub dir_size_pending: HashSet<path::PathBuf>,
    pub remote_dir_size_tx: mpsc::Sender<(String, String)>,
    pub remote_dir_size_rx: mpsc::Receiver<(String, String, u64)>,
    pub remote_dir_sizes: HashMap<(String, String), u64>,
    pub remote_dir_size_pending: HashSet<(String, String)>,
    pub fs_last_selected_name: HashMap<path::PathBuf, String>,
    pub container_last_selected_name: HashMap<(path::PathBuf, String, ContainerKind), String>,
    pub container_dir_cache: HashMap<(path::PathBuf, String, ContainerKind), ContainerDirCache>,
    pub archive_index: HashMap<path::PathBuf, Arc<Mutex<ArchiveFullIndex>>>,
    /// The single overlay dialog currently displayed, if any. See `Modal`.
    pub modal: Option<Modal>,
    pub theme: Theme,
    pub theme_picker_selected: Option<usize>,
    /// Destination names that a pending Copy/Move would overwrite, computed once
    /// when the op is prepared and shown in the confirmation dialog so the user
    /// consents to clobbering existing files.
    pub pending_collisions: Vec<String>,
    pub rename_input: Option<String>,
    pub rename_focus: bool,
    pub edit_tx: mpsc::Sender<EditLoadRequest>,
    pub edit_rx: mpsc::Receiver<EditLoadResult>,
    pub edit_request_id: u64,
    pub search_query: String,
    pub search_focus: bool,
    pub search_case: SearchCase,
    pub search_mode: SearchMode,
    pub search_results: Vec<SearchResult>,
    pub search_selected: usize,
    pub search_request_id: u64,
    /// The (panel, tab) that started the current search. Search events are
    /// routed here rather than to whatever panel/tab happens to be active when
    /// they arrive, so switching panels or tabs mid-search doesn't inject
    /// results into an unrelated directory listing.
    pub search_target: Option<(ActivePanel, usize)>,
    pub search_status: SearchStatus,
    pub search_ui: SearchUiState,
    pub search_tx: mpsc::Sender<crate::core::SearchRequest>,
    pub search_rx: mpsc::Receiver<crate::core::SearchEvent>,
    /// Host of the currently-running remote search, if any.
    pub search_remote_host: Option<String>,
    pub refresh_tick: u64,
    pub update_status: UpdateStatus,
    pub update_rx: Option<mpsc::Receiver<UpdateStatus>>,
    pub gpu_info: String,
    /// Persistent error log surfaced in the Help screen. New entries are
    /// pushed via `record_error`; capped at `ERROR_LOG_CAP` entries.
    pub error_log: Vec<ErrorLogEntry>,
    /// User-editable settings persisted to RON at config_dir().
    pub settings: crate::settings::Settings,
    /// Active SFTP sessions keyed by hostname — local reference for quick lookups.
    pub sftp_sessions: HashMap<String, Arc<Mutex<crate::sftp::SftpSession>>>,
    /// Shared SFTP sessions for worker threads (IO, preview).
    pub sftp_sessions_shared: Arc<Mutex<HashMap<String, Arc<Mutex<crate::sftp::SftpSession>>>>>,
    /// Receives the result of an async SFTP connection.
    pub sftp_connect_rx: Option<mpsc::Receiver<Result<crate::sftp::SftpSession, String>>>,
    /// Pending navigation after SFTP connect completes.
    pub sftp_pending_nav: Option<(String, String, crate::core::ActivePanel)>, // (host, path, panel)
    /// Queued navigations waiting for prior SFTP connection to finish.
    pub sftp_nav_queue: std::collections::VecDeque<(String, String, crate::core::ActivePanel)>,
}

#[derive(Clone)]
pub struct CopyItem {
    pub src: EntryLocation,
    pub kind: CopyKind,
}

#[derive(Clone)]
pub enum CopyDest {
    Local(path::PathBuf),
    Remote { host: String, path: String },
}

#[derive(Clone)]
pub enum PendingOp {
    Copy {
        items: Vec<CopyItem>,
        dst: CopyDest,
    },
    Move {
        items: Vec<CopyItem>,
        dst: CopyDest,
    },
    Delete {
        targets: Vec<path::PathBuf>,
    },
    Rename {
        src: path::PathBuf,
    },
    Pack {
        sources: Vec<path::PathBuf>,
        dst_dir: path::PathBuf,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CopyKind {
    File,
    Directory,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SearchUiState {
    Closed,
    Open,
}

#[derive(Clone, Copy)]
pub enum SearchStatus {
    Idle,
    Running(crate::core::SearchProgress),
    Done(crate::core::SearchProgress),
}

#[derive(Clone)]
pub enum UpdateStatus {
    /// Feature not compiled in, or not checking
    Disabled,
    /// Background check in progress
    Checking,
    /// Already on latest version
    UpToDate,
    /// A newer version is available
    Available(String),
    /// Download + install in progress
    Installing(String),
    /// Successfully installed, restart needed
    Installed(String),
    /// Check or install failed
    Failed(String),
}

pub struct AsyncStatus {
    pub io_in_flight: usize,
    pub io_cancel_requested: bool,
    pub dir_size_pending: usize,
    pub search: SearchStatus,
    pub update: UpdateStatus,
    pub gpu_info: String,
}

impl AppState {
    /// Append an entry to the persistent error log and also surface it as the
    /// active toast/modal message. Caps the log at ERROR_LOG_CAP entries
    /// (oldest evicted from the front).
    pub fn record_error(&mut self, source: impl Into<String>, message: impl Into<String>) {
        let source = source.into();
        let message = message.into();
        self.modal = Some(Modal::Error(message.clone()));
        self.error_log.push(ErrorLogEntry {
            when: Instant::now(),
            source,
            message,
        });
        if self.error_log.len() > ERROR_LOG_CAP {
            let overflow = self.error_log.len() - ERROR_LOG_CAP;
            self.error_log.drain(..overflow);
        }
    }

    pub fn poll_update_status(&mut self) {
        if let Some(ref rx) = self.update_rx
            && let Ok(status) = rx.try_recv()
        {
            self.update_status = status;
            self.update_rx = None;
        }
    }

    pub fn async_status(&self) -> AsyncStatus {
        AsyncStatus {
            io_in_flight: self.io_in_flight,
            io_cancel_requested: self.io_cancel_requested,
            dir_size_pending: self.dir_size_pending.len(),
            search: self.search_status,
            update: self.update_status.clone(),
            gpu_info: self.gpu_info.clone(),
        }
    }

    pub fn panel(&self, which: ActivePanel) -> &PanelState {
        match which {
            ActivePanel::Left => &self.left_panel,
            ActivePanel::Right => &self.right_panel,
        }
    }

    pub fn panel_mut(&mut self, which: ActivePanel) -> &mut PanelState {
        match which {
            ActivePanel::Left => &mut self.left_panel,
            ActivePanel::Right => &mut self.right_panel,
        }
    }

    pub fn get_active_panel(&self) -> &PanelState {
        self.panel(self.active_panel)
    }

    pub fn get_active_panel_mut(&mut self) -> &mut PanelState {
        self.panel_mut(self.active_panel)
    }

    /// Whether an overlay modal is currently displayed. The browser panels
    /// behind it must not respond to mouse input while one is up, otherwise
    /// clicking "through" a dialog changes selection or the active panel. The
    /// busy overlay (io_in_flight) counts too, though it isn't a `Modal`.
    pub fn any_modal_open(&self) -> bool {
        self.modal.is_some() || self.io_in_flight > 0
    }

    // --- Modal accessors -----------------------------------------------------
    // Typed views over the single `modal` field, so callers read the specific
    // dialog they care about without matching the enum everywhere.

    pub fn pending_op(&self) -> Option<&PendingOp> {
        match self.modal {
            Some(Modal::Confirm(ref op)) => Some(op),
            _ => None,
        }
    }

    pub fn props_dialog(&self) -> Option<&PropsDialog> {
        match self.modal {
            Some(Modal::Props(ref d)) => Some(d),
            _ => None,
        }
    }

    pub fn props_dialog_mut(&mut self) -> Option<&mut PropsDialog> {
        match self.modal {
            Some(Modal::Props(ref mut d)) => Some(d),
            _ => None,
        }
    }

    pub fn settings_draft_mut(&mut self) -> Option<&mut crate::settings::Settings> {
        match self.modal {
            Some(Modal::Settings(ref mut s)) => Some(s),
            _ => None,
        }
    }

    pub fn settings_open(&self) -> bool {
        matches!(self.modal, Some(Modal::Settings(_)))
    }

    pub fn quick_jump(&self) -> Option<&QuickJumpState> {
        match self.modal {
            Some(Modal::QuickJump(ref q)) => Some(q),
            _ => None,
        }
    }

    pub fn quick_jump_mut(&mut self) -> Option<&mut QuickJumpState> {
        match self.modal {
            Some(Modal::QuickJump(ref mut q)) => Some(q),
            _ => None,
        }
    }

    pub fn error_message(&self) -> Option<&str> {
        match self.modal {
            Some(Modal::Error(ref m)) => Some(m),
            _ => None,
        }
    }

    pub fn sftp_connecting(&self) -> Option<&str> {
        match self.modal {
            Some(Modal::Connecting(ref h)) => Some(h),
            _ => None,
        }
    }

    pub fn elevation_message(&self) -> Option<&str> {
        match self.modal {
            Some(Modal::Elevation { ref message, .. }) => Some(message),
            _ => None,
        }
    }

    pub fn theme_picker_open(&self) -> bool {
        matches!(self.modal, Some(Modal::ThemePicker))
    }

    /// Open a modal, replacing any that is currently showing. Single-modal by
    /// construction: there is nowhere to put a second one.
    pub fn open_modal(&mut self, modal: Modal) {
        self.modal = Some(modal);
    }

    /// Close whatever modal is open (if any).
    pub fn close_modal(&mut self) {
        self.modal = None;
    }

    /// Take the pending elevation prompt, if that is the open modal.
    pub fn take_elevation(&mut self) -> Option<(String, crate::core::IOTask)> {
        if matches!(self.modal, Some(Modal::Elevation { .. })) {
            match self.modal.take() {
                Some(Modal::Elevation { message, task }) => Some((message, task)),
                _ => None,
            }
        } else {
            None
        }
    }

    /// Take the "connecting…" host and close that modal, if it is the open one.
    pub fn take_connecting(&mut self) -> Option<String> {
        if matches!(self.modal, Some(Modal::Connecting(_))) {
            match self.modal.take() {
                Some(Modal::Connecting(host)) => Some(host),
                _ => None,
            }
        } else {
            None
        }
    }

    /// The browser tab that owns the currently-running search, if it still
    /// exists. Search events must be applied here rather than to whatever panel
    /// is active when the event arrives. Returns None if no search is running
    /// or the originating tab has since been closed.
    pub fn search_target_browser_mut(&mut self) -> Option<&mut BrowserState> {
        let (which, tab) = self.search_target?;
        self.panel_mut(which).tabs.get_mut(tab)
    }

    pub fn preview_panel_side(&self) -> Option<ActivePanel> {
        let PanelState {
            mode: ref left_mode,
            ..
        } = self.left_panel;
        if let PanelMode::Preview(_) = *left_mode {
            return Some(ActivePanel::Left);
        }
        let PanelState {
            mode: ref right_mode,
            ..
        } = self.right_panel;
        if let PanelMode::Preview(_) = *right_mode {
            return Some(ActivePanel::Right);
        }
        None
    }

    pub fn preview_panel_mut(&mut self) -> Option<&mut PreviewState> {
        let side = self.preview_panel_side()?;
        let panel = self.panel_mut(side);
        match panel.mode {
            PanelMode::Preview(ref mut preview) => Some(preview),
            _ => None,
        }
    }

    pub fn preview_panel(&self) -> Option<&PreviewState> {
        let side = self.preview_panel_side()?;
        let panel = self.panel(side);
        match panel.mode {
            PanelMode::Preview(ref preview) => Some(preview),
            _ => None,
        }
    }

    pub fn edit_panel_side(&self) -> Option<ActivePanel> {
        let PanelState {
            mode: ref left_mode,
            ..
        } = self.left_panel;
        if let PanelMode::Edit(_) = *left_mode {
            return Some(ActivePanel::Left);
        }
        let PanelState {
            mode: ref right_mode,
            ..
        } = self.right_panel;
        if let PanelMode::Edit(_) = *right_mode {
            return Some(ActivePanel::Right);
        }
        None
    }

    pub fn help_panel_side(&self) -> Option<ActivePanel> {
        let PanelState {
            mode: ref left_mode,
            ..
        } = self.left_panel;
        if let PanelMode::Help(_) = *left_mode {
            return Some(ActivePanel::Left);
        }
        let PanelState {
            mode: ref right_mode,
            ..
        } = self.right_panel;
        if let PanelMode::Help(_) = *right_mode {
            return Some(ActivePanel::Right);
        }
        None
    }

    pub fn help_panel(&self, which: ActivePanel) -> Option<&HelpState> {
        let panel = self.panel(which);
        match panel.mode {
            PanelMode::Help(ref help) => Some(help),
            _ => None,
        }
    }

    pub fn edit_panel(&self) -> Option<&EditState> {
        let side = self.edit_panel_side()?;
        let panel = self.panel(side);
        match panel.mode {
            PanelMode::Edit(ref edit) => Some(edit),
            _ => None,
        }
    }

    pub fn edit_panel_mut(&mut self) -> Option<&mut EditState> {
        let side = self.edit_panel_side()?;
        let panel = self.panel_mut(side);
        match panel.mode {
            PanelMode::Edit(ref mut edit) => Some(edit),
            _ => None,
        }
    }

    pub fn select_entry(&mut self, index: usize, window_rows: usize) {
        let panel = self.get_active_panel_mut();
        let browser = panel.browser_mut();
        if index < browser.entries.len() {
            browser.selected_index = index;
            if browser.selected_index < browser.top_index {
                browser.top_index = browser.selected_index;
            } else if browser.selected_index >= browser.top_index + window_rows {
                browser.top_index = browser.selected_index + 1 - window_rows;
            }
            if self.preview_panel_side().is_some() {
                self.update_preview_for_current_selection();
            }
        } else {
            log::error!("Unable to select entry at index {}", index);
        }
    }

    pub fn switch_panel(&mut self) {
        self.active_panel = match self.active_panel {
            ActivePanel::Left => ActivePanel::Right,
            ActivePanel::Right => ActivePanel::Left,
        };
    }

    pub fn swap_panels(&mut self) {
        std::mem::swap(&mut self.left_panel, &mut self.right_panel);
    }

    pub fn store_current_selection_memory(&mut self) {
        self.store_selection_memory_for(self.active_panel);
    }

    pub fn store_selection_memory_for(&mut self, which: ActivePanel) {
        let (fs_key, container_key, selected_name_opt) = {
            let panel = self.panel(which);
            let browser = panel.browser();
            if browser.entries.is_empty() {
                return;
            }
            let selected_name = browser.entries[browser.selected_index].name.clone();
            match browser.browser_mode {
                BrowserMode::Fs => (
                    Some(browser.current_path.clone()),
                    None,
                    Some(selected_name),
                ),
                BrowserMode::Container {
                    ref archive_path,
                    ref cwd,
                    kind,
                    root: _,
                } => (
                    None,
                    Some((archive_path.clone(), cwd.clone(), kind)),
                    Some(selected_name),
                ),
                BrowserMode::Search { .. } | BrowserMode::Remote { .. } => (None, None, None),
            }
        };
        if let Some(selected_name) = selected_name_opt {
            if let Some(path) = fs_key {
                self.fs_last_selected_name.insert(path, selected_name);
            } else if let Some((ap, cwd, kind)) = container_key {
                self.container_last_selected_name
                    .insert((ap, cwd, kind), selected_name);
            }
        }
    }

    /// Save the current panel's directory listing into a cache so it can be
    /// restored instantly when the user navigates back.
    pub fn stash_container_cache(&mut self, which: ActivePanel) {
        let (key, cache) = {
            let panel = self.panel_mut(which);
            let browser = panel.browser_mut();
            let BrowserMode::Container {
                ref archive_path,
                ref cwd,
                kind,
                root: _,
            } = browser.browser_mode
            else {
                return;
            };
            let key = (archive_path.clone(), cwd.clone(), kind);
            let cache = ContainerDirCache {
                entries: browser.entries.clone(),
                load: std::mem::take(&mut browser.load),
                selected_index: browser.selected_index,
                top_index: browser.top_index,
                root: browser.container_root.clone(),
            };
            (key, cache)
        };
        self.container_dir_cache.insert(key, cache);
    }

    pub fn select_entry_by_name(&mut self, which: ActivePanel, name: &str) {
        let panel = self.panel_mut(which);
        let browser = panel.browser_mut();
        if let Some(idx) = browser.entries.iter().position(|e| e.name == name) {
            browser.selected_index = idx;
        }
    }

    pub fn push_history(&mut self, which: ActivePanel) {
        let snapshot = {
            let panel = self.panel(which);
            let browser = panel.browser();
            let selected = browser.entries.get(browser.selected_index).map(|e| {
                if matches!(browser.browser_mode, BrowserMode::Search { .. })
                    && let EntryLocation::Fs(path) = e.location.clone()
                {
                    return format!("fs:{}", path.to_string_lossy());
                }
                e.name.clone()
            });
            PanelSnapshot {
                mode: browser.browser_mode.clone(),
                current_path: browser.current_path.clone(),
                selected_name: selected,
            }
        };
        let panel = self.panel_mut(which);
        let browser = panel.browser_mut();
        if let Some(last) = browser.history_back.last()
            && history_key(last) == history_key(&snapshot)
        {
            return;
        }
        browser.history_back.push(snapshot);
        browser.history_forward.clear();
    }

    pub fn pop_history_back(&mut self, which: ActivePanel) -> Option<PanelSnapshot> {
        let current = {
            let panel = self.panel(which);
            let browser = panel.browser();
            let selected = browser.entries.get(browser.selected_index).map(|e| {
                if matches!(browser.browser_mode, BrowserMode::Search { .. })
                    && let EntryLocation::Fs(path) = e.location.clone()
                {
                    return format!("fs:{}", path.to_string_lossy());
                }
                e.name.clone()
            });
            PanelSnapshot {
                mode: browser.browser_mode.clone(),
                current_path: browser.current_path.clone(),
                selected_name: selected,
            }
        };
        let panel = self.panel_mut(which);
        let browser = panel.browser_mut();
        let prev = browser.history_back.pop();
        if prev.is_some() {
            browser.history_forward.push(current);
        }
        prev
    }

    pub fn pop_history_forward(&mut self, which: ActivePanel) -> Option<PanelSnapshot> {
        let current = {
            let panel = self.panel(which);
            let browser = panel.browser();
            let selected = browser.entries.get(browser.selected_index).map(|e| {
                if matches!(browser.browser_mode, BrowserMode::Search { .. })
                    && let EntryLocation::Fs(path) = e.location.clone()
                {
                    return format!("fs:{}", path.to_string_lossy());
                }
                e.name.clone()
            });
            PanelSnapshot {
                mode: browser.browser_mode.clone(),
                current_path: browser.current_path.clone(),
                selected_name: selected,
            }
        };
        let panel = self.panel_mut(which);
        let browser = panel.browser_mut();
        let next = browser.history_forward.pop();
        if next.is_some() {
            browser.history_back.push(current);
        }
        next
    }

    pub fn prepare_rename_selected(&mut self) {
        let name = {
            let panel = self.get_active_panel();
            let browser = panel.browser();
            if browser.entries.is_empty() {
                return;
            }
            let entry = &browser.entries[browser.selected_index];
            if entry.name == ".." {
                return;
            }
            match entry.location {
                EntryLocation::Fs(ref path) => path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_string()),
                EntryLocation::Remote { ref path, .. } => {
                    path.rsplit('/').next().map(|s| s.to_string())
                }
                _ => None,
            }
        };
        if let Some(name) = name {
            let panel = self.get_active_panel_mut();
            let browser = panel.browser_mut();
            browser.inline_rename = Some(InlineRename {
                index: browser.selected_index,
                text: name,
                kind: InlineEditKind::Rename,
                focus: true,
            });
            self.rename_input = None;
            self.modal = None;
            self.rename_focus = false;
        }
    }

    pub fn start_inline_new_file(&mut self) {
        enum InsertMode {
            Fs(path::PathBuf),
            Remote { host: String, path: String },
        }

        let mode = {
            let panel = self.get_active_panel();
            let browser = panel.browser();
            match browser.browser_mode {
                BrowserMode::Fs => InsertMode::Fs(browser.current_path.clone()),
                BrowserMode::Remote { ref host, ref path } => InsertMode::Remote {
                    host: host.clone(),
                    path: path.clone(),
                },
                _ => {
                    self.record_error(
                        "new-file",
                        "Cannot create files inside archives or search results.",
                    );
                    return;
                }
            }
        };
        let panel = self.get_active_panel_mut();
        let browser = panel.browser_mut();
        let base = "new_file".to_string();
        let mut candidate = base.clone();
        let mut counter = 1;
        while browser.entries.iter().any(|e| e.name == candidate) {
            candidate = format!("{base}_{counter}");
            counter += 1;
        }
        let insert_at = browser
            .entries
            .iter()
            .position(|e| e.name != "..")
            .unwrap_or(browser.entries.len());
        let location = match mode {
            InsertMode::Fs(ref dir) => EntryLocation::Fs(dir.join(&candidate)),
            InsertMode::Remote { ref host, ref path } => EntryLocation::Remote {
                host: host.clone(),
                path: format!("{}/{}", path.trim_end_matches('/'), candidate),
            },
        };
        browser.entries.insert(
            insert_at,
            DirEntry {
                name: candidate.clone(),
                is_dir: false,
                is_symlink: false,
                link_target: None,
                location,
                size: None,
                modified: None,
            },
        );
        browser.selected_index = insert_at;
        browser.inline_rename = Some(InlineRename {
            index: insert_at,
            text: candidate,
            kind: InlineEditKind::NewFile,
            focus: true,
        });
    }

    pub fn start_inline_new_dir(&mut self) {
        enum InsertMode {
            Fs(path::PathBuf),
            Remote { host: String, path: String },
        }

        let mode = {
            let panel = self.get_active_panel();
            let browser = panel.browser();
            match browser.browser_mode {
                BrowserMode::Fs => InsertMode::Fs(browser.current_path.clone()),
                BrowserMode::Remote { ref host, ref path } => InsertMode::Remote {
                    host: host.clone(),
                    path: path.clone(),
                },
                _ => {
                    self.record_error(
                        "mkdir",
                        "Cannot create directories inside archives or search results.",
                    );
                    return;
                }
            }
        };
        let panel = self.get_active_panel_mut();
        let browser = panel.browser_mut();
        let base = "new_dir".to_string();
        let mut candidate = base.clone();
        let mut counter = 1;
        while browser.entries.iter().any(|e| e.name == candidate) {
            candidate = format!("{base}_{counter}");
            counter += 1;
        }
        // Insert among directories, after ".." but before files
        let insert_at = browser
            .entries
            .iter()
            .position(|e| !e.is_dir)
            .unwrap_or(browser.entries.len());
        let location = match mode {
            InsertMode::Fs(ref dir) => EntryLocation::Fs(dir.join(&candidate)),
            InsertMode::Remote { ref host, ref path } => EntryLocation::Remote {
                host: host.clone(),
                path: format!("{}/{}", path.trim_end_matches('/'), candidate),
            },
        };
        browser.entries.insert(
            insert_at,
            DirEntry {
                name: candidate.clone(),
                is_dir: true,
                is_symlink: false,
                link_target: None,
                location,
                size: None,
                modified: None,
            },
        );
        browser.selected_index = insert_at;
        browser.inline_rename = Some(InlineRename {
            index: insert_at,
            text: candidate,
            kind: InlineEditKind::NewDir,
            focus: true,
        });
    }

    pub fn prepare_edit_selected(&mut self) {
        let (path, ext, remote) = {
            let panel = self.get_active_panel();
            let browser = panel.browser();
            if browser.entries.is_empty() {
                return;
            }
            let entry = &browser.entries[browser.selected_index];
            if entry.is_dir || entry.name == ".." {
                return;
            }
            match entry.location.clone() {
                EntryLocation::Fs(path) => {
                    let ext = syntax_ext_for_path(&path);
                    (path.clone(), ext, None)
                }
                EntryLocation::Remote { ref host, ref path } => {
                    let remote_name = path.rsplit('/').next().unwrap_or("remote_file");
                    let ext = syntax_ext_for_path(std::path::Path::new(remote_name));
                    let synthetic = std::path::PathBuf::from(format!("/sftp/{host}{path}"));
                    (synthetic, ext, Some((host.clone(), path.clone())))
                }
                _ => return,
            }
        };
        let target_panel = if let Some(side) = self.preview_panel_side() {
            side
        } else {
            match self.active_panel {
                ActivePanel::Left => ActivePanel::Right,
                ActivePanel::Right => ActivePanel::Left,
            }
        };
        let return_focus = self.active_panel;
        let target_panel_clone = target_panel;
        let request_id = self.edit_request_id.wrapping_add(1);
        self.edit_request_id = request_id;
        let path_to_send = {
            let panel = self.panel_mut(target_panel);
            let edit = EditState {
                path: Some(path),
                text: String::new(),
                ext,
                load: EditLoad::Loading,
                dirty: false,
                confirm_discard: false,
                return_focus,
                highlight_key: None,
                highlight_hash: 0,
                highlight_wrap_width: 0.0,
                highlight_dirty_at: None,
                request_id,
                wrap: true,
                crlf: false,
                force_select: None,
            };
            panel.mode = PanelMode::Edit(edit);
            match panel.mode {
                PanelMode::Edit(ref edit) => edit.path.clone(),
                _ => None,
            }
        };
        if let Some(path) = path_to_send {
            if self
                .edit_tx
                .send(EditLoadRequest {
                    id: request_id,
                    path,
                    remote,
                })
                .is_err()
                && let Some(edit) = self.edit_panel_mut()
            {
                edit.load = EditLoad::Failed;
                edit.text = "Failed to load file.".to_string();
            }
        } else if let Some(edit) = self.edit_panel_mut() {
            // No request was sent; the empty buffer must not be savable.
            edit.load = EditLoad::Failed;
        }
        self.active_panel = target_panel_clone;
    }

    pub fn update_preview_for_current_selection(&mut self) {
        let (is_dir, location, key, ext) = {
            let panel = self.get_active_panel();
            let browser = panel.browser();
            if browser.entries.is_empty() {
                self.clear_preview();
                return;
            }
            let entry = &browser.entries[browser.selected_index];
            let ext = syntax_ext_for_path(std::path::Path::new(&entry.name));
            let key = match entry.location.clone() {
                EntryLocation::Fs(path) => path.to_string_lossy().into_owned(),
                EntryLocation::Container {
                    kind,
                    archive_path,
                    inner_path,
                } => container_display_path(kind, &archive_path, &inner_path),
                EntryLocation::Remote { host, path } => format!("{host}:{path}"),
            };
            (entry.is_dir, entry.location.clone(), key, ext)
        };
        let target_panel = match self.active_panel {
            ActivePanel::Left => ActivePanel::Right,
            ActivePanel::Right => ActivePanel::Left,
        };
        self.preview_return_focus = Some(self.active_panel);
        let mut request_id = self.preview_request_id.wrapping_add(1);
        self.preview_request_id = request_id;
        // no capture
        let mut list_request: Option<(ContainerKind, path::PathBuf, u64)> = None;
        if let EntryLocation::Fs(path) = location.clone()
            && let Some(kind) = container_kind_from_path(&path)
        {
            let list_id = self.preview_request_id.wrapping_add(1);
            self.preview_request_id = list_id;
            list_request = Some((kind, path.clone(), list_id));
        }
        let _target_panel_clone = target_panel;
        {
            let panel = self.panel_mut(target_panel);
            let preview = PreviewState {
                content: None,
                key: Some(key),
                ext,
                scroll: 0.0,
                line_height: 16.0,
                page_height: 240.0,
                max_scroll: 0.0,
                can_scroll: false,
                find_open: false,
                find_query: String::new(),
                find_query_built: String::new(),
                find_matches: Vec::new(),
                find_match_num: 0,
                find_focus: false,
                request_id,
                wrap: false,
                bytes_per_row: 16,
                bytes_per_row_auto: true,
                loading_since: Some(Instant::now()),
                image_zoom: 0.0,
                image_pan: [0.0, 0.0],
            };
            panel.mode = PanelMode::Preview(preview);
        }
        let Some(preview) = self.preview_panel_mut() else {
            return;
        };
        if is_dir {
            preview.content = Some(PreviewContent::Text(format_preview_info(
                "Directory",
                &location,
            )));
            preview.loading_since = None;
            return;
        }
        match location {
            EntryLocation::Fs(path) => {
                if is_image_path(&path) {
                    preview.content = Some(PreviewContent::Image(ImageLocation::Fs(
                        std::sync::Arc::from(path),
                    )));
                    preview.loading_since = None;
                    return;
                }
                if let Some((kind, archive_path, list_id)) = list_request {
                    request_id = list_id;
                    preview.request_id = request_id;
                    preview.content = Some(PreviewContent::Text(format_preview_info(
                        "Archive",
                        &EntryLocation::Container {
                            kind,
                            archive_path: archive_path.clone(),
                            inner_path: String::new(),
                        },
                    )));
                    preview.loading_since = None;
                    let _ = self.preview_tx.send(PreviewRequest::ListContainer {
                        id: request_id,
                        kind,
                        archive_path,
                        max_entries: 200,
                    });
                    return;
                }
                let max_bytes = if is_text_path(&path) {
                    Some(64 * 1024)
                } else {
                    Some(8 * 1024)
                };
                let _ = self.preview_tx.send(PreviewRequest::Read {
                    id: request_id,
                    location: EntryLocation::Fs(path),
                    max_bytes,
                });
            }
            EntryLocation::Container {
                kind,
                archive_path,
                inner_path,
            } => {
                if is_image_name(&inner_path) {
                    preview.content = Some(PreviewContent::Image(ImageLocation::Container {
                        kind,
                        archive_path: archive_path.clone(),
                        inner_path: inner_path.clone(),
                    }));
                    preview.loading_since = None;
                    return;
                }
                let max_bytes = if is_text_name(&inner_path) {
                    Some(64 * 1024)
                } else {
                    Some(8 * 1024)
                };
                let _ = self.preview_tx.send(PreviewRequest::Read {
                    id: request_id,
                    location: EntryLocation::Container {
                        kind,
                        archive_path,
                        inner_path,
                    },
                    max_bytes,
                });
            }
            EntryLocation::Remote { host, path } => {
                if is_image_name(&path) {
                    preview.content = Some(PreviewContent::Image(ImageLocation::Remote {
                        host: host.clone(),
                        path: path.clone(),
                    }));
                    preview.loading_since = None;
                    return;
                }
                let max_bytes = if is_text_name(&path) {
                    Some(64 * 1024)
                } else {
                    Some(8 * 1024)
                };
                let _ = self.preview_tx.send(PreviewRequest::Read {
                    id: request_id,
                    location: EntryLocation::Remote { host, path },
                    max_bytes,
                });
            }
        }
    }

    pub fn toggle_preview(&mut self) {
        if self.preview_panel_side().is_some() {
            self.clear_preview();
            return;
        }
        self.update_preview_for_current_selection();
    }

    pub fn toggle_help(&mut self) {
        if let Some(side) = self.help_panel_side() {
            let fallback = self.active_panel;
            let panel = self.panel_mut(side);
            let return_focus = match panel.mode {
                PanelMode::Help(HelpState { return_focus }) => return_focus,
                _ => fallback,
            };
            panel.mode = PanelMode::Browser;
            self.active_panel = return_focus;
            return;
        }
        let target_panel = match self.active_panel {
            ActivePanel::Left => ActivePanel::Right,
            ActivePanel::Right => ActivePanel::Left,
        };
        let return_focus = self.active_panel;
        let panel = self.panel_mut(target_panel);
        panel.mode = PanelMode::Help(HelpState { return_focus });
    }

    pub fn prepare_copy_selected(&mut self) {
        if self.modal.is_some() {
            return;
        }
        if let Some(op) = self.build_copy_op() {
            self.pending_collisions = Self::op_collisions(&op);
            self.modal = Some(Modal::Confirm(op));
        }
    }

    pub fn prepare_move_selected(&mut self) {
        if self.modal.is_some() {
            return;
        }
        if let Some(op) = self.build_move_op() {
            self.pending_collisions = Self::op_collisions(&op);
            self.modal = Some(Modal::Confirm(op));
        }
    }

    /// Names of existing entries a Copy/Move would overwrite at a *local*
    /// destination. Remote destinations are not stat-checked here (that would
    /// require a blocking round-trip), so they return no collisions.
    fn op_collisions(op: &PendingOp) -> Vec<String> {
        let (items, dst) = match *op {
            PendingOp::Copy { ref items, ref dst } | PendingOp::Move { ref items, ref dst } => {
                (items, dst)
            }
            _ => return Vec::new(),
        };
        let CopyDest::Local(ref dir) = *dst else {
            return Vec::new();
        };
        items
            .iter()
            .filter_map(|item| {
                let name = item.src.display_name();
                if dir.join(&name).symlink_metadata().is_ok() {
                    Some(name)
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn prepare_delete_selected(&mut self) {
        if self.modal.is_some() {
            return;
        }
        if let Some(op) = self.build_delete_op() {
            self.modal = Some(Modal::Confirm(op));
        }
    }

    pub fn prepare_pack_selected(&mut self) {
        if self.modal.is_some() {
            return;
        }
        if let Some(op) = self.build_pack_op() {
            // Pre-fill the archive name from the first source, before `op` moves
            // into the modal.
            let name = match op {
                PendingOp::Pack { ref sources, .. } => sources
                    .first()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .map(|n| format!("{n}.zip"))
                    .unwrap_or_else(|| "archive.zip".to_string()),
                _ => "archive.zip".to_string(),
            };
            self.modal = Some(Modal::Confirm(op));
            self.rename_input = Some(name);
            self.rename_focus = true;
        }
    }

    pub fn take_pending_op(&mut self) -> Option<PendingOp> {
        self.pending_collisions.clear();
        match self.modal {
            Some(Modal::Confirm(_)) => match self.modal.take() {
                Some(Modal::Confirm(op)) => Some(op),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn clear_pending_op(&mut self) {
        self.modal = None;
        self.pending_collisions.clear();
        self.rename_input = None;
        self.rename_focus = false;
    }

    pub fn open_quick_jump(&mut self) {
        let mut entries = Vec::new();

        // Group 1: Remote paths (only if the active panel is on a remote)
        if let BrowserMode::Remote { ref host, .. } = self.get_active_panel().browser().browser_mode
        {
            let host = host.clone();
            // Remote root
            entries.push(QuickJumpEntry {
                label: format!("{host}:/"),
                path: path::PathBuf::from("/"),
                category: QuickJumpCategory::Remote,
            });
            // Remote home (if we have a session with home_dir)
            if let Some(session_arc) = self.sftp_sessions.get(&host) {
                if let Ok(session) = session_arc.try_lock() {
                    if let Some(ref home) = session.home_dir {
                        entries.push(QuickJumpEntry {
                            label: format!("{host}:~"),
                            path: path::PathBuf::from(home),
                            category: QuickJumpCategory::Remote,
                        });
                    }
                }
            }
        }

        // Group 2: Local paths — Home directory (cross-platform)
        let home_dir = std::env::var("HOME")
            .ok()
            .or_else(|| std::env::var("USERPROFILE").ok());
        if let Some(home) = home_dir {
            entries.push(QuickJumpEntry {
                label: format!("~ {}", home),
                path: path::PathBuf::from(&home),
                category: QuickJumpCategory::Home,
            });
        }

        // Windows: enumerate drive letters
        #[cfg(target_os = "windows")]
        {
            for letter in b'A'..=b'Z' {
                let drive = format!("{}:\\", letter as char);
                let dp = path::PathBuf::from(&drive);
                if dp.exists() && !entries.iter().any(|e| e.path == dp) {
                    entries.push(QuickJumpEntry {
                        label: drive,
                        path: dp,
                        category: QuickJumpCategory::Mount,
                    });
                }
            }
        }

        // macOS: enumerate /Volumes
        #[cfg(target_os = "macos")]
        if let Ok(rd) = std::fs::read_dir("/Volumes") {
            for entry in rd.flatten() {
                let mp = entry.path();
                if !entries.iter().any(|e| e.path == mp) {
                    entries.push(QuickJumpEntry {
                        label: mp.to_string_lossy().to_string(),
                        path: mp,
                        category: QuickJumpCategory::Mount,
                    });
                }
            }
        }

        // Mount points from /proc/mounts
        #[cfg(target_os = "linux")]
        if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
            let skip_types = [
                "proc",
                "sysfs",
                "devtmpfs",
                "devpts",
                "tmpfs",
                "securityfs",
                "cgroup",
                "cgroup2",
                "pstore",
                "debugfs",
                "hugetlbfs",
                "mqueue",
                "configfs",
                "fusectl",
                "tracefs",
                "bpf",
                "autofs",
                "efivarfs",
                "binfmt_misc",
                "ramfs",
                "nsfs",
                "overlay",
            ];
            let skip_paths = ["/boot", "/efi", "/snap", "/run", "/sys", "/dev", "/proc"];
            for line in mounts.lines() {
                let fields: Vec<&str> = line.split_whitespace().collect();
                if fields.len() < 3 {
                    continue;
                }
                // /proc/mounts octal-escapes space/tab/newline/backslash in the mount point.
                let mount_point = unescape_proc_mount(fields[1]);
                let fs_type = fields[2];
                if skip_types.contains(&fs_type) {
                    continue;
                }
                // udisks2 mounts removable drives under /run/media/<user>/<label> —
                // keep those even though /run itself is skipped.
                let is_removable = mount_point.starts_with("/run/media/");
                if !is_removable && skip_paths.iter().any(|p| mount_point.starts_with(p)) {
                    continue;
                }
                let mp = path::PathBuf::from(&mount_point);
                if entries.iter().any(|e| e.path == mp) {
                    continue;
                }
                entries.push(QuickJumpEntry {
                    label: mount_point,
                    path: mp,
                    category: QuickJumpCategory::Mount,
                });
            }
        }

        // SSH hosts from ~/.ssh/config (cross-platform, native SFTP)
        for host in crate::sftp::discover_ssh_hosts() {
            entries.push(QuickJumpEntry {
                label: format!("ssh: {}", host),
                path: path::PathBuf::from(&host),
                category: QuickJumpCategory::Ssh,
            });
        }

        let filtered: Vec<usize> = (0..entries.len()).collect();
        self.modal = Some(Modal::QuickJump(QuickJumpState {
            input: String::new(),
            entries,
            filtered,
            selected: 0,
            focus_input: true,
        }));
    }

    pub fn close_quick_jump(&mut self) {
        if matches!(self.modal, Some(Modal::QuickJump(_))) {
            self.modal = None;
        }
    }

    pub fn enqueue_io(&mut self, task: IOTask) {
        if let Err(e) = self.io_tx.send(task) {
            eprintln!("Failed to enqueue IO: {e}");
        } else {
            self.io_in_flight = self.io_in_flight.saturating_add(1);
            // Peak tracking — io_in_flight only grows during enqueue, so
            // taking the max here captures the batch's high-water mark.
            if self.io_in_flight > self.io_batch_total {
                self.io_batch_total = self.io_in_flight;
            }
        }
    }

    // This rule is misfiring when coupled with "pattern_type_mismatch"
    #[allow(clippy::needless_borrowed_reference)]
    pub fn enqueue_pending_op(&mut self, op: &PendingOp) {
        self.io_verb = match *op {
            PendingOp::Copy { .. } => "Copying",
            PendingOp::Move { .. } => "Moving",
            PendingOp::Delete { .. } => "Deleting",
            PendingOp::Rename { .. } => "Renaming",
            PendingOp::Pack { .. } => "Packing",
        };
        match *op {
            PendingOp::Copy { ref items, ref dst } => {
                for item in items {
                    let task = match (&item.src, dst) {
                        // Local → Local
                        (&EntryLocation::Fs(ref src), &CopyDest::Local(ref dst_dir)) => {
                            IOTask::Copy {
                                src: src.clone(),
                                dst_dir: dst_dir.clone(),
                            }
                        }
                        // Local → Remote
                        (&EntryLocation::Fs(ref src), &CopyDest::Remote { ref host, ref path }) => {
                            IOTask::CopyLocalToRemote {
                                src: src.clone(),
                                host: host.clone(),
                                remote_dir: path.clone(),
                                is_dir: item.kind == CopyKind::Directory,
                                delete_source_on_success: false,
                            }
                        }
                        // Container → Local
                        (
                            &EntryLocation::Container {
                                ref kind,
                                ref archive_path,
                                ref inner_path,
                            },
                            &CopyDest::Local(ref dst_dir),
                        ) => match item.kind {
                            CopyKind::File => IOTask::CopyContainer {
                                kind: *kind,
                                archive_path: archive_path.clone(),
                                inner_path: inner_path.clone(),
                                dst_dir: dst_dir.clone(),
                                display_name: item.src.display_name(),
                            },
                            CopyKind::Directory => IOTask::CopyContainerDir {
                                kind: *kind,
                                archive_path: archive_path.clone(),
                                inner_path: inner_path.clone(),
                                dst_dir: dst_dir.clone(),
                                display_name: item.src.display_name(),
                            },
                        },
                        // Remote → Local
                        (
                            &EntryLocation::Remote { ref host, ref path },
                            &CopyDest::Local(ref dst_dir),
                        ) => IOTask::CopyRemoteToLocal {
                            host: host.clone(),
                            remote_path: path.clone(),
                            dst_dir: dst_dir.clone(),
                            name: item.src.display_name(),
                            is_dir: item.kind == CopyKind::Directory,
                            delete_source_on_success: false,
                        },
                        // Remote → Remote
                        (
                            &EntryLocation::Remote { ref host, ref path },
                            &CopyDest::Remote {
                                host: ref dst_host,
                                path: ref dst_dir,
                            },
                        ) => {
                            if host == dst_host {
                                IOTask::CopyRemoteSameHost {
                                    host: host.clone(),
                                    src_path: path.clone(),
                                    dst_dir: dst_dir.clone(),
                                    name: item.src.display_name(),
                                }
                            } else {
                                IOTask::CopyRemoteCrossHost {
                                    src_host: host.clone(),
                                    src_path: path.clone(),
                                    dst_host: dst_host.clone(),
                                    dst_dir: dst_dir.clone(),
                                    name: item.src.display_name(),
                                    is_dir: item.kind == CopyKind::Directory,
                                }
                            }
                        }
                        // Container → Remote: extract the entry, then upload it.
                        (
                            &EntryLocation::Container {
                                ref kind,
                                ref archive_path,
                                ref inner_path,
                            },
                            &CopyDest::Remote {
                                ref host,
                                path: ref remote_dir,
                            },
                        ) => IOTask::CopyContainerToRemote {
                            kind: *kind,
                            archive_path: archive_path.clone(),
                            inner_path: inner_path.clone(),
                            host: host.clone(),
                            remote_dir: remote_dir.clone(),
                            display_name: item.src.display_name(),
                            is_dir: item.kind == CopyKind::Directory,
                        },
                    };
                    self.enqueue_io(task);
                }
            }
            PendingOp::Move { ref items, ref dst } => {
                for item in items {
                    match (&item.src, dst) {
                        // Local → Local: native rename/move
                        (&EntryLocation::Fs(ref src), &CopyDest::Local(ref dst_dir)) => {
                            self.enqueue_io(IOTask::Move {
                                src: src.clone(),
                                dst_dir: dst_dir.clone(),
                            });
                        }
                        // Local → Remote: copy, then delete local only on success.
                        // The delete is folded into the copy task so it cannot
                        // run if the upload fails (which would lose the source).
                        (&EntryLocation::Fs(ref src), &CopyDest::Remote { ref host, ref path }) => {
                            self.enqueue_io(IOTask::CopyLocalToRemote {
                                src: src.clone(),
                                host: host.clone(),
                                remote_dir: path.clone(),
                                is_dir: item.kind == CopyKind::Directory,
                                delete_source_on_success: true,
                            });
                        }
                        // Remote → Local: copy, then delete remote only on success.
                        (
                            &EntryLocation::Remote { ref host, ref path },
                            &CopyDest::Local(ref dst_dir),
                        ) => {
                            self.enqueue_io(IOTask::CopyRemoteToLocal {
                                host: host.clone(),
                                remote_path: path.clone(),
                                dst_dir: dst_dir.clone(),
                                name: item.src.display_name(),
                                is_dir: item.kind == CopyKind::Directory,
                                delete_source_on_success: true,
                            });
                        }
                        // Remote → Remote same host: use SFTP rename
                        (
                            &EntryLocation::Remote { ref host, ref path },
                            &CopyDest::Remote {
                                host: ref dst_host,
                                path: ref dst_dir,
                            },
                        ) => {
                            if host != dst_host {
                                self.record_error("move", "Cross-host move is not supported.");
                                continue;
                            }
                            self.enqueue_io(IOTask::MoveRemoteSameHost {
                                host: host.clone(),
                                src_path: path.clone(),
                                dst_dir: dst_dir.clone(),
                                name: item.src.display_name(),
                            });
                            continue;
                        }
                        // Container → Remote / other unsupported combinations
                        _ => {}
                    }
                }
            }
            PendingOp::Delete { ref targets } => {
                // Collect remote items by host so they go in one batched task each,
                // keeping the progress counter from resetting between files.
                let mut remote_batches: std::collections::HashMap<String, Vec<(String, bool)>> =
                    std::collections::HashMap::new();
                for target in targets {
                    let target_str = target.to_string_lossy();
                    if let Some(rest) = target_str.strip_prefix("/sftp/")
                        && let Some(slash) = rest.find('/')
                    {
                        let host = rest[..slash].to_string();
                        let path = rest[slash..].to_string();
                        let is_dir = self
                            .get_active_panel()
                            .browser()
                            .entries
                            .iter()
                            .find(|e| {
                                if let EntryLocation::Remote {
                                    host: ref h,
                                    path: ref p,
                                } = e.location
                                {
                                    *h == host && *p == path
                                } else {
                                    false
                                }
                            })
                            .map(|e| e.is_dir)
                            .unwrap_or(false);
                        remote_batches.entry(host).or_default().push((path, is_dir));
                        continue;
                    }
                    self.enqueue_io(IOTask::Delete {
                        target: target.clone(),
                    });
                }
                for (host, items) in remote_batches {
                    self.enqueue_io(IOTask::DeleteRemote { host, items });
                }
            }
            PendingOp::Rename { ref src } => {
                if let Some(new_name) = self.rename_input.clone() {
                    self.enqueue_io(IOTask::Rename {
                        src: src.clone(),
                        new_name,
                    });
                }
            }
            PendingOp::Pack {
                ref sources,
                ref dst_dir,
            } => {
                if let Some(archive_name) = self.rename_input.clone() {
                    let archive_path = dst_dir.join(&archive_name);
                    let kind = crate::core::container_kind_from_path(&archive_path)
                        .unwrap_or(ContainerKind::Zip);
                    self.enqueue_io(IOTask::Pack {
                        sources: sources.clone(),
                        archive_path,
                        kind,
                    });
                }
            }
        }
        // Clear marks after operation is enqueued
        self.get_active_panel_mut().browser_mut().marked.clear();
    }

    pub fn on_io_completed(&mut self, count: usize) {
        self.io_in_flight = self.io_in_flight.saturating_sub(count);
        if self.io_in_flight == 0 {
            self.io_cancel_requested = false;
            self.io_cancel_flag
                .store(false, std::sync::atomic::Ordering::Relaxed);
            self.io_batch_total = 0;
        }
    }

    pub fn request_io_cancel(&mut self) {
        if self.io_in_flight == 0 {
            return;
        }
        self.io_cancel_requested = true;
        self.io_cancel_flag
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // Only signal the worker channel when there are queued tasks to skip.
        // A single in-flight task is cancelled via the AtomicBool above; sending
        // the channel message would leave it in the queue and wrongly skip the
        // next independent task dispatched after the cancel completes.
        if self.io_in_flight > 1 {
            let _ = self.io_cancel_tx.send(());
        }
    }

    /// Returns indices of marked entries, or just the cursor entry if nothing is marked.
    /// Excludes ".." entries.
    fn effective_selection(&self) -> Vec<usize> {
        let browser = self.get_active_panel().browser();
        if browser.entries.is_empty() {
            return Vec::new();
        }
        if !browser.marked.is_empty() {
            let mut indices: Vec<usize> = (0..browser.entries.len())
                .filter(|i| browser.marked.contains(&browser.entries[*i].name))
                .collect();
            indices.sort();
            return indices;
        }
        let idx = browser.selected_index;
        if idx < browser.entries.len() && browser.entries[idx].name != ".." {
            vec![idx]
        } else {
            Vec::new()
        }
    }

    fn other_panel_copy_dest(&self) -> Option<CopyDest> {
        let other = match self.active_panel {
            ActivePanel::Left => &self.right_panel,
            ActivePanel::Right => &self.left_panel,
        };
        match other.browser().browser_mode {
            BrowserMode::Fs => Some(CopyDest::Local(other.browser().current_path.clone())),
            BrowserMode::Remote { ref host, ref path } => Some(CopyDest::Remote {
                host: host.clone(),
                path: path.clone(),
            }),
            _ => None,
        }
    }

    fn build_copy_op(&self) -> Option<PendingOp> {
        let indices = self.effective_selection();
        if indices.is_empty() {
            return None;
        }
        let dst = self.other_panel_copy_dest()?;
        let browser = self.get_active_panel().browser();
        let items: Vec<CopyItem> = indices
            .iter()
            .map(|&i| {
                let entry = &browser.entries[i];
                CopyItem {
                    src: entry.location.clone(),
                    kind: if entry.is_dir {
                        CopyKind::Directory
                    } else {
                        CopyKind::File
                    },
                }
            })
            .collect();
        Some(PendingOp::Copy { items, dst })
    }

    fn build_move_op(&self) -> Option<PendingOp> {
        let indices = self.effective_selection();
        if indices.is_empty() {
            return None;
        }
        let dst = self.other_panel_copy_dest()?;
        let browser = self.get_active_panel().browser();
        let items: Vec<CopyItem> = indices
            .iter()
            .filter_map(|&i| {
                let entry = &browser.entries[i];
                match entry.location {
                    EntryLocation::Fs(_) | EntryLocation::Remote { .. } => Some(CopyItem {
                        src: entry.location.clone(),
                        kind: if entry.is_dir {
                            CopyKind::Directory
                        } else {
                            CopyKind::File
                        },
                    }),
                    EntryLocation::Container { .. } => None,
                }
            })
            .collect();
        if items.is_empty() {
            return None;
        }
        Some(PendingOp::Move { items, dst })
    }

    fn build_delete_op(&self) -> Option<PendingOp> {
        let indices = self.effective_selection();
        if indices.is_empty() {
            return None;
        }
        let browser = self.get_active_panel().browser();
        // For remote entries, use the synthetic /sftp/host/path as target
        let targets: Vec<path::PathBuf> = indices
            .iter()
            .filter_map(|&i| match browser.entries[i].location {
                EntryLocation::Fs(ref path) => Some(path.clone()),
                EntryLocation::Remote { ref host, ref path } => {
                    Some(path::PathBuf::from(format!("/sftp/{host}{path}")))
                }
                EntryLocation::Container { .. } => None,
            })
            .collect();
        if targets.is_empty() {
            return None;
        }
        Some(PendingOp::Delete { targets })
    }

    fn build_pack_op(&self) -> Option<PendingOp> {
        let indices = self.effective_selection();
        if indices.is_empty() {
            return None;
        }
        let browser = self.get_active_panel().browser();
        // Only pack filesystem entries
        let sources: Vec<path::PathBuf> = indices
            .iter()
            .filter_map(|&i| match browser.entries[i].location {
                EntryLocation::Fs(ref path) => Some(path.clone()),
                EntryLocation::Container { .. } | EntryLocation::Remote { .. } => None,
            })
            .collect();
        if sources.is_empty() {
            return None;
        }
        // Archive goes into the current panel's directory
        let dst_dir = browser.current_path.clone();
        Some(PendingOp::Pack { sources, dst_dir })
    }

    pub fn switch_theme(&mut self) {
        if self.theme.selected_external.is_some() && self.theme_picker_open() {
            self.apply_selected_theme();
        } else {
            self.theme.toggle();
        }
    }

    pub fn open_theme_picker(&mut self) {
        self.modal = Some(Modal::ThemePicker);
        self.theme_picker_selected = self.theme.selected_external.or(Some(0));
    }

    pub fn close_theme_picker(&mut self) {
        if self.theme_picker_open() {
            self.modal = None;
        }
    }

    pub fn select_next_theme(&mut self) {
        if self.theme.external.is_empty() {
            return;
        }
        let len = self.theme.external.len();
        let cur = self.theme_picker_selected.unwrap_or(0);
        self.theme_picker_selected = Some((cur + 1) % len);
    }

    pub fn select_prev_theme(&mut self) {
        if self.theme.external.is_empty() {
            return;
        }
        let len = self.theme.external.len();
        let cur = self.theme_picker_selected.unwrap_or(0);
        self.theme_picker_selected = Some((cur + len - 1) % len);
    }

    pub fn apply_selected_theme(&mut self) {
        if let Some(i) = self.theme_picker_selected
            && i < self.theme.external.len()
        {
            self.theme.selected_external = Some(i);
        }
        if self.theme_picker_open() {
            self.modal = None;
        }
    }

    pub fn theme_names(&self) -> Vec<String> {
        if self.theme.external.is_empty() {
            vec!["Dark".to_string(), "Light".to_string()]
        } else {
            self.theme
                .external
                .iter()
                .map(|pair| pair.0.clone())
                .collect()
        }
    }

    pub fn clear_preview(&mut self) {
        let request_id = self.preview_request_id.wrapping_add(1);
        self.preview_request_id = request_id;
        let side = self.preview_panel_side();
        let Some(side) = side else {
            return;
        };
        let panel = self.panel_mut(side);
        panel.mode = PanelMode::Browser;
        if let Some(return_focus) = self.preview_return_focus.take() {
            self.active_panel = return_focus;
        }
    }
}

#[cfg(test)]
mod collision_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TmpDir(std::path::PathBuf);
    impl TmpDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let mut p = std::env::temp_dir();
            p.push(format!("fileman-collision-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&p).unwrap();
            TmpDir(p)
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fs_item(path: &str) -> CopyItem {
        CopyItem {
            src: EntryLocation::Fs(path.into()),
            kind: CopyKind::File,
        }
    }

    #[test]
    fn reports_only_existing_local_targets() {
        let dst = TmpDir::new();
        std::fs::write(dst.0.join("a.txt"), b"x").unwrap();
        let op = PendingOp::Copy {
            items: vec![fs_item("/src/a.txt"), fs_item("/src/b.txt")],
            dst: CopyDest::Local(dst.0.clone()),
        };
        assert_eq!(AppState::op_collisions(&op), vec!["a.txt".to_string()]);
    }

    #[test]
    fn move_targets_are_checked_too() {
        let dst = TmpDir::new();
        std::fs::write(dst.0.join("keep.bin"), b"x").unwrap();
        let op = PendingOp::Move {
            items: vec![fs_item("/src/keep.bin")],
            dst: CopyDest::Local(dst.0.clone()),
        };
        assert_eq!(AppState::op_collisions(&op), vec!["keep.bin".to_string()]);
    }

    #[test]
    fn remote_dest_reports_no_collisions() {
        let op = PendingOp::Copy {
            items: vec![fs_item("/src/a.txt")],
            dst: CopyDest::Remote {
                host: "h".into(),
                path: "/p".into(),
            },
        };
        assert!(AppState::op_collisions(&op).is_empty());
    }
}
