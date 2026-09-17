//! SSH transport built on sunset-client.
//!
//! Connection setup (config, ProxyJump, host keys, identities) is the shared
//! desktop client. FileMan then drives SFTP on that session's channel, and
//! opens a separate connection for each exec so a search or tar cannot stall
//! the listing. There is no executor: worker threads block on the client.

use std::{
    collections::HashMap,
    io::{self, Read as _, Write as _},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use sunset_client::{self as ssh_client, Channel, ChannelExit, Options};
use sunset_sftp::client::{MAX_READ_LEN, MAX_WRITE_LEN, SftpEvent, SftpRunner, pflags};
use sunset_sftp::protocol::{Attrs, StatusCode};

const SFTP_BUF: usize = 8192;
/// Read/write chunk for file transfers.
pub const CHUNK: usize = 256 * 1024;
const EXEC_STEP: Duration = Duration::from_millis(200);
const MAX_EXEC_CAPTURE: usize = 64 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const SFTP_HANDSHAKE: Duration = Duration::from_secs(5);

/// A remote file or directory handle, addressed by id so callers never hold
/// the server's opaque bytes.
pub type HandleId = u64;

/// Metadata for one directory entry, decoupled from sunset's borrowed types.
#[derive(Debug, Clone)]
pub struct DirItem {
    pub name: String,
    pub attrs: FileAttrs,
}

/// The subset of SFTP attributes the app uses.
#[derive(Debug, Clone, Copy, Default)]
pub struct FileAttrs {
    pub size: Option<u64>,
    pub permissions: Option<u32>,
    pub mtime: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}

impl FileAttrs {
    fn from(a: &Attrs) -> Self {
        Self {
            size: a.size,
            permissions: a.permissions,
            mtime: a.mtime,
            uid: a.uid,
            gid: a.gid,
        }
    }

    fn file_type(&self) -> u32 {
        self.permissions.unwrap_or(0) & 0o170000
    }

    pub fn is_dir(&self) -> bool {
        self.file_type() == 0o040000
    }

    pub fn is_symlink(&self) -> bool {
        self.file_type() == 0o120000
    }
}

/// How a remote file should be opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    Read,
    Write,
}

/// Whether a streamed command is given an input pipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stdin {
    Piped,
    Closed,
}

/// An error from the remote side.
///
/// `fatal` marks the ones that mean the connection itself is gone, which the
/// app uses to decide whether to drop the session rather than just report the
/// operation as failed.
#[derive(Debug, Clone)]
pub struct SshError {
    pub message: String,
    pub fatal: bool,
}

impl SshError {
    pub fn op(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            fatal: false,
        }
    }

    pub fn fatal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            fatal: true,
        }
    }
}

impl std::fmt::Display for SshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SshError {}

pub type SshResult<T> = Result<T, SshError>;

/// How a remote command finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitStatus {
    Code(u32),
    Signal(String),
}

impl std::fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Self::Code(c) => write!(f, "exit status {c}"),
            Self::Signal(ref s) => write!(f, "killed by {s}"),
        }
    }
}

impl From<&ChannelExit> for ExitStatus {
    fn from(e: &ChannelExit) -> Self {
        match *e {
            ChannelExit::Status(c) => Self::Code(c),
            ChannelExit::Signal(ref s) => Self::Signal(s.clone()),
        }
    }
}

/// Captured result of a command run over exec.
#[derive(Debug, Clone, Default)]
pub struct ExecOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// How it finished, when the server said. Servers are not obliged to.
    pub exit: Option<ExitStatus>,
}

impl ExecOutput {
    /// Whether the command reported failure.
    ///
    /// A server that sends no exit status leaves this false, so a caller that
    /// needs to know should look at stderr as well.
    pub fn failed(&self) -> bool {
        !matches!(self.exit, None | Some(ExitStatus::Code(0)))
    }
}

/// An SFTP reply, with nothing borrowed from the runner.
#[derive(Debug)]
enum Reply {
    Version,
    Handle(Vec<u8>),
    Attrs(FileAttrs),
    Status(StatusCode),
    Data(usize),
    NameStart(u32),
    Name(String, FileAttrs),
    NameEnd,
}

