//! The "Save view" popup: name the current tab and save it as a view.

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{DefaultTerminal, Frame};

use crate::herdr::MARK;
use crate::{context, views};

pub fn run() -> Result<(), String> {
    let context = context::Context::from_env();
    let tab_id = context
        .tab_id
        .clone()
        .ok_or("no tab to save: open this from a herdr tab")?;
    // Herdr numbers unnamed tabs; a number is no name.
    let name = context
        .tab_label
        .clone()
        .filter(|label| !label.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or_default();
    let mut terminal = ratatui::init();
    let mut prompt = Prompt {
        name,
        message: None,
        confirm_overwrite: false,
    };
    let result = prompt.run(&mut terminal, &tab_id);
    ratatui::restore();
    result
}

struct Prompt {
    name: String,
    message: Option<String>,
    confirm_overwrite: bool,
}

impl Prompt {
    fn run(&mut self, terminal: &mut DefaultTerminal, tab_id: &str) -> Result<(), String> {
        loop {
            terminal
                .draw(|frame| self.draw(frame))
                .map_err(|err| format!("draw: {err}"))?;
            let Event::Key(key) = event::read().map_err(|err| format!("read key: {err}"))? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if self.confirm_overwrite {
                self.confirm_overwrite = false;
                if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    return self.save(tab_id);
                }
                self.message = None;
                continue;
            }
            match key.code {
                KeyCode::Esc => return Ok(()),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(())
                }
                KeyCode::Enter if self.name.trim().is_empty() => {
                    self.message = Some("type a name first".into());
                }
                KeyCode::Enter if views::exists(&self.name) => {
                    self.confirm_overwrite = true;
                    self.message = Some(format!(
                        "replace the saved view {:?}? y/n",
                        self.name.trim()
                    ));
                }
                KeyCode::Enter => return self.save(tab_id),
                KeyCode::Backspace => {
                    self.name.pop();
                }
                KeyCode::Char(c) => self.name.push(c),
                _ => {}
            }
        }
    }

    fn save(&self, tab_id: &str) -> Result<(), String> {
        views::save_tab(&self.name, tab_id).map(|_| ())
    }

    fn draw(&self, frame: &mut Frame) {
        let [title, input, help, message] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Min(1),
        ])
        .areas(frame.area());
        frame.render_widget(
            Paragraph::new(format!(" {MARK} remuda: save this tab as a view")).bold(),
            title,
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::from(" name: ").dim(),
                Span::from(self.name.clone()),
                Span::from("_").slow_blink(),
            ])),
            input,
        );
        frame.render_widget(
            Paragraph::new(" enter save · esc cancel · saved views open from the picker").dim(),
            help,
        );
        if let Some(text) = &self.message {
            frame.render_widget(Paragraph::new(format!(" {text}")).yellow(), message);
        }
    }
}
