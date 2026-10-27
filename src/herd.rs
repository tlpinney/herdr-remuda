//! A herd: agents on a remote machine that come to you when they need input.
//!
//! `remuda herd` starts agents on a saved machine, one tab each, or adopts the
//! agents of a remote workspace, and turns the pane it runs in into a board of
//! their states. When an agent turns `blocked`, the herd opens a driven mirror
//! of it in this tab and shows a toast; once the agent is working again the
//! mirror closes, freeing its seat for the next one. Seats split this tab
//! right while the columns stay wide enough, then down, so the board stays in
//! the top-left corner.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use serde_json::Value;

use crate::herdr::{self, MARK};
use crate::remote;
use crate::spec;

const POLL: Duration = Duration::from_secs(1);
const TICK: Duration = Duration::from_millis(200);
/// Polls in a row an agent must be out of `blocked` before its seat closes, so
/// a flicker between dialogs does not close it.
const CALM_POLLS: u8 = 2;
/// The smallest seat worth opening; smaller panes would wrap the agent's UI.
const MIN_COLS: u64 = 70;
const MIN_ROWS: u64 = 14;
/// Tries at `agent start` while a new pane's shell is still starting.
const START_TRIES: u32 = 5;
/// How long an agent run in the shell may stay unreported before the herd
/// treats it as waiting at a startup prompt.
const UNREPORTED_AFTER: Duration = Duration::from_secs(8);

struct Options {
    machine: String,
    count: usize,
    kind: String,
    cwd: Option<String>,
    name: String,
    seats: usize,
    workspace: Option<String>,
    agent_args: Vec<String>,
}

const USAGE: &str = "usage: remuda herd --machine NAME [--count 3] [--kind claude] [--cwd DIR] \
[--name herd] [--seats 3] [--workspace ID] [-- AGENT_ARGS...]";

impl Options {
    fn parse(args: &[String]) -> Result<Options, String> {
        let mut options = Options {
            machine: String::new(),
            count: 3,
            kind: "claude".into(),
            cwd: None,
            name: "herd".into(),
            seats: 3,
            workspace: None,
            agent_args: Vec::new(),
        };
        let mut args = args.iter();
        while let Some(flag) = args.next() {
            if flag == "--" {
                options.agent_args = args.cloned().collect();
                break;
            }
            let value = args
                .next()
                .ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))?
                .clone();
            let number = |value: &str| {
                value
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| format!("{flag} needs a positive number"))
            };
            match flag.as_str() {
                "--machine" => options.machine = value,
                "--count" => options.count = number(&value)?,
                "--kind" => options.kind = value,
                "--cwd" => options.cwd = Some(value),
                "--name" => options.name = value,
                "--seats" => options.seats = number(&value)?,
                "--workspace" => options.workspace = Some(value),
                _ => return Err(format!("unknown herd option {flag}\n{USAGE}")),
            }
        }
        if options.machine.is_empty() {
            return Err(format!("no machine: pass --machine\n{USAGE}"));
        }
        Ok(options)
    }
}

struct Member {
    name: String,
    /// The agent's pane on the remote machine.
    pane: String,
    status: String,
    note: String,
    /// When the current blocked spell began; orders the queue for seats.
    blocked_since: Option<Instant>,
    /// The local mirror pane showing this agent.
    seat: Option<String>,
    calm: u8,
    /// The user closed this agent's seat while it was blocked: leave it until
    /// it is blocked again.
    waived: bool,
    /// When the agent was run in the shell, until it reports itself to herdr.
    /// Hook-reported agents (auggie) can stop at a startup prompt before
    /// their first report, so a long silence counts as needing input.
    launched: Option<Instant>,
}

