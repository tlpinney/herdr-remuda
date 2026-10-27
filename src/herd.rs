//! The herd board: agents on remote machines that come to you when they need
//! input.
//!
//! `remuda herd` turns the pane it runs in into a board of every herd this
//! machine watches (see `herds`). When an agent turns `blocked`, the board
//! opens a driven mirror of it (a seat) in this tab and shows a toast; once
//! the agent is working again the seat closes, freeing it for the next one.
//! Seats split this tab right while the columns stay wide enough, then down,
//! so the board stays in the top-left corner. Membership is read from each
//! herd's workspace on every poll, so agents added to or removed from a herd
//! come and go without restarting the board.

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

use crate::config::Config;
use crate::herdr::{self, MARK};
use crate::herds::{self, Seen};

const POLL: Duration = Duration::from_secs(1);
const TICK: Duration = Duration::from_millis(200);
/// Polls in a row an agent must be out of `blocked` before its seat closes, so
/// a flicker between dialogs does not close it.
const CALM_POLLS: u8 = 2;
/// The smallest grid cell the default grid uses; a smaller seat would wrap
/// the agent's UI.
const MIN_COLS: u64 = 70;
const MIN_ROWS: u64 = 24;
/// Rows the default grid has at most.
const MAX_ROWS: u64 = 2;
/// How long an agent run in the shell may stay unreported before the board
/// treats it as waiting at a startup prompt.
const UNREPORTED_AFTER: u64 = 8;

/// Why an agent wants a seat. The order is the queue's: questions before
/// finished work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Reason {
    /// Waiting on a question or an approval (or a startup screen).
    Blocked,
    /// Finished a turn nobody has looked at yet.
    Review,
}

impl Reason {
    fn of(status: &str) -> Option<Reason> {
        match status {
            "blocked" | "startup" => Some(Reason::Blocked),
            "done" => Some(Reason::Review),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Reason::Blocked => "needs input",
            Reason::Review => "finished · review",
        }
    }
}

struct Member {
    machine: String,
    /// The agent's pane on `machine`.
    pane: String,
    /// The herd's `name@machine`.
    herd: String,
    name: String,
    status: String,
    note: String,
    /// What the agent is doing, from its terminal title.
    context: String,
    /// Why it wants a seat now, and since when; orders the queue.
    reason: Option<Reason>,
    since: Option<Instant>,
    /// The local mirror pane showing this agent.
    seat: Option<String>,
    calm: u8,
    /// The user closed this agent's seat while it wanted one: leave it until
    /// it wants one for another reason, or again after not wanting one.
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
            context: seen.title.clone(),
            reason: None,
            since: None,
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
    /// This member just started wanting a seat, or for a new reason.
    Notify(usize),
    /// This member, run in the shell, just reported itself to herdr.
    Reported(usize),
}

