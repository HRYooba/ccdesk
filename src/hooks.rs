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
//! **hook はイベント、`~/.claude/sessions/` の `status` は現在値。**
//! 行の状態は 2 つを同じ語彙へ揃えたうえで**新しい方**を採る
//! （[`crate::poll::row_state`]）。hook の取り柄は 0 遅延、`status` の取り柄は
//! 取りこぼしても次の観測で必ず正しくなること ＝ どちらが欠けても縮退で済む。
//! **受けた state を行へ写さない**のが要点で、写していた頃は
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

use crate::poll::State;
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
/// **より新しい `status` の観測**（[`crate::poll::row_state`]）が担う:
/// 解除を表すイベントは全操作には存在しない（許可プロンプトの許可には無い）ので、
/// イベントを列挙しても状態機械は閉じない。解除の口を 2 系統持たない。
///
/// **縮退**: matcher が効かない版、またはここに載っていない通知種別では
/// `Notification` が一度も発火しない。それでも「情報なし」にはならない ＝
/// 直前の hook（`UserPromptSubmit` 等）が書いた `working` が残り、行は赤・明滅の
/// まま止まる（**取り逃すのは軽くない**。以前はここを「経過時間表示が古びるだけ」の
/// つもりで軽視していたが、経過時間表示は既に廃止済みで、実際の害はこちらの方だった）。
/// この赤固着は**次の `status` 観測**が受け皿になる（claude はダイアログが開いて
/// いる間その旨を自分で報告する）＝ ここで取りこぼしても最大 1 周期で正しい色になる
///
/// **`Elicitation` hook は今回入れない。** MCP の入力要求に反応する専用 hook が
/// 実在する（バイナリ内に確認済み）が、`PermissionRequest` と同種の decision hook
/// （空 stdout / exit 0 が無害か、6 秒ゲートが無いか等）の安全性検証をしていない。
/// 同種の口として存在することだけ書き残す
const ATTENTION_MATCHER: &str =
    "permission_prompt|elicitation_dialog|agent_needs_input|worker_permission_prompt|elicitation_url_dialog";

/// 注入する hook 1 件。**`--settings` の生成（[`inject_settings`]）と受け口
/// （[`run_hook`]）が同じ表を読む**ので、片方だけ増えた状態にならない。
pub(crate) struct HookEvent {
    pub(crate) event: &'static str,
    /// 発火を絞る matcher。None は全発火を拾う（そのイベント自体が 1 つの意味しか
    /// 持たないもの）
    pub(crate) matcher: Option<&'static str>,
    /// この hook が意味する state。**要約文は持たない**（行に出るのは状態だけ）
    pub(crate) state: State,
    /// この hook が**ユーザーの見るべき新しい出来事**か（未読 `●` の材料）。
    /// 状態と未読を同じ時刻で判定していた頃は、`SessionEnd` の記録が
    /// 「claude が何か言った」に数えられ、stop しただけの行が再起動後に未読になった
    pub(crate) activity: bool,
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
pub(crate) const HOOK_EVENTS: [HookEvent; 7] = [
    // 起動直後・再開直後は**プロンプトで待機している**（claude は動いておらず、
    // ユーザーへの要求も無い）。ユーザー自身の操作なので未読にはしない。
    //
    // **`waiting` ではない。** 以前は入力待ちと書いていたので、停止したセッションを
    // 開き直すと（止める前が Idle でも）黄「Needs input」に変わり、そのまま
    // 固着していた。claude 自身もこの状態を `idle` と報告する（`waitingFor` は無い
    // ＝ 何も待っていない）ので、[`crate::poll::state_of_status`] の写し先と一致する
    HookEvent { event: "SessionStart", matcher: None, state: State::Idle, activity: false },
    HookEvent { event: "UserPromptSubmit", matcher: None, state: State::Working, activity: false },
    HookEvent { event: "Notification", matcher: Some(ATTENTION_MATCHER), state: State::Waiting, activity: true },
    // 道具の許可ダイアログが実際に表示されるときだけ飛ぶ（公式カタログ:
    // "Run before permission prompt"）。`Notification` の 2 つの matcher
    // （`permission_prompt` / `worker_permission_prompt`）と役割が重なるが、
    // こちらは黄（waiting）の表示が確定するまでの遅れが無い（`Notification` は
    // 6 秒ゲートの後にしか来ない版がある）。**matcher は None（全ツール）**:
    // 道具ごとに絞る意味が無い（どの道具の許可でも待っているのはユーザー）
    //
    // **この hook が書いた黄は、次の観測で赤へ戻ったりしない。** claude は
    // ダイアログが開いている間、種類を問わず `status` に `waiting` を報告する
    // （決定条件は [`crate::claude_format`]）ので、観測が追い越しても同じ答えになる。
    // かつてはここが「未検証のリスク」だった: ダイアログを意図的に出す手段が
    // 無かったため実測できず、`waitingFor` という専用フィールドの存在を傍証に
    // 置いていた。claude 本体の実装を読んで裏が取れたので、その但し書きは外した
    HookEvent { event: "PermissionRequest", matcher: None, state: State::Waiting, activity: true },
    HookEvent { event: "Stop", matcher: None, state: State::Idle, activity: true },
    // Stop の代わりに、API エラー（rate limit・認証失敗・max_output_tokens 等）で
    // ターンが終わったときだけ飛ぶ（claude の公式カタログに文書化: "Fires instead of
    // Stop when an API error ended the turn"）。**state は Stop と同じ idle**:
    // ターンが終わった以上その行に手は要らないので、成功でもエラーでもここに収まる。
    // 成否を区別する別の State を新設するのは、その区別を
    // 導入するための State 全体の設計変更になり、この 1 件のためには釣り合わない。
    // activity も Stop と同じ true: max_output_tokens（出力が長くて打ち切られた）が
    // 含まれるのが効く経路で、ユーザーが見るべき出来事という点は Stop と変わらない
    HookEvent { event: "StopFailure", matcher: None, state: State::Idle, activity: true },
    // プロセスの終了は「claude が何か言った」ではない
    HookEvent { event: "SessionEnd", matcher: None, state: State::Stopped, activity: false },
];

/// state 引数を持たない旧形式 `ccdesk hook <event>` の解決表。
///
/// 旧 settings で起きた claude セッション（注入ファイルは起動時に読まれるので、
/// ccdesk を更新しても走行中のセッションは旧コマンドを呼び続ける）のための
/// 後方互換で、**[`HOOK_EVENTS`] の並び順から独立させる**: 「表の最初の一致」で
/// 解決すると、並べ替えが旧 settings の意味を黙って変える。
/// ここの組はすべて [`HOOK_EVENTS`] に存在しなければならない（テストが固定する）
const LEGACY_HOOK_STATES: [(&str, State); 5] = [
    ("SessionStart", State::Idle),
    ("UserPromptSubmit", State::Working),
    // 旧形式の Notification は waiting 系 matcher でしか注入されていない
    ("Notification", State::Waiting),
    ("Stop", State::Idle),
    ("SessionEnd", State::Stopped),
];

/// 保管ファイルのトップレベルキー（`{"states": { "<session-id>": { … } }}`）
const STATES_KEY: &str = "states";
/// 項目のキー。**読みと書きで同じ定数を使う**（片側だけ直した状態を作らない）
const STATE_KEY: &str = "state";
const AT_KEY: &str = "at";
const ACTIVITY_AT_KEY: &str = "activity_at";
/// その hook が名乗った**会話 ID**（payload の `session_id`）。保管のキー ＝
/// ccdesk の行とは別物で、ペインの中の `/clear` `/resume` でこちらだけが変わる
const CONVERSATION_KEY: &str = "conversation";


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
    state: State,
    /// 受けた時刻（epoch ms）。**書き手（[`record`]）と同じ時計**
    at: u64,
    /// **ユーザーの見るべき新しい出来事**が最後に起きた時刻（未読 `●` の材料）。
    /// activity を持たない hook（[`HookEvent::activity`] が false）は前回の値を
    /// 引き継ぐ ＝ `Stop(idle)` の後に `SessionEnd(stopped)` が来ても
    /// 「まだ見ていない完了」の記録は消えない。None は一度も起きていない
    /// （旧形式の保管を含む）
    activity_at: Option<u64>,
    /// その hook が名乗った**会話 ID**（payload の `session_id`）。
    ///
    /// **キーとは別物。** 保管のキーは ccdesk の行 ID（`CCDESK_ROW`）で、
    /// 会話 ID は agent 側の世界の値。ペインの中で `/clear` `/resume` すると
    /// 同じ行の記録のままこちらだけが変わる ＝ **行の会話追随の唯一の材料**
    /// （[`crate::app`] の `adopt_conversations`）
    conversation: Option<String>,
}

