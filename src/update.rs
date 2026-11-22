//! 新しいバージョンが出ていないかを GitHub Releases で確かめる．
//!
//! `https://github.com/arata/shouji/releases/latest` は最新のタグ
//! （`.../releases/tag/v0.1.4`）へリダイレクトするので，その飛び先からバージョンを読む．
//! HTTP のライブラリは入れず，install.sh と同じく curl か wget を使う（どちらもなければ何もしない）．
//! 環境変数 `SHOUJI_NO_UPDATE_CHECK` が空でなければ確かめない．

use std::process::{Command, Stdio};

const LATEST_URL: &str = "https://github.com/arata/shouji/releases/latest";
const INSTALL_URL: &str = "https://raw.githubusercontent.com/arata/shouji/master/install.sh";
/// 応答を待つ時間（秒）．ネットワークがなくても長く待たない．
const TIMEOUT_SECS: &str = "5";

/// 今のバージョンより新しいものが出ていれば，ステータスバーに出す文を返す．
pub fn check() -> Option<String> {
    if std::env::var_os("SHOUJI_NO_UPDATE_CHECK").is_some_and(|v| !v.is_empty()) {
        return None;
    }
    let latest = latest_tag()?;
    let current = env!("CARGO_PKG_VERSION");
    is_newer(&latest, current).then(|| {
        format!("shouji {latest} is available (now v{current}). Update: curl -fsSL {INSTALL_URL} | sh")
    })
}

/// 最新リリースのタグ（例: "v0.1.4"）．
fn latest_tag() -> Option<String> {
    let location = redirect_with_curl().or_else(redirect_with_wget)?;
    let tag = location.trim().rsplit_once("/tag/")?.1.trim().to_string();
    (!tag.is_empty()).then_some(tag)
}

fn run(cmd: &mut Command) -> Option<std::process::Output> {
    cmd.stdin(Stdio::null()).output().ok()
}

fn redirect_with_curl() -> Option<String> {
    let out = run(Command::new("curl").args(["-sS", "-o", "/dev/null", "--max-time", TIMEOUT_SECS, "-w", "%{redirect_url}", LATEST_URL]))?;
    let url = String::from_utf8_lossy(&out.stdout).into_owned();
    (!url.trim().is_empty()).then_some(url)
}

fn redirect_with_wget() -> Option<String> {
    // リダイレクトをたどらず，応答ヘッダー（標準エラーに出る）の Location を読む
    let out = run(Command::new("wget").args(["-q", "-S", "--spider", "--max-redirect=0", "-T", TIMEOUT_SECS, "-t", "1", LATEST_URL]))?;
    let text = String::from_utf8_lossy(&out.stderr).into_owned();
    text.lines().find_map(|l| {
        let l = l.trim();
        let (key, value) = l.split_once(':')?;
        key.eq_ignore_ascii_case("location").then(|| value.trim().to_string())
    })
}

/// "v0.1.10" → [0, 1, 10]．数字でない部分（"-rc1" など）以降は無視する．
fn parse(v: &str) -> Vec<u64> {
    v.trim().trim_start_matches('v').split('.').map_while(|p| {
        let digits: String = p.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    }).collect()
}

/// `latest` が `current` より新しいか．
fn is_newer(latest: &str, current: &str) -> bool {
    let (a, b) = (parse(latest), parse(current));
    !a.is_empty() && a > b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(is_newer("v0.1.4", "0.1.3"));
        assert!(is_newer("v0.2.0", "0.1.10"));
        assert!(is_newer("v0.1.10", "0.1.9"));
        assert!(!is_newer("v0.1.3", "0.1.3"));
        assert!(!is_newer("v0.1.2", "0.1.3"));
        assert!(!is_newer("garbage", "0.1.3"));
    }
}
