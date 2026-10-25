//! The mirror pane: shows a source pane and optionally drives it.
//!
//! Display always comes from a read-only `terminal session observe` stream sized
//! to this pane, so looking at a source never resizes it. Control mode adds a
//! `terminal session control` stream for input, and the size it asks for is set
//! by the resize policy.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{json, Value};

use crate::config::Config;
use crate::geometry::{Size, SourceGeometry};
use crate::herdr::{self, Machine, StreamKind, MARK};
use crate::presence::{self, Presence};
use crate::report::{self, Report, Reporter};
use crate::spec::{self, Hints, MirrorSpec, Mode};

const DETACH: u8 = 0x1d; // ctrl+]
const LAYOUT_POLL: Duration = Duration::from_secs(1);
const STATUS_POLL: Duration = Duration::from_secs(2);
const RESIZE_SETTLE: Duration = Duration::from_millis(50);
const IDLE_TICK: Duration = Duration::from_millis(200);

/// Stream generations are unique for the whole process, so a frame from a
/// stream that belonged to an earlier connection is never drawn.
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

enum Event {
    Input(Vec<u8>),
    InputClosed,
    Resized,
    Frame {
        generation: u64,
        bytes: Vec<u8>,
    },
    ObserveEnded {
        generation: u64,
        note: String,
    },
    ControlEnded(String),
    NaturalSize(Size),
    SourceInfo(Value),
    SourceMissing(String),
    /// A background lookup found the source (possibly under a new pane id).
    SourceFound {
        pane: Value,
        machine: Option<Machine>,
        source: String,
    },
    SourceLookupFailed(String),
}

/// How a mirror's connection ended.
enum Exit {
    /// The user quit with ctrl+]: release the pane.
    Quit,
    /// The pane closed under us, e.g. at server shutdown: keep the resume command.
    Closed,
    /// The source stopped answering (network, sleep, a restarting machine):
    /// reconnect.
    Lost(String),
}

pub fn run(args: &[String]) -> Result<(), String> {
    let mut spec = MirrorSpec::from_args_or_env(args)?;
    let (events, inbox) = mpsc::channel();
    spawn_input_reader(events.clone());
    spawn_resize_watcher(events.clone())?;
    let screen = Screen::enter()?;
    let mut reporter = Reporter::start();
    let resumable = on_path("remuda");
    let mut presence = None;
    let mut lost: Option<String> = None;

    let outcome = loop {
        let waited = wait_for_source(
            &mut spec,
            &events,
            &inbox,
            &mut reporter,
            resumable,
            lost.as_deref(),
        );
        let (pane, machine) = match waited {
            Waited::Found { pane, machine } => (pane, machine),
            Waited::Quit => break Ok(Exit::Quit),
            Waited::Closed => break Ok(Exit::Closed),
        };
        spec.hints = Hints::from_pane(&pane);
        let exit = match Mirror::new(
            spec.clone(),
            machine,
            &pane,
            events.clone(),
            reporter.take(),
            resumable,
        ) {
            Ok(mut mirror) => {
                let exit = mirror.event_loop(&inbox);
                mirror.stop_streams();
                reporter = mirror.reporter.take();
                spec = mirror.spec.clone();
                // A reconnect marks the source again under the same metadata
                // source; until then the old mark's TTL covers the gap.
                presence = mirror.presence.take();
                exit
            }
            Err(err) => Ok(Exit::Lost(err)),
        };
        match exit {
            Ok(Exit::Lost(why)) => lost = Some(why),
            other => break other,
        }
    };
    drop(screen);
    // However the mirror ended, it no longer watches the source. A resumed
    // mirror marks it again.
    if let Some(presence) = presence {
        presence.clear();
    }
    if let (Ok(Exit::Quit), Some(reporter)) = (&outcome, reporter) {
        reporter.release();
    }
    outcome.map(|_| ())
}

/// Raw mode and the alternate screen, restored on drop.
struct Screen;

