//! 子の claude から ccdesk へ状態を戻す口（`--settings` で注入する hook）。
//!
//! **hook の実体は ccdesk 自身のサブコマンド**（`ccdesk hook <event>`）。外部スクリプトを
//! 撒かないので、ccdesk を置き換えれば hook も一緒に入れ替わり、「古いスクリプトが
//! 残っていて新しい ccdesk と噛み合わない」状態が作れない。
//!
//! 受けた state は `~/.ccdesk/hook-states.json` へ置き、TUI が周期的に読む。
//! 書き方（advisory lock と tmp → rename）は lib 側の 1 実装を使う
//! （[`ccdesk::Lock`] / [`ccdesk::write_json_atomically`]）。
//!
//! **state の正本は 2 段構え**:
//! hook が主で、hook が一度も来ていないセッションだけ `claude agents --json` の
//! `status` から導く。**受けた state を行へ写さない**のが要点で、写していた頃は
//! 保管（`sessions.json`）と hook が食い違い、しかもどちらが新しいかが行ごとに
//! 逆になっていた（[`crate::sessions::SessionRow`]）。
//!
//! ここが答えるのは 3 つ。どれも**行に保存せず、そのつど引く**:
//! 状態（[`HookStates::get`]）・未読（[`HookStates::unread`]）・
//! 行が今の姿になった時刻（[`HookStates::changed_at`]）。
//!
//! **注入する settings はここが 1 箇所で組む**（[`inject_settings`]）。載るのは hook だけ。
//!
//! **`statusLine` を載せてはいけない。** `--settings` の値はキー単位でユーザー設定を
//! 上書きする（公式仕様）ので、書いた瞬間にそのセッションのユーザー自身の statusline が
//! 消える。奪ってから代理実行で返す形は実際に試して壊れており（返す側が環境差で失敗し、
//! 使用率表示を使っていない人の statusline まで空にした）、**代理実行に戻さないこと**。
//! 使用率は statusLine に相乗りせず [`crate::usage`] が独立に取る。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use ccdesk::{lock_path_for, now_ms, write_json_atomically, Lock, LOCK_STALE};

// hook のイベント名と入力 JSON は**公式**（文書化されている）が、pid を渡す
// 環境変数は非公開なので綴りは [`crate::claude_format`] が持つ
use crate::claude_format::CLAUDE_PID_ENV;
use crate::poll::{COMPLETED, STOPPED, WAITING, WORKING};
use crate::sessions::{SessionId, SessionRow};

/// 注入する hook イベントと、絞り込みの matcher と、それが意味する state。
///
/// **`--settings` の生成（[`inject_settings`]）と受け口（[`run_hook`]）が同じ表を読む**
/// ので、片方だけ増えた状態にならない。state 値は [`crate::poll::classify`] が読む語彙
/// （`waiting` / `working` / `completed` / `stopped` ＝ 画面に出る語の小文字）で、
/// **要約文は持たない**（行に出るのは状態だけ）。
///
/// **turn 単位のイベントだけを載せる。** hook は毎回 ccdesk を 1 プロセス起こすので、
/// `PreToolUse` / `PostToolUse` のような道具ごとに飛ぶイベントを足すと、Windows の
/// プロセス起動コストがそのままセッションの遅さになる。
///
/// # `Notification` を絞る理由
///
/// `Notification` は通知の種類ごとに matcher を持つ（公式に文書化。値は完全一致で、
/// `|` 区切りで複数指定できる）。**絞らずに全部拾うと `idle_prompt`（60 秒放置の催促）が
/// 混ざり、ターンを終えた行が時間経過だけで入力待ちへ落ちる**。実害は 2 つあった:
/// 「Needs input」が「claude が止まっている」意味を失い、既読にした行の未読印が復活した。
///
/// 拾うのは**ユーザーが動くまで進まない**通知だけ。`auth_success` /
/// `elicitation_complete` / `elicitation_response` / `agent_completed` は完了・情報通知
/// なので拾わない（完了は `Stop` が答える）。
///
/// 縮退: matcher が効かない版では `Notification` が一度も発火しない ＝ 入力待ちは
/// `agents --json` の `status` 経由だけになる。**`done` が壊れるより取り逃すほうが軽い**
const NOTIFICATION_MATCHER: &str = "permission_prompt|elicitation_dialog|agent_needs_input";

/// 注入する hook（イベント名, matcher, state）。matcher が None のイベントは
/// 全発火を拾う（そのイベント自体が 1 つの意味しか持たないもの）
const HOOK_EVENTS: [(&str, Option<&str>, &str); 5] = [
    // 起動直後・再開直後はまだプロンプトを受けていない ＝ 入力待ち
    ("SessionStart", None, WAITING),
    ("UserPromptSubmit", None, WORKING),
    ("Notification", Some(NOTIFICATION_MATCHER), WAITING),
    ("Stop", None, COMPLETED),
    ("SessionEnd", None, STOPPED),
];

/// 保管ファイルのトップレベルキー（`{"states": { "<session-id>": { … } }}`）
const STATES_KEY: &str = "states";
/// 項目のキー。**読みと書きで同じ定数を使う**（片側だけ直した状態を作らない）
const STATE_KEY: &str = "state";
const AT_KEY: &str = "at";
const PID_KEY: &str = "pid";


