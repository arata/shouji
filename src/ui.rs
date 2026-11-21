//! 画面の組み立て（tui_mock のデザイン）．
//!
//! ```text
//!    1  app ●     2  tools                   Tasks    + Window    Split │    Split ─    ? Help
//! ──────────────────────────────────────────────────────────────────────────────────────────
//! ┌ Layout ──────┐ ╭─ build  cargo build ─────── pid 42  ● Running  Stop ─╮
//! │ 1 app        │ │ ...                                                   │
//! │   ├ build  ● │ ╰───────────────────────────────────────────────────────╯
//! │ ● running    │
//!  What you type goes to "build". Click another pane to switch.
//!  C-q z Zoom  C-q q Close pane  C-q [ Scroll back  C-q t Hide layout  C-q ? Help  C-q d Leave
//! ```
//!
//! 描画とクリック判定は同じ部品の一覧（[`Part`]）から行う．

use crossterm::style::Color;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::deps::{self, DepStatus, Wait};
use crate::layout::{Dir, PaneId, Rect};
use crate::pane::Pane;
use crate::render::{Frame, Modes, Style};
use crate::{App, Click, put_clipped};

/// 配色（tui_mock と同じ色）．
pub mod theme {
    use crossterm::style::Color;

    const fn rgb(hex: u32) -> Color {
        Color::Rgb { r: (hex >> 16) as u8, g: (hex >> 8) as u8, b: hex as u8 }
    }

    /// 画面全体の背景．
    pub const BG: Color = rgb(0x191816);
    /// タブバー・下部・ペインの中の背景．
    pub const PANEL: Color = rgb(0x1d1c19);
    pub const SIDEBAR: Color = rgb(0x1b1a17);
    /// 区切り線．
    pub const LINE: Color = rgb(0x2e2b27);
    /// 選んでいないペインの枠．
    pub const BORDER: Color = rgb(0x3a3631);
    /// キーの札の背景．
    pub const CHIP: Color = rgb(0x2e2b27);
    /// クリックできるボタンの背景．
    pub const BUTTON: Color = rgb(0x45403a);
    /// サイドバーで表示中のウィンドウの背景．
    pub const SEL_BG: Color = rgb(0x26231f);
    /// サイドバーで選択中のペインの背景．
    pub const AMBER_BG: Color = rgb(0x3b3122);
    /// 止まったプログラムの案内の背景．
    pub const RED_BG: Color = rgb(0x2e211d);
    /// ダイアログの背景．
    pub const DIALOG: Color = rgb(0x22201d);

    pub const TEXT: Color = rgb(0xe8e2d6);
    pub const MUTED: Color = rgb(0xb5ad9f);
    pub const DIM: Color = rgb(0x8a8378);
    pub const FAINT: Color = rgb(0x6f695f);
    pub const AMBER: Color = rgb(0xe8b45a);
    pub const GREEN: Color = rgb(0x8fc77a);
    pub const RED: Color = rgb(0xe67e6b);
    pub const BLUE: Color = rgb(0x7fb4d9);
}

use theme::*;

/// サイドバーの幅（区切り線を含む）．
const SIDEBAR_W: u16 = 26;

fn style(fg: Color, bg: Color) -> Style {
    Style { fg, bg, ..Style::default() }
}

fn bold(fg: Color, bg: Color) -> Style {
    Style { fg, bg, bold: true, ..Style::default() }
}

/// 画面に描く部品 1 つ．`click` があればクリックできる．
pub struct Part {
    pub x: u16,
    pub y: u16,
    pub text: String,
    pub style: Style,
    pub click: Option<Click>,
    /// この列の手前で切る．
    pub end: u16,
}

impl Part {
    fn new(x: u16, y: u16, text: impl Into<String>, style: Style, end: u16) -> Self {
        Self { x, y, text: text.into(), style, click: None, end }
    }

    fn click(mut self, c: Click) -> Self {
        self.click = Some(c);
        self
    }

    fn width(&self) -> u16 {
        (self.text.width() as u16).min(self.end.saturating_sub(self.x))
    }

