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
//! **state の正本は 2 段構え**（`docs/foreground-migration.md` のフェーズ3）:
//! hook が主で、hook が一度も来ていないセッションだけ `claude agents --json` の
//! `status` から導く。行に残す `last_state`（[`crate::sessions::SessionRow`]）は
//! ここで受けた state を写したもので、プロセスが死んだ後の表示に使う。
//!
//! **注入する settings はここが 1 箇所で組む**（[`inject_settings`]）。使用率表示の
//! statusLine も同じファイルに載るため（`--settings` は 1 つしか渡せない）で、
//! statusLine コマンドの中身だけは [`crate::cli::statusline_hook`] にある。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use ccdesk::{lock_path_for, now_ms, write_json_atomically, Lock, LOCK_STALE};

use crate::sessions::SessionId;

/// 注入する hook イベントと、それが意味する state。
///
/// **`--settings` の生成（[`inject_settings`]）と受け口（[`run_hook`]）が同じ表を読む**
/// ので、片方だけ増えた状態にならない。state 値は [`crate::poll::classify`] が読む語彙
/// （`working` / `blocked` / `done` / `stopped`）で、**要約文は持たない**
/// （行に出るのは状態だけ ＝ `docs/foreground-migration.md` の確定仕様）。
///
/// **turn 単位のイベントだけを載せる。** hook は毎回 ccdesk を 1 プロセス起こすので、
/// `PreToolUse` / `PostToolUse` のような道具ごとに飛ぶイベントを足すと、Windows の
/// プロセス起動コストがそのままセッションの遅さになる
const HOOK_EVENTS: [(&str, &str); 5] = [
    // 起動直後・再開直後はまだプロンプトを受けていない ＝ 入力待ち
    ("SessionStart", "blocked"),
    ("UserPromptSubmit", "working"),
    // 入力待ち・許可待ちのどちらもユーザーの操作を待っている状態
    ("Notification", "blocked"),
    ("Stop", "done"),
    ("SessionEnd", "stopped"),
];

/// 保管ファイルのトップレベルキー（`{"states": { "<session-id>": { … } }}`）
const STATES_KEY: &str = "states";
/// 項目のキー。**読みと書きで同じ定数を使う**（片側だけ直した状態を作らない）
const STATE_KEY: &str = "state";
const AT_KEY: &str = "at";

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

/// hook が書いた state の写し（`session_id` → state）。
///
/// **時刻は持たない**: 読み手が使うのは「今この行の state は何か」だけで、
/// 古い項目を落とすのは書き手側（[`record`]）の仕事
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct HookStates(BTreeMap<SessionId, String>);

impl HookStates {
    /// その行に hook 由来の state があるか。**None は「hook が来ていない」**で、
    /// 呼び手（描画）はそのとき `agents --json` の従経路へ落ちる
    pub(crate) fn get(&self, id: &SessionId) -> Option<&str> {
        self.0.get(id).map(String::as_str)
    }

    #[cfg(test)]
    pub(crate) fn from_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        Self(
            pairs
                .into_iter()
                .map(|(id, state)| (SessionId::new(id), state.to_string()))
                .collect(),
        )
    }
}

/// イベント名 → state（[`HOOK_EVENTS`] の引き）。未知のイベントは None
/// （知らない名前で呼ばれても何も書かない）
fn state_of(event: &str) -> Option<&'static str> {
    HOOK_EVENTS
        .iter()
        .find(|(name, _)| *name == event)
        .map(|(_, state)| *state)
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
    let Some(state) = state_of(event) else {
        return Ok(());
    };
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let Some(session_id) = session_id_of(&input) else {
        return Ok(());
    };
    if let Some(path) = ccdesk::hook_states_path() {
        record(&path, &session_id, state, now_ms());
    }
    Ok(())
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
fn record(path: &Path, session_id: &SessionId, state: &str, now: u64) {
    let Ok(_guard) = Lock::acquire(&lock_path_for(path), LOCK_WAIT, LOCK_STALE) else {
        return;
    };
    let mut entries = read_entries(path);
    entries.retain(|_, (_, at)| now.saturating_sub(*at) < KEEP.as_millis() as u64);
    entries.insert(session_id.to_string(), (state.to_string(), now));
    let document = json!({
        STATES_KEY: entries
            .iter()
            .map(|(id, (state, at))| (id.clone(), json!({ STATE_KEY: state, AT_KEY: at })))
            .collect::<serde_json::Map<_, _>>()
    });
    let _ = write_json_atomically(path, &document);
}

