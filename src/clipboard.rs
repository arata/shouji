//! 外側の端末のクリップボードへ書き込む（OSC 52）．
//!
//! サーバーは端末を持たないので，コピーした文字列はクライアントへ送り，クライアントが
//! 自分の端末へ OSC 52 を書く（session.rs の CLIPBOARD）．SSH 越しでも届く．
//! 対応している端末: kitty, WezTerm, iTerm2, Windows Terminal, foot, Alacritty, xterm（設定が必要）など．
//!
//! GNOME Terminal など OSC 52 を無視する端末のために，クライアントはデスクトップの
//! クリップボードのコマンド（wl-copy・xclip・xsel）があれば，それにも渡す．
//! tmux の中では `tmux load-buffer -w -` にも渡す．tmux が接続中の端末へ OSC 52 を送るので，
//! 素通し（DCS）が通らない設定や，ssh 越しの tmux でも手元のクリップボードに届く．

use std::io::Write;
use std::process::{Command, Stdio};

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// クリップボードに `text` を入れる OSC 52．`tmux` が true なら tmux の中で動いているので，
/// tmux を素通りさせる（tmux 側で `allow-passthrough on` が必要）．
pub fn osc52(text: &str, tmux: bool) -> Vec<u8> {
    let seq = format!("\x1b]52;c;{}\x07", base64(text.as_bytes()));
    if tmux {
        // DCS の中の ESC は 2 つ重ねる
        format!("\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b")).into_bytes()
    } else {
        seq.into_bytes()
    }
}

/// 試すクリップボードのコマンド（先に見つかったものを使う）．
/// `SHOUJI_COPY_COMMAND` があれば，それだけを `sh -c` で実行する（標準入力に文字列を渡す）．
fn commands(custom: Option<String>, wayland: bool, x11: bool) -> Vec<(String, Vec<String>)> {
    let owned = |c: &str, a: &[&str]| (c.to_string(), a.iter().map(|s| s.to_string()).collect());
    if let Some(c) = custom.filter(|c| !c.trim().is_empty()) {
        return vec![owned("sh", &["-c", &c])];
    }
    let mut out = Vec::new();
    if wayland {
        out.push(owned("wl-copy", &[]));
    }
    if x11 {
        out.push(owned("xclip", &["-selection", "clipboard", "-in"]));
        out.push(owned("xsel", &["--clipboard", "--input"]));
    }
    out
}

/// デスクトップのクリップボードのコマンドに `text` を渡す（裏のスレッドで実行する）．
/// 端末（クライアント）側で呼ぶ．ここで使う DISPLAY などはその端末のもの．
pub fn copy_with_command(text: String) {
    let present = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
    let custom = std::env::var("SHOUJI_COPY_COMMAND").ok().filter(|c| !c.trim().is_empty());
    // tmux のバッファへ（-w: tmux から端末のクリップボードへも送る）．指定のコマンドがあればそれだけ使う
    let tmux = custom.is_none() && present("TMUX");
    let cmds = commands(custom, present("WAYLAND_DISPLAY"), present("DISPLAY"));
    if cmds.is_empty() && !tmux {
        return;
    }
    std::thread::spawn(move || {
        // tmux の手前の端末が OSC 52 を無視するときのために，デスクトップのコマンドにも渡す
        if tmux {
            run(&("tmux".into(), vec!["load-buffer".into(), "-w".into(), "-".into()]), &text);
        }
        for cmd in &cmds {
            if run(cmd, &text) {
                return;
            }
        }
    });
}

/// `cmd` の標準入力に `text` を渡して実行する．成功したか（入っていなければ false）．
fn run((cmd, args): &(String, Vec<String>), text: &str) -> bool {
    let child = Command::new(cmd).args(args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    let Ok(mut child) = child else { return false };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    // xclip などは中身を渡すと裏に回って終わる
    child.wait().is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64("あ".as_bytes()), "44GC");
        assert_eq!(osc52("hi", false), b"\x1b]52;c;aGk=\x07");
        assert_eq!(osc52("hi", true), b"\x1bPtmux;\x1b\x1b]52;c;aGk=\x07\x1b\\");
    }

    #[test]
    fn picks_commands() {
        let names = |v: Vec<(String, Vec<String>)>| v.into_iter().map(|(c, _)| c).collect::<Vec<_>>();
        assert_eq!(names(commands(None, false, true)), ["xclip", "xsel"]);
        assert_eq!(names(commands(None, true, true)), ["wl-copy", "xclip", "xsel"]);
        assert!(commands(None, false, false).is_empty());
        assert_eq!(commands(Some("pbcopy".into()), true, true), vec![("sh".into(), vec!["-c".into(), "pbcopy".into()])]);
    }
}
