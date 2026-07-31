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
//! ここが答えるのは 2 つ。どちらも**行に保存せず、そのつど引く**:
//! 状態（[`HookStates::get`]）・未読（[`HookStates::unread`]）。
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

/// 入力待ちを知らせる `Notification` の matcher（通知の種類ごとに発火を絞る。
/// 公式に文書化。値は完全一致で、`|` 区切りで複数指定できる）。
///
/// **絞らずに全部拾うと `idle_prompt`（60 秒放置の催促）が混ざり、ターンを終えた行が
/// 時間経過だけで入力待ちへ落ちる**。実害は 2 つあった: 「Needs input」が
/// 「claude が止まっている」意味を失い、既読にした行の未読印が復活した。
///
/// 拾うのは**ユーザーが動くまで進まない**通知だけ。`auth_success` / `agent_completed` /
/// `elicitation_complete` は完了・情報通知なので拾わない（完了は `Stop` が答える）。
///
/// **`worker_permission_prompt` と `elicitation_url_dialog` も拾う。** どちらも
/// claude が実際に撃つ通知種別だが、**claude 自身の文書化された matcher 一覧には
/// 載っていない**（バイナリ内の文字列で存在を確認済み）。`worker_permission_prompt` は
/// チームのワーカーが許可を求めたとき（`${agent_id} needs permission for ${tool_name}` 等）
/// に飛ぶ。matcher は**完全一致**なので `permission_prompt` では拾えず、
/// 別の値として並べる必要がある。
///
/// **入力待ちの解除（`elicitation_response` 等）も拾わない。** 解除は
/// 「waiting より新しい busy 観測」の裁定（[`crate::poll::row_state`]）が担う:
/// 解除を表すイベントは全操作には存在しない（許可プロンプトの許可には無い）ので、
/// イベントを列挙しても状態機械は閉じない。解除の口を 2 系統持たない。
///
/// **縮退**: matcher が効かない版、またはここに載っていない通知種別では
/// `Notification` が一度も発火しない。それでも「情報なし」にはならない ＝
/// 直前の hook（`UserPromptSubmit` 等）が書いた `working` が残り、行は赤・明滅の
/// まま止まる（**取り逃すのは軽くない**。以前はここを「経過時間表示が古びるだけ」の
/// つもりで軽視していたが、経過時間表示は既に廃止済みで、実際の害はこちらの方だった）。
/// この赤固着は [`crate::poll::row_state`] の逆向き裁定則（より新しい非 `busy`
/// 観測で `working` から降りる）が受け皿になる ＝ ここで取りこぼしても
/// 次のポーリングで自己修復する
///
/// **`Elicitation` hook は今回入れない。** MCP の入力要求に反応する専用 hook が
/// 実在する（バイナリ内に確認済み）が、`PermissionRequest` と同種の decision hook
/// （空 stdout / exit 0 が無害か、6 秒ゲートが無いか等）の安全性検証をしていない。
/// 同種の口として存在することだけ書き残す
const ATTENTION_MATCHER: &str =
    "permission_prompt|elicitation_dialog|agent_needs_input|worker_permission_prompt|elicitation_url_dialog";

/// 注入する hook 1 件。**`--settings` の生成（[`inject_settings`]）と受け口
/// （[`run_hook`]）が同じ表を読む**ので、片方だけ増えた状態にならない。
struct HookEvent {
    event: &'static str,
    /// 発火を絞る matcher。None は全発火を拾う（そのイベント自体が 1 つの意味しか
    /// 持たないもの）
    matcher: Option<&'static str>,
    /// この hook が意味する state。[`crate::poll::classify`] が読む語彙
    /// （`waiting` / `working` / `completed` / `stopped` ＝ 画面に出る語の小文字）で、
    /// **要約文は持たない**（行に出るのは状態だけ）
    state: &'static str,
    /// この hook が**ユーザーの見るべき新しい出来事**か（未読 `●` の材料）。
    /// 状態と未読を同じ時刻で判定していた頃は、`SessionEnd` の記録が
    /// 「claude が何か言った」に数えられ、stop しただけの行が再起動後に未読になった
    activity: bool,
}

