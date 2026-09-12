//! SSH transport built on sunset.
//!
//! Both sunset layers used here are sans-io: `sunset::Runner` is the SSH
//! protocol and `SftpRunner` is SFTP on top of it, and neither does any IO of
//! its own. That suits FileMan, which is blocking and thread-per-operation, so
//! this module drives them straight from a blocking socket. There is no
//! executor and no background thread: a [`Conn`] is a `Mutex` around the
//! session, and whichever worker thread calls in does the pumping itself.
//!
//! Four properties of the runners shape the loop, and all of them cause a
//! hang rather than an error when ignored:
//!
//! - An event borrows the runner, so anything that needs the runner again has
//!   to wait until that borrow ends.
//! - The SSH runner stops accepting socket input while a payload is waiting to
//!   be collected, so [`Session::feed`] must return and let the caller drain a
//!   channel rather than looping.
//! - Channel data has to be moved out by hand; nothing else will do it, and
//!   the peer stops sending once its window is unacknowledged.
//! - `SftpRunner::want_buf` asks for exactly the bytes it wants next, so it
//!   must be filled from the channel rather than read past.

use std::{
    collections::HashMap,
    io::{self, Read as _, Write as _},
    net::TcpStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use sunset::{ChanData, ChanHandle, CliEvent, Event, Runner, SignKey};
use sunset_sftp::client::{MAX_READ_LEN, MAX_WRITE_LEN, SftpEvent, SftpRunner, pflags};
use sunset_sftp::protocol::{Attrs, StatusCode};

mod agent;
pub mod knownhosts;

/// One encoded request, and the largest reply part that has to be buffered.
const SFTP_BUF: usize = 8192;
/// Read/write chunk for file transfers. The runner splits this into protocol
/// sized requests, so a large chunk costs fewer round trips.
pub const CHUNK: usize = 256 * 1024;
/// Socket read size.
const SOCK_BUF: usize = 32 * 1024;
const SOCK_TIMEOUT: Duration = Duration::from_millis(200);
/// How much of a command's output to hold before leaving the rest on the
/// channel. The peer's window then throttles it, rather than us discarding
/// what does not fit.
const EXEC_HIGH_WATER: usize = 1024 * 1024;
/// Bound on a command whose output is captured whole, since nothing drains it
/// until the command ends. Exceeding it fails rather than truncating.
const MAX_EXEC_CAPTURE: usize = 64 * 1024 * 1024;

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
    /// The status of a request whose only answer is a status.
    fn into_status(self, what: &str) -> SshResult<()> {
        match self {
            Reply::Status(StatusCode::SSH_FX_OK) => Ok(()),
            Reply::Status(c) => Err(status_err(what, c)),
            other => Err(SshError::fatal(format!("{what}: unexpected {other:?}"))),
        }
    }
}

/// A status reply is the server refusing one request, not a broken session.
fn status_err(what: &str, code: StatusCode) -> SshError {
    SshError::op(format!("{what}: {code}"))
}

/// A live connection to one host.
///
/// Operations take the lock for their duration, so the connection is used by
/// one caller at a time. That matches how the app already serialises a host's
/// session, and is what the single set of runners requires.
pub struct Conn {
    session: Mutex<Session>,
    alive: Arc<AtomicBool>,
    /// Kept so a dropped connection can be rebuilt without the caller
    /// having to notice, reconnect, and navigate back to where it was.
    params: ConnectParams,
    pub host: String,
    pub home_dir: Option<String>,
}

impl Conn {
    /// True until an operation has failed in a way that ends the connection.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// Runs `f` against the locked session, retiring the connection if it
    /// turns out to be broken.
    ///
    /// For anything naming a [`HandleId`]: the handle belongs to this session,
    /// so a reconnect would leave it addressing nothing. Those report the
    /// failure and let the caller open again.
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

