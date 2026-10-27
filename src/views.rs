//! Saved views: a tab's layout with its mirrors, kept as JSON and reopened as a
//! new tab with `layout.apply`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::herdr;
use crate::spec::MirrorSpec;

const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Node {
    Split {
        direction: String,
        ratio: f64,
        first: Box<Node>,
        second: Box<Node>,
    },
    Pane {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mirror: Option<MirrorSpec>,
    },
}

impl Node {
    fn panes(&self) -> Vec<&Node> {
        match self {
            Node::Split { first, second, .. } => {
                let mut panes = first.panes();
                panes.extend(second.panes());
                panes
            }
            pane => vec![pane],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedView {
    pub version: u32,
    pub name: String,
    pub saved_at: u64,
    pub root: Node,
}

impl SavedView {
    pub fn pane_count(&self) -> usize {
        self.root.panes().len()
    }

    pub fn mirror_count(&self) -> usize {
        self.root
            .panes()
            .into_iter()
            .filter(|pane| {
                matches!(
                    pane,
                    Node::Pane {
                        mirror: Some(_),
                        ..
                    }
                )
            })
            .count()
    }
}

/// `$HERDR_PLUGIN_STATE_DIR/views`.
pub fn views_dir() -> PathBuf {
    state_dir().join("views")
}

/// `$HERDR_PLUGIN_STATE_DIR`. Outside a plugin command (the CLI, or a mirror
/// herdr resumed) this is the same directory, found the way herdr finds
/// plugin state on unix: `$XDG_STATE_HOME/herdr/plugins/<id>` or
/// `~/.local/state/herdr/plugins/<id>`.
pub fn state_dir() -> PathBuf {
    std::env::var_os("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let state = std::env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
                });
            state.join("herdr/plugins").join(herdr::plugin_id())
        })
}

/// The file name for a view: lowercase letters, digits and dashes.
pub fn slug(name: &str) -> String {
    let mut slug = String::new();
    for c in name.trim().chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        "view".into()
    } else {
        slug.into()
    }
}

fn path_for(name: &str) -> PathBuf {
    views_dir().join(format!("{}.json", slug(name)))
}

pub fn exists(name: &str) -> bool {
    path_for(name).is_file()
}

/// Saved views sorted by name; unreadable files are skipped.
pub fn list() -> Vec<SavedView> {
    let Ok(entries) = std::fs::read_dir(views_dir()) else {
        return Vec::new();
    };
    let mut views: Vec<SavedView> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| std::fs::read(entry.path()).ok())
        .filter_map(|bytes| serde_json::from_slice(&bytes).ok())
        .collect();
    views.sort_by_key(|view| view.name.to_lowercase());
    views
}

