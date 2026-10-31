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

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
/// The same rule for a herd, whose states arrive as events: how long an
/// agent must have been ready.
const SETTLE: Duration = Duration::from_secs(1);
/// How often a herd send looks at the states the events keep current.
const CHECK: Duration = Duration::from_millis(50);
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
            let listed = api(Some(machine), "pane.list", json!({}), &["pane", "list"])?;
            herds::herd_members(&listed, &herd)
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
    // A herd's states come from one feed for all of its agents; one agent
    // is asked for its own.
    let states = options.agent.starts_with('@').then(|| {
        States::follow(
            machine,
            recipients.iter().map(|(_, pane)| pane.clone()).collect(),
        )
    });
    let sends: Vec<_> = recipients
        .into_iter()
        .map(|(to, agent)| {
            let (machine, wait, message) = (options.machine.clone(), options.wait, message.clone());
            let states = states.clone();
            let handle = thread::spawn(move || match states {
                Some(states) => {
                    deliver_settled(&agent, &states, machine.as_deref(), wait, &message)
                }
                None => deliver(&agent, machine.as_deref(), wait, &message),
            });
            (to, handle)
        })
        .collect();
    let mut failed = 0;
    let outcomes: Vec<_> = sends
        .into_iter()
        .map(|(to, handle)| (to, handle.join()))
        .collect();
    if let Some(states) = &states {
        states.stop();
    }
    for (to, outcome) in outcomes {
        let outcome = outcome.unwrap_or_else(|_| Err("send crashed".into()));
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

/// `[from aug-1@devbox]`.
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
        let got = api(
            machine,
            "agent.get",
            json!({"target": target}),
            &["agent", "get", target],
        );
        let agent = match got {
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

/// Waits until a herd agent has been ready for `SETTLE`, by the states
/// `states` keeps, then types the message. Returns how it was typed.
fn deliver_settled(
    pane: &str,
    states: &States,
    machine: Option<&str>,
    wait: Duration,
    message: &str,
) -> Result<&'static str, String> {
    let deadline = Instant::now() + wait;
    let mut said_waiting = false;
    loop {
        let (status, since) = states.get(pane);
        if is_ready(&status) && since.elapsed() >= SETTLE {
            match type_message(pane, pane, machine, message) {
                // Blocked after all: wait for the next time it is ready.
                Err(err) if err.contains("agent_blocked") => states.touch(pane),
                other => return other,
            }
        }
        if Instant::now() >= deadline {
            return Err(format!("{pane} is {status}; gave up waiting"));
        }
        if !said_waiting && !is_ready(&status) {
            eprintln!(
                "{} remuda: {pane} is {status}; sending once it is idle",
                herdr::MARK
            );
            said_waiting = true;
        }
        thread::sleep(CHECK);
    }
}

/// A herd's agent states and when each last changed, kept current for a
/// send by one feed: herdr's events over the machine's forwarded socket, or
/// one `pane list` a second when there is no socket.
#[derive(Clone)]
struct States {
    states: Arc<Mutex<HashMap<String, (String, Instant)>>>,
    done: Arc<AtomicBool>,
}

impl States {
    fn follow(machine: Option<&str>, panes: Vec<String>) -> States {
        let states = States {
            states: Arc::default(),
            done: Arc::default(),
        };
        let (feed, machine) = (states.clone(), machine.map(str::to_string));
        thread::spawn(move || feed.run(machine.as_deref(), &panes));
        states
    }

    fn run(&self, machine: Option<&str>, panes: &[String]) {
        while !self.done.load(Ordering::Relaxed) {
            #[cfg(unix)]
            if let Some(socket) = herdr::api_socket(machine) {
                self.follow_events(&socket, panes);
                continue;
            }
            if let Ok(listed) = herdr::json(&["pane", "list"], machine) {
                self.set_listed(&listed);
            }
            thread::sleep(POLL);
        }
    }

    /// Subscribes, then lists, then applies events until the subscription
    /// ends or the send is over.
    #[cfg(unix)]
    fn follow_events(&self, socket: &std::path::Path, panes: &[String]) {
        let Ok(mut subscription) = crate::watch::Subscription::start(socket, panes) else {
            thread::sleep(POLL);
            return;
        };
        match crate::watch::list(socket) {
            Ok(listed) => self.set_listed(&listed),
            Err(_) => return,
        }
        while !self.done.load(Ordering::Relaxed) {
            match subscription.next() {
                Ok(Some(event)) => {
                    let data = &event["data"];
                    if let (Some(pane), Some(status)) =
                        (data["pane_id"].as_str(), data["agent_status"].as_str())
                    {
                        self.set(pane, status);
                    }
                }
                Ok(None) => {}
                Err(_) => return,
            }
        }
    }

    fn set_listed(&self, listed: &Value) {
        for pane in listed["panes"].as_array().into_iter().flatten() {
            if let (Some(id), Some(status)) =
                (pane["pane_id"].as_str(), pane["agent_status"].as_str())
            {
                self.set(id, status);
            }
        }
    }

    /// Records `status`, keeping when it began if it has not changed.
    fn set(&self, pane: &str, status: &str) {
        let Ok(mut states) = self.states.lock() else {
            return;
        };
        let unchanged = states.get(pane).is_some_and(|(was, _)| was == status);
        if !unchanged {
            states.insert(pane.to_string(), (status.to_string(), Instant::now()));
        }
    }

    /// Restarts the agent's settle time, after herdr refused it as blocked.
    fn touch(&self, pane: &str) {
        if let Ok(mut states) = self.states.lock() {
            if let Some((_, since)) = states.get_mut(pane) {
                *since = Instant::now();
            }
        }
    }

    /// The agent's state and since when; `unreachable` until first heard.
    fn get(&self, pane: &str) -> (String, Instant) {
        self.states
            .lock()
            .ok()
            .and_then(|states| states.get(pane).cloned())
            .unwrap_or_else(|| ("unreachable".into(), Instant::now()))
    }

    fn stop(&self) {
        self.done.store(true, Ordering::Relaxed);
    }
}

/// A herdr call over the machine's API socket when one reaches it without
/// starting a process, else the same call through the CLI.
fn api(machine: Option<&str>, method: &str, params: Value, cli: &[&str]) -> Result<Value, String> {
    #[cfg(unix)]
    if let Some(socket) = herdr::api_socket(machine) {
        return herdr::request_at(&socket, method, params);
    }
    let _ = (method, params);
    herdr::json(cli, machine)
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
    let prompt = json!({"target": agent, "text": message});
    match api(
        machine,
        "agent.prompt",
        prompt,
        &["agent", "prompt", agent, message],
    ) {
        Ok(_) => Ok("agent prompt"),
        // Herdr only prompts agents it starts itself.
        Err(err) if err.contains("not an active named agent") => {
            let line = one_line(message);
            let text = json!({"pane_id": pane, "text": line});
            api(
                machine,
                "pane.send_text",
                text,
                &["pane", "send-text", pane, &line],
            )?;
            thread::sleep(Duration::from_millis(300));
            let keys = json!({"pane_id": pane, "keys": ["enter"]});
            api(
                machine,
                "pane.send_keys",
                keys,
                &["pane", "send-keys", pane, "enter"],
            )?;
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
            parse_target("aug-1@devbox").unwrap(),
            ("aug-1".into(), Some("devbox".into()))
        );
        assert_eq!(parse_target("wR:p1").unwrap(), ("wR:p1".into(), None));
        assert_eq!(
            parse_target("herd-2@local").unwrap(),
            ("herd-2".into(), None)
        );
        // Machine labels may contain @; the agent part never does.
        assert_eq!(
            parse_target("a@env:dev@devbox").unwrap(),
            ("a@env:dev".into(), Some("devbox".into()))
        );
        assert!(parse_target("@devbox").is_err());
        assert!(parse_target("aug-1@").is_err());
    }

    #[test]
    fn options_take_flags_anywhere_and_the_rest_as_text() {
        let options = Options::parse(&args(&[
            "aug-1@devbox",
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
        let herd = Options::parse(&args(&["@ops@devbox", "status?"])).unwrap();
        assert_eq!(
            (herd.agent.as_str(), herd.machine.as_deref()),
            ("@ops", Some("devbox"))
        );
        assert!(Options::parse(&args(&["@ops", "hi"])).is_err());
    }

    #[test]
    fn the_sender_is_the_agent_name_then_its_kind_and_pane() {
        let named = json!({"name": "herd-1", "agent": "claude", "pane_id": "wM:p1"});
        assert_eq!(sender_name(Some(&named), "devbox"), "herd-1@devbox");
        let unnamed = json!({"agent": "claude", "pane_id": "wA:p7"});
        assert_eq!(sender_name(Some(&unnamed), "air"), "claude wA:p7@air");
        assert_eq!(sender_name(None, "air"), "air");
        assert_eq!(prefix("herd-1@devbox"), "[from herd-1@devbox]");
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
