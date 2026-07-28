//! App 状態機械・イベントループ（run）・マウス／キー処理・セッションのディスパッチ。
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Position;

use ccdesk::{log_error, now_ms, same_dir, LockExt};

use crate::hooks::HookStates;
use crate::keys::{encode_key, forward_mouse};
use crate::poll::{AgentInfo, FooterInfo, Grouping};
use crate::usage::Usage;
use crate::session::{Launch, Session};
use crate::sessions::{SessionId, SessionRow};
use crate::source::{DataSource, PollSinks, WindowItem, PROJECTS_LIMIT};
use crate::title::Titles;
use crate::ui::new_view::{handle_new_view_key, NewState};
use crate::ui::{
    draw, fit_sidebar, menu_zone, popup_rect, row_at, row_y, sidebar_cols, sidebar_layout,
};

// 一覧の正本（~/.ccdesk/sessions.json）を読み直す周期。**他インスタンスが起こした
// セッションを取り込むため**に要る（小さな JSON 1 本の read。描画は dirty 時のみ）
const SCAN_INTERVAL: Duration = Duration::from_secs(2);
// 自分の PTY の生死を見る周期（前景では `child.try_wait()` が生死の唯一の真実）
const LIVE_SCAN_INTERVAL: Duration = Duration::from_secs(2);
/// イベント待ちの上限（＝ 何も起きないときの周回間隔）
const POLL_IDLE: Duration = Duration::from_millis(33);
/// スピナーが回っている間の描き直し間隔（点滅周期 400ms の半周期 ＝
/// これより短くしても見た目は変わらず、フレームが増えるだけ）
const SPINNER_REDRAW: Duration = Duration::from_millis(200);
/// 何も動いていないときの描き直す間隔。動くものは経過時間表示（`· 23s`）だけで、
/// その粒度は 1 秒なので、これより短い周期は**前フレームと同一の出力**を組み直すだけ
const IDLE_REDRAW: Duration = Duration::from_secs(1);
/// 描画を見送っている間のイベント待ちの上限。子が静まったことに早く気づくため
/// 短くする（見送りは [`crate::session::REDRAW_HOLD_MAX`] で打ち切られるので、
/// この短い周回が続くのは出力が途切れない間だけ）
const POLL_HELD: Duration = Duration::from_millis(8);

/// ペインフォーカス。キー入力はフォーカス中のペインにだけ流す
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Focus {
    Sidebar,
    Terminal,
}

/// サイドバー行のクリック動作。セッションは [`SessionId`] で参照する。
/// 一覧は 2 秒毎に読み直され並びも変わるため、描画時の生 index を
/// 保持すると実行時に別セッションを閉じたり消したりし得る
#[derive(Clone, PartialEq, Debug)]
pub(crate) enum RowAction {
    New, // 新規セッション画面を開く
    /// プロジェクト見出し行 = そのフォルダのメニュー（new session / remove project）を開く。
    /// **クリックで即セッションが立つ行ではない**（起動は開いたメニューの中で選ぶ）
    Project(String),
    ToggleGroup, // グルーピング切替（state ⇔ directory）
    /// セッション行: ウィンドウが開いていれば切替、無ければ `claude -r` で再開
    Open(SessionId),
    UpdateCcdesk,  // ccdesk 自身を更新（サイドバー先頭の版行）
    UpdateClaude,  // claude 本体を更新（同じく版行）
}

/// サイドバー一覧に積まれた 1 行（[`App::sidebar_rows`] の要素）。
///
/// **「飾り」と「押しても何も起きない行」を型で分けるのが要点。** 以前は
/// `Option<RowAction>` の `None` がその両方を意味していたので、更新の無い版行が
/// 区切り線と同じ扱いになり、選択もホバーもハイライトも一括で漏れていた
/// （3 箇所が別々に `is_some()` を見ていたため、そこへ `if` を足すと
/// 「実体のある行か」の知識が 3 つに増える）。
///
/// 型で分けたので判断は [`Self::selectable`] 1 つに集まり、
/// キーボードの選択・マウスのホバー・描画のハイライトが同じ答えを読む
#[derive(Clone, PartialEq, Debug)]
pub(crate) enum SidebarRow {
    /// 区切り線・空行・グループ見出し・集計行 ＝ 画面を組む飾りで、行の実体が無い。
    /// 選択もホバーもしない
    Decoration,
    /// 実体はある行だが、今は押しても何も起きない（更新の無い版行）。
    /// 選択とホバーの対象で、Enter は無反応
    Inert,
    /// 押すと動く行
    Action(RowAction),
}

impl SidebarRow {
    /// 選択・ホバー・ハイライトの対象か ＝ **飾りではない（行の実体がある）か**
    pub(crate) fn selectable(&self) -> bool {
        !matches!(self, Self::Decoration)
    }

    /// 押したときの動作。飾りと無反応な行は `None`
    pub(crate) fn action(&self) -> Option<&RowAction> {
        match self {
            Self::Action(action) => Some(action),
            Self::Decoration | Self::Inert => None,
        }
    }
}

/// サイドバーで指せる位置。**キーボード選択（[`App::selection`]）と
/// マウスホバー（[`App::hovered`]）が共有する**ので、型が表すのは「選択」ではなく
/// 「位置」だけ（選択かホバーかの意味はフィールド名が持つ）。
///
/// **一覧の行とアカウント行を 1 つの型で表す**のが要点。アカウント行は
/// フッター（一覧の外）に描かれるので `sidebar_rows` の index では指せず、
/// かといって「行 index + アカウントを指しているか」の 2 つに分けると
/// 排他であるはずの状態が両立し得る形になる。
///
/// 画面 y への写像は [`selected_row_y`] 1 箇所に閉じる
/// （一覧の行は [`row_y`]、アカウント行は [`sidebar_layout`] の `account_y` ＝
/// **どちらもマウスの当たり判定と同じ計算**）
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum SidebarPos {
    /// 一覧の行（[`App::sidebar_rows`] の index）
    Row(usize),
    /// フッターのアカウント行。**今サインインしているアカウントを出すだけの行**で、
    /// 選択とホバーはできるが押しても何も起きない（[`SidebarRow::Inert`] と同じ扱い。
    /// 切り替えを持たない理由は [`crate::poll::AccountStatus`]）。
    ///
    /// **`SidebarRow::Inert` へは寄せられない**: あちらは `sidebar_rows` に積まれた
    /// 行の種類で、この行はフッター（一覧の外・下端に固定）に描かれるため index を
    /// 持たない。「押しても何も起きない」という**答え**は
    /// [`selected_enter`] が `None` を返す 1 箇所に揃えてある
    Account,
}

impl SidebarPos {
    /// 一覧の行なら index。**アカウント行は一覧の外なので `None`**
    /// （行のハイライト・行の動作の引き当て・スクロール追従はこれを見る）
    pub(crate) fn row(self) -> Option<usize> {
        match self {
            Self::Row(row) => Some(row),
            Self::Account => None,
        }
    }
}

/// ccdesk 自身の更新の進行状態。**バックグラウンドスレッドが書き、UI が読む正本**。
///
/// AtomicBool を並べずに 1 つの状態にしてあるのは、実行中・完了・失敗が排他で、
/// 多重起動の防止もこのロックの中で決まるため（同じ知識を 2 つのフラグに分けない）
pub(crate) enum SelfUpdate {
    /// 未実行、または失敗を通知し終えた後（＝再試行できる）
    Idle,
    Running,
    /// 差し替え済み。反映は次回起動なので、以降このセッション中はずっと再起動を促す
    Done,
    /// 失敗。run ループが下部バーへ 1 度出して Idle へ戻す
    Failed(String),
}

/// モーダルの種類。**メニューの中身（項目・項目の意味）はこの型が答える**。
/// [`Popup`] は「どこに開いたか・どれを選んでいるか」だけを持つので、
/// 種類を足すときの変更は [`PopupKind::entries`] の 1 つの match に閉じる
/// （幅は項目から導くので触らない。枠の桁は描く側 ＝ `ui::popup_rect` が持つ）
#[derive(Debug, PartialEq)]
pub(crate) enum PopupKind {
    /// セッション 1 行への二次操作。**`pinned` / `open` は開いた時点の写し**
    /// （他のメニューと同じ作り）で、項目の表示名（ピン留めか解除か）と
    /// 実行可能かの判断に使う。
    ///
    /// 写しにするのは、メニューが開いている間に一覧の読み直し（2 秒周期）や
    /// 他インスタンスの操作で行が変わっても、**見えている項目と押した結果が
    /// ずれないようにする**ため。行が消えていた場合は実行側が何もしない
    Session {
        id: SessionId,
        pinned: bool,
        /// 窓が開いていて子プロセスが生きているか（`stop` を出せるかの判断）
        open: bool,
    },
    Group,
    /// プロジェクト単位の操作。`has_sessions` は開いた時点の写し（[`PopupKind::Session`] の
    /// `open` と同じ作り）で、`remove project` を出せるかの判断に使う
    Project { cwd: String, has_sessions: bool },
}

/// 項目を選んだときに起きること。**選択 index から作る**ので、表示名が同じ項目が
/// 並んでも対象を取り違えない（ラベル文字列から対象を復元しない）。
/// 副作用は持たず、実行は [`run_popup_action`] だけが行う
#[derive(Debug, PartialEq)]
enum PopupAction {
    /// そのセッションを開く（窓があれば切替、無ければ `claude -r` で再開）。
    /// **行クリックと同じ [`open_session`]** を通る ＝ 開く経路を 2 つ持たない
    OpenSession(SessionId),
    /// ピン留めの入切（ピンした行は各グループの先頭へ）
    TogglePin(SessionId),
    /// 未読を消す（`last_opened_at` を今にする）
    MarkRead(SessionId),
    /// セッションを止める ＝ 子プロセスを終わらせる。**行は残す**（`open` で再開できる）
    Stop(SessionId),
    /// 一覧から行を外す（transcript は消さない ＝ 会話ログは残る）
    Close(SessionId),
    SetGrouping(Grouping),
    /// 指定フォルダで新規セッション
    NewSessionIn(String),
    /// プロジェクトを一覧から外す
    RemoveProject(String),
}

/// メニューの項目 1 つ。**表示（label / enabled）と動作（action）を 1 つの表で持つ**:
/// 以前は表示の match と動作の match が index で暗黙に対応しており、項目を
/// 1 つ挿入すると対応がずれ「押した項目と違う動作が走る」形が作れた。
/// 描画・幅計算・実行のすべてがこの 1 つの表を読む
pub(crate) struct PopupEntry {
    pub(crate) label: String,
    pub(crate) enabled: bool,
    action: PopupAction,
}

impl PopupKind {
    /// メニューの項目表（並び順 = 表示順）
    pub(crate) fn entries(&self, grouping: Grouping) -> Vec<PopupEntry> {
        match self {
            // 二次操作はここに集約する（ショートカットキーを併設しない ＝
            // 入口を 2 つ持たない）。
            //
            // **先頭は `open`**: サイドバーのキーは `↑↓` と `Enter` だけになり、
            // セッション行の `Enter` もこのメニューを開くので、キーボードから
            // セッションを開く導線はこの項目になる（窓が開いていれば切替、
            // 無ければ再開 ＝ 行クリックとまったく同じ [`open_session`]）。
            // 停止中の行でも再開できるので落とさない。
            //
            // **語は実態に合わせてある**: `stop` はプロセスを止める（行は残り
            // `open` で再開できる）、`close` は ccdesk の一覧から外す（会話ログ ＝
            // transcript は残る）。「消す」語を使わないのは実際に消えないからで、
            // 一覧から閉じるだけという実態を語がそのまま表す。
            //
            // **`stop` だけが窓の有無で落ちる**: 窓が無い行は既に止まっているので、
            // 押せるのに何も起きない項目になる（`remove project` と同じ扱い）。
            // 他は窓の有無に関係なく行に効く ＝ 停止中でも選べる。
            // 入切するピン留めは**今の状態の逆を出す**（`● ` 印だとどちらが起きるか
            // 読めない。ここは選択ではなく動作の名前）。
            //
            // **アーカイブは持たない**: `close` は行を忘れるだけで
            // `~/.claude/projects/**/*.jsonl` を消さないので、アーカイブとの差は
            // 「戻す導線があるか」だけになる ＝ 節を 1 つ増やす価値が無い
            PopupKind::Session { id, pinned, open } => {
                let entry = |label: &str, enabled: bool, action: PopupAction| PopupEntry {
                    label: label.to_string(),
                    enabled,
                    action,
                };
                vec![
                    entry("open", true, PopupAction::OpenSession(id.clone())),
                    entry(
                        if *pinned { "unpin" } else { "pin" },
                        true,
                        PopupAction::TogglePin(id.clone()),
                    ),
                    entry("mark as read", true, PopupAction::MarkRead(id.clone())),
                    entry("stop", *open, PopupAction::Stop(id.clone())),
                    entry("close", true, PopupAction::Close(id.clone())),
                ]
            }
            // 項目は [`Grouping::ORDER`] から導く（variant を足すとここも自動で増える ＝
            // メニューだけ古い 2 分岐のまま、という形を作れない）
            PopupKind::Group => Grouping::ORDER
                .into_iter()
                .map(|g| PopupEntry {
                    label: format!("{}{}", if grouping == g { "● " } else { "  " }, g.as_str()),
                    enabled: true,
                    action: PopupAction::SetGrouping(g),
                })
                .collect(),
            // **セッションが残っているフォルダは登録解除させない。** 見出しの一覧は
            // 「登録リスト ∪ セッションの cwd」なので、登録を外してもセッション由来で
            // 見出しは出続ける。押せるのに表示が変わらないのは嘘なので、
            // stop と同じ仕組み（実行可能フラグ）で落とす
            PopupKind::Project { cwd, has_sessions } => vec![
                PopupEntry {
                    label: "new session".to_string(),
                    enabled: true,
                    action: PopupAction::NewSessionIn(cwd.clone()),
                },
                PopupEntry {
                    label: "remove project".to_string(),
                    enabled: !has_sessions,
                    action: PopupAction::RemoveProject(cwd.clone()),
                },
            ],
        }
    }

    /// 選択 index の項目が意味する動作（範囲外は None）。**表（[`Self::entries`]）から
    /// 引く**ので、表示と動作が index でずれることは構造的に無い
    #[cfg(test)]
    fn action(&self, grouping: Grouping, index: usize) -> Option<PopupAction> {
        let mut entries = self.entries(grouping);
        (index < entries.len()).then(|| entries.swap_remove(index).action)
    }
}

/// 行頭の `=` / group 行クリックで開くコンテキストメニューの開き状態。
/// **階層は持たない**（Esc・外クリックは常に全閉。戻り先を持たない）
pub(crate) struct Popup {
    pub(crate) kind: PopupKind,
    pub(crate) anchor_y: u16, // 開いた元の画面行（矩形はこの 1 つ下に出る）
    pub(crate) selected: usize,
}

/// 右ペインの表示内容
pub(crate) enum RightView {
    Sessions,
    New(NewState),
}

pub(crate) struct App {
    /// 開いているウィンドウ（前景セッションの PTY そのもの）。**一覧の行とは別物**で、
    /// 窓を閉じてもプロセスが終わるだけ ＝ 行（[`Self::sessions`]）は残る
    pub(crate) windows: Vec<Session>,
    /// 表示中のウィンドウ（[`Self::windows`] の添字）
    pub(crate) active: usize,
    // claude agents --json のライブ状態（正規 IF。バックグラウンドスレッドが更新）
    pub(crate) agents: Vec<AgentInfo>,
    pub(crate) agents_shared: Arc<Mutex<Vec<AgentInfo>>>,
    pub(crate) agents_dirty: Arc<std::sync::atomic::AtomicBool>,
    /// サイドバーに並ぶ行。**正本は `~/.ccdesk/sessions.json`**（供給元が読み書きする）
    pub(crate) sessions: Vec<SessionRow>,
    /// hook（`--settings` で注入した公式 hook）が書いた state の写し。
    /// **行の state・未読・経過時間はどれもここから導く**（行に保存しない）。
    /// hook が一度も来ていない行だけ `agents --json` の `status` へ落ちる
    /// （[`crate::hooks`]）
    pub(crate) hook_states: HookStates,
    /// 撮影用の固定 state（`session_id` → state）。**実データでは必ず空**で、
    /// 窓を持たない行を「動いている」ものとして描くためだけにある
    /// （[`crate::source::DataSource::fixed_states`]）
    pub(crate) fixed_states: std::collections::HashMap<SessionId, String>,
    /// 最後に見た hook 受け渡しファイルの見え方（長さ・更新時刻）。
    /// **中身ではなく「変わったか」だけを持つ**ので、run ループが毎周見ても安い。
    /// 変わった周は周期を待たずに一覧を読み直す ＝ ペイン内の `/resume` `/clear` が
    /// 立てた新しいセッションが即座にサイドバーへ出る
    pub(crate) hook_stamp: Option<(u64, std::time::SystemTime)>,
    /// transcript から表示名を追う道具。**読んだ transcript の見え方を覚えている**
    /// ので、追記されていないファイルは読み直さない（[`crate::title`]）
    pub(crate) titles: Titles,
    pub(crate) last_scan: std::time::Instant,
    pub(crate) last_live_scan: std::time::Instant,
    pub(crate) sidebar_width: u16,
    pub(crate) dragging: bool,
    pub(crate) last_drag_resize: std::time::Instant,
    pub(crate) term_size: (u16, u16), // (width, height)
    // サイドバーに積まれた行（draw で構築）。飾りと押せない行の区別は [`SidebarRow`]
    pub(crate) sidebar_rows: Vec<SidebarRow>,
    // サイドバー上部の固定行数（ccdesk 版行・claude 版行・区切り線・+ new session・
    // 区切り線・⊞ group・集計行）。正本は draw（積んだ行数をそのまま記録する）で、
    // ヒットテストとスクロール計算は sidebar_rows と同じく「最後に描いた値」を読む
    pub(crate) sidebar_header_rows: usize,
    // サイドバーのスクロール位置（先頭に表示する行 index。draw でクランプ）
    pub(crate) sidebar_scroll: usize,
    // ↑↓ で選択を動かした直後だけ true: 次の draw で選択行が見える位置へ追従する
    // （ホイールスクロールを選択位置へ引き戻さないための区別）
    pub(crate) sidebar_follow_sel: bool,
    // マウスが乗っている位置（押して意味のある位置のときだけ Some）。
    // **選択と同じ [`SidebarPos`]** なので、アカウント行のように一覧の外にある行も
    // ホバーで表せる（ハイライトの規則は描画側 1 箇所）
    pub(crate) hovered: Option<SidebarPos>,
    // サイドバーフォーカス時のキーボード選択位置（一覧の行 or フッターのアカウント行）
    pub(crate) selection: SidebarPos,
    // **最後に選択を揃えたペインのセッション。** ペインが指すセッションが
    // これと違うフレームだけ選択を寄せる（`ui::follow_pane`）ので、
    // `↑↓` で選択だけを動かしている間は勝手に戻らない
    pub(crate) pane_shown: Option<SessionId>,
    pub(crate) dispatch_cwd: String,
    pub(crate) right_view: RightView,
    // サイドバー下部のアカウント・バージョン表示（バックグラウンド取得）
    pub(crate) footer: FooterInfo,
    pub(crate) footer_shared: Arc<Mutex<FooterInfo>>,
    pub(crate) footer_dirty: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) footer_refresh: Arc<std::sync::atomic::AtomicBool>,
    // claude update 実行中（行の連打防止と "updating…" 表示）
    pub(crate) claude_updating: Arc<std::sync::atomic::AtomicBool>,
    // ccdesk 自身の更新の進行状態（版行の表示と多重起動防止の正本）
    pub(crate) ccdesk_update: Arc<Mutex<SelfUpdate>>,
    // ccdesk 自身の新しいリリース（起動時 1 回のチェック）。
    // 新しい版があるときだけ Some = 版行に ⟳ と update が出る
    pub(crate) ccdesk_latest: Option<String>,
    pub(crate) ccdesk_latest_shared: Arc<Mutex<Option<String>>>,
    pub(crate) ccdesk_latest_dirty: Arc<std::sync::atomic::AtomicBool>,
    // 使用率（5h / 7d / モデル別週次）。**取るかどうかの判断は供給元**
    // （`DataSource::usage`）が持つので、ここは受け取った値だけを持つ。
    // 注入する settings には一切関係しない（[`crate::hooks::inject_settings`]）
    pub(crate) usage: Usage,
    /// 使用率が更新されたことを取得スレッドが立てる合図（フッターと同じ作法）。
    /// **周期で読みに行かない**ので、使用率を切った環境ではこの旗が一度も立たない
    pub(crate) usage_dirty: Arc<std::sync::atomic::AtomicBool>,
    /// **クリック起点の**取得が進行中か（リングをスピナーに変える材料）。
    /// 自動取得（保険の周期・ターン完了）では立たない ＝ 押していないのに回らない。
    /// 立てるのはクリック（即時）と取得スレッド、降ろすのは取得スレッドだけ
    pub(crate) usage_fetching: Arc<std::sync::atomic::AtomicBool>,
    /// マウスが使用率ゲージの上にいるか（押せることを帯で示す）。
    /// 一覧の行ではないので [`Self::hovered`]（[`SidebarPos`]）では表せない
    pub(crate) usage_hovered: bool,
    // 画面に出す値の供給元（実データ / 撮影用の固定データ）。起動時に 1 度だけ選ばれ、
    // 以降ここを通る限り「今 demo か」を問う必要が無い
    pub(crate) source: Arc<dyn DataSource>,
    // 起動した子がまだ端末を掴んでいない間だけ Some（起こした時刻）。
    // 降ろす契機は「子が最初の出力を出した」（run ループ）と期限切れ
    // （[`expire_input_gate`]）の 2 つで、**降ろすのは [`lift_input_gate`] だけ**
    pub(crate) input_gate: Option<std::time::Instant>,
    // 下部バーに数秒表示するエラー等の通知
    pub(crate) notice: Option<(String, std::time::Instant)>,
    pub(crate) grouping: Grouping,
    // 登録済みプロジェクト（ディレクトリ）の絶対パス。**この Vec が登録内容の正本**で、
    // 変更のたび全量を供給元へ書き戻す。directory グルーピングの見出しは
    // 「この一覧 ∪ セッションの cwd」なので、セッションが 0 本になっても
    // ここに残っている限り見出しは消えない（＝そのフォルダで新規を開く入口が残る）
    pub(crate) projects: Vec<String>,
    pub(crate) popup: Option<Popup>,
    pub(crate) focus: Focus,
    // 直前のフレームでスピナー（Working の点滅）が出ていたか。
    // run ループがアイドル時の描き直し間隔を選ぶ材料（描画が毎フレーム更新する）
    pub(crate) spinner_active: bool,
}

/// テストの土台になる中立な `App`。各テストは関心のあるフィールドだけを
/// `App { .., ..Default::default() }` で上書きする。
///
/// **置き場所が要点で、`mod tests` ではなく構造体定義の直後に置いてある。**
/// フィールド列挙をテスト側に持つと、`App` にフィールドを足した変更と、
/// 全フィールドを列挙するテストヘルパを足した変更が別ブランチで並んだとき、
/// テキスト衝突が起きないまま**テストビルドだけが壊れたマージ**が生まれる（実際に
/// 起きた: `ccdesk_update` の追加とヘルパの追加で E0063）。定義の隣なら、
/// フィールドを足す変更が同じ場所の編集になるので取り違えようがない。
///
/// ここは「同じ知識を 2 箇所に持たせない」より
/// **「1 つの変更が 1 箇所に閉じる（局所性）」を優先した**判断:
/// 中立値の列挙自体は `main` の本番組み立てと重複するが、それを消すには
/// `source` に偽の供給元を既定値として持たせる必要があり、本番の構造を
/// テストのために歪めることになる。だから重複は残し、代わりに
/// 「足す場所が 1 箇所に見える」ことを取った。
///
/// `#[cfg(test)]` なのは、`source` の既定値が [`DemoSource`]（ファイルも
/// ネットワークも触らない）で、本番でこれを既定にしてはいけないため
#[cfg(test)]
impl Default for App {
    fn default() -> Self {
        Self {
            windows: Vec::new(),
            active: 0,
            agents: Vec::new(),
            agents_shared: Arc::new(Mutex::new(Vec::new())),
            agents_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            sessions: Vec::new(),
            hook_states: HookStates::default(),
            fixed_states: std::collections::HashMap::new(),
            hook_stamp: None,
            titles: Titles::default(),
            last_scan: std::time::Instant::now(),
            last_live_scan: std::time::Instant::now(),
            sidebar_width: 34,
            dragging: false,
            last_drag_resize: std::time::Instant::now(),
            term_size: (120, 30),
            sidebar_rows: Vec::new(),
            sidebar_header_rows: 0,
            sidebar_scroll: 0,
            sidebar_follow_sel: false,
            hovered: None,
            selection: SidebarPos::Row(0),
            pane_shown: None,
            dispatch_cwd: String::new(),
            right_view: RightView::Sessions,
            footer: FooterInfo::default(),
            footer_shared: Arc::new(Mutex::new(FooterInfo::default())),
            footer_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            footer_refresh: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            claude_updating: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ccdesk_update: Arc::new(Mutex::new(SelfUpdate::Idle)),
            ccdesk_latest: None,
            ccdesk_latest_shared: Arc::new(Mutex::new(None)),
            ccdesk_latest_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            usage: Usage::default(),
            usage_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            usage_fetching: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            usage_hovered: false,
            // 撮影用の供給元は state.json / config.json を書かないので、
            // テストが開発者の設定を踏まない
            source: Arc::new(crate::source::DemoSource),
            input_gate: None,
            notice: None,
            grouping: Grouping::State,
            projects: Vec::new(),
            popup: None,
            // サイドバー側にしておく（set_focus が PTY へ通知を出さない）
            focus: Focus::Sidebar,
            spinner_active: false,
        }
    }
}

/// 起動 1 回の結果。**成功なら起こしたセッション**（起こさない供給元 ＝ 撮影用は
/// `Ok(None)`。「起動を試していない ＝ 失敗もしていない」を表す）、失敗なら理由。
///
/// 反映は [`apply_launch`] だけが行う（フォルダの登録を成功時に 1 箇所で行うため）
type Launched = Result<Option<SessionId>, String>;

impl App {
    fn pane_size(&self) -> (u16, u16) {
        // 右ペインの矩形（正本は ui::pane_rect）から Block 枠線 2 桁/2 行を引いた
        // 内側サイズ (rows, cols)
        let pane = crate::ui::pane_rect(self);
        (
            pane.height.saturating_sub(2).max(1),
            pane.width.saturating_sub(2).max(1),
        )
    }

    fn resize_sessions(&mut self) {
        let (rows, cols) = self.pane_size();
        for window in &mut self.windows {
            window.resize(rows, cols);
        }
    }

    pub(crate) fn open_new_view(&mut self) {
        self.right_view = RightView::New(NewState::browse(&self.dispatch_cwd));
        // 次回起動時に同じ画面を復元する（None = new session 画面）
        self.source.save_window(WindowItem::LastView(None));
    }

    /// ポーラーの書き込み先をまとめて渡す。どのポーラーを起こすかは供給元が決めるので、
    /// 呼び出し側は demo かどうかを知らない
    pub(crate) fn poll_sinks(&self) -> PollSinks {
        PollSinks {
            agents: self.agents_shared.clone(),
            agents_dirty: self.agents_dirty.clone(),
            footer: self.footer_shared.clone(),
            footer_dirty: self.footer_dirty.clone(),
            footer_refresh: self.footer_refresh.clone(),
            ccdesk_latest: self.ccdesk_latest_shared.clone(),
            ccdesk_latest_dirty: self.ccdesk_latest_dirty.clone(),
        }
    }

    /// フォーカス変更（PTY への focus in/out 通知つき）。
    /// サイドバーへ移った瞬間は一覧と生死を即スキャンして表示を最新化する
    fn set_focus(&mut self, focus: Focus) {
        if self.focus == focus {
            return;
        }
        if matches!(self.right_view, RightView::Sessions)
            && let Some(window) = self.windows.get_mut(self.active) {
                window.send_focus(focus == Focus::Terminal);
            }
        self.focus = focus;
        if focus == Focus::Sidebar {
            self.last_scan = instant_ago(SCAN_INTERVAL);
            self.last_live_scan = instant_ago(LIVE_SCAN_INTERVAL);
        }
    }

    /// 右ペインに表示するウィンドウを切り替える（フォーカスは動かさない）
    fn show_session(&mut self, idx: usize) {
        if self.focus == Focus::Terminal && idx != self.active
            && let Some(old) = self.windows.get_mut(self.active) {
                old.send_focus(false);
            }
        self.active = idx;
        self.right_view = RightView::Sessions;
        // 次回起動時に同じセッションを復元する
        if let Some(id) = self.windows.get(idx).map(|w| w.session_id.clone()) {
            self.source.save_window(WindowItem::LastView(Some(&id)));
        }
        if self.focus == Focus::Terminal
            && let Some(window) = self.windows.get_mut(idx) {
                window.send_focus(true);
            }
    }

    /// **今ペインに出ているセッション**（右ペインがセッション表示でないなら None）。
    /// 「キー入力の宛先」と「ユーザーが見ている行」がどちらもこの 1 つの判断から出る
    pub(crate) fn shown_session(&self) -> Option<&SessionId> {
        matches!(self.right_view, RightView::Sessions)
            .then(|| self.windows.get(self.active).map(|w| &w.session_id))
            .flatten()
    }