impl Reply {
    fn into_status(self, what: &str) -> SshResult<()> {
        match self {
            Reply::Status(StatusCode::SSH_FX_OK) => Ok(()),
            Reply::Status(c) => Err(status_err(what, c)),
            other => Err(SshError::fatal(format!("{what}: unexpected {other:?}"))),
        }
    }
}

fn status_err(what: &str, code: StatusCode) -> SshError {
    SshError::op(format!("{what}: {code}"))
}

fn sftp_err(e: sunset_sftp::error::SftpError) -> SshError {
    SshError::fatal(format!("SFTP: {e}"))
}

fn client_err(e: ssh_client::Error) -> SshError {
    let message = e.detail().to_string();
    match e.kind {
        ssh_client::Kind::Configuration => SshError::op(message),
        _ => SshError::fatal(message),
    }
}

fn blocked(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}

/// A live SFTP connection to one host.
///
/// Operations take the lock for their duration. Exec uses a separate SSH
/// connection so a long command cannot stall the listing.
pub struct Conn {
    session: Mutex<Session>,
    alive: Arc<AtomicBool>,
    options: Options,
    pub host: String,
    pub home_dir: Option<String>,
}

impl Conn {
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    fn with<T>(&self, f: impl FnOnce(&mut Session) -> SshResult<T>) -> SshResult<T> {
        let mut s = self.lock()?;
        let r = f(&mut s);
        if let Err(ref e) = r
            && e.fatal
        {
            self.alive.store(false, Ordering::Relaxed);
        }
        r
    }

    fn with_retry<T>(&self, f: impl Fn(&mut Session) -> SshResult<T>) -> SshResult<T> {
        let mut s = self.lock()?;
        let first = f(&mut s);
        let Err(e) = first else {
            return first;
        };
        if !e.fatal {
            return Err(e);
        }

        log::info!("reconnecting to {}: {e}", self.host);
        match open_sftp(&self.options) {
            Ok(fresh) => {
                *s = fresh;
                let again = f(&mut s);
                if let Err(ref e) = again
                    && e.fatal
                {
                    self.alive.store(false, Ordering::Relaxed);
                }
                again
            }
            Err(reconnect_err) => {
                self.alive.store(false, Ordering::Relaxed);
                log::warn!("reconnecting to {} failed: {reconnect_err}", self.host);
                Err(e)
            }
        }
    }

