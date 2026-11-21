//! `C-q Space` のメニュー．
//!
//! 1. タスクの一覧: Enter で全部を起動，→ で選択画面へ．
//! 2. 選択画面: タスクのウィンドウとプログラムにチェックを付け，選んだものだけを起動する．

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::config::{Task, TaskEntry};
use crate::render::{Frame, Style};

/// メニューでの操作結果．
pub enum MenuResult {
    /// メニューを開いたままにする．
    Stay,
    Cancel,
    /// 選んだタスク（添字）を全部起動する．
    Launch(usize),
    /// タスク（添字）の選択画面を開く（タスクファイルの読み込みは App が行う）．
    OpenSelect(usize),
    /// 選択画面で選んだものだけに絞ったタスクを起動する．
    LaunchTask(Task),
    /// 選択画面で何も選ばれていないのに起動しようとした．
    NothingSelected,
}

/// キー入力を解釈したもの．
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    Up,
    Down,
    Left,
    Right,
    Enter,
    Space,
    Cancel,
    Char(char),
}

fn keys(bytes: &[u8]) -> Vec<Key> {
    // 単独の ESC はキャンセル，ESC [ A などは矢印キー
    if bytes == b"\x1b" {
        return vec![Key::Cancel];
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let (step, key) = match &bytes[i..] {
            [0x1b, b'[' | b'O', k, ..] => (
                3,
                match k {
                    b'A' => Some(Key::Up),
                    b'B' => Some(Key::Down),
                    b'C' => Some(Key::Right),
                    b'D' => Some(Key::Left),
                    _ => None,
                },
            ),
            [b, ..] => (
                1,
                Some(match b {
                    // 文字キーはタスクのショートカット（1-9, a-z）に使うので，移動は矢印キーで行う
                    0x10 => Key::Up,   // C-p
                    0x0e => Key::Down, // C-n
                    0x7f | 0x08 => Key::Left,
                    b'\t' => Key::Right,
                    b'\r' | b'\n' => Key::Enter,
                    b' ' => Key::Space,
                    0x1b | 0x11 => Key::Cancel, // Esc / C-q
                    b => Key::Char(*b as char),
                }),
            ),
            [] => break,
        };
        i += step;
        out.extend(key);
    }
    out
}

/// 選択画面の 1 行．
enum Item {
    /// ウィンドウ（中のプログラムの添字）．
    Window(Vec<usize>),
    /// プログラム（添字）．
    Program(usize),
}

/// 選択画面の状態．
pub struct Selection {
    task: Task,
    items: Vec<(Item, String)>,
    /// プログラムごとのチェック．
    checked: Vec<bool>,
    cursor: usize,
}

impl Selection {
    /// 最初はすべてチェックした状態で開く．
    pub fn new(task: Task) -> Self {
        let mut items = Vec::new();
        for w in &task.windows {
            items.push((Item::Window(w.programs.clone()), w.name.clone()));
            for &k in &w.programs {
                let p = &task.programs[k];
                items.push((Item::Program(k), format!("{:<12} {}", p.name, p.summary())));
            }
        }
        let checked = vec![true; task.programs.len()];
        Self { task, items, checked, cursor: 0 }
    }

    /// ウィンドウなら中のチェック数（全部・一部・なし），プログラムならチェック．
    fn mark(&self, item: &Item) -> &'static str {
        let ks: &[usize] = match item {
            Item::Window(ks) => ks,
            Item::Program(k) => std::slice::from_ref(k),
        };
        let n = ks.iter().filter(|&&k| self.checked[k]).count();
        match n {
            0 => "[ ]",
            _ if n == ks.len() => "[x]",
            _ => "[-]",
        }
    }

    /// カーソルの行のチェックを切り替える．ウィンドウならその中をまとめて．
    fn toggle(&mut self) {
        let Some((item, _)) = self.items.get(self.cursor) else { return };
        let ks: Vec<usize> = match item {
            Item::Window(ks) => ks.clone(),
            Item::Program(k) => vec![*k],
        };
        let on = !ks.iter().all(|&k| self.checked[k]);
        for k in ks {
            self.checked[k] = on;
        }
    }

    fn launch(&self) -> MenuResult {
        if !self.checked.iter().any(|&c| c) {
            return MenuResult::NothingSelected;
        }
        let mut task = self.task.clone();
        task.select_mask(&self.checked);
        MenuResult::LaunchTask(task)
    }

    fn lines(&self) -> Vec<String> {
        self.items
            .iter()
            .map(|(item, label)| match item {
                Item::Window(_) => format!(" {} {label}", self.mark(item)),
                Item::Program(_) => format!("     {} {label}", self.mark(item)),
            })
            .collect()
    }
}