    /// **キー入力が今このセッションへ届く形になっているか。**
    /// `focus` は見ない: 判定したいのは「端末へ流したとき誰に届くか」で、
    /// 流すかどうかを決める側（[`lift_input_gate`]）がこれを材料にする
    fn showing(&self, id: &SessionId) -> bool {
        self.shown_session() == Some(id)
    }

    /// 一覧の行を ID で引く。**「消えた行は何もしない」の判断ごと 1 箇所に置く**
    /// （読み直し・他インスタンスの削除とクリックは競合しうるので、
    /// 引き当てが空振りする経路はどの呼び手にもある）
    pub(crate) fn row(&self, id: &SessionId) -> Option<&SessionRow> {
        self.sessions.iter().find(|row| &row.session_id == id)
    }

    /// [`Self::row`] の可変版
    fn row_mut(&mut self, id: &SessionId) -> Option<&mut SessionRow> {
        self.sessions.iter_mut().find(|row| &row.session_id == id)
    }

    /// その行の窓の添字（開いていなければ None）
    fn window_index(&self, id: &SessionId) -> Option<usize> {
        self.windows.iter().position(|w| &w.session_id == id)
    }
}

/// now - d の Instant（アンダーフローしない）。「次の周期処理を即発火させる」ための
/// 過去時刻づくりに使う。Windows の Instant はブート起点のため、OS 起動直後は
/// 素の減算が panic する
pub(crate) fn instant_ago(d: Duration) -> std::time::Instant {
    std::time::Instant::now()
        .checked_sub(d)
        .unwrap_or_else(std::time::Instant::now)
}

/// 同期出力（DECSET 2026）のスコープガード。Drop で閉じるため、draw のクロージャが
/// panic して巻き戻した場合も開いたままにならない。閉じ忘れると mode 2026 に
/// タイムアウトを持たない端末では画面が固まり、panic メッセージも表示されないまま
/// 操作不能になる。
///
/// 既知の制限（挙動は変えない）: crossterm 0.29 の Begin/EndSynchronizedUpdate は
/// `is_ansi_code_supported()` を true 決め打ちで実装しているため、VT 処理の無い
/// レガシー Windows コンソールでは他コマンドが winapi 経路を通る一方この 2 つだけ
/// 生 ANSI を書き、`[?2026h` が文字として表示され得る。ccdesk は ConPTY を
/// 前提とするため許容する。
///
/// カーソルの `Show` はここでは出さない。panic 時は ratatui の hook が
/// alt screen を離脱し、その後の巻き戻しで Terminal の Drop が
/// （hidden_cursor が立っていれば）`?25h` を出すので、通常画面に戻った後に
/// 復帰する順序が既に成立している。ここで二重に出す必要はない
struct SyncOutput;

impl SyncOutput {
    /// **flush はしない（queue のみ）。** Begin は次の `terminal.draw` の flush と
    /// 一緒に端末へ届けば足りる（生 stdout と CrosstermBackend は同一のグローバル
    /// stdout バッファを共有するので順序は保たれる）。execute! で書くと
    /// 1 フレームにつき write+flush の syscall が 2〜3 回余計に増える
    fn begin() -> Self {
        let _ = crossterm::queue!(
            std::io::stdout(),
            crossterm::terminal::BeginSynchronizedUpdate
        );
        Self
    }
}

impl Drop for SyncOutput {
    fn drop(&mut self) {
        // フレームの終端なのでここでだけ flush する（駐車の MoveTo も一緒に流れる）
        use std::io::Write as _;
        let mut out = std::io::stdout();
        let _ = crossterm::queue!(out, crossterm::terminal::EndSynchronizedUpdate);
        let _ = out.flush();
    }
}

/// 1 フレームぶんの出力。同期出力（DECSET 2026）で包み、終端カーソルを必ず確定させる。
///
/// ratatui は 1 フレームを「差分 + 非表示/表示 + MoveTo」の複数 flush に分けて書く。
/// 途中状態を端末に観測させないため全体を同期出力で囲む（非対応端末は無視するだけ）。
/// 見せるフレームだけ ratatui に位置を渡し、隠すフレームは位置を渡さず（= None）
/// 自前の MoveTo でカーソルをペイン内へ駐車させる。この非対称が要点で、理由は 2 つ:
///
/// 1. 終了後にカーソルが隠れたまま残るのを防ぐ。ratatui は自前の hidden_cursor
///    フラグを持ち、Terminal の Drop ではそれが立っているときだけ `?25h` を出す。
///    フラグが立つのは位置 None（= 内部で hide_cursor を通る）のときだけなので、
///    位置を常に渡して生の `cursor::Hide` で隠すとフラグが永久に false のまま
///    「実際は隠れているのに ratatui は表示中だと思っている」状態になり、
///    alt screen 離脱で DECTCEM を復元しない端末では終了後のシェルにカーソルが戻らない。
/// 2. 毎フレームの `?25h` を出さない。位置ありの draw は毎回 show_cursor を呼ぶため、
///    隠すフレームでも Show → MoveTo → Hide を送ることになり、DECTCEM の実装が
///    素直でない端末ではこれがちらつきになる。None ならそもそも Show を出さない。
///
/// 元の IME バグ（位置を渡さないと MoveTo が出ず、物理カーソルが差分の最終セル
/// = サイドバーに残る）は自前の MoveTo で駐車させるので再発しない。差分描画側の
/// MoveTo 省略判定（last_pos）は Backend::draw のメソッドローカルで毎回リセット
/// されるため、後から MoveTo を出しても次フレームの差分とは干渉しない。
/// 生 stdout と CrosstermBackend<Stdout> は同一のグローバル stdout を共有するので
/// 書き込み順序も保証される
fn draw_frame(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> anyhow::Result<()> {
    let _sync = SyncOutput::begin();
    // 隠すフレームでカーソルを駐車させたい位置（Some = 非表示フレーム）
    let mut park: Option<ratatui::layout::Position> = None;
    let drawn = terminal.draw(|frame| {
        let cursor = draw(frame, app);
        if cursor.visible {
            frame.set_cursor_position(cursor.pos);
        } else {
            park = Some(cursor.pos);
        }
    });
    drawn?;
    if let Some(pos) = park {
        // queue のみ（flush は SyncOutput の Drop が 1 回だけ行う）
        let _ = crossterm::queue!(std::io::stdout(), crossterm::cursor::MoveTo(pos.x, pos.y));
    }
    Ok(())
}

/// アクティブ窓へバイト列を送る。**書き込みエラーで run ループを抜けない**:
/// 壊れたのはその窓の PTY だけなので、その窓を閉じて通知する
/// （live-scan の「死んだ PTY は自分の窓だけ閉じる」と同じ扱い。以前は `?` で
/// run() ごと抜け、健全な全セッションを道連れに ccdesk が終了していた）
fn send_to_active(app: &mut App, bytes: &[u8]) {
    let Some(window) = app.windows.get(app.active) else {
        return;
    };
    if window.send(bytes).is_ok() {
        return;
    }
    let id = window.session_id.clone();
    set_notice(app, format!("could not write to session {id}; closing its window"));
    close_window_of(app, &id);
}

/// この周に描くか。**再描画は「PTY に新出力」「UI イベント」「無変化でも
/// [`IDLE_REDRAW`] 周期（スピナー・経過時間）」のときだけ**（無条件 60fps は
/// claude 画面全体の再構築が毎フレーム走り重い）。
///
/// `holding` は**すべてに優先する**: 子が画面を作り替えている最中に掴んだ
/// フレームはカーソルが中間位置で確定し、IME の変換窓がそこへ飛ぶ
/// （判断は [`crate::session::Session::holds_frame`]、上限も向こうが持つので
/// ここが false を返し続けることはない）
/// `idle_after` は無変化でも描き直す間隔（スピナーが回っている間だけ短くする ＝
/// [`SPINNER_REDRAW`] / [`IDLE_REDRAW`]）
fn should_draw(holding: bool, force: bool, pty_dirty: bool, since_draw: Duration, idle_after: Duration) -> bool {
    !holding && (force || pty_dirty || since_draw > idle_after)
}

pub(crate) fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> anyhow::Result<()> {
    let mut last_draw = std::time::Instant::now();
    let mut force_draw = true;
    loop {
        if app.last_live_scan.elapsed() > LIVE_SCAN_INTERVAL {
            // **前景では `child.try_wait()` が生死の唯一の真実。** 死んだ PTY の窓は
            // 閉じるが、**行は消さない**（一覧に残り `claude -r` で再開できる ＝
            // 窓を閉じる ≠ 行を消す）。**行へ書き戻すものは何も無い**: 窓が無い行は
            // 動かしているものが無いので、そのまま Stopped として描かれる
            while let Some(pos) = app.windows.iter_mut().position(|w| !w.alive()) {
                remove_window(app, pos);
            }
            app.last_live_scan = std::time::Instant::now();
        }
        // **hook の書き込みに気づいたら周期を待たずに読み直す。** ペインの中で
        // `/resume` `/clear` すると claude は新しいセッションの `SessionStart` を
        // その場で撃つので、これが「一覧に新しい行を出す合図」になる。
        // 見るのはファイルの長さと更新時刻だけ（中身は読まない）ので毎周でも安い
        let hooks_changed = hook_store_changed(&mut app.hook_stamp, app.source.hook_stamp());
        if hooks_changed {
            app.last_scan = instant_ago(SCAN_INTERVAL);
        }
        if app.last_scan.elapsed() > SCAN_INTERVAL {
            refresh_sessions(app);
            // 一覧を読み直した直後に hook の state を載せる。**順序に意味がある**:
            // 読み直しは丸ごとの置き換えなので、先に載せるとその場で上書きされる。
            // **張り替えより前**でもある: ペインの中で切り替わったことに気づく材料が
            // hook の写しなので、古い写しのまま張り替えを判断させない。
            // **読み直すのはファイルが実際に動いた周だけ**（何も起きていない
            // 2 秒周期のたびに全読み + JSON パースをやり直さない）
            if hooks_changed {
                adopt_hook_states(app);
            }
            // ペインの中で `/resume` された窓を新しいセッションの行へ張り替える。
            // **名前の読み直しの前**に置く: 張り替えで作った行の表示名は
            // transcript から来るので、同じ周期の refresh_transcripts が拾う
            follow_session_switches(app);
            refresh_transcripts(app);
            app.last_scan = std::time::Instant::now();
            force_draw = true; // 並びが変わったら即描画（表示と行データのずれを残さない）
        }
        // 起こした子が端末を掴んだら門番を降ろす（前景では宛先は起動の時点で
        // 決まっているので、待つのは「子が入力を読める状態になるまで」だけ）
        if app.input_gate.is_some()
            && let Some(id) = app
                .windows
                .get(app.active)
                .filter(|w| w.started())
                .map(|w| w.session_id.clone())
        {
            lift_input_gate(app, Some(&id));
            force_draw = true;
        }
        // 起動が応答しないまま期限を過ぎたら入力を取り戻す。**打鍵が無くても
        // 通知が出る**ように run ループ側で見る（門番の中で期限を見ると、
        // ハングに気づけるのが「打った人」だけになる）
        if expire_input_gate(app) {
            force_draw = true;
        }
        // 期限切れの通知を落とす（キーヒントと使用率の表示に戻す）
        if expire_notice(app) {
            force_draw = true;
        }
        // 使用率の更新を取り込む（取得スレッドが旗を立てたときだけ。
        // 実データなら [`crate::usage`] の取得結果、撮影用なら固定値で、
        // どちらを読むかは供給元が決める）
        if app
            .usage_dirty
            .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            let usage = app.source.usage();
            if usage != app.usage {
                app.usage = usage;
                force_draw = true;
            }
        }
        // フッター（アカウント・バージョン）の更新を取り込む
        if app
            .footer_dirty
            .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            app.footer = app
                .footer_shared
                .lock_recover()
                .clone();
            force_draw = true;
        }
        // ccdesk 自身の更新の失敗を下部バーへ出す。成功は版行の "restart" が伝えるので
        // ここでは扱わない（Idle へ戻すので、失敗した更新はもう一度押せる）
        let failure = {
            let mut state = app
                .ccdesk_update
                .lock_recover();
            match &*state {
                SelfUpdate::Failed(msg) => {
                    let msg = msg.clone();
                    *state = SelfUpdate::Idle;
                    Some(msg)
                }
                _ => None,
            }
        };
        if let Some(msg) = failure {
            set_notice(app, msg);
            force_draw = true;
        }
        // ccdesk 自身の新しいリリース（起動時チェック）を取り込む
        if app
            .ccdesk_latest_dirty
            .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            app.ccdesk_latest = app
                .ccdesk_latest_shared
                .lock_recover()
                .clone();
            force_draw = true;
        }
        // agents --json のライブ状態を取り込む（state 変化の即時反映）。
        // **生死はここでは見ない**（前景セッションは自分の子なので `try_wait` が真実）
        if app
            .agents_dirty
            .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            app.agents = app
                .agents_shared
                .lock_recover()
                .clone();
            force_draw = true;
        }
        // claude が画面を作り替えている最中は掴まない（[`Session::holds_frame`]）。
        // 途中で掴むとカーソルが中間位置で確定し、IME の変換窓がそこへ飛ぶ。
        // 見るのは右ペインに出ている窓だけ ＝ New 画面は自前のカーソルなので待たせない
        let holding = !matches!(app.right_view, RightView::New(_))
            && app
                .windows
                .get(app.active)
                .is_some_and(|w| w.holds_frame(last_draw.elapsed()));
        // **合図（dirty）を降ろすのは実際に描く周だけ**なので、読むのはここ。
        // 見送る周で降ろすと、次の出力が来るまで画面が古いままになる
        let pty_dirty = app
            .windows
            .iter()
            .any(|w| w.dirty.load(std::sync::atomic::Ordering::Relaxed));
        // スピナー（Working の点滅）が出ている間だけ短い周期で描き直す。
        // 出ていなければ動くのは経過時間（1 秒粒度）だけなので、それに合わせる
        let idle_after = if app.spinner_active {
            SPINNER_REDRAW
        } else {
            IDLE_REDRAW
        };
        if should_draw(holding, force_draw, pty_dirty, last_draw.elapsed(), idle_after) {
            for window in &app.windows {
                window
                    .dirty
                    .store(false, std::sync::atomic::Ordering::Relaxed);
            }
            draw_frame(terminal, app)?;
            last_draw = std::time::Instant::now();
            force_draw = false;
        }

        // 見送った周は短く待ち直す。通常の周期のまま待つと、子が静まった直後ではなく
        // 次の周まで描画がずれ、保留の分だけ打鍵の反映が遅れて見える
        let wait = if holding { POLL_HELD } else { POLL_IDLE };
        if !crossterm::event::poll(wait)? {
            continue;
        }
        force_draw = true; // イベントを処理したら必ず描画
        match crossterm::event::read()? {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                // 予約キー（claude へ渡さない唯一の打鍵）は [`reserved_key`] が答える
                match reserved_key(&key) {
                    Some(Reserved::Quit) => return Ok(()),
                    Some(Reserved::Focus(to)) => {
                        app.set_focus(to);
                        continue;
                    }
                    None => {}
                }
                // サイドバーフォーカス中のキー操作（入力欄は名前の変更中だけ）
                if app.focus == Focus::Sidebar {
                    handle_sidebar_key(app, &key);
                    continue;
                }
                // 新規セッション画面のキー操作
                if let RightView::New(_) = app.right_view {
                    handle_new_view_key(app, &key)?;
                    continue;
                }
                // 起動処理中の打鍵は捨てる（宛先のセッションがまだ無い）
                if drop_input_while_starting(app) {
                    continue;
                }
                // フォーカスがターミナル側にあるときだけ PTY へ流す
                if app.windows.is_empty() {
                    continue;
                }
                let bytes = {
                    let window = &app.windows[app.active];
                    encode_key(&key, &window.parser.lock_recover())
                };
                if !bytes.is_empty() {
                    send_to_active(app, &bytes);
                }
            }
            Event::Paste(text) => {
                // New 画面の D&D/貼り付けの解釈は new_view 側（キー・マウスと同じ場所）
                if let RightView::New(state) = &mut app.right_view {
                    state.handle_paste(&text);
                    continue;
                }
                if app.focus != Focus::Terminal {
                    continue;
                }
                // 打鍵と同じ門番（貼り付けのほうが 1 回で送る量が多い ＝ 素通しの害が大きい）
                if drop_input_while_starting(app) {
                    continue;
                }
                if app.windows.is_empty() {
                    continue;
                }
                // sanitize と bracketed paste の包みはキー入力と同じ keys 側
                // （「入力を VT バイト列にする」知識を run ループに置かない）
                let bytes = {
                    let window = &app.windows[app.active];
                    crate::keys::encode_paste(&text, &window.parser.lock_recover())
                };
                send_to_active(app, &bytes);
            }
            Event::Mouse(mouse) => {
                let prev_hover = (app.hovered, app.usage_hovered);
                if handle_mouse(app, &mouse)? {
                    return Ok(());
                }
                if !mouse_needs_redraw(mouse.kind, prev_hover, (app.hovered, app.usage_hovered)) {
                    force_draw = false;
                }
            }
            Event::Resize(w, h) => resize_terminal(app, w, h),
            // ホスト端末のフォーカス変化をアクティブ PTY へ中継
            // （ターミナルペインがフォーカス中のときだけ意味を持つ）
            Event::FocusGained => {
                if app.focus == Focus::Terminal
                    && let Some(window) = app.windows.get_mut(app.active) {
                        window.send_focus(true);
                    }
            }
            Event::FocusLost => {
                if app.focus == Focus::Terminal
                    && let Some(window) = app.windows.get_mut(app.active) {
                        window.send_focus(false);
                    }
            }
            _ => {}
        }
    }
}

/// **ccdesk が横取りする打鍵。ここに無いキーは 1 つ残らず claude へ渡る。**
///
/// 予約を 2 つだけに絞ったのは、二次操作をポップアップへ集めたから。
/// 入口が「メニュー」と「ショートカット」の
/// 2 つあると、どちらが正なのか読む側にも実装側にも分岐が生まれるうえ、
/// **予約キーの数だけ claude 本体のキーバインドが死ぬ**（`Ctrl+S` / `Ctrl+X` は
/// 実際に claude 側の打鍵だった）。
///
/// 判定を 1 つの純関数にしてあるので、「素通しするはずのキーを増やしていないか」を
/// テストで固定できる（run ループの中に条件を散らすと検査できない）
#[derive(Clone, Copy, PartialEq, Debug)]
enum Reserved {
    /// 緊急脱出（マウスが効かない環境向け）
    Quit,
    /// ペインフォーカスの移動
    Focus(Focus),
}

fn reserved_key(key: &KeyEvent) -> Option<Reserved> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        // Shift の付いた `Ctrl+Shift+Q` は予約しない（1 打鍵でも減らす）
        KeyCode::Char('q') if ctrl => Some(Reserved::Quit),
        KeyCode::Left if alt => Some(Reserved::Focus(Focus::Sidebar)),
        KeyCode::Right if alt => Some(Reserved::Focus(Focus::Terminal)),
        _ => None,
    }
}

/// サイドバーにフォーカスがあるときのキー操作。
///
/// **ここは claude と打鍵を取り合わない**: サイドバーへ流れるのはフォーカスが
/// こちらにあるときだけで、端末側にフォーカスがあれば同じキーは PTY へ行く
/// （[`reserved_key`] に載っていないキーは横取りされない）。
///
/// 受け手の優先順は メニュー → 一覧。メニューは開いている間
/// **すべてのキーを飲む**（開いたまま裏の一覧が動くと、見えているものと
/// 効くものがずれる）
fn handle_sidebar_key(app: &mut App, key: &KeyEvent) {
    if app.popup.is_some() {
        handle_popup_key(app, key.code);
        return;
    }
    match key.code {
        KeyCode::Up => move_selection(app, -1),
        KeyCode::Down => move_selection(app, 1),
        KeyCode::Enter => run_enter(app),
        _ => {}
    }
}

/// 選択行で `Enter` が起こすこと。**この型が「下部バーに出す語」と「実行」の
/// 共通の正本**なので、行の種類を足したときに案内だけが黙って古くなることがない
/// （[`Self::label`] と [`run_enter`] はどちらもここを網羅する match ＝
/// 変種を足せばコンパイルが通らない）。
///
/// 行の種類からの写像は [`selected_enter`] 1 箇所で、そこは [`RowAction`] を
/// 網羅する match なので**行の種類を足しても漏れない**
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Enter {
    /// その行のメニューを開く（セッション行・見出し行・`⊞ group` 行・アカウント行）
    Menu,
    /// 新規セッション画面を開く（`+ new session`）
    NewSession,
    UpdateCcdesk,
    UpdateClaude,
}

impl Enter {
    /// 下部バーへ出す語（`Enter <label>` の形で並ぶ）
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Menu => "menu",
            Self::NewSession => "new session",
            // どちらの版行も利用者から見れば「更新する」1 つの動作
            Self::UpdateCcdesk | Self::UpdateClaude => "update",
        }
    }
}

/// いま選択している位置で `Enter` が起こすこと（何も起きない位置は `None`）。
///
/// **キーボードの実行と下部バーの案内が読む唯一の写像。** 押しても何も起きない行は
/// ここが `None` を返すことで表す ＝ 更新の無い版行（[`SidebarRow::Inert`]）と
/// アカウント行（[`SidebarPos::Account`]）が同じ 1 つの答えを共有する
pub(crate) fn selected_enter(app: &App) -> Option<Enter> {
    let SidebarPos::Row(row) = app.selection else {
        // アカウント行は今サインインしているアカウントを出すだけ
        return None;
    };
    match app.sidebar_rows.get(row)?.action()? {
        RowAction::New => Some(Enter::NewSession),
        RowAction::Open(_) | RowAction::Project(_) | RowAction::ToggleGroup => Some(Enter::Menu),
        RowAction::UpdateCcdesk => Some(Enter::UpdateCcdesk),
        RowAction::UpdateClaude => Some(Enter::UpdateClaude),
    }
}

/// **`Enter` = 選択行の動作。** サイドバーのキーは `↑↓`（選択）とこれだけで、
/// `←` `→` は持たない: 「開く」と「メニュー」の 2 つを持つのはセッション行だけ
/// なので、方向で区別すると他の行では嘘の案内になる。セッションを開く導線は
/// メニューの `open`（[`PopupKind::Session`] の先頭項目）へ寄せた。
///
/// **実行表はクリックと同じ [`run_row_action`]**。違うのはセッション行だけ
/// （Enter = メニュー / クリック = 開く）で、その 1 つの差だけをここに書く。
/// 位置はクリックで開くときと同じ [`selected_row_y`]（開き方で場所が変わらない）
fn run_enter(app: &mut App) {
    let anchor_y = selected_row_y(app);
    let SidebarPos::Row(row) = app.selection else {
        return; // メニューを持たない位置（アカウント行）
    };
    let Some(action) = app.sidebar_rows.get(row).and_then(SidebarRow::action).cloned() else {
        return;
    };
    match action {
        RowAction::Open(id) => open_session_popup(app, &id, anchor_y),
        other => run_row_action(app, other, anchor_y),
    }
}

/// 行の動作の実行表。**キーボード（Enter）とクリックが同じ 1 つの表を通る**ので、
/// [`RowAction`] を足したときに「クリックでは効くのに Enter では無反応」の形が
/// 作れない（match は網羅なのでコンパイラが両経路ぶんを一度に要求する）。
/// セッション行（Open）だけは入口ごとに意味が違うので、呼び手が先に分岐する
fn run_row_action(app: &mut App, action: RowAction, anchor_y: u16) {
    match action {
        RowAction::New => {
            app.open_new_view();
            app.set_focus(Focus::Terminal);
        }
        RowAction::ToggleGroup => open_popup(app, PopupKind::Group, anchor_y),
        // 見出し行はメニューを開くだけ。**フォーカスは移さない**（メニューがキーを受ける）
        RowAction::Project(cwd) => open_project_popup(app, cwd, anchor_y),
        // 更新行はその場で実行するだけ（右ペインを切り替えない）
        RowAction::UpdateCcdesk => start_ccdesk_update(app),
        RowAction::UpdateClaude => start_claude_update(app),
        // セッション行は呼び手が経路を選ぶ（クリック = 開く / Enter = メニュー）
        RowAction::Open(id) => {
            if open_session(app, &id) {
                app.set_focus(Focus::Terminal);
            }
        }
    }
}

/// New 画面からの起動。**セッションの実体は ccdesk の子プロセス**になり、
/// ccdesk を閉じると終わる（行は `sessions.json` に残り `claude -r` で再開できる）
pub(crate) fn start_new_session(app: &mut App) -> anyhow::Result<()> {
    let RightView::New(state) = &app.right_view else {
        return Ok(());
    };
    let cwd = state.cur_dir.clone();
    let prompt = state.prompt.text.trim().to_string();
    dispatch_session(app, cwd, prompt);
    Ok(())
}

/// 起動 1 回の結果を状態へ反映する（[`dispatch_session`] だけが呼ぶ）。
///
/// **「そのフォルダを使った」の記録をここ 1 箇所に集める**のが要点: 登録プロジェクトと
/// new session 画面の初期値（[`WindowItem::LastFolder`]）は同じ操作に対する 2 つの
/// 永続化なので、判断が別だと通知は失敗を報告しているのに見出しだけが生える。
/// 起動できないフォルダ（打ち間違い・権限が無い・古いネットワークパス）を登録すると
/// state.json に永久に残るので、**成功した起動だけを記録する**。打った文字列は
/// `dispatch_cwd`（メモリ上の初期値）に残るので、直して押し直す邪魔にはならない
fn apply_launch(app: &mut App, cwd: String, launched: Launched) {
    // 成否の判定は `Result` 1 つ。`Ok(None)`（セッションを起こさない撮影用の供給元）は
    // 「起動を試していない ＝ 失敗もしていない」ので記録する側へ倒す
    match launched {
        Ok(started) => {
            app.source.save_window(WindowItem::LastFolder(&cwd));
            register_project(app, &cwd);
            // **門番を立てるのは起こせたときだけ。** 子が端末を掴むまでの打鍵は
            // 捨てる（run ループが最初の出力で降ろす。理由は
            // [`drop_input_while_starting`]）
            if started.is_some() {
                app.input_gate = Some(std::time::Instant::now());
            } else {
                // 起こしていない ＝ 待つものが無いので、打ち先だけ確かめて戻す
                lift_input_gate(app, None);
            }
        }
        Err(err) => {
            // 宛先のセッションが無いまま端末にフォーカスが残ると、打った文字が
            // 直前まで見ていたセッションへ流れる（[`lift_input_gate`] が戻す）
            lift_input_gate(app, None);
            app.set_focus(Focus::Sidebar);
            set_notice(app, err);
        }
    }
}

/// 登録プロジェクト一覧を保存し、**永続化された内容を自分の一覧として取り込む**。
/// 一覧を変える 3 つの操作（登録・埋め戻し・登録解除）はどれもここを通る。
///
/// 取り込みが要点: 保存はディスクとのマージと上限の適用を通るので、渡した一覧が
/// そのまま載るわけではない（[`crate::source::DataSource::store_projects`]）。
/// 取り込まないと、**上限で落ちた登録が画面には出続けるのに再起動で消える**
/// ＝ 見出しの正本が state.json とメモリの 2 箇所に割れる。あわせて他インスタンスの
/// 登録もこの時点で一覧に入るので、次の保存でそれを「自分が外した」と読ませない
fn save_projects(app: &mut App) {
    app.projects = app.source.store_projects(&app.projects);
}

/// セッション一覧を保存し、**永続化された内容を自分の一覧として取り込む**。
/// 一覧を変える操作（起動・状態の更新・削除）はどれもここを通る。
///
/// 取り込む理由は [`save_projects`] と同じで、保存はディスクとのマージを通るので
/// 渡した一覧と保存された一覧は一致しない（他インスタンスが起こしたセッションが
/// 増える）。取り込まないと次の保存でそれを「自分が削除した」と読ませてしまう
/// （[`crate::sessions`] の `merge_sessions`）
fn save_sessions(app: &mut App) {
    app.sessions = app.source.store_sessions(&app.sessions);
}

/// 一覧をディスクから読み直す（他インスタンスが起こしたセッションを取り込む。
/// 読むたびにマージの基準も進む ＝ [`crate::sessions::SessionStore::list`]）。
///
/// **開いている窓の行は必ず残す。** 読み直しは丸ごとの置き換えなので、保存が
/// まだディスクへ載っていない（ロックが取れなかった）間に読むと、その行だけが
/// 消えて**プロセスは動いているのにサイドバーのどこからも指せない**状態になる。
/// 落ちていた行はここで戻し、次の保存でもう一度ディスクへ載せに行く
fn refresh_sessions(app: &mut App) {
    let open: Vec<SessionId> = app.windows.iter().map(|w| w.session_id.clone()).collect();
    let fresh = app.source.sessions();
    let dropped: Vec<SessionRow> = rows_dropped_while_open(&fresh, &app.sessions, &open)
        .into_iter()
        .cloned()
        .collect();
    app.sessions = fresh;
    if !dropped.is_empty() {
        app.sessions.extend(dropped);
        save_sessions(app);
    }
}

