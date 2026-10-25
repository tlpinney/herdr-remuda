//! Splits what the mirror pane receives into keystrokes and mouse events.
//!
//! A control mirror asks herdr for SGR mouse reporting, so the wheel and clicks
//! arrive as `ESC [ < b ; x ; y M|m` instead of herdr's fallback for
//! full-screen apps, arrow keys, which in an agent's prompt would page
//! through input history. Mouse events become `terminal.scroll` and
//! `terminal.mouse` commands; everything else is forwarded as typed.
//!
//! A sequence split across reads is held until the rest arrives. A lone
//! Escape looks like the start of one, so `flush` releases held bytes once
//! input pauses; Escape is never swallowed.

/// What the mirror pane asks its terminal for: button presses, drags with a
/// button held, and SGR coordinates.
pub const MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1006h";
pub const MOUSE_OFF: &[u8] = b"\x1b[?1006l\x1b[?1002l\x1b[?1000l";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
}

impl Button {
    pub fn as_str(self) -> &'static str {
        match self {
            Button::Left => "left",
            Button::Middle => "middle",
            Button::Right => "right",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    Down(Button),
    Up(Button),
    Drag(Button),
    Move,
    WheelUp,
    WheelDown,
}

/// A mouse event at a zero-based cell, with herdr's modifier bits
/// (Shift = 1, Ctrl = 2, Alt = 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mouse {
    pub action: MouseAction,
    pub column: u16,
    pub row: u16,
    pub modifiers: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    Keys(Vec<u8>),
    Mouse(Mouse),
}

#[derive(Default)]
pub struct Parser {
    pending: Vec<u8>,
}

impl Parser {
    /// Splits `bytes` (plus anything held from before) into pieces.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Piece> {
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(bytes);
        let mut pieces = Vec::new();
        let mut keys = Vec::new();
        let mut i = 0;
        while i < data.len() {
            if data[i] == 0x1b {
                match parse_sgr_mouse(&data[i..]) {
                    Sgr::Complete(mouse, len) => {
                        flush_keys(&mut keys, &mut pieces);
                        if let Some(mouse) = mouse {
                            pieces.push(Piece::Mouse(mouse));
                        }
                        i += len;
                        continue;
                    }
                    Sgr::Partial => {
                        self.pending = data[i..].to_vec();
                        break;
                    }
                    Sgr::NotMouse => {}
                }
            }
            keys.push(data[i]);
            i += 1;
        }
        flush_keys(&mut keys, &mut pieces);
        pieces
    }

    /// Releases held bytes as keystrokes when input has paused.
    pub fn flush(&mut self) -> Option<Vec<u8>> {
        (!self.pending.is_empty()).then(|| std::mem::take(&mut self.pending))
    }

    pub fn holding(&self) -> bool {
        !self.pending.is_empty()
    }
}

fn flush_keys(keys: &mut Vec<u8>, pieces: &mut Vec<Piece>) {
    if !keys.is_empty() {
        pieces.push(Piece::Keys(std::mem::take(keys)));
    }
}

enum Sgr {
    /// A whole sequence of this many bytes; `None` for one we ignore.
    Complete(Option<Mouse>, usize),
    /// Could still become a mouse sequence; wait for more bytes.
    Partial,
    NotMouse,
}

/// Parses `ESC [ < b ; x ; y (M|m)` at the start of `data`.
fn parse_sgr_mouse(data: &[u8]) -> Sgr {
    const PREFIX: &[u8] = b"\x1b[<";
    let prefix_len = PREFIX.len().min(data.len());
    if data[..prefix_len] != PREFIX[..prefix_len] {
        return Sgr::NotMouse;
    }
    if data.len() < PREFIX.len() {
        return Sgr::Partial;
    }
    let body = &data[PREFIX.len()..];
    let Some(end) = body
        .iter()
        .position(|b| !(b.is_ascii_digit() || *b == b';'))
    else {
        // Digits so far; a real sequence is short, so give up on runaway input.
        return if body.len() < 24 {
            Sgr::Partial
        } else {
            Sgr::NotMouse
        };
    };
    let terminator = body[end];
    if terminator != b'M' && terminator != b'm' {
        return Sgr::NotMouse;
    }
    let numbers: Vec<u16> = body[..end]
        .split(|b| *b == b';')
        .filter_map(|n| std::str::from_utf8(n).ok()?.parse().ok())
        .collect();
    let len = PREFIX.len() + end + 1;
    let [code, x, y] = numbers[..] else {
        return Sgr::Complete(None, len);
    };
    Sgr::Complete(decode(code, x, y, terminator == b'm'), len)
}