    fn lock(&self) -> SshResult<std::sync::MutexGuard<'_, Session>> {
        if !self.is_alive() {
            return Err(SshError::fatal("SSH connection closed"));
        }
        self.session.lock().map_err(|_| {
            self.alive.store(false, Ordering::Relaxed);
            SshError::fatal("SSH session poisoned")
        })
    }

    pub fn realpath(&self, path: &str) -> SshResult<String> {
        self.with_retry(|s| {
            s.sftp.realpath(path).map_err(sftp_err)?;
            s.one_name(&format!("realpath {path}"))
        })
    }

    pub fn readlink(&self, path: &str) -> SshResult<String> {
        self.with_retry(|s| {
            s.sftp.readlink(path).map_err(sftp_err)?;
            s.one_name(&format!("readlink {path}"))
        })
    }

    pub fn stat(&self, path: &str) -> SshResult<FileAttrs> {
        self.with_retry(|s| {
            s.sftp.stat(path).map_err(sftp_err)?;
            s.attrs_reply(&format!("stat {path}"))
        })
    }

    pub fn lstat(&self, path: &str) -> SshResult<FileAttrs> {
        self.with_retry(|s| {
            s.sftp.lstat(path).map_err(sftp_err)?;
            s.attrs_reply(&format!("lstat {path}"))
        })
    }

    pub fn set_stat(&self, path: &str, attrs: FileAttrs) -> SshResult<()> {
        self.with(|s| {
            let a = Attrs {
                permissions: attrs.permissions,
                mtime: attrs.mtime,
                atime: attrs.mtime,
                ..Attrs::default()
            };
            s.sftp.setstat(path, &a).map_err(sftp_err)?;
            s.reply()?.into_status(&format!("setstat {path}"))
        })
    }

    pub fn symlink(&self, target: &str, link: &str) -> SshResult<()> {
        self.with(|s| {
            s.sftp.symlink(target, link).map_err(sftp_err)?;
            s.reply()?.into_status(&format!("symlink {link}"))
        })
    }

    pub fn mkdir(&self, path: &str) -> SshResult<()> {
        self.with(|s| {
            s.sftp.mkdir(path, &Attrs::default()).map_err(sftp_err)?;
            s.reply()?.into_status(&format!("mkdir {path}"))
        })
    }

    pub fn rmdir(&self, path: &str) -> SshResult<()> {
        self.with(|s| {
            s.sftp.rmdir(path).map_err(sftp_err)?;
            s.reply()?.into_status(&format!("rmdir {path}"))
        })
    }

    pub fn remove(&self, path: &str) -> SshResult<()> {
        self.with(|s| {
            s.sftp.remove(path).map_err(sftp_err)?;
            s.reply()?.into_status(&format!("remove {path}"))
        })
    }

    pub fn rename(&self, from: &str, to: &str) -> SshResult<()> {
        self.with(|s| {
            s.sftp.rename(from, to).map_err(sftp_err)?;
            s.reply()?.into_status(&format!("rename {from} -> {to}"))
        })
    }

    pub fn open_dir(&self, path: &str) -> SshResult<HandleId> {
        self.with_retry(|s| {
            s.sftp.opendir(path).map_err(sftp_err)?;
            let what = format!("opendir {path}");
            match s.reply()? {
                Reply::Handle(h) => Ok(s.store_handle(h)),
                other => Err(handle_err(&what, other)),
            }
        })
    }

    pub fn read_dir(&self, handle: HandleId) -> SshResult<Option<Vec<DirItem>>> {
        self.with(|s| {
            let h = s.handle(handle)?;
            s.sftp.readdir(&h).map_err(sftp_err)?;
            let mut items = Vec::new();
            loop {
                match s.reply()? {
                    Reply::NameStart(n) => items.reserve(n as usize),
                    Reply::Name(name, attrs) => items.push(DirItem { name, attrs }),
                    Reply::NameEnd => return Ok(Some(items)),
                    Reply::Status(StatusCode::SSH_FX_EOF) => return Ok(None),
                    Reply::Status(c) => return Err(status_err("readdir", c)),
                    other => return Err(handle_err("readdir", other)),
                }
            }
        })
    }

    pub fn open(&self, path: &str, mode: OpenMode) -> SshResult<HandleId> {
        let run = |s: &mut Session| {
            let flags = match mode {
                OpenMode::Read => pflags::READ,
                OpenMode::Write => pflags::WRITE | pflags::CREAT | pflags::TRUNC,
            };
            s.sftp
                .open(path, flags, &Attrs::default())
                .map_err(sftp_err)?;
            let what = format!("open {path}");
            match s.reply()? {
                Reply::Handle(h) => Ok(s.store_handle(h)),
                other => Err(handle_err(&what, other)),
            }
        };
        match mode {
            OpenMode::Read => self.with_retry(run),
            OpenMode::Write => self.with(run),
        }
    }

    pub fn read_at(&self, handle: HandleId, offset: u64, len: usize) -> SshResult<Vec<u8>> {
        self.with(|s| s.read_at(handle, offset, len))
    }

    pub fn write_at(&self, handle: HandleId, offset: u64, data: Vec<u8>) -> SshResult<()> {
        self.with(|s| s.write_at(handle, offset, &data))
    }

    pub fn close(&self, handle: HandleId) -> SshResult<()> {
        self.with(|s| {
            let Some(h) = s.handles.remove(&handle) else {
                return Ok(());
            };
            s.sftp.close(&h).map_err(sftp_err)?;
            s.reply()?.into_status("close")
        })
    }

    pub fn exec(&self, cmd: &str) -> SshResult<ExecOutput> {
        let mut stream = open_exec(&self.options, cmd, Stdin::Closed)?;
        let mut out = ExecOutput::default();
        let mut buf = [0u8; 8192];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.stdout.extend_from_slice(&buf[..n]),
                Err(e) => return Err(SshError::fatal(e.to_string())),
            }
            if out.stdout.len() + out.stderr.len() > MAX_EXEC_CAPTURE {
                stream.channel.abort();
                return Err(SshError::op(format!(
                    "command produced more than {MAX_EXEC_CAPTURE} bytes"
                )));
            }
        }
        let mut err = [0u8; 8192];
        loop {
            match stream.channel.read_stderr(&mut err) {
                Ok(0) => break,
                Ok(n) => out.stderr.extend_from_slice(&err[..n]),
                Err(e) if blocked(&e) => {
                    if stream.channel.eof() {
                        break;
                    }
                    wait_channel(&mut stream.channel, None)?;
                }
                Err(e) => return Err(SshError::fatal(e.to_string())),
            }
        }
        out.exit = stream.channel.exit().map(ExitStatus::from);
        Ok(out)
    }

    pub fn exec_stream(&self, cmd: &str, stdin: Stdin) -> SshResult<ExecStream> {
        open_exec(&self.options, cmd, stdin)
    }
}

