//! One shared SSH connection per saved machine.
//!
//! Every remote call (status polls, presence marks, starting a stream) used to
//! start its own `ssh` or `herdr --machine`, and each paid the full cost of
//! reading the user's SSH config and connecting, seconds per call when the
//! config probes the network (`Match exec`). Instead, the first call starts an
//! SSH control master with the user's config, and every call after that
//! attaches to it with `-F /dev/null`, which skips the config entirely and
//! takes milliseconds. All remuda processes on this machine share the master;
//! it stays up while mirrors stream through it and for 10 minutes after.
//!
//! When the master cannot be started, callers fall back to their old path, so
//! a broken shared connection only costs speed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::herdr::Machine;

/// How long the master outlives its last session.
const PERSIST: &str = "600";
/// After a failed start, how long this process uses the fallback path before
/// trying to start the master again.
const RETRY_AFTER: Duration = Duration::from_secs(30);

pub struct Remote {
    target: String,
    session: Option<String>,
    socket: PathBuf,
}

impl Remote {
    pub fn new(machine: &Machine) -> Remote {
        let session = machine
            .session
            .clone()
            .filter(|session| session != "default");
        Remote {
            socket: socket_path(&machine.target, session.as_deref()),
            target: machine.target.clone(),
            session,
        }
    }

    /// An `ssh` command running `herdr <args>` on the machine: through the
    /// shared connection when it is up (starting it if needed), otherwise a
    /// plain `ssh` with the user's config.
    pub fn herdr_command(&self, args: &[&str]) -> Command {
        self.command_via(args, self.connection_ready())
    }

    /// Like `herdr_command`, for a command that runs until it is stopped and
    /// ignores its input (an observe stream). It gets a terminal on the
    /// machine, in raw mode so its output passes unchanged, so that closing
    /// the connection hangs it up. Without one it would outlive the
    /// connection and keep holding a session of the shared connection until
    /// it next wrote, which for a quiet pane is never.
    pub fn herdr_stream_command(&self, args: &[&str]) -> Command {
        let script = format!(
            "stty raw -echo 2>/dev/null; exec {}",
            self.remote_script(args)
        );
        self.script_via(&script, self.connection_ready(), true)
    }

    /// An `ssh` command running a shell `script` on the machine, the same way
    /// as `herdr_command`.
    pub fn shell_command(&self, script: &str) -> Command {
        self.script_via(script, self.connection_ready(), false)
    }

    fn command_via(&self, args: &[&str], shared: bool) -> Command {
        self.script_via(&format!("exec {}", self.remote_script(args)), shared, false)
    }

    /// `tty` forces a terminal on the machine (`-tt`) instead of none (`-T`).
    fn script_via(&self, script: &str, shared: bool, tty: bool) -> Command {
        let mut command = Command::new("ssh");
        if shared {
            command.args(["-F", "/dev/null", "-o", "ControlMaster=no", "-o"]);
            command.arg(format!("ControlPath={}", self.socket.display()));
            // When the master refuses a session (sshd's MaxSessions), ssh falls
            // back to a direct connection that cannot work without the user's
            // config; fail it fast instead of waiting out the TCP timeout.
            command.args(["-o", "ConnectTimeout=5"]);
        } else {
            command.args(["-o", "ServerAliveInterval=15"]);
        }
        let tty = if tty { "-tt" } else { "-T" };
        command.args(["-o", "BatchMode=yes", tty, &self.target]);
        command.arg(script);
        command
    }

    /// Whether the shared connection is up through `herdr_command`'s fast path.
    pub fn connection_ready(&self) -> bool {
        if self.master_alive() {
            return true;
        }
        if recently_failed(&self.socket) {
            return false;
        }
        let started = self.start_master();
        if !started {
            note_failure(&self.socket);
        }
        started
    }