impl Member {
    fn new(name: String, pane: String) -> Member {
        Member {
            name,
            pane,
            status: "starting".into(),
            note: String::new(),
            blocked_since: None,
            seat: None,
            calm: 0,
            waived: false,
            launched: None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    /// Close this member's seat: its agent no longer needs input.
    Close(usize),
    /// This member just turned blocked.
    Notify(usize),
    /// This member, run in the shell, just reported itself to herdr.
    Reported(usize),
}

/// The seating rules, apart from herdr so they can be tested.
struct Herd {
    members: Vec<Member>,
    max_seats: usize,
}

impl Herd {
    /// Takes in the agents' latest statuses (by remote pane id; a missing pane
    /// is gone) and which local panes are still open.
    fn observe(
        &mut self,
        statuses: &HashMap<String, String>,
        open: &HashSet<String>,
        now: Instant,
    ) -> Vec<Action> {
        let mut actions = Vec::new();
        for (index, member) in self.members.iter_mut().enumerate() {
            let mut status = statuses
                .get(&member.pane)
                .cloned()
                .unwrap_or_else(|| "gone".into());
            if status != "no agent" {
                if member.launched.take().is_some() && status != "gone" {
                    actions.push(Action::Reported(index));
                }
            } else if member
                .launched
                .is_some_and(|at| now.saturating_duration_since(at) >= UNREPORTED_AFTER)
            {
                status = "startup".into();
            }
            let blocked = matches!(status.as_str(), "blocked" | "startup");
            if member
                .seat
                .as_ref()
                .is_some_and(|seat| !open.contains(seat))
            {
                // Closed by the user (ctrl+]).
                member.seat = None;
                member.calm = 0;
                member.waived = blocked;
            }
            if blocked {
                member.calm = 0;
                if member.blocked_since.is_none() {
                    member.blocked_since = Some(now);
                    actions.push(Action::Notify(index));
                }
            } else {
                member.blocked_since = None;
                member.waived = false;
                if member.seat.is_some() {
                    member.calm += 1;
                    if member.calm >= CALM_POLLS || status == "gone" {
                        actions.push(Action::Close(index));
                    }
                }
            }
            member.status = status;
        }
        actions
    }

    /// Members waiting for a seat, longest-waiting first.
    fn queue(&self) -> Vec<usize> {
        let mut waiting: Vec<(Instant, usize)> = self
            .members
            .iter()
            .enumerate()
            .filter(|(_, m)| m.seat.is_none() && !m.waived)
            .filter_map(|(index, m)| Some((m.blocked_since?, index)))
            .collect();
        waiting.sort();
        waiting.into_iter().map(|(_, index)| index).collect()
    }

    fn seated(&self) -> usize {
        self.members.iter().filter(|m| m.seat.is_some()).count()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Right,
    Down,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Direction::Right => "right",
            Direction::Down => "down",
        }
    }
}

/// Where the next seat goes in this tab's `pane layout`: a split of the board
/// or a seat (`ours`), never of the user's other panes. Splitting right wins
/// while both halves stay `MIN_COLS` wide; then down while both stay
/// `MIN_ROWS` tall. `None` when the tab has no room.
fn place(layout: &Value, ours: &HashSet<String>) -> Option<(String, Direction)> {
    let panes: Vec<(&str, u64, u64)> = layout["panes"]
        .as_array()?
        .iter()
        .filter_map(|pane| {
            let id = pane["pane_id"].as_str()?;
            let rect = &pane["rect"];
            Some((id, rect["width"].as_u64()?, rect["height"].as_u64()?))
        })
        .filter(|(id, _, _)| ours.contains(*id))
        .collect();
    let right = panes
        .iter()
        .filter(|(_, width, _)| *width >= 2 * MIN_COLS)
        .max_by_key(|(_, width, height)| (*width, *height));
    if let Some((id, _, _)) = right {
        return Some((id.to_string(), Direction::Right));
    }
    panes
        .iter()
        .filter(|(_, _, height)| *height >= 2 * MIN_ROWS)
        .max_by_key(|(_, width, height)| (*height, *width))
        .map(|(id, _, _)| (id.to_string(), Direction::Down))
}

enum Event {
    Statuses(Result<HashMap<String, String>, String>),
    Started {
        index: usize,
        note: String,
    },
    /// The agent was run in its pane's shell rather than by `agent start`.
    Launched {
        index: usize,
    },
}

pub fn run(args: &[String]) -> Result<(), String> {
    let options = Options::parse(args)?;
    let board = std::env::var("HERDR_PANE_ID")
        .map_err(|_| "remuda herd runs in a herdr pane (HERDR_PANE_ID is not set)")?;
    herdr::machine_by_label(&options.machine)
        .ok_or_else(|| format!("no enabled saved machine named {}", options.machine))?;

    let (events, inbox) = mpsc::channel();
    let (workspace, members) = match &options.workspace {
        Some(workspace) => (workspace.clone(), adopt(&options.machine, workspace)?),
        None => {
            eprintln!(
                "{MARK} starting {} {} agents on {}…",
                options.count, options.kind, options.machine
            );
            spawn(&options, &events)?
        }
    };
    if members.is_empty() {
        return Err(format!(
            "no agents in workspace {workspace} on {}",
            options.machine
        ));
    }
    spawn_poller(options.machine.clone(), events);

    let mut terminal = ratatui::init();
    let mut herd = Herd {
        members,
        max_seats: options.seats,
    };
    let result = watch(
        &mut herd,
        &options,
        &workspace,
        &board,
        &inbox,
        &mut terminal,
    );
    ratatui::restore();
    for member in &herd.members {
        if let Some(seat) = &member.seat {
            herdr::run_quietly(&["pane", "close", seat]);
        }
    }
    println!(
        "{MARK} the herd keeps running on {}; watch it again with\n  remuda herd --machine {} --workspace {workspace}",
        options.machine, options.machine
    );
    result
}

fn watch(
    herd: &mut Herd,
    options: &Options,
    workspace: &str,
    board: &str,
    inbox: &Receiver<Event>,
    terminal: &mut ratatui::DefaultTerminal,
) -> Result<(), String> {
    let mut error = String::new();
    loop {
        while let Ok(event) = inbox.try_recv() {
            match event {
                Event::Started { index, note } => herd.members[index].note = note,
                Event::Launched { index } => herd.members[index].launched = Some(Instant::now()),
                Event::Statuses(Err(err)) => error = err,
                Event::Statuses(Ok(statuses)) => {
                    error.clear();
                    let layout = herdr::json(&["pane", "layout", "--pane", board], None)
                        .map(|mut result| result["layout"].take())
                        .unwrap_or(Value::Null);
                    let open: HashSet<String> = layout["panes"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|pane| pane["pane_id"].as_str().map(str::to_string))
                        .collect();
                    let actions = herd.observe(&statuses, &open, Instant::now());
                    let mut fresh = Vec::new();
                    for action in actions {
                        match action {
                            Action::Close(index) => {
                                if let Some(seat) = herd.members[index].seat.take() {
                                    herdr::run_quietly(&["pane", "close", &seat]);
                                }
                                herd.members[index].calm = 0;
                            }
                            Action::Notify(index) => fresh.push(index),
                            Action::Reported(index) => {
                                // Names make `herdr agent` commands find it.
                                let (pane, name) = (
                                    herd.members[index].pane.clone(),
                                    herd.members[index].name.clone(),
                                );
                                let machine = options.machine.clone();
                                thread::spawn(move || {
                                    herdr::run_quietly_on(
                                        Some(&machine),
                                        &["agent", "rename", &pane, &name],
                                    );
                                });
                            }
                        }
                    }
                    seat_waiting(herd, &options.machine, board);
                    for index in fresh {
                        notify(&herd.members[index], &options.machine);
                    }
                }
            }
        }
        terminal
            .draw(|frame| draw(frame, herd, &options.machine, workspace, &error))
            .map_err(|err| format!("draw herd: {err}"))?;
        if event::poll(TICK).map_err(|err| format!("read key: {err}"))? {
            if let TermEvent::Key(key) = event::read().map_err(|err| format!("read key: {err}"))? {
                let quit = key.kind == KeyEventKind::Press
                    && (matches!(key.code, KeyCode::Char('q'))
                        || (key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL)));
                if quit {
                    return Ok(());
                }
            }
        }
    }
}

