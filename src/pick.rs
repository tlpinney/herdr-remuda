//! The picker popup: choose a pane from any machine and open a mirror of it,
//! or open a saved view.

use std::thread;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use serde_json::Value;

use crate::context::Context;
use crate::geometry::Policy;
use crate::herdr::{self, MARK};
use crate::spec::Mode;
use crate::views::{self, SavedView};

/// A pane offered by the picker. `machine` is `None` for local panes.
struct Candidate {
    machine: Option<String>,
    pane_id: String,
    agent: String,
    status: String,
    name: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Panes,
    Views,
}

enum Choice {
    Mirror {
        machine: Option<String>,
        pane_id: String,
        mode: Mode,
        policy: Policy,
    },
    OpenView(String),
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
    let policy = std::env::var("REMUDA_SIZE")
        .ok()
        .and_then(|name| name.parse().ok())
        .unwrap_or_default();
    let mut picker = Picker {
        candidates,
        views: views::list(),
        errors,
        tab: Tab::Panes,
        panes_state: ListState::default().with_selected(Some(0)),
        views_state: ListState::default().with_selected(Some(0)),
        policy,
        confirm_delete: None,
    };
    let choice = picker.run(&mut terminal);
    ratatui::restore();
    let context = Context::from_env();
    match choice? {
        Some(Choice::Mirror {
            machine,
            pane_id,
            mode,
            policy,
        }) => open_mirror(machine.as_deref(), &pane_id, mode, policy, &context),
        Some(Choice::OpenView(name)) => {
            let workspace = context
                .workspace_id
                .ok_or("no workspace to open the view in")?;
            views::open(&views::load(&name)?, &workspace, true)
        }
        None => Ok(()),
    }
}

struct Picker {
    candidates: Vec<Candidate>,
    views: Vec<SavedView>,
    errors: Vec<String>,
    tab: Tab,
    panes_state: ListState,
    views_state: ListState,
    policy: Policy,
    /// A view waiting for `y` to confirm its deletion.
    confirm_delete: Option<String>,
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
            if let Some(name) = self.confirm_delete.take() {
                if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    if let Err(err) = views::delete(&name) {
                        self.errors.push(err);
                    }
                    self.views = views::list();
                }
                continue;
            }
            let state = match self.tab {
                Tab::Panes => &mut self.panes_state,
                Tab::Views => &mut self.views_state,
            };
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(None)
                }
                KeyCode::Tab | KeyCode::BackTab => {
                    self.tab = match self.tab {
                        Tab::Panes => Tab::Views,
                        Tab::Views => Tab::Panes,
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => state.select_next(),
                KeyCode::Up | KeyCode::Char('k') => state.select_previous(),
                _ => match self.tab {
                    Tab::Panes => match key.code {
                        KeyCode::Char('p') => self.policy = self.policy.next(),
                        KeyCode::Enter => return Ok(self.choose_pane(Mode::View)),
                        KeyCode::Char('c') => return Ok(self.choose_pane(Mode::Control)),
                        _ => {}
                    },
                    Tab::Views => match key.code {
                        KeyCode::Enter => {
                            return Ok(self
                                .selected_view()
                                .map(|v| Choice::OpenView(v.name.clone())))
                        }
                        KeyCode::Char('d') => {
                            self.confirm_delete = self.selected_view().map(|v| v.name.clone());
                        }
                        _ => {}
                    },
                },
            }
        }
    }

    fn choose_pane(&self, mode: Mode) -> Option<Choice> {
        let candidate = self.candidates.get(self.panes_state.selected()?)?;
        Some(Choice::Mirror {
            machine: candidate.machine.clone(),
            pane_id: candidate.pane_id.clone(),
            mode,
            policy: self.policy,
        })
    }

    fn selected_view(&self) -> Option<&SavedView> {
        self.views.get(self.views_state.selected()?)
    }

    fn draw(&mut self, frame: &mut Frame) {
        let [header, list, footer] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(self.errors.len().max(1) as u16),
        ])
        .areas(frame.area());

        let tab_label = |tab: Tab, text: &'static str| {
            if self.tab == tab {
                Span::from(text).bold().reversed()
            } else {
                Span::from(text).dim()
            }
        };
        let mut title = vec![
            Span::from(format!(" {MARK} remuda  ")).bold(),
            tab_label(Tab::Panes, " panes "),
            Span::from(" "),
            tab_label(Tab::Views, " views "),
            Span::from("   "),
        ];
        match self.tab {
            Tab::Panes => title.extend([
                Span::from("enter view-only · c control · p size: ").dim(),
                Span::from(self.policy.as_str()).bold(),
                Span::from(" · tab views · q quit").dim(),
            ]),
            Tab::Views => title
                .push(Span::from("enter open in a new tab · d delete · tab panes · q quit").dim()),
        }
        frame.render_widget(Paragraph::new(Line::from(title)), header);

        let (items, state): (Vec<ListItem>, _) = match self.tab {
            Tab::Panes if self.candidates.is_empty() => (
                vec![ListItem::new("  no panes found")],
                &mut self.panes_state,
            ),
            Tab::Panes => (
                self.candidates.iter().map(candidate_row).collect(),
                &mut self.panes_state,
            ),
            Tab::Views if self.views.is_empty() => (
                vec![ListItem::new(
                    "  no saved views yet: arrange mirrors in a tab, then run \"Save view\"",
                )],
                &mut self.views_state,
            ),
            Tab::Views => (
                self.views.iter().map(view_row).collect(),
                &mut self.views_state,
            ),
        };
        frame.render_stateful_widget(
            List::new(items)
                .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
                .highlight_symbol("> "),
            list,
            state,
        );

        let footer_lines: Vec<Line> = match &self.confirm_delete {
            Some(name) => {
                vec![Line::from(format!(" delete the saved view {name:?}? y/n")).yellow()]
            }
            None => self
                .errors
                .iter()
                .map(|error| Line::from(format!(" {error}")).red())
                .collect(),
        };
        frame.render_widget(Paragraph::new(footer_lines), footer);
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