    fn remote_script(&self, args: &[&str]) -> String {
        let herdr = std::env::var("REMUDA_REMOTE_HERDR").unwrap_or_else(|_| "herdr".into());
        let mut words = vec![herdr];
        if let Some(session) = &self.session {
            words.extend(["--session".to_string(), session.clone()]);
        }
        words.extend(args.iter().map(|arg| arg.to_string()));
        words
            .iter()
            .map(|word| shell_quote(word))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn master_alive(&self) -> bool {
        self.socket.exists()
            && Command::new("ssh")
                .args(["-F", "/dev/null", "-o"])
                .arg(format!("ControlPath={}", self.socket.display()))
                .args(["-O", "check", &self.target])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
    }

    /// Starts the master with the user's SSH config. Mirrors start together
    /// after a restart, so starts are serialised with a lock and each re-checks
    /// whether another process already started it.
    fn start_master(&self) -> bool {
        let Some(_lock) = Lock::acquire(&self.socket.with_extension("lock")) else {
            return false;
        };
        if self.master_alive() {
            return true;
        }
        let _ = std::fs::remove_file(&self.socket);
        Command::new("ssh")
            .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"])
            .args(["-o", "ControlMaster=yes", "-o"])
            .arg(format!("ControlPath={}", self.socket.display()))
            .args(["-o", &format!("ControlPersist={PERSIST}")])
            .args([
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=4",
            ])
            .args(["-N", "-f", "-T", &self.target])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
            && self.master_alive()
    }
}

/// `/tmp/remuda-<uid>/<hash>`: control sockets must fit in 104 bytes, which
/// rules out the plugin state directory.
fn socket_path(target: &str, session: Option<&str>) -> PathBuf {
    let key = format!("{target}\n{}", session.unwrap_or("default"));
    socket_dir().join(format!("{:016x}", fnv1a(key.as_bytes())))
}

/// `/tmp/remuda-<uid>`, as herdr keeps its own SSH sockets in `/tmp/hssh-<uid>`.
fn socket_dir() -> PathBuf {
    let base = if cfg!(unix) {
        PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    };
    let dir = base.join(format!("remuda-{}", uid()));
    if !dir.is_dir() {
        let _ = std::fs::create_dir_all(&dir);
        restrict(&dir);
    }
    dir
}

#[cfg(unix)]
fn uid() -> u32 {
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn uid() -> u32 {
    0
}

#[cfg(unix)]
fn restrict(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn restrict(_dir: &std::path::Path) {}

/// A stable 64-bit hash, so every remuda process names a machine's socket the same.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

fn failures() -> &'static Mutex<HashMap<PathBuf, Instant>> {
    static FAILURES: OnceLock<Mutex<HashMap<PathBuf, Instant>>> = OnceLock::new();
    FAILURES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn recently_failed(socket: &Path) -> bool {
    failures()
        .lock()
        .ok()
        .and_then(|failures| failures.get(socket).copied())
        .is_some_and(|at| at.elapsed() < RETRY_AFTER)
}

fn note_failure(socket: &Path) {
    if let Ok(mut failures) = failures().lock() {
        failures.insert(socket.to_path_buf(), Instant::now());
    }
}

/// An exclusive `flock` held until dropped.
struct Lock(#[allow(dead_code)] std::fs::File);

impl Lock {
    #[cfg(unix)]
    fn acquire(path: &std::path::Path) -> Option<Lock> {
        use std::os::fd::AsRawFd;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .ok()?;
        let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0;
        locked.then_some(Lock(file))
    }

    #[cfg(not(unix))]
    fn acquire(path: &std::path::Path) -> Option<Lock> {
        std::fs::File::create(path).ok().map(Lock)
    }
}

pub fn shell_quote(arg: &str) -> String {
    let safe = !arg.is_empty()
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./:=@%+,".contains(&b));
    if safe {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(session: Option<&str>) -> Machine {
        Machine {
            id: "id".into(),
            label: "ziggy".into(),
            target: "ziggy".into(),
            session: session.map(str::to_string),
            enabled: true,
        }
    }

    #[test]
    fn sockets_are_short_stable_and_per_session() {
        let default = Remote::new(&machine(None)).socket;
        assert_eq!(default, Remote::new(&machine(Some("default"))).socket);
        assert_ne!(default, Remote::new(&machine(Some("agents"))).socket);
        assert!(default.as_os_str().len() < 104);
    }

    #[test]
    fn remote_script_quotes_and_adds_the_session() {
        let remote = Remote::new(&machine(Some("agents")));
        assert_eq!(
            remote.remote_script(&["pane", "rename", "w1:p1", "it's mine"]),
            r"herdr --session agents pane rename w1:p1 'it'\''s mine'"
        );
    }

    #[test]
    fn shell_quote_leaves_plain_words_alone_and_wraps_the_rest() {
        assert_eq!(shell_quote("w1:p3"), "w1:p3");
        assert_eq!(shell_quote("--cols"), "--cols");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    fn args_of(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn shared_commands_skip_the_user_config() {
        let remote = Remote::new(&machine(None));
        let args = args_of(&remote.command_via(&["pane", "list"], true));
        assert_eq!(&args[..2], ["-F", "/dev/null"]);
        assert!(args.iter().any(|arg| arg.starts_with("ControlPath=/")));
        assert_eq!(args.last().unwrap(), "exec herdr pane list");
    }

    #[test]
    fn streams_get_a_raw_terminal_so_closing_hangs_them_up() {
        let remote = Remote::new(&machine(None));
        let args = args_of(&remote.script_via("x", true, true));
        assert!(args.iter().any(|arg| arg == "-tt"));
        assert!(!args.iter().any(|arg| arg == "-T"));
        let plain = args_of(&remote.command_via(&["pane", "list"], true));
        assert!(plain.iter().any(|arg| arg == "-T"));
    }

    #[test]
    fn fallback_commands_use_the_user_config() {
        let remote = Remote::new(&machine(Some("agents")));
        let args = args_of(&remote.command_via(&["pane", "list"], false));
        assert!(!args.iter().any(|arg| arg == "/dev/null"));
        assert_eq!(
            args.last().unwrap(),
            "exec herdr --session agents pane list"
        );
    }
}