/// 保管ファイルの項目（`session_id` → (state, 受けた時刻 ms)）。
/// **無い・壊れている・書き換え途中はすべて空**（起動も turn も止めない）
fn read_entries(path: &Path) -> BTreeMap<String, (String, u64)> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
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
            (!id.is_empty() && !state.is_empty()).then(|| (id.clone(), (state.to_string(), at)))
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
            .map(|(id, (state, _))| (SessionId::new(id), state))
            .collect(),
    )
}

/// 起動時の掃除: rename の前に死んだ hook プロセスが残した `.tmp` を回収する
/// （どう回収するかは [`ccdesk::reap_leftover_tmp`]。ここが持つのは対象の指定だけ）
pub(crate) fn cleanup_leftover_tmp() {
    if let Some(path) = ccdesk::hook_states_path() {
        ccdesk::reap_leftover_tmp(&path);
    }
}

/// 子の claude へ `--settings` で渡す注入ファイルを書き、そのパスを返す。
///
/// 載るのは 2 つ: **hook（常に）** と、使用率表示が opt-in のときだけ statusLine。
/// 1 ファイルに束ねてあるのは `--settings` を 1 つしか渡せないためで、
/// **何を注入するかの判断はここ 1 箇所**（呼び出し側は opt-in の可否だけを渡す）。
///
/// コマンドのパスは `/` 区切り必須: claude は hook / statusline を bash 経由で
/// 実行するため `\` 区切りはエスケープとして食われる（実測）。
///
/// **前提**: `--settings` の hook はユーザー自身の設定（`~/.claude/settings.json`）の
/// hook と併存する（claude は設定ソースごとの hook を合成する）。仮に置き換えだった
/// 場合、ccdesk が起こしたセッションではユーザーの hook が動かなくなる
pub(crate) fn inject_settings(usage_display: bool) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = ccdesk::ccdesk_dir()?;
    let exe_fwd = exe.to_string_lossy().replace('\\', "/");
    let path = dir.join("inject-settings.json");
    std::fs::write(&path, settings_document(&exe_fwd, usage_display).to_string()).ok()?;
    Some(path)
}

