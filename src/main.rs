mod clipboard;
mod config;
mod deps;
mod help;
mod input;
mod launcher;
mod layout;
mod logfile;
mod mouse;
mod paste;
mod pane;
mod render;
mod scroll;
mod select;
mod session;
mod ui;

use std::collections::HashMap;
use std::io::{self, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute};
use unicode_width::UnicodeWidthChar;

use config::{Config, Restart, Task, TaskEntry};
use deps::{Dep, RunState, Wait};
use input::{Action, Direction, InputParser};
use help::Help;
use launcher::{Launcher, MenuResult, Selection};
use layout::{Dir, Node, PaneId, Rect};
use mouse::{Button, Kind as MouseKind, MouseEvent};
use pane::Pane;
use render::{Frame, Renderer, Style};
use select::{Mode as SelectMode, Pos, View};

/// `end` の手前までに収まる分だけ書き，使ったセル数を返す（全角文字は 2 セル）．
fn put_clipped(frame: &mut Frame, x: u16, y: u16, s: &str, style: Style, end: u16) -> u16 {
    let mut cx = x;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0) as u16;
        if cw == 0 {
            continue;
        }
        if cx + cw > end {
            break;
        }
        frame.set(cx, y, ch.encode_utf8(&mut [0u8; 4]), style);
        if cw == 2 {
            frame.set(cx + 1, y, "", style);
        }
        cx += cw;
    }
    cx - x
}

const MOUSE_HINT: &str = "Mouse: on (drag to copy, middle-click or C-q ] to paste, C-q m to turn off)";
/// 続けてクリックしたとみなす間隔（ダブルクリックで単語，トリプルクリックで行を選ぶ）．
const MULTI_CLICK: Duration = Duration::from_millis(400);

/// メインループに届くイベント．
pub enum Event {
    /// キーボードからの入力（生バイト列）．
    Input(Vec<u8>),
    /// いずれかのペインに出力があった．
    Output,
    /// ペインのプロセスが終了した（ペイン, 何回目の起動か）．
    Exited(PaneId, u64),
    /// 端末サイズが変わった（幅, 高さ）．
    Resize(u16, u16),
    /// クライアントが接続した（接続番号, 書き込み先, 幅, 高さ）．
    Attach(u64, UnixStream, u16, u16),
    /// クライアントからのキー入力．
    ClientInput(u64, Vec<u8>),
    /// クライアントの端末のサイズが変わった．
    ClientResize(u64, u16, u16),
    /// クライアントが切断した．
    ClientGone(u64),
    /// `shouji ls` からの問い合わせ（返事を書く先）．
    Info(UnixStream),
    /// `shouji kill`．
    Kill,
}

/// raw モードと代替画面を有効にし，drop 時に元へ戻す．
struct TerminalGuard;

impl TerminalGuard {
    fn new() -> Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut out = io::stdout();
        let _ = write!(out, "{}\x1b[?1l\x1b>\x1b[?2004l\x1b[0m", mouse::DISABLE);
        let _ = execute!(out, LeaveAlternateScreen, cursor::Show);
        let _ = terminal::disable_raw_mode();
    }
}


/// ウィンドウ = 画面全体を使うペインの集まり（tmux の window）．
struct Window {
    /// ステータスバーに出す名前（シェルなら "shell"，ランチャーからならプログラム名）．
    name: String,
    root: Option<Node>,
    active: PaneId,
    /// 直近のレイアウト結果（表示順）．各ペインの中身の領域（タイトル行を除く）．
    rects: Vec<(PaneId, Rect)>,
    /// 各ペインのタイトル行（枠の上辺）．
    titles: Vec<(PaneId, Rect)>,
    /// 各ペインの枠を含めた領域．
    boxes: Vec<(PaneId, Rect)>,
    /// 選択中のペインだけを大きく表示しているか．
    zoom: bool,
}

impl Window {
    fn new(name: String, pane: PaneId) -> Self {
        Self { name, root: Some(Node::Leaf(pane)), active: pane, rects: Vec::new(), titles: Vec::new(), boxes: Vec::new(), zoom: false }
    }

    fn rect_of(&self, id: PaneId) -> Option<Rect> {
        self.rects.iter().find(|(i, _)| *i == id).map(|(_, r)| *r)
    }

    fn contains(&self, id: PaneId) -> bool {
        self.ids().contains(&id)
    }

    /// ウィンドウの全ペイン（表示順．大きく表示中で隠れているものも含む）．
    fn ids(&self) -> Vec<PaneId> {
        self.root.as_ref().map(Node::ids).unwrap_or_default()
    }

    /// レイアウトを計算し直し，サイズが変わったペインの PTY を合わせる．
    fn relayout(&mut self, area: Rect, panes: &mut HashMap<PaneId, Pane>) {
        let old = std::mem::take(&mut self.rects);
        self.titles.clear();
        self.boxes.clear();
        let mut full = Vec::new();
        if let Some(root) = &self.root {
            if self.zoom && self.contains(self.active) {
                full.push((self.active, area));
            } else {
                // 左右の区切り線の 1 列は枠どうしのすき間にする．
                // 上下の区切り線の 1 行は上のペインに足し，枠どうしをくっつける
                let mut seps = Vec::new();
                root.layout(area, &mut full, &mut seps);
                for sep in seps.iter().filter(|s| s.dir == Dir::Vertical) {
                    for (_, r) in full.iter_mut() {
                        if r.y + r.h == sep.y && r.x >= sep.x && r.x + r.w <= sep.x + sep.len {
                            r.h += 1;
                        }
                    }
                }
            }
        }
        // 各ペインを枠で囲み，上辺をタイトルにする（小さすぎれば枠なしでタイトル行だけ）
        for (id, r) in full {
            self.boxes.push((id, r));
            if r.w >= 4 && r.h >= 3 {
                self.titles.push((id, Rect { h: 1, ..r }));
                self.rects.push((id, Rect { x: r.x + 1, y: r.y + 1, w: r.w - 2, h: r.h - 2 }));
            } else if r.h >= 2 {
                self.titles.push((id, Rect { h: 1, ..r }));
                self.rects.push((id, Rect { y: r.y + 1, h: r.h - 1, ..r }));
            } else {
                self.rects.push((id, r));
            }
        }
        for (id, r) in &self.rects {
            let changed = old.iter().find(|(i, _)| i == id).is_none_or(|(_, o)| (o.w, o.h) != (r.w, r.h));
            if changed && let Some(p) = panes.get_mut(id) {
                p.resize(r.h, r.w);
            }
        }
    }

    /// 表示順で次（`back` なら前）のペインへ移る．
    fn focus_next(&mut self, back: bool) {
        let ids: Vec<PaneId> = self.rects.iter().map(|(id, _)| *id).collect();
        if let Some(i) = ids.iter().position(|id| *id == self.active) {
            let n = ids.len();
            self.active = ids[if back { (i + n - 1) % n } else { (i + 1) % n }];
        }
    }

    /// 指定方向に隣接するペインのうち，最も近いものへ移る．
    fn focus_dir(&mut self, d: Direction) {
        let Some(a) = self.rect_of(self.active) else { return };
        let overlap = |s1: u16, l1: u16, s2: u16, l2: u16| s1 < s2 + l2 && s2 < s1 + l1;
        let center = |s: u16, l: u16| s as i32 * 2 + l as i32;

        let best = self
            .rects
            .iter()
            .filter(|(id, _)| *id != self.active)
            .filter_map(|(id, r)| {
                let (ok, dist, off) = match d {
                    Direction::Left => (r.x + r.w <= a.x && overlap(r.y, r.h, a.y, a.h), a.x.saturating_sub(r.x + r.w), center(r.y, r.h) - center(a.y, a.h)),
                    Direction::Right => (r.x >= a.x + a.w && overlap(r.y, r.h, a.y, a.h), r.x.saturating_sub(a.x + a.w), center(r.y, r.h) - center(a.y, a.h)),
                    Direction::Up => (r.y + r.h <= a.y && overlap(r.x, r.w, a.x, a.w), a.y.saturating_sub(r.y + r.h), center(r.x, r.w) - center(a.x, a.w)),
                    Direction::Down => (r.y >= a.y + a.h && overlap(r.x, r.w, a.x, a.w), r.y.saturating_sub(a.y + a.h), center(r.x, r.w) - center(a.x, a.w)),
                };
                ok.then_some((dist, off.abs(), *id))
            })
            .min();
        if let Some((_, _, id)) = best {
            self.active = id;
        }
    }
}