impl Entry {
    /// この項目を書いたのが**ターンの終わりを名乗るイベント**（`Stop` /
    /// `StopFailure`）か。
    ///
    /// **state だけでは足りない。** [`HOOK_EVENTS`] で `Idle` を書くイベントは
    /// 3 つ（`SessionStart` / `Stop` / `StopFailure`）あり、`SessionStart` は
    /// ターンの終わりではない ＝ これを混ぜると**ペインの中の `/clear` `/resume`
    /// が完了通知になる**（[`crate::poll::RowState::interrupted`]）。
    ///
    /// 材料は `activity_at` で足りる: activity を持つ hook は `at` と同じ時刻で
    /// これを更新し、持たない hook は前回値を引き継ぐ（[`record`]）＝ 一致は
    /// 「この項目を書いたイベントが activity 付きだった」と同義。そして `Idle` を
    /// 書く 3 つのうち **activity を持たないのは `SessionStart` だけ**。
    ///
    /// この対応は表（[`HOOK_EVENTS`]）を書き換えれば崩れるので、崩れたら
    /// `only_a_turn_ending_event_marks_the_entry_as_ended` が落ちるようにしてある
    fn ended(&self) -> bool {
        self.state == State::Idle && self.activity_at == Some(self.at)
    }
}

/// hook が名乗った 1 件。**state と「誰が名乗ったか」を割らない**
/// （割ると、撃つ判断と写しの更新が別々の規則で決まる形になる）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Reported {
    pub(crate) state: State,
    /// 受けた時刻（epoch ms）。`status` の観測時刻とどちらが新しいかを見る材料
    pub(crate) at: u64,
    /// **ターンの終わりを名乗るイベントが書いたか**（[`Entry::ended`]）。
    /// 通知はこれを見て「完了」と「手が空いただけ」を分ける
    pub(crate) ended: bool,
}

#[cfg(test)]
impl Reported {
    /// テスト用: **ターンの終わりを名乗った** hook 1 件（`Idle` は `Stop` 相当）。
    /// `SessionStart` の `Idle`（＝ 終わりではない）を作るテストは
    /// 構造体リテラルで `ended: false` を明示する
    pub(crate) fn new(state: State, at: u64) -> Self {
        Self { state, at, ended: state == State::Idle }
    }
}

/// hook が書いた state の写し（`session_id` → [`Entry`]）
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct HookStates(BTreeMap<SessionId, Entry>);

impl HookStates {
    /// その行の hook 由来の state。`launched` は**その行を今動かしている窓を
    /// 起こした時刻**（窓が無ければ None）。**None は「使える hook が無い」**で、
    /// 呼び手（描画）はそのとき `status` の観測だけで決める。
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
    /// 返り値は [`Reported`]（state・記録時刻・ターンの終わりを名乗ったか）。
    /// 時刻は `status` の観測時刻とどちらが新しいかを見る材料
    /// （[`crate::poll::row_state`]）
    pub(crate) fn get(&self, id: &SessionId, launched: Option<u64>) -> Option<Reported> {
        let entry = self.0.get(id)?;
        (entry.at >= launched?).then_some(Reported {
            state: entry.state,
            at: entry.at,
            ended: entry.ended(),
        })
    }

