//! `remuda send`: a message to an agent on this or a saved machine.
//!
//! The message is typed into the agent once it is idle, after a
//! `[from <sender>]` prefix so the agent can tell it from its user's own words.
//! It goes through `herdr agent prompt`, which handles bracketed paste and
//! refuses a blocked agent; agents a plugin reports (such as auggie), which
//! `agent prompt` refuses, get the text typed into their pane. Every message
//! is appended to `messages.jsonl` in the plugin state directory.
//!
//! `@<herd>@<machine>` sends to every agent in a herd at once.

use std::io::{Read, Write};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::config::Config;
use crate::herdr;
use crate::herds;
use crate::views;

const USAGE: &str =
    "usage: remuda send <agent>[@machine] | @<herd>@<machine> [--from NAME] [--wait SECONDS] [--] [TEXT...]";
const POLL: Duration = Duration::from_secs(1);
/// Polls in a row the agent must be ready before it is typed into, so a
/// moment of idle between two steps of a turn is not taken for the end.
const READY_POLLS: u32 = 2;
const DEFAULT_WAIT: u64 = 600;

#[derive(Debug, PartialEq, Eq)]
struct Options {
    /// A live agent name, a pane id, or `@<herd>`.
    agent: String,
    /// `None` for this machine.
    machine: Option<String>,
    from: Option<String>,
    wait: Duration,
    text: Option<String>,
}

impl Options {
    fn parse(args: &[String]) -> Result<Options, String> {
        let mut target = None;
        let mut from = None;
        let mut wait = Duration::from_secs(DEFAULT_WAIT);
        let mut words: Vec<String> = Vec::new();
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--" => {
                    words.extend(args.by_ref().cloned());
                    break;
                }
                "--from" => from = Some(args.next().ok_or("--from needs a name")?.clone()),
                "--wait" => {
                    let secs = args.next().ok_or("--wait needs seconds")?;
                    let secs: u64 = secs
                        .parse()
                        .map_err(|_| format!("--wait needs seconds, not {secs}"))?;
                    wait = Duration::from_secs(secs);
                }
                flag if flag.starts_with("--") => {
                    return Err(format!("unknown send option {flag}\n{USAGE}"))
                }
                _ if target.is_none() => target = Some(arg.clone()),
                _ => words.push(arg.clone()),
            }
        }
        let target = target.ok_or_else(|| format!("no agent to send to\n{USAGE}"))?;
        let (agent, machine) = match target.strip_prefix('@') {
            Some(herd) => {
                let (name, machine) = herds::parse_ref(herd)?;
                (format!("@{name}"), Some(machine))
            }
            None => parse_target(&target)?,
        };
        let text = (!words.is_empty()).then(|| words.join(" "));
        Ok(Options {
            agent,
            machine,
            from,
            wait,
            text,
        })
    }
}

/// Splits `agent@machine`; `local`, or no machine, is this machine.
fn parse_target(target: &str) -> Result<(String, Option<String>), String> {
    let (agent, machine) = match target.rsplit_once('@') {
        Some((agent, machine)) => (agent, Some(machine)),
        None => (target, None),
    };
    if agent.is_empty() || machine.is_some_and(str::is_empty) {
        return Err(format!("{target:?} is not <agent>[@machine]"));
    }
    let machine = machine.filter(|m| *m != "local").map(str::to_string);
    Ok((agent.to_string(), machine))
}