/// ボタン（枠の下辺に並べ，クリックで押せる）．
#[derive(Clone, Copy)]
enum Button {
    LaunchSelected,
    Toggle,
    Back,
    /// 押しても何も起きない説明．
    Hint,
}

/// 画面に出す内容．
struct Content {
    title: String,
    lines: Vec<String>,
    /// 強調する行．
    cursor: Option<usize>,
    buttons: Vec<(&'static str, Button)>,
}

struct Geometry {
    x0: u16,
    y0: u16,
    w: u16,
    h: u16,
    /// 表示を始める行（行が多くて入りきらないとき）．
    offset: usize,
}

impl Geometry {
    fn visible(&self) -> usize {
        self.h.saturating_sub(2) as usize
    }
}

/// タスクの一覧で `i` 番目を起動するキー（1-9 の次は a-z）．
fn shortcut(i: usize) -> Option<char> {
    match i {
        0..9 => Some((b'1' + i as u8) as char),
        9..35 => Some((b'a' + (i - 9) as u8) as char),
        _ => None,
    }
}

/// キーが何番目のタスクのショートカットか．
fn shortcut_index(c: char) -> Option<usize> {
    match c {
        '1'..='9' => Some(c as usize - '1' as usize),
        'a'..='z' => Some(c as usize - 'a' as usize + 9),
        _ => None,
    }
}

/// タスクの一覧の行に付ける，選択画面を開くボタン．
const SELECT_LABEL: &str = "[select]";

#[derive(Default)]
pub struct Launcher {
    /// タスクの一覧で選んでいる行．
    pub selected: usize,
    /// 選択画面（開いていれば）．
    pub select: Option<Selection>,
}

impl Launcher {
    /// キー入力（生バイト列）を処理する．
    pub fn feed(&mut self, bytes: &[u8], len: usize) -> MenuResult {
        for key in keys(bytes) {
            let result = match &mut self.select {
                Some(sel) => match key {
                    Key::Up => {
                        sel.cursor = (sel.cursor + sel.items.len() - 1) % sel.items.len().max(1);
                        MenuResult::Stay
                    }
                    Key::Down => {
                        sel.cursor = (sel.cursor + 1) % sel.items.len().max(1);
                        MenuResult::Stay
                    }
                    Key::Space | Key::Char('x') => {
                        sel.toggle();
                        MenuResult::Stay
                    }
                    Key::Char('a') => {
                        let on = !sel.checked.iter().all(|&c| c);
                        sel.checked.iter_mut().for_each(|c| *c = on);
                        MenuResult::Stay
                    }
                    Key::Enter => sel.launch(),
                    Key::Left => {
                        self.select = None;
                        MenuResult::Stay
                    }
                    Key::Cancel => MenuResult::Cancel,
                    _ => MenuResult::Stay,
                },
                None if len == 0 => MenuResult::Cancel,
                None => match key {
                    Key::Up => {
                        self.selected = (self.selected + len - 1) % len;
                        MenuResult::Stay
                    }
                    Key::Down => {
                        self.selected = (self.selected + 1) % len;
                        MenuResult::Stay
                    }
                    Key::Enter => MenuResult::Launch(self.selected),
                    Key::Right => MenuResult::OpenSelect(self.selected),
                    Key::Cancel => MenuResult::Cancel,
                    Key::Char(c) => match shortcut_index(c) {
                        Some(i) if i < len => MenuResult::Launch(i),
                        _ => MenuResult::Stay,
                    },
                    _ => MenuResult::Stay,
                },
            };
            if !matches!(result, MenuResult::Stay) {
                return result;
            }
        }
        MenuResult::Stay
    }

