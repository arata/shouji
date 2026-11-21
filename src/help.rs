//! ヘルプ画面（? か C-q ? で開く）．操作モード・入力モードの説明と，キー操作の一覧．

use unicode_width::UnicodeWidthStr;

use crate::launcher::put_clipped;
use crate::render::{Frame, Style};
use crate::ui::theme::*;

struct Group {
    title: &'static str,
    items: &'static [(&'static str, &'static str)],
}

const GROUPS: &[Group] = &[
    Group {
        title: "Panes",
        items: &[
            ("Click", "Select a pane"),
            ("C-q o, C-q Tab", "Next pane"),
            ("C-q arrows", "Pane on that side (hjkl too)"),
            ("C-q z", "Zoom in / back to normal"),
            ("C-q % / C-q \"", "Split left-right / top-bottom"),
            ("C-q q", "Close the pane"),
        ],
    },
    Group {
        title: "Windows",
        items: &[
            ("Click a tab", "Switch window"),
            ("C-q 1-9", "Window number N in this group"),
            ("C-q n / C-q p", "Next / previous window"),
            ("C-q c", "New window in this group"),
        ],
    },
    Group {
        title: "Groups (one per task)",
        items: &[
            ("Click a group", "Switch group (row above tabs)"),
            ("C-q g / C-q G", "Next / previous group"),
            ("Click ▾ / ▸", "Fold / unfold in the layout"),
        ],
    },
    Group {
        title: "Stopped or waiting programs",
        items: &[
            ("r", "Run a stopped program again"),
            ("s", "Open a shell in its directory"),
            ("C-q s", "Run its stop command"),
            ("q", "Close the pane"),
            ("Enter", "Start a waiting program now"),
        ],
    },
    Group {
        title: "Reading past output",
        items: &[
            ("Wheel, C-q [", "Start scrolling"),
            ("k / j", "One line up / down"),
            ("u / d", "Half page up / down"),
            ("g / G", "Oldest / newest output"),
            ("q, Esc", "Stop scrolling"),
        ],
    },
    Group {
        title: "Copy and paste",
        items: &[
            ("Drag", "Copy the text"),
            ("2 / 3 clicks", "Copy a word / a line"),
            ("Shift+drag", "Your terminal's own selection"),
            ("C-q [, v ... y", "Select with keys and copy"),
            ("h l w b 0 $", "Move while selecting"),
            ("V", "Select whole lines"),
            ("C-q ], Middle", "Paste what you copied"),
        ],
    },
    Group {
        title: "Programs",
        items: &[("C-q Space", "Task launcher: start programs"), ("Click Tasks", "The same, with the mouse")],
    },
    Group {
        title: "Other",
        items: &[
            ("C-q t", "Show / hide the layout panel"),
            ("C-q m", "Mouse on / off"),
            ("C-q ?", "This help"),
            ("C-q d", "Leave: detach or quit"),
            ("C-q C-q", "Send Ctrl-q to the program"),
        ],
    },
];

const INTRO: [&str; 2] = [
    "Everything you type goes to the selected pane. Most things can be done by clicking.",
    "Keyboard commands start with Ctrl-q (C-q): press Ctrl-q, let go, then press the key.",
];

const KEY_W: usize = 18;

/// 1 行に並べる部品（左端からの列, 文字, 色）．
type Line = Vec<(u16, String, Style)>;

/// ヘルプ画面．`offset` は表示している先頭の行．
#[derive(Default)]
pub struct Help {
    offset: usize,
}

struct Geometry {
    x0: u16,
    y0: u16,
    w: u16,
    h: u16,
}

impl Geometry {
    fn visible(&self) -> usize {
        self.h.saturating_sub(4) as usize
    }

    fn contains(&self, x: u16, y: u16) -> bool {
        (self.x0..self.x0 + self.w).contains(&x) && (self.y0..self.y0 + self.h).contains(&y)
    }
}

fn group_width(g: &Group) -> usize {
    g.items.iter().map(|(_, d)| KEY_W + d.width()).max().unwrap_or(0).max(g.title.width())
}

fn group_lines(g: &Group, x: u16) -> Vec<Line> {
    let mut lines = vec![vec![(x, g.title.to_string(), Style { fg: AMBER, bg: DIALOG, bold: true, ..Style::default() })]];
    for (k, d) in g.items {
        lines.push(vec![
            (x, format!(" {k} "), Style { fg: TEXT, bg: CHIP, ..Style::default() }),
            (x + KEY_W as u16, d.to_string(), Style { fg: MUTED, bg: DIALOG, ..Style::default() }),
        ]);
    }
    lines
}

/// 内側の幅 `inner` に合わせた全行（2 列に入るなら 2 列）．
fn lines(inner: u16) -> Vec<Line> {
    let mut out: Vec<Line> = Vec::new();
    for text in INTRO {
        out.push(vec![(0, text.to_string(), Style { fg: MUTED, bg: DIALOG, ..Style::default() })]);
    }
    out.push(Vec::new());

    let col_w = GROUPS.iter().map(group_width).max().unwrap_or(0) as u16 + 2;
    let two = inner >= col_w * 2;
    if two {
        for pair in GROUPS.chunks(2) {
            let left = group_lines(&pair[0], 0);
            let right = pair.get(1).map(|g| group_lines(g, col_w)).unwrap_or_default();
            for i in 0..left.len().max(right.len()) {
                let mut line = left.get(i).cloned().unwrap_or_default();
                line.extend(right.get(i).cloned().unwrap_or_default());
                out.push(line);
            }
            out.push(Vec::new());
        }
    } else {
        for g in GROUPS {
            out.extend(group_lines(g, 0));
            out.push(Vec::new());
        }
    }
    out.pop();
    out
}

