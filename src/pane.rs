//! 1 つのペイン = PTY 上で動くシェル（またはプログラム）+ その仮想画面．
//!
//! プロセスが終了しても仮想画面（ログ）は残せる．再起動すると同じ画面の続きに出力される．

use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::sync::mpsc::{self, Sender};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::Event;
use crate::config::Program;
use crate::logfile::LogFile;
use crate::layout::PaneId;

/// PTY へ書き込むスレッドへの送り口．大きな貼り付けをプログラムがすぐ読まなくても，
/// 画面の描画やキー入力の処理が止まらないように，書き込みは別のスレッドで行う．
type Writer = Sender<Vec<u8>>;

/// vt100 が扱わない問い合わせ（カーソル位置・端末種別）に応答する．
/// 応答しないと，fish など起動時に問い合わせるシェルが待たされる．
///
/// 過去の出力から捨てられた行の数も数える（選択の位置を絶対行で持つため，select.rs）．
#[derive(Default)]
pub struct Replies {
    buf: Vec<u8>,
    /// 過去の出力がいっぱいになって捨てられた行の数（代替画面の分は数えない）．
    pub dropped: u64,
}

impl vt100::Callbacks for Replies {
    fn on_scroll(&mut self, _contents: vt100::capture::RowContents<'_>, alternate_screen: bool) {
        if !alternate_screen {
            self.dropped += 1;
        }
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        _i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let first = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        match (i1, c, first) {
            // DSR: カーソル位置の報告
            (None, 'n', 6) => {
                let (row, col) = screen.cursor_position();
                self.buf.extend(format!("\x1b[{};{}R", row + 1, col + 1).bytes());
            }
            // DSR: 状態報告
            (None, 'n', 5) => self.buf.extend(b"\x1b[0n"),
            // DA1: VT102 として応答
            (None, 'c', 0) => self.buf.extend(b"\x1b[?6c"),
            _ => {}
        }
    }
}

pub type Parser = vt100::Parser<Replies>;

/// 実行中のプロセスと，その PTY．
struct Proc {
    writer: Writer,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    /// 設定のプログラムとして起動したか（`s: shell` で開いたシェルなら false）．
    is_program: bool,
}

/// プロセスの終わり方．
pub struct Exit {
    /// 正常終了（終了コード 0）したか．
    pub success: bool,
    /// 終わり方の説明（`code 1`，`signal: Interrupt` など）．
    pub what: String,
    /// 設定のプログラムとして起動したプロセスだったか．
    pub was_program: bool,
}

pub struct Pane {
    pub parser: Arc<Mutex<Parser>>,
    /// 起動するプログラム（`None` ならログインシェル）．
    program: Option<Program>,
    /// 実行中のプロセス．終了後は `None`．
    proc: Option<Proc>,
    /// 何回目に起動したプロセスか．古い reader スレッドからの通知を見分ける．
    pub generation: u64,
    size: (u16, u16),
    /// ユーザーが閉じようとしている（終了しても残さない）．
    closing: bool,
    /// 出力を追記するログファイル（task.yaml の `log`）．
    log: Option<Arc<Mutex<LogFile>>>,
    /// ユーザーに伝えたい問題（ログファイルを開けないなど）．App が取り出して表示する．
    pub warning: Option<String>,
    /// 最後に終了したプロセスの終わり方（正常終了か，"code 1" などの説明）．
    pub last_exit: Option<(bool, String)>,
    /// Stop ボタン（C-q s）で止めようとしている（終了しても自動再起動しない）．
    pub stop_requested: bool,
}

/// スクロールバックとして保持する行数．
const SCROLLBACK: usize = 10_000;

fn nz(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n.max(1)).unwrap()
}

fn pty_size(rows: u16, cols: u16) -> PtySize {
    PtySize { rows: rows.max(1), cols: cols.max(1), pixel_width: 0, pixel_height: 0 }
}

impl Pane {
    /// `program` を（省略時はログインシェルを）起動する．
    pub fn spawn(id: PaneId, rows: u16, cols: u16, program: Option<&Program>, tx: Sender<Event>) -> Result<Self> {
        let mut pane = Self::idle(rows, cols, program);
        pane.start(id, tx, false)?;
        Ok(pane)
    }

