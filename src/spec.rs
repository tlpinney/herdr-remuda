//! What a mirror shows and how to find its source again.
//!
//! A `MirrorSpec` travels three ways: as `remuda view` arguments (including the
//! resume command herdr runs after a restart), as the pane's `remuda` metadata
//! token, and inside saved views.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::geometry::Policy;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    View,
    Control,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::View => "view",
            Mode::Control => "control",
        }
    }
}

/// Clues for finding the source when its pane id no longer exists.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hints {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

impl Hints {
    pub fn is_empty(&self) -> bool {
        *self == Hints::default()
    }

    /// Reads the clues from a herdr pane info object.
    pub fn from_pane(pane: &Value) -> Hints {
        let text = |key: &str| {
            pane[key]
                .as_str()
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        Hints {
            label: text("label"),
            agent: text("agent"),
            title: text("terminal_title_stripped").or_else(|| text("title")),
            cwd: text("cwd"),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorSpec {
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    #[serde(default)]
    pub mode: Mode,
    #[serde(default)]
    pub size: Policy,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub takeover: bool,
    #[serde(default, skip_serializing_if = "Hints::is_empty")]
    pub hints: Hints,
}

impl MirrorSpec {
    /// Parses `remuda view` arguments, falling back to `REMUDA_*` variables,
    /// which is how `plugin pane open` passes them.
    pub fn from_args_or_env(args: &[String]) -> Result<MirrorSpec, String> {
        let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let mut spec = MirrorSpec {
            source: env("REMUDA_SOURCE").unwrap_or_default(),
            machine: env("REMUDA_MACHINE"),
            takeover: env("REMUDA_TAKEOVER").as_deref() == Some("1"),
            ..MirrorSpec::default()
        };
        if let Some(mode) = env("REMUDA_MODE") {
            spec.mode = parse_mode(&mode)?;
        }
        if let Some(size) = env("REMUDA_SIZE") {
            spec.size = size.parse()?;
        }
        spec.apply_args(args)?;
        if spec.source.is_empty() {
            return Err("no source pane: pass --source or set REMUDA_SOURCE".into());
        }
        Ok(spec)
    }

    fn apply_args(&mut self, args: &[String]) -> Result<(), String> {
        let mut args = args.iter();
        while let Some(flag) = args.next() {
            if flag == "--takeover" {
                self.takeover = true;
                continue;
            }
            let value = args
                .next()
                .ok_or_else(|| format!("{flag} needs a value"))?
                .clone();
            match flag.as_str() {
                "--source" => self.source = value,
                "--machine" => self.machine = Some(value),
                "--mode" => self.mode = parse_mode(&value)?,
                "--size" => self.size = value.parse()?,
                "--hint-label" => self.hints.label = Some(value),
                "--hint-agent" => self.hints.agent = Some(value),
                "--hint-title" => self.hints.title = Some(value),
                "--hint-cwd" => self.hints.cwd = Some(value),
                _ => return Err(format!("unknown view option {flag}")),
            }
        }
        Ok(())
    }

    /// The `remuda view` arguments that recreate this mirror.
    pub fn to_args(&self) -> Vec<String> {
        let mut args = vec!["--source".to_string(), self.source.clone()];
        if let Some(machine) = &self.machine {
            args.extend(["--machine".into(), machine.clone()]);
        }
        args.extend(["--mode".into(), self.mode.as_str().into()]);
        args.extend(["--size".into(), self.size.as_str().into()]);
        if self.takeover {
            args.push("--takeover".into());
        }
        let hints = [
            ("--hint-label", &self.hints.label),
            ("--hint-agent", &self.hints.agent),
            ("--hint-title", &self.hints.title),
            ("--hint-cwd", &self.hints.cwd),
        ];
        for (flag, value) in hints {
            if let Some(value) = value {
                args.extend([flag.to_string(), value.clone()]);
            }
        }
        args
    }

    /// The command herdr runs to bring this mirror back after a server restart.
    ///
    /// herdr only accepts a plain command name, arguments without apostrophes
    /// or control characters, at most 64 arguments and 8 KiB. Hints that break
    /// those rules are left out; a source that breaks them cannot be resumed.
    pub fn resume_argv(&self) -> Option<Vec<String>> {
        let mut spec = self.clone();
        for hint in [
            &mut spec.hints.label,
            &mut spec.hints.agent,
            &mut spec.hints.title,
            &mut spec.hints.cwd,
        ] {
            if hint.as_deref().is_some_and(|value| !resumable_arg(value)) {
                *hint = None;
            }
        }
        let mut argv = vec!["remuda".to_string(), "view".to_string()];
        argv.extend(spec.to_args());
        let fits = argv.len() <= 64 && argv.iter().map(String::len).sum::<usize>() <= 8192;
        (fits && argv.iter().all(|arg| resumable_arg(arg))).then_some(argv)
    }

    /// Where the source lives, for labels: `ziggy` or `local`.
    pub fn place(&self) -> &str {
        self.machine.as_deref().unwrap_or("local")
    }

    /// The pane metadata tokens that describe this mirror. Absent settings map
    /// to `None` so a report clears stale values.
    ///
    /// herdr caps token values at 80 characters, so each setting gets its own
    /// token and long hints are cut, the same way herdr would cut them.
    pub fn to_tokens(&self) -> Vec<(&'static str, Option<String>)> {
        let some = |value: &str| Some(token_value(value));
        vec![
            (TOKEN_SOURCE, some(&self.source)),
            ("remuda_machine", self.machine.as_deref().and_then(some)),
            ("remuda_mode", some(self.mode.as_str())),
            ("remuda_size", some(self.size.as_str())),
            ("remuda_takeover", self.takeover.then(|| "1".to_string())),
            (
                "remuda_hint_label",
                self.hints.label.as_deref().and_then(some),
            ),
            (
                "remuda_hint_agent",
                self.hints.agent.as_deref().and_then(some),
            ),
            (
                "remuda_hint_title",
                self.hints.title.as_deref().and_then(some),
            ),
            ("remuda_hint_cwd", self.hints.cwd.as_deref().and_then(some)),
        ]
    }

    /// Reads a mirror back from a pane's `tokens`; `None` if it is not a mirror.
    pub fn from_tokens(tokens: &Value) -> Option<MirrorSpec> {
        let text = |key: &str| tokens[key].as_str().map(str::to_string);
        Some(MirrorSpec {
            source: text(TOKEN_SOURCE)?,
            machine: text("remuda_machine"),
            mode: text("remuda_mode")
                .and_then(|mode| parse_mode(&mode).ok())
                .unwrap_or_default(),
            size: text("remuda_size")
                .and_then(|size| size.parse().ok())
                .unwrap_or_default(),
            takeover: text("remuda_takeover").as_deref() == Some("1"),
            hints: Hints {
                label: text("remuda_hint_label"),
                agent: text("remuda_hint_agent"),
                title: text("remuda_hint_title"),
                cwd: text("remuda_hint_cwd"),
            },
        })
    }
}

/// The token every mirror carries; its presence marks a pane as a mirror.
pub const TOKEN_SOURCE: &str = "remuda_source";

/// Whether a herdr pane info object is a remuda mirror.
pub fn is_mirror(pane: &Value) -> bool {
    pane["tokens"][TOKEN_SOURCE].is_string()
}

/// A value as herdr stores it in a token: trimmed, without control
/// characters, at most 80 characters.
fn token_value(value: &str) -> String {
    let value: String = value
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(80)
        .collect();
    value.trim().to_string()
}

fn parse_mode(value: &str) -> Result<Mode, String> {
    match value {
        "view" => Ok(Mode::View),
        "control" => Ok(Mode::Control),
        other => Err(format!("mode must be view or control, not {other}")),
    }
}

fn resumable_arg(arg: &str) -> bool {
    !arg.contains('\'') && !arg.chars().any(char::is_control)
}

/// Finds the source pane among `panes` (herdr pane info objects).
///
/// The saved pane id wins when it still exists. Otherwise the pane that best
/// matches the hints is used, as long as the match is unambiguous and stronger
/// than a shared working directory alone.
pub fn resolve_source(spec: &MirrorSpec, panes: &[Value]) -> Option<String> {
    if panes
        .iter()
        .any(|pane| pane["pane_id"] == spec.source.as_str())
    {
        return Some(spec.source.clone());
    }
    let mut scored: Vec<(u32, &str)> = panes
        .iter()
        // Never resolve to another mirror.
        .filter(|pane| !is_mirror(pane))
        .filter_map(|pane| Some((hint_score(&spec.hints, pane), pane["pane_id"].as_str()?)))
        .filter(|(score, _)| *score >= 3)
        .collect();
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    match scored.as_slice() {
        [(best, id), rest @ ..] if rest.first().is_none_or(|(next, _)| next < best) => {
            Some((*id).to_string())
        }
        _ => None,
    }
}

fn hint_score(hints: &Hints, pane: &Value) -> u32 {
    let found = Hints::from_pane(pane);
    // Hints read back from tokens were cut to herdr's token length; compare
    // live values the same way.
    let matches = |want: &Option<String>, have: &Option<String>| match (want, have) {
        (Some(want), Some(have)) => token_value(want) == token_value(have),
        _ => false,
    };
    let mut score = 0;
    if matches(&hints.agent, &found.agent) {
        score += 4;
    }
    if matches(&hints.label, &found.label) {
        score += 3;
    }
    if matches(&hints.title, &found.title) {
        score += 2;
    }
    if matches(&hints.cwd, &found.cwd) {
        score += 1;
    }
    score
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec() -> MirrorSpec {
        MirrorSpec {
            source: "w8:p1".into(),
            machine: Some("ziggy".into()),
            mode: Mode::Control,
            size: Policy::Freeze,
            takeover: false,
            hints: Hints {
                agent: Some("claude".into()),
                cwd: Some("/work/today".into()),
                ..Hints::default()
            },
        }
    }

    #[test]
    fn args_round_trip() {
        let spec = spec();
        let mut parsed = MirrorSpec::default();
        parsed.apply_args(&spec.to_args()).unwrap();
        assert_eq!(parsed, spec);
    }

    #[test]
    fn token_json_round_trips_and_omits_defaults() {
        let spec = spec();
        let text = serde_json::to_string(&spec).unwrap();
        assert!(!text.contains("takeover"));
        assert_eq!(serde_json::from_str::<MirrorSpec>(&text).unwrap(), spec);
    }

    #[test]
    fn resume_argv_starts_with_plain_command_and_drops_bad_hints() {
        let mut spec = spec();
        spec.hints.title = Some("it's done".into());
        let argv = spec.resume_argv().unwrap();
        assert_eq!(&argv[..2], ["remuda", "view"]);
        assert!(!argv.iter().any(|arg| arg == "--hint-title"));
        assert!(argv.iter().any(|arg| arg == "--hint-agent"));
    }

    #[test]
    fn resume_argv_refuses_an_unresumable_source() {
        let mut spec = spec();
        spec.source = "w'1".into();
        assert_eq!(spec.resume_argv(), None);
    }

    #[test]
    fn unknown_view_options_are_rejected() {
        let mut spec = MirrorSpec::default();
        assert!(spec.apply_args(&["--bogus".into(), "x".into()]).is_err());
        assert!(spec.apply_args(&["--source".into()]).is_err());
    }

    #[test]
    fn resolve_keeps_an_existing_pane_id() {
        let panes = [
            json!({"pane_id": "w8:p1"}),
            json!({"pane_id": "w8:p2", "agent": "claude"}),
        ];
        assert_eq!(resolve_source(&spec(), &panes).as_deref(), Some("w8:p1"));
    }

    #[test]
    fn resolve_falls_back_to_the_best_unambiguous_hint_match() {
        let panes = [
            json!({"pane_id": "w9:p1", "cwd": "/work/today"}),
            json!({"pane_id": "w9:p2", "agent": "claude", "cwd": "/work/today"}),
        ];
        assert_eq!(resolve_source(&spec(), &panes).as_deref(), Some("w9:p2"));
    }

    #[test]
    fn resolve_refuses_ties_weak_matches_and_mirrors() {
        let tie = [
            json!({"pane_id": "w9:p1", "agent": "claude", "cwd": "/work/today"}),
            json!({"pane_id": "w9:p2", "agent": "claude", "cwd": "/work/today"}),
        ];
        assert_eq!(resolve_source(&spec(), &tie), None);
        let weak = [json!({"pane_id": "w9:p1", "cwd": "/work/today"})];
        assert_eq!(resolve_source(&spec(), &weak), None);
        let mirror =
            [json!({"pane_id": "w9:p3", "agent": "claude", "tokens": {"remuda_source": "w1:p1"}})];
        assert_eq!(resolve_source(&spec(), &mirror), None);
    }
    #[test]
    fn long_hints_are_cut_like_herdr_and_still_match() {
        let long = format!("/work/{}", "x".repeat(100));
        let spec = MirrorSpec {
            source: "w8:p1".into(),
            hints: Hints {
                agent: Some("claude".into()),
                cwd: Some(long.clone()),
                ..Hints::default()
            },
            ..MirrorSpec::default()
        };
        let tokens: serde_json::Map<String, Value> = spec
            .to_tokens()
            .into_iter()
            .filter_map(|(key, value)| Some((key.to_string(), Value::String(value?))))
            .collect();
        assert!(tokens
            .values()
            .all(|value| value.as_str().unwrap().chars().count() <= 80));
        let back = MirrorSpec::from_tokens(&Value::Object(tokens)).unwrap();
        let pane = json!({"pane_id": "w9:p1", "agent": "claude", "cwd": long});
        assert_eq!(hint_score(&back.hints, &pane), 5);
    }
}