    /// Runs `f`, and if the connection turns out to have died, dials again
    /// and runs it once more.
    ///
    /// A connection dropped by a sleep or a network change is only discovered
    /// when something is next asked of it, which would otherwise surface as an
    /// error the user has to clear by reconnecting by hand.
    ///
    /// Only for operations that change nothing. A fatal error means no reply
    /// was seen, so a mutation may or may not have been applied, and running
    /// it again would report a confusing "already exists" or "no such file"
    /// for work that in fact succeeded.
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
        match open_session(&self.params) {
            Ok(fresh) => {
                // Handles from the old session die with it; nothing outside
                // holds one across a call that reconnects.
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
                // The original failure is the useful one to report.
                Err(e)
            }
        }
    }

    fn lock(&self) -> SshResult<std::sync::MutexGuard<'_, Session>> {
        if !self.is_alive() {
            return Err(SshError::fatal("SSH connection closed"));
        }
        self.session.lock().map_err(|_| {
            // A panic while holding it leaves the runners mid-packet.
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
            // Plain rename, which fails rather than replacing an existing
            // destination. File operations are not meant to clobber.
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

    /// One server batch of directory entries. `None` means the listing ended.
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
                    // EOF ends the listing rather than failing it.
                    Reply::Status(StatusCode::SSH_FX_EOF) => return Ok(None),
                    Reply::Status(c) => return Err(status_err("readdir", c)),
                    other => return Err(handle_err("readdir", other)),
                }
            }
        })
    }

    pub fn open(&self, path: &str, mode: OpenMode) -> SshResult<HandleId> {
        // Opening for reading is free to repeat; opening for writing
        // truncates, so it is not.
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
                // Closing twice is not worth surfacing.
                return Ok(());
            };
            s.sftp.close(&h).map_err(sftp_err)?;
            s.reply()?.into_status("close")
        })
    }

    /// Runs a command, returning its captured output.
    pub fn exec(&self, cmd: &str) -> SshResult<ExecOutput> {
        self.with(|s| {
            let id = s.start_exec(cmd)?;
            let mut out = ExecOutput::default();
            let r = loop {
                let done = match s.pump_exec(id) {
                    Ok(d) => d,
                    Err(e) => break Err(e),
                };
                let e = s.exec_mut(id)?;
                out.stdout.append(&mut e.out);
                out.stderr.append(&mut e.err);
                if out.stdout.len() + out.stderr.len() > MAX_EXEC_CAPTURE {
                    break Err(SshError::op(format!(
                        "command produced more than {MAX_EXEC_CAPTURE} bytes"
                    )));
                }
                if done {
                    out.exit = e.exit.clone();
                    break Ok(());
                }
            };
            s.finish_exec(id);
            r.map(|()| out)
        })
    }

    /// Starts a command, streaming its stdout through the returned reader.
    ///
    /// With [`Stdin::Piped`] the stream is also an [`io::Write`] feeding the
    /// command's input; [`ExecStream::finish_input`] then signals end of input,
    /// which is what `tar xf -` waits for.
    pub fn exec_stream(self: &Arc<Self>, cmd: &str, stdin: Stdin) -> SshResult<ExecStream> {
        let id = self.with(|s| s.start_exec(cmd))?;
        Ok(ExecStream {
            conn: self.clone(),
            id,
            stdin,
            eof: false,
            pending: Vec::new(),
            pos: 0,
            cancel: None,
        })
    }
}

fn handle_err(what: &str, got: Reply) -> SshError {
    match got {
        Reply::Status(c) => status_err(what, c),
        other => SshError::fatal(format!("{what}: unexpected {other:?}")),
    }
}

/// Maps an SFTP-level failure onto our error type. These come from the
/// protocol machinery rather than the server, so the session is in doubt.
fn sftp_err(e: sunset_sftp::error::SftpError) -> SshError {
    SshError::fatal(format!("SFTP: {e}"))
}

fn ssh_err(e: sunset::Error) -> SshError {
    SshError::fatal(format!("SSH: {e}"))
}

/// A command running on its own channel.
struct Exec {
    chan: ChanHandle,
    out: Vec<u8>,
    err: Vec<u8>,
    /// Set once the channel will produce nothing more.
    eof: bool,
    /// Set once the peer has closed the channel, which is what follows the
    /// exit status. Waiting for it is how the status is not missed.
    closed: bool,
    sent_eof: bool,
    exit: Option<ExitStatus>,
}

impl Exec {
    /// Whether the command is finished and its result is in.
    ///
    /// EOF alone is too early: the exit status is a channel request that
    /// arrives around it, so a caller stopping at EOF sees no status at all.
    fn done(&self) -> bool {
        self.eof && (self.exit.is_some() || self.closed)
    }
}

