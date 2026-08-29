//! App 状態機械・イベントループ（run）・マウス／キー処理・セッションのディスパッチ。
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};

use ccdesk::{log_error, now_ms, same_dir, LockExt};

use crate::hooks::HookStates;
use crate::keys::{encode_key, forward_mouse, HeldInput};
use crate::poll::{AgentInfo, FooterInfo, Grouping, Run, State};
use crate::usage::Usage;
use std::collections::BTreeMap;

use crate::backend::{Inject, Kind, Launch};
use crate::session::Session;
use crate::sessions::{SessionId, SessionRow};
use crate::source::{DataSource, PollSinks, SlotView, WindowItem, PROJECTS_LIMIT};
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
/// 何かが動いている間の描き直し間隔。**明滅 1 コマの長さ**
/// （[`crate::theme::BLINK_TICK_MS`]）そのもの ＝ これより短くしても同じコマを
/// 描き直すだけで見た目は変わらない。数字を 2 箇所に持たないので、コマを増やしても
/// フレームは増えない（明滅の段階数は 1 周のコマ数で決まり、間隔は変わらない）
const ANIMATION_REDRAW: Duration = Duration::from_millis(crate::theme::BLINK_TICK_MS);
/// 何も動いていないときの描き直す間隔。残るのは通知の期限切れ等の低頻度な変化
/// だけなので、これより短い周期は**前フレームと同一の出力**を組み直すだけ
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
    /// スロットの並べ方を選ぶメニューを開く（▦ layout 行）
    ChooseLayout,
    /// セッション行: ウィンドウが開いていれば切替、無ければ `claude -r` で再開
    Open(SessionId),
    UpdateCcdesk,  // ccdesk 自身を更新（サイドバー先頭の版行）
    /// その agent 本体を更新（同じく版行）
    UpdateAgent(Kind),
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
    /// 区切り線・空行・グループ見出し ＝ 画面を組む飾りで、行の実体が無い。
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
}

impl SidebarPos {
    /// 一覧の行の index（行のハイライト・行の動作の引き当て・スクロール追従が読む）。
    ///
    /// **`Option` のまま残してある**: 一覧の外を指す位置（かつてのアカウント行）が
    /// また要るかもしれないので、呼び手を「必ず行がある」前提にしない
    pub(crate) fn row(self) -> Option<usize> {
        let Self::Row(row) = self;
        Some(row)
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
    /// 差し替え済み。反映は次回起動から（自動では再起動しない — 詳細は
    /// [`crate::ui`] の `UpdateState::RestartPending`）ので、置いた先の exe パスは持たない
    Done,
    /// 失敗。run ループが下部バーへ 1 度出して Idle へ戻す
    Failed(String),
}

/// agent 本体（`<agent> update`）の進行状態。**kind ごとに 1 つで、これが正本**。
///
/// 以前は「実行中の旗」「据え置きの旗」「失敗の文面」の 3 本に割れていて、版行は
/// そこから状態を再合成し、run ループは「latest が消えたら据え置きを降ろす」という
/// 照合規則を別に持っていた ＝ **同じ真実が 2 箇所にあり、間を規則で埋める形**。
/// 1 つにすると在り得ない組（Running かつ Stalled）が表現できなくなり、
/// 照合規則そのものが要らなくなる（[`SelfUpdate`] と同じ形）。
///
/// **`Done` を持たない**のは agent 側に再起動の案内が要らないため:
/// 更新後は `<agent> --version` が新しい版を返して `footer.latest` が消え、
/// 行が自然に最新表示へ戻る
pub(crate) enum AgentUpdate {
    /// 未実行、または失敗を通知し終えた後（＝押し直せる）
    Idle,
    Running,
    /// 成功と報告されたのに版が動かなかった。
    ///
    /// **押した時点の版を持つ。** 「据え置きである」ことの成立条件が
    /// 「今もその版のまま」だからで、条件を状態の中に持たせると、
    /// 別経路で版が動いた場合（手で入れ直した・agent 自身の自動更新が通った）に
    /// **降ろす規則を書かずに**行が最新表示へ戻る。
    ///
    /// `announced` は下部バーへ 1 度出したか。通知は 5 秒で消えるが押しても
    /// 効かない状態は続くので、**状態は消さずに「出したか」だけを持つ**
    /// （[`Failed`](Self::Failed) が Idle へ戻るのと同じ「1 度だけ出す」）
    Stalled { version: String, announced: bool },
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
    State,
    /// スロットの並べ方。**どちらの値も開いた時点の写し**（[`PopupKind::Session`] と
    /// 同じ作り）で、`current` は `●` を付ける項目、`fits` は押せる項目の判断に使う
    Layout {
        current: crate::panes::Layout,
        /// 今の端末で崩れずに出せる配置（狭い端末では枚数の多いものが落ちる）
        fits: Vec<crate::panes::Layout>,
    },
    /// プロジェクト単位の操作。`has_sessions` は開いた時点の写し（[`PopupKind::Session`] の
    /// `open` と同じ作り）で、`remove project` を出せるかの判断に使う
    Project { cwd: String, has_sessions: bool },
}

/// 項目を選んだときに起きること。**選択 index から作る**ので、表示名が同じ項目が
/// 並んでも対象を取り違えない（ラベル文字列から対象を復元しない）。
/// 副作用は持たず、実行は [`run_popup_action`] だけが行う
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PopupAction {
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
    /// スロットの並べ方を変える（溢れたスロットの中身は表示から外れるだけ）
    SetLayout(crate::panes::Layout),
    /// 指定フォルダで新規セッション（agent を選んで起こす）
    NewSessionIn(Kind, String),
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
    /// `kinds` は今出す agent（[`crate::app::App::kinds`]）。**切った agent で
    /// 新規セッションを起こす項目を出さない**ためだけに要る
    pub(crate) fn entries(&self, grouping: Grouping, kinds: &[Kind]) -> Vec<PopupEntry> {
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
                    // **常に押せる。** 会話を確かめていない行も agent 自身の
                    // ピッカーで開く（`relaunch`）ので、押せない行が無い
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
            PopupKind::State => Grouping::ORDER
                .into_iter()
                .map(|g| PopupEntry {
                    label: format!("{}{}", if grouping == g { "● " } else { "  " }, g.as_str()),
                    enabled: true,
                    action: PopupAction::SetGrouping(g),
                })
                .collect(),
            // 項目は [`crate::panes::Layout::ORDER`] から導く（値を足すとメニューも
            // 自動で増える）。**入らない配置は押せない**ので、選んだ瞬間に崩れることが無い。
            //
            // **1 枚だけは常に押せる。** 端末を縮めてどの配置も入らなくなったとき、
            // 全部を落とすとメニューが全灰色になり、**画面の中から 1 枚へ戻る道が
            // 消える**（端末を広げるまで詰む）。1 枚は退避先なので例外にする
            PopupKind::Layout { current, fits } => crate::panes::Layout::ORDER
                .into_iter()
                .map(|l| PopupEntry {
                    label: format!("{}{}", if *current == l { "● " } else { "  " }, l.as_str()),
                    enabled: fits.contains(&l) || l == crate::panes::Layout::One,
                    action: PopupAction::SetLayout(l),
                })
                .collect(),
            // **セッションが残っているフォルダは登録解除させない。** 見出しの一覧は
            // 「登録リスト ∪ セッションの cwd」なので、登録を外してもセッション由来で
            // 見出しは出続ける。押せるのに表示が変わらないのは嘘なので、
            // stop と同じ仕組み（実行可能フラグ）で落とす
            //
            // **`new session` は agent ごとに項目を分ける。** この入口は New 画面を
            // 通らず即起動するので、既定で黙って起こすと「押すまで何が起きるか
            // 分からない」ことになる
            PopupKind::Project { cwd, has_sessions } => kinds
                .iter()
                .copied()
                .map(|kind| PopupEntry {
                    label: format!("new {} session", kind.title()),
                    enabled: true,
                    action: PopupAction::NewSessionIn(kind, cwd.clone()),
                })
                .chain([
                    PopupEntry {
                        label: "remove project".to_string(),
                        enabled: !has_sessions,
                        action: PopupAction::RemoveProject(cwd.clone()),
                    },
                ])
                .collect(),
        }
    }

    /// 選択 index の項目が意味する動作（範囲外は None）。**表（[`Self::entries`]）から
    /// 引く**ので、表示と動作が index でずれることは構造的に無い
    #[cfg(test)]
    fn action(&self, grouping: Grouping, index: usize) -> Option<PopupAction> {
        let mut entries = self.entries(grouping, &Kind::ORDER);
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

/// スロット 1 枚の中身。**空も 1 つの中身**（`no session` 画面）。
///
/// **セッションは添字ではなく [`SessionId`] で指す。** 窓（[`App::windows`]）は
/// 死んだものから抜けるので、添字で持つと他のスロットの指す先が黙ってずれる
/// （かつて `active: usize` が窓を抜くたびに補正を要していたのと同じ問題を、
/// スロットの数だけ抱えることになる）
pub(crate) enum Slot {
    Empty,
    Session(SessionId),
    New(NewState),
}

impl Slot {
    /// そのスロットが映しているセッション
    pub(crate) fn session(&self) -> Option<&SessionId> {
        match self {
            Self::Session(id) => Some(id),
            _ => None,
        }
    }
}

/// **掴んだセッションを落とせる先。** 座標 1 つから [`drop_target`] が導き、
/// ガイドの塗り（[`drop_rect`]）とドロップの実行（[`drop_session`]）が
/// **同じ 1 つの答えを読む**。別々に判定すると「塗った場所と入る場所が違う」が
/// いつか必ず起きる ＝ ここが分岐点を持つ唯一の場所
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DropAt {
    /// そのスロットへ入れる（元の中身は押し出される）
    Slot(usize),
    /// そのスロットを割って、できた片方へ入れる
    Split {
        at: usize,
        axis: crate::panes::Axis,
        /// [`crate::panes::Grown::halves`] のどちら（0 = 左/上、1 = 右/下）
        half: usize,
    },
}

/// 掴めるもの ＝ **ペインへ何かを出す操作**。サイドバーの行とメニューの項目は
/// 綴りが違うだけで、落とし先の決め方も出るものも同じなのでここで 1 つに束ねる。
///
/// **持つのは元の動作そのもの**で、掴んだ物の種類を別の言葉へ写し取らない:
/// 運ばずに離したときはこれをそのまま元の実行表（[`run_row_action`] /
/// [`run_popup_action`]）へ渡す ＝ **押して離すだけの意味は、掴めるようにする
/// 前と 1 ビットも変わらない**
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Grab {
    /// サイドバーの行
    Row(RowAction),
    /// メニューの項目
    Menu(PopupAction),
}

impl Grab {
    /// 掴めるか。**掴めるのは「ペインに何かを出す」操作だけ**で、その場で走る
    /// もの（版行の更新・グルーピング切替・ピン留め・stop / close）は運ぶ先を持たない
    fn grabbable(&self) -> bool {
        match self {
            // **プロジェクト見出しは掴めない。** この行の動作はメニューを開くこと
            // で、そこから起こす項目は agent を選ばせてから走る ＝ 見出しごと
            // 運ぶと、選ばせる段が落ちるか、選ばせる画面を出すだけの別物になる。
            // 運びたいのはメニューの `new <agent> session` の側（下）
            Self::Row(action) => matches!(action, RowAction::Open(_) | RowAction::New),
            Self::Menu(action) => matches!(
                action,
                PopupAction::OpenSession(_) | PopupAction::NewSessionIn(..)
            ),
        }
    }

    /// **落とすと何が出るか。** ガイドの見出し（[`crate::ui`]）はこれだけを読むので、
    /// 掴める入口が増えても見出しの綴りは 1 箇所のまま
    pub(crate) fn shows(&self) -> Grabbed<'_> {
        match self {
            Self::Row(RowAction::Open(id)) | Self::Menu(PopupAction::OpenSession(id)) => {
                Grabbed::Session(id)
            }
            Self::Menu(PopupAction::NewSessionIn(_, cwd)) => Grabbed::NewIn(cwd),
            _ => Grabbed::New,
        }
    }
}

/// 掴んでいるものを**出るもの**で言い換えたもの（[`Grab::shows`]）
pub(crate) enum Grabbed<'a> {
    /// 既にあるセッション（その名前で出る）
    Session(&'a SessionId),
    /// 新規セッション（フォルダは直近のもの ＝ [`App::dispatch_cwd`]）
    New,
    /// 新規セッション（このフォルダ）
    NewIn(&'a str),
}

/// 掴んでいるもの（ドラッグ＆ドロップ）
pub(crate) struct DragSession {
    pub(crate) what: Grab,
    /// 掴んだ行の画面 y。**離した位置ではなくここを使う**ので、少しずれた
    /// 手つきでもメニューは掴んだ行の下に出る
    pub(crate) anchor_y: u16,
    /// この掴みが**メニューを閉じた押し**から始まったか（[`arm_grab`]）。
    /// 運ばずに離したときは何もしない: 閉じるのがその押しの仕事だったので、
    /// 同じ押しで行の動作まで走らせると閉じた直後に開き直す
    pub(crate) dismissed: bool,
    /// 実際に動いたか。**押しただけでは掴みにしない**ので、押して離すだけの
    /// 操作は今までどおりのクリックとして通る
    pub(crate) moved: bool,
    /// 今の指し先（落とせないところを指しているなら `None`）
    pub(crate) target: Option<DropAt>,
}

pub(crate) struct App {
    /// 開いているウィンドウ（前景セッションの PTY そのもの）。**一覧の行とは別物**で、
    /// 窓を閉じてもプロセスが終わるだけ ＝ 行（[`Self::sessions`]）は残る
    pub(crate) windows: Vec<Session>,
    /// スロットの並べ方（入口は サイドバーの ▦ layout 行のメニュー）
    pub(crate) layout: crate::panes::Layout,
    /// 十字の位置（境界ドラッグで動く）
    pub(crate) split: crate::panes::Split,
    /// スロットの中身。**長さは常に `layout.slots()`**（保つのは [`App::set_layout`] だけ）
    pub(crate) slots: Vec<Slot>,
    /// フォーカス中のスロット。**[`Self::focus`] がサイドバーでも保たれる**ので、
    /// 一覧の `open` が入る先と `Alt+→` の戻り先が必ず一致する。
    ///
    /// **触るキーが分かれているのが要点**: `Alt+←/→` は [`Self::focus`] だけを、
    /// `Alt+Shift+方向` はここだけを動かす。1 つのキーが両方へ書くと、
    /// サイドバーへ抜ける道中でこの値が壊れ、最左列以外のスロットを
    /// `open` の宛先にできなくなる
    pub(crate) focus_slot: usize,
    // 生きている前景セッションのライブ状態（`~/.claude/sessions/` 由来。
    // バックグラウンドスレッドが更新）
    pub(crate) agents: Vec<AgentInfo>,
    /// [`Self::agents`] の**取得を始めた時刻**（ms）。**status の観測時刻**として
    /// **ccdesk が `~/.claude/sessions/` を最後に読めた時刻**（ms）。
    ///
    /// 用途は [`Self::stopped_at`] との比較 1 つだけ ＝「自分が止めた後に見たか」。
    /// **行の状態の新旧裁定には使わない**（それは claude 自身が書いた
    /// [`crate::poll::AgentInfo::status_at`]）。ここを裁定に使っていた頃は、値が
    /// 常に「今」なので status が hook に必ず勝ち、陳腐化した `busy` を新しい
    /// `idle` hook で降ろせなかった。
    ///
    /// **取り込みの瞬間（run ループの swap）を刻まない**: 取得と一緒に運ばれてきた
    /// 時刻をそのまま読む（[`crate::poll::AgentSnapshot`]）ので、取得に失敗した
    /// 周回でこの値が進むことがない
    pub(crate) agents_observed_at: u64,
    pub(crate) agents_shared: Arc<Mutex<crate::poll::AgentSnapshot>>,
    pub(crate) agents_dirty: Arc<std::sync::atomic::AtomicBool>,
    /// ccdesk がその窓を閉じた時刻（ms）。**「自分が今止めたセッション」だけを覚える**。
    /// 刻む場所は [`remove_window`] 1 箇所（窓を外す経路が増えても刻み忘れが起きない）。
    ///
    /// ライブ状態の観測は最大 [`LIVE_SCAN_INTERVAL`] 分古いことがあるので、
    /// kill した直後は「たった今殺した自分の前景セッション」がまだ busy 等として
    /// 載っている。この残像を[`crate::ui`]の別インスタンス救済（窓が無い行を
    /// 他インスタンスの実行として拾う分岐）が実行と誤認すると、Stopped になるはずの
    /// 行が一瞬 Waiting 等を経由してしまう。ここに刻んだ時刻より観測が新しくなる
    /// （＝ 次のポーリングで残像が消える）までは、その行を救済の対象にしない。
    ///
    /// 行に保存しない（`sessions.json` は触らない）。窓を再び開いた
    /// （[`App::window_index`] が Some を返すようになった）ら消す ＝ 無限に溜めない
    pub(crate) stopped_at: std::collections::HashMap<SessionId, u64>,
    /// サイドバーに並ぶ行。**正本は `~/.ccdesk/sessions.json`**（供給元が読み書きする）
    pub(crate) sessions: Vec<SessionRow>,
    /// hook（`--settings` で注入した公式 hook）が書いた state の写し。
    /// **行の state・未読はどれもここから導く**（行に保存しない）。
    /// hook が一度も来ていない行だけ `~/.claude/sessions/` の `status` へ落ちる
    /// （[`crate::hooks`]）
    pub(crate) hook_states: HookStates,
    /// 撮影用の固定 state（`session_id` → state）。**実データでは必ず空**で、
    /// 窓を持たない行を「動いている」ものとして描くためだけにある
    /// （[`crate::source::DataSource::fixed_states`]）
    pub(crate) fixed_states: std::collections::HashMap<SessionId, crate::poll::State>,
    /// 入力待ちになった行を OS の通知で知らせるか（`config.json` の `"notify"`。
    /// 既定は false ＝ [`crate::notify`]）。**設定を読むのは `main` だけ**で、
    /// ここに載っているのはその結果
    pub(crate) notify: crate::notify::Wanted,
    /// その行について**どの時刻の呼び出しまで撃ったか**（[`update_notifications`]）。
    ///
    /// **覚えるのはこれだけ**（前の周の state の写しは持たない）。同じ出来事を
    /// 毎周撃たないための 1 本で、窓の無くなった行は落とすので行数で有界
    pub(crate) announced: std::collections::HashMap<SessionId, u64>,
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
    /// 十字の境界をつかんでいる間だけ Some。中身は `(縦を動かすか, 横を動かすか)` で、
    /// **交点をつかめば両方 true** ＝ 縦横を同時に動かせる
    pub(crate) cross_drag: Option<(bool, bool)>,
    /// サイドバーの行を掴んでいる間だけ Some（ドラッグ＆ドロップ）。
    /// **押した時点で Some になる**が、動くまでは掴みとして扱わない
    /// （[`DragSession::moved`]）＝ ただのクリックは今までどおり通る
    pub(crate) drag: Option<DragSession>,
    pub(crate) last_drag_resize: std::time::Instant,
    pub(crate) term_size: (u16, u16), // (width, height)
    // サイドバーに積まれた行（draw で構築）。飾りと押せない行の区別は [`SidebarRow`]
    pub(crate) sidebar_rows: Vec<SidebarRow>,
    // サイドバー上部の固定行数（版行・区切り線・+ new session・⊞ group・▦ layout
    // など）。**行の一覧をここに書き写さない**（足すたびに黙って古くなる）:
    // 正本は draw が積んだ行数そのもので、ヒットテストとスクロール計算は
    // sidebar_rows と同じく「最後に描いた値」を読む
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
    // サイドバー下部のアカウント・バージョン表示（バックグラウンド取得）
    pub(crate) footer: FooterInfo,
    pub(crate) footer_shared: Arc<Mutex<FooterInfo>>,
    pub(crate) footer_dirty: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) footer_refresh: Arc<std::sync::atomic::AtomicBool>,
    /// agent 本体の更新の進行状態（[`AgentUpdate`]）。**バックグラウンドスレッドが
    /// 書き、UI が読む正本**。
    ///
    /// **agent ごとに 1 つ。** 共有にすると片方の更新中にもう片方の版行まで
    /// Running になり、押せなくなる（実機で踏んだ）。
    /// 多重起動の防止もこのロックの中で決まる（同じ知識を旗に分けない）
    pub(crate) agent_update: BTreeMap<Kind, Arc<Mutex<AgentUpdate>>>,
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
    pub(crate) usage: BTreeMap<Kind, Usage>,
    /// 使用率が更新されたことを取得スレッドが立てる合図（フッターと同じ作法）。
    /// **周期で読みに行かない**ので、使用率を切った環境ではこの旗が一度も立たない
    pub(crate) usage_dirty: Arc<std::sync::atomic::AtomicBool>,
    /// **クリック起点の**取得が進行中か（リングをスピナーに変える材料）。
    /// 自動取得（保険の周期・ターン完了）では立たない ＝ 押していないのに回らない。
    /// 立てるのはクリック（即時）と取得スレッド、降ろすのは取得スレッドだけ
    pub(crate) usage_fetching: BTreeMap<Kind, Arc<std::sync::atomic::AtomicBool>>,
    /// マウスが使用率ゲージの上にいるか（押せることを帯で示す）。
    /// 一覧の行ではないので [`Self::hovered`]（[`SidebarPos`]）では表せない
    /// マウスが乗っている使用率の行（**どの agent の行か**。無ければ None）。
    /// 行ごとに取り直せるので、乗っている 1 行だけを帯にする
    pub(crate) usage_hovered: Option<Kind>,
    // 画面に出す値の供給元（実データ / 撮影用の固定データ）。起動時に 1 度だけ選ばれ、
    // 以降ここを通る限り「今 demo か」を問う必要が無い
    pub(crate) source: Arc<dyn DataSource>,
    // 起動した子がまだ端末を掴んでいない間だけ Some（起こした時刻）。
    // 降ろす契機は「子が最初の出力を出した」（run ループ）と期限切れ
    // （[`expire_input_gate`]）の 2 つで、**降ろすのは [`lift_input_gate`] だけ**
    pub(crate) input_gate: Option<std::time::Instant>,
    /// 門番（[`Self::input_gate`]）が立っている間に打たれた入力を、打った順に
    /// 溜めておく列（[`hold_input_while_starting`] が積み、門番を降ろす
    /// [`lift_input_gate`] が流すか捨てるかを決める）。
    ///
    /// **溜めるのは生のイベントで、バイト列ではない**（理由は [`HeldInput`]）。
    /// 打鍵と貼り付けを 1 本で持つのは順序を保つため。
    ///
    /// 似た名前の [`Self::pending_submit`] とは別物: あちらはセッション間の
    /// 受け渡し（`ccdesk send`）の送信待ちで、こちらはこの端末で打った人の入力
    pub(crate) held_input: Vec<HeldInput>,
    // 下部バーに数秒表示するエラー等の通知
    pub(crate) notice: Option<(String, std::time::Instant)>,
    pub(crate) grouping: Grouping,
    /// **今出す agent**（設定から起動時に 1 度組む。[`Kind::enabled`]）。
    ///
    /// 版行・使用率行・grouping の節・New 画面の切替・フォルダメニューの
    /// new session・一覧に出す行 が全部これを読むので、**「codex を出すか」の
    /// 判断が画面のあちこちに散らない**。設定は config.json だけなので
    /// 起動中は変わらないが、`Kind::ORDER` を直に読む場所を残すと
    /// そこだけ off に従わない、という抜けができる
    pub(crate) kinds: Vec<Kind>,
    // 登録済みプロジェクト（ディレクトリ）の絶対パス。**この Vec が登録内容の正本**で、
    // 変更のたび全量を供給元へ書き戻す。directory グルーピングの見出しは
    // 「この一覧 ∪ セッションの cwd」なので、セッションが 0 本になっても
    // ここに残っている限り見出しは消えない（＝そのフォルダで新規を開く入口が残る）
    pub(crate) projects: Vec<String>,
    pub(crate) popup: Option<Popup>,
    pub(crate) focus: Focus,
    /// 直前のフレームで速い描き直しが要る何かが動いていたか。
    /// run ループがアイドル時の描き直し間隔を選ぶ材料（描画が毎フレーム更新する）。
    ///
    /// **材料は 2 つ束ねている**: 行のドットの明滅（[`crate::poll::State::blinks`]）と、
    /// 使用率の取得中スピナー（[`Self::usage_fetching`]。回るのは本物のブライユ点字
    /// アニメ ＝ [`crate::ui`] の `usage_spinner_frame`）。どちらも「今フレームを
    /// 速く描き直す必要があるか」という同じ問いに答えるので、
    /// 名前を「spinner」に寄せず両方を指せる名前にしてある
    pub(crate) animating: bool,
    /// 最後に公開した「走っているセッション」の一覧（[`crate::relay`]）。
    /// **前回との差だけが書く条件**で、窓の増減を契機にしない: 名前は transcript が
    /// 伸びるたびに変わり得るので、契機で書くと `ccdesk list` に古い名前が残る
    pub(crate) published_sessions: Vec<crate::relay::Open>,
    /// 貼り付け済みで、まだ送信の `\r` を出していないセッション（[`SUBMIT_DELAY`]）。
    /// 積んだ時刻を一緒に持つのは、期限が来たものだけを出すため
    pub(crate) pending_submit: Vec<(SessionId, std::time::Instant)>,
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
            layout: crate::panes::Layout::default(),
            split: crate::panes::Split::default(),
            slots: vec![Slot::Empty],
            focus_slot: 0,
            agents: Vec::new(),
            agents_observed_at: 0,
            agents_shared: Arc::new(Mutex::new(crate::poll::AgentSnapshot::default())),
            agents_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            stopped_at: std::collections::HashMap::new(),
            sessions: Vec::new(),
            hook_states: HookStates::default(),
            fixed_states: std::collections::HashMap::new(),
            notify: crate::notify::Wanted::default(),
            announced: std::collections::HashMap::new(),
            hook_stamp: None,
            titles: Titles::default(),
            last_scan: std::time::Instant::now(),
            last_live_scan: std::time::Instant::now(),
            sidebar_width: crate::ui::DEFAULT_SIDEBAR,
            dragging: false,
            cross_drag: None,
            drag: None,
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
            footer: FooterInfo::default(),
            footer_shared: Arc::new(Mutex::new(FooterInfo::default())),
            footer_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            footer_refresh: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            agent_update: agent_update_states(),
            ccdesk_update: Arc::new(Mutex::new(SelfUpdate::Idle)),
            ccdesk_latest: None,
            ccdesk_latest_shared: Arc::new(Mutex::new(None)),
            ccdesk_latest_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            usage: BTreeMap::new(),
            usage_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            usage_fetching: per_agent_flags(),
            usage_hovered: None,
            // 撮影用の供給元は state.json / config.json を書かないので、
            // テストが開発者の設定を踏まない
            source: Arc::new(crate::source::DemoSource),
            input_gate: None,
            held_input: Vec::new(),
            notice: None,
            grouping: Grouping::State,
            // テストの既定は**全 agent**。本番の既定（claude だけ。
            // [`Kind::enabled`]）とは違えてある: 複数 agent の桁割り・節・
            // 下部バーの行数を見るテストが多く、既定を claude だけにすると
            // それらが黙って 1 agent の経路しか通らなくなる。
            // claude だけの見え方はその一覧を明示するテストが受け持つ
            kinds: Kind::ORDER.to_vec(),
            projects: Vec::new(),
            popup: None,
            // サイドバー側にしておく（set_focus が PTY へ通知を出さない）
            focus: Focus::Sidebar,
            animating: false,
            published_sessions: Vec::new(),
            pending_submit: Vec::new(),
        }
    }
}

/// 起動 1 回の結果。**成功なら起こしたセッション**（起こさない供給元 ＝ 撮影用は
/// `Ok(None)`。「起動を試していない ＝ 失敗もしていない」を表す）、失敗なら理由。
///
/// 反映は [`apply_launch`] だけが行う（フォルダの登録を成功時に 1 箇所で行うため）
type Launched = Result<Option<SessionId>, String>;

impl App {
    /// 各スロットの矩形（並びはスロット番号順）。**矩形の正本はここ 1 つ**で、
    /// 描画・ヒットテスト・PTY のサイズが同じ答えを読む
    pub(crate) fn slot_rects(&self) -> Vec<Rect> {
        self.layout.rects(crate::ui::pane_rect(self), self.split)
    }

    /// スロット矩形から Block 枠線 2 桁/2 行を引いた内側サイズ `(rows, cols)`
    fn inner_size(rect: Rect) -> (u16, u16) {
        (
            rect.height.saturating_sub(2).max(1),
            rect.width.saturating_sub(2).max(1),
        )
    }

    /// PTY のサイズを合わせる。**見えている窓は自分のスロットの大きさ**、
    /// どのスロットにも出ていない窓はフォーカススロットの大きさにしておく
    /// （次に映したときに正しい大きさで出る ＝ 映した瞬間の作り直しが要らない）
    fn resize_sessions(&mut self) {
        let rects = self.slot_rects();
        let default = rects
            .get(self.focus_slot)
            .or_else(|| rects.first())
            .map_or((1, 1), |r| Self::inner_size(*r));
        let mut want: std::collections::HashMap<SessionId, (u16, u16)> =
            std::collections::HashMap::new();
        for (slot, rect) in self.slots.iter().zip(rects.iter()) {
            if let Some(id) = slot.session() {
                want.insert(id.clone(), Self::inner_size(*rect));
            }
        }
        for window in &mut self.windows {
            let (rows, cols) = want.get(&window.session_id).copied().unwrap_or(default);
            window.resize(rows, cols);
        }
    }

    /// 配置を変える。**スロットの数を `layout` に合わせるのはここだけ**なので、
    /// 「長さが合っていない `slots`」が他所で作られることがない。
    /// 溢れたスロットの中身は捨てるだけ（PTY は [`Self::windows`] が持ったまま
    /// 生き続ける ＝ 表示から外れるだけで何も終わらない）
    pub(crate) fn set_layout(&mut self, layout: crate::panes::Layout) {
        let want = layout.slots();
        let mut slots = std::mem::take(&mut self.slots);
        slots.truncate(want);
        while slots.len() < want {
            slots.push(Slot::Empty);
        }
        self.apply_layout(layout, slots);
    }

    /// 配置とスロットの中身を**同時に**差し替える。
    ///
    /// **枚数を `layout` に合わせるのはここと [`Self::set_layout`] だけ**なので、
    /// 長さの合わない `slots` が他所で作られない。
    ///
    /// 中身を組み替える呼び手（ドロップで割るとき ＝ [`make_room`]）が居るので
    /// 分けてある: 枚数合わせと中身の入れ替えが 1 つの関数だと、**中身の欠けた
    /// 瞬間に下のリサイズが走る**（触っていない窓まで誤ったサイズへ潰れ、
    /// vt100 の画面が刈られて内容が消える）
    fn apply_layout(&mut self, layout: crate::panes::Layout, slots: Vec<Slot>) {
        debug_assert_eq!(slots.len(), layout.slots(), "slots do not match the layout");
        self.layout = layout;
        self.slots = slots;
        self.focus_slot = self.focus_slot.min(layout.slots().saturating_sub(1));
        // **枚数を減らすとフォーカススロットの中身が変わる**（丸めた先に別の行が
        // 出る）ので、移動と同じく既読を合わせる。溢れて消えた側は画面から
        // 消えるだけなので、そちらは未読のまま残るのが正しい
        self.mark_focus_read();
        self.resize_sessions();
    }

