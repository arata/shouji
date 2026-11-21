//! ペインの中の文字の選択（マウスのドラッグとコピーモード）．
//!
//! 位置は「絶対行」で持つ．絶対行は過去の出力（スクロールバック）の先頭からの行番号で，
//! スクロールバックがいっぱいになって捨てられた行の数も足しておく．
//! こうすると，選択中に出力が流れたりスクロールしたりしても位置がずれない．

use crate::layout::PaneId;
use crate::pane::Parser;
use crate::scroll::{self, ScrollCmd};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    /// 絶対行．
    pub row: u64,
    pub col: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Char,
    /// 単語ごと（ダブルクリック）．
    Word,
    /// 行ごと（トリプルクリック，コピーモードの V）．
    Line,
}

/// 選択範囲．始点と終点はそれぞれ「範囲」を持つ（単語・行で選ぶときに端を広げるため）．
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub pane: PaneId,
    pub mode: Mode,
    anchor: (Pos, Pos),
    head: (Pos, Pos),
}

impl Selection {
    pub fn new(pane: PaneId, mode: Mode, span: (Pos, Pos)) -> Self {
        Self { pane, mode, anchor: span, head: span }
    }

    pub fn set_head(&mut self, span: (Pos, Pos)) {
        self.head = span;
    }

    /// 選択範囲の最初と最後（どちらも含む）．
    pub fn range(&self) -> (Pos, Pos) {
        (self.anchor.0.min(self.head.0), self.anchor.1.max(self.head.1))
    }

    pub fn contains(&self, p: Pos) -> bool {
        let (a, b) = self.range();
        a <= p && p <= b
    }

    /// クリックしただけ（何も選んでいない）か．
    pub fn is_click(&self) -> bool {
        self.mode == Mode::Char && self.anchor == self.head
    }
}

/// 行全体の範囲．
pub fn line_span(row: u64) -> (Pos, Pos) {
    (Pos { row, col: 0 }, Pos { row, col: u16::MAX })
}

/// 1 文字の範囲．
pub fn char_span(p: Pos) -> (Pos, Pos) {
    (p, p)
}

/// 文字の種類（単語の切れ目を決める）．
#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Space,
    Word,
    /// 括弧などの記号．1 文字ずつ選ぶ．
    Other,
}

fn class(s: &str) -> Class {
    match s.chars().next() {
        None => Class::Space,
        Some(c) if c.is_whitespace() => Class::Space,
        // パスや URL をひとまとまりで選べるように，よく使う記号も単語に含める
        Some(c) if c.is_alphanumeric() || "-_./~:@%+=#?&*!$".contains(c) => Class::Word,
        Some(_) => Class::Other,
    }
}

/// 1 行の中で `col` を含む単語の桁の範囲．`cells` は各桁の文字（全角の右半分は空文字列）．
pub fn word_cols(cells: &[String], col: u16) -> (u16, u16) {
    let n = cells.len();
    if n == 0 {
        return (col, col);
    }
    // 全角の右半分は左半分と同じ種類にする
    let mut cls = Vec::with_capacity(n);
    for (j, c) in cells.iter().enumerate() {
        let k = if c.is_empty() && j > 0 { cls[j - 1] } else { class(c) };
        cls.push(k);
    }
    let col = (col as usize).min(n - 1);
    let k = cls[col];
    let (mut a, mut b) = (col, col);
    if k == Class::Other {
        // 記号は 1 文字（全角なら 2 桁）だけ
        if cells[a].is_empty() && a > 0 {
            a -= 1;
        }
        if b + 1 < n && cells[b + 1].is_empty() {
            b += 1;
        }
    } else {
        while a > 0 && cls[a - 1] == k {
            a -= 1;
        }
        while b + 1 < n && cls[b + 1] == k {
            b += 1;
        }
    }
    (a as u16, b as u16)
}

/// ペインの画面と過去の出力の位置関係．
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct View {
    /// 捨てられた行の数（= 残っている過去の出力の先頭の絶対行）．
    pub base: u64,
    /// 残っている過去の出力の行数．
    pub len: usize,
    /// いまさかのぼっている行数（0 = 最新）．
    pub offset: usize,
    pub rows: u16,
    pub cols: u16,
}

