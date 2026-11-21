//! セッション（tmux の attach / detach のような仕組み）．
//!
//! shouji はサーバーとクライアントに分かれて動く．
//!
//! - サーバー: 端末から切り離して裏で動き続けるプロセス．ペイン・PTY・タスクを持ち，画面を描画する．
//!   セッションごとに 1 つで，Unix ソケット（`$XDG_RUNTIME_DIR/shouji/NAME.sock`）で待ち受ける．
//! - クライアント: 端末を raw モードにし，キー入力と画面サイズをサーバーへ送り，
//!   サーバーが描画したバイト列をそのまま端末に書く．detach するとクライアントだけが終わる．
//!
//! 通信は [種類 1 バイト][長さ 4 バイト][中身] の繰り返し．

use std::io::{self, Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

// クライアント → サーバー
/// 接続して画面を受け取る（中身: 幅・高さ 各 2 バイト）．
pub const HELLO: u8 = 1;
/// キー入力（中身: 生バイト列）．
pub const INPUT: u8 = 2;
/// 端末のサイズが変わった（中身: 幅・高さ）．
pub const RESIZE: u8 = 3;
/// セッションの様子を尋ねる（返事は INFO_REPLY）．
pub const INFO: u8 = 4;
/// セッションを終わらせる（全プログラムを止める）．
pub const KILL: u8 = 5;

// サーバー → クライアント
/// 描画したバイト列．
pub const OUTPUT: u8 = 10;
/// detach した（または別の端末が接続した）．
pub const DETACHED: u8 = 11;
/// セッションが終わった．
pub const EXIT: u8 = 12;
/// INFO への返事（中身: 1 行の説明）．
pub const INFO_REPLY: u8 = 13;
/// クリップボードに入れる文字列（中身: UTF-8）．クライアントが自分の端末へ OSC 52 で書く．
pub const CLIPBOARD: u8 = 14;

pub fn write_frame(w: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    let mut buf = Vec::with_capacity(payload.len() + 5);
    buf.push(kind);
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(payload);
    w.write_all(&buf)?;
    w.flush()
}

/// 1 つ読む．相手が閉じたら `None`．
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut head = [0u8; 5];
    match r.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(Some((head[0], payload)))
}

pub fn size_payload(w: u16, h: u16) -> Vec<u8> {
    [w.to_be_bytes(), h.to_be_bytes()].concat()
}

pub fn parse_size(p: &[u8]) -> Option<(u16, u16)> {
    match p {
        [a, b, c, d] => Some((u16::from_be_bytes([*a, *b]), u16::from_be_bytes([*c, *d]))),
        _ => None,
    }
}

/// ソケットを置くディレクトリ（本人だけが読み書きできる）．
pub fn dir() -> Result<PathBuf> {
    let dir = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d).join("shouji"),
        _ => PathBuf::from(format!("/tmp/shouji-{}", unsafe { libc::getuid() })),
    };
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    Ok(dir)
}

pub fn socket_path(name: &str) -> Result<PathBuf> {
    Ok(dir()?.join(format!("{name}.sock")))
}

pub fn log_path(name: &str) -> Result<PathBuf> {
    Ok(dir()?.join(format!("{name}.log")))
}

/// セッション名に使えない文字を _ にする．
pub fn sanitize(name: &str) -> String {
    let s: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' }).collect();
    if s.is_empty() { "main".into() } else { s }
}

/// セッションに接続する．サーバーがいなければ（古いソケットは消して）`None`．
pub fn connect(name: &str) -> Result<Option<UnixStream>> {
    let path = socket_path(name)?;
    match UnixStream::connect(&path) {
        Ok(s) => Ok(Some(s)),
        Err(_) => {
            // サーバーが落ちて残ったソケット
            let _ = std::fs::remove_file(&path);
            Ok(None)
        }
    }
}