fn handle_err(what: &str, got: Reply) -> SshError {
    match got {
        Reply::Status(c) => status_err(what, c),
        other => SshError::fatal(format!("{what}: unexpected {other:?}")),
    }
}

struct Session {
    channel: Channel,
    sftp: SftpRunner<SFTP_BUF, SFTP_BUF>,
    leftover: Vec<u8>,
    leftover_at: usize,
    handles: HashMap<HandleId, Vec<u8>>,
    next_handle: HandleId,
}

impl Session {
    fn store_handle(&mut self, h: Vec<u8>) -> HandleId {
        self.next_handle += 1;
        self.handles.insert(self.next_handle, h);
        self.next_handle
    }

    fn handle(&self, id: HandleId) -> SshResult<Vec<u8>> {
        self.handles
            .get(&id)
            .cloned()
            .ok_or_else(|| SshError::op("stale remote handle"))
    }

    fn deadline(&self) -> Instant {
        Instant::now() + self.channel.timeout()
    }

    fn send(&mut self, mut data: &[u8]) -> SshResult<()> {
        let deadline = self.deadline();
        loop {
            remaining(deadline)?;
            let mut progressed = false;
            while !self.sftp.output_buf().is_empty() {
                match self.channel.write(self.sftp.output_buf()) {
                    Ok(0) => return Err(SshError::fatal("SFTP channel closed during write")),
                    Ok(n) => {
                        self.sftp.consume_output(n);
                        progressed = true;
                    }
                    Err(e) if blocked(&e) => break,
                    Err(e) => return Err(SshError::fatal(format!("write SFTP: {e}"))),
                }
            }
            if let Some(len) = self.sftp.send_data() {
                if data.len() < len {
                    return Err(SshError::fatal(
                        "SFTP write payload is shorter than the request",
                    ));
                }
                match self.channel.write(&data[..len]) {
                    Ok(0) => return Err(SshError::fatal("SFTP channel closed during write")),
                    Ok(n) => {
                        self.sftp.data_sent(n);
                        data = &data[n..];
                        progressed = true;
                    }
                    Err(e) if blocked(&e) => {}
                    Err(e) => return Err(SshError::fatal(format!("write SFTP payload: {e}"))),
                }
            }
            if self.sftp.output_done() {
                match self.channel.flush() {
                    Ok(()) => return Ok(()),
                    Err(e) if blocked(&e) => {}
                    Err(e) => return Err(SshError::fatal(format!("flush SFTP: {e}"))),
                }
            }
            if !progressed {
                wait_channel(&mut self.channel, Some(deadline))?;
            }
        }
    }

    fn reply(&mut self) -> SshResult<Reply> {
        self.reply_until(self.deadline())
    }

