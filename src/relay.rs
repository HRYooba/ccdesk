//! セッション間の受け渡し。**同じインスタンスのセッションどうし**で、
//! 一覧する・送る・読む・起こす。
//!
//! 使うのは**セッションの中の agent**（`ccdesk list` / `send` / `read` / `new`）。
//! ccdesk の UI からは使わないので、ここに UI の知識は無い。
//!
//! **「ペイン」ではなくセッションを指す。** このプロジェクトで pane と呼ぶのは
//! 右側の矩形 1 つ（[`crate::ui::pane_rect`]）で、そこに出るセッションは
//! 切り替わる。送る相手は矩形ではなく、走っているセッション
//! （[`crate::session::Session`] ＝ [`crate::app::App::windows`] の要素）。
//!
//! # 3 つのファイルと 1 つの env
//!
//! | 置き場所 | 書く者 | 読む者 | 中身 |
//! |:--|:--|:--|:--|
//! | `ipc/open-<pid>.json` | TUI | CLI | そのインスタンスで今走っているセッション |
//! | `ipc/outbox-<pid>.json` | CLI | TUI | まだ消化していない要求 |
//! | `ipc/reply-<pid>.json` | TUI | CLI | 応答（宛名は要求元プロセスの pid） |
//! | `CCDESK_INSTANCE` env | TUI（子を起こすとき） | CLI | 自分がどのインスタンスの子か |
//!
//! **pid で区切るのが「同一インスタンス内のみ」の実装そのもの。** 別インスタンスの
//! ファイルは読みに行かないので、宛先の絞り込みを判定として書く場所が無い
//! （書き忘れも起こらない）。
//!
//! # 往復するものとしないもの
//!
//! transcript は agent 自身がディスクへ書くので、`read` は TUI を経由せず直接
//! 読める（**ccdesk が固まっていても答えが返る**）。往復するのは TUI しか
//! 答えを持たない 2 つだけ ＝ 画面（vt100 はメモリにしか無い）と、
//! `new`（起こせたかと、採番された ID）。
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use ccdesk::{lock_path_for, read_json, write_json_atomically, Lock, LOCK_STALE};

use crate::backend::Kind;
use crate::sessions::SessionId;

/// 子プロセスへ**どのインスタンスの子か**を渡す環境変数。
///
/// **[`crate::hooks::ROW_ENV`] と対になる。** あちらは「どの行か」、こちらは
/// 「どの ccdesk か」で、2 つ揃って初めて `ccdesk send` は自分を名乗れる。
/// 立てるのは同じ 1 箇所（[`crate::backend::Kind::spawn_command`]）
pub(crate) const INSTANCE_ENV: &str = "CCDESK_INSTANCE";

/// ロック待ちの上限。[`crate::hooks`] と同じ値（同じ性質の小さな JSON 1 本）
const LOCK_WAIT: Duration = Duration::from_millis(500);

/// 応答を待つ上限。TUI は 1 周 33ms 前後で回るので、これを使い切るときは
/// 相手が止まっている（待ち続けるより諦めて理由を出す方が使える）
const REPLY_WAIT: Duration = Duration::from_secs(5);
/// 応答ファイルを見に行く間隔
const REPLY_POLL: Duration = Duration::from_millis(20);

/// `read` が既定で返す発言数
pub(crate) const READ_DEFAULT: usize = 20;

const SESSIONS_KEY: &str = "sessions";
const REQUESTS_KEY: &str = "requests";
const SCREEN_KEY: &str = "screen";
const STARTED_KEY: &str = "started";
const ERROR_KEY: &str = "error";
const ID_KEY: &str = "id";
const NAME_KEY: &str = "name";
const CWD_KEY: &str = "cwd";
const KIND_KEY: &str = "kind";
const TRANSCRIPT_KEY: &str = "transcript";
const RUNNING_KEY: &str = "running";
const TO_KEY: &str = "to";
const TEXT_KEY: &str = "text";
const PROMPT_KEY: &str = "prompt";
const REPLY_KEY: &str = "reply";

fn open_path(instance: u32) -> Option<PathBuf> {
    Some(ccdesk::ipc_dir()?.join(format!("open-{instance}.json")))
}

fn outbox_path(instance: u32) -> Option<PathBuf> {
    Some(ccdesk::ipc_dir()?.join(format!("outbox-{instance}.json")))
}

