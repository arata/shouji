//! 設定ファイルの読み込み．
//!
//! `shouji.yaml` にはタスクファイルの一覧を書く．相対パスは shouji.yaml のある
//! ディレクトリから解決する．
//!
//! ```yaml
//! env:                          # すべてのタスクに付ける環境変数（pre_command も書ける）
//!   ROS_MASTER_URI: ${ROS_MASTER_URI:-http://localhost:11311}
//! tasks:
//!   - task.yaml                 # パスだけでもよい（名前はファイル名から）
//!   - path: ~/work/api/task.yaml
//!     name: api
//! ```
//!
//! `-t` で開いたタスクファイルは，同じディレクトリに shouji.yaml があれば，その env と
//! pre_command を使う（一覧に載っていればその shouji.yaml のもの）．
//!
//! env の値はシェルのダブルクォートの中と同じように展開する（`${VAR:-既定値}` や `$(コマンド)`）．
//! shouji.yaml → タスク → ウィンドウ → プログラム の順に，上から書いた順で export するので，
//! 後に書いた値から前の変数を使える．
//!
//! タスクファイル（例: `task.yaml`）には，一緒に起動するプログラムを書く．
//! 選ぶと新しいウィンドウを開き，各プログラムを分割したペインで起動する．
//!
//! ```yaml
//! name: dev                     # 省略時は shouji.yaml 側の名前
//! programs:
//!   - name: server
//!     command: cargo run
//!     cwd: ./server             # 相対パスはタスクファイルのディレクトリから
//!     env:
//!       RUST_LOG: debug
//!     pre_command: . ./env.sh   # command の前に同じシェルで実行（タスク・ウィンドウにも書ける）
//!     keep_on_exit: true        # 終了してもペインを残す（省略時 true）
//!     autostart: true           # false ならシェルにコマンドを入力した状態で待つ（Enter で実行）
//!   - name: client
//!     command: ./client
//!     after:                    # 起動を待つ相手（同じタスク内のプログラム名）
//!       - server: started       #   起動したら
//!       - build: done           #   正常終了（終了コード 0）したら
//!     delay: 2                  # さらに 2 秒待ってから起動
//!   - name: top
//!     command: top
//!   - name: viewer
//!     if_env: { VIEWER: "1" }   # 環境変数がこの値のときだけ起動する（合わなければペインを作らない）
//!     command: ./viewer
//!   - name: planner
//!     commands:                 # 上から見て，最初に if_env が合ったものを実行する
//!       - if_env: { PLANNER: [fast, "quick*"] }
//!         command: ./planner --fast
//!       - command: ./planner    # if_env のない項目は「それ以外」
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// `~` を展開し，相対パスなら `base` からのパスにする．
fn resolve(path: &str, base: &Path) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let p = match (path.strip_prefix('~'), home) {
        (Some(rest), Some(home)) => home.join(rest.trim_start_matches('/')),
        _ => PathBuf::from(path),
    };
    if p.is_absolute() { p } else { base.join(p) }
}

fn read_yaml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    serde_yaml_ng::from_str(&text).with_context(|| format!("invalid format in {}", path.display()))
}

// ---- shouji.yaml ----

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    /// すべてのタスクに付ける環境変数．
    #[serde(default, deserialize_with = "ordered_env")]
    env: Env,
    /// すべてのタスクのプログラムの command の前に実行するコマンド．
    pre_command: Option<String>,
    #[serde(default)]
    tasks: Vec<RawTaskEntry>,
}

/// 環境変数（名前, 値）．値はシェルで展開する．YAML に書いた順に並べる（後の値から前の変数を使える）．
pub type Env = Vec<(String, String)>;

/// `env:` のマップを，書いた順のまま読む．
fn ordered_env<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Env, D::Error> {
    struct V;
    impl<'de> serde::de::Visitor<'de> for V {
        type Value = Env;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("env must be a map of NAME: value")
        }
        fn visit_unit<E>(self) -> Result<Env, E> {
            Ok(Vec::new())
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(self, mut m: A) -> Result<Env, A::Error> {
            let mut out = Vec::new();
            while let Some((k, v)) = m.next_entry::<String, String>()? {
                out.push((k, v));
            }
            Ok(out)
        }
    }
    d.deserialize_any(V)
}

/// `if_env` の条件: 変数名と，値のパターンの並び．書いた変数のすべてが，それぞれのパターンの
/// どれかに合えば成り立つ．パターンは `*`（0 文字以上）と `?`（1 文字）が使える．
pub type IfEnv = Vec<(String, Vec<String>)>;

/// `if_env:` のマップを読む．値は 1 つのパターンか，パターンの並び．
/// `RVIZ: 1` のような数値や真偽値も，文字列として読む．
fn if_env<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<IfEnv>, D::Error> {
    use serde::de::Error;
    use serde_yaml_ng::Value;
    const EXPECTING: &str = "if_env must be a map of NAME: value (or NAME: [value, ...])";
    fn text(v: Value) -> Option<String> {
        match v {
            Value::String(s) => Some(s),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    }
    struct V;
    impl<'de> serde::de::Visitor<'de> for V {
        type Value = Option<IfEnv>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str(EXPECTING)
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(self, mut m: A) -> Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while let Some((k, v)) = m.next_entry::<String, Value>()? {
                let patterns = match v {
                    Value::Sequence(items) if !items.is_empty() => items.into_iter().map(text).collect::<Option<Vec<_>>>(),
                    v => text(v).map(|p| vec![p]),
                };
                out.push((k, patterns.ok_or_else(|| A::Error::custom(EXPECTING))?));
            }
            Ok(Some(out))
        }
    }
    d.deserialize_any(V)
}

/// `pattern`（`*` と `?` が使える）が `text` の全体に合うか．
fn glob(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut i, mut j) = (0, 0);
    // 最後に見た `*` の位置と，そのとき text のどこまで進んでいたか（合わなければここに戻る）
    let mut star: Option<(usize, usize)> = None;
    while j < t.len() {
        if i < p.len() && (p[i] == '?' || p[i] == t[j]) && p[i] != '*' {
            (i, j) = (i + 1, j + 1);
        } else if i < p.len() && p[i] == '*' {
            star = Some((i, j));
            i += 1;
        } else if let Some((si, sj)) = star {
            (i, j) = (si + 1, sj + 1);
            star = Some((si, sj + 1));
        } else {
            return false;
        }
    }
    p[i..].iter().all(|&c| c == '*')
}

