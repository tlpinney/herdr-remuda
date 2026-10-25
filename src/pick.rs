//! The picker popup: choose a pane from any machine and open a mirror of it.

use std::thread;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use serde_json::Value;

use crate::geometry::Policy;
use crate::herdr::{self, MARK};
use crate::view::Mode;

/// A pane offered by the picker. `machine` is `None` for local panes.
struct Candidate {
    machine: Option<String>,
    pane_id: String,
    agent: String,
    status: String,
    name: String,
}

struct Choice {
    machine: Option<String>,
    pane_id: String,
    mode: Mode,
    policy: Policy,
}

pub fn run() -> Result<(), String> {
    let mut terminal = ratatui::init();
    let _ = terminal.draw(|frame| {
        frame.render_widget(
            Paragraph::new(format!(" {MARK} loading panes…")),
            frame.area(),
        );
    });
    let (candidates, errors) = collect_candidates();
    let initial_policy = std::env::var("REMUDA_SIZE")
        .ok()
        .and_then(|name| name.parse().ok())
        .unwrap_or(Policy::Source);
    let mut picker = Picker {
        candidates,
        errors,
        state: ListState::default().with_selected(Some(0)),
        policy: initial_policy,
    };
    let choice = picker.run(&mut terminal);
    ratatui::restore();
    match choice? {
        Some(choice) => open_mirror(&choice),
        None => Ok(()),
    }
}

struct Picker {
    candidates: Vec<Candidate>,
    errors: Vec<String>,
    state: ListState,
    policy: Policy,
}

impl Picker {
    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<Option<Choice>, String> {
        loop {
            terminal
                .draw(|frame| self.draw(frame))
                .map_err(|err| format!("draw picker: {err}"))?;
            let Event::Key(key) = event::read().map_err(|err| format!("read key: {err}"))? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(None)
                }
                KeyCode::Down | KeyCode::Char('j') => self.state.select_next(),
                KeyCode::Up | KeyCode::Char('k') => self.state.select_previous(),
                KeyCode::Char('p') => self.policy = self.policy.next(),
                KeyCode::Enter => return Ok(self.choose(Mode::View)),
                KeyCode::Char('c') => return Ok(self.choose(Mode::Control)),
                _ => {}
            }
        }
    }

    fn choose(&self, mode: Mode) -> Option<Choice> {
        let candidate = self.candidates.get(self.state.selected()?)?;
        Some(Choice {
            machine: candidate.machine.clone(),
            pane_id: candidate.pane_id.clone(),
            mode,
            policy: self.policy,
        })
    }

    fn draw(&mut self, frame: &mut Frame) {
        let [header, list, footer] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(self.errors.len() as u16),
        ])
        .areas(frame.area());

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::from(format!(" {MARK} remuda: pick a pane   ")).bold(),
                Span::from("enter view-only · c control · p size policy: ").dim(),
                Span::from(self.policy.as_str()).bold(),
                Span::from(" · q quit").dim(),
            ])),
            header,
        );

        let items: Vec<ListItem> = if self.candidates.is_empty() {
            vec![ListItem::new("  no panes found")]
        } else {
            self.candidates.iter().map(candidate_row).collect()
        };
        frame.render_stateful_widget(
            List::new(items)
                .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
                .highlight_symbol("> "),
            list,
            &mut self.state,
        );

        let errors: Vec<Line> = self
            .errors
            .iter()
            .map(|error| Line::from(format!(" {error}")).red())
            .collect();
        frame.render_widget(Paragraph::new(errors), footer);
    }
}

fn candidate_row(candidate: &Candidate) -> ListItem<'static> {
    let place = candidate.machine.as_deref().unwrap_or("local");
    ListItem::new(Line::from(vec![
        Span::from(format!("{:<14} ", truncate(place, 14))).dim(),
        Span::from(format!("{:<8} ", candidate.pane_id)),
        Span::from(format!("{:<12} ", truncate(&candidate.agent, 12))).cyan(),
        Span::from(format!("{:<8} ", candidate.status)).dim(),
        Span::from(candidate.name.clone()),
    ]))
}