fn view_row(view: &SavedView) -> ListItem<'static> {
    let mirrors = view.mirror_count();
    let panes = view.pane_count();
    let detail = match panes - mirrors {
        0 => format!("{mirrors} mirrors"),
        shells => format!("{mirrors} mirrors, {shells} other panes"),
    };
    ListItem::new(Line::from(vec![
        Span::from(format!("{:<24} ", truncate(&view.name, 24))).bold(),
        Span::from(detail).dim(),
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
    let is_mirror = label.starts_with(MARK) || crate::spec::is_mirror(pane);
    if is_mirror {
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

fn open_mirror(
    machine: Option<&str>,
    pane_id: &str,
    mode: Mode,
    policy: Policy,
    context: &Context,
) -> Result<(), String> {
    let mut args = vec![
        "plugin".to_string(),
        "pane".into(),
        "open".into(),
        "--plugin".into(),
        herdr::plugin_id(),
        "--entrypoint".into(),
        "view".into(),
        "--placement".into(),
        "split".into(),
        "--direction".into(),
        "right".into(),
        "--env".into(),
        format!("REMUDA_SOURCE={pane_id}"),
        "--env".into(),
        format!("REMUDA_MODE={}", mode.as_str()),
        "--env".into(),
        format!("REMUDA_SIZE={}", policy.as_str()),
    ];
    if let Some(machine) = machine {
        args.extend(["--env".into(), format!("REMUDA_MACHINE={machine}")]);
    }
    if let Some(pane) = &context.focused_pane_id {
        args.extend(["--target-pane".into(), pane.clone()]);
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
        let labelled = serde_json::json!({"pane_id": "w1:p4", "label": "\u{21c4} local:w1:p2"});
        assert!(candidate_from_pane(None, &labelled).is_none());
        let tokened = serde_json::json!({"pane_id": "w1:p5", "tokens": {"remuda_source": "w1:p1"}});
        assert!(candidate_from_pane(None, &tokened).is_none());
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
