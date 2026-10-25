//! The mirror pane: shows a source pane and optionally drives it.
//!
//! Display always comes from a read-only `terminal session observe` stream sized
//! to this pane, so looking at a source never resizes it. Control mode adds a
//! `terminal session control` stream for input, and the size it asks for is set
//! by the resize policy.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{json, Value};

use crate::geometry::{Policy, Size, SourceGeometry};
use crate::herdr::{self, Machine, StreamKind, MARK};

const DETACH: u8 = 0x1d; // ctrl+]
const LAYOUT_POLL: Duration = Duration::from_secs(1);
const RESIZE_SETTLE: Duration = Duration::from_millis(50);
const IDLE_TICK: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    View,
    Control,
}

/// What a mirror pane shows and how it drives its source, read from `REMUDA_*`.
pub struct Config {
    pub source: String,
    pub machine: Option<String>,
    pub mode: Mode,
    pub policy: Policy,
    pub takeover: bool,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let source = var("REMUDA_SOURCE").ok_or("REMUDA_SOURCE is not set")?;
        let mode = match var("REMUDA_MODE").as_deref() {
            None | Some("view") => Mode::View,
            Some("control") => Mode::Control,
            Some(other) => return Err(format!("REMUDA_MODE must be view or control, not {other}")),
        };
        let policy = match var("REMUDA_SIZE") {
            Some(name) => name.parse()?,
            None => Policy::Source,
        };
        Ok(Self {
            source,
            machine: var("REMUDA_MACHINE"),
            mode,
            policy,
            takeover: var("REMUDA_TAKEOVER").as_deref() == Some("1"),
        })
    }
}

enum Event {
    Input(Vec<u8>),
    InputClosed,
    Resized,
    Frame { generation: u64, bytes: Vec<u8> },
    ObserveEnded { generation: u64, note: String },
    ControlEnded(String),
    NaturalSize(Size),
}

pub fn run() -> Result<(), String> {
    let config = Config::from_env()?;
    let machine = match config.machine.as_deref() {
        Some(label) => Some(
            herdr::machine_by_label(label)
                .ok_or_else(|| format!("no enabled saved machine named {label:?}"))?,
        ),
        None => None,
    };
    let geometry = SourceGeometry::new(&config.source, config.machine.as_deref())?;
    let (events, inbox) = mpsc::channel();
    let mut mirror = Mirror {
        natural: geometry.initial,
        geometry: Arc::new(geometry),
        config,
        machine,
        events,
        stop: Arc::new(AtomicBool::new(false)),
        observe: None,
        generation: 0,
        control: None,
        control_size: None,
        note: String::new(),
        out: std::io::stdout(),
    };
    mirror.run(inbox)
}

struct Mirror {
    config: Config,
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
    out: std::io::Stdout,
}

impl Mirror {
    fn run(&mut self, inbox: Receiver<Event>) -> Result<(), String> {
        self.label_pane();
        crossterm::terminal::enable_raw_mode().map_err(|err| format!("raw mode: {err}"))?;
        self.write(b"\x1b[?1049h\x1b[2J");
        let result = self.event_loop(inbox);
        self.shutdown();
        result
    }

    fn event_loop(&mut self, inbox: Receiver<Event>) -> Result<(), String> {
        spawn_input_reader(self.events.clone());
        spawn_resize_watcher(self.events.clone())?;
        self.start_observe();
        if self.config.mode == Mode::Control {
            self.start_control()?;
            spawn_layout_watcher(
                Arc::clone(&self.geometry),
                self.natural,
                self.events.clone(),
                Arc::clone(&self.stop),
            );
        }

        let mut resize_due: Option<Instant> = None;
        loop {
            let timeout = resize_due
                .map(|due| due.saturating_duration_since(Instant::now()))
                .unwrap_or(IDLE_TICK);
            match inbox.recv_timeout(timeout) {
                Ok(Event::Input(bytes)) => {
                    if bytes.contains(&DETACH) {
                        return Ok(());
                    }
                    if self.config.mode == Mode::Control {
                        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
                        self.send_control(json!({"type": "terminal.input", "bytes": data}));
                    }
                }
                Ok(Event::InputClosed) => return Ok(()),
                // A window drag sends a burst of these; act once it settles.
                Ok(Event::Resized) => resize_due = Some(Instant::now() + RESIZE_SETTLE),
                Ok(Event::Frame { generation, bytes }) if generation == self.generation => {
                    self.write(&bytes);
                    self.note.clear();
                    self.draw_status();
                }
                Ok(Event::ObserveEnded { generation, note }) if generation == self.generation => {
                    self.note = note;
                    self.draw_status();
                }
                Ok(Event::Frame { .. } | Event::ObserveEnded { .. }) => {}
                Ok(Event::ControlEnded(note)) => {
                    self.note = note;
                    self.draw_status();
                }
                Ok(Event::NaturalSize(size)) => {
                    self.natural = size;
                    self.apply_policy();
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
            if resize_due.is_some_and(|due| Instant::now() >= due) {
                resize_due = None;
                self.start_observe();
                self.apply_policy();
            }
        }
    }

    fn shutdown(&mut self) {
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
        self.write(b"\x1b[?1049l");
        let _ = crossterm::terminal::disable_raw_mode();
    }

    /// This pane's size, less the status bar row.
    fn mirror_size(&self) -> Size {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        Size {
            cols: cols.max(1),
            rows: rows.saturating_sub(1).max(1),
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        let _ = self.out.write_all(bytes);
        let _ = self.out.flush();
    }

    fn status_line(&self) -> String {
        let place = self.config.machine.as_deref().unwrap_or("local");
        let mut line = format!(" {MARK} remuda · {place} {}", self.config.source);
        match self.config.mode {
            Mode::View => line.push_str(" · view-only"),
            Mode::Control => {
                line.push_str(" · control");
                if let Some(size) = self.control_size {
                    let approx = if self.geometry.estimated { "~" } else { "" };
                    line.push_str(&format!(
                        " · {} {approx}{size}",
                        self.config.policy.as_str()
                    ));
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
        let size = self.mirror_size();
        let width = usize::from(size.cols);
        let mut text: String = self.status_line().chars().take(width).collect();
        let used = text.chars().count();
        text.extend(std::iter::repeat_n(' ', width - used));
        // Save the cursor, draw in reverse video on the last row, restore it.
        let bar = format!("\x1b7\x1b[{};1H\x1b[0;7m{text}\x1b[0m\x1b8", size.rows + 1);
        self.write(bar.as_bytes());
    }

    fn label_pane(&self) {
        let Ok(pane_id) = std::env::var("HERDR_PANE_ID") else {
            return;
        };
        let place = self.config.machine.as_deref().unwrap_or("local");
        let label = format!("{MARK} {place}:{}", self.config.source);
        herdr::run_quietly(&["pane", "rename", &pane_id, &label]);
    }

    fn start_observe(&mut self) {
        let size = self.mirror_size();
        let previous = self.observe.take();
        self.generation += 1;
        let generation = self.generation;
        let spawned = herdr::stream_command(
            StreamKind::Observe,
            &self.config.source,
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
            &self.config.source,
            size.cols,
            size.rows,
            self.machine.as_ref(),
            self.config.takeover,
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
        self.config
            .policy
            .size(self.mirror_size(), self.natural, self.geometry.initial)
    }

    fn apply_policy(&mut self) {
        if self.config.mode != Mode::Control {
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
    let detail = closed_reason.or_else(|| last_line(stderr));
    let note = match detail {
        Some(detail) => format!("source closed: {detail}"),
        None => "source closed".into(),
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