/// 応答の置き場。**宛名は要求元プロセスの pid**（要求を出した `ccdesk` 自身）。
/// 要求ごとの ID を採番しないのは、要求元プロセスが生きている間しかこの応答に
/// 用が無く、pid がその期間ちょうどで一意だから
fn reply_path(caller: u32) -> Option<PathBuf> {
    Some(ccdesk::ipc_dir()?.join(format!("reply-{caller}.json")))
}

/// このインスタンスが面倒を見ているセッション 1 つ。
///
/// **走っているものだけではない。** 止めた行も載る（[`Self::running`]）ので、
/// 「終わった助手セッションの結論を読む」ができる。走っているものに絞っていた頃は、
/// **相手が終わった瞬間に宛先ごと消えて transcript も読めなくなっていた**
/// （記録はディスクに在るのに、指す手段が無い）。
///
/// **一覧の行（[`crate::sessions::SessionRow`]）の写しでもない**: 行は他の
/// インスタンスが起こしたものや前回の起動の残りも含むが、ここに載るのは
/// このインスタンスが起こしたか止めたものだけ
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Open {
    pub(crate) id: SessionId,
    /// 表示名（transcript 由来。正本は [`crate::title`]）
    pub(crate) name: String,
    pub(crate) cwd: String,
    pub(crate) kind: Kind,
    /// 会話の記録。**`read` はこれを直接開く**ので、解決できていないセッションは
    /// `list` には出るが `read` できない
    pub(crate) transcript: Option<PathBuf>,
    /// プロセスが生きているか。**何を宛先にできるかはこれで決まる**
    /// （[`require_running`]）: 打ち込む先と画面は生きた PTY が要るが、
    /// 記録を読むのと行を消すのは要らない
    pub(crate) running: bool,
}

impl Open {
    fn to_json(&self) -> Value {
        json!({
            ID_KEY: self.id.as_str(),
            NAME_KEY: self.name,
            CWD_KEY: self.cwd,
            KIND_KEY: self.kind.as_str(),
            TRANSCRIPT_KEY: self.transcript.as_ref().map(|p| p.to_string_lossy()),
            RUNNING_KEY: self.running,
        })
    }

    /// **読みは寛容**（形が欠けた項目は落とすだけ）。id が無い項目だけは
    /// 宛先にできないので捨てる。
    ///
    /// **`running` が無ければ走っていると読む。** 書き手は必ず書くので、
    /// 欠けるのは**この項目を持たない版が書いた**ときだけ ＝ その版は走っている
    /// ものしか載せていない（exe を入れ替えた直後、走り続けている旧版の TUI が
    /// 書いた一覧を新しい CLI が読む、という組み合わせで実際に起こり得る）
    fn from_json(value: &Value) -> Option<Self> {
        let text = |key: &str| value.get(key).and_then(Value::as_str).unwrap_or_default();
        let id = text(ID_KEY);
        (!id.is_empty()).then(|| Self {
            id: SessionId::new(id),
            name: text(NAME_KEY).to_string(),
            cwd: text(CWD_KEY).to_string(),
            kind: Kind::parse(text(KIND_KEY)).unwrap_or_default(),
            transcript: Some(text(TRANSCRIPT_KEY))
                .filter(|p| !p.is_empty())
                .map(PathBuf::from),
            running: value.get(RUNNING_KEY).and_then(Value::as_bool).unwrap_or(true),
        })
    }
}

/// 生きた PTY を要る操作の門。**止まった行にもできることがある**ので、
/// 要求は操作ごとに分かれる（記録を読むのと行を消すのはここを通らない）
fn require_running(session: &Open, verb: &str) -> anyhow::Result<()> {
    if session.running {
        return Ok(());
    }
    anyhow::bail!(
        "{} ({}) is not running; `{verb}` needs a running session",
        session.name,
        session.id.short()
    )
}

/// TUI が「今走っているセッション」を公開する。**前回と違う周だけ呼ぶ**
/// （呼ぶ側の判断は [`crate::app`]）
pub(crate) fn publish(instance: u32, open: &[Open]) {
    let Some(path) = open_path(instance) else {
        return;
    };
    let document = json!({ SESSIONS_KEY: open.iter().map(Open::to_json).collect::<Vec<_>>() });
    let _ = write_json_atomically(&path, &document);
}