/// What a freshly opened channel should be asked to run.
enum Want {
    Sftp,
    Exec(String),
}

struct Session {
    sock: TcpStream,
    ssh: Runner<'static, sunset::Client>,
    sftp: SftpRunner<SFTP_BUF, SFTP_BUF>,
    sftp_chan: Option<ChanHandle>,
    /// Channels opened but not yet answered with their request.
    wanted: HashMap<u32, Want>,
    execs: HashMap<u64, Exec>,
    next_exec: u64,
    handles: HashMap<HandleId, Vec<u8>>,
    next_handle: HandleId,
    /// Socket bytes the SSH runner has not accepted yet.
    inbuf: Vec<u8>,
    in_pos: usize,
    /// Auth material, consumed during the handshake.
    keys: Vec<SignKey>,
    /// Signs for any key the agent holds, rather than a file we loaded.
    agent: Option<agent::AgentClient>,
    user: String,
    host: String,
    port: u16,
    authenticated: bool,
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

    fn exec_mut(&mut self, id: u64) -> SshResult<&mut Exec> {
        self.execs
            .get_mut(&id)
            .ok_or_else(|| SshError::op("stale exec channel"))
    }

    // --- transport ---

    /// Pushes everything the SSH runner has queued out to the socket.
    fn flush(&mut self) -> SshResult<()> {
        loop {
            let out = self.ssh.output_buf();
            if out.is_empty() {
                return Ok(());
            }
            let n = self
                .sock
                .write(out)
                .map_err(|e| SshError::fatal(format!("socket write: {e}")))?;
            if n == 0 {
                return Err(SshError::fatal("connection closed while writing"));
            }
            self.ssh.consume_output(n);
        }
    }

    /// Hands the SSH runner as much of the socket as it will take.
    ///
    /// Anything it will not accept yet stays buffered for the next call. The
    /// runner stops accepting while a payload waits to be collected, so this
    /// returns instead of looping: only the caller can drain a channel, and
    /// spinning here would never let it.
    fn feed(&mut self) -> SshResult<()> {
        if self.in_pos == self.inbuf.len() {
            if !self.ssh.is_input_ready() {
                return Ok(());
            }
            let mut buf = [0u8; SOCK_BUF];
            let n = match self.sock.read(&mut buf) {
                Ok(0) => return Err(SshError::fatal("connection closed by peer")),
                Ok(n) => n,
                Err(ref e)
                    if e.kind() == io::ErrorKind::TimedOut
                        || e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::Interrupted =>
                {
                    return Ok(());
                }
                Err(e) => return Err(SshError::fatal(format!("socket read: {e}"))),
            };
            self.inbuf.clear();
            self.inbuf.extend_from_slice(&buf[..n]);
            self.in_pos = 0;
        }
        let took = self
            .ssh
            .input(&self.inbuf[self.in_pos..])
            .map_err(ssh_err)?;
        self.in_pos += took;
        Ok(())
    }

