//! App 状態機械・イベントループ（run）・マウス／キー処理・セッションのディスパッチ。
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{
    Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};

use ccdesk::{log_error, save_setting, save_state, scan_jobs, BgJob};

use crate::keys::{encode_key, forward_mouse};
use crate::poll::{demo_jobs, read_usage, AgentInfo, FooterInfo, Grouping, UsageInfo};
use crate::session::Session;
use crate::ui::new_view::{handle_new_view_key, NewFocus, NewLayout, NewState};
use crate::ui::{draw, popup_rect, sidebar_layout};

const MIN_SIDEBAR: u16 = 12;
const MIN_PANE: u16 = 40;

/// サイドバー上部の固定行数（+ new session / 区切り線 / ⊞ group / 集計行）。
/// スクロールはこの下のセッション一覧にだけ効く
pub(crate) const SIDEBAR_HEADER_ROWS: usize = 4;

pub(crate) const JOBS_LIMIT: usize = 50;
// state.json は name(/rename)・needs・summary の正本なので短周期で読む
// （数十ファイルの小さな read。描画は dirty 時のみなので負荷は無視できる）
const SCAN_INTERVAL: Duration = Duration::from_secs(2);
const LIVE_SCAN_INTERVAL: Duration = Duration::from_secs(2);

/// ペインフォーカス。キー入力はフォーカス中のペインにだけ流す
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Focus {
    Sidebar,
    Terminal,
}

/// サイドバー行のクリック動作。セッションは short id で参照する。
/// jobs / sessions は 2 秒毎に再構築され並びも変わるため、描画時の生 index を
/// 保持すると実行時に別セッションを stop/rm し得る
#[derive(Clone, PartialEq)]
pub(crate) enum RowAction {
    New,           // 新規セッション画面を開く
    NewIn(String), // 指定フォルダで新規セッション画面を開く（プロジェクト見出しの +）
    ToggleGroup,   // グルーピング切替（state ⇔ directory）
    Open(String),  // short id: ウィンドウが開いていれば切替、無ければ claude attach
}

/// モーダルの種類
pub(crate) enum PopupKind {
    Session { short: String, stopped: bool },
    Group,
}

/// ☰ / group 行クリックで開くコンテキストメニュー
pub(crate) struct Popup {
    pub(crate) kind: PopupKind,
    pub(crate) anchor_y: u16, // 開いた元の画面行
    pub(crate) selected: usize,
}