    fn content(&self, tasks: &[TaskEntry]) -> Content {
        match &self.select {
            Some(sel) => Content {
                title: format!(" Select: {} ", sel.task.name),
                lines: sel.lines(),
                cursor: Some(sel.cursor),
                buttons: vec![
                    ("[Enter: launch]", Button::LaunchSelected),
                    ("[Space: toggle]", Button::Toggle),
                    ("[<-: back]", Button::Back),
                ],
            },
            None => Content {
                title: " Launch task ".into(),
                lines: if tasks.is_empty() {
                    vec!["(no tasks: write tasks: in shouji.yaml)".into()]
                } else {
                    // 名前の列は一番長い名前にそろえる
                    let name_w = tasks.iter().map(|t| t.name.width()).max().unwrap_or(0).max(12);
                    tasks
                        .iter()
                        .enumerate()
                        .map(|(i, t)| {
                            let key = shortcut(i).unwrap_or(' ');
                            let pad = " ".repeat(name_w - t.name.width());
                            format!(" {key}  {}{pad} {SELECT_LABEL}  {}", t.name, t.path.display())
                        })
                        .collect()
                },
                cursor: (!tasks.is_empty()).then_some(self.selected),
                buttons: vec![("Enter: launch all", Button::Hint), ("->: select", Button::Hint), ("Esc: cancel", Button::Hint)],
            },
        }
    }

    /// 枠の下辺の文字列と，各ボタンの位置（下辺の左端からのセル数）．
    fn footer(content: &Content) -> (String, Vec<(usize, usize, Button)>) {
        let mut text = String::from(" ");
        let mut spans = Vec::new();
        for (label, button) in &content.buttons {
            let start = text.width();
            text.push_str(label);
            spans.push((start, start + label.width(), *button));
            text.push_str("  ");
        }
        (text, spans)
    }

    /// メニューの位置を求める（描画とクリック判定で共通）．
    fn geometry(&self, fw: u16, fh: u16, content: &Content) -> Option<Geometry> {
        let width = |s: &str| s.width() as u16;
        let (footer, _) = Self::footer(content);
        let inner_w = content.lines.iter().map(|l| width(l) + 1).chain([width(&footer), width(&content.title) + 2]).max().unwrap_or(0);
        let w = (inner_w + 2).min(fw);
        let h = (content.lines.len() as u16 + 2).min(fh.saturating_sub(1));
        if w < 4 || h < 3 {
            return None;
        }
        let x0 = (fw - w) / 2;
        let y0 = (fh.saturating_sub(1) - h) / 2;
        // カーソルの行が見えるように表示位置をずらす
        let visible = h as usize - 2;
        let offset = content.cursor.map_or(0, |c| c.saturating_sub(visible.saturating_sub(1)));
        Some(Geometry { x0, y0, w, h, offset })
    }

