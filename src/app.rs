//! App 状態機械・イベントループ（run）・マウス／キー処理・セッションのディスパッチ。
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{
    Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};

use ccdesk::{log_error, same_dir, BgJob};

use crate::keys::{encode_key, forward_mouse};
use crate::poll::{AgentInfo, FooterInfo, Grouping, UsageInfo};
use crate::session::Session;
use crate::source::{DataSource, PollSinks, WindowItem, PROJECTS_LIMIT};
use crate::ui::new_view::{handle_new_view_key, NewFocus, NewLayout, NewState};
use crate::ui::{draw, popup_rect, row_at, sidebar_layout};

const MIN_SIDEBAR: u16 = 12;
const MIN_PANE: u16 = 40;

// state.json は name(/rename)・needs・summary の正本なので短周期で読む
// （数十ファイルの小さな read。描画は dirty 時のみなので負荷は無視できる）
const SCAN_INTERVAL: Duration = Duration::from_secs(2);
const LIVE_SCAN_INTERVAL: Duration = Duration::from_secs(2);
/// 使用率の読み取り周期（statusline フックが書くキャッシュを見に行く間隔）
const USAGE_INTERVAL: Duration = Duration::from_secs(5);

/// ペインフォーカス。キー入力はフォーカス中のペインにだけ流す
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Focus {
    Sidebar,
    Terminal,
}

/// サイドバー行のクリック動作。セッションは short id で参照する。
/// jobs / sessions は 2 秒毎に再構築され並びも変わるため、描画時の生 index を
/// 保持すると実行時に別セッションを stop/rm し得る
#[derive(Clone, PartialEq, Debug)]
pub(crate) enum RowAction {
    New, // 新規セッション画面を開く
    /// プロジェクト見出し行 = そのフォルダのメニュー（new session / remove project）を開く。
    /// **クリックで即セッションが立つ行ではない**（起動は開いたメニューの中で選ぶ）
    Project(String),
    ToggleGroup, // グルーピング切替（state ⇔ directory）
    Open(String),  // short id: ウィンドウが開いていれば切替、無ければ claude attach
    UpdateCcdesk,  // ccdesk 自身を更新（サイドバー先頭の版行）
    UpdateClaude,  // claude 本体を更新（同じく版行）
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

/// メニュー枠が食う桁数: 左右の枠線 2 + 項目行の先頭空白 1（描画が `" {label}"` を出す）
const POPUP_CHROME: u16 = 3;
/// メニュー幅の下限。stop/delete・grouping 切替の見た目を従来（14 桁）から動かさないため
const POPUP_MIN_WIDTH: u16 = 14;

/// ポップアップに並べるアカウント 1 件。表示名と識別子を分けて持つ:
/// 表示名（`ooba · 1→10, Inc.` 等）は組織違い・別 email で重複し得るので、
/// 対象の特定はラベル一致ではなく「選択 index → id」で行う
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AccountItem {
    pub(crate) label: String,
    pub(crate) id: String,
}

/// モーダルの種類。**メニューの中身（項目・幅・項目の意味）はこの型が答える**。
/// [`Popup`] は「どこに開いたか・どれを選んでいるか」だけを持つので、
/// 種類を足すときの変更は [`PopupKind::entries`] と [`PopupKind::action`] の
/// 2 つの match に閉じる（幅は項目から導くので触らない）
#[derive(Debug, PartialEq)]
pub(crate) enum PopupKind {
    Session { short: String, stopped: bool },
    Group,
    /// アカウント一覧。開いた時点の写しを持つ（一覧の供給はデータ層の責務で、
    /// メニューは受け取った並びをそのまま出す）。保管 0 件でも
    /// `register current` だけのメニューとして成立する
    // 構築側（フッターのアカウント行クリック）はアカウント切替の作業が入れる
    #[allow(dead_code)]
    Account { accounts: Vec<AccountItem> },
    /// アカウント 1 件への操作（Account から遷移する 2 階層目）
    AccountActions { account: AccountItem },
    /// プロジェクト単位の操作。`has_sessions` は開いた時点の写し（[`PopupKind::Session`] の
    /// `stopped` と同じ作り）で、`remove project` を出せるかの判断に使う
    Project { cwd: String, has_sessions: bool },
}

/// 項目を選んだときに起きること。**選択 index から作る**ので、表示名が同じ項目が
/// 並んでも対象を取り違えない（ラベル文字列から対象を復元しない）。
/// 副作用は持たず、実行は [`run_popup_action`] だけが行う
#[derive(Debug, PartialEq)]
enum PopupAction {
    Stop(String),
    Delete(String),
    SetGrouping(Grouping),
    /// 2 階層目のメニューへ遷移する
    Open(PopupKind),
    /// 現在ログイン中のアカウントを保管に加える
    RegisterCurrent,
    /// 保管アカウントへ切り替える（[`AccountItem::id`]）
    SwitchAccount(String),
    /// 保管アカウントを一覧から外す（[`AccountItem::id`]）
    UnregisterAccount(String),
    /// 指定フォルダで新規セッション
    NewSessionIn(String),
    /// プロジェクトを一覧から外す
    RemoveProject(String),
}

impl PopupKind {
    /// (表示名, 実行可能か)。並びは [`PopupKind::action`] の index 解釈と対になるので、
    /// 項目を足すときは両方を同じ順で直す
    pub(crate) fn entries(&self, grouping: Grouping) -> Vec<(String, bool)> {
        match self {
            // delete は稼働中でも選べる（実行側が stop → rm の 2 段で処理する）
            PopupKind::Session { stopped, .. } => vec![
                ("stop".to_string(), !stopped),
                ("delete".to_string(), true),
            ],
            PopupKind::Group => {
                let mark = |g: Grouping| if grouping == g { "● " } else { "  " };
                vec![
                    (format!("{}state", mark(Grouping::State)), true),
                    (format!("{}directory", mark(Grouping::Directory)), true),
                ]
            }
            // 保管一覧が先、`register current` が末尾（0 件でもこの 1 項目は残る）
            PopupKind::Account { accounts } => accounts
                .iter()
                .map(|a| (a.label.clone(), true))
                .chain(std::iter::once(("register current".to_string(), true)))
                .collect(),
            PopupKind::AccountActions { .. } => vec![
                ("switch".to_string(), true),
                ("unregister".to_string(), true),
            ],
            // **セッションが残っているフォルダは登録解除させない。** 見出しの一覧は
            // 「登録リスト ∪ セッションの cwd」なので、登録を外してもセッション由来で
            // 見出しは出続ける。押せるのに表示が変わらないのは嘘なので、
            // stop と同じ仕組み（実行可能フラグ）で落とす
            PopupKind::Project { has_sessions, .. } => vec![
                ("new session".to_string(), true),
                ("remove project".to_string(), !has_sessions),
            ],
        }
    }

    /// メニュー幅。**項目の表示幅から決める**ので、アカウント表示名や email のような
    /// 動的な項目でも切れない。種類ごとに固定値を置くと項目を足した時点で嘘になるため、
    /// 幅の知識はここ 1 箇所だけに持たせる。端末へ収める責任は `popup_rect` 側
    pub(crate) fn width(&self, grouping: Grouping) -> u16 {
        use unicode_width::UnicodeWidthStr;
        let widest = self
            .entries(grouping)
            .iter()
            .map(|(label, _)| label.width().min(u16::MAX as usize) as u16)
            .max()
            .unwrap_or(0);
        widest.saturating_add(POPUP_CHROME).max(POPUP_MIN_WIDTH)
    }