/// stop を実行して，プログラムが終わるのを待っているところ．
struct Stopping {
    /// stop のプロセス（終わったら `None`．そのあともプログラムが終わるのを deadline まで待つ）．
    child: Option<std::process::Child>,
    /// この時刻を過ぎても終わらなければ強制終了する．
    deadline: Instant,
    /// 終わったらペインを閉じるか（false なら Stop ボタン: ペインは残す）．
    close: bool,
}

/// 画面のクリックできる部分．
#[derive(Clone, Copy, Debug)]
enum Click {
    Window(usize),
    /// サイドバーのペイン（ウィンドウ, ペイン）．
    Pane(usize, PaneId),
    /// ペインの中のボタン（ペイン, 押したことにするキー）．
    PaneKey(PaneId, u8),
    /// Stop ボタン（stop を実行する）．
    Stop(PaneId),
    /// 下部のヒント（押したことにするキー入力）．
    Keys(&'static [u8]),
    Split(Dir),
    NewWindow,
    Launcher,
    Help,
}

struct App {
    /// 全ウィンドウのペイン．
    panes: HashMap<PaneId, Pane>,
    windows: Vec<Window>,
    /// 表示中のウィンドウの添字．
    current: usize,
    next_id: PaneId,
    /// 端末全体のサイズ (cols, rows)．
    size: (u16, u16),
    input: InputParser,
    config: Config,
    /// 開いているランチャーメニュー．
    launcher: Option<Launcher>,
    /// 開いているヘルプ画面．
    help: Option<Help>,
    /// 構成のサイドバーを出すか（C-q t で切り替え）．
    sidebar: bool,
    /// 終了の確認中か（C-q d か Quit ボタン）．
    confirm_quit: bool,
    /// detach する（クライアントだけ切り離し，プログラムは動かし続ける）．
    detach: bool,
    /// スクロールモード中か（アクティブなペインの過去の出力を見ている）．
    scrolling: bool,
    /// ステータスバーに出すエラー．次のキー入力かクリックで消える．
    message: Option<String>,
    /// ステータスバーに出すお知らせ．次のキー入力かクリックで消える．
    info: Option<String>,
    /// マウス操作を使うか．
    mouse: bool,
    /// 起動を待っているペイン（task.yaml の after / delay）．
    waits: HashMap<PaneId, Wait>,
    /// 各ペインのプログラムの状態（after の判定に使う）．
    runs: HashMap<PaneId, RunState>,
    /// 自動再起動を待っているペインと，再起動する時刻．
    restarts: HashMap<PaneId, Instant>,
    /// 各ペインで自動再起動した回数（max_restarts の判定に使う）．
    restart_counts: HashMap<PaneId, u32>,
    /// `stop` を実行して，プログラムが終わるのを待っているペイン．
    stopping: HashMap<PaneId, Stopping>,
    /// ペインを閉じた後も動いている stop のプロセス（終わったら回収する）．
    background: Vec<std::process::Child>,
    mouse_input: mouse::Extractor,
    /// 貼り付け（ブラケットペースト）をキー入力から切り分ける．
    paste_input: paste::Splitter,
    /// 選んでいる文字の範囲（マウスのドラッグかコピーモード）．
    selection: Option<select::Selection>,
    /// マウスでドラッグして選んでいる最中か．
    selecting: bool,
    /// 直前のクリックの時刻・位置・続けてクリックした回数（1〜3）．
    last_click: Option<(Instant, u16, u16, u8)>,
    /// コピーモード（スクロールモード中にキーで動かす）のカーソル．
    copy_cursor: Option<Pos>,
    /// shouji の中で最後にコピーした文字列（C-q ] と中クリックで貼り付ける）．
    clipboard: Option<String>,
    /// クライアントの端末のクリップボードへ送る文字列．
    pending_clipboard: Option<String>,
    tx: Sender<Event>,
    quit: bool,
}

impl App {
    /// `startup` はコマンドラインの `-t` で指定されたタスクの添字と，起動するものの名前（空なら全部）．
    /// 起動後はその最後のウィンドウを表示する．
    fn new(tx: Sender<Event>, config: Config, startup: &[(usize, Vec<String>)], size: (u16, u16)) -> Result<Self> {
        // マウス操作は常にオンで起動する（C-q m で切り替えられる）
        let mouse = true;
        let mut app = Self {
            panes: HashMap::new(),
            windows: Vec::new(),
            current: 0,
            next_id: 0,
            size,
            input: InputParser::default(),
            config,
            launcher: None,
            help: None,
            sidebar: true,
            confirm_quit: false,
            detach: false,
            message: None,
            info: None,
            mouse,
            mouse_input: mouse::Extractor::default(),
            paste_input: paste::Splitter::default(),
            selection: None,
            selecting: false,
            last_click: None,
            copy_cursor: None,
            clipboard: None,
            pending_clipboard: None,
            waits: HashMap::new(),
            runs: HashMap::new(),
            restarts: HashMap::new(),
            restart_counts: HashMap::new(),
            stopping: HashMap::new(),
            background: Vec::new(),
            scrolling: false,
            tx,
            quit: false,
        };
        // -t で指定されたタスクを開く（表示は最後に開いたもの）
        for (i, names) in startup {
            app.launch_task_only(*i, names);
        }
        if app.windows.is_empty() {
            // タスクが 1 つも開かなかったときだけ，シェルのウィンドウを開く
            app.new_window()?;
        }
        Ok(app)
    }

    fn win(&self) -> &Window {
        &self.windows[self.current]
    }

    fn win_mut(&mut self) -> &mut Window {
        &mut self.windows[self.current]
    }

    /// シェルを 1 つ持つウィンドウを末尾に作り，そこへ切り替える．
    fn new_window(&mut self) -> Result<()> {
        let area = self.pane_area();
        let id = self.next_id;
        let pane = Pane::spawn(id, area.h, area.w, None, self.tx.clone())?;
        self.next_id += 1;
        self.panes.insert(id, pane);
        let mut win = Window::new("shell".into(), id);
        win.relayout(area, &mut self.panes);
        self.windows.push(win);
        self.current = self.windows.len() - 1;
        Ok(())
    }

    /// shouji.yaml の `i` 番目のタスクを読み込んで起動する．失敗はステータスバーに出す．
    fn launch_task(&mut self, i: usize) {
        self.launch_task_only(i, &[]);
    }

    /// タスクのうち `names`（ウィンドウ名かプログラム名）で選んだものだけを起動する．空なら全部．
    fn launch_task_only(&mut self, i: usize, names: &[String]) {
        let Some(entry) = self.config.tasks.get(i).cloned() else { return };
        let result = entry.load().and_then(|mut task| {
            task.select(names)?;
            self.open_task(task)
        });
        if let Err(e) = result {
            self.report(e);
        }
    }