/// 公開されているセッションを読む。**無い・壊れているは空**（`ccdesk list` が
/// 「1 つも無い」と答えるだけで、呼び出し元の agent は止まらない）
pub(crate) fn load(instance: u32) -> Vec<Open> {
    let Some(value) = open_path(instance).as_deref().and_then(read_json) else {
        return Vec::new();
    };
    let Some(items) = value.get(SESSIONS_KEY).and_then(Value::as_array) else {
        return Vec::new();
    };
    items.iter().filter_map(Open::from_json).collect()
}

/// 公開をやめる（TUI の終了時と**起動時**）。
///
/// **起動時にも呼ぶのが要点。** 落ちた前回の残骸は自分の pid で残り得る
/// （pid は再利用される）。outbox が残っていると、**前のインスタンス宛の
/// 未消化の送信が、起動した途端に今のセッションへ打ち込まれる**
pub(crate) fn unpublish(instance: u32) {
    for path in [open_path(instance), outbox_path(instance)]
        .into_iter()
        .flatten()
    {
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(lock_path_for(&path));
    }
}

/// 死んだプロセスの残骸を掃除する（起動時に 1 回）。
///
/// **契機を持たないと消えない**: 正常終了は [`unpublish`] が消すが、強制終了と
/// 電源断は消さない。放っておくと `ipc/` にファイルが積もり続ける
/// （どれも小さいが、積もる仕組みを残す方が問題）
pub(crate) fn reap() {
    let Some(dir) = ccdesk::ipc_dir() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // `<種類>-<pid>.json` から pid を取る。読めない名前は触らない
        let Some(pid) = name
            .rsplit_once('-')
            .and_then(|(_, tail)| tail.strip_suffix(".json"))
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        if ccdesk::process_alive(pid) {
            continue;
        }
        let path = entry.path();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(lock_path_for(&path));
    }
}

/// CLI から TUI への要求 1 件。**TUI しか答えを持たないものだけがここを通る**
/// （transcript の読みは通らない ＝ TUI が止まっていても答えが出る）
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Request {
    /// 宛先の入力欄へ貼って送る。**本文は書いたものがそのまま届く**
    /// （出所の印は付けない ＝ [`run_send`]）
    Send { to: SessionId, text: String },
    /// 宛先の画面を写して `reply` の pid 宛に置く
    Screen { to: SessionId, reply: u32 },
    /// 新しいセッションを起こし、採番された ID を `reply` の pid 宛に置く
    New {
        kind: Kind,
        cwd: String,
        /// 起動時に渡す最初のプロンプト（空なら渡さない）
        prompt: String,
        reply: u32,
    },
    /// プロセスを終わらせる（**行は残す** ＝ メニューの `stop`）
    Stop { to: SessionId },
    /// プロセスを終わらせ、行も消す（＝ メニューの `close`）
    Close { to: SessionId },
}

/// 要求の種類。**綴りの正本はここ 1 箇所**（[`Request::to_json`] と
/// [`Request::from_json`] が同じ表を引く）。
///
/// **鍵の有無で見分ける形から置き換えた。** 種類が 3 つで、それぞれ持つ鍵が
/// 違ううちは見分けられたが、`stop` と `close` は**形が同じ**（宛先だけ）なので
/// 見分けようがない。種類が形から導けない以上、種類は名乗るしかない
const SEND: &str = "send";
const SCREEN: &str = "screen";
const NEW: &str = "new";
const STOP: &str = "stop";
const CLOSE: &str = "close";
/// 種類を運ぶ鍵
const DO_KEY: &str = "do";

impl Request {
    fn to_json(&self) -> Value {
        match self {
            Self::Send { to, text } => {
                json!({ DO_KEY: SEND, TO_KEY: to.as_str(), TEXT_KEY: text })
            }
            Self::Screen { to, reply } => {
                json!({ DO_KEY: SCREEN, TO_KEY: to.as_str(), REPLY_KEY: reply })
            }
            Self::New {
                kind,
                cwd,
                prompt,
                reply,
            } => json!({
                DO_KEY: NEW,
                KIND_KEY: kind.as_str(),
                CWD_KEY: cwd,
                PROMPT_KEY: prompt,
                REPLY_KEY: reply,
            }),
            Self::Stop { to } => json!({ DO_KEY: STOP, TO_KEY: to.as_str() }),
            Self::Close { to } => json!({ DO_KEY: CLOSE, TO_KEY: to.as_str() }),
        }
    }