    /// 選択 index の項目が意味する動作（範囲外・意味を持たない index は None）。
    /// 動的な項目は index で対象（アカウント）を引く
    fn action(&self, index: usize) -> Option<PopupAction> {
        match self {
            PopupKind::Session { short, .. } => match index {
                0 => Some(PopupAction::Stop(short.clone())),
                1 => Some(PopupAction::Delete(short.clone())),
                _ => None,
            },
            PopupKind::Group => match index {
                0 => Some(PopupAction::SetGrouping(Grouping::State)),
                1 => Some(PopupAction::SetGrouping(Grouping::Directory)),
                _ => None,
            },
            PopupKind::Account { accounts } => match accounts.get(index) {
                // 一覧の行 → そのアカウントの 2 階層目
                Some(account) => Some(PopupAction::Open(PopupKind::AccountActions {
                    account: account.clone(),
                })),
                // 一覧の 1 つ後ろが末尾項目
                None if index == accounts.len() => Some(PopupAction::RegisterCurrent),
                None => None,
            },
            PopupKind::AccountActions { account } => match index {
                0 => Some(PopupAction::SwitchAccount(account.id.clone())),
                1 => Some(PopupAction::UnregisterAccount(account.id.clone())),
                _ => None,
            },
            PopupKind::Project { cwd, .. } => match index {
                0 => Some(PopupAction::NewSessionIn(cwd.clone())),
                1 => Some(PopupAction::RemoveProject(cwd.clone())),
                _ => None,
            },
        }
    }
}

/// ☰ / group 行クリックで開くコンテキストメニューの開き状態。
/// 2 階層目は**階層を積まずに開き直す**（Esc・外クリックは常に全閉。戻り先を持たない）
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
    pub(crate) sessions: Vec<Session>,
    pub(crate) active: usize,
    // claude agents --json のライブ状態（正規 IF。バックグラウンドスレッドが更新）
    pub(crate) agents: Vec<AgentInfo>,
    pub(crate) agents_shared: Arc<Mutex<Vec<AgentInfo>>>,
    pub(crate) agents_dirty: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) jobs: Vec<BgJob>,
    pub(crate) last_scan: std::time::Instant,
    pub(crate) last_live_scan: std::time::Instant,
    // stop/delete 直後は反映を早めるため、この時刻まで 1 秒間隔で再スキャン
    pub(crate) rescan_hot_until: Option<std::time::Instant>,
    pub(crate) sidebar_width: u16,
    pub(crate) dragging: bool,
    pub(crate) last_drag_resize: std::time::Instant,
    pub(crate) term_size: (u16, u16), // (width, height)
    // サイドバー行 → クリック動作の対応（draw で構築）
    pub(crate) sidebar_rows: Vec<Option<RowAction>>,
    // サイドバー上部の固定行数（ccdesk 版行・claude 版行・区切り線・+ new session・
    // 区切り線・⊞ group・集計行）。正本は draw（積んだ行数をそのまま記録する）で、
    // ヒットテストとスクロール計算は sidebar_rows と同じく「最後に描いた値」を読む
    pub(crate) sidebar_header_rows: usize,
    // サイドバーのスクロール位置（先頭に表示する行 index。draw でクランプ）
    pub(crate) sidebar_scroll: usize,
    // ↑↓ で選択を動かした直後だけ true: 次の draw で選択行が見える位置へ追従する
    // （ホイールスクロールを選択位置へ引き戻さないための区別）
    pub(crate) sidebar_follow_sel: bool,
    pub(crate) hovered_row: Option<usize>,
    // サイドバーフォーカス時のキーボード選択行（sidebar_rows の index）
    pub(crate) selected_row: usize,
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
    // 使用率表示（opt-in: config.json の usage_display = "on"）。
    // 表示するかどうかの判断は供給元（DataSource::usage）が持つので、ここは
    // dispatch 時に statusline フックを注入するかの判断だけに使う
    pub(crate) usage_display: bool,
    pub(crate) usage: Option<UsageInfo>,
    pub(crate) last_usage_read: std::time::Instant,
    // 画面に出す値の供給元（実データ / 撮影用の固定データ）。起動時に 1 度だけ選ばれ、
    // 以降ここを通る限り「今 demo か」を問う必要が無い
    pub(crate) source: Box<dyn DataSource>,
    // Ctrl+X の 2 度押し削除（short id と 1 回目 stop の時刻。2 秒以内の再押下 = rm）
    pub(crate) pending_delete: Option<(String, std::time::Instant)>,
    // `claude --bg` は ~1s かかるため別スレッドで実行し、完了を channel で受ける
    pub(crate) spawn_rx: Option<std::sync::mpsc::Receiver<SpawnOutcome>>,
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
            sessions: Vec::new(),
            active: 0,
            agents: Vec::new(),
            agents_shared: Arc::new(Mutex::new(Vec::new())),
            agents_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            jobs: Vec::new(),
            last_scan: std::time::Instant::now(),
            last_live_scan: std::time::Instant::now(),
            rescan_hot_until: None,
            sidebar_width: 34,
            dragging: false,
            last_drag_resize: std::time::Instant::now(),
            term_size: (120, 30),
            sidebar_rows: Vec::new(),
            sidebar_header_rows: 0,
            sidebar_scroll: 0,
            sidebar_follow_sel: false,
            hovered_row: None,
            selected_row: 0,
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
            usage_display: false,
            usage: None,
            last_usage_read: std::time::Instant::now(),
            // 撮影用の供給元は state.json / config.json を書かないので、
            // テストが開発者の設定を踏まない
            source: Box::new(crate::source::DemoSource),
            pending_delete: None,
            spawn_rx: None,
            notice: None,
            grouping: Grouping::State,
            projects: Vec::new(),
            popup: None,
            // サイドバー側にしておく（set_focus が PTY へ通知を出さない）
            focus: Focus::Sidebar,
        }
    }
}

/// `claude --bg` ディスパッチ（別スレッド）の結果
pub(crate) struct SpawnOutcome {
    pub(crate) id: Option<String>,
    pub(crate) label: String,
    pub(crate) cwd: String,
    pub(crate) error: Option<String>,
}

impl App {
    fn pane_size(&self) -> (u16, u16) {
        // 右ペインの Block 枠線 2 行 + 下部バー 1 行を引いた内側サイズ (rows, cols)
        let rows = self.term_size.1.saturating_sub(3).max(1);
        let cols = self
            .term_size
            .0
            .saturating_sub(self.sidebar_width + 2)
            .max(1);
        (rows, cols)
    }

    fn resize_sessions(&mut self) {
        let (rows, cols) = self.pane_size();
        for session in &mut self.sessions {
            session.resize(rows, cols);
        }
    }

    pub(crate) fn open_new_view(&mut self) {
        self.right_view = RightView::New(NewState::browse(&self.dispatch_cwd));
        // 次回起動時に同じ画面を復元する
        self.source.save_window(WindowItem::LastView("new"));
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
    /// サイドバーへ移った瞬間は state.json を即スキャンして表示を最新化する
    fn set_focus(&mut self, focus: Focus) {
        if self.focus == focus {
            return;
        }
        if matches!(self.right_view, RightView::Sessions)
            && let Some(session) = self.sessions.get_mut(self.active) {
                session.send_focus(focus == Focus::Terminal);
            }
        self.focus = focus;
        if focus == Focus::Sidebar {
            self.last_scan = instant_ago(SCAN_INTERVAL);
            self.last_live_scan = instant_ago(LIVE_SCAN_INTERVAL);
        }
    }

    /// 右ペインに表示するセッションを切り替える（フォーカスは動かさない）
    fn show_session(&mut self, idx: usize) {
        if self.focus == Focus::Terminal && idx != self.active
            && let Some(old) = self.sessions.get_mut(self.active) {
                old.send_focus(false);
            }
        self.active = idx;
        self.right_view = RightView::Sessions;
        // 次回起動時に同じセッションを復元する
        if let Some(short) = self.sessions.get(idx).and_then(|s| s.attach_id.clone()) {
            self.source.save_window(WindowItem::LastView(&short));
        }
        if self.focus == Focus::Terminal
            && let Some(session) = self.sessions.get_mut(idx) {
                session.send_focus(true);
            }
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

/// 使用率表示 opt-in 用の注入 settings ファイルを書き、そのパスを返す。
/// `claude --bg` の dispatch 時に --settings で渡す（attach 側に渡しても statusLine は
/// 無視される・実測）。コマンドのパスは / 区切り必須: claude は statusline を
/// bash 経由で実行するため \ 区切りはエスケープとして食われる（実測）
fn write_inject_settings() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = ccdesk::ccdesk_dir()?;
    let exe_fwd = exe.to_string_lossy().replace('\\', "/");
    let settings = serde_json::json!({
        "statusLine": {
            "type": "command",
            "command": format!("\"{exe_fwd}\" statusline-hook"),
        }
    });
    let path = dir.join("inject-settings.json");
    std::fs::write(&path, settings.to_string()).ok()?;
    Some(path)
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
    fn begin() -> Self {
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::BeginSynchronizedUpdate
        );
        Self
    }
}

impl Drop for SyncOutput {
    fn drop(&mut self) {
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::EndSynchronizedUpdate
        );
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
        let _ = crossterm::execute!(std::io::stdout(), crossterm::cursor::MoveTo(pos.x, pos.y));
    }
    Ok(())
}