    fn contains(&self, x: u16, y: u16) -> bool {
        y == self.y && x >= self.x && x < self.x + self.width()
    }
}

/// 部品を左から並べる道具．
struct Row {
    parts: Vec<Part>,
    x: u16,
    y: u16,
    end: u16,
}

impl Row {
    fn new(x: u16, y: u16, end: u16) -> Self {
        Self { parts: Vec::new(), x, y, end }
    }

    fn text(&mut self, text: impl Into<String>, style: Style) -> &mut Self {
        self.push(text.into(), style, None)
    }

    fn push(&mut self, text: String, style: Style, click: Option<Click>) -> &mut Self {
        let w = text.width() as u16;
        self.parts.push(Part { x: self.x, y: self.y, text, style, click, end: self.end });
        self.x = self.x.saturating_add(w);
        self
    }

    /// キーの札と説明（例: [r] Run again）．`hot` なら目立たせる．
    /// ボタン（明るい背景のかたまり）．中に説明と，薄くキーボードでの操作を書く．`bg` はボタンの外の背景．
    fn button(&mut self, label: &str, key: Option<&str>, hot: bool, click: Option<Click>, bg: Color) -> &mut Self {
        let (face, dim) = if hot { (bold(BG, RED), style(RED_BG, RED)) } else { (bold(TEXT, BUTTON), style(DIM, BUTTON)) };
        self.push(format!("  {label} "), face, click);
        if let Some(key) = key {
            self.push(format!("{key} "), dim, click);
        }
        self.push(" ".into(), face, click);
        self.push("  ".into(), style(TEXT, bg), None)
    }

    /// キーの札と説明のボタン（下部やペインの中）．
    fn hint(&mut self, key: &str, label: &str, hot: bool, click: Option<Click>, bg: Color) -> &mut Self {
        self.button(label, Some(key), hot, click, bg)
    }
}

/// 下部に並べるキーのヒント．
struct Hint {
    key: &'static str,
    label: String,
    keys: Option<&'static [u8]>,
    hot: bool,
}

fn hint(key: &'static str, label: impl Into<String>, keys: Option<&'static [u8]>) -> Hint {
    Hint { key, label: label.into(), keys, hot: false }
}

/// "code 1" などの終わり方を文にする．
fn exit_text(what: &str) -> String {
    match what.strip_prefix("signal: ") {
        Some(sig) => format!("stopped by a signal ({sig})"),
        None => what.replace("code", "exit code"),
    }
}

impl App {
    /// タブバーの行数（下に区切り線を引く．背が低い端末では 1 行だけ）．
    fn tab_rows(&self) -> u16 {
        if self.size.1 >= 16 { 2 } else { 1 }
    }

    fn footer_rows(&self) -> u16 {
        if self.size.1 >= 12 { 2 } else { 1 }
    }

    /// サイドバーを出すか（狭い端末では自動で隠す）．
    pub fn sidebar_width(&self) -> u16 {
        if self.sidebar && self.size.0 >= 90 { SIDEBAR_W } else { 0 }
    }

    /// ペインを置ける領域（上 1 行はタブバー，下はヒント，左はサイドバー）．
    pub fn pane_area(&self) -> Rect {
        let (w, h) = self.size;
        let sw = self.sidebar_width();
        // 左右に 1 列ずつすき間を空ける
        let pad = if w > sw + 20 { 1 } else { 0 };
        let top = self.tab_rows();
        Rect { x: sw + pad, y: top, w: w.saturating_sub(sw + 2 * pad), h: h.saturating_sub(top + self.footer_rows()) }
    }

    /// ペインの状態の名前と色．
    pub fn pane_status(&self, id: PaneId, pane: &Pane) -> (&'static str, Color) {
        if self.stopping.contains_key(&id) {
            ("Stopping", AMBER)
        } else if self.waits.contains_key(&id) {
            ("Waiting", AMBER)
        } else if self.restarts.contains_key(&id) {
            ("Restarting", AMBER)
        } else if pane.is_dead() {
            match pane.last_exit {
                Some((true, _)) => ("Finished", FAINT),
                _ => ("Stopped", RED),
            }
        } else if pane.running_shell() || pane.program().is_none() {
            ("Shell", BLUE)
        } else {
            ("Running", GREEN)
        }
    }

