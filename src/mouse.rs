//! 端末から届くマウス操作（SGR 形式: `ESC [ < ボタン ; 桁 ; 行 M/m`）の解析．
//!
//! マウス報告を有効にすると，クリックやホイールがキー入力と同じ経路で届く．
//! ここでキー入力からマウスの部分だけを取り出す．

/// 外側の端末にマウス報告（クリック・ドラッグ・SGR 形式）を頼む．
pub const ENABLE: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
/// マウス報告をやめる．
pub const DISABLE: &str = "\x1b[?1006l\x1b[?1002l\x1b[?1000l";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Press(Button),
    Release,
    Drag,
    WheelUp,
    WheelDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    pub kind: Kind,
    /// 画面上の位置（0 始まり）．
    pub x: u16,
    pub y: u16,
}

/// キー入力のバイト列からマウス操作を取り出す．読み込みの切れ目で途切れた
/// マウスのシーケンスは，次の入力とつなげて解析する．
#[derive(Default)]
pub struct Extractor {
    pending: Vec<u8>,
}

impl Extractor {
    /// `(マウス以外のバイト列, マウス操作)` を返す．
    pub fn feed(&mut self, bytes: &[u8]) -> (Vec<u8>, Vec<MouseEvent>) {
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(bytes);

        let mut rest = Vec::with_capacity(buf.len());
        let mut events = Vec::new();
        let mut i = 0;
        while i < buf.len() {
            if buf[i..].starts_with(b"\x1b[<") {
                match parse_sgr(&buf[i + 3..]) {
                    Parsed::Event(ev, len) => {
                        events.extend(ev);
                        i += 3 + len;
                        continue;
                    }
                    Parsed::Incomplete => {
                        self.pending = buf[i..].to_vec();
                        break;
                    }
                    Parsed::Invalid => {}
                }
            } else if matches!(&buf[i..], b"\x1b" | b"\x1b[") && i > 0 {
                // マウスのシーケンスの途中で切れた可能性があるので持ち越す
                // （単独の ESC キーは i == 0 のときそのまま渡す）
                self.pending = buf[i..].to_vec();
                break;
            }
            rest.push(buf[i]);
            i += 1;
        }
        (rest, events)
    }
}

enum Parsed {
    /// 解析できた（`None` は無視してよい操作）と，消費したバイト数．
    Event(Option<MouseEvent>, usize),
    Incomplete,
    Invalid,
}

/// `ESC [ <` に続く `ボタン ; 桁 ; 行 M/m` を解析する．
fn parse_sgr(s: &[u8]) -> Parsed {
    let mut nums = [0u32; 3];
    let mut idx = 0;
    let mut has_digit = false;
    for (pos, &b) in s.iter().enumerate() {
        match b {
            b'0'..=b'9' => {
                nums[idx] = nums[idx].saturating_mul(10).saturating_add((b - b'0') as u32);
                has_digit = true;
            }
            b';' if idx < 2 && has_digit => {
                idx += 1;
                has_digit = false;
            }
            b'M' | b'm' if idx == 2 && has_digit => {
                return Parsed::Event(to_event(nums, b == b'M'), pos + 1);
            }
            _ => return Parsed::Invalid,
        }
    }
    Parsed::Incomplete
}

fn to_event([code, x, y]: [u32; 3], pressed: bool) -> Option<MouseEvent> {
    let x = (x.max(1) - 1).min(u16::MAX as u32) as u16;
    let y = (y.max(1) - 1).min(u16::MAX as u32) as u16;
    let motion = code & 32 != 0;
    // Shift を押しながらの操作は外側の端末の選択に任せる（届いても無視する）
    if code & 4 != 0 {
        return None;
    }
    let kind = if code & 64 != 0 {
        match code & 3 {
            0 => Kind::WheelUp,
            1 => Kind::WheelDown,
            _ => return None, // 横スクロールは扱わない
        }
    } else if !pressed {
        Kind::Release
    } else if motion {
        Kind::Drag
    } else {
        match code & 3 {
            0 => Kind::Press(Button::Left),
            1 => Kind::Press(Button::Middle),
            2 => Kind::Press(Button::Right),
            _ => return None,
        }
    };
    Some(MouseEvent { kind, x, y })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: Kind, x: u16, y: u16) -> MouseEvent {
        MouseEvent { kind, x, y }
    }

    #[test]
    fn extract_mixed() {
        let mut e = Extractor::default();
        let (rest, events) = e.feed(b"ab\x1b[<0;10;5Mcd\x1b[<0;10;5m\x1b[<64;3;4M\x1b[<65;3;4M");
        assert_eq!(rest, b"abcd");
        assert_eq!(
            events,
            vec![
                ev(Kind::Press(Button::Left), 9, 4),
                ev(Kind::Release, 9, 4),
                ev(Kind::WheelUp, 2, 3),
                ev(Kind::WheelDown, 2, 3),
            ]
        );
    }

    #[test]
    fn split_across_reads() {
        let mut e = Extractor::default();
        let (rest, events) = e.feed(b"x\x1b[<0;1");
        assert_eq!((rest.as_slice(), events.len()), (&b"x"[..], 0));
        let (rest, events) = e.feed(b"2;3M");
        assert!(rest.is_empty());
        assert_eq!(events, vec![ev(Kind::Press(Button::Left), 11, 2)]);
    }

    #[test]
    fn shift_is_ignored() {
        let mut e = Extractor::default();
        let (rest, events) = e.feed(b"\x1b[<4;1;1M\x1b[<36;2;1M");
        assert!(rest.is_empty() && events.is_empty());
    }

    #[test]
    fn keys_pass_through() {
        let mut e = Extractor::default();
        assert_eq!(e.feed(b"\x1b").0, b"\x1b");
        assert_eq!(e.feed(b"\x1b[A").0, b"\x1b[A");
        assert_eq!(e.feed(b"\x1b[<x").0, b"\x1b[<x");
    }
}