/// 条件が成り立つか．`value` は変数の値を返す（設定されていなければ空文字列）．
fn holds(cond: &IfEnv, value: impl Fn(&str) -> String) -> bool {
    cond.iter().all(|(name, patterns)| {
        let v = value(name);
        patterns.iter().any(|p| glob(p, &v))
    })
}

/// 条件を 1 行で表す（`RVIZ=1 MODE=a|b`）．
fn describe(cond: &IfEnv) -> String {
    cond.iter().map(|(k, ps)| format!("{k}={}", ps.join("|"))).collect::<Vec<_>>().join(" ")
}

/// `env` を書いた順に設定したとき，`names` の変数がどんな値になるかをシェルで確かめる．
/// env の値はシェルが展開する（`${VAR:-x}` や `$(cmd)`）ので，ペインと同じく `$SHELL -c` で調べる．
/// 設定されていない変数は空文字列になる．
fn env_values(env: &Env, cwd: Option<&Path>, names: &[String]) -> Result<Vec<String>> {
    let mut script = String::new();
    for (k, v) in env {
        script.push_str(&format!("export {k}=\"{v}\"\n"));
    }
    script.push_str("printf '%s\\0'");
    for name in names {
        script.push_str(&format!(" \"${{{name}-}}\""));
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let mut cmd = std::process::Command::new(&shell);
    cmd.args(["-c", &script]).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    let out = cmd.output().with_context(|| format!("cannot run {shell} to read the environment for if_env"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut values: Vec<String> = text.split('\0').map(String::from).collect();
    values.pop(); // 最後の区切りの後ろ
    if !out.status.success() || values.len() != names.len() {
        bail!("cannot read the environment for if_env ({shell} failed while setting env)");
    }
    Ok(values)
}

/// shouji.yaml に書いた，すべてのタスクに共通の設定．
#[derive(Debug, Default)]
struct Shared {
    env: Env,
    pre_command: Option<String>,
}

impl Shared {
    fn load(path: &Path) -> Result<Self> {
        let raw: RawConfig = read_yaml(path)?;
        Ok(Self { env: raw.env, pre_command: raw.pre_command })
    }
}

#[derive(Debug, Deserialize)]
#[serde(
    untagged,
    expecting = "a task must be a path string, or an entry with path and (optional) name"
)]
enum RawTaskEntry {
    Path(String),
    Full(RawTaskFull),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTaskFull {
    path: String,
    name: Option<String>,
}

#[derive(Debug, Default)]
pub struct Config {
    pub tasks: Vec<TaskEntry>,
}

/// shouji.yaml に書かれたタスクファイルへの参照．
#[derive(Debug, Clone)]
pub struct TaskEntry {
    pub name: String,
    /// 解決済みの絶対パス（または起動ディレクトリからのパス）．
    pub path: PathBuf,
    /// 共通の env・pre_command を読む shouji.yaml（タスクを開くたびに読み直す）．
    pub shared: Option<PathBuf>,
}

impl Config {
    fn from_raw(raw: RawConfig, base: &Path, file: Option<&Path>) -> Self {
        let tasks = raw
            .tasks
            .into_iter()
            .map(|e| {
                let (path, name) = match e {
                    RawTaskEntry::Path(p) => (p, None),
                    RawTaskEntry::Full(RawTaskFull { path, name }) => (path, name),
                };
                TaskEntry { shared: file.map(Path::to_path_buf), ..TaskEntry::new(&path, name, base) }
            })
            .collect();
        Self { tasks }
    }

    /// タスクを一覧に加え，その添字を返す．同じファイルがすでにあればその添字を返す．
    pub fn add_task(&mut self, entry: TaskEntry) -> usize {
        let key = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let new_key = key(&entry.path);
        if let Some(i) = self.tasks.iter().position(|t| key(&t.path) == new_key) {
            return i;
        }
        self.tasks.push(entry);
        self.tasks.len() - 1
    }

    fn load_file(path: &Path) -> Result<Self> {
        let raw = read_yaml(path)?;
        let base = path.parent().map(Path::to_path_buf).unwrap_or_default();
        Ok(Self::from_raw(raw, &base, Some(path)))
    }

    /// `explicit` が指定されていればそれを，なければ
    /// `./shouji.yaml` → `~/.config/shouji/config.yaml` の順に探す．
    /// どれも無ければ空の設定を返す．
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        if let Some(path) = explicit {
            return Self::load_file(path);
        }
        let mut candidates = vec![PathBuf::from("shouji.yaml")];
        if let Some(home) = std::env::var_os("HOME") {
            candidates.push(PathBuf::from(home).join(".config/shouji/config.yaml"));
        }
        match candidates.iter().find(|p| p.is_file()) {
            Some(path) => Self::load_file(path),
            None => Ok(Self::default()),
        }
    }
}

// ---- task.yaml ----

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTask {
    name: Option<String>,
    /// タスク内のすべてのプログラムに付ける環境変数．
    #[serde(default, deserialize_with = "ordered_env")]
    env: Env,
    /// タスク内のすべてのプログラムの command の前に実行するコマンド．
    pre_command: Option<String>,
    /// 1 つのウィンドウに並べる場合．
    programs: Option<Vec<RawProgram>>,
    /// ウィンドウに分ける場合．
    windows: Option<Vec<RawWindow>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWindow {
    name: String,
    /// このウィンドウのプログラムに付ける環境変数．
    #[serde(default, deserialize_with = "ordered_env")]
    env: Env,
    /// このウィンドウのプログラムの command の前に実行するコマンド．
    pre_command: Option<String>,
    programs: Vec<RawProgram>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProgram {
    name: String,
    /// 実行するコマンド．`commands` とどちらか一方を書く．
    command: Option<String>,
    /// 条件ごとのコマンド．上から見て，最初に if_env が合ったものを実行する．
    commands: Option<Vec<RawChoice>>,
    /// 起動する条件．合わなければペインを作らない．
    #[serde(default, deserialize_with = "if_env")]
    if_env: Option<IfEnv>,
    pre_command: Option<String>,
    cwd: Option<String>,
    #[serde(default, deserialize_with = "ordered_env")]
    env: Env,
    #[serde(default = "default_true")]
    keep_on_exit: bool,
    #[serde(default = "default_true")]
    autostart: bool,
    #[serde(default)]
    after: Vec<RawAfter>,
    delay: Option<f64>,
    restart: Option<RawRestart>,
    log: Option<String>,
    stop: Option<RawStop>,
}

/// `commands` の 1 項目．if_env のない項目は「それ以外」で，最後にだけ書ける．
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawChoice {
    #[serde(default, deserialize_with = "if_env")]
    if_env: Option<IfEnv>,
    command: String,
    /// このコマンドを選んだときの stop（省略時はプログラムの stop）．
    stop: Option<RawStop>,
}

/// `stop: docker compose down` か，入れ子の `stop: { command, timeout }`．
#[derive(Debug, Deserialize)]
#[serde(untagged, expecting = "stop must be a command, or an entry with command and (optional) timeout")]
enum RawStop {
    Command(String),
    Full(RawStopFull),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStopFull {
    command: String,
    timeout: Option<f64>,
}

/// `restart: on-failure` か，入れ子の `restart: { when, delay, max }`．
#[derive(Debug, Deserialize)]
#[serde(
    untagged,
    expecting = "restart must be never / on-failure / always, or an entry with when, delay and max"
)]
enum RawRestart {
    When(Restart),
    Full(RawRestartFull),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRestartFull {
    when: Restart,
    delay: Option<f64>,
    max: Option<u32>,
}

/// 終了したときに自動で再起動するか．
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Restart {
    /// 再起動しない．
    #[default]
    Never,
    /// エラー（終了コード 0 以外，シグナル）で終了したときだけ．
    OnFailure,
    /// どう終わっても．
    Always,
}

/// `after` の 1 項目: `- build` か `- build: done`．
#[derive(Debug, Deserialize)]
#[serde(untagged, expecting = "each after item must be  - NAME  or  - NAME: started / done")]
enum RawAfter {
    Name(String),
    WithCond(BTreeMap<String, Cond>),
}

/// 待つ相手がどうなったら起動するか．
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cond {
    /// 起動したら．
    Started,
    /// 正常終了（終了コード 0）したら．
    Done,
}

fn default_true() -> bool {
    true
}

/// 1 つのタスク = 一緒に起動するプログラムの集まり．
#[derive(Debug, Clone)]
pub struct Task {
    pub name: String,
    /// タスク内の全プログラム（after の添字はこの並び）．
    pub programs: Vec<Program>,
    /// ウィンドウごとの名前と，そこに並べるプログラムの添字．
    pub windows: Vec<TaskWindow>,
}

#[derive(Debug, Clone)]
pub struct TaskWindow {
    pub name: String,
    pub programs: Vec<usize>,
}

#[derive(Debug, Clone)]
pub struct Program {
    pub name: String,
    /// `$SHELL -c` で実行するコマンド．`commands` で書いたプログラムでは，
    /// `Task::apply_conditions` が選んだものに置き換える（それまでは最初の項目のもの）．
    pub command: String,
    /// 起動する条件（空なら条件なし）．
    pub if_env: IfEnv,
    /// `commands` に書いた項目（`command` で書いたプログラムでは空）．
    pub choices: Vec<Choice>,
    /// command の前に同じシェルで実行するコマンド（`source setup.bash` など）．
    /// 失敗したら command は実行しない．シェルを開くとき（autostart: false や s）にも実行する．
    pub pre_command: Option<String>,
    /// 作業ディレクトリ．省略時は shouji を起動したディレクトリ．
    pub cwd: Option<PathBuf>,
    /// 環境変数．shouji.yaml → タスク → ウィンドウ → プログラム の順に，書いた順で並ぶ
    /// （同じ名前があれば後のものが効く）．値はシェルで展開する．
    pub env: Env,
    /// 終了してもペインを残す（ログを読めるように）．省略時は true．
    pub keep_on_exit: bool,
    /// すぐ起動するか．false ならシェルを開き，コマンドを入力した状態で待つ（Enter で実行）．
    pub autostart: bool,
    /// 起動前に待つ相手（同じタスク内のプログラムの添字）と条件．
    pub after: Vec<(usize, Cond)>,
    /// 待つ相手がそろってから，さらに待つ時間．
    pub delay: Duration,
    /// 自動再起動の方針．
    pub restart: Restart,
    /// 再起動までの待ち時間．
    pub restart_delay: Duration,
    /// 自動で再起動する回数の上限（`None` は無制限）．
    pub max_restarts: Option<u32>,
    /// ペインを閉じるときやセッションを終えるときに実行するコマンド（`docker compose down` など）．
    /// 実行してからプログラムが終わるのを `stop_timeout` まで待ち，終わらなければ強制終了する．
    pub stop: Option<String>,
    pub stop_timeout: Duration,
    /// 出力を追記するログファイル．相対パスは作業ディレクトリ（cwd，なければ shouji を起動した
    /// ディレクトリ）から解決する．
    pub log: Option<PathBuf>,
}

/// `commands` の 1 項目．
#[derive(Debug, Clone)]
pub struct Choice {
    /// 選ぶ条件（空なら「それ以外」）．
    pub if_env: IfEnv,
    pub command: String,
    /// このコマンドを選んだときの stop と，待つ時間（`None` ならプログラムの stop のまま）．
    pub stop: Option<(String, Duration)>,
}

impl Program {
    /// 選択画面に出す 1 行: 条件と command．`commands` なら最初の項目と，残りの数．
    pub fn summary(&self) -> String {
        let line = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut out = String::new();
        if !self.if_env.is_empty() {
            out.push_str(&format!("[if {}] ", describe(&self.if_env)));
        }
        match self.choices.split_first() {
            None => out.push_str(&line(&self.command)),
            Some((first, rest)) => {
                if !first.if_env.is_empty() {
                    out.push_str(&format!("[if {}] ", describe(&first.if_env)));
                }
                out.push_str(&line(&first.command));
                if !rest.is_empty() {
                    out.push_str(&format!(" (+{} more)", rest.len()));
                }
            }
        }
        out
    }

    /// 起動を待つ必要があるか．
    pub fn waits(&self) -> bool {
        !self.after.is_empty() || !self.delay.is_zero()
    }
}

impl TaskEntry {
    /// `path` は `~` 始まり可．相対パスなら `base` から解決する．`name` の省略時はファイル名．
    pub fn new(path: &str, name: Option<String>, base: &Path) -> Self {
        let path = resolve(path, base);
        let name = name.unwrap_or_else(|| {
            path.file_stem().map_or_else(|| "task".into(), |s| s.to_string_lossy().into_owned())
        });
        Self { name, path, shared: None }
    }

    /// `-t` で開くタスク．同じディレクトリに shouji.yaml があれば，その共通設定を使う．
    pub fn standalone(path: &str, base: &Path) -> Self {
        let entry = Self::new(path, None, base);
        let config = entry.path.parent().map(|d| d.join("shouji.yaml")).filter(|p| p.is_file());
        Self { shared: config, ..entry }
    }

    /// タスクファイルを読む（選ばれるたびに読むので，編集はすぐ反映される）．
    pub fn load(&self) -> Result<Task> {
        let raw: RawTask = read_yaml(&self.path)?;
        let base = self.path.parent().map(Path::to_path_buf).unwrap_or_default();
        let shared = match &self.shared {
            Some(path) => Shared::load(path)?,
            None => Shared::default(),
        };
        Task::from_raw(raw, &self.name, &base, &shared).with_context(|| format!("invalid format in {}", self.path.display()))
    }
}

impl Task {
    fn from_raw(raw: RawTask, default_name: &str, base: &Path, shared: &Shared) -> Result<Self> {
        let task_name = raw.name.unwrap_or_else(|| default_name.into());
        // programs だけならタスク名のウィンドウ 1 つにまとめる
        let raw_windows = match (raw.programs, raw.windows) {
            (Some(programs), None) => vec![RawWindow { name: task_name.clone(), env: Vec::new(), pre_command: None, programs }],
            (None, Some(windows)) => windows,
            (Some(_), Some(_)) => bail!("write either programs or windows, not both"),
            (None, None) => bail!("write programs or windows"),
        };
        let mut windows = Vec::with_capacity(raw_windows.len());
        let mut all = Vec::new();
        for w in raw_windows {
            if w.programs.is_empty() {
                bail!("window {} has no programs", w.name);
            }
            let start = all.len();
            // env は shouji.yaml → タスク全体 → ウィンドウ → プログラム の順に並べる
            // （この順に export するので，狭い領域の値が優先され，外側の値を使って書ける）
            all.extend(w.programs.into_iter().map(|mut p| {
                let mut env = shared.env.clone();
                env.extend(raw.env.iter().cloned());
                env.extend(w.env.iter().cloned());
                env.append(&mut p.env);
                p.env = env;
                // pre_command は狭い領域のものが丸ごと優先される（空文字列なら付けない）
                p.pre_command = p
                    .pre_command
                    .take()
                    .or_else(|| w.pre_command.clone())
                    .or_else(|| raw.pre_command.clone())
                    .or_else(|| shared.pre_command.clone());
                p
            }));
            windows.push(TaskWindow { name: w.name, programs: (start..all.len()).collect() });
        }
        let raw_programs = all;

        let names: Vec<&str> = raw_programs.iter().map(|p| p.name.as_str()).collect();
        let index_of = |me: &str, target: &str| -> Result<usize> {
            let mut found = names.iter().enumerate().filter(|(_, n)| **n == target).map(|(i, _)| i);
            match (found.next(), found.next()) {
                (Some(i), None) if target != me => Ok(i),
                (Some(_), None) => bail!("{me}: a program cannot wait for itself in after"),
                (Some(_), Some(_)) => bail!("{me}: more than one program is named {target} (in after)"),
                (None, _) => bail!("{me}: no program named {target} (in after)"),
            }
        };

        let mut programs = Vec::with_capacity(raw_programs.len());
        for p in &raw_programs {
            if let Some((k, _)) = p.env.iter().find(|(k, _)| !valid_env_name(k)) {
                bail!("{}: {k:?} cannot be an environment variable name (use letters, digits and _)", p.name);
            }
            let mut after = Vec::new();
            for a in &p.after {
                match a {
                    RawAfter::Name(n) => after.push((index_of(&p.name, n)?, Cond::Started)),
                    RawAfter::WithCond(map) => {
                        if map.len() != 1 {
                            bail!("{}: write one program per after item (e.g. - build: done)", p.name);
                        }
                        let (n, cond) = map.iter().next().unwrap();
                        after.push((index_of(&p.name, n)?, *cond));
                    }
                }
            }
            let secs = |key: &str, v: Option<f64>, default: f64| -> Result<Duration> {
                match v.unwrap_or(default) {
                    d if d.is_finite() && d >= 0.0 => Ok(Duration::from_secs_f64(d)),
                    d => bail!("{}: {key} must be a number of seconds, 0 or more ({d})", p.name),
                }
            };
            let delay = secs("delay", p.delay, 0.0)?;
            let (restart, restart_delay, max_restarts) = match &p.restart {
                None => (Restart::Never, None, None),
                Some(RawRestart::When(w)) => (*w, None, None),
                Some(RawRestart::Full(f)) => (f.when, f.delay, f.max),
            };
            let restart_delay = secs("restart.delay", restart_delay, 1.0)?;
            let parse_stop = |stop: &Option<RawStop>| -> Result<(Option<String>, Duration)> {
                let (command, timeout) = match stop {
                    None => (None, None),
                    Some(RawStop::Command(c)) => (Some(c.clone()), None),
                    Some(RawStop::Full(f)) => (Some(f.command.clone()), f.timeout),
                };
                Ok((command.filter(|c| !c.trim().is_empty()), secs("stop.timeout", timeout, 10.0)?))
            };
            let (stop, stop_timeout) = parse_stop(&p.stop)?;
            let check_cond = |cond: &IfEnv| -> Result<()> {
                if cond.is_empty() {
                    bail!("{}: if_env needs at least one NAME: value", p.name);
                }
                match cond.iter().find(|(k, _)| !valid_env_name(k)) {
                    Some((k, _)) => bail!("{}: {k:?} cannot be an environment variable name (in if_env)", p.name),
                    None => Ok(()),
                }
            };
            let if_env = p.if_env.clone().unwrap_or_default();
            if p.if_env.is_some() {
                check_cond(&if_env)?;
            }
            let (command, choices) = match (&p.command, &p.commands) {
                (Some(c), None) => (c.clone(), Vec::new()),
                (None, Some(raw)) => {
                    let mut choices = Vec::with_capacity(raw.len());
                    for (i, c) in raw.iter().enumerate() {
                        match &c.if_env {
                            Some(cond) => check_cond(cond)?,
                            None if i + 1 < raw.len() => {
                                bail!("{}: in commands, only the last item can omit if_env (it is the fallback)", p.name)
                            }
                            None => {}
                        }
                        let stop = match parse_stop(&c.stop)? {
                            (Some(command), timeout) => Some((command, timeout)),
                            (None, _) => None,
                        };
                        choices.push(Choice { if_env: c.if_env.clone().unwrap_or_default(), command: c.command.clone(), stop });
                    }
                    let Some(first) = choices.first() else { bail!("{}: commands has no items", p.name) };
                    (first.command.clone(), choices)
                }
                (Some(_), Some(_)) => bail!("{}: write either command or commands, not both", p.name),
                (None, None) => bail!("{}: write command or commands", p.name),
            };
            // log の相対パスは command と同じく作業ディレクトリから
            let cwd = p.cwd.as_ref().map(|c| resolve(c, base));
            let log_base = cwd.clone().or_else(|| std::env::current_dir().ok()).unwrap_or_default();
            programs.push(Program {
                name: p.name.clone(),
                command,
                if_env,
                choices,
                pre_command: p.pre_command.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from),
                cwd,
                env: p.env.clone(),
                keep_on_exit: p.keep_on_exit,
                autostart: p.autostart,
                after,
                delay,
                restart,
                restart_delay,
                max_restarts,
                stop,
                stop_timeout,
                log: p.log.as_ref().map(|l| resolve(l, &log_base)),
            });
        }
        check_cycle(&programs)?;
        Ok(Self { name: task_name, programs, windows })
    }
}

impl Task {
    /// 名前（ウィンドウ名かプログラム名）で起動するプログラムを絞り込む．
    /// ウィンドウ名ならその中のプログラムすべて，プログラム名ならそれだけ．
    /// 選ばなかったプログラムを待つ after は取り除く（待ち続けないように）．
    pub fn select(&mut self, names: &[String]) -> Result<()> {
        if names.is_empty() {
            return Ok(());
        }
        let mut selected = vec![false; self.programs.len()];
        for name in names {
            let window = self.windows.iter().find(|w| w.name == *name);
            let program = self.programs.iter().position(|p| p.name == *name);
            match (window, program) {
                (Some(w), None) => w.programs.iter().for_each(|&k| selected[k] = true),
                (None, Some(k)) => selected[k] = true,
                (Some(_), Some(_)) => bail!("{name} is both a window and a program name, so it is ambiguous"),
                (None, None) => {
                    let windows: Vec<&str> = self.windows.iter().map(|w| w.name.as_str()).collect();
                    let programs: Vec<&str> = self.programs.iter().map(|p| p.name.as_str()).collect();
                    bail!(
                        "no window or program named {name} (windows: {} / programs: {})",
                        windows.join(", "),
                        programs.join(", ")
                    );
                }
            }
        }
        self.select_mask(&selected);
        Ok(())
    }

    /// プログラムごとに選ぶか（`selected[k]`）で絞り込む．
    /// 選ばなかったプログラムを待つ after は取り除く（待ち続けないように）．
    pub fn select_mask(&mut self, selected: &[bool]) {
        let selected = |k: usize| selected.get(k).copied().unwrap_or(false);
        for w in &mut self.windows {
            w.programs.retain(|&k| selected(k));
        }
        self.windows.retain(|w| !w.programs.is_empty());
        for (k, p) in self.programs.iter_mut().enumerate() {
            if selected(k) {
                p.after.retain(|&(dep, _)| selected(dep));
            }
        }
    }
}

impl Task {
    /// if_env と commands を，いまの環境変数（shouji の環境に env を重ねたもの）で決める．
    /// 条件に合わないプログラムは起動する対象から外し（それを待つ after も外す），その名前を返す．
    /// commands のプログラムは，選んだ項目の command（と stop）に置き換える．
    /// pre_command は実行しないので，pre_command が作る変数は条件に使えない．
    pub fn apply_conditions(&mut self) -> Result<Vec<String>> {
        let mut keep = vec![false; self.programs.len()];
        for w in &self.windows {
            w.programs.iter().for_each(|&k| keep[k] = true);
        }
        // 条件に出てくる変数（タスク全体で 1 つの並びにし，env と cwd が同じプログラムは 1 回で調べる）
        let mut names: Vec<String> = Vec::new();
        for (k, p) in self.programs.iter().enumerate() {
            let conds = std::iter::once(&p.if_env).chain(p.choices.iter().map(|c| &c.if_env));
            for (name, _) in conds.flatten().filter(|_| keep[k]) {
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
        }
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let mut seen: Vec<(Env, Option<PathBuf>, Vec<String>)> = Vec::new();
        let mut skipped = Vec::new();
        for (k, p) in self.programs.iter_mut().enumerate() {
            if !keep[k] || (p.if_env.is_empty() && p.choices.is_empty()) {
                continue;
            }
            let values = match seen.iter().find(|(env, cwd, _)| *env == p.env && *cwd == p.cwd) {
                Some((_, _, values)) => values.clone(),
                None => {
                    let values = env_values(&p.env, p.cwd.as_deref(), &names).with_context(|| p.name.clone())?;
                    seen.push((p.env.clone(), p.cwd.clone(), values.clone()));
                    values
                }
            };
            let value = |name: &str| names.iter().position(|n| n == name).map(|i| values[i].clone()).unwrap_or_default();
            let mut ok = holds(&p.if_env, value);
            if ok && !p.choices.is_empty() {
                match p.choices.iter().find(|c| holds(&c.if_env, value)).cloned() {
                    Some(choice) => {
                        p.command = choice.command;
                        if let Some((stop, timeout)) = choice.stop {
                            (p.stop, p.stop_timeout) = (Some(stop), timeout);
                        }
                    }
                    None => ok = false,
                }
            }
            if ok {
                p.choices.clear();
            } else {
                keep[k] = false;
                skipped.push(p.name.clone());
            }
        }
        self.select_mask(&keep);
        Ok(skipped)
    }
}

/// シェルの変数名として使えるか（英字か _ で始まり，英数字と _ だけ）．
fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// after が循環していないか（a → b → a など）を調べる．
fn check_cycle(programs: &[Program]) -> Result<()> {
    // 0: 未訪問，1: 探索中，2: 済み
    fn visit(i: usize, programs: &[Program], mark: &mut [u8], path: &mut Vec<usize>) -> Result<()> {
        match mark[i] {
            2 => return Ok(()),
            1 => {
                let start = path.iter().position(|&p| p == i).unwrap_or(0);
                let mut names: Vec<&str> = path[start..].iter().map(|&p| programs[p].name.as_str()).collect();
                names.push(&programs[i].name);
                bail!("after has a cycle: {}", names.join(" → "));
            }
            _ => {}
        }
        mark[i] = 1;
        path.push(i);
        for &(dep, _) in &programs[i].after {
            visit(dep, programs, mark, path)?;
        }
        path.pop();
        mark[i] = 2;
        Ok(())
    }
    let mut mark = vec![0u8; programs.len()];
    for i in 0..programs.len() {
        visit(i, programs, &mut mark, &mut Vec::new())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_config() {
        let raw: RawConfig =
            serde_yaml_ng::from_str("tasks:\n  - task.yaml\n  - path: /abs/api.yaml\n    name: api\n").unwrap();
        let cfg = Config::from_raw(raw, Path::new("/proj"), None);
        assert_eq!(cfg.tasks.len(), 2);
        assert_eq!(cfg.tasks[0].name, "task");
        assert_eq!(cfg.tasks[0].path, PathBuf::from("/proj/task.yaml"));
        assert_eq!(cfg.tasks[1].name, "api");
        assert_eq!(cfg.tasks[1].path, PathBuf::from("/abs/api.yaml"));
    }

    #[test]
    fn parse_task() {
        let raw: RawTask = serde_yaml_ng::from_str(
            "programs:\n  - name: top\n    command: top\n  - name: srv\n    command: python3 -m http.server\n    cwd: ./srv\n    env: {PORT: '8000'}\n",
        )
        .unwrap();
        let task = Task::from_raw(raw, "dev", Path::new("/proj"), &Shared::default()).unwrap();
        assert_eq!(task.name, "dev");
        assert_eq!(task.programs.len(), 2);
        assert_eq!(task.programs[1].env, vec![("PORT".to_string(), "8000".to_string())]);
        assert_eq!(task.programs[1].cwd, Some(PathBuf::from("/proj/./srv")));
        assert_eq!(task.programs[0].cwd, None);
        assert!(task.programs[0].keep_on_exit);
        assert!(task.programs[0].autostart);
    }

    fn task(yaml: &str) -> Result<Task> {
        Task::from_raw(serde_yaml_ng::from_str(yaml)?, "t", Path::new("/proj"), &Shared::default())
    }

    #[test]
    fn parse_after_and_delay() {
        let t = task(
            "programs:\n  - name: build\n    command: make\n  - name: srv\n    command: ./srv\n  - name: cli\n    command: ./cli\n    after:\n      - build: done\n      - srv\n    delay: 1.5\n",
        )
        .unwrap();
        assert_eq!(t.programs[2].after, vec![(0, Cond::Done), (1, Cond::Started)]);
        assert_eq!(t.programs[2].delay, Duration::from_millis(1500));
        assert!(t.programs[2].waits() && !t.programs[0].waits());
    }

    #[test]
    fn parse_restart_and_log() {
        let t = task(
            "programs:\n  - name: a\n    command: x\n  - name: b\n    command: y\n    restart:\n      when: on-failure\n      delay: 0.5\n      max: 3\n    log: logs/b.log\n  - name: c\n    command: z\n    restart: always\n    cwd: ./srv\n    log: c.log\n  - name: d\n    command: w\n    log: /var/log/d.log\n",
        )
        .unwrap();
        let (a, b) = (&t.programs[0], &t.programs[1]);
        assert_eq!((a.restart, a.restart_delay, a.max_restarts, &a.log), (Restart::Never, Duration::from_secs(1), None, &None));
        assert_eq!(b.restart, Restart::OnFailure);
        assert_eq!(b.restart_delay, Duration::from_millis(500));
        assert_eq!(b.max_restarts, Some(3));
        // log の相対パスは cwd から．cwd がなければ shouji を起動したディレクトリから
        assert_eq!(b.log, Some(std::env::current_dir().unwrap().join("logs/b.log")));
        assert_eq!(t.programs[2].log, Some(PathBuf::from("/proj/./srv/c.log")));
        assert_eq!(t.programs[3].log, Some(PathBuf::from("/var/log/d.log")));
        // 1 行の書き方では delay と max は省略時の値
        let c = &t.programs[2];
        assert_eq!((c.restart, c.restart_delay, c.max_restarts), (Restart::Always, Duration::from_secs(1), None));

        let e = |y: &str| format!("{:#}", task(y).unwrap_err());
        assert!(e("programs:\n  - name: a\n    command: x\n    restart: sometimes\n").contains("restart must be"));
        // 入れ子の中の不明な項目や，以前の書き方はエラーにする
        assert!(e("programs:\n  - name: a\n    command: x\n    restart:\n      when: always\n      retries: 3\n").contains("restart must be"));
        assert!(e("programs:\n  - name: a\n    command: x\n    restart: always\n    max_restarts: 3\n").contains("max_restarts"));
        assert!(e("programs:\n  - name: a\n    command: x\n    restart:\n      when: always\n      delay: -1\n").contains("restart.delay must be a number of seconds"));
    }

    #[test]
    fn env_layers() {
        let t = task(
            "env: {A: task, B: task, C: task}\nwindows:\n  - name: w1\n    env: {B: win, C: win}\n    programs:\n      - name: p1\n        command: x\n        env: {C: prog}\n      - name: p2\n        command: y\n  - name: w2\n    programs:\n      - name: p3\n        command: z\n",
        )
        .unwrap();
        let env = |t: &Task, i: usize| t.programs[i].env.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(" ");
        // 外側から順に並ぶ（後のものが効く）: タスク → ウィンドウ → プログラム
        assert_eq!(env(&t, 0), "A=task B=task C=task B=win C=win C=prog");
        assert_eq!(env(&t, 2), "A=task B=task C=task");

        // programs だけのタスクでも，タスク全体の env が効く．書いた順のまま
        let t = task("env: {Z: task}\nprograms:\n  - name: p\n    command: x\n    env: {B: prog, A: $B}\n").unwrap();
        assert_eq!(env(&t, 0), "Z=task B=prog A=$B");

        // shouji.yaml の共通設定が一番外側．pre_command は狭いものが優先
        let raw: RawConfig = serde_yaml_ng::from_str("env: {S: shared}\npre_command: . ./env.sh\n").unwrap();
        let shared = Shared { env: raw.env, pre_command: raw.pre_command };
        let yaml = "env: {T: task}\nprograms:\n  - name: p\n    command: x\n  - name: q\n    command: y\n    pre_command: echo q\n";
        let t = Task::from_raw(serde_yaml_ng::from_str(yaml).unwrap(), "t", Path::new("/proj"), &shared).unwrap();
        assert_eq!(env(&t, 0), "S=shared T=task");
        assert_eq!(t.programs[0].pre_command.as_deref(), Some(". ./env.sh"));
        assert_eq!(t.programs[1].pre_command.as_deref(), Some("echo q"));

        let e = format!("{:#}", task("programs:\n  - name: p\n    command: x\n    env: {MY-VAR: 1}\n").unwrap_err());
        assert!(e.contains("cannot be an environment variable name"), "{e}");
    }

    #[test]
    fn parse_stop() {
        let t = task(
            "programs:\n  - name: a\n    command: x\n  - name: b\n    command: y\n    stop: docker compose down\n  - name: c\n    command: z\n    stop:\n      command: docker stop c\n      timeout: 3\n",
        )
        .unwrap();
        let st = |i: usize| (t.programs[i].stop.as_deref(), t.programs[i].stop_timeout);
        assert_eq!(st(0), (None, Duration::from_secs(10)));
        assert_eq!(st(1), (Some("docker compose down"), Duration::from_secs(10)));
        assert_eq!(st(2), (Some("docker stop c"), Duration::from_secs(3)));
        let e = |y: &str| format!("{:#}", task(y).unwrap_err());
        assert!(e("programs:\n  - name: a\n    command: x\n    stop:\n      cmd: y\n").contains("stop must be"));
        assert!(e("programs:\n  - name: a\n    command: x\n    stop:\n      command: y\n      timeout: -1\n").contains("stop.timeout must be"));
    }

    #[test]
    fn pre_command_layers() {
        let t = task(
            "pre_command: . task.sh\nwindows:\n  - name: w1\n    pre_command: . win.sh\n    programs:\n      - name: p1\n        command: x\n        pre_command: '  . prog.sh\n'\n      - name: p2\n        command: y\n      - name: p3\n        command: y\n        pre_command: ''\n  - name: w2\n    programs:\n      - name: p4\n        command: z\n",
        )
        .unwrap();
        let pre = |i: usize| t.programs[i].pre_command.as_deref();
        // 狭い領域のものが丸ごと優先される．空文字列なら付けない
        assert_eq!(pre(0), Some(". prog.sh"));
        assert_eq!(pre(1), Some(". win.sh"));
        assert_eq!(pre(2), None);
        assert_eq!(pre(3), Some(". task.sh"));
    }

    #[test]
    fn select_by_name() {
        let yaml = "windows:\n  - name: app\n    programs:\n      - name: build\n        command: make\n      - name: server\n        command: ./srv\n        after: [{build: done}]\n  - name: tools\n    programs:\n      - name: log\n        command: git log\n";
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let shape = |t: &Task| t.windows.iter().map(|w| format!("{}:{:?}", w.name, w.programs)).collect::<Vec<_>>();

        // ウィンドウ名ならその中すべて
        let mut t = task(yaml).unwrap();
        t.select(&names(&["app"])).unwrap();
        assert_eq!(shape(&t), ["app:[0, 1]"]);
        assert_eq!(t.programs[1].after, vec![(0, Cond::Done)]);

        // プログラム名ならそれだけ．選ばなかった build を待つ after は外れる
        let mut t = task(yaml).unwrap();
        t.select(&names(&["server", "log"])).unwrap();
        assert_eq!(shape(&t), ["app:[1]", "tools:[2]"]);
        assert!(t.programs[1].after.is_empty() && !t.programs[1].waits());

        // 名前なしなら全部
        let mut t = task(yaml).unwrap();
        t.select(&[]).unwrap();
        assert_eq!(shape(&t), ["app:[0, 1]", "tools:[2]"]);

        let mut t = task(yaml).unwrap();
        let e = format!("{:#}", t.select(&names(&["nope"])).unwrap_err());
        assert!(e.contains("no window or program named nope") && e.contains("windows: app, tools"));
        let mut t = task("windows:\n  - name: x\n    programs:\n      - name: x\n        command: y\n").unwrap();
        assert!(format!("{:#}", t.select(&names(&["x"])).unwrap_err()).contains("ambiguous"));
    }

    #[test]
    fn parse_windows() {
        let t = task(
            "name: dev\nwindows:\n  - name: app\n    programs:\n      - name: build\n        command: make\n      - name: srv\n        command: ./srv\n        after: [build]\n  - name: mon\n    programs:\n      - name: logs\n        command: tail -f log\n        after: [srv]\n",
        )
        .unwrap();
        assert_eq!(t.windows.len(), 2);
        assert_eq!((t.windows[0].name.as_str(), t.windows[0].programs.clone()), ("app", vec![0, 1]));
        assert_eq!((t.windows[1].name.as_str(), t.windows[1].programs.clone()), ("mon", vec![2]));
        // 別のウィンドウのプログラムも after で待てる
        assert_eq!(t.programs[2].after, vec![(1, Cond::Started)]);

        // programs だけなら，タスク名のウィンドウ 1 つ
        let t = task("programs:\n  - name: a\n    command: x\n").unwrap();
        assert_eq!(t.windows.len(), 1);
        assert_eq!(t.windows[0].name, "t");

        let e = |y: &str| format!("{:#}", task(y).unwrap_err());
        assert!(e("programs:\n  - name: a\n    command: x\nwindows: []\n").contains("not both"));
        assert!(e("name: x\n").contains("write programs or windows"));
        assert!(e("windows:\n  - name: w\n    programs: []\n").contains("window w has no programs"));
    }

    #[test]
    fn after_errors() {
        let e = |y: &str| format!("{:#}", task(y).unwrap_err());
        assert!(e("programs:\n  - name: a\n    command: x\n    after: [b]\n").contains("no program named b"));
        assert!(e("programs:\n  - name: a\n    command: x\n    after: [a]\n").contains("cannot wait for itself"));
        assert!(e("programs:\n  - name: a\n    command: x\n    after: [b]\n  - name: b\n    command: y\n    after: [a]\n")
            .contains("cycle: a → b → a"));
        assert!(e("programs:\n  - name: a\n    command: x\n  - name: b\n    command: y\n    after:\n      - a: finished\n").contains("each after item must be"));
        assert!(e("programs:\n  - name: a\n    command: x\n    delay: -1\n").contains("delay must be a number of seconds"));
    }

    #[test]
    fn glob_patterns() {
        assert!(glob("1", "1") && !glob("1", "10") && !glob("1", ""));
        assert!(glob("", "") && !glob("", "x"));
        assert!(glob("*", "") && glob("*", "anything"));
        assert!(glob("?*", "x") && glob("?*", "xyz") && !glob("?*", ""));
        assert!(glob("sam3_*", "sam3_object") && !glob("sam3_*", "sam2_object"));
        assert!(glob("*codex*", "my-codex-2") && glob("a*b*c", "aXbYbZc") && !glob("a*b*c", "aXbYbZ"));
    }

    #[test]
    fn parse_if_env_and_commands() {
        let t = task(
            "programs:\n  - name: a\n    if_env: { RVIZ: 1, MODE: [fast, 'quick*'] }\n    command: x\n  - name: b\n    stop: s0\n    commands:\n      - if_env: { V3: '1' }\n        command: one\n        stop: { command: s1, timeout: 3 }\n      - command: two\n",
        )
        .unwrap();
        // 数値も文字列として読む．値は 1 つでも並びでもよい
        let a = &t.programs[0];
        assert_eq!(a.if_env, vec![("RVIZ".to_string(), vec!["1".to_string()]), ("MODE".to_string(), vec!["fast".to_string(), "quick*".to_string()])]);
        assert_eq!(a.summary(), "[if RVIZ=1 MODE=fast|quick*] x");
        let b = &t.programs[1];
        assert_eq!(b.choices.len(), 2);
        assert_eq!(b.choices[0].stop, Some(("s1".to_string(), Duration::from_secs(3))));
        assert!(b.choices[1].if_env.is_empty() && b.choices[1].stop.is_none());
        assert_eq!(b.summary(), "[if V3=1] one (+1 more)");

        let e = |y: &str| format!("{:#}", task(y).unwrap_err());
        assert!(e("programs:\n  - name: a\n").contains("write command or commands"));
        assert!(e("programs:\n  - name: a\n    command: x\n    commands: [{command: y}]\n").contains("not both"));
        assert!(e("programs:\n  - name: a\n    commands: []\n").contains("commands has no items"));
        assert!(e("programs:\n  - name: a\n    commands:\n      - command: x\n      - if_env: {A: 1}\n        command: y\n").contains("only the last item can omit if_env"));
        assert!(e("programs:\n  - name: a\n    command: x\n    if_env: {}\n").contains("at least one"));
        assert!(e("programs:\n  - name: a\n    command: x\n    if_env: {MY-VAR: 1}\n").contains("in if_env"));
        assert!(e("programs:\n  - name: a\n    command: x\n    if_env: {A: {b: c}}\n").contains("if_env must be a map"));
        assert!(e("programs:\n  - name: a\n    command: x\n    if_env: {A: []}\n").contains("if_env must be a map"));
        assert!(e("programs:\n  - name: a\n    command: x\n    if_env: A\n").contains("if_env must be a map"));
    }

    #[test]
    fn apply_conditions() {
        // SHOUJI_T_* は設定されていない変数．env の既定値を展開した後の値で決まる
        let yaml = "env: { ON: '${SHOUJI_T_ON:-1}', OFF: '${SHOUJI_T_OFF:-0}', MODE: '${SHOUJI_T_MODE:-codex}' }\nwindows:\n  - name: w1\n    programs:\n      - name: on\n        if_env: { ON: 1 }\n        command: x\n      - name: off\n        if_env: { OFF: 1 }\n        command: x\n      - name: both\n        if_env: { ON: 1, OFF: 1 }\n        command: x\n      - name: waits\n        command: x\n        after: [off, on]\n  - name: w2\n    programs:\n      - name: unset\n        if_env: { SHOUJI_T_NONE: '?*' }\n        command: x\n      - name: empty\n        if_env: { SHOUJI_T_NONE: '' }\n        command: x\n        env: { OFF: 1 }\n      - name: pick\n        stop: s0\n        commands:\n          - if_env: { MODE: vlm }\n            command: vlm\n          - if_env: { MODE: [sidecar, 'co*'], ON: 1 }\n            command: codex\n            stop: s1\n          - command: other\n      - name: none\n        commands:\n          - if_env: { MODE: vlm }\n            command: vlm\n      - name: gated\n        if_env: { OFF: 1 }\n        commands:\n          - command: x\n  - name: w3\n    programs:\n      - name: gone\n        if_env: { OFF: 1 }\n        command: x\n";
        let mut t = task(yaml).unwrap();
        // cwd は /proj（無い）になるので外す
        t.programs.iter_mut().for_each(|p| p.cwd = None);
        let skipped = t.apply_conditions().unwrap();
        assert_eq!(skipped, ["off", "both", "unset", "none", "gated", "gone"]);
        // 合わないものはウィンドウから外れ，すべて外れたウィンドウは無くなる
        let shape: Vec<String> = t.windows.iter().map(|w| format!("{}:{:?}", w.name, w.programs)).collect();
        assert_eq!(shape, ["w1:[0, 3]", "w2:[5, 6]"]);
        // 外れた off を待つ after は外れる
        assert_eq!(t.programs[3].after, vec![(0, Cond::Started)]);
        // 上から最初に合った項目の command と stop になる
        let pick = &t.programs[6];
        assert_eq!((pick.command.as_str(), pick.stop.as_deref(), pick.choices.len()), ("codex", Some("s1"), 0));

        // どれにも合わなければ「それ以外」．stop はプログラムのもの
        let mut t = task("env: { MODE: zzz }\nprograms:\n  - name: pick\n    stop: s0\n    commands:\n      - if_env: { MODE: vlm }\n        command: vlm\n        stop: s1\n      - command: other\n").unwrap();
        assert!(t.apply_conditions().unwrap().is_empty());
        assert_eq!((t.programs[0].command.as_str(), t.programs[0].stop.as_deref()), ("other", Some("s0")));

        // 名前で選ばなかったプログラムは調べない（報告もしない）
        let mut t = task(yaml).unwrap();
        t.programs.iter_mut().for_each(|p| p.cwd = None);
        t.select(&["on".to_string(), "off".to_string()]).unwrap();
        assert_eq!(t.apply_conditions().unwrap(), ["off"]);
    }

    #[test]
    fn unknown_field_is_error() {
        assert!(serde_yaml_ng::from_str::<RawTask>("programs:\n  - name: a\n    comand: x\n").is_err());
        assert!(serde_yaml_ng::from_str::<RawConfig>("programs: []\n").is_err());
        // 不明な項目（以前の autostart など）は無視せずエラーにする
        assert!(serde_yaml_ng::from_str::<RawConfig>("tasks:\n  - path: a.yaml\n    autostart: true\n").is_err());
    }
}