/// 注入ファイルの中身（[`inject_settings`] の判断だけを取り出したもの。
/// ファイルを書かずに検査できる）
fn settings_document(exe_fwd: &str, usage_display: bool) -> Value {
    let mut settings = serde_json::Map::new();
    settings.insert(
        "hooks".to_string(),
        Value::Object(
            HOOK_EVENTS
                .iter()
                .map(|(event, _)| {
                    (
                        (*event).to_string(),
                        json!([{
                            "hooks": [{
                                "type": "command",
                                "command": format!("\"{exe_fwd}\" hook {event}"),
                            }],
                        }]),
                    )
                })
                .collect(),
        ),
    );
    if usage_display {
        settings.insert(
            "statusLine".to_string(),
            json!({
                "type": "command",
                "command": format!("\"{exe_fwd}\" statusline-hook"),
            }),
        );
    }
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

    /// **注入する表と受け口が同じ表を読む。** 片方だけ知っているイベントがあると、
    /// 注入したのに何も起きない（または登録されていない口が残る）
    #[test]
    fn every_injected_event_is_understood_by_the_receiver() {
        let document = settings_document("C:/bin/ccdesk.exe", false);
        let hooks = document.get("hooks").and_then(Value::as_object).unwrap();
        assert_eq!(hooks.len(), HOOK_EVENTS.len(), "number of injected hooks differs from the table");
        for (event, state) in HOOK_EVENTS {
            let command = hooks[event][0]["hooks"][0]["command"].as_str().unwrap();
            assert_eq!(
                command,
                format!("\"C:/bin/ccdesk.exe\" hook {event}"),
                "{event} has a different invocation shape"
            );
            assert_eq!(state_of(event), Some(state), "the receiver doesn't know {event}");
        }
        assert_eq!(state_of("PreToolUse"), None, "received an unregistered hook");
    }

    /// **道具ごとに飛ぶイベントは登録しない**（hook は毎回 ccdesk を 1 プロセス
    /// 起こすので、turn より細かい粒度を足すとセッションが目に見えて遅くなる）
    #[test]
    fn only_turn_level_events_are_injected() {
        for event in ["PreToolUse", "PostToolUse", "PreCompact", "SubagentStop"] {
            assert!(
                !HOOK_EVENTS.iter().any(|(name, _)| *name == event),
                "{event} is not turn-level"
            );
        }
    }

    /// hook は常に注入し、statusLine は opt-in のときだけ載る
    /// （使用率表示を切っていても state は取れる）
    #[test]
    fn hooks_are_always_injected_and_the_status_line_is_opt_in() {
        let off = settings_document("C:/bin/ccdesk.exe", false);
        assert!(off.get("hooks").is_some(), "hooks were not injected");
        assert!(off.get("statusLine").is_none(), "injected statusLine even though not opt-in");

        let on = settings_document("C:/bin/ccdesk.exe", true);
        assert_eq!(on.get("hooks"), off.get("hooks"), "hook shape changed under opt-in");
        assert_eq!(
            on["statusLine"]["command"].as_str(),
            Some("\"C:/bin/ccdesk.exe\" statusline-hook")
        );
    }

    /// 受けた state は保管へ載り、TUI 側の読みで同じ値が返る
    #[test]
    fn a_recorded_state_reaches_the_reader() {
        let temp = TempStore::new("a_recorded_state_reaches_the_reader");
        assert_eq!(states_at(&temp.path()), HookStates::default(), "not empty for a missing file");

        record(&temp.path(), &id("s-1"), "working", 1_000);
        record(&temp.path(), &id("s-2"), "blocked", 1_000);
        assert_eq!(
            states_at(&temp.path()),
            HookStates::from_pairs([("s-1", "working"), ("s-2", "blocked")])
        );

        // 同じセッションの次のイベントは上書き（状態は最後に受けたものが正しい）
        record(&temp.path(), &id("s-1"), "done", 2_000);
        let states = states_at(&temp.path());
        assert_eq!(states.get(&id("s-1")), Some("done"));
        assert_eq!(states.get(&id("s-2")), Some("blocked"), "affected another session");
        assert_eq!(states.get(&id("s-3")), None, "answered for an unknown session");
    }

    /// **古い項目は書くたびに落ちる**（1 セッション 1 項目で永久に積もらない）。
    /// 落ちるのは保つ期間を過ぎたものだけで、動いているセッションは毎 turn
    /// 書き直されるので落ちない
    #[test]
    fn recording_drops_entries_older_than_the_keep_window() {
        let temp = TempStore::new("recording_drops_entries_older_than_the_keep_window");
        let keep = KEEP.as_millis() as u64;
        record(&temp.path(), &id("old"), "done", 0);
        record(&temp.path(), &id("fresh"), "working", keep);
        // old は keep をちょうど過ぎた時点で落ちる
        record(&temp.path(), &id("now"), "blocked", keep + 1);
        let states = states_at(&temp.path());
        assert_eq!(states.get(&id("old")), None, "an entry past the keep window remains");
        assert_eq!(states.get(&id("fresh")), Some("working"), "dropped an entry still within the window");
        assert_eq!(states.get(&id("now")), Some("blocked"));
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
        assert_eq!(states_at(&temp.path()).get(&id("s")), Some("done"));
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
        record(&temp.path(), &id("s"), "working", 1_000);
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
        record(&temp.path(), &id("s"), "working", 1_000);
        let before = std::fs::read(temp.path()).unwrap();

        let held = Lock::acquire(&lock_path_for(&temp.path()), Duration::ZERO, LOCK_STALE).unwrap();
        let started = std::time::Instant::now();
        record(&temp.path(), &id("s"), "done", 2_000);
        let waited = started.elapsed();
        drop(held);

        assert!(waited < Duration::from_secs(5), "wait was not bounded: {waited:?}");
        assert_eq!(std::fs::read(temp.path()).unwrap(), before, "wrote even though the lock wasn't acquired");
        // 解放後は通常どおり載る（ロックが理由で壊れているわけではない）
        record(&temp.path(), &id("s"), "done", 2_000);
        assert_eq!(states_at(&temp.path()).get(&id("s")), Some("done"));
    }
}