    /// タスクの各プログラムを，新しいウィンドウを均等に分割したペインで起動する．
    fn open_task(&mut self, task: Task) -> Result<()> {
        // after は別のウィンドウのプログラムも指せるので，先にタスク全体のペイン番号を決める
        let ids: Vec<PaneId> = (self.next_id..self.next_id + task.programs.len()).collect();
        self.next_id += ids.len();

        let area = self.pane_area();
        let mut failed = Vec::new();
        let mut first_window = None;
        for tw in &task.windows {
            let win_ids: Vec<PaneId> = tw.programs.iter().map(|&k| ids[k]).collect();
            let mut win = Window::new(tw.name.clone(), win_ids[0]);
            win.root = Some(Node::tiled(&win_ids, Dir::Horizontal));
            // 先に配置を決め，各ペインをその大きさで起動する
            win.relayout(area, &mut self.panes);
            for &k in &tw.programs {
                let (id, prog) = (ids[k], &task.programs[k]);
                let r = win.rect_of(id).unwrap();
                if prog.waits() {
                    // after / delay のあるものは，待機中のペインとして置いておく
                    let deps = prog
                        .after
                        .iter()
                        .map(|&(d, cond)| Dep { pane: ids[d], name: task.programs[d].name.clone(), cond })
                        .collect();
                    self.panes.insert(id, Pane::idle(r.h, r.w, Some(prog)));
                    self.waits.insert(id, Wait { deps, delay: prog.delay, ready_since: None });
                    continue;
                }
                match Pane::spawn(id, r.h, r.w, Some(prog), self.tx.clone()) {
                    Ok(p) => {
                        self.panes.insert(id, p);
                        self.mark_started(id);
                    }
                    Err(e) => {
                        failed.push(format!("{}: {e}", prog.name));
                        win.root = win.root.take().and_then(|r| r.remove(id));
                    }
                }
            }
            if win.root.is_none() {
                continue; // このウィンドウのプログラムがすべて起動できなかった
            }
            win.relayout(area, &mut self.panes);
            win.active = win.rects[0].0;
            self.windows.push(win);
            first_window.get_or_insert(self.windows.len() - 1);
        }
        let Some(first) = first_window else {
            bail!("cannot start {}: {}", task.name, failed.join(", "));
        };
        // 表示するのはタスクの最初のウィンドウ
        self.current = first;
        if !failed.is_empty() {
            self.message = Some(format!("failed to start: {}", failed.join(", ")));
        }
        Ok(())
    }

    fn relayout_current(&mut self) {
        let area = self.pane_area();
        self.windows[self.current].relayout(area, &mut self.panes);
    }

    /// 非表示のウィンドウも含めて，全ペインの配置とサイズを合わせ直す．
    fn relayout_all(&mut self) {
        let area = self.pane_area();
        for win in &mut self.windows {
            win.relayout(area, &mut self.panes);
        }
    }

    /// 大きく表示しているのをやめる（ペインを移るときなど）．
    fn unzoom(&mut self) {
        if self.win().zoom {
            self.win_mut().zoom = false;
            self.relayout_current();
        }
    }

    fn handle(&mut self, ev: Event) {
        // 最後のペインが閉じて終わるところ（ウィンドウが無い）．同じときに届いた
        // 入力などは捨てる（win() が使えない）
        if self.windows.is_empty() {
            return;
        }
        match ev {
            Event::Input(bytes) => {
                for chunk in self.paste_input.feed(&bytes) {
                    match chunk {
                        paste::Chunk::Keys(bytes) => {
                            let (keys, events) = self.mouse_input.feed(&bytes);
                            for ev in events {
                                self.on_mouse(ev);
                            }
                            if !keys.is_empty() {
                                self.on_keys(&keys);
                            }
                        }
                        paste::Chunk::Paste(text) => self.on_paste(&text),
                    }
                }
            }
            Event::Output => {}
            Event::Exited(id, generation) => self.on_exit(id, generation),
            Event::Resize(w, h) => {
                self.size = (w.max(1), h.max(1));
                self.relayout_all();
            }
            // クライアントとのやり取りはサーバーのループで扱う
            Event::Attach(..) | Event::ClientInput(..) | Event::ClientResize(..) | Event::ClientGone(_) | Event::Info(_) | Event::Kill => {}
        }
    }

    fn on_keys(&mut self, bytes: &[u8]) {
        self.info = None;
        if let Some(h) = self.help.as_mut() {
            if h.feed(bytes, self.size.0, self.size.1) {
                self.help = None;
            }
        } else if let Some(l) = self.launcher.as_mut() {
            self.message = None;
            let result = l.feed(bytes, self.config.tasks.len());
            self.menu_result(result);
        } else if self.confirm_quit {
            self.confirm_key(bytes);
        } else if self.scrolling {
            self.scroll(bytes);
        } else {
            self.message = None;
            self.selection = None;
            for action in self.input.feed(bytes) {
                // C-q d に続けて打ったキー（同時に届いたもの）は，確認への答えにする
                match action {
                    Action::Forward(b) if self.confirm_quit => self.confirm_key(&b),
                    action => self.do_action(action),
                }
            }
        }
    }

    /// 終了の確認への答え: d で動かしたまま離れる，y ですべて止めて終了，それ以外はやめる．
    fn confirm_key(&mut self, bytes: &[u8]) {
        self.confirm_quit = false;
        match bytes.first() {
            Some(b'd' | b'D') => self.detach = true,
            Some(b'y' | b'Y') => self.quit = true,
            _ => {}
        }
    }

    fn menu_result(&mut self, result: MenuResult) {
        match result {
            MenuResult::Stay => {}
            MenuResult::Cancel => self.launcher = None,
            MenuResult::Launch(i) => {
                self.launcher = None;
                self.launch_task(i);
            }
            MenuResult::OpenSelect(i) => {
                // 選択画面はタスクファイルを読んでから開く
                let Some(entry) = self.config.tasks.get(i).cloned() else { return };
                match entry.load() {
                    Ok(task) => {
                        if let Some(l) = self.launcher.as_mut() {
                            l.select = Some(Selection::new(task));
                        }
                    }
                    Err(e) => {
                        self.launcher = None;
                        self.report(e);
                    }
                }
            }
            MenuResult::LaunchTask(task) => {
                self.launcher = None;
                if let Err(e) = self.open_task(task) {
                    self.report(e);
                }
            }
            MenuResult::NothingSelected => self.message = Some("nothing selected (press Space to check items)".into()),
        }
    }

    /// エラーをステータスバーに出す．原因（ファイルが無い・不明な項目など）を先に，
    /// どのファイルかを後ろに付ける．
    fn report(&mut self, e: anyhow::Error) {
        let msg = match e.chain().count() {
            1 => format!("{e}"),
            _ => format!("{} [{e}]", e.root_cause()),
        };
        self.message = Some(msg.replace('\n', " "));
    }

    // ---- マウス ----

    fn on_mouse(&mut self, ev: MouseEvent) {
        if !self.mouse {
            return;
        }
        let (w, h) = self.size;
        let pressed = matches!(ev.kind, MouseKind::Press(_));

        if let Some(help) = self.help.as_mut() {
            match ev.kind {
                MouseKind::Press(_) if help.click_outside(ev.x, ev.y, w, h) => self.help = None,
                MouseKind::WheelUp => help.scroll(-3, w, h),
                MouseKind::WheelDown => help.scroll(3, w, h),
                _ => {}
            }
            return;
        }
        if let Some(l) = self.launcher.as_mut() {
            match ev.kind {
                MouseKind::Press(Button::Left) => {
                    let result = l.click(ev.x, ev.y, w, h, &self.config.tasks);
                    self.menu_result(result);
                }
                MouseKind::WheelUp => l.wheel(true, self.config.tasks.len()),
                MouseKind::WheelDown => l.wheel(false, self.config.tasks.len()),
                _ => {}
            }
            return;
        }

        // ボタン・タブ・ヒントなど: 表示中の部品で判定してからメッセージを消す
        let click = if ev.kind == MouseKind::Press(Button::Left) { self.click_at(ev.x, ev.y) } else { None };
        if pressed {
            self.message = None;
            self.info = None;
        }
        if self.selecting {
            match ev.kind {
                MouseKind::Drag => return self.drag_to(ev.x, ev.y),
                MouseKind::Release => return self.finish_selection(),
                _ => self.selecting = false,
            }
        }
        if let Some(c) = click {
            self.selection = None;
            self.do_click(c);
            return;
        }

        let Some(id) = self.pane_at(ev.x, ev.y) else {
            if pressed {
                self.selection = None;
            }
            return;
        };
        match ev.kind {
            MouseKind::Press(Button::Left) => {
                self.focus_pane(id);
                self.start_selection(id, ev.x, ev.y);
            }
            MouseKind::Press(Button::Middle) => {
                self.focus_pane(id);
                self.paste_clipboard();
            }
            MouseKind::WheelUp => self.wheel(id, true),
            MouseKind::WheelDown => self.wheel(id, false),
            _ => {}
        }
    }