impl View {
    pub fn of(parser: &Parser) -> Self {
        let screen = parser.screen();
        let (rows, cols) = screen.size();
        Self {
            base: parser.callbacks().dropped,
            len: screen.scrollback_len(),
            offset: screen.scrollback(),
            rows: rows.get(),
            cols: cols.get(),
        }
    }

    /// 表示中の一番上の行の絶対行．
    pub fn top(&self) -> u64 {
        self.base + (self.len - self.offset.min(self.len)) as u64
    }

    /// 選べる最初と最後の絶対行．
    pub fn first(&self) -> u64 {
        self.base
    }

    pub fn last(&self) -> u64 {
        self.base + self.len as u64 + self.rows.max(1) as u64 - 1
    }

    /// 表示中の `row` 行目の絶対行．
    pub fn abs(&self, row: u16) -> u64 {
        self.top() + row as u64
    }

    /// 絶対行が表示中なら，その画面上の行．
    pub fn visible(&self, row: u64) -> Option<u16> {
        let top = self.top();
        (row >= top && row < top + self.rows as u64).then(|| (row - top) as u16)
    }

    /// 絶対行 `row` を表示するための最小の移動をしたスクロール位置．
    pub fn offset_showing(&self, row: u64) -> usize {
        let row = row.clamp(self.first(), self.last());
        let top = self.top();
        if row < top {
            self.offset + (top - row) as usize
        } else if row >= top + self.rows as u64 {
            self.offset.saturating_sub((row - top - self.rows as u64 + 1) as usize)
        } else {
            self.offset
        }
    }
}

/// 表示中の `row` 行目の各桁の文字（全角の右半分は空文字列）．
pub fn row_cells(parser: &Parser, row: u16) -> Vec<String> {
    let screen = parser.screen();
    let cols = screen.size().1.get();
    (0..cols)
        .map(|c| match screen.cell(row, c) {
            Some(cell) if cell.is_wide_continuation() => String::new(),
            Some(cell) if cell.has_contents() => cell.contents().to_string(),
            _ => " ".to_string(),
        })
        .collect()
}

/// 表示中の (`row`, `col`) を `mode` で選んだときの範囲．
pub fn span_at(parser: &Parser, mode: Mode, row: u16, col: u16) -> (Pos, Pos) {
    let view = View::of(parser);
    let abs = view.abs(row);
    match mode {
        Mode::Char => char_span(Pos { row: abs, col }),
        Mode::Line => line_span(abs),
        Mode::Word => {
            let (a, b) = word_cols(&row_cells(parser, row), col);
            (Pos { row: abs, col: a }, Pos { row: abs, col: b })
        }
    }
}

/// コピーモードのカーソル `cur` に `cmd` を適用し，新しいカーソルとスクロール位置を返す．
/// `cells` はカーソルのある行の各桁の文字，`page` は 1 画面の行数．
pub fn step(cur: Pos, cmd: &ScrollCmd, view: &View, page: usize, cells: &[String]) -> (Pos, usize) {
    let page = page.max(1);
    let last_col = view.cols.saturating_sub(1);
    let up = |n: usize| cur.row.saturating_sub(n as u64).max(view.first());
    let down = |n: usize| (cur.row + n as u64).min(view.last());
    let blank = |c: &String| c.trim().is_empty();
    let mut offset = view.offset;
    let next = match cmd {
        ScrollCmd::Up(n) => Pos { row: up(*n), ..cur },
        ScrollCmd::Down(n) => Pos { row: down(*n), ..cur },
        // 画面ごと動かし，カーソルも同じだけ動かす
        ScrollCmd::HalfUp | ScrollCmd::HalfDown | ScrollCmd::PageUp | ScrollCmd::PageDown => {
            offset = scroll::apply(view.offset, cmd, page, view.len);
            let n = match cmd {
                ScrollCmd::HalfUp | ScrollCmd::HalfDown => (page / 2).max(1),
                _ => page,
            };
            let row = if matches!(cmd, ScrollCmd::HalfUp | ScrollCmd::PageUp) { up(n) } else { down(n) };
            Pos { row, ..cur }
        }
        ScrollCmd::Top => Pos { row: view.first(), col: 0 },
        ScrollCmd::Bottom => Pos { row: view.last(), col: 0 },
        ScrollCmd::Left => Pos { col: cur.col.saturating_sub(1), ..cur },
        ScrollCmd::Right => Pos { col: (cur.col + 1).min(last_col), ..cur },
        ScrollCmd::LineStart => Pos { col: 0, ..cur },
        ScrollCmd::LineEnd => {
            let end = cells.iter().rposition(|c| !blank(c)).unwrap_or(0);
            Pos { col: end as u16, ..cur }
        }
        ScrollCmd::WordNext => {
            let from = cur.col as usize;
            let (_, b) = word_cols(cells, cur.col);
            // 今の単語の後ろで，空白でない最初の桁．なければ次の行の先頭
            match (b as usize + 1..cells.len()).find(|&j| !blank(&cells[j]) && j > from) {
                Some(j) => Pos { col: j as u16, ..cur },
                None if cur.row < view.last() => Pos { row: cur.row + 1, col: 0 },
                None => cur,
            }
        }
        ScrollCmd::WordPrev => {
            // 左にある，空白でない単語の先頭．なければ前の行の末尾
            let mut j = cur.col.min(cells.len().saturating_sub(1) as u16);
            while j > 0 && blank(&cells[j as usize - 1]) {
                j -= 1;
            }
            if j == 0 {
                if cur.col == 0 && cur.row > view.first() {
                    Pos { row: cur.row - 1, col: last_col }
                } else {
                    Pos { col: 0, ..cur }
                }
            } else {
                let (a, _) = word_cols(cells, j - 1);
                Pos { col: a, ..cur }
            }
        }
        ScrollCmd::BeginSelect | ScrollCmd::LineSelect | ScrollCmd::Copy | ScrollCmd::Exit => cur,
    };
    let moved = View { offset, ..*view };
    (next, moved.offset_showing(next.row))
}

