//! Reports a mirror pane to herdr as an agent.
//!
//! The report gives the mirror the source's state in the sidebar, a display
//! name such as `⇄ claude@ziggy`, the `remuda_*` tokens that saved views read,
//! and a resume command so herdr reopens the mirror after a server restart.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::herdr;
use crate::spec::MirrorSpec;

const SOURCE: &str = "remuda";
const AGENT: &str = "remuda";

/// A metadata token to set, or to clear when `None`.
pub type Token = (&'static str, Option<String>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// `idle`, `working` or `blocked`.
    pub state: &'static str,
    pub display: String,
    pub tokens: Vec<Token>,
    pub resume: Option<Vec<String>>,
}

/// Maps a source pane's `agent_status` to a state herdr accepts in a report.
pub fn report_state(agent_status: &str) -> &'static str {
    match agent_status {
        "working" => "working",
        "blocked" => "blocked",
        _ => "idle",
    }
}

enum Message {
    Report(Box<Report>),
    Release,
}

pub struct Reporter {
    sender: Sender<Message>,
    worker: Option<JoinHandle<()>>,
    last: Option<Report>,
}

impl Reporter {
    /// Starts reporting for this pane, or returns `None` outside herdr.
    pub fn start() -> Option<Reporter> {
        let pane = std::env::var("HERDR_PANE_ID").ok()?;
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || work(&pane, &receiver));
        Some(Reporter {
            sender,
            worker: Some(worker),
            last: None,
        })
    }

    /// Queues a report unless it repeats the previous one.
    pub fn report(&mut self, report: Report) {
        if self.last.as_ref() == Some(&report) {
            return;
        }
        self.last = Some(report.clone());
        let _ = self.sender.send(Message::Report(Box::new(report)));
    }

    /// Releases the pane and waits for it, for when the user quits the mirror.
    ///
    /// Not used when the pane closes under us (server shutdown), so herdr keeps
    /// the resume command.
    pub fn release(mut self) {
        let _ = self.sender.send(Message::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn work(pane: &str, receiver: &Receiver<Message>) {
    let mut seq = Seq::default();
    while let Ok(mut message) = receiver.recv() {
        // Send only the latest state; older queued reports are stale.
        while let Ok(newer) = receiver.try_recv() {
            message = newer;
        }
        match message {
            Message::Report(report) => send_report(pane, &report, &mut seq),
            Message::Release => {
                send_release(pane, &mut seq);
                return;
            }
        }
    }
}

fn send_report(pane: &str, report: &Report, seq: &mut Seq) {
    let seq_agent = seq.next().to_string();
    let mut args = vec![
        "pane",
        "report-agent",
        pane,
        "--source",
        SOURCE,
        "--agent",
        AGENT,
        "--state",
        report.state,
        "--seq",
        &seq_agent,
    ];
    if let Some(resume) = &report.resume {
        args.push("--");
        args.extend(resume.iter().map(String::as_str));
    }
    herdr::run_quietly(&args);

    let seq_metadata = seq.next().to_string();
    let mut args: Vec<String> = [
        "pane",
        "report-metadata",
        pane,
        "--source",
        SOURCE,
        "--agent",
        AGENT,
        "--display-agent",
        &report.display,
        "--seq",
        &seq_metadata,
    ]
    .map(str::to_string)
    .into();
    args.extend(token_args(&report.tokens));
    herdr::run_quietly(&args.iter().map(String::as_str).collect::<Vec<_>>());
}

fn token_args(tokens: &[Token]) -> Vec<String> {
    tokens
        .iter()
        .flat_map(|(key, value)| match value {
            Some(value) => ["--token".to_string(), format!("{key}={value}")],
            None => ["--clear-token".to_string(), (*key).to_string()],
        })
        .collect()
}

fn send_release(pane: &str, seq: &mut Seq) {
    let seq_metadata = seq.next().to_string();
    let mut args: Vec<String> = [
        "pane",
        "report-metadata",
        pane,
        "--source",
        SOURCE,
        "--clear-display-agent",
        "--seq",
        &seq_metadata,
    ]
    .map(str::to_string)
    .into();
    let cleared: Vec<Token> = MirrorSpec::default()
        .to_tokens()
        .into_iter()
        .map(|(key, _)| (key, None))
        .collect();
    args.extend(token_args(&cleared));
    herdr::run_quietly(&args.iter().map(String::as_str).collect::<Vec<_>>());
    let seq_agent = seq.next().to_string();
    herdr::run_quietly(&[
        "pane",
        "release-agent",
        pane,
        "--source",
        SOURCE,
        "--agent",
        AGENT,
        "--seq",
        &seq_agent,
    ]);
}

/// Report sequence numbers: milliseconds since the epoch, kept increasing so
/// they also stay ahead of reports from an earlier run of this mirror.
#[derive(Default)]
struct Seq(u64);

impl Seq {
    fn next(&mut self) -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or_default();
        self.0 = now.max(self.0 + 1);
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_map_to_reportable_values() {
        assert_eq!(report_state("working"), "working");
        assert_eq!(report_state("blocked"), "blocked");
        for other in ["idle", "done", "unknown", ""] {
            assert_eq!(report_state(other), "idle");
        }
    }

    #[test]
    fn seq_always_increases() {
        let mut seq = Seq(u64::MAX - 10);
        let first = seq.next();
        assert!(seq.next() > first);
    }
}