    // ---- 選択とコピー・貼り付け ----

    /// ペインの中身を押したところから選び始める．続けて押すと単語・行を選ぶ．
    fn start_selection(&mut self, id: PaneId, x: u16, y: u16) {
        self.selection = None;
        let Some(r) = self.win().rect_of(id).filter(|r| r.contains(x, y)) else { return };
        let now = Instant::now();
        let count = match self.last_click {
            Some((t, lx, ly, n)) if (lx, ly) == (x, y) && now.duration_since(t) < MULTI_CLICK => n % 3 + 1,
            _ => 1,
        };
        self.last_click = Some((now, x, y, count));
        let mode = match count {
            1 => SelectMode::Char,
            2 => SelectMode::Word,
            _ => SelectMode::Line,
        };
        let Some(pane) = self.panes.get(&id) else { return };
        let span = select::span_at(&pane.parser.lock().unwrap(), mode, y - r.y, x - r.x);
        self.selection = Some(select::Selection::new(id, mode, span));
        self.selecting = true;
    }

    /// ドラッグ中: 選択の終わりを動かす．ペインの上下の外へ出たら 1 行ずつスクロールする．
    fn drag_to(&mut self, x: u16, y: u16) {
        let Some(mut sel) = self.selection else { return };
        let Some(r) = self.win().rect_of(sel.pane) else { return };
        let Some(pane) = self.panes.get(&sel.pane) else { return };
        let mut parser = pane.parser.lock().unwrap();
        let screen = parser.screen_mut();
        let offset = screen.scrollback();
        let row = if y < r.y {
            screen.set_scrollback(offset + 1);
            0
        } else if y >= r.y + r.h {
            screen.set_scrollback(offset.saturating_sub(1));
            r.h.saturating_sub(1)
        } else {
            y - r.y
        };
        let col = x.clamp(r.x, r.x + r.w.saturating_sub(1)) - r.x;
        let scrolled = screen.scrollback() > 0;
        sel.set_head(select::span_at(&parser, sel.mode, row, col));
        drop(parser);
        self.selection = Some(sel);
        if sel.pane == self.win().active && !self.scrolling && scrolled {
            self.scrolling = true;
        }
    }

    /// ボタンを離した: 何か選んでいればコピーする（表示は次の操作まで残す）．
    fn finish_selection(&mut self) {
        self.selecting = false;
        match self.selection {
            Some(sel) if !sel.is_click() => self.copy_selection(),
            _ => self.selection = None,
        }
    }

    /// 選んでいる文字列をクリップボードへ入れる．
    fn copy_selection(&mut self) {
        let Some(sel) = self.selection else { return };
        let Some(pane) = self.panes.get(&sel.pane) else { return };
        let text = select::text(&mut pane.parser.lock().unwrap(), &sel);
        if text.is_empty() {
            return;
        }
        let n = text.chars().count();
        self.info = Some(format!("Copied {n} character{} (C-q ] to paste)", if n == 1 { "" } else { "s" }));
        self.pending_clipboard = Some(text.clone());
        self.clipboard = Some(text);
    }

    /// 端末から貼り付けられた: キーとして解釈せず，そのまま選択中のペインへ送る．
    fn on_paste(&mut self, text: &[u8]) {
        if self.help.is_some() || self.launcher.is_some() || self.confirm_quit {
            return;
        }
        self.info = None;
        self.message = None;
        self.input = InputParser::default();
        self.stop_scrolling();
        self.selection = None;
        self.paste_to_active(text);
    }

    /// shouji の中でコピーした文字列を貼り付ける（C-q ] と中クリック）．
    fn paste_clipboard(&mut self) {
        let Some(text) = self.clipboard.clone() else {
            self.info = Some("Nothing copied yet (drag over text or use C-q [ then v / y)".into());
            return;
        };
        self.stop_scrolling();
        self.selection = None;
        self.paste_to_active(&paste::from_text(&text));
    }

    fn paste_to_active(&mut self, text: &[u8]) {
        let active = self.win().active;
        let Some(p) = self.panes.get_mut(&active) else { return };
        if p.is_dead() || self.waits.contains_key(&active) {
            self.info = Some("Not pasted: the program is not running".into());
            return;
        }
        let bracketed = p.parser.lock().unwrap().screen().bracketed_paste();
        p.write(&paste::wrap(text, bracketed));
    }

    /// 表示中のウィンドウで (x, y) にあるペイン．
    fn pane_at(&self, x: u16, y: u16) -> Option<PaneId> {
        let win = self.win();
        win.boxes.iter().find(|(_, r)| r.contains(x, y)).map(|(id, _)| *id)
    }

    /// スクロールモードを抜け，表示を最新に戻す．
    fn stop_scrolling(&mut self) {
        if self.scrolling {
            if let Some(p) = self.panes.get(&self.win().active) {
                p.parser.lock().unwrap().screen_mut().set_scrollback(0);
            }
            self.scrolling = false;
        }
        self.copy_cursor = None;
    }

    fn focus_pane(&mut self, id: PaneId) {
        if self.win().active != id {
            self.stop_scrolling();
            self.unzoom();
            self.win_mut().active = id;
        }
    }

    /// ホイール: 全画面アプリ（less・top など）には矢印キーを送り，
    /// それ以外は過去の出力をスクロールする．
    fn wheel(&mut self, id: PaneId, up: bool) {
        const LINES: usize = 3;
        let Some(pane) = self.panes.get_mut(&id) else { return };
        let (alt, app_cursor) = {
            let p = pane.parser.lock().unwrap();
            (p.screen().alternate_screen(), p.screen().application_cursor())
        };
        if alt {
            if !pane.is_dead() {
                let seq: &[u8] = match (up, app_cursor) {
                    (true, true) => b"\x1bOA",
                    (true, false) => b"\x1b[A",
                    (false, true) => b"\x1bOB",
                    (false, false) => b"\x1b[B",
                };
                for _ in 0..LINES {
                    pane.write(seq);
                }
            }
            return;
        }

        self.focus_pane(id);
        let page = self.win().rect_of(id).map_or(1, |r| r.h as usize);
        let Some(pane) = self.panes.get(&id) else { return };
        let mut parser = pane.parser.lock().unwrap();
        let screen = parser.screen_mut();
        let cmd = if up { scroll::ScrollCmd::Up(LINES) } else { scroll::ScrollCmd::Down(LINES) };
        let next = scroll::apply(screen.scrollback(), &cmd, page, screen.scrollback_len());
        screen.set_scrollback(next);
        // 一番下まで戻ったらスクロールモードを抜ける（コピーモードのカーソルがあれば続ける）
        self.scrolling = next > 0 || self.copy_cursor.is_some();
    }

