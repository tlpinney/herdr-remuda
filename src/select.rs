//! Drag-to-copy in a control mirror.
//!
//! A control mirror asks for mouse reporting so the wheel and clicks reach
//! the source, and that stops herdr from selecting text in the pane: drags
//! come to remuda instead. Forwarding them does not help either, because the
//! source's herdr drops drags unless its app asked for the mouse, and a copy
//! made there would land on that machine's clipboard anyway.
//!
//! So a plain left drag selects in the mirror itself. remuda paints the
//! selection over what the pane shows, reads the selected text back from this
//! machine's herdr (the mirror pane holds exactly what is on screen), and
//! copies it with OSC 52, which herdr puts on this machine's clipboard. A
//! drag with a modifier held still goes to the source, for apps that select
//! on their own.

use base64::Engine;
use serde_json::json;
use unicode_width::UnicodeWidthChar;

use crate::herdr;

/// A cell as (row, column), zero-based within the mirror.
pub type Cell = (u16, u16);

/// A stream selection from `anchor` (where the drag began) to `cursor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: Cell,
    pub cursor: Cell,
}

impl Selection {
    /// Start and end in reading order.
    pub fn ordered(&self) -> (Cell, Cell) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    /// The rows the selection covers.
    pub fn rows(&self) -> std::ops::RangeInclusive<u16> {
        let (start, end) = self.ordered();
        start.0..=end.0
    }

    /// The selected columns on `row`, inclusive, in a pane `cols` wide: the
    /// first row runs from the start to the edge, middle rows are whole, the
    /// last row runs from the edge to the end.
    pub fn span(&self, row: u16, cols: u16) -> Option<(u16, u16)> {
        let (start, end) = self.ordered();
        if row < start.0 || row > end.0 || cols == 0 {
            return None;
        }
        let last = cols - 1;
        let from = if row == start.0 { start.1.min(last) } else { 0 };
        let to = if row == end.0 { end.1.min(last) } else { last };
        Some((from, to))
    }
}

/// What the mirror pane showed when a drag began: each row as herdr renders
/// it (with its styles, to restore) and as plain cells (to paint over).
#[derive(Debug, Default)]
pub struct Snapshot {
    styled: Vec<String>,
    plain: Vec<String>,
}

impl Snapshot {
    /// Reads the mirror pane's visible rows from this machine's herdr.
    pub fn read(pane_id: &str) -> Result<Snapshot, String> {
        let result = herdr::socket_request(
            "pane.read",
            json!({"pane_id": pane_id, "source": "visible", "format": "ansi", "strip_ansi": false}),
        )?;
        let text = result["read"]["text"]
            .as_str()
            .ok_or("pane.read: no text")?;
        Ok(Snapshot::from_ansi(text))
    }

    pub fn from_ansi(text: &str) -> Snapshot {
        let styled: Vec<String> = text
            .split('\n')
            .map(|row| row.strip_suffix('\r').unwrap_or(row).to_string())
            .collect();
        let plain = styled.iter().map(|row| strip_ansi(row)).collect();
        Snapshot { styled, plain }
    }

    /// Bytes that put `rows` back as they were.
    pub fn restore(&self, rows: impl IntoIterator<Item = u16>) -> Vec<u8> {
        let mut out = Vec::new();
        for row in rows {
            if let Some(styled) = self.styled.get(usize::from(row)) {
                out.extend_from_slice(format!("\x1b[{};1H\x1b[0m", row + 1).as_bytes());
                out.extend_from_slice(styled.as_bytes());
                out.extend_from_slice(b"\x1b[0m");
            }
        }
        out
    }

    /// Bytes that paint `selection` in reverse video over the snapshot's
    /// cells, in a pane `cols` wide.
    pub fn paint(&self, selection: &Selection, cols: u16) -> Vec<u8> {
        let mut out = Vec::new();
        for row in selection.rows() {
            let Some((from, to)) = selection.span(row, cols) else {
                continue;
            };
            let plain = self.plain.get(usize::from(row)).map_or("", String::as_str);
            let (start, text) = cells(plain, from, to);
            out.extend_from_slice(
                format!("\x1b[{};{}H\x1b[0;7m{text}\x1b[0m", row + 1, start + 1).as_bytes(),
            );
        }
        out
    }
}