/// 選択範囲の文字列．範囲が画面の外にあれば一時的にスクロールして読む（位置は元に戻す）．
/// 行末の空白は除き，折り返された行はつなげる．
pub fn text(parser: &mut Parser, sel: &Selection) -> String {
    let (a, b) = sel.range();
    let saved = parser.screen().scrollback();
    let mut out = String::new();
    let first = a.row.max(View::of(parser).first());
    let mut row = first;
    while row <= b.row {
        let view = View::of(parser);
        if row > view.last() {
            break;
        }
        parser.screen_mut().set_scrollback(view.offset_showing(row));
        let view = View::of(parser);
        let Some(vr) = view.visible(row) else { break };
        let screen = parser.screen();
        let start = if row == a.row { a.col.min(view.cols) } else { 0 };
        let end = if row == b.row { (b.col as u32 + 1).min(view.cols as u32) as u16 } else { view.cols };
        let line = screen.rows(start, end.saturating_sub(start)).nth(vr as usize).unwrap_or_default();
        let wrapped = screen.row_wrapped(vr) && row != b.row;
        if wrapped {
            out.push_str(&line);
        } else {
            out.push_str(line.trim_end());
            if row != b.row {
                out.push('\n');
            }
        }
        row += 1;
    }
    parser.screen_mut().set_scrollback(saved);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::Replies;
    use std::num::NonZeroU16;

    fn parser(rows: u16, cols: u16, scrollback: usize) -> Parser {
        vt100::Parser::new_with_callbacks(NonZeroU16::new(rows).unwrap(), NonZeroU16::new(cols).unwrap(), scrollback, Replies::default())
    }

    fn p(row: u64, col: u16) -> Pos {
        Pos { row, col }
    }

    #[test]
    fn range_is_ordered() {
        let mut s = Selection::new(0, Mode::Char, char_span(p(5, 3)));
        s.set_head(char_span(p(2, 7)));
        assert_eq!(s.range(), (p(2, 7), p(5, 3)));
        assert!(s.contains(p(3, 0)) && s.contains(p(5, 3)) && !s.contains(p(5, 4)));
        assert!(!s.is_click());
        assert!(Selection::new(0, Mode::Char, char_span(p(1, 1))).is_click());
        assert!(!Selection::new(0, Mode::Line, line_span(1)).is_click());
    }

    #[test]
    fn words() {
        let cells: Vec<String> = "ls /tmp/a.txt  (x)".chars().map(|c| c.to_string()).collect();
        assert_eq!(word_cols(&cells, 0), (0, 1));
        assert_eq!(word_cols(&cells, 5), (3, 12));
        assert_eq!(word_cols(&cells, 13), (13, 14));
        assert_eq!(word_cols(&cells, 15), (15, 15)); // ( は 1 文字だけ
        // 全角文字（右半分は空文字列）
        let cells: Vec<String> = ["a", "あ", "", "い", "", " ", "b"].iter().map(|s| s.to_string()).collect();
        assert_eq!(word_cols(&cells, 2), (0, 4));
    }

    #[test]
    fn view_math() {
        let v = View { base: 100, len: 50, offset: 10, rows: 20, cols: 80 };
        assert_eq!(v.top(), 140);
        assert_eq!(v.abs(0), 140);
        assert_eq!(v.visible(159), Some(19));
        assert_eq!(v.visible(160), None);
        assert_eq!(v.offset_showing(130), 20);
        assert_eq!(v.offset_showing(165), 4);
        assert_eq!(v.offset_showing(150), 10);
        assert_eq!(v.offset_showing(0), 50); // 捨てられた行は選べない
        assert_eq!(v.last(), 169);
    }

    #[test]
    fn copy_mode_steps() {
        let v = View { base: 0, len: 100, offset: 0, rows: 10, cols: 20 };
        let cells: Vec<String> = "foo  bar.baz (q)".chars().map(|c| c.to_string()).chain(std::iter::repeat(" ".into()).take(4)).collect();
        let cur = p(105, 0);
        let go = |cur: Pos, cmd: ScrollCmd| step(cur, &cmd, &v, 10, &cells);
        assert_eq!(go(cur, ScrollCmd::WordNext), (p(105, 5), 0));
        assert_eq!(go(p(105, 5), ScrollCmd::WordNext), (p(105, 13), 0));
        assert_eq!(go(p(105, 15), ScrollCmd::WordNext), (p(106, 0), 0));
        assert_eq!(go(p(105, 13), ScrollCmd::WordPrev), (p(105, 5), 0));
        assert_eq!(go(p(105, 7), ScrollCmd::WordPrev), (p(105, 5), 0));
        assert_eq!(go(p(105, 0), ScrollCmd::WordPrev), (p(104, 19), 0));
        assert_eq!(go(cur, ScrollCmd::LineEnd), (p(105, 15), 0));
        assert_eq!(go(p(105, 19), ScrollCmd::Right), (p(105, 19), 0));
        // 画面の上に出たらスクロールする
        assert_eq!(go(p(100, 3), ScrollCmd::Up(2)), (p(98, 3), 2));
        assert_eq!(go(p(105, 3), ScrollCmd::PageUp), (p(95, 3), 10));
        assert_eq!(go(cur, ScrollCmd::Top), (p(0, 0), 100));
        assert_eq!(go(p(0, 0), ScrollCmd::Up(1)), (p(0, 0), 100));
        assert_eq!(go(cur, ScrollCmd::Bottom), (p(109, 0), 0));
    }

    #[test]
    fn positions_survive_scrolling() {
        let mut ps = parser(3, 10, 2);
        ps.process(b"a\r\nb\r\nc");
        let row_c = View::of(&ps).abs(2);
        // 3 行流れる: 1 行は過去の出力にも残らず捨てられる
        ps.process(b"\r\nd\r\ne\r\nf");
        let v = View::of(&ps);
        assert_eq!(v.base, 1);
        assert_eq!(v.visible(row_c), None);
        let s = Selection::new(0, Mode::Line, line_span(row_c));
        assert_eq!(text(&mut ps, &s), "c");
    }

    #[test]
    fn text_across_scrollback() {
        let mut ps = parser(2, 10, 100);
        ps.process(b"one  \r\ntwo\r\nthree\r\nfour");
        // 表示は three / four．one の途中から four の途中まで
        let v = View::of(&ps);
        let mut s = Selection::new(0, Mode::Char, char_span(p(v.base, 1)));
        s.set_head(char_span(p(v.abs(1), 1)));
        assert_eq!(text(&mut ps, &s), "ne\ntwo\nthree\nfo");
        assert_eq!(ps.screen().scrollback(), 0);
    }

    #[test]
    fn wrapped_lines_are_joined() {
        let mut ps = parser(3, 4, 0);
        ps.process(b"abcdef\r\ngh");
        let mut s = Selection::new(0, Mode::Line, line_span(0));
        s.set_head(line_span(2));
        assert_eq!(text(&mut ps, &s), "abcdef\ngh");
    }
}
