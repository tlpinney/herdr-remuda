//! The herd board: agents on remote machines that come to you when they need
//! input.
//!
//! `remuda herd` turns the pane it runs in into a board of every herd this
//! machine watches (see `herds`). When an agent turns `blocked`, the board
//! opens a driven mirror of it (a seat) in this tab and shows a toast; once
//! the agent is working again the seat closes, freeing it for the next one.
//! Seats stack below the pane they open beside while the rows stay tall
//! enough, then go side by side, so that pane stays in the top-left corner.
//! Membership is read from each herd's workspace on every poll, so agents
//! added to or removed from a herd come and go without restarting the board.

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
use crate::herds::{self, Seen};

const POLL: Duration = Duration::from_secs(1);
const TICK: Duration = Duration::from_millis(200);
/// Polls in a row an agent must be out of `blocked` before its seat closes, so
/// a flicker between dialogs does not close it.
const CALM_POLLS: u8 = 2;
/// The smallest seat worth opening; smaller panes would wrap the agent's UI.
const MIN_COLS: u64 = 70;
const MIN_ROWS: u64 = 14;
/// How long an agent run in the shell may stay unreported before the board
/// treats it as waiting at a startup prompt.
const UNREPORTED_AFTER: u64 = 8;

struct Member {
    machine: String,
    /// The agent's pane on `machine`.
    pane: String,
    /// The herd's `name@machine`.
    herd: String,
    name: String,
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
    /// Already named after its first report, for agents run in the shell.
    reported: bool,
}

impl Member {
    fn new(machine: &str, herd: &str, seen: &Seen) -> Member {
        Member {
            machine: machine.to_string(),
            pane: seen.pane.clone(),
            herd: herd.to_string(),
            name: seen.name.clone(),
            status: "starting".into(),
            note: String::new(),
            blocked_since: None,
            seat: None,
            calm: 0,
            waived: false,
            reported: false,
        }
    }
}