/// The characters of `row` that start in columns `from..=to`, and the column
/// the first one starts at. Past the end of the row's text, blanks fill in.
fn cells(row: &str, from: u16, to: u16) -> (u16, String) {
    let mut column: u16 = 0;
    let mut start = None;
    let mut text = String::new();
    for c in row.chars() {
        let width = c.width().unwrap_or(0) as u16;
        if width == 0 {
            if start.is_some() {
                text.push(c);
            }
            continue;
        }
        if column > to {
            break;
        }
        if column >= from {
            start.get_or_insert(column);
            text.push(c);
        }
        column += width;
    }
    let start = start.unwrap_or(column.max(from));
    let end = column.max(start);
    if end <= to {
        text.extend(std::iter::repeat_n(' ', usize::from(to + 1 - end)));
    }
    (start, text)
}

/// `row` without its escape sequences.
fn strip_ansi(row: &str) -> String {
    let mut out = String::with_capacity(row.len());
    let mut chars = row.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI: parameters, then one final byte.
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: up to BEL or ST.
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\x07' {
                        break;
                    }
                    if c == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// The selected text, read from this machine's herdr. The mirror pane is on
/// the alternate screen, so its rows are absolute rows.
pub fn read_text(pane_id: &str, selection: &Selection) -> Result<String, String> {
    let (start, end) = selection.ordered();
    let result = herdr::socket_request(
        "pane.selection.read",
        json!({
            "pane_id": pane_id,
            "anchor": {"row": start.0, "col": start.1},
            "cursor": {"row": end.0, "col": end.1},
        }),
    )?;
    Ok(result["text"].as_str().unwrap_or_default().to_string())
}

/// OSC 52 that sets the clipboard to `text`; herdr passes a pane's clipboard
/// writes on to this machine's clipboard.
pub fn osc52(text: &str) -> Vec<u8> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    format!("\x1b]52;c;{encoded}\x07").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selection(anchor: Cell, cursor: Cell) -> Selection {
        Selection { anchor, cursor }
    }

    #[test]
    fn a_backwards_drag_reads_in_order() {
        let s = selection((3, 4), (1, 9));
        assert_eq!(s.ordered(), ((1, 9), (3, 4)));
        assert_eq!(s.rows(), 1..=3);
    }

    #[test]
    fn spans_run_to_the_edges_between_the_ends() {
        let s = selection((1, 5), (3, 2));
        assert_eq!(s.span(0, 10), None);
        assert_eq!(s.span(1, 10), Some((5, 9)));
        assert_eq!(s.span(2, 10), Some((0, 9)));
        assert_eq!(s.span(3, 10), Some((0, 2)));
        assert_eq!(s.span(4, 10), None);
        // One row, either direction.
        assert_eq!(selection((2, 7), (2, 3)).span(2, 10), Some((3, 7)));
    }

    #[test]
    fn styles_are_stripped_but_cells_kept() {
        let snapshot = Snapshot::from_ansi("\x1b[0m\x1b[38;2;1;2;3mab\x1b[0m cd  \r\nxy");
        assert_eq!(snapshot.plain, ["ab cd  ", "xy"]);
        assert_eq!(snapshot.styled[0], "\x1b[0m\x1b[38;2;1;2;3mab\x1b[0m cd  ");
    }

    #[test]
    fn painting_reverses_the_spans_text() {
        let snapshot = Snapshot::from_ansi("hello world\r\nsecond row");
        let painted = snapshot.paint(&selection((0, 6), (1, 2)), 11);
        assert_eq!(
            String::from_utf8(painted).unwrap(),
            "\x1b[1;7H\x1b[0;7mworld\x1b[0m\x1b[2;1H\x1b[0;7msec\x1b[0m"
        );
    }

    #[test]
    fn wide_characters_keep_their_columns() {
        // "日" covers columns 0-1, "本" 2-3.
        assert_eq!(cells("日本x", 2, 4), (2, "本x".into()));
        // Starting on a wide character's second half begins at the next one.
        assert_eq!(cells("日本x", 1, 2), (2, "本".into()));
        // Past the text, blanks.
        assert_eq!(cells("ab", 1, 4), (1, "b   ".into()));
        assert_eq!(cells("", 3, 4), (3, "  ".into()));
    }

    #[test]
    fn restoring_puts_styled_rows_back() {
        let snapshot = Snapshot::from_ansi("\x1b[1mbold\x1b[0m\r\nplain");
        assert_eq!(
            String::from_utf8(snapshot.restore([1, 0, 7])).unwrap(),
            "\x1b[2;1H\x1b[0mplain\x1b[0m\x1b[1;1H\x1b[0m\x1b[1mbold\x1b[0m\x1b[0m"
        );
    }

    #[test]
    fn osc52_sets_the_clipboard() {
        assert_eq!(osc52("hello"), b"\x1b]52;c;aGVsbG8=\x07");
    }
}