    fn do_click(&mut self, click: Click) {
        match click {
            Click::Window(i) if i < self.windows.len() => {
                self.stop_scrolling();
                self.current = i;
            }
            Click::Window(_) => {}
            Click::Pane(i, id) if i < self.windows.len() => {
                self.stop_scrolling();
                self.current = i;
                self.focus_pane(id);
            }
            Click::Pane(..) => {}
            Click::PaneKey(id, key) => {
                self.focus_pane(id);
                if self.waits.contains_key(&id) {
                    self.waiting_key(id, key);
                } else {
                    self.dead_pane_key(id, key);
                }
            }
            Click::Keys(keys) => self.on_keys(keys),
            Click::Stop(id) => {
                self.focus_pane(id);
                self.stop_pane(id);
            }
            Click::Split(dir) => {
                self.stop_scrolling();
                self.split(dir);
            }
            Click::NewWindow => {
                self.stop_scrolling();
                let _ = self.new_window();
            }
            Click::Launcher => {
                self.stop_scrolling();
                self.launcher = Some(Launcher::default());
            }
            Click::Help => {
                self.stop_scrolling();
                self.help = Some(Help::default());
            }
        }
    }

    /// 終了したペインでのキー: r で再起動，s でシェル，q で閉じる．
    fn dead_pane_key(&mut self, id: PaneId, key: u8) {
        let Some(p) = self.panes.get_mut(&id) else { return };
        // 手で操作したら自動再起動の予定は取りやめる（r は回数も数え直す）
        self.restarts.remove(&id);
        match key {
            b'r' => match p.restart(id, self.tx.clone()) {
                Ok(()) => {
                    self.restart_counts.remove(&id);
                    self.mark_started(id);
                }
                Err(e) => self.message = Some(format!("failed to restart: {e}")),
            },
            b's' => {
                if let Err(e) = p.open_shell(id, self.tx.clone()) {
                    self.message = Some(format!("cannot start a shell: {e}"));
                }
            }
            b'q' => self.close_pane(id),
            _ => {}
        }
    }

    fn do_action(&mut self, action: Action) {
        let n = self.windows.len();
        match action {
            Action::Forward(bytes) => {
                let active = self.win().active;
                if self.waits.contains_key(&active) {
                    if let Some(&key) = bytes.iter().find(|b| b"\r\nq".contains(b)) {
                        self.waiting_key(active, key);
                    }
                    return;
                }
                let Some(p) = self.panes.get_mut(&active) else { return };
                if !p.is_dead() {
                    p.write(&bytes);
                } else if let Some(&key) = bytes.iter().find(|b| b"rsq".contains(b)) {
                    self.dead_pane_key(active, key);
                }
            }
            Action::Split(dir) => {
                self.unzoom();
                self.split(dir);
            }
            Action::FocusNext => {
                self.unzoom();
                self.win_mut().focus_next(false);
            }
            Action::Focus(d) => {
                self.unzoom();
                self.win_mut().focus_dir(d);
            }
            Action::Zoom => {
                let win = self.win_mut();
                win.zoom = !win.zoom;
                self.relayout_current();
            }
            Action::Stop => {
                let active = self.win().active;
                self.stop_pane(active);
            }
            Action::ToggleSidebar => {
                self.sidebar = !self.sidebar;
                self.relayout_all();
            }
            Action::Kill => {
                let active = self.win().active;
                self.close_pane(active);
            }
            Action::ScrollMode => self.scrolling = true,
            Action::Paste => self.paste_clipboard(),
            Action::ToggleMouse => {
                self.mouse = !self.mouse;
                self.info = Some(if self.mouse { MOUSE_HINT.into() } else { "Mouse: off (C-q m to turn on)".into() });
            }
            // 動いているプログラムが止まるので，確認してから終了する
            Action::Quit => self.confirm_quit = true,
            Action::NewWindow => {
                let _ = self.new_window();
            }
            Action::OpenLauncher => self.launcher = Some(Launcher::default()),
            Action::Help => self.help = Some(Help::default()),
            Action::NextWindow => self.current = (self.current + 1) % n,
            Action::PrevWindow => self.current = (self.current + n - 1) % n,
            Action::SelectWindow(i) if i < n => self.current = i,
            Action::SelectWindow(_) => {}
        }
    }

    fn split(&mut self, dir: Dir) {
        let new = self.next_id;
        let win = self.win_mut();
        let active = win.active;
        let Some(r) = win.rect_of(active) else { return };
        // 分割後に各ペインが 1 セル以上になるか
        let too_small = match dir {
            Dir::Horizontal => r.w < 3,
            Dir::Vertical => r.h < 4,
        };
        let Some(root) = win.root.as_mut() else { return };
        if too_small || !root.split(active, new, dir) {
            return;
        }
        self.next_id += 1;
        self.relayout_current();

        let nr = self.win().rect_of(new).unwrap();
        match Pane::spawn(new, nr.h, nr.w, None, self.tx.clone()) {
            Ok(p) => {
                self.panes.insert(new, p);
                self.win_mut().active = new;
            }
            Err(_) => {
                let win = self.win_mut();
                win.root = win.root.take().and_then(|r| r.remove(new));
                self.relayout_current();
            }
        }
    }

    /// プロセスが終了した: 設定に応じてペインを残すか閉じる．
    fn on_exit(&mut self, id: PaneId, generation: u64) {
        let Some(pane) = self.panes.get_mut(&id) else { return };
        if pane.generation != generation {
            return; // 再起動前のプロセスからの古い通知
        }
        let keep = pane.keep_on_exit();
        let closing = pane.is_closing();
        let stop_requested = pane.stop_requested;
        let program = pane.program().cloned();
        let Some(exit) = pane.mark_exited() else { return };
        if exit.was_program {
            self.runs.entry(id).or_default().exit = Some(exit.success);
        }

        // Stop ボタンで止めたのなら，待つのは終わり（stop のプロセスは裏で回収する）
        if !closing && let Some(st) = self.stopping.remove(&id) {
            self.background.extend(st.child);
        }

        // 自動再起動（restart）．C-q q で閉じたとき，Stop ボタンで止めたとき，autostart: false のペインは対象外
        let wants_restart = exit.was_program
            && !closing
            && !stop_requested
            && program.as_ref().is_some_and(|p| {
                p.autostart
                    && match p.restart {
                        Restart::Never => false,
                        Restart::OnFailure => !exit.success,
                        Restart::Always => true,
                    }
            });
        if let (true, Some(p)) = (wants_restart, &program) {
            let count = self.restart_counts.get(&id).copied().unwrap_or(0);
            pane.show_exit(&exit.what);
            if p.max_restarts.is_none_or(|max| count < max) {
                let limit = p.max_restarts.map_or(String::new(), |max| format!(" of {max}"));
                pane.note(&format!(
                    "Restarting in {}s (attempt {}{limit}). Press q to cancel.",
                    p.restart_delay.as_secs_f64(),
                    count + 1
                ));
                self.restarts.insert(id, Instant::now() + p.restart_delay);
                self.restart_counts.insert(id, count + 1);
            } else {
                pane.note(&format!("Not restarting again: it already restarted {count} times (restart.max)."));
            }
            return;
        }

        if keep {
            pane.show_exit(&exit.what);
        } else {
            self.remove_pane(id);
        }
    }

    /// 時刻になった自動再起動を実行する．
    fn run_restarts(&mut self) {
        let now = Instant::now();
        let due: Vec<PaneId> = self.restarts.iter().filter(|(_, at)| **at <= now).map(|(id, _)| *id).collect();
        for id in due {
            self.restarts.remove(&id);
            let Some(pane) = self.panes.get_mut(&id) else { continue };
            match pane.restart(id, self.tx.clone()) {
                Ok(()) => self.mark_started(id),
                Err(e) => self.message = Some(format!("failed to restart: {e}")),
            }
        }
    }

    fn mark_started(&mut self, id: PaneId) {
        self.runs.insert(id, RunState { started: true, exit: None });
    }

    // ---- 起動順（after / delay） ----