    /// **知らない種類・欠けた項目は None**（呼び手が捨てる ＝ 版がずれた
    /// CLI からの要求で TUI が落ちない）
    fn from_json(value: &Value) -> Option<Self> {
        let text = |key: &str| value.get(key).and_then(Value::as_str);
        let reply = || value.get(REPLY_KEY).and_then(Value::as_u64).map(|v| v as u32);
        let to = || text(TO_KEY).map(SessionId::new);
        match text(DO_KEY)? {
            SEND => Some(Self::Send {
                to: to()?,
                text: text(TEXT_KEY)?.to_string(),
            }),
            SCREEN => Some(Self::Screen {
                to: to()?,
                reply: reply()?,
            }),
            NEW => Some(Self::New {
                kind: Kind::parse(text(KIND_KEY)?)?,
                cwd: text(CWD_KEY)?.to_string(),
                prompt: text(PROMPT_KEY).unwrap_or_default().to_string(),
                reply: reply()?,
            }),
            STOP => Some(Self::Stop { to: to()? }),
            CLOSE => Some(Self::Close { to: to()? }),
            _ => None,
        }
    }
}

/// 要求を 1 件積む。**ロックの内側で読み直してから足す**（複数のセッションから
/// 同時に呼ばれるので、読みと書きの間に他の要求が挟まると落ちる）
fn push(instance: u32, request: &Request) -> anyhow::Result<()> {
    let path = outbox_path(instance)
        .ok_or_else(|| anyhow::anyhow!("could not locate the ccdesk directory"))?;
    let _guard = Lock::acquire(&lock_path_for(&path), LOCK_WAIT, LOCK_STALE)?;
    let mut items = read_requests(&path);
    items.push(request.to_json());
    write_json_atomically(&path, &json!({ REQUESTS_KEY: items }))
}

/// 要求が来ているか。**TUI は毎周これを見る**ので、ロックもファイルの読みも
/// 伴わない形にしてある（`exists` 1 回 ＝ hook の受け渡しを見るのと同じ重さ）。
///
/// **「空のファイル」を作らない**のがこの安さの条件で、[`drain`] は空にする
/// 代わりに消す（残る長さから空を見分ける形にすると、書式が変わった日に黙って
/// 壊れる）
pub(crate) fn pending(instance: u32) -> bool {
    outbox_path(instance).is_some_and(|path| path.exists())
}

/// 溜まった要求を取り出す。**TUI 側の入口**
pub(crate) fn drain(instance: u32) -> Vec<Request> {
    let Some(path) = outbox_path(instance) else {
        return Vec::new();
    };
    let Ok(_guard) = Lock::acquire(&lock_path_for(&path), LOCK_WAIT, LOCK_STALE) else {
        return Vec::new();
    };
    let items = read_requests(&path);
    // **消すのは「読めた」からではなく「取り出した」から。** 解釈できない項目も
    // ここで落とす（残すと毎周同じものを読み直す）
    let _ = std::fs::remove_file(&path);
    items.iter().filter_map(Request::from_json).collect()
}