pub fn load(name: &str) -> Result<SavedView, String> {
    let path = path_for(name);
    let bytes = std::fs::read(&path).map_err(|err| format!("read {}: {err}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|err| format!("parse {}: {err}", path.display()))
}

pub fn delete(name: &str) -> Result<(), String> {
    let path = path_for(name);
    std::fs::remove_file(&path).map_err(|err| format!("delete {}: {err}", path.display()))
}

/// Saves the tab `tab_id` as the view `name`, replacing any view of that name.
pub fn save_tab(name: &str, tab_id: &str) -> Result<SavedView, String> {
    let export = herdr::socket_request("layout.export", json!({"tab_id": tab_id}))?;
    let listed = herdr::json(&["pane", "list"], None)?;
    let panes: HashMap<String, Value> = listed["panes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|pane| Some((pane["pane_id"].as_str()?.to_string(), pane.clone())))
        .collect();
    let view = SavedView {
        version: VERSION,
        name: name.trim().to_string(),
        saved_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default(),
        root: from_export(&export["layout"]["root"], &panes)?,
    };
    write(&view)?;
    Ok(view)
}

fn write(view: &SavedView) -> Result<(), String> {
    let dir = views_dir();
    std::fs::create_dir_all(&dir).map_err(|err| format!("create {}: {err}", dir.display()))?;
    let path = path_for(&view.name);
    let temp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(view).map_err(|err| err.to_string())?;
    std::fs::write(&temp, bytes).map_err(|err| format!("write {}: {err}", temp.display()))?;
    std::fs::rename(&temp, &path).map_err(|err| format!("write {}: {err}", path.display()))
}

/// Converts a `layout.export` tree, using pane infos to recognise mirrors.
fn from_export(node: &Value, panes: &HashMap<String, Value>) -> Result<Node, String> {
    match node["type"].as_str() {
        Some("split") => Ok(Node::Split {
            direction: node["direction"].as_str().unwrap_or("right").to_string(),
            ratio: node["ratio"].as_f64().unwrap_or(0.5),
            first: Box::new(from_export(&node["first"], panes)?),
            second: Box::new(from_export(&node["second"], panes)?),
        }),
        Some("pane") => {
            let info = node["pane_id"].as_str().and_then(|id| panes.get(id));
            let mirror = info.and_then(|info| MirrorSpec::from_tokens(&info["tokens"]));
            // A mirror labels itself when it starts; keep only user labels.
            let label = if mirror.is_some() {
                None
            } else {
                info.and_then(|info| info["label"].as_str())
                    .or_else(|| node["label"].as_str())
                    .map(str::to_string)
            };
            let cwd = node["cwd"]
                .as_str()
                .or_else(|| info.and_then(|info| info["cwd"].as_str()))
                .map(str::to_string);
            Ok(Node::Pane { label, cwd, mirror })
        }
        other => Err(format!("unexpected layout node type {other:?}")),
    }
}

/// Opens `view` as a new tab in `workspace_id`.
pub fn open(view: &SavedView, workspace_id: &str, focus: bool) -> Result<(), String> {
    let exe = std::env::current_exe()
        .map_err(|err| format!("locate remuda: {err}"))?
        .to_string_lossy()
        .into_owned();
    let params = json!({
        "workspace_id": workspace_id,
        "tab_label": view.name,
        "focus": focus,
        "root": to_apply(&view.root, &exe),
    });
    herdr::socket_request("layout.apply", params).map(|_| ())
}

/// Converts a saved tree to `layout.apply` form. Mirrors run `remuda view`.
fn to_apply(node: &Node, exe: &str) -> Value {
    match node {
        Node::Split {
            direction,
            ratio,
            first,
            second,
        } => json!({
            "type": "split",
            "direction": direction,
            "ratio": ratio,
            "first": to_apply(first, exe),
            "second": to_apply(second, exe),
        }),
        Node::Pane { label, cwd, mirror } => {
            let mut pane = json!({"type": "pane"});
            if let Some(cwd) = cwd {
                pane["cwd"] = json!(cwd);
            }
            if let Some(label) = label {
                pane["label"] = json!(label);
            }
            if let Some(mirror) = mirror {
                let mut command = vec![exe.to_string(), "view".to_string()];
                command.extend(mirror.to_args());
                pane["command"] = json!(command);
                if let Ok(bin) = std::env::var("HERDR_BIN_PATH") {
                    pane["env"] = json!({"HERDR_BIN_PATH": bin});
                }
            }
            pane
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::Mode;

    fn export() -> Value {
        json!({
            "type": "split", "direction": "right", "ratio": 0.6,
            "first": {"type": "pane", "pane_id": "w1:p1", "cwd": "/repo"},
            "second": {"type": "pane", "pane_id": "w1:p2", "cwd": "/repo"},
        })
    }

    fn panes() -> HashMap<String, Value> {
        let tokens = json!({
            "remuda_source": "w8:p1",
            "remuda_machine": "devbox",
            "remuda_mode": "control",
            "remuda_size": "freeze",
        });
        HashMap::from([
            (
                "w1:p1".to_string(),
                json!({"pane_id": "w1:p1", "label": "editor"}),
            ),
            (
                "w1:p2".to_string(),
                json!({"pane_id": "w1:p2", "label": "\u{21c4} devbox:w8:p1", "tokens": tokens}),
            ),
        ])
    }

    #[test]
    fn export_keeps_user_labels_and_recognises_mirrors() {
        let root = from_export(&export(), &panes()).unwrap();
        let Node::Split {
            ratio,
            first,
            second,
            ..
        } = &root
        else {
            panic!("expected a split");
        };
        assert_eq!(*ratio, 0.6);
        assert_eq!(
            **first,
            Node::Pane {
                label: Some("editor".into()),
                cwd: Some("/repo".into()),
                mirror: None
            }
        );
        let Node::Pane {
            label,
            mirror: Some(mirror),
            ..
        } = &**second
        else {
            panic!("expected a mirror");
        };
        assert_eq!(*label, None);
        assert_eq!(mirror.mode, Mode::Control);
        assert_eq!(mirror.machine.as_deref(), Some("devbox"));
    }

    #[test]
    fn apply_runs_remuda_view_for_mirrors_only() {
        let root = from_export(&export(), &panes()).unwrap();
        let apply = to_apply(&root, "/bin/remuda");
        assert_eq!(apply["first"]["label"], "editor");
        assert!(apply["first"].get("command").is_none());
        let command: Vec<&str> = apply["second"]["command"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| arg.as_str().unwrap())
            .collect();
        assert_eq!(&command[..4], ["/bin/remuda", "view", "--source", "w8:p1"]);
        assert!(command.contains(&"freeze"));
    }

    #[test]
    fn saved_views_round_trip_and_count_panes() {
        let view = SavedView {
            version: VERSION,
            name: "Ops".into(),
            saved_at: 1,
            root: from_export(&export(), &panes()).unwrap(),
        };
        let text = serde_json::to_string(&view).unwrap();
        let back: SavedView = serde_json::from_str(&text).unwrap();
        assert_eq!(back, view);
        assert_eq!((view.pane_count(), view.mirror_count()), (2, 1));
    }

    #[test]
    fn slugs_are_file_safe() {
        assert_eq!(slug("Ops: Devbox + local"), "ops-devbox-local");
        assert_eq!(slug("  "), "view");
        assert_eq!(slug("../etc"), "etc");
    }
}