    /// Answers auth and channel events until the runner has nothing to say.
    fn events(&mut self) -> SshResult<()> {
        loop {
            // The event borrows the runner, so anything needing the runner
            // itself is deferred until the borrow ends.
            let mut open_sftp = false;
            let mut idle = false;
            {
                let ev = self.ssh.progress().map_err(ssh_err)?;
                match ev {
                    Event::Cli(CliEvent::Hostkey(h)) => {
                        let key = h.hostkey().map_err(ssh_err)?;
                        match knownhosts::verify(&self.host, self.port, &key) {
                            Ok(()) => h.accept().map_err(ssh_err)?,
                            Err(e) => {
                                let _ = h.reject();
                                return Err(SshError::fatal(e));
                            }
                        }
                    }
                    Event::Cli(CliEvent::Username(u)) => u.username(&self.user).map_err(ssh_err)?,
                    // There is no terminal to prompt on, so skip rather than hang.
                    Event::Cli(CliEvent::Password(p)) => p.skip().map_err(ssh_err)?,
                    Event::Cli(CliEvent::Pubkey(p)) => {
                        let r = match self.keys.pop() {
                            Some(k) => p.pubkey(k),
                            None => p.skip(),
                        };
                        r.map_err(ssh_err)?
                    }
                    Event::Cli(CliEvent::AgentSign(req)) => {
                        let a = self.agent.as_mut().ok_or_else(|| {
                            SshError::fatal("agent signature wanted without an agent")
                        })?;
                        let key = req.key().map_err(ssh_err)?;
                        let msg = req.message().map_err(ssh_err)?;
                        let sig = a.sign_auth(key, &msg).map_err(ssh_err)?;
                        req.signed(&sig).map_err(ssh_err)?;
                    }
                    Event::Cli(CliEvent::Authenticated) => {
                        self.authenticated = true;
                        open_sftp = self.sftp_chan.is_none();
                    }
                    Event::Cli(CliEvent::SessionOpened(mut o)) => {
                        let num = o.channel().0;
                        match self.wanted.remove(&num) {
                            Some(Want::Sftp) => o.subsystem("sftp").map_err(ssh_err)?,
                            Some(Want::Exec(ref cmd)) => o.exec(cmd).map_err(ssh_err)?,
                            None => (),
                        }
                    }
                    Event::Cli(CliEvent::SessionExit(e)) => {
                        // The event says which channel it belongs to, so it
                        // can be attributed with several commands running.
                        let status = match e.exit {
                            sunset::SessionExit::Status(c) => ExitStatus::Code(c),
                            sunset::SessionExit::Signal(ref s) => {
                                ExitStatus::Signal(s.signal.to_string())
                            }
                        };
                        if let Some(x) = self.execs.values_mut().find(|x| x.chan.num() == e.num) {
                            x.exit = Some(status);
                        }
                    }
                    Event::Cli(CliEvent::Banner(_)) => (),
                    Event::Cli(CliEvent::Defunct) => {
                        return Err(SshError::fatal("SSH connection closed"));
                    }
                    Event::Cli(CliEvent::PollAgain) | Event::Progressed => (),
                    Event::None => idle = true,
                    Event::Serv(_) => {
                        return Err(SshError::fatal("server event on a client session"));
                    }
                }
            }
            if open_sftp {
                let ch = self.ssh.open_client_session().map_err(ssh_err)?;
                self.wanted.insert(ch.num().0, Want::Sftp);
                self.sftp_chan = Some(ch);
            }
            if idle {
                return Ok(());
            }
        }
    }

    /// Moves waiting channel data where it belongs: SFTP replies into the SFTP
    /// runner, command output into that command's buffers.
    ///
    /// Nothing else moves it, and the peer stops sending once the window it
    /// has given us is unacknowledged, so this has to run every time round.
    fn drain_channels(&mut self) -> SshResult<()> {
        // Commands first: their buffers always have room, so this cannot stall.
        let ids: Vec<u64> = self.execs.keys().copied().collect();
        for id in ids {
            let mut buf = [0u8; SOCK_BUF];
            loop {
                // Stop once enough is buffered and let the peer's window hold
                // the rest on the channel. Discarding it instead would corrupt
                // a stream the caller is still reading.
                if self
                    .execs
                    .get(&id)
                    .is_some_and(|e| e.out.len() + e.err.len() >= EXEC_HIGH_WATER)
                {
                    break;
                }
                let read = {
                    let Session {
                        ref mut ssh,
                        ref execs,
                        ..
                    } = *self;
                    let Some(e) = execs.get(&id) else { break };
                    ssh.read_channel_either(&e.chan, &mut buf)
                };
                let (n, dt) = match read {
                    Ok(v) => v,
                    Err(sunset::Error::ChannelEOF) => {
                        self.exec_mut(id)?.eof = true;
                        break;
                    }
                    Err(e) => return Err(ssh_err(e)),
                };
                if n == 0 {
                    // Nothing waiting. `read_channel_either` does not report
                    // EOF of its own accord, so ask the channel directly:
                    // without this a finished command never looks finished.
                    let ended = {
                        let Session {
                            ref mut ssh,
                            ref execs,
                            ..
                        } = *self;
                        execs.get(&id).is_none_or(|e| {
                            ssh.is_channel_eof(&e.chan) || ssh.is_channel_closed(&e.chan)
                        })
                    };
                    if ended && let Ok(e) = self.exec_mut(id) {
                        e.eof = true;
                    }
                    break;
                }
                let e = self.exec_mut(id)?;
                let sink = match dt {
                    ChanData::Stderr => &mut e.err,
                    _ => &mut e.out,
                };
                sink.extend_from_slice(&buf[..n]);
            }
            let closed = {
                let Session {
                    ref ssh, ref execs, ..
                } = *self;
                execs
                    .get(&id)
                    .is_some_and(|e| ssh.is_channel_closed(&e.chan))
            };
            if closed && let Ok(e) = self.exec_mut(id) {
                e.closed = true;
            }
        }

        // Then SFTP, filling exactly what the runner asks for next.
        let Some(chan) = self.sftp_chan.take() else {
            return Ok(());
        };
        let r = (|| -> SshResult<()> {
            while !self.sftp.has_event() {
                let dest = self.sftp.want_buf();
                if dest.is_empty() {
                    break;
                }
                let n = match self.ssh.read_channel(&chan, ChanData::Normal, dest) {
                    Ok(n) => n,
                    Err(sunset::Error::ChannelEOF) => {
                        return Err(SshError::fatal("SFTP channel closed"));
                    }
                    Err(e) => return Err(ssh_err(e)),
                };
                if n == 0 {
                    break;
                }
                self.sftp.input_done(n).map_err(sftp_err)?;
            }
            Ok(())
        })();
        self.sftp_chan = Some(chan);
        r
    }