fn read_requests(path: &Path) -> Vec<Value> {
    read_json(path)
        .as_ref()
        .and_then(|value| value.get(REQUESTS_KEY))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// 応答を置く（TUI 側）。**要求 1 件につき必ず 1 回**
/// （返さないと要求元は上限まで待つ）
pub(crate) fn answer(caller: u32, value: &Value) {
    let Some(path) = reply_path(caller) else {
        return;
    };
    let _ = write_json_atomically(&path, value);
}

/// 画面の写しの応答
pub(crate) fn screen_answer(screen: &str) -> Value {
    json!({ SCREEN_KEY: screen })
}

/// 起動の応答（採番された ID か、起こせなかった理由）
pub(crate) fn started_answer(started: Result<SessionId, String>) -> Value {
    match started {
        Ok(id) => json!({ STARTED_KEY: id.as_str() }),
        Err(error) => json!({ ERROR_KEY: error }),
    }
}

/// 自分宛の応答が来るまで待つ（CLI 側）。**取ったら消す**ので、
/// 前回の残りを次の呼び出しが拾うことはない。
///
/// **理由を運ぶ応答はここで失敗にする**（[`ERROR_KEY`]）ので、呼び出し側は
/// 成功した応答の中身だけを見ればよい
fn wait_answer() -> anyhow::Result<Value> {
    let path = reply_path(std::process::id())
        .ok_or_else(|| anyhow::anyhow!("could not locate the ccdesk directory"))?;
    let deadline = Instant::now() + REPLY_WAIT;
    loop {
        if let Some(value) = read_json(&path) {
            let _ = std::fs::remove_file(&path);
            if let Some(error) = value.get(ERROR_KEY).and_then(Value::as_str) {
                anyhow::bail!("{error}");
            }
            return Ok(value);
        }
        if Instant::now() >= deadline {
            anyhow::bail!("ccdesk did not answer within {REPLY_WAIT:?}");
        }
        std::thread::sleep(REPLY_POLL);
    }
}

/// 宛先の指定をセッション 1 つに解く。
///
/// **id の前方一致 → 名前の前方一致 → 名前の部分一致**の順に狭い方から試し、
/// **絞れなければ候補を並べて失敗する**（黙って 1 つ目へ送ると、送った本人にも
/// 誰に届いたか分からない）
pub(crate) fn resolve<'a>(target: &str, open: &'a [Open]) -> anyhow::Result<&'a Open> {
    let needle = target.trim();
    if needle.is_empty() {
        anyhow::bail!("no target given; run `ccdesk list` to see the sessions");
    }
    if open.is_empty() {
        anyhow::bail!("no sessions are running in this ccdesk");
    }
    let fold = needle.to_lowercase();
    let by_id: Vec<&Open> = open
        .iter()
        .filter(|session| session.id.as_str().starts_with(needle))
        .collect();
    let by_head: Vec<&Open> = open
        .iter()
        .filter(|session| session.name.to_lowercase().starts_with(&fold))
        .collect();
    let by_part: Vec<&Open> = open
        .iter()
        .filter(|session| session.name.to_lowercase().contains(&fold))
        .collect();
    for found in [by_id, by_head, by_part] {
        match found.len() {
            0 => continue,
            1 => return Ok(found[0]),
            _ => anyhow::bail!(
                "`{needle}` matches {} sessions:\n{}",
                found.len(),
                found
                    .iter()
                    .map(|session| format!("  {}  {}", session.id.short(), session.name))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        }
    }
    anyhow::bail!("no session matches `{needle}`; run `ccdesk list` to see the sessions")
}

/// 自分がどのインスタンスの子か。**無ければ失敗**（ccdesk の外で叩かれた）
fn instance() -> anyhow::Result<u32> {
    let value = std::env::var(INSTANCE_ENV).ok().unwrap_or_default();
    value.trim().parse().map_err(|_| {
        anyhow::anyhow!(
            "this command only works inside a ccdesk session ({INSTANCE_ENV} is not set)"
        )
    })
}

/// 呼び出し元のセッション。**名乗るためだけに要る**ので、分からなくても止めない
fn caller(open: &[Open]) -> Option<&Open> {
    let row = std::env::var(crate::hooks::ROW_ENV).ok()?;
    open.iter().find(|session| session.id.as_str() == row.trim())
}

// ---------------------------------------------------------------------------
// サブコマンド
// ---------------------------------------------------------------------------

/// `ccdesk list`
pub(crate) fn run_list() -> anyhow::Result<()> {
    let instance = instance()?;
    let open = load(instance);
    if open.is_empty() {
        println!("no sessions here yet");
        return Ok(());
    }
    let me = caller(&open).map(|session| session.id.clone());
    for session in &open {
        // 自分の行に印を付ける（宛先に自分を選ぶ事故は、印があれば起きにくい）
        let mark = if Some(&session.id) == me.as_ref() { "*" } else { " " };
        // **状態を列で出す。** 止まったセッションも載るので、何を宛先にできるかが
        // 一覧の時点で読める（`read` と `close` はできる、`send` はできない）
        let state = if session.running { "running" } else { "stopped" };
        println!(
            "{mark} {}  {:<8} {:<8} {}  ({})",
            session.id.short(),
            session.kind.as_str(),
            state,
            session.name,
            session.cwd
        );
    }
    Ok(())
}