    fn reply_until(&mut self, deadline: Instant) -> SshResult<Reply> {
        self.send(&[])?;
        let mut incoming = [0u8; SFTP_BUF];
        while !self.sftp.has_event() {
            remaining(deadline)?;
            if self.leftover_at < self.leftover.len() {
                let used = self
                    .sftp
                    .input(&self.leftover[self.leftover_at..])
                    .map_err(sftp_err)?;
                self.leftover_at += used;
                if self.leftover_at == self.leftover.len() {
                    self.leftover.clear();
                    self.leftover_at = 0;
                }
                if used == 0 {
                    return Err(SshError::fatal("SFTP parser stalled on leftover input"));
                }
                continue;
            }
            match self.channel.read(&mut incoming) {
                Ok(0) => return Err(SshError::fatal("SFTP channel ended")),
                Ok(n) => {
                    let mut used = 0;
                    while used < n && !self.sftp.has_event() {
                        let step = self.sftp.input(&incoming[used..n]).map_err(sftp_err)?;
                        if step == 0 {
                            break;
                        }
                        used += step;
                    }
                    if used < n {
                        self.leftover.extend_from_slice(&incoming[used..n]);
                    }
                }
                Err(e) if blocked(&e) => wait_channel(&mut self.channel, Some(deadline))?,
                Err(e) => return Err(SshError::fatal(format!("read SFTP: {e}"))),
            }
        }
        let ev = self
            .sftp
            .event()
            .ok_or_else(|| SshError::fatal("SFTP reply vanished"))?;
        Ok(match ev {
            SftpEvent::Version { .. } => Reply::Version,
            SftpEvent::Handle { handle, .. } => Reply::Handle(handle.to_vec()),
            SftpEvent::Attrs { attrs, .. } => Reply::Attrs(FileAttrs::from(&attrs)),
            SftpEvent::Status { code, .. } => Reply::Status(code),
            SftpEvent::Data { len, .. } => Reply::Data(len),
            SftpEvent::NameStart { count, .. } => Reply::NameStart(count),
            SftpEvent::Name {
                filename, attrs, ..
            } => Reply::Name(
                String::from_utf8_lossy(filename).into_owned(),
                FileAttrs::from(&attrs),
            ),
            SftpEvent::NameEnd { .. } => Reply::NameEnd,
        })
    }

    fn attrs_reply(&mut self, what: &str) -> SshResult<FileAttrs> {
        match self.reply()? {
            Reply::Attrs(a) => Ok(a),
            other => Err(handle_err(what, other)),
        }
    }

    fn one_name(&mut self, what: &str) -> SshResult<String> {
        let mut found = None;
        loop {
            match self.reply()? {
                Reply::NameStart(_) => (),
                Reply::Name(n, _) => found = found.or(Some(n)),
                Reply::NameEnd => {
                    return found.ok_or_else(|| SshError::op(format!("{what}: no name returned")));
                }
                Reply::Status(c) => return Err(status_err(what, c)),
                other => return Err(handle_err(what, other)),
            }
        }
    }

    fn read_at(&mut self, handle: HandleId, offset: u64, len: usize) -> SshResult<Vec<u8>> {
        let h = self.handle(handle)?;
        let want = len.min(MAX_READ_LEN as usize) as u32;
        self.sftp.read(&h, offset, want).map_err(sftp_err)?;
        match self.reply()? {
            Reply::Data(n) => {
                let mut out = vec![0u8; n];
                self.take_file_data(&mut out)?;
                Ok(out)
            }
            Reply::Status(StatusCode::SSH_FX_EOF) => Ok(Vec::new()),
            Reply::Status(c) => Err(status_err("read", c)),
            other => Err(handle_err("read", other)),
        }
    }

    fn take_file_data(&mut self, dest: &mut [u8]) -> SshResult<()> {
        let deadline = self.deadline();
        let mut got = 0;
        while got < dest.len() {
            remaining(deadline)?;
            if self.leftover_at < self.leftover.len() {
                let n = (self.leftover.len() - self.leftover_at).min(dest.len() - got);
                dest[got..got + n]
                    .copy_from_slice(&self.leftover[self.leftover_at..self.leftover_at + n]);
                self.leftover_at += n;
                got += n;
                if self.leftover_at == self.leftover.len() {
                    self.leftover.clear();
                    self.leftover_at = 0;
                }
                continue;
            }
            match self.channel.read(&mut dest[got..]) {
                Ok(0) => return Err(SshError::fatal("SFTP channel closed mid-transfer")),
                Ok(n) => got += n,
                Err(e) if blocked(&e) => wait_channel(&mut self.channel, Some(deadline))?,
                Err(e) => return Err(SshError::fatal(format!("read SFTP data: {e}"))),
            }
        }
        self.sftp.data_taken(dest.len());
        Ok(())
    }