/// 注入する hook の表。同じイベント名を複数載せてよい（matcher で発火を分ける形を
/// 注入・受け口とも受け付ける）。
///
/// **原則は turn 単位のイベントだけを載せる。** hook は毎回 ccdesk を 1 プロセス起こすので、
/// `PreToolUse` / `PostToolUse` のような道具ごとに飛ぶイベントを足すと、Windows の
/// プロセス起動コストがそのままセッションの遅さになる。
///
/// **`PermissionRequest` はこの原則の例外**（`only_turn_level_events_are_injected` は
/// これを禁止リストに入れていない）。道具ごとに飛ぶように見えるが、実測では
/// 発火の上限は「ユーザーが手で許可に答える回数」であって道具の呼び出し回数ではない
/// （Bash を 3 回連発しても `PreToolUse` は 3 回・`PermissionRequest` は自動承認された
/// ものを除いた 1 回だけ）。turn より頻繁になり得るのは事実だが、頻度の実体は
/// 「ユーザーへの割り込み」であって「道具の呼び出し」ではないので、この原則が
/// 避けたい害（turn の何倍もプロセスを起こす）には当たらない
const HOOK_EVENTS: [HookEvent; 7] = [
    // 起動直後・再開直後はまだプロンプトを受けていない ＝ 入力待ち。
    // ユーザー自身の操作なので未読にはしない
    HookEvent { event: "SessionStart", matcher: None, state: WAITING, activity: false },
    HookEvent { event: "UserPromptSubmit", matcher: None, state: WORKING, activity: false },
    HookEvent { event: "Notification", matcher: Some(ATTENTION_MATCHER), state: WAITING, activity: true },
    // 道具の許可ダイアログが実際に表示されるときだけ飛ぶ（公式カタログ:
    // "Run before permission prompt"）。`Notification` の 2 つの matcher
    // （`permission_prompt` / `worker_permission_prompt`）と役割が重なるが、
    // こちらは黄（waiting）の表示が確定するまでの遅れが無い（`Notification` は
    // 6 秒ゲートの後にしか来ない版がある）。**matcher は None（全ツール）**:
    // 道具ごとに絞る意味が無い（どの道具の許可でも待っているのはユーザー）
    //
    // **未検証のリスク**: `crate::poll::row_state` の裁定則（hook の `waiting` は
    // より新しい `busy` 観測に負ける）と噛み合うかは実測できていない。
    // 許可ダイアログの表示中も `claude agents --json` の `status` が `busy` を
    // 返し続けるなら、この hook が書いた waiting が次のポーリング（最大 2 秒後）で
    // 覆り、黄がまた赤へ戻ってしまう。ダイアログ中に `status` を握って
    // 意図的に安全なプロンプト・許可待ちを再現する手段が無く（この環境の
    // permission mode は自動承認が広く効いていて、実際にダイアログを止められなかった）
    // 確認できなかった。**傍証**: claude の生存セッション一覧の項目は
    // `status`（`busy`/`shell`/`idle`/`waiting` の 4 値）とは別に `waitingFor`
    // という専用フィールドを持つ（バイナリの文字列で確認済み）。「待っている」を
    // 表す状態と、それを説明する専用フィールドがわざわざ両方あるのは、
    // 許可待ちのような「ユーザーの決定待ち」を `busy` と区別して表すためだと
    // 考えるのが自然で、既存コードの doc（`waiting` ＝ 確認待ち）とも整合するが、
    // **実測による裏取りではない**。次に触る人はここを疑ってよい
    HookEvent { event: "PermissionRequest", matcher: None, state: WAITING, activity: true },
    HookEvent { event: "Stop", matcher: None, state: COMPLETED, activity: true },
    // Stop の代わりに、API エラー（rate limit・認証失敗・max_output_tokens 等）で
    // ターンが終わったときだけ飛ぶ（claude の公式カタログに文書化: "Fires instead of
    // Stop when an API error ended the turn"）。**state は Stop と同じ completed**:
    // `Group::Completed` の定義は「ターンが終わった」であって「成功した」ではないので、
    // エラーで終わったターンもここに収まる。別の Group を新設するのは、成否の区別を
    // 導入するための Group 全体の設計変更になり、この 1 件のためには釣り合わない。
    // activity も Stop と同じ true: max_output_tokens（出力が長くて打ち切られた）が
    // 含まれるのが効く経路で、ユーザーが見るべき出来事という点は Stop と変わらない
    HookEvent { event: "StopFailure", matcher: None, state: COMPLETED, activity: true },
    // プロセスの終了は「claude が何か言った」ではない
    HookEvent { event: "SessionEnd", matcher: None, state: STOPPED, activity: false },
];

/// state 引数を持たない旧形式 `ccdesk hook <event>` の解決表。
///
/// 旧 settings で起きた claude セッション（注入ファイルは起動時に読まれるので、
/// ccdesk を更新しても走行中のセッションは旧コマンドを呼び続ける）のための
/// 後方互換で、**[`HOOK_EVENTS`] の並び順から独立させる**: 「表の最初の一致」で
/// 解決すると、並べ替えが旧 settings の意味を黙って変える。
/// ここの組はすべて [`HOOK_EVENTS`] に存在しなければならない（テストが固定する）
const LEGACY_HOOK_STATES: [(&str, &str); 5] = [
    ("SessionStart", WAITING),
    ("UserPromptSubmit", WORKING),
    // 旧形式の Notification は waiting 系 matcher でしか注入されていない
    ("Notification", WAITING),
    ("Stop", COMPLETED),
    ("SessionEnd", STOPPED),
];

/// 保管ファイルのトップレベルキー（`{"states": { "<session-id>": { … } }}`）
const STATES_KEY: &str = "states";
/// 項目のキー。**読みと書きで同じ定数を使う**（片側だけ直した状態を作らない）
const STATE_KEY: &str = "state";
const AT_KEY: &str = "at";
const PID_KEY: &str = "pid";
const ACTIVITY_AT_KEY: &str = "activity_at";