/// `ccdesk send <session> <text>`
///
/// **投げっぱなし。** 届いたかも、相手が何と答えたかも返さない
/// （知りたければ `ccdesk read` で取りに行く）。
///
/// **本文へ出所の印を付けない**（生のパイプ ＝ tmux の `send-keys` と同じ）。
/// 印を付けないので、受け取った agent には**人が打ったのと区別が付かない**
/// ＝ ccdesk 経由の指示もユーザーの指示として扱われる。同じ人の同じ機械の
/// 中で完結する経路なので、その区別に意味を置かないという判断
pub(crate) fn run_send(target: &str, text: &str) -> anyhow::Result<()> {
    if text.trim().is_empty() {
        anyhow::bail!("nothing to send");
    }
    let (instance, to) = other(target)?;
    require_running(&to, "send")?;
    push(
        instance,
        &Request::Send {
            to: to.id.clone(),
            text: text.to_string(),
        },
    )?;
    println!("sent to {} ({})", to.name, to.id.short());
    Ok(())
}

/// `ccdesk stop <session>` — プロセスを終わらせ、**行は残す**
/// （サイドバーから開き直せば会話を再開できる ＝ メニューの `stop` と同じ）
pub(crate) fn run_stop(target: &str) -> anyhow::Result<()> {
    let (instance, to) = other(target)?;
    require_running(&to, "stop")?;
    push(instance, &Request::Stop { to: to.id.clone() })?;
    println!("stopped {} ({})", to.name, to.id.short());
    Ok(())
}

/// `ccdesk close <session>` — プロセスを終わらせ、**行も消す**
/// （＝ メニューの `close`）。
///
/// **記録は消えない**（transcript は agent のもの）ので、消えるのは
/// ccdesk の一覧から辿る道だけ
pub(crate) fn run_close(target: &str) -> anyhow::Result<()> {
    let (instance, to) = other(target)?;
    push(instance, &Request::Close { to: to.id.clone() })?;
    println!("closed {} ({})", to.name, to.id.short());
    Ok(())
}

/// 宛先を**自分以外の**セッション 1 つに解く。
///
/// **自分を指せない**のは 3 つの動作に共通の規則: 送るのは会話にならず、
/// 止める・閉じるは**このコマンドを動かしている親を殺す**（結果を報告する前に
/// 死ぬので、成否も分からない）。判断を 1 箇所に置いてあるので、
/// 動作が増えても抜けない
fn other(target: &str) -> anyhow::Result<(u32, Open)> {
    let instance = instance()?;
    let open = load(instance);
    let to = resolve(target, &open)?.clone();
    if caller(&open).is_some_and(|session| session.id == to.id) {
        anyhow::bail!("that is this session; pick another one");
    }
    Ok((instance, to))
}

/// `ccdesk read <session>`
///
/// **止まったセッションでも読める。** 記録はディスクに在り、`ccdesk` も相手の
/// プロセスも要らない ＝ **終わった助手セッションの結論を取りに行ける**のが要点。
/// 生きた PTY が要るのは `--screen` だけ（画面は TUI のメモリにしかない）
pub(crate) fn run_read(target: &str, last: usize, screen: bool) -> anyhow::Result<()> {
    let instance = instance()?;
    let open = load(instance);
    let session = resolve(target, &open)?;
    if screen {
        require_running(session, "read --screen")?;
        // **画面は TUI のメモリにしかない**ので、ここだけ往復する
        push(
            instance,
            &Request::Screen {
                to: session.id.clone(),
                reply: std::process::id(),
            },
        )?;
        let answer = wait_answer()?;
        print!(
            "{}",
            answer
                .get(SCREEN_KEY)
                .and_then(Value::as_str)
                .unwrap_or_default()
        );
        return Ok(());
    }
    let Some(path) = session.transcript.as_deref() else {
        anyhow::bail!(
            "no transcript for {} yet; try `--screen`",
            session.id.short()
        );
    };
    let messages = read_transcript(path, session.kind, last);
    if messages.is_empty() {
        println!("nothing has been said in {} yet", session.name);
        return Ok(());
    }
    for message in messages {
        println!("{}: {}", message.speaker(), message.text);
    }
    Ok(())
}