    /// まだ起動しないペインを作る（`after` や `delay` で待つプログラム用）．
    /// 起動するときは [`Pane::launch`] を呼ぶ．
    pub fn idle(rows: u16, cols: u16, program: Option<&Program>) -> Self {
        let parser = vt100::Parser::new_with_callbacks(nz(rows), nz(cols), SCROLLBACK, Replies::default());
        let (log, warning) = match program.and_then(|p| p.log.as_deref()) {
            None => (None, None),
            Some(path) => match LogFile::open(path) {
                Ok(f) => (Some(Arc::new(Mutex::new(f))), None),
                Err(e) => (None, Some(format!("cannot open log file {}: {e}", path.display()))),
            },
        };
        Self {
            parser: Arc::new(Mutex::new(parser)),
            program: program.cloned(),
            proc: None,
            generation: 0,
            size: (rows, cols),
            closing: false,
            log,
            warning,
            last_exit: None,
            stop_requested: false,
        }
    }

    /// 待っていたプログラムを起動する．
    pub fn launch(&mut self, id: PaneId, tx: Sender<Event>) -> Result<()> {
        if self.proc.is_some() {
            return Ok(());
        }
        self.start(id, tx, false)
    }

    /// 新しいプロセスを起動し，出力を読み取るスレッドを立てる．仮想画面は引き継ぐ．
    /// `shell` が true なら，プログラムの代わりにシェルを（プログラムの cwd と env で）起動する．
    /// `autostart: false` のプログラムは，シェルを起動してコマンドを入力した状態にする．
    fn start(&mut self, id: PaneId, tx: Sender<Event>, shell: bool) -> Result<()> {
        let (rows, cols) = self.size;
        let pair = native_pty_system().openpty(pty_size(rows, cols))?;
        let program = self.program.as_ref();
        let typeahead = program.filter(|p| !shell && !p.autostart).map(|p| p.command.trim_end().to_string());

        let login_shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let script = match program {
            Some(p) if !shell && typeahead.is_none() => Some(program_script(&p.env, p.pre_command.as_deref(), &p.command)),
            // シェルを開くときも env と pre_command を設定してから，その環境でシェルに入れ替える
            Some(p) if !p.env.is_empty() || p.pre_command.is_some() => {
                Some(shell_script(&p.env, p.pre_command.as_deref(), &login_shell))
            }
            _ => None,
        };
        let mut cmd = match script {
            Some(script) => {
                let mut cmd = CommandBuilder::new(&login_shell);
                cmd.args(["-c", &script]);
                cmd
            }
            None => CommandBuilder::new_default_prog(),
        };
        cmd.env("TERM", "xterm-256color");
        cmd.env("SHOUJI", "1");
        if let Some(cwd) = program.and_then(|p| p.cwd.clone()).or_else(|| std::env::current_dir().ok()) {
            cmd.cwd(cwd);
        }
        let child = pair.slave.spawn_command(cmd)?;
        // slave を閉じておかないと，子が終了しても reader が EOF にならない．
        drop(pair.slave);

        let writer: Writer = {
            let mut out = pair.master.take_writer()?;
            let (writer, rx) = mpsc::channel::<Vec<u8>>();
            // 送り口（Proc と読み取りスレッドが持つ）がすべてなくなると終わる
            thread::spawn(move || {
                for buf in rx {
                    if out.write_all(&buf).and_then(|()| out.flush()).is_err() {
                        break;
                    }
                }
            });
            writer
        };
        let mut reader = pair.master.try_clone_reader()?;
        self.generation += 1;
        self.stop_requested = false;
        let generation = self.generation;
        let got_output = Arc::new(AtomicBool::new(false));

        if let Some(command) = typeahead {
            // シェルの準備（最初のプロンプト）を待ってからコマンドを入力する．
            // 早すぎると端末のエコーと二重に表示されてしまう．
            let writer = writer.clone();
            let got_output = Arc::clone(&got_output);
            thread::spawn(move || {
                for _ in 0..500 {
                    if got_output.load(Ordering::Relaxed) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                thread::sleep(Duration::from_millis(150));
                let _ = writer.send(command.into_bytes());
            });
        }

        {
            if let Some(log) = &self.log {
                let what = match program {
                    Some(p) if !shell => format!("started: {}", p.command.trim_end()),
                    _ => "started: shell".into(),
                };
                log.lock().unwrap().marker(&what);
            }
            let parser = Arc::clone(&self.parser);
            let writer = writer.clone();
            let got_output = Arc::clone(&got_output);
            let log = self.log.clone();
            thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    let n = match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    let replies = {
                        let mut p = parser.lock().unwrap();
                        p.process(&buf[..n]);
                        std::mem::take(&mut p.callbacks_mut().buf)
                    };
                    got_output.store(true, Ordering::Relaxed);
                    if let Some(log) = &log {
                        log.lock().unwrap().write(&buf[..n]);
                    }
                    if !replies.is_empty() {
                        let _ = writer.send(replies);
                    }
                    if tx.send(Event::Output).is_err() {
                        return;
                    }
                }
                let _ = tx.send(Event::Exited(id, generation));
            });
        }

        self.proc = Some(Proc { writer, master: pair.master, child, is_program: !shell });
        self.closing = false;
        Ok(())
    }

    /// 実行中のプロセスの PID と，前面で動いているプロセス（グループ）の PID．
    /// 後者はシェルの中で実行したコマンドなど，Ctrl-C で止めたい相手になる．
    pub fn pids(&self) -> Option<(u32, Option<u32>)> {
        let proc = self.proc.as_ref()?;
        let pid = proc.child.process_id()?;
        let fg = proc.master.process_group_leader().and_then(|p| u32::try_from(p).ok());
        Some((pid, fg))
    }

    /// 設定のプログラム（シェルのペインなら `None`）．
    pub fn program(&self) -> Option<&Program> {
        self.program.as_ref()
    }

    /// プログラムのペインで，`s: shell` で開いたシェルが動いているか．
    pub fn running_shell(&self) -> bool {
        self.program.is_some() && self.proc.as_ref().is_some_and(|p| !p.is_program)
    }

    pub fn is_dead(&self) -> bool {
        self.proc.is_none()
    }

    /// 終了したときにペインを残すか．シェルは残さず，プログラムは設定に従う．
    pub fn keep_on_exit(&self) -> bool {
        !self.closing && self.program.as_ref().is_some_and(|p| p.keep_on_exit)
    }

    /// プロセスが終了したことを記録し，終わり方を返す．画面への案内は [`Pane::show_exit`] で出す．
    pub fn mark_exited(&mut self) -> Option<Exit> {
        let mut proc = self.proc.take()?;
        // PTY が閉じた直後はまだ終了していないことがあるので少し待つ
        let mut status = None;
        for _ in 0..20 {
            if let Ok(Some(s)) = proc.child.try_wait() {
                status = Some(s);
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let what = match &status {
            Some(s) => match s.signal() {
                Some(sig) => format!("signal: {sig}"),
                None => format!("code {}", s.exit_code()),
            },
            None => "status unknown".into(),
        };
        if let Some(log) = &self.log {
            log.lock().unwrap().marker(&format!("exited ({what})"));
        }
        let success = status.as_ref().is_some_and(|s| s.success());
        self.last_exit = Some((success, what.clone()));
        Some(Exit { success, what, was_program: proc.is_program })
    }

    /// 終了したことを 1 行だけ画面に書き込む（過去の出力に記録として残る）．
    /// 再起動・シェル・閉じるのボタンは画面の一番下に重ねて描く（App::exit_bar）．
    pub fn show_exit(&mut self, what: &str) {
        let msg = format!("\x1b[0m\r\n\x1b[2m--- exited ({what}) ---\x1b[0m\r\n");
        self.parser.lock().unwrap().process(msg.as_bytes());
    }

    /// 画面に 1 行の案内を書き込む（自動再起動の待ちなど）．
    pub fn note(&mut self, text: &str) {
        let msg = format!("\x1b[0m\x1b[33m {text}\x1b[0m\r\n");
        self.parser.lock().unwrap().process(msg.as_bytes());
    }

    /// `C-q q` で閉じようとしているか（このときは自動再起動しない）．
    pub fn is_closing(&self) -> bool {
        self.closing
    }

    /// 終了したプロセスを，同じペイン（同じ画面）で起動し直す．
    pub fn restart(&mut self, id: PaneId, tx: Sender<Event>) -> Result<()> {
        if self.proc.is_some() {
            return Ok(());
        }
        self.parser.lock().unwrap().process(b"\x1b[0m\r\n");
        self.start(id, tx, false)
    }

    /// 終了したペインで，プログラムと同じ cwd・env のシェルを起動する．
    /// シェルを exit すると，また終了時の案内に戻る（r でプログラムを再起動できる）．
    pub fn open_shell(&mut self, id: PaneId, tx: Sender<Event>) -> Result<()> {
        if self.proc.is_some() {
            return Ok(());
        }
        self.parser.lock().unwrap().process(b"\x1b[0m\r\n");
        self.start(id, tx, true)
    }

    pub fn write(&mut self, bytes: &[u8]) {
        if let Some(proc) = &self.proc {
            let _ = proc.writer.send(bytes.to_vec());
        }
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.size = (rows, cols);
        if let Some(proc) = &self.proc {
            let _ = proc.master.resize(pty_size(rows, cols));
        }
        self.parser.lock().unwrap().set_size(nz(rows), nz(cols));
    }

    /// プロセスを止める．止まったあとペインは残さず閉じる．
    pub fn kill(&mut self) {
        self.closing = true;
        if let Some(proc) = &mut self.proc {
            let _ = proc.child.kill();
        }
    }

    /// プロセスを止める（ペインは閉じない）．SIGHUP を送り，終わらなければ SIGKILL．
    pub fn terminate(&mut self) {
        if let Some(proc) = &mut self.proc {
            let _ = proc.child.kill();
        }
    }

    /// 閉じようとしている印を付ける（終了しても残さず，自動再起動もしない）．
    pub fn set_closing(&mut self) {
        self.closing = true;
    }

    /// task.yaml の `stop` を，プログラムと同じ cwd・env（と pre_command）で裏で実行する．
    /// 出力はログファイルがあればそこへ追記する．`stop` がなければ `None`．
    pub fn spawn_stop(&mut self) -> Option<std::process::Child> {
        let p = self.program.as_ref()?;
        let stop = p.stop.as_ref()?;
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let mut cmd = std::process::Command::new(shell);
        cmd.arg("-c").arg(program_script(&p.env, p.pre_command.as_deref(), stop));
        cmd.env("SHOUJI", "1").stdin(std::process::Stdio::null());
        if let Some(cwd) = p.cwd.clone().or_else(|| std::env::current_dir().ok()) {
            cmd.current_dir(cwd);
        }
        let out = p.log.as_ref().and_then(|path| std::fs::OpenOptions::new().create(true).append(true).open(path).ok());
        if let (Some(log), Some(_)) = (&self.log, &out) {
            log.lock().unwrap().marker(&format!("stop: {stop}"));
        }
        match out.as_ref().and_then(|f| Some((f.try_clone().ok()?, f.try_clone().ok()?))) {
            Some((o, e)) => cmd.stdout(o).stderr(e),
            None => cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()),
        };
        match cmd.spawn() {
            Ok(child) => Some(child),
            Err(e) => {
                self.warning = Some(format!("cannot run stop for {}: {e}", p.name));
                None
            }
        }
    }

    /// プロセスが終わっているか（終了の通知を待たずに確かめる）．
    pub fn has_exited(&mut self) -> bool {
        self.proc.as_mut().is_none_or(|p| !matches!(p.child.try_wait(), Ok(None)))
    }

    /// 終了済みの子プロセスを回収する（ゾンビ化を防ぐ）．
    pub fn reap(&mut self) {
        if let Some(proc) = &mut self.proc {
            let _ = proc.child.try_wait();
        }
    }
}