    /// **スロット `at` を配置から外す**（枠の右上の ✕）。
    ///
    /// **終わるものは何も無い。** ここで閉じるのは表示枠で、映していた
    /// セッションは裏で走り続ける（[`crate::panes`] の書き出し ＝ ペインは
    /// プロセスの入れ物ではない）。行もサイドバーに残るので、押し直せば戻る。
    ///
    /// 空いた範囲を誰が吸うかは [`crate::panes::Layout::close_slot`] が答え、
    /// **残ったスロットの中身は番号の振り直しに沿って写す**（[`make_room`] と同じ作法）
    /// ＝ 触っていないセッションは 1 つも画面から消えない。
    ///
    /// **1 枚の盤面には減らす先が無い**ので、そのときは中身だけ空ける
    /// （枠が消えて右ペインが無くなる形は作らない）
    pub(crate) fn close_slot(&mut self, at: usize) {
        if at >= self.slots.len() {
            return;
        }
        let Some(shrunk) = self.layout.close_slot(at) else {
            self.put_in_slot(at, Slot::Empty);
            return;
        };
        // **離れる窓へ focus out を送るのが先**（[`Self::place_session`] と同じ作法）
        if self.focus == Focus::Terminal
            && let Some(w) = self.focused_window()
        {
            self.windows[w].send_focus(false);
        }
        // **新しい並びを組み終えてから入れ替える**（[`make_room`] と同じ理由:
        // 中身の欠けた並びを持たせると、そこで走るリサイズが触っていない窓まで潰す）
        let mut before = std::mem::take(&mut self.slots);
        let mut next: Vec<Slot> = (0..shrunk.layout.slots()).map(|_| Slot::Empty).collect();
        for (from, to) in &shrunk.moved {
            if let Some(slot) = before.get_mut(*from) {
                next[*to] = std::mem::replace(slot, Slot::Empty);
            }
        }
        // **フォーカスは番号の振り直しに沿って写す**（読み順が変わるので、
        // 番号を据え置くと見ていたのと違うスロットへ焦点が移る）。
        // 閉じた枠を見ていたなら、その跡地を持つスロット ＝ 空きを吸った 1 枚へ。
        // **`apply_layout` より先に載せる**のが要点で、既読を合わせるのはあちら
        // （後から動かすと、閉じる前のスロットの行を既読にしてしまう）
        self.focus_slot = if self.focus_slot == at {
            shrunk.absorbed
        } else {
            shrunk
                .moved
                .iter()
                .find(|(from, _)| *from == self.focus_slot)
                .map_or(shrunk.absorbed, |(_, to)| *to)
        };
        self.apply_layout(shrunk.layout, next);
        self.focus_terminal_on(self.focus_slot);
        // **配置ごと書き残す**（スロットだけ保存すると、次の起動で古い枚数が復元される）
        save_layout(self);
    }

    /// フォーカス中のスロットへ new session 画面を出す。
    ///
    /// **既に出ているなら作り直さない。** この操作の意味は「この画面を出す」で、
    /// 出ているものを捨てることではない: 作り直すと打ちかけのプロンプトと
    /// 参照中のフォルダが黙って消える（`+ new session` を押し直しただけで
    /// 書きかけが失われていた）。
    ///
    /// **フォルダを名指しして出す入口（ドラッグ＆ドロップ ＝ [`drop_new_view`]）は
    /// ここを通らない**: あちらは「このフォルダの画面をここへ」なので作り直しが意味を持つ
    pub(crate) fn open_new_view(&mut self) {
        if self.focus_is_new() {
            return;
        }
        let state = NewState::browse(&self.dispatch_cwd);
        self.put_in_focus(Slot::New(state));
    }

    /// フォーカス中のスロットの中身を差し替える。**保存もここから**
    fn put_in_focus(&mut self, slot: Slot) {
        if let Some(at) = self.slots.get_mut(self.focus_slot) {
            *at = slot;
        }
        self.save_slots();
    }

    /// **宛先を明示してスロットの中身を差し替え、フォーカスもそこへ移す。**
    /// 入口はドラッグ＆ドロップ（[`drop_new_view`]）＝ どのスロットへ出すかを
    /// ユーザーが名指しした操作。
    ///
    /// [`Self::put_in_focus`] と分けてあるのは焦点の受け渡しの有無:
    /// あちらは今見ているスロットの中身を入れ替えるだけで、フォーカスは動かない
    fn put_in_slot(&mut self, at: usize, slot: Slot) {
        if at >= self.slots.len() {
            return;
        }
        // **離れる窓へ focus out を送るのが先**（[`Self::place_session`] と同じ作法）
        if self.focus == Focus::Terminal
            && let Some(w) = self.focused_window()
        {
            self.windows[w].send_focus(false);
        }
        self.slots[at] = slot;
        self.save_slots();
        self.focus_slot = at;
        self.mark_focus_read();
        // 出したのは New 画面 ＝ 映す窓が無いので focus in は誰にも行かない
        self.focus_terminal_on(at);
        self.resize_sessions();
    }

    /// 次回起動で同じ並びを復元できるように書き残す
    pub(crate) fn save_slots(&self) {
        let views: Vec<crate::source::SlotView> = self
            .slots
            .iter()
            .map(|slot| match slot {
                Slot::Empty => crate::source::SlotView::Empty,
                Slot::New(_) => crate::source::SlotView::New,
                Slot::Session(id) => crate::source::SlotView::Session(id.as_str().to_string()),
            })
            .collect();
        self.source.save_window(WindowItem::Slots(&views));
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
    pub(crate) fn set_focus(&mut self, focus: Focus) {
        if self.focus == focus {
            return;
        }
        if let Some(at) = self.focused_window() {
            self.windows[at].send_focus(focus == Focus::Terminal);
        }
        self.focus = focus;
        if focus == Focus::Sidebar {
            self.last_scan = instant_ago(SCAN_INTERVAL);
            self.last_live_scan = instant_ago(LIVE_SCAN_INTERVAL);
        }
    }

    /// **そのセッションを見えるところへ出す。**
    ///
    /// 既に別スロットへ出ているなら、中身は動かさずフォーカスをそこへ移すだけ
    /// （[`Self::set_focus_slot`]）。画面のどこかに出ているセッションを選んだのに
    /// 中身ごとフォーカス中のスロットへ引き寄せると、押した人から見て
    /// 「別スロットの中身が消えた」ように映る ＝ 見えている場所へ視線を移す方が
    /// 説明できる動きになる。
    ///
    /// まだどこにも出ていなければ、フォーカス中のスロットへ表示する。
    /// **動くのは触ったセッションだけ。** 入れ替え（押し出された側を元居た場所へ
    /// 送る）にはしていない: 触っていないセッションが勝手に別のスロットへ
    /// 飛ぶのは、押した人から見て説明できない動きになる。押し出された側は
    /// 表示から外れるだけで、**行も PTY も残る**（選び直せば戻る）。
    ///
    /// 規則はこの 2 つで全部の場合を覆う（他スロットに表示中 / 未表示）。
    /// **同じセッションが 2 スロットに出ることは構造的に起きない**: 1 つの
    /// [`crate::session::Session`] は PTY もパーサもサイズを 1 つしか持てないので、
    /// 2 箇所に違う大きさで映すことがそもそもできない
    fn show_session(&mut self, id: &SessionId) {
        if let Some(from) = self.slot_of(id) {
            // 見えているなら視線だけ移す（中身は動かさない）
            self.set_focus_slot(from);
            return;
        }
        self.place_session(id, self.focus_slot);
    }

    /// **宛先を明示してセッションを出す。** 元居たスロットは空になり、
    /// フォーカスも宛先へ移る。
    ///
    /// [`Self::show_session`] と違い、既に別のスロットへ出ていても**動かす**:
    /// こちらの入口はドラッグ＆ドロップ（[`drop_session`]）＝ どこへ置くかを
    /// ユーザーが名指しした操作なので、「見えているから動かさない」の遠慮は要らない
    pub(crate) fn place_session(&mut self, id: &SessionId, to: usize) {
        if to >= self.slots.len() {
            return; // 配置がまだ組まれていない（起動列の途中）
        }
        let from = self.slot_of(id);
        if from == Some(to) {
            self.set_focus_slot(to);
            return;
        }
        // **押し出される窓へ focus out を送るのが先。** これを落とすと、
        // 追い出された側と入ってきた側の両方が「自分が端末を持っている」と
        // 思い込んだまま残る（次に戻したとき focus in が 2 回続けて届く）
        if self.focus == Focus::Terminal
            && let Some(at) = self.focused_window()
        {
            self.windows[at].send_focus(false);
        }
        self.slots[to] = Slot::Session(id.clone());
        // 元居たスロットは空にする（そこへ何かを送り込まない ＝ 触っていない
        // セッションは 1 つも動かない）
        if let Some(from) = from {
            self.slots[from] = Slot::Empty;
        }
        self.save_slots();
        // フォーカスも宛先へ。**`set_focus_slot` は通さない**（focus out は上で
        // 済ませてあるので、通すと離れる側へ 2 回続けて届く）
        self.focus_slot = to;
        self.mark_focus_read();
        self.focus_terminal_on(to);
        self.resize_sessions();
    }

    /// フォーカススロットを移す（PTY への focus in/out 通知つき）。
    /// **[`Self::focus`] は触らない**（サイドバーにいるかどうかは `Alt+←/→` の担当）
    pub(crate) fn set_focus_slot(&mut self, to: usize) {
        if to == self.focus_slot || to >= self.slots.len() {
            return;
        }
        if self.focus == Focus::Terminal
            && let Some(at) = self.focused_window()
        {
            self.windows[at].send_focus(false);
        }
        self.focus_slot = to;
        self.mark_focus_read();
        self.focus_terminal_on(to);
    }

    /// **フォーカススロットに出ている行を既読にする**（[`mark_read`] の規則の実装）。
    ///
    /// 呼ぶのは「フォーカススロットの中身が変わり得た直後」＝ フォーカスの移動と
    /// 配置の変更。**スロットが複数のとき、見に行く操作はこの 2 つしか無い**:
    /// セッションは既に画面へ出ているので [`open_session`] を通らない
    /// （どちらも通していなかった頃は、**サイドバーの行を押し直すまで `●` が
    /// 消えなかった**）
    fn mark_focus_read(&mut self) {
        if let Some(id) = self.shown_session().cloned() {
            mark_read(self, &id);
        }
    }

    /// スロット `to` にフォーカスがあるとき、その窓へ focus in を送る
    fn focus_terminal_on(&mut self, to: usize) {
        if self.focus != Focus::Terminal || self.focus_slot != to {
            return;
        }
        if let Some(at) = self.focused_window() {
            self.windows[at].send_focus(true);
        }
    }

    /// new session 画面をやめて空スロットへ戻す（Esc）
    pub(crate) fn leave_new_view(&mut self) {
        if self.focus_is_new() {
            self.put_in_focus(Slot::Empty);
        }
    }

    /// フォーカス中のスロットの内側サイズ `(rows, cols)`（起動する PTY の初期サイズ）
    pub(crate) fn focus_slot_size(&self) -> (u16, u16) {
        let rects = self.slot_rects();
        rects
            .get(self.focus_slot)
            .or_else(|| rects.first())
            .map_or((1, 1), |r| Self::inner_size(*r))
    }

    /// フォーカス中のスロットが new session 画面か
    pub(crate) fn focus_is_new(&self) -> bool {
        matches!(self.slots.get(self.focus_slot), Some(Slot::New(_)))
    }

    /// フォーカス中のスロットの new session 画面
    pub(crate) fn focused_new(&mut self) -> Option<&mut NewState> {
        match self.slots.get_mut(self.focus_slot) {
            Some(Slot::New(state)) => Some(state),
            _ => None,
        }
    }

    /// そのセッションを映しているスロット
    fn slot_of(&self, id: &SessionId) -> Option<usize> {
        self.slots.iter().position(|s| s.session() == Some(id))
    }

    /// フォーカス中のスロットが映している窓（[`Self::windows`] の添字）
    pub(crate) fn focused_window(&self) -> Option<usize> {
        self.shown_session()
            .and_then(|id| self.windows.iter().position(|w| &w.session_id == id))
    }

    /// **今フォーカススロットに出ているセッション**（空 / New 画面なら None）。
    /// 「キー入力の宛先」と「ユーザーが見ている行」がどちらもこの 1 つの判断から出る
    pub(crate) fn shown_session(&self) -> Option<&SessionId> {
        self.slots.get(self.focus_slot).and_then(Slot::session)
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
    let Some(window) = app.focused_window().map(|at| &app.windows[at]) else {
        return;
    };
    // 打鍵・貼り付けは「今」に対する操作なので、スクロールバックを見ていたら
    // 最下部へ戻す（子の応答が画面外で起きて止まって見えるのを防ぐ）。
    // **マウス転送はここを通らない**ので、ホイールで戻した位置は保たれる
    window.parser.lock_recover().screen_mut().set_scrollback(0);
    if window.send(bytes).is_ok() {
        return;
    }
    let id = window.session_id.clone();
    set_notice(app, format!("could not write to session {id}; closing its window"));
    close_window_of(app, &id);
}

/// ホスト端末のフォーカス変化を中継する。**届くのはフォーカススロットの窓だけ**
/// （裏のスロットは端末を持っていないので、通知しても意味が無い）
fn forward_host_focus(app: &mut App, gained: bool) {
    if app.focus != Focus::Terminal {
        return;
    }
    if let Some(at) = app.focused_window() {
        app.windows[at].send_focus(gained);
    }
}

/// この周に描くか。**再描画は「PTY に新出力」「UI イベント」「無変化でも
/// [`IDLE_REDRAW`] 周期（ドットの点滅・通知の期限切れ等）」のときだけ**
/// （無条件 60fps は claude 画面全体の再構築が毎フレーム走り重い）。
///
/// `holding` は**すべてに優先する**: 子が画面を作り替えている最中に掴んだ
/// フレームはカーソルが中間位置で確定し、IME の変換窓がそこへ飛ぶ
/// （判断は [`crate::session::Session::holds_frame`]、上限も向こうが持つので
/// ここが false を返し続けることはない）
/// `idle_after` は無変化でも描き直す間隔（何か動いている間だけ短くする ＝
/// [`ANIMATION_REDRAW`] / [`IDLE_REDRAW`]）
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
                force_draw = true;
            }
            app.last_live_scan = std::time::Instant::now();
        }
        // **hook の書き込みに気づいたら周期を待たずに読み直す。** ペインの中で
        // `/resume` `/clear` すると agent は新しい会話の `SessionStart` を
        // その場で撃つので、これが「行が別の会話へ移った合図」になる
        // （行は増えない ＝ `adopt_conversations`）。
        // 見るのはファイルの長さと更新時刻だけ（中身は読まない）ので毎周でも安い
        let hooks_changed = hook_store_changed(&mut app.hook_stamp, app.source.hook_stamp());
        if hooks_changed {
            app.last_scan = instant_ago(SCAN_INTERVAL);
        }
        if app.last_scan.elapsed() > SCAN_INTERVAL {
            refresh_sessions(app);
            // 一覧を読み直した直後に hook の state を載せる。**順序に意味がある**:
            // 読み直しは丸ごとの置き換えなので、先に載せるとその場で上書きされる。
            // **名前の読み直しより前**でもある: 行が今どの会話に載っているかを
            // ここで確定させてから、その会話の transcript を読ませる
            // （逆にすると `/clear` の周だけ古い会話の名前が 1 周残る）。
            // **読み直すのはファイルが実際に動いた周だけ**（何も起きていない
            // 2 秒周期のたびに全読み + JSON パースをやり直さない）
            if hooks_changed {
                adopt_hook_states(app);
            }
            refresh_transcripts(app);
            app.last_scan = std::time::Instant::now();
            force_draw = true; // 並びが変わったら即描画（表示と行データのずれを残さない）
        }
        // セッションの中の agent が使う口（`ccdesk send` / `read` / `list` / `new`）。
        // **一覧と名前を確定させた後に置く**: 公開する名前は直前の
        // `refresh_transcripts` の結果で、逆にすると 1 周古い名前を公開する
        serve_relay(app);
        // 押された OS 通知が指していた行を開く。**押された瞬間ではなくここで開く**:
        // クリックは WinRT のイベントスレッドで届くので、そこから `App` を触らない
        // （[`crate::notify`]）。
        //
        // **前面へ戻すのは行が開けたかに関わらず。** 押した人が求めているのは
        // 「ccdesk を見せること」で、開けなかった理由（別の窓で動いている等）は
        // 下部バーに出る ＝ 前に出ていなければその通知も読めない
        for id in crate::notify::clicked() {
            crate::notify::raise_terminal();
            // **通知は引っ込むまで押せる ＝ その間に行が消えていることがある。**
            // 消えた行で `open_session` が黙って false を返すと、端末だけが前へ
            // 出て何も起きない ＝ 押した人には固まったように見える。開けない
            // 他の理由（別の窓で動いている等）は `ensure_window` が下部バーへ
            // 出すので、ここが足すのは「もう一覧に無い」1 つだけ
            if !open_session(app, &id) && app.row(&id).is_none() {
                set_notice(app, format!("session {id} is no longer in the list"));
            }
            force_draw = true;
        }
        // 起こした子が端末を掴んだら門番を降ろす（前景では宛先は起動の時点で
        // 決まっているので、待つのは「子が入力を読める状態になるまで」だけ）
        if app.input_gate.is_some()
            && let Some(id) = app
                .focused_window()
                .map(|at| &app.windows[at])
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
            let usage: BTreeMap<Kind, Usage> = Kind::ORDER
                .into_iter()
                .map(|kind| (kind, app.source.usage(kind)))
                .collect();
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
            // **据え置きを降ろす規則はここに要らない。** 成立条件（押した時点の版の
            // まま）を [`AgentUpdate::Stalled`] 自身が持っているので、版が動けば
            // 条件が外れて行が最新表示へ戻る
            force_draw = true;
        }
        // ccdesk 自身の更新の失敗を下部バーへ出す。成功は版行の案内
        // （"restart to update"）が伝えるので
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
        // agent 本体（`<agent> update`）の失敗も同じ場所へ出す。**成功は版行が
        // 伝える**（再取得した版が新しくなり ⟳ が消える）ので、ここは失敗だけ。
        // 出したら Idle へ戻す ＝ 失敗した更新はもう一度押せる（ccdesk 自身の
        // 更新と同じ作法）。据え置き（Stalled）は版行に残す材料なので取らない
        let agent_failure = app.agent_update.iter().find_map(|(kind, state)| {
            let mut state = state.lock_recover();
            match &mut *state {
                AgentUpdate::Failed(msg) => {
                    let msg = msg.clone();
                    *state = AgentUpdate::Idle;
                    Some(msg)
                }
                // 据え置きは**状態を消さない**（版行に残す材料）。出したことだけ記録する
                AgentUpdate::Stalled { version, announced: announced @ false } => {
                    let msg = stale_update_message(kind.backend().update_program(), version);
                    *announced = true;
                    Some(msg)
                }
                _ => None,
            }
        });
        if let Some(msg) = agent_failure {
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
        // 前景セッションのライブ状態を取り込む（state 変化の即時反映）。
        // **生死はここでは見ない**（前景セッションは自分の子なので `try_wait` が真実）
        if app
            .agents_dirty
            .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            // **値と時刻を一緒に受け取る。** run ループ自身は時計を読まない
            // （[`App::agents_observed_at`]）
            let snapshot = app.agents_shared.lock_recover().clone();
            app.agents = snapshot.agents;
            app.agents_observed_at = snapshot.observed_at;
            force_draw = true;
        }
        // 通知は hook の変化ではなく、画面と同じ有効 state の変わり目で撃つ。
        // status による許可解除、窓の停止、一覧の増減もここなら取りこぼさない
        if force_draw {
            update_notifications(app);
        }
        // claude が画面を作り替えている最中は掴まない（[`Session::holds_frame`]）。
        // 途中で掴むとカーソルが中間位置で確定し、IME の変換窓がそこへ飛ぶ。
        // **見るのはフォーカススロットの窓 1 つだけ。** 全スロットを見ると、
        // 4 枚のうち誰か 1 人が常に書いている状況で描画が止まり続ける。
        // 待つ理由（カーソルが中間位置で確定し IME の変換窓が飛ぶ）は
        // カーソルを出している窓にしか無いので、これで足りる
        let holding = app
            .focused_window()
            .is_some_and(|at| app.windows[at].holds_frame(last_draw.elapsed()));
        // **合図（dirty）を降ろすのは実際に描く周だけ**なので、読むのはここ。
        // 見送る周で降ろすと、次の出力が来るまで画面が古いままになる
        let pty_dirty = app
            .windows
            .iter()
            .any(|w| w.dirty.load(std::sync::atomic::Ordering::Relaxed));
        // 何か動いている（ドットの点滅・使用率取得中スピナー）間だけ短い周期で
        // 描き直す。出ていない間は通知の期限切れ等の低頻度な変化しか無いので、
        // 1 秒粒度で足りる
        let idle_after = if app.animating {
            ANIMATION_REDRAW
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
                    // 行き先が無い向きは何もしない（配置の端）
                    Some(Reserved::Slot(dir)) => {
                        if let Some(to) = app.layout.neighbor(app.focus_slot, dir) {
                            app.set_focus_slot(to);
                        }
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
                if app.focus_is_new() {
                    handle_new_view_key(app, &key)?;
                    continue;
                }
                // 起動処理中の打鍵は溜める（子が掴んだ時点でその宛先へ流れる）
                if hold_input_while_starting(app, || HeldInput::Key(key)) {
                    continue;
                }
                // フォーカスがターミナル側にあるときだけ PTY へ流す
                if app.windows.is_empty() {
                    continue;
                }
                let Some(at) = app.focused_window() else {
                    continue;
                };
                let bytes = encode_key(&key, &app.windows[at].parser.lock_recover());
                if !bytes.is_empty() {
                    send_to_active(app, &bytes);
                }
            }
            Event::Paste(text) => {
                // New 画面の D&D/貼り付けの解釈は new_view 側（キー・マウスと同じ場所）
                if let Some(state) = app.focused_new() {
                    state.handle_paste(&text);
                    continue;
                }
                if app.focus != Focus::Terminal {
                    continue;
                }
                // 打鍵と同じ門番（貼り付けのほうが 1 回で送る量が多い ＝ 素通しの害が大きい）。
                // **同じ列に積む**ので、打鍵と貼り付けの順序は打った順のまま
                if hold_input_while_starting(app, || HeldInput::Paste(text.clone())) {
                    continue;
                }
                if app.windows.is_empty() {
                    continue;
                }
                // sanitize と bracketed paste の包みはキー入力と同じ keys 側
                // （「入力を VT バイト列にする」知識を run ループに置かない）
                let Some(at) = app.focused_window() else {
                    continue;
                };
                let bytes =
                    crate::keys::encode_paste(&text, &app.windows[at].parser.lock_recover());
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
                // **前面に居るウィンドウを控える唯一の契機。** 端末がフォーカスを
                // 得た瞬間なら、前面のウィンドウは ccdesk を映しているそれ
                // （通知クリックで戻す先。[`crate::notify`]）
                crate::notify::remember_terminal_window();
                forward_host_focus(app, true);
            }
            Event::FocusLost => forward_host_focus(app, false),
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
    /// サイドバー ⇄ メインビュー
    Focus(Focus),
    /// スロット間の移動
    Slot(crate::panes::Dir),
}

/// **Shift の有無で触るものが分かれているのが要点。**
///
/// `Alt+←/→` は「サイドバーにいるか」だけを、`Alt+Shift+方向` は
/// 「どのスロットが宛先か」だけを動かす。1 つのキーが両方へ書く形にすると、
/// サイドバーへ抜ける道中で宛先スロットが壊れ、**最左列以外のスロットへ
/// セッションを開けなくなる**（`Alt+←` を 2 回押してサイドバーへ行った時点で、
/// 宛先が左列のスロットに変わってしまうため）。
///
/// 行き先が無い向き（1 枚のときの `Alt+Shift+↑` 等）も**予約は外さない**:
/// 外すとこの関数が配置を見る必要が生じ、`&KeyEvent` だけを見る純関数で
/// なくなる。その純粋性は「素通しするキーを黙って減らしていないか」を
/// テストで固定する土台なので崩さない
fn reserved_key(key: &KeyEvent) -> Option<Reserved> {
    use crate::panes::Dir;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        // Shift の付いた `Ctrl+Shift+Q` は予約しない（1 打鍵でも減らす）
        KeyCode::Char('q') if ctrl => Some(Reserved::Quit),
        // Alt+Shift が先（後ろに置くと Alt だけの腕に食われる）
        KeyCode::Left if alt && shift => Some(Reserved::Slot(Dir::Left)),
        KeyCode::Right if alt && shift => Some(Reserved::Slot(Dir::Right)),
        KeyCode::Up if alt && shift => Some(Reserved::Slot(Dir::Up)),
        KeyCode::Down if alt && shift => Some(Reserved::Slot(Dir::Down)),
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
    UpdateAgent,
}

impl Enter {
    /// 下部バーへ出す語（`Enter <label>` の形で並ぶ）
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Menu => "menu",
            Self::NewSession => "new session",
            // どちらの版行も利用者から見れば「更新する」1 つの動作
            Self::UpdateCcdesk | Self::UpdateAgent => "update",
        }
    }
}