pub fn run(args: &[String]) -> Result<(), String> {
    let options = Options::parse(args)?;
    let text = match &options.text {
        Some(text) => text.clone(),
        None => {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .map_err(|err| format!("read the message from stdin: {err}"))?;
            text
        }
    };
    let text = text.trim();
    if text.is_empty() {
        return Err("the message is empty".into());
    }
    if let Some(machine) = &options.machine {
        herdr::machine_by_label(machine)
            .ok_or_else(|| format!("no enabled saved machine named {machine}"))?;
    }
    let from = options.from.clone().unwrap_or_else(default_sender);
    let message = format!("{} {text}", prefix(&from));
    let machine = options.machine.as_deref();
    let at = |name: &str| match machine {
        Some(machine) => format!("{name}@{machine}"),
        None => name.to_string(),
    };

    // (who, agent target) for each recipient: one agent, or a herd's agents.
    let recipients: Vec<(String, String)> = match options.agent.strip_prefix('@') {
        Some(name) => {
            let machine = machine.unwrap_or_default();
            let herd = herds::find(name, machine)
                .ok_or_else(|| format!("no herd {name}@{machine} here"))?;
            let listed = herdr::json(&["pane", "list"], Some(machine))?;
            herds::members(&listed, &herd.workspace)
                .into_iter()
                .filter(|member| member.status != "no agent")
                .map(|member| (at(&member.name), member.pane))
                .collect()
        }
        None => vec![(at(&options.agent), options.agent.clone())],
    };
    if recipients.is_empty() {
        return Err(format!("no agents in herd {}", at(&options.agent)));
    }
    let sends: Vec<_> = recipients
        .into_iter()
        .map(|(to, agent)| {
            let (machine, wait, message) = (options.machine.clone(), options.wait, message.clone());
            let handle = thread::spawn(move || deliver(&agent, machine.as_deref(), wait, &message));
            (to, handle)
        })
        .collect();
    let mut failed = 0;
    for (to, handle) in sends {
        let outcome = handle.join().unwrap_or_else(|_| Err("send crashed".into()));
        log(&from, &to, text, &outcome);
        match outcome {
            Ok(via) => println!("sent to {to} ({via})"),
            Err(err) => {
                failed += 1;
                eprintln!("{} remuda: not sent to {to}: {err}", herdr::MARK);
            }
        }
    }
    match failed {
        0 => Ok(()),
        n => Err(format!("{n} not sent")),
    }
}

/// `[from aug-1@ziggy]`.
fn prefix(from: &str) -> String {
    format!("[from {from}]")
}

/// Who this message is from: this pane's agent name, or its agent kind and
/// pane id, at this machine's name; the machine name alone outside an agent.
fn default_sender() -> String {
    let machine = Config::load().unwrap_or_default().machine_name();
    let agent = std::env::var("HERDR_PANE_ID")
        .ok()
        .and_then(|pane| herdr::json(&["agent", "get", &pane], None).ok())
        .map(|mut result| result["agent"].take());
    sender_name(agent.as_ref(), &machine)
}

fn sender_name(agent: Option<&Value>, machine: &str) -> String {
    let text = |key: &str| {
        agent
            .and_then(|agent| agent[key].as_str())
            .filter(|value| !value.is_empty())
    };
    match (text("name"), text("agent"), text("pane_id")) {
        (Some(name), _, _) => format!("{name}@{machine}"),
        (None, Some(kind), Some(pane)) => format!("{kind} {pane}@{machine}"),
        _ => machine.to_string(),
    }
}

/// Waits until the agent is ready, then types the message. Returns how it
/// was typed.
fn deliver(
    target: &str,
    machine: Option<&str>,
    wait: Duration,
    message: &str,
) -> Result<&'static str, String> {
    let deadline = Instant::now() + wait;
    let mut ready = 0;
    let mut said_waiting = false;
    loop {
        let agent = match herdr::json(&["agent", "get", target], machine) {
            Ok(mut result) => result["agent"].take(),
            Err(err) if err.contains("agent_not_found") => return Err(err),
            // A machine that does not answer this once (a busy connection, a
            // reconnect) may next time; keep waiting.
            Err(_) => Value::Null,
        };
        let status = agent["agent_status"].as_str().unwrap_or("unreachable");
        ready = if is_ready(status) { ready + 1 } else { 0 };
        if ready >= READY_POLLS {
            let pane = agent["pane_id"].as_str().ok_or("herdr gave no pane id")?;
            match type_message(target, pane, machine, message) {
                Err(err) if err.contains("agent_blocked") => ready = 0,
                other => return other,
            }
        }
        if Instant::now() >= deadline {
            return Err(format!("{target} is {status}; gave up waiting"));
        }
        if !said_waiting && ready == 0 {
            eprintln!(
                "{} remuda: {} is {status}; sending once it is idle",
                herdr::MARK,
                target
            );
            said_waiting = true;
        }
        thread::sleep(POLL);
    }
}

/// An agent at its prompt waiting for the next instruction.
fn is_ready(status: &str) -> bool {
    matches!(status, "idle" | "done")
}

