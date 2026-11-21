//! スクロールモード（過去の出力をさかのぼって見る・キーボードで選んでコピーする）のキー操作．

#[derive(Debug, PartialEq, Eq)]
pub enum ScrollCmd {
    /// 上へ（過去へ）n 行．
    Up(usize),
    /// 下へ（最新へ）n 行．
    Down(usize),
    /// 上へ半画面．
    HalfUp,
    HalfDown,
    PageUp,
    PageDown,
    Top,
    Bottom,
    /// カーソルを左右に 1 文字．
    Left,
    Right,
    /// 行の先頭・末尾へ．
    LineStart,
    LineEnd,
    /// 次・前の単語へ．
    WordNext,
    WordPrev,
    /// ここから選び始める（もう一度で選択をやめる）．
    BeginSelect,
    /// 行単位で選び始める．
    LineSelect,
    /// 選んだところをコピーしてスクロールモードを抜ける．
    Copy,
    Exit,
}


/// キー入力（生バイト列）をスクロール操作に変換する．
pub fn parse(bytes: &[u8]) -> Vec<ScrollCmd> {
    use ScrollCmd::*;
    // 単独の ESC は終了
    if bytes == b"\x1b" {
        return vec![Exit];
    }
    let mut cmds = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let rest = &bytes[i..];
        let (step, cmd) = match rest {
            [0x1b, b'[', b'5', b'~', ..] => (4, Some(PageUp)),
            [0x1b, b'[', b'6', b'~', ..] => (4, Some(PageDown)),
            [0x1b, b'[' | b'O', b'A', ..] => (3, Some(Up(1))),
            [0x1b, b'[' | b'O', b'B', ..] => (3, Some(Down(1))),
            [0x1b, b'[' | b'O', b'C', ..] => (3, Some(Right)),
            [0x1b, b'[' | b'O', b'D', ..] => (3, Some(Left)),
            [0x1b, b'[' | b'O', b'H', ..] => (3, Some(Top)),
            [0x1b, b'[' | b'O', b'F', ..] => (3, Some(Bottom)),
            [0x1b, b'[' | b'O', _, ..] => (3, None),
            [b, ..] => (
                1,
                match b {
                    b'k' | 0x10 => Some(Up(1)),   // k / C-p
                    b'j' | 0x0e => Some(Down(1)), // j / C-n
                    b'h' => Some(Left),
                    b'l' => Some(Right),
                    b'u' | 0x15 => Some(HalfUp),  // u / C-u
                    b'd' | 0x04 => Some(HalfDown),
                    0x02 => Some(PageUp),         // C-b（b は前の単語）
                    b'f' | 0x06 => Some(PageDown),
                    b'g' => Some(Top),
                    b'G' => Some(Bottom),
                    b'0' | b'^' => Some(LineStart),
                    b'$' => Some(LineEnd),
                    b'w' | b'e' => Some(WordNext),
                    b'b' => Some(WordPrev),
                    b'v' | b' ' => Some(BeginSelect),
                    b'V' => Some(LineSelect),
                    b'y' | b'\r' | b'\n' => Some(Copy),
                    b'q' | 0x1b | 0x11 | 0x03 => Some(Exit), // q / Esc / C-q / C-c
                    _ => None,
                },
            ),
            [] => break,
        };
        i += step;
        cmds.extend(cmd);
    }
    cmds
}

/// 現在位置 `offset`（0 = 最新）に操作を適用した新しい位置を返す．
/// 範囲は `0..=max` に収める．`page` は 1 画面の行数．
pub fn apply(offset: usize, cmd: &ScrollCmd, page: usize, max: usize) -> usize {
    let half = (page / 2).max(1);
    let page = page.max(1);
    let next = match cmd {
        ScrollCmd::Up(n) => offset.saturating_add(*n),
        ScrollCmd::Down(n) => offset.saturating_sub(*n),
        ScrollCmd::HalfUp => offset.saturating_add(half),
        ScrollCmd::HalfDown => offset.saturating_sub(half),
        ScrollCmd::PageUp => offset.saturating_add(page),
        ScrollCmd::PageDown => offset.saturating_sub(page),
        ScrollCmd::Top => max,
        ScrollCmd::Bottom | ScrollCmd::Exit => 0,
        _ => offset,
    };
    next.min(max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_keys() {
        use ScrollCmd::*;
        assert_eq!(parse(b"kkj"), vec![Up(1), Up(1), Down(1)]);
        assert_eq!(parse(b"\x1b[A\x1b[5~\x1b[6~"), vec![Up(1), PageUp, PageDown]);
        assert_eq!(parse(b"\x1b"), vec![Exit]);
        assert_eq!(parse(b"gGq"), vec![Top, Bottom, Exit]);
        assert_eq!(parse(b"v\x1b[Dlw$y"), vec![BeginSelect, Left, Right, WordNext, LineEnd, Copy]);
        assert_eq!(parse(b"b\x02 V\r"), vec![WordPrev, PageUp, BeginSelect, LineSelect, Copy]);
    }

    #[test]
    fn apply_clamps() {
        assert_eq!(apply(0, &ScrollCmd::Up(1), 20, 100), 1);
        assert_eq!(apply(0, &ScrollCmd::Down(1), 20, 100), 0);
        assert_eq!(apply(95, &ScrollCmd::PageUp, 20, 100), 100);
        assert_eq!(apply(30, &ScrollCmd::HalfDown, 20, 100), 20);
        assert_eq!(apply(30, &ScrollCmd::Top, 20, 100), 100);
    }
}