    /// Sends whatever the SFTP runner has queued, with `payload` for a write.
    fn sftp_send(&mut self, payload: &[u8]) -> SshResult<()> {
        let Some(chan) = self.sftp_chan.take() else {
            return Err(SshError::fatal("no SFTP channel"));
        };
        let r = (|| -> SshResult<()> {
            loop {
                let out = self.sftp.output_buf();
                if out.is_empty() {
                    break;
                }
                let n = self
                    .ssh
                    .write_channel(&chan, ChanData::Normal, out)
                    .map_err(ssh_err)?;
                if n == 0 {
                    // The window is full; let the peer catch up.
                    self.flush()?;
                    self.feed()?;
                    self.events()?;
                    continue;
                }
                self.sftp.consume_output(n);
            }
            // A write's payload follows its header, straight from the caller.
            if let Some(len) = self.sftp.send_data() {
                let mut sent = 0;
                while sent < len {
                    let n = self
                        .ssh
                        .write_channel(&chan, ChanData::Normal, &payload[sent..len])
                        .map_err(ssh_err)?;
                    if n == 0 {
                        self.flush()?;
                        self.feed()?;
                        self.events()?;
                        continue;
                    }
                    sent += n;
                }
                self.sftp.data_sent(len);
            }
            self.flush()
        })();
        self.sftp_chan = Some(chan);
        r
    }

    /// Runs the connection until `done` is satisfied.
    fn pump(&mut self, done: impl FnMut(&mut Self) -> SshResult<bool>) -> SshResult<()> {
        self.pump_deadline(None, done)
    }