    fn write_at(&mut self, handle: HandleId, offset: u64, data: &[u8]) -> SshResult<()> {
        let h = self.handle(handle)?;
        let mut sent = 0u64;
        for part in data.chunks(MAX_WRITE_LEN as usize) {
            self.sftp
                .write(&h, offset + sent, part.len())
                .map_err(sftp_err)?;
            self.send(part)?;
            self.reply()?.into_status("write")?;
            sent += part.len() as u64;
        }
        Ok(())
    }
}

fn remaining(deadline: Instant) -> SshResult<()> {
    if Instant::now() >= deadline {
        Err(SshError::fatal("timed out waiting for the SSH server"))
    } else {
        Ok(())
    }
}

fn wait_channel(channel: &mut Channel, deadline: Option<Instant>) -> SshResult<()> {
    let deadline = deadline.unwrap_or_else(|| Instant::now() + channel.timeout());
    remaining(deadline)?;
    channel.wait(deadline).map_err(client_err)
}

fn open_sftp(options: &Options) -> SshResult<Session> {
    let channel = ssh_client::Connection::connect(options)
        .map_err(client_err)?
        .subsystem("sftp")
        .map_err(client_err)?;
    let mut session = Session {
        channel,
        sftp: SftpRunner::new(),
        leftover: Vec::new(),
        leftover_at: 0,
        handles: HashMap::new(),
        next_handle: 0,
    };
    session.sftp.init().map_err(sftp_err)?;
    let host = options.host.clone();
    match session.reply_until(Instant::now() + SFTP_HANDSHAKE) {
        Ok(Reply::Version) => Ok(session),
        Ok(other) => Err(SshError::fatal(format!(
            "SFTP handshake: unexpected {other:?}"
        ))),
        Err(e) => Err(SshError::fatal(format!(
            "SFTP is not available on {host} ({})",
            e.message
        ))),
    }
}

fn open_exec(options: &Options, cmd: &str, stdin: Stdin) -> SshResult<ExecStream> {
    let mut channel = ssh_client::Connection::connect(options)
        .map_err(client_err)?
        .exec(cmd)
        .map_err(client_err)?;
    if stdin == Stdin::Closed {
        let _ = channel.send_eof();
    }
    Ok(ExecStream {
        channel,
        stdin,
        cancel: None,
    })
}

/// The local end of a streamed command.
pub struct ExecStream {
    channel: Channel,
    stdin: Stdin,
    cancel: Option<Arc<AtomicBool>>,
}

impl ExecStream {
    pub fn with_cancel(mut self, cancel: Arc<AtomicBool>) -> Self {
        self.cancel = Some(cancel);
        self
    }

    pub fn finish_input(&mut self) {
        if self.stdin == Stdin::Piped {
            let _ = self.channel.send_eof();
            self.stdin = Stdin::Closed;
        }
    }

    pub fn wait(mut self) -> SshResult<ExecOutput> {
        self.finish_input();
        let mut out = ExecOutput::default();
        let mut buf = [0u8; 8192];
        loop {
            self.check_cancel()?;
            match self.channel.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) if blocked(&e) => {
                    wait_exec(&mut self.channel, self.cancel.as_ref())?;
                }
                Err(e) => return Err(SshError::fatal(e.to_string())),
            }
        }
        loop {
            match self.channel.read_stderr(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.stderr.extend_from_slice(&buf[..n]),
                Err(e) if blocked(&e) => {
                    if self.channel.eof() {
                        break;
                    }
                    wait_exec(&mut self.channel, self.cancel.as_ref())?;
                }
                Err(e) => return Err(SshError::fatal(e.to_string())),
            }
        }
        out.exit = self.channel.exit().map(ExitStatus::from);
        Ok(out)
    }

    fn check_cancel(&mut self) -> SshResult<()> {
        if self
            .cancel
            .as_ref()
            .is_some_and(|c| c.load(Ordering::Relaxed))
        {
            self.channel.abort();
            return Err(SshError::op("Cancelled"));
        }
        Ok(())
    }
}