/// いま選択している位置で `Enter` が起こすこと（何も起きない位置は `None`）。
///
/// **キーボードの実行と下部バーの案内が読む唯一の写像。** 押しても何も起きない行は
/// ここが `None` を返すことで表す ＝ 更新の無い版行（[`SidebarRow::Inert`]）と
/// 一覧の外を指す位置（かつてのアカウント行。[`SidebarPos::row`] が `Option` の
/// まま残っている理由）が同じ 1 つの答えを共有する
pub(crate) fn selected_enter(app: &App) -> Option<Enter> {
    let SidebarPos::Row(row) = app.selection;
    match app.sidebar_rows.get(row)?.action()? {
        RowAction::New => Some(Enter::NewSession),
        RowAction::Open(_)
        | RowAction::Project(_)
        | RowAction::ToggleGroup
        | RowAction::ChooseLayout => Some(Enter::Menu),
        RowAction::UpdateCcdesk => Some(Enter::UpdateCcdesk),
        RowAction::UpdateAgent(_) => Some(Enter::UpdateAgent),
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
    let SidebarPos::Row(row) = app.selection;
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
        RowAction::ToggleGroup => open_popup(app, PopupKind::State, anchor_y),
        RowAction::ChooseLayout => {
            // 押せる項目の判断は**開いた時点の端末の大きさ**で決める
            // （矩形の正本は ui::pane_rect、判定の正本は Layout::fits）
            let area = crate::ui::pane_rect(app);
            let fits = crate::panes::Layout::ORDER
                .into_iter()
                .filter(|l| l.fits(area, app.split))
                .collect();
            let kind = PopupKind::Layout {
                current: app.layout,
                fits,
            };
            open_popup(app, kind, anchor_y);
        }
        // 見出し行はメニューを開くだけ。**フォーカスは移さない**（メニューがキーを受ける）
        RowAction::Project(cwd) => open_project_popup(app, cwd, anchor_y),
        // 更新行はその場で実行するだけ（右ペインを切り替えない）
        RowAction::UpdateCcdesk => start_ccdesk_update(app),
        RowAction::UpdateAgent(kind) => start_agent_update(app, kind),
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
    let Some(state) = app.slots.get(app.focus_slot).and_then(|slot| match slot {
        Slot::New(state) => Some(state),
        _ => None,
    }) else {
        return Ok(());
    };
    let cwd = state.cur_dir.clone();
    let prompt = state.prompt.text.trim().to_string();
    // **どの agent を起こすかは New 画面の選択が正本**
    let kind = state.kind;
    dispatch_session(app, kind, cwd, prompt);
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
            // 溜める（run ループが最初の出力で降ろす。理由は
            // [`hold_input_while_starting`]）
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
    let mut fresh = app.source.sessions();
    let dropped: Vec<SessionRow> = rows_dropped_while_open(&fresh, &app.sessions, &open)
        .into_iter()
        .cloned()
        .collect();
    restore_conversations(&mut fresh, &app.sessions);
    app.sessions = fresh;
    if !dropped.is_empty() {
        app.sessions.extend(dropped);
        save_sessions(app);
    }
}

/// 読み直しで会話を失った行へ、**メモリ上の写しが知っている会話を戻す**
/// （[`refresh_sessions`] の判断。副作用を持たないので単体で検査できる）。
///
/// **保存されるのは確かめた会話だけ**（[`crate::sessions::Conversation::Observed`]）
/// なので、起動直後の `Assigned`（ccdesk が採番して `--session-id` で渡した値）は
/// ディスクに載らない。読み直しは丸ごとの置き換えなので、戻さないと
/// **2 秒ごとに会話を失う**。
///
/// 普段は同じ周期の `adopt_conversations` が hook から `Observed` を戻すので
/// 気づけないが、**hook が一度も来ない行は永久に会話を失う**（表示名が
/// `new session` に固定され、開き直しが agent のピッカーになる）。hook の注入は
/// 実際に失敗し得る（[`hook_settings`] が None を返す環境、codex なら exe パスに
/// 空白がある場合）ので、hook 1 本に全部を賭けない。
///
/// **戻すのは `Unknown` の行だけ。** ディスクが会話を持っているなら、それは
/// 他インスタンスが確かめた値かもしれない ＝ こちらの古い写しで踏み潰さない
fn restore_conversations(fresh: &mut [SessionRow], mine: &[SessionRow]) {
    for row in fresh
        .iter_mut()
        .filter(|row| row.conversation.id().is_none())
    {
        if let Some(known) = mine.iter().find(|m| m.session_id == row.session_id) {
            row.conversation = known.conversation.clone();
        }
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
    if app.hook_states.any_row_went_idle_since(&previous) {
        app.source.note_turn_finished();
    }
    adopt_conversations(app);
    let shown: Option<SessionId> = app.shown_session().cloned();
    if let Some(id) = shown {
        mark_read(app, &id);
    }
}

/// その行の現在の有効 state。
///
/// hook・記録／`status` から読んだ現在値・PTY・別インスタンス救済の組み立てを
/// ここ 1 箇所に置く。
///
/// **通知はもうこれを読まない。** 表示の state は「今どうなっているか」しか
/// 答えず、その中には**ユーザー自身が開いたダイアログ**も入る（claude は
/// `/config` `/resume` でも `status: waiting` を書く）。呼び出すかどうかは
/// agent が名乗った出来事の側で決める（[`update_notifications`]）
pub(crate) fn effective_row_states(
    app: &mut App,
) -> std::collections::HashMap<SessionId, State> {
    struct WindowView {
        session_id: SessionId,
        alive: bool,
        pty: crate::poll::PtyHint,
        launched_at: u64,
    }
    let windows: Vec<WindowView> = app
        .windows
        .iter_mut()
        .map(|window| WindowView {
            session_id: window.session_id.clone(),
            alive: window.alive(),
            pty: window.pty_hint(),
            launched_at: window.started_at,
        })
        .collect();
    let mut states = std::collections::HashMap::new();
    for row in &app.sessions {
        let window = windows.iter().find(|window| window.session_id == row.session_id);
        let (status, status_at) = live_status(&app.agents, row.conversation.id());
        let observed = observed_state(app, row, status, status_at);
        let run = window
            .filter(|window| window.alive)
            .map(|window| Run {
                hook: app
                    .hook_states
                    .get(&row.session_id, Some(window.launched_at)),
                observed,
                pty: Some(window.pty),
            })
            .or_else(|| {
                app.fixed_states.get(&row.session_id).map(|state| Run {
                    // 撮影用の固定 state は「その行を動かしている実行」の代役
                    hook: Some(crate::hooks::Reported { state: *state, at: 0 }),
                    observed: None,
                    pty: None,
                })
            })
            // **別インスタンスの行の救済は `status` にしか乗せられない。**
            // 記録から読んだ現在値は生死を語らない（先週終わった会話の rollout にも
            // `task_complete` が残っている）ので、ここの条件にすると**閉じた行が
            // 永久に Idle** になる。`~/.claude/sessions/` の方はプロセスが生きて
            // いる間しか存在しないので、条件として成立する
            .or_else(|| {
                let stopped_at = app.stopped_at.get(&row.session_id).copied().unwrap_or(0);
                (!status.is_empty() && app.agents_observed_at > stopped_at).then_some(Run {
                    hook: None,
                    observed,
                    pty: None,
                })
            });
        states.insert(row.session_id.clone(), crate::poll::row_state(run));
    }
    states
}

/// その行について **agent 自身が名乗っている現在値**（[`Run::observed`]）。
///
/// **2 つの出どころを引いて新しい方を採る**ので、呼び手は agent ごとに分岐しない:
/// claude は `~/.claude/sessions/` の `status`、codex は rollout の末尾
/// （[`crate::backend::Backend::record_states`]）。行が両方を持つことは無いので、
/// 実際にはどちらか一方が必ず None になる ＝ 「新しい方」は分岐の代わりに置いた
/// 規則であって、競合の調停ではない
fn observed_state(
    app: &App,
    row: &crate::sessions::SessionRow,
    status: &str,
    status_at: u64,
) -> Option<(State, u64)> {
    let live = crate::poll::state_of_status(status).map(|state| (state, status_at));
    let recorded = row
        .conversation
        .id()
        .and_then(|conversation| app.titles.live_state(conversation));
    match (live, recorded) {
        (Some(a), Some(b)) => Some(if b.1 > a.1 { b } else { a }),
        (found, None) | (None, found) => found,
    }
}

/// 行の出来事を OS の通知へ流す（[`crate::notify`]）。
///
/// **撃つのは agent が名乗った出来事**であって、行の state の変わり目ではない。
///
/// # なぜ state の変わり目をやめたか
///
/// 表示の state は「今どうなっているか」しか答えず、**ユーザー自身の操作でも
/// 動く**。claude は**開いているダイアログがあれば種類を問わず** `status: waiting`
/// を書く（決定条件は [`crate::claude_format`]）ので、ペインの中で `/config` や
/// `/resume` を開くと「Needs input」が鳴り、閉じると Waiting → Idle で
/// 「Turn finished」が鳴っていた（報告された症状）。**呼ばれてもいないのに
/// 呼び出される**のは通知としては壊れている。
///
/// 変わり目で撃つ限り、この手の誤爆は個別に潰すしかなく、実際に
/// `settled`（起動を見届けたか）・`interrupted`（中断か完了か）・
/// `owned`（自分の窓か）という 3 つの補正ビットが state の側に生えていた。
/// **どれも「この変化はユーザーを呼ぶ理由か」を後から推し量るための道具**で、
/// 推し量る必要があること自体が材料の選び方の誤りだった。
///
/// # 今の材料
///
/// hook は `PermissionRequest` / `Notification` で**呼んでいる**ことを、
/// `Stop` / `StopFailure` で**終わった**ことを、agent 自身が名乗る
/// （[`crate::hooks::HOOK_EVENTS`] の `alert` 列）。ユーザーが開いたダイアログは
/// hook を 1 つも撃たないので、**構造的に鳴らない**。
///
/// 補正ビットも要らなくなる:
///
/// | かつての補正 | 今そうなる理由 |
/// |:--|:--|
/// | 起動しただけで鳴る | 起動は `SessionStart`（`alert` を持たない）＝ 材料に上がらない |
/// | Esc 中断が完了になる | 中断は `Interrupt`（`alert` を持たない）／claude は何も撃たない |
/// | 他インスタンスの行で 2 回鳴る | 窓を持つ行しか見ない（下の `windows`） |
/// | 起動直後に一斉に鳴る | 窓を起こした時刻より古い記録は撃たない |
fn update_notifications(app: &mut App) {
    if !app.notify.any() {
        return;
    }
    // **窓を持つ行だけ**（＝ この ccdesk が動かしている行）。他インスタンスの行は
    // 一覧には出るが撃たない: 同じ出来事を持ち主の ccdesk も見ているので、
    // 撃つと 2 プロセスで 2 回鳴る
    let windows: Vec<(SessionId, u64)> = app
        .windows
        .iter_mut()
        .filter_map(|window| {
            window
                .alive()
                .then(|| (window.session_id.clone(), window.started_at))
        })
        .collect();
    let hook_states = std::mem::take(&mut app.hook_states);
    let fired = announce(
        &windows,
        |id| hook_states.alert(id),
        &mut app.announced,
        app.notify,
    );
    app.hook_states = hook_states;
    for (id, kind) in fired {
        let Some(row) = app.sessions.iter().find(|row| row.session_id == id) else {
            continue;
        };
        let project = crate::ui::leaf_name(&row.cwd).unwrap_or_else(|| row.cwd.clone());
        crate::notify::post(kind, &project, &app.titles.of(row), &id);
    }
}

/// **撃つものを決める**（保管も窓も画面も触らない）。撃った出来事は `announced` に
/// 刻む ＝ **判定と写しの更新を 1 つの関数が返す**（2 つに割ると、撃った出来事と
/// 次の周が比べる相手が別々の規則で決まる形になる）。
///
/// `alert_of` はその行で最後に起きた呼び出し（[`crate::hooks::HookStates::alert`]）。
///
/// **ここが見るのは時刻だけで、種類は見ない。** 同じ待ちを 2 つの hook が名乗る
/// （`PermissionRequest` と `Notification`）ことは起きるが、その畳み込みは
/// 保管へ書く側が済ませてある（[`crate::hooks::Alert`]）＝ ここへ来る時点で
/// 時刻が違えば、それは別の呼び出し
fn announce(
    windows: &[(SessionId, u64)],
    alert_of: impl Fn(&SessionId) -> Option<(crate::notify::Kind, u64)>,
    announced: &mut std::collections::HashMap<SessionId, u64>,
    wanted: crate::notify::Wanted,
) -> Vec<(SessionId, crate::notify::Kind)> {
    let mut fired: Vec<(SessionId, crate::notify::Kind)> = Vec::new();
    for (id, launched_at) in windows {
        let Some((kind, at)) = alert_of(id) else {
            continue;
        };
        // **前回の実行の呼び出しを撃たない。** 保管はセッションが終わっても残るので、
        // 窓を起こし直した行には古い `Stop` がそのまま載っている ＝ ここが無いと
        // ccdesk の起動やセッションの開き直しで一斉に鳴る
        if at < *launched_at {
            continue;
        }
        // 同じ出来事を毎周撃たない（撃った時刻を覚えるのはここ 1 箇所）
        if announced.get(id).copied() >= Some(at) {
            continue;
        }
        // **設定で切っていても「撃った」ことにする。** 覚えるのが「知らせた出来事」
        // ではなく「見た出来事」なら、設定を途中で入れ替えても過去の出来事が
        // 遡って鳴らない
        announced.insert(id.clone(), at);
        if wanted.wants(kind) {
            fired.push((id.clone(), kind));
        }
    }
    // 窓の無くなった行の記録は落とす（覚えているのは「今動いている行で、どの
    // 出来事まで撃ったか」だけ ＝ 行数で有界）
    announced.retain(|id, _| windows.iter().any(|(open, _)| open == id));
    // **並びを固定する**（同じ入力で同じ順に撃つ）
    fired.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    fired
}

/// ライブ状態と時刻は必ず対で取り出す。照合の鍵は行 ID ではなく会話 ID。
pub(crate) fn live_status<'a>(
    agents: &'a [AgentInfo],
    conversation: Option<&str>,
) -> (&'a str, u64) {
    let Some(conversation) = conversation else {
        return ("", 0);
    };
    agents
        .iter()
        .find(|agent| agent.is_interactive() && agent.session_id == conversation)
        .map_or(("", 0), |agent| (agent.status.as_str(), agent.status_at))
}

/// hook が名乗った会話を行へ写す（[`crate::sessions::Conversation::Observed`]）。
///
/// **ペインの中の `/clear` `/resume` `/new` に追随する唯一の口。** どちらの agent も
/// 会話が変われば `SessionStart` を撃ち、その payload は**その時点の**会話 ID を
/// 運ぶ（両方実測）。記録の鍵は `CCDESK_ROW` ＝ 行なので、行はそのまま、
/// 載っている会話だけが差し替わる ＝ **サイドバーに行は増えない**。
///
/// **claude と codex で分岐しない**のが以前との違い。かつては codex の行だけを
/// 写していた（claude は「行 ID ＝ 会話 ID」だったので写す意味が無かった）が、
/// その前提こそがペイン内の切り替えを pid 相関で追いかける羽目になった原因で、
/// 行 ID を会話から切り離した今は両方が同じ 1 本の経路を通る
fn adopt_conversations(app: &mut App) {
    let mut changed = false;
    for row in &mut app.sessions {
        let Some(observed) = app.hook_states.conversation(&row.session_id) else {
            continue;
        };
        changed |= row.conversation.observe(observed);
    }
    if changed {
        save_sessions(app);
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
    // 1 周期に読む量の上限。UI スレッド ＝ 最初の描画を数百 MB の read で止めない。
    // **どの行の何を先に読むかは [`Titles`] 側の判断**（予算が足りないときに
    // 何を捨てるかは行をまたいだ順序で決まるので、ここで配ると呼び手が
    // 順序を守る責任を負う）。段の表は [`crate::title::Titles::refresh_all`]
    let mut budget = crate::title::SCAN_BUDGET;
    let changed = titles.refresh_all(&mut app.sessions, &mut budget);
    app.titles = titles;
    if changed {
        save_sessions(app);
    }
}

/// その行を既読にする。**規則は 1 つ ＝ 「フォーカススロットにその行が出た」**で、
/// そこへ至る道が複数ある: 行を開く（開き方は問わない）・フォーカススロットの
/// 中身が変わる（[`App::mark_focus_read`]）・出ている行へ hook が届く
/// （[`adopt_hook_states`]）。**`mark as read` だけが規則の外**で、
/// 出ていない行を直接既読にする。
///
/// 進めるのは `last_opened_at` だけで **`updated_at` は触らない**: 既読は
/// 行の中身（cwd・transcript・ピン留め）を 1 つも変えないので、ここで
/// `updated_at` を進めると [`edit_row`] の doc が言う踏み潰しをこちらが起こす
/// （中身は古いままの写しが後勝ち判定に勝ち、他インスタンスが直前に書いた
/// ピン留め等が消える）。
///
/// **では既読はどうやって他インスタンスへ伝わるか**: `last_opened_at` は
/// [`crate::sessions`] の `merge_sessions` が**行の後勝ちとは別軸で**大きい方を
/// 採る。既読は「行の内容」ではなく「このユーザーがどこまで見たか」なので、
/// 内容の新旧とは別に合流させるのが正しい。
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
/// （[`crate::sessions`] の `merge_sessions`）。**進めるのは before/after を比べて
/// 行の中身が実際に変わったときだけ**: 変わっていないのに進めてしまうと、
/// 他インスタンスが先に書いた本当の変更（ピン留め等）より `updated_at` だけが
/// 新しくなり、後勝ち判定でこちらの（中身は古いままの）写しが勝って
/// 相手の変更を踏み潰す。
///
/// **`mark as read` はここを通らない別経路**（[`mark_read`]）。あちらは
/// `updated_at` を進めない ＝ ここで言う踏み潰しを起こさないための別扱いで、
/// 既読は `merge_sessions` が別軸（`last_opened_at` の大きい方）で合流させる。
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
    let SidebarPos::Row(row) = app.selection;
    row_y(row, app.sidebar_header_rows, app.sidebar_scroll)
}

/// 指定フォルダ・プロンプトで前景セッションを 1 本起こす
/// （見出しメニューの new session は空プロンプトで直接ここに来る）。
///
/// **PTY の起動は同期**（数 ms）。結果を待つ別スレッドが要らないので、
/// 起動と反映が 1 本の流れに収まる
fn dispatch_session(app: &mut App, kind: Kind, cwd: String, prompt: String) {
    dispatch_session_into(app, kind, cwd, prompt, None);
}

/// 起こした窓の出し先を名指しできる版（`at` が `None` なら今までどおり
/// フォーカススロットへ）。
///
/// **`at` を渡す側（ドロップ）は、起こしてから配置を育てる**: 逆にすると
/// 起動に失敗したとき空のペインだけが増えて残る（[`drop_session`] と同じ順序）。
/// 起こす前に大きさが要るのは PTY の初期サイズのため ＝ 落とし先の内寸は
/// 配置を変える前に [`drop_rect`] から分かる
fn dispatch_session_into(
    app: &mut App,
    kind: Kind,
    cwd: String,
    prompt: String,
    at: Option<DropAt>,
) {
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
    // 起こす先の内寸（落とし先が決まっているならそちら ＝ 起こした直後に
    // 作り直させない）
    let size = at
        .and_then(|target| drop_rect(app, target))
        .map_or_else(|| app.focus_slot_size(), App::inner_size);
    let launched = if app.source.spawns_sessions() {
        start_foreground(app, kind, &cwd, &prompt, size)
    } else {
        Ok(None)
    };
    // 起こせたものを画面へ出す。**落とし先を名指しされているときだけ**、
    // 配置をここで育てる（起こせなかったら育てない）
    if let Ok(Some(id)) = &launched {
        let id = id.clone();
        match at.and_then(|target| make_room(app, target)) {
            Some(to) => app.place_session(&id, to),
            None => app.show_session(&id),
        }
    }
    apply_launch(app, cwd, launched);
}

/// 新規の前景セッションを起こして一覧へ行を足す。
///
/// **UUID は ccdesk が採番する**（`claude --session-id` へ渡した値がそのまま
/// transcript の `sessionId` になる ＝ 行と claude 側の記録が同じ鍵で結びつく）。
/// 新規生成なので同 cwd の既存 transcript と衝突しない。
///
/// **行を足すのは起動できてから**（起動できなかったセッションを一覧に残さない）。
///
/// **画面のどこへ出すかは含まない**（[`ensure_window`] と同じ切れ目）: 出し先が
/// 違う入口が起こし方を 2 本持たないため。`size` は PTY の初期サイズ
fn start_foreground(
    app: &mut App,
    kind: Kind,
    cwd: &str,
    prompt: &str,
    size: (u16, u16),
) -> Launched {
    let session_id = SessionId::new(uuid::Uuid::new_v4().to_string());
    // state を取る hook を注入する（statusLine は載せない ＝ ユーザーの
    // statusline を奪わない。[`crate::hooks::inject_settings`]）
    let injection = hook_settings(app);
    let inject = injection.as_ref().map(as_inject);
    let (rows, cols) = size;
    let spawn = kind.spawn_command(&session_id, cwd, Launch::New { prompt }, inject.as_ref());
    // **渡した会話 ID を控えるのは起こす前。** 起こしてから読み直せる場所は無い
    // （argv には出ているが、そこから読み戻すのは同じ知識の 2 本目になる）
    let conversation = spawn.conversation;
    let window = Session::spawn(&session_id, spawn.cmd, rows, cols)
        .map_err(|e| format!("failed to start session: {e}"))?;
    // **名前は入れない。** 1 ターン目が終わるまで transcript は無いので、
    // それまでこの行は [`UNTITLED`] で出る（起動プロンプトの写しを行へ置くと、
    // 正本が 2 つになって同じ問題が戻る）
    let mut row = SessionRow {
        kind,
        ..SessionRow::new(session_id.clone(), cwd, now_ms())
    };
    row.conversation.assign(conversation);
    app.sessions.push(row);
    save_sessions(app);
    app.windows.push(window);
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
    prev_hover: (Option<SidebarPos>, Option<Kind>),
    hover: (Option<SidebarPos>, Option<Kind>),
) -> bool {
    !matches!(kind, MouseEventKind::Moved) || prev_hover != hover
}

/// マウス処理。true を返したら終了。
fn handle_mouse(app: &mut App, mouse: &MouseEvent) -> anyhow::Result<bool> {
    // モーダル表示中はモーダルが全クリックを受ける。**幅変更のつかみ代より先に**
    // 判定するのが要点で、内容から幅を決めるメニューは境界線の列に被り得るため、
    // 被った列の項目クリックがサイドバー幅変更に化けてはいけない
    // （ドラッグ中だけは掴んだ操作を優先する = 下のドラッグ分岐へ落とす）
    // 掴んでいる間は他の判定を通さない（境界の掴み代・スロットのクリック・
    // 子への転送・モーダルのどれにも化けさせない）。**モーダルより先**なのが
    // 要点で、メニューの項目を掴んだ直後はまだ閉じ切っていないフレームがあり得る
    if app.drag.is_some() && handle_session_drag(app, mouse) {
        return Ok(false);
    }
    if app.popup.is_some() && !app.dragging {
        let MouseEventKind::Down(MouseButton::Left) = mouse.kind else {
            return Ok(false);
        };
        if handle_popup_click(app, mouse.column, mouse.row) {
            return Ok(false);
        }
        // メニューを閉じた押しは**何も実行しない**（背後の版行やセッション行が
        // 誤爆しない ＝ 閉じるのがこの押しの仕事）。**掴みだけは始める**ので、
        // メニューを開いた行（行末の `=` を押したセッション行）だけ運べない、
        // という説明できない差が生まれない
        arm_grab(app, mouse);
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

    // 枠右上の ✕（そのスロットを配置から外す）。**十字の掴み代より先**に見る:
    // 下段スロットの上辺は横の境界そのものなので、後回しにすると印を押した操作が
    // リサイズの掴みに化ける。**印の列は角の 1 桁内側**（[`crate::ui::close_zone`]）
    // なので、縦の境界の掴み代（枠線 2 列）はここに食われない
    if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
        && let Some(at) = crate::ui::close_hit(app, mouse.column, mouse.row)
    {
        app.close_slot(at);
        return Ok(false);
    }

    // 枠左上の短い ID（押すと 8 桁がクリップボードへ）。**十字の掴み代より先**に
    // 見るのは ✕ と同じ理由（下段スロットの上辺は横の境界そのもの）で、
    // ID の桁は角の内側 ＝ 縦の境界の掴み代（枠線 2 列）とも重ならない
    if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
        && let Some(id) = crate::ui::id_hit(app, mouse.column, mouse.row)
    {
        copy_session_id(app, &id);
        return Ok(false);
    }

    // 十字の境界ドラッグ（サイドバー幅の掴み代と同じ作法）。**スロットのクリック判定
    // より先**に見るのが要点で、境界はスロットの枠線に重なっているため、後回しにすると
    // 掴み代がフォーカス移動に化ける。交点をつかめば縦横が同時に動く
    if handle_cross_drag(app, mouse) {
        return Ok(false);
    }

    // 下部バーの使用率をクリックしたらその場で取り直す（周期を待たない）。
    // **サイドバー／右ペインの振り分けより先**に見るのが要点で、使用率は右端 ＝
    // 右ペインの列範囲に描かれるため、後回しにするとペインのクリックに食われる。
    // 当たり判定は描画と同じ導出（[`crate::ui::usage_hit`]）なので、
    // 出していないとき（notice 表示中・狭い端末・使用率を切っている）は当たらない
    // **行ごとに当たる**（押した行の agent だけを取り直す）
    let on_usage = crate::ui::usage_hits(app).into_iter().find(|hit| {
        hit.row == mouse.row && hit.columns.contains(&mouse.column)
    });
    // 乗っている間は帯で「押せる」ことを示す（一覧の行のホバーと同じ手段）
    app.usage_hovered = on_usage.as_ref().map(|hit| hit.kind);
    if let Some(hit) = on_usage
        && let MouseEventKind::Down(MouseButton::Left) = mouse.kind
    {
        // 取り直しを実際に頼めたときだけスピナーを始める（撮影用の供給元は
        // 取得しない ＝ 降ろす者がいない旗を立てない）。ここで立てるのは
        // クリック直後のフレームから回すため（取得スレッド任せだと最初の
        // 描き直しまで押した反応が出ない）
        if app.source.refresh_usage(hit.kind)
            && let Some(flag) = app.usage_fetching.get(&hit.kind)
        {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
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
        // 画面 y → 行 index（列は見ないので行のどこを押しても当たる）。
        // 計算は描画側と同じ ui::row_at を共有する
        let row = row_at(mouse.row, sl.capacity, app.sidebar_header_rows, app.sidebar_scroll);
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
            // 行クリックはフォーカススロットの中身だけ切り替える（フォーカス移動はスロットのクリック or Enter）
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
            // **ペインへ何かを出す行は掴めるので、押した時点では走らせない。**
            // 走るのは離したとき（運んでいなければクリック ＝ [`handle_session_drag`]）。
            // ここで走らせると、ドロップ先へ運ぶ前にフォーカススロットで一度
            // 開いてから移動する、という二段動作になる
            if let Some(action) = action {
                if Grab::Row(action.clone()).grabbable() {
                    app.drag = Some(DragSession {
                        what: Grab::Row(action),
                        anchor_y: mouse.row,
                        dismissed: false,
                        moved: false,
                        target: None,
                    });
                    return Ok(false);
                }
                // 実行表はキーボードの Enter と同じ [`run_row_action`]。
                // 掴めない行（版行の更新・グルーピング切替・配置メニュー）は
                // 運ぶ先を持たないので押した時点で走る
                run_row_action(app, action, mouse.row);
            }
        }
    } else {
        app.hovered = None;
        // スロット矩形は state の可変借用の前に取る（矩形の正本は App::slot_rects）
        let rects = app.slot_rects();
        let on = rects
            .iter()
            .position(|r| r.contains(Position::new(mouse.column, mouse.row)));
        if let MouseEventKind::Down(_) = mouse.kind {
            app.set_focus(Focus::Terminal);
            // **押したスロットが宛先になる**（`Alt+Shift+方向` で移るのと同じ結果）
            if let Some(on) = on {
                app.set_focus_slot(on);
            }
        }
        // **フォーカススロット以外へのイベントは中身へ渡さない。** 裏のスロットの
        // claude にホイールやクリックが届くと、見ていない画面が勝手に動く
        if on != Some(app.focus_slot) {
            return Ok(false);
        }
        let pane = rects[app.focus_slot];
        let kinds = app.kinds.clone();
        // New 画面: 入力の解釈は new_view 側（ヒットテストのジオメトリ知識を
        // レイアウトと同じファイルに閉じる）。起動だけは state の借用を抜けて実行
        if let Some(state) = app.focused_new() {
            let action = state.handle_mouse(pane, mouse, &kinds);
            if action == Some(crate::ui::new_view::NewAction::Launch) {
                start_new_session(app)?;
            }
            return Ok(false);
        }
        if app.windows.is_empty() {
            return Ok(false);
        }
        // フォーカススロット: イベントを claude へ転送（ホイールも claude 自身が処理する）
        forward_mouse(app, mouse);
    }
    Ok(false)
}

/// 十字の境界ドラッグ。**掴んだ・動かした・離した周は true**（呼び手はそこで打ち切る）。
///
/// 掴み代は境界の 2 列（行）＝ 隣り合うスロットの枠線が並んで見える幅で、
/// 位置の正本は [`crate::panes::Layout::cross`]（描画と同じ導出なので、
/// 見えている線と掴める場所がずれない）
fn handle_cross_drag(app: &mut App, mouse: &MouseEvent) -> bool {
    let area = crate::ui::pane_rect(app);
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) if app.cross_drag.is_none() => {
            // **掴めるかの判断は [`crate::panes::Layout::grab_at`] 1 つ**。
            // 「列が合っている」だけでは足りない（3 分割では境界が途中で消えるので、
            // 全高スロットの内側でそこを押した打鍵は claude へ届かねばならない）
            let (on_v, on_h) = app.layout.grab_at(area, app.split, mouse.column, mouse.row);
            if !on_v && !on_h {
                return false;
            }
            app.cross_drag = Some((on_v, on_h));
            true
        }
        MouseEventKind::Drag(MouseButton::Left) if app.cross_drag.is_some() => {
            let (v, h) = app.cross_drag.unwrap_or((false, false));
            let pct = |n: u16, total: u16| {
                if total == 0 {
                    50
                } else {
                    (u32::from(n) * 100 / u32::from(total)) as u16
                }
            };
            if v {
                app.split.v = pct(mouse.column.saturating_sub(area.x), area.width);
            }
            if h {
                app.split.h = pct(mouse.row.saturating_sub(area.y), area.height);
            }
            app.split = app.split.clamped();
            // PTY リサイズは間引く（サイドバー幅のドラッグと同じ理由）
            if app.last_drag_resize.elapsed() > Duration::from_millis(50) {
                app.resize_sessions();
                app.last_drag_resize = std::time::Instant::now();
            }
            true
        }
        MouseEventKind::Up(MouseButton::Left) if app.cross_drag.is_some() => {
            app.cross_drag = None;
            app.resize_sessions(); // 最終サイズを確定
            app.source.save_window(WindowItem::Split(app.split));
            true
        }
        // つかんでいる間は他の判定へ落とさない（掴んだ操作を優先する）
        _ => app.cross_drag.is_some(),
    }
}

/// **メニューを閉じた押しから掴みを始める。** 掴む以外の副作用（ホバー・選択・
/// 行の実行）は持たないので、閉じた押しが背後の行を誤爆させない。
///
/// 掴めない行・サイドバーの外なら何もしない
fn arm_grab(app: &mut App, mouse: &MouseEvent) {
    // **幅変更の掴み代（サイドバー右枠の 2 列）は掴みにしない。** ここを含めると、
    // メニューを開いたまま境界をつかもうとした操作が行の掴みに化ける
    // （掴みが成立すると幅変更の判定はもう見られない ＝ 幅を広げるつもりが
    // セッションをペインへ落としてしまう）
    if mouse.column >= sidebar_cols(app).saturating_sub(1) {
        return;
    }
    let sl = sidebar_layout(app);
    let row = row_at(mouse.row, sl.capacity, app.sidebar_header_rows, app.sidebar_scroll);
    let Some(action) = app.sidebar_rows.get(row).and_then(SidebarRow::action).cloned() else {
        return;
    };
    let what = Grab::Row(action);
    if !what.grabbable() {
        return;
    }
    app.drag = Some(DragSession {
        what,
        anchor_y: mouse.row,
        dismissed: true,
        moved: false,
        target: None,
    });
}

/// 掴んでいるセッションの後始末（動いたか / 落としたか）。
/// 戻り値 true ＝ このイベントは掴みが受け取った（他の判定へ落とさない）。
///
/// **クリックとドラッグを分けるのはここ 1 箇所。** 押して動かさずに離せば
/// 今までどおり「その行を開く」で、動かしてから離せばドロップになる
fn handle_session_drag(app: &mut App, mouse: &MouseEvent) -> bool {
    match mouse.kind {
        MouseEventKind::Drag(MouseButton::Left) => {
            let target = drop_target(app, mouse.column, mouse.row);
            if let Some(drag) = app.drag.as_mut() {
                drag.moved = true;
                drag.target = target;
            }
            true
        }
        MouseEventKind::Up(MouseButton::Left) => {
            let Some(drag) = app.drag.take() else {
                return false;
            };
            if let Some(target) = drag.target {
                // フォーカスを端末へ移すのは**出せたときだけ**（[`drop_row`]）。
                // 起こし直しに失敗したのに移すと、打鍵が直前まで出ていた別の
                // セッションへ流れる
                drop_row(app, &drag.what, target);
                return true;
            }
            // **運んだ末にペインの上で離したのに行き先が無いなら、何もしない。**
            // ガイドが塗らなかった ＝ そこへは落とせないと画面で断ってあるので、
            // 代わりに別のスロットで開くと断った直後に別のことをすることになる。
            //
            // **動かしていないなら断りは効かせない**: メニューはペインの上へ
            // はみ出して描かれるので、項目をただ押しただけの操作がここへ来る
            // （はみ出した列の項目が押しても走らなくなっていた）
            if drag.moved
                && crate::ui::pane_rect(app).contains(Position::new(mouse.column, mouse.row))
            {
                return true;
            }
            // 運ばずに離した押しがメニューを閉じた押しなら、それで仕事は済んでいる
            // （ここで行の動作まで走らせると、閉じた直後に同じメニューが開き直す）
            if drag.dismissed {
                return true;
            }
            // ペインの外（サイドバーやメニューの上）で離した ＝ どこにも運んで
            // いない。**動いたかどうかで分けない**のが要点で、トラックパッドの
            // 微妙なずれで 1 桁動いただけの操作が「押したのに何も起きない」に
            // 化けない。**掴んだものの動作をそのまま元の実行表へ流す**ので、
            // 押して離すだけの意味は掴めるようにする前と変わらない
            match drag.what {
                Grab::Row(action) => run_row_action(app, action, drag.anchor_y),
                Grab::Menu(action) => run_popup_action(app, action),
            }
            true
        }
        // ボタンを離したイベントを取り逃がしたときの逃げ道（掴んだまま戻らない
        // 状態を残さない）。押している間は Drag しか来ないので、Moved が届いた
        // 時点で指は離れている
        MouseEventKind::Moved => {
            app.drag = None;
            false
        }
        // 掴んでいる間の他のイベント（ホイール等）は捨てる
        _ => true,
    }
}

/// 掴んだセッションを落としたときの行き先（落とせないなら `None`）。
///
/// **判定の正本はここ 1 つ。** ガイドの塗り（[`drop_rect`]）も実行
/// （[`drop_session`]）も同じ答えを読むので、「塗られた場所と違うところに入る」が
/// 構造的に起きない。
///
/// スロットの**縁**（各辺の 1/4 の帯）を指していれば割って入れる、
/// **真ん中**ならそのスロットへ入れる。角のように帯が重なるところは、
/// 帯の中でどれだけ端に寄っているかで決める（辺の長さが違っても比べられる）。
///
/// **割れない縁は `None`**（4 分割の縁・端末が狭くて育った先が入らないとき）。
/// 落とせないことをガイドが塗らないことで表す ＝ 落としてから何も起きないより早く分かる
pub(crate) fn drop_target(app: &App, column: u16, row: u16) -> Option<DropAt> {
    use crate::panes::Axis;
    let rects = app.slot_rects();
    let at = rects
        .iter()
        .position(|r| r.contains(Position::new(column, row)))?;
    let r = rects[at];
    // 縁の帯は各辺の 1/4（狭いスロットでも 1 桁は確保する）
    let band_w = (r.width / 4).max(1);
    let band_h = (r.height / 4).max(1);
    let edges = [
        (column.saturating_sub(r.x), band_w, Axis::Vertical, 0usize),
        (
            (r.x + r.width).saturating_sub(1).saturating_sub(column),
            band_w,
            Axis::Vertical,
            1,
        ),
        (row.saturating_sub(r.y), band_h, Axis::Horizontal, 0),
        (
            (r.y + r.height).saturating_sub(1).saturating_sub(row),
            band_h,
            Axis::Horizontal,
            1,
        ),
    ];
    // 帯に入っている辺のうち、**帯の中で最も端に寄っている**もの
    // （生の距離で比べると、長い辺の帯が短い辺の帯に必ず勝ってしまう）
    let Some(&(_, _, axis, half)) = edges
        .iter()
        .filter(|(dist, band, _, _)| dist < band)
        .min_by_key(|(dist, band, _, _)| u32::from(*dist) * 100 / u32::from(*band))
    else {
        return Some(DropAt::Slot(at)); // 真ん中 ＝ そのスロットへ
    };
    // 育った先が今の端末に収まらないなら落とせない
    // （**判断はメニューと同じ [`crate::panes::Layout::fits`]** ＝ 崩れる形へ育たない）
    let grown = app.layout.split_slot(at, axis)?;
    grown
        .layout
        .fits(crate::ui::pane_rect(app), app.split)
        .then_some(DropAt::Split { at, axis, half })
}

/// ドロップ先の矩形（ガイドが塗る範囲 ＝ 落とした結果そのもの）。
///
/// **割った先は本物の矩形を引く**（半分に割った近似ではない）: 育った配置に
/// 同じ [`crate::panes::Split`] を渡して計算するので、十字を寄せてあるときも
/// 塗った通りの大きさで入る
pub(crate) fn drop_rect(app: &App, target: DropAt) -> Option<Rect> {
    match target {
        DropAt::Slot(at) => app.slot_rects().get(at).copied(),
        DropAt::Split { at, axis, half } => {
            let grown = app.layout.split_slot(at, axis)?;
            let rects = grown.layout.rects(crate::ui::pane_rect(app), app.split);
            rects.get(grown.halves[half]).copied()
        }
    }
}

/// 掴んだものを落とす。[`drop_target`] が返した行き先をそのまま実行する。
///
/// **即起動するかは「agent が決まっているか」で分かれる。** `+ new session` は
/// 決まっていないので選ばせる画面を出し、メニューの `new <agent> session` は
/// 項目が名指ししているのでそのまま起こす（[`PopupKind::Project`] の項目が
/// agent ごとに分かれているのは、黙って選ばせないため）
fn drop_row(app: &mut App, what: &Grab, target: DropAt) {
    match what {
        Grab::Row(RowAction::Open(id)) | Grab::Menu(PopupAction::OpenSession(id)) => {
            drop_session(app, id, target);
        }
        // `+ new session` は直近のフォルダで**選ばせる画面**を出す
        // （どの agent で起こすかはその画面が受け持つ）
        Grab::Row(RowAction::New) => drop_new_view(app, app.dispatch_cwd.clone(), target),
        // **メニューの `new <agent> session` は落とした先で即起動する。**
        // 選ばせる画面を挟まないのは、この項目が agent を名指ししているから ＝
        // 落とす操作に選ばせるものが残っていない
        Grab::Menu(PopupAction::NewSessionIn(kind, cwd)) => {
            drop_new_session(app, *kind, cwd.clone(), target);
        }
        // 掴めないものはここまで来ない（[`Grab::grabbable`]）
        _ => {}
    }
}

/// 落とし先で新規セッションを起こす（メニューの `new <agent> session`）。
/// **起こしてから配置を育てる**順序は [`dispatch_session_into`] が持つ
fn drop_new_session(app: &mut App, kind: Kind, cwd: String, target: DropAt) {
    dispatch_session_into(app, kind, cwd, String::new(), Some(target));
}