    /// 選択中のペインの名前．
    fn active_name(&self) -> String {
        let id = self.win().active;
        self.panes.get(&id).and_then(Pane::program).map_or("shell".into(), |p| p.name.clone())
    }

    /// タイトルの右側: PID と，状態（● Running など）とその色．
    fn title_right(&self, id: PaneId, pane: &Pane) -> (String, String, Color) {
        let (label, color) = self.pane_status(id, pane);
        let pid = match pane.pids() {
            Some((pid, Some(fg))) if fg != pid => format!(" pid {pid} fg {fg} "),
            Some((pid, _)) => format!(" pid {pid} "),
            None => String::new(),
        };
        (pid, format!(" ● {label} "), color)
    }

    /// 動いているプログラムに stop があれば，タイトルの右端（pid・状態の右）に出す Stop ボタン．`b` は枠．
    /// タイトルの中で一番優先して出す（狭いときは pid・状態・名前の方を削る）．
    fn stop_button(&self, id: PaneId, b: Rect) -> Option<Part> {
        let pane = self.panes.get(&id)?;
        pane.program()?.stop.as_ref()?;
        if pane.is_dead() || self.waits.contains_key(&id) || b.h < 3 {
            return None;
        }
        let forcing = self.stopping.contains_key(&id);
        let text = if forcing { " Force stop " } else { " Stop " };
        let bw = text.width() as u16;
        // 左の角と名前の頭（数文字）が残る幅があれば出す
        if b.w < bw + 8 {
            return None;
        }
        let x1 = b.x + b.w - 1;
        let face = if forcing { bold(BG, RED) } else { bold(RED, RED_BG) };
        Some(Part::new(x1 - 1 - bw, b.y, text, face, x1).click(Click::Stop(id)))
    }

    /// クリックした位置の部品．
    pub fn click_at(&self, x: u16, y: u16) -> Option<Click> {
        self.chrome().into_iter().find(|p| p.click.is_some() && p.contains(x, y)).and_then(|p| p.click)
    }

    /// タブバー・サイドバー・下部・ペインの中の案内の部品．
    fn chrome(&self) -> Vec<Part> {
        let mut parts = self.tab_bar();
        parts.extend(self.sidebar_parts());
        parts.extend(self.footer());
        let win = self.win();
        for (id, r) in &win.rects {
            parts.extend(self.overlay(*id, *r));
        }
        for (id, b) in &win.boxes {
            parts.extend(self.stop_button(*id, *b));
        }
        parts
    }

    fn tab_bar(&self) -> Vec<Part> {
        let (w, _) = self.size;
        let mut row = Row::new(1, 0, w);
        for (i, win) in self.windows.iter().enumerate() {
            let cur = i == self.current;
            let bg = if cur { AMBER } else { PANEL };
            let fg = if cur { BG } else { MUTED };
            let failed = win
                .root
                .as_ref()
                .is_some_and(|r| r.ids().iter().any(|id| self.panes.get(id).is_some_and(|p| p.is_dead() && p.last_exit.as_ref().is_some_and(|e| !e.0))));
            // タブの中は左右 2 文字ずつ空け，タブどうしは 2 文字離す
            row.push(format!("  {} ", i + 1), style(fg, bg), Some(Click::Window(i)));
            row.push(format!(" {} ", win.name), bold(fg, bg), Some(Click::Window(i)));
            if failed {
                row.push("● ".into(), style(if cur { BG } else { RED }, bg), Some(Click::Window(i)));
            }
            row.push(" ".into(), style(fg, bg), Some(Click::Window(i)));
            row.text("  ", style(TEXT, PANEL));
        }
        let left_end = row.x;
        let mut parts = row.parts;

        // 右寄せ: マウス用のボタンと Help．入りきらなければ Help 以外を後ろから省く
        let mut buttons: Vec<(&str, Click)> = Vec::new();
        if self.mouse {
            buttons = vec![
                ("Tasks", Click::Launcher),
                ("+ Window", Click::NewWindow),
                ("Split │", Click::Split(Dir::Horizontal)),
                ("Split ─", Click::Split(Dir::Vertical)),
            ];
        }
        buttons.push(("? Help", Click::Help));
        // ボタン 1 つの幅: "  label " + " " + すき間 "  "
        let total = |b: &[(&str, Click)]| b.iter().map(|(t, _)| t.width() as u16 + 6).sum::<u16>();
        while buttons.len() > 1 && left_end + total(&buttons) > w {
            buttons.remove(buttons.len() - 2);
        }
        if left_end + total(&buttons) <= w + 1 {
            let mut row = Row::new((w + 2).saturating_sub(total(&buttons)), 0, w);
            for (t, c) in buttons {
                row.button(t, None, false, Some(c), PANEL);
            }
            parts.extend(row.parts);
        }
        parts
    }