impl Screen {
    fn enter() -> Result<Screen, String> {
        crossterm::terminal::enable_raw_mode().map_err(|err| format!("raw mode: {err}"))?;
        write_out(b"\x1b[?1049h\x1b[2J");
        Ok(Screen)
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        write_out(b"\x1b[?1049l");
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

fn write_out(bytes: &[u8]) {
    let mut out = std::io::stdout();
    let _ = out.write_all(bytes);
    let _ = out.flush();
}

enum Waited {
    Found {
        pane: Value,
        machine: Option<Machine>,
    },
    Quit,
    Closed,
}

/// Delay before lookup attempt `attempt` (0-based) after a failure.
fn retry_delay(attempt: u32) -> Duration {
    const DELAYS: [u64; 5] = [1, 2, 4, 8, 15];
    Duration::from_secs(DELAYS[(attempt as usize).min(DELAYS.len() - 1)])
}

/// Finds the source, retrying with backoff while it is missing or its machine
/// does not answer, and shows why. `lost` is why the previous connection
/// ended, when this is a reconnect: the last frame then stays on screen under
/// a "reconnecting" status bar. Updates `spec.source` if the pane was found
/// again under a new id.
///
/// Lookups run on a background thread, so ctrl+] works while a machine hangs.
fn wait_for_source(
    spec: &mut MirrorSpec,
    events: &Sender<Event>,
    inbox: &Receiver<Event>,
    reporter: &mut Option<Reporter>,
    resumable: bool,
    lost: Option<&str>,
) -> Waited {
    let place = spec.place().to_string();
    let draw = |why: &str, retry_in: Option<Duration>| {
        let mut line = match lost {
            Some(_) => format!(" {MARK} remuda · {place} {} · reconnecting", spec.source),
            None => format!(" {MARK} remuda · {place} {}", spec.source),
        };
        if !why.is_empty() {
            line.push_str(&format!(" · {why}"));
        }
        if let Some(delay) = retry_in {
            line.push_str(&format!(" · retry in {}s", delay.as_secs().max(1)));
        }
        line.push_str(" · ctrl+] quit ");
        if lost.is_some() {
            draw_bar(&line);
        } else {
            let (cols, _) = crossterm::terminal::size().unwrap_or((80, 24));
            let line: String = line.chars().take(usize::from(cols)).collect();
            write_out(format!("\x1b[2J\x1b[H{line}").as_bytes());
        }
    };

    let mut attempt = 0;
    let mut why = lost.unwrap_or("connecting").to_string();
    loop {
        draw(&why, None);
        let lookup = spec.clone();
        let sender = events.clone();
        thread::spawn(move || {
            let event = match find_source(&lookup) {
                Ok((pane, machine, source)) => Event::SourceFound {
                    pane,
                    machine,
                    source,
                },
                Err(why) => Event::SourceLookupFailed(why),
            };
            let _ = sender.send(event);
        });

        // Wait for this lookup's answer, staying responsive to input.
        loop {
            match inbox.recv() {
                Ok(Event::Input(bytes)) if bytes.contains(&DETACH) => return Waited::Quit,
                Ok(Event::InputClosed) | Err(_) => return Waited::Closed,
                Ok(Event::Resized) => draw(&why, None),
                Ok(Event::SourceFound {
                    pane,
                    machine,
                    source,
                }) => {
                    spec.source = source;
                    return Waited::Found { pane, machine };
                }
                Ok(Event::SourceLookupFailed(failure)) => {
                    why = failure;
                    break;
                }
                // Leftovers from the previous connection's streams and pollers.
                Ok(_) => {}
            }
        }

        if let Some(reporter) = reporter {
            reporter.report(mirror_report(spec, None, resumable));
        }
        let deadline = Instant::now() + retry_delay(attempt);
        attempt += 1;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            draw(&why, Some(left));
            match inbox.recv_timeout(left.min(Duration::from_secs(1))) {
                Ok(Event::Input(bytes)) if bytes.contains(&DETACH) => return Waited::Quit,
                Ok(Event::InputClosed) | Err(RecvTimeoutError::Disconnected) => {
                    return Waited::Closed
                }
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            }
            if Instant::now() >= deadline {
                break;
            }
        }
    }
}

/// Looks the source up: its pane info, its machine, and its (possibly new) id.
fn find_source(spec: &MirrorSpec) -> Result<(Value, Option<Machine>, String), String> {
    let machine = match spec.machine.as_deref() {
        Some(label) => Some(
            herdr::machine_by_label(label)
                .ok_or_else(|| format!("no enabled saved machine named {label}"))?,
        ),
        None => None,
    };
    let result = herdr::json(&["pane", "list"], spec.machine.as_deref())
        .map_err(|_| format!("{} not answering", spec.place()))?;
    let panes = result["panes"].as_array().cloned().unwrap_or_default();
    let id = spec::resolve_source(spec, &panes).ok_or("source missing")?;
    let pane = panes
        .into_iter()
        .find(|pane| pane["pane_id"] == id.as_str())
        .ok_or("source missing")?;
    Ok((pane, machine, id))
}

/// Draws `text` as the reverse-video status bar on the last row.
fn draw_bar(text: &str) {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let width = usize::from(cols.max(1));
    let mut text: String = text.chars().take(width).collect();
    let used = text.chars().count();
    text.extend(std::iter::repeat_n(' ', width - used));
    // Save the cursor, draw on the last row, restore it.
    write_out(format!("\x1b7\x1b[{};1H\x1b[0;7m{text}\x1b[0m\x1b8", rows.max(1)).as_bytes());
}

/// Whether a control stream ended because another client holds control, in
/// which case the mirror stays view-only rather than fighting for it.
fn control_refused(note: &str) -> bool {
    note.contains("taken over") || note.contains("already has an attached client")
}

/// The report for a mirror whose source pane info is `source` (None if missing).
fn mirror_report(spec: &MirrorSpec, source: Option<&Value>, resumable: bool) -> Report {
    let agent = source
        .and_then(|pane| pane["agent"].as_str())
        .filter(|a| !a.is_empty());
    let state = match (agent, source) {
        (Some(_), Some(pane)) => report::report_state(pane["agent_status"].as_str().unwrap_or("")),
        _ => "idle",
    };
    let display = match agent {
        Some(agent) => format!("{MARK} {agent}@{}", spec.place()),
        None => format!("{MARK} {}:{}", spec.place(), spec.source),
    };
    Report {
        state,
        display,
        tokens: spec.to_tokens(),
        resume: if resumable { spec.resume_argv() } else { None },
    }
}

/// Whether `name` is an executable on PATH, which herdr requires of a resume command.
fn on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
}