/// Opens seats for waiting members while there are free seats and room.
fn seat_waiting(herd: &mut Herd, machine: &str, board: &str) {
    for index in herd.queue() {
        if herd.seated() >= herd.max_seats {
            return;
        }
        let Ok(mut layout) = herdr::json(&["pane", "layout", "--pane", board], None) else {
            return;
        };
        let layout = layout["layout"].take();
        let mut ours: HashSet<String> =
            herd.members.iter().filter_map(|m| m.seat.clone()).collect();
        ours.insert(board.to_string());
        let Some((target, direction)) = place(&layout, &ours) else {
            return;
        };
        // Take focus only from the board, never from a seat being typed in.
        let focus = layout["focused_pane_id"] == board;
        match open_seat(&herd.members[index], machine, &target, direction, focus) {
            Ok(seat) => herd.members[index].seat = Some(seat),
            Err(err) => {
                herd.members[index].note = err;
                return;
            }
        }
    }
}

fn open_seat(
    member: &Member,
    machine: &str,
    target: &str,
    direction: Direction,
    focus: bool,
) -> Result<String, String> {
    let plugin = herdr::plugin_id();
    let source = format!("REMUDA_SOURCE={}", member.pane);
    let machine = format!("REMUDA_MACHINE={machine}");
    let args = [
        "plugin",
        "pane",
        "open",
        "--plugin",
        &plugin,
        "--entrypoint",
        "view",
        "--placement",
        "split",
        "--direction",
        direction.as_str(),
        "--target-pane",
        target,
        if focus { "--focus" } else { "--no-focus" },
        "--env",
        &source,
        "--env",
        &machine,
        "--env",
        "REMUDA_MODE=control",
        "--env",
        "REMUDA_SIZE=follow",
    ];
    let result = herdr::json(&args, None)?;
    result["plugin_pane"]["pane"]["pane_id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "herdr did not say which pane it opened".into())
}

fn notify(member: &Member, machine: &str) {
    let title = format!("{} needs input", member.name);
    let body = match member.seat {
        Some(_) => format!("{machine} · opened beside the herd"),
        None => format!("{machine} · waiting for a free seat"),
    };
    thread::spawn(move || {
        herdr::run_quietly(&[
            "notification",
            "show",
            &title,
            "--body",
            &body,
            "--sound",
            "request",
        ]);
    });
}

/// Creates a remote workspace with one tab per agent and starts the agents in
/// the background; each start reports back as `Event::Started`.
fn spawn(options: &Options, events: &Sender<Event>) -> Result<(String, Vec<Member>), String> {
    let machine = Some(options.machine.as_str());
    let names = free_names(&options.name, options.count, machine)?;
    let mut create = vec![
        "workspace",
        "create",
        "--label",
        &options.name,
        "--no-focus",
    ];
    if let Some(cwd) = &options.cwd {
        create.extend(["--cwd", cwd]);
    }
    let created = herdr::json(&create, machine)?;
    let workspace = created["workspace"]["workspace_id"]
        .as_str()
        .ok_or("herdr did not say which workspace it created")?
        .to_string();
    let mut panes = vec![created["root_pane"]["pane_id"]
        .as_str()
        .ok_or("herdr did not say which pane it created")?
        .to_string()];
    for name in &names[1..] {
        let mut create = vec![
            "tab",
            "create",
            "--workspace",
            &workspace,
            "--label",
            name,
            "--no-focus",
        ];
        if let Some(cwd) = &options.cwd {
            create.extend(["--cwd", cwd]);
        }
        let created = herdr::json(&create, machine)?;
        panes.push(
            created["root_pane"]["pane_id"]
                .as_str()
                .ok_or("herdr did not say which pane it created")?
                .to_string(),
        );
    }
    let members: Vec<Member> = names
        .into_iter()
        .zip(panes)
        .map(|(name, pane)| Member::new(name, pane))
        .collect();
    for (index, member) in members.iter().enumerate() {
        let (machine, kind, name, pane) = (
            options.machine.clone(),
            options.kind.clone(),
            member.name.clone(),
            member.pane.clone(),
        );
        let agent_args = options.agent_args.clone();
        let events = events.clone();
        thread::spawn(move || {
            let launched = || {
                let _ = events.send(Event::Launched { index });
            };
            let note = match start_agent(&machine, &kind, &name, &pane, &agent_args, launched) {
                Ok(()) => String::new(),
                Err(err) => err,
            };
            let _ = events.send(Event::Started { index, note });
        });
    }
    Ok((workspace, members))
}

fn start_agent(
    machine: &str,
    kind: &str,
    name: &str,
    pane: &str,
    agent_args: &[String],
    launched: impl FnOnce(),
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
            // It started and is already asking something: the herd seats it.
            Err(err) if err.contains("agent_not_ready") => return Ok(()),
            // Not a kind herdr starts itself (such as auggie, whose state a
            // plugin reports): run it in the shell instead.
            Err(err) if err.contains("unsupported interactive agent kind") => {
                return run_agent(machine, kind, pane, agent_args, launched)
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

/// Runs `kind` as a command in the pane's shell. The herd names the agent
/// once it reports itself (see `name_reported`).
fn run_agent(
    machine: &str,
    kind: &str,
    pane: &str,
    agent_args: &[String],
    launched: impl FnOnce(),
) -> Result<(), String> {
    let command = std::iter::once(kind)
        .chain(agent_args.iter().map(String::as_str))
        .map(remote::shell_quote)
        .collect::<Vec<_>>()
        .join(" ");
    herdr::run_on(Some(machine), &["pane", "run", pane, &command])?;
    launched();
    Ok(())
}

/// `count` agent names `<prefix>-<n>` not taken by a live agent on the machine.
fn free_names(prefix: &str, count: usize, machine: Option<&str>) -> Result<Vec<String>, String> {
    let listed = herdr::json(&["agent", "list"], machine)?;
    let taken: HashSet<&str> = listed["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|agent| agent["name"].as_str())
        .collect();
    Ok(pick_names(prefix, count, &taken))
}

fn pick_names(prefix: &str, count: usize, taken: &HashSet<&str>) -> Vec<String> {
    (1..)
        .map(|n| format!("{prefix}-{n}"))
        .filter(|name| !taken.contains(name.as_str()))
        .take(count)
        .collect()
}

/// The agents already running in a remote workspace.
fn adopt(machine: &str, workspace: &str) -> Result<Vec<Member>, String> {
    let listed = herdr::json(&["pane", "list"], Some(machine))?;
    // Pane infos carry the agent kind; live agent names come from `agent list`.
    let agents = herdr::json(&["agent", "list"], Some(machine))?;
    let names: HashMap<&str, &str> = agents["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|agent| Some((agent["pane_id"].as_str()?, agent["name"].as_str()?)))
        .collect();
    Ok(listed["panes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|pane| pane["workspace_id"] == workspace && !spec::is_mirror(pane))
        .filter_map(|pane| {
            let agent = pane["agent"].as_str().filter(|agent| !agent.is_empty())?;
            let id = pane["pane_id"].as_str()?.to_string();
            let name = match names.get(id.as_str()) {
                Some(name) => name.to_string(),
                None => format!("{agent} {id}"),
            };
            Some(Member::new(name, id))
        })
        .collect())
}

/// Polls the machine's panes for agent statuses, by pane id.
fn spawn_poller(machine: String, events: Sender<Event>) {
    thread::spawn(move || loop {
        let statuses = herdr::json(&["pane", "list"], Some(&machine))
            .map(|listed| statuses(&listed))
            .map_err(|_| format!("{machine} not answering"));
        if events.send(Event::Statuses(statuses)).is_err() {
            return;
        }
        thread::sleep(POLL);
    });
}

fn statuses(listed: &Value) -> HashMap<String, String> {
    listed["panes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|pane| {
            let id = pane["pane_id"].as_str()?.to_string();
            let agent = pane["agent"].as_str().unwrap_or_default();
            let status = if agent.is_empty() {
                "no agent"
            } else {
                pane["agent_status"].as_str().unwrap_or("unknown")
            };
            Some((id, status.to_string()))
        })
        .collect()
}

fn draw(frame: &mut Frame, herd: &Herd, machine: &str, workspace: &str, error: &str) {
    let mut lines = vec![
        Line::from(vec![
            Span::from(format!(" {MARK} remuda herd ")).bold(),
            Span::from(format!(
                "· {machine} {workspace} · seats {}/{}",
                herd.seated(),
                herd.max_seats
            ))
            .dim(),
        ]),
        Line::from(""),
    ];
    let waiting = herd.queue();
    for (index, member) in herd.members.iter().enumerate() {
        let style = match member.status.as_str() {
            "blocked" | "startup" => Style::new().red().bold(),
            "working" => Style::new().yellow(),
            "idle" | "done" => Style::new().green(),
            _ => Style::new().dim(),
        };
        let place = if member.seat.is_some() {
            "▸ open".to_string()
        } else if let Some(position) = waiting.iter().position(|i| *i == index) {
            format!("waiting #{}", position + 1)
        } else if member.waived {
            "closed until it asks again".into()
        } else {
            String::new()
        };
        let mut spans = vec![
            Span::from(format!("  {:<16} ", truncate(&member.name, 16))),
            Span::styled(format!("{:<9} ", member.status), style),
            Span::from(place).bold(),
        ];
        if !member.note.is_empty() {
            spans.push(Span::from(format!("  {}", member.note)).red());
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(""));
    if !error.is_empty() {
        lines.push(Line::from(format!(" {error}")).red());
    }
    lines.push(Line::from(" q quit (agents keep running)").dim());
    frame.render_widget(Paragraph::new(lines), frame.area());
}

fn truncate(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn herd(count: usize) -> Herd {
        Herd {
            members: (1..=count)
                .map(|n| Member::new(format!("herd-{n}"), format!("w1:p{n}")))
                .collect(),
            max_seats: 3,
        }
    }

    fn statuses(list: &[(&str, &str)]) -> HashMap<String, String> {
        list.iter()
            .map(|(pane, status)| (pane.to_string(), status.to_string()))
            .collect()
    }

    fn open(list: &[&str]) -> HashSet<String> {
        list.iter().map(|pane| pane.to_string()).collect()
    }

    #[test]
    fn blocked_agents_queue_longest_waiting_first_and_notify_once() {
        let mut herd = herd(3);
        let start = Instant::now();
        let later = start + Duration::from_secs(1);
        let first = herd.observe(&statuses(&[("w1:p2", "blocked")]), &open(&[]), start);
        assert_eq!(first, [Action::Notify(1)]);
        let second = herd.observe(
            &statuses(&[("w1:p1", "blocked"), ("w1:p2", "blocked")]),
            &open(&[]),
            later,
        );
        assert_eq!(second, [Action::Notify(0)]);
        assert_eq!(herd.queue(), [1, 0]);
    }

    #[test]
    fn a_seat_closes_once_its_agent_stays_out_of_blocked() {
        let mut herd = herd(1);
        let now = Instant::now();
        herd.observe(&statuses(&[("w1:p1", "blocked")]), &open(&[]), now);
        herd.members[0].seat = Some("wA:p9".into());
        let seated = open(&["wA:p9"]);
        let working = statuses(&[("w1:p1", "working")]);
        assert_eq!(herd.observe(&working, &seated, now), []);
        assert_eq!(herd.observe(&working, &seated, now), [Action::Close(0)]);
    }

    #[test]
    fn a_flicker_back_to_blocked_keeps_the_seat() {
        let mut herd = herd(1);
        let now = Instant::now();
        herd.members[0].seat = Some("wA:p9".into());
        let seated = open(&["wA:p9"]);
        herd.observe(&statuses(&[("w1:p1", "working")]), &seated, now);
        herd.observe(&statuses(&[("w1:p1", "blocked")]), &seated, now);
        assert_eq!(
            herd.observe(&statuses(&[("w1:p1", "working")]), &seated, now),
            []
        );
    }

    #[test]
    fn a_shell_run_agent_silent_at_startup_needs_input_until_it_reports() {
        let mut herd = herd(1);
        let start = Instant::now();
        herd.members[0].launched = Some(start);
        let silent = statuses(&[("w1:p1", "no agent")]);
        assert_eq!(herd.observe(&silent, &open(&[]), start), []);
        let later = start + UNREPORTED_AFTER;
        assert_eq!(
            herd.observe(&silent, &open(&[]), later),
            [Action::Notify(0)]
        );
        assert_eq!(herd.members[0].status, "startup");
        herd.members[0].seat = Some("wA:p9".into());
        let idle = statuses(&[("w1:p1", "idle")]);
        assert_eq!(
            herd.observe(&idle, &open(&["wA:p9"]), later),
            [Action::Reported(0)]
        );
        assert_eq!(herd.members[0].launched, None);
        assert_eq!(
            herd.observe(&idle, &open(&["wA:p9"]), later),
            [Action::Close(0)]
        );
    }

    #[test]
    fn a_gone_agent_closes_its_seat_at_once() {
        let mut herd = herd(1);
        herd.members[0].seat = Some("wA:p9".into());
        let actions = herd.observe(&statuses(&[]), &open(&["wA:p9"]), Instant::now());
        assert_eq!(actions, [Action::Close(0)]);
        assert_eq!(herd.members[0].status, "gone");
    }

    #[test]
    fn a_seat_the_user_closed_waits_until_the_agent_asks_again() {
        let mut herd = herd(1);
        let now = Instant::now();
        let blocked = statuses(&[("w1:p1", "blocked")]);
        herd.observe(&blocked, &open(&[]), now);
        herd.members[0].seat = Some("wA:p9".into());
        herd.observe(&blocked, &open(&[]), now);
        assert_eq!(herd.members[0].seat, None);
        assert!(herd.queue().is_empty());
        herd.observe(&statuses(&[("w1:p1", "working")]), &open(&[]), now);
        assert_eq!(herd.observe(&blocked, &open(&[]), now), [Action::Notify(0)]);
        assert_eq!(herd.queue(), [0]);
    }

    fn layout(panes: &[(&str, u64, u64)]) -> Value {
        json!({"panes": panes.iter().map(|(id, width, height)| json!({
            "pane_id": id, "rect": {"width": width, "height": height},
        })).collect::<Vec<_>>()})
    }

    #[test]
    fn seats_split_right_while_columns_stay_wide_then_down() {
        let ours = open(&["b", "s1", "s2"]);
        let wide = layout(&[("b", 163, 51)]);
        assert_eq!(place(&wide, &ours), Some(("b".into(), Direction::Right)));
        let columns = layout(&[("b", 82, 51), ("s1", 81, 51)]);
        assert_eq!(place(&columns, &ours), Some(("b".into(), Direction::Down)));
        let full = layout(&[("b", 82, 25), ("s2", 82, 25), ("s1", 81, 25)]);
        assert_eq!(place(&full, &ours), None);
    }

    #[test]
    fn seats_never_split_the_users_other_panes() {
        let ours = open(&["b"]);
        let tab = layout(&[("b", 70, 20), ("editor", 200, 60)]);
        assert_eq!(place(&tab, &ours), None);
    }

    #[test]
    fn names_skip_live_agents() {
        let taken = HashSet::from(["herd-1", "herd-3"]);
        assert_eq!(
            pick_names("herd", 3, &taken),
            ["herd-2", "herd-4", "herd-5"]
        );
    }

    #[test]
    fn options_need_a_machine_and_pass_agent_args_through() {
        let args = |list: &[&str]| list.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        assert!(Options::parse(&args(&["--count", "2"])).is_err());
        let options =
            Options::parse(&args(&["--machine", "ziggy", "--", "--model", "opus"])).unwrap();
        assert_eq!(options.count, 3);
        assert_eq!(options.agent_args, ["--model", "opus"]);
        assert!(Options::parse(&args(&["--machine", "ziggy", "--seats", "0"])).is_err());
    }
}
