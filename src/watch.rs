//! What herdr lists on each machine with a watched herd, kept current by
//! herdr's events instead of polling.
//!
//! One thread per machine reads `pane.list` over the machine's forwarded API
//! socket, subscribes to status changes of the panes in its herds, and
//! applies each change to that listing as it arrives. It reads the listing
//! again every few seconds, for new agents, names and titles, and whenever
//! the subscription ends (herdr drops a subscriber that falls behind its
//! event history), so a change lost with it is caught there. A machine
//! without a forwarded socket is polled with `pane list` as before.

use std::collections::{BTreeSet, HashSet};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::herdr;
use crate::herds;

/// How often a machine's listing is read again while events keep it current.
const REFRESH: Duration = Duration::from_secs(5);
/// How often a machine with no forwarded socket is polled, and how long
/// to wait before trying a machine that did not answer.
const POLL: Duration = Duration::from_secs(1);
/// How long a read of the subscription waits before checking for a refresh.
const READ_TIMEOUT: Duration = Duration::from_millis(250);

/// A machine's latest `pane list` result, or `None` when it did not answer.
pub struct Listing {
    pub machine: String,
    pub listed: Option<Value>,
}

/// Watches every machine with a herd in the registry, sending a `Listing`
/// each time one changes. Machines are picked up as herds are added and
/// dropped once their last herd is forgotten.
pub fn spawn(updates: Sender<Listing>) {
    let running: Arc<Mutex<HashSet<String>>> = Arc::default();
    thread::spawn(move || loop {
        for machine in machines() {
            let Ok(mut running_now) = running.lock() else {
                return;
            };
            if running_now.insert(machine.clone()) {
                let (updates, running) = (updates.clone(), Arc::clone(&running));
                thread::spawn(move || {
                    watch_machine(&machine, &updates);
                    if let Ok(mut running) = running.lock() {
                        running.remove(&machine);
                    }
                });
            }
        }
        thread::sleep(POLL);
    });
}

fn machines() -> BTreeSet<String> {
    herds::load().into_iter().map(|herd| herd.machine).collect()
}

/// The workspaces of `machine`'s herds, empty once it has none.
fn workspaces(machine: &str) -> HashSet<String> {
    herds::load()
        .into_iter()
        .filter(|herd| herd.machine == machine)
        .map(|herd| herd.workspace)
        .collect()
}

/// Keeps `machine`'s listing current until it has no herds left.
fn watch_machine(machine: &str, updates: &Sender<Listing>) {
    loop {
        let workspaces = workspaces(machine);
        if workspaces.is_empty() {
            return;
        }
        let send = |listed: Option<Value>| {
            updates
                .send(Listing {
                    machine: machine.to_string(),
                    listed,
                })
                .is_ok()
        };
        let Some(socket) = herdr::api_socket(Some(machine)) else {
            if !send(herdr::json(&["pane", "list"], Some(machine)).ok()) {
                return;
            }
            thread::sleep(POLL);
            continue;
        };
        if follow(machine, &socket, &send).is_err() {
            return;
        }
    }
}

/// The board went away.
struct Stop;

/// Subscribes to the herd panes on `socket` and keeps sending their listing
/// until the subscription ends, the herds' panes change, or a read fails.
/// The caller then starts over with a fresh listing.
fn follow(machine: &str, socket: &Path, send: &dyn Fn(Option<Value>) -> bool) -> Result<(), Stop> {
    let sent = |listed: Option<Value>| if send(listed) { Ok(()) } else { Err(Stop) };
    let Ok(listed) = list(socket) else {
        sent(None)?;
        thread::sleep(POLL);
        return Ok(());
    };
    let panes = herd_panes(&listed, &workspaces(machine));
    let subscription = match panes.is_empty() {
        true => None,
        false => Subscription::start(socket, &panes).ok(),
    };
    let Some(mut subscription) = subscription else {
        // Nothing to follow (or herdr refused): poll this once instead.
        sent(Some(listed))?;
        thread::sleep(POLL);
        return Ok(());
    };
    // Subscribed first, then listed, so no change falls between the two.
    let Ok(mut listed) = list(socket) else {
        return Ok(());
    };
    sent(Some(listed.clone()))?;
    let mut refresh = Instant::now() + REFRESH;
    loop {
        match subscription.next() {
            // A pane the listing lacks: start over with a fresh one.
            Ok(Some(event)) if !apply(&mut listed, &event) => return Ok(()),
            Ok(Some(_)) => sent(Some(listed.clone()))?,
            Ok(None) => {}
            Err(_) => return Ok(()),
        }
        if Instant::now() >= refresh {
            let Ok(fresh) = list(socket) else {
                return Ok(());
            };
            if herd_panes(&fresh, &workspaces(machine)) != panes {
                return Ok(());
            }
            listed = fresh;
            sent(Some(listed.clone()))?;
            refresh = Instant::now() + REFRESH;
        }
    }
}

pub(crate) fn list(socket: &Path) -> Result<Value, String> {
    herdr::request_at(socket, "pane.list", json!({}))
}