/// 落とし先へ新規セッション画面を出す
fn drop_new_view(app: &mut App, cwd: String, target: DropAt) {
    let Some(to) = make_room(app, target) else {
        return;
    };
    app.put_in_slot(to, Slot::New(NewState::browse(&cwd)));
    app.set_focus(Focus::Terminal);
}

/// 掴んだセッションを落とす。
///
/// **起こし直しはクリックと同じ [`ensure_window`] を通る**ので、止まっている行を
/// 落としても再開する（窓を用意せずスロットへ入れるだけだと、死んだ枠だけが出る）。
///
/// **窓を用意するのが先、配置を変えるのは後。** 逆にすると、起こし直しに失敗した
/// ときに空のペインだけが増えて残る
fn drop_session(app: &mut App, id: &SessionId, target: DropAt) {
    // 落とし先の**内寸**で起こす（育った先の大きさは配置を変える前に分かる ＝
    // 起こした直後に作り直させない）
    let size = drop_rect(app, target).map_or_else(|| app.focus_slot_size(), App::inner_size);
    if !ensure_window(app, id, size) {
        return;
    }
    let Some(to) = make_room(app, target) else {
        return;
    };
    app.place_session(id, to);
    app.set_focus(Focus::Terminal);
}

/// 落とし先のスロットを空けて、その番号を返す（**中身は入れない**）。
///
/// 割って入れる先なら配置を育て、割らなかったスロットの中身は番号の
/// 振り直しに沿って写す（[`crate::panes::Grown::moved`]）。
/// **窓の用意は含まない**ので、育てる側だけを起動なしで検査できる
fn make_room(app: &mut App, target: DropAt) -> Option<usize> {
    let to = match target {
        DropAt::Slot(at) => at,
        DropAt::Split { at, axis, half } => {
            let grown = app.layout.split_slot(at, axis)?;
            // **新しい並びを組み終えてから入れ替える。** 途中で `App` に
            // 中身の欠けた並びを持たせると、そこで走るリサイズが触っていない窓まで
            // 誤ったサイズへ潰す（[`App::apply_layout`]）
            let mut before = std::mem::take(&mut app.slots);
            let mut next: Vec<Slot> = (0..grown.layout.slots()).map(|_| Slot::Empty).collect();
            for (from, to) in &grown.moved {
                if let Some(slot) = before.get_mut(*from) {
                    next[*to] = std::mem::replace(slot, Slot::Empty);
                }
            }
            // 割ったスロットの中身は、**落とさなかった側へ残る**
            // （押し出さない ＝ 触っていないセッションは 1 つも画面から消えない）
            if let Some(slot) = before.get_mut(at) {
                next[grown.halves[1 - half]] = std::mem::replace(slot, Slot::Empty);
            }
            app.apply_layout(grown.layout, next);
            // **配置ごと書き残す**（スロットだけ保存すると、次の起動で古い配置の
            // 枚数しか復元されず、割って増えたペインが消える）
            save_layout(app);
            grown.halves[half]
        }
    };
    Some(to)
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
    let kinds = app.kinds.clone();
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
                let last = popup.kind.entries(grouping, &kinds).len().saturating_sub(1);
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
fn handle_popup_click(app: &mut App, col: u16, row: u16) -> bool {
    let Some(popup) = &app.popup else { return false };
    let rect = popup_rect(app, popup);
    if !rect.contains(Position::new(col, row)) {
        app.popup = None; // 外クリックで閉じる（階層を持たないので全閉）
        // **閉じるだけで押しを使い切らない。** 使い切ると、メニューを開いた行
        // （行末の `=` を押したセッション行）は開いた直後に掴めなくなる ＝
        // メニューを開いた行にだけドラッグの入口が無い、という差が生まれる
        return false;
    }
    // 枠線上のクリックは何もしない（上枠が先頭項目に化けて誤発火しない）
    if row == rect.y
        || row == rect.y + rect.height - 1
        || col == rect.x
        || col == rect.x + rect.width - 1
    {
        return true;
    }
    // 枠内に入りきらないメニューは描画がスクロールしている（[`crate::ui::popup_scroll`]）
    // ので、クリックの行 → 項目 index も同じずらしを通す
    let visible = rect.height.saturating_sub(2) as usize;
    let entries = popup.kind.entries(app.grouping, &app.kinds);
    let offset = crate::ui::popup_scroll(popup.selected, entries.len(), visible);
    let index = offset + (row - rect.y - 1) as usize;
    // **ペインへ何かを出す項目は掴める**（`new <agent> session` / `open`）。
    // 走らせるのは離したとき ＝ 押した時点で走らせると、運ぶ前にフォーカス
    // スロットで一度起きてから移動する二段動作になる。
    //
    // **掴んだ時点でメニューを閉じる**のは、開いたままだとこの後の Drag が
    // モーダル側に飲まれてドロップ先を選べないため（＋ ペインを覆って
    // ガイドが見えない）
    let grab = entries
        .get(index)
        .filter(|entry| entry.enabled)
        .map(|entry| Grab::Menu(entry.action.clone()))
        .filter(Grab::grabbable);
    if let Some(what) = grab {
        app.popup = None;
        app.drag = Some(DragSession {
            what,
            anchor_y: row,
            dismissed: false,
            moved: false,
            target: None,
        });
        return true;
    }
    activate_popup(app, index);
    true
}

/// 選択項目の実行（Enter / クリック共通）。実行できない項目・範囲外の index は無視する
fn activate_popup(app: &mut App, index: usize) {
    let Some(popup) = app.popup.as_ref() else {
        return;
    };
    let mut entries = popup.kind.entries(app.grouping, &app.kinds);
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
/// 判定は [`PopupKind::entries`] が返す表（純関数）に置く
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
        PopupAction::SetLayout(next) => set_layout(app, next),
        // 空プロンプトで起動する（登録は dispatch_session が行う）
        PopupAction::NewSessionIn(kind, cwd) => dispatch_session(app, kind, cwd, String::new()),
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

/// スロットの並べ方の選択（入口は ▦ layout 行のメニューだけ）。
/// **スロット数を合わせるのは [`App::set_layout`] 1 箇所**なので、ここは
/// 選ばれた値を渡して保存するだけ。溢れたスロットの中身は表示から外れるが、
/// PTY は生きたまま残る（何も終わらない）
fn set_layout(app: &mut App, next: crate::panes::Layout) {
    if app.layout == next {
        return;
    }
    app.set_layout(next);
    save_layout(app);
}

/// **配置を変えたことを書き残す唯一の口**（メニューで選んだとき / ドロップで
/// 割ったとき）。
///
/// 配置とスロットは**必ず一緒に**保存する: 片方だけ書くと、次の起動で
/// `main` が古い配置の枚数だけスロットを復元して**残りが黙って消える**
/// （割って増やしたペインが再起動のたびに失われていた）
fn save_layout(app: &mut App) {
    app.source.save_window(WindowItem::Layout(app.layout));
    app.save_slots();
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
    // 行ごと消えるので、close_window_of が刻んだ記録も一緒に捨てる
    // （行が無い session_id を持ち続けても引く先が無く、溜まるだけ）
    app.stopped_at.remove(id);
}

/// hook 注入ファイルのパス（実体は [`crate::hooks::inject_settings`]）。
///
/// **注入するかどうかは供給元が決める**（[`crate::source::DataSource::hook_dir`]）:
/// 置き場を持たない供給元（撮影・テスト）では**黙って None** ＝ 書けなかったのでは
/// なく、書く先が無い。ここで実ホームを引かないのが、テストが実ユーザーの
/// `~/.ccdesk/inject-settings.json` を上書きしない唯一の担保。
///
/// **書けなかったことは黙らせない**: hooks 無しで起動したセッションは状態報告が
/// 縮退する（入力待ち・完了が導出できなくなる）ので、下部バーへ 1 行出す。
/// セッション自体は hooks 無しで起動を続ける（起動を止めるほどの失敗ではない）
fn hook_settings(app: &mut App) -> Option<crate::hooks::Injection> {
    let dir = app.source.hook_dir()?;
    let settings = crate::hooks::inject_settings(&dir);
    if settings.is_none() {
        set_notice(
            app,
            "could not write the hook settings; session states may not update (see ccdesk logs)"
                .to_string(),
        );
    }
    settings
}

/// 書き出し済みの注入（[`crate::hooks::Injection`]）を、起動 1 回ぶんの借用へ。
/// **所有と借用の変換だけ**（どう載せるかは [`crate::backend`] が決める）
fn as_inject(injection: &crate::hooks::Injection) -> Inject<'_> {
    Inject {
        exe: &injection.exe,
        settings: &injection.settings,
    }
}

/// 指定セッションのウィンドウを閉じる（＝ 子プロセスを終わらせる）。
/// 窓が開いていなければ何もしない。
///
/// **`stop` / `close` / PTY 書き込み失敗の後始末、この関数を通る窓閉じ全てが
/// [`remove_window`] へ収束する**ので、[`App::stopped_at`] を刻む場所は
/// そちら 1 箇所に一本化してある（ここでは刻まない）
fn close_window_of(app: &mut App, id: &SessionId) {
    let Some(i) = app.window_index(id) else {
        return;
    };
    let _ = app.windows[i].child.kill();
    remove_window(app, i);
}

/// ウィンドウを一覧から外す。**その窓を映していたスロットは空になる**
/// （行は残るので、開き直せば同じスロットへ戻せる）。
///
/// **`App::stopped_at` を刻む場所はここ 1 箇所。** 窓を外す経路は 3 つある
/// （[`close_window_of`] 経由の `stop`/`close`/PTY 書き込み失敗、生死スキャンが
/// 拾う自然死、[`open_session`] が起こし直す前に片付ける「死んでいるがまだ
/// スキャンが拾っていない窓」）が、**行を動かす窓が無くなったという事実は
/// 3 つとも同じ**なので、刻む理由も同じ。以前はこの関数を経由しない自然死の
/// 経路（生死スキャン）だけ刻み忘れており、`/exit` や外部からの kill の直後に
/// 最大 [`LIVE_SCAN_INTERVAL`] 秒ぶん古いライブ状態の観測が素通りして、
/// 行が数秒 Working/Waiting に見えてから Stopped になっていた（実機で観測されたバグ）。
/// ここへ一本化したことで、経路を増やしても刻み忘れが起きない
fn remove_window(app: &mut App, idx: usize) {
    if idx >= app.windows.len() {
        return;
    }
    let id = app.windows[idx].session_id.clone();
    app.stopped_at.insert(id.clone(), now_ms());
    app.windows.remove(idx);
    app.hovered = None;
    // **その窓を映していたスロットは空になる**（New 画面へは奪われない）。
    // 以前は New 画面を開いていたが、`stop` した直後に書きかけのフォームが出るのは
    // 「止めた」という操作の結果として読めない。空の `no session` はそのまま
    // 「ここには今なにも無い」を表す
    if let Some(at) = app.slots.iter().position(|s| s.session() == Some(&id)) {
        app.slots[at] = Slot::Empty;
        app.save_slots();
    }
}

/// 走っているセッションの公開と、溜まった要求の消化（[`crate::relay`]）。
///
/// **run ループから毎周呼ぶ。** 契機を「窓が増減したとき」に絞らないのは、
/// 公開する内容に名前が含まれるから（名前は transcript が伸びるたびに変わり得る
/// ので、契機で書くと `ccdesk list` に古い名前が残る）。実際に書くのは前回と
/// 違う周だけで、要求の有無も `exists` 1 回で見る ＝ 何も起きていない周の代償は
/// 小さい
/// 貼り付けてから送信の `\r` を送るまで空ける間。
///
/// **0 にはできない（実機で確認）。** 貼り付けと `\r` を 1 回の書き込みに
/// 載せると、codex は `\r` を**送信ではなく composer の改行**として扱い、
/// 送った本文が入力欄に積み上がったまま返事が来ない（claude は送信する）。
/// 人が操作するときは貼り付けと Enter が別のイベントとして届くので、
/// **その形に揃えるのがこの間の意味**。
///
/// run ループは何も起きていなくても [`POLL_IDLE`] で回るので、実際に送信が
/// 出るのはこの値の直後の周回になる（人には見えない遅れ）
const SUBMIT_DELAY: Duration = Duration::from_millis(50);

fn serve_relay(app: &mut App) {
    let instance = std::process::id();
    // **新しい要求を当てるより先に流す。** 逆にすると、同じ周で貼って同じ周で
    // 送ることになり、間を空けた意味が無くなる
    flush_submits(app);
    publish_open(app, instance);
    if !crate::relay::pending(instance) {
        return;
    }
    // **答えは溜めてから返す。** 要求を当てると走っているセッションの顔ぶれが
    // 変わる（`new` が増やし、`stop`/`close` が減らす）ので、先に答えを返すと
    // **要求元が「まだ古い一覧」を読む**。`ccdesk new` が返した ID は次のコマンドの
    // 宛先として通るべきなので、公開を答えより前に置く（実機で踏んだ:
    // `new` の直後の `list` に、起こしたばかりのセッションが出なかった）
    let answers: Vec<(u32, serde_json::Value)> = crate::relay::drain(instance)
        .iter()
        .filter_map(|request| apply_relay_request(app, request))
        .collect();
    publish_open(app, instance);
    for (reply, value) in answers {
        crate::relay::answer(reply, &value);
    }
}

/// このインスタンスが面倒を見ているセッションを公開する（前回と違うときだけ書く）。
///
/// **供給元は 2 つ**: 走っている窓と、**このインスタンスが止めた行**
/// （[`App::stopped_at`]）。止めた側を載せるのは、**終わったセッションの記録を
/// 読めるようにするため**（走っているものだけに絞っていた頃は、相手が終わった
/// 瞬間に宛先ごと消えて `ccdesk read` が届かなくなっていた）。
///
/// **止めた側は ID で整列する。** [`App::stopped_at`] は `HashMap` なので
/// 並びが周回ごとに変わり得る ＝ 中身が同じでも「前回と違う」と見えて、
/// 何も起きていない周にディスクへ書き続けることになる
fn publish_open(app: &mut App, instance: u32) {
    let open = open_sessions(app);
    if open != app.published_sessions {
        crate::relay::publish(instance, &open);
        app.published_sessions = open;
    }
}

/// 公開する一覧を組む。**書き出しから分けてある**ので、並びと顔ぶれの規則を
/// ディスクを触らずに検査できる（テストが実ユーザーの `~/.ccdesk` を触らない
/// というこの repo の約束がここに乗っている）
fn open_sessions(app: &App) -> Vec<crate::relay::Open> {
    let open_of = |id: &SessionId, running: bool| {
        let row = app.row(id);
        crate::relay::Open {
            id: id.clone(),
            // 名前の正本は一覧と同じ（[`crate::title`]）。行が読めない窓は
            // 名前を持たないだけで、ID では指せる
            name: row.map_or_else(|| crate::title::UNTITLED.to_string(), |row| app.titles.of(row)),
            cwd: row.map(|row| row.cwd.clone()).unwrap_or_default(),
            kind: row.map(|row| row.kind).unwrap_or_default(),
            transcript: row.and_then(|row| row.transcript.clone()),
            running,
        }
    };
    let mut open: Vec<crate::relay::Open> = app
        .windows
        .iter()
        .map(|window| open_of(&window.session_id, true))
        .collect();
    // **窓を持つ行は載せ直さない**（開き直した行が `stopped_at` に残っていても
    // 二重に出ない ＝ 走っている側が勝つ）。行ごと消えたものも載せない
    let mut stopped: Vec<&SessionId> = app
        .stopped_at
        .keys()
        .filter(|id| app.window_index(id).is_none() && app.row(id).is_some())
        .collect();
    stopped.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    open.extend(stopped.into_iter().map(|id| open_of(id, false)));
    open
}

/// 間の空いた送信（`\r`）を出す。**宛先が閉じていたら捨てる**（窓が無ければ
/// 打つ先も無い）。
///
/// 並びは積んだ順のままで、期限の来たものだけを前から外す ＝ 同じセッションへ
/// 続けて送っても、貼り付けと送信の対が入れ替わらない
fn flush_submits(app: &mut App) {
    let mut due = Vec::new();
    app.pending_submit.retain(|(id, at)| {
        if at.elapsed() < SUBMIT_DELAY {
            return true;
        }
        due.push(id.clone());
        false
    });
    for id in due {
        if let Some(window) = app.windows.iter().find(|w| w.session_id == id) {
            let _ = window.send(b"\r");
        }
    }
}

/// 要求 1 件を当てる。**宛先が閉じていたら黙って捨てる**（送り主は別プロセスで、
/// ここから届く相手がもう居ない）。
///
/// **答えは返さずに戻す。** 応答を待つ要求には必ず答えが要る（返さないと要求元は
/// 上限まで待つ）が、**いつ返すかは呼び手が決める**: 走っているセッションの
/// 顔ぶれを公開し直してから返す必要がある（[`serve_relay`]）
fn apply_relay_request(
    app: &mut App,
    request: &crate::relay::Request,
) -> Option<(u32, serde_json::Value)> {
    match request {
        crate::relay::Request::Send { to, text } => {
            let window = app.windows.iter().find(|w| &w.session_id == to)?;
            // **打鍵と同じ経路で組む**（[`crate::keys::encode_paste`]）ので、
            // 複数行の本文が途中で送信されない（bracketed paste の包み）
            let bytes = crate::keys::encode_paste(text, &window.parser.lock_recover());
            let _ = window.send(&bytes);
            // **送信の `\r` は同じ書き込みに載せない**（[`SUBMIT_DELAY`]）
            app.pending_submit
                .push((to.clone(), std::time::Instant::now()));
            None
        }
        crate::relay::Request::Screen { to, reply } => {
            let screen = app
                .windows
                .iter()
                .find(|w| &w.session_id == to)
                .map(|window| window.parser.lock_recover().screen().contents());
            Some((
                *reply,
                crate::relay::screen_answer(&screen.unwrap_or_default()),
            ))
        }
        crate::relay::Request::New {
            kind,
            cwd,
            prompt,
            reply,
        } => {
            let started = start_unattended(app, *kind, cwd, prompt);
            Some((*reply, crate::relay::started_answer(started)))
        }
        // **メニューの `stop` / `close` と同じ 1 実装を通す。** 別に書くと、
        // 行の後始末（[`App::stopped_at`] の記録・保存）が片方だけ変わり得る
        crate::relay::Request::Stop { to } => {
            menu_stop(app, to);
            None
        }
        crate::relay::Request::Close { to } => {
            menu_close(app, to);
            None
        }
    }
}

/// **押した人が居ない**セッションの起動（`ccdesk new`）。
///
/// [`start_foreground`] との違いは 2 つで、どちらも「誰も見ていないところで
/// 起きた」ことから来る:
///
/// - **見ていた画面を奪わない。** 起こすと同時にペインがそちらを向くと、
///   ユーザーが読んでいた出力が消える。入力途中の New 画面なら**打ちかけの
///   プロンプトごと**消える（[`remove_window`] が同じ問題を避けている）
/// - **入力の門番を立てない。** 門番（[`App::input_gate`]）は「今から打つ人」の
///   打鍵を子が端末を掴むまで預かるものなので、押した人が居ないこの経路で
///   立てると、**別のセッションを打っているユーザーの打鍵を飲む**
///
/// **出さない agent は起こさない**（codex を off にしている環境）。UI が
/// 選ばせないものを、この口からだけ起こせる状態を作らない
fn start_unattended(
    app: &mut App,
    kind: Kind,
    cwd: &str,
    prompt: &str,
) -> Result<SessionId, String> {
    if !app.kinds.contains(&kind) {
        return Err(format!("{} is not enabled in this ccdesk", kind.as_str()));
    }
    // **頼まれていない起動が、今見ているものを画面から追い出さない。**
    // 起こすだけで画面へは出さない（[`start_foreground`] は出し先を持たない）＝
    // 起きたセッションはどのスロットにも出ない。行としては一覧に出るので、
    // 見たければ選べばよい。
    //
    // **窓の大きさはフォーカススロットに合わせておく**（[`App::resize_sessions`] が
    // 出ていない窓に与えるのと同じ値）＝ 次に映したときに作り直しが要らない
    let started = start_foreground(app, kind, cwd, prompt, app.focus_slot_size());
    match started {
        Ok(Some(id)) => Ok(id),
        // 起こさない供給元（撮影用）＝「試していない」ので、失敗として返す
        Ok(None) => Err("this ccdesk does not start sessions".to_string()),
        Err(err) => Err(err),
    }
}

/// サイドバーの選択を、**行の実体がある位置**へ上下に移動する
/// （飾り ＝ [`SidebarRow::Decoration`] は飛ばす。押しても何も起きない行は止まる ＝
/// 「触れる行」の集合はホバーと同じ [`SidebarRow::selectable`] 1 つで決まる）。
///
/// **一覧は 1 つの輪**: 末尾で `↓` を押すと先頭へ、先頭で `↑` を押すと末尾へ回る。
/// 端で止めると、先頭へ戻るために一覧全体を遡ることになる。
///
/// 触れる行が 1 つも無ければ何も動かさない（無限に回らない）
pub(crate) fn move_selection(app: &mut App, dir: i32) {
    let ring = app.sidebar_rows.len() as i32;
    if ring <= 0 {
        return;
    }
    let SidebarPos::Row(row) = app.selection;
    let mut at = row as i32;
    // 輪を 1 周するまで探す（触れる行が無ければ元の位置のまま戻る）
    for _ in 0..ring {
        at = (at + dir).rem_euclid(ring);
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
/// "restart to update" の案内を出すだけに留める（自動では再起動しない ＝
/// [`crate::ui`] の `UpdateState::RestartPending`）。利用者が自分のタイミングで
/// ccdesk を終了・起動し直すと新しい版が動く。`SelfUpdate::Done` はこのセッション中戻らない。
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

/// agent ごとの旗。**作るのはここ 1 箇所**なので、agent が増えても
/// 旗を作り忘れた kind ができない（[`Kind::ORDER`] から導く）。
///
/// 用途を名前に書かない: 使い手は使用率の取得中（[`App::usage_fetching`]）だけで、
/// 「更新中の旗」という名前で通っていた頃は**名前が嘘をついていた**
pub(crate) fn per_agent_flags(
) -> BTreeMap<Kind, Arc<std::sync::atomic::AtomicBool>> {
    Kind::ORDER
        .into_iter()
        .map(|kind| (kind, Arc::new(std::sync::atomic::AtomicBool::new(false))))
        .collect()
}

/// agent ごとの更新状態（[`App::agent_update`]）。旗と同じ理由でここ 1 箇所
pub(crate) fn agent_update_states() -> BTreeMap<Kind, Arc<Mutex<AgentUpdate>>> {
    Kind::ORDER
        .into_iter()
        .map(|kind| (kind, Arc::new(Mutex::new(AgentUpdate::Idle))))
        .collect()
}

/// `<agent> update` を 1 回走らせる。**失敗は必ず文面にして返す**
/// （握り潰すと「押しても何も起きない」に見え、原因を追う手がかりが残らない）。
///
/// **PATH の解決は自前でやる**（[`ccdesk::resolve_program`]）。Windows の
/// `Command::new("codex")` は `CreateProcess` が `PATHEXT` を見ないので、npm が
/// 並べて置く `codex`（sh のシム）と `codex.cmd` のうち実行できる方を掴めず
/// `NotFound` で終わる。claude は native インストールで `claude.exe` があるため
/// 露見せず、**codex の更新ボタンだけが無反応**になっていた
fn run_agent_update(argv: (&str, &[&str])) -> Result<(), String> {
    use std::process::Stdio;
    let (program, args) = argv;
    let resolved = ccdesk::resolve_program(program)
        .ok_or_else(|| format!("{program} update failed: {program} not found on PATH"))?;
    let out = std::process::Command::new(resolved)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{program} update failed: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    // 終了コードだけでは何が起きたか分からないので、stderr の最初の 1 行を添える
    // （更新は npm 経由で失敗しうる ＝ 権限・ネットワークの区別が要る）。
    // 成功時の warning は出さない（`codex update` は成功しても npm の警告を吐く）
    let detail = String::from_utf8_lossy(&out.stderr)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .to_string();
    Err(match detail.is_empty() {
        true => format!("{program} update failed ({})", out.status),
        false => format!("{program} update failed ({}): {detail}", out.status),
    })
}

/// agent 本体の更新を実行する（`<agent> update`）。
/// 公式仕様: 更新は次回起動時から有効で、実行中セッションは現行版のまま動き続ける。
/// 完了後はフッターを再取得し、最新化されれば版行は最新表示へ戻る。
///
/// 結果は [`App::agent_update`] 1 つへ書く。失敗は下部バーへ回って Idle へ戻る
/// （＝押し直せる）。**成功と報告されても版が動かないことがある**ので、そちらは
/// [`AgentUpdate::Stalled`] として版行に残す（[`update_left_the_version_behind`]）
fn start_agent_update(app: &mut App, kind: Kind) {
    let Some(state) = app.agent_update.get(&kind).cloned() else {
        return;
    };
    {
        let mut guard = state.lock_recover();
        // その agent の更新が走っている間の連打を防ぐ。**判断はロックの中**
        // （多重起動の防止と状態の書き換えを別の場所に持たない）
        if matches!(*guard, AgentUpdate::Running) {
            return;
        }
        // 前回の据え置き・失敗は押し直しで降りる（行が Running へ入る ＝ やり直しが見える）
        *guard = AgentUpdate::Running;
    }
    // **どのコマンドを叩くかは agent が答える**（[`crate::backend`]）
    let argv = kind.backend().update_argv();
    // 押した時点の版。更新が本当に効いたかは、これと取り直した版の差でしか分からない
    let before = app.footer.version(kind).current;
    let refresh = app.footer_refresh.clone();
    let dirty = app.footer_dirty.clone();
    let footer = app.footer_shared.clone();
    std::thread::spawn(move || {
        // **更新の前後で残骸を片付ける。** 起動時の掃除（`main`）だけでは足りない:
        // 塞ぐ残骸を作るのは更新そのものなので、掃除がそこに無いと
        // **更新ボタンが「1 回効いて以降ずっと失敗する」**（塞ぐ残骸の意味は
        // [`crate::backend::Garbage::blocks_next_update`]）。前も掃くのは、
        // 押した時点で既に塞がっている場合（前回の更新のあと ccdesk を
        // 再起動していない）のため
        let sweep = || {
            let _ = crate::update::sweep_agent_leftovers(&[kind]);
        };
        sweep();
        let outcome = run_agent_update(argv);
        sweep();
        // 状態を書く**前に**版を取り直しておく。ここを飛ばして状態だけ戻すと、
        // 周期ポーラー（1 秒間隔）が追いつくまでの間、版行が古い `latest` を
        // 読んで一度 Available（"update"）へ戻ってしまう
        // （`Running → Available → Current` と一往復して見える不具合の原因）。
        // 取得に失敗した（current が空）場合は書かない ＝ 古い表示のまま
        // 周期ポーラーの再取得に委ねる（他の版取得と同じ「空振りは無視」の作法）
        let fresh = kind.backend().version();
        let next = match outcome {
            Err(msg) => AgentUpdate::Failed(msg),
            Ok(()) if update_left_the_version_behind(&before, &fresh) => {
                AgentUpdate::Stalled { version: before, announced: false }
            }
            Ok(()) => AgentUpdate::Idle,
        };
        if !fresh.current.is_empty() {
            footer.lock_recover().versions.insert(kind, fresh);
        }
        *state.lock_recover() = next;
        refresh.store(true, std::sync::atomic::Ordering::Relaxed);
        dirty.store(true, std::sync::atomic::Ordering::Relaxed);
    });
}

/// 更新は成功したと報告されたのに、版が 1 つも動いていないか。
///
/// **これを見ないと「押しても効かない」が無言になる。** 実際に claude が
/// `update` にも `install` にも成功を返しながら実行ファイルを差し替えない状態に
/// なることがあり（終了コード 0・"Successfully updated" の出力つき）、ccdesk 側は
/// 版行を `update` のまま描き直すだけなので、利用者からは押しても押しても
/// 何も起きない行に見える。
///
/// 3 つ揃ったときだけ疑う ＝ 正常な経路を誤って失敗と呼ばない:
///
/// - **押す前の版が分かっている**（空 ＝ そもそも版を取れていなかった）
/// - **版が変わっていない**
/// - **まだ新しい版がある**（`latest` が消えていれば、押した時点で既に最新
///   だっただけ ＝ 更新するものが無かったので版が動かないのが正しい）
fn update_left_the_version_behind(before: &str, after: &crate::backend::AgentVersion) -> bool {
    !before.is_empty() && after.current == before && after.latest.is_some()
}

/// 据え置きを伝える文面。**原因を断定せず、手順も書かない。**
///
/// ccdesk に見えているのは「成功と言われたのに版が動かなかった」ことだけで、
/// なぜ差し替えられなかったかは agent 側にしか分からない。
///
/// **「shell で打ち直せ」とは書かない**: 同じコマンドが同じ嘘を返すことを実測して
/// いる（claude は `update` にも `install` にも成功を返しながら実行ファイルを
/// 置き換えなかった）ので、それは行き止まりへの案内になる。同じ理由で agent 内部の
/// 置き場所（ダウンロード済みの版がどこにあるか）も書かない ＝ agent 側が配置を
/// 変えた日に黙って嘘になる。**ccdesk が観測した事実だけを渡し、判断は人に返す**
fn stale_update_message(program: &str, before: &str) -> String {
    format!(
        "{program} reported a successful update but is still v{before} -- \
         its installer did not replace the executable"
    )
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

/// 起動中に溜めておける入力の量（打鍵は 1、貼り付けは文字数 ＝
/// [`HeldInput::weight`]）。**上限が要るのは、応答しない子に対して
/// 際限なくメモリを食わないため**（`-r` の再開は数十秒かかることがあり、
/// その間ずっと積める）。
///
/// **超えた分は新しい方から受け付けない**（古い方を押し出すのではなく）。
/// どちらを残しても届く入力は途中までになるが、**頭を残す方が壊さない**:
/// プロンプトは頭から溜まるので、末尾を落とせば送信の `\r` が落ちて
/// 文字が入力欄に残る ＝ 送られる前に本人が見て直せる。頭を落とすと
/// `\r` だけが生き残り、**切れたプロンプトがそのまま送信される**
pub(crate) const HELD_INPUT_LIMIT: usize = 64 * 1024;

/// 起動処理中（[`App::input_gate`] が生きている間）にターミナルペインへ来た入力を
/// **溜める**。溜めたら `true`（呼び手は素通しをやめる）。
///
/// **子が端末を掴む前の打鍵を守る門番。** 前景セッションは PTY を開いた時点で
/// 宛先が決まるが、claude が raw mode に入るまでの打鍵は行き場が定まらない
/// （読み捨てられる・エコーが混ざる）。特に `-r` の再開は transcript の読み直しに
/// 時間がかかりうるので、**掴むまでは素通しさせない**。
///
/// **捨てずに溜めるのが要点**: 実ユーザーの transcript は数十 MB あり、再開が
/// 期限（[`INPUT_GATE_LIMIT`]）を超えるのは常態。その間の打鍵を捨てると、
/// 「打ったのに何も起きなかった」が普通に起きる。溜めた入力は子が掴んだ時点で
/// その宛先へ流す（[`flush_held_input`]）。
///
/// **フォーカスの移動を遅らせる形は採らない**: それは直したはずの問題
/// （起動したのにキーがサイドバーへ行き ↑↓ で選択が動く）を戻すことになる。
/// フォーカスは即座に端末へ移し、**子が掴むまでの入力だけを溜める**。
///
/// 降ろす契機は「子が最初の出力を出した」（run ループ）と期限切れ
/// （[`expire_input_gate`]）の 2 つ。**判断は [`lift_input_gate`] 1 箇所**。
///
/// `input` を関数で受けるのは、門番が立っていないときに貼り付けの本文を
/// 複製しないため（素通しの経路がそのまま元の文字列を使う）。
///
/// マウスは門番の対象外: 届くのはクリックとホイールでプロンプトへ文字を送る経路ではなく、
/// 移動イベントごとに案内を出すとノイズになる
fn hold_input_while_starting(app: &mut App, input: impl FnOnce() -> HeldInput) -> bool {
    if app.input_gate.is_none() {
        return false;
    }
    let input = input();
    let held: usize = app.held_input.iter().map(HeldInput::weight).sum();
    if held + input.weight() > HELD_INPUT_LIMIT {
        // 溜め切れない。**黙って捨てない**（打鍵のたびに来うるので [`set_hint`] ＝
        // error.log には残さない。1 秒の取りこぼしで数十行増える）
        set_hint(
            app,
            "too much input while starting — the newest input is dropped".to_string(),
        );
        return true;
    }
    app.held_input.push(input);
    // 下部バーの "starting session…" は通知が出ている間は隠れるため、
    // 打った入力がどうなったかをここで伝える（異常ではないので [`set_hint`]）
    set_hint(
        app,
        "starting session — keys are held until it takes the terminal".to_string(),
    );
    true
}

/// 溜めた入力を宛先へ流す。**宛先が打ち先として成立していることは呼び手
/// （[`lift_input_gate`]）が確かめた後**なので、ここは流すだけ。
///
/// **encode はここで初めて行う**（[`crate::keys::encode_held`]）。溜める時点で
/// バイト列にすると、子が有効化する前のプロトコルで組んだ列を流すことになる。
///
/// 表示が宛先を指しているのに窓が無い（表示と窓がずれた）ときは流し先が無いので
/// 捨てる ＝ ここでも黙っては捨てない
fn flush_held_input(app: &mut App) {
    if app.held_input.is_empty() {
        return;
    }
    let Some(at) = app.focused_window() else {
        discard_held_input(app);
        return;
    };
    let held = std::mem::take(&mut app.held_input);
    let bytes = crate::keys::encode_held(&held, &app.windows[at].parser.lock_recover());
    if !bytes.is_empty() {
        send_to_active(app, &bytes);
    }
}

/// 溜めた入力を捨てて、**捨てたことを伝える**。
///
/// 報告が [`set_notice`]（error.log にも残す）なのは、打った本人にしか
/// 気づけない損失だから: 下部バーの数秒を見逃すと「打ったはずの文字が無い」
/// だけが残る。門番を降ろすときに 1 度しか通らないので、行が溢れることもない
fn discard_held_input(app: &mut App) {
    let dropped = app.held_input.len();
    if dropped == 0 {
        return;
    }
    app.held_input.clear();
    set_notice(
        app,
        format!("dropped {dropped} input(s) typed while starting — no session to deliver them to"),
    );
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
/// **溜めた入力（[`App::held_input`]）の行き先も同じ答えから出す。** 門番が立って
/// いる間にユーザーがフォーカススロットを別のセッションへ移していたら、そちらへ
/// 流してはいけない ＝ 「打ち先はどこか」を 2 度別々に判断しない。
///
/// **門番が立っていなければ何もしない。** セッションを起こさない供給元（撮影用）は
/// 門番を立てずにここへ合流するので、そこでフォーカスを動かすと
/// 「起動したのにキーがサイドバーへ行く」という直したはずの問題が戻る
fn lift_input_gate(app: &mut App, destination: Option<&SessionId>) {
    if app.input_gate.take().is_none() {
        return;
    }
    if destination.is_some_and(|id| app.showing(id)) {
        flush_held_input(app);
        return;
    }
    // 宛先が居ない。フォーカスを戻せばキーはサイドバー操作になり、
    // ユーザーは打ち先を選び直せる（Alt+→ / 行を開く）。
    // 溜めた入力は流し先が無いので捨てる（黙っては捨てない）
    app.set_focus(Focus::Sidebar);
    discard_held_input(app);
}

/// 応答しない起動から入力を取り戻す（run ループが毎周見る）。降ろしたら `true`。
///
/// **宛先はフォーカススロットの表示が答える。** 期限切れは「宛先が消えた」ではなく
/// 「まだ掴んでいない」なので、**窓が出ているならフォーカスは動かさない**:
/// ここで奪うと、子が生きて見えているペインへ打てなくなる（キーはサイドバー操作に
/// なり ↑↓ で選択だけが動く ＝ ペインが無反応に見える）。`-r` の再開は transcript が
/// 大きいほど遅く、期限を超えること自体は珍しくないので、**期限切れ ＝ 異常**とは
/// 扱わない（報告も分ける。宛先が居る間は [`set_hint`] ＝ error.log に残さない）。
///
/// **溜めた入力もここで流れる**（宛先が居れば）。期限切れは「もう素通しさせる」
/// という決定なので、以降の打鍵が生の PTY へ行く以上、それより前に打たれた
/// 入力だけを捨てる理由が無い（捨てると順序も壊れる）。
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
    // 打ち先の判断は [`lift_input_gate`] 1 箇所に残す（ここは宛先を渡すだけ）。
    // **`shown_session` は定義上「打ち先として成立している行」**（[`App::showing`]
    // が見るのと同じ 1 つの表示）なので、居るかどうかがそのまま報告の分かれ目になる
    let shown = app.shown_session().cloned();
    // **報告を先に置くのは、溜めた入力を捨てたときの報告を消さないため。**
    // 通知は 1 つしか出せない（[`App::notice`]）ので、後から出す側が勝つ。
    // ここで伝えたいのは「遅い / 応答しない」だが、**打った文字が消えたことの方が
    // 利用者にとって取り返しがつかない**ので、そちらを後に出す
    // （[`discard_held_input`] は error.log にも残す）
    if shown.is_some() {
        // 遅いだけで打ち先は生きている。**フォーカスは端末に残る**ので、
        // 以降の打鍵はそのペインへ届く（異常ではない ＝ error.log には残さない）
        set_hint(
            app,
            "session start is slow — keys are delivered again".to_string(),
        );
    } else {
        // 宛先が居ないまま期限を過ぎた（下部バーと error.log の両方。ここは異常）
        set_notice(
            app,
            "session start is not responding — input moved back to the sidebar".to_string(),
        );
    }
    lift_input_gate(app, shown.as_ref());
    true
}

/// 止まっている行の起こし直し方と、**その起動先の cwd**。
///
/// **推測で resume しない**のがこの関数の芯。渡す ID が違えば別の会話が開くか、
/// 見つからずに落ちる。だから材料は 2 つで、両方揃ったときだけ名指しする:
///
/// | 行の会話 | 記録の在り処 | 起こし方 |
/// |:--|:--|:--|
/// | 確かめた（`Observed`） | 分かる | その cwd で `-r <id>` / `resume <id>` |
/// | 確かめた（`Observed`） | 分からない | 行の cwd で**新規**（後述） |
/// | それ以外 | — | 行の cwd で **agent のピッカー**（[`Launch::Pick`]） |
///
/// **2 段目が新規なのは、名指しもピッカーも答えを持たないから**: 記録の在り処が
/// 分からない ＝ その作業ツリーが消えている（[`crate::title::Titles::resume_cwd`]）
/// ので、`-r <id>` は `No conversation found`（実測）、ピッカーはその cwd の会話
/// しか並べないのでその会話が出てこない。新規なら少なくとも行が動く。
///
/// **3 段目がピッカーなのは、こちらには答えがあり得るから**: 会話を確かめて
/// いないだけで、agent 側には会話が在るかもしれない（hook を取り逃した等）。
/// ccdesk が勝手に 1 つ選ぶより、ユーザーに選ばせる方が壊さない。
///
/// **cwd を返すのが要点**: セッションは走行中に git worktree へ移れて、移った先の
/// 会話は行の cwd から `-r` を打っても見つからない
/// （`/resume` のピッカーに出ないのと同じ範囲の話）。
///
/// **材料を揃えるのはここではない**（[`recover_conversation`]）: この関数は
/// 「材料がこうなら、こう起こす」だけを言う ＝ 副作用を持たないので単体で検査できる
fn relaunch<'a>(
    titles: &crate::title::Titles,
    row: &'a SessionRow,
) -> (Launch<'a>, std::borrow::Cow<'a, str>) {
    let here = std::borrow::Cow::Borrowed(row.cwd.as_str());
    let Some(id) = row.conversation.observed() else {
        return (Launch::Pick, here);
    };
    match titles.resume_cwd(row) {
        Some(cwd) => (Launch::Resume { id }, std::borrow::Cow::Owned(cwd)),
        None => (Launch::New { prompt: "" }, here),
    }
}

/// 1 行を起こすときに決まっていること（[`launch_plan`] の答え）。
///
/// **起動先の cwd は行の cwd と別物**（走行中に git worktree へ移った会話は
/// そちらで開く。[`relaunch`]）なので、決めた側が持って回る
struct LaunchPlan<'a> {
    kind: crate::backend::Kind,
    launch: Launch<'a>,
    cwd: String,
}

/// **起こし直し方を決める唯一の口。** 材料を揃えてから [`relaunch`] に聞く
/// （揃える中身は [`recover_conversation`]）。
///
/// 揃える段を [`relaunch`] の外に置いたのは、あちらが副作用を持たない純関数だから
/// （表の 3 段は「材料がこうならこう起こす」だけを言う）。逆に、揃える段を
/// [`ensure_window`] に直に書くと、行を借りる前後の順番と混ざって読めなくなる
fn launch_plan<'a>(app: &'a mut App, id: &SessionId) -> Option<LaunchPlan<'a>> {
    recover_conversation(app, id);
    let row = app.row(id)?;
    let kind = row.kind;
    let (launch, cwd) = relaunch(&app.titles, row);
    Some(LaunchPlan { kind, launch, cwd: cwd.into_owned() })
}