/// 受けた state を保つ期間。**動いているセッションは毎 turn 書き直す**ので、
/// これを過ぎた項目は既に終わったセッションのもの ＝ 読んでも意味が無い。
/// 消さないと 1 セッション 1 項目で永久に積もる
const KEEP: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// 保管の read-modify-write を直列化するロックの待ち時間。
///
/// **セッションを待たせないことが最優先**なので、保管ロック（2 秒）より短い:
/// hook はユーザーの turn の途中で走り、待った分だけ claude の応答が遅れる。
/// 取れなければ書かずに諦める ＝ その turn の state が 1 つ落ちるだけで、
/// 次のイベントで載る（状態は毎 turn 上書きされるので取り返しがつく）
const LOCK_WAIT: Duration = Duration::from_millis(500);

/// hook が書いた state 1 件。**受けた時刻（`at`）を捨てない**のが要点で、
/// 保管に残っている項目が「今動いている実行のもの」か「前回の実行の残骸」かは
/// この時刻でしか区別できない（[`HookStates::get`]）
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    state: String,
    /// 受けた時刻（epoch ms）。**書き手（[`record`]）と同じ時計**
    at: u64,
    /// その hook を呼んだ claude のプロセス ID（[`CLAUDE_PID_ENV`]）。
    /// 取れない環境では None ＝ pid での引き当て（[`HookStates::session_of`]）に
    /// 出てこないだけで、state の受け渡しには影響しない
    pid: Option<u32>,
}

/// hook が書いた state の写し（`session_id` → [`Entry`]）
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct HookStates(BTreeMap<SessionId, Entry>);

impl HookStates {
    /// その行の hook 由来の state。`launched` は**その行を今動かしている窓を
    /// 起こした時刻**（窓が無ければ None）。**None は「使える hook が無い」**で、
    /// 呼び手（描画）はそのとき `agents --json` の従経路へ落ちる。
    ///
    /// **保管に残っているだけでは採らない。** 受け渡しファイルはセッションが
    /// 終わっても残る（[`KEEP`]）ので、次の 2 つはどちらも残骸として捨てる:
    ///
    /// - 窓が無い ＝ その行は動いていない（前景セッションの実体は ccdesk の子）
    /// - 記録が窓の起動より古い ＝ 前回の実行のもの（再開直後に前回の
    ///   `SessionEnd` が残っている場合）
    ///
    /// **「行が生きているか」では判断しない。** 生死の観測（`try_wait`）は
    /// 2 秒周期で遅れて届くので、それを材料にすると `stop` 直後の**正当な
    /// `stopped` が捨てられ**、前の state（`blocked` ＝ Needs input）に戻る
    pub(crate) fn get(&self, id: &SessionId, launched: Option<u64>) -> Option<&str> {
        let entry = self.0.get(id)?;
        (entry.at >= launched?).then_some(entry.state.as_str())
    }

    /// その行について **hook が最後に何か書いた時刻**（記録が無ければ None）。
    /// 窓の有無は見ない ＝ 動いていない行についても「最後に動いたのはいつか」を答える
    fn last_at(&self, id: &SessionId) -> Option<u64> {
        self.0.get(id).map(|entry| entry.at)
    }

    /// 前回の写しと比べて、**新しくターンを終えた行があるか**。
    ///
    /// 使用率はターンが終わった瞬間に動くので、これが取得の合図になる
    /// （[`crate::usage`]。周期で叩き続けるより、変わった直後に 1 回叩くほうが
    /// 正確で、何もしていない間は claude を 1 プロセスも起こさない）。
    ///
    /// **`at` まで比べる**のが要点: state だけを見ると、`completed` のまま残っている
    /// 行が毎周「終わった」と言い続ける
    pub(crate) fn any_turn_finished_since(&self, previous: &Self) -> bool {
        self.0.iter().any(|(id, entry)| {
            entry.state == COMPLETED && previous.0.get(id).map(|p| p.at) != Some(entry.at)
        })
    }

    /// その行が**未読**か（行頭の `●`）。
    ///
    /// **claude が何か言ったのが、最後にその行を開いた後か**で決まる。材料は
    /// hook の `at` だけで、行の `updated_at` は見ない。だから:
    ///
    /// - ピン留め・メニュー操作など**ユーザー自身の操作では未読にならない**
    ///   （行を書き換えても hook の記録は動かない）
    /// - **ccdesk を起動し直しただけでも未読にならない**（`last_opened_at` は
    ///   保管されるので、hook の記録がそれより古ければ既読のまま）
    pub(crate) fn unread(&self, row: &SessionRow) -> bool {
        self.last_at(&row.session_id)
            .is_some_and(|at| at > row.last_opened_at)
    }

    /// その行が**今の姿になった時刻**（行に出る経過時間 `· 23s` の起点）。
    ///
    /// **未読とは別の材料を見る**: 姿は「claude が言った状態」と「保管の中身」の
    /// 両方で変わるので、新しい方を採る。未読（[`Self::unread`]）が hook だけを
    /// 見るのは、答える問いが「claude が何か言ったか」だから
    pub(crate) fn changed_at(&self, row: &SessionRow) -> u64 {
        self.last_at(&row.session_id)
            .unwrap_or(0)
            .max(row.updated_at)
    }