struct Mirror {
    spec: MirrorSpec,
    machine: Option<Machine>,
    geometry: Arc<SourceGeometry>,
    natural: Size,
    events: Sender<Event>,
    stop: Arc<AtomicBool>,
    observe: Option<Child>,
    generation: u64,
    control: Option<(Child, ChildStdin)>,
    control_size: Option<Size>,
    note: String,
    reporter: Option<Reporter>,
    resumable: bool,
    presence: Option<Presence>,
    /// The source agent last marked, and when, to pace refreshes.
    presence_mark: Option<(Option<String>, Instant)>,
}

impl Mirror {
    fn new(
        spec: MirrorSpec,
        machine: Option<Machine>,
        source: &Value,
        events: Sender<Event>,
        reporter: Option<Reporter>,
        resumable: bool,
    ) -> Result<Mirror, String> {
        let geometry = SourceGeometry::new(&spec.source, spec.machine.as_deref())?;
        let (config, config_error) = match Config::load() {
            Ok(config) => (config, None),
            Err(err) => (Config::default(), Some(err)),
        };
        let presence = Presence::start(
            &spec.source,
            spec.machine.as_deref(),
            &config.machine_name(),
            spec.mode,
        );
        let note = if let Some(err) = config_error {
            err
        } else if resumable || reporter.is_none() {
            String::new()
        } else {
            "run `remuda install` to keep mirrors across restarts".into()
        };
        let mut mirror = Mirror {
            natural: geometry.initial,
            geometry: Arc::new(geometry),
            spec,
            machine,
            events,
            stop: Arc::new(AtomicBool::new(false)),
            observe: None,
            generation: 0,
            control: None,
            control_size: None,
            note,
            reporter,
            resumable,
            presence: Some(presence),
            presence_mark: None,
        };
        mirror.report(Some(source));
        Ok(mirror)
    }

