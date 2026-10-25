//! Marks the source pane as mirrored, so the machine it runs on can see who is
//! watching or driving it.
//!
//! The mark is pane metadata on the source, written through `herdr --machine`
//! like everything else remuda does remotely, so nothing runs on the source
//! machine. While the source runs an agent, its sidebar name becomes e.g.
//! `claude ⇄ travis-air (control)`; every source also gets `remuda_watcher`
//! and `remuda_watch_mode` tokens for custom sidebar rows. The mark carries a
//! TTL and is refreshed while the mirror lives, so a mirror that dies without
//! clearing it only leaves it behind briefly.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::herdr::{self, MARK};
use crate::spec::Mode;

/// How long a mark outlives its last refresh.
pub const TTL: Duration = Duration::from_secs(90);
/// How often a living mirror refreshes its mark.
pub const REFRESH: Duration = Duration::from_secs(30);

enum Message {
    Show { agent: Option<String> },
    Clear,
}

pub struct Presence {
    sender: Sender<Message>,
    worker: Option<JoinHandle<()>>,
}

/// Who is marking the source, and how.
struct Watcher {
    /// Metadata source id, unique to this mirror.
    id: String,
    source_pane: String,
    machine: Option<String>,
    name: String,
    mode: Mode,
}

impl Presence {
    pub fn start(source_pane: &str, machine: Option<&str>, name: &str, mode: Mode) -> Presence {
        let mirror_pane = std::env::var("HERDR_PANE_ID").ok();
        let watcher = Watcher {
            id: source_id(name, mirror_pane.as_deref()),
            source_pane: source_pane.to_string(),
            machine: machine.map(str::to_string),
            name: name.to_string(),
            mode,
        };
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || work(&watcher, &receiver));
        Presence {
            sender,
            worker: Some(worker),
        }
    }

    /// Marks (or re-marks) the source. `agent` is the source's current agent.
    pub fn show(&self, agent: Option<String>) {
        let _ = self.sender.send(Message::Show { agent });
    }

    /// Removes the mark and waits for it.
    pub fn clear(mut self) {
        let _ = self.sender.send(Message::Clear);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn work(watcher: &Watcher, receiver: &Receiver<Message>) {
    let mut last_agent = None;
    let mut pending = None;
    loop {
        let mut message = match pending.take() {
            Some(message) => message,
            None => match receiver.recv() {
                Ok(message) => message,
                Err(_) => return,
            },
        };
        while let Ok(newer) = receiver.try_recv() {
            message = newer;
        }
        match message {
            Message::Show { agent } => {
                let args = show_args(watcher, agent.as_deref());
                last_agent = agent;
                // A new mirror's first write competes with its other startup
                // calls to the same machine and can fail; retry it rather than
                // leaving the source unmarked until the next refresh.
                for _ in 1..SHOW_ATTEMPTS {
                    if run(watcher, &args) {
                        break;
                    }
                    match receiver.recv_timeout(SHOW_RETRY) {
                        Ok(newer) => {
                            pending = Some(newer);
                            break;
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
            }
            Message::Clear => {
                run(watcher, &clear_args(watcher, last_agent.as_deref()));
                return;
            }
        }
    }
}

const SHOW_ATTEMPTS: u32 = 5;
const SHOW_RETRY: Duration = Duration::from_secs(2);

fn run(watcher: &Watcher, args: &[String]) -> bool {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    herdr::run_on(watcher.machine.as_deref(), &args).is_ok()
}

/// Metadata source id for one mirror: the machine name plus the mirror's own
/// pane, so two mirrors of the same source (from one machine or several) keep
/// separate marks, and closing one leaves the other's mark alone.
fn source_id(name: &str, mirror_pane: Option<&str>) -> String {
    let safe = |text: &str, max: usize| -> String {
        text.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == ':' {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .take(max)
            .collect()
    };
    match mirror_pane {
        Some(pane) => format!("remuda-watch-{}-{}", safe(name, 40), safe(pane, 24)),
        None => format!("remuda-watch-{}", safe(name, 60)),
    }
}

fn mode_word(mode: Mode) -> &'static str {
    match mode {
        Mode::View => "watching",
        Mode::Control => "control",
    }
}

fn show_args(watcher: &Watcher, agent: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "pane".to_string(),
        "report-metadata".into(),
        watcher.source_pane.clone(),
        "--source".into(),
        watcher.id.clone(),
    ];
    // The display name only shows for agents; the agent guard keeps it from
    // outliving the agent it describes.
    if let Some(agent) = agent {
        args.extend([
            "--agent".into(),
            agent.into(),
            "--display-agent".into(),
            format!(
                "{agent} {MARK} {} ({})",
                watcher.name,
                mode_word(watcher.mode)
            ),
        ]);
    }
    args.extend([
        "--token".into(),
        format!("remuda_watcher={}", watcher.name),
        "--token".into(),
        format!("remuda_watch_mode={}", mode_word(watcher.mode)),
        "--ttl-ms".into(),
        TTL.as_millis().to_string(),
    ]);
    args
}

fn clear_args(watcher: &Watcher, agent: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "pane".to_string(),
        "report-metadata".into(),
        watcher.source_pane.clone(),
        "--source".into(),
        watcher.id.clone(),
    ];
    if let Some(agent) = agent {
        args.extend(["--agent".into(), agent.into()]);
    }
    args.extend([
        "--clear-display-agent".into(),
        "--clear-token".into(),
        "remuda_watcher".into(),
        "--clear-token".into(),
        "remuda_watch_mode".into(),
    ]);
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watcher(mode: Mode) -> Watcher {
        Watcher {
            id: source_id("Travis Air", Some("wA:p6Q")),
            source_pane: "w9:p1".into(),
            machine: Some("ziggy".into()),
            name: "Travis Air".into(),
            mode,
        }
    }

    #[test]
    fn agent_sources_get_a_display_name_guarded_by_the_agent() {
        let args = show_args(&watcher(Mode::Control), Some("claude"));
        let joined = args.join(" ");
        assert!(joined.contains("--source remuda-watch-travis-air-wa:p6q"));
        assert!(
            joined.contains("--agent claude --display-agent claude \u{21c4} Travis Air (control)")
        );
        assert!(joined.contains("--ttl-ms 90000"));
    }

    #[test]
    fn shell_sources_only_get_tokens() {
        let args = show_args(&watcher(Mode::View), None);
        assert!(!args.iter().any(|arg| arg == "--display-agent"));
        assert!(args.iter().any(|arg| arg == "remuda_watch_mode=watching"));
    }

    #[test]
    fn clearing_removes_the_name_and_tokens() {
        let args = clear_args(&watcher(Mode::View), Some("claude")).join(" ");
        assert!(args.contains("--agent claude --clear-display-agent"));
        assert!(args.contains("--clear-token remuda_watcher --clear-token remuda_watch_mode"));
    }

    #[test]
    fn each_mirror_gets_its_own_source_id() {
        assert_ne!(
            source_id("air", Some("wA:p1")),
            source_id("air", Some("wA:p2"))
        );
        assert!(source_id(&"x".repeat(200), Some(&"p".repeat(200))).len() <= 80);
    }
}