/// 受けた state を保つ期間。**動いているセッションは毎 turn 書き直す**ので、
/// これを過ぎた項目は既に終わったセッションのもの ＝ 読んでも意味が無い。
/// 消さないと 1 セッション 1 項目で永久に積もる
const KEEP: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// 未来の記録を受け入れる猶予。
///
/// 時計が巻き戻る（NTP 補正）と、巻き戻し前に書かれた記録の `at` が「未来」になる。
/// そのまま読むと再起動後の `launched_at` より新しい ＝ **前回の実行の state が
/// 今の実行のものとして誤帰属され**（稼働中の行が Stopped 表示に固まる）、
/// [`KEEP`] の掃除でも経過 0 扱いで永久に落ちない。未来の記録は読みの時点で捨てる。
///
/// 猶予の幅は、同一マシン内の書き手同士のわずかな前後（読み手が `now` を取った後に
/// 別の hook が書く）を誤って捨てないためのもの
const FUTURE_SKEW: Duration = Duration::from_secs(60);

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
    /// **ユーザーの見るべき新しい出来事**が最後に起きた時刻（未読 `●` の材料）。
    /// activity を持たない hook（[`HookEvent::activity`] が false）は前回の値を
    /// 引き継ぐ ＝ `Stop(completed)` の後に `SessionEnd(stopped)` が来ても
    /// 「まだ見ていない完了」の記録は消えない。None は一度も起きていない
    /// （旧形式の保管を含む）
    activity_at: Option<u64>,
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
    ///
    /// 返り値は (state, 記録時刻)。時刻は `status` の観測時刻との新旧裁定
    /// （[`crate::poll::row_state`]）の材料
    pub(crate) fn get(&self, id: &SessionId, launched: Option<u64>) -> Option<(&str, u64)> {
        let entry = self.0.get(id)?;
        (entry.at >= launched?).then_some((entry.state.as_str(), entry.at))
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
    /// **ユーザーの見るべき出来事（入力待ち・ターン完了）が、最後にその行を
    /// 開いた後に起きたか**で決まる。材料は hook の `activity_at` だけで、
    /// 行の `updated_at` も state の `at` も見ない。だから:
    ///
    /// - ピン留め・メニュー操作など**ユーザー自身の操作では未読にならない**
    ///   （行を書き換えても hook の記録は動かない）
    /// - **ccdesk を起動し直しただけでも未読にならない**（`last_opened_at` は
    ///   保管されるので、記録がそれより古ければ既読のまま）
    /// - **stop やアプリ終了でも未読にならない**（`SessionEnd` は状態を stopped に
    ///   するだけで activity を持たない。`at` で判定していた頃は、終了の記録が
    ///   「claude が何か言った」に数えられ、再起動後に未読が生えた）
    pub(crate) fn unread(&self, row: &SessionRow) -> bool {
        self.0
            .get(&row.session_id)
            .and_then(|entry| entry.activity_at)
            .is_some_and(|at| at > row.last_opened_at)
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

    /// テスト用の組み立て。**activity_at は at と同じ値**で入る（「その hook が
    /// 未読の材料でもある」素朴な形。区別が要るテストは [`record`] を通して作る）
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
                            activity_at: Some(at),
                        },
                    )
                })
                .collect(),
        )
    }
}

/// (イベント名, state 引数) → (state, activity)。**受理するのは [`HOOK_EVENTS`] に
/// ある組だけ**（知らない名前・表に無い組で呼ばれても何も書かない ＝ 表が正本のまま）。
///
/// state 引数なしは旧形式（[`LEGACY_HOOK_STATES`]）で state を補ってから同じ表を引く
fn resolve(event: &str, state: Option<&str>) -> Option<(&'static str, bool)> {
    let state = match state {
        Some(state) => state,
        None => {
            LEGACY_HOOK_STATES
                .iter()
                .find(|(name, _)| *name == event)
                .map(|(_, state)| *state)?
        }
    };
    HOOK_EVENTS
        .iter()
        .find(|row| row.event == event && row.state == state)
        .map(|row| (row.state, row.activity))
}

/// `ccdesk hook <event> <state>`。**注入した hook の受け口**（ユーザーは直接使わない）。
/// state 引数なしは旧 settings からの呼び出し（後方互換。[`LEGACY_HOOK_STATES`]）。
///
/// claude は hook の入力を stdin の JSON で渡すので、そこから `session_id` を取る
/// （どのセッションの state かは呼び出し側からしか分からない）。
///
/// **fail-open**: 何が起きても `Ok` で返り、**標準出力へ何も書かない**。
/// `UserPromptSubmit` の標準出力はそのままセッションの文脈へ足されるため、
/// ここが何か書くと ccdesk がユーザーの会話に割り込むことになる
pub(crate) fn run_hook(event: &str, state: Option<&str>) -> anyhow::Result<()> {
    use std::io::Read as _;
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let Some((session_id, state, activity, pid)) = hook_entry(event, state, &input) else {
        return Ok(());
    };
    if let Some(path) = ccdesk::hook_states_path() {
        record(&path, &session_id, state, activity, now_ms(), pid);
    }
    Ok(())
}