fn type_message(
    agent: &str,
    pane: &str,
    machine: Option<&str>,
    message: &str,
) -> Result<&'static str, String> {
    match herdr::json(&["agent", "prompt", agent, message], machine) {
        Ok(_) => Ok("agent prompt"),
        // Herdr only prompts agents it starts itself.
        Err(err) if err.contains("not an active named agent") => {
            herdr::run_on(machine, &["pane", "send-text", pane, &one_line(message)])?;
            thread::sleep(Duration::from_millis(300));
            herdr::run_on(machine, &["pane", "send-keys", pane, "enter"])?;
            Ok("typed")
        }
        Err(err) => Err(err),
    }
}

/// Typed text has no bracketed paste, so a newline would submit early.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Appends the message and what became of it to `messages.jsonl`.
fn log(from: &str, to: &str, text: &str, outcome: &Result<&'static str, String>) {
    let dir = views::state_dir();
    let at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let entry = match outcome {
        Ok(via) => {
            json!({"at": at, "from": from, "to": to, "text": text, "sent": true, "via": via})
        }
        Err(err) => {
            json!({"at": at, "from": from, "to": to, "text": text, "sent": false, "error": err})
        }
    };
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("messages.jsonl"))
    {
        let _ = writeln!(file, "{entry}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn targets_name_an_agent_and_optionally_a_machine() {
        assert_eq!(
            parse_target("aug-1@ziggy").unwrap(),
            ("aug-1".into(), Some("ziggy".into()))
        );
        assert_eq!(parse_target("wR:p1").unwrap(), ("wR:p1".into(), None));
        assert_eq!(
            parse_target("herd-2@local").unwrap(),
            ("herd-2".into(), None)
        );
        // Machine labels may contain @; the agent part never does.
        assert_eq!(
            parse_target("a@env:dev@ziggy").unwrap(),
            ("a@env:dev".into(), Some("ziggy".into()))
        );
        assert!(parse_target("@ziggy").is_err());
        assert!(parse_target("aug-1@").is_err());
    }

    #[test]
    fn options_take_flags_anywhere_and_the_rest_as_text() {
        let options = Options::parse(&args(&[
            "aug-1@ziggy",
            "--wait",
            "5",
            "rebase",
            "on",
            "main",
        ]))
        .unwrap();
        assert_eq!(options.agent, "aug-1");
        assert_eq!(options.wait, Duration::from_secs(5));
        assert_eq!(options.text.as_deref(), Some("rebase on main"));
        let dashed = Options::parse(&args(&["herd-1", "--", "--not-a-flag"])).unwrap();
        assert_eq!(dashed.text.as_deref(), Some("--not-a-flag"));
        assert_eq!(Options::parse(&args(&["herd-1"])).unwrap().text, None);
        assert!(Options::parse(&args(&[])).is_err());
        assert!(Options::parse(&args(&["herd-1", "--bogus"])).is_err());
        let herd = Options::parse(&args(&["@ops@ziggy", "status?"])).unwrap();
        assert_eq!(
            (herd.agent.as_str(), herd.machine.as_deref()),
            ("@ops", Some("ziggy"))
        );
        assert!(Options::parse(&args(&["@ops", "hi"])).is_err());
    }

    #[test]
    fn the_sender_is_the_agent_name_then_its_kind_and_pane() {
        let named = json!({"name": "herd-1", "agent": "claude", "pane_id": "wM:p1"});
        assert_eq!(sender_name(Some(&named), "ziggy"), "herd-1@ziggy");
        let unnamed = json!({"agent": "claude", "pane_id": "wA:p7"});
        assert_eq!(sender_name(Some(&unnamed), "air"), "claude wA:p7@air");
        assert_eq!(sender_name(None, "air"), "air");
        assert_eq!(prefix("herd-1@ziggy"), "[from herd-1@ziggy]");
    }

    #[test]
    fn only_idle_or_done_agents_are_typed_into() {
        assert!(is_ready("idle") && is_ready("done"));
        assert!(!is_ready("working") && !is_ready("blocked") && !is_ready("unknown"));
    }

    #[test]
    fn typed_messages_stay_on_one_line() {
        assert_eq!(one_line("a\nb\n\n  c"), "a b c");
    }
}