/// `ccdesk new [--agent <name>] [--cwd <dir>] [prompt]`
///
/// **採番された ID を返す**ので、起こしてすぐ `send` / `read` の宛先にできる
/// （返さないと、どれが今起こしたものか呼び出し元には分からない）
pub(crate) fn run_new(kind: Option<Kind>, cwd: Option<&str>, prompt: &str) -> anyhow::Result<()> {
    let instance = instance()?;
    let open = load(instance);
    let me = caller(&open);
    // **既定は呼び出し元に揃える。** 別の agent を起こしたいときだけ名指しする
    let kind = kind
        .or_else(|| me.map(|session| session.kind))
        .unwrap_or_default();
    // 既定は**この CLI が走っている場所**（agent はそこで作業している）。
    // 取れなければ呼び出し元のセッションの cwd へ落とす
    let cwd = match cwd {
        Some(cwd) => cwd.to_string(),
        None => std::env::current_dir()
            .map(|dir| dir.to_string_lossy().to_string())
            .ok()
            .or_else(|| me.map(|session| session.cwd.clone()))
            .ok_or_else(|| anyhow::anyhow!("could not tell where to start it; pass --cwd"))?,
    };
    push(
        instance,
        &Request::New {
            kind,
            cwd,
            prompt: prompt.to_string(),
            reply: std::process::id(),
        },
    )?;
    let answer = wait_answer()?;
    let id = answer
        .get(STARTED_KEY)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("ccdesk answered without a session id"))?;
    // **短い ID を出す**（そのまま次のコマンドの宛先として通る）
    println!("started {}", SessionId::new(id).short());
    Ok(())
}