/// One poll of every watched herd.
#[derive(Default)]
struct Poll {
    /// Herd `name@machine`, its machine, and an agent in it.
    seen: Vec<(String, String, Seen)>,
    /// Machines that answered.
    answered: HashSet<String>,
    /// Machines with a watched herd.
    watched: HashSet<String>,
    /// Herds in the registry, in order, for the board's sections.
    herds: Vec<String>,
    errors: Vec<String>,
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
struct Board {
    members: Vec<Member>,
    max_seats: usize,
}

impl Board {
    /// Takes in a poll and which local panes are still open. An agent missing
    /// from a machine that answered has left its herd (or the herd was
    /// forgotten); one on a machine that did not answer keeps its last state.
    fn observe(
        &mut self,
        poll: &Poll,
        open: &HashSet<String>,
        now: Instant,
        now_secs: u64,
    ) -> Vec<Action> {
        let seen: HashMap<(&str, &str), (&str, &Seen)> = poll
            .seen
            .iter()
            .map(|(herd, machine, seen)| {
                (
                    (machine.as_str(), seen.pane.as_str()),
                    (herd.as_str(), seen),
                )
            })
            .collect();
        // New agents join in the order their machine lists them.
        for (herd, machine, agent) in &poll.seen {
            let known = self
                .members
                .iter()
                .any(|m| m.machine == *machine && m.pane == agent.pane);
            if !known {
                self.members.push(Member::new(machine, herd, agent));
            }
        }

        let mut actions = Vec::new();
        for (index, member) in self.members.iter_mut().enumerate() {
            let found = seen.get(&(member.machine.as_str(), member.pane.as_str()));
            let mut status = match found {
                Some((herd, agent)) => {
                    member.herd = herd.to_string();
                    member.name = agent.name.clone();
                    agent.status.clone()
                }
                None if poll.watched.contains(&member.machine)
                    && !poll.answered.contains(&member.machine) =>
                {
                    continue
                }
                None => "gone".into(),
            };
            let launched = found.and_then(|(_, agent)| agent.launched);
            if status == "no agent" {
                if launched.is_some_and(|at| now_secs.saturating_sub(at) >= UNREPORTED_AFTER) {
                    status = "startup".into();
                }
            } else if launched.is_some() && status != "gone" && !member.reported {
                member.reported = true;
                actions.push(Action::Reported(index));
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

    /// Forgets members that left, once their seats are closed.
    fn prune(&mut self) {
        self.members
            .retain(|m| m.status != "gone" || m.seat.is_some());
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
/// or a seat (`ours`), never of the user's other panes. Stacking (a split
/// down) wins while both halves stay `MIN_ROWS` tall; then side by side (a
/// split right) while both stay `MIN_COLS` wide. `None` when the tab has no
/// room.
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
    let down = panes
        .iter()
        .filter(|(_, _, height)| *height >= 2 * MIN_ROWS)
        .max_by_key(|(_, width, height)| (*height, *width));
    if let Some((id, _, _)) = down {
        return Some((id.to_string(), Direction::Down));
    }
    panes
        .iter()
        .filter(|(_, width, _)| *width >= 2 * MIN_COLS)
        .max_by_key(|(_, width, height)| (*width, *height))
        .map(|(id, _, _)| (id.to_string(), Direction::Right))
}

/// `remuda herd …`: the board, a herd command, or the original one-shot form
/// (`remuda herd --machine M …`), which starts or adopts a herd and watches it.
pub fn run(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        None => board(BoardOptions::default()),
        Some("board") => board(BoardOptions::parse(&args[1..])?),
        Some("--seats" | "--beside") => board(BoardOptions::parse(args)?),
        Some(flag) if flag.starts_with("--") => one_shot(args),
        Some(command) => herds::command(command, &args[1..]),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct BoardOptions {
    seats: usize,
    /// The pane seats open beside, in its own tab; the board's pane when
    /// unset.
    beside: Option<String>,
}

impl Default for BoardOptions {
    fn default() -> BoardOptions {
        BoardOptions {
            seats: 3,
            beside: None,
        }
    }
}

impl BoardOptions {
    /// `[--seats N] [--beside PANE]`.
    fn parse(args: &[String]) -> Result<BoardOptions, String> {
        let mut options = BoardOptions::default();
        let mut args = args.iter();
        while let Some(flag) = args.next() {
            let value = args
                .next()
                .ok_or_else(|| format!("{flag} needs a value\n{}", herds::USAGE))?;
            match flag.as_str() {
                "--seats" => options.seats = parse_seats(value)?,
                "--beside" => options.beside = Some(value.clone()),
                _ => return Err(herds::USAGE.into()),
            }
        }
        Ok(options)
    }
}

fn parse_seats(value: &str) -> Result<usize, String> {
    value
        .parse()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| "--seats needs a positive number".into())
}

/// `remuda herd --machine M [--count N] [--kind K] [--cwd D] [--name H]
/// [--seats S] [--workspace W] [-- ARGS]`: starts herd `H` (default `herd`) on
/// `M`, or adopts workspace `W` as it, then shows the board.
fn one_shot(args: &[String]) -> Result<(), String> {
    let mut machine = None;
    let mut name = "herd".to_string();
    let mut seats = 3;
    let mut workspace = None;
    let mut start_args = Vec::new();
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        if flag == "--" {
            start_args.push(flag.clone());
            start_args.extend(args.by_ref().cloned());
            break;
        }
        let value = args
            .next()
            .ok_or_else(|| format!("{flag} needs a value"))?
            .clone();
        match flag.as_str() {
            "--machine" => machine = Some(value),
            "--name" => name = value,
            "--seats" => seats = parse_seats(&value)?,
            "--workspace" => workspace = Some(value),
            "--count" | "--kind" | "--cwd" => start_args.extend([flag.clone(), value]),
            _ => return Err(format!("unknown herd option {flag}\n{}", herds::USAGE)),
        }
    }
    let machine = machine.ok_or_else(|| format!("no machine: pass --machine\n{}", herds::USAGE))?;
    let target = format!("{name}@{machine}");
    let registered = herds::find(&name, &machine);
    match (&workspace, registered) {
        (Some(workspace), Some(herd)) if herd.workspace == *workspace => {}
        (Some(workspace), None) => herds::command("adopt", &[target, workspace.clone()])?,
        (None, None) => {
            let mut new_args = vec![target];
            new_args.extend(start_args);
            herds::command("new", &new_args)?;
        }
        (_, Some(_)) => {
            return Err(format!(
                "there is already a herd {target}; watch it with `remuda herd`, or add agents with `remuda herd add {target}`"
            ))
        }
    }
    board(BoardOptions {
        seats,
        beside: None,
    })
}

fn board(options: BoardOptions) -> Result<(), String> {
    let board = std::env::var("HERDR_PANE_ID")
        .map_err(|_| "the herd board runs in a herdr pane (HERDR_PANE_ID is not set)")?;
    // Seats split the anchor and each other: the board's own pane, or the
    // pane given with --beside, in whatever tab that is.
    let anchor = options.beside.unwrap_or_else(|| board.clone());
    herdr::json(&["pane", "get", &anchor], None)
        .map_err(|_| format!("no pane {anchor} here to open seats beside"))?;
    let max_seats = options.seats;
    let (events, inbox) = mpsc::channel();
    spawn_poller(events);
    let mut terminal = ratatui::init();
    let mut state = Board {
        members: Vec::new(),
        max_seats,
    };
    let result = watch(&mut state, &anchor, &board, &inbox, &mut terminal);
    ratatui::restore();
    for member in &state.members {
        if let Some(seat) = &member.seat {
            herdr::run_quietly(&["pane", "close", seat]);
        }
    }
    println!("{MARK} the herds keep running; `remuda herd` watches them again");
    result
}

fn watch(
    state: &mut Board,
    anchor: &str,
    board: &str,
    inbox: &Receiver<Poll>,
    terminal: &mut ratatui::DefaultTerminal,
) -> Result<(), String> {
    let mut last = Poll::default();
    let mut polled = false;
    loop {
        while let Ok(poll) = inbox.try_recv() {
            polled = true;
            let layout = herdr::json(&["pane", "layout", "--pane", anchor], None)
                .map(|mut result| result["layout"].take())
                .unwrap_or(Value::Null);
            let open: HashSet<String> = layout["panes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|pane| pane["pane_id"].as_str().map(str::to_string))
                .collect();
            let actions = state.observe(&poll, &open, Instant::now(), herds::now_secs());
            let mut fresh = Vec::new();
            for action in actions {
                match action {
                    Action::Close(index) => {
                        if let Some(seat) = state.members[index].seat.take() {
                            herdr::run_quietly(&["pane", "close", &seat]);
                        }
                        state.members[index].calm = 0;
                    }
                    Action::Notify(index) => fresh.push(index),
                    Action::Reported(index) => name_reported(&state.members[index]),
                }
            }
            seat_waiting(state, anchor, board);
            for index in fresh {
                notify(&state.members[index]);
            }
            state.prune();
            last = poll;
        }
        terminal
            .draw(|frame| draw(frame, state, &last, polled))
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

/// Names an agent run in the shell after its first report, so `herdr agent`
/// commands find it, and drops its launch tag.
fn name_reported(member: &Member) {
    let (machine, pane, name) = (
        member.machine.clone(),
        member.pane.clone(),
        member.name.clone(),
    );
    thread::spawn(move || {
        herdr::run_quietly_on(Some(&machine), &["agent", "rename", &pane, &name]);
        let _ = herds::tag(&machine, &pane, &[(herds::TOKEN_LAUNCHED, None)]);
    });
}

/// Opens seats for waiting members while there are free seats and room in
/// the anchor's tab.
fn seat_waiting(state: &mut Board, anchor: &str, board: &str) {
    for index in state.queue() {
        if state.seated() >= state.max_seats {
            return;
        }
        let Ok(mut layout) = herdr::json(&["pane", "layout", "--pane", anchor], None) else {
            return;
        };
        let layout = layout["layout"].take();
        let mut ours: HashSet<String> = state
            .members
            .iter()
            .filter_map(|m| m.seat.clone())
            .collect();
        ours.insert(anchor.to_string());
        let Some((target, direction)) = place(&layout, &ours) else {
            return;
        };
        // Take focus only from the board, never from a seat being typed in
        // or the pane seats open beside.
        let focus = layout["focused_pane_id"] == board;
        match open_seat(&state.members[index], &target, direction, focus) {
            Ok(seat) => state.members[index].seat = Some(seat),
            Err(err) => {
                state.members[index].note = err;
                return;
            }
        }
    }
}

fn open_seat(
    member: &Member,
    target: &str,
    direction: Direction,
    focus: bool,
) -> Result<String, String> {
    let plugin = herdr::plugin_id();
    let source = format!("REMUDA_SOURCE={}", member.pane);
    let machine = format!("REMUDA_MACHINE={}", member.machine);
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

fn notify(member: &Member) {
    let title = format!("{} needs input", member.name);
    let body = match member.seat {
        Some(_) => format!("{} · opened beside the board", member.herd),
        None => format!("{} · waiting for a free seat", member.herd),
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

/// Polls every watched herd once a second, one `pane list` per machine.
fn spawn_poller(events: Sender<Poll>) {
    thread::spawn(move || loop {
        let herds = herds::load();
        let mut poll = Poll {
            herds: herds.iter().map(herds::Herd::label).collect(),
            ..Poll::default()
        };
        let mut machines: Vec<&str> = herds.iter().map(|h| h.machine.as_str()).collect();
        machines.sort();
        machines.dedup();
        for machine in machines {
            poll.watched.insert(machine.to_string());
            let Ok(listed) = herdr::json(&["pane", "list"], Some(machine)) else {
                poll.errors.push(format!("{machine} not answering"));
                continue;
            };
            poll.answered.insert(machine.to_string());
            for herd in herds.iter().filter(|h| h.machine == machine) {
                for seen in herds::members(&listed, &herd.workspace) {
                    poll.seen.push((herd.label(), machine.to_string(), seen));
                }
            }
        }
        if events.send(poll).is_err() {
            return;
        }
        thread::sleep(POLL);
    });
}

fn draw(frame: &mut Frame, state: &Board, poll: &Poll, polled: bool) {
    let mut lines = vec![
        Line::from(vec![
            Span::from(format!(" {MARK} remuda herd ")).bold(),
            Span::from(format!("· seats {}/{}", state.seated(), state.max_seats)).dim(),
        ]),
        Line::from(""),
    ];
    if polled && poll.herds.is_empty() {
        lines.push(Line::from(
            "  no herds yet: remuda herd new <herd>@<machine> [--count 3] [--kind claude]",
        ));
    }
    let waiting = state.queue();
    for herd in &poll.herds {
        let count = state.members.iter().filter(|m| m.herd == *herd).count();
        lines.push(Line::from(vec![
            Span::from(format!(" {herd} ")).bold(),
            Span::from(format!("· {}", herds::count_agents(count))).dim(),
        ]));
        for (index, member) in state.members.iter().enumerate() {
            if member.herd != *herd {
                continue;
            }
            lines.push(member_line(
                member,
                waiting.iter().position(|i| *i == index),
            ));
        }
        lines.push(Line::from(""));
    }
    for error in &poll.errors {
        lines.push(Line::from(format!(" {error}")).red());
    }
    lines.push(Line::from(" q quit (agents keep running)").dim());
    frame.render_widget(Paragraph::new(lines), frame.area());
}

fn member_line(member: &Member, waiting: Option<usize>) -> Line<'static> {
    let style = match member.status.as_str() {
        "blocked" | "startup" => Style::new().red().bold(),
        "working" => Style::new().yellow(),
        "idle" | "done" => Style::new().green(),
        _ => Style::new().dim(),
    };
    let place = if member.seat.is_some() {
        "▸ open".to_string()
    } else if let Some(position) = waiting {
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
    Line::from(spans)
}

fn truncate(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const HERD: &str = "ops@ziggy";

    fn seen(pane: &str, status: &str) -> Seen {
        Seen {
            pane: pane.into(),
            name: format!("agent {pane}"),
            status: status.into(),
            launched: None,
        }
    }

    fn poll(agents: Vec<Seen>) -> Poll {
        Poll {
            seen: agents
                .into_iter()
                .map(|agent| (HERD.to_string(), "ziggy".to_string(), agent))
                .collect(),
            answered: HashSet::from(["ziggy".to_string()]),
            watched: HashSet::from(["ziggy".to_string()]),
            herds: vec![HERD.into()],
            errors: Vec::new(),
        }
    }

    fn board() -> Board {
        Board {
            members: Vec::new(),
            max_seats: 3,
        }
    }

    fn open(list: &[&str]) -> HashSet<String> {
        list.iter().map(|pane| pane.to_string()).collect()
    }

    #[test]
    fn agents_join_as_they_appear_and_blocked_ones_queue_in_order() {
        let mut board = board();
        let start = Instant::now();
        let first = board.observe(&poll(vec![seen("w1:p2", "blocked")]), &open(&[]), start, 0);
        assert_eq!(first, [Action::Notify(0)]);
        let later = start + Duration::from_secs(1);
        let both = poll(vec![seen("w1:p2", "blocked"), seen("w1:p1", "blocked")]);
        assert_eq!(
            board.observe(&both, &open(&[]), later, 0),
            [Action::Notify(1)]
        );
        assert_eq!(board.queue(), [0, 1]);
        assert_eq!(board.members[1].herd, HERD);
    }

    #[test]
    fn new_agents_join_in_the_order_their_machine_lists_them() {
        let mut board = board();
        let three = poll(vec![
            seen("w1:p1", "idle"),
            seen("w1:p2", "idle"),
            seen("w1:p3", "idle"),
        ]);
        board.observe(&three, &open(&[]), Instant::now(), 0);
        let panes: Vec<&str> = board.members.iter().map(|m| m.pane.as_str()).collect();
        assert_eq!(panes, ["w1:p1", "w1:p2", "w1:p3"]);
    }

    #[test]
    fn a_seat_closes_once_its_agent_stays_out_of_blocked() {
        let mut board = board();
        let now = Instant::now();
        board.observe(&poll(vec![seen("w1:p1", "blocked")]), &open(&[]), now, 0);
        board.members[0].seat = Some("wA:p9".into());
        let working = poll(vec![seen("w1:p1", "working")]);
        assert_eq!(board.observe(&working, &open(&["wA:p9"]), now, 0), []);
        assert_eq!(
            board.observe(&working, &open(&["wA:p9"]), now, 0),
            [Action::Close(0)]
        );
    }

    #[test]
    fn an_agent_that_leaves_its_herd_closes_its_seat_and_is_forgotten() {
        let mut board = board();
        let now = Instant::now();
        board.observe(&poll(vec![seen("w1:p1", "blocked")]), &open(&[]), now, 0);
        board.members[0].seat = Some("wA:p9".into());
        let actions = board.observe(&poll(vec![]), &open(&["wA:p9"]), now, 0);
        assert_eq!(actions, [Action::Close(0)]);
        board.members[0].seat = None;
        board.prune();
        assert!(board.members.is_empty());
    }

    #[test]
    fn a_machine_that_does_not_answer_keeps_its_agents() {
        let mut board = board();
        let now = Instant::now();
        board.observe(&poll(vec![seen("w1:p1", "blocked")]), &open(&[]), now, 0);
        let mut silent = poll(vec![]);
        silent.answered.clear();
        assert_eq!(board.observe(&silent, &open(&[]), now, 0), []);
        board.prune();
        assert_eq!(board.members[0].status, "blocked");
    }

    #[test]
    fn a_seat_the_user_closed_waits_until_the_agent_asks_again() {
        let mut board = board();
        let now = Instant::now();
        let blocked = poll(vec![seen("w1:p1", "blocked")]);
        board.observe(&blocked, &open(&[]), now, 0);
        board.members[0].seat = Some("wA:p9".into());
        board.observe(&blocked, &open(&[]), now, 0);
        assert_eq!(board.members[0].seat, None);
        assert!(board.queue().is_empty());
        board.observe(&poll(vec![seen("w1:p1", "working")]), &open(&[]), now, 0);
        assert_eq!(
            board.observe(&blocked, &open(&[]), now, 0),
            [Action::Notify(0)]
        );
        assert_eq!(board.queue(), [0]);
    }

    #[test]
    fn a_shell_run_agent_silent_at_startup_needs_input_until_it_reports() {
        let mut board = board();
        let now = Instant::now();
        let mut silent = seen("w1:p1", "no agent");
        silent.launched = Some(100);
        let early = board.observe(
            &poll(vec![silent]),
            &open(&[]),
            now,
            100 + UNREPORTED_AFTER - 1,
        );
        assert_eq!(early, []);
        let mut silent = seen("w1:p1", "no agent");
        silent.launched = Some(100);
        let late = board.observe(&poll(vec![silent]), &open(&[]), now, 100 + UNREPORTED_AFTER);
        assert_eq!(late, [Action::Notify(0)]);
        assert_eq!(board.members[0].status, "startup");
        let mut reported = seen("w1:p1", "idle");
        reported.launched = Some(100);
        let first = board.observe(&poll(vec![reported]), &open(&[]), now, 200);
        assert_eq!(first, [Action::Reported(0)]);
        let mut again = seen("w1:p1", "idle");
        again.launched = Some(100);
        assert_eq!(board.observe(&poll(vec![again]), &open(&[]), now, 201), []);
    }

    fn layout(panes: &[(&str, u64, u64)]) -> Value {
        json!({"panes": panes.iter().map(|(id, width, height)| json!({
            "pane_id": id, "rect": {"width": width, "height": height},
        })).collect::<Vec<_>>()})
    }

    #[test]
    fn seats_stack_while_rows_stay_tall_then_go_side_by_side() {
        let ours = open(&["b", "s1", "s2", "s3"]);
        let tall = layout(&[("b", 163, 51)]);
        assert_eq!(place(&tall, &ours), Some(("b".into(), Direction::Down)));
        let stacked = layout(&[("b", 163, 25), ("s1", 163, 26)]);
        assert_eq!(
            place(&stacked, &ours),
            Some(("s1".into(), Direction::Right))
        );
        let full = layout(&[
            ("b", 81, 25),
            ("s2", 82, 25),
            ("s1", 81, 26),
            ("s3", 82, 26),
        ]);
        assert_eq!(place(&full, &ours), None);
    }

    #[test]
    fn seats_never_split_the_users_other_panes() {
        let ours = open(&["b"]);
        let tab = layout(&[("b", 70, 20), ("editor", 200, 60)]);
        assert_eq!(place(&tab, &ours), None);
    }

    #[test]
    fn board_options_take_seats_and_a_pane_to_open_them_beside() {
        let args = |list: &[&str]| list.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        assert_eq!(BoardOptions::parse(&[]).unwrap(), BoardOptions::default());
        let options = BoardOptions::parse(&args(&["--beside", "wA:p7", "--seats", "1"])).unwrap();
        assert_eq!(
            options,
            BoardOptions {
                seats: 1,
                beside: Some("wA:p7".into())
            }
        );
        assert!(BoardOptions::parse(&args(&["--seats", "0"])).is_err());
        assert!(BoardOptions::parse(&args(&["--seats"])).is_err());
        assert!(BoardOptions::parse(&args(&["--bogus", "x"])).is_err());
    }
}