/// The seating rules, apart from herdr so they can be tested.
struct Board {
    members: Vec<Member>,
    /// Seats the grid has room for, within `cap`; set from the layout.
    max_seats: usize,
    cap: Option<usize>,
    /// A fixed grid; one that fits the anchor's area when `None`.
    grid: Option<Grid>,
    /// The grid in use, for the board's header.
    shown_grid: Option<Grid>,
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
                    member.context = agent.title.clone();
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
            let want = Reason::of(&status);
            if member
                .seat
                .as_ref()
                .is_some_and(|seat| !open.contains(seat))
            {
                // Closed by the user (ctrl+]).
                member.seat = None;
                member.calm = 0;
                member.waived = want.is_some();
            }
            match want {
                Some(reason) => {
                    member.calm = 0;
                    // A review that turns into a question keeps its seat.
                    if member.reason != Some(reason) {
                        member.reason = Some(reason);
                        member.since = Some(now);
                        member.waived = false;
                        actions.push(Action::Notify(index));
                    }
                }
                None => {
                    member.reason = None;
                    member.since = None;
                    member.waived = false;
                    if member.seat.is_some() {
                        member.calm += 1;
                        if member.calm >= CALM_POLLS || status == "gone" {
                            actions.push(Action::Close(index));
                        }
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

    /// Members waiting for a seat: questions first, then finished work, each
    /// longest-waiting first.
    fn queue(&self) -> Vec<usize> {
        let mut waiting: Vec<(Reason, Instant, usize)> = self
            .members
            .iter()
            .enumerate()
            .filter(|(_, m)| m.seat.is_none() && !m.waived)
            .filter_map(|(index, m)| Some((m.reason?, m.since?, index)))
            .collect();
        waiting.sort();
        waiting.into_iter().map(|(_, _, index)| index).collect()
    }

    fn seated(&self) -> usize {
        self.members.iter().filter(|m| m.seat.is_some()).count()
    }

    /// The anchor and the open seats.
    fn ours(&self, anchor: &str) -> HashSet<String> {
        let mut ours: HashSet<String> =
            self.members.iter().filter_map(|m| m.seat.clone()).collect();
        ours.insert(anchor.to_string());
        ours
    }

    /// Works out the grid and how many seats it has room for from `layout`.
    fn size_up(&mut self, layout: &Value, anchor: &str) {
        let Some(grid) = grid_for(layout, &self.ours(anchor), self.grid) else {
            return;
        };
        self.shown_grid = Some(grid);
        self.max_seats = grid.seats().min(self.cap.unwrap_or(usize::MAX));
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

/// How seats tile the area of the pane they open beside: that pane takes the
/// top-left cell and seats the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grid {
    rows: u64,
    cols: u64,
}

impl Grid {
    /// The grid that fits an area: as many `MIN_COLS`-wide columns as fit,
    /// and up to `MAX_ROWS` rows `MIN_ROWS` tall, with room for one seat at
    /// least.
    fn fit(width: u64, height: u64) -> Grid {
        let rows = (height / MIN_ROWS).clamp(1, MAX_ROWS);
        let mut cols = (width / MIN_COLS).max(1);
        if rows * cols == 1 {
            cols = 2;
        }
        Grid { rows, cols }
    }

    fn seats(self) -> usize {
        (self.rows * self.cols - 1) as usize
    }
}

impl std::str::FromStr for Grid {
    type Err = String;

    /// `ROWSxCOLS`, e.g. `2x4`.
    fn from_str(text: &str) -> Result<Grid, String> {
        let bad = || format!("grid must be ROWSxCOLS with room for a seat, like 2x4, not {text:?}");
        let (rows, cols) = text.trim().split_once(['x', 'X']).ok_or_else(bad)?;
        let rows: u64 = rows.trim().parse().map_err(|_| bad())?;
        let cols: u64 = cols.trim().parse().map_err(|_| bad())?;
        if rows == 0 || cols == 0 || rows * cols < 2 {
            return Err(bad());
        }
        Ok(Grid { rows, cols })
    }
}

impl std::fmt::Display for Grid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}", self.rows, self.cols)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    x: u64,
    y: u64,
    w: u64,
    h: u64,
}

impl Rect {
    fn from_value(rect: &Value) -> Option<Rect> {
        Some(Rect {
            x: rect["x"].as_u64()?,
            y: rect["y"].as_u64()?,
            w: rect["width"].as_u64()?,
            h: rect["height"].as_u64()?,
        })
    }
}

/// The anchor and seats (`ours`) in a `pane layout`, with their rects.
fn our_panes(layout: &Value, ours: &HashSet<String>) -> Vec<(String, Rect)> {
    layout["panes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|pane| {
            let id = pane["pane_id"].as_str()?;
            ours.contains(id)
                .then(|| Some((id.to_string(), Rect::from_value(&pane["rect"])?)))?
        })
        .collect()
}

/// The area the anchor and its seats share: the box around them.
fn region(panes: &[(String, Rect)]) -> Option<Rect> {
    let x = panes.iter().map(|(_, r)| r.x).min()?;
    let y = panes.iter().map(|(_, r)| r.y).min()?;
    let right = panes.iter().map(|(_, r)| r.x + r.w).max()?;
    let bottom = panes.iter().map(|(_, r)| r.y + r.h).max()?;
    Some(Rect {
        x,
        y,
        w: right - x,
        h: bottom - y,
    })
}

/// The grid seats use: `fixed` (from `--grid` or the config), or the one that
/// fits the shared area.
fn grid_for(layout: &Value, ours: &HashSet<String>, fixed: Option<Grid>) -> Option<Grid> {
    match fixed {
        Some(grid) => Some(grid),
        None => region(&our_panes(layout, ours)).map(|area| Grid::fit(area.w, area.h)),
    }
}

/// A split that opens the next seat: `target` split `direction`, its first
/// part keeping `ratio` of it so the new boundary lands on a grid line.
#[derive(Debug, PartialEq)]
struct Placement {
    target: String,
    rect: Rect,
    direction: Direction,
    ratio: f64,
}

/// Where the next seat goes. Cells fill row by row from the top left, so the
/// anchor's row fills first with side-by-side (vertical) splits and the next
/// row with stacked ones. Each pane owns its top-left cell; the first other
/// cell is taken from the pane covering it, split side by side when the cell
/// is to its right, stacked when it is below. `None` when every cell is a
/// pane.
fn place(layout: &Value, ours: &HashSet<String>, grid: Grid) -> Option<Placement> {
    let panes = our_panes(layout, ours);
    let area = region(&panes)?;
    let cell_w = area.w as f64 / grid.cols as f64;
    let cell_h = area.h as f64 / grid.rows as f64;
    // The cells a pane covers: rows r0..r1, columns c0..c1.
    let span = |rect: &Rect| {
        let at =
            |offset: u64, size: f64, cells: u64| ((offset as f64 / size).round() as u64).min(cells);
        let c0 = at(rect.x - area.x, cell_w, grid.cols);
        let c1 = at(rect.x + rect.w - area.x, cell_w, grid.cols).max(c0 + 1);
        let r0 = at(rect.y - area.y, cell_h, grid.rows);
        let r1 = at(rect.y + rect.h - area.y, cell_h, grid.rows).max(r0 + 1);
        (r0, c0, r1, c1)
    };
    for row in 0..grid.rows {
        for col in 0..grid.cols {
            let covering = panes.iter().find(|(_, rect)| {
                let (r0, c0, r1, c1) = span(rect);
                (r0..r1).contains(&row) && (c0..c1).contains(&col)
            });
            let Some((id, rect)) = covering else {
                continue;
            };
            let (r0, c0, r1, c1) = span(rect);
            // A pane owns its top-left cell; the others it covers are free.
            if (row, col) == (r0, c0) {
                continue;
            }
            let (direction, ratio) = if col > c0 {
                (Direction::Right, (col - c0) as f64 / (c1 - c0) as f64)
            } else {
                (Direction::Down, (row - r0) as f64 / (r1 - r0) as f64)
            };
            return Some(Placement {
                target: id.clone(),
                rect: *rect,
                direction,
                ratio,
            });
        }
    }
    None
}

/// Moves the split a seat just made onto its grid line: herdr splits in half,
/// and `pane resize` moves a split by a fraction of its size.
fn align(placement: &Placement, seat: &str) {
    let Ok(mut layout) = herdr::json(&["pane", "layout", "--pane", seat], None) else {
        return;
    };
    let layout = layout["layout"].take();
    let split = layout["splits"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|split| {
            split["direction"] == placement.direction.as_str()
                && Rect::from_value(&split["rect"]) == Some(placement.rect)
        });
    let Some(current) = split.and_then(|split| split["ratio"].as_f64()) else {
        return;
    };
    let delta = placement.ratio - current;
    if delta.abs() < 0.01 {
        return;
    }
    // Grow the first part towards the second, or the second towards the first.
    let (pane, direction) = match (delta > 0.0, placement.direction) {
        (true, Direction::Right) => (placement.target.as_str(), "right"),
        (true, Direction::Down) => (placement.target.as_str(), "down"),
        (false, Direction::Right) => (seat, "left"),
        (false, Direction::Down) => (seat, "up"),
    };
    let amount = format!("{:.4}", delta.abs());
    herdr::run_quietly(&[
        "pane",
        "resize",
        "--pane",
        pane,
        "--direction",
        direction,
        "--amount",
        &amount,
    ]);
}

/// `remuda herd …`: the board, a herd command, or the original one-shot form
/// (`remuda herd --machine M …`), which starts or adopts a herd and watches it.
pub fn run(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        None => board(BoardOptions::default()),
        Some("board") => board(BoardOptions::parse(&args[1..])?),
        Some("--seats" | "--beside" | "--grid") => board(BoardOptions::parse(args)?),
        Some(flag) if flag.starts_with("--") => one_shot(args),
        Some(command) => herds::command(command, &args[1..]),
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct BoardOptions {
    /// At most this many seats; the grid's cells otherwise.
    seats: Option<usize>,
    /// The grid seats tile; the config's `grid`, or one that fits, otherwise.
    grid: Option<Grid>,
    /// The pane seats open beside, in its own tab; the board's pane when
    /// unset.
    beside: Option<String>,
}

impl BoardOptions {
    /// `[--seats N] [--grid ROWSxCOLS] [--beside PANE]`.
    fn parse(args: &[String]) -> Result<BoardOptions, String> {
        let mut options = BoardOptions::default();
        let mut args = args.iter();
        while let Some(flag) = args.next() {
            let value = args
                .next()
                .ok_or_else(|| format!("{flag} needs a value\n{}", herds::USAGE))?;
            match flag.as_str() {
                "--seats" => options.seats = Some(parse_seats(value)?),
                "--grid" => options.grid = Some(value.parse()?),
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
    let mut seats = None;
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
            "--seats" => seats = Some(parse_seats(&value)?),
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
        ..BoardOptions::default()
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
    let grid = match options.grid {
        Some(grid) => Some(grid),
        None => match Config::load()?.grid {
            Some(grid) => Some(grid.parse()?),
            None => None,
        },
    };
    let (events, inbox) = mpsc::channel();
    spawn_poller(events);
    let mut terminal = ratatui::init();
    let mut state = Board {
        members: Vec::new(),
        max_seats: 0,
        cap: options.seats,
        grid,
        shown_grid: None,
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
            state.size_up(&layout, anchor);
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
        state.size_up(&layout, anchor);
        if state.seated() >= state.max_seats {
            return;
        }
        let ours = state.ours(anchor);
        let Some(grid) = state.shown_grid else {
            return;
        };
        let Some(placement) = place(&layout, &ours, grid) else {
            return;
        };
        // Take focus only from the board, never from a seat being typed in
        // or the pane seats open beside.
        let focus = layout["focused_pane_id"] == board;
        let opened = open_seat(
            &state.members[index],
            &placement.target,
            placement.direction,
            focus,
        );
        match opened {
            Ok(seat) => {
                align(&placement, &seat);
                state.members[index].seat = Some(seat);
            }
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
    let note = format!("REMUDA_NOTE={}", seat_note(member));
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
        "--env",
        &note,
    ];
    let result = herdr::json(&args, None)?;
    result["plugin_pane"]["pane"]["pane_id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "herdr did not say which pane it opened".into())
}

/// What a seat's bar says about why it opened: `finished · review: Fix the
/// login test`.
fn seat_note(member: &Member) -> String {
    let reason = member.reason.map(Reason::label).unwrap_or_default();
    match truncate(&member.context, 60) {
        context if context.is_empty() => reason.to_string(),
        context => format!("{reason}: {context}"),
    }
}

fn notify(member: &Member) {
    let (title, sound) = match member.reason {
        Some(Reason::Review) => (format!("{} finished", member.name), "done"),
        _ => (format!("{} needs input", member.name), "request"),
    };
    let place = match member.seat {
        Some(_) => "opened beside the board",
        None => "waiting for a free seat",
    };
    let body = match truncate(&member.context, 60) {
        context if context.is_empty() => format!("{} · {place}", member.herd),
        context => format!("{} · {context} · {place}", member.herd),
    };
    thread::spawn(move || {
        herdr::run_quietly(&[
            "notification",
            "show",
            &title,
            "--body",
            &body,
            "--sound",
            sound,
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
            Span::from(format!(
                "· seats {}/{}{}",
                state.seated(),
                state.max_seats,
                state
                    .shown_grid
                    .map(|grid| format!(" · grid {grid}"))
                    .unwrap_or_default()
            ))
            .dim(),
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
        "done" => Style::new().cyan().bold(),
        "working" => Style::new().yellow(),
        "idle" => Style::new().green(),
        _ => Style::new().dim(),
    };
    let reason = member.reason.map(Reason::label).unwrap_or_default();
    let place = if member.seat.is_some() {
        format!("▸ open · {reason}")
    } else if let Some(position) = waiting {
        format!("waiting #{} · {reason}", position + 1)
    } else if member.waived {
        match member.reason {
            Some(Reason::Review) => "reviewed".into(),
            _ => "closed until it asks again".into(),
        }
    } else {
        String::new()
    };
    let mut spans = vec![
        Span::from(format!("  {:<16} ", truncate(&member.name, 16))),
        Span::styled(format!("{:<9} ", member.status), style),
        Span::from(place).bold(),
    ];
    if !member.context.is_empty() {
        spans.push(Span::from(format!("  {}", truncate(&member.context, 50))).dim());
    }
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
            title: String::new(),
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
            cap: None,
            grid: None,
            shown_grid: None,
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
    fn finished_agents_wait_behind_questions_for_review() {
        let mut board = board();
        let now = Instant::now();
        let first = board.observe(&poll(vec![seen("w1:p1", "done")]), &open(&[]), now, 0);
        assert_eq!(first, [Action::Notify(0)]);
        assert_eq!(board.members[0].reason, Some(Reason::Review));
        let later = now + Duration::from_secs(5);
        let both = poll(vec![seen("w1:p1", "done"), seen("w1:p2", "blocked")]);
        assert_eq!(
            board.observe(&both, &open(&[]), later, 0),
            [Action::Notify(1)]
        );
        // The question was asked later, but goes first.
        assert_eq!(board.queue(), [1, 0]);
    }

    #[test]
    fn a_review_seat_closes_on_new_work_and_stays_for_a_question() {
        let mut board = board();
        let now = Instant::now();
        board.observe(&poll(vec![seen("w1:p1", "done")]), &open(&[]), now, 0);
        board.members[0].seat = Some("wA:p9".into());
        let seated = open(&["wA:p9"]);
        // The agent asks something while you review: same seat, new reason.
        let asks = board.observe(&poll(vec![seen("w1:p1", "blocked")]), &seated, now, 0);
        assert_eq!(asks, [Action::Notify(0)]);
        assert_eq!(board.members[0].reason, Some(Reason::Blocked));
        let working = poll(vec![seen("w1:p1", "working")]);
        assert_eq!(board.observe(&working, &seated, now, 0), []);
        assert_eq!(board.observe(&working, &seated, now, 0), [Action::Close(0)]);
    }

    #[test]
    fn a_review_closed_by_hand_stays_closed_until_the_next_finish() {
        let mut board = board();
        let now = Instant::now();
        let done = poll(vec![seen("w1:p1", "done")]);
        board.observe(&done, &open(&[]), now, 0);
        board.members[0].seat = Some("wA:p9".into());
        board.observe(&done, &open(&[]), now, 0);
        assert!(board.members[0].waived && board.queue().is_empty());
        board.observe(&poll(vec![seen("w1:p1", "working")]), &open(&[]), now, 0);
        assert_eq!(
            board.observe(&done, &open(&[]), now, 0),
            [Action::Notify(0)]
        );
        assert_eq!(board.queue(), [0]);
    }

    #[test]
    fn seat_bars_say_why_they_opened() {
        let mut board = board();
        let mut agent = seen("w1:p1", "done");
        agent.title = "Fix the login test".into();
        board.observe(&poll(vec![agent]), &open(&[]), Instant::now(), 0);
        assert_eq!(
            seat_note(&board.members[0]),
            "finished · review: Fix the login test"
        );
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

    /// A layout of `(id, x, y, width, height)` panes.
    fn layout(panes: &[(&str, u64, u64, u64, u64)]) -> Value {
        json!({"panes": panes.iter().map(|(id, x, y, w, h)| json!({
            "pane_id": id, "rect": {"x": x, "y": y, "width": w, "height": h},
        })).collect::<Vec<_>>()})
    }

    fn placed(
        layout: &Value,
        ours: &HashSet<String>,
        grid: &str,
    ) -> Option<(String, Direction, f64)> {
        place(layout, ours, grid.parse().unwrap())
            .map(|p| (p.target, p.direction, (p.ratio * 1000.0).round() / 1000.0))
    }

    #[test]
    fn the_default_grid_fits_the_area() {
        assert_eq!(Grid::fit(163, 53), "2x2".parse().unwrap());
        assert_eq!(Grid::fit(330, 60), "2x4".parse().unwrap());
        assert_eq!(Grid::fit(240, 80), "2x3".parse().unwrap());
        assert_eq!(Grid::fit(163, 30), "1x2".parse().unwrap());
        // Always room for one seat, however small.
        assert_eq!(Grid::fit(60, 20), "1x2".parse().unwrap());
        assert_eq!(Grid::fit(330, 60).seats(), 7);
    }

    #[test]
    fn grids_parse_as_rows_by_columns() {
        assert_eq!("2x4".parse::<Grid>().unwrap(), Grid { rows: 2, cols: 4 });
        assert_eq!(" 1X3 ".parse::<Grid>().unwrap(), Grid { rows: 1, cols: 3 });
        for bad in ["1x1", "0x4", "2", "2x", "axb"] {
            assert!(bad.parse::<Grid>().is_err(), "{bad}");
        }
    }

    #[test]
    fn a_2x2_grid_fills_the_anchors_row_first() {
        let ours = open(&["a", "s1", "s2", "s3"]);
        let alone = layout(&[("a", 0, 0, 163, 53)]);
        assert_eq!(
            placed(&alone, &ours, "2x2"),
            Some(("a".into(), Direction::Right, 0.5))
        );
        let row = layout(&[("a", 0, 0, 82, 53), ("s1", 82, 0, 81, 53)]);
        assert_eq!(
            placed(&row, &ours, "2x2"),
            Some(("a".into(), Direction::Down, 0.5))
        );
        let three = layout(&[
            ("a", 0, 0, 82, 26),
            ("s2", 0, 26, 82, 27),
            ("s1", 82, 0, 81, 53),
        ]);
        assert_eq!(
            placed(&three, &ours, "2x2"),
            Some(("s1".into(), Direction::Down, 0.5))
        );
        let full = layout(&[
            ("a", 0, 0, 82, 26),
            ("s2", 0, 26, 82, 27),
            ("s1", 82, 0, 81, 26),
            ("s3", 82, 26, 81, 27),
        ]);
        assert_eq!(placed(&full, &ours, "2x2"), None);
    }

    #[test]
    fn a_wide_grid_makes_even_columns() {
        let ours = open(&["a", "s1", "s2"]);
        let alone = layout(&[("a", 0, 0, 330, 60)]);
        assert_eq!(
            placed(&alone, &ours, "2x4"),
            Some(("a".into(), Direction::Right, 0.25))
        );
        // The seat covers the other three columns; a third of it stays.
        let one = layout(&[("a", 0, 0, 82, 60), ("s1", 82, 0, 248, 60)]);
        assert_eq!(
            placed(&one, &ours, "2x4"),
            Some(("s1".into(), Direction::Right, 0.333))
        );
        let two = layout(&[
            ("a", 0, 0, 82, 60),
            ("s1", 82, 0, 83, 60),
            ("s2", 165, 0, 165, 60),
        ]);
        assert_eq!(
            placed(&two, &ours, "2x4"),
            Some(("s2".into(), Direction::Right, 0.5))
        );
    }

    #[test]
    fn a_closed_seat_is_filled_again() {
        // s1 (top right) closed and s3 below it took its column.
        let ours = open(&["a", "s2", "s3"]);
        let gap = layout(&[
            ("a", 0, 0, 82, 26),
            ("s2", 0, 26, 82, 27),
            ("s3", 82, 0, 81, 53),
        ]);
        assert_eq!(
            placed(&gap, &ours, "2x2"),
            Some(("s3".into(), Direction::Down, 0.5))
        );
    }

    #[test]
    fn seats_never_split_the_users_other_panes() {
        let ours = open(&["a"]);
        let tab = layout(&[("a", 0, 0, 82, 53), ("editor", 82, 0, 81, 53)]);
        // The anchor alone is one column wide; its grid is the anchor's area.
        let placement = placed(&tab, &ours, "1x2").unwrap();
        assert_eq!(placement.0, "a");
    }

    #[test]
    fn board_options_take_seats_and_a_pane_to_open_them_beside() {
        let args = |list: &[&str]| list.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        assert_eq!(BoardOptions::parse(&[]).unwrap(), BoardOptions::default());
        let options = BoardOptions::parse(&args(&[
            "--beside", "wA:p7", "--seats", "1", "--grid", "2x4",
        ]))
        .unwrap();
        assert_eq!(
            options,
            BoardOptions {
                seats: Some(1),
                grid: Some(Grid { rows: 2, cols: 4 }),
                beside: Some("wA:p7".into())
            }
        );
        assert!(BoardOptions::parse(&args(&["--grid", "1x1"])).is_err());
        assert!(BoardOptions::parse(&args(&["--seats", "0"])).is_err());
        assert!(BoardOptions::parse(&args(&["--seats"])).is_err());
        assert!(BoardOptions::parse(&args(&["--bogus", "x"])).is_err());
    }
}
