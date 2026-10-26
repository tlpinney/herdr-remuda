//! Splits what the mirror pane receives into keystrokes and mouse events.
//!
//! A control mirror asks herdr for SGR mouse reporting, so the wheel and clicks
//! arrive as `ESC [ < b ; x ; y M|m` instead of herdr's fallback for
//! full-screen apps, arrow keys, which in an agent's prompt would page
//! through input history. Mouse events become `terminal.scroll` and
//! `terminal.mouse` commands; everything else is forwarded as typed.
//!
//! Modified keys need help too. herdr's `terminal session` stream does not say
//! which keyboard mode the source's app has enabled, so a mirror cannot match
//! it, and with classic encoding Shift+Enter is indistinguishable from Enter;
//! in an agent's prompt that submits instead of starting a new line. So the
//! mirror asks for xterm modifyOtherKeys (level 2), under which herdr sends
//! every modified key as `ESC [ 27 ; mod ; code ~`, and converts those back to
//! classic keys for the source, sending Shift+Enter as Escape-Enter, which
//! agents such as Claude Code read as a new line.
//!
//! A sequence split across reads is held until the rest arrives. A lone
//! Escape looks like the start of one, so `flush` releases held bytes once
//! input pauses; Escape is never swallowed.

/// What the mirror pane asks its terminal for: button presses, drags with a
/// button held, and SGR coordinates.
pub const MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1006h";
pub const MOUSE_OFF: &[u8] = b"\x1b[?1006l\x1b[?1002l\x1b[?1000l";
/// modifyOtherKeys level 2 on and off.
pub const MODIFIED_KEYS_ON: &[u8] = b"\x1b[>4;2m";
pub const MODIFIED_KEYS_OFF: &[u8] = b"\x1b[>4;0m";

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
                let rest = &data[i..];
                match (parse_sgr_mouse(rest), parse_modified_key(rest)) {
                    (Seq::Complete(mouse, len), _) => {
                        flush_keys(&mut keys, &mut pieces);
                        if let Some(mouse) = mouse {
                            pieces.push(Piece::Mouse(mouse));
                        }
                        i += len;
                        continue;
                    }
                    (_, Seq::Complete(key, len)) => {
                        keys.extend(key.unwrap_or_default());
                        i += len;
                        continue;
                    }
                    (Seq::Partial, _) | (_, Seq::Partial) => {
                        self.pending = rest.to_vec();
                        break;
                    }
                    (Seq::Other, Seq::Other) => {}
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

enum Seq<T> {
    /// A whole sequence of this many bytes; `None` for one we ignore.
    Complete(Option<T>, usize),
    /// Could still become this kind of sequence; wait for more bytes.
    Partial,
    Other,
}

/// Matches `prefix`, then digits and semicolons, then a terminator from
/// `terminators`. Returns the numbers, the terminator and the length.
fn parse_csi<T>(
    data: &[u8],
    prefix: &[u8],
    terminators: &[u8],
) -> Result<(Vec<u32>, u8, usize), Seq<T>> {
    let prefix_len = prefix.len().min(data.len());
    if data[..prefix_len] != prefix[..prefix_len] {
        return Err(Seq::Other);
    }
    if data.len() < prefix.len() {
        return Err(Seq::Partial);
    }
    let body = &data[prefix.len()..];
    let Some(end) = body
        .iter()
        .position(|b| !(b.is_ascii_digit() || *b == b';'))
    else {
        // Digits so far; a real sequence is short, so give up on runaway input.
        return Err(if body.len() < 24 {
            Seq::Partial
        } else {
            Seq::Other
        });
    };
    if !terminators.contains(&body[end]) {
        return Err(Seq::Other);
    }
    let numbers = body[..end]
        .split(|b| *b == b';')
        .filter_map(|n| std::str::from_utf8(n).ok()?.parse().ok())
        .collect();
    Ok((numbers, body[end], prefix.len() + end + 1))
}

/// Parses `ESC [ < b ; x ; y (M|m)` at the start of `data`.
fn parse_sgr_mouse(data: &[u8]) -> Seq<Mouse> {
    match parse_csi(data, b"\x1b[<", b"Mm") {
        Ok((numbers, terminator, len)) => match numbers[..] {
            [code, x, y] => Seq::Complete(
                decode(
                    u16::try_from(code).unwrap_or(0),
                    u16::try_from(x).unwrap_or(u16::MAX),
                    u16::try_from(y).unwrap_or(u16::MAX),
                    terminator == b'm',
                ),
                len,
            ),
            _ => Seq::Complete(None, len),
        },
        Err(seq) => seq,
    }
}

/// Parses a modifyOtherKeys key, `ESC [ 27 ; mod ; code ~`, into the bytes a
/// classic terminal sends for it.
fn parse_modified_key(data: &[u8]) -> Seq<Vec<u8>> {
    match parse_csi(data, b"\x1b[27;", b"~") {
        Ok((numbers, _, len)) => match numbers[..] {
            [modifier, code] => Seq::Complete(Some(classic_key(modifier, code)), len),
            _ => Seq::Complete(None, len),
        },
        Err(seq) => seq,
    }
}

/// What a classic terminal sends for key `code` with xterm modifier `modifier`
/// (1 + Shift 1 + Alt 2 + Ctrl 4). Shift+Enter and Alt+Enter become
/// Escape-Enter, the new-line key in agents' prompts.
fn classic_key(modifier: u32, code: u32) -> Vec<u8> {
    let bits = modifier.saturating_sub(1);
    let (shift, alt, ctrl) = (bits & 1 != 0, bits & 2 != 0, bits & 4 != 0);
    match code {
        13 if shift || alt => return b"\x1b\r".to_vec(),
        13 => return b"\r".to_vec(),
        9 if shift => return b"\x1b[Z".to_vec(),
        _ => {}
    }
    let mut out = Vec::new();
    if alt {
        out.push(0x1b);
    }
    match (code, char::from_u32(code)) {
        (127, _) => out.push(if ctrl { 0x08 } else { 0x7f }),
        (_, Some(c)) if ctrl => match c.to_ascii_uppercase() {
            upper @ '@'..='_' => out.push(upper as u8 & 0x1f),
            ' ' | '2' => out.push(0),
            '?' => out.push(0x7f),
            _ => push_char(&mut out, c),
        },
        (_, Some(c)) if shift && c.is_ascii_lowercase() => out.push(c.to_ascii_uppercase() as u8),
        (_, Some(c)) => push_char(&mut out, c),
        (_, None) => {}
    }
    out
}

fn push_char(out: &mut Vec<u8>, c: char) {
    let mut buffer = [0; 4];
    out.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
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
    fn modified_keys_become_classic_keys() {
        let mut parser = Parser::default();
        let keys = |pieces: Vec<Piece>| match &pieces[..] {
            [Piece::Keys(keys)] => keys.clone(),
            other => panic!("expected keys, got {other:?}"),
        };
        // Encodings captured from herdr with modifyOtherKeys level 2.
        assert_eq!(keys(parser.feed(b"\x1b[27;2;13~")), b"\x1b\r");
        assert_eq!(keys(parser.feed(b"\x1b[27;3;13~")), b"\x1b\r");
        assert_eq!(keys(parser.feed(b"\x1b[27;5;13~")), b"\r");
        assert_eq!(keys(parser.feed(b"\x1b[27;2;9~")), b"\x1b[Z");
        assert_eq!(keys(parser.feed(b"\x1b[27;3;120~")), b"\x1bx");
        assert_eq!(keys(parser.feed(b"\x1b[27;5;97~")), [0x01]);
        assert_eq!(keys(parser.feed(b"\x1b[27;5;100~")), [0x04]);
        assert_eq!(keys(parser.feed(b"\x1b[27;7;99~")), [0x1b, 0x03]);
        assert_eq!(keys(parser.feed(b"\x1b[27;5;127~")), [0x08]);
        assert_eq!(keys(parser.feed(b"\x1b[27;2;97~")), b"A");
        assert!(!parser.holding());
    }

    #[test]
    fn a_split_modified_key_is_held() {
        let mut parser = Parser::default();
        assert_eq!(parser.feed(b"\x1b[27;2"), []);
        assert_eq!(parser.feed(b";13~"), [Piece::Keys(b"\x1b\r".to_vec())]);
    }

    #[test]
    fn other_escape_sequences_are_keys() {
        let mut parser = Parser::default();
        // Arrow up and a bracketed paste start pass straight through.
        assert_eq!(
            parser.feed(b"\x1b[A\x1b[200~\x1b[2~"),
            [Piece::Keys(b"\x1b[A\x1b[200~\x1b[2~".to_vec())]
        );
        assert!(!parser.holding());
    }
}