pub(crate) fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> anyhow::Result<()> {
    let mut last_draw = std::time::Instant::now();
    let mut force_draw = true;
    loop {
        if app.last_live_scan.elapsed() > LIVE_SCAN_INTERVAL {
            // 死んだ attach クライアント PTY は行として残さない
            // （セッション本体は bg 行が代表する。detach 後の重複行を防ぐ）
            while let Some(pos) = app.sessions.iter_mut().position(|s| !s.alive()) {
                remove_window(app, pos);
            }
            app.last_live_scan = std::time::Instant::now();
        }
        let hot = app
            .rescan_hot_until
            .is_some_and(|t| std::time::Instant::now() < t);
        let scan_due = if hot {
            app.last_scan.elapsed() > Duration::from_millis(500)
        } else {
            app.last_scan.elapsed() > SCAN_INTERVAL
        };
        if scan_due {
            app.jobs = app.source.jobs();
            app.last_scan = std::time::Instant::now();
            if !hot {
                app.rescan_hot_until = None;
            }
            force_draw = true; // 並びが変わったら即描画（表示と行データのずれを残さない）
        }
        // `claude --bg`（別スレッド）の完了を受け取って attach。UI はブロックしない
        if let Some(rx) = app.spawn_rx.take() {
            match rx.try_recv() {
                Ok(outcome) => {
                    if let Some(id) = &outcome.id {
                        // 起動に成功したフォルダだけを次回の new session 初期値にする。
                        // 保存は UI スレッドに寄せて state.json の書込み競合を避ける
                        app.source.save_window(WindowItem::LastFolder(&outcome.cwd));
                        attach_by_id(app, id, &outcome.label, &outcome.cwd);
                    }
                    if let Some(err) = outcome.error {
                        set_notice(app, err);
                    }
                    app.last_scan = instant_ago(SCAN_INTERVAL);
                    app.last_live_scan = instant_ago(LIVE_SCAN_INTERVAL);
                    force_draw = true;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => app.spawn_rx = Some(rx),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    set_notice(app, "claude --bg の実行スレッドが異常終了".to_string());
                    force_draw = true;
                }
            }
        }
        // 使用率を 5 秒毎に取り込む（実データなら statusline フックが書いた
        // キャッシュ、撮影用なら固定値。どちらを読むかは供給元が決める）
        if app.last_usage_read.elapsed() > USAGE_INTERVAL {
            app.last_usage_read = std::time::Instant::now();
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
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            force_draw = true;
        }
        // ccdesk 自身の更新の失敗を下部バーへ出す。成功は版行の "restart" が伝えるので
        // ここでは扱わない（Idle へ戻すので、失敗した更新はもう一度押せる）
        let failure = {
            let mut state = app
                .ccdesk_update
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
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
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            force_draw = true;
        }
        // agents --json のライブ状態を取り込む（rename・state 変化の即時反映）
        if app
            .agents_dirty
            .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            app.agents = app
                .agents_shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            // attach ウィンドウの表示名もライブ名に追従
            for session in &mut app.sessions {
                if let Some(id) = &session.attach_id
                    && let Some(agent) = app.agents.iter().find(|a| &a.id == id)
                        && !agent.name.is_empty() {
                            session.name = agent.name.clone();
                        }
            }
            // セッション本体の生死を追跡し、生存 → 終了へ遷移した attach ウィンドウは
            // 閉じて新規セッション画面へ（/exit・外部 stop 追従。claude は /exit 後に
            // 操作できない画面が残るため）。停止中への attach 復帰は誤検知しない
            let mut dead: Vec<String> = Vec::new();
            for session in &mut app.sessions {
                let Some(id) = &session.attach_id else { continue };
                let Some(agent) = app.agents.iter().find(|a| &a.id == id) else {
                    continue;
                };
                if agent.has_pid {
                    session.seen_alive = true;
                } else if session.seen_alive {
                    dead.push(id.clone());
                }
            }
            for short in dead {
                close_window_of(app, &short);
            }
            force_draw = true;
        }
        // 再描画は「PTY に新出力」「UI イベント」「250ms 周期（スピナー等）」のときだけ。
        // 無条件 60fps 再描画は claude 画面全体の再構築が毎フレーム走り重い
        let pty_dirty = app
            .sessions
            .iter()
            .any(|s| s.dirty.swap(false, std::sync::atomic::Ordering::Relaxed));
        if force_draw || pty_dirty || last_draw.elapsed() > Duration::from_millis(250) {
            draw_frame(terminal, app)?;
            last_draw = std::time::Instant::now();
            force_draw = false;
        }

        if !crossterm::event::poll(Duration::from_millis(33))? {
            continue;
        }
        force_draw = true; // イベントを処理したら必ず描画
        match crossterm::event::read()? {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                // 緊急脱出（マウスが効かない環境向け）。他は全部アクティブ PTY へ
                if key.code == KeyCode::Char('q')
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    return Ok(());
                }
                // グローバルキー: Alt+← = サイドバーへ / Alt+→ = ターミナルへ
                if key.modifiers.contains(KeyModifiers::ALT) {
                    match key.code {
                        KeyCode::Left => {
                            app.set_focus(Focus::Sidebar);
                            continue;
                        }
                        KeyCode::Right => {
                            app.set_focus(Focus::Terminal);
                            continue;
                        }
                        _ => {}
                    }
                }
                // サイドバーフォーカス中のキー操作（公式 Agent View 準拠、入力欄なし）:
                // ↑↓ = 行選択 / Enter・→ = 開く / Ctrl+X = stop→delete / Ctrl+S = グルーピング
                if app.focus == Focus::Sidebar {
                    // モーダル表示中はモーダルがキーを受ける
                    if app.popup.is_some() {
                        handle_popup_key(app, key.code);
                        continue;
                    }
                    match key.code {
                        KeyCode::Up => move_selection(app, -1),
                        KeyCode::Down => move_selection(app, 1),
                        // Ctrl+S = グルーピング切替（公式 Agent View と同じ）
                        KeyCode::Char('s')
                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            toggle_grouping(app);
                        }
                        KeyCode::Enter | KeyCode::Right => {
                            match app.sidebar_rows.get(app.selected_row).cloned().flatten() {
                                Some(RowAction::New) => {
                                    app.open_new_view();
                                    app.set_focus(Focus::Terminal);
                                }
                                Some(RowAction::ToggleGroup) => {
                                    app.popup = Some(Popup {
                                        kind: PopupKind::Group,
                                        anchor_y: selected_row_y(app),
                                        selected: 0,
                                    });
                                }
                                // 見出し行はメニューを開くだけ（セッションは起動しない）
                                Some(RowAction::Project(cwd)) => {
                                    let y = selected_row_y(app);
                                    open_project_popup(app, cwd, y);
                                }
                                Some(RowAction::Open(short)) => {
                                    open_short(app, &short);
                                    app.set_focus(Focus::Terminal);
                                }
                                Some(RowAction::UpdateCcdesk) => start_ccdesk_update(app),
                                Some(RowAction::UpdateClaude) => start_claude_update(app),
                                None => {}
                            }
                        }
                        KeyCode::Char('x')
                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            if let Some(RowAction::Open(short)) =
                                app.sidebar_rows.get(app.selected_row).cloned().flatten()
                            {
                                ctrl_x_short(app, &short);
                            }
                        }
                        _ => {}
                    }
                    continue;
                }
                // 新規セッション画面のキー操作
                if let RightView::New(_) = app.right_view {
                    handle_new_view_key(app, &key)?;
                    continue;
                }
                // フォーカスがターミナル側にあるときだけ PTY へ流す
                if app.sessions.is_empty() {
                    continue;
                }
                let session = &mut app.sessions[app.active];
                let bytes = encode_key(&key, &session.parser.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
                if !bytes.is_empty() {
                    let mut writer = session.writer.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    writer.write_all(&bytes)?;
                    writer.flush()?;
                }
            }
            Event::Paste(text) => {
                // New 画面の D&D/貼り付けはフォーカス中のフィールドで受ける:
                // Folder: → フォルダ切替（一覧も更新）/ それ以外 → プロンプトへ挿入
                // （パスを最初のメッセージ本文に書きたいケースがあるため）
                if let RightView::New(state) = &mut app.right_view {
                    if state.focus == NewFocus::Path {
                        if let Some(dir) = NewState::extract_dir(&text) {
                            state.set_dir(dir); // パスは丸ごと置き換える
                        } else {
                            state.path.insert_str(text.trim());
                            state.refresh_from_input();
                        }
                    } else {
                        state.prompt.insert_str(text.trim());
                        state.focus = NewFocus::Prompt;
                    }
                    continue;
                }
                if app.focus != Focus::Terminal {
                    continue;
                }
                if app.sessions.is_empty() {
                    continue;
                }
                // paste injection 対策: 制御文字（特に ESC = ペースト終端の偽装）を除去
                let sanitized: String = text
                    .chars()
                    .filter(|c| matches!(c, '\n' | '\r' | '\t') || !c.is_control())
                    .collect();
                let session = &mut app.sessions[app.active];
                let bracketed = session.parser.lock().unwrap_or_else(std::sync::PoisonError::into_inner).screen().bracketed_paste();
                let mut writer = session.writer.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                if bracketed {
                    writer.write_all(b"\x1b[200~")?;
                    writer.write_all(sanitized.as_bytes())?;
                    writer.write_all(b"\x1b[201~")?;
                } else {
                    writer.write_all(sanitized.as_bytes())?;
                }
                writer.flush()?;
            }
            Event::Mouse(mouse) => {
                let prev_hover = app.hovered_row;
                if handle_mouse(app, &mouse)? {
                    return Ok(());
                }
                // マウス移動だけで表示が変わらないなら再描画しない（FPS 対策）
                if matches!(mouse.kind, MouseEventKind::Moved)
                    && prev_hover == app.hovered_row
                {
                    force_draw = false;
                }
            }
            Event::Resize(w, h) => {
                app.term_size = (w, h);
                clamp_sidebar(app);
                app.resize_sessions();
            }
            // ホスト端末のフォーカス変化をアクティブ PTY へ中継
            // （ターミナルペインがフォーカス中のときだけ意味を持つ）
            Event::FocusGained => {
                if app.focus == Focus::Terminal
                    && let Some(session) = app.sessions.get_mut(app.active) {
                        session.send_focus(true);
                    }
            }
            Event::FocusLost => {
                if app.focus == Focus::Terminal
                    && let Some(session) = app.sessions.get_mut(app.active) {
                        session.send_focus(false);
                    }
            }
            _ => {}
        }
    }
}