    /// 画面の (x, y) がクリックされたときの結果．メニューの外ならキャンセル．
    pub fn click(&mut self, x: u16, y: u16, fw: u16, fh: u16, tasks: &[TaskEntry]) -> MenuResult {
        let content = self.content(tasks);
        let Some(g) = self.geometry(fw, fh, &content) else { return MenuResult::Cancel };
        let inside = x >= g.x0 && x < g.x0 + g.w && y >= g.y0 && y < g.y0 + g.h;
        if !inside {
            return MenuResult::Cancel;
        }
        let col = (x - g.x0 - 1) as usize; // 枠の内側の左端からのセル数

        // 下辺のボタン（選択画面のみ）
        if y == g.y0 + g.h - 1 {
            let (_, spans) = Self::footer(&content);
            let Some(sel) = &mut self.select else { return MenuResult::Stay };
            return match spans.iter().find(|(s, e, _)| (*s..*e).contains(&col)).map(|(_, _, b)| *b) {
                Some(Button::LaunchSelected) => sel.launch(),
                Some(Button::Toggle) => {
                    sel.toggle();
                    MenuResult::Stay
                }
                Some(Button::Back) => {
                    self.select = None;
                    MenuResult::Stay
                }
                _ => MenuResult::Stay,
            };
        }
        if y == g.y0 || x == g.x0 || x == g.x0 + g.w - 1 {
            return MenuResult::Stay; // 枠
        }
        let row = (y - g.y0 - 1) as usize + g.offset;
        match &mut self.select {
            Some(sel) if row < sel.items.len() => {
                sel.cursor = row;
                sel.toggle();
                MenuResult::Stay
            }
            Some(_) => MenuResult::Stay,
            None if row < tasks.len() => {
                self.selected = row;
                // 行の [select] なら選択画面，それ以外は全部起動
                let line = &content.lines[row];
                let on_select = line.find(SELECT_LABEL).is_some_and(|idx| {
                    let start = line[..idx].width();
                    (start..start + SELECT_LABEL.width()).contains(&col)
                });
                if on_select { MenuResult::OpenSelect(row) } else { MenuResult::Launch(row) }
            }
            None => MenuResult::Stay,
        }
    }

    /// ホイールで選択を上下させる．
    pub fn wheel(&mut self, up: bool, len: usize) {
        match &mut self.select {
            Some(sel) if !sel.items.is_empty() => {
                let n = sel.items.len();
                sel.cursor = if up { (sel.cursor + n - 1) % n } else { (sel.cursor + 1) % n };
            }
            Some(_) => {}
            None if len > 0 => {
                self.selected = if up { (self.selected + len - 1) % len } else { (self.selected + 1) % len };
            }
            None => {}
        }
    }

    /// 画面中央に枠付きのメニューを描く．
    pub fn draw(&self, frame: &mut Frame, tasks: &[TaskEntry]) {
        let content = self.content(tasks);
        let Some(g) = self.geometry(frame.w, frame.h, &content) else { return };
        let Geometry { x0, y0, w, h, .. } = g;

        use crate::ui::theme::*;
        let border = Style { fg: AMBER, bg: DIALOG, ..Style::default() };
        let normal = Style { fg: TEXT, bg: DIALOG, ..Style::default() };
        let sel = Style { fg: BG, bg: AMBER, ..Style::default() };

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
                frame.set(x, y, ch, if ch == " " { normal } else { border });
            }
        }
        put_clipped(frame, x0 + 2, y0, &content.title, border, x0 + w - 1);
        let (footer, _) = Self::footer(&content);
        put_clipped(frame, x0 + 1, y0 + h - 1, &footer, border, x0 + w - 1);

        for (i, line) in content.lines.iter().enumerate().skip(g.offset).take(g.visible()) {
            let y = y0 + 1 + (i - g.offset) as u16;
            let style = if content.cursor == Some(i) { sel } else { normal };
            for x in x0 + 1..x0 + w - 1 {
                frame.set(x, y, " ", style);
            }
            put_clipped(frame, x0 + 1, y, line, style, x0 + w - 1);
        }
    }
}

