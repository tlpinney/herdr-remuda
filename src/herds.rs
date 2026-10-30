//! Named herds: a herd is one workspace on one saved machine, named by the
//! workspace's label, whose agents the board watches.
//!
//! This machine keeps the herds it watches in `herds.json` in the plugin
//! state directory. Membership is not stored: a herd's agents are the agent
//! panes in its workspace, so adding a tab adds an agent and moving a pane
//! out removes it. remuda tags the panes it starts or adopts with
//! `remuda_herd` and `remuda_name` tokens, and an agent run in the shell with
//! `remuda_launched` until it first reports itself.

use std::collections::HashSet;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::herdr;
use crate::remote;
use crate::spec;
use crate::views;

/// Metadata source for the herd tokens, apart from mirrors' presence marks.
const TOKEN_SOURCE: &str = "remuda-herd";
pub const TOKEN_HERD: &str = "remuda_herd";
pub const TOKEN_NAME: &str = "remuda_name";
pub const TOKEN_LAUNCHED: &str = "remuda_launched";
/// Tries at `agent start` while a new pane's shell is still starting.
const START_TRIES: u32 = 5;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Herd {
    pub name: String,
    pub machine: String,
    pub workspace: String,
}

impl Herd {
    pub fn label(&self) -> String {
        format!("{}@{}", self.name, self.machine)
    }
}

fn registry_path() -> PathBuf {
    views::state_dir().join("herds.json")
}

/// The herds this machine watches; an unreadable registry means none.
pub fn load() -> Vec<Herd> {
    std::fs::read(registry_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save(herds: &[Herd]) -> Result<(), String> {
    let path = registry_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|err| format!("create {}: {err}", dir.display()))?;
    }
    let temp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(herds).map_err(|err| err.to_string())?;
    std::fs::write(&temp, bytes).map_err(|err| format!("write {}: {err}", temp.display()))?;
    std::fs::rename(&temp, &path).map_err(|err| format!("write {}: {err}", path.display()))
}

/// Splits `name@machine`. Herd names are word characters and dashes, so the
/// first `@` ends the name and machine labels may contain more.
pub fn parse_ref(text: &str) -> Result<(String, String), String> {
    let (name, machine) = text
        .split_once('@')
        .ok_or_else(|| format!("{text:?} is not <herd>@<machine>"))?;
    let name_ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !name_ok || machine.is_empty() {
        return Err(format!(
            "{text:?} is not <herd>@<machine> (herd names are letters, digits, - and _)"
        ));
    }
    Ok((name.to_string(), machine.to_string()))
}

pub fn find(name: &str, machine: &str) -> Option<Herd> {
    load()
        .into_iter()
        .find(|herd| herd.name == name && herd.machine == machine)
}

fn find_ref(text: &str) -> Result<Herd, String> {
    let (name, machine) = parse_ref(text)?;
    find(&name, &machine).ok_or_else(|| {
        format!("no herd {name}@{machine} here; start one with `remuda herd new {name}@{machine}`")
    })
}

/// A herd's agent as a pane list shows it.
#[derive(Debug, Clone)]
pub struct Seen {
    pub pane: String,
    pub name: String,
    /// `idle`, `working`, `blocked`, `done`, `unknown`, or `no agent` for a
    /// pane whose agent has not reported itself (yet).
    pub status: String,
    /// When remuda ran the agent in the shell, until it first reports.
    pub launched: Option<u64>,
    /// The pane's terminal title: agents set it to what they are doing.
    pub title: String,
}

/// The members of `workspace` in a machine's `pane list`: agent panes, and
/// panes remuda tagged that have no agent yet.
pub fn members(listed: &Value, workspace: &str) -> Vec<Seen> {
    listed["panes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|pane| pane["workspace_id"] == workspace && !spec::is_mirror(pane))
        .filter_map(|pane| {
            let id = pane["pane_id"].as_str()?.to_string();
            let tokens = &pane["tokens"];
            let agent = pane["agent"].as_str().filter(|agent| !agent.is_empty());
            if agent.is_none() && !tokens[TOKEN_HERD].is_string() {
                return None;
            }
            let name = tokens[TOKEN_NAME]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| format!("{} {id}", agent.unwrap_or("shell")));
            let status = match agent {
                Some(_) => pane["agent_status"].as_str().unwrap_or("unknown"),
                None => "no agent",
            };
            Some(Seen {
                pane: id,
                name,
                status: status.to_string(),
                launched: tokens[TOKEN_LAUNCHED]
                    .as_str()
                    .and_then(|at| at.parse().ok()),
                title: pane["terminal_title_stripped"]
                    .as_str()
                    .or_else(|| pane["title"].as_str())
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
            })
        })
        .collect()
}