/// New 画面からの起動 = 公式と同じ「`claude --bg` でディスパッチ → 即 attach」。
/// セッション実体は supervisor 管理になり、ccdesk を閉じても残り再起動後も一覧に出る。
/// `claude --bg` は ~1s かかるため別スレッドで実行する（UI スレッドを止めない）。
/// 結果は run ループが spawn_rx で受けて attach する
pub(crate) fn start_new_session(app: &mut App) -> anyhow::Result<()> {
    let RightView::New(state) = &app.right_view else {
        return Ok(());
    };
    let cwd = state.cur_dir.clone();
    let prompt = state.prompt.text.trim().to_string();
    dispatch_session(app, cwd, prompt);
    Ok(())
}

/// そのフォルダを登録プロジェクトへ加える。**プロジェクトの登録経路はここだけ**
/// （明示的な「追加」UI は持たず、セッションを作った時点で登録される）。
/// 並びは登録順＝新しいものが末尾で、画面上の並びは見出し側がアルファベット順に決める
fn register_project(app: &mut App, cwd: &str) {
    if cwd.is_empty() || app.projects.iter().any(|p| same_dir(p, cwd)) {
        return;
    }
    app.projects.push(cwd.to_string());
    // 上限を超えたら古い側から落とす。登録が自動なので、放っておくと
    // 「一度試しただけのフォルダ」が state.json に永久に積まれ見出しも際限なく増える。
    // 落ちたフォルダにセッションが残っていれば見出しは cwd 由来で出続けるので、
    // 落ちたこと自体が操作の邪魔にならない
    let excess = app.projects.len().saturating_sub(PROJECTS_LIMIT);
    app.projects.drain(..excess);
    app.source.save_window(WindowItem::Projects(&app.projects));
}

/// 登録プロジェクトから外す。セッションが残っているかの判断はメニュー側
/// （[`PopupKind::entries`] が項目を無効にする）で済んでいるので、ここは削るだけ
fn remove_project(app: &mut App, cwd: &str) {
    let before = app.projects.len();
    app.projects.retain(|p| !same_dir(p, cwd));
    if app.projects.len() != before {
        app.source.save_window(WindowItem::Projects(&app.projects));
    }
}

/// そのフォルダにセッションがあるか。材料は jobs と attach 中のウィンドウで、
/// **描画側が見出しの配下へ振り分ける集合と同じ**（片方だけを見ると、
/// 行が出ているのに `remove project` が押せてしまう）
fn project_has_sessions(app: &App, cwd: &str) -> bool {
    app.jobs.iter().any(|j| same_dir(&j.cwd, cwd))
        || app.sessions.iter().any(|s| same_dir(&s.cwd, cwd))
}

/// プロジェクト見出し行のメニューを開く（Enter とクリックで同じものが出る）。
/// `has_sessions` は開いた時点の写しにする（[`PopupKind::Project`] 参照）
fn open_project_popup(app: &mut App, cwd: String, anchor_y: u16) {
    let has_sessions = project_has_sessions(app, &cwd);
    app.popup = Some(Popup {
        kind: PopupKind::Project { cwd, has_sessions },
        anchor_y,
        selected: 0,
    });
}

/// キーボード選択行の画面 y。固定ヘッダーより下はスクロール分を引く。
/// メニューの矩形はこの 1 つ下に出るので、Enter でメニューを開く行は全部この式を使う
fn selected_row_y(app: &App) -> u16 {
    let row = if app.selected_row < app.sidebar_header_rows {
        app.selected_row
    } else {
        app.selected_row.saturating_sub(app.sidebar_scroll)
    };
    row as u16 + 1
}

/// 指定フォルダ・プロンプトで `claude --bg` をディスパッチし、完了後に attach する
/// （見出しメニューの new session は空プロンプトで直接ここに来る）
fn dispatch_session(app: &mut App, cwd: String, prompt: String) {
    if app.spawn_rx.is_some() {
        return; // 起動処理中の多重ディスパッチを防ぐ
    }
    // そのフォルダで作業を始めた ＝ プロジェクト。**登録をここに置くのが要点**で、
    // new session 画面の起動ボタンと見出しメニューの new session はどちらも
    // この関数へ収束するため、経路が増えても登録漏れが起きない
    register_project(app, &cwd);
    app.dispatch_cwd = cwd.clone();
    // 撮影用データは本物のセッションを起こさない（架空の一覧に実セッションが混ざらない）。
    // フォルダの登録と初期値の更新までは済んでいるので、供給元が違っても状態の育ち方は同じ
    if !app.source.spawns_sessions() {
        return;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    app.spawn_rx = Some(rx);
    // 使用率表示（opt-in）: dispatch にだけ statusline フックが効く（実測）
    let inject = app.usage_display.then(write_inject_settings).flatten();
    std::thread::spawn(move || {
        // 空プロンプトも可: "idle — send a prompt to start" のセッションになる
        let mut bg = std::process::Command::new("claude");
        bg.arg("--bg").arg(&prompt);
        if let Some(path) = inject {
            bg.arg("--settings");
            bg.arg(path);
        }
        let output = bg
            .current_dir(&cwd)
            .stdin(std::process::Stdio::null())
            .output();
    let outcome = match output {
            Err(e) => SpawnOutcome {
                id: None,
                label: String::new(),
                cwd,
                error: Some(format!("claude --bg 起動失敗: {e}")),
            },
            Ok(output) => {
                // 公式ドキュメント記載の出力形式「backgrounded · <id> · <name>」の行から id を取る
                let text = format!(
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                let id = text
                    .lines()
                    .find_map(|line| {
                        line.trim()
                            .strip_prefix("backgrounded")
                            .and_then(|rest| rest.split('·').nth(1))
                            .and_then(|field| field.split_whitespace().next())
                    })
                    .map(str::to_string)
                    .filter(|id| !id.is_empty());
                let label: String = if prompt.is_empty() {
                    "new session".to_string()
                } else {
                    prompt.chars().take(30).collect()
                };
                let error = id
                    .is_none()
                    .then(|| "claude --bg がセッション id を返さなかった".to_string());
                SpawnOutcome { id, label, cwd, error }
            }
        };
        let _ = tx.send(outcome);
    });
}

pub(crate) fn clamp_sidebar(app: &mut App) {
    let max = app.term_size.0.saturating_sub(MIN_PANE).max(MIN_SIDEBAR);
    app.sidebar_width = app.sidebar_width.clamp(MIN_SIDEBAR, max);
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
    // 境界線ドラッグ（サイドバー右枠線と右ペイン左枠線の 2 列をつかみ代にする）
    let border_zone =
        mouse.column >= app.sidebar_width.saturating_sub(1) && mouse.column <= app.sidebar_width;
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) if border_zone => {
            app.dragging = true;
            return Ok(false);
        }
        MouseEventKind::Drag(MouseButton::Left) if app.dragging => {
            app.sidebar_width = mouse.column.saturating_add(1);
            clamp_sidebar(app);
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

    if mouse.column < app.sidebar_width {
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
        // 画面 y → 行 index（列は見ないので行のどこを押しても当たる）。
        // 計算は描画側と同じ ui::row_at を共有する
        let row = row_at(
            mouse.row,
            sl.capacity,
            app.sidebar_header_rows,
            app.sidebar_scroll,
        );
        let action = app.sidebar_rows.get(row).cloned().flatten();
        // hover: クリック可能な行の上にいるときだけハイライト
        app.hovered_row = action.as_ref().map(|_| row);
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
            // サイドバー内クリックはサイドバーへフォーカス。
            // 行クリックは右ペインの内容だけ切り替える（フォーカス移動は右ペインクリック or Enter）
            app.set_focus(Focus::Sidebar);
            if action.is_some() {
                app.selected_row = row;
            }
            // 行頭の ☰ クリック → コンテキストメニューを開く
            if let Some(RowAction::Open(short)) = &action
                && mouse.column <= 2 {
                    let stopped = short_stopped(app, short);
                    app.popup = Some(Popup {
                        kind: PopupKind::Session {
                            short: short.clone(),
                            stopped,
                        },
                        anchor_y: mouse.row,
                        selected: 0,
                    });
                    return Ok(false);
                }
            // セッション行・new session クリックは右ペインへフォーカスを移す
            match action {
                Some(RowAction::New) => {
                    app.open_new_view();
                    app.set_focus(Focus::Terminal);
                }
                Some(RowAction::ToggleGroup) => {
                    app.popup = Some(Popup {
                        kind: PopupKind::Group,
                        anchor_y: mouse.row,
                        selected: 0,
                    });
                }
                // 見出し行クリックはメニューを開くだけ。**フォーカスは移さない**
                // （メニューがキーを受ける。セッション行クリックとは動作が違う）
                Some(RowAction::Project(cwd)) => {
                    open_project_popup(app, cwd, mouse.row);
                }
                Some(RowAction::Open(short)) => {
                    open_short(app, &short);
                    app.set_focus(Focus::Terminal);
                }
                // 更新行はその場で実行するだけ（右ペインを切り替えない）
                Some(RowAction::UpdateCcdesk) => start_ccdesk_update(app),
                Some(RowAction::UpdateClaude) => start_claude_update(app),
                None => {}
            }
        }
    } else {
        app.hovered_row = None;
        if let MouseEventKind::Down(_) = mouse.kind {
            app.set_focus(Focus::Terminal);
        }
        // New 画面: クリックでフォルダ選択・プロンプト欄フォーカス
        if let RightView::New(state) = &mut app.right_view {
            // 起動ボタン行のクリックは state の借用を抜けてからディスパッチする
            let mut launch = false;
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    // 描画と同じジオメトリでヒットテスト（右ペイン矩形を chunks[1] と同一に再構成）
                    let pane = Rect::new(
                        app.sidebar_width,
                        0,
                        app.term_size.0.saturating_sub(app.sidebar_width),
                        app.term_size.1.saturating_sub(1),
                    );
                    let layout = NewLayout::compute(pane);
                    let box_bottom = layout.prompt_box.y + layout.prompt_box.height;
                    if !layout.ok {
                        // ペインが小さすぎて未描画。フィールド判定はしない
                    } else if mouse.row >= layout.folder_hd_y && mouse.row <= layout.sep_y {
                        // FOLDER セクション（見出し・パス値・┄ 区切り）クリック → パスフィールド。
                        // パス値の行ならカーソルも移動、他はカーソル位置維持
                        state.focus = NewFocus::Path;
                        if mouse.row == layout.path_y {
                            let text_x = mouse.column.saturating_sub(layout.path_text_x);
                            state.path.click(text_x);
                        }
                    } else if mouse.row >= layout.prompt_hd_y && mouse.row < box_bottom {
                        // PROMPT セクション（見出し + 入力枠 3 行）クリック → プロンプト欄
                        state.focus = NewFocus::Prompt;
                        if mouse.row == layout.input_y {
                            let text_x = mouse.column.saturating_sub(layout.input_text_x);
                            state.prompt.click(text_x);
                        }
                    } else if mouse.row >= layout.list_top
                        && mouse.row < layout.list_top + layout.list_height
                    {
                        // フォルダ一覧エリア（空白部分も含む）→ 一覧フォーカス。
                        // 実在する行の上なら選択も動かし、選択済み行の再クリックで実行する
                        let row_in = (mouse.row - layout.list_top) as usize;
                        if row_in < state.shown {
                            let idx = state.scroll + row_in;
                            // 起動ボタン行もフォルダ行と同じ 2 段階（選択 → 再クリック）にする。
                            // 1 クリックで起動すると、プロンプト入力中に一覧へフォーカスを
                            // 移すだけのクリックが書きかけのプロンプトでセッションを起動して
                            // しまう（supervisor 管理なので取り消せない）。
                            // 判定はクリックで選択を動かす前に取る（動かした後では
                            // 常に dir_idx == idx になり 2 段階が崩れる）
                            let reclick = state.click_activates(idx);
                            state.select(idx);
                            state.focus = NewFocus::Browser;
                            if reclick {
                                if state.selected_is_launch() {
                                    launch = true;
                                } else {
                                    state.descend(); // 選択済みを再クリック = 潜る
                                }
                            }
                        } else {
                            state.focus = NewFocus::Browser;
                        }
                    }
                }
                MouseEventKind::ScrollUp => {
                    state.focus = NewFocus::Browser;
                    state.select_prev();
                }
                MouseEventKind::ScrollDown => {
                    state.focus = NewFocus::Browser;
                    state.select_next();
                }
                _ => {}
            }
            if launch {
                start_new_session(app)?;
            }
            return Ok(false);
        }
        if app.sessions.is_empty() {
            return Ok(false);
        }
        // 右ペイン: イベントを claude へ転送（ホイールも claude 自身がスクロール処理する）
        forward_mouse(app, mouse)?;
    }
    Ok(false)
}