/// シェルの 1 引数として渡せるようにシングルクォートで囲む．
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// env を設定するスクリプト．値はダブルクォートの中に置くので，`${VAR:-x}` や `$(cmd)` を
/// シェルが展開する．書いた順に export するので，後の値から前の変数を使える．
/// 展開する値（`$` か `` ` `` を含む）が空になったら，警告を出す（プログラムは起動する）．
fn env_script(env: &[(String, String)]) -> String {
    let mut s = String::new();
    for (k, v) in env {
        s.push_str(&format!("export {k}=\"{v}\"\n"));
    }
    // 同じ名前は最後の値だけ確かめる
    for (i, (k, v)) in env.iter().enumerate() {
        let last = !env[i + 1..].iter().any(|(k2, _)| k2 == k);
        if last && (v.contains('$') || v.contains('`')) {
            s.push_str(&format!(
                "[ -n \"${k}\" ] || printf '\\033[33mshouji: warning: %s is empty (env: %s)\\033[0m\\n' {} {} >&2\n",
                quote(k),
                quote(v)
            ));
        }
    }
    s
}

/// プログラムとして実行するスクリプト．env を設定し，pre_command が失敗したら command は実行しない．
/// pre_command は複数行やコメントでもよいように `{ }` で囲む（終了コードは最後のコマンドのもの）．
fn program_script(env: &[(String, String)], pre: Option<&str>, command: &str) -> String {
    let env = env_script(env);
    match pre {
        Some(pre) => format!("{env}{{\n{pre}\n}} && {{\n{command}\n}}"),
        None => format!("{env}{command}"),
    }
}