    fn event_loop(&mut self, inbox: &Receiver<Event>) -> Result<Exit, String> {
        self.label_pane();
        self.start_observe();
        if self.spec.mode == Mode::Control {
            if let Err(err) = self.start_control() {
                return Ok(Exit::Lost(err));
            }
            spawn_layout_watcher(
                Arc::clone(&self.geometry),
                self.natural,
                self.events.clone(),
                Arc::clone(&self.stop),
            );
        }
        spawn_status_watcher(
            self.spec.source.clone(),
            self.spec.machine.clone(),
            self.events.clone(),
            Arc::clone(&self.stop),
        );

        let mut resize_due: Option<Instant> = None;
        loop {
            let timeout = resize_due
                .map(|due| due.saturating_duration_since(Instant::now()))
                .unwrap_or(IDLE_TICK);
            match inbox.recv_timeout(timeout) {
                Ok(Event::Input(bytes)) => {
                    if bytes.contains(&DETACH) {
                        return Ok(Exit::Quit);
                    }
                    if self.spec.mode == Mode::Control {
                        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
                        self.send_control(json!({"type": "terminal.input", "bytes": data}));
                    }
                }
                Ok(Event::InputClosed) | Err(RecvTimeoutError::Disconnected) => {
                    return Ok(Exit::Closed)
                }
                // A window drag sends a burst of these; act once it settles.
                Ok(Event::Resized) => resize_due = Some(Instant::now() + RESIZE_SETTLE),
                Ok(Event::Frame { generation, bytes }) if generation == self.generation => {
                    write_out(&bytes);
                    self.draw_status();
                }
                Ok(Event::ObserveEnded { generation, note }) if generation == self.generation => {
                    return Ok(Exit::Lost(note));
                }
                Ok(Event::Frame { .. } | Event::ObserveEnded { .. }) => {}
                Ok(Event::ControlEnded(note)) if control_refused(&note) => {
                    // Keep watching; another client drives the source now.
                    if let Some((mut child, stdin)) = self.control.take() {
                        drop(stdin);
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                    self.control_size = None;
                    self.note = "another client has control · view-only".into();
                    self.draw_status();
                }
                Ok(Event::ControlEnded(note)) => return Ok(Exit::Lost(note)),
                Ok(Event::SourceFound { .. } | Event::SourceLookupFailed(_)) => {}
                Ok(Event::NaturalSize(size)) => {
                    self.natural = size;
                    self.apply_policy();
                }
                Ok(Event::SourceInfo(pane)) => {
                    self.spec.hints = Hints::from_pane(&pane);
                    self.report(Some(&pane));
                }
                Ok(Event::SourceMissing(why)) => {
                    self.note = why;
                    self.report(None);
                    self.draw_status();
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            if resize_due.is_some_and(|due| Instant::now() >= due) {
                resize_due = None;
                self.start_observe();
                self.apply_policy();
            }
        }
    }

    fn stop_streams(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.send_control(json!({"type": "terminal.release"}));
        if let Some(mut child) = self.observe.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some((mut child, stdin)) = self.control.take() {
            drop(stdin);
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn report(&mut self, source: Option<&Value>) {
        let report = mirror_report(&self.spec, source, self.resumable);
        if let Some(reporter) = &mut self.reporter {
            reporter.report(report);
        }
        if let Some(source) = source {
            self.mark_source(source);
        }
    }

    /// Marks the source as mirrored when its agent changes, and refreshes the
    /// mark before its TTL runs out.
    fn mark_source(&mut self, source: &Value) {
        let Some(presence) = &self.presence else {
            return;
        };
        let agent = source["agent"]
            .as_str()
            .filter(|agent| !agent.is_empty())
            .map(str::to_string);
        let due = match &self.presence_mark {
            None => true,
            Some((marked, at)) => *marked != agent || at.elapsed() >= presence::REFRESH,
        };
        if due {
            presence.show(agent.clone());
            self.presence_mark = Some((agent, Instant::now()));
        }
    }

    /// This pane's size, less the status bar row.
    fn mirror_size(&self) -> Size {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        Size {
            cols: cols.max(1),
            rows: rows.saturating_sub(1).max(1),
        }
    }

    fn status_line(&self) -> String {
        let mut line = format!(
            " {MARK} remuda · {} {}",
            self.spec.place(),
            self.spec.source
        );
        match self.spec.mode {
            Mode::View => line.push_str(" · view-only"),
            Mode::Control => {
                line.push_str(" · control");
                if let Some(size) = self.control_size {
                    let approx = if self.geometry.estimated { "~" } else { "" };
                    line.push_str(&format!(" · {} {approx}{size}", self.spec.size.as_str()));
                }
            }
        }
        if !self.note.is_empty() {
            line.push_str(&format!(" · {}", self.note));
        }
        line.push_str(" · ctrl+] quit ");
        line
    }

    fn draw_status(&mut self) {
        draw_bar(&self.status_line());
    }

    fn label_pane(&self) {
        let Ok(pane_id) = std::env::var("HERDR_PANE_ID") else {
            return;
        };
        let label = format!("{MARK} {}:{}", self.spec.place(), self.spec.source);
        herdr::run_quietly(&["pane", "rename", &pane_id, &label]);
    }

    fn start_observe(&mut self) {
        let size = self.mirror_size();
        let previous = self.observe.take();
        let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
        self.generation = generation;
        let spawned = herdr::stream_command(
            StreamKind::Observe,
            &self.spec.source,
            size.cols,
            size.rows,
            self.machine.as_ref(),
            false,
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
        // Start the new stream before stopping the old one so the view never blanks.
        if let Some(mut child) = previous {
            let _ = child.kill();
            let _ = child.wait();
        }
        match spawned {
            Ok(mut child) => {
                let stdout = child.stdout.take();
                let stderr = child.stderr.take();
                self.observe = Some(child);
                let events = self.events.clone();
                thread::spawn(move || pump_observe(generation, stdout, stderr, events));
            }
            Err(err) => {
                self.note = format!("could not start the stream: {err}");
                self.draw_status();
            }
        }
    }

    fn start_control(&mut self) -> Result<(), String> {
        let size = self.target_size();
        let mut child = herdr::stream_command(
            StreamKind::Control,
            &self.spec.source,
            size.cols,
            size.rows,
            self.machine.as_ref(),
            self.spec.takeover,
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("could not start control: {err}"))?;
        let stdin = child.stdin.take().ok_or("control stream has no stdin")?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let events = self.events.clone();
        let stop = Arc::clone(&self.stop);
        thread::spawn(move || pump_control(stdout, stderr, events, stop));
        self.control = Some((child, stdin));
        self.control_size = Some(size);
        Ok(())
    }

    fn send_control(&mut self, message: Value) {
        let Some((_, stdin)) = self.control.as_mut() else {
            return;
        };
        let mut line = message.to_string();
        line.push('\n');
        let _ = stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.flush());
    }

    fn target_size(&self) -> Size {
        self.spec
            .size
            .size(self.mirror_size(), self.natural, self.geometry.initial)
    }

    fn apply_policy(&mut self) {
        if self.spec.mode != Mode::Control {
            return;
        }
        let size = self.target_size();
        if self.control_size == Some(size) {
            return;
        }
        self.control_size = Some(size);
        self.send_control(json!({"type": "terminal.resize", "cols": size.cols, "rows": size.rows}));
        self.draw_status();
    }
}

/// Decodes observe frames into events until the stream closes.
fn pump_observe(
    generation: u64,
    stdout: Option<impl Read>,
    stderr: Option<impl Read>,
    events: Sender<Event>,
) {
    let mut closed_reason = None;
    if let Some(stdout) = stdout {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            let Ok(record) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            match record["type"].as_str() {
                Some("terminal.frame") => {
                    let Some(bytes) = record["bytes"]
                        .as_str()
                        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
                    else {
                        continue;
                    };
                    if events.send(Event::Frame { generation, bytes }).is_err() {
                        return;
                    }
                }
                Some("terminal.closed") => {
                    closed_reason = record["reason"].as_str().map(str::to_string);
                    break;
                }
                _ => {}
            }
        }
    }
    // herdr says why when it closes a stream; a stream that just stops lost
    // its connection.
    let note = match (closed_reason, last_line(stderr)) {
        (Some(reason), _) => format!("source closed: {reason}"),
        (None, Some(detail)) => format!("connection lost: {detail}"),
        (None, None) => "connection lost".into(),
    };
    let _ = events.send(Event::ObserveEnded { generation, note });
}

/// Drains the control stream (display comes from observe) and reports its loss.
fn pump_control(
    stdout: Option<impl Read>,
    stderr: Option<impl Read>,
    events: Sender<Event>,
    stop: Arc<AtomicBool>,
) {
    let mut closed_reason = None;
    if let Some(stdout) = stdout {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if !line.contains("\"terminal.closed\"") {
                continue;
            }
            closed_reason = serde_json::from_str::<Value>(&line)
                .ok()
                .and_then(|record| record["reason"].as_str().map(str::to_string));
            break;
        }
    }
    if stop.load(Ordering::Relaxed) {
        return;
    }
    let note = match closed_reason.or_else(|| last_line(stderr)) {
        Some(detail) => format!("input lost: {detail}"),
        None => "input lost".into(),
    };
    let _ = events.send(Event::ControlEnded(note));
}

fn last_line(stream: Option<impl Read>) -> Option<String> {
    let mut text = String::new();
    stream?.read_to_string(&mut text).ok()?;
    text.lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .map(str::to_string)
}

fn spawn_input_reader(events: Sender<Event>) {
    thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buffer = [0u8; 4096];
        loop {
            match stdin.read(&mut buffer) {
                Ok(0) | Err(_) => {
                    let _ = events.send(Event::InputClosed);
                    return;
                }
                Ok(n) => {
                    if events.send(Event::Input(buffer[..n].to_vec())).is_err() {
                        return;
                    }
                }
            }
        }
    });
}

#[cfg(unix)]
fn spawn_resize_watcher(events: Sender<Event>) -> Result<(), String> {
    let mut signals = signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH])
        .map_err(|err| format!("watch resize: {err}"))?;
    thread::spawn(move || {
        for _ in signals.forever() {
            if events.send(Event::Resized).is_err() {
                return;
            }
        }
    });
    Ok(())
}