/// 動いているセッションの名前の一覧．
pub fn sessions() -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir()?)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "sock")
            && let Some(name) = path.file_stem().map(|s| s.to_string_lossy().into_owned())
            && connect(&name)?.is_some()
        {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// 名前を省略したときのセッション（1 つだけ動いていればそれ）．
pub fn pick(name: Option<&str>) -> Result<String> {
    if let Some(n) = name {
        return Ok(n.to_string());
    }
    match sessions()?.as_slice() {
        [] => bail!("no shouji session is running"),
        [one] => Ok(one.clone()),
        many => bail!("several sessions are running, name one: {}", many.join(", ")),
    }
}

/// セッションの様子（1 行）．
pub fn info(name: &str) -> Result<String> {
    let Some(mut s) = connect(name)? else { bail!("no session named {name}") };
    write_frame(&mut s, INFO, &[])?;
    s.set_read_timeout(Some(Duration::from_secs(2)))?;
    // 終了処理（stop の実行）の途中などで返事がなければ busy
    match read_frame(&mut s) {
        Ok(Some((INFO_REPLY, text))) => Ok(String::from_utf8_lossy(&text).into_owned()),
        _ => Ok("(busy: maybe stopping)".into()),
    }
}

pub fn kill(name: &str) -> Result<()> {
    let Some(mut s) = connect(name)? else { bail!("no session named {name}") };
    write_frame(&mut s, KILL, &[])?;
    Ok(())
}

/// サーバーを裏で起動し，ソケットができるまで待つ．`args` はサーバーに渡す引数．
pub fn spawn_server(name: &str, args: &[String]) -> Result<()> {
    let exe = std::env::current_exe()?;
    let log = log_path(name)?;
    let log_file = std::fs::File::create(&log)?;
    let mut cmd = Command::new(exe);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(log_file);
    // 端末から切り離す（端末を閉じてもサーバーが止まらないように）
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("cannot start the shouji server")?;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if socket_path(name)?.exists() && UnixStream::connect(socket_path(name)?).is_ok() {
            // 子プロセスは回収しないまま放す（サーバーとして動き続ける）
            thread::spawn(move || child.wait());
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            bail!("the shouji server stopped ({status}):\n{}", text.trim());
        }
        if Instant::now() > deadline {
            bail!("the shouji server did not start (see {})", log.display());
        }
        thread::sleep(Duration::from_millis(30));
    }
}

/// 接続が終わった理由．
pub enum End {
    Detached,
    Exited,
    Lost,
}

/// セッションに接続し，detach かセッションの終了まで画面を中継する．
pub fn attach(name: &str) -> Result<End> {
    let Some(stream) = connect(name)? else { bail!("no session named {name}") };
    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    let mut reader = stream;

    let _guard = crate::TerminalGuard::new()?;
    let (w, h) = crossterm::terminal::size()?;
    write_frame(&mut *writer.lock().unwrap(), HELLO, &size_payload(w, h))?;

    // キー入力 → サーバー
    {
        let writer = Arc::clone(&writer);
        thread::spawn(move || {
            let mut stdin = io::stdin();
            let mut buf = [0u8; 4096];
            while let Ok(n) = stdin.read(&mut buf) {
                if n == 0 || write_frame(&mut *writer.lock().unwrap(), INPUT, &buf[..n]).is_err() {
                    break;
                }
            }
        });
    }
    // 端末のサイズ変更 → サーバー
    {
        let writer = Arc::clone(&writer);
        let mut signals = signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH])?;
        thread::spawn(move || {
            for _ in signals.forever() {
                let Ok((w, h)) = crossterm::terminal::size() else { continue };
                if write_frame(&mut *writer.lock().unwrap(), RESIZE, &size_payload(w, h)).is_err() {
                    break;
                }
            }
        });
    }

    // サーバーの描画 → 端末
    let mut out = io::stdout();
    let in_tmux = std::env::var_os("TMUX").is_some();
    loop {
        match read_frame(&mut reader) {
            Ok(Some((OUTPUT, bytes))) => {
                out.write_all(&bytes)?;
                out.flush()?;
            }
            Ok(Some((CLIPBOARD, bytes))) => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                out.write_all(&crate::clipboard::osc52(&text, in_tmux))?;
                out.flush()?;
                // OSC 52 を無視する端末（GNOME Terminal など）のため，xclip などにも渡す
                crate::clipboard::copy_with_command(text);
            }
            Ok(Some((DETACHED, _))) => return Ok(End::Detached),
            Ok(Some((EXIT, _))) => return Ok(End::Exited),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => return Ok(End::Lost),
        }
    }
}

/// セッション名の既定値: 最初のタスクファイルの名前（なければ main）．
pub fn default_name(first_task: Option<&Path>) -> String {
    first_task.and_then(|p| p.file_stem()).map_or("main".into(), |s| sanitize(&s.to_string_lossy()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, INPUT, b"abc").unwrap();
        write_frame(&mut buf, RESIZE, &size_payload(120, 40)).unwrap();
        let mut r = &buf[..];
        assert_eq!(read_frame(&mut r).unwrap(), Some((INPUT, b"abc".to_vec())));
        let (kind, p) = read_frame(&mut r).unwrap().unwrap();
        assert_eq!((kind, parse_size(&p)), (RESIZE, Some((120, 40))));
        assert_eq!(read_frame(&mut r).unwrap(), None);
    }

    #[test]
    fn names() {
        assert_eq!(sanitize("my app/1"), "my_app_1");
        assert_eq!(sanitize(""), "main");
        assert_eq!(default_name(Some(Path::new("sample_config/ros.yaml"))), "ros");
        assert_eq!(default_name(None), "main");
    }
}