    /// **その claude プロセスが今動かしているセッション。** hook はどのイベントでも
    /// 「その時点の `session_id`」と「呼び出した claude の pid」を一緒に書くので、
    /// その pid で一番新しい記録が答えになる。
    ///
    /// `launched` は**その pid の窓を起こした時刻**。それより古い記録は前回の実行
    /// （pid の使い回しを含む）なので採らない ＝ 判断の材料は [`Self::get`] と同じ。
    ///
    /// **ペイン内の `/resume` `/clear` に周期を待たずに気づく口**がこれで、
    /// `claude agents --json`（1 回 ~900ms のプロセス起動）を待たずに済む
    pub(crate) fn session_of(&self, pid: u32, launched: u64) -> Option<&SessionId> {
        self.0
            .iter()
            .filter(|(_, entry)| entry.pid == Some(pid) && entry.at >= launched)
            .max_by_key(|(_, entry)| entry.at)
            .map(|(id, _)| id)
    }

    #[cfg(test)]
    pub(crate) fn from_entries<'a>(
        entries: impl IntoIterator<Item = (&'a str, &'a str, u64)>,
    ) -> Self {
        Self::from_records(entries.into_iter().map(|(id, state, at)| (id, state, at, None)))
    }

    #[cfg(test)]
    pub(crate) fn from_records<'a>(
        entries: impl IntoIterator<Item = (&'a str, &'a str, u64, Option<u32>)>,
    ) -> Self {
        Self(
            entries
                .into_iter()
                .map(|(id, state, at, pid)| {
                    (
                        SessionId::new(id),
                        Entry {
                            state: state.to_string(),
                            at,
                            pid,
                        },
                    )
                })
                .collect(),
        )
    }
}

/// イベント名 → state（[`HOOK_EVENTS`] の引き）。未知のイベントは None
/// （知らない名前で呼ばれても何も書かない）
fn state_of(event: &str) -> Option<&'static str> {
    HOOK_EVENTS
        .iter()
        .find(|(name, _, _)| *name == event)
        .map(|(_, _, state)| *state)
}

/// `ccdesk hook <event>`。**注入した hook の受け口**（ユーザーは直接使わない）。
///
/// claude は hook の入力を stdin の JSON で渡すので、そこから `session_id` を取る
/// （どのセッションの state かは呼び出し側からしか分からない）。
///
/// **fail-open**: 何が起きても `Ok` で返り、**標準出力へ何も書かない**。
/// `UserPromptSubmit` の標準出力はそのままセッションの文脈へ足されるため、
/// ここが何か書くと ccdesk がユーザーの会話に割り込むことになる
pub(crate) fn run_hook(event: &str) -> anyhow::Result<()> {
    use std::io::Read as _;
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let Some((session_id, state, pid)) = hook_entry(event, &input) else {
        return Ok(());
    };
    if let Some(path) = ccdesk::hook_states_path() {
        record(&path, &session_id, state, now_ms(), pid);
    }
    Ok(())
}

/// hook 1 回で保管へ載せるもの（イベント名・stdin・環境から決まる）。
/// **[`run_hook`] が持つ判断はこれだけ**で、残りはファイルの読み書き ＝
/// 環境から pid を拾う経路も含めて、保管を触らずに検査できる。
///
/// 知らないイベント / `session_id` が読めない入力は None（何も書かない）
fn hook_entry(event: &str, input: &str) -> Option<(SessionId, &'static str, Option<u32>)> {
    let state = state_of(event)?;
    let session_id = session_id_of(input)?;
    Some((session_id, state, claude_pid()))
}

/// この hook を呼んだ claude の pid（[`CLAUDE_PID_ENV`]）。
/// **読めなければ None**（記録は載るが pid での引き当てには出てこない）
fn claude_pid() -> Option<u32> {
    std::env::var(CLAUDE_PID_ENV).ok()?.trim().parse().ok()
}

/// hook 入力から `session_id` を取る。**読みは寛容**（形が変わっても落ちない:
/// 取れなければ何も書かないだけ）
fn session_id_of(input: &str) -> Option<SessionId> {
    let value = serde_json::from_str::<Value>(input).ok()?;
    let id = value.get("session_id").and_then(Value::as_str)?;
    let id = SessionId::new(id);
    (!id.is_empty()).then_some(id)
}

/// 1 件の state を保管へ載せる。**ロックの内側で読み直してから置く**
/// （hook は複数のセッションから同時に走るので、読みと書きの間に他の hook の
/// 書き込みが挟まると、その turn の state が落ちる）。
///
/// 古い項目はここで落とす（[`KEEP`]）。掃除の契機を別に持たないのは、
/// 書くのがこの 1 箇所だけで、**書くたびに掃除すれば積もらない**ため
fn record(path: &Path, session_id: &SessionId, state: &str, now: u64, pid: Option<u32>) {
    let Ok(_guard) = Lock::acquire(&lock_path_for(path), LOCK_WAIT, LOCK_STALE) else {
        return;
    };
    let mut entries = read_entries(path);
    entries.retain(|_, entry| now.saturating_sub(entry.at) < KEEP.as_millis() as u64);
    entries.insert(
        session_id.to_string(),
        Entry {
            state: state.to_string(),
            at: now,
            pid,
        },
    );
    let document = json!({
        STATES_KEY: entries
            .iter()
            .map(|(id, entry)| (id.clone(), json!({
                STATE_KEY: entry.state,
                AT_KEY: entry.at,
                PID_KEY: entry.pid,
            })))
            .collect::<serde_json::Map<_, _>>()
    });
    let _ = write_json_atomically(path, &document);
}