fn truncate(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

/// Lists panes from Local and every enabled machine, querying machines in parallel.
fn collect_candidates() -> (Vec<Candidate>, Vec<String>) {
    let mut sources: Vec<Option<String>> = vec![None];
    sources.extend(herdr::machines().into_iter().map(|m| Some(m.label)));
    let handles: Vec<_> = sources
        .into_iter()
        .map(|machine| {
            thread::spawn(move || {
                let panes = herdr::json(&["pane", "list"], machine.as_deref());
                (machine, panes)
            })
        })
        .collect();

    let mut candidates = Vec::new();
    let mut errors = Vec::new();
    for handle in handles {
        let Ok((machine, panes)) = handle.join() else {
            continue;
        };
        let panes = match panes {
            Ok(result) => result["panes"].as_array().cloned().unwrap_or_default(),
            Err(err) => {
                errors.push(format!("{}: {err}", machine.as_deref().unwrap_or("local")));
                continue;
            }
        };
        candidates.extend(
            panes
                .iter()
                .filter_map(|pane| candidate_from_pane(machine.clone(), pane)),
        );
    }
    (candidates, errors)
}

fn candidate_from_pane(machine: Option<String>, pane: &Value) -> Option<Candidate> {
    let text = |key: &str| pane[key].as_str().unwrap_or_default().to_string();
    let label = text("label");
    if label.starts_with(MARK) {
        return None; // don't offer mirrors of mirrors
    }
    let agent = text("agent");
    let status = if agent.is_empty() {
        String::new()
    } else {
        text("agent_status")
    };
    let cwd_name = || {
        let cwd = pane["foreground_cwd"]
            .as_str()
            .or_else(|| pane["cwd"].as_str())
            .unwrap_or_default();
        cwd.rsplit('/').next().unwrap_or_default().to_string()
    };
    let name = [label, text("title"), text("terminal_title_stripped")]
        .into_iter()
        .find(|name| !name.is_empty())
        .unwrap_or_else(cwd_name);
    Some(Candidate {
        machine,
        pane_id: pane["pane_id"].as_str()?.to_string(),
        agent,
        status,
        name,
    })
}

/// The pane the picker was opened from, so the mirror splits next to it.
fn context_pane() -> Option<String> {
    let context: Value =
        serde_json::from_str(&std::env::var("HERDR_PLUGIN_CONTEXT_JSON").ok()?).ok()?;
    ["focused_pane_id", "pane_id"]
        .iter()
        .find_map(|key| context[*key].as_str())
        .or_else(|| {
            let pane = context
                .get("pane")
                .or_else(|| context.get("focused_pane"))?;
            pane["pane_id"].as_str().or_else(|| pane["id"].as_str())
        })
        .map(str::to_string)
}

fn open_mirror(choice: &Choice) -> Result<(), String> {
    let plugin = herdr::plugin_id();
    let mode = match choice.mode {
        Mode::View => "view",
        Mode::Control => "control",
    };
    let mut args = vec![
        "plugin".to_string(),
        "pane".into(),
        "open".into(),
        "--plugin".into(),
        plugin,
        "--entrypoint".into(),
        "view".into(),
        "--placement".into(),
        "split".into(),
        "--direction".into(),
        "right".into(),
        "--env".into(),
        format!("REMUDA_SOURCE={}", choice.pane_id),
        "--env".into(),
        format!("REMUDA_MODE={mode}"),
        "--env".into(),
        format!("REMUDA_SIZE={}", choice.policy.as_str()),
    ];
    if let Some(machine) = &choice.machine {
        args.extend(["--env".into(), format!("REMUDA_MACHINE={machine}")]);
    }
    if let Some(pane) = context_pane() {
        args.extend(["--target-pane".into(), pane]);
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    herdr::json(&args, None).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_prefer_label_then_titles_then_cwd() {
        let pane = serde_json::json!({
            "pane_id": "w1:p2",
            "terminal_title_stripped": "vim",
            "cwd": "/home/me/project",
        });
        let candidate = candidate_from_pane(None, &pane).unwrap();
        assert_eq!(candidate.name, "vim");

        let pane = serde_json::json!({"pane_id": "w1:p3", "cwd": "/home/me/project"});
        assert_eq!(candidate_from_pane(None, &pane).unwrap().name, "project");
    }

    #[test]
    fn mirrors_are_not_offered() {
        let pane = serde_json::json!({"pane_id": "w1:p4", "label": "\u{21c4} local:w1:p2"});
        assert!(candidate_from_pane(None, &pane).is_none());
    }

    #[test]
    fn agent_status_only_shows_for_agents() {
        let shell = serde_json::json!({"pane_id": "w1:p1", "agent_status": "unknown"});
        assert_eq!(candidate_from_pane(None, &shell).unwrap().status, "");
        let agent =
            serde_json::json!({"pane_id": "w1:p2", "agent": "claude", "agent_status": "idle"});
        assert_eq!(candidate_from_pane(None, &agent).unwrap().status, "idle");
    }
}