/// `end` の手前までに収まる分だけ書く（全角文字は 2 セル）．
pub fn put_clipped(frame: &mut Frame, x: u16, y: u16, s: &str, style: Style, end: u16) {
    let mut x = x;
    let mut buf = [0u8; 4];
    for ch in s.chars() {
        let wide = ch.width() == Some(2);
        let need = if wide { 2 } else { 1 };
        if x + need > end {
            break;
        }
        frame.set(x, y, ch.encode_utf8(&mut buf), style);
        if wide {
            frame.set(x + 1, y, "", style);
        }
        x += need;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Program, TaskWindow};
    use std::time::Duration;

    #[test]
    fn navigate_and_launch() {
        let mut l = Launcher::default();
        assert!(matches!(l.feed(b"\x1b[B", 3), MenuResult::Stay));
        assert!(matches!(l.feed(b"\x1b[B", 3), MenuResult::Stay));
        assert_eq!(l.selected, 2);
        assert!(matches!(l.feed(b"\x1b[B", 3), MenuResult::Stay));
        assert_eq!(l.selected, 0);
        assert!(matches!(l.feed(b"\r", 3), MenuResult::Launch(0)));
        assert!(matches!(l.feed(b"2", 3), MenuResult::Launch(1)));
        assert!(matches!(l.feed(b"\x1b[C", 3), MenuResult::OpenSelect(0)));
        assert!(matches!(l.feed(b"\x1b", 3), MenuResult::Cancel));
    }

    fn program(name: &str) -> Program {
        Program {
            name: name.into(),
            command: format!("run {name}"),
            if_env: Vec::new(),
            choices: Vec::new(),
            pre_command: None,
            cwd: None,
            env: Vec::new(),
            keep_on_exit: true,
            autostart: true,
            after: Vec::new(),
            delay: Duration::ZERO,
            restart: Default::default(),
            restart_delay: Duration::from_secs(1),
            max_restarts: None,
            stop: None,
            stop_timeout: Duration::from_secs(10),
            log: None,
        }
    }

    fn sample_task() -> Task {
        Task {
            name: "t".into(),
            programs: vec![program("build"), program("server"), program("top")],
            windows: vec![
                TaskWindow { name: "app".into(), programs: vec![0, 1] },
                TaskWindow { name: "mon".into(), programs: vec![2] },
            ],
        }
    }

    fn launched(r: MenuResult) -> Vec<(String, Vec<usize>)> {
        match r {
            MenuResult::LaunchTask(t) => t.windows.into_iter().map(|w| (w.name, w.programs)).collect(),
            _ => panic!("LaunchTask ではない"),
        }
    }

    #[test]
    fn letter_shortcuts() {
        assert_eq!((shortcut(0), shortcut(8), shortcut(9), shortcut(34), shortcut(35)), (Some('1'), Some('9'), Some('a'), Some('z'), None));
        let mut l = Launcher::default();
        // 10 番目は a，15 番目は f．数より後の文字や，以前の移動キーは何もしない
        assert!(matches!(l.feed(b"a", 15), MenuResult::Launch(9)));
        assert!(matches!(l.feed(b"f", 15), MenuResult::Launch(14)));
        assert!(matches!(l.feed(b"g", 15), MenuResult::Stay));
        assert!(matches!(l.feed(b"q", 3), MenuResult::Stay));
        assert!(matches!(l.feed(b"k", 30), MenuResult::Launch(19)));
    }

    #[test]
    fn select_screen() {
        let mut l = Launcher { selected: 0, select: Some(Selection::new(sample_task())) };
        // 行: 0 app, 1 build, 2 server, 3 mon, 4 top
        assert_eq!(l.select.as_ref().unwrap().lines()[0], " [x] app");
        // server を外すと app は一部だけ選ばれた状態
        l.feed(b"\x1b[B\x1b[B ", 1);
        assert_eq!(l.select.as_ref().unwrap().lines()[0], " [-] app");
        // mon（ウィンドウ）を外すと中の top も外れる
        l.feed(b"\x1b[B ", 1);
        assert_eq!(l.select.as_ref().unwrap().lines()[4], "     [ ] top          run top");
        assert_eq!(launched(l.feed(b"\r", 1)), vec![("app".to_string(), vec![0])]);

        // a は「全部選ぶ ⇔ 全部外す」．一部だけ選ばれているときは全部選ぶので，2 回で全部外れる
        assert!(matches!(l.feed(b"aa\r", 1), MenuResult::NothingSelected));
        assert_eq!(launched(l.feed(b"a\r", 1)).len(), 2);

        // ← で一覧に戻る
        l.feed(b"\x1b[D", 1);
        assert!(l.select.is_none());
    }
}