/// 保管ファイルの項目（`session_id` → [`Entry`]）。
/// **無い・壊れている・書き換え途中はすべて空**（起動も turn も止めない）
fn read_entries(path: &Path) -> BTreeMap<String, Entry> {
    let Some(value) = ccdesk::read_json(path) else {
        return BTreeMap::new();
    };
    let Some(states) = value.get(STATES_KEY).and_then(Value::as_object) else {
        return BTreeMap::new();
    };
    states
        .iter()
        .filter_map(|(id, entry)| {
            let state = entry.get(STATE_KEY).and_then(Value::as_str)?;
            // state を持たない項目は捨てる（state が無い項目は何も答えられない）。
            // 時刻は既定 0 で読む ＝ 次の書き込みで古い項目として落ちる
            let at = entry.get(AT_KEY).and_then(Value::as_u64).unwrap_or(0);
            // pid は桁が u32 に収まる値だけ採る（読めなければ pid 無しの記録として扱う）
            let pid = entry
                .get(PID_KEY)
                .and_then(Value::as_u64)
                .and_then(|pid| u32::try_from(pid).ok());
            (!id.is_empty() && !state.is_empty()).then(|| {
                (
                    id.clone(),
                    Entry {
                        state: state.to_string(),
                        at,
                        pid,
                    },
                )
            })
        })
        .collect()
}

/// 保管を読む（TUI 側。**ロックを取らない**のは書き込みが tmp → rename で
/// 原子的なため ＝ 中途の JSON は読めない。読みのたびに待つと、周期的に呼ぶ側が
/// hook の書き込みで止まる）
pub(crate) fn read_states() -> HookStates {
    ccdesk::hook_states_path()
        .map(|path| states_at(&path))
        .unwrap_or_default()
}

fn states_at(path: &Path) -> HookStates {
    HookStates(
        read_entries(path)
            .into_iter()
            .map(|(id, entry)| (SessionId::new(id), entry))
            .collect(),
    )
}

/// 保管ファイルの見え方（長さ・更新時刻）。**中身を読まずに「変わったか」だけを
/// 見る口**で、run ループがこれを毎周見て変化した周だけ読み直す
/// （hook が来た瞬間に一覧へ反映するための合図。ファイルを開かないので安い）
pub(crate) fn states_stamp() -> Option<(u64, std::time::SystemTime)> {
    ccdesk::file_stamp(&ccdesk::hook_states_path()?)
}

/// 子の claude へ `--settings` で渡す注入ファイルを書き、そのパスを返す。
///
/// 載るのは **hook だけ**。`--settings` を 1 つしか渡せない以上、ここに何を書くかは
/// そのセッションのユーザー設定を上書きするかどうかの判断そのもので、
/// **何を注入するかの判断はここ 1 箇所**にある。
///
/// コマンドのパスは `/` 区切り必須: claude は hook を bash 経由で
/// 実行するため `\` 区切りはエスケープとして食われる（実測）。
///
/// **前提**: `--settings` の hook はユーザー自身の設定（`~/.claude/settings.json`）の
/// hook と併存する（claude は設定ソースごとの hook を合成する。公式に文書化）。
/// スカラーのキーは併存せず上書きになるので、**hook 以外は載せない**
/// （[`settings_document`] のテストがそれを固定する）
pub(crate) fn inject_settings() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = ccdesk::ccdesk_dir()?;
    let exe_fwd = exe.to_string_lossy().replace('\\', "/");
    let path = dir.join("inject-settings.json");
    std::fs::write(&path, settings_document(&exe_fwd).to_string()).ok()?;
    Some(path)
}