#[cfg(not(unix))]
fn spawn_resize_watcher(_events: Sender<Event>) -> Result<(), String> {
    Ok(())
}

/// Polls the source's layout so `source` and `smallest` follow its own resizes.
fn spawn_layout_watcher(
    geometry: Arc<SourceGeometry>,
    mut last: Size,
    events: Sender<Event>,
    stop: Arc<AtomicBool>,
) {
    thread::spawn(move || loop {
        thread::sleep(LAYOUT_POLL);
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let Ok(natural) = geometry.natural() else {
            continue;
        };
        if natural != last {
            last = natural;
            if events.send(Event::NaturalSize(natural)).is_err() {
                return;
            }
        }
    });
}

/// Polls the source pane so the mirror's sidebar state and hints stay current.
fn spawn_status_watcher(
    source: String,
    machine: Option<String>,
    events: Sender<Event>,
    stop: Arc<AtomicBool>,
) {
    thread::spawn(move || loop {
        thread::sleep(STATUS_POLL);
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let event = match herdr::json(&["pane", "get", &source], machine.as_deref()) {
            Ok(mut result) => Event::SourceInfo(result["pane"].take()),
            Err(err) if err.contains("not found") => Event::SourceMissing("source missing".into()),
            // A transient failure (a remote reconnecting) is not news.
            Err(_) => continue,
        };
        if events.send(event).is_err() {
            return;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> MirrorSpec {
        MirrorSpec {
            source: "w8:p1".into(),
            machine: Some("ziggy".into()),
            ..MirrorSpec::default()
        }
    }

    #[test]
    fn report_mirrors_an_agent_source() {
        let pane = json!({"agent": "claude", "agent_status": "working"});
        let report = mirror_report(&spec(), Some(&pane), true);
        assert_eq!(report.state, "working");
        assert_eq!(report.display, "\u{21c4} claude@ziggy");
        assert_eq!(report.resume.unwrap()[..2], ["remuda", "view"]);
    }

    #[test]
    fn report_for_a_shell_or_missing_source_is_idle() {
        let shell = json!({"agent_status": "unknown"});
        let report = mirror_report(&spec(), Some(&shell), false);
        assert_eq!(report.state, "idle");
        assert_eq!(report.display, "\u{21c4} ziggy:w8:p1");
        assert_eq!(report.resume, None);
        assert_eq!(mirror_report(&spec(), None, true).state, "idle");
    }

    #[test]
    fn report_tokens_read_back_as_the_spec() {
        let report = mirror_report(&spec(), None, true);
        let tokens: serde_json::Map<String, Value> = report
            .tokens
            .into_iter()
            .filter_map(|(key, value)| Some((key.to_string(), Value::String(value?))))
            .collect();
        assert_eq!(
            MirrorSpec::from_tokens(&Value::Object(tokens)),
            Some(spec())
        );
    }

    #[test]
    fn retries_back_off_and_cap() {
        let delays: Vec<u64> = (0..7).map(|n| retry_delay(n).as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 15, 15, 15]);
    }

    #[test]
    fn losing_control_to_another_client_is_not_a_lost_connection() {
        assert!(control_refused("input lost: terminal attach taken over"));
        assert!(control_refused(
            "input lost: terminal attach failed: terminal term_1 already has an attached client; retry with --takeover"
        ));
        assert!(!control_refused(
            "input lost: Timeout, server ziggy not responding"
        ));
        assert!(!control_refused("input lost"));
    }
}