/// 読み直しで落ちてしまった「窓が開いている行」（[`refresh_sessions`] の判断。
/// 副作用を持たないので単体で検査できる）。
///
/// 戻す対象を**窓が開いている行だけ**に絞るのが要点: そうしないと他インスタンスの
/// `close` が自分の写しから復活し続ける（行外しがどちらのインスタンスからも効かなくなる）
fn rows_dropped_while_open<'a>(
    fresh: &[SessionRow],
    mine: &'a [SessionRow],
    open: &[SessionId],
) -> Vec<&'a SessionRow> {
    mine.iter()
        .filter(|row| {
            open.contains(&row.session_id)
                && !fresh.iter().any(|r| r.session_id == row.session_id)
        })
        .collect()
}

/// hook の受け渡しファイルが前回見たときから変わったか（一覧の読み直しを
/// 周期より前へ倒す判断。副作用は「見え方を覚え直す」だけなので単体で検査できる）。
///
/// **見え方が取れないときは前倒ししない**: 追いかけるファイルを持たない供給元
/// （撮影用）と、まだ hook が一度も書いていない状態を同じに扱う ＝
/// 「無い」が毎周「変わった」に化けない
fn hook_store_changed(
    seen: &mut Option<(u64, std::time::SystemTime)>,
    now: Option<(u64, std::time::SystemTime)>,
) -> bool {
    if now.is_none() || now == *seen {
        return false;
    }
    *seen = now;
    true
}

/// その pid が**今**動かしているセッション。材料は 2 つあり、**hook が主**:
///
/// - **hook**（[`crate::hooks::HookStates::session_of`]）: claude は hook の子へ
///   `CLAUDE_PID` を渡すので、記録は「どの claude が」「どのセッションで」起きたかを
///   持っている。**turn が動いた瞬間に届く**ので、`/resume` `/clear` の張り替えを
///   周期で待たない
/// - **`claude agents --json`**（`~/.claude/sessions/<pid>.json` 由来）: hook を
///   注入できていないセッションのための従経路。2 秒周期のプロセス起動で届く
///
/// pid が分からない / どちらにもエントリが無い / `sessionId` が空なら None
/// （＝ 何も張り替えない。npm 版のように `claude` が中間プロセス越しに起動する
/// 環境では自分の子の pid が載らないので、この機能は黙って効かないだけになる）
fn live_session_of(
    pid: Option<u32>,
    launched_at: u64,
    hooks: &HookStates,
    agents: &[AgentInfo],
) -> Option<SessionId> {
    let pid = pid?;
    if let Some(id) = hooks.session_of(pid, launched_at) {
        return Some(id.clone());
    }
    agents
        .iter()
        .filter(|a| a.is_interactive() && a.pid == Some(pid))
        .map(|a| SessionId::new(a.session_id.clone()))
        .find(|id| !id.is_empty())
}

/// ペインの中で `/resume` された窓を、**今動かしているセッションの行へ張り替える**。
///
/// `/resume` は claude の内部で起きるので ccdesk は関与しない ＝ 窓が起動時の UUID を
/// 指したままだと、行が古い会話（と古い名前）を指し続ける。追従の鍵は pid で、
/// 自分の子の pid はこちらが知っており、その pid が今どのセッションを動かしているかは
/// [`live_session_of`] が答える。
///
/// **張り替えの検出はこの関数の比較 1 箇所だけ。** 行の作成・表示名・`last_view` の
/// 更新はすべて [`adopt_switched_session`] が受け持つので、「切り替わったか」の
/// 判断が 2 箇所に増えない
fn follow_session_switches(app: &mut App) {
    let switches: Vec<(usize, SessionId)> = app
        .windows
        .iter()
        .enumerate()
        .filter_map(|(i, window)| {
            let next = live_session_of(
                window.child.process_id(),
                window.started_at,
                &app.hook_states,
                &app.agents,
            )?;
            (next != window.session_id).then_some((i, next))
        })
        .collect();
    for (index, next) in switches {
        let Some(window) = app.windows.get_mut(index) else {
            continue;
        };
        let previous = std::mem::replace(&mut window.session_id, next.clone());
        adopt_switched_session(app, &previous, &next, index == app.active);
    }
}

/// 張り替え先の行を用意する（[`follow_session_switches`] だけが呼ぶ）。
///
/// **窓に触らないので単体で検査できる**（PTY を起こさずに、行の作成・`last_view` の
/// 保存を確かめられる）。`shown` は「その窓が今ペインに出ているか」＝
/// `last_view` を書き換えてよいかの判断。
///
/// 行が無ければ作る（`/resume` で選べる会話は ccdesk の一覧に無いこともある）。
/// **名前は行が持たない**ので、作った行の表示名は次の描画で transcript から導かれる
fn adopt_switched_session(app: &mut App, previous: &SessionId, next: &SessionId, shown: bool) {
    let cwd = app
        .row(previous)
        .map(|row| row.cwd.clone())
        .unwrap_or_default();
    if app.row(next).is_none() {
        app.sessions.push(SessionRow::new(next.clone(), cwd, now_ms()));
        save_sessions(app);
    }
    // **次回起動で開く画面もこの窓の新しいセッションにする。** 保存は
    // [`App::show_session`] と同じ [`WindowItem::LastView`] だが、切替の契機は
    // ccdesk の外（claude の `/resume`）なのでその経路を通らない ＝ ここで書く。
    // 呼ばれるのは張り替わった周期だけなので、書き込みも張り替え 1 回につき 1 度
    if shown {
        app.source.save_window(WindowItem::LastView(Some(next)));
        // **離れた行へは何も書かない。** 窓が新しいセッションへ移った時点でその行を
        // 動かしているものは無くなり、次の描画でそのまま Stopped になる
        // （書き戻していた頃は、hook が持つ新しい記録より古い `stopped` が行に残った）。
        // 既読にするのは**ペインに出ている窓**の張り替えだけ: 裏へ回した窓の
        // `/resume` が遅れて反映された周で、見てもいない行の未読 ● を消さない
        mark_read(app, next);
    }
}

/// hook が書いた state を読み直す。**行へは何も写さない**（写していた頃は
/// 保管と hook が食い違い、しかもどちらが新しいかが行ごとに逆になった）。
///
/// 唯一の書き込みが「**ペインに出ている行を既読にする**」で、これは未読の材料が
/// hook の `at` になったことの裏返し: `UserPromptSubmit` はユーザー自身の打鍵でも
/// 飛ぶので、見ている行の記録が進んだらその場で既読を合わせる
fn adopt_hook_states(app: &mut App) {
    let previous = std::mem::replace(&mut app.hook_states, app.source.hook_states());
    // **ターンが終わった行があれば使用率を取り直す。** 使用率が動くのはこの瞬間だけで、
    // 周期で叩き続けるより正確なうえ、何もしていない間は claude を 1 プロセスも
    // 起こさない（間引きは供給元の側。[`crate::usage`]）
    if app.hook_states.any_turn_finished_since(&previous) {
        app.source.note_turn_finished();
    }
    let shown: Option<SessionId> = app.shown_session().cloned();
    if let Some(id) = shown {
        mark_read(app, &id);
    }
}

/// transcript を解決し直し、増えたぶんを走査する（読み方は [`crate::title`]）。
///
/// **行に書き戻すのは transcript の場所だけ。** 表示名は行が持たず、描画のたびに
/// [`Titles::of`] が導く ＝ 名前が変わっても `updated_at` は動かない
/// （動かしていた頃は、claude が名前を付け直すたびに行の経過時間が 0s へ戻った）。
///
/// **保存するのは解決が動いた周だけ。** 解決結果は再起動後も使う値なので
/// `sessions.json` に載せるが、`updated_at` は進めない（行の中身が変わったわけでは
/// なく、同じ会話の在り処が分かっただけ）。
///
/// **起動直後にも 1 度呼ぶ**（`main.rs` の起動列）。走査の結果を持っているのは
/// [`Titles`] のキャッシュだけなので、呼ばないと最初の周期（2 秒）が来るまで
/// **全部の行が `new session` に見える**うえ、未記録の行が解決し直されるのも
/// その周期まで遅れる
pub(crate) fn refresh_transcripts(app: &mut App) {
    let mut titles = std::mem::take(&mut app.titles);
    let mut changed = false;
    // 1 周期に読む量の上限。初回の全量読み（リネーム記録の探索）を数周期へ分け、
    // UI スレッド ＝ 最初の描画を数百 MB の read で止めない
    // （[`crate::title::SCAN_BUDGET`]）
    let mut budget = crate::title::SCAN_BUDGET;
    for row in &mut app.sessions {
        changed |= titles.refresh(row, &mut budget);
    }
    app.titles = titles;
    if changed {
        save_sessions(app);
    }
}

/// その行を既読にする。**契機は 3 つで、どれも同じことをする**:
/// ペインを開いた（開き方は問わない）・`mark as read`・ペインに出ている行へ
/// hook が届いた（[`adopt_hook_states`]）。
///
/// 進めるのは `last_opened_at` だけで **`updated_at` は触らない**: 既読にしても
/// 行の姿は変わらないので、経過時間（`· 23s`）が 0s へ戻ってはいけない。
/// 既に読み終えている行なら書き込みもしない（周期処理が毎周保管を書かない）
fn mark_read(app: &mut App, id: &SessionId) {
    let now = now_ms();
    let Some(row) = app.row(id) else {
        return;
    };
    // 時計が巻き戻っても既読を巻き戻さない。既読の行は書かない
    if row.last_opened_at >= now || !app.hook_states.unread(row) {
        return;
    }
    if let Some(row) = app.row_mut(id) {
        row.last_opened_at = now;
    }
    save_sessions(app);
}

/// **メニューからの行操作を保存する唯一の口**（今はピン留めだけ）。
/// 行が無ければ何もしない（メニューを開いたまま他インスタンスが消した場合）。
///
/// `updated_at` を進めるのはマージの後勝ち判定に要るから
/// （[`crate::sessions`] の `merge_sessions`）。**進めるのは行の中身が実際に
/// 変わったときだけ**: `updated_at` は行に出る経過時間の材料でもあるので
/// （[`crate::hooks::HookStates::changed_at`]）、何も変えない操作で進めると
/// **行に何も起きていないのに経過時間が 0s へ戻る**。
///
/// **未読には触らない。** 未読の材料は hook の `at` だけなので、行を書き換えても
/// `●` は点かないし消えない ＝ 「自分の操作で未読が生えない」は保証ではなく構造
fn edit_row(app: &mut App, id: &SessionId, edit: impl FnOnce(&mut SessionRow)) {
    let changed = {
        let Some(row) = app.row_mut(id) else {
            return;
        };
        let before = row.clone();
        edit(row);
        if *row != before {
            row.updated_at = now_ms();
            true
        } else {
            false
        }
    };
    if changed {
        save_sessions(app);
    }
}

/// そのフォルダを登録プロジェクトへ加える。**呼ばれるのは [`apply_launch`]
/// だけ**（明示的な「追加」UI は持たず、セッションの起動が成功した時点で登録される。
/// 「登録するか」の判断を散らさないため、呼び出し口を増やさない）。
///
/// 並びは**最近使った順**で、末尾が最後に使ったフォルダ。既に登録済みでも末尾へ
/// 動かすのが要点: 追い出しは先頭から起きるので、動かさないと「毎日使っているが
/// 最初に登録したフォルダ」が次の 1 件で落ちる ＝ 上限が LRU ではなく FIFO になり、
/// この一覧が防ごうとしている失敗（最後のセッションを消すと見出し＝入口が消える）を
/// 自分で招く。画面上の並びは見出し側がアルファベット順に決めるので、
/// この並び替えは表示には出ない
fn register_project(app: &mut App, cwd: &str) {
    if cwd.is_empty() {
        return;
    }
    // 登録済みなら**その表記のまま**末尾へ動かす（同じフォルダの大小・末尾区切り違いで
    // 保存済みの見た目が入れ替わらない。同一性の判定は same_dir が持つ）
    let entry = match app.projects.iter().position(|p| same_dir(p, cwd)) {
        Some(i) => app.projects.remove(i),
        None => cwd.to_string(),
    };
    app.projects.push(entry);
    // 上限の適用はここではやらない。**上限と追い出しの規則は保存の正本
    // （`merge_projects`）1 箇所**にあり、[`save_projects`] が適用後の一覧を
    // 取り込むので画面もそれに揃う（2 箇所で削ると、追い出しの向きを変えた
    // ときに片方だけ直して「画面では消えたのに次の保存で戻る」が作れる）
    save_projects(app);
}

/// 起動時に、既にあるセッションの cwd を登録へ埋め戻す（初回読み込みで 1 度だけ）。
///
/// **既存ユーザーのための経路**: 登録は [`register_project`]（＝ ccdesk から
/// 立てたセッションの起動が成功したとき）だけで起きるので、以前から使っているフォルダは
/// 「セッションの cwd」由来でしか見出しが出ない ＝ 最後のセッションを消した時点で
/// 見出し（＝そのフォルダで新規を開く入口）が消える。ccdesk から次のセッションを
/// 立てるまでその状態が続くので、起動時に埋めておく。
///
/// **上限を超えていても既存の登録は落とさない**のが [`register_project`] との違い:
/// 登録はユーザーの操作の記録（state.json が唯一の正本）で、埋め戻しは入口を
/// 増やすためのものなので、空きが尽きたらそこで止める。空きの取り合いになったら
/// **最近更新した行のフォルダを優先する**（一覧の並び順は保存順で新旧を表さないので、
/// ここで `updated_at` の降順に並べ直してから積む）
pub(crate) fn backfill_projects(app: &mut App) {
    let room = PROJECTS_LIMIT.saturating_sub(app.projects.len());
    let mut newest: Vec<&SessionRow> = app.sessions.iter().collect();
    newest.sort_by_key(|r| std::cmp::Reverse(r.updated_at));
    let mut fresh: Vec<String> = Vec::new();
    for row in newest {
        if fresh.len() >= room {
            break;
        }
        // cwd の取れなかった行から空の見出しを作らない（register_project と同じ扱い）
        if row.cwd.is_empty() {
            continue;
        }
        if app.projects.iter().chain(fresh.iter()).any(|p| same_dir(p, &row.cwd)) {
            continue;
        }
        fresh.push(row.cwd.clone());
    }
    if fresh.is_empty() {
        return;
    }
    // 登録の並びは最近使った順（末尾が最新）なので、新しい順の一覧を逆に積む
    fresh.reverse();
    app.projects.extend(fresh);
    save_projects(app);
}

/// 登録プロジェクトから外す。セッションが残っているかの判断はメニュー側
/// （[`PopupKind::entries`] が項目を無効にする）で済んでいるので、ここは削るだけ
fn remove_project(app: &mut App, cwd: &str) {
    let before = app.projects.len();
    app.projects.retain(|p| !same_dir(p, cwd));
    if app.projects.len() != before {
        save_projects(app);
    }
}

/// そのフォルダにセッションがあるか。材料は一覧の行で、
/// **描画側が見出しの配下へ振り分ける集合と同じ**（別の集合を見ると、
/// 行が出ているのに `remove project` が押せてしまう）
fn project_has_sessions(app: &App, cwd: &str) -> bool {
    app.sessions.iter().any(|row| same_dir(&row.cwd, cwd))
}

/// プロジェクト見出し行のメニューを開く（Enter とクリックで同じものが出る）。
/// `has_sessions` は開いた時点の写しにする（[`PopupKind::Project`] 参照）
fn open_project_popup(app: &mut App, cwd: String, anchor_y: u16) {
    let has_sessions = project_has_sessions(app, &cwd);
    open_popup(app, PopupKind::Project { cwd, has_sessions }, anchor_y);
}

/// キーボード選択位置の画面 y。**式そのものは描画側が持つ**（一覧の行は [`row_y`]、
/// アカウント行は [`sidebar_layout`] の `account_y`）。位置の対応が 2 つあると
/// メニューが行からずれて出るので、**キーボードもマウスもこの 1 つの計算に乗せる**。
/// メニューの矩形はこの 1 つ下に出るので、Enter でメニューを開く位置は全部これを使う
fn selected_row_y(app: &App) -> u16 {
    match app.selection {
        SidebarPos::Row(row) => row_y(row, app.sidebar_header_rows, app.sidebar_scroll),
        SidebarPos::Account => sidebar_layout(app).account_y,
    }
}

/// 指定フォルダ・プロンプトで前景セッションを 1 本起こす
/// （見出しメニューの new session は空プロンプトで直接ここに来る）。
///
/// **PTY の起動は同期**（数 ms）。結果を待つ別スレッドが要らないので、
/// 起動と反映が 1 本の流れに収まる
fn dispatch_session(app: &mut App, cwd: String, prompt: String) {
    // フォルダの登録はここでは行わない（起動が成功してから ＝ [`apply_launch`]）。
    // 打った文字列は new session 画面の初期値として持つだけに留める
    app.dispatch_cwd = cwd.clone();
    // 起動したら打ち先はそのセッションなので、フォーカスを端末へ移す。
    // **ここに置くのが要点**で、new session 画面の起動ボタンと見出しメニューの
    // new session はどちらもこの関数へ収束するため、経路が増えても漏れが起きない
    // （[`App::show_session`] は「フォーカスは動かさない」契約で切替と共用）
    app.set_focus(Focus::Terminal);
    // 撮影用データは本物のセッションを起こさない（架空の一覧に実セッションが混ざらない）。
    // 起動しない ＝ 失敗もしないので `Ok(None)` で実データと同じ反映経路へ渡す
    // （demo だけフォルダの登録の意味が違う、という状態を作らない）
    let launched = if app.source.spawns_sessions() {
        start_foreground(app, &cwd, &prompt)
    } else {
        Ok(None)
    };
    apply_launch(app, cwd, launched);
}

/// 新規の前景セッションを起こし、一覧へ行を足してその窓を表示する。
///
/// **UUID は ccdesk が採番する**（`claude --session-id` へ渡した値がそのまま
/// transcript の `sessionId` になる ＝ 行と claude 側の記録が同じ鍵で結びつく）。
/// 新規生成なので同 cwd の既存 transcript と衝突しない。
///
/// **行を足すのは起動できてから**（起動できなかったセッションを一覧に残さない）
fn start_foreground(app: &mut App, cwd: &str, prompt: &str) -> Launched {
    let session_id = SessionId::new(uuid::Uuid::new_v4().to_string());
    // state を取る hook を注入する（statusLine は載せない ＝ ユーザーの
    // statusline を奪わない。[`crate::hooks::inject_settings`]）
    let settings = hook_settings(app);
    let (rows, cols) = app.pane_size();
    let window = Session::spawn(
        &session_id,
        cwd,
        rows,
        cols,
        Launch::New { prompt },
        settings.as_deref(),
    )
    .map_err(|e| format!("failed to start session: {e}"))?;
    // **名前は入れない。** 1 ターン目が終わるまで transcript は無いので、
    // それまでこの行は [`UNTITLED`] で出る（起動プロンプトの写しを行へ置くと、
    // 正本が 2 つになって同じ問題が戻る）
    app.sessions.push(SessionRow::new(session_id.clone(), cwd, now_ms()));
    save_sessions(app);
    app.windows.push(window);
    app.show_session(app.windows.len() - 1);
    Ok(Some(session_id))
}

/// 端末サイズが変わったときの反映。
///
/// **サイドバー幅は触らない。** 画面に出す桁数は端末幅から導く（[`sidebar_cols`]）ので、
/// 狭い端末では自動で縮み、広がればユーザーが選んだ幅へ戻る。
/// **ここで丸めて保存値を書き換えてはいけない**: 端末サイズ変化は一時的なことがあり
/// （Windows では PTY の破棄でも届く）、書き換えると縮んだ幅が戻らなくなる
fn resize_terminal(app: &mut App, w: u16, h: u16) {
    app.term_size = (w, h);
    app.resize_sessions();
}

/// マウスイベントの後に描き直す必要があるか（FPS 対策）。
///
/// **移動だけのイベントで変わり得る表示はホバー（一覧の行と使用率ゲージ）だけ**
/// なので、そこが同じなら描き直さない。移動以外（クリック・ホイール・ドラッグ）は
/// 表示を変えるので常に描く
fn mouse_needs_redraw(
    kind: MouseEventKind,
    prev_hover: (Option<SidebarPos>, bool),
    hover: (Option<SidebarPos>, bool),
) -> bool {
    !matches!(kind, MouseEventKind::Moved) || prev_hover != hover
}

/// マウス処理。true を返したら終了。
fn handle_mouse(app: &mut App, mouse: &MouseEvent) -> anyhow::Result<bool> {
    // モーダル表示中はモーダルが全クリックを受ける。**幅変更のつかみ代より先に**
    // 判定するのが要点で、内容から幅を決めるメニューは境界線の列に被り得るため、
    // 被った列の項目クリックがサイドバー幅変更に化けてはいけない
    // （ドラッグ中だけは掴んだ操作を優先する = 下のドラッグ分岐へ落とす）
    if app.popup.is_some() && !app.dragging {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
            handle_popup_click(app, mouse.column, mouse.row);
        }
        return Ok(false);
    }
    // 境界線ドラッグ（サイドバー右枠線と右ペイン左枠線の 2 列をつかみ代にする）。
    // **当たり判定は描画と同じ導出幅**（[`sidebar_cols`]）を見る
    let drawn = sidebar_cols(app);
    let border_zone = mouse.column >= drawn.saturating_sub(1) && mouse.column <= drawn;
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) if border_zone => {
            app.dragging = true;
            return Ok(false);
        }
        MouseEventKind::Drag(MouseButton::Left) if app.dragging => {
            // **ユーザーが選んだ幅を書き換える唯一の経路**（保存もここから）。
            // 端末に収まらない位置まで引いても、載せるのは収まる値だけ
            app.sidebar_width = fit_sidebar(mouse.column.saturating_add(1), app.term_size.0);
            // PTY リサイズは間引く（claude 側の全再レイアウト連打を避ける）
            if app.last_drag_resize.elapsed() > Duration::from_millis(50) {
                app.resize_sessions();
                app.last_drag_resize = std::time::Instant::now();
            }
            return Ok(false);
        }
        MouseEventKind::Up(MouseButton::Left) if app.dragging => {
            app.dragging = false;
            app.resize_sessions(); // 最終サイズを確定
            app.source
                .save_window(WindowItem::SidebarWidth(app.sidebar_width));
            return Ok(false);
        }
        _ if app.dragging => return Ok(false),
        _ => {}
    }

    // 下部バーの使用率をクリックしたらその場で取り直す（周期を待たない）。
    // **サイドバー／右ペインの振り分けより先**に見るのが要点で、使用率は右端 ＝
    // 右ペインの列範囲に描かれるため、後回しにするとペインのクリックに食われる。
    // 当たり判定は描画と同じ導出（[`crate::ui::usage_hit`]）なので、
    // 出していないとき（notice 表示中・狭い端末・使用率を切っている）は当たらない
    let on_usage = crate::ui::usage_hit(app)
        .is_some_and(|hit| mouse.row == hit.row && hit.columns.contains(&mouse.column));
    // 乗っている間は帯で「押せる」ことを示す（一覧の行のホバーと同じ手段）
    app.usage_hovered = on_usage;
    if on_usage && let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
        // 取り直しを実際に頼めたときだけスピナーを始める（撮影用の供給元は
        // 取得しない ＝ 降ろす者がいない旗を立てない）。ここで立てるのは
        // クリック直後のフレームから回すため（取得スレッド任せだと最初の
        // 描き直しまで押した反応が出ない）
        if app.source.refresh_usage() {
            app.usage_fetching
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        return Ok(false);
    }

    if mouse.column < drawn {
        let sl = sidebar_layout(app);
        // ホイールでサイドバーをスクロール（クランプは draw 側で行う）
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                app.sidebar_scroll = app.sidebar_scroll.saturating_sub(3);
                return Ok(false);
            }
            MouseEventKind::ScrollDown => {
                app.sidebar_scroll = app.sidebar_scroll.saturating_add(3);
                return Ok(false);
            }
            _ => {}
        }
        // アカウント行（フッター下段）はサイドバー一覧の行ではないので、
        // 一覧のヒットテスト（`row_at` はフッター帯を不感帯にする）ではなくここで受ける。
        // **列は見ない = 行のどこを押しても当たる**（一覧の行と同じ規則）。
        // **押しても何も起きない行**（今サインインしているアカウントを出すだけ）
        // なので、受けるのは選択とホバーだけ ＝ 更新の無い版行と同じ扱い
        if sl.footer_visible && mouse.row == sl.account_y {
            // ホバーもここで決める ＝ **当たり判定はクリックと同じこの 1 分岐**
            // （フッターを描いていない狭い端末はこの分岐に入らないのでホバーもしない）
            app.hovered = Some(SidebarPos::Account);
            if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
                app.set_focus(Focus::Sidebar);
                // 選択もここへ移す（キーボードで降りたときと同じ位置に居る）
                app.selection = SidebarPos::Account;
            }
            return Ok(false);
        }
        // 画面 y → 行 index（列は見ないので行のどこを押しても当たる）。
        // 計算は描画側と同じ ui::row_at を共有する
        let row = row_at(
            mouse.row,
            sl.capacity,
            app.sidebar_header_rows,
            app.sidebar_scroll,
        );
        // hover: **実体のある行**の上にいるときだけハイライト（飾りは光らせない）。
        // 押しても何も起きない行も行なので、ここは動作の有無では見ない。
        // **hover の判断に clone は要らない**（マウス移動は毎秒 100 回以上届くので、
        // Down のときだけ動作を写し取る）
        let selectable = app
            .sidebar_rows
            .get(row)
            .is_some_and(SidebarRow::selectable);
        app.hovered = selectable.then_some(SidebarPos::Row(row));
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
            let action = app.sidebar_rows.get(row).and_then(SidebarRow::action).cloned();
            // サイドバー内クリックはサイドバーへフォーカス。
            // 行クリックは右ペインの内容だけ切り替える（フォーカス移動は右ペインクリック or Enter）
            app.set_focus(Focus::Sidebar);
            if selectable {
                app.selection = SidebarPos::Row(row);
            }
            // 行末の `=` クリック → コンテキストメニューを開く。
            // **当たり判定は描画と同じ導出**（[`menu_zone`]）なので、
            // サイドバー幅を変えても見えている記号と押せる場所がずれない
            if let Some(RowAction::Open(id)) = &action
                && menu_zone(drawn).contains(&mouse.column) {
                    open_session_popup(app, &id.clone(), mouse.row);
                    return Ok(false);
                }
            // 実行表はキーボードの Enter と同じ [`run_row_action`]。
            // セッション行のクリックは「開く」（Enter はメニュー）で、開けたとき
            // だけフォーカスを端末へ移す判断も表の側にある
            if let Some(action) = action {
                run_row_action(app, action, mouse.row);
            }
        }
    } else {
        app.hovered = None;
        if let MouseEventKind::Down(_) = mouse.kind {
            app.set_focus(Focus::Terminal);
        }
        // 右ペイン矩形は state の可変借用の前に取る（正本は ui::pane_rect）
        let pane = crate::ui::pane_rect(app);
        // New 画面: 入力の解釈は new_view 側（ヒットテストのジオメトリ知識を
        // レイアウトと同じファイルに閉じる）。起動だけは state の借用を抜けて実行
        if let RightView::New(state) = &mut app.right_view {
            let action = state.handle_mouse(pane, mouse);
            if action == Some(crate::ui::new_view::NewAction::Launch) {
                start_new_session(app)?;
            }
            return Ok(false);
        }
        if app.windows.is_empty() {
            return Ok(false);
        }
        // 右ペイン: イベントを claude へ転送（ホイールも claude 自身がスクロール処理する）
        forward_mouse(app, mouse);
    }
    Ok(false)
}

/// 窓が開いていて子プロセスが生きているか。**前景では自分の子プロセスが
/// 唯一の真実**なので、生きた窓を持たない行はすべて停止済み
/// （`claude -r` で再開できる ＝ メニューの `close` は出せない）
fn session_open(app: &mut App, id: &SessionId) -> bool {
    app.windows
        .iter_mut()
        .any(|w| &w.session_id == id && w.alive())
}

/// メニューを開く唯一の口。開いた瞬間の共通処理（選択の初期値）をここで揃える
/// （種類ごとに `Popup` を組み立てると、開き方の作法が入口の数だけ増える）
fn open_popup(app: &mut App, kind: PopupKind, anchor_y: u16) {
    app.popup = Some(Popup {
        kind,
        anchor_y,
        selected: 0,
    });
}

/// セッション行のメニューを開く（行頭の `=` クリック / 選択行の `Enter`）。
/// 項目の見た目に効く 2 つ（ピン留め・窓の有無）は開いた時点の写し
fn open_session_popup(app: &mut App, id: &SessionId, anchor_y: u16) {
    let open = session_open(app, id);
    let row = app.row(id);
    let kind = PopupKind::Session {
        id: id.clone(),
        pinned: row.is_some_and(|r| r.pinned),
        open,
    };
    open_popup(app, kind, anchor_y);
}