fn wait_exec(channel: &mut Channel, cancel: Option<&Arc<AtomicBool>>) -> SshResult<()> {
    loop {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            channel.abort();
            return Err(SshError::op("Cancelled"));
        }
        match channel.wait(Instant::now() + EXEC_STEP) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind == ssh_client::Kind::Timeout => continue,
            Err(e) => return Err(client_err(e)),
        }
    }
}

impl io::Read for ExecStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            self.check_cancel().map_err(io::Error::other)?;
            match self.channel.read(buf) {
                Ok(n) => return Ok(n),
                Err(e) if blocked(&e) => {
                    wait_exec(&mut self.channel, self.cancel.as_ref()).map_err(io::Error::other)?;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

impl io::Write for ExecStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.stdin != Stdin::Piped {
            return Err(io::Error::other("exec stdin is closed"));
        }
        let mut sent = 0;
        while sent < buf.len() {
            self.check_cancel().map_err(io::Error::other)?;
            match self.channel.write(&buf[sent..]) {
                Ok(0) => return Err(io::ErrorKind::BrokenPipe.into()),
                Ok(n) => sent += n,
                Err(e) if blocked(&e) => {
                    wait_exec(&mut self.channel, self.cancel.as_ref()).map_err(io::Error::other)?;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(sent)
    }

    fn flush(&mut self) -> io::Result<()> {
        loop {
            match self.channel.flush() {
                Ok(()) => return Ok(()),
                Err(e) if blocked(&e) => {
                    wait_exec(&mut self.channel, self.cancel.as_ref()).map_err(io::Error::other)?;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

impl Drop for ExecStream {
    fn drop(&mut self) {
        self.channel.abort();
    }
}

/// A remote file opened for reading, presented as a seekable byte stream.
pub struct RemoteFile {
    conn: Arc<Conn>,
    handle: HandleId,
    offset: u64,
    size: Option<u64>,
}

impl RemoteFile {
    pub fn open(conn: Arc<Conn>, path: &str) -> SshResult<Self> {
        let size = conn.stat(path).ok().and_then(|a| a.size);
        let handle = conn.open(path, OpenMode::Read)?;
        Ok(Self {
            conn,
            handle,
            offset: 0,
            size,
        })
    }

    pub fn size(&self) -> Option<u64> {
        self.size
    }
}

impl io::Read for RemoteFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let data = self
            .conn
            .read_at(self.handle, self.offset, buf.len().min(CHUNK))
            .map_err(io::Error::other)?;
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        self.offset += n as u64;
        Ok(n)
    }
}

impl io::Seek for RemoteFile {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        let (base, delta) = match pos {
            io::SeekFrom::Start(n) => {
                self.offset = n;
                return Ok(n);
            }
            io::SeekFrom::Current(d) => (self.offset as i64, d),
            io::SeekFrom::End(d) => {
                let size = self
                    .size
                    .ok_or_else(|| io::Error::other("remote file size is unknown"))?;
                (size as i64, d)
            }
        };
        let target = base.checked_add(delta).unwrap_or(0);
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before start of file",
            ));
        }
        self.offset = target as u64;
        Ok(self.offset)
    }
}

impl Drop for RemoteFile {
    fn drop(&mut self) {
        let _ = self.conn.close(self.handle);
    }
}

/// Opens a connection, blocking until the SFTP subsystem is ready.
pub fn connect(alias: &str, options: Options) -> SshResult<Conn> {
    let mut session = open_sftp(&options)?;
    let home_dir = match session.sftp.realpath(".") {
        Ok(_) => session
            .one_name("realpath .")
            .ok()
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty() && p != "." && p != "./"),
        Err(_) => None,
    };

    let mut conn = Conn {
        session: Mutex::new(session),
        alive: Arc::new(AtomicBool::new(true)),
        options,
        host: alias.to_string(),
        home_dir,
    };
    if conn.home_dir.as_deref().is_none_or(|p| p == "/")
        && let Ok(out) = conn.exec("pwd")
    {
        let p = String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        if !p.is_empty() && p != "/" && p != "." {
            conn.home_dir = Some(p);
        }
    }

    Ok(conn)
}

pub fn connect_timeout() -> Duration {
    CONNECT_TIMEOUT
}