/// transcript の末尾 `last` 発言。
///
/// **丸ごと読んでから末尾を取る。** 記録は 1 MB を超えることがあるが、これを
/// 呼ぶのは 1 回きりの短命なプロセスで、TUI の周回には乗らない
/// （途中から読む仕掛けを持つ [`crate::title`] とは要件が違う）
fn read_transcript(path: &Path, kind: Kind, last: usize) -> Vec<crate::backend::Message> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let backend = kind.backend();
    let mut all: Vec<crate::backend::Message> = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|value| backend.message(&value))
        .collect();
    if all.len() > last {
        all.drain(..all.len() - last);
    }
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(id: &str, name: &str) -> Open {
        Open {
            id: SessionId::new(id),
            name: name.to_string(),
            cwd: "C:/work".to_string(),
            kind: Kind::Claude,
            transcript: None,
            running: true,
        }
    }

    fn stopped(id: &str, name: &str) -> Open {
        Open { running: false, ..open(id, name) }
    }

    #[test]
    fn a_target_resolves_by_id_prefix() {
        let sessions = [open("abcd1234-0000", "login form"), open("ffff0000", "docs")];
        assert_eq!(resolve("abcd", &sessions).unwrap().name, "login form");
    }

    #[test]
    fn a_target_resolves_by_name_regardless_of_case() {
        let sessions = [open("aaaa", "Login form"), open("bbbb", "docs")];
        assert_eq!(resolve("login", &sessions).unwrap().id.as_str(), "aaaa");
    }

    /// 前方一致が 1 つに決まるなら、部分一致で増える候補は見に行かない
    #[test]
    fn a_head_match_wins_over_a_longer_partial_match() {
        let sessions = [open("aaaa", "docs site"), open("bbbb", "the docs")];
        assert_eq!(resolve("docs", &sessions).unwrap().id.as_str(), "aaaa");
    }

    #[test]
    fn an_ambiguous_target_fails_and_names_the_candidates() {
        let sessions = [open("aaaa", "docs one"), open("bbbb", "docs two")];
        let message = resolve("docs", &sessions).unwrap_err().to_string();
        assert!(message.contains("docs one"), "{message}");
        assert!(message.contains("docs two"), "{message}");
    }

    #[test]
    fn an_unknown_target_fails() {
        assert!(resolve("nothing", &[open("aaaa", "docs")]).is_err());
    }

    #[test]
    fn no_sessions_is_said_plainly() {
        let message = resolve("docs", &[]).unwrap_err().to_string();
        assert!(message.contains("no sessions"), "{message}");
    }

    #[test]
    fn a_request_survives_a_round_trip_through_json() {
        for request in [
            Request::Send {
                to: SessionId::new("aaaa"),
                text: "hello".to_string(),
            },
            Request::Screen {
                to: SessionId::new("bbbb"),
                reply: 4321,
            },
            Request::New {
                kind: Kind::Codex,
                cwd: "C:/work/docs".to_string(),
                prompt: "write the release notes".to_string(),
                reply: 8765,
            },
            Request::Stop {
                to: SessionId::new("cccc"),
            },
            Request::Close {
                to: SessionId::new("dddd"),
            },
        ] {
            assert_eq!(Request::from_json(&request.to_json()), Some(request));
        }
    }

    /// **形が同じ 2 つ**（宛先だけ）が種類名で分かれることを固定する。
    /// 鍵の有無で見分けていた頃の形へ戻すと、ここが落ちる
    #[test]
    fn stop_and_close_are_told_apart_by_name_not_by_shape() {
        let stop = Request::Stop {
            to: SessionId::new("aaaa"),
        };
        let close = Request::Close {
            to: SessionId::new("aaaa"),
        };
        assert_ne!(stop.to_json(), close.to_json());
    }

    /// 知らない種類は捨てる（新しい CLI から古い TUI へ届いても落ちない）
    #[test]
    fn an_unknown_action_is_dropped() {
        assert_eq!(
            Request::from_json(&json!({ DO_KEY: "explode", TO_KEY: "aaaa" })),
            None
        );
    }

    #[test]
    fn a_session_survives_a_round_trip_through_json() {
        for running in [true, false] {
            let original = Open {
                transcript: Some(PathBuf::from("C:/t/aaaa.jsonl")),
                kind: Kind::Codex,
                running,
                ..open("aaaa", "login form")
            };
            assert_eq!(Open::from_json(&original.to_json()), Some(original));
        }
    }

    /// **`running` を持たない一覧は、走っているものとして読む。**
    /// 書かない版は走っているものしか載せていない（exe を差し替えた直後、
    /// 走り続けている旧版の TUI が書いた一覧を新しい CLI が読む形で起こり得る）
    #[test]
    fn a_session_from_a_version_that_did_not_record_running_reads_as_running() {
        let value = json!({ ID_KEY: "aaaa", NAME_KEY: "login form" });
        assert!(Open::from_json(&value).expect("dropped a usable entry").running);
    }

    /// 止まったセッションにできること・できないことの表。**この分かれ方が
    /// 「止めた行も載せる」ことの意味そのもの**（載せるだけで何でもできる
    /// わけではない）
    #[test]
    fn a_stopped_session_refuses_what_needs_a_live_pty() {
        let session = stopped("aaaa", "login form");
        for verb in ["send", "stop", "read --screen"] {
            let message = require_running(&session, verb)
                .expect_err(&format!("`{verb}` was allowed on a stopped session"))
                .to_string();
            assert!(message.contains("not running"), "{message}");
            // 名前と ID の両方を出す（どれを指したのか呼び出し元が確かめられる）
            assert!(message.contains("login form"), "{message}");
        }
    }

    /// 走っているセッションは何も拒まない
    #[test]
    fn a_running_session_passes_the_gate() {
        assert!(require_running(&open("aaaa", "login form"), "send").is_ok());
    }

    /// 止まった行も宛先に**解ける**（解けないと `read` も `close` も届かない）
    #[test]
    fn a_stopped_session_can_still_be_named_as_a_target() {
        let sessions = [open("aaaa", "running one"), stopped("bbbb", "finished one")];
        assert_eq!(resolve("finished", &sessions).unwrap().id.as_str(), "bbbb");
    }

    /// id を持たない項目は宛先にできないので落とす
    #[test]
    fn a_session_without_an_id_is_dropped() {
        assert_eq!(Open::from_json(&json!({ NAME_KEY: "nameless" })), None);
    }

    /// 起こせなかった理由は要求元へ渡り、そこで失敗として出る
    #[test]
    fn a_failed_start_is_answered_with_its_reason() {
        let answer = started_answer(Err("codex is not enabled".to_string()));
        assert_eq!(
            answer.get(ERROR_KEY).and_then(Value::as_str),
            Some("codex is not enabled")
        );
    }
}