/// 起こし直す前に、**会話を知っている材料を全部使う**（hook の記録 → 記録の解決）。
///
/// 行の会話は `sessions.json` だけが持っているのではない: hook の記録
/// （`hook-states.json`。鍵は行 ID）もその行が最後に名乗った会話を覚えていて、
/// 周期処理（[`adopt_hook_states`]）が 2 秒ごとにそれを行へ写している。
/// **起こし直しはその周期を待たない**ので、写す前に起こすと保管に会話が
/// 載っていない行が「確かめていない行」として `claude -r`（値なし ＝ agent の
/// 選択画面）で開く ＝ **ccdesk を再起動すると、前回動いていたセッションが
/// resume の選択画面になる**（報告されたバグ。起動列は前回のスロットを
/// 復元してから最初の周期を回すので、この経路が必ず先に来る）。
///
/// **記録の解決までやり直す**のが要点: [`relaunch`] は記録の在り処から起動先を
/// 決めるので、会話だけ戻して記録が無いままだと*新規*で起こしてしまう
/// （会話が分からない行の記録は [`crate::title`] が落とす ＝ 会話を失った行は
/// 記録も失っている）。
///
/// 確かめ済みの行では何もしない（起こし直しのたびに走査を増やさない）
fn recover_conversation(app: &mut App, id: &SessionId) {
    if app
        .row(id)
        .is_none_or(|row| row.conversation.observed().is_some())
    {
        return;
    }
    adopt_conversations(app);
    // 会話が戻った行だけ記録を引き直す（戻らなければ走査する理由も無い）
    if app
        .row(id)
        .is_some_and(|row| row.conversation.observed().is_some())
    {
        refresh_transcripts(app);
    }
}