    /// 条件がそろった待機中のペインを起動する．起動したことで別のペインの
    /// 条件（started）がそろうことがあるので，起動するものがなくなるまで繰り返す．
    fn check_waits(&mut self) {
        self.run_restarts();
        self.poll_stops();
        // ペインが抱えている問題（ログファイルを開けないなど）を表示する
        for pane in self.panes.values_mut() {
            if let Some(w) = pane.warning.take() {
                self.message = Some(w);
            }
        }
        loop {
            let now = Instant::now();
            let panes = &self.panes;
            let runs = &self.runs;
            let ready: Vec<PaneId> = self
                .waits
                .iter_mut()
                .filter_map(|(id, w)| w.poll(now, runs, |p| panes.contains_key(&p)).then_some(*id))
                .collect();
            if ready.is_empty() {
                return;
            }
            for id in ready {
                self.start_waiting(id);
            }
        }
    }

    /// 次に check_waits が必要になる時刻（delay の待ち）．
    fn next_deadline(&self) -> Option<Instant> {
        // stop の実行中は，終わったかを 0.1 秒ごとに見る
        let poll = (!self.stopping.is_empty() || !self.background.is_empty()).then(|| Instant::now() + std::time::Duration::from_millis(100));
        self.waits.values().filter_map(Wait::deadline).chain(self.restarts.values().copied()).chain(poll).min()
    }

    /// 待機中のペインを（条件にかかわらず）今すぐ起動する．
    fn start_waiting(&mut self, id: PaneId) {
        self.waits.remove(&id);
        let Some(pane) = self.panes.get_mut(&id) else { return };
        match pane.launch(id, self.tx.clone()) {
            Ok(()) => self.mark_started(id),
            Err(e) => self.message = Some(format!("failed to start: {e}")),
        }
    }

    /// 待機中のペインでのキー: Enter で今すぐ起動，q で閉じる．
    fn waiting_key(&mut self, id: PaneId, key: u8) {
        match key {
            b'\r' | b'\n' => self.start_waiting(id),
            b'q' => self.remove_pane(id),
            _ => {}
        }
    }

    /// スクロールモード（コピーモード）のキー入力を処理する．
    /// 移動するとカーソルが出て，v で選び始め，y か Enter でコピーして抜ける．
    fn scroll(&mut self, bytes: &[u8]) {
        let active = self.win().active;
        let page = self.win().rect_of(active).map_or(1, |r| r.h as usize);
        let Some(pane) = self.panes.get(&active) else {
            self.scrolling = false;
            self.copy_cursor = None;
            return;
        };
        let mut parser = pane.parser.lock().unwrap();
        // カーソルが無ければ，ペインのカーソルと同じ画面上の位置に出す
        let initial = |parser: &pane::Parser| {
            let view = View::of(parser);
            let (r, c) = parser.screen().cursor_position();
            Pos { row: view.abs(r.min(view.rows.saturating_sub(1))), col: c.min(view.cols.saturating_sub(1)) }
        };
        let mut copy = false;
        let mut exit = false;
        for cmd in scroll::parse(bytes) {
            match cmd {
                scroll::ScrollCmd::Exit => {
                    exit = true;
                    break;
                }
                scroll::ScrollCmd::Copy => {
                    copy = self.selection.is_some_and(|s| s.pane == active);
                    exit = true;
                    break;
                }
                scroll::ScrollCmd::BeginSelect | scroll::ScrollCmd::LineSelect => {
                    let cur = *self.copy_cursor.get_or_insert_with(|| initial(&parser));
                    let mode = if cmd == scroll::ScrollCmd::LineSelect { SelectMode::Line } else { SelectMode::Char };
                    let span = if mode == SelectMode::Line { select::line_span(cur.row) } else { select::char_span(cur) };
                    // 同じ選び方をもう一度押したら選択をやめる
                    self.selection = match self.selection {
                        Some(s) if s.pane == active && s.mode == mode => None,
                        _ => Some(select::Selection::new(active, mode, span)),
                    };
                }
                cmd => {
                    let cur = *self.copy_cursor.get_or_insert_with(|| initial(&parser));
                    let view = View::of(&parser);
                    // カーソルの行は常に表示されている
                    let cells = view.visible(cur.row).map(|r| select::row_cells(&parser, r)).unwrap_or_default();
                    let (next, offset) = select::step(cur, &cmd, &view, page, &cells);
                    parser.screen_mut().set_scrollback(offset);
                    self.copy_cursor = Some(next);
                    if let Some(sel) = self.selection.as_mut().filter(|s| s.pane == active) {
                        sel.set_head(match sel.mode {
                            SelectMode::Line => select::line_span(next.row),
                            _ => select::char_span(next),
                        });
                    }
                }
            }
        }
        drop(parser);
        if copy {
            self.copy_selection();
        }
        if exit {
            self.selection = None;
            self.stop_scrolling();
        }
    }

    /// ペインを閉じる．`stop` があれば先に実行し，プログラムが終わるのを待つ
    /// （待っている間にもう一度閉じると，すぐ強制終了する）．
    fn close_pane(&mut self, id: PaneId) {
        let started = self.runs.get(&id).is_some_and(|r| r.started);
        let waiting = self.waits.contains_key(&id);
        let already = self.stopping.contains_key(&id);
        let Some(p) = self.panes.get_mut(&id) else { return };
        // Stop ボタンで止めている途中なら，閉じる方に切り替える
        let already = already && self.stopping.get(&id).is_some_and(|s| s.close);
        let has_stop = started && !waiting && p.program().is_some_and(|pr| pr.stop.is_some());
        if p.is_dead() {
            // プログラムは終わっているが，後始末（コンテナの削除など）は行う
            if has_stop && let Some(child) = p.spawn_stop() {
                self.background.push(child);
            }
            self.remove_pane(id);
            return;
        }
        if has_stop && !already {
            let timeout = p.program().map_or(std::time::Duration::from_secs(10), |pr| pr.stop_timeout);
            if let Some(child) = p.spawn_stop() {
                p.set_closing();
                self.stopping.insert(id, Stopping { child: Some(child), deadline: Instant::now() + timeout, close: true });
                let name = p.program().map_or(String::new(), |pr| pr.name.clone());
                self.info = Some(format!("Stopping {name}... (close it again to force it)"));
                return;
            }
        }
        // 実際の削除は reader スレッドから Exited が届いたときに行う
        if let Some(st) = self.stopping.remove(&id) {
            self.background.extend(st.child);
        }
        if let Some(p) = self.panes.get_mut(&id) {
            p.kill();
        }
    }

    /// Stop ボタン（C-q s）: stop を実行し，プログラムが終わるのを待つ（ペインは残す）．
    /// プログラムがもう止まっていれば，後始末として stop だけを実行する．
    /// 待っている間にもう一度押すと，すぐ強制終了する．
    fn stop_pane(&mut self, id: PaneId) {
        if self.waits.contains_key(&id) {
            return;
        }
        let Some(p) = self.panes.get_mut(&id) else { return };
        let Some(program) = p.program().cloned() else { return };
        let Some(stop) = program.stop.clone() else {
            self.info = Some(format!("{} has no stop command (write stop: in the task file)", program.name));
            return;
        };
        if self.stopping.contains_key(&id) {
            p.terminate();
            return;
        }
        if p.is_dead() {
            if let Some(child) = p.spawn_stop() {
                self.background.push(child);
                self.info = Some(format!("Running stop for {}: {stop}", program.name));
            }
            return;
        }
        if let Some(child) = p.spawn_stop() {
            p.stop_requested = true;
            self.restarts.remove(&id);
            self.stopping.insert(id, Stopping { child: Some(child), deadline: Instant::now() + program.stop_timeout, close: false });
            self.info = Some(format!("Stopping {}... (press Stop again to force it)", program.name));
        }
    }