/// 対象が停止済みかどうか（agents --json の pid 有無 = プロセス生存で判定）
fn short_stopped(app: &App, short: &str) -> bool {
    !app.agents.iter().any(|a| a.id == short && a.has_pid)
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
    // 枠線上のクリックは何もしない（上枠が先頭項目 "stop" に化けて誤発火しない）
    if row == rect.y
        || row == rect.y + rect.height - 1
        || col == rect.x
        || col == rect.x + rect.width - 1
    {
        return;
    }
    activate_popup(app, (row - rect.y - 1) as usize);
}

/// 選択項目の実行（Enter / クリック共通）。実行できない項目・範囲外の index は無視する
fn activate_popup(app: &mut App, index: usize) {
    let Some(popup) = app.popup.as_ref() else {
        return;
    };
    let entries = popup.kind.entries(app.grouping);
    if !entries.get(index).is_some_and(|(_, enabled)| *enabled) {
        return;
    }
    let Some(action) = popup.kind.action(index) else {
        return;
    };
    // 2 階層目は 1 階層目の選択行から生えて見えるようにする。矩形は anchor_y の
    // 1 つ下に出るので、渡すのは「選択行 - 1」= 枠の上端 + index
    let anchor_y = popup_rect(app, popup).y + index as u16;
    app.popup = None;
    run_popup_action(app, action, anchor_y);
}

/// メニュー項目の実行。**副作用はここだけ**に集め、「どの項目が何を意味するか」の
/// 判定は [`PopupKind::action`]（純関数）に置く
fn run_popup_action(app: &mut App, action: PopupAction, anchor_y: u16) {
    match action {
        PopupAction::Stop(short) => menu_stop(app, &short),
        PopupAction::Delete(short) => menu_delete(app, &short),
        PopupAction::SetGrouping(next) => {
            if app.grouping != next {
                toggle_grouping(app);
            }
        }
        // 開き直し（積まない）。anchor は親の選択行なので親から生えて見える
        PopupAction::Open(kind) => {
            app.popup = Some(Popup {
                kind,
                anchor_y,
                selected: 0,
            });
        }
        // 空プロンプトで起動する（登録は dispatch_session が行う）
        PopupAction::NewSessionIn(cwd) => dispatch_session(app, cwd, String::new()),
        PopupAction::RemoveProject(cwd) => remove_project(app, &cwd),
        // ここから下はメニュー基盤だけ先に用意した項目。実処理はアカウント切替の
        // 作業がこの arm を埋める
        PopupAction::RegisterCurrent
        | PopupAction::SwitchAccount(_)
        | PopupAction::UnregisterAccount(_) => {}
    }
}

/// グルーピング切替（UI クリック / Ctrl+S 共通）。選択は ~/.ccdesk/config.json に永続化
/// （撮影用の供給元は保存しない ＝ 開発者の設定を踏まない）
fn toggle_grouping(app: &mut App) {
    app.grouping = match app.grouping {
        Grouping::State => Grouping::Directory,
        Grouping::Directory => Grouping::State,
    };
    app.source.save_window(WindowItem::Grouping(app.grouping));
}

/// stop/delete 後の反映を早める（数秒間 1 秒間隔で再スキャン）
fn schedule_hot_rescan(app: &mut App) {
    app.rescan_hot_until = Some(std::time::Instant::now() + Duration::from_secs(8));
    app.last_scan = instant_ago(SCAN_INTERVAL);
}

