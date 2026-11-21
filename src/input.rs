//! 端末からの生バイト列を解釈し，プレフィックスキー（Ctrl-q）のコマンドを取り出す．

use crate::layout::Dir;

pub const PREFIX: u8 = 0x11; // Ctrl-q

#[derive(Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// アクティブなペインへそのまま送る．
    Forward(Vec<u8>),
    Split(Dir),
    FocusNext,
    Focus(Direction),
    Kill,
    Quit,
    NewWindow,
    NextWindow,
    PrevWindow,
    /// 表示中のグループの N 番目のウィンドウ．
    SelectWindow(usize),
    /// 次・前のグループ（起動したタスク）．
    NextGroup,
    PrevGroup,
    OpenLauncher,
    /// スクロールモード（過去の出力を見る）に入る．
    ScrollMode,
    /// マウス操作のオン・オフを切り替える．
    ToggleMouse,
    /// ヘルプ画面を開く．
    Help,
    /// 選択中のペインだけを大きく表示する（もう一度で戻す）．
    Zoom,
    /// 構成のサイドバーの表示を切り替える．
    ToggleSidebar,
    /// 選択中のプログラムの stop を実行する（ペインは閉じない）．
    Stop,
    /// shouji の中で最後にコピーした文字列を貼り付ける．
    Paste,
}

#[derive(Default)]
enum State {
    #[default]
    Normal,
    Prefix,
    /// プレフィックス後の矢印キー（ESC [ A など）を読んでいる途中．
    PrefixEsc(Vec<u8>),
}

#[derive(Default)]
pub struct InputParser {
    state: State,
}

impl InputParser {
    pub fn in_prefix(&self) -> bool {
        !matches!(self.state, State::Normal)
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Action> {
        let mut actions = Vec::new();
        let mut forward = Vec::new();

        for &b in bytes {
            match std::mem::take(&mut self.state) {
                State::Normal if b == PREFIX => {
                    if !forward.is_empty() {
                        actions.push(Action::Forward(std::mem::take(&mut forward)));
                    }
                    self.state = State::Prefix;
                }
                State::Normal => forward.push(b),
                State::Prefix => match b {
                    PREFIX => forward.push(PREFIX),
                    b'%' => actions.push(Action::Split(Dir::Horizontal)),
                    b'"' => actions.push(Action::Split(Dir::Vertical)),
                    b'o' => actions.push(Action::FocusNext),
                    b'q' => actions.push(Action::Kill),
                    b'd' => actions.push(Action::Quit),
                    b'c' => actions.push(Action::NewWindow),
                    b'[' => actions.push(Action::ScrollMode),
                    b']' => actions.push(Action::Paste),
                    b'm' => actions.push(Action::ToggleMouse),
                    b'?' => actions.push(Action::Help),
                    b'z' => actions.push(Action::Zoom),
                    b's' => actions.push(Action::Stop),
                    b't' => actions.push(Action::ToggleSidebar),
                    b'\t' => actions.push(Action::FocusNext),
                    // Space．Ctrl を押したままの C-Space（NUL）も受け付ける
                    b' ' | 0x00 => actions.push(Action::OpenLauncher),
                    b'n' => actions.push(Action::NextWindow),
                    b'p' => actions.push(Action::PrevWindow),
                    b'g' => actions.push(Action::NextGroup),
                    b'G' => actions.push(Action::PrevGroup),
                    // ウィンドウ番号は 1 から（0 は 10 番目）
                    b'1'..=b'9' => actions.push(Action::SelectWindow((b - b'1') as usize)),
                    b'0' => actions.push(Action::SelectWindow(9)),
                    b'h' => actions.push(Action::Focus(Direction::Left)),
                    b'j' => actions.push(Action::Focus(Direction::Down)),
                    b'k' => actions.push(Action::Focus(Direction::Up)),
                    b'l' => actions.push(Action::Focus(Direction::Right)),
                    0x1b => self.state = State::PrefixEsc(Vec::new()),
                    _ => {} // 未定義のキーは捨てる
                },
                State::PrefixEsc(mut seq) => {
                    seq.push(b);
                    match seq.as_slice() {
                        [b'[' | b'O'] => self.state = State::PrefixEsc(seq),
                        [b'[' | b'O', .., last] if (0x40..=0x7e).contains(last) => {
                            let dir = match last {
                                b'A' => Some(Direction::Up),
                                b'B' => Some(Direction::Down),
                                b'C' => Some(Direction::Right),
                                b'D' => Some(Direction::Left),
                                _ => None,
                            };
                            actions.extend(dir.map(Action::Focus));
                        }
                        [b'[' | b'O', ..] => self.state = State::PrefixEsc(seq),
                        _ => {}
                    }
                }
            }
        }
        if !forward.is_empty() {
            actions.push(Action::Forward(forward));
        }
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_and_commands() {
        let mut p = InputParser::default();
        assert_eq!(
            p.feed(b"ls\x11%pwd"),
            vec![
                Action::Forward(b"ls".to_vec()),
                Action::Split(Dir::Horizontal),
                Action::Forward(b"pwd".to_vec()),
            ]
        );
        assert_eq!(p.feed(b"\x11\x11"), vec![Action::Forward(vec![PREFIX])]);
        assert_eq!(p.feed(b"a\x07"), vec![Action::Forward(b"a\x07".to_vec())]);
        assert_eq!(p.feed(b"\x11z\x11t"), vec![Action::Zoom, Action::ToggleSidebar]);
        assert_eq!(p.feed(b"\x111\x110"), vec![Action::SelectWindow(0), Action::SelectWindow(9)]);
        assert_eq!(p.feed(b"\x11[\x11]"), vec![Action::ScrollMode, Action::Paste]);
    }

    #[test]
    fn prefix_split_across_reads() {
        let mut p = InputParser::default();
        assert_eq!(p.feed(b"\x11"), vec![]);
        assert!(p.in_prefix());
        assert_eq!(p.feed(b"\x1b["), vec![]);
        assert_eq!(p.feed(b"D"), vec![Action::Focus(Direction::Left)]);
        assert!(!p.in_prefix());
    }
}