/// モーダル表示中のキー操作（Esc = 全閉 / ↑↓ = 選択 / Enter = 実行）
fn handle_popup_key(app: &mut App, code: KeyCode) {
    let grouping = app.grouping;
    match code {
        // 階層を積まないので戻り先は無い。どの階層でも 1 度で全部閉じる
        KeyCode::Esc => app.popup = None,
        KeyCode::Up => {
            if let Some(popup) = app.popup.as_mut() {
                popup.selected = popup.selected.saturating_sub(1);
            }
        }
        KeyCode::Down => {
            if let Some(popup) = app.popup.as_mut() {
                let last = popup.kind.entries(grouping).len().saturating_sub(1);
                popup.selected = (popup.selected + 1).min(last);
            }
        }
        KeyCode::Enter => {
            if let Some(index) = app.popup.as_ref().map(|p| p.selected) {
                activate_popup(app, index);
            }
        }
        _ => {}
    }
}

/// モーダル内クリック
fn handle_popup_click(app: &mut App, col: u16, row: u16) {
    let Some(popup) = &app.popup else { return };
    let rect = popup_rect(app, popup);
    if !rect.contains(Position::new(col, row)) {
        app.popup = None; // 外クリックで閉じる（階層を持たないので全閉）
        return;
    }
    // 枠線上のクリックは何もしない（上枠が先頭項目に化けて誤発火しない）
    if row == rect.y
        || row == rect.y + rect.height - 1
        || col == rect.x
        || col == rect.x + rect.width - 1
    {
        return;
    }
    // 枠内に入りきらないメニューは描画がスクロールしている（[`crate::ui::popup_scroll`]）
    // ので、クリックの行 → 項目 index も同じずらしを通す
    let visible = rect.height.saturating_sub(2) as usize;
    let total = popup.kind.entries(app.grouping).len();
    let offset = crate::ui::popup_scroll(popup.selected, total, visible);
    activate_popup(app, offset + (row - rect.y - 1) as usize);
}

/// 選択項目の実行（Enter / クリック共通）。実行できない項目・範囲外の index は無視する
fn activate_popup(app: &mut App, index: usize) {
    let Some(popup) = app.popup.as_ref() else {
        return;
    };
    let mut entries = popup.kind.entries(app.grouping);
    if index >= entries.len() {
        return;
    }
    let entry = entries.swap_remove(index);
    if !entry.enabled {
        return;
    }
    app.popup = None;
    run_popup_action(app, entry.action);
}

/// メニュー項目の実行。**副作用はここだけ**に集め、「どの項目が何を意味するか」の
/// 判定は [`PopupKind::action`]（純関数）に置く
fn run_popup_action(app: &mut App, action: PopupAction) {
    match action {
        // 開けたら打ち先はそのセッションなので、行クリックと同じくフォーカスを端末へ移す
        // （失敗時は移さない ＝ 打鍵が別セッションへ流れない）
        PopupAction::OpenSession(id) => {
            if open_session(app, &id) {
                app.set_focus(Focus::Terminal);
            }
        }
        PopupAction::TogglePin(id) => edit_row(app, &id, |row| row.pinned = !row.pinned),
        PopupAction::MarkRead(id) => mark_read(app, &id),
        PopupAction::Stop(id) => menu_stop(app, &id),
        PopupAction::Close(id) => menu_close(app, &id),
        PopupAction::SetGrouping(next) => set_grouping(app, next),
        // 空プロンプトで起動する（登録は dispatch_session が行う）
        PopupAction::NewSessionIn(cwd) => dispatch_session(app, cwd, String::new()),
        PopupAction::RemoveProject(cwd) => remove_project(app, &cwd),
    }
}

/// グルーピングの選択（入口は ⊞ group 行のメニューだけ）。選択は
/// ~/.ccdesk/config.json に永続化（撮影用の供給元は保存しない ＝ 開発者の設定を
/// 踏まない）。**選ばれた値を代入する**（反転ではない ＝ 3 つ目の grouping を
/// 足してもメニューの項目がそのまま答えになる）。同じ値なら保存も走らせない
fn set_grouping(app: &mut App, next: Grouping) {
    if app.grouping == next {
        return;
    }
    app.grouping = next;
    app.source.save_window(WindowItem::Grouping(next));
}

/// メニュー: stop（セッションのプロセスを終わらせる）。
///
/// **行は残す**うえ、**行へは何も書かない。** 前景セッションは ccdesk の子なので、
/// 止める ＝ プロセスが終わること ＝ その行を動かしているものが無くなること。
/// 表示が Stopped になるのはその結果で、記録によるものではない（だから
/// `stop` でも `/clear` でも `/resume` でも同じ表示になる）
fn menu_stop(app: &mut App, id: &SessionId) {
    close_window_of(app, id);
}

/// 終了時に子プロセスを残さない。**行へは何も書かない**（`sessions.json` は
/// そのまま ＝ 次の起動で一覧に出て `claude -r` で再開できる）。
///
/// 以前はここで開いていた行を `stopped` として**記録**していた。状態を行に
/// 保存していたので、記録せずに殺すと次の起動で「動いていた頃の state」を
/// 出し続けたためだが、**ccdesk が異常終了すればその記録は残らない**（実データで
/// 保管が `blocked`・hook が `stopped` のまま固まっていた）。状態を導くように
/// なった今は、窓が 1 つも無い起動直後は必ず全部 Stopped になるので、
/// 終了時に書き残すものは無い
pub(crate) fn kill_sessions_on_exit(app: &mut App) {
    for window in &mut app.windows {
        let _ = window.child.kill();
    }
}

/// メニュー: close（一覧から行を外す）。**外れるのは ccdesk の一覧だけ**で、
/// transcript（`~/.claude/projects/**/*.jsonl`）は残す
/// （`claude -r` の記録は claude 側の持ち物で、ccdesk の一覧はその索引ではない ＝
/// 一覧から外したいだけの操作で会話の記録まで消してはいけない）。
/// 動いていれば先にプロセスを終わらせる（窓の無い行になってプロセスだけが残らない）
fn menu_close(app: &mut App, id: &SessionId) {
    close_window_of(app, id);
    let before = app.sessions.len();
    app.sessions.retain(|row| &row.session_id != id);
    if app.sessions.len() != before {
        save_sessions(app);
    }
}

/// hook 注入ファイルのパス（実体は [`crate::hooks::inject_settings`]）。
/// **書けなかったことを黙らせない**: hooks 無しで起動したセッションは状態報告が
/// 縮退する（入力待ち・完了が導出できなくなる）ので、下部バーへ 1 行出す。
/// セッション自体は hooks 無しで起動を続ける（起動を止めるほどの失敗ではない）
fn hook_settings(app: &mut App) -> Option<std::path::PathBuf> {
    let settings = crate::hooks::inject_settings();
    if settings.is_none() {
        set_notice(
            app,
            "could not write the hook settings; session states may not update (see ccdesk logs)"
                .to_string(),
        );
    }
    settings
}

/// 指定セッションのウィンドウを閉じる（＝ 子プロセスを終わらせる）。
/// 窓が開いていなければ何もしない
fn close_window_of(app: &mut App, id: &SessionId) {
    let Some(i) = app.window_index(id) else {
        return;
    };
    let _ = app.windows[i].child.kill();
    remove_window(app, i);
}

/// ウィンドウを一覧から外す（active 添字も詰める）。
/// 表示するウィンドウが無くなったら右ペインは New 画面へ
fn remove_window(app: &mut App, idx: usize) {
    if idx >= app.windows.len() {
        return;
    }
    let was_active = idx == app.active;
    app.windows.remove(idx);
    app.hovered = None;
    if app.active >= idx && app.active > 0 {
        app.active -= 1;
    }
    // 右ペインが既に New 画面なら奪わない: 表示されていたのは死んだ窓ではなく
    // New 画面で、作り直すと**入力途中のプロンプトが無警告で消える**
    if (app.windows.is_empty() || was_active) && !matches!(app.right_view, RightView::New(_)) {
        app.open_new_view();
    }
}

/// サイドバーの選択を、**行の実体がある位置**へ上下に移動する
/// （飾り ＝ [`SidebarRow::Decoration`] は飛ばす。押しても何も起きない行は止まる ＝
/// 「触れる行」の集合はホバーと同じ [`SidebarRow::selectable`] 1 つで決まる）。
///
/// **一覧とアカウント行は 1 つの輪**（マウスで押せる行はキーボードでも届く）:
/// 一覧の末尾で `↓` → アカウント行 → もう一度 `↓` で一覧の先頭へ戻り、
/// 一覧の先頭で `↑` はアカウント行へ回る。端で止めると「アカウント行から
/// 一覧の先頭へ戻る」ために一覧全体を遡ることになる。
///
/// **フッターを描いていないときは一覧の中だけで巡回する** ＝ 判断はマウスの
/// 当たり判定と同じ [`sidebar_layout`] の `footer_visible`。
/// 触れる行が 1 つも無ければ何も動かさない（無限に回らない）
pub(crate) fn move_selection(app: &mut App, dir: i32) {
    let len = app.sidebar_rows.len() as i32;
    let account = sidebar_layout(app).footer_visible;
    // アカウント行は「一覧の末尾の 1 つ先」に居る扱い。輪の長さはその 1 行を含む
    let ring = len + i32::from(account);
    if ring <= 0 {
        return;
    }
    let mut at = match app.selection {
        SidebarPos::Row(row) => row as i32,
        SidebarPos::Account => len,
    };
    // 輪を 1 周するまで探す（触れる行が無ければ元の位置のまま戻る）
    for _ in 0..ring {
        at = (at + dir).rem_euclid(ring);
        if at == len {
            app.selection = SidebarPos::Account;
            return;
        }
        if app.sidebar_rows[at as usize].selectable() {
            app.selection = SidebarPos::Row(at as usize);
            app.sidebar_follow_sel = true; // 次の draw で選択行が見えるようスクロール
            return;
        }
    }
}

/// ccdesk 自身の更新を実行する（`ccdesk update` と同じ [`crate::update::install`]）。
///
/// **走ったまま差し替えられる。** Windows は実行中の exe を上書きできないが改名は
/// できるので、update.rs の 3 段改名（`.new` へ置く → 現行を `.old` へ退避 →
/// `.new` を本体へ）がそのまま成立する。反映は次回起動なので、成功後は版行が
/// "restart" を出し続ける（`SelfUpdate::Done` はこのセッション中戻らない）。
/// 数 MB のダウンロードと SHA-256 検証が入るため別スレッドで行う
fn start_ccdesk_update(app: &mut App) {
    let Some(tag) = app.ccdesk_latest.clone() else {
        return; // 新しい版を知らないうちは何もしない（行もクリック不可）
    };
    {
        let mut state = app
            .ccdesk_update
            .lock_recover();
        // 実行中の多重起動と、済んだ更新の再実行を防ぐ
        if matches!(*state, SelfUpdate::Running | SelfUpdate::Done) {
            return;
        }
        *state = SelfUpdate::Running;
    }
    let shared = app.ccdesk_update.clone();
    std::thread::spawn(move || {
        let outcome = match crate::update::install(&tag) {
            Ok(_) => SelfUpdate::Done,
            Err(e) => SelfUpdate::Failed(format!("ccdesk update failed: {e}")),
        };
        *shared
            .lock_recover() = outcome;
    });
}

/// claude 本体の更新を実行する（公式 `claude update`）。
/// 公式仕様: 更新は次回起動時から有効で、実行中セッションは現行版のまま動き続ける。
/// 完了後はフッターを再取得し、最新化されれば版行は最新表示へ戻る
fn start_claude_update(app: &mut App) {
    if app
        .claude_updating
        .swap(true, std::sync::atomic::Ordering::Relaxed)
    {
        return; // 実行中の多重起動を防ぐ
    }
    let updating = app.claude_updating.clone();
    let refresh = app.footer_refresh.clone();
    let dirty = app.footer_dirty.clone();
    std::thread::spawn(move || {
        use std::process::Stdio;
        let _ = std::process::Command::new("claude")
            .arg("update")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output();
        updating.store(false, std::sync::atomic::Ordering::Relaxed);
        refresh.store(true, std::sync::atomic::Ordering::Relaxed);
        dirty.store(true, std::sync::atomic::Ordering::Relaxed);
    });
}

/// 通知の寿命。**表示だけでなく当たり判定にも効く**（notice 表示中は下部バーの
/// 使用率が出ない ＝ 使用率クリックの有無が notice の生死で決まる）ので、
/// 失効の判断は描画ではなく run ループ（[`expire_notice`]) が持つ
const NOTICE_TTL: Duration = Duration::from_secs(5);

/// 期限を過ぎた通知を落とす（消えたら true ＝ 描き直す）。
/// **描画の中で落とさない**: 期限切れでも描画が走らない間は `notice` が残り、
/// 「消えたはずの通知が使用率クリックを殺し続ける」形になる
fn expire_notice(app: &mut App) -> bool {
    if app
        .notice
        .as_ref()
        .is_some_and(|(_, at)| at.elapsed() >= NOTICE_TTL)
    {
        app.notice = None;
        return true;
    }
    false
}

/// 下部バーに数秒表示する通知（起動失敗など、無反応に見せないため）。
/// あわせて ~/.ccdesk/error.log にも残す
fn set_notice(app: &mut App, msg: String) {
    log_error(&msg);
    set_hint(app, msg);
}

/// 下部バーへ出すだけの案内（error.log には残さない）。
/// **異常ではない案内**用で、打鍵のたびに呼ばれ得るものはこちらを使う
/// （error.log に残すと 1 秒の取りこぼしで数十行増え、本物の失敗が埋まる）。
/// 表示の作法（数秒で消える・キーヒントを一時的に隠す）は通知と同じなので、
/// 置き場所は `notice` 1 つのまま
fn set_hint(app: &mut App, msg: String) {
    app.notice = Some((msg, std::time::Instant::now()));
}

/// 入力を捨てる門番の期限。**これが有界であることが要点**で、期限を持たないと
/// 起こした子が端末を掴まないまま（起動直後の認証プロンプト・AV スキャン・
/// 長い `-r` の読み込み）門番が降りず、既存の全セッションへのタイプが死ぬ
const INPUT_GATE_LIMIT: Duration = Duration::from_secs(10);

/// 起動処理中（[`App::input_gate`] が生きている間）に
/// ターミナルペインへ来た入力を捨てる。捨てたら `true`（呼び手は何もしない）。
///
/// **子が端末を掴む前の打鍵を守る門番。** 前景セッションは PTY を開いた時点で
/// 宛先が決まるが、claude が raw mode に入るまでの打鍵は行き場が定まらない
/// （読み捨てられる・エコーが混ざる）。特に `-r` の再開は transcript の読み直しに
/// 時間がかかりうるので、掴むまでは捨てて「届いていない」と伝える方が確実。
///
/// **フォーカスの移動を遅らせる形は採らない**: それは直したはずの問題
/// （起動したのにキーがサイドバーへ行き ↑↓ で選択が動く）を戻すことになる。
/// フォーカスは即座に端末へ移し、**子が掴むまでの入力だけを捨てる**。
///
/// 降ろす契機は「子が最初の出力を出した」（run ループ）と期限切れ
/// （[`expire_input_gate`]）の 2 つ。**判断は [`lift_input_gate`] 1 箇所**。
///
/// **黙って捨てない**: 下部バーの "starting session…" は通知が出ている間は隠れるため、
/// 捨てたこと自体をここで伝える（[`set_hint`] ＝ 異常ではないので error.log には残さない）。
///
/// マウスは門番の対象外: 届くのはクリックとホイールでプロンプトへ文字を送る経路ではなく、
/// 移動イベントごとに案内を出すとノイズになる
fn drop_input_while_starting(app: &mut App) -> bool {
    if app.input_gate.is_none() {
        return false;
    }
    set_hint(app, "starting session — keys are not delivered yet".to_string());
    true
}

/// 門番を降ろす。**`input_gate` を降ろすのはここだけ**で、
/// 「門番を降ろすときは必ず打ち先を確かめる」という判断をこの 1 箇所に閉じる。
///
/// **なぜ 1 箇所に集めるか。** 降ろす契機は 3 つある（起こした子が端末を掴んだ /
/// 起動が失敗した / 子が応答しない）。降ろすだけでは入力は `right_view` が指した
/// ままの**直前まで見ていたセッション**へ流れる ＝ 門番を置いた理由そのものが
/// 復活するので、降ろす側とフォーカスを戻す側が別だと**片方だけ直した状態**
/// （実際にそうなっていた: ハングの経路だけ戻していた）が生まれる。
/// 契機が増えてもここを通る限り穴が開かない。
///
/// `destination` は「宛先にしたつもりの [`SessionId`]」。**それが本当に打ち先に
/// なっているかは呼び手の報告ではなく右ペインの実際の表示で確かめる**
/// （[`App::showing`]）＝ 呼び手が「成功した」と言い間違える余地を持たせない。
///
/// **門番が立っていなければ何もしない。** セッションを起こさない供給元（撮影用）は
/// 門番を立てずにここへ合流するので、そこでフォーカスを動かすと
/// 「起動したのにキーがサイドバーへ行く」という直したはずの問題が戻る
fn lift_input_gate(app: &mut App, destination: Option<&SessionId>) {
    if app.input_gate.take().is_none() {
        return;
    }
    if !destination.is_some_and(|id| app.showing(id)) {
        // 宛先が居ない。フォーカスを戻せばキーはサイドバー操作になり、
        // ユーザーは打ち先を選び直せる（Alt+→ / 行を開く）
        app.set_focus(Focus::Sidebar);
    }
}

/// 応答しない起動から入力を取り戻す（run ループが毎周見る）。降ろしたら `true`。
///
/// 打ち先の扱いは [`lift_input_gate`]（宛先は無い ＝ サイドバーへ戻る）。
///
/// **窓は閉じない。** 子は生きているかもしれない（読み込みが長い `-r` など）ので、
/// 生死の判断は `child.try_wait()` に任せる ＝ ここが決めるのは入力の行き先だけ
fn expire_input_gate(app: &mut App) -> bool {
    if !app
        .input_gate
        .is_some_and(|since| since.elapsed() >= INPUT_GATE_LIMIT)
    {
        return false;
    }
    lift_input_gate(app, None);
    // ハングしていることを伝える（下部バーと error.log の両方。ここは異常）
    set_notice(
        app,
        "session start is not responding — input moved back to the sidebar".to_string(),
    );
    true
}

/// 止まっている行の起こし直し方と、**その起動先の cwd**。
///
/// 判断材料は 1 つ ＝ **`claude -r` が会話を見つけられる cwd があるか**
/// （[`crate::title::Titles::resume_cwd`]）。副作用を持たないので単体で検査できる。
///
/// - ある → その cwd で `claude -r <uuid>` を打つ
/// - ない → **同じ UUID で新規として起こす**（行の cwd で）。会話が無い行
///   （起こしただけで 1 ターンも終わっていない）と、会話はあるがその作業ツリーが
///   消えている行の両方がここへ来る。どちらも `claude -r` は
///   `No conversation found` になる（実測）
///
/// **cwd を返すのが要点**: セッションは走行中に git worktree へ移れて、移った先の
/// 会話は行の cwd から `claude -r` を打っても見つからない
/// （`/resume` のピッカーに出ないのと同じ範囲の話）
fn relaunch<'a>(
    titles: &crate::title::Titles,
    row: &'a SessionRow,
) -> (Launch<'static>, std::borrow::Cow<'a, str>) {
    match titles.resume_cwd(row) {
        Some(cwd) => (Launch::Resume, std::borrow::Cow::Owned(cwd)),
        None => (
            Launch::New { prompt: "" },
            std::borrow::Cow::Borrowed(row.cwd.as_str()),
        ),
    }
}