    /// stop を実行中のペインのうち，stop が終わったか時間切れになったものを強制終了する
    /// （閉じる場合はペインも閉じる）．終わった stop のプロセスを回収する．
    /// stop が成功して終わったあとも，プログラムが自分で終わるのを deadline まで待つ
    /// （`kill` のようにシグナルを送ってすぐ戻る stop でも，プログラムの後始末を待てるように）．
    /// stop が失敗したら，それ以上は待たない．
    fn poll_stops(&mut self) {
        let now = Instant::now();
        let mut done = Vec::new();
        for (id, st) in self.stopping.iter_mut() {
            // stop が終わっていれば Some(成功したか)
            let finished = match st.child.as_mut().map(|c| c.try_wait()) {
                Some(Ok(Some(status))) => Some(status.success()),
                Some(Err(_)) => Some(false),
                _ => None,
            };
            if finished.is_some() {
                self.background.extend(st.child.take());
            }
            if finished == Some(false) || now >= st.deadline {
                done.push(*id);
            }
        }
        for id in done {
            let Some(st) = self.stopping.remove(&id) else { continue };
            self.background.extend(st.child);
            match self.panes.get_mut(&id) {
                Some(p) if p.is_dead() && st.close => self.remove_pane(id),
                Some(p) if p.is_dead() => {}
                Some(p) if st.close => p.kill(),
                Some(p) => p.terminate(),
                None => {}
            }
        }
        self.background.retain_mut(|c| !matches!(c.try_wait(), Ok(Some(_)) | Err(_)));
    }

    /// セッションを終えるとき: 全ペインの stop を実行し，終わるのを（いちばん長い timeout まで）待つ．
    fn run_all_stops(&mut self) {
        let mut deadline = Instant::now();
        // stop を実行したペイン: (ペイン, stop のプロセス, 待つ期限)
        let mut pending: Vec<(PaneId, Option<std::process::Child>, Instant)> = Vec::new();
        let ids: Vec<PaneId> = self.panes.keys().copied().collect();
        for id in ids {
            if self.stopping.contains_key(&id) || self.waits.contains_key(&id) || !self.runs.get(&id).is_some_and(|r| r.started) {
                continue;
            }
            let Some(p) = self.panes.get_mut(&id) else { continue };
            let timeout = p.program().map_or(std::time::Duration::ZERO, |pr| pr.stop_timeout);
            if let Some(child) = p.spawn_stop() {
                pending.push((id, Some(child), Instant::now() + timeout));
            }
        }
        for (id, st) in self.stopping.drain() {
            pending.push((id, st.child, st.deadline));
        }
        for (_, _, d) in &pending {
            deadline = deadline.max(*d);
        }
        // stop が終わり，プログラムも終わるまで（stop が失敗したら，そのペインは待たない）
        loop {
            let now = Instant::now();
            let panes = &mut self.panes;
            pending.retain_mut(|(id, child, d)| {
                match child.as_mut().map(|c| c.try_wait()) {
                    Some(Ok(Some(status))) if !status.success() => return false,
                    Some(Ok(Some(_)) | Err(_)) => *child = None,
                    _ => {}
                }
                let exited = panes.get_mut(id).is_none_or(|p| p.has_exited());
                (child.is_some() || !exited) && now < *d
            });
            self.background.retain_mut(|c| !matches!(c.try_wait(), Ok(Some(_)) | Err(_)));
            if (pending.is_empty() && self.background.is_empty()) || now >= deadline {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    fn remove_pane(&mut self, id: PaneId) {
        if let Some(st) = self.stopping.remove(&id) {
            self.background.extend(st.child);
        }
        self.waits.remove(&id);
        self.restarts.remove(&id);
        self.restart_counts.remove(&id);
        let Some(mut pane) = self.panes.remove(&id) else { return };
        pane.reap();
        let Some(wi) = self.windows.iter().position(|w| w.contains(id)) else { return };

        let area = self.pane_area();
        let win = &mut self.windows[wi];
        let idx = win.rects.iter().position(|(i, _)| *i == id).unwrap_or(0);
        win.root = win.root.take().and_then(|r| r.remove(id));
        win.relayout(area, &mut self.panes);

        if win.rects.is_empty() {
            // ウィンドウ最後のペインだったので，ウィンドウごと消す
            self.windows.remove(wi);
            if self.windows.is_empty() {
                self.quit = true;
                return;
            }
            if wi < self.current || self.current >= self.windows.len() {
                self.current -= 1;
            }
        } else if win.active == id {
            // 表示順で一つ前のペインに移る
            win.active = win.rects[idx.saturating_sub(1).min(win.rects.len() - 1)].0;
        }
    }
}

const USAGE: &str = "usage: shouji [-s NAME] [-c CONFIG.yaml] [-t TASK.yaml [NAME...]]...\n\
    \n       shouji attach [SESSION]   go back to a running session (also: a)\
    \n       shouji ls                 list running sessions\
    \n       shouji kill [SESSION]     stop a session and all its programs\
    \n\
    \n  -s, --session NAME    session name (default: the first task file's name, or main)\
    \n                        if it is already running, shouji attaches to it\
    \n  -c, --config FILE     config file (default: ./shouji.yaml, then ~/.config/shouji/config.yaml)\
    \n  -t, --task FILE       task file to open at startup (can be repeated)\
    \n                        NAMEs (window or program names) after it start only those\
    \n                        e.g. shouji -t task.yaml app           every program in the app window\
    \n                             shouji -t task.yaml build server  only build and server\
    \n\
    \n  In shouji, Ctrl-q d leaves the session (programs keep running).";

/// 新しいセッションの指定（コマンドライン）．
struct Opts {
    session: Option<String>,
    config_path: Option<std::path::PathBuf>,
    /// (タスクファイル, 起動するものの名前)
    task_args: Vec<(String, Vec<String>)>,
}

fn parse_opts(args: &[String]) -> Result<Opts> {
    let mut opts = Opts { session: None, config_path: None, task_args: Vec::new() };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-s" | "--session" => opts.session = Some(session::sanitize(it.next().context(USAGE)?)),
            "-c" | "--config" => opts.config_path = Some(it.next().context(USAGE)?.into()),
            "-t" | "--task" => opts.task_args.push((it.next().context(USAGE)?.clone(), Vec::new())),
            _ if arg.starts_with('-') => bail!("unknown argument: {arg}\n{USAGE}"),
            // -t FILE に続く名前
            _ => match opts.task_args.last_mut() {
                Some((_, names)) => names.push(arg.clone()),
                None => bail!("{arg}: names must come after -t TASK.yaml\n{USAGE}"),
            },
        }
    }
    Ok(opts)
}

/// 設定と -t のタスクを読む．書式や名前の誤りはここで報告する．
fn load(opts: &Opts) -> Result<(Config, Vec<(usize, Vec<String>)>)> {
    let mut config = Config::load(opts.config_path.as_deref())?;
    // メニューから開き直せるよう，タスク一覧にも加える
    let cwd = std::env::current_dir()?;
    let mut startup = Vec::new();
    for (path, names) in &opts.task_args {
        let entry = TaskEntry::standalone(path, &cwd);
        entry.load()?.select(names).with_context(|| format!("cannot select programs in {path}"))?;
        startup.push((config.add_task(entry), names.clone()));
    }
    Ok((config, startup))
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rest = args.get(1..).unwrap_or_default();
    match args.first().map(String::as_str) {
        Some("-h" | "--help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some("-V" | "--version") => {
            println!("shouji {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("--server") => {
            // shouji --server NAME WIDTHxHEIGHT [オプション...]（クライアントが起動する）
            let name = rest.first().context("--server needs a name")?.clone();
            let size = rest.get(1).and_then(|s| s.split_once('x')).and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))).unwrap_or((80, 24));
            run_server(&name, size, &parse_opts(rest.get(2..).unwrap_or_default())?)
        }
        Some("ls" | "list") => {
            let names = session::sessions()?;
            if names.is_empty() {
                println!("no shouji session is running");
            }
            for name in names {
                println!("{name}: {}", session::info(&name)?);
            }
            Ok(())
        }
        Some("kill") => {
            let name = session::pick(rest.first().map(String::as_str))?;
            session::kill(&name)?;
            println!("stopped session {name}");
            Ok(())
        }
        Some("attach" | "a") => {
            refuse_nesting()?;
            let name = session::pick(rest.first().map(String::as_str))?;
            run_client(&name)
        }
        _ => {
            refuse_nesting()?;
            let opts = parse_opts(&args)?;
            // 設定の誤りは，サーバーを起動する前にここで報告する
            load(&opts)?;
            let name = opts.session.clone().unwrap_or_else(|| {
                let first = opts.task_args.first().map(|(p, _)| std::path::PathBuf::from(p));
                session::default_name(first.as_deref())
            });
            if session::connect(&name)?.is_none() {
                let (w, h) = terminal::size().unwrap_or((80, 24));
                let mut server_args = vec!["--server".to_string(), name.clone(), format!("{w}x{h}")];
                server_args.extend(args.iter().cloned());
                session::spawn_server(&name, &server_args)?;
            } else if !opts.task_args.is_empty() {
                eprintln!("session {name} is already running: attaching to it (use -s NAME to start another)");
                thread::sleep(std::time::Duration::from_millis(800));
            }
            run_client(&name)
        }
    }
}