/// 枠の位置と大きさ（ステータス行にはかからないようにする）．
fn geometry(fw: u16, fh: u16) -> Option<Geometry> {
    let area_h = fh.saturating_sub(1);
    let w = fw.saturating_sub(4).min(104);
    let inner = w.saturating_sub(6);
    let h = (lines(inner).len() as u16 + 4).min(area_h);
    if w < 20 || h < 6 {
        return None;
    }
    Some(Geometry { x0: (fw - w) / 2, y0: (area_h - h) / 2, w, h })
}

impl Help {
    /// キー入力を処理する．閉じるなら true．
    pub fn feed(&mut self, bytes: &[u8], fw: u16, fh: u16) -> bool {
        let page = geometry(fw, fh).map_or(1, |g| g.visible().max(1));
        if bytes == b"\x1b" {
            return true;
        }
        let mut i = 0;
        while i < bytes.len() {
            let rest = &bytes[i..];
            let (step, delta): (usize, isize) = match rest {
                [0x1b, b'[', b'5', b'~', ..] => (4, -(page as isize)),
                [0x1b, b'[', b'6', b'~', ..] => (4, page as isize),
                [0x1b, b'[' | b'O', b'A', ..] => (3, -1),
                [0x1b, b'[' | b'O', b'B', ..] => (3, 1),
                [0x1b, b'[' | b'O', _, ..] => (3, 0),
                [b'q' | b'?' | b'\r' | 0x1b, ..] => return true,
                [b'k', ..] => (1, -1),
                [b'j', ..] => (1, 1),
                [b'u', ..] => (1, -(page as isize)),
                [b'd' | b' ', ..] => (1, page as isize),
                [b'g', ..] => (1, isize::MIN / 2),
                [b'G', ..] => (1, isize::MAX / 2),
                _ => (1, 0),
            };
            self.scroll(delta, fw, fh);
            i += step;
        }
        false
    }

    pub fn scroll(&mut self, delta: isize, fw: u16, fh: u16) {
        let max = geometry(fw, fh).map_or(0, |g| lines(g.w.saturating_sub(6)).len().saturating_sub(g.visible()));
        self.offset = (self.offset as isize).saturating_add(delta).clamp(0, max as isize) as usize;
    }

    /// クリックした位置がヘルプの外なら true（閉じる）．
    pub fn click_outside(&self, x: u16, y: u16, fw: u16, fh: u16) -> bool {
        geometry(fw, fh).is_none_or(|g| !g.contains(x, y))
    }

    /// 画面中央に枠付きで描く．
    pub fn draw(&self, frame: &mut Frame) {
        let Some(g) = geometry(frame.w, frame.h) else { return };
        let Geometry { x0, y0, w, h } = g;
        let border = Style { fg: BORDER, bg: DIALOG, ..Style::default() };
        let blank = Style { fg: TEXT, bg: DIALOG, ..Style::default() };
        for y in y0..y0 + h {
            for x in x0..x0 + w {
                let ch = match (x == x0, x == x0 + w - 1, y == y0, y == y0 + h - 1) {
                    (true, _, true, _) => "╭",
                    (_, true, true, _) => "╮",
                    (true, _, _, true) => "╰",
                    (_, true, _, true) => "╯",
                    (_, _, true, _) | (_, _, _, true) => "─",
                    (true, _, _, _) | (_, true, _, _) => "│",
                    _ => " ",
                };
                frame.set(x, y, ch, if ch == " " { blank } else { border });
            }
        }
        let end = x0 + w - 3;
        put_clipped(frame, x0 + 3, y0 + 1, "Keyboard shortcuts", Style { fg: TEXT, bg: DIALOG, bold: true, ..Style::default() }, end);
        let all = lines(w.saturating_sub(6));
        let visible = g.visible();
        let close = if all.len() > visible {
            format!("j/k: scroll ({}-{} of {})   Esc or ?: close", self.offset + 1, (self.offset + visible).min(all.len()), all.len())
        } else {
            "Esc or ?: close".to_string()
        };
        let cx = end.saturating_sub(close.width() as u16);
        put_clipped(frame, cx, y0 + 1, &close, Style { fg: DIM, bg: DIALOG, ..Style::default() }, end);

        for (i, line) in all.iter().skip(self.offset).take(visible).enumerate() {
            let y = y0 + 3 + i as u16;
            for (dx, text, style) in line {
                put_clipped(frame, x0 + 3 + dx, y, text, *style, end);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_and_close() {
        let mut h = Help::default();
        // 背が低い端末ではスクロールできる
        assert!(!h.feed(b"jj", 80, 14));
        assert_eq!(h.offset, 2);
        assert!(!h.feed(b"g", 80, 14));
        assert_eq!(h.offset, 0);
        // 全部入るならスクロールしない
        assert!(!h.feed(b"j", 120, 100));
        assert_eq!(h.offset, 0);
        assert!(h.feed(b"q", 80, 14));
        assert!(h.feed(b"\x1b", 80, 14));
        assert!(h.click_outside(0, 0, 200, 14));
        assert!(!h.click_outside(100, 5, 200, 14));
        // 広ければ 2 列，狭ければ 1 列
        assert!(lines(100).len() < lines(40).len());
    }
}