/// env と pre_command を設定してから，その環境のままシェルに入れ替えるスクリプト．
/// pre_command が失敗しても，原因を調べられるようにシェルは開く．
fn shell_script(env: &[(String, String)], pre: Option<&str>, shell: &str) -> String {
    let env = env_script(env);
    let pre = pre.map_or(String::new(), |p| format!("{{\n{p}\n}}\n"));
    format!("{env}{pre}exec {}", quote(shell))
}

#[cfg(test)]
mod script_tests {
    use super::*;
    use std::process::Command;

    fn run(script: &str) -> (bool, String) {
        let out = Command::new("sh").args(["-c", script]).output().unwrap();
        (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned())
    }

    #[test]
    fn pre_command_runs_in_same_shell() {
        assert_eq!(program_script(&[], None, "echo hi"), "echo hi");
        // pre_command の変数が command から見える．コメントで終わっても壊れない
        let (ok, out) = run(&program_script(&[], Some("X=from-pre # comment"), "echo $X"));
        assert!(ok);
        assert_eq!(out, "from-pre\n");
        // pre_command が失敗したら command は実行せず，失敗で終わる
        let (ok, out) = run(&program_script(&[], Some("false"), "echo ran"));
        assert!(!ok);
        assert_eq!(out, "");
    }