fn decode(code: u16, x: u16, y: u16, release: bool) -> Option<Mouse> {
    let modifiers = (if code & 4 != 0 { 1 } else { 0 })
        | (if code & 16 != 0 { 2 } else { 0 })
        | (if code & 8 != 0 { 4 } else { 0 });
    let button = match code & 3 {
        0 => Some(Button::Left),
        1 => Some(Button::Middle),
        2 => Some(Button::Right),
        _ => None,
    };
    let action = if code & 64 != 0 {
        match code & 3 {
            0 => MouseAction::WheelUp,
            1 => MouseAction::WheelDown,
            // Horizontal wheel: nothing to scroll.
            _ => return None,
        }
    } else if code & 32 != 0 {
        match button {
            Some(button) => MouseAction::Drag(button),
            None => MouseAction::Move,
        }
    } else if release {
        MouseAction::Up(button.unwrap_or(Button::Left))
    } else {
        MouseAction::Down(button?)
    };
    Some(Mouse {
        action,
        column: x.saturating_sub(1),
        row: y.saturating_sub(1),
        modifiers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mouse(action: MouseAction, column: u16, row: u16) -> Piece {
        Piece::Mouse(Mouse {
            action,
            column,
            row,
            modifiers: 0,
        })
    }

    #[test]
    fn plain_keys_pass_through() {
        let mut parser = Parser::default();
        assert_eq!(parser.feed(b"ls\r"), [Piece::Keys(b"ls\r".to_vec())]);
        assert!(!parser.holding());
    }

    #[test]
    fn wheel_and_clicks_become_mouse_events_with_zero_based_cells() {
        let mut parser = Parser::default();
        assert_eq!(
            parser.feed(b"\x1b[<64;10;5M\x1b[<65;10;5M"),
            [
                mouse(MouseAction::WheelUp, 9, 4),
                mouse(MouseAction::WheelDown, 9, 4)
            ]
        );
        assert_eq!(
            parser.feed(b"\x1b[<0;1;1M\x1b[<32;2;1M\x1b[<0;2;1m"),
            [
                mouse(MouseAction::Down(Button::Left), 0, 0),
                mouse(MouseAction::Drag(Button::Left), 1, 0),
                mouse(MouseAction::Up(Button::Left), 1, 0),
            ]
        );
    }

    #[test]
    fn modifiers_map_to_herdr_bits() {
        let mut parser = Parser::default();
        // Shift (4) + Ctrl (16) on a right click.
        let pieces = parser.feed(b"\x1b[<22;3;4M");
        let [Piece::Mouse(event)] = &pieces[..] else {
            panic!("expected one mouse event, got {pieces:?}");
        };
        assert_eq!(event.action, MouseAction::Down(Button::Right));
        assert_eq!(event.modifiers, 1 | 2);
    }

    #[test]
    fn keys_around_mouse_events_keep_their_order() {
        let mut parser = Parser::default();
        assert_eq!(
            parser.feed(b"a\x1b[<64;1;1Mb"),
            [
                Piece::Keys(b"a".to_vec()),
                mouse(MouseAction::WheelUp, 0, 0),
                Piece::Keys(b"b".to_vec()),
            ]
        );
    }

    #[test]
    fn split_sequences_are_held_until_complete() {
        let mut parser = Parser::default();
        assert_eq!(parser.feed(b"x\x1b[<64;1"), [Piece::Keys(b"x".to_vec())]);
        assert!(parser.holding());
        assert_eq!(parser.feed(b"2;3M"), [mouse(MouseAction::WheelUp, 11, 2)]);
    }

    #[test]
    fn a_lone_escape_is_released_by_flush() {
        let mut parser = Parser::default();
        assert_eq!(parser.feed(b"\x1b"), []);
        assert_eq!(parser.flush(), Some(b"\x1b".to_vec()));
        assert_eq!(parser.flush(), None);
    }

    #[test]
    fn other_escape_sequences_are_keys() {
        let mut parser = Parser::default();
        // Arrow up and a bracketed paste start pass straight through.
        assert_eq!(
            parser.feed(b"\x1b[A\x1b[200~"),
            [Piece::Keys(b"\x1b[A\x1b[200~".to_vec())]
        );
        assert!(!parser.holding());
    }
}