fn refuse_nesting() -> Result<()> {
    if std::env::var_os("SHOUJI").is_some() {
        bail!("shouji cannot be started inside shouji (SHOUJI is set)");
    }
    Ok(())
}

/// セッションに接続し，終わったら理由を表示する．
fn run_client(name: &str) -> Result<()> {
    let end = session::attach(name)?;
    match end {
        session::End::Detached => println!("[detached from session {name}: the programs keep running. `shouji attach {name}` to go back]"),
        session::End::Exited => println!("[session {name} ended]"),
        session::End::Lost => println!("[lost the connection to session {name}]"),
    }
    // 標準入力を読んでいるスレッドが残っているので，ここで終える
    std::process::exit(0);
}

/// 接続中のクライアント．
struct Client {
    id: u64,
    stream: UnixStream,
}

/// サーバー: 端末なしで動き続け，接続したクライアントに画面を送る．
fn run_server(name: &str, size: (u16, u16), opts: &Opts) -> Result<()> {
    let (config, startup) = load(opts)?;
    let path = session::socket_path(name)?;
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).with_context(|| format!("cannot listen on {}", path.display()))?;

    let (tx, rx) = mpsc::channel();
    {
        let tx = tx.clone();
        thread::spawn(move || accept_loop(listener, tx));
    }

    let mut app = App::new(tx, config, &startup, size)?;
    let mut renderer = Renderer::default();
    let mut client: Option<Client> = None;

    loop {
        app.check_waits();
        if let Some(c) = &mut client {
            let (frame, cursor, modes) = app.compose();
            let mut buf = Vec::with_capacity(1 << 14);
            renderer.draw(&mut buf, frame, cursor, modes)?;
            if session::write_frame(&mut c.stream, session::OUTPUT, &buf).is_err() {
                client = None;
            }
        }
        // コピーした文字列は，接続中の端末のクリップボードへ（クライアントが OSC 52 で書く）
        if let Some(text) = app.pending_clipboard.take()
            && let Some(c) = &mut client
            && session::write_frame(&mut c.stream, session::CLIPBOARD, text.as_bytes()).is_err()
        {
            client = None;
        }

        // delay の待ちがあれば，その時刻までにイベントがなくても起き上がる
        let ev = match app.next_deadline() {
            Some(deadline) => match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(ev) => Some(ev),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            },
            None => match rx.recv() {
                Ok(ev) => Some(ev),
                Err(_) => break,
            },
        };
        let Some(ev) = ev else { continue };
        server_event(&mut app, &mut client, &mut renderer, ev);
        // 溜まっているイベントをまとめて処理してから 1 回だけ描画する
        while let Ok(ev) = rx.try_recv() {
            server_event(&mut app, &mut client, &mut renderer, ev);
        }
        if app.detach {
            app.detach = false;
            if let Some(mut c) = client.take() {
                let _ = session::write_frame(&mut c.stream, session::DETACHED, &[]);
            }
        }
        if app.quit {
            break;
        }
    }

    // stop を実行して待つ間，接続中の端末には「Stopping」と出しておく
    // （最後のペインが閉じて終わるときはウィンドウが無いので描けない）
    if let Some(c) = &mut client
        && !app.windows.is_empty()
    {
        app.info = Some("Stopping the programs...".into());
        app.confirm_quit = false;
        let (frame, cursor, modes) = app.compose();
        let mut buf = Vec::new();
        renderer.draw(&mut buf, frame, cursor, modes)?;
        let _ = session::write_frame(&mut c.stream, session::OUTPUT, &buf);
    }
    app.run_all_stops();
    for pane in app.panes.values_mut() {
        pane.kill();
    }
    if let Some(mut c) = client {
        let _ = session::write_frame(&mut c.stream, session::EXIT, &[]);
    }
    let _ = std::fs::remove_file(&path);
    Ok(())
}

fn server_event(app: &mut App, client: &mut Option<Client>, renderer: &mut Renderer, ev: Event) {
    let current = client.as_ref().map(|c| c.id);
    match ev {
        Event::Attach(id, stream, w, h) => {
            // 別の端末が接続していれば，そちらは切り離す
            if let Some(mut old) = client.take() {
                let _ = session::write_frame(&mut old.stream, session::DETACHED, &[]);
            }
            *client = Some(Client { id, stream });
            *renderer = Renderer::default();
            app.handle(Event::Resize(w, h));
        }
        Event::ClientInput(id, bytes) if Some(id) == current => app.handle(Event::Input(bytes)),
        Event::ClientResize(id, w, h) if Some(id) == current => {
            app.handle(Event::Resize(w, h));
            renderer.invalidate();
        }
        Event::ClientGone(id) if Some(id) == current => *client = None,
        Event::ClientInput(..) | Event::ClientResize(..) | Event::ClientGone(_) => {}
        Event::Info(mut stream) => {
            let running = app.panes.values().filter(|p| !p.is_dead()).count();
            let state = if client.is_some() { "attached" } else { "detached" };
            let text = format!("{} window(s), {running} running program(s), {state}", app.windows.len());
            let _ = session::write_frame(&mut stream, session::INFO_REPLY, text.as_bytes());
        }
        Event::Kill => app.quit = true,
        ev => app.handle(ev),
    }
}

/// ソケットで待ち受け，接続ごとに読み取りスレッドを立てる．
fn accept_loop(listener: UnixListener, tx: Sender<Event>) {
    let mut next_id = 0u64;
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let tx = tx.clone();
        next_id += 1;
        let id = next_id;
        thread::spawn(move || {
            let Ok(Some((kind, payload))) = session::read_frame(&mut stream) else { return };
            match kind {
                session::HELLO => {
                    let (w, h) = session::parse_size(&payload).unwrap_or((80, 24));
                    let Ok(writer) = stream.try_clone() else { return };
                    if tx.send(Event::Attach(id, writer, w, h)).is_err() {
                        return;
                    }
                    while let Ok(Some((kind, payload))) = session::read_frame(&mut stream) {
                        let ev = match kind {
                            session::INPUT => Event::ClientInput(id, payload),
                            session::RESIZE => match session::parse_size(&payload) {
                                Some((w, h)) => Event::ClientResize(id, w, h),
                                None => continue,
                            },
                            _ => continue,
                        };
                        if tx.send(ev).is_err() {
                            return;
                        }
                    }
                    let _ = tx.send(Event::ClientGone(id));
                }
                session::INFO => {
                    let _ = tx.send(Event::Info(stream));
                }
                session::KILL => {
                    let _ = tx.send(Event::Kill);
                }
                _ => {}
            }
        });
    }
}