    fn pump_deadline(
        &mut self,
        deadline: Option<Instant>,
        mut done: impl FnMut(&mut Self) -> SshResult<bool>,
    ) -> SshResult<()> {
        loop {
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Err(SshError::fatal("timed out waiting for the SSH server"));
            }
            self.events()?;
            self.drain_channels()?;
            self.flush()?;
            if done(self)? {
                return Ok(());
            }
            self.feed()?;
        }
    }

    /// Waits for the next SFTP reply.
    fn reply(&mut self) -> SshResult<Reply> {
        self.reply_deadline(None)
    }

    fn reply_deadline(&mut self, deadline: Option<Instant>) -> SshResult<Reply> {
        self.sftp_send(&[])?;
        self.pump_deadline(deadline, |s| Ok(s.sftp.has_event()))?;
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

    /// A reply that should be a single attributes record.
    fn attrs_reply(&mut self, what: &str) -> SshResult<FileAttrs> {
        match self.reply()? {
            Reply::Attrs(a) => Ok(a),
            other => Err(handle_err(what, other)),
        }
    }

    /// A reply that should be a one-entry name list, as realpath and readlink
    /// both answer.
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

    // --- file data ---

    fn read_at(&mut self, handle: HandleId, offset: u64, len: usize) -> SshResult<Vec<u8>> {
        let h = self.handle(handle)?;
        // One request per call: the runner encodes exactly what is asked for,
        // and a server may refuse anything past the protocol's packet size.
        let want = len.min(MAX_READ_LEN as usize) as u32;
        self.sftp.read(&h, offset, want).map_err(sftp_err)?;
        match self.reply()? {
            Reply::Data(n) => {
                let mut out = vec![0u8; n];
                self.take_file_data(&mut out)?;
                Ok(out)
            }
            // End of file, reported as a status rather than an empty read.
            Reply::Status(StatusCode::SSH_FX_EOF) => Ok(Vec::new()),
            Reply::Status(c) => Err(status_err("read", c)),
            other => Err(handle_err("read", other)),
        }
    }

    /// Takes file data straight off the channel, never through the runner.
    fn take_file_data(&mut self, dest: &mut [u8]) -> SshResult<()> {
        let Some(chan) = self.sftp_chan.take() else {
            return Err(SshError::fatal("no SFTP channel"));
        };
        let mut got = 0;
        let r = (|| -> SshResult<()> {
            while got < dest.len() {
                let n = match self
                    .ssh
                    .read_channel(&chan, ChanData::Normal, &mut dest[got..])
                {
                    Ok(n) => n,
                    Err(sunset::Error::ChannelEOF) => {
                        return Err(SshError::fatal("SFTP channel closed mid-transfer"));
                    }
                    Err(e) => return Err(ssh_err(e)),
                };
                if n == 0 {
                    self.events()?;
                    self.flush()?;
                    self.feed()?;
                    continue;
                }
                got += n;
            }
            Ok(())
        })();
        self.sftp_chan = Some(chan);
        r?;
        self.sftp.data_taken(dest.len());
        Ok(())
    }

    fn write_at(&mut self, handle: HandleId, offset: u64, data: &[u8]) -> SshResult<()> {
        let h = self.handle(handle)?;
        let mut sent = 0u64;
        // Split to the largest request a server has to accept; a bigger packet
        // gets the channel closed rather than an error.
        for part in data.chunks(MAX_WRITE_LEN as usize) {
            self.sftp
                .write(&h, offset + sent, part.len())
                .map_err(sftp_err)?;
            self.sftp_send(part)?;
            self.reply()?.into_status("write")?;
            sent += part.len() as u64;
        }
        Ok(())
    }

    // --- exec ---

    fn start_exec(&mut self, cmd: &str) -> SshResult<u64> {
        let ch = self.ssh.open_client_session().map_err(ssh_err)?;
        self.next_exec += 1;
        let id = self.next_exec;
        self.wanted.insert(ch.num().0, Want::Exec(cmd.to_string()));
        self.execs.insert(
            id,
            Exec {
                chan: ch,
                out: Vec::new(),
                err: Vec::new(),
                eof: false,
                closed: false,
                sent_eof: false,
                exit: None,
            },
        );
        // Get the request onto the wire before returning.
        self.events()?;
        self.flush()?;
        Ok(id)
    }

    /// One step of a running command. Returns true once nothing more will arrive.
    ///
    /// Does not wait for output: a stream reader loops this so the session
    /// lock is released between socket waits.
    fn pump_exec(&mut self, id: u64) -> SshResult<bool> {
        self.events()?;
        self.drain_channels()?;
        self.flush()?;
        {
            let e = self.exec_mut(id)?;
            if e.done() {
                return Ok(true);
            }
            if !e.out.is_empty() || !e.err.is_empty() {
                return Ok(false);
            }
        }
        self.feed()?;
        self.events()?;
        self.drain_channels()?;
        Ok(self.exec_mut(id)?.done())
    }

    fn write_exec(&mut self, id: u64, data: &[u8]) -> SshResult<()> {
        let mut sent = 0;
        while sent < data.len() {
            let n = {
                let Session {
                    ref mut ssh,
                    ref execs,
                    ..
                } = *self;
                let e = execs
                    .get(&id)
                    .ok_or_else(|| SshError::op("stale exec channel"))?;
                match ssh.write_channel(&e.chan, ChanData::Normal, &data[sent..]) {
                    Ok(n) => n,
                    Err(sunset::Error::ChannelEOF) => {
                        return Err(SshError::op("command closed its input"));
                    }
                    Err(e) => return Err(ssh_err(e)),
                }
            };
            if n == 0 {
                // The window is full; let the peer catch up.
                self.flush()?;
                self.feed()?;
                self.events()?;
                self.drain_channels()?;
                continue;
            }
            sent += n;
            self.flush()?;
        }
        Ok(())
    }

    fn eof_exec(&mut self, id: u64) -> SshResult<()> {
        let Some(e) = self.execs.get(&id) else {
            return Ok(());
        };
        if e.sent_eof {
            return Ok(());
        }
        let chan = &e.chan;
        self.ssh.send_channel_eof(chan).map_err(ssh_err)?;
        self.exec_mut(id)?.sent_eof = true;
        self.flush()
    }

    fn finish_exec(&mut self, id: u64) {
        if let Some(e) = self.execs.remove(&id) {
            let _ = self.ssh.channel_done(e.chan);
        }
    }
}