/// 見出しの短い ID をクリップボードへ（押せる場所は [`crate::ui::id_zone`]）。
///
/// **載せるのは 8 桁**（`ccdesk list` が並べる値 ＝ `ccdesk send` が前置き一致で
/// 受ける値）なので、貼ればそのまま宛先として打てる。フル ID を載せても
/// 同じ宛先を指せるが、見えている文字と載る文字が違うと「何をコピーしたのか」が
/// 画面から読めない。
///
/// **フォーカスは動かさない**（✕ と同じ）: コピーは今打っている先を変えない操作で、
/// ここでスロットを掴むと打鍵の行き先が黙って移る。
/// 失敗（クリップボードを取れない環境）は下部バーへ ＝ 押したのに何も起きない、を作らない
fn copy_session_id(app: &mut App, id: &SessionId) {
    let short = id.short();
    if ccdesk::set_clipboard(&short) {
        set_hint(app, format!("copied {short}"));
    } else {
        set_notice(app, format!("could not copy {short} to the clipboard"));
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
    if !ensure_window(app, id, app.focus_slot_size()) {
        return false;
    }
    app.show_session(id);
    true
}

/// 前回の並びを復元する（起動列の最後。呼ぶ前に [`App::set_layout`] で
/// スロットの枚数を配置へ合わせておく）。
///
/// **スロットごとに `focus_slot` を移してから開く**ので、「開いたものは
/// フォーカススロットへ入る」という規則 1 つで復元まで賄える。
///
/// **保存どおりに戻すだけ**で、空スロットを new session 画面で埋めない:
/// 空の `no session` は「ここには今なにも無い」という状態そのもの
/// （stop / close が残すのと同じもの。[`remove_window`]）なので、再起動のたびに
/// 書きかけのフォームへ化けるのは、閉じたという操作の結果として読めない。
/// 以前は「窓が 1 つも開かなかったら New 画面」という条件で塞いでいたため、
/// **全スロットを空にして終了すると次の起動で New 画面が出ていた**。
///
/// New 画面を出すのは**保存された並びが 1 つも無いとき ＝ 初回起動**だけ
/// （そこは戻すべき「前回」が無い ＝ 入口を出す以外にできることが無い）
pub(crate) fn restore_slots(app: &mut App, saved: Vec<SlotView>) {
    // 保存が無い ＝ 初回起動（並びを 1 度でも書いていれば、全部空でも "-" が残る）
    let first_run = saved.is_empty();
    for (at, view) in saved.into_iter().enumerate() {
        if at >= app.slots.len() {
            break;
        }
        app.focus_slot = at;
        match view {
            SlotView::Session(id) => {
                let id = SessionId::new(id);
                // 行が消えているセッションは開けない ＝ そのスロットは空のまま
                if app.row(&id).is_some() {
                    open_session(app, &id);
                }
            }
            SlotView::New => app.open_new_view(),
            SlotView::Empty => {}
        }
    }
    app.focus_slot = 0;
    if first_run {
        app.open_new_view();
    }
}

/// **その行を動かす窓を用意する**（生きていればそのまま、無ければ起こし直す）。
/// 起こし方と起動先は [`relaunch`] が決める。
///
/// **画面のどこへ出すかは含まない。** 出し先が違う入口（サイドバーのクリック ＝
/// [`open_session`] / ドラッグ＆ドロップ ＝ [`drop_session`]）で、起こし直しの
/// 手順を 2 本持たないための切れ目 ＝ **止まっている行はどちらの入口からでも
/// 同じように再開する**（ドロップだけ再開しなかったのはここが分かれていたため）。
///
/// `size` は起こすときの PTY 初期サイズ（落とし先のスロットの内寸）。
/// 戻り値は「この行を動かす窓が居るか」
fn ensure_window(app: &mut App, id: &SessionId, size: (u16, u16)) -> bool {
    if let Some(i) = app.window_index(id) {
        if app.windows[i].alive() {
            // **その行を開いた時点が既読の契機**（切替も再開も同じ）
            mark_read(app, id);
            return true;
        }
        // 直前に死んだ窓（生死スキャンがまだ拾っていない）は開かない:
        // 固まった死画面が出て、次のスキャンで意図せず New 画面へ飛ばされる。
        // 外して下の起こし直しへ落とす（メニューの stop 判定 `session_open` と
        // 同じく、生きた窓だけを「開いている」と数える）
        let _ = app.windows[i].child.kill();
        remove_window(app, i);
    }
    // **別のインスタンス（または ccdesk の外）で動いている会話を
    // `claude -r` で二重に起こさない**: 同じ会話を 2 プロセスが同時に更新する。
    // **照合は会話 ID**（ライブ状態が載せているのは会話の側で、ccdesk の行 ID は
    // どこにも出ない）。判定材料はライブ状態（生きている前景セッションだけが載る）。
    // **自分が止めた行の残像は数えない。** 観測は 2 秒周期なので、`stop` の直後は
    // まだ「動いている」ものとして載っている。**観測が自分の停止より古いなら、
    // その観測は停止を反映しようがない**ので、実行中の証拠として使わない
    // （行の状態表示は同じ比較で既に避けている ＝ [`crate::ui`] の救済分岐。
    // この判定だけが漏れており、**止めた直後にその行をクリックすると
    // 「別のウィンドウで動いている」と言われて開けなかった**）。
    //
    // **自分が止めた行にだけ効かせる。** 止めた覚えの無い行は捨てる観測が無い
    // （`stopped_at` に無い行まで「観測が古い」で弾くと、一度も観測していない
    // 起動直後に二重起動の防止そのものが効かなくなる）
    let afterimage = app
        .stopped_at
        .get(id)
        .is_some_and(|stopped_at| app.agents_observed_at <= *stopped_at);
    let running = !afterimage
        && app
            .row(id)
            .and_then(|row| row.conversation.observed())
            .is_some_and(|conversation| {
                app.agents
                    .iter()
                    .any(|a| a.is_interactive() && a.session_id == conversation)
            });
    if running {
        set_notice(
            app,
            format!("session {id} is already running in another window"),
        );
        return false;
    }
    // **注入は行を借りる前に済ませる**（`hook_settings` は notice を出すので
    // `&mut App` が要り、`relaunch` が返す `Launch` は行を借り続ける）
    let injection = hook_settings(app);
    let inject = injection.as_ref().map(as_inject);
    let Some(plan) = launch_plan(app, id) else {
        return false; // 再読み込みで消えた行（クリックと削除の競合）は何もしない
    };
    let spawn = plan
        .kind
        .spawn_command(id, &plan.cwd, plan.launch, inject.as_ref());
    let conversation = spawn.conversation;
    let (rows, cols) = size;
    match Session::spawn(id, spawn.cmd, rows, cols) {
        Ok(window) => {
            // **起こし直しで会話が変わり得る**（記録を見失った行の新規起動）。
            // 行はそのままで、載っている会話だけを差し替える。**保存まで済ませる**
            // のは、記録を見失った行を新規で起こし直したときに古い会話が
            // ディスクへ残るため（次に開くとその古い会話を名指ししてしまう）
            if let Some(row) = app.row_mut(id) {
                row.conversation.assign(conversation);
            }
            save_sessions(app);
            // 既読は**起こせてから**付ける（起こせなかった行の未読 ● を、
            // 内容を見ていないのに消さない）
            mark_read(app, id);
            // この行を動かす窓がまた居る ＝ 「自分が止めた」の記録は役目を終えた
            // （消さないと止めた行の数だけ溜まる）
            app.stopped_at.remove(id);
            app.windows.push(window);
            // 再開は transcript の読み直しに時間がかかりうる。子が端末を掴むまでの
            // 打鍵は溜める（[`hold_input_while_starting`]）
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

    use crate::sessions::Conversation;
    use crate::source::{persist_projects, WindowState};
    use crate::ui::MIN_SIDEBAR;

    const TERM: (u16, u16) = (120, 40);

    /// **通知は agent が名乗った出来事で撃つ**（行の state の変わり目ではない）。
    ///
    /// この 1 本で消える誤報は、どれも実際に鳴っていたもの:
    /// ccdesk の起動・セッションの起動・`stop` の解除・Esc 中断・
    /// ペインの中の `/config` `/resume`（[`update_notifications`]）
    #[test]
    fn notifications_fire_on_what_the_agent_announced() {
        use crate::notify::{Kind, Wanted};

        let all = Wanted { waiting: true, finished: true };
        let row = |id: &str, launched| (SessionId::new(id), launched);
        // 周を順に食わせて、その間に撃った物を全部並べる（1 周だけの判定では
        // 「同じ出来事を撃ち直さない」が表せない）
        let fired = |windows: &[(SessionId, u64)],
                     rounds: &[Option<(Kind, u64)>],
                     wanted| {
            let mut announced = std::collections::HashMap::new();
            let mut out = Vec::new();
            for alert in rounds {
                out.extend(
                    announce(windows, |_| *alert, &mut announced, wanted)
                        .into_iter()
                        .map(|(_, kind)| kind),
                );
            }
            out
        };
        let one = [row("s", 1_000)];

        // **呼び出しを 1 つも名乗っていない行では鳴らない。**
        // 起動（`SessionStart`）・打鍵（`UserPromptSubmit`）・終了（`SessionEnd`）・
        // Esc 中断（`Interrupt`）はどれも `alert` を持たない ＝ ここへ来ない。
        // **ユーザーが開いた `/config` `/resume` のダイアログも hook を撃たない**
        assert!(fired(&one, &[None, None], all).is_empty());

        // 名乗った出来事はその 1 回だけ撃つ（同じ出来事を毎周撃ち直さない）
        let waiting = Some((Kind::NeedsInput, 2_000));
        assert_eq!(fired(&one, &[waiting, waiting, waiting], all), vec![Kind::NeedsInput]);
        // 続きの出来事は別の 1 回（時刻が進んでいれば種類が同じでも撃つ）
        assert_eq!(
            fired(&one, &[waiting, Some((Kind::NeedsInput, 3_000))], all),
            vec![Kind::NeedsInput, Kind::NeedsInput]
        );
        assert_eq!(
            fired(&one, &[waiting, Some((Kind::Finished, 3_000))], all),
            vec![Kind::NeedsInput, Kind::Finished]
        );

        // **窓を起こす前の出来事は撃たない。** 保管はセッションが終わっても残るので、
        // これが無いと ccdesk の起動や行の開き直しで過去の `Stop` が一斉に鳴る
        assert!(fired(&one, &[Some((Kind::Finished, 999))], all).is_empty());
        assert_eq!(fired(&one, &[Some((Kind::Finished, 1_000))], all), vec![Kind::Finished]);

        // **窓を持たない行は撃たない**（他インスタンスの行。持ち主の ccdesk も
        // 同じ出来事を見ているので、撃つと 2 プロセスで 2 回鳴る）
        assert!(fired(&[], &[Some((Kind::Finished, 2_000))], all).is_empty());

        // 設定で選んだ種類だけが出る
        let only_waiting = Wanted { waiting: true, finished: false };
        assert!(fired(&one, &[Some((Kind::Finished, 2_000))], only_waiting).is_empty());
        let only_done = Wanted { waiting: false, finished: true };
        assert!(fired(&one, &[waiting], only_done).is_empty());

        // **並びは行 ID 順に固定**（HashMap の走査順に引きずられない）
        let mut announced = std::collections::HashMap::new();
        let two = announce(
            &[row("b", 0), row("a", 0)],
            |_| Some((Kind::Finished, 1_000)),
            &mut announced,
            all,
        );
        assert_eq!(
            two.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        // 窓の無くなった行の記録は残さない（覚えるのは今動いている行だけ）
        announce(&[], |_| Some((Kind::Finished, 1_000)), &mut announced, all);
        assert!(announced.is_empty(), "the memo outlived the windows");
    }

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

    /// 起動中に打つ 1 打鍵。門番（[`hold_input_while_starting`]）は「今の入力」を
    /// 関数で受けるので、素の打鍵を渡すだけのテストはこれを使う
    fn typed(c: char) -> impl FnOnce() -> HeldInput {
        move || HeldInput::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    fn open(app: &mut App, kind: PopupKind, anchor_y: u16) {
        app.popup = Some(Popup {
            kind,
            anchor_y,
            selected: 0,
        });
    }

    /// 既定（全 agent）の項目表。agent を切ったときの見え方は
    /// 一覧を明示するテストが自分で `entries` を呼ぶ
    fn labels(kind: &PopupKind, grouping: Grouping) -> Vec<String> {
        kind.entries(grouping, &Kind::ORDER)
            .into_iter()
            .map(|entry| entry.label)
            .collect()
    }

    /// (表示名, 実行可能か) の一覧（項目表の見た目を比べるテスト用）
    fn entry_pairs(kind: &PopupKind, grouping: Grouping) -> Vec<(String, bool)> {
        kind.entries(grouping, &Kind::ORDER)
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
        app.usage = Kind::ORDER.into_iter().map(|k| (k, usage.clone())).collect();
        (app, refreshes)
    }

    /// テスト用供給元が記録した「取り直しを頼まれた agent」の並び
    fn usage_asked() -> Arc<Mutex<Vec<Kind>>> {
        USAGE_ASKED.with(|slot| Arc::clone(&slot.borrow()))
    }

    thread_local! {
        /// [`TestSource::for_usage`] が作った記録の写し（テストから引くため）
        static USAGE_ASKED: std::cell::RefCell<Arc<Mutex<Vec<Kind>>>> =
            std::cell::RefCell::new(Arc::new(Mutex::new(Vec::new())));
    }

    /// **右下の使用率を押すとその場で取り直す**（周期を待たない）。
    /// 当たり判定は描画と同じ導出（`ui::usage_hit`）なので、見えている場所と
    /// 押せる場所がずれない
    #[test]
    fn clicking_the_usage_gauge_refetches_it() {
        let (mut app, refreshes) = usage_app();
        let hits = crate::ui::usage_hits(&app);
        // **行は出す agent ごとに 1 本ずつ**（数も並びも [`App::kinds`] が決める）
        assert_eq!(
            hits.iter().map(|h| h.kind).collect::<Vec<_>>(),
            app.kinds,
            "the gauge does not have one row per agent"
        );
        let top = TERM.1 - crate::ui::bottom_bar_rows(&app);
        assert_eq!(
            hits.iter().map(|h| h.row).collect::<Vec<_>>(),
            vec![top, top + 1]
        );

        // 領域の左端・右端どちらを押しても当たる
        let hit = &hits[0];
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

    /// **押した行の agent だけを取り直す。** 行をまたいで取りに行くと、
    /// 見てもいない agent のプロセスが起きる
    #[test]
    fn clicking_one_usage_row_refetches_only_that_agent() {
        let (mut app, refreshes) = usage_app();
        let asked = usage_asked();
        for hit in crate::ui::usage_hits(&app) {
            let before = refreshes.load(std::sync::atomic::Ordering::Relaxed);
            handle_mouse(&mut app, &click(hit.columns.start, hit.row)).unwrap();
            assert_eq!(
                refreshes.load(std::sync::atomic::Ordering::Relaxed),
                before + 1,
                "one click asked for more than one refetch"
            );
            assert_eq!(
                asked.lock_recover().last().copied(),
                Some(hit.kind),
                "the click on row {} went to the wrong agent",
                hit.row
            );
        }
    }

    /// **帯が乗るのは乗っている 1 行だけ**（押せる場所と光る場所を 1 対 1 にする）
    #[test]
    fn hovering_one_usage_row_marks_only_that_row() {
        let (mut app, _) = usage_app();
        let hits = crate::ui::usage_hits(&app);
        for hit in &hits {
            handle_mouse(&mut app, &moved(hit.columns.start, hit.row)).unwrap();
            assert_eq!(
                app.usage_hovered,
                Some(hit.kind),
                "row {} marked the wrong agent",
                hit.row
            );
        }
    }

    /// **使用率の外を押しても取り直さない。** 右ペイン側の列に描かれるので、
    /// 1 桁ずれただけでペインのクリックへ落ちることを固定する
    #[test]
    fn clicking_outside_the_usage_gauge_does_not_refetch() {
        let (mut app, refreshes) = usage_app();
        let hits = crate::ui::usage_hits(&app);
        let hit = hits.first().expect("the gauge is on screen");
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
        let hits = crate::ui::usage_hits(&app);
        let hit = hits.first().expect("the gauge is on screen");

        // 乗った ＝ 点く + 描き直す
        let prev = (app.hovered, app.usage_hovered);
        handle_mouse(&mut app, &moved(hit.columns.start, hit.row)).unwrap();
        assert!(app.usage_hovered.is_some(), "the gauge is not marked as hovered");
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
        assert!(app.usage_hovered.is_none(), "the mark stays after the mouse left");
    }

    /// **押した瞬間からスピナーが回る。** 旗はクリック側が立てる（取得スレッド任せ
    /// だと最初の描き直しまで反応が出ない）。ただし立てるのは取り直しを実際に
    /// 頼めたときだけ ＝ 取得しない供給元（撮影用）では、降ろす者がいない旗を立てない
    #[test]
    fn a_click_starts_the_spinner_only_when_a_fetch_was_requested() {
        // 取り直しを頼めた ＝ 回し始める
        let (mut app, _) = usage_app();
        let hit = crate::ui::usage_hits(&app).remove(0);
        handle_mouse(&mut app, &click(hit.columns.start, hit.row)).unwrap();
        assert!(
            app.usage_fetching[&hit.kind].load(std::sync::atomic::Ordering::Relaxed),
            "the spinner did not start on click"
        );

        // 取得しない供給元（既定の DemoSource は取り直さない）＝ 立てない
        let mut app = test_app(34, TERM);
        app.usage = Kind::ORDER
            .into_iter()
            .map(|k| (k, crate::usage::sample_ready(Vec::new())))
            .collect();
        let hit = crate::ui::usage_hits(&app).remove(0);
        handle_mouse(&mut app, &click(hit.columns.start, hit.row)).unwrap();
        assert!(
            !app.usage_fetching[&hit.kind].load(std::sync::atomic::Ordering::Relaxed),
            "a spinner started that no one will stop"
        );
    }

    /// **出ていないものは押せない。** 使用率を切っている（`Unknown`）ときは
    /// 当たり判定そのものが無い ＝ 見えない場所を押して claude が起きない
    #[test]
    fn a_hidden_usage_gauge_has_no_hit_area() {
        let app = test_app(34, TERM);
        assert!(app.usage.is_empty(), "the fixture's premise broke");
        assert!(crate::ui::usage_hits(&app).is_empty());
    }

    /// notice を出している間は下部バーが notice に置き換わるので、当たり判定も無い
    #[test]
    fn a_notice_takes_the_bottom_bar_and_the_hit_area_with_it() {
        let (mut app, _) = usage_app();
        assert!(
            !crate::ui::usage_hits(&app).is_empty(),
            "the fixture's premise broke"
        );
        app.notice = Some(("something happened".to_string(), std::time::Instant::now()));
        assert!(crate::ui::usage_hits(&app).is_empty());
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
                .entries(Grouping::State, &Kind::ORDER)
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
            labels(&PopupKind::State, Grouping::State),
            ["● state", "  directory", "  agent"]
        );
        assert_eq!(
            labels(&PopupKind::State, Grouping::Directory),
            ["  state", "● directory", "  agent"]
        );
        assert_eq!(
            PopupKind::State.action(Grouping::State, 0),
            Some(PopupAction::SetGrouping(Grouping::State))
        );
        assert_eq!(
            PopupKind::State.action(Grouping::State, 1),
            Some(PopupAction::SetGrouping(Grouping::Directory))
        );
        assert_eq!(
            PopupKind::State.action(Grouping::State, 2),
            Some(PopupAction::SetGrouping(Grouping::Agent))
        );
        assert_eq!(
            PopupKind::State.action(Grouping::State, Grouping::ORDER.len()),
            None
        );
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
            Some(&PopupKind::State),
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
        open(&mut app, PopupKind::State, 3);
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
        // **agent ごとに項目が並ぶ**（この入口は New 画面を通さず即起動するので、
        // 押す前にどちらが起きるか分かる必要がある）
        assert_eq!(
            labels(&kind, Grouping::State),
            ["new claude session", "new codex session", "remove project"]
        );
        assert_eq!(
            kind.action(Grouping::State, 0),
            Some(PopupAction::NewSessionIn(
                Kind::Claude,
                "C:\\dev\\shop-app".to_string()
            ))
        );
        assert_eq!(
            kind.action(Grouping::State, 1),
            Some(PopupAction::NewSessionIn(
                Kind::Codex,
                "C:\\dev\\shop-app".to_string()
            ))
        );
        assert_eq!(
            kind.action(Grouping::State, 2),
            Some(PopupAction::RemoveProject("C:\\dev\\shop-app".to_string()))
        );
        assert_eq!(kind.action(Grouping::State, 3), None);
    }

    /// Esc は開いているメニューを閉じる（階層を持たないので戻り先も無い）。
    /// 外クリックも同じ
    #[test]
    fn esc_and_outside_click_close_the_popup() {
        let mut app = test_app(34, TERM);
        let menu = || PopupKind::State;
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
        open(&mut app, PopupKind::State, 3);
        for _ in 0..5 {
            handle_popup_key(&mut app, KeyCode::Down);
        }
        // 末尾で止まる（項目数は [`Grouping::ORDER`] が決める ＝ 数を書き写さない）
        assert_eq!(
            app.popup.as_ref().unwrap().selected,
            Grouping::ORDER.len() - 1
        );
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
            .entries(app.grouping, &app.kinds)
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
        open(&mut app, PopupKind::State, 3);
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
        let kinds = || vec![session("s1", false), PopupKind::State, project_menu()];
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
        // 末尾の行 = `remove project`（実行されると登録から外れる ＝ 結果が見える）
        let last = rect.y + rect.height - 2;
        handle_mouse(&mut app, &click(border_col, last)).unwrap();
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
    /// `⊞ group`・プロジェクト見出し・セッション行が積まれ、
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

    /// 1 フレーム描いて下部バーの案内の行を読む。**本番と同じ [`draw`]** を通す
    /// ので、サイドバーの行の積み方と案内の対応がそのまま検査対象になる。
    ///
    /// 読むのは下部バーの**1 行目**（案内はそこに出る。位置の正本は
    /// [`crate::ui::BOTTOM_BAR_ROWS`] なので、最下行を数え直さない）
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
        let hint_row = h - crate::ui::bottom_bar_rows(app);
        (0..w).map(|x| buffer[(x, hint_row)].symbol()).collect()
    }

    /// 触れる位置（一覧の実体のある行）を、**描画が積んだ行から**列挙する
    fn touchable_positions(app: &mut App) -> Vec<SidebarPos> {
        drawn_bottom_bar(app); // 1 フレーム描いて行を積む
        (0..app.sidebar_rows.len())
            .filter(|row| app.sidebar_rows[*row].selectable())
            .map(SidebarPos::Row)
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
            if matches!(expected, Some(Enter::UpdateCcdesk | Enter::UpdateAgent)) {
                continue;
            }
            press(&mut app, KeyCode::Enter);
            match expected {
                Some(Enter::Menu) => assert!(app.popup.is_some(), "{pos:?}: no menu opened"),
                Some(Enter::NewSession) => assert!(
                    app.focus_is_new(),
                    "{pos:?}: the new session screen did not open"
                ),
                Some(Enter::UpdateCcdesk | Enter::UpdateAgent) => unreachable!(),
                None => {
                    assert!(app.popup.is_none(), "{pos:?}: a menu opened on a row that offers nothing");
                    assert!(
                        !app.focus_is_new(),
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

        // `+ new session` まで下りると別の動詞になる。**間に何行あるかは数えない**:
        // ヘッダーに行を足すたびに数を直す形にすると、この test が測っている
        // 「案内が選択に追従するか」とは無関係な理由で落ちるようになる
        let steps = app.sidebar_rows.len();
        for _ in 0..steps {
            if selected_enter(&app) == Some(Enter::NewSession) {
                break;
            }
            press(&mut app, KeyCode::Down);
        }
        let new_session_row = drawn_bottom_bar(&mut app);
        assert!(
            new_session_row.contains("Enter new session"),
            "the hint did not switch to the next row's verb: {new_session_row:?}"
        );

        // 戻しても同じフレームで戻る（片方向だけ追従しているのではない）
        app.selection = SidebarPos::Row(0);
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
            SidebarRow::Action(RowAction::UpdateAgent(Kind::Claude)),
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
            (2, 1, RowAction::UpdateAgent(Kind::Claude)),
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

    /// 版行の右端に置く語（`update` / `restart to update`）は内容の最右列で終わるので、
    /// **幅変更のつかみ代（境界線の 2 列）には掛からない**。1 桁でも外すと
    /// 右端の語のクリックがサイドバー幅変更に化ける
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
        open(&mut app, PopupKind::State, 3);
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
        /// [`DataSource::refresh_usage`] が呼ばれた回数（agent は起こさない）。
        /// **回数まで見る**のは、押していないのに取り直す経路が生えたら気づくため
        usage_refreshes: Arc<std::sync::atomic::AtomicUsize>,
        /// 取り直しを頼まれた agent の並び。**どの行を押したかが正しく届いたか**を
        /// 見る（回数だけだと、別の agent を取りに行っても気づけない）
        usage_asked: Arc<Mutex<Vec<Kind>>>,
        /// [`DataSource::usage`] が返す値（当たり判定は「今出ているか」で決まるので、
        /// 出ている状態を作れる必要がある）
        usage: Usage,
        /// 一覧の保存が呼ばれた回数（**保存しないことを検査する軸**）
        session_saves: Arc<std::sync::atomic::AtomicUsize>,
        /// 書き残された `(配置, スロット枚数)`。**両方を 1 つの軸で持つ**のが要点で、
        /// 片方だけ書かれた状態（次の起動で増えたペインが消える）をそのまま検出できる
        saved_window: Arc<Mutex<(Option<crate::panes::Layout>, Option<usize>)>>,
        /// 「ディスクにはこう載っている」一覧（[`DataSource::sessions`] が返す値）。
        /// **読み直しが何を落とすか**を見るための軸で、実ファイルは読まない
        disk_sessions: Vec<SessionRow>,
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
                usage_refreshes: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                usage_asked: Arc::new(Mutex::new(Vec::new())),
                usage: Usage::Unknown,
                session_saves: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                saved_window: Arc::new(Mutex::new((None, None))),
                disk_sessions: Vec::new(),
            }
        }

        /// ディスクにこの一覧が載っている供給元（読み直しの検査）
        fn for_disk_sessions(disk_sessions: Vec<SessionRow>) -> Self {
            Self {
                disk_sessions,
                ..Self::plain()
            }
        }

        /// 一覧の保存回数だけを見る供給元
        fn for_session_saves(session_saves: Arc<std::sync::atomic::AtomicUsize>) -> Self {
            Self {
                session_saves,
                ..Self::plain()
            }
        }

        /// 使用率が出ている供給元（クリック当たり判定の検査）
        fn for_usage(usage: Usage, usage_refreshes: Arc<std::sync::atomic::AtomicUsize>) -> Self {
            let source = Self {
                usage,
                usage_refreshes,
                ..Self::plain()
            };
            // 記録の写しをテストから引けるようにする（供給元は Arc<dyn> の裏に
            // 隠れるので、ダウンキャストせずに済ませる）
            USAGE_ASKED.with(|slot| *slot.borrow_mut() = Arc::clone(&source.usage_asked));
            source
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

    }

    impl DataSource for TestSource {
        // セッション一覧は差し替えの軸に入れていない（今の検査対象はアカウントと
        // プロジェクト永続化の 2 つだけ）。永続化層を持たない ＝ 読みは 0 件、
        // 保存は渡された一覧をそのまま返す（ディスクが空の単独起動と同じ結果なので
        // live の意味論と矛盾しない。[`ProjectsBackend::Absent`] と同じ判断）
        fn sessions(&self) -> Vec<SessionRow> {
            self.disk_sessions.clone()
        }

        fn store_sessions(&self, next: &[SessionRow]) -> Vec<SessionRow> {
            self.session_saves
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
        fn fixed_states(&self) -> std::collections::HashMap<SessionId, crate::poll::State> {
            std::collections::HashMap::new()
        }

        fn hook_stamp(&self) -> Option<(u64, std::time::SystemTime)> {
            // テストの供給元はファイルを持たない（周期の前倒しは起きない）
            None
        }

        fn footer(&self) -> FooterInfo {
            FooterInfo::default()
        }

        fn usage(&self, _kind: Kind) -> Usage {
            // テスト用の供給元は agent を起こさない（値は仕込まれたものを返すだけ）
            self.usage.clone()
        }

        fn refresh_usage(&self, kind: Kind) -> bool {
            self.usage_asked.lock_recover().push(kind);
            self.usage_refreshes
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            true
        }

        fn window_state(&self) -> WindowState {
            WindowState {
                sidebar_width: 34,
                layout: crate::panes::Layout::One,
                split: crate::panes::Split::default(),
                slots: vec![crate::source::SlotView::New],
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

        // 実ファイル（`~/.ccdesk/state.json`）は書かない。**配置とスロットだけ
        // 控える**（次の起動で読み直される 2 つで、片方だけ書かれると壊れる）
        fn save_window(&self, item: WindowItem<'_>) {
            let mut saved = self.saved_window.lock_recover();
            match item {
                WindowItem::Layout(layout) => saved.0 = Some(layout),
                WindowItem::Slots(slots) => saved.1 = Some(slots.len()),
                _ => {}
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

        /// テストの供給元は設定を読まない（全 agent）。切った見え方を見たい
        /// テストは `App::kinds` を直接置く
        fn kinds(&self) -> Vec<Kind> {
            Kind::ORDER.to_vec()
        }

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
                accounts: Kind::ORDER
                    .into_iter()
                    .map(|kind| (kind, AccountStatus::LoggedIn("taro".to_string())))
                    .collect(),
                ..FooterInfo::default()
            },
            ..test_app(34, TERM)
        }
    }


    /// ホバーの行き先が両方ある `App`: フッターのアカウント行と、一覧の押せる行 1 本
    fn app_with_hoverable_rows() -> App {
        let mut app = app_with_account_footer();
        app.sidebar_rows = vec![SidebarRow::Action(RowAction::New)];
        app.sidebar_header_rows = 1;
        app
    }

    /// マウス移動でホバー位置が変わらないなら描き直さない（FPS 対策）。
    /// 行の中で桁が動いただけでは再描画しない
    #[test]
    fn moving_the_mouse_inside_the_same_row_does_not_redraw() {
        let mut app = app_with_hoverable_rows();
        handle_mouse(&mut app, &moved(3, 1)).unwrap();
        let prev = (app.hovered, app.usage_hovered);
        handle_mouse(&mut app, &moved(10, 1)).unwrap();
        assert_eq!(app.hovered, prev.0, "the same row must resolve to the same hover");
        assert!(
            !mouse_needs_redraw(MouseEventKind::Moved, prev, (app.hovered, app.usage_hovered)),
            "moving inside a row must not ask for a redraw"
        );
        // 行が変われば描き直す
        let prev = (app.hovered, app.usage_hovered);
        handle_mouse(&mut app, &moved(3, 2)).unwrap();
        assert!(
            mouse_needs_redraw(MouseEventKind::Moved, prev, (app.hovered, app.usage_hovered)),
            "leaving a row must ask for a redraw"
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
        // 何か動いている間だけ短い周期でも描く（動いていなければ 1 秒粒度）
        let between = ANIMATION_REDRAW + Duration::from_millis(50);
        assert!(should_draw(false, false, false, between, ANIMATION_REDRAW));
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
        // 末尾の先は一覧の先頭（端で止まらない）
        press(&mut app, KeyCode::Down);
        assert_eq!(app.selection, SidebarPos::Row(0), "must wrap around to the top of the list");
        // 先頭で `↑` は末尾へ回る
        press(&mut app, KeyCode::Up);
        assert_eq!(app.selection, SidebarPos::Row(2), "must wrap around to the bottom");
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
        for (state, name) in [
            (SelfUpdate::Running, "Running"),
            (SelfUpdate::Done, "Done"),
        ] {
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

    /// 起動すらできなかった更新を**無言で終わらせない**。
    ///
    /// 以前は `let _ = …output()` で捨てていたので、押しても何も起きない行だけが
    /// 残った。文面には agent 名と「PATH に無い」が要る（更新はユーザーの環境の
    /// 問題で失敗しうるので、どちらを直せばよいか分かる必要がある）
    #[test]
    fn an_agent_update_that_cannot_start_says_so_instead_of_going_quiet() {
        let err = run_agent_update(("ccdesk-no-such-agent-here", &["update"]))
            .expect_err("a program that is not installed reported success");
        assert!(err.contains("ccdesk-no-such-agent-here"), "{err}");
        assert!(err.contains("PATH"), "the message does not say where it looked: {err}");
    }

    /// **npm が並べて置くシムのうち、Windows が実行できる方を起こす。**
    ///
    /// npm は同じディレクトリへ `codex`（sh のシム）と `codex.cmd` を置く。
    /// `Command::new("codex")` は `CreateProcess` が `PATHEXT` を見ないので
    /// 拡張子なしの方しか見つけられず `NotFound` で終わる ＝ **codex の更新
    /// ボタンだけが無反応**だった（claude は `claude.exe` なので露見しなかった）。
    /// あわせて、渡す部分コマンドが `update` であることと、失敗が理由付きで
    /// 返ることも同じ PATH で見る（PATH の差し替えを 1 回に閉じる）
    #[cfg(windows)]
    #[test]
    fn an_agent_update_runs_the_shim_windows_can_execute_and_reports_failures() {
        let dir = crate::testutil::TempDir::new("app", "agent-update-shim");
        // npm と同じ並び: 拡張子なし（Windows では実行できない）と `.cmd`
        std::fs::write(dir.join("ccdesk-fake-agent"), "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::write(
            dir.join("ccdesk-fake-agent.cmd"),
            "@echo off\r\nif not \"%1\"==\"update\" exit /b 3\r\nexit /b 0\r\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("ccdesk-failing-agent.cmd"),
            "@echo off\r\necho no write access 1>&2\r\nexit /b 1\r\n",
        )
        .unwrap();

        // PATH の先頭へ足すだけにする（丸ごと差し替えると、並列で走る他の
        // テストが本物の実行ファイルを引けなくなる）
        let saved = std::env::var_os("PATH");
        let prepended = match &saved {
            Some(path) => format!("{};{}", dir.path().display(), path.to_string_lossy()),
            None => dir.path().display().to_string(),
        };
        unsafe { std::env::set_var("PATH", prepended) };
        let ok = run_agent_update(("ccdesk-fake-agent", &["update"]));
        let failed = run_agent_update(("ccdesk-failing-agent", &["update"]));
        match saved {
            Some(path) => unsafe { std::env::set_var("PATH", path) },
            None => unsafe { std::env::remove_var("PATH") },
        }

        assert_eq!(
            ok,
            Ok(()),
            "the update did not reach the shim Windows can execute (exit 3 = the subcommand was not `update`)"
        );
        let err = failed.expect_err("a shim that exits non-zero reported success");
        assert!(err.contains("ccdesk-failing-agent"), "{err}");
        assert!(err.contains("no write access"), "the reason was dropped: {err}");
    }

    /// **成功と報告された更新も、版が動かなければ黙って通さない。**
    ///
    /// claude は `update` にも `install latest` にも終了コード 0 と
    /// "Successfully updated" を返しながら実行ファイルを差し替えないことがある
    /// （実測: 版行を押しても `claude --version` は前の版のまま）。ccdesk 側は
    /// 終了コードしか材料を持たないので、版が動いたかどうかで判断する
    #[test]
    fn an_update_that_reported_success_but_moved_nothing_is_not_taken_at_its_word() {
        let behind = |latest: Option<&str>| crate::backend::AgentVersion {
            current: "2.1.225".to_string(),
            latest: latest.map(str::to_string),
        };
        assert!(
            update_left_the_version_behind("2.1.225", &behind(Some("2.1.226"))),
            "a version that did not move was taken as a successful update"
        );
        // 版が動いていれば当然よい
        assert!(!update_left_the_version_behind(
            "2.1.225",
            &crate::backend::AgentVersion {
                current: "2.1.226".to_string(),
                latest: None,
            }
        ));
        // **latest が消えていれば更新するものが無かっただけ**（押した時点で最新
        // だった / 別経路で上がった）＝ 版が動かないのが正しい
        assert!(!update_left_the_version_behind("2.1.225", &behind(None)));
        // 押す前の版が分からなければ何も言えない（版取得そのものが失敗していた）
        assert!(!update_left_the_version_behind("", &behind(Some("2.1.226"))));
        // 取り直しに失敗した（current が空）ときも黙る ＝ 周期ポーラーに委ねる
        assert!(!update_left_the_version_behind(
            "2.1.225",
            &crate::backend::AgentVersion {
                current: String::new(),
                latest: Some("2.1.226".to_string()),
            }
        ));
    }

    /// 据え置きの文面には**どの agent がどの版で止まっているか**が要る
    /// （下部バーは 1 行なので、読んだだけで次の一手が分かる必要がある）
    #[test]
    fn the_stale_update_message_names_the_agent_and_the_version_it_is_stuck_on() {
        let msg = stale_update_message("claude", "2.1.225");
        assert!(msg.contains("claude"), "{msg}");
        assert!(msg.contains("2.1.225"), "the version it is stuck on is missing: {msg}");
        // 原因を断定しない（差し替えられなかった理由は agent 側にしか分からない）
        assert!(
            !msg.contains("failed"),
            "the message blames a failure ccdesk cannot see: {msg}"
        );
    }

    /// 差し替え済み（`Done`）の版行は案内だけで、押しても何も起きない。
    ///
    /// **自動再起動はやめた**: 走ったまま自プロセスを起こすとコンソールを
    /// 親子で奪い合いマウスが効かなくなる不具合が実機で出たため、案内（"restart to update"）
    /// を出すだけに留め、利用者が自分のタイミングで終了・起動し直す運用にした
    /// （[`crate::ui`] の `UpdateState::RestartPending`）。
    #[test]
    fn the_done_row_offers_no_enter_and_does_nothing_when_pressed() {
        let mut app = app_with_every_row_kind();
        *app.ccdesk_update
            .lock_recover() = SelfUpdate::Done;
        let bar = drawn_bottom_bar(&mut app);
        assert_eq!(app.selection, SidebarPos::Row(0), "the fixture's premise broke");
        assert!(
            !bar.contains("Enter"),
            "a done-but-unactionable row must not offer Enter: {bar:?}"
        );
        press(&mut app, KeyCode::Enter);
        assert!(app.popup.is_none(), "Enter on the done row opened something");
        assert!(!app.focus_is_new(), "Enter on the done row switched the right pane");
    }

    /// テスト用の一覧行 1 本（cwd と更新時刻だけが関心事）。
    /// `updated_at` は埋め戻しの「新しい順」に効くので明示で受ける。
    ///
    /// **会話 ID は行 ID とわざと別の値**（`title` 側の fixture と同じ理由 ＝
    /// 行 ID で会話を引く実装でも通ってしまうテストにしない）
    fn session_row(id: &str, cwd: &str, updated_at: u64) -> SessionRow {
        SessionRow {
            updated_at,
            conversation: Conversation::Observed(format!("conv-{id}")),
            ..SessionRow::new(SessionId::new(id), cwd, updated_at)
        }
    }

    /// **別インスタンス（や ccdesk の外）で動いているセッションを `claude -r` で
    /// 二重に起こさない。** ライブ状態に interactive で載っている ＝
    /// どこかで生きて動いている ＝ 二重に開くと同じ会話を 2 プロセスが同時更新する
    #[test]
    fn opening_a_session_running_elsewhere_does_not_double_resume() {
        let mut app = test_app(34, TERM);
        // **照合は会話 ID**（ライブ状態が名乗るのは会話の側。行 ID は出てこない）
        app.sessions = vec![SessionRow {
            conversation: Conversation::Observed("conv-1".to_string()),
            ..session_row("s", "C:\\dev\\api", 1)
        }];
        app.agents = vec![AgentInfo {
            session_id: "conv-1".to_string(),
            kind: "interactive".to_string(),
            status: "busy".to_string(),
            ..AgentInfo::default()
        }];
        assert!(
            !open_session(&mut app, &SessionId::new("s")),
            "reported the session as opened"
        );
        assert!(
            app.windows.is_empty(),
            "spawned a second claude for a session that is already running"
        );
        assert!(app.notice.is_some(), "the user was not told why nothing opened");
    }

    /// **自分で止めた行は、その直後にクリックしても開ける。**
    ///
    /// ライブ状態の観測は [`LIVE_SCAN_INTERVAL`] 周期なので、`stop` の直後は
    /// 止めたばかりのセッションがまだ interactive として載っている。その残像を
    /// 上のテストの状況（＝ 別のどこかで動いている）と同じに扱うと、**止めた行を
    /// 押しても「別のウィンドウで動いている」と言われて開けない**（実機で踏んだ）。
    ///
    /// 観測（[`App::agents_observed_at`]）が自分の停止（[`App::stopped_at`]）より
    /// 古い間は、その観測に停止が反映されているはずがない ＝ 実行中の証拠にしない
    #[test]
    fn opening_a_session_this_ccdesk_just_stopped_is_not_mistaken_for_a_double_resume() {
        let mut app = test_app(35, TERM);
        app.sessions = vec![SessionRow {
            conversation: Conversation::Observed("conv-1".to_string()),
            ..session_row("s", "C:\\dev\\api", 1)
        }];
        // 止めたばかり ＝ 観測はまだ止める前のもの
        app.agents = vec![AgentInfo {
            session_id: "conv-1".to_string(),
            kind: "interactive".to_string(),
            status: "busy".to_string(),
            ..AgentInfo::default()
        }];
        app.agents_observed_at = 1_000;
        app.stopped_at.insert(SessionId::new("s"), 2_000);
        open_session(&mut app, &SessionId::new("s"));
        assert!(
            !app.notice.as_ref().is_some_and(|(msg, _)| msg.contains("already running")),
            "the row this ccdesk just stopped was reported as running elsewhere"
        );
    }

    /// 逆に、**停止より新しい観測で載っているなら本当に動いている**
    /// （別インスタンスが起こし直した場合）ので、二重起動の防止は効いたまま
    #[test]
    fn a_stopped_row_seen_running_again_afterwards_still_blocks_a_double_resume() {
        let mut app = test_app(36, TERM);
        app.sessions = vec![SessionRow {
            conversation: Conversation::Observed("conv-1".to_string()),
            ..session_row("s", "C:\\dev\\api", 1)
        }];
        app.agents = vec![AgentInfo {
            session_id: "conv-1".to_string(),
            kind: "interactive".to_string(),
            status: "busy".to_string(),
            ..AgentInfo::default()
        }];
        app.stopped_at.insert(SessionId::new("s"), 1_000);
        app.agents_observed_at = 2_000;
        assert!(
            !open_session(&mut app, &SessionId::new("s")),
            "reported the session as opened"
        );
        assert!(app.windows.is_empty(), "spawned a second claude anyway");
    }

    /// **テストの供給元は hook 設定を注入しない。**
    ///
    /// 上の 3 つのように `open_session` を通るテストは、そのまま
    /// [`hook_settings`] へ落ちる。ここが実ホームを引いていた頃は、`cargo test` の
    /// たびに開発者の `~/.ccdesk/inject-settings.json` が**テストバイナリのパス**へ
    /// 書き換わり、その後 ccdesk が起こした claude の hook が libtest の
    /// ハーネスを起動していた ＝ **state も会話も記録されず、`stop` した行を
    /// 開くと `-r` のピッカーが出る**（報告されたバグの原因）。
    ///
    /// **書く先が無いことは失敗ではない**ので下部バーにも出さない
    /// （出すと、撮影とテストが毎回「hook を書けなかった」と言い出す）
    #[test]
    fn a_source_that_names_no_hook_dir_injects_nothing_and_does_not_complain() {
        let mut app = test_app(34, TERM);
        assert!(
            app.source.hook_dir().is_none(),
            "the test source offered a directory to write the injection into"
        );
        assert!(
            hook_settings(&mut app).is_none(),
            "an injection was written for a source that has nowhere to write it"
        );
        assert!(
            app.notice.is_none(),
            "having nowhere to inject was reported as a failure"
        );
    }

    /// **止めた行も公開に載る。** 載せないと、相手が終わった瞬間に宛先ごと消えて
    /// `ccdesk read` が記録へ届かなくなる（記録はディスクに在るのに指せない）
    #[test]
    fn a_row_this_ccdesk_stopped_is_still_published() {
        let mut app = test_app(37, TERM);
        app.sessions = vec![session_row("s", "C:\\dev\\api", 1)];
        app.stopped_at.insert(SessionId::new("s"), 1_000);
        let open = open_sessions(&app);
        assert_eq!(open.len(), 1, "the stopped row was dropped: {open:?}");
        assert!(!open[0].running, "a stopped row was published as running");
        assert_eq!(open[0].cwd, "C:\\dev\\api", "the row's details were lost");
    }

    /// 行ごと消えたものは載せない（指す先が無い）
    #[test]
    fn a_stopped_row_that_no_longer_exists_is_not_published() {
        let mut app = test_app(38, TERM);
        app.stopped_at.insert(SessionId::new("gone"), 1_000);
        assert!(open_sessions(&app).is_empty());
    }

    /// **並びが安定していること。** [`App::stopped_at`] は `HashMap` なので、
    /// 整列しないと中身が同じでも周回ごとに並びが変わり、「前回と違う」と見えて
    /// 何も起きていない周にディスクへ書き続けることになる
    #[test]
    fn the_published_order_does_not_change_between_identical_rounds() {
        let mut app = test_app(39, TERM);
        app.sessions = (0..8)
            .map(|i| session_row(&format!("s{i}"), "C:\\dev\\api", 1))
            .collect();
        for i in 0..8 {
            app.stopped_at.insert(SessionId::new(format!("s{i}")), 1_000);
        }
        let first = open_sessions(&app);
        assert_eq!(first.len(), 8, "not every stopped row was published");
        for _ in 0..5 {
            assert_eq!(open_sessions(&app), first, "the published order moved");
        }
    }

    fn project(cwd: &str, has_sessions: bool) -> PopupKind {
        PopupKind::Project {
            cwd: cwd.to_string(),
            has_sessions,
        }
    }

    // ── 起こし直し方 / `/resume` の追従 / リネームの書き先 ──────────────────

    /// **推測で resume しない。** 起こし直し方は 3 通りで、`relaunch` の doc の
    /// 表をそのまま固定する。
    ///
    /// 3 通りに割れているのは、間違った ID を渡すと**別の会話が開くか、
    /// 見つからずに落ちる**から。前景セッションは 1 ターン終わるまで transcript を
    /// 作らないので、起こしてすぐ `close` した行を `-r` で開くと
    /// `No conversation found` になっていた
    #[test]
    fn a_row_is_only_resumed_when_both_the_conversation_and_its_record_are_known() {
        let temp = crate::title::tests::TempProjects::new("relaunch_needs_conversation_and_record");
        let mut titles = temp.titles();

        // 1) 会話を確かめていない行 → agent 自身のピッカー（ID を渡さない）
        let unknown = SessionRow {
            conversation: Conversation::Unknown,
            ..session_row("u", "C:\\dev\\api", 1)
        };
        let (launch, cwd) = relaunch(&titles, &unknown);
        assert!(matches!(launch, Launch::Pick), "guessed a conversation to resume");
        assert_eq!(cwd, unknown.cwd);

        // 渡しただけ（`Assigned`）も同じ扱い。まだ 1 ターンも終わっていない会話を
        // 名指しすると `No conversation found` になる
        let assigned = SessionRow {
            conversation: Conversation::Assigned("conv-a".to_string()),
            ..session_row("a", "C:\\dev\\api", 1)
        };
        assert!(
            matches!(relaunch(&titles, &assigned).0, Launch::Pick),
            "an unconfirmed conversation was resumed by name"
        );

        // 2) 確かめた会話だが記録の在り処が分からない → 行の cwd で新規
        let mut row = session_row("s", "C:\\dev\\api", 1);
        let (launch, cwd) = relaunch(&titles, &row);
        assert!(
            matches!(launch, Launch::New { prompt } if prompt.is_empty()),
            "resumed a conversation whose record cannot be found"
        );
        assert_eq!(cwd, row.cwd, "a fresh start must use the row's own cwd");

        // 3) 両方揃った → **会話 ID**で名指し（行 ID ではない）
        titles.write_transcript(&row, "{\"type\":\"user\"}\n");
        titles.title_now(&mut row);
        let (launch, cwd) = relaunch(&titles, &row);
        assert!(
            matches!(launch, Launch::Resume { id } if id == "conv-s"),
            "resumed with something other than the observed conversation"
        );
        assert_eq!(cwd, row.cwd);
    }

    /// **再起動しても、前回動いていた行は会話へ戻る（選択画面にならない）。**
    ///
    /// 保管（`sessions.json`）に会話が載っていない行でも、hook の記録は
    /// その行が最後に名乗った会話を覚えている。起こし直しがそれを使わないと
    /// `claude -r`（値なし ＝ agent の選択画面）で開く ＝ 報告されたバグ
    /// （ccdesk を再起動すると、前回動いていた claude が resume の選択画面で開く）。
    ///
    /// **記録の解決までやり直すことも一緒に固定する**: 会話だけ戻して記録が
    /// 無いままだと [`relaunch`] は*新規*で起こす ＝ 前回の会話は開かない
    #[test]
    fn a_row_whose_conversation_only_the_hooks_know_is_resumed_not_picked() {
        let temp = crate::title::tests::TempProjects::new("recover_conversation_before_relaunch");
        let mut app = App {
            titles: temp.titles(),
            // hook は前回の会話を覚えている（受け渡しファイルは再起動をまたぐ）
            hook_states: HookStates::from_records([(
                "s",
                crate::poll::State::Idle,
                5_000,
                Some("conv-s"),
            )]),
            ..app_with_row("s")
        };
        // 前回の実行が残した記録（claude 側の持ち物なので再起動では消えない）
        let recorded = SessionRow {
            conversation: Conversation::Observed("conv-s".to_string()),
            ..app.sessions[0].clone()
        };
        app.titles.write_transcript(&recorded, "{\"type\":\"user\"}
");
        // 保管が会話を持たないまま起動した姿（`Assigned` はディスクに載らない ＝
        // hook が一度も保存へ届かなかった行はここに居る）
        app.sessions[0].conversation = Conversation::Unknown;
        app.sessions[0].transcript = None;

        // 前提: このまま聞けば選択画面（＝ バグの姿）
        assert!(
            matches!(relaunch(&app.titles, &app.sessions[0]).0, Launch::Pick),
            "the fixture's premise broke"
        );

        let plan = launch_plan(&mut app, &SessionId::new("s")).expect("the row vanished");

        assert!(
            matches!(plan.launch, Launch::Resume { id } if id == "conv-s"),
            "the row the hooks still know was opened with the agent's picker"
        );
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

    /// **前の行の先頭側スキャンが、後ろの行の名前を飢えさせない。**
    ///
    /// 予算（[`crate::title::SCAN_BUDGET`]）は全行で分け合う。先頭側スキャンは
    /// リネーム記録（`custom-title`）の探索で、**記録が無い transcript では
    /// 先頭までファイルを舐め切るまで止まらない**。実データ 6 本すべてに
    /// `custom-title` が無かったので、これは例外ではなく普通の姿。
    ///
    /// 行ごとに予算を食い切らせていた頃は、後ろの行が末尾走査
    /// （＝名前が出るのに必要な唯一の読み）にすら届かず、**起動から数秒間
    /// `new session` に見えた**（実測で 6 行目に名前が付くのは約 4 秒後）
    #[test]
    fn a_long_head_scan_does_not_starve_the_names_of_later_rows() {
        let temp = crate::title::tests::TempProjects::new("a_long_head_scan_does_not_starve");
        let mut app = app_with_row("first");
        app.sessions.push(session_row("second", "C:\\dev\\api", 1));
        app.titles = temp.titles();

        // 1 行目: リネーム記録が無く、予算を丸ごと食う大きさ（実データの 4.3 MB 相当）
        let filler = format!("{{\"type\":\"noise\",\"text\":\"{}\"}}\n", "x".repeat(1_000));
        let bulk = filler.repeat(crate::title::SCAN_BUDGET as usize / filler.len() + 200);
        let head_bytes = bulk.len() as u64;
        app.titles.write_transcript(
            &app.sessions[0].clone(),
            &format!("{bulk}{{\"type\":\"ai-title\",\"aiTitle\":\"the first name\"}}\n"),
        );
        // 2 行目: 末尾を読めば名前が出る小さな transcript
        app.titles.write_transcript(
            &app.sessions[1].clone(),
            "{\"type\":\"ai-title\",\"aiTitle\":\"the second name\"}\n",
        );
        assert!(
            head_bytes > crate::title::SCAN_BUDGET,
            "the premise broke - the first row does not exhaust the budget"
        );

        refresh_transcripts(&mut app);

        assert_eq!(
            app.titles.of(&app.sessions[1]),
            "the second name",
            "the head scan of the first row ate the budget the second row needed for its name"
        );
        assert_eq!(app.titles.of(&app.sessions[0]), "the first name");
    }

    /// **末尾窓の外にあるリネーム記録が、この関数を通しても拾える。**
    ///
    /// 先頭側の読み残しの消化は末尾走査とは別のパスなので、**そのパスを落としても
    /// 名前は出てしまう**（下位の候補で埋まる）。[`crate::title`] 側の
    /// `a_rename_before_the_tail_window_is_still_found` はテスト用ヘルパーを
    /// 通っていて本番の配線を見ないため、ここで本番の入口ごと固定する
    #[test]
    fn refreshing_transcripts_also_finds_a_rename_outside_the_tail_window() {
        let temp = crate::title::tests::TempProjects::new("refresh_finds_a_rename_outside_the_tail");
        let mut app = app_with_row("s");
        app.titles = temp.titles();

        // 先頭にリネーム、末尾窓を越える詰め物、末尾に下位の候補
        let filler = format!("{{\"type\":\"noise\",\"text\":\"{}\"}}\n", "x".repeat(200))
            .repeat(1_000);
        assert!(
            filler.len() as u64 > crate::title::TAIL_BYTES,
            "the premise broke - the filler fits inside the tail window"
        );
        // 先頭側は 1 周期の残予算で読み切れる大きさ（読み切れないなら、
        // この検査が落ちても「段 3 が走らない」ではなく「予算が足りない」になる）
        assert!(
            filler.len() as u64 + crate::title::TAIL_BYTES < crate::title::SCAN_BUDGET,
            "the premise broke - the head does not fit in one cycle's budget"
        );
        app.titles.write_transcript(
            &app.sessions[0].clone(),
            &format!(
                "{{\"type\":\"custom-title\",\"customTitle\":\"named at the top\"}}\n\
                 {filler}\
                 {{\"type\":\"last-prompt\",\"lastPrompt\":\"the latest prompt\"}}\n"
            ),
        );

        refresh_transcripts(&mut app);

        assert_eq!(
            app.titles.of(only_row(&app)),
            "named at the top",
            "the head scan never ran - the name fell back to the tail candidate"
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
    /// 毎周一覧を読み直すことにはならない
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


    /// **セッションの中で会話が変わっても行は増えない**（1 セッション = 1 行）。
    ///
    /// `/clear` `/resume` `/new` はどれも agent の内部で起きるので ccdesk は
    /// 関与しない。気づく口は hook 1 本で、記録の鍵が行（`CCDESK_ROW`）なぶん、
    /// 差し替わるのは行に載っている会話だけになる。
    ///
    /// **これが「行を増やす」方式との分かれ目。** 増やす方式では `/clear` の
    /// たびにサイドバーへ行が生え、しかも会話 ID を採番しない codex では
    /// 行が作れなかった
    #[test]
    fn a_switch_inside_the_session_moves_the_rows_conversation_without_adding_a_row() {
        let mut app = App {
            sessions: vec![
                SessionRow {
                    conversation: Conversation::Observed("conv-1".to_string()),
                    ..session_row("row", "C:\\dev\\api", 1)
                },
                session_row("untouched", "C:\\dev\\api", 1),
            ],
            hook_states: HookStates::from_records([(
                "row",
                crate::poll::State::Idle,
                5_000,
                Some("conv-2"),
            )]),
            ..test_app(34, TERM)
        };
        adopt_conversations(&mut app);

        assert_eq!(app.sessions.len(), 2, "a row appeared for the new conversation");
        assert_eq!(
            app.sessions[0].conversation,
            Conversation::Observed("conv-2".to_string()),
            "the row kept pointing at the conversation from before the /clear"
        );
        // hook が何も知らない行は触らない
        assert_eq!(
            app.sessions[1].conversation,
            Conversation::Observed("conv-untouched".to_string())
        );
    }

    /// **一覧の読み直しが、渡したばかりの会話を捨てない。**
    ///
    /// ディスクに載るのは確かめた会話だけなので、起動直後の `Assigned`
    /// （ccdesk が採番して `--session-id` で渡した値）は読み直しで消える。
    /// 普段は同じ周期の hook が `Observed` を戻すが、**hook が一度も来ない行**
    /// （注入に失敗した環境）はここで会話を失うと二度と取り戻せない
    #[test]
    fn a_reload_does_not_drop_a_conversation_the_disk_never_stored() {
        let assigned = SessionRow {
            conversation: Conversation::Assigned("conv-just-launched".to_string()),
            ..session_row("s", "C:\\dev\\api", 1)
        };
        // ディスクから返る姿（`Assigned` は保存されないので会話を持たない）
        let mut fresh = vec![SessionRow {
            conversation: Conversation::Unknown,
            ..session_row("s", "C:\\dev\\api", 1)
        }];
        restore_conversations(&mut fresh, std::slice::from_ref(&assigned));
        assert_eq!(
            fresh[0].conversation,
            Conversation::Assigned("conv-just-launched".to_string()),
            "the conversation ccdesk minted was thrown away by the reload"
        );

        // **ディスクが会話を持っていれば触らない**（他インスタンスが確かめた値を
        // こちらの古い写しで踏み潰さない）
        let mut fresh = vec![SessionRow {
            conversation: Conversation::Observed("conv-from-another-instance".to_string()),
            ..session_row("s", "C:\\dev\\api", 1)
        }];
        restore_conversations(&mut fresh, std::slice::from_ref(&assigned));
        assert_eq!(
            fresh[0].conversation,
            Conversation::Observed("conv-from-another-instance".to_string()),
            "clobbered another instance's conversation"
        );

        // 知らない行には何もしない（他インスタンスが作った行）
        let mut fresh = vec![SessionRow {
            conversation: Conversation::Unknown,
            ..session_row("other", "C:\\dev\\api", 1)
        }];
        restore_conversations(&mut fresh, std::slice::from_ref(&assigned));
        assert_eq!(fresh[0].conversation, Conversation::Unknown);

        // **読み直しの経路が実際にこれを通る。** 純関数だけを固定すると、
        // 呼び出しごと外れても気づけない（外れた瞬間、hook が来ない行が
        // 2 秒ごとに会話を失う）
        let mut app = App {
            sessions: vec![assigned.clone()],
            source: Arc::new(TestSource::for_disk_sessions(vec![SessionRow {
                conversation: Conversation::Unknown,
                ..session_row("s", "C:\\dev\\api", 1)
            }])),
            ..test_app(34, TERM)
        };
        refresh_sessions(&mut app);
        assert_eq!(
            app.sessions[0].conversation,
            Conversation::Assigned("conv-just-launched".to_string()),
            "the reload path does not restore the conversation"
        );
    }

    /// **同じ会話を名乗り直した周では保存しない。** 名乗りは turn のたびに来るので、
    /// 毎回保存すると `sessions.json` のロックを無駄に取り合う
    #[test]
    fn re_announcing_the_same_conversation_saves_nothing() {
        let saves = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut app = App {
            sessions: vec![SessionRow {
                conversation: Conversation::Observed("conv-1".to_string()),
                ..session_row("row", "C:\\dev\\api", 1)
            }],
            source: Arc::new(TestSource::for_session_saves(Arc::clone(&saves))),
            hook_states: HookStates::from_records([(
                "row",
                crate::poll::State::Idle,
                5_000,
                Some("conv-1"),
            )]),
            ..test_app(34, TERM)
        };
        adopt_conversations(&mut app);
        assert_eq!(saves.load(std::sync::atomic::Ordering::Relaxed), 0, "saved without a change");

        // 別の会話を名乗ったら 1 度だけ保存する
        app.hook_states =
            HookStates::from_records([("row", crate::poll::State::Idle, 6_000, Some("conv-2"))]);
        adopt_conversations(&mut app);
        assert_eq!(saves.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    /// **セッションが残っているプロジェクトは登録解除できない。** 一覧は
    /// 「登録リスト ∪ セッションの cwd」なので、外しても見出しは出続ける ＝
    /// 押せるのに何も変わらないことになる。stop と同じ実行可能フラグで落とす
    #[test]
    fn project_menu_disables_remove_while_sessions_remain() {
        assert_eq!(
            entry_pairs(&project("C:\\dev\\api", false), Grouping::Directory),
            [
                ("new claude session".to_string(), true),
                ("new codex session".to_string(), true),
                ("remove project".to_string(), true),
            ]
        );
        assert_eq!(
            entry_pairs(&project("C:\\dev\\api", true), Grouping::Directory),
            [
                ("new claude session".to_string(), true),
                ("new codex session".to_string(), true),
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
    /// **フォーカスはサイドバーに残る**（開いたメニューがキーを受ける）。
    ///
    /// **見出しは掴めない**ので、開くのは押した時点（運べるのはメニューの中の
    /// `new <agent> session` の側 ＝ [`drop_new_session`]）
    #[test]
    fn clicking_a_project_heading_opens_its_menu() {
        let mut app = test_app(34, TERM);
        app.sidebar_rows = vec![SidebarRow::Action(RowAction::Project("C:\\dev\\api".to_string()))];
        app.sidebar_header_rows = 1;
        handle_mouse(&mut app, &click(5, 1)).unwrap();
        assert!(app.drag.is_none(), "the heading was grabbed instead of opening its menu");
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
        // 末尾の `remove project` まで下げる（項目数は [`Kind::ORDER`] が決める）
        for _ in 0..Kind::ORDER.len() {
            handle_popup_key(&mut app, KeyCode::Down);
        }
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
        app.slots = vec![Slot::New(NewState::browse(&dir))];
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

    /// **子が端末を掴むまでに打った文字は、素通しもせず捨てもせず溜める。**
    /// フォーカスは端末へ移っているが claude はまだ raw mode に入っていないので、
    /// 素通しすると打った文字が読み捨てられたりエコーに混ざったりする。
    /// かといって捨てると、transcript が大きくて再開の遅い環境では
    /// 「打ったのに何も起きない」が常態になる。
    ///
    /// **打鍵と貼り付けが打った順のまま 1 本の列に載る**ことをここで固定する:
    /// 種類ごとに列を分けると「貼り付けてから Enter」が壊れる。
    /// バイト列の並びと形は [`crate::keys::encode_held`] のテストが受け持つ
    #[test]
    fn input_typed_while_a_session_is_starting_is_held_in_the_order_it_was_typed() {
        let mut app = test_app(34, TERM);
        open(&mut app, project("C:\\dev\\api", false), 5);
        handle_popup_key(&mut app, KeyCode::Enter); // 先頭 = new session
        // 撮影用の供給元は実際に claude を起こさないので、起動直後の状態を作る
        app.input_gate = Some(std::time::Instant::now());
        assert!(
            hold_input_while_starting(&mut app, typed('h')),
            "input typed while a session is starting flows straight to the PTY"
        );
        assert!(hold_input_while_starting(&mut app, || HeldInput::Paste(
            "ls".to_string()
        )));
        assert!(hold_input_while_starting(&mut app, || HeldInput::Key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
        )));
        assert_eq!(
            app.held_input,
            vec![
                HeldInput::Key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)),
                HeldInput::Paste("ls".to_string()),
                HeldInput::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ],
            "the held input is not what was typed, in the order it was typed"
        );
        assert!(app.notice.is_some(), "holding the input was not reported");
        // 子が端末を掴めば素通しに戻る（門番が残り続けるとタイプできない画面になる）
        app.input_gate = None;
        assert!(
            !hold_input_while_starting(&mut app, typed('x')),
            "input is still held after the launch finished"
        );
    }

    /// **溜めるのは有界。** 応答しない子を相手に無制限に溜めると、
    /// 貼り付け 1 回で何 MB でも積める（[`HeldInput::weight`] が文字数で測る理由）。
    ///
    /// **上限を超えたら新しい方から捨てる**（古い方を押し出さない）: 頭を落とすと
    /// 送信の `\r` だけが生き残り、切れたプロンプトがそのまま送信される。
    /// **捨てたことは下部バーで伝える**（黙って消さない）
    #[test]
    fn holding_stops_at_a_bound_and_says_what_it_dropped() {
        let mut app = test_app(34, TERM);
        app.input_gate = Some(std::time::Instant::now());
        assert!(hold_input_while_starting(&mut app, typed('h')));
        // 上限ちょうどまでは受ける（境界の内側）
        let fits = "x".repeat(HELD_INPUT_LIMIT - 1);
        assert!(hold_input_while_starting(&mut app, || HeldInput::Paste(
            fits.clone()
        )));
        assert_eq!(app.held_input.len(), 2, "the paste that fits was refused");

        // これ以上は入らない。**素通しはしない**（門番の理由は消えていない）
        assert!(
            hold_input_while_starting(&mut app, typed('y')),
            "input flows straight to the PTY once the buffer is full"
        );
        assert_eq!(
            app.held_input.first(),
            Some(&HeldInput::Key(KeyEvent::new(
                KeyCode::Char('h'),
                KeyModifiers::NONE
            ))),
            "the newest input pushed out what was typed first"
        );
        assert_eq!(app.held_input.len(), 2, "the input that does not fit was held");
        let (msg, _) = app.notice.as_ref().expect("the drop was not reported");
        assert!(
            msg.contains("dropped"),
            "the notice does not say the input was dropped: {msg:?}"
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
            !hold_input_while_starting(&mut app, typed('a')),
            "input is held even though the launch is done"
        );
    }

    /// **応答しない起動から入力が有界時間で戻る。**
    ///
    /// 門番が降りる合図は「子が最初の出力を出した」なので、子が端末を掴まないまま
    /// （起動直後の認証プロンプト・AV スキャン・長い `-r` の読み込み）だと
    /// **既存の全セッションへのタイプがその間ずっと死ぬ**。
    /// 期限（[`INPUT_GATE_LIMIT`]）を超えたら門番を降ろす。
    ///
    /// **降りた後の打ち先は「フォーカススロットに窓が出ているか」で決まる。**
    /// ここは窓を 1 枚も持たない ＝ 宛先が居ないので、フォーカスがサイドバーへ戻り、
    /// キーはサイドバー操作として処理される（キーが PTY へ行くのは
    /// `focus == Terminal` のときだけ）。打ち先はユーザーが選び直す。
    /// **窓が出ている場合は奪わない**（[`a_slow_launch_keeps_the_focus_on_its_pane`]）
    #[test]
    fn a_hung_launch_gives_input_back_within_a_bounded_time() {
        let mut app = test_app(34, TERM);
        // 子がまだ何も出力していない状態（起動直後）
        app.input_gate = Some(std::time::Instant::now());
        app.set_focus(Focus::Terminal); // dispatch_session と同じ状態

        // 期限内は門番が効いている（有界 ＝ 即座に降りる、ではない）
        assert!(!expire_input_gate(&mut app), "the gate lifted before its deadline");
        assert!(
            hold_input_while_starting(&mut app, typed('a')),
            "input typed while a session is starting flows straight to the PTY"
        );

        // 期限ぶん前に起こした状態（時刻を注入して待たずに検査する）
        app.input_gate = Some(instant_ago(INPUT_GATE_LIMIT));
        assert!(expire_input_gate(&mut app), "the gate does not lift past its deadline");
        assert!(
            !hold_input_while_starting(&mut app, typed('b')),
            "a hung launch kills input forever"
        );
        assert!(
            app.focus == Focus::Sidebar,
            "the gate lifted without moving focus back — typing would flow to the previous session"
        );
        // 宛先が居ないので溜めた入力は捨てられ、**そちらが下部バーに残る**
        // （打った文字が消えたことの方が取り返しがつかない。ハングそのものの
        // 報告は [`set_notice`] ＝ error.log 側に残る）
        let (msg, _) = app.notice.as_ref().expect("the drop was not reported");
        assert!(
            msg.contains("dropped"),
            "the notice does not say the held input was dropped: {msg:?}"
        );
        // 溜めた入力が無ければ、ハングそのものの報告が下部バーに出る
        app.input_gate = Some(instant_ago(INPUT_GATE_LIMIT));
        assert!(expire_input_gate(&mut app), "the gate does not lift past its deadline");
        let (msg, _) = app.notice.as_ref().expect("the hang was not reported");
        assert!(
            msg.contains("not responding"),
            "the notice does not say what happened: {msg:?}"
        );
        // 2 度目は何もしない（毎周通知を出し直さない）
        assert!(!expire_input_gate(&mut app), "an already lifted gate was lifted again");
    }

    /// **遅いだけの起動からフォーカスを奪わない。**
    ///
    /// 期限切れが意味するのは「宛先が消えた」ではなく「まだ端末を掴んでいない」。
    /// 窓はフォーカススロットに出ていて子も生きているので、ここでサイドバーへ戻すと
    /// **見えているペインへ打てなくなる**（↑↓ が一覧の選択を動かすだけで、
    /// ペインが完全に無反応に見える ＝ クリックか `Alt+→` を押すまで戻せない）。
    ///
    /// `-r` の再開は transcript が大きいほど遅く、期限を超えること自体は珍しくない
    /// （実機の `error.log` にはこの経路の記録が数十件溜まっていた）＝
    /// **期限切れを異常として報告しない**のもここで固定する
    #[test]
    fn a_slow_launch_keeps_the_focus_on_its_pane() {
        let mut app = test_app(34, TERM);
        app.set_layout(crate::panes::Layout::One);
        app.slots[0] = Slot::Session(SessionId::new("s"));
        app.set_focus(Focus::Terminal); // dispatch_session と同じ状態
        // 期限ぶん前に起こした状態（時刻を注入して待たずに検査する）
        app.input_gate = Some(instant_ago(INPUT_GATE_LIMIT));

        assert!(expire_input_gate(&mut app), "the gate does not lift past its deadline");
        assert!(
            !hold_input_while_starting(&mut app, typed('a')),
            "a slow launch kills input forever"
        );
        assert!(
            app.focus == Focus::Terminal,
            "the focus left a pane that is still showing its session — typing would go to the sidebar"
        );
        let (msg, _) = app.notice.as_ref().expect("the slow start was not reported");
        assert!(
            !msg.contains("not responding"),
            "a launch that is only slow is reported as a hang: {msg:?}"
        );
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
            !hold_input_while_starting(&mut app, typed('a')),
            "input is still held after the launch failed"
        );
        assert!(
            app.focus == Focus::Sidebar,
            "the gate lifted without moving focus back — typing would flow to the previous session"
        );
    }

    /// **宛先が居ないまま門番を降ろすと打ち先も戻り、溜めた入力は捨てられる。**
    /// 起動には成功したのにその窓が表示されていない（切替と削除が競合した等）
    /// 状態でも、素通しに戻ると打った文字が別のセッションへ流れる。溜めた入力も
    /// 同じで、流し先が無いのだから流してはいけない ＝ **捨てたことを伝える**。
    ///
    /// [`apply_launch`] 越しには書けない: 本物の子プロセス生成が要るため
    /// （portable-pty は存在しない cwd を `USERPROFILE` に差し替えて**成功させる**
    /// ＝ テストが本物の claude を起こしてしまう）。代わりに、その後の状態
    /// （宛先の窓が開いていない）をそのまま作って判断だけを検査する
    #[test]
    fn lifting_the_gate_without_its_destination_drops_the_held_input() {
        let mut app = test_app(34, TERM);
        let id = SessionId::new("abc123");
        app.input_gate = Some(std::time::Instant::now());
        app.set_focus(Focus::Terminal);
        assert!(hold_input_while_starting(&mut app, typed('a')));
        assert!(
            !app.showing(&id),
            "the destination window is open — the premise of this test broke"
        );
        lift_input_gate(&mut app, Some(&id));
        assert!(
            !hold_input_while_starting(&mut app, typed('b')),
            "input is still held after the gate lifted"
        );
        assert!(
            app.focus == Focus::Sidebar,
            "the gate lifted but focus stayed on the terminal"
        );
        assert!(
            app.held_input.is_empty(),
            "the held input survived a lift that had nowhere to deliver it"
        );
        let (msg, _) = app.notice.as_ref().expect("the drop was not reported");
        assert!(
            msg.contains("dropped"),
            "the input was dropped without saying so: {msg:?}"
        );
    }

    /// **溜めている間に打ち先が別のセッションへ移っていたら、そちらへは流さない。**
    /// 起動が終わるまでの間にユーザーはスロットを移せる（`Alt+←→` / クリック）ので、
    /// 「起こしたときの宛先」をそのまま信じると、**別プロジェクトのセッションへ
    /// 打ちかけのプロンプトが送られる**。宛先の確認は [`App::showing`] 1 つに
    /// 寄せてあるので、フォーカスの行き先と溜めた入力の行き先が食い違わない
    #[test]
    fn held_input_never_goes_to_a_session_the_user_moved_to() {
        let mut app = test_app(34, TERM);
        let launched = SessionId::new("launched");
        app.set_layout(crate::panes::Layout::One);
        // 溜めている間にユーザーが別のセッションへ移った状態
        app.slots[0] = Slot::Session(SessionId::new("another"));
        app.set_focus(Focus::Terminal);
        app.input_gate = Some(std::time::Instant::now());
        assert!(hold_input_while_starting(&mut app, typed('a')));

        lift_input_gate(&mut app, Some(&launched));
        assert!(
            app.held_input.is_empty(),
            "the held input is still queued — it would land on whatever is shown next"
        );
        let (msg, _) = app.notice.as_ref().expect("the drop was not reported");
        assert!(
            msg.contains("dropped"),
            "the input was dropped without saying so: {msg:?}"
        );
    }

    /// **子が端末を掴んだら、溜めた入力はその窓へ実際に流れる。**
    ///
    /// 門番を置いた理由（掴む前の打鍵は読み捨てられる）は掴んだ時点で消えるので、
    /// そこからは打った順に届かなければならない。ここは PTY への書き込みまで
    /// 通す（[`Session::fake`] は本物の PTY 対を開く）＝ 「捨てずに溜めた」だけで
    /// 終わっていないことを固定する。バイト列の並びと形は
    /// [`crate::keys::encode_held`] のテストが受け持つ
    #[test]
    fn held_input_reaches_the_destination_once_the_child_takes_the_terminal() {
        let mut app = test_app(34, TERM);
        let id = SessionId::new("launched");
        // slave は窓と同じ寿命で持つ（落とすと PTY が閉じて書き込みが失敗する）
        let (window, _slave) = Session::fake(&id, 24, 80);
        window.pretend_started();
        app.windows.push(window);
        app.set_layout(crate::panes::Layout::One);
        app.slots[0] = Slot::Session(id.clone());
        app.set_focus(Focus::Terminal);
        app.input_gate = Some(std::time::Instant::now());
        assert!(hold_input_while_starting(&mut app, typed('h')));
        assert!(hold_input_while_starting(&mut app, || HeldInput::Paste(
            "ls".to_string()
        )));

        lift_input_gate(&mut app, Some(&id));
        assert!(
            app.held_input.is_empty(),
            "the held input never left the buffer"
        );
        assert!(
            app.notice.is_none() || !app.notice.as_ref().unwrap().0.contains("dropped"),
            "the input was dropped even though its destination was on screen: {:?}",
            app.notice
        );
        // 書き込みに失敗していれば窓は閉じられている（[`send_to_active`]）＝
        // 窓が残っていることが「宛先へ届いた」の証拠
        assert_eq!(
            app.windows.len(),
            1,
            "writing the held input to its session failed"
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
            drawn.contains("new claude session"),
            "the label is chopped by the right pane: {drawn:?}"
        );
        // そのはみ出した列のクリックが、描かれている項目どおりに効く
        // （`new <agent> session` は掴める項目なので、走るのは離したとき）
        let col = rect.right() - 2;
        assert!(col >= MIN_SIDEBAR, "this column is not past the sidebar: {col}");
        handle_mouse(&mut app, &click(col, rect.y + 1)).unwrap();
        handle_mouse(&mut app, &release(col, rect.y + 1)).unwrap();
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
                "s", crate::poll::State::Waiting, 9_999,
            )]))),
            ..Default::default()
        };
        adopt_hook_states(&mut app);
        assert_eq!(app.sessions, rows, "a leftover hook wrote to the row");
        // 写しそのものは取り直されている（表示はここから導く）
        assert_eq!(
            app.hook_states.get(&SessionId::new("s"), Some(0)).map(|hook| hook.state),
            Some(crate::poll::State::Waiting)
        );
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
        let unread = |at| HookStates::from_entries([("s", crate::poll::State::Idle, at)]).unread(&row);
        assert!(!HookStates::default().unread(&row), "a row with no hook record is unread");
        assert!(!unread(1_000), "a hook from before the row was opened marks it unread");
        assert!(unread(1_001), "claude spoke after the row was opened but it stayed read");

        // 自分の操作（ピン留め）は行を書き換えるが未読の材料を動かさない
        let hooks = HookStates::from_entries([("s", crate::poll::State::Idle, 999)]);
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

    /// **行を開いた時点で既読になる**（開き方は問わない ＝ [`mark_read`] の
    /// 1 箇所で済ませてある。`mark as read` と、ペインに出ている行へ hook が届いた
    /// ときも同じ関数）。消えた行を指しても何も起きない
    #[test]
    fn opening_a_session_marks_the_row_read() {
        let rows = [session_row("s", "C:\\dev\\api", 1)];
        let mut app = app_with_hooks(&rows, HookStates::from_entries([("s", crate::poll::State::Idle, 2)]));
        assert!(app.hook_states.unread(row_of(&app, "s")), "the premise (an unread row) broke");

        mark_read(&mut app, &SessionId::new("s"));
        let row = row_of(&app, "s");
        assert!(!app.hook_states.unread(row), "still unread after being opened");
        // 既読は `updated_at` を進めない ＝ 行の内容の後勝ち判定には乗らない
        // （乗せると、既読にしただけで他インスタンスの本当の変更を踏み潰しうる。
        // `crate::sessions` の `merge_sessions` が `last_opened_at` を別軸で合流させる）
        assert_eq!(row.updated_at, 1, "marking as read advanced updated_at");

        mark_read(&mut app, &SessionId::new("gone-row"));
        assert_eq!(app.sessions.len(), 1, "an unknown row changed the list");
    }

    /// **スロットのフォーカスを移した先も既読になる。**
    ///
    /// スロットが複数あるとき、未読の行は既に画面へ出ているので
    /// [`open_session`] を通らない ＝ 見に行く操作はフォーカスの移動
    /// （クリック / `Alt+Shift+方向`）しかない。ここが既読の契機でなかった頃は、
    /// **サイドバーの行を押し直すまで `●` が消えなかった**
    #[test]
    fn focusing_a_slot_marks_the_session_it_shows_read() {
        let rows = [session_row("s", "C:\\dev\\api", 1)];
        let mut app = app_with_hooks(&rows, HookStates::from_entries([("s", crate::poll::State::Idle, 2)]));
        app.term_size = TERM;
        app.set_layout(crate::panes::Layout::TwoColumns);
        app.slots[1] = Slot::Session(SessionId::new("s"));
        assert!(app.hook_states.unread(row_of(&app, "s")), "the premise (an unread row) broke");

        app.set_focus_slot(1);
        assert!(
            !app.hook_states.unread(row_of(&app, "s")),
            "the row stayed unread after its slot took the focus"
        );
    }

    /// **枚数を減らして残った行も既読になる。**
    ///
    /// 4 分割の左上に未読の行、フォーカスは別のスロット。`1 pane` へ戻すと
    /// フォーカスが 0 へ丸められ、**残った 1 枚に出るのはその未読の行**
    /// （見に行く操作はしたのに [`App::set_focus_slot`] は通らない）。
    /// ここが抜けていた頃は `●` が消えなかった
    #[test]
    fn shrinking_the_layout_marks_the_row_that_survives_read() {
        let rows = [session_row("s", "C:\\dev\\api", 1)];
        let mut app = app_with_hooks(&rows, HookStates::from_entries([("s", crate::poll::State::Idle, 2)]));
        app.term_size = TERM;
        app.set_layout(crate::panes::Layout::Four);
        app.slots[0] = Slot::Session(SessionId::new("s"));
        app.focus_slot = 2;
        assert!(app.hook_states.unread(row_of(&app, "s")), "the premise (an unread row) broke");

        app.set_layout(crate::panes::Layout::One);
        assert_eq!(app.focus_slot, 0, "the premise (the focus was rounded down) broke");
        assert!(
            !app.hook_states.unread(row_of(&app, "s")),
            "the row that survived the shrink stayed unread"
        );
    }

    /// **溢れて消えた行は未読のまま。** 画面から消えるだけで、見に行ってはいない
    #[test]
    fn shrinking_the_layout_leaves_the_rows_it_drops_unread() {
        let rows = [session_row("s", "C:\\dev\\api", 1)];
        let mut app = app_with_hooks(&rows, HookStates::from_entries([("s", crate::poll::State::Idle, 2)]));
        app.term_size = TERM;
        app.set_layout(crate::panes::Layout::Four);
        app.slots[3] = Slot::Session(SessionId::new("s"));

        app.set_layout(crate::panes::Layout::One);
        assert!(
            app.hook_states.unread(row_of(&app, "s")),
            "a row that fell off the layout was marked read without being looked at"
        );
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
        app.hook_states = HookStates::from_entries([("s", crate::poll::State::Idle, 2_000)]);
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
    /// 進めてしまうと、他インスタンスが先に書いた本当の変更（ピン留め等）より
    /// こちらの写しの `updated_at` だけが新しくなり、マージの後勝ち判定
    /// （[`crate::sessions`] の `merge_sessions`）でこちらの古い中身が勝って
    /// 相手の変更を踏み潰す。`mark as read` は行の内容を 1 つも変えない
    /// （既読は `last_opened_at` を通じて別軸で合流する。[`mark_read`]）ので
    /// 何度押しても進めず、ピン留めは中身を変えるので進む
    #[test]
    fn an_edit_that_changes_nothing_leaves_updated_at_alone() {
        let mut app = app_with_row("s");
        let id = SessionId::new("s");
        app.sessions[0].last_opened_at = 1_000;
        app.sessions[0].updated_at = 2_000;
        app.hook_states = HookStates::from_entries([("s", crate::poll::State::Idle, 2_000)]);

        // 未読の行への `mark as read`: 既読にはなるが行の内容は変わっていない
        run_popup_action(&mut app, PopupAction::MarkRead(id.clone()));
        assert_eq!(only_row(&app).updated_at, 2_000, "mark as read advanced updated_at");
        assert!(
            !app.hook_states.unread(only_row(&app)),
            "mark as read did not clear unread"
        );

        // もう一度押しても何も動かない
        run_popup_action(&mut app, PopupAction::MarkRead(id.clone()));
        assert_eq!(only_row(&app).updated_at, 2_000, "a second mark as read advanced updated_at");

        // 中身が変わる操作は進める（マージの後勝ち判定の材料なので必ず進む）
        run_popup_action(&mut app, PopupAction::TogglePin(id));
        assert!(only_row(&app).updated_at > 2_000, "a real change did not advance updated_at");
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
            (2, PopupKind::State),
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
        app.sessions = vec![SessionRow {
            conversation: Conversation::Observed("conv-1".to_string()),
            ..session_row("s", "C:\\ccdesk-test-no-such-folder", 1)
        }];
        // claude が行を開いた後に何か言った ＝ 未読
        app.hook_states = HookStates::from_entries([("s", crate::poll::State::Idle, 2)]);
        // 別インスタンスで稼働中 ＝ open_session のガードが決定的に止める
        app.agents = vec![AgentInfo {
            session_id: "conv-1".to_string(),
            kind: "interactive".to_string(),
            status: "busy".to_string(),
            ..AgentInfo::default()
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
            .entries(app.grouping, &app.kinds)
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
        let positions = (0..app.sidebar_rows.len()).map(SidebarPos::Row);
        for pos in positions {
            for code in [KeyCode::Left, KeyCode::Right] {
                app.selection = pos;
                app.popup = None;
                press(&mut app, code);
                assert!(app.popup.is_none(), "{pos:?}: {code:?} opened a menu");
                assert_eq!(app.selection, pos, "{pos:?}: {code:?} moved the selection");
                assert!(
                    !app.focus_is_new(),
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

    /// **予約はこの 7 つだけ。** ここに増やすと、その打鍵ぶんだけ claude 側の
    /// キーバインドが死ぬ（二次操作はポップアップに集約した）
    #[test]
    fn only_quit_focus_and_slot_moves_are_reserved() {
        use crate::panes::Dir;
        let alt = |code| KeyEvent::new(code, KeyModifiers::ALT);
        let alt_shift = |code| {
            KeyEvent::new(code, KeyModifiers::ALT | KeyModifiers::SHIFT)
        };
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
        for (code, dir) in [
            (KeyCode::Left, Dir::Left),
            (KeyCode::Right, Dir::Right),
            (KeyCode::Up, Dir::Up),
            (KeyCode::Down, Dir::Down),
        ] {
            assert_eq!(
                reserved_key(&alt_shift(code)),
                Some(Reserved::Slot(dir)),
                "Alt+Shift+{code:?} does not move between slots"
            );
        }
        // 修飾が違えば claude のもの（素の ←→↑↓ / 素の q / Ctrl+←→ / Shift だけ）
        for key in [
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        ] {
            assert_eq!(reserved_key(&key), None, "{key:?} is reserved");
        }
    }

    /// **▦ layout のメニューは配置の一覧そのもの。**
    /// 現在値に `●` が付き、**その端末に入らない配置は押せない**（選んだ瞬間に
    /// 崩れる項目を出さない）
    #[test]
    fn the_layout_menu_marks_the_current_one_and_disables_what_does_not_fit() {
        use crate::panes::{Layout, Split};
        let split = Split::default();
        let roomy = Rect::new(0, 0, 80, 40);
        let fits: Vec<Layout> = Layout::ORDER
            .into_iter()
            .filter(|l| l.fits(roomy, split))
            .collect();
        let kind = PopupKind::Layout {
            current: Layout::TwoColumns,
            fits,
        };
        let pairs = entry_pairs(&kind, Grouping::State);
        assert_eq!(pairs.len(), Layout::ORDER.len(), "the menu is not the full list");
        assert!(
            pairs.iter().all(|(_, enabled)| *enabled),
            "a layout is unusable on an 80x40 terminal: {pairs:?}"
        );
        let marked: Vec<&String> = pairs
            .iter()
            .filter(|(label, _)| label.starts_with("● "))
            .map(|(label, _)| label)
            .collect();
        assert_eq!(marked.len(), 1, "the current layout is not marked exactly once");
        assert!(marked[0].ends_with(Layout::TwoColumns.as_str()));

        // 1 枚ぶんしか無い端末では、分割した配置が全部落ちる
        let tiny = Rect::new(0, 0, Layout::MIN_SLOT.1, Layout::MIN_SLOT.0);
        let kind = PopupKind::Layout {
            current: Layout::One,
            fits: Layout::ORDER
                .into_iter()
                .filter(|l| l.fits(tiny, split))
                .collect(),
        };
        let usable: Vec<String> = entry_pairs(&kind, Grouping::State)
            .into_iter()
            .filter(|(_, enabled)| *enabled)
            .map(|(label, _)| label)
            .collect();
        assert_eq!(usable.len(), 1, "a split layout is offered on a one-slot terminal");
    }

    /// **見えているセッションを選ぶとフォーカスがそこへ動く。** 中身は動かさない ＝
    /// 既に別のスロットに出ている行を選んでも、両方のスロットの中身はそのまま、
    /// フォーカスだけがそのセッションの居るスロットへ移る
    #[test]
    fn choosing_a_visible_session_moves_focus_there_instead() {
        let mut app = app_with_row("a");
        app.sessions.push(session_row("b", "C:\\dev\\api", 1));
        app.set_layout(crate::panes::Layout::TwoColumns);
        app.slots[0] = Slot::Session(SessionId::new("a"));
        app.slots[1] = Slot::Session(SessionId::new("b"));
        // 右（b が居る）にフォーカスして a を選ぶ
        app.focus_slot = 1;
        app.show_session(&SessionId::new("a"));
        assert_eq!(app.focus_slot, 0, "focus did not move to the slot showing a");
        assert_eq!(
            app.slots[0].session(),
            Some(&SessionId::new("a")),
            "a moved even though it was already visible"
        );
        assert_eq!(
            app.slots[1].session(),
            Some(&SessionId::new("b")),
            "b was pushed out of its slot"
        );
    }

    /// **押したスロットが宛先になる**（キーボードの `Alt+Shift+方向` と同じ結果）。
    /// 裏のスロットを押しても、そのスロットの中身へはイベントを渡さない
    #[test]
    fn clicking_a_slot_makes_it_the_target() {
        let mut app = test_app(34, TERM);
        app.set_layout(crate::panes::Layout::Four);
        let rects = app.slot_rects();
        for (at, rect) in rects.into_iter().enumerate() {
            let click = MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: rect.x + rect.width / 2,
                row: rect.y + rect.height / 2,
                modifiers: KeyModifiers::NONE,
            };
            handle_mouse(&mut app, &click).unwrap();
            assert_eq!(app.focus_slot, at, "clicking slot {at} did not target it");
            assert_eq!(app.focus, Focus::Terminal);
        }
    }

    /// **十字の掴み代はスロットのクリックより先に効く。**
    /// 境界はスロットの枠線に重なっているので、順序が逆だとドラッグが
    /// フォーカス移動に化けて掴めなくなる
    #[test]
    fn the_cross_can_be_grabbed_where_the_slots_meet() {
        let mut app = test_app(34, TERM);
        app.set_layout(crate::panes::Layout::Four);
        let area = crate::ui::pane_rect(&app);
        let (vx, hy) = app.layout.cross(area, app.split);
        let at = |column, row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut app, &at(vx.unwrap(), hy.unwrap())).unwrap();
        assert_eq!(
            app.cross_drag,
            Some((true, true)),
            "grabbing the intersection did not take both axes"
        );
        // 動かすと比率が変わり、離すと掴みが外れる
        let before = app.split;
        let drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: area.x + area.width / 4,
            row: area.y + area.height / 4,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut app, &drag).unwrap();
        assert_ne!(app.split, before, "dragging the cross did not move it");
        let up = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: drag.column,
            row: drag.row,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut app, &up).unwrap();
        assert_eq!(app.cross_drag, None, "the cross stayed grabbed after release");
    }

    // ── サイドバーの行を掴んでペインへ落とす ──────────────────────────

    /// ボタンを押したまま動かす
    fn drag_to(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// ボタンを離す
    fn release(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// 掴める行 1 本を持つサイドバー。**別インスタンスで動いている扱い**にして
    /// あるので、開く経路へ落ちても実際の起動は踏まない（[`open_session`] の二重
    /// 再開ガードで止まる）
    fn app_with_grabbable_row() -> App {
        let mut app = app_with_row("s");
        app.sessions[0].conversation = Conversation::Observed("conv-1".to_string());
        app.agents = vec![AgentInfo {
            session_id: "conv-1".to_string(),
            kind: "interactive".to_string(),
            status: "busy".to_string(),
            ..AgentInfo::default()
        }];
        app
    }

    /// **押しただけでは開かない。離して初めて開く。**
    ///
    /// 掴める行になった以上、押した時点で開くと「運ぶ前に一度開いてから移動する」
    /// 二段動作になる。動かさずに離す ＝ ただのクリックは今までどおり通る
    #[test]
    fn pressing_a_session_row_waits_for_the_release_before_opening_it() {
        let mut app = app_with_grabbable_row();
        handle_mouse(&mut app, &click(5, 1)).unwrap();
        assert!(app.drag.is_some(), "pressing the row did not arm a grab");
        assert!(app.notice.is_none(), "the row was opened on press instead of release");
        handle_mouse(&mut app, &release(5, 1)).unwrap();
        assert!(app.drag.is_none(), "the grab outlived the release");
        assert!(app.notice.is_some(), "releasing without moving did not open the row");
    }

    /// **サイドバーの中で少し動いてから離しても、クリックとして通る。**
    ///
    /// 動いたかどうかで分けると、トラックパッドで 1 桁ずれただけの操作が
    /// 「押したのに何も起きない」に化ける。運んでいない ＝ ペインの外で離した
    /// なら、行き先が無いのだからクリックとして扱うのが素直
    #[test]
    fn a_grab_that_never_left_the_sidebar_still_counts_as_a_click() {
        let mut app = app_with_grabbable_row();
        handle_mouse(&mut app, &click(5, 1)).unwrap();
        handle_mouse(&mut app, &drag_to(6, 1)).unwrap();
        assert!(app.drag.as_ref().is_some_and(|d| d.moved), "the fixture's premise broke");
        assert_eq!(
            app.drag.as_ref().and_then(|d| d.target),
            None,
            "the sidebar is not a drop target"
        );
        handle_mouse(&mut app, &release(6, 1)).unwrap();
        assert!(app.notice.is_some(), "a jittered click did nothing at all");
    }

    /// **スロットの真ん中を指すと、そのスロットがそのまま落とし先。**
    /// 配置は育たない
    #[test]
    fn the_middle_of_a_slot_reads_as_a_plain_drop() {
        let mut app = app_with_grabbable_row();
        app.set_layout(crate::panes::Layout::TwoColumns);
        let right = app.slot_rects()[1];
        let (x, y) = (right.x + right.width / 2, right.y + right.height / 2);
        handle_mouse(&mut app, &click(5, 1)).unwrap();
        handle_mouse(&mut app, &drag_to(x, y)).unwrap();
        assert_eq!(
            app.drag.as_ref().and_then(|d| d.target),
            Some(DropAt::Slot(1)),
            "the middle of a slot is not a plain drop"
        );
        assert_eq!(make_room(&mut app, DropAt::Slot(1)), Some(1));
        assert_eq!(app.layout, crate::panes::Layout::TwoColumns, "the layout grew");
    }

    /// **縁へ落とすと配置が育ち、落とした側が空く。元居た中身は反対側へ残る**
    /// （触っていないセッションは画面から消えない）
    #[test]
    fn dropping_on_an_edge_grows_the_layout_and_keeps_the_old_content() {
        use crate::panes::{Axis, Layout};
        let mut app = app_with_grabbable_row();
        app.sessions.push(session_row("old", "C:\\dev\\api", 2));
        app.slots[0] = Slot::Session(SessionId::new("old"));
        assert_eq!(app.layout, Layout::One, "the fixture's premise broke");
        let only = app.slot_rects()[0];
        let (x, y) = (only.x + only.width - 1, only.y + only.height / 2);
        handle_mouse(&mut app, &click(5, 1)).unwrap();
        handle_mouse(&mut app, &drag_to(x, y)).unwrap();
        let target = DropAt::Split {
            at: 0,
            axis: Axis::Vertical,
            half: 1,
        };
        assert_eq!(
            app.drag.as_ref().and_then(|d| d.target),
            Some(target),
            "the right edge did not read as a split to the right"
        );
        assert_eq!(make_room(&mut app, target), Some(1), "the freed slot is not the right half");
        assert_eq!(app.layout, Layout::TwoColumns, "the layout did not grow");
        assert!(matches!(app.slots[1], Slot::Empty), "the drop target was not left empty");
        assert_eq!(
            app.slots[0].session(),
            Some(&SessionId::new("old")),
            "the slot's old content was pushed off the screen"
        );
    }

    /// **`+ new session` を落とすと、そのスロットへ新規セッション画面が出る。**
    /// 即起動にしないのは、この入口が agent を選ばせてから起こす契約だから
    /// （落とす操作にその選択を載せる場所が無い）
    #[test]
    fn dropping_the_new_session_row_opens_the_form_in_that_slot() {
        let mut app = test_app(34, TERM);
        app.sidebar_rows = vec![SidebarRow::Action(RowAction::New)];
        app.sidebar_header_rows = 1;
        app.dispatch_cwd = std::env::temp_dir().to_string_lossy().to_string();
        app.set_layout(crate::panes::Layout::TwoColumns);
        let right = app.slot_rects()[1];
        let (x, y) = (right.x + right.width / 2, right.y + right.height / 2);
        handle_mouse(&mut app, &click(5, 1)).unwrap();
        handle_mouse(&mut app, &drag_to(x, y)).unwrap();
        handle_mouse(&mut app, &release(x, y)).unwrap();
        assert!(matches!(app.slots[1], Slot::New(_)), "the form did not land in the slot");
        assert!(matches!(app.slots[0], Slot::Empty), "the other slot was disturbed");
        assert_eq!(app.focus_slot, 1, "the focus did not follow the drop");
        assert_eq!(app.focus, Focus::Terminal, "keys still go to the sidebar");
        assert!(app.sessions.is_empty(), "the drop started a session without asking");
    }

    /// **メニューを開いた行を、そのまま掴み直せる。**
    ///
    /// メニューを閉じるためだけに 1 回押させると、**メニューを持つ行にだけ
    /// ドラッグの入口が無くなる**: 見出し行はクリックでメニューが開くので、
    /// 次に掴もうとした押しがメニューを閉じるのに使われて掴みが始まらない
    /// （プロジェクト見出しが運べなかったのはこれ）
    #[test]
    fn a_row_whose_menu_is_open_can_still_be_grabbed() {
        let mut app = app_with_grabbable_row();
        // セッション行のメニューは行末の `=` で開く
        let mark_x = *menu_zone(sidebar_cols(&app)).end();
        handle_mouse(&mut app, &click(mark_x, 1)).unwrap();
        assert!(app.popup.is_some(), "the fixture's premise broke");
        // そのまま押し直せば掴める（閉じるだけで終わらない）
        handle_mouse(&mut app, &click(5, 1)).unwrap();
        assert!(app.popup.is_none(), "the menu stayed open");
        assert!(
            app.drag.is_some(),
            "the row could not be grabbed while its own menu was open"
        );
    }

    /// **メニューの `new <agent> session` を掴んでペインへ落とせる。**
    ///
    /// この項目は agent を名指ししている ＝ 落とす操作に選ばせるものが残って
    /// いないので、落とした先でそのまま起こす（見出しを掴んだときは選ばせる
    /// 画面が出る、との違いはそこ）。
    ///
    /// 掴んだ時点でメニューは閉じる（開いたままだと Drag がモーダルに飲まれて
    /// ドロップ先を選べず、ペインを覆ってガイドも見えない）
    #[test]
    fn a_new_session_menu_item_can_be_dropped_on_a_pane() {
        use crate::panes::Layout;
        let mut app = test_app(34, TERM);
        app.set_layout(Layout::TwoColumns);
        open(&mut app, project("C:\\dev\\api", false), 1);
        let rect = popup_rect(&app, app.popup.as_ref().unwrap());
        // 先頭項目 = new claude session を押す
        handle_mouse(&mut app, &click(rect.x + 1, rect.y + 1)).unwrap();
        assert!(app.popup.is_none(), "the menu stayed open over the panes");
        assert!(app.drag.is_some(), "the menu item was not grabbed");
        assert_eq!(app.dispatch_cwd, "", "the item ran on press instead of release");
        // 右のスロットの真ん中へ運ぶ
        let right = app.slot_rects()[1];
        let (x, y) = (right.x + right.width / 2, right.y + right.height / 2);
        handle_mouse(&mut app, &drag_to(x, y)).unwrap();
        assert_eq!(
            app.drag.as_ref().and_then(|d| d.target),
            Some(DropAt::Slot(1)),
            "the menu item found no drop target"
        );
        handle_mouse(&mut app, &release(x, y)).unwrap();
        // 起動そのものは撮影用の供給元が止めるが、**そのフォルダで起こしに
        // いった**ことは観測できる（落とし先の反映は起動できたときだけ ＝ 下のテスト）
        assert_eq!(app.dispatch_cwd, "C:\\dev\\api", "the item did not run on release");
        assert_eq!(app.layout, Layout::TwoColumns, "the layout grew for a plain drop");
    }

    /// **起こせなかったら配置を育てない。** 育ててから起こすと、起動に失敗した
    /// とき空のペインだけが増えて残る（[`drop_session`] が守っている順序と同じ）。
    ///
    /// 撮影用の供給元はセッションを起こさないので、そのまま「起こせなかった」
    /// 側の経路になる
    #[test]
    fn a_new_session_that_never_starts_does_not_grow_the_layout() {
        use crate::panes::{Axis, Layout};
        let mut app = test_app(34, TERM);
        open(&mut app, project("C:\\dev\\api", false), 1);
        let rect = popup_rect(&app, app.popup.as_ref().unwrap());
        handle_mouse(&mut app, &click(rect.x + 1, rect.y + 1)).unwrap();
        let only = app.slot_rects()[0];
        let (x, y) = (only.x + only.width - 1, only.y + only.height / 2);
        handle_mouse(&mut app, &drag_to(x, y)).unwrap();
        // 縁を指しているので、起こせていれば育つはずの落とし先が出ている
        assert_eq!(
            app.drag.as_ref().and_then(|d| d.target),
            Some(DropAt::Split {
                at: 0,
                axis: Axis::Vertical,
                half: 1,
            }),
            "the edge did not read as a split"
        );
        handle_mouse(&mut app, &release(x, y)).unwrap();
        assert_eq!(app.layout, Layout::One, "the layout grew for a session that never started");
        assert_eq!(app.slots.len(), 1, "a stray empty pane was left behind");
    }

    /// **メニューを閉じた押しは、運ばずに離しても行の動作まで走らせない。**
    /// 閉じるのがその押しの仕事なので、同じ押しで開くところまでやると
    /// 1 回の押しが 2 つのことをする
    #[test]
    fn the_press_that_closes_a_menu_does_nothing_else() {
        let mut app = app_with_grabbable_row();
        let mark_x = *menu_zone(sidebar_cols(&app)).end();
        handle_mouse(&mut app, &click(mark_x, 1)).unwrap();
        assert!(app.popup.is_some(), "the fixture's premise broke");
        handle_mouse(&mut app, &click(5, 1)).unwrap();
        handle_mouse(&mut app, &release(5, 1)).unwrap();
        assert!(app.popup.is_none(), "the menu reopened on the press that closed it");
        assert!(app.drag.is_none(), "the grab outlived the release");
        assert!(app.notice.is_none(), "the press that closed the menu also opened the row");
    }

    /// **メニューを閉じた押しでも、幅変更の掴み代は掴みにしない。**
    ///
    /// 掴みが成立すると幅変更の判定はもう見られない ＝ サイドバーを広げる
    /// つもりの操作が、そのままセッションをペインへ落とす動きに化ける
    #[test]
    fn closing_a_menu_on_the_resize_grip_does_not_grab_the_row() {
        let mut app = app_with_row("s");
        open(&mut app, PopupKind::State, 8); // 掴み代から離れた位置に出す
        let grip = sidebar_cols(&app) - 1;
        let rect = popup_rect(&app, app.popup.as_ref().unwrap());
        assert!(
            !rect.contains(Position::new(grip, 1)),
            "the menu covers the grip, so this is not an outside click: {rect:?}"
        );
        handle_mouse(&mut app, &click(grip, 1)).unwrap();
        assert!(app.popup.is_none(), "the outside click did not close the menu");
        assert!(app.drag.is_none(), "the resize grip grabbed the row behind it");
    }

    /// **メニューの項目を押した分は下へ通さない。** 通すと、項目を実行した
    /// その押しが背後の行のクリックとしても効いてしまう
    #[test]
    fn a_click_inside_the_menu_does_not_reach_the_row_behind_it() {
        let mut app = test_app(34, TERM);
        app.sidebar_rows = vec![SidebarRow::Action(RowAction::Project("C:\\dev\\api".to_string()))];
        app.sidebar_header_rows = 1;
        open(&mut app, PopupKind::State, 1);
        let rect = popup_rect(&app, app.popup.as_ref().unwrap());
        handle_mouse(&mut app, &click(rect.x + 1, rect.y + 2)).unwrap(); // 2 行目 = directory
        assert_eq!(app.grouping, Grouping::Directory, "the item did not run");
        assert!(app.drag.is_none(), "the item's click also grabbed the row behind the menu");
    }

    /// **掴めるのはペインへ何かを出すものだけ。** その場で走るもの（版行の更新・
    /// グルーピング切替・ピン留め・stop / close）は運ぶ先が無いので、押した時点で走る
    #[test]
    fn only_the_things_that_put_something_in_a_pane_can_be_grabbed() {
        let cwd = || "C:\\dev\\api".to_string();
        for what in [
            Grab::Row(RowAction::Open(SessionId::new("s"))),
            Grab::Row(RowAction::New),
            Grab::Menu(PopupAction::OpenSession(SessionId::new("s"))),
            Grab::Menu(PopupAction::NewSessionIn(Kind::Claude, cwd())),
        ] {
            assert!(what.grabbable(), "{what:?} must be grabbable");
        }
        for what in [
            // 見出しの動作は**メニューを開くこと**なので運ぶ先を持たない
            // （運べるのはそのメニューの中の `new <agent> session`）
            Grab::Row(RowAction::Project(cwd())),
            Grab::Row(RowAction::ToggleGroup),
            Grab::Row(RowAction::ChooseLayout),
            Grab::Row(RowAction::UpdateCcdesk),
            Grab::Row(RowAction::UpdateAgent(Kind::Claude)),
            Grab::Menu(PopupAction::TogglePin(SessionId::new("s"))),
            Grab::Menu(PopupAction::Stop(SessionId::new("s"))),
            Grab::Menu(PopupAction::Close(SessionId::new("s"))),
            Grab::Menu(PopupAction::RemoveProject(cwd())),
        ] {
            assert!(!what.grabbable(), "{what:?} must not be grabbable");
        }
        // 掴めない行は押した時点で走る（離すまで待たない）
        let mut app = test_app(34, TERM);
        app.sidebar_rows = vec![SidebarRow::Action(RowAction::ToggleGroup)];
        app.sidebar_header_rows = 1;
        handle_mouse(&mut app, &click(5, 1)).unwrap();
        assert!(app.drag.is_none(), "an unmovable row was grabbed");
        assert!(app.popup.is_some(), "the row did not run on press");
    }

    /// **割って増えた配置は書き残される。** スロットだけ保存すると、次の起動で
    /// `main` が古い配置の枚数しかスロットを復元せず、増えたペインが黙って消える
    #[test]
    fn growing_the_layout_by_a_drop_is_written_down() {
        use crate::panes::{Axis, Layout};
        let source = TestSource::plain();
        let saved = source.saved_window.clone();
        let mut app = App {
            source: Arc::new(source),
            ..test_app(34, TERM)
        };
        make_room(
            &mut app,
            DropAt::Split {
                at: 0,
                axis: Axis::Vertical,
                half: 1,
            },
        )
        .expect("the split had nowhere to go");
        assert_eq!(app.layout, Layout::TwoColumns, "the fixture's premise broke");
        let (layout, slots) = *saved.lock_recover();
        assert_eq!(layout, Some(Layout::TwoColumns), "the grown layout was not written down");
        // **枚数が配置と食い違わない。** 食い違うと次の起動で溢れた側が黙って消える
        assert_eq!(
            slots,
            Some(Layout::TwoColumns.slots()),
            "the saved slots do not match the saved layout"
        );
    }

    /// **窓を用意できなければ配置を触らない。** 逆順（先に育ててから起こす）だと、
    /// 起こし直しに失敗したとき空のペインだけが増えて残る
    #[test]
    fn a_drop_that_cannot_open_the_session_leaves_the_layout_alone() {
        use crate::panes::{Axis, Layout};
        // 別インスタンスで動いている ＝ 二重再開ガードで開けない行
        let mut app = app_with_grabbable_row();
        let target = DropAt::Split {
            at: 0,
            axis: Axis::Vertical,
            half: 1,
        };
        drop_session(&mut app, &SessionId::new("s"), target);
        assert_eq!(app.layout, Layout::One, "the layout grew for a session that never opened");
        assert!(
            app.slots.iter().all(|s| s.session() != Some(&SessionId::new("s"))),
            "the session was placed even though it could not be opened"
        );
        assert!(app.notice.is_some(), "the user was not told why nothing happened");
        assert_ne!(app.focus, Focus::Terminal, "the focus moved to a pane that opened nothing");
    }

    /// **これ以上割れない縁は落とし先にならない。** 4 分割のスロットはどれも
    /// 1 セルなので育てられない ＝ ガイドが塗らないことで「ここは無理」を示す
    #[test]
    fn an_edge_that_cannot_grow_is_not_a_drop_target() {
        let mut app = app_with_grabbable_row();
        app.set_layout(crate::panes::Layout::Four);
        let first = app.slot_rects()[0];
        let (x, y) = (first.x + first.width - 1, first.y + first.height / 2);
        handle_mouse(&mut app, &click(5, 1)).unwrap();
        handle_mouse(&mut app, &drag_to(x, y)).unwrap();
        assert_eq!(
            app.drag.as_ref().and_then(|d| d.target),
            None,
            "an unsplittable edge offered a drop"
        );
        let before = app.layout;
        handle_mouse(&mut app, &release(x, y)).unwrap();
        assert_eq!(app.layout, before, "releasing on a dead edge changed the layout");
        assert!(
            app.slots.iter().all(|s| s.session() != Some(&SessionId::new("s"))),
            "the session was placed even though there was no target"
        );
    }

    /// **ガイドが塗る矩形と、実際に入る矩形が同じ。** 2 つが別の導出を持つと
    /// 「塗られた場所と違うところに入る」が起きるので、答えが 1 つであることを固定する
    #[test]
    fn the_guide_paints_exactly_where_the_session_lands() {
        use crate::panes::Axis;
        let mut app = app_with_grabbable_row();
        // 十字を寄せてある状態で確かめる（半分に割った近似だとここでずれる）
        app.split = crate::panes::Split { v: 30, h: 50 };
        let target = DropAt::Split {
            at: 0,
            axis: Axis::Vertical,
            half: 1,
        };
        let painted = drop_rect(&app, target).expect("the guide had nothing to paint");
        let freed = make_room(&mut app, target).expect("the drop had nowhere to go");
        assert_eq!(painted, app.slot_rects()[freed], "the guide and the drop disagree");
    }

    /// **ボタンを離したイベントを取り逃がしても掴んだままにならない。**
    /// 押していない移動（[`MouseEventKind::Moved`]）が届いた時点で指は離れている
    #[test]
    fn a_grab_does_not_survive_a_lost_release() {
        let mut app = app_with_grabbable_row();
        handle_mouse(&mut app, &click(5, 1)).unwrap();
        assert!(app.drag.is_some());
        handle_mouse(&mut app, &moved(5, 1)).unwrap();
        assert!(app.drag.is_none(), "the grab outlived the button");
    }

    /// **枠右上の ✕ は表示枠だけを閉じる。** 配置が 1 枚減り、残ったセッションは
    /// 番号の振り直しに沿って画面に残り、PTY（[`App::windows`]）も生きたまま
    /// （＝ 押して終わるものは何も無い）
    #[test]
    fn the_close_mark_drops_the_slot_from_the_layout_without_ending_anything() {
        let mut app = app_with_row("s");
        app.set_layout(crate::panes::Layout::TwoColumns);
        let id = SessionId::new("s");
        app.slots[1] = Slot::Session(id.clone());
        let windows = app.windows.len();
        // 左のスロットの ✕ を押す ＝ 残る右のスロットは 1 番から 0 番へ振り直される。
        // **押す場所は当たり判定の正本から取る**（座標を手で書くと導出とずれる）
        let (cols, row) = crate::ui::close_zone(app.slot_rects()[0]).expect("no close mark");
        handle_mouse(&mut app, &click(*cols.end(), row)).unwrap();
        assert_eq!(app.layout, crate::panes::Layout::One, "the layout did not shrink");
        assert_eq!(app.slots.len(), 1, "the slots did not follow the layout");
        assert_eq!(
            app.slot_of(&id),
            Some(0),
            "the surviving session did not follow its new number"
        );
        assert_eq!(app.windows.len(), windows, "closing a frame killed a window");
    }

    /// **1 枚の盤面には減らす先が無い**ので、✕ は中身を空けるだけ（枠は残る）
    #[test]
    fn the_close_mark_only_empties_the_slot_when_one_pane_is_left() {
        let mut app = app_with_row("s");
        app.slots[0] = Slot::Session(SessionId::new("s"));
        let (cols, row) = crate::ui::close_zone(app.slot_rects()[0]).expect("no close mark");
        handle_mouse(&mut app, &click(*cols.end(), row)).unwrap();
        assert_eq!(app.layout, crate::panes::Layout::One, "the last pane went away");
        assert_eq!(app.slots.len(), 1);
        assert!(
            matches!(app.slots[0], Slot::Empty),
            "the slot still shows the session"
        );
    }

    /// **✕ は自分が乗っている掴み代に勝つ。** 下段スロットの上辺は横の境界そのもの
    /// なので、判定の順序が崩れると印を押した操作がリサイズの掴みに化ける
    #[test]
    fn the_close_mark_wins_over_the_resize_grip_it_sits_on() {
        let mut app = app_with_row("s");
        app.set_layout(crate::panes::Layout::TwoRows);
        let (cols, row) = crate::ui::close_zone(app.slot_rects()[1]).expect("no close mark");
        // 掴み代に乗っていることを先に確かめる（前提が消えたらこの検査は無意味になる）
        let (_, on_h) = app
            .layout
            .grab_at(crate::ui::pane_rect(&app), app.split, *cols.end(), row);
        assert!(on_h, "the fixture no longer puts the mark on the height grip");
        handle_mouse(&mut app, &click(*cols.end(), row)).unwrap();
        assert!(app.cross_drag.is_none(), "the click started a resize instead");
        assert_eq!(app.layout, crate::panes::Layout::One, "the layout did not shrink");
    }

    /// **スロットを減らしても何も終わらない。** 溢れたセッションは表示から
    /// 外れるだけで、PTY（[`App::windows`]）はそのまま残る
    #[test]
    fn shrinking_the_layout_only_hides_the_sessions_it_drops() {
        let mut app = app_with_row("s");
        app.set_layout(crate::panes::Layout::Four);
        app.slots[2] = Slot::Session(SessionId::new("s"));
        let windows = app.windows.len();
        app.set_layout(crate::panes::Layout::One);
        assert_eq!(app.slots.len(), 1, "the slots did not follow the layout");
        assert_eq!(app.windows.len(), windows, "shrinking the layout killed a window");
        assert_eq!(app.focus_slot, 0, "the focus stayed on a slot that no longer exists");
    }

    /// **`Alt+←/→` はスロットの宛先を壊さない。**
    ///
    /// これが崩れると、最左列以外のスロットを選んでサイドバーへ行き、
    /// 行を開いても別のスロットに出る（設計上いちばん踏みやすい罠なので固定する）
    #[test]
    fn walking_to_the_sidebar_and_back_keeps_the_target_slot() {
        let mut app = app_with_row("s");
        app.set_layout(crate::panes::Layout::Four);
        app.focus = Focus::Terminal;
        // `Alt+Shift+→` `Alt+Shift+↓` で右下（3）へ
        for code in [KeyCode::Right, KeyCode::Down] {
            let key = KeyEvent::new(code, KeyModifiers::ALT | KeyModifiers::SHIFT);
            let Some(Reserved::Slot(dir)) = reserved_key(&key) else {
                panic!("Alt+Shift+{code:?} is not reserved for slot moves");
            };
            let to = app.layout.neighbor(app.focus_slot, dir).expect("no neighbour");
            app.set_focus_slot(to);
        }
        assert_eq!(app.focus_slot, 3, "Alt+Shift did not reach the bottom-right slot");
        // `Alt+←/→` はサイドバーとの行き来だけ ＝ 宛先スロットは動かない
        app.set_focus(Focus::Sidebar);
        assert_eq!(app.focus_slot, 3, "walking to the sidebar moved the target slot");
        app.set_focus(Focus::Terminal);
        assert_eq!(app.focus_slot, 3, "coming back landed on a different slot");
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

    /// **`no session` のまま終了したら、次の起動も `no session`。**
    /// 空スロットは「ここには今なにも無い」という状態そのものなので、
    /// 再起動が勝手に書きかけのフォームへ差し替えてはいけない
    /// （窓が 1 つも開かなかったことを入口の不在と見なしていた頃のバグ）
    #[test]
    fn restoring_empty_slots_keeps_the_no_session_panes() {
        let mut app = test_app(34, TERM);
        app.set_layout(crate::panes::Layout::TwoColumns);
        restore_slots(&mut app, vec![SlotView::Empty, SlotView::Empty]);
        assert!(
            app.slots.iter().all(|s| matches!(s, Slot::Empty)),
            "the restore replaced a no session pane"
        );
    }

    /// 保存が 1 つも無い ＝ 初回起動。戻すべき「前回」が無いので入口を出す
    #[test]
    fn the_first_run_opens_the_new_session_form() {
        let mut app = test_app(34, TERM);
        app.dispatch_cwd = std::env::temp_dir().to_string_lossy().to_string();
        restore_slots(&mut app, Vec::new());
        assert!(matches!(app.slots[0], Slot::New(_)), "the first run had no entry point");
    }

    /// 開いていた new session 画面は開いたまま戻る
    #[test]
    fn a_saved_new_session_form_comes_back() {
        let mut app = test_app(34, TERM);
        app.dispatch_cwd = std::env::temp_dir().to_string_lossy().to_string();
        restore_slots(&mut app, vec![SlotView::New]);
        assert!(matches!(app.slots[0], Slot::New(_)), "the form did not come back");
    }

    /// **行の消えたセッションは空スロットになる**（フォームへは化けない）。
    /// 一覧から外れた会話は復元しようがないだけで、`+ new session` を押させる
    /// 場面ではない（入口はサイドバーに常にある）
    #[test]
    fn a_session_whose_row_is_gone_leaves_the_slot_empty() {
        let mut app = test_app(34, TERM);
        restore_slots(&mut app, vec![SlotView::Session("gone".to_string())]);
        assert!(matches!(app.slots[0], Slot::Empty), "the missing row opened a form");
        assert!(app.windows.is_empty(), "a window was started for a row that is gone");
    }

    /// **`+ new session` を押し直しても、開いている画面は作り直さない。**
    /// 打ちかけのプロンプトと参照中のフォルダが黙って消えるのを防ぐ
    /// （この行の意味は「この画面を出す」で、出ているものを捨てる操作ではない）
    #[test]
    fn pressing_new_session_again_keeps_the_open_form() {
        let mut app = test_app(34, TERM);
        app.dispatch_cwd = std::env::temp_dir().to_string_lossy().to_string();
        app.open_new_view();
        app.focused_new().unwrap().prompt.text = "half typed".to_string();
        run_row_action(&mut app, RowAction::New, 0);
        assert_eq!(
            app.focused_new().map(|s| s.prompt.text.as_str()),
            Some("half typed"),
            "the form was rebuilt and the typed prompt was lost"
        );
        assert_eq!(app.focus, Focus::Terminal, "the row no longer hands the form the keys");
    }
}
