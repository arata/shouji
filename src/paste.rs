//! 貼り付け（ブラケットペースト: `ESC [200~ … ESC [201~`）をキー入力から切り分ける．
//!
//! 貼り付けた文字列は，プレフィックスキー（Ctrl-q）やマウス，スクロールモードの
//! キーとして解釈せずに，そのままペインへ送る．

const START: &[u8] = b"\x1b[200~";
const END: &[u8] = b"\x1b[201~";
/// 終わりの印が届かないときに溜めておく上限．超えたらそこまでを貼り付けとして渡す．
const MAX: usize = 16 << 20;

#[derive(Debug, PartialEq, Eq)]
pub enum Chunk {
    Keys(Vec<u8>),
    /// 貼り付けられた中身（印は除く）．
    Paste(Vec<u8>),
}

#[derive(Default)]
pub struct Splitter {
    /// 貼り付けの途中か（始まりの印を読んだ）．
    in_paste: bool,
    /// まだ判断できないバイト（印の途中かもしれない末尾）と，貼り付けの中身．
    buf: Vec<u8>,
}

/// `buf` の末尾が `marker` の先頭の一部（全部ではない）になっている長さ．
fn partial_suffix(buf: &[u8], marker: &[u8]) -> usize {
    (1..marker.len()).rev().find(|&n| buf.ends_with(&marker[..n])).unwrap_or(0)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

impl Splitter {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Chunk> {
        let mut out = Vec::new();
        let mut buf = std::mem::take(&mut self.buf);
        buf.extend_from_slice(bytes);
        loop {
            if self.in_paste {
                match find(&buf, END) {
                    Some(i) => {
                        out.push(Chunk::Paste(buf[..i].to_vec()));
                        buf.drain(..i + END.len());
                        self.in_paste = false;
                    }
                    None if buf.len() > MAX => {
                        out.push(Chunk::Paste(std::mem::take(&mut buf)));
                        break;
                    }
                    None => break,
                }
            } else {
                match find(&buf, START) {
                    Some(i) => {
                        if i > 0 {
                            out.push(Chunk::Keys(buf[..i].to_vec()));
                        }
                        buf.drain(..i + START.len());
                        self.in_paste = true;
                    }
                    None => {
                        // 印の途中で切れているかもしれない末尾は次の入力まで持ち越す．
                        // ただし ESC 1 つだけなら Esc キーなので，それだけの入力は待たずに渡す
                        let mut keep = partial_suffix(&buf, START);
                        if keep == buf.len() && keep == 1 {
                            keep = 0;
                        }
                        let keys: Vec<u8> = buf.drain(..buf.len() - keep).collect();
                        if !keys.is_empty() {
                            out.push(Chunk::Keys(keys));
                        }
                        break;
                    }
                }
            }
        }
        self.buf = buf;
        out
    }
}

/// ペインへ送る形にする．ペインのプログラムがブラケットペーストを使っていれば印で囲む．
/// 中身に終わりの印があると，そこで貼り付けが終わったと誤解されるので取り除く．
pub fn wrap(text: &[u8], bracketed: bool) -> Vec<u8> {
    let mut body = text.to_vec();
    while let Some(i) = find(&body, END) {
        body.drain(i..i + END.len());
    }
    if !bracketed {
        return body;
    }
    let mut out = Vec::with_capacity(body.len() + START.len() + END.len());
    out.extend_from_slice(START);
    out.extend_from_slice(&body);
    out.extend_from_slice(END);
    out
}

/// shouji の中でコピーした文字列を，端末から貼り付けたときと同じ形にする（改行は CR）．
pub fn from_text(text: &str) -> Vec<u8> {
    text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use Chunk::*;

    #[test]
    fn splits_paste() {
        let mut s = Splitter::default();
        assert_eq!(
            s.feed(b"ab\x1b[200~x\x11y\x1b[<0;1;1M\x1b[201~cd"),
            vec![Keys(b"ab".to_vec()), Paste(b"x\x11y\x1b[<0;1;1M".to_vec()), Keys(b"cd".to_vec())]
        );
    }

    #[test]
    fn markers_split_across_reads() {
        let mut s = Splitter::default();
        assert_eq!(s.feed(b"a\x1b[20"), vec![Keys(b"a".to_vec())]);
        assert_eq!(s.feed(b"0~hel"), vec![]);
        assert_eq!(s.feed(b"lo\x1b[2"), vec![]);
        assert_eq!(s.feed(b"01~"), vec![Paste(b"hello".to_vec())]);
    }

    #[test]
    fn keys_pass_through() {
        let mut s = Splitter::default();
        assert_eq!(s.feed(b"\x1b"), vec![Keys(b"\x1b".to_vec())]);
        assert_eq!(s.feed(b"\x1b[A"), vec![Keys(b"\x1b[A".to_vec())]);
        assert_eq!(s.feed(b"\x1b[2~"), vec![Keys(b"\x1b[2~".to_vec())]); // Insert キー
    }

    #[test]
    fn wraps() {
        assert_eq!(wrap(b"a\x1b[201~b", false), b"ab");
        assert_eq!(wrap(b"a", true), b"\x1b[200~a\x1b[201~");
        assert_eq!(from_text("a\nb\r\nc"), b"a\rb\rc");
    }
}