impl Popup {
    /// (表示名, 実行可能か)
    pub(crate) fn entries(&self, grouping: Grouping) -> Vec<(String, bool)> {
        match &self.kind {
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
        }
    }
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
    // claude update 実行中（ボタン連打防止と "updating…" 表示）
    pub(crate) claude_updating: Arc<std::sync::atomic::AtomicBool>,
    // 使用率表示（opt-in: config.json の usage_display = "on"）
    pub(crate) usage_display: bool,
    pub(crate) usage: Option<UsageInfo>,
    pub(crate) last_usage_read: std::time::Instant,
    // スクリーンショット撮影用の架空データ描画（--demo）
    pub(crate) demo: bool,
    // Ctrl+X の 2 度押し削除（short id と 1 回目 stop の時刻。2 秒以内の再押下 = rm）
    pub(crate) pending_delete: Option<(String, std::time::Instant)>,
    // `claude --bg` は ~1s かかるため別スレッドで実行し、完了を channel で受ける
    pub(crate) spawn_rx: Option<std::sync::mpsc::Receiver<SpawnOutcome>>,
    // 下部バーに数秒表示するエラー等の通知
    pub(crate) notice: Option<(String, std::time::Instant)>,
    pub(crate) grouping: Grouping,
    pub(crate) popup: Option<Popup>,
    pub(crate) focus: Focus,
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
        save_state("last_view", "new"); // 次回起動時に同じ画面を復元する
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
            save_state("last_view", &short);
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
            app.jobs = if app.demo {
                demo_jobs() // 撮影用: 実セッションを一切表示しない
            } else {
                scan_jobs(JOBS_LIMIT)
            };
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
                        save_state("last_folder", &outcome.cwd);
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
        // 使用率キャッシュ（statusline フックが書く）を 5 秒毎に読む
        if app.demo {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            app.usage = Some(UsageInfo {
                five: Some((34.0, now + 2 * 3600 + 40 * 60)),
                seven: Some((58.0, now + 3 * 86400 + 5 * 3600)),
                stale: false,
            });
        } else if app.usage_display && app.last_usage_read.elapsed() > Duration::from_secs(5) {
            app.last_usage_read = std::time::Instant::now();
            let usage = read_usage();
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
                        let grouping = app.grouping;
                        let popup = app.popup.as_mut().unwrap();
                        match key.code {
                            KeyCode::Esc => app.popup = None,
                            KeyCode::Up => popup.selected = popup.selected.saturating_sub(1),
                            KeyCode::Down => {
                                popup.selected =
                                    (popup.selected + 1).min(popup.entries(grouping).len() - 1);
                            }
                            KeyCode::Enter => {
                                let entries = popup.entries(grouping);
                                let (label, enabled) = entries[popup.selected].clone();
                                if enabled {
                                    let popup = app.popup.take().unwrap();
                                    run_popup_action(app, &popup, &label);
                                }
                            }
                            _ => {}
                        }
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
                                    // 画面上の行位置（固定ヘッダーより下はスクロール補正）
                                    let y = if app.selected_row < SIDEBAR_HEADER_ROWS {
                                        app.selected_row
                                    } else {
                                        app.selected_row.saturating_sub(app.sidebar_scroll)
                                    } as u16
                                        + 1;
                                    app.popup = Some(Popup {
                                        kind: PopupKind::Group,
                                        anchor_y: y,
                                        selected: 0,
                                    });
                                }
                                Some(RowAction::NewIn(cwd)) => {
                                    dispatch_session(app, cwd, String::new());
                                }
                                Some(RowAction::Open(short)) => {
                                    open_short(app, &short);
                                    app.set_focus(Focus::Terminal);
                                }
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

/// 指定フォルダ・プロンプトで `claude --bg` をディスパッチし、完了後に attach する
/// （プロジェクト見出しの + は空プロンプトで直接ここに来る）
fn dispatch_session(app: &mut App, cwd: String, prompt: String) {
    if app.spawn_rx.is_some() {
        return; // 起動処理中の多重ディスパッチを防ぐ
    }
    let (tx, rx) = std::sync::mpsc::channel();
    app.spawn_rx = Some(rx);
    app.dispatch_cwd = cwd.clone();
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
            save_state("sidebar_width", &app.sidebar_width.to_string());
            return Ok(false);
        }
        _ if app.dragging => return Ok(false),
        _ => {}
    }

    // モーダル表示中はモーダルが全クリックを受ける
    if app.popup.is_some() {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
            handle_popup_click(app, mouse.column, mouse.row);
        }
        return Ok(false);
    }

    if mouse.column < app.sidebar_width {
        let sl = sidebar_layout(app);
        // フッターの更新ボタン行クリック（アカウント行の 1 つ上。描画時のみ有効）
        if sl.footer_visible
            && sl.update_row_visible
            && mouse.row == sl.account_y.saturating_sub(1)
        {
            if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
                start_claude_update(app);
            }
            app.hovered_row = None;
            return Ok(false);
        }
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
        // 上枠線ぶんを引き、固定ヘッダーより下はスクロールぶんも補正して行 index へ。
        // 表示窓（capacity）の外＝フッター帯や下枠のクリックは、スクロールで隠れた
        // 行のアクションを誤発火しないよう不感帯にする
        let r = mouse.row.saturating_sub(1) as usize;
        let row = if mouse.row == 0 || r >= sl.capacity {
            usize::MAX // 枠線・フッター帯 → どの行にも対応しない
        } else if r < SIDEBAR_HEADER_ROWS {
            r
        } else {
            r + app.sidebar_scroll
        };
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
                Some(RowAction::NewIn(cwd)) => {
                    // セッション切替クリックと同じく、フォーカスは右ペインへ
                    dispatch_session(app, cwd, String::new());
                    app.set_focus(Focus::Terminal);
                }
                Some(RowAction::Open(short)) => {
                    open_short(app, &short);
                    app.set_focus(Focus::Terminal);
                }
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

/// モーダル内クリック
fn handle_popup_click(app: &mut App, col: u16, row: u16) {
    let Some(popup) = &app.popup else { return };
    let rect = popup_rect(app, popup);
    if !rect.contains(Position::new(col, row)) {
        app.popup = None; // 外クリックで閉じる
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
    let idx = (row - rect.y - 1) as usize;
    let entries = popup.entries(app.grouping);
    if idx < entries.len() && entries[idx].1 {
        let label = entries[idx].0.clone();
        let popup = app.popup.take().unwrap();
        run_popup_action(app, &popup, &label);
    }
}

/// メニュー項目の実行
fn run_popup_action(app: &mut App, popup: &Popup, label: &str) {
    match &popup.kind {
        PopupKind::Session { short, .. } => match label {
            "stop" => menu_stop(app, short),
            "delete" => menu_delete(app, short),
            _ => {}
        },
        PopupKind::Group => {
            let next = if label.contains("state") {
                Grouping::State
            } else {
                Grouping::Directory
            };
            if app.grouping != next {
                toggle_grouping(app);
            }
        }
    }
}

/// グルーピング切替（UI クリック / Ctrl+S 共通）。選択は ~/.ccdesk/config.json に永続化
fn toggle_grouping(app: &mut App) {
    app.grouping = match app.grouping {
        Grouping::State => Grouping::Directory,
        Grouping::Directory => Grouping::State,
    };
    save_setting(
        "grouping",
        if app.grouping == Grouping::Directory {
            "directory"
        } else {
            "state"
        },
    );
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

/// claude 本体の更新を実行する（公式 `claude update`）。
/// 公式仕様: 更新は次回起動時から有効で、実行中セッションは現行版のまま動き続ける。
/// 完了後はフッターを再取得し、最新化されれば更新ボタン行は消える
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
