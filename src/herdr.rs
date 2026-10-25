//! Access to herdr through its CLI, locally or on a saved SSH machine.

use std::process::{Command, Stdio};

use serde::Deserialize;
use serde_json::Value;

pub const MARK: &str = "\u{21c4}"; // ⇄

pub fn bin() -> String {
    std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into())
}

pub fn plugin_id() -> String {
    std::env::var("HERDR_PLUGIN_ID").unwrap_or_else(|_| "remuda".into())
}

/// Runs a herdr CLI command and returns its `result` object.
pub fn json(args: &[&str], machine: Option<&str>) -> Result<Value, String> {
    let mut command = Command::new(bin());
    if let Some(machine) = machine {
        command.args(["--machine", machine]);
    }
    let output = command
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|err| format!("could not run herdr: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let message = if stderr.trim().is_empty() {
            stdout
        } else {
            stderr
        };
        return Err(format!("herdr {}: {}", args.join(" "), message.trim()));
    }
    let mut value: Value = serde_json::from_slice(&output.stdout)
        .map_err(|err| format!("herdr {}: invalid JSON: {err}", args.join(" ")))?;
    if let Some(error) = value.get("error") {
        let message = error["message"].as_str().unwrap_or("unknown error");
        return Err(format!("herdr {}: {message}", args.join(" ")));
    }
    Ok(match value.get_mut("result") {
        Some(result) => result.take(),
        None => value,
    })
}

/// Sends one request over herdr's API socket, for methods the CLI lacks
/// (`layout.export`, `layout.apply`).
#[cfg(unix)]
pub fn socket_request(method: &str, params: Value) -> Result<Value, String> {
    use std::io::{BufRead, BufReader, Write};

    let path = std::env::var("HERDR_SOCKET_PATH")
        .map_err(|_| "HERDR_SOCKET_PATH is not set; run this inside herdr".to_string())?;
    let mut stream = std::os::unix::net::UnixStream::connect(&path)
        .map_err(|err| format!("connect to herdr at {path}: {err}"))?;
    let request = serde_json::json!({"id": "remuda", "method": method, "params": params});
    writeln!(stream, "{request}").map_err(|err| format!("{method}: {err}"))?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|err| format!("{method}: {err}"))?;
    let mut response: Value =
        serde_json::from_str(&line).map_err(|err| format!("{method}: invalid response: {err}"))?;
    if let Some(error) = response.get("error") {
        let message = error["message"].as_str().unwrap_or("unknown error");
        return Err(format!("{method}: {message}"));
    }
    Ok(response["result"].take())
}

#[cfg(not(unix))]
pub fn socket_request(method: &str, _params: Value) -> Result<Value, String> {
    Err(format!(
        "{method}: the herdr socket is only supported on unix"
    ))
}

/// Runs a herdr CLI command for its side effect, ignoring the result.
pub fn run_quietly(args: &[&str]) {
    run_quietly_on(None, args);
}

/// Like `run_quietly`, on a saved machine when `machine` is set.
pub fn run_quietly_on(machine: Option<&str>, args: &[&str]) {
    let _ = run_on(machine, args);
}

/// Runs a herdr CLI command that prints nothing on success (such as the
/// `report-*` commands), on a saved machine when `machine` is set.
pub fn run_on(machine: Option<&str>, args: &[&str]) -> Result<(), String> {
    let mut command = Command::new(bin());
    if let Some(machine) = machine {
        command.args(["--machine", machine]);
    }
    let output = command
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|err| format!("could not run herdr: {err}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if output.status.success() && !stdout.contains("\"error\"") {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let message = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    Err(format!("herdr {}: {}", args.join(" "), message.trim()))
}

#[derive(Debug, Clone, Deserialize)]
pub struct Machine {
    pub id: String,
    pub label: String,
    pub target: String,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub enabled: bool,
}

/// Saved SSH machines that are enabled. An unreadable catalog means no machines.
pub fn machines() -> Vec<Machine> {
    let Ok(output) = Command::new(bin())
        .args(["machine", "list", "--json"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    serde_json::from_slice::<Vec<Machine>>(&output.stdout)
        .map(|machines| machines.into_iter().filter(|m| m.enabled).collect())
        .unwrap_or_default()
}

pub fn machine_by_label(label: &str) -> Option<Machine> {
    machines()
        .into_iter()
        .find(|machine| machine.label == label || machine.id == label)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Observe,
    Control,
}

/// Builds the command for a `terminal session` stream.
///
/// `--machine` does not forward interactive streams, so remote streams run the
/// remote herdr over plain ssh against the machine's saved target and session.
pub fn stream_command(
    kind: StreamKind,
    source: &str,
    cols: u16,
    rows: u16,
    machine: Option<&Machine>,
    takeover: bool,
) -> Command {
    let mode = match kind {
        StreamKind::Observe => "observe",
        StreamKind::Control => "control",
    };
    let mut args: Vec<String> = vec![
        "terminal".into(),
        "session".into(),
        mode.into(),
        source.into(),
        "--cols".into(),
        cols.to_string(),
        "--rows".into(),
        rows.to_string(),
    ];
    // Flags go after the target: herdr rejects them before it.
    if takeover {
        args.push("--takeover".into());
    }
    match machine {
        None => {
            let mut command = Command::new(bin());
            command.args(args);
            command
        }
        Some(machine) => {
            let remote_herdr =
                std::env::var("REMUDA_REMOTE_HERDR").unwrap_or_else(|_| "herdr".into());
            let mut remote = vec![remote_herdr];
            if let Some(session) = machine.session.as_deref().filter(|s| *s != "default") {
                remote.push("--session".into());
                remote.push(session.into());
            }
            remote.extend(args);
            let script = remote
                .iter()
                .map(|arg| shell_quote(arg))
                .collect::<Vec<_>>()
                .join(" ");
            let mut command = Command::new("ssh");
            command.args([
                "-T",
                "-o",
                "ServerAliveInterval=15",
                &machine.target,
                &format!("exec {script}"),
            ]);
            command
        }
    }
}

fn shell_quote(arg: &str) -> String {
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

    #[test]
    fn shell_quote_leaves_plain_words_alone() {
        assert_eq!(shell_quote("w1:p3"), "w1:p3");
        assert_eq!(shell_quote("--cols"), "--cols");
    }

    #[test]
    fn shell_quote_wraps_spaces_and_quotes() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn remote_stream_runs_remote_herdr_over_ssh_with_session() {
        let machine = Machine {
            id: "id".into(),
            label: "ziggy".into(),
            target: "ziggy".into(),
            session: Some("agents".into()),
            enabled: true,
        };
        let command = stream_command(StreamKind::Control, "w1:p3", 80, 24, Some(&machine), true);
        let args: Vec<_> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(command.get_program(), "ssh");
        assert_eq!(args[3], "ziggy");
        assert!(args[4].starts_with("exec herdr --session agents terminal session control w1:p3"));
        assert!(args[4].ends_with("--rows 24 --takeover"));
    }
}
