//! The size a controlled source PTY should have under each resize policy.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::herdr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}", self.cols, self.rows)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Policy {
    /// The source keeps the size its own layout gives it.
    #[default]
    Source,
    /// The source follows this mirror's size.
    Follow,
    /// The smaller of the two, per axis.
    Smallest,
    /// The size the source had when the mirror started.
    Freeze,
}

impl Policy {
    pub const ALL: [Policy; 4] = [
        Policy::Source,
        Policy::Follow,
        Policy::Smallest,
        Policy::Freeze,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Policy::Source => "source",
            Policy::Follow => "follow",
            Policy::Smallest => "smallest",
            Policy::Freeze => "freeze",
        }
    }

    pub fn next(self) -> Policy {
        let index = Policy::ALL.iter().position(|p| *p == self).unwrap_or(0);
        Policy::ALL[(index + 1) % Policy::ALL.len()]
    }

    pub fn size(self, mirror: Size, natural: Size, initial: Size) -> Size {
        match self {
            Policy::Source => natural,
            Policy::Follow => mirror,
            Policy::Smallest => Size {
                cols: mirror.cols.min(natural.cols),
                rows: mirror.rows.min(natural.rows),
            },
            Policy::Freeze => initial,
        }
    }
}

impl FromStr for Policy {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Policy::ALL
            .into_iter()
            .find(|p| p.as_str() == value)
            .ok_or_else(|| {
                let names: Vec<_> = Policy::ALL.iter().map(|p| p.as_str()).collect();
                format!("size policy must be one of {}", names.join(", "))
            })
    }
}

/// Tracks the size the source pane's own layout would give its PTY.
///
/// The PTY is smaller than the layout rect by the pane's chrome (borders and
/// the scrollbar gutter). That difference is measured once, while nothing
/// controls the source, and applied to later layout rects.
pub struct SourceGeometry {
    source: String,
    machine: Option<String>,
    chrome: (i32, i32),
    pub initial: Size,
    /// The server did not report the PTY size, so it was estimated.
    pub estimated: bool,
}

impl SourceGeometry {
    pub fn new(source: &str, machine: Option<&str>) -> Result<Self, String> {
        let rect = layout_rect(source, machine)?;
        let pane = herdr::json(&["pane", "get", source], machine)?;
        let pane = &pane["pane"];
        let (initial, estimated) = match reported_size(pane) {
            Some(size) => (size, false),
            None => (estimate_size(rect, viewport_rows(pane)), true),
        };
        Ok(Self {
            source: source.into(),
            machine: machine.map(str::to_string),
            chrome: (
                i32::from(rect.cols) - i32::from(initial.cols),
                i32::from(rect.rows) - i32::from(initial.rows),
            ),
            initial,
            estimated,
        })
    }

    pub fn natural(&self) -> Result<Size, String> {
        let rect = layout_rect(&self.source, self.machine.as_deref())?;
        Ok(apply_chrome(rect, self.chrome))
    }
}

fn layout_rect(source: &str, machine: Option<&str>) -> Result<Size, String> {
    let result = herdr::json(&["pane", "layout", "--pane", source], machine)?;
    let layout = &result["layout"];
    let rect = layout["panes"]
        .as_array()
        .and_then(|panes| panes.iter().find(|p| p["pane_id"] == source))
        .map(|pane| &pane["rect"])
        // A zoomed tab lists only the focused pane; fall back to the whole area.
        .unwrap_or(&layout["area"]);
    let cols = rect["width"].as_u64().ok_or("pane layout has no width")?;
    let rows = rect["height"].as_u64().ok_or("pane layout has no height")?;
    Ok(Size {
        cols: clamp_u16(cols),
        rows: clamp_u16(rows),
    })
}

/// The PTY size from `pane.size`, which only newer herdr servers report.
fn reported_size(pane: &Value) -> Option<Size> {
    let size = pane.get("size")?;
    Some(Size {
        cols: clamp_u16(size["cols"].as_u64()?),
        rows: clamp_u16(size["rows"].as_u64()?),
    })
}

fn viewport_rows(pane: &Value) -> Option<u16> {
    pane["scroll"]["viewport_rows"].as_u64().map(clamp_u16)
}

/// Estimates the PTY size from the layout rect when the server only reports rows.
///
/// Rows are exact. Borders cost as many columns as rows, and the stable scrollbar
/// gutter costs one more column outside the alternate screen.
fn estimate_size(rect: Size, viewport_rows: Option<u16>) -> Size {
    let rows = viewport_rows.unwrap_or(rect.rows).min(rect.rows);
    let borders = rect.rows - rows;
    Size {
        cols: rect.cols.saturating_sub(borders).saturating_sub(1).max(2),
        rows,
    }
}

fn apply_chrome(rect: Size, chrome: (i32, i32)) -> Size {
    Size {
        cols: clamp_u16((i32::from(rect.cols) - chrome.0).max(2) as u64),
        rows: clamp_u16((i32::from(rect.rows) - chrome.1).max(1) as u64),
    }
}

fn clamp_u16(value: u64) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIRROR: Size = Size { cols: 92, rows: 53 };
    const NATURAL: Size = Size { cols: 64, rows: 54 };
    const INITIAL: Size = Size { cols: 93, rows: 54 };

    #[test]
    fn policies_pick_their_size() {
        assert_eq!(Policy::Source.size(MIRROR, NATURAL, INITIAL), NATURAL);
        assert_eq!(Policy::Follow.size(MIRROR, NATURAL, INITIAL), MIRROR);
        assert_eq!(Policy::Freeze.size(MIRROR, NATURAL, INITIAL), INITIAL);
        assert_eq!(
            Policy::Smallest.size(MIRROR, NATURAL, INITIAL),
            Size { cols: 64, rows: 53 }
        );
    }

    #[test]
    fn policy_names_round_trip_and_cycle() {
        for policy in Policy::ALL {
            assert_eq!(policy.as_str().parse::<Policy>(), Ok(policy));
        }
        assert_eq!(Policy::Freeze.next(), Policy::Source);
        assert!("bogus".parse::<Policy>().is_err());
    }

    #[test]
    fn estimate_without_borders_drops_the_scrollbar_gutter() {
        // Measured on a borderless pane: rect 191x56, PTY 190x56.
        let rect = Size {
            cols: 191,
            rows: 56,
        };
        assert_eq!(
            estimate_size(rect, Some(56)),
            Size {
                cols: 190,
                rows: 56
            }
        );
    }

    #[test]
    fn estimate_with_borders_drops_border_columns_too() {
        let rect = Size {
            cols: 100,
            rows: 40,
        };
        assert_eq!(estimate_size(rect, Some(38)), Size { cols: 97, rows: 38 });
    }

    #[test]
    fn chrome_carries_over_to_new_layout_rects() {
        let chrome = (1, 0);
        assert_eq!(
            apply_chrome(Size { cols: 65, rows: 54 }, chrome),
            Size { cols: 64, rows: 54 }
        );
    }

    #[test]
    fn reported_size_wins_when_present() {
        let pane = serde_json::json!({"size": {"cols": 120, "rows": 40}});
        assert_eq!(
            reported_size(&pane),
            Some(Size {
                cols: 120,
                rows: 40
            })
        );
        assert_eq!(reported_size(&serde_json::json!({})), None);
    }
}