/// hook 1 回で保管へ載せるもの（イベント名・state 引数・stdin・環境から決まる）。
/// **[`run_hook`] が持つ判断はこれだけ**で、残りはファイルの読み書き ＝
/// 環境から pid を拾う経路も含めて、保管を触らずに検査できる。
///
/// 知らないイベント / 表に無い (event, state) / `session_id` が読めない入力は
/// None（何も書かない）
fn hook_entry(
    event: &str,
    state: Option<&str>,
    input: &str,
) -> Option<(SessionId, &'static str, bool, Option<u32>)> {
    let (state, activity) = resolve(event, state)?;
    let session_id = session_id_of(input)?;
    Some((session_id, state, activity, claude_pid()))
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
/// activity を持たない hook は前回の `activity_at` を引き継ぐ（状態の上書きで
/// 未読の記録を消さない。[`Entry::activity_at`]）。
///
/// 古い項目はここで落とす（[`KEEP`]）。掃除の契機を別に持たないのは、
/// 書くのがこの 1 箇所だけで、**書くたびに掃除すれば積もらない**ため
fn record(path: &Path, session_id: &SessionId, state: &str, activity: bool, now: u64, pid: Option<u32>) {
    let Ok(_guard) = Lock::acquire(&lock_path_for(path), LOCK_WAIT, LOCK_STALE) else {
        return;
    };
    let mut entries = read_entries(path, now);
    entries.retain(|_, entry| now.saturating_sub(entry.at) < KEEP.as_millis() as u64);
    let activity_at = if activity {
        Some(now)
    } else {
        entries.get(session_id).and_then(|entry| entry.activity_at)
    };
    entries.insert(
        session_id.clone(),
        Entry {
            state: state.to_string(),
            at: now,
            pid,
            activity_at,
        },
    );
    let document = json!({
        STATES_KEY: entries
            .iter()
            .map(|(id, entry)| (id.to_string(), json!({
                STATE_KEY: entry.state,
                AT_KEY: entry.at,
                PID_KEY: entry.pid,
                ACTIVITY_AT_KEY: entry.activity_at,
            })))
            .collect::<serde_json::Map<_, _>>()
    });
    let _ = write_json_atomically(path, &document);
}

/// 保管ファイルの項目（`session_id` → [`Entry`]）。
/// **無い・壊れている・書き換え途中はすべて空**（起動も turn も止めない）。
///
/// キーは最初から [`SessionId`] で組む（読み手がもう一度 B-tree を組み直さない）。
/// `now` より未来（[`FUTURE_SKEW`] を超える）の記録はここで捨てる ＝
/// 時計の巻き戻しで残った前回実行の記録が、読み・書きのどちらの経路にも乗らない
fn read_entries(path: &Path, now: u64) -> BTreeMap<SessionId, Entry> {
    let Some(value) = ccdesk::read_json(path) else {
        return BTreeMap::new();
    };
    let Some(states) = value.get(STATES_KEY).and_then(Value::as_object) else {
        return BTreeMap::new();
    };
    let horizon = now.saturating_add(FUTURE_SKEW.as_millis() as u64);
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
            // 旧形式の保管（キーが無い）は None で読む ＝ 未読は付かない。
            // 移行データ（KEEP=7日）としてそのまま許容する
            let activity_at = entry.get(ACTIVITY_AT_KEY).and_then(Value::as_u64);
            (!id.is_empty() && !state.is_empty() && at <= horizon).then(|| {
                (
                    SessionId::new(id.clone()),
                    Entry {
                        state: state.to_string(),
                        at,
                        pid,
                        activity_at,
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
    HookStates(read_entries(path, now_ms()))
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
    // 内容は exe パスにしか依存せず、走っているプロセスの `current_exe()` は
    // 自己更新でも変わらない ＝ **1 プロセス 1 回書けば足りる**。
    // 失敗はキャッシュしない（次のセッション起動で再試行する）
    static PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    if let Some(path) = PATH.get() {
        return Some(path.clone());
    }
    let path = write_inject_settings()?;
    let _ = PATH.set(path.clone());
    Some(path)
}

/// 注入ファイルの実書き込み。
///
/// **書き方は lib の 1 実装（tmp → rename）。** 素の `fs::write`（truncate →
/// write）だと、複数インスタンスの同時起動で、書いている最中の空/部分 JSON を
/// 別インスタンスが起こした claude が `--settings` として読み、そのセッションの
/// state hook が黙って消える。
/// 失敗も黙らない: hooks 無しで起動すると行の状態が縮退するので、ログに 1 行残す
/// （呼び手は None を受けて下部バーにも出す。[`crate::app`]）
fn write_inject_settings() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = ccdesk::ccdesk_dir()?;
    let exe_fwd = exe.to_string_lossy().replace('\\', "/");
    let path = dir.join("inject-settings.json");
    if let Err(e) = write_json_atomically(&path, &settings_document(&exe_fwd)) {
        ccdesk::log_error(&format!("could not write the hook settings: {e}"));
        return None;
    }
    Some(path)
}

/// 注入する hook 1 本あたりのタイムアウト（秒）。claude 側の既定は 600 秒だが、
/// ccdesk の hook は実測 170〜190ms（JSON を tmp → rename で 1 回書くだけ）なので、
/// 5 秒あれば固まった場合との区別に十分な余裕がある。
///
/// **万一 ccdesk の hook が固まると、`PermissionRequest` は表示前に待つ経路がある**
/// （サブエージェント生成コンテキストの `awaitAutomatedChecksBeforeDialog`）ので、
/// 既定の 600 秒のままだと許可ダイアログ自体が最大 600 秒出てこない。
/// タイムアウトを明示するのはこの経路への保険。
///
/// **全 hook に一律で出す**: イベントごとに待って良い長さを変える理由が無い
/// （どのイベントも同じ 1 プロセス起動＋ JSON 書き込みだけ）ので、`HookEvent` に
/// フィールドを足して行ごとに持たせるより、注入する側でまとめて出す方が
/// 構造が増えない
const HOOK_TIMEOUT_SECS: u64 = 5;

/// 注入ファイルの中身（[`inject_settings`] の判断だけを取り出したもの。
/// ファイルを書かずに検査できる）
fn settings_document(exe_fwd: &str) -> Value {
    // イベント名をキーに、表の行を**配列へ足し込む**（同名イベントを 2 枚
    // 載せられる形。キー単位の `.collect()` だと同名の後の行が前の行を黙って潰す）
    let mut hooks = serde_json::Map::new();
    for row in &HOOK_EVENTS {
        // matcher を持つ行だけ `matcher` を載せる。
        // **空文字を載せない**（`""` は「何にも一致しない」とも
        // 「全部に一致」とも読めるので、意図が伝わらない形にしない）
        let mut group = serde_json::Map::new();
        if let Some(matcher) = row.matcher {
            group.insert("matcher".to_string(), json!(matcher));
        }
        group.insert(
            "hooks".to_string(),
            json!([{
                "type": "command",
                // state まで運ぶ（受け口は (event, state) の組で表を引く。
                // 同名イベントの 2 枚をイベント名だけでは区別できない）
                "command": format!("\"{exe_fwd}\" hook {} {}", row.event, row.state),
                "timeout": HOOK_TIMEOUT_SECS,
            }]),
        );
        if let Value::Array(groups) = hooks
            .entry(row.event.to_string())
            .or_insert_with(|| Value::Array(Vec::new()))
        {
            groups.push(Value::Object(group));
        }
    }
    let mut settings = serde_json::Map::new();
    settings.insert("hooks".to_string(), Value::Object(hooks));
    Value::Object(settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト専用の保管先。**実ユーザーの `~/.ccdesk` を絶対に触らない**ための境界
    /// （安全な置き場の実装は [`crate::testutil::TempDir`] 1 つ）
    struct TempStore(crate::testutil::TempDir);

    impl TempStore {
        fn new(test: &str) -> Self {
            Self(crate::testutil::TempDir::new("hooks", test))
        }

        fn path(&self) -> PathBuf {
            self.0.join("hook-states.json")
        }
    }

    fn id(text: &str) -> SessionId {
        SessionId::new(text)
    }

    /// 保管に載っている state（時刻の新旧は問わない）。**新旧の判断そのものは
    /// [`a_hook_state_belongs_to_the_run_that_was_launched_before_it`] が固定する**ので、
    /// 保管の読み書きを見る他のテストは起動時刻 0（＝ 何でも受ける）で引く
    fn stored(states: &HookStates, id: &SessionId) -> Option<String> {
        states.get(id, Some(0)).map(|(state, _)| state.to_string())
    }

    /// **注入する表と受け口が同じ表を読む。** 片方だけ知っているイベントがあると、
    /// 注入したのに何も起きない（または登録されていない口が残る）。
    /// 同名イベント（`Notification` の 2 枚）は**同じキーの配列に並ぶ**
    /// （キー単位で潰すと後の行が前の行を黙って消す）
    #[test]
    fn every_injected_event_is_understood_by_the_receiver() {
        let document = settings_document("C:/bin/ccdesk.exe");
        let hooks = document.get("hooks").and_then(Value::as_object).unwrap();
        let mut seen = 0;
        for row in &HOOK_EVENTS {
            let groups = hooks[row.event].as_array().unwrap();
            let group = groups
                .iter()
                .find(|group| {
                    group["hooks"][0]["command"].as_str()
                        == Some(&format!("\"C:/bin/ccdesk.exe\" hook {} {}", row.event, row.state))
                })
                .unwrap_or_else(|| panic!("{} {} is not injected", row.event, row.state));
            // matcher は表のとおりに載る（持たない行にはキー自体を書かない）
            assert_eq!(
                group.get("matcher").and_then(Value::as_str),
                row.matcher,
                "{} {} has a different matcher",
                row.event,
                row.state
            );
            // 注入した組は受け口が受理し、activity まで表のとおりに引ける
            assert_eq!(
                resolve(row.event, Some(row.state)),
                Some((row.state, row.activity)),
                "the receiver doesn't accept {} {}",
                row.event,
                row.state
            );
            seen += 1;
        }
        let injected: usize = hooks.values().map(|groups| groups.as_array().unwrap().len()).sum();
        assert_eq!(injected, seen, "injected a hook that is not in the table");
        // 表に無い組・知らないイベントは受けない
        assert_eq!(resolve("PreToolUse", None), None, "received an unregistered hook");
        assert_eq!(resolve("Notification", Some(STOPPED)), None, "accepted a pair not in the table");
    }

    /// **`StopFailure` は Stop の代わりに、API エラーでターンが終わったときだけ飛ぶ。**
    /// state は Stop と同じ `completed`（`Group::Completed` の定義は「ターンが終わった」
    /// であって「成功した」ではないので、エラーで終わったターンもここに収まる）。
    /// activity も Stop と同じ true（max_output_tokens 等、ユーザーが見るべき出来事
    /// という点は変わらない）。旧形式（[`LEGACY_HOOK_STATES`]）には無い ＝
    /// この hook を知らない旧セッションが呼ぶことは無いので、後方互換の組は要らない
    #[test]
    fn stop_failure_is_treated_like_stop() {
        assert_eq!(resolve("StopFailure", Some(COMPLETED)), Some((COMPLETED, true)));
        assert!(
            !LEGACY_HOOK_STATES.iter().any(|(event, _)| *event == "StopFailure"),
            "StopFailure does not need a legacy form (it is a new hook)"
        );
    }

    /// **`PermissionRequest` は道具ごとに飛びうるが、turn 単位の原則の例外として
    /// 意図的に登録してある。** `only_turn_level_events_are_injected` がこれを
    /// 禁止リストに入れていないのは見落としではなく、[`HOOK_EVENTS`] の doc が言う
    /// 判断（頻度の実体は「ユーザーへの割り込み」であって「道具の呼び出し」ではない）
    /// をこのテストでも固定しておく。matcher は None（全ツール共通）、
    /// 旧形式（[`LEGACY_HOOK_STATES`]）には無い（新しい hook なので旧セッションが
    /// 呼ぶことは無い）
    #[test]
    fn permission_request_is_registered_as_a_deliberate_exception() {
        let row = HOOK_EVENTS
            .iter()
            .find(|row| row.event == "PermissionRequest")
            .expect("PermissionRequest is not registered");
        assert_eq!(row.matcher, None, "PermissionRequest should not filter by tool");
        assert_eq!(row.state, WAITING);
        assert!(row.activity, "a permission wait is something the user should see");
        assert!(
            !LEGACY_HOOK_STATES.iter().any(|(event, _)| *event == "PermissionRequest"),
            "PermissionRequest does not need a legacy form (it is a new hook)"
        );
    }

    /// **注入する全 hook のコマンドに、同じタイムアウトが一律で載る。**
    /// イベントごとに待って良い長さを変える理由が無いので、`HookEvent` に
    /// フィールドを足さず [`settings_document`] 側でまとめて出す設計を固定する
    #[test]
    fn every_injected_command_carries_the_same_timeout() {
        let document = settings_document("C:/bin/ccdesk.exe");
        let hooks = document.get("hooks").and_then(Value::as_object).unwrap();
        for (event, groups) in hooks {
            for group in groups.as_array().unwrap() {
                assert_eq!(
                    group["hooks"][0]["timeout"].as_u64(),
                    Some(HOOK_TIMEOUT_SECS),
                    "{event} does not carry the shared hook timeout"
                );
            }
        }
    }

    /// **state 引数の無い旧形式は、専用の表で解決する。** [`HOOK_EVENTS`] の並び順で
    /// 解決すると、並べ替えが旧 settings の意味を黙って変える。
    /// 旧形式の組はすべて注入表にも存在する（勝手な状態を作らない）
    #[test]
    fn a_legacy_call_resolves_from_its_own_table() {
        for (event, state) in LEGACY_HOOK_STATES {
            let resolved = resolve(event, None);
            assert_eq!(resolved.map(|(state, _)| state), Some(state), "{event} resolves differently");
            assert_eq!(
                resolved,
                resolve(event, Some(state)),
                "{event}'s legacy pair is missing from HOOK_EVENTS"
            );
        }
        // 旧形式の Notification は waiting（入力待ちの通知でしか注入されていなかった）
        assert_eq!(resolve("Notification", None), Some((WAITING, true)));
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

    /// **waiting に入れる `Notification` は「ユーザーが動くまで進まない」通知だけを拾う。**
    ///
    /// 絞らずに全部拾うと `idle_prompt`（60 秒放置の催促）が混ざり、ターンを終えた行が
    /// 時間経過だけで入力待ちへ落ちる ＝ 「Needs input」が「claude が止まっている」
    /// 意味を失い、既読にした行の未読印が復活する。**この 2 つは実際に起きた**
    #[test]
    fn the_notification_hook_ignores_the_idle_reminder() {
        let kinds: Vec<&str> = ATTENTION_MATCHER.split('|').collect();
        for wanted in [
            "permission_prompt",
            "elicitation_dialog",
            "agent_needs_input",
            // 公式の matcher 一覧には無いが実在する（バイナリの文字列で確認済み）
            "worker_permission_prompt",
            "elicitation_url_dialog",
        ] {
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

    /// **matcher は完全一致で、前方一致では拾えない。**
    /// `permission_prompt` を登録しただけでは `worker_permission_prompt` は
    /// 拾えない ＝ チームのワーカーが許可を求める通知を別途登録する必要があった
    /// 理由そのもの
    #[test]
    fn the_worker_permission_prompt_needs_its_own_entry_because_matchers_are_exact() {
        let kinds: Vec<&str> = ATTENTION_MATCHER.split('|').collect();
        assert!(
            kinds.contains(&"permission_prompt") && kinds.contains(&"worker_permission_prompt"),
            "both matchers should be listed as separate, exact entries"
        );
    }

    /// **入力待ちの解除（`elicitation_response` 等）を hook で拾わない。**
    /// 解除は「waiting より新しい busy 観測」の裁定（[`crate::poll::row_state`]）が
    /// 担う。イベントでも拾うと、解除の口が 2 系統になり
    /// （遅れた `working` が `Stop → completed` を上書きする競合も増える）、
    /// しかもイベントが存在しない操作（許可プロンプトの許可）には効かない
    #[test]
    fn no_hook_resumes_a_waiting_row() {
        assert!(
            !HOOK_EVENTS
                .iter()
                .any(|row| row.event == "Notification" && row.state == WORKING),
            "a resume notification is registered; the arbitration in row_state owns that"
        );
    }

    /// **道具ごとに飛ぶイベントは登録しない**（hook は毎回 ccdesk を 1 プロセス
    /// 起こすので、turn より細かい粒度を足すとセッションが目に見えて遅くなる）。
    ///
    /// **`PermissionRequest` をここで禁止していないのは見落としではない。**
    /// 見た目は道具ごとのイベントだが、意図的な例外として登録してある
    /// （[`HOOK_EVENTS`] の doc、および `permission_request_is_registered_as_a_deliberate_exception`
    /// を参照）
    #[test]
    fn only_turn_level_events_are_injected() {
        for event in ["PreToolUse", "PostToolUse", "PreCompact", "SubagentStop"] {
            assert!(
                !HOOK_EVENTS.iter().any(|row| row.event == event),
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

        record(&temp.path(), &id("s-1"), "working", true, 1_000, None);
        record(&temp.path(), &id("s-2"), "blocked", true, 1_000, None);
        assert_eq!(
            states_at(&temp.path()),
            HookStates::from_entries([("s-1", "working", 1_000), ("s-2", "blocked", 1_000)])
        );

        // 同じセッションの次のイベントは上書き（状態は最後に受けたものが正しい）
        record(&temp.path(), &id("s-1"), "done", true, 2_000, None);
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
        // 起動より後に記録された ＝ 今の実行のもの（記録時刻も一緒に返る）
        assert_eq!(states.get(&id("s"), Some(1_000)), Some(("stopped", 2_000)));
        // 起動と同時刻も今の実行（時計の分解能で同じ ms に並び得る）
        assert_eq!(states.get(&id("s"), Some(2_000)), Some(("stopped", 2_000)));
        // 起動より前に記録された ＝ 前回の実行の残骸
        assert_eq!(states.get(&id("s"), Some(3_000)), None);
        // 窓が無い行は動いていない ＝ 保管の値は過去の実行のもの
        assert_eq!(states.get(&id("s"), None), None);
        // 記録の無い行はいつでも None（hook が一度も来ていない）
        assert_eq!(states.get(&id("other"), Some(0)), None);
    }

    /// **未読は「ユーザーの見るべき出来事が、最後に開いた後に起きたか」。**
    ///
    /// 材料が hook の `activity_at` だけなので、次の 2 つは**書けなくなっている**:
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

    /// **ユーザー操作起点の working（`UserPromptSubmit`）は状態を動かすが、未読は作らない。**
    ///
    /// ユーザー自身の操作で未読が生えると「claude が何か言った」印の意味が壊れる。
    /// activity を持たない記録は前回の `activity_at` を引き継ぐだけ
    #[test]
    fn an_answered_dialog_resumes_working_without_creating_unread() {
        let temp = TempStore::new("an_answered_dialog_resumes_working_without_creating_unread");
        let row = |last_opened_at| SessionRow {
            last_opened_at,
            ..SessionRow::new(id("s"), "C:\\dev\\app", 0)
        };
        // 質問ダイアログ表示（waiting・未読の材料）→ 回答（working・材料ではない）
        record(&temp.path(), &id("s"), WAITING, true, 1_000, None);
        record(&temp.path(), &id("s"), WORKING, false, 2_000, None);
        let states = states_at(&temp.path());
        assert_eq!(stored(&states, &id("s")).as_deref(), Some(WORKING), "the answer did not resume the state");
        // 未読の起点は質問の時刻のまま（回答の時刻に進まない）
        assert!(states.unread(&row(999)), "the question is no longer unread");
        assert!(!states.unread(&row(1_000)), "the answer itself created an unread mark");
    }

    /// **stop・アプリ終了は未読を作らず、消しもしない。**
    ///
    /// `SessionEnd(stopped)` が未読の材料に数えられていた頃は、stop した行が
    /// 再起動後に未読 ● で復活した（実際に報告された）。逆に、まだ見ていない
    /// 完了（`Stop(completed)`）の記録は stopped の上書きでも消えない
    #[test]
    fn a_session_end_neither_creates_nor_destroys_unread() {
        let temp = TempStore::new("a_session_end_neither_creates_nor_destroys_unread");
        let row = |last_opened_at| SessionRow {
            last_opened_at,
            ..SessionRow::new(id("s"), "C:\\dev\\app", 0)
        };
        // ターン完了（未読の材料）→ stop（材料ではない）
        record(&temp.path(), &id("s"), COMPLETED, true, 1_000, None);
        record(&temp.path(), &id("s"), STOPPED, false, 2_000, None);
        let states = states_at(&temp.path());
        assert_eq!(stored(&states, &id("s")).as_deref(), Some(STOPPED));
        // 完了を見ていない ＝ stop 後も未読のまま
        assert!(states.unread(&row(500)), "the unseen completion was destroyed by the stop");
        // 完了を見た後に stop ＝ 未読は生えない（stop の時刻 2_000 では判定しない）
        assert!(!states.unread(&row(1_500)), "the stop itself created an unread mark");

        // 一度も activity が無い行（起動して stop しただけ）はいつでも既読
        record(&temp.path(), &id("t"), WAITING, false, 3_000, None);
        record(&temp.path(), &id("t"), STOPPED, false, 4_000, None);
        assert!(
            !states_at(&temp.path()).unread(&SessionRow {
                last_opened_at: 0,
                ..SessionRow::new(id("t"), "C:\\dev\\app", 0)
            }),
            "a row with no activity became unread"
        );
    }

    /// **`activity_at` は保管を往復する。** 落ちると再起動のたびに未読が全部消える。
    /// 旧形式（キーが無い保管）は None で読む ＝ 未読は付かない（7 日で消える移行データ）
    #[test]
    fn the_activity_time_survives_a_round_trip() {
        let temp = TempStore::new("the_activity_time_survives_a_round_trip");
        record(&temp.path(), &id("s"), COMPLETED, true, 1_000, None);
        record(&temp.path(), &id("s"), STOPPED, false, 2_000, None);
        let row = SessionRow {
            last_opened_at: 500,
            ..SessionRow::new(id("s"), "C:\\dev\\app", 0)
        };
        assert!(states_at(&temp.path()).unread(&row), "activity_at did not survive the file");

        // 旧形式の項目（activity_at 無し）は未読にならない
        std::fs::write(
            temp.path(),
            r#"{"states":{"s":{"state":"completed","at":9000}}}"#,
        )
        .unwrap();
        assert!(!states_at(&temp.path()).unread(&row), "a legacy entry created an unread mark");
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
        let padded = hook_entry("SessionStart", None, input);
        let bare = claude_pid();
        unsafe { std::env::set_var(CLAUDE_PID_ENV, "not a number") };
        let broken = hook_entry("SessionStart", None, input);
        unsafe { std::env::remove_var(CLAUDE_PID_ENV) };
        let missing = hook_entry("SessionStart", None, input);

        // **記録に pid まで載る**（載らないと pid での引き当てが黙って効かなくなる）
        assert_eq!(
            padded,
            Some((id("s-1"), WAITING, false, Some(4242))),
            "the pid did not reach the record"
        );
        assert_eq!(bare, Some(4242), "the pid is not read from the environment");
        assert_eq!(
            broken,
            Some((id("s-1"), WAITING, false, None)),
            "built a pid out of something that is not a number"
        );
        assert_eq!(
            missing,
            Some((id("s-1"), WAITING, false, None)),
            "answered with a pid when the variable is not set"
        );
        // 知らないイベント / 読めない入力は何も書かない
        assert_eq!(hook_entry("PreToolUse", None, input), None);
        assert_eq!(hook_entry("SessionStart", None, "not json"), None);
    }

    /// **pid は保管を往復する。** ここが落ちると、ペイン内の `/resume` `/clear` に
    /// 気づく口（[`HookStates::session_of`]）が黙って効かなくなる
    #[test]
    fn the_pid_of_the_calling_claude_survives_a_round_trip() {
        let temp = TempStore::new("the_pid_of_the_calling_claude_survives_a_round_trip");
        record(&temp.path(), &id("s"), "working", true, 1_000, Some(4242));
        assert_eq!(
            states_at(&temp.path()),
            HookStates::from_records([("s", "working", 1_000, Some(4242))])
        );
        // pid の無い記録も読める（環境変数が取れなかった場合）
        record(&temp.path(), &id("s"), "done", true, 2_000, None);
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
        record(&temp.path(), &id("s"), "working", true, 1_000, Some(1));
        let before = stamp(&temp.path()).expect("no stamp after a write");
        record(&temp.path(), &id("s-2"), "blocked", true, 2_000, Some(2));
        assert_ne!(stamp(&temp.path()), Some(before), "the stamp did not move");
    }

    /// **古い項目は書くたびに落ちる**（1 セッション 1 項目で永久に積もらない）。
    /// 落ちるのは保つ期間を過ぎたものだけで、動いているセッションは毎 turn
    /// 書き直されるので落ちない
    #[test]
    fn recording_drops_entries_older_than_the_keep_window() {
        let temp = TempStore::new("recording_drops_entries_older_than_the_keep_window");
        let keep = KEEP.as_millis() as u64;
        record(&temp.path(), &id("old"), "done", true, 0, None);
        record(&temp.path(), &id("fresh"), "working", true, keep, None);
        // old は keep をちょうど過ぎた時点で落ちる
        record(&temp.path(), &id("now"), "blocked", true, keep + 1, None);
        let states = states_at(&temp.path());
        assert_eq!(stored(&states, &id("old")), None, "an entry past the keep window remains");
        assert_eq!(
            stored(&states, &id("fresh")).as_deref(),
            Some("working"),
            "dropped an entry still within the window"
        );
        assert_eq!(stored(&states, &id("now")).as_deref(), Some("blocked"));
    }

    /// **未来の記録は読まない。** 時計が巻き戻る（NTP 補正）と巻き戻し前の記録の
    /// `at` が未来になり、再起動後の `launched_at` より新しい ＝ **前回実行の
    /// state が今の実行に誤帰属される**（稼働中の行が Stopped 表示に固まる）。
    /// [`KEEP`] の掃除も経過 0 扱いで落とせないので、読みの時点で捨てる
    /// （書き直しも同じ読みを通るので、次の記録でファイルからも消える）
    #[test]
    fn records_from_a_future_clock_are_ignored_and_swept() {
        let temp = TempStore::new("records_from_a_future_clock_are_ignored_and_swept");
        let skew = FUTURE_SKEW.as_millis() as u64;
        record(&temp.path(), &id("future"), "stopped", true, 1_000_000, None);

        // 時計が 1_000 まで巻き戻った世界では、その記録は見えない
        assert_eq!(
            read_entries(&temp.path(), 1_000).get(&id("future")),
            None,
            "a record from a rolled-back clock's future is still being served"
        );
        // 猶予の内側（書き手同士のわずかな前後）は捨てない
        assert!(
            read_entries(&temp.path(), 1_000_000 - skew).contains_key(&id("future")),
            "a record within the skew allowance was dropped"
        );

        // 巻き戻った時計で次の記録が載ると、未来の記録はファイルからも落ちる
        record(&temp.path(), &id("s"), "working", true, 1_000, None);
        assert_eq!(
            stored(&states_at(&temp.path()), &id("future")),
            None,
            "the future record survived the next write"
        );
        assert_eq!(stored(&states_at(&temp.path()), &id("s")).as_deref(), Some("working"));
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
        record(&temp.path(), &id("s"), "working", true, 1_000, None);
        let leftovers: Vec<_> = std::fs::read_dir(temp.0.path())
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
        record(&temp.path(), &id("s"), "working", true, 1_000, None);
        let before = std::fs::read(temp.path()).unwrap();

        let held = Lock::acquire(&lock_path_for(&temp.path()), Duration::ZERO, LOCK_STALE).unwrap();
        let started = std::time::Instant::now();
        record(&temp.path(), &id("s"), "done", true, 2_000, None);
        let waited = started.elapsed();
        drop(held);

        assert!(waited < Duration::from_secs(5), "wait was not bounded: {waited:?}");
        assert_eq!(std::fs::read(temp.path()).unwrap(), before, "wrote even though the lock wasn't acquired");
        // 解放後は通常どおり載る（ロックが理由で壊れているわけではない）
        record(&temp.path(), &id("s"), "done", true, 2_000, None);
        assert_eq!(stored(&states_at(&temp.path()), &id("s")).as_deref(), Some("done"));
    }
}