/// 一覧の行を開く: ウィンドウが開いていれば切替、無ければ起こし直す
/// （起こし方と起動先は [`relaunch`] が決める）。
///
/// 新規側でプロンプトを渡さないのは、起動時のプロンプトは最初の 1 回で使い切って
/// いるため（二度目に送ると同じ指示が 2 回走る）。`--session-id` を渡すので
/// **行の identity は変わらず**、履歴が生まれたときにこの行の transcript になる。
///
/// 失敗（cwd 消失等）は握りつぶさず下部バーへ通知する。
/// **戻り値は「ペインがこのセッションを出したか」。** 呼び手はこれを見てから
/// フォーカスを端末へ移す（失敗したのに移すと、打鍵が直前まで表示していた
/// 別セッションへ流れる）
pub(crate) fn open_session(app: &mut App, id: &SessionId) -> bool {
    if let Some(i) = app.window_index(id) {
        if app.windows[i].alive() {
            // **ペインを開いた時点が既読の契機**（切替も再開も同じ）
            mark_read(app, id);
            app.show_session(i);
            return true;
        }
        // 直前に死んだ窓（生死スキャンがまだ拾っていない）は開かない:
        // 固まった死画面が出て、次のスキャンで意図せず New 画面へ飛ばされる。
        // 外して下の起こし直しへ落とす（メニューの stop 判定 `session_open` と
        // 同じく、生きた窓だけを「開いている」と数える）
        let _ = app.windows[i].child.kill();
        remove_window(app, i);
    }
    // **別のインスタンス（または ccdesk の外）で動いているセッションを
    // `claude -r` で二重に起こさない**: 同じ会話を 2 プロセスが同時に更新する。
    // 判定材料は `agents --json`（生きている前景セッションだけが載る）。
    // 観測は 2 秒周期なので、止めた直後の自分のセッションが最大 2 秒ほど
    // 残って見えることがある（その間の open は 1 度空振りするだけで、壊れない）
    if app
        .agents
        .iter()
        .any(|a| a.is_interactive() && a.session_id == id.as_str())
    {
        set_notice(
            app,
            format!("session {id} is already running in another window"),
        );
        return false;
    }
    let Some(row) = app.row(id) else {
        return false; // 再読み込みで消えた行（クリックと削除の競合）は何もしない
    };
    let (launch, cwd) = relaunch(&app.titles, row);
    let cwd = cwd.into_owned();
    let settings = hook_settings(app);
    let (rows, cols) = app.pane_size();
    match Session::spawn(id, &cwd, rows, cols, launch, settings.as_deref()) {
        Ok(window) => {
            // 既読は**起こせてから**付ける（起こせなかった行の未読 ● を、
            // 内容を見ていないのに消さない）
            mark_read(app, id);
            app.windows.push(window);
            app.show_session(app.windows.len() - 1);
            // 再開は transcript の読み直しに時間がかかりうる。子が端末を掴むまでの
            // 打鍵は捨てる（[`drop_input_while_starting`]）
            app.input_gate = Some(std::time::Instant::now());
            true
        }
        Err(e) => {
            set_notice(app, format!("failed to resume session {id}: {e}"));
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::source::{persist_projects, WindowState};
    use crate::ui::MIN_SIDEBAR;

    const TERM: (u16, u16) = (120, 40);

    /// ポップアップ・ヒットテスト判定に必要な最小の App。
    /// 中立値は `App` の [`Default`]（構造体定義の直後）が持つので、ここは
    /// このヘルパが決める 2 つだけを上書きする（フィールドを列挙し直さない）
    fn test_app(sidebar_width: u16, term_size: (u16, u16)) -> App {
        App {
            sidebar_width,
            term_size,
            ..Default::default()
        }
    }

    fn open(app: &mut App, kind: PopupKind, anchor_y: u16) {
        app.popup = Some(Popup {
            kind,
            anchor_y,
            selected: 0,
        });
    }

    fn labels(kind: &PopupKind, grouping: Grouping) -> Vec<String> {
        kind.entries(grouping)
            .into_iter()
            .map(|entry| entry.label)
            .collect()
    }

    /// (表示名, 実行可能か) の一覧（項目表の見た目を比べるテスト用）
    fn entry_pairs(kind: &PopupKind, grouping: Grouping) -> Vec<(String, bool)> {
        kind.entries(grouping)
            .into_iter()
            .map(|entry| (entry.label, entry.enabled))
            .collect()
    }

    fn click(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// ボタンを押さないマウス移動（ホバーだけを動かす）
    fn moved(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Moved,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// 使用率が出ている App と、取り直しが呼ばれた回数の記録
    /// （fixture は型の持ち主 = usage 側の 1 つを使う）
    fn usage_app() -> (App, Arc<std::sync::atomic::AtomicUsize>) {
        let refreshes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let usage = crate::usage::sample_ready(Vec::new());
        let mut app = test_app(34, TERM);
        app.source = Arc::new(TestSource::for_usage(usage.clone(), Arc::clone(&refreshes)));
        app.usage = usage;
        (app, refreshes)
    }

    /// **右下の使用率を押すとその場で取り直す**（周期を待たない）。
    /// 当たり判定は描画と同じ導出（`ui::usage_hit`）なので、見えている場所と
    /// 押せる場所がずれない
    #[test]
    fn clicking_the_usage_gauge_refetches_it() {
        let (mut app, refreshes) = usage_app();
        let hit = crate::ui::usage_hit(&app).expect("the gauge is on screen");
        assert_eq!(hit.row, TERM.1 - 1, "the gauge is on the bottom bar");

        // 領域の左端・右端どちらを押しても当たる
        for column in [hit.columns.start, hit.columns.end - 1] {
            let before = refreshes.load(std::sync::atomic::Ordering::Relaxed);
            handle_mouse(&mut app, &click(column, hit.row)).unwrap();
            assert_eq!(
                refreshes.load(std::sync::atomic::Ordering::Relaxed),
                before + 1,
                "clicking column {column} did not refetch"
            );
        }
    }

    /// **使用率の外を押しても取り直さない。** 右ペイン側の列に描かれるので、
    /// 1 桁ずれただけでペインのクリックへ落ちることを固定する
    #[test]
    fn clicking_outside_the_usage_gauge_does_not_refetch() {
        let (mut app, refreshes) = usage_app();
        let hit = crate::ui::usage_hit(&app).expect("the gauge is on screen");
        // 領域の 1 桁左、そして同じ列の 1 行上
        for (column, row) in [(hit.columns.start - 1, hit.row), (hit.columns.start, hit.row - 1)] {
            handle_mouse(&mut app, &click(column, row)).unwrap();
        }
        assert_eq!(refreshes.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    /// **使用率の上では帯が点き、外れたら消える。** 判定はクリックと同じ導出
    /// （`ui::usage_hit`）なので、光る場所と押せる場所がずれない。
    /// 点いた・消えた瞬間は描き直し、乗ったまま動いても描き直さない（FPS 対策）
    #[test]
    fn hovering_the_usage_gauge_lights_it_up_and_redraws_once() {
        let (mut app, _) = usage_app();
        let hit = crate::ui::usage_hit(&app).expect("the gauge is on screen");

        // 乗った ＝ 点く + 描き直す
        let prev = (app.hovered, app.usage_hovered);
        handle_mouse(&mut app, &moved(hit.columns.start, hit.row)).unwrap();
        assert!(app.usage_hovered, "the gauge is not marked as hovered");
        assert!(
            mouse_needs_redraw(MouseEventKind::Moved, prev, (app.hovered, app.usage_hovered)),
            "entering the gauge must ask for a redraw"
        );

        // 乗ったまま動いた ＝ 描き直さない
        let prev = (app.hovered, app.usage_hovered);
        handle_mouse(&mut app, &moved(hit.columns.end - 1, hit.row)).unwrap();
        assert!(
            !mouse_needs_redraw(MouseEventKind::Moved, prev, (app.hovered, app.usage_hovered)),
            "moving inside the gauge must not ask for a redraw"
        );

        // 外れた ＝ 消える（1 桁左はゲージの外）
        handle_mouse(&mut app, &moved(hit.columns.start - 1, hit.row)).unwrap();
        assert!(!app.usage_hovered, "the mark stays after the mouse left");
    }

    /// **押した瞬間からスピナーが回る。** 旗はクリック側が立てる（取得スレッド任せ
    /// だと最初の描き直しまで反応が出ない）。ただし立てるのは取り直しを実際に
    /// 頼めたときだけ ＝ 取得しない供給元（撮影用）では、降ろす者がいない旗を立てない
    #[test]
    fn a_click_starts_the_spinner_only_when_a_fetch_was_requested() {
        // 取り直しを頼めた ＝ 回し始める
        let (mut app, _) = usage_app();
        let hit = crate::ui::usage_hit(&app).expect("the gauge is on screen");
        handle_mouse(&mut app, &click(hit.columns.start, hit.row)).unwrap();
        assert!(
            app.usage_fetching.load(std::sync::atomic::Ordering::Relaxed),
            "the spinner did not start on click"
        );

        // 取得しない供給元（既定の DemoSource は取り直さない）＝ 立てない
        let mut app = test_app(34, TERM);
        app.usage = crate::usage::sample_ready(Vec::new());
        let hit = crate::ui::usage_hit(&app).expect("the gauge is on screen");
        handle_mouse(&mut app, &click(hit.columns.start, hit.row)).unwrap();
        assert!(
            !app.usage_fetching.load(std::sync::atomic::Ordering::Relaxed),
            "a spinner started that no one will stop"
        );
    }

    /// **出ていないものは押せない。** 使用率を切っている（`Unknown`）ときは
    /// 当たり判定そのものが無い ＝ 見えない場所を押して claude が起きない
    #[test]
    fn a_hidden_usage_gauge_has_no_hit_area() {
        let app = test_app(34, TERM);
        assert!(matches!(app.usage, Usage::Unknown), "the fixture's premise broke");
        assert!(crate::ui::usage_hit(&app).is_none());
    }

    /// notice を出している間は下部バーが notice に置き換わるので、当たり判定も無い
    #[test]
    fn a_notice_takes_the_bottom_bar_and_the_hit_area_with_it() {
        let (mut app, _) = usage_app();
        assert!(crate::ui::usage_hit(&app).is_some(), "the fixture's premise broke");
        app.notice = Some(("something happened".to_string(), std::time::Instant::now()));
        assert!(crate::ui::usage_hit(&app).is_none());
    }

    /// プロジェクトメニューが指すフォルダ
    const PROJECT_CWD: &str = "C:\\dev\\shop-app";

    /// **項目が最も長いメニュー**（`remove project` ＝ 14 桁）。矩形まわりの検査は
    /// 「サイドバーより広いメニュー」を要るので、幅の下限を越えるこれを使う
    fn project_menu() -> PopupKind {
        PopupKind::Project {
            cwd: PROJECT_CWD.to_string(),
            has_sessions: false,
        }
    }

    /// 窓が開いているセッションのメニュー（ピン留めされていない行）。
    /// 写しを 1 つだけ変えたいテストは `PopupKind::Session { .. }` を直接組む
    fn session(id: &str, open: bool) -> PopupKind {
        PopupKind::Session {
            id: SessionId::new(id),
            pinned: false,
            open,
        }
    }

    /// **メニューは 5 項目で、先頭が `open`。** 落ちるのは `stop` だけで、それは
    /// 窓が開いていない行 ＝ 止めるプロセスが無いから（他の 5 つは停止中の行にも効く ＝
    /// `open` は止まっている行を `claude -r` で再開する）。
    ///
    /// **語は実態そのもの**: `stop` はプロセスを止める（行は残る）、`close` は
    /// 一覧から外す（会話ログは残る）。だから「削除」の語はどこにも出さない。
    /// **アーカイブも項目に無い**: `close` が外すのは行だけなので、
    /// アーカイブとの差は「戻す導線があるか」だけになる
    #[test]
    fn session_menu_disables_stop_only_when_no_window_is_open() {
        assert_eq!(
            entry_pairs(&session("s1", true), Grouping::State),
            [
                ("open".to_string(), true),
                ("pin".to_string(), true),
                ("mark as read".to_string(), true),
                ("stop".to_string(), true),
                ("close".to_string(), true),
            ]
        );
        assert_eq!(
            session("s1", false)
                .entries(Grouping::State)
                .into_iter()
                .map(|entry| entry.enabled)
                .collect::<Vec<_>>(),
            [true, true, true, false, true],
            "stop must be the only entry disabled when there is no window"
        );
        // アーカイブと「削除」の語はどちらの状態でも出さない
        // （節ごと廃止した / 会話ログは消えないので嘘になる）
        for open in [true, false] {
            let labels = labels(&session("s1", open), Grouping::State);
            for word in ["archive", "delete"] {
                assert!(
                    !labels.iter().any(|label| label.contains(word)),
                    "{word} came back into the menu: {labels:?}"
                );
            }
        }
    }

    /// 入切するピン留めは**今の状態の逆**を出す（押したら何が起きるかがラベルになる）
    #[test]
    fn session_menu_labels_the_toggles_with_what_they_will_do() {
        let marked = PopupKind::Session {
            id: SessionId::new("s1"),
            pinned: true,
            open: false,
        };
        let labels = labels(&marked, Grouping::State);
        assert!(labels.contains(&"unpin".to_string()), "a pinned row must show unpin: {labels:?}");
        assert!(!labels.contains(&"pin".to_string()), "both states are listed: {labels:?}");
    }

    /// 5 項目それぞれの動作は行 index から引く（ラベル文字列で分岐しない）
    #[test]
    fn session_menu_maps_each_row_index_to_its_action() {
        let kind = session("abc123", true);
        let g = Grouping::State;
        let id = || SessionId::new("abc123");
        assert_eq!(kind.action(g, 0), Some(PopupAction::OpenSession(id())));
        assert_eq!(kind.action(g, 1), Some(PopupAction::TogglePin(id())));
        assert_eq!(kind.action(g, 2), Some(PopupAction::MarkRead(id())));
        assert_eq!(kind.action(g, 3), Some(PopupAction::Stop(id())));
        assert_eq!(kind.action(g, 4), Some(PopupAction::Close(id())));
        assert_eq!(kind.action(g, 5), None, "an index past the last entry must do nothing");
    }

    /// grouping メニューは現在の選択に ● を付け、各行はその grouping を指す
    #[test]
    fn group_menu_marks_the_current_grouping_and_maps_each_row_to_it() {
        assert_eq!(
            labels(&PopupKind::Group, Grouping::State),
            ["● state", "  directory"]
        );
        assert_eq!(
            labels(&PopupKind::Group, Grouping::Directory),
            ["  state", "● directory"]
        );
        assert_eq!(
            PopupKind::Group.action(Grouping::State, 0),
            Some(PopupAction::SetGrouping(Grouping::State))
        );
        assert_eq!(
            PopupKind::Group.action(Grouping::State, 1),
            Some(PopupAction::SetGrouping(Grouping::Directory))
        );
        assert_eq!(PopupKind::Group.action(Grouping::State, 2), None);
    }

    /// セッション行の**行末** `=` クリックでメニューが開く（二次操作の入口）。
    /// 開いた時点の行の状態（ピン留め・窓の有無）が写る
    #[test]
    fn clicking_the_hamburger_opens_the_session_menu() {
        let mut app = test_app(34, TERM);
        app.sessions = vec![SessionRow {
            pinned: true,
            ..session_row("abc123", "C:\\dev\\api", 1)
        }];
        app.sidebar_rows = vec![SidebarRow::Action(RowAction::Open(SessionId::new("abc123")))];
        app.sidebar_header_rows = 1;
        let mark_x = *menu_zone(sidebar_cols(&app)).end();
        handle_mouse(&mut app, &click(mark_x, 1)).unwrap();
        let popup = app.popup.as_ref().expect("menu must be open");
        assert_eq!(
            popup.kind,
            PopupKind::Session {
                id: SessionId::new("abc123"),
                pinned: true,
                // 生きた窓が無い = 止めるものが無い
                open: false,
            }
        );
        assert_eq!(
            labels(&popup.kind, app.grouping),
            ["open", "unpin", "mark as read", "stop", "close"]
        );
        assert_eq!(popup.anchor_y, 1, "must open below the clicked row");
    }

    /// ⊞ group 行クリック → メニュー → 別の行を選ぶと grouping が切り替わる。
    /// クリック判定は描画と同じ popup_rect の座標で行う
    #[test]
    fn clicking_the_group_row_and_picking_a_row_switches_grouping() {
        let mut app = test_app(34, TERM);
        app.sidebar_rows = vec![SidebarRow::Action(RowAction::ToggleGroup)];
        app.sidebar_header_rows = 1;
        handle_mouse(&mut app, &click(5, 1)).unwrap();
        assert_eq!(
            app.popup.as_ref().map(|p| &p.kind),
            Some(&PopupKind::Group),
            "grouping menu must be open"
        );
        let rect = popup_rect(&app, app.popup.as_ref().unwrap());
        handle_mouse(&mut app, &click(rect.x + 1, rect.y + 2)).unwrap(); // 2 行目 = directory
        assert_eq!(app.grouping, Grouping::Directory);
        assert!(app.popup.is_none(), "must close after running the action");
    }

    /// 選択中の grouping をもう一度選んでも切り替わらない（トグルにならない）
    #[test]
    fn picking_the_current_grouping_leaves_it_unchanged() {
        let mut app = test_app(34, TERM);
        open(&mut app, PopupKind::Group, 3);
        activate_popup(&mut app, 0); // ● state
        assert_eq!(app.grouping, Grouping::State);
        assert!(app.popup.is_none());
    }

    /// プロジェクトメニューは対象フォルダを各動作へ持ち込む
    #[test]
    fn project_menu_carries_its_folder_into_each_action() {
        let kind = PopupKind::Project {
            cwd: "C:\\dev\\shop-app".to_string(),
            has_sessions: false,
        };
        assert_eq!(
            labels(&kind, Grouping::State),
            ["new session", "remove project"]
        );
        assert_eq!(
            kind.action(Grouping::State, 0),
            Some(PopupAction::NewSessionIn("C:\\dev\\shop-app".to_string()))
        );
        assert_eq!(
            kind.action(Grouping::State, 1),
            Some(PopupAction::RemoveProject("C:\\dev\\shop-app".to_string()))
        );
        assert_eq!(kind.action(Grouping::State, 2), None);
    }

    /// Esc は開いているメニューを閉じる（階層を持たないので戻り先も無い）。
    /// 外クリックも同じ
    #[test]
    fn esc_and_outside_click_close_the_popup() {
        let mut app = test_app(34, TERM);
        let menu = || PopupKind::Group;
        open(&mut app, menu(), 5);
        handle_popup_key(&mut app, KeyCode::Esc);
        assert!(app.popup.is_none(), "esc must close the menu");

        open(&mut app, menu(), 5);
        let rect = popup_rect(&app, app.popup.as_ref().unwrap());
        handle_mouse(&mut app, &click(rect.right() + 2, rect.bottom() + 2)).unwrap();
        assert!(app.popup.is_none(), "an outside click must close the menu");
    }

    /// ↑↓ は項目数の範囲で止まる（端で溢れない）
    #[test]
    fn arrow_keys_clamp_the_selection_to_the_entry_range() {
        let mut app = test_app(34, TERM);
        // 2 項目のメニュー（state / directory）
        open(&mut app, PopupKind::Group, 3);
        for _ in 0..5 {
            handle_popup_key(&mut app, KeyCode::Down);
        }
        assert_eq!(app.popup.as_ref().unwrap().selected, 1);
        for _ in 0..5 {
            handle_popup_key(&mut app, KeyCode::Up);
        }
        assert_eq!(app.popup.as_ref().unwrap().selected, 0);
    }

    /// 実行できない項目（窓の無い行の stop）は Enter でも動かず、メニューも閉じない
    #[test]
    fn disabled_item_is_not_executed() {
        let mut app = test_app(34, TERM);
        let kind = session("s1", false);
        // 位置は並びから引く（項目を足したときに黙って別の行を突かない）
        let stop = kind
            .entries(app.grouping)
            .iter()
            .position(|entry| entry.label == "stop")
            .expect("the stop entry is gone");
        open(&mut app, kind, 3);
        for _ in 0..stop {
            handle_popup_key(&mut app, KeyCode::Down); // stop を選ぶ
        }
        assert_eq!(app.popup.as_ref().unwrap().selected, stop);
        handle_popup_key(&mut app, KeyCode::Enter);
        assert!(
            app.popup.is_some(),
            "menu must stay open for a disabled entry"
        );
    }

    /// 枠線クリックは項目を発火しない（上枠が先頭項目に化けない）
    #[test]
    fn clicking_the_menu_border_does_not_run_an_item() {
        let mut app = test_app(34, TERM);
        open(&mut app, PopupKind::Group, 3);
        let rect = popup_rect(&app, app.popup.as_ref().unwrap());
        for (col, row) in [
            (rect.x, rect.y + 1),
            (rect.x + 1, rect.y),
            (rect.x + 1, rect.bottom() - 1),
            (rect.right() - 1, rect.y + 1),
        ] {
            handle_mouse(&mut app, &click(col, row)).unwrap();
            assert!(
                app.popup.is_some(),
                "a border click at ({col},{row}) must not close the menu"
            );
            assert_eq!(
                app.grouping,
                Grouping::State,
                "a border click at ({col},{row}) must not run an entry"
            );
        }
    }

    /// 内容から幅を決めるので、狭いサイドバーでは右ペインに被る。
    /// 被ることは許容するが、端末の外へは出さない
    #[test]
    fn wide_menu_overlaps_the_right_pane_but_keeps_its_left_edge() {
        let mut app = test_app(12, TERM);
        open(&mut app, project_menu(), 3);
        let rect = popup_rect(&app, app.popup.as_ref().unwrap());
        assert_eq!(rect.x, 1, "left edge must stay at sidebar x=1 while it fits");
        assert!(
            rect.right() > app.sidebar_width,
            "must overflow the sidebar width"
        );
        assert!(rect.right() <= TERM.0);
    }

    /// どのサイドバー幅・端末サイズ・anchor でも矩形は端末内に収まり、潰れない。
    /// 幅は内容で決まるため「サイドバーより広い」「端末より広い」が起こり得る
    #[test]
    fn popup_rect_stays_inside_the_terminal_for_any_sidebar_width() {
        let kinds = || vec![session("s1", false), PopupKind::Group, project_menu()];
        for (term_w, term_h) in [(120u16, 40u16), (80, 24), (52, 8), (14, 5), (1, 1)] {
            for sidebar_width in [12u16, 26, 34, term_w] {
                for anchor_y in [0u16, 1, 5, u16::MAX] {
                    for kind in kinds() {
                        let mut app = test_app(sidebar_width, (term_w, term_h));
                        open(&mut app, kind, anchor_y);
                        let rect = popup_rect(&app, app.popup.as_ref().unwrap());
                        assert!(
                            rect.right() <= term_w.max(1) && rect.bottom() <= term_h.max(1),
                            "rect {rect:?} must stay inside the terminal for terminal {term_w}x{term_h} / sidebar {sidebar_width} / anchor {anchor_y}"
                        );
                        assert!(
                            rect.width >= 1 && rect.height >= 1,
                            "rect {rect:?} must not collapse to zero size"
                        );
                    }
                }
            }
        }
    }

    /// **メニューは押した記号の位置から出る。**
    ///
    /// 実機では「行末の `=` を押すと画面の反対側からメニューが出る」形で出た:
    /// 記号を右端へ移したのに矩形の左端が x=1 固定のままだった。
    /// 当たり判定（[`menu_zone`]）と同じ規則から導くので、幅を変えても付いてくる
    #[test]
    fn a_menu_opens_from_the_mark_that_was_clicked() {
        for sidebar_width in [20u16, 26, 34, 60] {
            let mut app = test_app(sidebar_width, TERM);
            open(&mut app, session("s1", false), 3);
            let rect = popup_rect(&app, app.popup.as_ref().unwrap());
            let mark_right = *menu_zone(sidebar_cols(&app)).end();
            assert_eq!(
                rect.right() - 1,
                mark_right,
                "the menu is not aligned with the mark for sidebar {sidebar_width}: {rect:?}"
            );
            assert!(rect.x >= 1, "the menu ran over the sidebar border: {rect:?}");
            assert_eq!(rect.y, 4, "the menu must open below the row that was clicked");
        }
    }

    /// **サイドバー幅を書き換える経路はドラッグだけ。**
    ///
    /// 実機では「セッションを止めるとサイドバーが数桁狭くなり、戻らない」形で出た:
    /// 端末幅で丸める処理が**保存値そのものを上書き**していたので、端末サイズ変化
    /// イベントが 1 度届くだけで幅が縮み、端末が元へ戻っても復元しなかった
    /// （Windows では PTY の破棄がそのイベントを連れてくる）。
    /// 保存値はユーザーが選んだ幅のまま、**画面に出す桁数だけを端末幅から導く**
    #[test]
    fn only_a_drag_changes_the_sidebar_width() {
        let mut app = test_app(34, TERM);
        // 端末が狭くなると出す桁は縮むが、選んだ幅は残る（本番と同じ反映を通す）
        resize_terminal(&mut app, 60, 20);
        assert_eq!(sidebar_cols(&app), 20, "the drawn width ignored the narrow terminal");
        assert_eq!(app.sidebar_width, 34, "resizing overwrote the chosen width");
        // 広がれば選んだ幅へ戻る（縮んだままにならない）
        resize_terminal(&mut app, TERM.0, TERM.1);
        assert_eq!(sidebar_cols(&app), 34, "the width did not come back");
        assert_eq!(app.sidebar_width, 34);

        // 丸めの規則そのもの: 下限 MIN_SIDEBAR、右ペインに MIN_PANE を残す
        assert_eq!(fit_sidebar(34, 120), 34);
        assert_eq!(fit_sidebar(34, 60), 20);
        assert_eq!(fit_sidebar(34, 40), MIN_SIDEBAR, "the floor did not hold");
        assert_eq!(fit_sidebar(2, 120), MIN_SIDEBAR);
        assert_eq!(fit_sidebar(34, 0), MIN_SIDEBAR);

        // ドラッグは保存値を書き換える（唯一の経路）
        handle_mouse(&mut app, &click(34, 5)).unwrap();
        let mut drag = click(50, 5);
        drag.kind = MouseEventKind::Drag(MouseButton::Left);
        handle_mouse(&mut app, &drag).unwrap();
        assert_eq!(app.sidebar_width, 51, "dragging did not move the chosen width");
    }

    /// サイドバー幅より広いメニューは幅変更のつかみ代（境界線の列）に被る。
    /// その列のクリックは項目の実行で、幅変更ドラッグにはならない
    #[test]
    fn clicking_a_menu_row_over_the_resize_border_does_not_start_a_drag() {
        let mut app = test_app(12, TERM);
        app.projects = vec![PROJECT_CWD.to_string()];
        open(&mut app, project_menu(), 3);
        let rect = popup_rect(&app, app.popup.as_ref().unwrap());
        assert!(
            rect.right() > app.sidebar_width,
            "must overlap the resize border by the test's premise"
        );
        let border_col = app.sidebar_width;
        // 2 行目 = `remove project`（実行されると登録から外れる ＝ 結果が見える）
        handle_mouse(&mut app, &click(border_col, rect.y + 2)).unwrap();
        assert!(!app.dragging, "must not start a resize drag");
        assert_eq!(app.sidebar_width, 12, "sidebar width must not change");
        assert!(
            app.projects.is_empty(),
            "the overlapped column's entry must run"
        );
    }

    // ── 行の種類・下部バーの案内（描画を通した検査） ──────────────────────

    /// **サイドバーに出る行の種類が 1 フレームで全部そろう `App`。**
    /// 版行（更新あり = ccdesk / 更新なし = claude）・区切り線・`+ new session`・
    /// `⊞ group`・集計行・プロジェクト見出し・セッション行が積まれ、
    /// フッターのアカウント行も描かれる。
    ///
    /// 行の一覧は**描画が積んだ結果**を読む（種類を書き写した表を持たない）
    fn app_with_every_row_kind() -> App {
        let mut app = test_app(34, (120, 40));
        app.grouping = Grouping::Directory; // 見出し行を出す
        app.projects = vec!["C:\\dev\\api".to_string()];
        app.sessions = vec![session_row("s", "C:\\dev\\api", 1)];
        // ccdesk の版行 = 更新あり、claude の版行 = 更新なし（＝ 押しても何も起きない行）
        app.ccdesk_latest = Some("v9.9.9".to_string());
        app
    }

    /// 1 フレーム描いて最下行（下部バーの案内）を読む。**本番と同じ [`draw`]** を通す
    /// ので、サイドバーの行の積み方と案内の対応がそのまま検査対象になる
    fn drawn_bottom_bar(app: &mut App) -> String {
        let (w, h) = app.term_size;
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h))
            .expect("test terminal");
        terminal
            .draw(|frame| {
                draw(frame, app);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        (0..w).map(|x| buffer[(x, h - 1)].symbol()).collect()
    }

    /// 触れる位置（一覧の実体のある行 + フッターのアカウント行）を、
    /// **描画が積んだ行から**列挙する
    fn touchable_positions(app: &mut App) -> Vec<SidebarPos> {
        drawn_bottom_bar(app); // 1 フレーム描いて行を積む
        (0..app.sidebar_rows.len())
            .filter(|row| app.sidebar_rows[*row].selectable())
            .map(SidebarPos::Row)
            .chain(std::iter::once(SidebarPos::Account))
            .collect()
    }

    /// **案内に出る `Enter` は、その行で `Enter` が本当にすることの名前。**
    /// 行の種類ごとの対応表をテストに書き写さず、[`Enter::label`]（動作の名前の正本）と
    /// 描画結果を突き合わせる。行の種類が増えても [`selected_enter`] の網羅 match が
    /// コンパイル時に対応を強制するので、この検査はそのまま新しい種類へ効く
    #[test]
    fn the_bottom_bar_names_what_enter_does_on_the_selected_row() {
        let mut app = app_with_every_row_kind();
        let positions = touchable_positions(&mut app);
        assert!(positions.len() > 5, "the fixture stopped covering the row kinds: {positions:?}");
        let mut verbs: Vec<&'static str> = Vec::new();
        for pos in positions {
            let mut app = app_with_every_row_kind();
            drawn_bottom_bar(&mut app); // 行を積む
            app.selection = pos;
            let bar = drawn_bottom_bar(&mut app);
            assert!(bar.contains("↑↓ select"), "{pos:?}: {bar:?}");
            match selected_enter(&app) {
                Some(enter) => {
                    verbs.push(enter.label());
                    assert!(
                        bar.contains(&format!("Enter {}", enter.label())),
                        "{pos:?}: the bar does not name what Enter does: {bar:?}"
                    );
                }
                // 押しても何も起きない行では `Enter` を出さない（出したら嘘になる）
                None => assert!(
                    !bar.contains("Enter"),
                    "{pos:?}: the bar offers Enter where nothing happens: {bar:?}"
                ),
            }
        }
        // 舐めた行が「メニュー」1 色になっていない ＝ 種類ごとの違いが本当に出ている
        for verb in ["menu", "new session", "update"] {
            assert!(verbs.contains(&verb), "no row offered {verb:?}: {verbs:?}");
        }
    }

    /// **案内した `Enter` は本当に効き、案内しなかった `Enter` は本当に効かない。**
    /// 語と実装が別々に育つのを止めるための突き合わせ。
    ///
    /// 版行の `update` だけは押さない: 実行すると本物のダウンロードと
    /// `claude update` が走るため（案内と実行が同じ [`selected_enter`] を読むことは
    /// [`run_enter`] の網羅 match が保証する）
    #[test]
    fn enter_does_exactly_what_the_bottom_bar_said() {
        let mut app = app_with_every_row_kind();
        for pos in touchable_positions(&mut app) {
            let mut app = app_with_every_row_kind();
            drawn_bottom_bar(&mut app);
            app.selection = pos;
            let expected = selected_enter(&app);
            if matches!(expected, Some(Enter::UpdateCcdesk | Enter::UpdateClaude)) {
                continue;
            }
            press(&mut app, KeyCode::Enter);
            match expected {
                Some(Enter::Menu) => assert!(app.popup.is_some(), "{pos:?}: no menu opened"),
                Some(Enter::NewSession) => assert!(
                    matches!(app.right_view, RightView::New(_)),
                    "{pos:?}: the new session screen did not open"
                ),
                Some(Enter::UpdateCcdesk | Enter::UpdateClaude) => unreachable!(),
                None => {
                    assert!(app.popup.is_none(), "{pos:?}: a menu opened on a row that offers nothing");
                    assert!(
                        matches!(app.right_view, RightView::Sessions),
                        "{pos:?}: the right pane switched on a row that offers nothing"
                    );
                    assert_eq!(state_name(&app), "Idle", "{pos:?}: an update started");
                }
            }
        }
    }

    /// **案内は同じフレームの選択行を見る（1 フレーム遅れない）。**
    ///
    /// 案内が読む材料は「選択位置」と「一覧に積まれた行」の 2 つで、後者を作るのは
    /// サイドバーの描画そのもの。**下部バーを先に描くと材料が 1 フレーム古くなる**ので、
    /// (a) 最初のフレーム（行がまだ積まれていない）と
    /// (b) 選択行の種類がそのフレームで変わった場合に、案内が実際の行と食い違う。
    /// どちらも「触れる行なのに `Enter` を出さない / 何も起きないのに `Enter` を出す」
    /// という嘘になる
    #[test]
    fn the_bottom_bar_reads_the_same_frame_as_the_sidebar() {
        // (a) 最初の 1 フレームから正しい。行を積む前に案内を作っていると
        // 「選択行が無い」ことになり `Enter` が落ちる
        let mut app = app_with_every_row_kind();
        let first = drawn_bottom_bar(&mut app);
        assert_eq!(app.selection, SidebarPos::Row(0), "the fixture's premise broke");
        assert!(
            first.contains("Enter update"),
            "the first frame's hint does not know the selected row yet: {first:?}"
        );

        // (b) 同じ選択位置のまま行の種類が変わったら、そのフレームで案内も変わる
        // （更新が終わって版行が「押しても何も起きない行」になった状況）
        app.ccdesk_latest = None;
        let after = drawn_bottom_bar(&mut app);
        assert_eq!(app.sidebar_rows[0], SidebarRow::Inert, "the row kind did not change");
        assert!(
            !after.contains("Enter"),
            "the hint still describes the row from the previous frame: {after:?}"
        );
    }

    /// **`↑↓` の直後の 1 フレームで案内が入れ替わる。** 行の種類で `Enter` の意味が
    /// 違うので、選択が動いたのに案内が残ると「押したら何が起きるか」が読めなくなる
    #[test]
    fn the_bottom_bar_switches_verbs_as_the_selection_moves() {
        let mut app = app_with_every_row_kind();
        // 先頭の触れる行 = ccdesk の版行（更新あり）
        let update_row = drawn_bottom_bar(&mut app);
        assert!(update_row.contains("Enter update"), "the premise broke: {update_row:?}");

        // 1 つ下は claude の版行（更新なし）＝ `Enter` を出さない行
        press(&mut app, KeyCode::Down);
        assert_eq!(app.selection, SidebarPos::Row(1), "the selection did not move");
        let inert_row = drawn_bottom_bar(&mut app);
        assert!(
            !inert_row.contains("Enter"),
            "the hint lagged behind the selection: {inert_row:?}"
        );

        // さらに下は `+ new session`（区切り線は飛ばす）＝ 別の動詞になる
        press(&mut app, KeyCode::Down);
        let new_session_row = drawn_bottom_bar(&mut app);
        assert!(
            new_session_row.contains("Enter new session"),
            "the hint did not switch to the next row's verb: {new_session_row:?}"
        );

        // 戻しても同じフレームで戻る（片方向だけ追従しているのではない）
        press(&mut app, KeyCode::Up);
        press(&mut app, KeyCode::Up);
        assert_eq!(drawn_bottom_bar(&mut app), update_row, "the hint did not follow back");
    }

    /// **更新の無い版行も「触れる行」。** 押しても何も起きないが行の実体はあるので、
    /// `↑↓` で止まり、マウスを乗せればホバーする（以前は区切り線と同じ扱いで、
    /// 選択・ホバー・ハイライトの全部から漏れていた）
    #[test]
    fn a_version_row_without_an_update_is_still_selectable_and_hoverable() {
        let mut app = app_with_every_row_kind();
        drawn_bottom_bar(&mut app);
        // claude の版行（更新なし）は行 1。飾りではないことを描画結果から確かめる
        assert_eq!(app.sidebar_rows[1], SidebarRow::Inert, "the fixture's premise broke");

        app.selection = SidebarPos::Row(0);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.selection, SidebarPos::Row(1), "↑↓ skipped the row");

        handle_mouse(&mut app, &moved(3, 2)).unwrap(); // 版行 1 の画面 y は 2
        assert_eq!(app.hovered, Some(SidebarPos::Row(1)), "the row is not hoverable");
    }

    /// **区切り線は飾りなので触れない。** 「押しても何も起きない行」と同じ扱いに
    /// してしまうと、実体の無い行に選択が止まってハイライトが出る
    #[test]
    fn a_separator_is_neither_selectable_nor_hoverable() {
        let mut app = app_with_every_row_kind();
        drawn_bottom_bar(&mut app);
        let separator = app
            .sidebar_rows
            .iter()
            .position(|row| *row == SidebarRow::Decoration)
            .expect("no decoration row was stacked");

        // ↑↓ はこの行を飛ばす（下から上へ・上から下への両方向）
        app.selection = SidebarPos::Row(separator.saturating_sub(1));
        press(&mut app, KeyCode::Down);
        assert_ne!(app.selection, SidebarPos::Row(separator), "↑↓ stopped on a decoration row");
        app.selection = SidebarPos::Row(separator + 1);
        press(&mut app, KeyCode::Up);
        assert_ne!(app.selection, SidebarPos::Row(separator), "↑↓ stopped on a decoration row");

        // マウスを乗せてもホバーしない
        handle_mouse(&mut app, &moved(3, separator as u16 + 1)).unwrap();
        assert_eq!(app.hovered, None, "a decoration row is hovered");
    }

    /// ヘッダーの版行 2 本 + 区切り線 + `+ new session` を積んだサイドバー。
    /// 版行のヒットテストを見るテストの土台
    fn app_with_version_rows(sidebar_width: u16) -> App {
        let mut app = test_app(sidebar_width, TERM);
        app.sidebar_rows = vec![
            SidebarRow::Action(RowAction::UpdateCcdesk),
            SidebarRow::Action(RowAction::UpdateClaude),
            SidebarRow::Decoration, // 区切り線
            SidebarRow::Action(RowAction::New),
        ];
        app.sidebar_header_rows = 4;
        app
    }

    /// 版行は**行全体が当たる**。列 0（一覧行なら `=` の桁）から内容の最右列まで、
    /// どこを押しても同じ行に解決する（更新行にメニューは無いので列 0 も行に当たる）。
    ///
    /// 更新の実行そのものは副作用（ダウンロード / `claude update` 起動）なので、
    /// 判定の到達点は「クリックがどの行に解決したか」で見る。ディスパッチが読むのと
    /// 同じ `row` / `action` の組なので、これが一致していれば実行先も一致する
    #[test]
    fn clicking_anywhere_on_a_version_row_resolves_to_its_update_action() {
        let mut app = app_with_version_rows(34);
        // 内容の桁は x=1..=sidebar_width-2（左右の枠を除く内側）
        let rightmost = app.sidebar_width - 2;
        for (y, row, expected) in [
            (1u16, 0usize, RowAction::UpdateCcdesk),
            (2, 1, RowAction::UpdateClaude),
        ] {
            for col in [0, 1, 2, 5, rightmost - 1, rightmost] {
                handle_mouse(&mut app, &click(col, y)).unwrap();
                assert_eq!(app.hovered, Some(SidebarPos::Row(row)), "y={y} col={col}");
                assert_eq!(app.selection, SidebarPos::Row(row), "y={y} col={col}");
                assert_eq!(
                    app.sidebar_rows[row].action(),
                    Some(&expected),
                    "y={y} col={col}"
                );
                assert!(app.popup.is_none(), "an update row must not open a menu y={y} col={col}");
                assert!(!app.dragging, "must not start a resize drag y={y} col={col}");
            }
        }
        assert_eq!(app.sidebar_width, 34, "sidebar width must not change");
    }

    /// 版行の右端に置く動詞（`update` / `restart`）は内容の最右列で終わるので、
    /// **幅変更のつかみ代（境界線の 2 列）には掛からない**。1 桁でも外すと
    /// 動詞のクリックがサイドバー幅変更に化ける
    #[test]
    fn the_verb_at_the_right_edge_of_a_version_row_is_not_the_resize_grip() {
        let mut app = app_with_version_rows(34);
        // ui::version_row が右端寄せする先は内側幅 = sidebar_width - 2 桁ぶん。
        // その最終桁の画面 x は 1 + (内側幅 - 1) = sidebar_width - 2
        let verb_end = app.sidebar_width - 2;
        handle_mouse(&mut app, &click(verb_end, 1)).unwrap();
        assert!(!app.dragging, "the verb's last column must not be the resize grip");
        assert_eq!(
            app.selection,
            SidebarPos::Row(0),
            "clicking the verb must hit the row"
        );
        // つかみ代はその 1 つ外（右枠の列）から始まる = 境界がここにあることの固定
        let mut app = app_with_version_rows(34);
        handle_mouse(&mut app, &click(verb_end + 1, 1)).unwrap();
        assert!(app.dragging, "the border column must start a resize");
    }

    /// メニュー表示中の版行クリックは**メニューが受ける**（誤爆しない）。
    /// popup 判定が行のヒットテストより先にあることの固定
    #[test]
    fn an_open_menu_swallows_clicks_aimed_at_the_version_rows() {
        let mut app = app_with_version_rows(34);
        app.selection = SidebarPos::Row(3); // `+ new session`。動いたら分かる位置に置く
        open(&mut app, PopupKind::Group, 3);
        let rect = popup_rect(&app, app.popup.as_ref().unwrap());
        assert!(rect.y > 2, "the menu must overlap the version row so this isn't an outside click");
        handle_mouse(&mut app, &click(5, 1)).unwrap();
        assert_eq!(
            app.selection,
            SidebarPos::Row(3),
            "a version row must not become selected"
        );
        assert!(app.hovered.is_none(), "a version row must not be hovered");
        assert!(app.popup.is_none(), "an outside click must close the menu");
        assert_eq!(state_name(&app), "Idle", "the update must not run");
    }

    /// 更新の進行状態の名前（中身の文面ではなく「どの状態か」だけを見たい）
    fn state_name(app: &App) -> &'static str {
        match &*app
            .ccdesk_update
            .lock_recover()
        {
            SelfUpdate::Idle => "Idle",
            SelfUpdate::Running => "Running",
            SelfUpdate::Done => "Done",
            SelfUpdate::Failed(_) => "Failed",
        }
    }

    /// テスト用の供給元は **これ 1 つだけ**。差し替えたいのは「プロジェクト永続化」
    /// なので、軸ごとの enum を差し込む形にして `impl DataSource` を 1 つに保つ。
    ///
    /// **なぜ軸ごとに別の struct を並べないか（判断の記録）**: 以前は
    /// `RecordingSource` / `StoreSource` / `MemoryDiskSource` の 3 つがそれぞれ
    /// `impl DataSource` を持っていた。[`DataSource`] にメソッドが 1 つ増えるだけで
    /// 直す場所が 3 箇所になり、しかも「メソッドを足す変更」と「戻り値を変える変更」が
    /// 別ブランチで並ぶと**テキスト衝突なしにテストビルドだけが壊れたマージ**が
    /// 生まれる（実際に E0046 / E0053 になった）。[`App`] の [`Default`] を
    /// 構造体定義の隣に置いてあるのと同じ判断で、
    /// **1 つの変更が 1 箇所に閉じる（局所性）**方を取る。
    ///
    /// **軸を enum にしたのは「実物を通す」性質を落とさないため**:
    /// [`ProjectsBackend::MemoryDisk`] は live と同じ [`persist_projects`] を通る
    /// ＝ ドメインを偽物へ置き換えていない
    struct TestSource {
        projects: ProjectsBackend,
        /// hook が書いた state の写し（[`DataSource::hook_states`] が返す値）。
        /// 実ファイルを読まないので、テストが開発者の
        /// `~/.ccdesk/hook-states.json` に左右されない
        hooks: HookStates,
        /// [`WindowItem::LastView`] として保存された値の記録（**保存された回数まで
        /// 見たい**ので Vec）。実ファイル（`~/.ccdesk/state.json`）は書かない
        views: Arc<Mutex<Vec<String>>>,
        /// [`DataSource::refresh_usage`] が呼ばれた回数（claude は起こさない）。
        /// **回数まで見る**のは、押していないのに取り直す経路が生えたら気づくため
        usage_refreshes: Arc<std::sync::atomic::AtomicUsize>,
        /// [`DataSource::usage`] が返す値（当たり判定は「今出ているか」で決まるので、
        /// 出ている状態を作れる必要がある）
        usage: Usage,
    }

    /// プロジェクト永続化側の振る舞い
    enum ProjectsBackend {
        /// 永続化層を持たない（アカウント側の検査）。渡された一覧をそのまま返す
        /// ＝ ディスクが空の単独起動と同じ結果なので、live の意味論と矛盾しない
        Absent,
        /// state.json をメモリに置く。**保存の意味論（他インスタンスの登録との
        /// マージ・上限・次の基準）は live と同じ関数**
        /// （[`persist_projects`]）を通すので、「保存するとどうなるか」を
        /// テスト側へ写し取らずに App の側を検査できる。
        /// 実ファイルを触らないので実ユーザーの ~/.ccdesk は動かない
        MemoryDisk {
            /// ディスク上の一覧（他インスタンスの登録を仕込むのもここ）
            disk: Mutex<Vec<String>>,
            /// 「ディスクはこうなっている」との判断 = マージの基準（live と同じ持ち方）
            baseline: Mutex<Vec<String>>,
        },
    }

    impl TestSource {
        /// どの軸も差し替えない土台。**軸ごとのヘルパはここから 1 つだけ変える**
        /// （軸を足したときに直すのがこの 1 箇所で済む）
        fn plain() -> Self {
            Self {
                projects: ProjectsBackend::Absent,
                hooks: HookStates::default(),
                views: Arc::new(Mutex::new(Vec::new())),
                usage_refreshes: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                usage: Usage::Unknown,
            }
        }

        /// 使用率が出ている供給元（クリック当たり判定の検査）
        fn for_usage(usage: Usage, usage_refreshes: Arc<std::sync::atomic::AtomicUsize>) -> Self {
            Self {
                usage,
                usage_refreshes,
                ..Self::plain()
            }
        }

        /// プロジェクト側だけを見る供給元
        fn for_projects(projects: ProjectsBackend) -> Self {
            Self {
                projects,
                ..Self::plain()
            }
        }

        /// hook が書いた state だけを見る供給元
        fn for_hooks(hooks: HookStates) -> Self {
            Self {
                hooks,
                ..Self::plain()
            }
        }

        /// 開いていた画面（[`WindowItem::LastView`]）の保存を記録する供給元
        fn for_views(views: Arc<Mutex<Vec<String>>>) -> Self {
            Self {
                views,
                ..Self::plain()
            }
        }
    }

    impl DataSource for TestSource {
        // セッション一覧は差し替えの軸に入れていない（今の検査対象はアカウントと
        // プロジェクト永続化の 2 つだけ）。永続化層を持たない ＝ 読みは 0 件、
        // 保存は渡された一覧をそのまま返す（ディスクが空の単独起動と同じ結果なので
        // live の意味論と矛盾しない。[`ProjectsBackend::Absent`] と同じ判断）
        fn sessions(&self) -> Vec<SessionRow> {
            Vec::new()
        }

        fn store_sessions(&self, next: &[SessionRow]) -> Vec<SessionRow> {
            next.to_vec()
        }

        // 表示名は transcript から導くが、この供給元は transcript を持たない
        fn titles(&self) -> Titles {
            Titles::fixed(std::collections::HashMap::new())
        }

        // 実ファイル（`~/.ccdesk/hook-states.json`）は読まない
        fn hook_states(&self) -> HookStates {
            self.hooks.clone()
        }

        // 窓を持たない行を動いていることにする経路は撮影用だけ（[`DemoSource`]）
        fn fixed_states(&self) -> std::collections::HashMap<SessionId, String> {
            std::collections::HashMap::new()
        }

        fn hook_stamp(&self) -> Option<(u64, std::time::SystemTime)> {
            // テストの供給元はファイルを持たない（周期の前倒しは起きない）
            None
        }

        fn footer(&self) -> FooterInfo {
            FooterInfo::default()
        }

        fn usage(&self) -> Usage {
            // テスト用の供給元は claude を起こさない（値は仕込まれたものを返すだけ）
            self.usage.clone()
        }

        fn refresh_usage(&self) -> bool {
            self.usage_refreshes
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            true
        }

        fn window_state(&self) -> WindowState {
            WindowState {
                sidebar_width: 34,
                last_view: None,
                dispatch_cwd: String::new(),
                grouping: Grouping::State,
                // 起動時に読むディスクの内容。永続化層が無ければ 0 件
                projects: match &self.projects {
                    ProjectsBackend::Absent => Vec::new(),
                    ProjectsBackend::MemoryDisk { disk, .. } => disk
                        .lock_recover()
                        .clone(),
                },
            }
        }

        // 記録するのは「次に開く画面」だけ（他の項目は実ファイルへも書かない）。
        // None（new session 画面）の保存表記は live 側の持ち物なので、
        // ここでは Option の形をそのまま記録する
        fn save_window(&self, item: WindowItem<'_>) {
            if let WindowItem::LastView(view) = item {
                self.views
                    .lock_recover()
                    .push(view.map(|id| id.to_string()).unwrap_or_default());
            }
        }

        fn store_projects(&self, next: &[String]) -> Vec<String> {
            match &self.projects {
                ProjectsBackend::Absent => next.to_vec(),
                ProjectsBackend::MemoryDisk { disk, baseline } => {
                    let mut disk = disk
                        .lock_recover();
                    let mut baseline = baseline
                        .lock_recover();
                    persist_projects(&mut baseline, next, |merge| {
                        *disk = merge(disk.clone());
                        true // メモリ上のディスクは書き込みに失敗しない
                    })
                }
            }
        }

        fn spawn_pollers(&self, _sinks: PollSinks) {}

        // テストが実プロセス（claude）を起こさない。既定の供給元
        // （[`crate::source::DemoSource`]）と同じ約束を、差し替えた側でも守る
        fn spawns_sessions(&self) -> bool {
            false
        }

    }

    /// ログイン済みのアカウント行を持つ `App`
    fn app_with_account_footer() -> App {
        use crate::poll::AccountStatus;
        App {
            footer: FooterInfo {
                account: AccountStatus::LoggedIn("taro".to_string()),
                ..FooterInfo::default()
            },
            ..test_app(34, TERM)
        }
    }

    /// **アカウント行は押しても何も起きない。** 出しているのは「今サインインして
    /// いるのは誰か」だけなので、行全体が当たっても起きるのは選択とホバーだけ
    /// （メニューは開かず、幅変更ドラッグにもならない）。
    /// 当たり判定は描画と同じ [`sidebar_layout`] の `account_y`
    #[test]
    fn clicking_the_account_row_only_moves_the_selection() {
        let mut app = app_with_account_footer();
        let sl = sidebar_layout(&app);
        assert!(sl.footer_visible, "the footer must be visible for this test's premise");
        // 内容の桁は x=1..=sidebar_width-2（枠の内側）。列 0 も行に当たる
        let rightmost = app.sidebar_width - 2;
        for col in [0, 1, 2, 5, rightmost - 1, rightmost] {
            app.selection = SidebarPos::Row(0);
            handle_mouse(&mut app, &click(col, sl.account_y)).unwrap();
            assert!(app.popup.is_none(), "a menu opened at col={col}");
            assert!(!app.dragging, "col={col} must not start a resize drag");
            // 選択はアカウント行へ移る（キーボードで降りたときと同じ位置に居る）
            assert_eq!(app.selection, SidebarPos::Account, "col={col}");
            // ホバーもアカウント行を指す（一覧の行 index には化けない）
            assert_eq!(app.hovered, Some(SidebarPos::Account), "col={col}");
        }
        assert_eq!(app.sidebar_width, 34, "sidebar width must not change");
    }

    /// **アカウント行を選んだ `Enter` は何も起こさない。**
    /// 更新の無い版行（[`SidebarRow::Inert`]）と同じ答えを
    /// [`selected_enter`] 1 箇所から得ることの固定
    #[test]
    fn pressing_enter_on_the_account_row_does_nothing() {
        let mut app = app_with_account_footer();
        app.sidebar_rows = vec![SidebarRow::Action(RowAction::New)];
        app.sidebar_header_rows = 1;
        app.selection = SidebarPos::Account;
        assert_eq!(selected_enter(&app), None, "the account row claims an Enter action");
        press(&mut app, KeyCode::Enter);
        assert!(app.popup.is_none(), "Enter opened a menu on the account row");
        assert!(
            matches!(app.right_view, RightView::Sessions),
            "Enter changed the right pane from the account row"
        );
        assert_eq!(app.selection, SidebarPos::Account, "Enter moved the selection");
    }

    /// ホバーの行き先が両方ある `App`: フッターのアカウント行と、一覧の押せる行 1 本
    fn app_with_hoverable_rows() -> App {
        let mut app = app_with_account_footer();
        app.sidebar_rows = vec![SidebarRow::Action(RowAction::New)];
        app.sidebar_header_rows = 1;
        app
    }

    /// **アカウント行はマウスを乗せただけでホバー対象になる。** 一覧の行 index では
    /// 表せない位置なので、以前は「一覧の行ではない」として除外されていた。
    /// マウスが離れれば（他の行・サイドバーの外）ホバーは外れる
    #[test]
    fn hovering_the_account_row_marks_it_as_hovered() {
        let mut app = app_with_hoverable_rows();
        let sl = sidebar_layout(&app);
        assert!(sl.footer_visible, "the footer must be visible for this test's premise");

        handle_mouse(&mut app, &moved(3, sl.account_y)).unwrap();
        assert_eq!(app.hovered, Some(SidebarPos::Account));

        // 一覧の行へ移ればそちらのホバーへ移る（アカウント行のハイライトは消える）
        handle_mouse(&mut app, &moved(3, 1)).unwrap();
        assert_eq!(app.hovered, Some(SidebarPos::Row(0)));
        // サイドバーの外へ出ればホバーは無くなる
        let outside = app.sidebar_width + 5;
        handle_mouse(&mut app, &moved(outside, sl.account_y)).unwrap();
        assert_eq!(app.hovered, None);
    }

    /// **ホバーとクリックはまったく同じ行に当たる。** 当たり判定を別に計算していない
    /// ことの担保なので、片方の y だけを見るのではなく、サイドバー内の全 y について
    /// 「ホバーがアカウント行になる y」と「クリックで選択がアカウント行へ移る y」の
    /// 集合が一致することを見る
    #[test]
    fn the_hover_and_the_click_hit_the_account_row_at_the_same_rows() {
        // 突く桁は枠の内側かつ幅変更のつかみ代の外（行のどこでも当たるので 1 つで足りる）
        const COL: u16 = 3;
        let sl = sidebar_layout(&app_with_hoverable_rows());
        assert!(
            COL < app_with_hoverable_rows().sidebar_width - 1,
            "the probed column must be inside the sidebar"
        );
        let rows = TERM.1;
        let hovered_rows: Vec<u16> = (0..rows)
            .filter(|y| {
                let mut app = app_with_hoverable_rows();
                handle_mouse(&mut app, &moved(COL, *y)).unwrap();
                app.hovered == Some(SidebarPos::Account)
            })
            .collect();
        let clicked_rows: Vec<u16> = (0..rows)
            .filter(|y| {
                let mut app = app_with_hoverable_rows();
                handle_mouse(&mut app, &click(COL, *y)).unwrap();
                app.selection == SidebarPos::Account
            })
            .collect();
        assert_eq!(hovered_rows, clicked_rows, "the hover and the click hit different rows");
        // 空集合どうしの一致で通らないように、その 1 行が描画のジオメトリと同じことも見る
        assert_eq!(
            hovered_rows,
            vec![sl.account_y],
            "the account row is not where sidebar_layout says"
        );
    }

    /// フッターが無い（狭い）端末ではアカウント行は描かれないのでホバー対象にもしない。
    /// **描いていない行を光らせない**という選択と同じ規則
    #[test]
    fn a_hidden_footer_keeps_the_account_row_out_of_the_hover() {
        let mut app = app_with_hoverable_rows();
        app.term_size = (60, 8); // 下部バー 1 行を引くと footer_visible が落ちる高さ
        let sl = sidebar_layout(&app);
        assert!(!sl.footer_visible, "the footer must be hidden for this test's premise");
        handle_mouse(&mut app, &moved(3, sl.account_y)).unwrap();
        assert_ne!(
            app.hovered,
            Some(SidebarPos::Account),
            "a row that is not drawn must not be hovered"
        );
    }

    /// マウス移動でホバー位置が変わらないなら描き直さない（FPS 対策）。
    /// **アカウント行でも同じ**: 行の中で桁が動いただけでは再描画しない
    #[test]
    fn moving_the_mouse_inside_the_same_row_does_not_redraw() {
        let mut app = app_with_hoverable_rows();
        let sl = sidebar_layout(&app);
        handle_mouse(&mut app, &moved(3, sl.account_y)).unwrap();
        let prev = (app.hovered, app.usage_hovered);
        handle_mouse(&mut app, &moved(10, sl.account_y)).unwrap();
        assert_eq!(app.hovered, prev.0, "the same row must resolve to the same hover");
        assert!(
            !mouse_needs_redraw(MouseEventKind::Moved, prev, (app.hovered, app.usage_hovered)),
            "moving inside the account row must not ask for a redraw"
        );
        // 行が変われば描き直す
        let prev = (app.hovered, app.usage_hovered);
        handle_mouse(&mut app, &moved(3, 1)).unwrap();
        assert!(
            mouse_needs_redraw(MouseEventKind::Moved, prev, (app.hovered, app.usage_hovered)),
            "leaving the account row must ask for a redraw"
        );
        // 移動以外は表示を変えるので常に描き直す
        assert!(mouse_needs_redraw(
            MouseEventKind::Down(MouseButton::Left),
            (app.hovered, app.usage_hovered),
            (app.hovered, app.usage_hovered)
        ));
    }

    /// **見送りはすべてに勝つ。** 子が画面を作り替えている最中は、打鍵でも
    /// PTY の新出力でも無変化の周期でも描かない（掴むとカーソルが中間位置で
    /// 確定し、IME の変換窓がそこへ飛ぶ）。
    ///
    /// 見送った周が `dirty` を降ろさないことは構造で担保する（降ろすのは
    /// この関数が true を返した側の枝だけ）。上限は
    /// [`crate::session::Session::holds_frame`] が持つのでここには無い
    #[test]
    fn a_frame_the_child_is_still_writing_is_not_grabbed_for_any_reason() {
        let fresh = Duration::from_millis(0);
        for (force, pty_dirty, since_draw) in [
            (true, false, fresh),
            (false, true, fresh),
            (false, false, IDLE_REDRAW * 10),
        ] {
            assert!(
                should_draw(false, force, pty_dirty, since_draw, IDLE_REDRAW),
                "nothing asked for a redraw: force={force} dirty={pty_dirty}"
            );
            assert!(
                !should_draw(true, force, pty_dirty, since_draw, IDLE_REDRAW),
                "a frame was grabbed while the child was mid-redraw: \
                 force={force} dirty={pty_dirty}"
            );
        }
        // 何も起きていない周は描かない（無条件 60fps にしない）
        assert!(!should_draw(false, false, false, fresh, IDLE_REDRAW));
        // スピナーが回っている間だけ短い周期でも描く（回っていなければ 1 秒粒度）
        let between = SPINNER_REDRAW + Duration::from_millis(50);
        assert!(should_draw(false, false, false, between, SPINNER_REDRAW));
        assert!(!should_draw(false, false, false, between, IDLE_REDRAW));
    }

    /// **一覧とアカウント行は 1 つの輪。** 下端の先がアカウント行で、その先は
    /// 一覧の先頭へ戻る（マウスで押せる行はキーボードでも届き、戻るために
    /// 一覧全体を遡らずに済む）
    #[test]
    fn the_arrow_keys_loop_through_the_list_and_the_account_row() {
        let mut app = app_with_account_footer();
        app.sidebar_rows = vec![
            SidebarRow::Action(RowAction::New),
            SidebarRow::Decoration,
            SidebarRow::Action(RowAction::ToggleGroup),
        ];
        app.sidebar_header_rows = 3;
        app.selection = SidebarPos::Row(0);

        press(&mut app, KeyCode::Down); // 区切り線は飛ばす
        assert_eq!(app.selection, SidebarPos::Row(2));
        press(&mut app, KeyCode::Down);
        assert_eq!(app.selection, SidebarPos::Account, "must reach the account row");
        // アカウント行の先は一覧の先頭（端で止まらない）
        press(&mut app, KeyCode::Down);
        assert_eq!(app.selection, SidebarPos::Row(0), "must wrap around to the top of the list");
        // 一覧の先頭で `↑` はアカウント行へ回る
        press(&mut app, KeyCode::Up);
        assert_eq!(app.selection, SidebarPos::Account, "must wrap around to the account row");
        press(&mut app, KeyCode::Up);
        assert_eq!(app.selection, SidebarPos::Row(2), "must return to the list");
    }

    /// フッターが無い（狭い）端末ではアカウント行は描かれないので選択対象にもしない。
    /// **押せない位置に選択を残さない**という一覧の行と同じ規則で、
    /// 巡回は一覧の中だけで閉じる
    #[test]
    fn a_hidden_footer_keeps_the_account_row_out_of_the_selection() {
        let mut app = app_with_account_footer();
        app.term_size = (60, 8); // 下部バー 1 行を引くと footer_visible が落ちる高さ
        assert!(
            !sidebar_layout(&app).footer_visible,
            "the footer must be hidden for this test's premise"
        );
        app.sidebar_rows = vec![SidebarRow::Action(RowAction::New)];
        app.sidebar_header_rows = 1;
        app.selection = SidebarPos::Row(0);
        for key in [KeyCode::Down, KeyCode::Up] {
            press(&mut app, key);
            assert_eq!(
                app.selection,
                SidebarPos::Row(0),
                "{key:?} must not select a row that is not drawn"
            );
        }
        // 触れる行が 2 本あれば一覧の中で巡回する（フッターへは出ない）
        app.sidebar_rows.push(SidebarRow::Action(RowAction::ToggleGroup));
        press(&mut app, KeyCode::Up);
        assert_eq!(app.selection, SidebarPos::Row(1), "the list must loop on its own");
    }

    /// 更新の入口のガード。**副作用（ダウンロード）が起きない経路だけを通す**:
    /// 新しい版を知らないとき / 実行中 / 再起動待ちは、押しても何も始まらない。
    /// `Idle` + タグありは本物のダウンロードが走るので、ここでは通さない
    #[test]
    fn the_ccdesk_update_entry_point_refuses_to_start_twice() {
        // 新しい版を知らない = 行もクリック不可なので、呼ばれても始まらない
        let mut app = test_app(34, TERM);
        start_ccdesk_update(&mut app);
        assert_eq!(state_name(&app), "Idle");
        // 実行中・再起動待ちは、タグを知っていても再実行しない
        for (state, name) in [(SelfUpdate::Running, "Running"), (SelfUpdate::Done, "Done")] {
            let mut app = test_app(34, TERM);
            app.ccdesk_latest = Some("v9.9.9".to_string());
            *app.ccdesk_update
                .lock_recover() = state;
            start_ccdesk_update(&mut app);
            assert_eq!(
                state_name(&app),
                name,
                "must not re-run an update that is finished or running"
            );
        }
    }

    /// テスト用の一覧行 1 本（cwd と更新時刻だけが関心事）。
    /// `updated_at` は埋め戻しの「新しい順」に効くので明示で受ける
    fn session_row(id: &str, cwd: &str, updated_at: u64) -> SessionRow {
        SessionRow {
            updated_at,
            ..SessionRow::new(SessionId::new(id), cwd, updated_at)
        }
    }

    /// **別インスタンス（や ccdesk の外）で動いているセッションを `claude -r` で
    /// 二重に起こさない。** `agents --json` に interactive で載っている ＝
    /// どこかで生きて動いている ＝ 二重に開くと同じ会話を 2 プロセスが同時更新する
    #[test]
    fn opening_a_session_running_elsewhere_does_not_double_resume() {
        let mut app = test_app(34, TERM);
        app.sessions = vec![session_row("s", "C:\\dev\\api", 1)];
        app.agents = vec![AgentInfo {
            session_id: "s".to_string(),
            kind: "interactive".to_string(),
            status: "busy".to_string(),
            pid: Some(4242),
        }];
        assert!(
            !open_session(&mut app, &SessionId::new("s")),
            "reported the pane as opened"
        );
        assert!(
            app.windows.is_empty(),
            "spawned a second claude for a session that is already running"
        );
        assert!(app.notice.is_some(), "the user was not told why nothing opened");
    }

    fn project(cwd: &str, has_sessions: bool) -> PopupKind {
        PopupKind::Project {
            cwd: cwd.to_string(),
            has_sessions,
        }
    }

    // ── 起こし直し方 / `/resume` の追従 / リネームの書き先 ──────────────────

    /// **1 ターンも会話していない行は `claude -r` で開けない。**
    ///
    /// 前景セッションは 1 ターン終わるまで transcript を作らないので、起こして
    /// すぐ `close` した行を `open` すると `No conversation found` になっていた。
    /// **起動先の cwd も一緒に決まる**（会話が在る作業ツリーで打つ）
    #[test]
    fn a_row_with_no_transcript_is_started_fresh_instead_of_resumed() {
        let temp =
            crate::title::tests::TempProjects::new("a_row_with_no_transcript_is_started_fresh");
        let mut titles = temp.titles();
        let mut row = session_row("s", "C:\\dev\\api", 1);
        let (launch, cwd) = relaunch(&titles, &row);
        assert!(
            matches!(launch, Launch::New { prompt } if prompt.is_empty()),
            "a row with no conversation must not be resumed with -r"
        );
        assert_eq!(cwd, row.cwd, "a fresh start must use the row's own cwd");
        // 1 ターン終わって transcript ができたら再開になる
        titles.write_transcript(&row, "{\"type\":\"user\"}\n");
        let mut budget = u64::MAX;
        titles.refresh(&mut row, &mut budget);
        let (launch, cwd) = relaunch(&titles, &row);
        assert!(
            matches!(launch, Launch::Resume),
            "a row that has a conversation must be resumed"
        );
        assert_eq!(cwd, row.cwd);
    }

    /// **セッションの中で `/rename` した結果がそのままサイドバーへ出る。**
    ///
    /// 名前の正本は transcript 1 箇所で、行は名前を持たない ＝ 「格下げしない
    /// ガード」も「`Custom` の行は触らない」除外も要らない。仕込む行は
    /// **実測した transcript の形そのまま**（claude が書く形に追従できているかを
    /// 見たいので、こちら側の組み立てを通さない）
    #[test]
    fn a_rename_inside_the_session_reaches_the_row() {
        let temp = crate::title::tests::TempProjects::new("a_rename_inside_the_session");
        let mut app = app_with_row("s");
        app.titles = temp.titles();
        // 名前が付く前（材料が無い）
        refresh_transcripts(&mut app);
        assert_eq!(app.titles.of(only_row(&app)), crate::title::UNTITLED);
        app.titles.write_transcript(
            &app.sessions[0].clone(),
            "{\"type\":\"custom-title\",\"customTitle\":\"renamed in the session\",\"sessionId\":\"s\"}
",
        );

        refresh_transcripts(&mut app);

        assert_eq!(
            app.titles.of(only_row(&app)),
            "renamed in the session",
            "the name given inside the session did not reach the row"
        );
    }

    /// **未記録の行は解決し直され、解決した場所が行に載る。**
    ///
    /// **これは起動直後にも 1 度走る**（`main.rs` の起動列が呼ぶ）。走らせないと
    /// 最初の周期（2 秒）まで Titles のキャッシュが空 ＝ 全部の行が `new session` に
    /// 見えるうえ、未記録の行の解決も同じだけ遅れる。
    ///
    /// **会話がまだ無い行は未記録のまま**なのが正しい姿（実データの
    /// `8d162272` は 1 ターンも終わらずに終了したセッションで、transcript が
    /// そもそも存在しない ＝ 名前が `new session` なのも記録が無いのも仕様どおり）
    #[test]
    fn refreshing_transcripts_records_the_path_of_a_row_that_had_none() {
        let temp = crate::title::tests::TempProjects::new("refreshing_transcripts_records_path");
        let mut app = app_with_row("s");
        app.titles = temp.titles();
        app.sessions[0].updated_at = 1_234;

        // 1 ターンも終わっていない行は解決できない（会話が無い ＝ 記録も無い）
        refresh_transcripts(&mut app);
        assert_eq!(only_row(&app).transcript, None, "recorded a path with no conversation");
        assert_eq!(app.titles.of(only_row(&app)), crate::title::UNTITLED);

        app.titles.write_transcript(
            &app.sessions[0].clone(),
            "{\"type\":\"ai-title\",\"aiTitle\":\"a generated name\"}\n",
        );
        refresh_transcripts(&mut app);

        assert!(only_row(&app).transcript.is_some(), "the resolved path was not recorded");
        assert_eq!(app.titles.of(only_row(&app)), "a generated name");
        assert_eq!(
            only_row(&app).updated_at,
            1_234,
            "resolving a path moved the age of the row"
        );
    }

    /// **名前が変わっても行の `updated_at` は動かない。** 動かしていた頃は、
    /// claude が名前を付け直すたびに行の経過時間が 0s へ戻っていた
    /// （表示名を行に保存していたことの直接の害）
    #[test]
    fn a_new_name_does_not_disturb_the_row_timestamps() {
        let temp = crate::title::tests::TempProjects::new("a_new_name_does_not_disturb");
        let mut app = app_with_row("s");
        app.titles = temp.titles();
        app.sessions[0].updated_at = 1_234;
        app.sessions[0].last_opened_at = 1_234;
        app.titles.write_transcript(
            &app.sessions[0].clone(),
            "{\"type\":\"ai-title\",\"aiTitle\":\"a generated name\"}
",
        );

        refresh_transcripts(&mut app);

        assert_eq!(app.titles.of(only_row(&app)), "a generated name");
        assert_eq!(only_row(&app).updated_at, 1_234, "the name moved updated_at");
        assert_eq!(only_row(&app).last_opened_at, 1_234, "the name marked the row unread");
        assert!(!app.hook_states.unread(only_row(&app)));
    }

    /// **hook が何か書いたら周期を待たずに一覧を読み直す。**
    ///
    /// ペインの中で `/resume` `/clear` すると claude は新しいセッションの
    /// `SessionStart` をその場で撃つので、受け渡しファイルの見え方が変わったことが
    /// 「一覧に新しい行を出す合図」になる。**変わっていない周は何もしない**ので、
    /// 毎周 `claude agents --json` を起こし直すことにはならない
    #[test]
    fn a_hook_write_pulls_the_next_list_refresh_forward() {
        let older = std::time::SystemTime::UNIX_EPOCH;
        let newer = older + Duration::from_secs(1);
        let mut seen = None;

        // 初回に見えた時点で「変わった」（起動時の値は main が先に控える）
        assert!(hook_store_changed(&mut seen, Some((10, older))));
        assert_eq!(seen, Some((10, older)));
        // 同じ見え方の周は何も起こさない
        assert!(!hook_store_changed(&mut seen, Some((10, older))));
        // 長さが動いた / 時刻が動いた のどちらでも気づく
        assert!(hook_store_changed(&mut seen, Some((11, older))));
        assert!(hook_store_changed(&mut seen, Some((11, newer))));
        // 見え方が取れない供給元では前倒ししない（覚えた値も捨てない）
        assert!(!hook_store_changed(&mut seen, None));
        assert_eq!(seen, Some((11, newer)), "forgot the stamp when the file went missing");
    }


    /// **`/resume` の追従は pid → sessionId の 1 つの写像で決まる。**
    /// 自分の子の pid はこちらが知っているので、その pid が今動かしている
    /// セッションと窓の指す行がずれたら張り替えの合図になる
    #[test]
    fn the_live_session_of_a_pid_comes_from_the_interactive_entry() {
        let agent = |pid: Option<u32>, kind: &str, session: &str| AgentInfo {
            session_id: session.to_string(),
            kind: kind.to_string(),
            pid,
            ..AgentInfo::default()
        };
        let agents = vec![
            agent(Some(10), "interactive", "after-resume"),
            agent(Some(11), "interactive", "someone-else"),
            agent(Some(12), "bg", "a-background-job"),
            agent(Some(13), "interactive", ""),
        ];
        // hook が何も知らない間は `agents --json` が答える（従経路）
        let none = HookStates::default();
        let of = |pid, hooks: &HookStates| live_session_of(pid, 0, hooks, &agents);
        assert_eq!(of(Some(10), &none), Some(SessionId::new("after-resume")));
        // pid が分からない / 載っていない / bg / sessionId が空 は追従しない
        assert_eq!(of(None, &none), None);
        assert_eq!(of(Some(99), &none), None);
        assert_eq!(of(Some(12), &none), None, "a bg entry answered for a pane");
        assert_eq!(of(Some(13), &none), None, "an empty id became a row");

        // **hook が主。** 同じ pid について hook が別のセッションを知っていれば
        // そちらを採る（hook は turn の瞬間に届くので `agents --json` より新しい）
        let hooks = HookStates::from_records([("just-cleared", "blocked", 5_000, Some(10))]);
        assert_eq!(of(Some(10), &hooks), Some(SessionId::new("just-cleared")));
        // 窓の起動より古い hook は前回の実行のもの ＝ 従経路へ落ちる
        assert_eq!(
            live_session_of(Some(10), 5_001, &hooks, &agents),
            Some(SessionId::new("after-resume"))
        );
        // hook しか知らない pid にも答える（`agents --json` が pid を載せない環境）
        let only_hook = HookStates::from_records([("hook-only", "working", 1, Some(77))]);
        assert_eq!(of(Some(77), &only_hook), Some(SessionId::new("hook-only")));
    }

    /// 張り替えたら**新しいセッションの行を用意し、次に開く画面もそれにする**。
    /// 行が無ければ作る（`/resume` で選べる会話は ccdesk の一覧に無いこともある）
    #[test]
    fn adopting_a_switched_session_adds_the_row_and_moves_the_last_view() {
        let views = Arc::new(Mutex::new(Vec::new()));
        let mut app = App {
            sessions: vec![session_row("before", "C:\\dev\\api", 1)],
            source: Arc::new(TestSource::for_views(views.clone())),
            ..test_app(34, TERM)
        };
        let (before, after) = (SessionId::new("before"), SessionId::new("after"));
        adopt_switched_session(&mut app, &before, &after, true);

        let added = app
            .sessions
            .iter()
            .find(|row| row.session_id == after)
            .expect("the row for the new session was not created");
        assert_eq!(added.cwd, "C:\\dev\\api", "the folder of the pane was not carried over");
        assert!(
            !app.hook_states.unread(added),
            "the session showing in the pane became unread"
        );
        assert_eq!(
            views.lock().unwrap().as_slice(),
            ["after"],
            "the next start would open the session from before the /resume"
        );
        // **離れた行へは何も書かない。** 窓が移った時点でその行を動かすものは無く、
        // 表示は描画側が Stopped にする（記録を残さないので古い値も残らない）
        let left = app
            .sessions
            .iter()
            .find(|row| row.session_id == before)
            .expect("the row that was left behind is gone");
        assert_eq!(left, &session_row("before", "C:\\dev\\api", 1), "the row left behind was written to");
        assert!(
            !app.hook_states.unread(left),
            "leaving a row put an unread mark on it"
        );

        // 既にある行へ戻ったときは行を増やさない（`last_view` だけが動く）
        adopt_switched_session(&mut app, &after, &before, true);
        assert_eq!(app.sessions.len(), 2, "an existing row was duplicated");
        assert_eq!(views.lock().unwrap().as_slice(), ["after", "before"]);
    }

    /// **ペインに出ていない窓の張り替えでは `last_view` を動かさない**
    /// （次に開くのは「最後に見ていた画面」なので、裏の窓で書き換えたら嘘になる）
    #[test]
    fn adopting_a_switch_in_a_background_window_leaves_the_last_view_alone() {
        let views = Arc::new(Mutex::new(Vec::new()));
        let mut app = App {
            sessions: vec![session_row("before", "C:\\dev\\api", 1)],
            source: Arc::new(TestSource::for_views(views.clone())),
            ..test_app(34, TERM)
        };
        adopt_switched_session(
            &mut app,
            &SessionId::new("before"),
            &SessionId::new("after"),
            false,
        );
        assert!(
            views.lock().unwrap().is_empty(),
            "a window that is not showing moved the next start's screen: {:?}",
            views.lock().unwrap()
        );
    }

    /// **変わっていない周期では何も保存しない。** 追従は 2 秒ごとのポーリングで
    /// 見るので、毎周期書くと `~/.ccdesk/state.json` のロックを無駄に取り合う
    #[test]
    fn a_pane_that_did_not_switch_saves_nothing() {
        let views = Arc::new(Mutex::new(Vec::new()));
        let mut app = App {
            sessions: vec![session_row("same", "C:\\dev\\api", 1)],
            source: Arc::new(TestSource::for_views(views.clone())),
            // その pid は起動時と同じセッションを動かしている
            agents: vec![AgentInfo {
                session_id: "same".to_string(),
                kind: "interactive".to_string(),
                pid: Some(4242),
                ..AgentInfo::default()
            }],
            ..test_app(34, TERM)
        };
        // 窓が無い（PTY を起こさない）ので、検出の入口が空回りすることも同時に見る
        follow_session_switches(&mut app);
        assert!(
            views.lock().unwrap().is_empty(),
            "saved the next start's screen without a switch"
        );
        assert_eq!(app.sessions.len(), 1, "a row appeared without a switch");
        // 「切り替わっていない」の判定そのもの（窓が指す ID と pid の現在の ID が同じ）
        assert_eq!(
            live_session_of(Some(4242), 0, &app.hook_states, &app.agents),
            Some(SessionId::new("same")),
            "the pid no longer resolves to the session it was started with"
        );
    }

    /// **セッションが残っているプロジェクトは登録解除できない。** 一覧は
    /// 「登録リスト ∪ セッションの cwd」なので、外しても見出しは出続ける ＝
    /// 押せるのに何も変わらないことになる。stop と同じ実行可能フラグで落とす
    #[test]
    fn project_menu_disables_remove_while_sessions_remain() {
        assert_eq!(
            entry_pairs(&project("C:\\dev\\api", false), Grouping::Directory),
            [
                ("new session".to_string(), true),
                ("remove project".to_string(), true),
            ]
        );
        assert_eq!(
            entry_pairs(&project("C:\\dev\\api", true), Grouping::Directory),
            [
                ("new session".to_string(), true),
                ("remove project".to_string(), false),
            ],
            "remove project must not be selectable while sessions remain"
        );
    }

    /// 無効な項目は Enter でも実行されない（登録が残る）
    #[test]
    fn picking_a_disabled_remove_project_does_nothing() {
        let mut app = test_app(34, TERM);
        app.projects = vec!["C:\\dev\\api".to_string()];
        open(&mut app, project("C:\\dev\\api", true), 5);
        handle_popup_key(&mut app, KeyCode::Down); // remove project を選ぶ
        handle_popup_key(&mut app, KeyCode::Enter);
        assert_eq!(
            app.projects,
            ["C:\\dev\\api"],
            "a disabled remove project ran anyway"
        );
    }

    /// 見出し行クリックでそのフォルダのメニューが開く（`+` を押して即起動ではない）。
    /// **フォーカスはサイドバーに残る**（開いたメニューがキーを受ける）
    #[test]
    fn clicking_a_project_heading_opens_its_menu() {
        let mut app = test_app(34, TERM);
        app.sidebar_rows = vec![SidebarRow::Action(RowAction::Project("C:\\dev\\api".to_string()))];
        app.sidebar_header_rows = 1;
        handle_mouse(&mut app, &click(5, 1)).unwrap();
        let popup = app.popup.as_ref().expect("no menu opened");
        assert_eq!(popup.kind, project("C:\\dev\\api", false));
        assert_eq!(popup.anchor_y, 1, "the menu is not anchored below the clicked row");
        assert!(app.focus == Focus::Sidebar, "focus moved to the right pane");
        assert!(app.sessions.is_empty(), "the click started a session");
    }

    /// メニューを開く時点でセッションの有無を写す。**同名の末端ディレクトリが別パスに
    /// 2 つあっても、判定は開いた行のフルパスで行う**（片方にだけセッションがある状況で
    /// 取り違えると、消せるはずの登録が消せなくなる）
    #[test]
    fn opening_a_project_menu_reads_sessions_for_that_exact_folder() {
        let mut app = test_app(34, TERM);
        app.sessions = vec![session_row("s1", "C:\\work\\api", 1)];
        // セッションを持つ側
        open_project_popup(&mut app, "C:\\work\\api".to_string(), 3);
        assert_eq!(app.popup.as_ref().unwrap().kind, project("C:\\work\\api", true));
        // 末端名が同じでも別パスならセッション無し扱い
        open_project_popup(&mut app, "C:\\dev\\api".to_string(), 3);
        assert_eq!(
            app.popup.as_ref().unwrap().kind,
            project("C:\\dev\\api", false),
            "picked up sessions from another folder with the same leaf name"
        );
        // 大小・末尾の区切り違いは同じフォルダとして拾う
        open_project_popup(&mut app, "c:\\work\\api\\".to_string(), 3);
        assert_eq!(
            app.popup.as_ref().unwrap().kind,
            project("c:\\work\\api\\", true),
            "missed a session whose path differs only in case"
        );
    }

    /// 登録は自動（明示的な追加 UI は無い）。重複・大小違い・末尾の区切り違いは
    /// 同じフォルダなので増えない
    #[test]
    fn registering_a_project_is_idempotent_per_folder() {
        let mut app = test_app(34, TERM);
        register_project(&mut app, "C:\\dev\\api");
        register_project(&mut app, "C:\\dev\\api");
        register_project(&mut app, "c:\\dev\\api\\");
        assert_eq!(app.projects, ["C:\\dev\\api"]);
        // 別フォルダは末尾に積む（並びは登録順。表示の並びは見出し側が決める）
        register_project(&mut app, "C:\\dev\\web");
        assert_eq!(app.projects, ["C:\\dev\\api", "C:\\dev\\web"]);
        // 空文字は登録しない（cwd が取れなかった経路で空の見出しを作らない）
        register_project(&mut app, "");
        assert_eq!(app.projects, ["C:\\dev\\api", "C:\\dev\\web"]);
    }

    /// 上限を超えたら古い側から落とす（登録が自動なので放っておくと際限なく積まれる）。
    /// **上限の適用は保存の正本（`merge_projects`）だけ**なので、保存を通る
    /// 供給元で検査する（`register_project` 自身は上限を知らない）
    #[test]
    fn registering_beyond_the_limit_drops_the_oldest() {
        let mut app = app_with_disk(&[], &[]);
        for i in 0..PROJECTS_LIMIT + 1 {
            register_project(&mut app, &format!("C:\\dev\\p{i}"));
        }
        assert_eq!(app.projects.len(), PROJECTS_LIMIT, "stacked beyond the limit");
        assert_eq!(
            app.projects.first().map(String::as_str),
            Some("C:\\dev\\p1"),
            "the oldest registration was not dropped"
        );
        assert_eq!(
            app.projects.last().map(String::as_str),
            Some(format!("C:\\dev\\p{PROJECTS_LIMIT}").as_str()),
            "the newest registration is missing"
        );
    }

    /// **使い直したフォルダは追い出されない（最近使った順で残す）。** 上限まで
    /// 埋まった状態で毎日使っているフォルダが、次の 1 件で落ちてはいけない
    /// （登録が消え、最後のセッションを消した時点で見出し＝入口まで消える）
    #[test]
    fn reusing_a_folder_keeps_it_when_the_limit_evicts() {
        let mut app = app_with_disk(&[], &[]);
        for i in 0..PROJECTS_LIMIT {
            register_project(&mut app, &format!("C:\\dev\\p{i}"));
        }
        // 最初に登録したフォルダを使い直す ＝ 最近使った側へ動く
        register_project(&mut app, "C:\\dev\\p0");
        register_project(&mut app, "C:\\dev\\new");
        assert_eq!(app.projects.len(), PROJECTS_LIMIT, "stacked beyond the limit");
        assert!(
            app.projects.iter().any(|p| p == "C:\\dev\\p0"),
            "a folder that was reused got evicted"
        );
        // 代わりに落ちるのは「最も長く使っていない」p1（p0 は使い直した時点で末尾へ動く）
        assert!(
            !app.projects.iter().any(|p| p == "C:\\dev\\p1"),
            "the least recently used registration survived the eviction"
        );
        assert_eq!(
            app.projects.last().map(String::as_str),
            Some("C:\\dev\\new"),
            "the most recently used folder is not last"
        );
    }

    /// ディスクに他インスタンスの登録が居る App。自分の一覧は起動時の読み込みと
    /// 同じく「そのとき読んだディスクの内容」＝ 基準と揃えておく。
    /// 保存は live と同じ [`persist_projects`] を通る
    /// （[`ProjectsBackend::MemoryDisk`]）
    fn app_with_disk(mine: &[&str], from_other: &[&str]) -> App {
        let mine: Vec<String> = mine.iter().map(|p| p.to_string()).collect();
        let mut disk = mine.clone();
        disk.extend(from_other.iter().map(|p| p.to_string()));
        App {
            projects: mine.clone(),
            source: Arc::new(TestSource::for_projects(ProjectsBackend::MemoryDisk {
                disk: Mutex::new(disk),
                baseline: Mutex::new(mine),
            })),
            ..test_app(34, TERM)
        }
    }

    /// ディスクに載った一覧（テストの検査用に読み直す）
    fn disk_projects(app: &App) -> Vec<String> {
        app.source.window_state().projects
    }

    /// **保存された一覧をそのまま自分の一覧にする（画面とディスクをずらさない）。**
    /// 上限まで埋まった状態で他インスタンスの登録がディスクに居ると、マージ後に
    /// 上限がかかるので**保存された内容は渡した一覧と違う**。取り込まないと、
    /// 上限で落ちた自分の最古の登録が画面には出続けて再起動で消え、しかも
    /// 保存するたび同じことが起き続ける（他インスタンスの登録も一覧に入らない）
    #[test]
    fn saving_projects_takes_up_what_was_actually_persisted() {
        let mine: Vec<String> = (0..PROJECTS_LIMIT).map(|i| format!("C:\\dev\\p{i}")).collect();
        let mine_refs: Vec<&str> = mine.iter().map(String::as_str).collect();
        let mut app = app_with_disk(&mine_refs, &["C:\\dev\\from-b"]);
        // 登録済みフォルダの使い直し ＝ 自分の一覧の件数は変わらないまま保存が走る
        register_project(&mut app, "C:\\dev\\p3");
        assert_eq!(
            app.projects,
            disk_projects(&app),
            "the persisted list was not taken up — the screen and the disk disagree"
        );
        assert!(
            app.projects.iter().any(|p| p == "C:\\dev\\from-b"),
            "a registration from another instance is missing from our list"
        );
    }

    /// **他インスタンスの登録が「恒久的に最近使った」扱いにならない（LRU の逆転を防ぐ）。**
    /// 取り込まないと、その登録は毎回マージで末尾（＝最後に使った位置）へ足し直されるので、
    /// 自分の本当に新しい登録より後に追い出される。取り込めば自分の一覧の一員として
    /// 普通に古くなる
    #[test]
    fn a_registration_from_another_instance_does_not_stay_the_most_recent() {
        let mut app = app_with_disk(&["C:\\dev\\mine"], &["C:\\dev\\from-b"]);
        register_project(&mut app, "C:\\dev\\mine"); // 1 度目の保存で取り込む
        register_project(&mut app, "C:\\dev\\fresh"); // その後で自分が新しく登録
        assert_eq!(
            disk_projects(&app).last().map(String::as_str),
            Some("C:\\dev\\fresh"),
            "a registration from another instance was re-appended as the most recent"
        );
        assert_eq!(
            app.projects,
            ["C:\\dev\\mine", "C:\\dev\\from-b", "C:\\dev\\fresh"],
            "the taken-up registrations are not in least-to-most-recently-used order"
        );
    }

    /// 登録解除は対象フォルダだけを外す。**同名の末端が別パスにあっても取り違えない**
    #[test]
    fn removing_a_project_only_drops_that_folder() {
        let mut app = test_app(34, TERM);
        app.projects = vec![
            "C:\\dev\\api".to_string(),
            "C:\\work\\api".to_string(),
            "C:\\dev\\web".to_string(),
        ];
        remove_project(&mut app, "C:\\work\\api");
        assert_eq!(app.projects, ["C:\\dev\\api", "C:\\dev\\web"]);
        // 大小・末尾の区切り違いでも同じフォルダとして外れる
        remove_project(&mut app, "c:\\dev\\API\\");
        assert_eq!(app.projects, ["C:\\dev\\web"]);
        // 登録に無いフォルダを外しても何も起きない
        remove_project(&mut app, "C:\\nope");
        assert_eq!(app.projects, ["C:\\dev\\web"]);
    }

    /// メニューの remove project が、開いた行のフォルダに効く
    #[test]
    fn the_remove_project_row_unregisters_the_folder_it_was_opened_for() {
        let mut app = test_app(34, TERM);
        app.projects = vec!["C:\\dev\\api".to_string(), "C:\\work\\api".to_string()];
        open(&mut app, project("C:\\work\\api", false), 5);
        handle_popup_key(&mut app, KeyCode::Down);
        handle_popup_key(&mut app, KeyCode::Enter);
        assert!(app.popup.is_none(), "the menu is still open after the action ran");
        assert_eq!(app.projects, ["C:\\dev\\api"]);
    }

    /// **自動登録の経路 1: 新規セッション画面の起動。** 供給元が撮影用データなので
    /// 本物の claude は起きず、フォルダの登録と初期値の更新だけが観測できる
    #[test]
    fn launching_from_the_new_session_view_registers_its_folder() {
        let dir = std::env::temp_dir().to_string_lossy().to_string();
        let mut app = test_app(34, TERM);
        app.right_view = RightView::New(NewState::browse(&dir));
        start_new_session(&mut app).unwrap();
        assert_eq!(app.projects, std::slice::from_ref(&dir), "the launched folder was not registered");
        assert_eq!(app.dispatch_cwd, dir);
        assert!(app.sessions.is_empty(), "the test source started a real claude");
    }

    /// **自動登録の経路 2: 見出しメニューの new session。** 経路 1 と同じ
    /// `dispatch_session` に収束するので、登録の知識は 1 箇所で足りている
    #[test]
    fn picking_new_session_from_the_project_menu_registers_its_folder() {
        let mut app = test_app(34, TERM);
        open(&mut app, project("C:\\dev\\api", false), 5);
        handle_popup_key(&mut app, KeyCode::Enter); // 先頭 = new session
        assert!(app.popup.is_none(), "the menu is still open after the action ran");
        assert_eq!(app.projects, ["C:\\dev\\api"]);
        assert_eq!(app.dispatch_cwd, "C:\\dev\\api");
    }

    /// **見出しメニューの new session はフォーカスを端末へ移す。** 起動したのに
    /// キーがサイドバーへ行き続けると、↑↓ で選択だけが動く ＝
    /// クリックか Alt+→ を押すまでタイプできない。窓を出す側
    /// （[`App::show_session`]）はフォーカスを動かさない契約なので、
    /// ディスパッチの時点で移す（New 画面の起動ボタンと同じ挙動）
    #[test]
    fn picking_new_session_from_the_project_menu_moves_focus_to_the_terminal() {
        let mut app = test_app(34, TERM);
        open(&mut app, project("C:\\dev\\api", false), 5);
        handle_popup_key(&mut app, KeyCode::Enter); // 先頭 = new session
        assert!(
            app.focus == Focus::Terminal,
            "keys still go to the sidebar after a launch"
        );
    }

    /// **子が端末を掴むまでに打った文字は、どのセッションへも届かない。**
    /// フォーカスは端末へ移っているが claude はまだ raw mode に入っていないので、
    /// 素通しすると打った文字が読み捨てられたりエコーに混ざったりする。
    /// 捨てたことは下部バーで伝える（無反応に見せない）
    #[test]
    fn input_typed_while_a_session_is_starting_reaches_no_session() {
        let mut app = test_app(34, TERM);
        open(&mut app, project("C:\\dev\\api", false), 5);
        handle_popup_key(&mut app, KeyCode::Enter); // 先頭 = new session
        // 撮影用の供給元は実際に claude を起こさないので、起動直後の状態を作る
        app.input_gate = Some(std::time::Instant::now());
        assert!(
            drop_input_while_starting(&mut app),
            "input typed while a session is starting flows straight to the PTY"
        );
        assert!(app.notice.is_some(), "dropping the input was not reported");
        // 子が端末を掴めば素通しに戻る（門番が残り続けるとタイプできない画面になる）
        app.input_gate = None;
        assert!(
            !drop_input_while_starting(&mut app),
            "input is still dropped after the launch finished"
        );
    }

    /// **起動の反映で門番は降りる。** 降ろす場所を live と撮影用の合流点
    /// （[`apply_launch`]）に置いてあることの固定で、ここが漏れると
    /// セッションを起こさない供給元でタイプできない画面になる
    #[test]
    fn a_finished_launch_lifts_the_input_gate() {
        let mut app = test_app(34, TERM);
        app.input_gate = Some(std::time::Instant::now());
        // 起動を試していない（撮影用の供給元）＝ 待つものが無いので門番は降りる
        apply_launch(&mut app, "C:\\dev\\api".to_string(), Ok(None));
        assert!(
            !drop_input_while_starting(&mut app),
            "input is dropped even though the launch is done"
        );
    }

    /// **応答しない起動から入力が有界時間で戻る。**
    ///
    /// 門番が降りる合図は「子が最初の出力を出した」なので、子が端末を掴まないまま
    /// （起動直後の認証プロンプト・AV スキャン・長い `-r` の読み込み）だと
    /// **既存の全セッションへのタイプがその間ずっと死ぬ**。
    /// 期限（[`INPUT_GATE_LIMIT`]）を超えたら門番を降ろす。
    ///
    /// **降りた後も、打った文字は直前まで見ていたセッションへ流れない**:
    /// フォーカスがサイドバーへ戻るので、キーはサイドバー操作として処理される
    /// （キーが PTY へ行くのは `focus == Terminal` のときだけ）。
    /// 打ち先はユーザーが選び直す
    #[test]
    fn a_hung_launch_gives_input_back_within_a_bounded_time() {
        let mut app = test_app(34, TERM);
        // 子がまだ何も出力していない状態（起動直後）
        app.input_gate = Some(std::time::Instant::now());
        app.set_focus(Focus::Terminal); // dispatch_session と同じ状態

        // 期限内は門番が効いている（有界 ＝ 即座に降りる、ではない）
        assert!(!expire_input_gate(&mut app), "the gate lifted before its deadline");
        assert!(
            drop_input_while_starting(&mut app),
            "input typed while a session is starting flows straight to the PTY"
        );

        // 期限ぶん前に起こした状態（時刻を注入して待たずに検査する）
        app.input_gate = Some(instant_ago(INPUT_GATE_LIMIT));
        assert!(expire_input_gate(&mut app), "the gate does not lift past its deadline");
        assert!(
            !drop_input_while_starting(&mut app),
            "a hung launch kills input forever"
        );
        assert!(
            app.focus == Focus::Sidebar,
            "the gate lifted without moving focus back — typing would flow to the previous session"
        );
        let (msg, _) = app.notice.as_ref().expect("the hang was not reported");
        assert!(
            msg.contains("not responding"),
            "the notice does not say what happened: {msg:?}"
        );
        // 2 度目は何もしない（毎周通知を出し直さない）
        assert!(!expire_input_gate(&mut app), "an already lifted gate was lifted again");
    }

    /// **起動に失敗したときも、打った文字は直前まで見ていたセッションへ届かない。**
    ///
    /// 起動が失敗すると窓は増えない ＝ `right_view` は直前まで見ていたセッションを
    /// 指したままで、[`dispatch_session`] が移した `Focus::Terminal` も戻らない。
    /// そのまま打鍵すると稼働中の別プロジェクトのセッションへプロンプトが送られる。
    /// 応答しない経路（[`expire_input_gate`]）と同じ扱いに揃えるのが要点で、
    /// 判断は [`lift_input_gate`] 1 箇所に置いてある
    #[test]
    fn a_failed_launch_gives_input_back_to_the_sidebar() {
        let mut app = test_app(34, TERM);
        app.input_gate = Some(std::time::Instant::now());
        app.set_focus(Focus::Terminal); // dispatch_session と同じ状態
        apply_launch(
            &mut app,
            "C:\\dev\\api".to_string(),
            Err("session launch failed".to_string()),
        );
        assert!(
            !drop_input_while_starting(&mut app),
            "input is still dropped after the launch failed"
        );
        assert!(
            app.focus == Focus::Sidebar,
            "the gate lifted without moving focus back — typing would flow to the previous session"
        );
    }

    /// **宛先が居ないまま門番を降ろすと打ち先も戻る。** 起動には成功したのに
    /// その窓が表示されていない（切替と削除が競合した等）状態でも、
    /// 素通しに戻ると打った文字が別のセッションへ流れる。
    ///
    /// [`apply_launch`] 越しには書けない: 本物の子プロセス生成が要るため
    /// （portable-pty は存在しない cwd を `USERPROFILE` に差し替えて**成功させる**
    /// ＝ テストが本物の claude を起こしてしまう）。代わりに、その後の状態
    /// （宛先の窓が開いていない）をそのまま作って判断だけを検査する
    #[test]
    fn lifting_the_gate_without_its_destination_gives_input_back_to_the_sidebar() {
        let mut app = test_app(34, TERM);
        let id = SessionId::new("abc123");
        app.input_gate = Some(std::time::Instant::now());
        app.set_focus(Focus::Terminal);
        assert!(
            !app.showing(&id),
            "the destination window is open — the premise of this test broke"
        );
        lift_input_gate(&mut app, Some(&id));
        assert!(
            !drop_input_while_starting(&mut app),
            "input is still dropped after the gate lifted"
        );
        assert!(
            app.focus == Focus::Sidebar,
            "the gate lifted but focus stayed on the terminal"
        );
    }

    /// **起動に失敗したフォルダは登録しない。** 通知が失敗を報告しているのに見出しが
    /// 生えると、打ち間違い・権限の無いフォルダ・古いネットワークパスが state.json に
    /// 永久に残る（new session 画面の初期値と同じ判断に揃える ＝ 同じ操作に対する
    /// 2 つの永続化が別の答えを出さない）
    #[test]
    fn a_failed_launch_registers_no_folder() {
        let mut app = test_app(34, TERM);
        apply_launch(
            &mut app,
            "C:\\dev\\api".to_string(),
            Err("session launch failed".to_string()),
        );
        assert!(app.projects.is_empty(), "a folder whose launch failed was registered");
        assert!(app.notice.is_some(), "the failure was not reported");
        // 成否の判定は Result 1 つ。`Ok(None)` は「起動を試していない」＝ 失敗では
        // ないので記録する（セッションを起こさない撮影用の供給元がこの形）
        apply_launch(&mut app, "C:\\dev\\web".to_string(), Ok(None));
        assert_eq!(
            app.projects,
            ["C:\\dev\\web"],
            "applying the launch result registers nothing — the decision still happens before the launch"
        );
    }

    /// **描画とクリック判定が同じ矩形を見ていることの検証。** サイドバーを最小幅まで
    /// 詰めるとメニューは右ペインに被る（[`crate::ui::popup_rect`] の意図）。描画順が
    /// 右ペインより前だと被った列が塗り潰され、**見た目は claude の画面なのに
    /// クリックすると new session が走る**状態になるので、
    /// 「クリックが当たる列 = メニューが塗った列」を実描画で確かめる
    #[test]
    fn a_click_on_a_menu_column_over_the_right_pane_hits_what_is_drawn() {
        let mut app = test_app(MIN_SIDEBAR, (60, 20));
        open(&mut app, project("C:\\dev\\api", false), 1);
        let rect = popup_rect(&app, app.popup.as_ref().unwrap());
        // 前提: メニューがサイドバーを越えている（越えていなければこのテストは無意味）
        assert!(rect.right() > MIN_SIDEBAR, "{rect:?}");
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 20)).unwrap();
        terminal
            .draw(|frame| {
                draw(frame, &mut app);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        // 先頭項目の行は右ペインへ食い込む列まで全部メニューが塗っている
        let drawn: String = (rect.x..rect.right())
            .map(|x| buffer[(x, rect.y + 1)].symbol())
            .collect();
        assert!(
            drawn.contains("new session"),
            "the label is chopped by the right pane: {drawn:?}"
        );
        // そのはみ出した列のクリックが、描かれている項目どおりに効く
        let col = rect.right() - 2;
        assert!(col >= MIN_SIDEBAR, "this column is not past the sidebar: {col}");
        handle_mouse(&mut app, &click(col, rect.y + 1)).unwrap();
        assert_eq!(app.dispatch_cwd, "C:\\dev\\api", "the item that was drawn did not run");
    }

    /// 登録・解除は撮影用の供給元では永続化されない ＝ **テストが開発者の
    /// ~/.ccdesk/state.json を書き換えない**（保存経路を足したときの事故を止める）
    #[test]
    fn registering_through_the_test_app_never_touches_the_real_state_file() {
        let before = ccdesk::load_state_list("projects");
        let mut app = test_app(34, TERM);
        register_project(&mut app, "C:\\must-not-be-persisted");
        remove_project(&mut app, "C:\\must-not-be-persisted");
        assert_eq!(
            ccdesk::load_state_list("projects"),
            before,
            "the test rewrote the real user's state.json"
        );
    }

    /// **既存ユーザー救済: 起動時に既存セッションの cwd を登録へ埋め戻す。**
    /// 埋め戻しが無いと、ccdesk から新しくセッションを立てるまで登録は空のままで、
    /// 最後のセッションを消した時点で見出し（＝入口）が消える
    #[test]
    fn startup_backfills_projects_from_existing_sessions() {
        let mut app = test_app(34, TERM);
        app.projects = vec!["C:\\dev\\registered".to_string()];
        // 一覧の並びは保存順で新旧を表さない。新しさは `updated_at` が持つ
        app.sessions = vec![
            session_row("s5", "C:\\dev\\old", 10),
            session_row("s1", "C:\\dev\\api", 50),
            // 同じフォルダ（大小・末尾の区切り違い）は 1 件だけ
            session_row("s2", "c:\\dev\\api\\", 40),
            // 既に登録済みのフォルダは増えない
            session_row("s3", "C:\\dev\\registered", 30),
            // cwd の取れなかった行から空の見出しを作らない
            session_row("s4", "", 20),
        ];
        backfill_projects(&mut app);
        // 既存の登録はそのまま。埋め戻しは古い側から積むので、末尾 = 最近使ったフォルダ
        assert_eq!(
            app.projects,
            ["C:\\dev\\registered", "C:\\dev\\old", "C:\\dev\\api"]
        );
    }

    /// **埋め戻しは既存の登録を押し出さない。** 上限を超える数の既存セッションが
    /// あっても、ユーザーの登録（唯一の記録）は落とさず、入る分だけを足す
    #[test]
    fn backfilling_never_evicts_registered_projects() {
        let mut app = test_app(34, TERM);
        app.projects = (0..PROJECTS_LIMIT)
            .map(|i| format!("C:\\dev\\p{i}"))
            .collect();
        app.sessions = (0..5)
            .map(|i| session_row(&format!("s{i}"), &format!("C:\\sessions\\j{i}"), i as u64))
            .collect();
        backfill_projects(&mut app);
        assert_eq!(app.projects.len(), PROJECTS_LIMIT);
        assert!(
            app.projects.iter().all(|p| p.starts_with("C:\\dev\\p")),
            "backfilling evicted a registered folder: {:?}",
            app.projects
        );
    }

    /// 既存セッションが上限を超えるときは**最近更新した行のフォルダを優先する**
    /// （最近使ったものが残るという登録の並びと同じ規則）
    #[test]
    fn backfilling_prefers_the_newest_sessions_when_they_exceed_the_limit() {
        let mut app = test_app(34, TERM);
        // `updated_at` は j0 が最新（新しい順に並べ替えられることを見る）
        let total = PROJECTS_LIMIT + 5;
        app.sessions = (0..total)
            .map(|i| {
                session_row(
                    &format!("s{i}"),
                    &format!("C:\\dev\\j{i}"),
                    (total - i) as u64,
                )
            })
            .collect();
        backfill_projects(&mut app);
        assert_eq!(app.projects.len(), PROJECTS_LIMIT, "stacked beyond the limit");
        assert_eq!(
            app.projects.last().map(String::as_str),
            Some("C:\\dev\\j0"),
            "the newest session's folder is not on the most recently used end"
        );
        assert!(
            !app.projects.iter().any(|p| p == &format!("C:\\dev\\j{PROJECTS_LIMIT}")),
            "a folder from a session past the limit got in"
        );
    }

    /// **開いている窓の行は一覧の読み直しで消えない。** 読み直しは丸ごとの
    /// 置き換えなので、保存がまだディスクへ載っていない（ロックが取れなかった）
    /// 間に読むとその行だけが落ち、**プロセスは動いているのにサイドバーの
    /// どこからも指せない**状態になる。
    ///
    /// 戻すのは窓が開いている行だけ ＝ 他インスタンスの `close` は普通に効く
    /// （窓を持たない行を戻すと、削除がどちらのインスタンスからも効かなくなる）
    #[test]
    fn refreshing_the_list_keeps_the_rows_of_open_windows() {
        let mine = [
            session_row("open", "C:\\dev\\api", 1),
            session_row("closed", "C:\\dev\\web", 1),
        ];
        let open = [SessionId::new("open")];

        // ディスクにまだ載っていない（＝ 保存が失敗した直後の読み直し）
        assert_eq!(
            rows_dropped_while_open(&[], &mine, &open),
            [&mine[0]],
            "a row whose window is open was dropped"
        );
        // 窓を持たない行は戻さない（他インスタンスの削除を復活させない）
        assert!(
            !rows_dropped_while_open(&[], &mine, &open)
                .iter()
                .any(|row| row.session_id.as_str() == "closed")
        );
        // ディスクに載っていれば戻すものは無い（通常の読み直し）
        assert!(rows_dropped_while_open(&mine, &mine, &open).is_empty());
    }

    /// hook が書いた state を持つ App（窓は開いていない）。**供給元と写しの両方に
    /// 同じものを入れる**ので、読み直し（[`adopt_hook_states`]）を通さずに引ける
    fn app_with_hooks(rows: &[SessionRow], hooks: HookStates) -> App {
        App {
            sessions: rows.to_vec(),
            hook_states: hooks.clone(),
            source: Arc::new(TestSource::for_hooks(hooks)),
            ..Default::default()
        }
    }

    fn row_of<'a>(app: &'a App, id: &str) -> &'a SessionRow {
        app.sessions
            .iter()
            .find(|r| r.session_id.as_str() == id)
            .expect("the row is gone")
    }

    /// **hook を読み直しても行には何も書かない。**
    ///
    /// 写していた頃の実データでは、保管と hook が食い違ったうえ**どちらが新しいかが
    /// 行ごとに逆**だった（保管 `blocked` / hook `stopped` 11:14:06 と、
    /// 保管 `stopped` / hook `blocked` 11:13:43）。前者は ccdesk が異常終了して
    /// 記録が止まった行、後者は窓を閉じるときに書き戻した行で、**どちらも
    /// 「保存する場所がある」ことが原因**。書かなくなった今は残骸の hook が
    /// 何を言っていても行は動かない
    #[test]
    fn refreshing_hook_states_writes_nothing_to_the_rows() {
        let rows = [session_row("s", "C:\\dev\\api", 1)];
        // 写しは空のまま（供給元だけが記録を持つ ＝ 読み直しで初めて届く）
        let mut app = App {
            sessions: rows.to_vec(),
            source: Arc::new(TestSource::for_hooks(HookStates::from_entries([(
                "s", "blocked", 9_999,
            )]))),
            ..Default::default()
        };
        adopt_hook_states(&mut app);
        assert_eq!(app.sessions, rows, "a leftover hook wrote to the row");
        // 写しそのものは取り直されている（表示はここから導く）
        assert_eq!(app.hook_states.get(&SessionId::new("s"), Some(0)), Some("blocked"));
    }

    /// **未読は「claude が何か言ったのが、最後に開いた後か」。**
    ///
    /// 材料は hook の `at` だけなので、次の 2 つは**構造的に**起きない:
    /// ユーザー自身の操作で未読が生えること（行を書き換えても hook は動かない）と、
    /// ccdesk を起動し直しただけで未読になること（`last_opened_at` は保管される）
    #[test]
    fn a_row_is_unread_only_when_claude_spoke_after_it_was_opened() {
        let mut row = session_row("s", "C:\\dev\\api", 1_000);
        row.last_opened_at = 1_000;
        let unread = |at| HookStates::from_entries([("s", "done", at)]).unread(&row);
        assert!(!HookStates::default().unread(&row), "a row with no hook record is unread");
        assert!(!unread(1_000), "a hook from before the row was opened marks it unread");
        assert!(unread(1_001), "claude spoke after the row was opened but it stayed read");

        // 自分の操作（ピン留め）は行を書き換えるが未読の材料を動かさない
        let hooks = HookStates::from_entries([("s", "done", 999)]);
        let mut app = App {
            sessions: vec![row.clone()],
            hook_states: hooks,
            ..Default::default()
        };
        edit_row(&mut app, &SessionId::new("s"), |row| row.pinned = true);
        assert!(only_row(&app).pinned, "the premise (an edited row) broke");
        assert!(
            !app.hook_states.unread(only_row(&app)),
            "our own edit created an unread mark"
        );
    }

    /// **ペインを開いた時点で既読になる**（開き方は問わない ＝ [`mark_read`] の
    /// 1 箇所で済ませてある。`mark as read` と、ペインに出ている行へ hook が届いた
    /// ときも同じ関数）。消えた行を指しても何も起きない
    #[test]
    fn opening_a_pane_marks_the_row_read() {
        let rows = [session_row("s", "C:\\dev\\api", 1)];
        let mut app = app_with_hooks(&rows, HookStates::from_entries([("s", "done", 2)]));
        assert!(app.hook_states.unread(row_of(&app, "s")), "the premise (an unread row) broke");

        mark_read(&mut app, &SessionId::new("s"));
        let row = row_of(&app, "s");
        assert!(!app.hook_states.unread(row), "still unread after being opened");
        // 既読にしても行の姿は変わらない ＝ 経過時間が 0s へ戻らない
        assert_eq!(row.updated_at, 1, "marking as read reset the age");

        mark_read(&mut app, &SessionId::new("gone-row"));
        assert_eq!(app.sessions.len(), 1, "an unknown row changed the list");
    }

    /// Enter でメニューを開く行の画面 y。固定ヘッダーはスクロールに動かされず、
    /// その下はスクロール分だけ引く（矩形はこの 1 つ下に出る）
    #[test]
    fn selected_row_y_corrects_for_scroll_below_the_fixed_header() {
        let mut app = test_app(34, TERM);
        app.sidebar_header_rows = 7;
        app.sidebar_scroll = 4;
        // ヘッダー内はスクロールの影響を受けない
        app.selection = SidebarPos::Row(2);
        assert_eq!(selected_row_y(&app), 3);
        // ヘッダーより下はスクロール分を引く
        app.selection = SidebarPos::Row(10);
        assert_eq!(selected_row_y(&app), 7);
        // 引きすぎても 0 未満にならない
        app.sidebar_scroll = 99;
        assert_eq!(selected_row_y(&app), 1);
    }

    // ── 二次操作（ポップアップの項目）と、撤去したショートカット ──────────

    /// 行 1 本だけを持ち、その行を選択しているサイドバー
    fn app_with_row(id: &str) -> App {
        let mut app = test_app(34, TERM);
        app.sessions = vec![session_row(id, "C:\\dev\\api", 1)];
        app.sidebar_rows = vec![SidebarRow::Action(RowAction::Open(SessionId::new(id)))];
        app.sidebar_header_rows = 1;
        app
    }

    /// サイドバーへの打鍵（本番と同じ [`handle_sidebar_key`] を通す）
    fn press(app: &mut App, code: KeyCode) {
        handle_sidebar_key(app, &KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn only_row(app: &App) -> &SessionRow {
        app.sessions.first().expect("the row is gone")
    }

    /// ピン留めは入切する（同じ項目をもう一度選べば戻る）
    #[test]
    fn the_menu_toggles_pin_on_the_row() {
        let mut app = app_with_row("s");
        let id = SessionId::new("s");
        assert!(!only_row(&app).pinned, "the flag is already set to begin with");

        run_popup_action(&mut app, PopupAction::TogglePin(id.clone()));
        assert!(only_row(&app).pinned, "the first pick does not set the flag");

        run_popup_action(&mut app, PopupAction::TogglePin(id));
        assert!(!only_row(&app).pinned, "the second pick does not clear the flag");
    }

    /// **メニューの操作は未読を作らない・消さない**（消せるのは `mark as read` だけ）。
    /// 未読の材料が hook しか無いので、行を書き換えるメニュー操作は `●` に触れない
    #[test]
    fn row_edits_leave_unread_alone_and_only_mark_as_read_clears_it() {
        let mut app = app_with_row("s");
        let id = SessionId::new("s");
        // 未読の行（claude が何か言ったのに、まだ開いていない）
        app.sessions[0].last_opened_at = 1_000;
        app.hook_states = HookStates::from_entries([("s", "done", 2_000)]);
        assert!(app.hook_states.unread(only_row(&app)), "the premise (an unread row) broke");

        run_popup_action(&mut app, PopupAction::TogglePin(id.clone()));
        assert!(app.hook_states.unread(only_row(&app)), "pinning cleared unread");

        run_popup_action(&mut app, PopupAction::MarkRead(id.clone()));
        assert!(!app.hook_states.unread(only_row(&app)), "still unread after mark as read");

        // 既読の行を触っても未読は生えない（`updated_at` はマージのために進む）
        let before = only_row(&app).updated_at;
        run_popup_action(&mut app, PopupAction::TogglePin(id));
        assert!(
            !app.hook_states.unread(only_row(&app)),
            "our own edit created an unread mark"
        );
        assert!(
            only_row(&app).updated_at >= before,
            "the last-write-wins input for merging did not advance"
        );
    }

    /// **何も変えない操作は `updated_at` を動かさない。**
    ///
    /// `updated_at` は行に出る経過時間の材料でもあるので
    /// （[`crate::hooks::HookStates::changed_at`]）、中身が変わっていないのに進めると
    /// **行に何も起きていないのに `· 0s` へ戻る**。`mark as read` は姿を変えないので
    /// 進めず、ピン留めは変えるので進む
    #[test]
    fn an_edit_that_changes_nothing_leaves_the_age_alone() {
        let mut app = app_with_row("s");
        let id = SessionId::new("s");
        app.sessions[0].last_opened_at = 1_000;
        app.sessions[0].updated_at = 2_000;
        app.hook_states = HookStates::from_entries([("s", "done", 2_000)]);

        // 未読の行への `mark as read`: 既読にはなるが行の姿は変わっていない
        run_popup_action(&mut app, PopupAction::MarkRead(id.clone()));
        assert_eq!(only_row(&app).updated_at, 2_000, "mark as read reset the age");
        assert!(
            !app.hook_states.unread(only_row(&app)),
            "mark as read did not clear unread"
        );

        // もう一度押しても何も動かない
        run_popup_action(&mut app, PopupAction::MarkRead(id.clone()));
        assert_eq!(only_row(&app).updated_at, 2_000, "a second mark as read reset the age");

        // 中身が変わる操作は進める（マージの後勝ち判定の材料なので必ず進む）
        run_popup_action(&mut app, PopupAction::TogglePin(id));
        assert!(only_row(&app).updated_at > 2_000, "a real change did not advance the age");
    }

    /// **`stop` は窓を閉じるだけで、行へは何も書かない**（行は消えず `open` で
    /// 再開できる）。表示が Stopped になるのは「動かしているものが無い」の結果なので、
    /// `stop` でも `/clear` でも `/resume` でも同じ表示になる（描画側は
    /// `a_row_with_no_run_is_stopped_whatever_the_hooks_say` が固定する）
    #[test]
    fn stopping_a_row_keeps_the_row_and_writes_nothing_to_it() {
        let mut app = app_with_row("s");
        let before = app.sessions.clone();
        run_popup_action(&mut app, PopupAction::Stop(SessionId::new("s")));
        assert_eq!(app.sessions, before, "stop wrote to the row (or removed it)");
    }

    /// **`close` は ccdesk の一覧からだけ外す。** transcript
    /// （`~/.claude/projects/**/*.jsonl`）は claude 側の持ち物で `claude -r` の材料。
    /// 一覧から外したいだけの操作で会話の記録まで消してはいけない
    /// （＝ この項目を「削除」と呼ばない理由そのもの）
    #[test]
    fn closing_a_row_leaves_its_transcript_on_disk() {
        let id = "8a1c0f52-0b3e-4a6d-9f11-2c7d5e8b0a34";
        // 記録のファイル名は session_id（`claude --session-id` へ渡した UUID そのもの）。
        // 置き場は共通のテスト用一時ディレクトリ（実ユーザーのファイルは触らない）
        let dir = crate::testutil::TempDir::new("transcript", "closing_a_row");
        let transcript = dir.join(&format!("{id}.jsonl"));
        std::fs::write(&transcript, "{}").unwrap();

        let mut app = app_with_row(id);
        run_popup_action(&mut app, PopupAction::Close(SessionId::new(id)));

        assert!(app.sessions.is_empty(), "the row is still in the list");
        assert!(transcript.exists(), "closing the row removed its transcript too");
    }

    /// **`Enter` はメニューを持つ行ではその行のメニュー。** セッション行も同じで、
    /// 開く導線はそのメニューの `open` になった（種類ごとに別のキーを覚えさせない）
    #[test]
    fn enter_opens_the_menu_of_whatever_row_has_one() {
        let mut app = app_with_row("s");
        app.sidebar_rows = vec![
            SidebarRow::Action(RowAction::Open(SessionId::new("s"))),
            SidebarRow::Action(RowAction::Project("C:\\dev\\api".to_string())),
            SidebarRow::Action(RowAction::ToggleGroup),
        ];
        app.sidebar_header_rows = 3;
        for (row, expected) in [
            (0usize, session("s", false)),
            // 行 "s" の cwd がこのフォルダ ＝ セッションが残っている見出し
            (1, project("C:\\dev\\api", true)),
            (2, PopupKind::Group),
        ] {
            app.popup = None;
            app.selection = SidebarPos::Row(row);
            press(&mut app, KeyCode::Enter);
            let popup = app
                .popup
                .as_ref()
                .unwrap_or_else(|| panic!("row={row}: no menu opened"));
            assert_eq!(popup.kind, expected, "row={row}");
            // 位置は行頭 `=` のクリックと同じ規則（開き方で場所が変わらない）
            assert_eq!(popup.anchor_y, selected_row_y(&app), "row={row}");
        }
    }

    /// **セッションのメニューの `open` が行クリックと同じ [`open_session`] を通る。**
    /// キーボードからセッションを開く導線はこれ 1 本なので、先頭項目であることと、
    /// open_session の判断（ここでは already-running ガード）が効くことを見る。
    ///
    /// **本物の `claude -r` は起こさない**: 行を「別インスタンスで稼働中」にして
    /// ガードで止める。spawn の成否は環境（claude の有無・portable-pty の cwd
    /// フォールバック）で変わるので、spawn に到達する形はこの単体テストでは扱えない。
    /// 開けなかったのだから、未読は残りフォーカスも移らない
    /// （成功時に既読になる順序は open_session 自身が持つ）
    #[test]
    fn the_session_menu_open_entry_routes_through_open_session() {
        let mut app = test_app(34, TERM);
        app.sessions = vec![session_row("s", "C:\\ccdesk-test-no-such-folder", 1)];
        // claude が行を開いた後に何か言った ＝ 未読
        app.hook_states = HookStates::from_entries([("s", "done", 2)]);
        // 別インスタンスで稼働中 ＝ open_session のガードが決定的に止める
        app.agents = vec![AgentInfo {
            session_id: "s".to_string(),
            kind: "interactive".to_string(),
            status: "busy".to_string(),
            pid: Some(4242),
        }];
        app.sidebar_rows = vec![SidebarRow::Action(RowAction::Open(SessionId::new("s")))];
        app.sidebar_header_rows = 1;
        app.selection = SidebarPos::Row(0);
        assert!(
            app.hook_states.unread(only_row(&app)),
            "the row must start unread for this test's premise"
        );

        press(&mut app, KeyCode::Enter); // 行のメニュー
        let index = app
            .popup
            .as_ref()
            .expect("no menu opened")
            .kind
            .entries(app.grouping)
            .iter()
            .position(|entry| entry.label == "open")
            .expect("the menu has no open entry");
        assert_eq!(index, 0, "open must be the first entry");

        activate_popup(&mut app, index);
        assert!(app.popup.is_none(), "the menu stayed open after open ran");
        // open_session のガードに到達した証拠 ＝ already-running の通知
        assert!(
            app.notice.as_ref().is_some_and(|(msg, _)| msg.contains("already running")),
            "the menu entry did not reach open_session: {:?}",
            app.notice
        );
        // 開けなかったので、内容を見ていない行の未読は残り、打ち先も移らない
        assert!(
            app.hook_states.unread(only_row(&app)),
            "a session that did not open was marked as read"
        );
        assert_ne!(app.focus, Focus::Terminal, "keys moved to a pane that did not open");
    }

    /// **`←` `→` はサイドバーから撤去した。** 「開く」と「メニュー」の 2 つを持つのは
    /// セッション行だけなので、方向で区別すると他の行では嘘になる。
    /// どの種類の行でも無反応であることを見る（残っていれば選択・メニュー・
    /// 右ペインのどれかが動く）
    #[test]
    fn the_arrow_keys_across_the_sidebar_do_nothing() {
        let mut app = app_with_row("s");
        app.sidebar_rows = vec![
            SidebarRow::Action(RowAction::UpdateCcdesk),
            SidebarRow::Inert, // 更新の無い版行
            SidebarRow::Decoration, // 区切り線
            SidebarRow::Action(RowAction::New),
            SidebarRow::Action(RowAction::Open(SessionId::new("s"))),
            SidebarRow::Action(RowAction::Project("C:\\dev\\api".to_string())),
            SidebarRow::Action(RowAction::ToggleGroup),
        ];
        app.sidebar_header_rows = app.sidebar_rows.len();
        let positions = (0..app.sidebar_rows.len())
            .map(SidebarPos::Row)
            .chain(std::iter::once(SidebarPos::Account));
        for pos in positions {
            for code in [KeyCode::Left, KeyCode::Right] {
                app.selection = pos;
                app.popup = None;
                press(&mut app, code);
                assert!(app.popup.is_none(), "{pos:?}: {code:?} opened a menu");
                assert_eq!(app.selection, pos, "{pos:?}: {code:?} moved the selection");
                assert!(
                    matches!(app.right_view, RightView::Sessions),
                    "{pos:?}: {code:?} switched the right pane"
                );
                assert_eq!(state_name(&app), "Idle", "{pos:?}: {code:?} started an update");
            }
        }
    }

    /// **`←` `→` はサイドバーの外では今までどおり。** 予約キーではないので、
    /// 端末側にフォーカスがあればカーソルキーとして claude へ渡る
    /// （ペインの移動は `Alt+←→` なので衝突しない）
    #[test]
    fn the_arrow_keys_are_not_reserved_and_still_reach_claude() {
        let parser = ccdesk::new_parser(24, 80, 0);
        for (code, seq) in [
            (KeyCode::Left, b"\x1b[D".as_slice()),
            (KeyCode::Right, b"\x1b[C".as_slice()),
        ] {
            let key = KeyEvent::new(code, KeyModifiers::NONE);
            assert_eq!(reserved_key(&key), None, "{code:?} is reserved");
            assert_eq!(
                encode_key(&key, &parser),
                seq,
                "{code:?} does not reach claude as a cursor key"
            );
        }
    }

    /// **撤去した打鍵は claude のものへ戻った。** 予約キーに載っていないので
    /// run ループは PTY へ流す（流れるバイト列も固定する）
    #[test]
    fn the_removed_shortcuts_reach_claude_untouched() {
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        let parser = ccdesk::new_parser(24, 80, 0);
        for (c, byte) in [('s', 0x13u8), ('x', 0x18)] {
            assert_eq!(reserved_key(&ctrl(c)), None, "Ctrl+{c} is still reserved");
            assert_eq!(
                encode_key(&ctrl(c), &parser),
                [byte],
                "Ctrl+{c} does not reach claude"
            );
        }
    }

    /// **予約はこの 3 つだけ。** ここに増やすと、その打鍵ぶんだけ claude 側の
    /// キーバインドが死ぬ（二次操作はポップアップに集約した）
    #[test]
    fn only_quit_and_pane_focus_are_reserved() {
        let alt = |code| KeyEvent::new(code, KeyModifiers::ALT);
        assert_eq!(
            reserved_key(&KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Some(Reserved::Quit)
        );
        assert_eq!(
            reserved_key(&alt(KeyCode::Left)),
            Some(Reserved::Focus(Focus::Sidebar))
        );
        assert_eq!(
            reserved_key(&alt(KeyCode::Right)),
            Some(Reserved::Focus(Focus::Terminal))
        );
        // 修飾が違えば claude のもの（素の ←→ / 素の q / Ctrl+←→）
        for key in [
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        ] {
            assert_eq!(reserved_key(&key), None, "{key:?} is reserved");
        }
    }

    /// サイドバーにフォーカスがあっても、撤去した打鍵はもう何も起こさない
    /// （グルーピングは ⊞ group のメニュー、停止と行外しは行のメニューへ移った）
    #[test]
    fn the_sidebar_no_longer_answers_the_removed_shortcuts() {
        let mut app = app_with_row("s");
        for c in ['s', 'x'] {
            handle_sidebar_key(
                &mut app,
                &KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL),
            );
        }
        assert_eq!(
            app.grouping,
            Grouping::State,
            "Ctrl+S still toggles the grouping"
        );
        assert_eq!(app.sessions.len(), 1, "Ctrl+X still drops the row from the list");
        assert!(app.popup.is_none(), "a removed shortcut opened a menu");
    }
}