/// The panes in `workspaces`, sorted, for subscribing and for noticing
/// when an agent joins or leaves.
fn herd_panes(listed: &Value, workspaces: &HashSet<String>) -> Vec<String> {
    let mut panes: Vec<String> = listed["panes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|pane| {
            pane["workspace_id"]
                .as_str()
                .is_some_and(|workspace| workspaces.contains(workspace))
        })
        .filter_map(|pane| pane["pane_id"].as_str().map(str::to_string))
        .collect();
    panes.sort();
    panes
}

/// Applies a `pane.agent_status_changed` event to the listing. False when
/// the event is for a pane the listing lacks, or names an agent the listing
/// does not show yet, so the caller reads the listing again.
fn apply(listed: &mut Value, event: &Value) -> bool {
    let data = &event["data"];
    let (Some(pane_id), Some(status)) = (data["pane_id"].as_str(), data["agent_status"].as_str())
    else {
        return true;
    };
    let Some(pane) = listed["panes"]
        .as_array_mut()
        .into_iter()
        .flatten()
        .find(|pane| pane["pane_id"] == pane_id)
    else {
        return false;
    };
    let unnamed = pane["agent"].as_str().is_none_or(str::is_empty);
    if unnamed
        && data["agent"]
            .as_str()
            .is_some_and(|agent| !agent.is_empty())
    {
        return false;
    }
    pane["agent_status"] = json!(status);
    true
}

/// A herdr `events.subscribe` stream of status changes for some panes.
pub(crate) struct Subscription {
    reader: BufReader<UnixStream>,
    line: String,
}

impl Subscription {
    pub(crate) fn start(socket: &Path, panes: &[String]) -> Result<Subscription, String> {
        let mut stream = UnixStream::connect(socket).map_err(|err| err.to_string())?;
        let subscriptions: Vec<Value> = panes
            .iter()
            .map(|pane| json!({"type": "pane.agent_status_changed", "pane_id": pane}))
            .collect();
        let request = json!({
            "id": "remuda-watch",
            "method": "events.subscribe",
            "params": {"subscriptions": subscriptions},
        });
        writeln!(stream, "{request}").map_err(|err| err.to_string())?;
        stream
            .set_read_timeout(Some(READ_TIMEOUT))
            .map_err(|err| err.to_string())?;
        let mut subscription = Subscription {
            reader: BufReader::new(stream),
            line: String::new(),
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match subscription.next()? {
                Some(ack) if ack.get("error").is_some() => {
                    return Err(ack["error"]["message"].as_str().unwrap_or("refused").into())
                }
                Some(_) => return Ok(subscription),
                None if Instant::now() > deadline => return Err("no answer".into()),
                None => {}
            }
        }
    }

    /// The next message, or `None` if none arrived within the read timeout.
    /// An error message from herdr (such as lost events) ends the stream.
    pub(crate) fn next(&mut self) -> Result<Option<Value>, String> {
        match self.reader.read_line(&mut self.line) {
            Ok(0) => Err("herdr closed the subscription".into()),
            Ok(_) if self.line.ends_with('\n') => {
                let message: Value =
                    serde_json::from_str(&self.line).map_err(|err| err.to_string())?;
                self.line.clear();
                if message.get("error").is_some() && message["id"] != "remuda-watch" {
                    return Err("the subscription ended".into());
                }
                Ok(Some(message))
            }
            // Part of a line; the rest comes with a later read.
            Ok(_) => Ok(None),
            Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                Ok(None)
            }
            Err(err) => Err(err.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing() -> Value {
        json!({"panes": [
            {"pane_id": "w1:p1", "workspace_id": "w1", "agent": "claude", "agent_status": "working"},
            {"pane_id": "w1:p2", "workspace_id": "w1", "agent": "", "agent_status": "unknown"},
            {"pane_id": "w2:p1", "workspace_id": "w2", "agent": "pi", "agent_status": "idle"},
        ]})
    }

    fn status(pane: &str, status: &str, agent: Option<&str>) -> Value {
        json!({"event": "pane_agent_status_changed",
               "data": {"pane_id": pane, "agent_status": status, "agent": agent}})
    }

    #[test]
    fn events_update_the_listing_in_place() {
        let mut listed = listing();
        assert!(apply(
            &mut listed,
            &status("w1:p1", "blocked", Some("claude"))
        ));
        assert_eq!(listed["panes"][0]["agent_status"], "blocked");
    }

    #[test]
    fn unknown_panes_and_newly_named_agents_need_a_fresh_listing() {
        let mut listed = listing();
        assert!(!apply(
            &mut listed,
            &status("w1:p9", "idle", Some("claude"))
        ));
        assert!(!apply(
            &mut listed,
            &status("w1:p2", "working", Some("auggie"))
        ));
        assert_eq!(listed["panes"][1]["agent_status"], "unknown");
    }

    #[test]
    fn herd_panes_are_those_in_the_herds_workspaces() {
        let workspaces = HashSet::from(["w1".to_string()]);
        assert_eq!(herd_panes(&listing(), &workspaces), ["w1:p1", "w1:p2"]);
    }
}