    #[test]
    fn shell_script_execs_shell() {
        let (ok, out) = run(&shell_script(&[], Some("X=1"), "/bin/echo"));
        assert!(ok);
        assert_eq!(out, "\n");
        assert!(shell_script(&[], Some("x"), "/it's/sh").ends_with(r"exec '/it'\''s/sh'"));
        let env = vec![("X".to_string(), "a b".to_string())];
        assert_eq!(shell_script(&env, None, "/bin/sh"), "export X=\"a b\"\nexec '/bin/sh'");
    }

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn env_is_expanded_in_order() {
        let e = env(&[
            ("IF", "${SHOUJI_TEST_UNSET:-eth0}"),
            ("ADDR", "$(echo 10.0.0.1)"),
            ("URL", "http://$ADDR:1 on $IF"),
            ("IF", "wlan0"),
        ]);
        let out = Command::new("sh").args(["-c", &program_script(&e, None, "echo \"$URL|$IF\"")]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "http://10.0.0.1:1 on eth0|wlan0\n");
        assert!(out.stderr.is_empty());
    }

    #[test]
    fn empty_env_warns() {
        let e = env(&[("IP", "$(true)"), ("PLAIN", ""), ("Q", "it's")]);
        let out = Command::new("sh").args(["-c", &program_script(&e, None, "echo \"ran $Q\"")]).output().unwrap();
        // 警告は出すが，プログラムは起動する
        assert_eq!(String::from_utf8_lossy(&out.stdout), "ran it's\n");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("shouji: warning: IP is empty (env: $(true))"), "{err}");
        assert!(!err.contains("PLAIN"));
    }
}