/// claude サブコマンドを画面を汚さずに実行する。
/// spawn のまま stdio を継承すると子プロセスの出力が ccdesk の画面に直接混ざる
fn run_claude_silent(args: &[&str]) {
    use std::process::Stdio;
    let _ = std::process::Command::new("claude")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// メニュー: stop（supervisor 側のセッション本体を停止）。
/// attach 中のウィンドウは閉じ、右ペインは New 画面へ戻す（死んだ画面を表示しない）
fn menu_stop(app: &mut App, short: &str) {
    if short.is_empty() {
        return;
    }
    run_claude_silent(&["stop", short]);
    close_window_of(app, short);
    schedule_hot_rescan(app);
}

/// メニュー: delete（セッション本体を削除。attach 中のウィンドウも閉じる）。
/// `claude rm` の文書上の保証は「終了済みに効く」なので、稼働中は stop → rm の 2 段で行う
fn menu_delete(app: &mut App, short: &str) {
    if short.is_empty() {
        return;
    }
    let running = app.agents.iter().any(|a| a.id == short && a.has_pid);
    let short_for_thread = short.to_string();
    std::thread::spawn(move || {
        let short = short_for_thread;
        use std::process::Stdio;
        let quiet = |args: &[&str]| {
            let _ = std::process::Command::new("claude")
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .output(); // 完了を待って順序を保証する
        };
        if running {
            quiet(&["stop", &short]);
        }
        quiet(&["rm", &short]);
    });
    close_window_of(app, short);
    schedule_hot_rescan(app);
}

/// 指定セッションを見ているウィンドウ（attach クライアント）を閉じる
fn close_window_of(app: &mut App, short: &str) {
    if let Some(i) = app
        .sessions
        .iter()
        .position(|s| s.attach_id.as_deref() == Some(short))
    {
        if let Some(session) = app.sessions.get_mut(i) {
            let _ = session.child.kill();
        }
        remove_window(app, i);
    }
}

/// PTY ウィンドウ行を一覧から外す（active 添字も詰める）。
/// 表示するウィンドウが無くなったら右ペインは New 画面へ
fn remove_window(app: &mut App, idx: usize) {
    if idx >= app.sessions.len() {
        return;
    }
    let was_active = idx == app.active;
    app.sessions.remove(idx);
    app.hovered_row = None;
    if app.active >= idx && app.active > 0 {
        app.active -= 1;
    }
    if app.sessions.is_empty() || was_active {
        app.open_new_view();
    }
}

/// Ctrl+X（公式準拠）: 1 回目 = stop、2 秒以内の 2 回目 or 停止済み = delete。
/// ウィンドウ行・bg 行とも short id で扱う
fn ctrl_x_short(app: &mut App, short: &str) {
    if short.is_empty() {
        return;
    }
    let recent = app
        .pending_delete
        .as_ref()
        .is_some_and(|(s, t)| s == short && t.elapsed() < Duration::from_secs(2));
    let stopped = short_stopped(app, short);
    if !stopped && !recent {
        menu_stop(app, short);
        app.pending_delete = Some((short.to_string(), std::time::Instant::now()));
        return;
    }
    app.pending_delete = None;
    menu_delete(app, short);
}

/// サイドバーの選択行を、クリック可能な行へ上下に移動する
fn move_selection(app: &mut App, dir: i32) {
    let len = app.sidebar_rows.len();
    let mut row = app.selected_row as i32;
    loop {
        row += dir;
        if row < 0 || row >= len as i32 {
            return; // 端で止まる
        }
        if app.sidebar_rows[row as usize].is_some() {
            app.selected_row = row as usize;
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
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
            Err(e) => SelfUpdate::Failed(format!("ccdesk update 失敗: {e}")),
        };
        *shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = outcome;
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

/// 下部バーに数秒表示する通知（attach 失敗など、無反応に見せないため）。
/// あわせて ~/.ccdesk/error.log にも残す
fn set_notice(app: &mut App, msg: String) {
    log_error(&msg);
    app.notice = Some((msg, std::time::Instant::now()));
}

/// id 指定で claude attach を PTY 起動（既に開いていれば切替のみ）。
/// 失敗（cwd 消失等）は握りつぶさず下部バーへ通知する
fn attach_by_id(app: &mut App, id: &str, label: &str, cwd: &str) {
    if let Some(i) = app
        .sessions
        .iter()
        .position(|s| s.attach_id.as_deref() == Some(id))
    {
        app.show_session(i);
        return;
    }
    let (rows, cols) = app.pane_size();
    match Session::spawn(label, cwd, rows, cols, id) {
        Ok(session) => {
            app.sessions.push(session);
            app.show_session(app.sessions.len() - 1);
        }
        Err(e) => set_notice(app, format!("attach {id} 失敗: {e}")),
    }
}

/// short id でセッションを開く: ウィンドウが開いていれば切替、無ければ bg 行から attach
/// （停止中でも supervisor が保存状態から復帰させる）
pub(crate) fn open_short(app: &mut App, short: &str) {
    if let Some(i) = app
        .sessions
        .iter()
        .position(|s| s.attach_id.as_deref() == Some(short))
    {
        app.show_session(i);
        return;
    }
    let Some(job) = app.jobs.iter().find(|j| j.short == short) else {
        return; // 再スキャンで消えた行（クリックと削除の競合）は何もしない
    };
    let label = if job.name.is_empty() {
        "bg".to_string()
    } else {
        job.name.clone()
    };
    let cwd = job.cwd.clone();
    attach_by_id(app, short, &label, &cwd);
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

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
            .map(|(label, _)| label)
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

    fn account(label: &str, id: &str) -> AccountItem {
        AccountItem {
            label: label.to_string(),
            id: id.to_string(),
        }
    }

    fn session(short: &str, stopped: bool) -> PopupKind {
        PopupKind::Session {
            short: short.to_string(),
            stopped,
        }
    }

    /// 稼働中は stop・delete の両方が選べ、停止済みは stop だけ選べない（従来どおり）
    #[test]
    fn session_menu_disables_stop_only_when_the_session_is_stopped() {
        assert_eq!(
            session("s1", false).entries(Grouping::State),
            [("stop".to_string(), true), ("delete".to_string(), true)]
        );
        assert_eq!(
            session("s1", true).entries(Grouping::State),
            [("stop".to_string(), false), ("delete".to_string(), true)]
        );
    }

    /// stop / delete は行 index から引く（ラベル文字列で分岐しない）
    #[test]
    fn session_menu_maps_each_row_index_to_its_action() {
        let kind = session("abc123", false);
        assert_eq!(
            kind.action(0),
            Some(PopupAction::Stop("abc123".to_string()))
        );
        assert_eq!(
            kind.action(1),
            Some(PopupAction::Delete("abc123".to_string()))
        );
        assert_eq!(kind.action(2), None, "項目の無い index は何も起こさない");
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
            PopupKind::Group.action(0),
            Some(PopupAction::SetGrouping(Grouping::State))
        );
        assert_eq!(
            PopupKind::Group.action(1),
            Some(PopupAction::SetGrouping(Grouping::Directory))
        );
        assert_eq!(PopupKind::Group.action(2), None);
    }

    /// セッション行の ☰ クリックで stop / delete のメニューが開く（従来の入口）
    #[test]
    fn clicking_the_hamburger_opens_the_session_menu() {
        let mut app = test_app(34, TERM);
        app.sidebar_rows = vec![Some(RowAction::Open("abc123".to_string()))];
        app.sidebar_header_rows = 1;
        handle_mouse(&mut app, &click(0, 1)).unwrap();
        let popup = app.popup.as_ref().expect("メニューが開いていない");
        // agents が空 = プロセス無しなので停止済み扱い
        assert_eq!(popup.kind, session("abc123", true));
        assert_eq!(labels(&popup.kind, app.grouping), ["stop", "delete"]);
        assert_eq!(popup.anchor_y, 1, "クリックした行の下に出る");
    }

    /// ⊞ group 行クリック → メニュー → 別の行を選ぶと grouping が切り替わる。
    /// クリック判定は描画と同じ popup_rect の座標で行う
    #[test]
    fn clicking_the_group_row_and_picking_a_row_switches_grouping() {
        let mut app = test_app(34, TERM);
        app.sidebar_rows = vec![Some(RowAction::ToggleGroup)];
        app.sidebar_header_rows = 1;
        handle_mouse(&mut app, &click(5, 1)).unwrap();
        assert_eq!(
            app.popup.as_ref().map(|p| &p.kind),
            Some(&PopupKind::Group),
            "grouping メニューが開いていない"
        );
        let rect = popup_rect(&app, app.popup.as_ref().unwrap());
        handle_mouse(&mut app, &click(rect.x + 1, rect.y + 2)).unwrap(); // 2 行目 = directory
        assert_eq!(app.grouping, Grouping::Directory);
        assert!(app.popup.is_none(), "実行後は閉じる");
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

    /// 保管 0 件でも register current だけのメニューとして成立し、
    /// 保管があれば一覧が先・register current が末尾に並ぶ
    #[test]
    fn account_menu_lists_stored_accounts_before_register_current() {
        let empty = PopupKind::Account {
            accounts: Vec::new(),
        };
        assert_eq!(labels(&empty, Grouping::State), ["register current"]);
        let two = PopupKind::Account {
            accounts: vec![
                account("ooba · 1→10, Inc.", "id-a"),
                account("you@example.com", "id-b"),
            ],
        };
        assert_eq!(
            labels(&two, Grouping::State),
            ["ooba · 1→10, Inc.", "you@example.com", "register current"]
        );
    }

    /// 表示名が同じ項目が並んでも、選んだ行の対象（id）が選ばれる。
    /// ラベル文字列から対象を復元する実装では区別できない組み合わせ
    #[test]
    fn account_menu_picks_the_row_target_even_when_labels_are_identical() {
        let kind = PopupKind::Account {
            accounts: vec![account("ooba", "id-personal"), account("ooba", "id-work")],
        };
        assert_eq!(
            labels(&kind, Grouping::State),
            ["ooba", "ooba", "register current"]
        );
        assert_eq!(
            kind.action(0),
            Some(PopupAction::Open(PopupKind::AccountActions {
                account: account("ooba", "id-personal"),
            }))
        );
        assert_eq!(
            kind.action(1),
            Some(PopupAction::Open(PopupKind::AccountActions {
                account: account("ooba", "id-work"),
            }))
        );
        assert_eq!(kind.action(2), Some(PopupAction::RegisterCurrent));
        assert_eq!(kind.action(3), None);
    }

    /// 2 階層目は対象アカウントの id を各動作へ持ち込む
    #[test]
    fn account_actions_menu_carries_the_account_id_into_each_action() {
        let kind = PopupKind::AccountActions {
            account: account("ooba", "id-work"),
        };
        assert_eq!(labels(&kind, Grouping::State), ["switch", "unregister"]);
        assert_eq!(
            kind.action(0),
            Some(PopupAction::SwitchAccount("id-work".to_string()))
        );
        assert_eq!(
            kind.action(1),
            Some(PopupAction::UnregisterAccount("id-work".to_string()))
        );
        assert_eq!(kind.action(2), None);
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
            kind.action(0),
            Some(PopupAction::NewSessionIn("C:\\dev\\shop-app".to_string()))
        );
        assert_eq!(
            kind.action(1),
            Some(PopupAction::RemoveProject("C:\\dev\\shop-app".to_string()))
        );
        assert_eq!(kind.action(2), None);
    }

    /// 一覧の項目を Enter で選ぶと 2 階層目が開き、矩形は 1 階層目の選択行に来る
    #[test]
    fn selecting_an_account_opens_the_second_level_at_the_selected_row() {
        let mut app = test_app(34, TERM);
        open(
            &mut app,
            PopupKind::Account {
                accounts: vec![account("ooba", "id-a"), account("you@example.com", "id-b")],
            },
            5,
        );
        handle_popup_key(&mut app, KeyCode::Down); // 2 行目（id-b）を選ぶ
        let parent = popup_rect(&app, app.popup.as_ref().unwrap());
        let selected_row = parent.y + 1 + 1; // 上枠 + 選択 index
        handle_popup_key(&mut app, KeyCode::Enter);
        let popup = app.popup.as_ref().expect("2 階層目が開いていない");
        assert_eq!(
            popup.kind,
            PopupKind::AccountActions {
                account: account("you@example.com", "id-b"),
            }
        );
        assert_eq!(popup.selected, 0, "2 階層目の選択は先頭から");
        assert_eq!(
            popup_rect(&app, popup).y,
            selected_row,
            "2 階層目が親の選択行に寄っていない"
        );
    }

    /// Esc は階層を戻らず全部閉じる（戻り先を持たない）。外クリックも同じ
    #[test]
    fn esc_and_outside_click_close_every_popup_level() {
        let mut app = test_app(34, TERM);
        let accounts = || PopupKind::Account {
            accounts: vec![account("ooba", "id-a")],
        };
        open(&mut app, accounts(), 5);
        handle_popup_key(&mut app, KeyCode::Enter); // 1 階層目 → 2 階層目
        assert!(
            matches!(
                app.popup.as_ref().map(|p| &p.kind),
                Some(PopupKind::AccountActions { .. })
            ),
            "2 階層目が開いていない"
        );
        handle_popup_key(&mut app, KeyCode::Esc);
        assert!(app.popup.is_none(), "Esc で一覧に戻っている");

        open(&mut app, accounts(), 5);
        handle_popup_key(&mut app, KeyCode::Enter);
        let rect = popup_rect(&app, app.popup.as_ref().unwrap());
        handle_mouse(&mut app, &click(rect.right() + 2, rect.bottom() + 2)).unwrap();
        assert!(app.popup.is_none(), "外クリックで一覧に戻っている");
    }

    /// ↑↓ は項目数の範囲で止まる（端で溢れない）
    #[test]
    fn arrow_keys_clamp_the_selection_to_the_entry_range() {
        let mut app = test_app(34, TERM);
        open(
            &mut app,
            PopupKind::Account {
                accounts: vec![account("ooba", "id-a")],
            },
            3,
        );
        for _ in 0..5 {
            handle_popup_key(&mut app, KeyCode::Down);
        }
        // 一覧 1 件 + register current の 2 項目
        assert_eq!(app.popup.as_ref().unwrap().selected, 1);
        for _ in 0..5 {
            handle_popup_key(&mut app, KeyCode::Up);
        }
        assert_eq!(app.popup.as_ref().unwrap().selected, 0);
    }

    /// 実行できない項目（停止済みの stop）は Enter でも動かず、メニューも閉じない
    #[test]
    fn disabled_item_is_not_executed() {
        let mut app = test_app(34, TERM);
        open(&mut app, session("s1", true), 3);
        handle_popup_key(&mut app, KeyCode::Enter);
        assert!(
            app.popup.is_some(),
            "実行できない項目でメニューが閉じている"
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
                "枠 ({col},{row}) のクリックで閉じている"
            );
            assert_eq!(
                app.grouping,
                Grouping::State,
                "枠 ({col},{row}) のクリックで項目が発火した"
            );
        }
    }

    /// stop/delete・grouping 切替の幅は従来（14 桁）から動かさない
    #[test]
    fn menu_width_keeps_the_static_menus_at_the_previous_size() {
        assert_eq!(session("s1", false).width(Grouping::State), 14);
        assert_eq!(PopupKind::Group.width(Grouping::State), 14);
        assert_eq!(PopupKind::Group.width(Grouping::Directory), 14);
    }

    /// 幅は最長項目の表示幅から決まる（email やアカウント表示名が切れない）。
    /// 桁数は文字数ではなく表示幅で数える（全角は 2 桁）
    #[test]
    fn menu_width_adapts_to_the_longest_entry() {
        let long = "very.long.address@example.co.jp";
        let kind = PopupKind::Account {
            accounts: vec![account("ooba", "id-a"), account(long, "id-b")],
        };
        assert_eq!(
            kind.width(Grouping::State),
            long.width() as u16 + POPUP_CHROME
        );
        assert!(kind.width(Grouping::State) > PopupKind::Group.width(Grouping::State));

        let wide_label = "大場 · 1→10, Inc.";
        let wide = PopupKind::Account {
            accounts: vec![account(wide_label, "id-c")],
        };
        assert_eq!(
            wide.width(Grouping::State),
            wide_label.width() as u16 + POPUP_CHROME
        );
        assert!(
            wide_label.width() > wide_label.chars().count(),
            "全角の前提が崩れている"
        );
    }

    /// 内容から幅を決めるので、狭いサイドバーでは右ペインに被る。
    /// 被ることは許容するが、端末の外へは出さない
    #[test]
    fn wide_menu_overlaps_the_right_pane_but_keeps_its_left_edge() {
        let mut app = test_app(12, TERM);
        open(
            &mut app,
            PopupKind::Account {
                accounts: vec![account("very.long.address@example.co.jp", "id-a")],
            },
            3,
        );
        let rect = popup_rect(&app, app.popup.as_ref().unwrap());
        assert_eq!(rect.x, 1, "収まる限り左端はサイドバー内の x=1");
        assert!(
            rect.right() > app.sidebar_width,
            "サイドバーに収まってしまっている"
        );
        assert!(rect.right() <= TERM.0);
    }

    /// どのサイドバー幅・端末サイズ・anchor でも矩形は端末内に収まり、潰れない。
    /// 幅は内容で決まるため「サイドバーより広い」「端末より広い」が起こり得る
    #[test]
    fn popup_rect_stays_inside_the_terminal_for_any_sidebar_width() {
        let kinds = || {
            vec![
                session("s1", false),
                PopupKind::Group,
                PopupKind::Account {
                    accounts: (0..30)
                        .map(|i| {
                            account(
                                &format!("very.long.address.number.{i}@example.co.jp"),
                                &format!("id-{i}"),
                            )
                        })
                        .collect(),
                },
            ]
        };
        for (term_w, term_h) in [(120u16, 40u16), (80, 24), (52, 8), (14, 5), (1, 1)] {
            for sidebar_width in [12u16, 26, 34, term_w] {
                for anchor_y in [0u16, 1, 5, u16::MAX] {
                    for kind in kinds() {
                        let mut app = test_app(sidebar_width, (term_w, term_h));
                        open(&mut app, kind, anchor_y);
                        let rect = popup_rect(&app, app.popup.as_ref().unwrap());
                        assert!(
                            rect.right() <= term_w.max(1) && rect.bottom() <= term_h.max(1),
                            "端末 {term_w}x{term_h} / sidebar {sidebar_width} / anchor {anchor_y} で矩形 {rect:?} が外へ出る"
                        );
                        assert!(
                            rect.width >= 1 && rect.height >= 1,
                            "矩形 {rect:?} が潰れている"
                        );
                    }
                }
            }
        }
    }

    /// サイドバー幅より広いメニューは幅変更のつかみ代（境界線の列）に被る。
    /// その列のクリックは項目の実行で、幅変更ドラッグにはならない
    #[test]
    fn clicking_a_menu_row_over_the_resize_border_does_not_start_a_drag() {
        let mut app = test_app(12, TERM);
        open(
            &mut app,
            PopupKind::Account {
                accounts: vec![account("very.long.address@example.co.jp", "id-a")],
            },
            3,
        );
        let rect = popup_rect(&app, app.popup.as_ref().unwrap());
        assert!(
            rect.right() > app.sidebar_width,
            "境界線に被っている前提が崩れている"
        );
        let border_col = app.sidebar_width;
        handle_mouse(&mut app, &click(border_col, rect.y + 1)).unwrap();
        assert!(!app.dragging, "幅変更ドラッグが始まっている");
        assert_eq!(app.sidebar_width, 12, "サイドバー幅が動いている");
        assert!(
            matches!(
                app.popup.as_ref().map(|p| &p.kind),
                Some(PopupKind::AccountActions { .. })
            ),
            "被った列の項目が実行されていない"
        );
    }

    /// ヘッダーの版行 2 本 + 区切り線 + `+ new session` を積んだサイドバー。
    /// 版行のヒットテストを見るテストの土台
    fn app_with_version_rows(sidebar_width: u16) -> App {
        let mut app = test_app(sidebar_width, TERM);
        app.sidebar_rows = vec![
            Some(RowAction::UpdateCcdesk),
            Some(RowAction::UpdateClaude),
            None, // 区切り線
            Some(RowAction::New),
        ];
        app.sidebar_header_rows = 4;
        app
    }

    /// 版行は**行全体が当たる**。列 0（`☰` の桁）から内容の最右列まで、どこを
    /// 押しても同じ行に解決する（更新行に ☰ メニューは無いので列 0 も行に当たる）。
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
                assert_eq!(app.hovered_row, Some(row), "y={y} col={col}");
                assert_eq!(app.selected_row, row, "y={y} col={col}");
                assert_eq!(
                    app.sidebar_rows[row].as_ref(),
                    Some(&expected),
                    "y={y} col={col}"
                );
                assert!(app.popup.is_none(), "更新行でメニューが開いた y={y} col={col}");
                assert!(!app.dragging, "幅変更ドラッグが始まった y={y} col={col}");
            }
        }
        assert_eq!(app.sidebar_width, 34, "サイドバー幅が動いている");
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
        assert!(!app.dragging, "動詞の最終桁が幅変更のつかみ代になっている");
        assert_eq!(app.selected_row, 0, "動詞のクリックが行に当たっていない");
        // つかみ代はその 1 つ外（右枠の列）から始まる = 境界がここにあることの固定
        let mut app = app_with_version_rows(34);
        handle_mouse(&mut app, &click(verb_end + 1, 1)).unwrap();
        assert!(app.dragging, "境界線の列が幅変更にならない");
    }

    /// メニュー表示中の版行クリックは**メニューが受ける**（誤爆しない）。
    /// popup 判定が行のヒットテストより先にあることの固定
    #[test]
    fn an_open_menu_swallows_clicks_aimed_at_the_version_rows() {
        let mut app = app_with_version_rows(34);
        app.selected_row = 3; // `+ new session`。動いたら分かる位置に置く
        open(&mut app, PopupKind::Group, 3);
        let rect = popup_rect(&app, app.popup.as_ref().unwrap());
        assert!(rect.y > 2, "メニューが版行に被っていて外クリックにならない");
        handle_mouse(&mut app, &click(5, 1)).unwrap();
        assert_eq!(app.selected_row, 3, "版行が選択されてしまっている");
        assert!(app.hovered_row.is_none(), "版行がホバー扱いになっている");
        assert!(app.popup.is_none(), "メニュー外クリックで閉じていない");
        assert_eq!(state_name(&app), "Idle", "更新が走ってしまっている");
    }

    /// 更新の進行状態の名前（中身の文面ではなく「どの状態か」だけを見たい）
    fn state_name(app: &App) -> &'static str {
        match &*app
            .ccdesk_update
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            SelfUpdate::Idle => "Idle",
            SelfUpdate::Running => "Running",
            SelfUpdate::Done => "Done",
            SelfUpdate::Failed(_) => "Failed",
        }
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
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = state;
            start_ccdesk_update(&mut app);
            assert_eq!(
                state_name(&app),
                name,
                "済んだ / 走っている更新を再実行している"
            );
        }
    }

    /// テスト用のセッション 1 本（cwd だけが関心事）
    fn job_in(short: &str, cwd: &str) -> BgJob {
        BgJob {
            short: short.to_string(),
            cwd: cwd.to_string(),
            state: "working".to_string(),
            tempo: String::new(),
            name: String::new(),
            needs: String::new(),
            detail: String::new(),
            result: String::new(),
            children: Vec::new(),
            mtime: std::time::SystemTime::now(),
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    fn project(cwd: &str, has_sessions: bool) -> PopupKind {
        PopupKind::Project {
            cwd: cwd.to_string(),
            has_sessions,
        }
    }

    /// **セッションが残っているプロジェクトは登録解除できない。** 一覧は
    /// 「登録リスト ∪ セッションの cwd」なので、外しても見出しは出続ける ＝
    /// 押せるのに何も変わらないことになる。stop と同じ実行可能フラグで落とす
    #[test]
    fn project_menu_disables_remove_while_sessions_remain() {
        assert_eq!(
            project("C:\\dev\\api", false).entries(Grouping::Directory),
            [
                ("new session".to_string(), true),
                ("remove project".to_string(), true),
            ]
        );
        assert_eq!(
            project("C:\\dev\\api", true).entries(Grouping::Directory),
            [
                ("new session".to_string(), true),
                ("remove project".to_string(), false),
            ],
            "セッションが残っているのに remove project が選べる"
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
            "無効な remove project が実行された"
        );
    }

    /// 見出し行クリックでそのフォルダのメニューが開く（`+` を押して即起動ではない）。
    /// **フォーカスはサイドバーに残る**（開いたメニューがキーを受ける）
    #[test]
    fn clicking_a_project_heading_opens_its_menu() {
        let mut app = test_app(34, TERM);
        app.sidebar_rows = vec![Some(RowAction::Project("C:\\dev\\api".to_string()))];
        app.sidebar_header_rows = 1;
        handle_mouse(&mut app, &click(5, 1)).unwrap();
        let popup = app.popup.as_ref().expect("メニューが開いていない");
        assert_eq!(popup.kind, project("C:\\dev\\api", false));
        assert_eq!(popup.anchor_y, 1, "クリックした行の下に出る");
        assert!(app.focus == Focus::Sidebar, "フォーカスが右ペインへ移った");
        assert!(app.spawn_rx.is_none(), "クリックでセッションが起動している");
    }

    /// メニューを開く時点でセッションの有無を写す。**同名の末端ディレクトリが別パスに
    /// 2 つあっても、判定は開いた行のフルパスで行う**（片方にだけセッションがある状況で
    /// 取り違えると、消せるはずの登録が消せなくなる）
    #[test]
    fn opening_a_project_menu_reads_sessions_for_that_exact_folder() {
        let mut app = test_app(34, TERM);
        app.jobs = vec![job_in("s1", "C:\\work\\api")];
        // セッションを持つ側
        open_project_popup(&mut app, "C:\\work\\api".to_string(), 3);
        assert_eq!(app.popup.as_ref().unwrap().kind, project("C:\\work\\api", true));
        // 末端名が同じでも別パスならセッション無し扱い
        open_project_popup(&mut app, "C:\\dev\\api".to_string(), 3);
        assert_eq!(
            app.popup.as_ref().unwrap().kind,
            project("C:\\dev\\api", false),
            "同名末端のフォルダのセッションを拾っている"
        );
        // 大小・末尾の区切り違いは同じフォルダとして拾う
        open_project_popup(&mut app, "c:\\work\\api\\".to_string(), 3);
        assert_eq!(
            app.popup.as_ref().unwrap().kind,
            project("c:\\work\\api\\", true),
            "大小違いのセッションを取りこぼしている"
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

    /// 上限を超えたら古い側から落とす（登録が自動なので放っておくと際限なく積まれる）
    #[test]
    fn registering_beyond_the_limit_drops_the_oldest() {
        let mut app = test_app(34, TERM);
        for i in 0..PROJECTS_LIMIT + 1 {
            register_project(&mut app, &format!("C:\\dev\\p{i}"));
        }
        assert_eq!(app.projects.len(), PROJECTS_LIMIT, "上限を超えて積まれている");
        assert_eq!(
            app.projects.first().map(String::as_str),
            Some("C:\\dev\\p1"),
            "最古の登録が落ちていない"
        );
        assert_eq!(
            app.projects.last().map(String::as_str),
            Some(format!("C:\\dev\\p{PROJECTS_LIMIT}").as_str()),
            "最新の登録が入っていない"
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
        assert!(app.popup.is_none(), "実行後もメニューが開いている");
        assert_eq!(app.projects, ["C:\\dev\\api"]);
    }

    /// **自動登録の経路 1: 新規セッション画面の起動。** 供給元が撮影用データなので
    /// 本物の `claude --bg` は起きず、フォルダの登録と初期値の更新だけが観測できる
    #[test]
    fn launching_from_the_new_session_view_registers_its_folder() {
        let dir = std::env::temp_dir().to_string_lossy().to_string();
        let mut app = test_app(34, TERM);
        app.right_view = RightView::New(NewState::browse(&dir));
        start_new_session(&mut app).unwrap();
        assert_eq!(app.projects, std::slice::from_ref(&dir), "起動したフォルダが登録されない");
        assert_eq!(app.dispatch_cwd, dir);
        assert!(app.spawn_rx.is_none(), "撮影用データで claude を起動している");
    }

    /// **自動登録の経路 2: 見出しメニューの new session。** 経路 1 と同じ
    /// `dispatch_session` に収束するので、登録の知識は 1 箇所で足りている
    #[test]
    fn picking_new_session_from_the_project_menu_registers_its_folder() {
        let mut app = test_app(34, TERM);
        open(&mut app, project("C:\\dev\\api", false), 5);
        handle_popup_key(&mut app, KeyCode::Enter); // 先頭 = new session
        assert!(app.popup.is_none(), "実行後もメニューが開いている");
        assert_eq!(app.projects, ["C:\\dev\\api"]);
        assert_eq!(app.dispatch_cwd, "C:\\dev\\api");
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
            "テストが実ユーザーの state.json を書き換えている"
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
        app.selected_row = 2;
        assert_eq!(selected_row_y(&app), 3);
        // ヘッダーより下はスクロール分を引く
        app.selected_row = 10;
        assert_eq!(selected_row_y(&app), 7);
        // 引きすぎても 0 未満にならない
        app.sidebar_scroll = 99;
        assert_eq!(selected_row_y(&app), 1);
    }
}