/// The local end of a streamed command.
pub struct ExecStream {
    conn: Arc<Conn>,
    id: u64,
    stdin: Stdin,
    eof: bool,
    pending: Vec<u8>,
    pos: usize,
    cancel: Option<Arc<AtomicBool>>,
}

impl ExecStream {
    pub fn with_cancel(mut self, cancel: Arc<AtomicBool>) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Signals end of input, so a command reading its stdin to EOF can finish.
    pub fn finish_input(&mut self) {
        if self.stdin == Stdin::Piped {
            let _ = self.conn.with(|s| s.eof_exec(self.id));
            self.stdin = Stdin::Closed;
        }
    }

    /// Waits for the command to finish, reporting how it went.
    ///
    /// Stdout is discarded: a caller that wants it reads the stream instead.
    pub fn wait(mut self) -> SshResult<ExecOutput> {
        self.finish_input();
        let id = self.id;
        self.conn.with(|s| {
            let mut out = ExecOutput::default();
            loop {
                let done = s.pump_exec(id)?;
                let e = s.exec_mut(id)?;
                e.out.clear();
                out.stderr.append(&mut e.err);
                if done {
                    out.exit = e.exit.clone();
                    break;
                }
            }
            Ok(out)
        })
    }
}

impl io::Read for ExecStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.pos == self.pending.len() {
            if self.eof {
                return Ok(0);
            }
            if self
                .cancel
                .as_ref()
                .is_some_and(|c| c.load(Ordering::Relaxed))
            {
                return Err(io::Error::other("Cancelled"));
            }
            let id = self.id;
            let (chunk, done) = self
                .conn
                .with(|s| {
                    let done = s.pump_exec(id)?;
                    let e = s.exec_mut(id)?;
                    Ok((std::mem::take(&mut e.out), done))
                })
                .map_err(io::Error::other)?;
            self.pending = chunk;
            self.pos = 0;
            self.eof = done;
        }
        let n = (self.pending.len() - self.pos).min(buf.len());
        buf[..n].copy_from_slice(&self.pending[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl io::Write for ExecStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.stdin != Stdin::Piped {
            return Err(io::Error::other("exec stdin is closed"));
        }
        let id = self.id;
        self.conn
            .with(|s| s.write_exec(id, buf))
            .map_err(io::Error::other)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for ExecStream {
    fn drop(&mut self) {
        let id = self.id;
        let _ = self.conn.with(|s| {
            s.finish_exec(id);
            Ok(())
        });
    }
}

/// A remote file opened for reading, presented as a seekable byte stream.
///
/// SFTP reads are positional, so seeking is just bookkeeping.
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

/// Everything needed to open one connection.
pub struct ConnectParams {
    /// Name the user typed, used for known-hosts and display.
    pub host: String,
    /// Address actually dialled, after ssh_config resolution.
    pub hostname: String,
    pub port: u16,
    pub user: String,
    /// Private key files to try, in order.
    pub identity_files: Vec<String>,
    /// Whether to offer keys held by a running ssh-agent.
    pub use_agent: bool,
}

/// Opens a connection, blocking until the SFTP subsystem is ready.
pub fn connect(params: ConnectParams) -> SshResult<Conn> {
    let mut session = open_session(&params)?;

    // SFTP cwd after login is usually the account home. Some servers do not
    // implement realpath, or start in `/`; a shell `pwd` fills those gaps.
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
        host: params.host.clone(),
        home_dir,
        params,
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

/// Dials, authenticates, and gets the SFTP subsystem talking.
///
/// Separate from [`connect()`] so a dropped connection can be rebuilt in
/// place without the caller knowing.
fn open_session(params: &ConnectParams) -> SshResult<Session> {
    let addr = format!("{}:{}", params.hostname, params.port);
    let sock = TcpStream::connect(&addr)
        .map_err(|e| SshError::fatal(format!("TCP connect to {addr}: {e}")))?;
    // SFTP alternates a small request with a large reply. Without this, Nagle
    // holds each request back until the peer's delayed ACK.
    let _ = sock.set_nodelay(true);
    let _ = sock.set_read_timeout(Some(SOCK_TIMEOUT));
    #[cfg(unix)]
    set_keepalive(&sock);

    let mut session = Session {
        sock,
        ssh: Runner::new_client_owned(),
        sftp: SftpRunner::new(),
        sftp_chan: None,
        wanted: HashMap::new(),
        execs: HashMap::new(),
        next_exec: 0,
        handles: HashMap::new(),
        next_handle: 0,
        inbuf: Vec::new(),
        in_pos: 0,
        keys: load_identities(&params.identity_files),
        agent: None,
        user: params.user.clone(),
        host: params.host.clone(),
        port: params.port,
        authenticated: false,
    };

    // Agent keys are offered first, matching ssh(1).
    if params.use_agent {
        session.agent = load_agent(&mut session.keys);
    }

    // Handshake, authentication, and the subsystem request.
    session.pump(|s| Ok(s.sftp_chan.is_some() && s.wanted.is_empty()))?;
    if !session.authenticated {
        return Err(SshError::fatal(format!(
            "Authentication failed for {}@{addr}. Ensure a usable key is \
             available in ~/.ssh or given by IdentityFile.",
            session.user
        )));
    }

    session.sftp.init().map_err(sftp_err)?;
    match session.reply_deadline(Some(Instant::now() + Duration::from_secs(5))) {
        Ok(Reply::Version) => (),
        Ok(other) => return Err(handle_err("SFTP handshake", other)),
        Err(e) => {
            return Err(SshError::fatal(format!(
                "SFTP is not available on {addr} ({})",
                e.message
            )));
        }
    }
    Ok(session)
}

/// Keeps the OS probing, so a connection dropped by a sleep or a network
/// change is noticed rather than hanging on a read that never returns.
#[cfg(unix)]
fn set_keepalive(sock: &TcpStream) {
    use std::os::fd::AsRawFd as _;
    let fd = sock.as_raw_fd();
    let set = |level: libc::c_int, name: libc::c_int, val: libc::c_int| unsafe {
        libc::setsockopt(
            fd,
            level,
            name,
            &val as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    };
    set(libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1);
    #[cfg(target_os = "macos")]
    const KEEPIDLE: libc::c_int = libc::TCP_KEEPALIVE;
    #[cfg(not(target_os = "macos"))]
    const KEEPIDLE: libc::c_int = libc::TCP_KEEPIDLE;
    // Probe after 15s idle, every 5s, giving up after 3 failures.
    set(libc::IPPROTO_TCP, KEEPIDLE, 15);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, 5);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPCNT, 3);
}

/// Connects to the running agent and appends the keys it holds.
fn load_agent(keys: &mut Vec<SignKey>) -> Option<agent::AgentClient> {
    let sock = agent::address()?;
    let mut a = match agent::AgentClient::new(&sock) {
        Ok(a) => a,
        Err(e) => {
            log::warn!("opening ssh-agent at {sock}: {e}");
            return None;
        }
    };
    match a.keys() {
        // Keys are offered by popping from the end, so the agent's go last.
        Ok(ks) => {
            keys.extend(ks);
            Some(a)
        }
        Err(e) => {
            log::warn!("listing ssh-agent keys: {e}");
            None
        }
    }
}

fn load_identities(paths: &[String]) -> Vec<SignKey> {
    let mut keys = Vec::new();
    for p in paths {
        let Ok(bytes) = std::fs::read(p) else {
            continue;
        };
        match SignKey::from_openssh(bytes) {
            Ok(k) => keys.push(k),
            Err(e) => log::warn!("skipping key {p}: {e}"),
        }
    }
    // Keys are offered by popping from the end, so restore the caller's order.
    keys.reverse();
    keys
}