/// 注入ファイルの中身（[`inject_settings`] の判断だけを取り出したもの。
/// ファイルを書かずに検査できる）
fn settings_document(exe_fwd: &str) -> Value {
    let mut settings = serde_json::Map::new();
    settings.insert(
        "hooks".to_string(),
        Value::Object(
            HOOK_EVENTS
                .iter()
                .map(|(event, matcher, _)| {
                    // matcher を持つイベントだけ `matcher` を載せる。
                    // **空文字を載せない**（`""` は「何にも一致しない」とも
                    // 「全部に一致」とも読めるので、意図が伝わらない形にしない）
                    let mut group = serde_json::Map::new();
                    if let Some(matcher) = matcher {
                        group.insert("matcher".to_string(), json!(matcher));
                    }
                    group.insert(
                        "hooks".to_string(),
                        json!([{
                            "type": "command",
                            "command": format!("\"{exe_fwd}\" hook {event}"),
                        }]),
                    );
                    (
                        (*event).to_string(),
                        json!([Value::Object(group)]),
                    )
                })
                .collect(),
        ),
    );
    Value::Object(settings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// テスト専用の保管先。**実ユーザーの `~/.ccdesk` を絶対に触らない**ための境界
    /// （[`crate::sessions::tests`] の `TempStore` と同じ規律）
    struct TempStore(PathBuf);

    impl TempStore {
        fn new(test: &str) -> Self {
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "ccdesk-hooks-{test}-{}-{seq}",
                std::process::id()
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn path(&self) -> PathBuf {
            self.0.join("hook-states.json")
        }
    }

    impl Drop for TempStore {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn id(text: &str) -> SessionId {
        SessionId::new(text)
    }

    /// 保管に載っている state（時刻の新旧は問わない）。**新旧の判断そのものは
    /// [`a_hook_state_belongs_to_the_run_that_was_launched_before_it`] が固定する**ので、
    /// 保管の読み書きを見る他のテストは起動時刻 0（＝ 何でも受ける）で引く
    fn stored(states: &HookStates, id: &SessionId) -> Option<String> {
        states.get(id, Some(0)).map(str::to_string)
    }

    /// **注入する表と受け口が同じ表を読む。** 片方だけ知っているイベントがあると、
    /// 注入したのに何も起きない（または登録されていない口が残る）
    #[test]
    fn every_injected_event_is_understood_by_the_receiver() {
        let document = settings_document("C:/bin/ccdesk.exe");
        let hooks = document.get("hooks").and_then(Value::as_object).unwrap();
        assert_eq!(hooks.len(), HOOK_EVENTS.len(), "number of injected hooks differs from the table");
        for (event, matcher, state) in HOOK_EVENTS {
            let command = hooks[event][0]["hooks"][0]["command"].as_str().unwrap();
            assert_eq!(
                command,
                format!("\"C:/bin/ccdesk.exe\" hook {event}"),
                "{event} has a different invocation shape"
            );
            // matcher は表のとおりに載る（持たないイベントにはキー自体を書かない）
            assert_eq!(
                hooks[event][0].get("matcher").and_then(Value::as_str),
                matcher,
                "{event} has a different matcher"
            );
            assert_eq!(state_of(event), Some(state), "the receiver doesn't know {event}");
        }
        assert_eq!(state_of("PreToolUse"), None, "received an unregistered hook");
    }

    /// **ターン完了は「新しく `completed` になった」だけを合図にする。**
    ///
    /// `completed` のまま残っている行を state だけで見ると、毎周「終わった」と
    /// 言い続けて使用率の取得が止まらない（`at` まで比べる理由）
    #[test]
    fn only_a_fresh_completion_counts_as_a_finished_turn() {
        let none = HookStates::default();
        let working = HookStates::from_entries([("s", WORKING, 1_000)]);
        let finished = HookStates::from_entries([("s", COMPLETED, 2_000)]);

        // 動いていた行が終わった ＝ 合図
        assert!(finished.any_turn_finished_since(&working));
        // 何も知らなかったところに完了が現れた ＝ 合図（起動直後）
        assert!(finished.any_turn_finished_since(&none));
        // 同じ完了が残っているだけ ＝ 合図にしない
        assert!(!finished.any_turn_finished_since(&finished));
        // 完了が無ければ合図にならない
        assert!(!working.any_turn_finished_since(&none));

        // **同じ行が次のターンを終えたら、また合図になる**（`at` が進むため）
        let again = HookStates::from_entries([("s", COMPLETED, 3_000)]);
        assert!(again.any_turn_finished_since(&finished));

        // 別の行が終わっても合図（複数セッションを見ている）
        let other = HookStates::from_entries([("s", COMPLETED, 2_000), ("t", COMPLETED, 2_500)]);
        assert!(other.any_turn_finished_since(&finished));
    }

    /// **`Notification` は「ユーザーが動くまで進まない」通知だけを拾う。**
    ///
    /// 絞らずに全部拾うと `idle_prompt`（60 秒放置の催促）が混ざり、ターンを終えた行が
    /// 時間経過だけで入力待ちへ落ちる ＝ 「Needs input」が「claude が止まっている」
    /// 意味を失い、既読にした行の未読印が復活する。**この 2 つは実際に起きた**
    #[test]
    fn the_notification_hook_ignores_the_idle_reminder() {
        let matcher = HOOK_EVENTS
            .iter()
            .find(|(event, _, _)| *event == "Notification")
            .and_then(|(_, matcher, _)| *matcher)
            .expect("Notification must be filtered by a matcher");
        let kinds: Vec<&str> = matcher.split('|').collect();
        for wanted in ["permission_prompt", "elicitation_dialog", "agent_needs_input"] {
            assert!(kinds.contains(&wanted), "{wanted} is not picked up");
        }
        // 催促と完了・情報通知は拾わない（完了は Stop が答える）
        for unwanted in [
            "idle_prompt",
            "auth_success",
            "elicitation_complete",
            "elicitation_response",
            "agent_completed",
        ] {
            assert!(!kinds.contains(&unwanted), "{unwanted} must not be picked up");
        }
    }

    /// **道具ごとに飛ぶイベントは登録しない**（hook は毎回 ccdesk を 1 プロセス
    /// 起こすので、turn より細かい粒度を足すとセッションが目に見えて遅くなる）
    #[test]
    fn only_turn_level_events_are_injected() {
        for event in ["PreToolUse", "PostToolUse", "PreCompact", "SubagentStop"] {
            assert!(
                !HOOK_EVENTS.iter().any(|(name, _, _)| *name == event),
                "{event} is not turn-level"
            );
        }
    }

    /// **注入するのは `hooks` だけ。** `hooks` は claude が設定ソースを跨いで合成するので
    /// 併存する（公式に文書化）が、スカラーのキーは併存せず上書きになる ＝ hook 以外を
    /// 載せた瞬間にそのセッションのユーザー設定が消える（経緯はモジュールの doc）
    #[test]
    fn only_hooks_are_injected() {
        let document = settings_document("C:/bin/ccdesk.exe");
        let keys: Vec<&str> = document
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["hooks"], "injected a key other than hooks");
    }

    /// 受けた state は保管へ載り、TUI 側の読みで同じ値が返る
    #[test]
    fn a_recorded_state_reaches_the_reader() {
        let temp = TempStore::new("a_recorded_state_reaches_the_reader");
        assert_eq!(states_at(&temp.path()), HookStates::default(), "not empty for a missing file");

        record(&temp.path(), &id("s-1"), "working", 1_000, None);
        record(&temp.path(), &id("s-2"), "blocked", 1_000, None);
        assert_eq!(
            states_at(&temp.path()),
            HookStates::from_entries([("s-1", "working", 1_000), ("s-2", "blocked", 1_000)])
        );

        // 同じセッションの次のイベントは上書き（状態は最後に受けたものが正しい）
        record(&temp.path(), &id("s-1"), "done", 2_000, None);
        let states = states_at(&temp.path());
        assert_eq!(stored(&states, &id("s-1")).as_deref(), Some("done"));
        assert_eq!(
            stored(&states, &id("s-2")).as_deref(),
            Some("blocked"),
            "affected another session"
        );
        assert_eq!(stored(&states, &id("s-3")), None, "answered for an unknown session");
    }

    /// **hook の記録は「その行を今動かしている窓の起動より新しいもの」だけが有効。**
    ///
    /// 受け渡しファイルはセッションが終わっても残るので、時刻で新旧を見ないと
    /// 前回の実行の `SessionEnd` が再開直後の行を Stopped に見せる。逆に
    /// 「行が生きているか」で判断すると（生死の観測は 2 秒周期で遅れて届く）
    /// `stop` 直後の正当な `stopped` が捨てられる
    #[test]
    fn a_hook_state_belongs_to_the_run_that_was_launched_before_it() {
        let states = HookStates::from_entries([("s", "stopped", 2_000)]);
        // 起動より後に記録された ＝ 今の実行のもの
        assert_eq!(states.get(&id("s"), Some(1_000)), Some("stopped"));
        // 起動と同時刻も今の実行（時計の分解能で同じ ms に並び得る）
        assert_eq!(states.get(&id("s"), Some(2_000)), Some("stopped"));
        // 起動より前に記録された ＝ 前回の実行の残骸
        assert_eq!(states.get(&id("s"), Some(3_000)), None);
        // 窓が無い行は動いていない ＝ 保管の値は過去の実行のもの
        assert_eq!(states.get(&id("s"), None), None);
        // 記録の無い行はいつでも None（hook が一度も来ていない）
        assert_eq!(states.get(&id("other"), Some(0)), None);
    }

    /// **未読は「claude が何か言ったのが、最後に開いた後か」。**
    ///
    /// 材料が hook の `at` だけなので、次の 2 つは**書けなくなっている**:
    /// ユーザー自身の操作で未読が付くこと（行を書き換えても記録は動かない）と、
    /// ccdesk を起動し直しただけで未読になること（`last_opened_at` は保管される）
    #[test]
    fn a_row_is_unread_when_the_hook_is_newer_than_the_last_open() {
        let row = |last_opened_at| SessionRow {
            last_opened_at,
            ..SessionRow::new(id("s"), "C:\\dev\\app", 0)
        };
        let states = HookStates::from_entries([("s", "done", 2_000)]);
        assert!(states.unread(&row(1_999)), "claude spoke after the open but the row stayed read");
        assert!(!states.unread(&row(2_000)), "a hook at the moment of the open marks it unread");
        assert!(!states.unread(&row(2_001)));
        // 記録の無い行は未読にならない（hook が一度も来ていない ＝ 何も言っていない）
        assert!(!HookStates::default().unread(&row(0)));

        // **行を書き換えても未読は動かない**（`updated_at` は材料ではない）
        let mut pinned = row(2_001);
        pinned.pinned = true;
        pinned.updated_at = 9_999;
        assert!(!states.unread(&pinned), "an edit to the row created an unread mark");
    }

    /// **経過時間の起点は未読とは別の材料。** 行の姿は「claude が言った状態」と
    /// 「保管の中身」の両方で変わるので、新しい方を採る（未読は hook だけを見る）
    #[test]
    fn the_age_of_a_row_starts_at_whichever_moved_last() {
        let row = |updated_at| SessionRow {
            updated_at,
            ..SessionRow::new(id("s"), "C:\\dev\\app", 0)
        };
        let states = HookStates::from_entries([("s", "done", 2_000)]);
        assert_eq!(states.changed_at(&row(1_000)), 2_000, "the hook did not move the age");
        assert_eq!(states.changed_at(&row(3_000)), 3_000, "an edit to the row did not move the age");
        // hook を持たない行は保管の時刻だけ（起動しただけの行も 0 にならない）
        assert_eq!(HookStates::default().changed_at(&row(1_000)), 1_000);
    }

    /// **pid は claude が hook の子へ渡す環境変数から読む**（実測: v2.1.220 の
    /// hook は `CLAUDE_PID=<claude の pid>` を持つ子として走る）。
    ///
    /// **親のプロセス環境を一時的に触る**（そうしないと、この名前が居ない環境で
    /// 検査が空振りする）。触るのはこの 1 つだけで、復元は読み取りの直後に行う
    #[test]
    fn the_pid_comes_from_the_environment_claude_gives_the_hook() {
        let input = r#"{"session_id":"s-1","source":"clear"}"#;
        unsafe { std::env::set_var(CLAUDE_PID_ENV, " 4242 ") };
        let padded = hook_entry("SessionStart", input);
        let bare = claude_pid();
        unsafe { std::env::set_var(CLAUDE_PID_ENV, "not a number") };
        let broken = hook_entry("SessionStart", input);
        unsafe { std::env::remove_var(CLAUDE_PID_ENV) };
        let missing = hook_entry("SessionStart", input);

        // **記録に pid まで載る**（載らないと pid での引き当てが黙って効かなくなる）
        assert_eq!(
            padded,
            Some((id("s-1"), WAITING, Some(4242))),
            "the pid did not reach the record"
        );
        assert_eq!(bare, Some(4242), "the pid is not read from the environment");
        assert_eq!(
            broken,
            Some((id("s-1"), WAITING, None)),
            "built a pid out of something that is not a number"
        );
        assert_eq!(
            missing,
            Some((id("s-1"), WAITING, None)),
            "answered with a pid when the variable is not set"
        );
        // 知らないイベント / 読めない入力は何も書かない
        assert_eq!(hook_entry("PreToolUse", input), None);
        assert_eq!(hook_entry("SessionStart", "not json"), None);
    }

    /// **pid は保管を往復する。** ここが落ちると、ペイン内の `/resume` `/clear` に
    /// 気づく口（[`HookStates::session_of`]）が黙って効かなくなる
    #[test]
    fn the_pid_of_the_calling_claude_survives_a_round_trip() {
        let temp = TempStore::new("the_pid_of_the_calling_claude_survives_a_round_trip");
        record(&temp.path(), &id("s"), "working", 1_000, Some(4242));
        assert_eq!(
            states_at(&temp.path()),
            HookStates::from_records([("s", "working", 1_000, Some(4242))])
        );
        // pid の無い記録も読める（環境変数が取れなかった場合）
        record(&temp.path(), &id("s"), "done", 2_000, None);
        assert_eq!(
            states_at(&temp.path()),
            HookStates::from_records([("s", "done", 2_000, None)])
        );
    }

    /// **その pid が今動かしているセッション ＝ その pid の一番新しい記録。**
    ///
    /// `/clear` は「古いセッションの `SessionEnd`」と「新しいセッションの
    /// `SessionStart`」を同じ pid で続けて書く（実測）ので、新しい方を採らないと
    /// 張り替え先が前のセッションのままになる
    #[test]
    fn the_newest_record_of_a_pid_names_the_session_it_is_running() {
        let states = HookStates::from_records([
            ("old", "stopped", 2_000, Some(7)),
            ("new", "blocked", 2_001, Some(7)),
            ("other-process", "working", 9_000, Some(8)),
            ("no-pid", "working", 9_000, None),
        ]);
        assert_eq!(states.session_of(7, 1_000), Some(&id("new")));
        assert_eq!(states.session_of(8, 1_000), Some(&id("other-process")));
        // 窓の起動より古い記録は前回の実行のもの（pid の使い回しを含む）
        assert_eq!(states.session_of(7, 2_001), Some(&id("new")));
        assert_eq!(states.session_of(7, 2_002), None);
        // 知らない pid には答えない
        assert_eq!(states.session_of(9, 0), None);
    }

    /// **保管の見え方は中身を読まずに取れる**（run ループが毎周見る合図）。
    /// 書き込みのたびに変わり、無いファイルには答えない
    #[test]
    fn the_store_stamp_changes_when_something_is_recorded() {
        let temp = TempStore::new("the_store_stamp_changes_when_something_is_recorded");
        let stamp = |path: &std::path::Path| {
            std::fs::metadata(path)
                .ok()
                .map(|m| (m.len(), m.modified().ok()))
        };
        assert_eq!(stamp(&temp.path()), None, "answered for a missing file");
        record(&temp.path(), &id("s"), "working", 1_000, Some(1));
        let before = stamp(&temp.path()).expect("no stamp after a write");
        record(&temp.path(), &id("s-2"), "blocked", 2_000, Some(2));
        assert_ne!(stamp(&temp.path()), Some(before), "the stamp did not move");
    }

    /// **古い項目は書くたびに落ちる**（1 セッション 1 項目で永久に積もらない）。
    /// 落ちるのは保つ期間を過ぎたものだけで、動いているセッションは毎 turn
    /// 書き直されるので落ちない
    #[test]
    fn recording_drops_entries_older_than_the_keep_window() {
        let temp = TempStore::new("recording_drops_entries_older_than_the_keep_window");
        let keep = KEEP.as_millis() as u64;
        record(&temp.path(), &id("old"), "done", 0, None);
        record(&temp.path(), &id("fresh"), "working", keep, None);
        // old は keep をちょうど過ぎた時点で落ちる
        record(&temp.path(), &id("now"), "blocked", keep + 1, None);
        let states = states_at(&temp.path());
        assert_eq!(stored(&states, &id("old")), None, "an entry past the keep window remains");
        assert_eq!(
            stored(&states, &id("fresh")).as_deref(),
            Some("working"),
            "dropped an entry still within the window"
        );
        assert_eq!(stored(&states, &id("now")).as_deref(), Some("blocked"));
    }

    /// 壊れた / 想定外の形でも読みは失敗しない（＝ TUI の周期処理が止まらない）。
    /// **state を持たない項目だけは捨てる**（読んでも何も答えられない）
    #[test]
    fn reads_tolerate_missing_broken_and_unexpected_shapes() {
        let temp = TempStore::new("hook_reads_tolerate_broken_shapes");
        let cases = [
            ("empty", ""),
            ("broken", r#"{"states":{"s":{"state":"done"}"#),
            ("not-object", "[1,2,3]"),
            ("no-key", r#"{"other":1}"#),
            ("not-map", r#"{"states":[1,2]}"#),
            ("no-state", r#"{"states":{"s":{"at":1}}}"#),
            ("empty-state", r#"{"states":{"s":{"state":""}}}"#),
            ("wrong-types", r#"{"states":{"s":{"state":7}}}"#),
        ];
        for (name, text) in cases {
            std::fs::write(temp.path(), text).unwrap();
            assert_eq!(states_at(&temp.path()), HookStates::default(), "not empty for {name}");
        }
        // 時刻が無い / 型違いでも state は読む（既定 0 ＝ 次の書き込みで落ちる）
        std::fs::write(temp.path(), r#"{"states":{"s":{"state":"done","at":"soon"}}}"#).unwrap();
        assert_eq!(stored(&states_at(&temp.path()), &id("s")).as_deref(), Some("done"));
    }

    /// hook 入力から取るのは `session_id` だけ。**取れなければ何も書かない**
    /// （形が変わっても turn を止めない）
    #[test]
    fn the_session_id_comes_from_the_hook_input() {
        assert_eq!(
            session_id_of(r#"{"session_id":"8a1c0f52","cwd":"C:\\dev"}"#),
            Some(id("8a1c0f52"))
        );
        for broken in [
            "",
            "not json",
            "{}",
            r#"{"session_id":""}"#,
            r#"{"session_id":7}"#,
            r#"{"sessionId":"camel-case-is-not-the-hook-shape"}"#,
        ] {
            assert_eq!(session_id_of(broken), None, "built an ID from {broken:?}");
        }
    }

    /// 書き込みは tmp → rename（[`write_json_atomically`]）。
    /// **書きかけの `.tmp` を残さない**うえ、ロックも残さない
    #[test]
    fn writes_land_atomically_without_leaving_a_tmp_or_a_lock() {
        let temp = TempStore::new("writes_land_atomically_without_leaving_a_tmp_or_a_lock");
        record(&temp.path(), &id("s"), "working", 1_000, None);
        let leftovers: Vec<_> = std::fs::read_dir(&temp.0)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "a partial tmp file remains: {leftovers:?}");
        assert!(!lock_path_for(&temp.path()).exists(), "left a lock behind");
    }

    /// **ロックが取れなければ書かずに諦める**（セッションを待たせない）。
    /// 落ちるのはその 1 回の state だけで、次のイベントで載る
    #[test]
    fn a_held_lock_makes_the_hook_give_up_instead_of_waiting() {
        let temp = TempStore::new("a_held_lock_makes_the_hook_give_up_instead_of_waiting");
        record(&temp.path(), &id("s"), "working", 1_000, None);
        let before = std::fs::read(temp.path()).unwrap();

        let held = Lock::acquire(&lock_path_for(&temp.path()), Duration::ZERO, LOCK_STALE).unwrap();
        let started = std::time::Instant::now();
        record(&temp.path(), &id("s"), "done", 2_000, None);
        let waited = started.elapsed();
        drop(held);

        assert!(waited < Duration::from_secs(5), "wait was not bounded: {waited:?}");
        assert_eq!(std::fs::read(temp.path()).unwrap(), before, "wrote even though the lock wasn't acquired");
        // 解放後は通常どおり載る（ロックが理由で壊れているわけではない）
        record(&temp.path(), &id("s"), "done", 2_000, None);
        assert_eq!(stored(&states_at(&temp.path()), &id("s")).as_deref(), Some("done"));
    }
}