    /// その行が**最後に名乗った会話**。
    ///
    /// 再開先（`claude -r <id>` / `codex resume <id>`）であり、ペインの中で
    /// `/clear` `/resume` `/new` を打ったときに行が追随する先でもある
    /// （[`crate::app`] の `adopt_conversations`）。
    ///
    /// **`launched` で絞らない**のが [`Self::get`] との違い: 状態は「前回の実行の
    /// 残骸」を捨てる必要があるが、会話は ccdesk の再起動をまたいで在り続けるので、
    /// 古い記録でも再開先として正しい
    pub(crate) fn conversation(&self, id: &SessionId) -> Option<&str> {
        self.0.get(id)?.conversation.as_deref()
    }

    /// 前回の写しと比べて、**新しく手が空いた行があるか**（使用率取得の合図）。
    ///
    /// 使用率はターンが終わった瞬間に動くので、そこで 1 回だけ取りたい
    /// （[`crate::usage`]。周期で叩き続けるより変わった直後の 1 回の方が正確で、
    /// 何もしていない間は claude を 1 プロセスも起こさない）。
    ///
    /// **「ターンが終わった」専用の state 値は持たない。** かつては `completed` という
    /// 5 番目の語を置いて厳密に turn の終わりだけを拾っていたが、状態の語彙に
    /// イベントを混ぜる代償に見合わなかった（claude 自身もそんな status を持たない）。
    /// 今は [`State::Idle`] への遷移で拾うので、**セッションの起動でも 1 回余分に撃つ**。
    /// 実害はほぼ無い: その瞬間はどのみち claude を起こしているし、別マシンで進んだ
    /// 消費が反映されるぶん、むしろ表示は新しくなる。
    ///
    /// **`at` まで比べる**のが要点: state だけを見ると、`idle` のまま残っている行が
    /// 毎周「今しがた空いた」と言い続ける
    pub(crate) fn any_row_went_idle_since(&self, previous: &Self) -> bool {
        self.0.iter().any(|(id, entry)| {
            entry.state == State::Idle && previous.0.get(id).map(|p| p.at) != Some(entry.at)
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

    /// テスト用の組み立て。**activity_at は at と同じ値**で入る（「その hook が
    /// 未読の材料でもある」素朴な形。区別が要るテストは [`record`] を通して作る）
    #[cfg(test)]
    pub(crate) fn from_entries<'a>(
        entries: impl IntoIterator<Item = (&'a str, State, u64)>,
    ) -> Self {
        Self::from_records(entries.into_iter().map(|(id, state, at)| (id, state, at, None)))
    }

    /// テスト用: 会話 ID まで指定した組み立て
    #[cfg(test)]
    pub(crate) fn from_records<'a>(
        entries: impl IntoIterator<Item = (&'a str, State, u64, Option<&'a str>)>,
    ) -> Self {
        Self(
            entries
                .into_iter()
                .map(|(id, state, at, conversation)| {
                    (
                        SessionId::new(id),
                        Entry {
                            state,
                            at,
                            activity_at: Some(at),
                            conversation: conversation.map(str::to_string),
                        },
                    )
                })
                .collect(),
        )
    }
}

/// **改名した state 値の読み替え表**（イベント名, 旧い綴り, 今の値）。
///
/// `--settings` に焼き込まれるコマンド文字列（[`settings_document`]）は
/// `ccdesk hook <event> <state>` で、**書いた時点のバイナリの [`HOOK_EVENTS`] の値**が
/// 入る。自己更新は exe を同じパスへ置き換える（[`crate::update`]）ので、更新前に
/// 起こした claude セッションが生き残っていると、**新しいバイナリへ古い綴りの引数が
/// 飛んでくる**（注入ファイルは claude の起動時に読まれるので、ファイル側を
/// 書き直しても間に合わない）。
///
/// [`LEGACY_HOOK_STATES`] では救えない: あちらは state 引数が**無い**さらに古い形式の
/// ための表で、「引数はあるが値が古い」呼び出しには効かない。
///
/// **表に無い組を受理しない**という規則は保ったまま、ここに載せた組だけを読み替える
/// （汎用のフォールバック ＝「知らない値は event の既定へ」にすると、
/// `Notification stopped` のような作られていない組まで通ってしまう）
const RENAMED_HOOK_STATES: [(&str, &str, State); 3] = [
    // 「起動直後は入力待ち」を「プロンプトで待機」へ改めた（[`HOOK_EVENTS`]）
    ("SessionStart", "waiting", State::Idle),
    // `completed` は語彙から外した（「ターンが終わった」は状態ではない ＝
    // [`crate::poll::State`]）。終わった後の行はプロンプトで待機している
    ("Stop", "completed", State::Idle),
    ("StopFailure", "completed", State::Idle),
];

/// (イベント名, state 引数) → (state, activity)。**受理するのは [`HOOK_EVENTS`] に
/// ある組だけ**（知らない名前・表に無い組で呼ばれても何も書かない ＝ 表が正本のまま）。
///
/// 引数の解決は 3 段: 改名された綴り（[`RENAMED_HOOK_STATES`]）→ 今の綴り →
/// 引数なしの旧形式（[`LEGACY_HOOK_STATES`]）
fn resolve(event: &str, state: Option<&str>) -> Option<(State, bool)> {
    let state = match state {
        Some(text) => renamed(event, text).or_else(|| State::parse(text))?,
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

/// 改名前の綴りで呼ばれていたら、今の値。そうでなければ None
fn renamed(event: &str, text: &str) -> Option<State> {
    RENAMED_HOOK_STATES
        .iter()
        .find(|(name, old, _)| *name == event && *old == text)
        .map(|(_, _, state)| *state)
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
    let Some(entry) = hook_entry(event, state, &input) else {
        return Ok(());
    };
    if let Some(path) = ccdesk::hook_states_path() {
        record(
            &path,
            &entry.row,
            entry.state,
            entry.activity,
            now_ms(),
            Some(entry.conversation.as_str()),
        );
    }
    Ok(())
}

/// hook 1 回で保管へ載せるもの（イベント名・state 引数・stdin・環境から決まる）。
/// **[`run_hook`] が持つ判断はこれだけ**で、残りはファイルの読み書き ＝
/// 環境から pid を拾う経路も含めて、保管を触らずに検査できる。
///
/// 知らないイベント / 表に無い (event, state) / `session_id` が読めない入力 /
/// **`CCDESK_ROW` が無い**呼び出しは None（何も書かない）
fn hook_entry(event: &str, state: Option<&str>, input: &str) -> Option<HookRecord> {
    let (state, activity) = resolve(event, state)?;
    let conversation = session_id_of(input)?;
    // **行を名指しできるのは env だけ。** payload が運ぶのは会話 ID で、行 ID は
    // どこにも出さない（[`ROW_ENV`]）。**payload へ落ちるフォールバックは持たない**:
    // 落ちれば行ではなく会話をキーにした記録ができ、その行は状態も未読も付かない
    // まま「動いているのに何も起きない」状態になる ＝ env の立て忘れが無音で
    // 効く。立て忘れは 1 箇所（[`crate::backend::Kind::spawn_command`]）でしか
    // 起こり得ないので、ここは黙らずに落とす
    let Some(row) = row_env() else {
        ccdesk::log_error(&format!(
            "hook {event} ran without {ROW_ENV}; the row it belongs to is unknown"
        ));
        return None;
    };
    Some(HookRecord {
        row,
        conversation,
        state,
        activity,
    })
}

/// hook 1 回ぶんの記録。**行と会話を分けて持つ**のが要点で、この 2 つが
/// 違い得ること（そして両方の agent で違うこと）が今の設計の核心（[`ROW_ENV`]）
#[derive(Debug, PartialEq, Eq)]
struct HookRecord {
    /// 保管のキー ＝ ccdesk の行
    row: SessionId,
    /// その時点で agent が動かしている会話（再開に使う）
    conversation: String,
    state: State,
    activity: bool,
}

/// ccdesk が起動時に立てた行 ID（[`ROW_ENV`]）。**空文字は無いものとして扱う**
/// （env が中途半端に立っている環境で、空のキーの記録を作らない）
fn row_env() -> Option<SessionId> {
    let value = std::env::var(ROW_ENV).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| SessionId::new(value))
}

/// hook 入力から `session_id` を取る。**これは会話 ID であって行 ID ではない**
/// （だから [`SessionId`] にしない ＝ 型で取り違えを止める）。
/// **読みは寛容**（形が変わっても落ちない: 取れなければ何も書かないだけ）
fn session_id_of(input: &str) -> Option<String> {
    let value: Value = serde_json::from_str(input).ok()?;
    let id = value.get("session_id").and_then(Value::as_str)?;
    (!id.is_empty()).then(|| id.to_string())
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
fn record(
    path: &Path,
    session_id: &SessionId,
    state: State,
    activity: bool,
    now: u64,
    conversation: Option<&str>,
) {
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
    // **取れなかったら前回の値を残す**（1 回でも拾えていれば再開できる）
    let conversation = conversation.map(str::to_string).or_else(|| {
        entries
            .get(session_id)
            .and_then(|entry| entry.conversation.clone())
    });
    entries.insert(
        session_id.clone(),
        Entry {
            state,
            at: now,
            activity_at,
            conversation,
        },
    );
    let document = json!({
        STATES_KEY: entries
            .iter()
            .map(|(id, entry)| (id.to_string(), json!({
                STATE_KEY: entry.state.as_str(),
                AT_KEY: entry.at,
                ACTIVITY_AT_KEY: entry.activity_at,
                CONVERSATION_KEY: entry.conversation,
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
            // **読めない語は項目ごと捨てる。** 語彙は [`State`] が正本で、
            // 知らない綴り（旧版が書いた語・壊れた値）は何も答えられない。
            // 捨てても live な行には影響しない: 読み手は必ず「今の窓を起こした時刻より
            // 新しい記録」しか採らない（[`HookStates::get`]）ので、古い綴りの記録は
            // どのみち前回以前の実行のもの
            let state = State::parse(entry.get(STATE_KEY).and_then(Value::as_str)?)?;
            // state を持たない項目は捨てる（state が無い項目は何も答えられない）。
            // 時刻は既定 0 で読む ＝ 次の書き込みで古い項目として落ちる
            let at = entry.get(AT_KEY).and_then(Value::as_u64).unwrap_or(0);
            // 旧形式の保管（キーが無い）は None で読む ＝ 未読は付かない。
            // 移行データ（KEEP=7日）としてそのまま許容する
            let activity_at = entry.get(ACTIVITY_AT_KEY).and_then(Value::as_u64);
            let conversation = entry
                .get(CONVERSATION_KEY)
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_string);
            (!id.is_empty() && at <= horizon).then(|| {
                (
                    SessionId::new(id.clone()),
                    Entry {
                        state,
                        at,
                        activity_at,
                        conversation,
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
pub(crate) fn inject_settings() -> Option<Injection> {
    // 内容は exe パスにしか依存せず、走っているプロセスの `current_exe()` は
    // 自己更新でも変わらない ＝ **1 プロセス 1 回書けば足りる**。
    // 失敗はキャッシュしない（次のセッション起動で再試行する）
    static DONE: std::sync::OnceLock<Injection> = std::sync::OnceLock::new();
    if let Some(done) = DONE.get() {
        return Some(done.clone());
    }
    let done = write_inject_settings()?;
    let _ = DONE.set(done.clone());
    Some(done)
}

/// 書き出し済みの hook 注入。**agent ごとに使う部分が違う**
/// （claude は `settings`、codex は `exe`。載せ方は
/// [`crate::backend::Backend::command`]）。
///
/// どちらも同じ 1 つの事実（ccdesk 実行ファイルの場所）から導かれるが、claude 側は
/// ファイルの書き出しを伴うので、書けたパスを一緒に運ぶ
#[derive(Clone)]
pub(crate) struct Injection {
    /// ccdesk 実行ファイル（`/` 区切り）
    pub(crate) exe: String,
    /// claude が `--settings` で読むファイル
    pub(crate) settings: PathBuf,
}

/// hook の子プロセスへ**行の identity を渡す**環境変数。
///
/// **行 ID がこの env 以外のどこにも出ないのが今の設計。** argv にも
/// transcript 名にも会話 ID にも出さないので、hook が「どの行の出来事か」を
/// 知る口はここ 1 本しかない。env は agent を経由して hook の子プロセスまで
/// 継承される（**claude・codex とも実測**）。
///
/// **両方の agent で立てる。** claude では「渡した UUID がそのまま payload の
/// `session_id` になる」ので要らない時期があったが、それは行 ID と会話 ID が
/// 同じ値だった頃の話で、ペインの中の `/clear` で崩れる前提だった。
/// 立てるのは共通の口 1 箇所（[`crate::backend::Kind::spawn_command`]）
pub(crate) const ROW_ENV: &str = "CCDESK_ROW";

/// 注入ファイルの実書き込み。
///
/// **書き方は lib の 1 実装（tmp → rename）。** 素の `fs::write`（truncate →
/// write）だと、複数インスタンスの同時起動で、書いている最中の空/部分 JSON を
/// 別インスタンスが起こした claude が `--settings` として読み、そのセッションの
/// state hook が黙って消える。
/// 失敗も黙らない: hooks 無しで起動すると行の状態が縮退するので、ログに 1 行残す
/// （呼び手は None を受けて下部バーにも出す。[`crate::app`]）
fn write_inject_settings() -> Option<Injection> {
    let exe = std::env::current_exe().ok()?;
    let dir = ccdesk::ccdesk_dir()?;
    let exe_fwd = exe.to_string_lossy().replace('\\', "/");
    let path = dir.join("inject-settings.json");
    if let Err(e) = write_json_atomically(&path, &settings_document(&exe_fwd)) {
        ccdesk::log_error(&format!("could not write the hook settings: {e}"));
        return None;
    }
    Some(Injection {
        exe: exe_fwd,
        settings: path,
    })
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
pub(crate) const HOOK_TIMEOUT_SECS: u64 = 5;

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
                "command": format!("\"{exe_fwd}\" hook {} {}", row.event, row.state.as_str()),
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

    // 語彙の別名。**本文（[`HOOK_EVENTS`]）と同じ値を短く書くためだけ**のもので、
    // 綴りの正本は [`State::as_str`]
    const WORKING: State = State::Working;
    const WAITING: State = State::Waiting;
    const IDLE: State = State::Idle;
    const STOPPED: State = State::Stopped;

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
    fn stored(states: &HookStates, id: &SessionId) -> Option<State> {
        states.get(id, Some(0)).map(|hook| hook.state)
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
                        == Some(&format!("\"C:/bin/ccdesk.exe\" hook {} {}", row.event, row.state.as_str()))
                })
                .unwrap_or_else(|| panic!("{} {} is not injected", row.event, row.state.as_str()));
            // matcher は表のとおりに載る（持たない行にはキー自体を書かない）
            assert_eq!(
                group.get("matcher").and_then(Value::as_str),
                row.matcher,
                "{} {} has a different matcher",
                row.event,
                row.state.as_str()
            );
            // 注入した組は受け口が受理し、activity まで表のとおりに引ける
            assert_eq!(
                resolve(row.event, Some(row.state.as_str())),
                Some((row.state, row.activity)),
                "the receiver doesn't accept {} {}",
                row.event,
                row.state.as_str()
            );
            seen += 1;
        }
        let injected: usize = hooks.values().map(|groups| groups.as_array().unwrap().len()).sum();
        assert_eq!(injected, seen, "injected a hook that is not in the table");
        // 表に無い組・知らないイベントは受けない
        assert_eq!(resolve("PreToolUse", None), None, "received an unregistered hook");
        assert_eq!(resolve("Notification", Some(STOPPED.as_str())), None, "accepted a pair not in the table");
    }

    /// **`StopFailure` は Stop の代わりに、API エラーでターンが終わったときだけ飛ぶ。**
    /// state は Stop と同じ `idle`（ターンが終わった行に手は要らない
    /// であって「成功した」ではないので、エラーで終わったターンもここに収まる）。
    /// activity も Stop と同じ true（max_output_tokens 等、ユーザーが見るべき出来事
    /// という点は変わらない）。旧形式（[`LEGACY_HOOK_STATES`]）には無い ＝
    /// この hook を知らない旧セッションが呼ぶことは無いので、後方互換の組は要らない
    #[test]
    fn stop_failure_is_treated_like_stop() {
        assert_eq!(resolve("StopFailure", Some(IDLE.as_str())), Some((IDLE, true)));
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
                resolve(event, Some(state.as_str())),
                "{event}'s legacy pair is missing from HOOK_EVENTS"
            );
        }
        // 旧形式の Notification は waiting（入力待ちの通知でしか注入されていなかった）
        assert_eq!(resolve("Notification", None), Some((WAITING, true)));
    }

    /// **合図は「新しく `idle` になった」こと。**
    ///
    /// `idle` のまま残っている行を state だけで見ると、毎周「今しがた空いた」と
    /// 言い続けて使用率の取得が止まらない（`at` まで比べる理由）
    #[test]
    fn only_a_fresh_completion_counts_as_a_finished_turn() {
        let none = HookStates::default();
        let working = HookStates::from_entries([("s", WORKING, 1_000)]);
        let finished = HookStates::from_entries([("s", IDLE, 2_000)]);

        // 動いていた行が終わった ＝ 合図
        assert!(finished.any_row_went_idle_since(&working));
        // 何も知らなかったところに完了が現れた ＝ 合図（起動直後）
        assert!(finished.any_row_went_idle_since(&none));
        // 同じ完了が残っているだけ ＝ 合図にしない
        assert!(!finished.any_row_went_idle_since(&finished));
        // 完了が無ければ合図にならない
        assert!(!working.any_row_went_idle_since(&none));

        // **同じ行が次のターンを終えたら、また合図になる**（`at` が進むため）
        let again = HookStates::from_entries([("s", IDLE, 3_000)]);
        assert!(again.any_row_went_idle_since(&finished));

        // 別の行が終わっても合図（複数セッションを見ている）
        let other = HookStates::from_entries([("s", IDLE, 2_000), ("t", IDLE, 2_500)]);
        assert!(other.any_row_went_idle_since(&finished));
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
    /// 解除はより新しい `status` の観測（[`crate::poll::row_state`]）が
    /// 担う。イベントでも拾うと、解除の口が 2 系統になり
    /// （遅れた `working` が `Stop → idle` を上書きする競合も増える）、
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

    /// **改名した state 値で呼ばれても記録が落ちない。**
    ///
    /// `--settings` のコマンド文字列は**書いた時点のバイナリの値**を焼き込む。
    /// 自己更新は exe を同じパスへ置き換えるので、更新前に起こした claude が
    /// 生き残っていると新バイナリへ古い綴りが飛んでくる（注入ファイルは claude の
    /// 起動時に読まれるため、ファイルを直しても間に合わない）。
    /// [`LEGACY_HOOK_STATES`] は引数**なし**の形しか救わないので、この口が要る
    #[test]
    fn a_state_value_from_an_older_binary_still_resolves() {
        for (event, activity) in [("SessionStart", false), ("Stop", true), ("StopFailure", true)] {
            let old = if event == "SessionStart" { "waiting" } else { "completed" };
            assert_eq!(
                resolve(event, Some(old)),
                Some((IDLE, activity)),
                "an old binary's {event} {old} is dropped instead of being read as idle"
            );
        }
        // **改名表は今の表を汚さない**: 載せた組の改名先は必ず実在し、
        // 旧い綴りは「そのイベントの今の値」であってはならない（自分自身への
        // 読み替えは無意味で、表に残っていると改名済みだと誤解させる）
        for (event, old, new) in RENAMED_HOOK_STATES {
            assert!(
                HOOK_EVENTS.iter().any(|row| row.event == event && row.state == new),
                "{event} is renamed to a state that is not in the table"
            );
            assert!(
                !HOOK_EVENTS
                    .iter()
                    .any(|row| row.event == event && row.state.as_str() == old),
                "{event} {old} is still a live pair; the rename entry is stale"
            );
        }
        // 表に無い組は改名表があっても通らない
        assert_eq!(resolve("Notification", Some("stopped")), None);
        assert_eq!(resolve("SessionStart", Some("nonsense")), None);
    }

    /// **`SessionStart` は入力待ちではない。**
    ///
    /// 起動・再開の直後はプロンプトで待機しているだけで、ユーザーへの要求は無い。
    /// `waiting` と書いていた頃は、止めたセッションを開き直すと（止める前が
    /// Idle でも）黄「Needs input」へ変わり、次のプロンプトまで固着していた。
    /// claude 自身もこの状態を `idle` と報告する（旧 settings で起きている
    /// セッションが呼ぶ後方互換の口も同じ値にする ＝ 片側だけ古い意味で残さない）
    #[test]
    fn starting_a_session_is_idle_not_a_request_for_input() {
        assert_eq!(resolve("SessionStart", Some(IDLE.as_str())), Some((IDLE, false)));
        assert_eq!(resolve("SessionStart", None), Some((IDLE, false)), "the legacy form disagrees");
        // 表そのものが「入力待ち」を名乗っていないこと（改名の読み替え
        // ([`RENAMED_HOOK_STATES`]) が下の口を開けているので、resolve では検査できない）
        assert!(
            !HOOK_EVENTS.iter().any(|row| row.event == "SessionStart" && row.state == WAITING),
            "SessionStart still asks for input"
        );
        // 画面に出る語は保管の綴りと同じ（[`crate::poll::State::title`]）ので、
        // 語をここへ書き写さない ＝ 綴りを変えたときにこの検査だけが古くならない
        assert_eq!(IDLE.title(), IDLE.as_str());
    }

    /// 受けた state は保管へ載り、TUI 側の読みで同じ値が返る
    #[test]
    fn a_recorded_state_reaches_the_reader() {
        let temp = TempStore::new("a_recorded_state_reaches_the_reader");
        assert_eq!(states_at(&temp.path()), HookStates::default(), "not empty for a missing file");

        record(&temp.path(), &id("s-1"), WORKING, true, 1_000, None);
        record(&temp.path(), &id("s-2"), WAITING, true, 1_000, None);
        assert_eq!(
            states_at(&temp.path()),
            HookStates::from_entries([("s-1", WORKING, 1_000), ("s-2", WAITING, 1_000)])
        );

        // 同じセッションの次のイベントは上書き（状態は最後に受けたものが正しい）
        record(&temp.path(), &id("s-1"), IDLE, true, 2_000, None);
        let states = states_at(&temp.path());
        assert_eq!(stored(&states, &id("s-1")), Some(IDLE));
        assert_eq!(
            stored(&states, &id("s-2")),
            Some(WAITING),
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
        let states = HookStates::from_entries([("s", STOPPED, 2_000)]);
        // 起動より後に記録された ＝ 今の実行のもの（記録時刻も一緒に返る）
        assert_eq!(states.get(&id("s"), Some(1_000)), Some(Reported::new(STOPPED, 2_000)));
        // 起動と同時刻も今の実行（時計の分解能で同じ ms に並び得る）
        assert_eq!(states.get(&id("s"), Some(2_000)), Some(Reported::new(STOPPED, 2_000)));
        // 起動より前に記録された ＝ 前回の実行の残骸
        assert_eq!(states.get(&id("s"), Some(3_000)), None);
        // 窓が無い行は動いていない ＝ 保管の値は過去の実行のもの
        assert_eq!(states.get(&id("s"), None), None);
        // 記録の無い行はいつでも None（hook が一度も来ていない）
        assert_eq!(states.get(&id("other"), Some(0)), None);
    }

    /// **ターンの終わりを名乗るイベントだけが `ended` を立てる。**
    ///
    /// [`Entry::ended`] は `activity_at == at` を「activity 付きのイベントが
    /// 書いた」の代わりに読む。その読み替えが成り立つのは **`Idle` を書く 3 つの
    /// うち activity を持たないのが `SessionStart` だけ**という表
    /// （[`HOOK_EVENTS`]）の性質のおかげなので、表を書き換えたらここが落ちる。
    ///
    /// これが崩れると**ペインの中の `/clear` `/resume` が完了通知になる**
    /// （`SessionStart` の `Idle` を「ターンが終わった」と読む）
    #[test]
    fn only_a_turn_ending_event_marks_the_entry_as_ended() {
        let ends_the_turn = |event: &str| matches!(event, "Stop" | "StopFailure");
        for row in HOOK_EVENTS {
            let temp = TempStore::new("ended");
            let (state, activity) =
                resolve(row.event, Some(row.state.as_str())).expect("the pair is not in the table");
            record(&temp.path(), &id("s"), state, activity, 1_000, None);
            let stored = states_at(&temp.path()).get(&id("s"), Some(0)).expect("nothing stored");
            assert_eq!(
                stored.ended,
                ends_the_turn(row.event),
                "{} {} was read as ended={}",
                row.event,
                row.state.as_str(),
                stored.ended
            );
        }
        // **前のイベントの activity は引き継がれるが、`ended` には化けない**:
        // `Stop(idle)` の直後に `SessionStart(idle)` が来ても終わり扱いにしない
        // （`/clear` `/resume` がこの順で書く）
        let temp = TempStore::new("cleared");
        record(&temp.path(), &id("s"), IDLE, true, 1_000, None);
        record(&temp.path(), &id("s"), IDLE, false, 2_000, None);
        let stored = states_at(&temp.path()).get(&id("s"), Some(0)).expect("nothing stored");
        assert!(!stored.ended, "a new session inherited the finished turn");
        // 未読の記録の方は引き継ぐ（[`Entry::activity_at`]）
        assert_eq!(stored.at, 2_000);
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
        let states = HookStates::from_entries([("s", IDLE, 2_000)]);
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
        assert_eq!(stored(&states, &id("s")), Some(WORKING), "the answer did not resume the state");
        // 未読の起点は質問の時刻のまま（回答の時刻に進まない）
        assert!(states.unread(&row(999)), "the question is no longer unread");
        assert!(!states.unread(&row(1_000)), "the answer itself created an unread mark");
    }

    /// **stop・アプリ終了は未読を作らず、消しもしない。**
    ///
    /// `SessionEnd(stopped)` が未読の材料に数えられていた頃は、stop した行が
    /// 再起動後に未読 ● で復活した（実際に報告された）。逆に、まだ見ていない
    /// 完了（`Stop(idle)`）の記録は stopped の上書きでも消えない
    #[test]
    fn a_session_end_neither_creates_nor_destroys_unread() {
        let temp = TempStore::new("a_session_end_neither_creates_nor_destroys_unread");
        let row = |last_opened_at| SessionRow {
            last_opened_at,
            ..SessionRow::new(id("s"), "C:\\dev\\app", 0)
        };
        // ターン完了（未読の材料）→ stop（材料ではない）
        record(&temp.path(), &id("s"), IDLE, true, 1_000, None);
        record(&temp.path(), &id("s"), STOPPED, false, 2_000, None);
        let states = states_at(&temp.path());
        assert_eq!(stored(&states, &id("s")), Some(STOPPED));
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
        record(&temp.path(), &id("s"), IDLE, true, 1_000, None);
        record(&temp.path(), &id("s"), STOPPED, false, 2_000, None);
        let row = SessionRow {
            last_opened_at: 500,
            ..SessionRow::new(id("s"), "C:\\dev\\app", 0)
        };
        assert!(states_at(&temp.path()).unread(&row), "activity_at did not survive the file");

        // 旧形式の項目（activity_at 無し）は未読にならない
        std::fs::write(
            temp.path(),
            r#"{"states":{"s":{"state":"idle","at":9000}}}"#,
        )
        .unwrap();
        assert!(!states_at(&temp.path()).unread(&row), "a legacy entry created an unread mark");
    }

    /// **記録の鍵は行（`CCDESK_ROW`）、載る値は会話（payload の `session_id`）。**
    ///
    /// この 2 つを取り違えると、ペインの中で `/clear` を打った行が「別の行」に
    /// なる（会話を鍵にした記録ができる）か、再開先が行 ID になって
    /// `No conversation found` になる。
    ///
    /// **`CCDESK_ROW` が無ければ何も書かない。** payload の値へ落ちるフォールバックを
    /// 持つと、env の立て忘れが**無音で**効く（行に状態も未読も付かないだけ）。
    ///
    /// **親のプロセス環境を一時的に触る**（そうしないと、この名前が居ない環境で
    /// 検査が空振りする）。触るのはこの 1 つだけで、復元は読み取りの直後に行う
    #[test]
    fn a_record_is_keyed_by_the_row_and_carries_the_conversation() {
        let input = r#"{"session_id":"conv-1","source":"clear"}"#;
        unsafe { std::env::set_var(ROW_ENV, " row-1 ") };
        let padded = hook_entry("SessionStart", None, input);
        unsafe { std::env::set_var(ROW_ENV, "  ") };
        let blank = hook_entry("SessionStart", None, input);
        unsafe { std::env::remove_var(ROW_ENV) };
        let missing = hook_entry("SessionStart", None, input);

        let record = padded.expect("no record was built");
        // 前後の空白は落とす（env が中途半端に立っている環境で空のキーを作らない）
        assert_eq!(record.row, id("row-1"), "the row id did not come from the environment");
        assert_eq!(record.conversation, "conv-1", "the conversation did not reach the record");
        assert_eq!(blank, None, "an all-blank row id was accepted");
        assert_eq!(missing, None, "fell back to the payload when the row id is missing");

        // 知らないイベント / 読めない入力は何も書かない
        unsafe { std::env::set_var(ROW_ENV, "row-1") };
        assert_eq!(hook_entry("PreToolUse", None, input), None);
        assert_eq!(hook_entry("SessionStart", None, "not json"), None);
        unsafe { std::env::remove_var(ROW_ENV) };
    }

    /// **会話は保管を往復する。** ここが落ちると、ペイン内の `/clear` `/resume`
    /// に行が追随する口（[`HookStates::conversation`]）が黙って効かなくなり、
    /// 再開先も失われる
    #[test]
    fn the_conversation_survives_a_round_trip() {
        let temp = TempStore::new("the_conversation_survives_a_round_trip");
        record(&temp.path(), &id("row"), WORKING, true, 1_000, Some("conv-1"));
        assert_eq!(
            states_at(&temp.path()),
            HookStates::from_records([("row", WORKING, 1_000, Some("conv-1"))])
        );
        assert_eq!(states_at(&temp.path()).conversation(&id("row")), Some("conv-1"));

        // **同じ行の会話が差し替わる**（ペインの中の `/clear`）。行は増えない
        record(&temp.path(), &id("row"), IDLE, true, 2_000, Some("conv-2"));
        let states = states_at(&temp.path());
        assert_eq!(states.conversation(&id("row")), Some("conv-2"));
        assert_eq!(states, HookStates::from_records([("row", IDLE, 2_000, Some("conv-2"))]));

        // **取れなかった回は前の値を残す**（1 回でも拾えていれば再開できる）
        record(&temp.path(), &id("row"), STOPPED, false, 3_000, None);
        assert_eq!(states_at(&temp.path()).conversation(&id("row")), Some("conv-2"));
        // 一度も名乗っていない行には答えない
        assert_eq!(states_at(&temp.path()).conversation(&id("other")), None);
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
        record(&temp.path(), &id("s"), WORKING, true, 1_000, None);
        let before = stamp(&temp.path()).expect("no stamp after a write");
        record(&temp.path(), &id("s-2"), WAITING, true, 2_000, None);
        assert_ne!(stamp(&temp.path()), Some(before), "the stamp did not move");
    }

    /// **古い項目は書くたびに落ちる**（1 セッション 1 項目で永久に積もらない）。
    /// 落ちるのは保つ期間を過ぎたものだけで、動いているセッションは毎 turn
    /// 書き直されるので落ちない
    #[test]
    fn recording_drops_entries_older_than_the_keep_window() {
        let temp = TempStore::new("recording_drops_entries_older_than_the_keep_window");
        let keep = KEEP.as_millis() as u64;
        record(&temp.path(), &id("old"), IDLE, true, 0, None);
        record(&temp.path(), &id("fresh"), WORKING, true, keep, None);
        // old は keep をちょうど過ぎた時点で落ちる
        record(&temp.path(), &id("now"), WAITING, true, keep + 1, None);
        let states = states_at(&temp.path());
        assert_eq!(stored(&states, &id("old")), None, "an entry past the keep window remains");
        assert_eq!(
            stored(&states, &id("fresh")),
            Some(WORKING),
            "dropped an entry still within the window"
        );
        assert_eq!(stored(&states, &id("now")), Some(WAITING));
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
        record(&temp.path(), &id("future"), STOPPED, true, 1_000_000, None);

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
        record(&temp.path(), &id("s"), WORKING, true, 1_000, None);
        assert_eq!(
            stored(&states_at(&temp.path()), &id("future")),
            None,
            "the future record survived the next write"
        );
        assert_eq!(stored(&states_at(&temp.path()), &id("s")), Some(WORKING));
    }

    /// 壊れた / 想定外の形でも読みは失敗しない（＝ TUI の周期処理が止まらない）。
    /// **state を持たない項目だけは捨てる**（読んでも何も答えられない）
    #[test]
    fn reads_tolerate_missing_broken_and_unexpected_shapes() {
        let temp = TempStore::new("hook_reads_tolerate_broken_shapes");
        let cases = [
            ("empty", ""),
            ("broken", r#"{"states":{"s":{"state":"idle"}"#),
            ("not-object", "[1,2,3]"),
            ("no-key", r#"{"other":1}"#),
            ("not-map", r#"{"states":[1,2]}"#),
            ("no-state", r#"{"states":{"s":{"at":1}}}"#),
            ("empty-state", r#"{"states":{"s":{"state":""}}}"#),
            ("wrong-types", r#"{"states":{"s":{"state":7}}}"#),
            // 語彙に無い綴り（旧版が書いた語）は項目ごと捨てる
            ("unknown-word", r#"{"states":{"s":{"state":"blocked","at":1}}}"#),
        ];
        for (name, text) in cases {
            std::fs::write(temp.path(), text).unwrap();
            assert_eq!(states_at(&temp.path()), HookStates::default(), "not empty for {name}");
        }
        // 時刻が無い / 型違いでも state は読む（既定 0 ＝ 次の書き込みで落ちる）
        std::fs::write(temp.path(), r#"{"states":{"s":{"state":"idle","at":"soon"}}}"#).unwrap();
        assert_eq!(stored(&states_at(&temp.path()), &id("s")), Some(IDLE));
    }

    /// hook 入力から取るのは `session_id` だけ。**取れなければ何も書かない**
    /// （形が変わっても turn を止めない）
    #[test]
    fn the_session_id_comes_from_the_hook_input() {
        assert_eq!(
            session_id_of(r#"{"session_id":"8a1c0f52","cwd":"C:\\dev"}"#).as_deref(),
            Some("8a1c0f52")
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
        record(&temp.path(), &id("s"), WORKING, true, 1_000, None);
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
        record(&temp.path(), &id("s"), WORKING, true, 1_000, None);
        let before = std::fs::read(temp.path()).unwrap();

        let held = Lock::acquire(&lock_path_for(&temp.path()), Duration::ZERO, LOCK_STALE).unwrap();
        let started = std::time::Instant::now();
        record(&temp.path(), &id("s"), IDLE, true, 2_000, None);
        let waited = started.elapsed();
        drop(held);

        assert!(waited < Duration::from_secs(5), "wait was not bounded: {waited:?}");
        assert_eq!(std::fs::read(temp.path()).unwrap(), before, "wrote even though the lock wasn't acquired");
        // 解放後は通常どおり載る（ロックが理由で壊れているわけではない）
        record(&temp.path(), &id("s"), IDLE, true, 2_000, None);
        assert_eq!(stored(&states_at(&temp.path()), &id("s")), Some(IDLE));
    }
}