/// `1 agent`, `3 agents`.
pub fn count_agents(count: usize) -> String {
    match count {
        1 => "1 agent".into(),
        n => format!("{n} agents"),
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// Now, in nanoseconds since the Unix epoch.
pub fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or_default()
}

/// Sets (or, with `None`, clears) herd tokens on a remote pane.
pub fn tag(machine: &str, pane: &str, tokens: &[(&str, Option<&str>)]) -> Result<(), String> {
    let mut args = vec![
        "pane".to_string(),
        "report-metadata".into(),
        pane.to_string(),
        "--source".into(),
        TOKEN_SOURCE.into(),
    ];
    for (key, value) in tokens {
        match value {
            Some(value) => args.extend(["--token".into(), format!("{key}={value}")]),
            None => args.extend(["--clear-token".into(), key.to_string()]),
        }
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    herdr::run_on(Some(machine), &args)
}

/// What to start in a herd.
pub struct Start {
    pub count: usize,
    pub kind: String,
    pub cwd: Option<String>,
    pub agent_args: Vec<String>,
}

impl Start {
    /// Parses `--count`, `--kind`, `--cwd` and agent arguments after `--`.
    fn parse(args: &[String], count: usize) -> Result<Start, String> {
        let mut start = Start {
            count,
            kind: "claude".into(),
            cwd: None,
            agent_args: Vec::new(),
        };
        let mut args = args.iter();
        while let Some(flag) = args.next() {
            if flag == "--" {
                start.agent_args = args.cloned().collect();
                break;
            }
            let value = args
                .next()
                .ok_or_else(|| format!("{flag} needs a value"))?
                .clone();
            match flag.as_str() {
                "--count" => {
                    start.count = value
                        .parse()
                        .ok()
                        .filter(|n| *n > 0)
                        .ok_or("--count needs a positive number")?
                }
                "--kind" => start.kind = value,
                "--cwd" => start.cwd = Some(value),
                _ => return Err(format!("unknown herd option {flag}")),
            }
        }
        Ok(start)
    }
}

/// The shell script that finds `cwd` on a machine: `~` is that machine's
/// home, and a path under this machine's home (`local_home`, as a shell
/// expands an unquoted `~/…` before remuda sees it) moves to the machine's
/// home when only that exists. Prints the directory, `mapped <dir>`, or
/// `missing <dir>`.
fn cwd_script(cwd: &str, local_home: &str) -> String {
    let (cwd, local) = (remote::shell_quote(cwd), remote::shell_quote(local_home));
    format!(
        r#"h=$HOME; d={cwd}; l={local}
case "$d" in "~") d=$h;; "~/"*) d=$h/${{d#"~/"}};; esac
if [ -d "$d" ]; then printf '%s\n' "$d"; exit 0; fi
case "$d" in "$l"/*) m=$h/${{d#"$l"/}}; if [ "$l" != "$h" ] && [ -d "$m" ]; then printf 'mapped %s\n' "$m"; exit 0; fi;; esac
printf 'missing %s\n' "$d""#
    )
}

/// Reads `cwd_script`'s answer: the directory to use, and whether it moved.
fn parse_cwd(output: &str, machine: &str) -> Result<(String, bool), String> {
    let line = output.trim();
    if let Some(dir) = line.strip_prefix("missing ") {
        return Err(format!("no directory {dir} on {machine}"));
    }
    match line.strip_prefix("mapped ") {
        Some(dir) => Ok((dir.to_string(), true)),
        None if !line.is_empty() => Ok((line.to_string(), false)),
        None => Err(format!("could not check the directory on {machine}")),
    }
}

/// `start` with its directory resolved on `machine`.
fn on_machine(start: &Start, machine: &str) -> Result<Start, String> {
    let cwd = match &start.cwd {
        None => None,
        Some(cwd) => {
            let local_home = std::env::var("HOME").unwrap_or_default();
            let output = herdr::shell_on(machine, &cwd_script(cwd, &local_home))?;
            let (dir, mapped) = parse_cwd(&output, machine)?;
            if mapped {
                println!(
                    "{} {cwd} is under this machine's home; using {dir} on {machine}",
                    herdr::MARK
                );
            }
            Some(dir)
        }
    };
    Ok(Start {
        count: start.count,
        kind: start.kind.clone(),
        cwd,
        agent_args: start.agent_args.clone(),
    })
}

/// Creates the herd `name` on `machine` (a new workspace labelled `name`),
/// registers it and starts its agents.
pub fn create(name: &str, machine: &str, start: &Start) -> Result<Herd, String> {
    if find(name, machine).is_some() {
        return Err(format!(
            "there is already a herd {name}@{machine}; use `remuda herd add {name}@{machine}`"
        ));
    }
    herdr::machine_by_label(machine)
        .ok_or_else(|| format!("no enabled saved machine named {machine}"))?;
    let start = &on_machine(start, machine)?;
    let mut create = vec!["workspace", "create", "--label", name, "--no-focus"];
    if let Some(cwd) = &start.cwd {
        create.extend(["--cwd", cwd]);
    }
    let created = herdr::json(&create, Some(machine))?;
    let herd = Herd {
        name: name.to_string(),
        machine: machine.to_string(),
        workspace: created["workspace"]["workspace_id"]
            .as_str()
            .ok_or("herdr did not say which workspace it created")?
            .to_string(),
    };
    let root = created["root_pane"]["pane_id"]
        .as_str()
        .ok_or("herdr did not say which pane it created")?
        .to_string();
    let tab = created["tab"]["tab_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let mut herds = load();
    herds.push(herd.clone());
    save(&herds)?;
    add_agents(&herd, start, Some((root, tab)))?;
    Ok(herd)
}

/// Starts `start.count` more agents in `herd`, one tab each (the first in
/// `first`, a pane and its tab, when given). Waits for every start and
/// reports each.
fn add_agents(herd: &Herd, start: &Start, first: Option<(String, String)>) -> Result<(), String> {
    let machine = herd.machine.as_str();
    let listed = herdr::json(&["pane", "list"], Some(machine))?;
    let agents = herdr::json(&["agent", "list"], Some(machine))?;
    let mut taken: HashSet<String> = agents["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|agent| agent["name"].as_str().map(str::to_string))
        .collect();
    // Agents run in the shell are named only once they report.
    taken.extend(
        members(&listed, &herd.workspace)
            .into_iter()
            .map(|m| m.name),
    );
    let taken: HashSet<&str> = taken.iter().map(String::as_str).collect();
    let names = pick_names(&herd.name, start.count, &taken);

    let mut first = first;
    let mut started = Vec::new();
    for name in names {
        let pane = match first.take() {
            Some((pane, tab)) => {
                herdr::run_quietly_on(Some(machine), &["tab", "rename", &tab, &name]);
                pane
            }
            None => {
                let mut create = vec![
                    "tab",
                    "create",
                    "--workspace",
                    &herd.workspace,
                    "--label",
                    &name,
                    "--no-focus",
                ];
                if let Some(cwd) = &start.cwd {
                    create.extend(["--cwd", cwd]);
                }
                let created = herdr::json(&create, Some(machine))?;
                created["root_pane"]["pane_id"]
                    .as_str()
                    .ok_or("herdr did not say which pane it created")?
                    .to_string()
            }
        };
        tag(
            machine,
            &pane,
            &[(TOKEN_HERD, Some(&herd.name)), (TOKEN_NAME, Some(&name))],
        )?;
        let (machine, kind, agent_args) = (
            machine.to_string(),
            start.kind.clone(),
            start.agent_args.clone(),
        );
        started.push((
            name.clone(),
            thread::spawn(move || start_agent(&machine, &kind, &name, &pane, &agent_args)),
        ));
    }
    for (name, handle) in started {
        match handle.join() {
            Ok(Ok(())) => println!("{} started {name}@{}", herdr::MARK, herd.machine),
            Ok(Err(err)) => println!("{} {name}@{}: {err}", herdr::MARK, herd.machine),
            Err(_) => println!("{} {name}@{}: start crashed", herdr::MARK, herd.machine),
        }
    }
    Ok(())
}

fn start_agent(
    machine: &str,
    kind: &str,
    name: &str,
    pane: &str,
    agent_args: &[String],
) -> Result<(), String> {
    let mut args = vec!["agent", "start", name, "--kind", kind, "--pane", pane];
    if !agent_args.is_empty() {
        args.push("--");
        args.extend(agent_args.iter().map(String::as_str));
    }
    let mut tries = 0;
    loop {
        match herdr::json(&args, Some(machine)) {
            Ok(_) => return Ok(()),
            // It started and is already asking something: the board seats it.
            Err(err) if err.contains("agent_not_ready") => return Ok(()),
            // Not a kind herdr starts itself (such as auggie, whose state a
            // plugin reports): run it in the shell instead.
            Err(err) if err.contains("unsupported interactive agent kind") => {
                return run_agent(machine, kind, pane, agent_args)
            }
            // A new pane's shell may not be at its prompt yet.
            Err(_) if tries + 1 < START_TRIES => {
                tries += 1;
                thread::sleep(Duration::from_secs(1));
            }
            Err(err) => return Err(err),
        }
    }
}

/// Runs `kind` as a command in the pane's shell and tags the pane with the
/// time, so the board can seat it while it sits at a startup screen before
/// its first report. The board names the agent once it reports.
fn run_agent(machine: &str, kind: &str, pane: &str, agent_args: &[String]) -> Result<(), String> {
    let command = std::iter::once(kind)
        .chain(agent_args.iter().map(String::as_str))
        .map(remote::shell_quote)
        .collect::<Vec<_>>()
        .join(" ");
    herdr::run_on(Some(machine), &["pane", "run", pane, &command])?;
    let now = now_secs().to_string();
    tag(machine, pane, &[(TOKEN_LAUNCHED, Some(&now))])
}

pub fn pick_names(prefix: &str, count: usize, taken: &HashSet<&str>) -> Vec<String> {
    (1..)
        .map(|n| format!("{prefix}-{n}"))
        .filter(|name| !taken.contains(name.as_str()))
        .take(count)
        .collect()
}

/// `remuda herd <command> …`, every command but the board.
pub fn command(command: &str, args: &[String]) -> Result<(), String> {
    match (command, args) {
        ("new", [target, rest @ ..]) => {
            let (name, machine) = parse_ref(target)?;
            let herd = create(&name, &machine, &Start::parse(rest, 3)?)?;
            println!(
                "{} herd {} is workspace {}; `remuda herd` watches it",
                herdr::MARK,
                herd.label(),
                herd.workspace
            );
            Ok(())
        }
        ("add", [target, rest @ ..]) => {
            let herd = find_ref(target)?;
            let start = on_machine(&Start::parse(rest, 1)?, &herd.machine)?;
            add_agents(&herd, &start, None)
        }
        ("remove", [target, agent]) => remove(&find_ref(target)?, agent),
        ("adopt", [target, workspace]) => adopt(target, workspace),
        ("forget", [target]) => {
            let herd = find_ref(target)?;
            unregister(&herd)?;
            println!(
                "{} stopped watching {}; its agents keep running in workspace {}",
                herdr::MARK,
                herd.label(),
                herd.workspace
            );
            Ok(())
        }
        ("stop", [target]) => {
            let herd = find_ref(target)?;
            // Closing the workspace ends every agent in it.
            if let Err(err) = herdr::run_on(
                Some(&herd.machine),
                &["workspace", "close", &herd.workspace],
            ) {
                // A workspace that is already gone leaves nothing to stop.
                if !err.contains("not found") {
                    return Err(err);
                }
            }
            unregister(&herd)?;
            println!(
                "{} stopped {}: closed workspace {} and its agents",
                herdr::MARK,
                herd.label(),
                herd.workspace
            );
            Ok(())
        }
        ("list", []) => list(),
        _ => Err(USAGE.into()),
    }
}

pub const USAGE: &str = "usage: remuda herd [board] [--grid ROWSxCOLS] [--seats N] [--beside PANE]
       remuda herd new <herd>@<machine> [--count 3] [--kind claude] [--cwd DIR] [-- AGENT_ARGS]
       remuda herd add <herd>@<machine> [--count 1] [--kind claude] [--cwd DIR] [-- AGENT_ARGS]
       remuda herd remove <herd>@<machine> <agent>
       remuda herd adopt <herd>@<machine> <workspace>
       remuda herd forget <herd>@<machine>     (stop watching; agents keep running)
       remuda herd stop <herd>@<machine>       (close the workspace: ends its agents)
       remuda herd list";

fn unregister(herd: &Herd) -> Result<(), String> {
    save(&load().into_iter().filter(|h| h != herd).collect::<Vec<_>>())
}

/// Moves `agent` out of the herd into a workspace of its own on the same
/// machine, where it keeps running.
fn remove(herd: &Herd, agent: &str) -> Result<(), String> {
    let machine = herd.machine.as_str();
    let listed = herdr::json(&["pane", "list"], Some(machine))?;
    let member = members(&listed, &herd.workspace)
        .into_iter()
        .find(|m| m.name == agent || m.pane == agent)
        .ok_or_else(|| format!("no agent {agent} in herd {}", herd.label()))?;
    tag(
        machine,
        &member.pane,
        &[(TOKEN_HERD, None), (TOKEN_LAUNCHED, None)],
    )?;
    let moved = herdr::json(
        &[
            "pane",
            "move",
            &member.pane,
            "--new-workspace",
            "--label",
            &member.name,
            "--no-focus",
        ],
        Some(machine),
    )?;
    let workspace = moved["move_result"]["pane"]["workspace_id"]
        .as_str()
        .unwrap_or("a new workspace");
    println!(
        "{} moved {} out of {} into {workspace} on {machine}; it keeps running",
        herdr::MARK,
        member.name,
        herd.label()
    );
    Ok(())
}

/// Registers an existing workspace as a herd and tags its agents.
fn adopt(target: &str, workspace: &str) -> Result<(), String> {
    let (name, machine) = parse_ref(target)?;
    if find(&name, &machine).is_some() {
        return Err(format!("there is already a herd {name}@{machine}"));
    }
    herdr::machine_by_label(&machine)
        .ok_or_else(|| format!("no enabled saved machine named {machine}"))?;
    let listed = herdr::json(&["pane", "list"], Some(&machine))?;
    let agents = herdr::json(&["agent", "list"], Some(&machine))?;
    let found = members(&listed, workspace);
    if found.is_empty() {
        return Err(format!("no agents in workspace {workspace} on {machine}"));
    }
    for member in &found {
        // Keep the live agent name; herdr knows it, pane infos do not.
        let live = agents["agents"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|agent| agent["pane_id"] == member.pane.as_str())
            .and_then(|agent| agent["name"].as_str())
            .unwrap_or(&member.name);
        tag(
            &machine,
            &member.pane,
            &[(TOKEN_HERD, Some(&name)), (TOKEN_NAME, Some(live))],
        )?;
    }
    let mut herds = load();
    herds.push(Herd {
        name: name.clone(),
        machine: machine.clone(),
        workspace: workspace.to_string(),
    });
    save(&herds)?;
    println!(
        "{} herd {name}@{machine} is workspace {workspace} with {}",
        herdr::MARK,
        count_agents(found.len())
    );
    Ok(())
}

fn list() -> Result<(), String> {
    let herds = load();
    if herds.is_empty() {
        println!("no herds yet: remuda herd new <herd>@<machine>");
    }
    for herd in herds {
        match herdr::json(&["pane", "list"], Some(&herd.machine)) {
            Ok(listed) => {
                let found = members(&listed, &herd.workspace);
                println!(
                    "{}\t{}\t{}",
                    herd.label(),
                    herd.workspace,
                    count_agents(found.len())
                );
                for member in found {
                    println!("  {}\t{}\t{}", member.name, member.pane, member.status);
                }
            }
            Err(_) => println!("{}\t{}\tnot answering", herd.label(), herd.workspace),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn refs_need_a_plain_name_and_a_machine() {
        assert_eq!(
            parse_ref("ops@devbox").unwrap(),
            ("ops".into(), "devbox".into())
        );
        assert_eq!(
            parse_ref("ops@env:dev@devbox").unwrap(),
            ("ops".into(), "env:dev@devbox".into())
        );
        assert!(parse_ref("ops").is_err());
        assert!(parse_ref("@devbox").is_err());
        assert!(parse_ref("ops@").is_err());
        assert!(parse_ref("my herd@devbox").is_err());
    }

    #[test]
    fn members_are_agents_and_tagged_panes_in_the_workspace() {
        let listed = json!({"panes": [
            {"pane_id": "wR:p1", "workspace_id": "wR", "agent": "claude", "agent_status": "idle",
             "tokens": {"remuda_herd": "ops", "remuda_name": "ops-1"}},
            {"pane_id": "wR:p2", "workspace_id": "wR",
             "tokens": {"remuda_herd": "ops", "remuda_name": "ops-2", "remuda_launched": "100"}},
            {"pane_id": "wR:p3", "workspace_id": "wR"},
            {"pane_id": "wR:p4", "workspace_id": "wR", "agent": "codex", "agent_status": "blocked"},
            {"pane_id": "wS:p1", "workspace_id": "wS", "agent": "claude", "agent_status": "idle"},
        ]});
        let found = members(&listed, "wR");
        let summary: Vec<(&str, &str, &str, Option<u64>)> = found
            .iter()
            .map(|m| {
                (
                    m.pane.as_str(),
                    m.name.as_str(),
                    m.status.as_str(),
                    m.launched,
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("wR:p1", "ops-1", "idle", None),
                ("wR:p2", "ops-2", "no agent", Some(100)),
                ("wR:p4", "codex wR:p4", "blocked", None),
            ]
        );
    }

    /// Runs `cwd_script` here with `$HOME` set to `home`.
    fn find_dir(cwd: &str, home: &std::path::Path, local_home: &str) -> String {
        let output = std::process::Command::new("sh")
            .args(["-c", &cwd_script(cwd, local_home)])
            .env("HOME", home)
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    #[test]
    fn directories_resolve_on_the_machine_with_its_own_home() {
        let root = std::env::temp_dir().join(format!("remuda-cwd-{}", std::process::id()));
        let home = root.join("remote");
        std::fs::create_dir_all(home.join("code/today")).unwrap();
        let home_text = home.display().to_string();
        let today = format!("{home_text}/code/today");
        assert_eq!(find_dir("~/code/today", &home, "/Users/me"), today);
        assert_eq!(find_dir("~", &home, "/Users/me"), home_text);
        assert_eq!(find_dir(&today, &home, "/Users/me"), today);
        // A local shell turned ~/code/today into this machine's home.
        assert_eq!(
            find_dir("/Users/me/code/today", &home, "/Users/me"),
            format!("mapped {today}")
        );
        assert_eq!(
            find_dir("~/nope", &home, "/Users/me"),
            format!("missing {home_text}/nope")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn directory_answers_parse_or_explain() {
        assert_eq!(
            parse_cwd("/h/x\n", "devbox").unwrap(),
            ("/h/x".into(), false)
        );
        assert_eq!(
            parse_cwd("mapped /h/x", "devbox").unwrap(),
            ("/h/x".into(), true)
        );
        assert_eq!(
            parse_cwd("missing /h/nope", "devbox").unwrap_err(),
            "no directory /h/nope on devbox"
        );
        assert!(parse_cwd("", "devbox").is_err());
    }

    #[test]
    fn names_skip_taken_ones() {
        let taken = HashSet::from(["ops-1", "ops-3"]);
        assert_eq!(pick_names("ops", 3, &taken), ["ops-2", "ops-4", "ops-5"]);
    }

    #[test]
    fn start_options_default_per_command() {
        let args = |list: &[&str]| list.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        let start = Start::parse(&args(&["--kind", "auggie", "--", "--model", "x"]), 1).unwrap();
        assert_eq!((start.count, start.kind.as_str()), (1, "auggie"));
        assert_eq!(start.agent_args, ["--model", "x"]);
        assert_eq!(Start::parse(&[], 3).unwrap().count, 3);
        assert!(Start::parse(&args(&["--count", "0"]), 1).is_err());
    }
}