    fn sidebar_parts(&self) -> Vec<Part> {
        let sw = self.sidebar_width();
        if sw == 0 {
            return Vec::new();
        }
        let area = self.pane_area();
        let (top, bottom) = (area.y, area.y + area.h);
        let end = sw - 1;
        let mut parts = Vec::new();
        let mut y = top;
        parts.push(Part::new(0, y, " Layout", style(FAINT, SIDEBAR), end));
        y += 2;

        const LEGEND: [(&str, Color); 5] =
            [("running", GREEN), ("waiting / restarting", AMBER), ("shell", BLUE), ("stopped (check it)", RED), ("finished", FAINT)];
        let legend_top = bottom.saturating_sub(LEGEND.len() as u16 + 1);
        let tree_end = if legend_top > y + 2 { legend_top } else { bottom };

        'tree: for (wi, win) in self.windows.iter().enumerate() {
            if y >= tree_end {
                break;
            }
            let cur = wi == self.current;
            let (fg, bg) = if cur { (AMBER, SEL_BG) } else { (TEXT, SIDEBAR) };
            let text = format!(" {}  {}", wi + 1, win.name);
            parts.push(Part::new(0, y, format!("{text:<width$}", width = end as usize), bold(fg, bg), end).click(Click::Window(wi)));
            y += 1;
            let ids = win.root.as_ref().map(|r| r.ids()).unwrap_or_default();
            for (k, id) in ids.iter().enumerate() {
                if y >= tree_end {
                    break 'tree;
                }
                let Some(pane) = self.panes.get(id) else { continue };
                let selected = cur && *id == win.active;
                let bg = if selected { AMBER_BG } else if cur { SEL_BG } else { SIDEBAR };
                let fg = if selected { TEXT } else { MUTED };
                let branch = if k + 1 == ids.len() { "└" } else { "├" };
                let name = pane.program().map_or("shell", |p| p.name.as_str());
                let label = format!("   {branch} {name}");
                let label = crate::ui::truncate(&label, end as usize - 3);
                parts.push(Part::new(0, y, format!("{label:<width$}", width = end as usize), style(fg, bg), end).click(Click::Pane(wi, *id)));
                let (_, color) = self.pane_status(*id, pane);
                parts.push(Part::new(end - 2, y, "●", style(color, bg), end).click(Click::Pane(wi, *id)));
                y += 1;
            }
        }
        if tree_end == legend_top {
            parts.push(Part::new(0, legend_top, "─".repeat(end as usize), style(LINE, SIDEBAR), end));
            for (i, (text, color)) in LEGEND.iter().enumerate() {
                let y = legend_top + 1 + i as u16;
                parts.push(Part::new(1, y, "●", style(*color, SIDEBAR), end));
                parts.push(Part::new(3, y, *text, style(DIM, SIDEBAR), end));
            }
        }
        parts
    }

    fn footer(&self) -> Vec<Part> {
        let (w, h) = self.size;
        let rows = self.footer_rows();
        let mut parts = Vec::new();
        let name = self.active_name();

        // ボタンの行は最下行に置かない: 端末は窓の下の余白（行に満たない部分や padding）での
        // クリックも最下行として送ってくるので，ボタンの下の余白まで押せてしまう．
        // そのため 2 行あるときは，ボタンを上の行，クリックできない説明を最下行に置く．
        let button_y = if rows == 2 { h - 2 } else { h - 1 };

        // いまの状態の説明（エラーやお知らせがあればそちら）
        if rows == 2 {
            let (text, color) = if let Some(msg) = &self.message {
                (msg.clone(), RED)
            } else if self.confirm_quit {
                let n = self.panes.values().filter(|p| !p.is_dead()).count();
                (format!("Leave shouji? {n} program(s) are running. Detach keeps them running; come back with `shouji attach`."), AMBER)
            } else if self.input.in_prefix() {
                ("Ctrl-q pressed: now press a command key (? shows the list).".into(), AMBER)
            } else if let Some(info) = &self.info {
                (info.clone(), BLUE)
            } else if self.scrolling {
                (format!("Reading past output of \"{name}\". New output keeps arriving below."), MUTED)
            } else {
                (format!("What you type goes to \"{name}\". Click another pane to switch."), MUTED)
            };
            parts.push(Part::new(1, h - 1, text, style(color, PANEL), w));
        }

        // よく使う操作のボタン（札はキーボードでの操作）
        let mut hints = if self.confirm_quit {
            vec![
                hint("d", "Detach (keep running)", Some(b"d")),
                Hint { hot: true, ..hint("y", "Quit and stop all programs", Some(b"y")) },
                hint("Esc", "Cancel", Some(b"\x1b")),
            ]
        } else if self.scrolling {
            vec![
                hint("k/j", "Line", None),
                hint("u/d", "Page", None),
                hint("g/G", "Oldest / newest", None),
                hint("q", "Stop scrolling", Some(b"q")),
            ]
        } else {
            let zoom = self.win().zoom;
            // 止まった・待機中のペインの操作（Run again など）は，そのペインの中にだけ出す
            let mut v = Vec::new();
            v.push(hint("C-q z", if zoom { "Unzoom" } else { "Zoom" }, Some(b"\x11z")));
            v.push(hint("C-q q", "Close pane", Some(b"\x11q")));
            v.push(hint("C-q [", "Scroll back", Some(b"\x11[")));
            v.push(hint("C-q t", if self.sidebar { "Hide layout" } else { "Show layout" }, Some(b"\x11t")));
            v.push(hint("C-q ?", "Help", Some(b"\x11?")));
            v.push(hint("C-q d", "Leave", Some(b"\x11d")));
            v
        };
        // 入りきらなければ，最後の 2 つ（Help と Quit など）は残して手前から省く
        // ボタン 1 つの幅: "  label " + "key " + " " + すき間 "  "
        let width = |hs: &[Hint]| -> usize { 1 + hs.iter().map(|h| h.key.width() + h.label.width() + 7).sum::<usize>() };
        while hints.len() > 2 && width(&hints) > w as usize {
            let i = hints.len() - 3;
            hints.remove(i);
        }
        let mut row = Row::new(1, button_y, w);
        for h in &hints {
            row.hint(h.key, &h.label, h.hot, h.keys.map(Click::Keys), PANEL);
        }
        parts.extend(row.parts);
        parts
    }

    /// 待機中・終了したペインの中に重ねる案内とボタン．`r` はペインの中身の領域．
    fn overlay(&self, id: PaneId, r: Rect) -> Vec<Part> {
        let Some(pane) = self.panes.get(&id) else { return Vec::new() };
        let end = r.x + r.w;
        let mut parts = Vec::new();
        if r.h == 0 || r.w < 4 {
            return parts;
        }
        if let Some(w) = self.waits.get(&id) {
            let lines = self.wait_lines(w);
            for (i, (text, s)) in lines.iter().enumerate().take(r.h as usize) {
                parts.push(Part::new(r.x, r.y + i as u16, text.clone(), *s, end));
            }
            let y = r.y + lines.len() as u16 + 1;
            if y < r.y + r.h {
                let mut row = Row::new(r.x + 1, y, end);
                row.hint("Enter", "Start now", false, Some(Click::PaneKey(id, b'\r')), PANEL);
                row.hint("q", "Close", false, Some(Click::PaneKey(id, b'q')), PANEL);
                parts.extend(row.parts);
            }
            return parts;
        }
        if !pane.is_dead() {
            return parts;
        }
        let restarting = self.restarts.contains_key(&id);
        let (ok, what) = pane.last_exit.clone().unwrap_or((false, "status unknown".into()));
        let has_stop = pane.program().is_some_and(|p| p.stop.is_some());
        let buttons = |row: &mut Row, bg: Color, long: bool| {
            let hot = !ok;
            let k = |c| Some(Click::PaneKey(id, c));
            row.hint("r", if long { "Run again" } else { "Run" }, hot, k(b'r'), bg);
            row.hint("s", "Shell", false, k(b's'), bg);
            if has_stop {
                // 止まった後の後始末（残ったコンテナを消すなど）
                row.hint("C-q s", "Stop", false, Some(Click::Stop(id)), bg);
            }
            row.hint("q", "Close", false, k(b'q'), bg);
        };
        let fill = |parts: &mut Vec<Part>, y: u16, bg: Color| parts.push(Part::new(r.x, y, " ".repeat(r.w as usize), style(TEXT, bg), end));

        if !ok && r.h >= 6 && r.w >= 40 {
            // 失敗: 3 行の案内
            let y = r.y + r.h - 3;
            for i in 0..3 {
                fill(&mut parts, y + i, RED_BG);
            }
            let tail = if restarting { ", restarting soon" } else { "" };
            parts.push(Part::new(r.x + 1, y, format!("■ This program stopped ({}){tail}", exit_text(&what)), bold(RED, RED_BG), end));
            parts.push(Part::new(r.x + 3, y + 1, "Other panes keep running. Scroll back (wheel or C-q [) to see why.", style(MUTED, RED_BG), end));
            let mut row = Row::new(r.x + 3, y + 2, end);
            buttons(&mut row, RED_BG, true);
            parts.extend(row.parts);
        } else if ok && r.w >= 60 {
            // 正常終了: 1 行
            let y = r.y + r.h - 1;
            fill(&mut parts, y, DIALOG);
            let mut row = Row::new(r.x + 1, y, end);
            row.text("✓ ", style(GREEN, DIALOG));
            row.text(format!("Finished ({})", exit_text(&what)), style(MUTED, DIALOG));
            row.text("    ", style(TEXT, DIALOG));
            buttons(&mut row, DIALOG, true);
            parts.extend(row.parts);
        } else {
            // 狭いペイン: ボタンだけ
            let y = r.y + r.h - 1;
            let bg = if ok { DIALOG } else { RED_BG };
            fill(&mut parts, y, bg);
            let mut row = Row::new(r.x + 1, y, end);
            buttons(&mut row, bg, r.w >= 40);
            parts.extend(row.parts);
        }
        parts
    }

    /// 待機中のペインに表示する内容．
    pub fn wait_lines(&self, w: &Wait) -> Vec<(String, Style)> {
        let head = bold(AMBER, PANEL);
        let ok = style(GREEN, PANEL);
        let wait = style(TEXT, PANEL);
        let ng = bold(RED, PANEL);
        let dim = style(DIM, PANEL);
        use crate::config::Cond;

        let mut lines = vec![(" Waiting to start".to_string(), head)];
        let mut blocked = false;
        for d in &w.deps {
            let st = deps::status(d, &self.runs, self.panes.contains_key(&d.pane));
            let n = &d.name;
            let (mark, style, text) = match (st, d.cond) {
                (DepStatus::Met, Cond::Done) => ("ready", ok, format!("{n} finished successfully")),
                (DepStatus::Met, Cond::Started) => ("ready", ok, format!("{n} has started")),
                (DepStatus::Waiting, Cond::Done) => ("waiting", wait, format!("until {n} finishes successfully")),
                (DepStatus::Waiting, Cond::Started) => ("waiting", wait, format!("until {n} starts")),
                (DepStatus::Blocked(why), _) => {
                    blocked = true;
                    ("blocked", ng, format!("{n} {why}"))
                }
            };
            lines.push((format!("   {mark:<8} {text}"), style));
        }
        if !w.delay.is_zero() {
            let state = if w.ready_since.is_some() { " (counting down)" } else { "" };
            lines.push((format!("   then     wait {}s{state}", w.delay.as_secs_f64()), wait));
        }
        if blocked {
            lines.push(("   It will not start by itself.".to_string(), dim));
        }
        lines
    }

    /// 現在の状態からフレームを組み立てる．
    pub fn compose(&self) -> (Frame, Option<(u16, u16)>, Modes) {
        let (w, h) = self.size;
        let mut frame = Frame::new(w, h);
        frame.dim_all(TEXT, BG);
        let mut cursor = None;
        let mut modes = Modes::default();
        let win = self.win();
        let mode_color = AMBER;

        // タブバー（と下の区切り線）と下部の背景
        for x in 0..w {
            frame.set(x, 0, " ", style(TEXT, PANEL));
            if self.tab_rows() == 2 {
                frame.set(x, 1, "─", style(LINE, BG));
            }
            for k in 0..self.footer_rows() {
                frame.set(x, h.saturating_sub(1 + k), " ", style(TEXT, PANEL));
            }
        }
        // サイドバーの背景と右の区切り線
        let sw = self.sidebar_width();
        if sw > 0 {
            let area = self.pane_area();
            for y in area.y..area.y + area.h {
                for x in 0..sw - 1 {
                    frame.set(x, y, " ", style(TEXT, SIDEBAR));
                }
                frame.set(sw - 1, y, "│", style(LINE, BG));
            }
        }

        // ペインの枠
        for (id, b) in &win.boxes {
            let Some(pane) = self.panes.get(id) else { continue };
            if b.w < 4 || b.h < 3 {
                continue;
            }
            let focused = *id == win.active;
            let border = style(if focused { mode_color } else { BORDER }, BG);
            let (x1, y1) = (b.x + b.w - 1, b.y + b.h - 1);
            for x in b.x..=x1 {
                frame.set(x, b.y, "─", border);
                frame.set(x, y1, "─", border);
            }
            for y in b.y..=y1 {
                frame.set(b.x, y, "│", border);
                frame.set(x1, y, "│", border);
            }
            frame.set(b.x, b.y, "╭", border);
            frame.set(x1, b.y, "╮", border);
            frame.set(b.x, y1, "╰", border);
            frame.set(x1, y1, "╯", border);

            // タイトル: 右側に PID と状態，左側に名前とコマンド
            let (right, status, color) = self.title_right(*id, pane);
            let rw = (right.width() + status.width()) as u16;
            // 右端の Stop ボタン（chrome で描く）とすき間 1 文字の分
            let bw = self.stop_button(*id, *b).map_or(0, |p| p.width() + 1);
            let mut left_end = x1.saturating_sub(1 + bw);
            // Stop の左の残り．名前（12 文字まで）の分を残してから，pid → 状態の文字 → ● の順に削る
            let program = pane.program();
            let name = program.map_or("shell", |p| p.name.as_str());
            let nw = (name.width() as u16 + 2).min(12);
            let room = left_end.saturating_sub(b.x + 2);
            let sw = status.width() as u16;
            if room >= rw + nw {
                let rx = left_end - rw;
                let used = put_clipped(&mut frame, rx, b.y, &right, style(FAINT, BG), x1);
                put_clipped(&mut frame, rx + used, b.y, &status, style(color, BG), x1);
                left_end = rx;
            } else if room >= sw + nw {
                let rx = left_end - sw;
                put_clipped(&mut frame, rx, b.y, &status, style(color, BG), x1);
                left_end = rx;
            } else if room >= 3 + nw {
                // 狭いときは状態の色の ● だけ
                let rx = left_end - 3;
                put_clipped(&mut frame, rx, b.y, " ● ", style(color, BG), x1);
                left_end = rx;
            }
            // 選んでいるペインは枠と名前の色で示す
            let mut x = b.x + 2;
            let name_style = bold(if focused { mode_color } else { TEXT }, BG);
            x += put_clipped(&mut frame, x, b.y, &format!(" {name} "), name_style, left_end);
            if let Some(p) = program {
                let command = p.command.split_whitespace().collect::<Vec<_>>().join(" ");
                put_clipped(&mut frame, x, b.y, &format!("{} ", truncate(&command, left_end.saturating_sub(x + 1) as usize)), style(DIM, BG), left_end);
            }
        }

        // ペインの中身（既定の色はデザインの色にする）
        for (id, r) in &win.rects {
            let Some(pane) = self.panes.get(id) else { continue };
            let parser = pane.parser.lock().unwrap();
            let screen = parser.screen();
            let view = crate::select::View::of(&parser);
            let selection = self.selection.filter(|s| s.pane == *id);
            for row in 0..r.h {
                for col in 0..r.w {
                    let Some(cell) = screen.cell(row, col) else { continue };
                    let mut s = Style::from_cell(cell);
                    if s.fg == Color::Reset {
                        s.fg = TEXT;
                    }
                    if s.bg == Color::Reset {
                        s.bg = PANEL;
                    }
                    // 選んでいる文字は色を反転して示す
                    if selection.is_some_and(|sel| sel.contains(crate::select::Pos { row: view.abs(row), col })) {
                        s.inverse = !s.inverse;
                    }
                    let text = if cell.is_wide_continuation() {
                        ""
                    } else if !cell.has_contents() || (cell.is_wide() && col + 1 >= r.w) {
                        " "
                    } else {
                        cell.contents()
                    };
                    frame.set(r.x + col, r.y + row, text, s);
                }
            }
            if *id == win.active {
                // スクロール中と終了の確認中はカーソルを出さない
                let typing = !self.scrolling && !self.confirm_quit;
                if typing && !screen.hide_cursor() && !pane.is_dead() {
                    let (cr, cc) = screen.cursor_position();
                    cursor = Some((r.x + cc.min(r.w.saturating_sub(1)), r.y + cr.min(r.h.saturating_sub(1))));
                }
                // コピーモードのカーソル
                if self.scrolling
                    && let Some(c) = self.copy_cursor
                    && let Some(cr) = view.visible(c.row)
                    && cr < r.h
                {
                    cursor = Some((r.x + c.col.min(r.w.saturating_sub(1)), r.y + cr));
                }
                modes = Modes {
                    app_cursor: screen.application_cursor(),
                    app_keypad: screen.application_keypad(),
                    // 貼り付けを常に印付きで受け取り，キーとして解釈しないようにする．
                    // ペインへは，そのプログラムが使っていれば印を付けて送る（paste.rs）
                    bracketed_paste: true,
                    mouse: false,
                };
            }
        }
        modes.mouse = self.mouse;

        for p in self.chrome() {
            put_clipped(&mut frame, p.x, p.y, &p.text, p.style, p.end);
        }

        if let Some(l) = &self.launcher {
            frame.dim_all(FAINT, BG);
            l.draw(&mut frame, &self.config.tasks);
            cursor = None;
        }
        if let Some(help) = &self.help {
            frame.dim_all(FAINT, BG);
            help.draw(&mut frame);
            cursor = None;
        }
        (frame, cursor, modes)
    }
}

/// 幅 `width` に入るように切り詰める（切ったら末尾を … にする）．
pub fn truncate(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    let mut out = String::new();
    for ch in s.chars() {
        if out.width() + ch.width().unwrap_or(0) + 1 > width {
            break;
        }
        out.push(ch);
    }
    out.push('…');
    out
}
