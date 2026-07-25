//! サイドバー・右ペインの描画と、描画／クリック判定で共有するジオメトリ計算。
pub(crate) mod new_view;

use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem};
use ratatui::Frame;
use std::time::Duration;
use tui_term::widget::PseudoTerminal;

use ccdesk::same_dir;

use crate::app::{active_unstored, App, Focus, Popup, RightView, RowAction, SelfUpdate};
use crate::poll::{classify, AccountStatus, Bucket, Group, Grouping, StateView};
use crate::session::SessionStatus;
use crate::theme::{
    ui, usage_color, C_ATTENTION, C_FAIL, C_WORKING, FOCUS_BORDER, MUTED_FG,
};
use crate::ui::new_view::draw_new_view;

/// リセット時刻のローカル表記。当日なら "14:00"、別日なら "7/29 09:00"
fn fmt_reset_at(resets_at: u64) -> String {
    use chrono::{Datelike, Local, TimeZone, Timelike};
    let Some(t) = Local.timestamp_opt(resets_at as i64, 0).single() else {
        return String::new();
    };
    let today = Local::now().date_naive();
    if t.date_naive() == today {
        format!("{:02}:{:02}", t.hour(), t.minute())
    } else {
        format!("{}/{} {:02}:{:02}", t.month(), t.day(), t.hour(), t.minute())
    }
}

/// サイドバーのジオメトリ（描画とクリック判定で同じ計算を共有する）
pub(crate) struct SidebarLayout {
    /// 一覧に使える行数（枠とフッターを除く）
    pub(crate) capacity: usize,
    /// フッターを描くか（狭すぎる端末では描かない = クリックも受けない）
    pub(crate) footer_visible: bool,
    /// アカウント行の画面 y（footer_visible のときだけ有効）
    pub(crate) account_y: u16,
}

pub(crate) fn sidebar_layout(app: &App) -> SidebarLayout {
    // 下部バー 1 行を除いたサイドバー矩形は draw の chunks[0] と一致する
    sidebar_layout_of(app.term_size.1.saturating_sub(1), app.sidebar_width)
}

/// [`sidebar_layout`] の本体。更新行が上部の版行に集約されたことでフッターは
/// 「区切り線 + アカウント行」の 2 行に固定され、ジオメトリはサイドバー矩形の
/// 大きさだけで決まる純関数になった（App を組まずにテストできる）
fn sidebar_layout_of(height: u16, sidebar_width: u16) -> SidebarLayout {
    let footer_visible = height >= 8 && sidebar_width > 4;
    let footer_rows = if footer_visible { 2 } else { 0 };
    SidebarLayout {
        capacity: (height as usize).saturating_sub(2 + footer_rows),
        footer_visible,
        account_y: height.saturating_sub(2),
    }
}

/// サイドバー内クリックの行 index。枠の 1 行を引き、固定ヘッダーより下は
/// スクロールぶんを足す。**列は見ない = 行のどこを押しても同じ行に当たる**。
///
/// 表示窓（`capacity`）の外＝フッター帯や下枠は `usize::MAX` を返して不感帯にする
/// （スクロールで隠れた行のアクションを誤発火させない）
pub(crate) fn row_at(mouse_row: u16, capacity: usize, header_rows: usize, scroll: usize) -> usize {
    let r = mouse_row.saturating_sub(1) as usize;
    if mouse_row == 0 || r >= capacity {
        usize::MAX
    } else if r < header_rows {
        r
    } else {
        r + scroll
    }
}

/// サイドバーを横断する区切り線のテキスト（枠の内側幅ぶん）
fn separator_text(inner_width: u16) -> String {
    "─".repeat(inner_width as usize)
}

/// 更新マーカー。**表示幅は実測 1 桁**（U+27F3 / unicode-width 0.2.2 で `1`。
/// East Asian Ambiguous で 2 桁を占める `☰` とは違う）。1 桁だと分かっているので、
/// 更新が無い行はスペース 1 個で同じ桁を確保できる
const UPDATE_MARK: &str = "⟳";

/// バージョン行の更新状態。マーカー桁と右端の動詞はこれだけで決まる。
/// ccdesk 側（[`SelfUpdate`]）と claude 側（`claude_updating` + `footer.latest`）で
/// 進行状態の持ち方が違うので、表示の語彙をここに 1 つだけ置いて両方を寄せる
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum UpdateState {
    /// 最新。マーカー桁は空白で確保する（更新が出たときに行が横へずれると
    /// 「増えたこと」に気づきにくい）
    Current,
    /// 新しい版がある = 行をクリックすれば更新できる
    Available,
    /// 更新の実行中
    Running,
    /// 差し替え済み。ccdesk も claude も反映は次回起動なので、
    /// そのセッション中はずっと再起動を促す
    Restart,
}

impl UpdateState {
    /// 右端に置く動詞。最新のときだけ空（やることが無い）。
    ///
    /// **新しい版の番号は出さない。** 新旧を並べた `⟳ claude v2.1.218 → v2.1.220` に
    /// 動詞まで足すと実測 35 桁で、既定幅（内側 32 桁）に収まらない。現行版と
    /// 「やること」のどちらも欠かせないので、落とすのは新版の番号にした
    fn verb(self) -> &'static str {
        match self {
            Self::Current => "",
            Self::Available => "update",
            Self::Running => "updating…",
            Self::Restart => "restart",
        }
    }

    /// クリックで更新を始められるか（＝行にアクションを付けるか）。
    /// 実行中と再起動待ちはもう押す意味が無いので、ハイライトも出さない
    fn actionable(self) -> bool {
        self == Self::Available
    }

    /// 行のスタイル。最新は dim（背景情報）、やることがある行は本文色にする
    /// （dim だと更新の存在に気づかない）
    fn style(self) -> Style {
        match self {
            Self::Current => Style::default().fg(ui().dim),
            Self::Running => Style::default().fg(C_WORKING),
            Self::Available | Self::Restart => Style::default().fg(MUTED_FG),
        }
    }
}

/// バージョン行 1 本の文面。`<マーカー> <名前> v<版>` を左に、動詞を右端へ寄せる。
///
/// 版が未取得（起動直後・CLI 失敗）なら番号を出さない ＝ 誤情報を出さない
fn version_row(name: &str, version: &str, state: UpdateState, inner_width: u16) -> String {
    use unicode_width::UnicodeWidthStr;
    let mark = if state == UpdateState::Current {
        " " // マーカー桁を空白で確保する（更新が出ても名前の桁が動かない）
    } else {
        UPDATE_MARK
    };
    let left = if version.is_empty() {
        format!("{mark} {name}")
    } else {
        format!("{mark} {name} v{version}")
    };
    let verb = state.verb();
    if verb.is_empty() {
        return left;
    }
    // 右端寄せ。入り切らない幅でも動詞は落とさず、間隔を 1 桁まで詰める
    // （List が右端で切るので、溢れたときに失われるのは動詞側になる）
    let gap = (inner_width as usize)
        .saturating_sub(left.width() + verb.width())
        .max(1);
    format!("{left}{}{verb}", " ".repeat(gap))
}

/// サイドバー最上部の固定行（ccdesk の版行 / claude の版行 / 区切り線）。
///
/// 2 つの更新をここへ集約する（下部フッターには置かない ＝ 同じことを 2 箇所に出さない）。
/// **行数は更新の有無で変わらない**ので、固定ヘッダー行数もマーカー桁の位置も動かない。
/// Frame に触らない純関数なので、4 状態の文面と当たり判定をテストで固定できる
fn version_rows(
    ccdesk: UpdateState,
    claude_version: &str,
    claude: UpdateState,
    inner_width: u16,
) -> Vec<(String, Style, Option<RowAction>)> {
    vec![
        (
            version_row("ccdesk", env!("CARGO_PKG_VERSION"), ccdesk, inner_width),
            ccdesk.style(),
            ccdesk.actionable().then_some(RowAction::UpdateCcdesk),
        ),
        (
            version_row("claude", claude_version, claude, inner_width),
            claude.style(),
            claude.actionable().then_some(RowAction::UpdateClaude),
        ),
        (
            separator_text(inner_width),
            Style::default().fg(ui().dim),
            None,
        ),
    ]
}

/// directory グルーピングの見出し行。返すのは (見出しの表示文字列, 対象フォルダ) で、
/// 1 要素目は**画面に出る文字列そのもの**（見出しに何を出すかの知識をここだけに持つ）。
///
/// 一覧は「登録リスト ∪ セッションの cwd」の**和集合**。登録リスト側があるので
/// セッションが 0 本になっても見出しは消えず、そのフォルダで新規を開く入口が残る
/// （以前はセッションの cwd から導出するだけだったので、最後のセッションが消えると
/// 見出しごと消えていた）。未登録でセッションだけあるフォルダも従来どおり出す。
///
/// 並びは末端ディレクトリ名のアルファベット順（大小無視）で従来どおり。**同名の末端が
/// 別パスに複数あるときはフルパスで決める**: キーが末端名だけだと安定ソートの入力順
/// （＝セッションの走査順）で並びが変わり、同じ画面が再描画で入れ替わり得る
fn project_rows(projects: &[String], session_cwds: &[&str]) -> Vec<(String, String)> {
    let mut dirs: Vec<&str> = Vec::new();
    // 登録リストが先。state.json を手で直された場合の自己重複もここで落ちる
    for cwd in projects.iter().map(String::as_str).chain(session_cwds.iter().copied()) {
        if !dirs.iter().any(|d| same_dir(d, cwd)) {
            dirs.push(cwd);
        }
    }
    let leaf_key = |cwd: &str| {
        std::path::Path::new(cwd)
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    };
    dirs.sort_by(|a, b| leaf_key(a).cmp(&leaf_key(b)).then_with(|| a.cmp(b)));
    dirs.into_iter()
        .map(|cwd| {
            // 見出しはプロジェクト名（末端ディレクトリ名）だけ。**ここに `+` は出さない**:
            // 行の動作はメニューを開くことなので、「押したら即セッションが立つ」という
            // ヒントは嘘になる。末端が取れないのはドライブ直下（`C:\`）等だけなので、
            // その場合はパスをそのまま出す（ホーム短縮が効く形にはならない）
            let leaf = std::path::Path::new(cwd)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| cwd.to_string());
            (leaf, cwd.to_string())
        })
        .collect()
}

/// ccdesk 自身の版行の状態。更新の進行状態が版チェックの結果より優先する
fn ccdesk_update_state(app: &App) -> UpdateState {
    match &*app
        .ccdesk_update
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        SelfUpdate::Running => UpdateState::Running,
        SelfUpdate::Done => UpdateState::Restart,
        // Failed は run ループが下部バーへ出して Idle へ戻すので、行は再試行可のまま
        SelfUpdate::Idle | SelfUpdate::Failed(_) => {
            if app.ccdesk_latest.is_some() {
                UpdateState::Available
            } else {
                UpdateState::Current
            }
        }
    }
}

/// claude 本体の版行の状態。ccdesk 側と違って Restart を持たないのは、更新後に
/// `claude --version` が新しい版を返して `footer.latest` が消える ＝ 行が自然に
/// 最新表示へ戻るため。ネイティブインストールは既定で自動更新するので、
/// 何もしなくてもこの行が消えることもある（公式仕様）
fn claude_update_state(app: &App) -> UpdateState {
    if app
        .claude_updating
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        UpdateState::Running
    } else if app.footer.latest.is_some() {
        UpdateState::Available
    } else {
        UpdateState::Current
    }
}

/// 表示幅で切る。**文字数で切ってはいけない**: アカウント名・組織名は任意の
/// 文字列なので全角を含み得て、文字数で数えると桁が溢れて枠を壊す
/// （`⟳` は実測 1 桁なので版行には影響しないが、切り方の知識を 1 つにしておく）
fn clip_to_width(s: &str, width: u16) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > width as usize {
            break;
        }
        out.push(ch);
        used += w;
    }
    out
}

/// 未保管警告のマーカー。**表示幅は実測 1 桁**（U+26A0 / unicode-width 0.2.2 で `1`。
/// 異体字セレクタを付けた `⚠️` は 2 桁になるので、素の 1 文字で持つ）。
/// 既定のサイドバー幅（内側 32 桁）にアカウント行を収める前提がこの実測値に乗っている
const WARN_MARK: &str = "⚠";

/// 未ログインのときのアカウント行。**再ログインの手順まで出す。**
///
/// 保管したアカウントのリフレッシュトークンは使い捨てで、ccdesk が動いていない間に
/// 別の場所でそのアカウントを使うと保管が無効になる。切替直後にこの状態へ落ちるのが
/// その現れで、**事前検知はしない**（検知には ccdesk 自身がトークン更新
/// エンドポイントを叩く必要があり、それは claude Code の client_id を借用する
/// 行為なので意図的に避けている）。事後にこの行で気づけることが唯一の出口なので、
/// 状態だけでなく打つ手も書く。文面は `ccdesk doctor` の案内と同じ語彙にそろえる
const LOGGED_OUT_ROW: &str = "not logged in · run /login";

/// アカウント行の文面とスタイル。Frame に触らない純関数なので、`⚠` の有無と
/// 桁数をテストで固定できる。`unstored` は [`active_unstored`] の判定
fn account_row(status: &AccountStatus, unstored: bool) -> (String, Style) {
    match status {
        // 未保管のときは `⚠` を前置し、色も dim から注意色へ上げる。dim のままだと
        // 登録し忘れに気づけず、次の /login で前のアカウントの認証情報が
        // 上書きされて失われる（`.credentials.json` は常に 1 アカウント分だけ）
        AccountStatus::LoggedIn(account) if unstored => (
            format!("{WARN_MARK} {}", account.label),
            Style::default().fg(C_ATTENTION),
        ),
        // 出すのはラベルだけ（email は同一性の保持用で、行には出さない）
        AccountStatus::LoggedIn(account) => {
            (account.label.clone(), Style::default().fg(ui().dim))
        }
        AccountStatus::LoggedOut => {
            (LOGGED_OUT_ROW.to_string(), Style::default().fg(C_ATTENTION))
        }
        // 未取得（起動直後・CLI 失敗）は誤情報を出さないため空行にする
        AccountStatus::Unknown => (String::new(), Style::default().fg(ui().dim)),
    }
}

/// モーダルの矩形。描画とクリック判定で同じ計算を共有する。
///
/// 幅は内容が決める（[`crate::app::PopupKind::width`]）ので、サイドバーより広い
/// メニューは右ペインに被る。アカウント表示名や email を切って読めなくするより、
/// 被せて全部読ませる方を選んだ。
///
/// ただし**端末の外へは出さない**: 矩形が画面外へ出ると ratatui の描画が壊れるので、
/// 幅・高さを端末サイズで丸めてから位置を決める（項目数が端末の高さを超える場合は
/// 入る分だけを描く）
pub(crate) fn popup_rect(app: &App, popup: &Popup) -> Rect {
    let entries = popup.kind.entries(app.grouping);
    let (term_w, term_h) = (app.term_size.0.max(1), app.term_size.1.max(1));
    let width = popup.kind.width(app.grouping).min(term_w);
    let height = entries.len().saturating_add(2).min(term_h as usize) as u16;
    // 左端はサイドバー内の x=1 固定。端末に収まらないときだけ左へ寄せる
    let x = 1u16.min(term_w - width);
    let y = popup.anchor_y.saturating_add(1).min(term_h - height);
    Rect::new(x, y, width, height)
}

fn fmt_age(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

/// 1 フレーム終端のカーソル状態。**位置は可視性に関係なく必ず返す**。
///
/// ratatui は位置が None だとカーソル非表示コマンドしか出さず MoveTo を出さない。
/// 一方で差分描画は「変更セルごとに MoveTo」なので、位置を渡さないフレームでは
/// 物理カーソルが最終変更セルに置き去りになる。日本語変換中は右ペインに差分が出ず
/// サイドバー（スピナー 400ms・経過時間 1s）だけが変わるため、その置き去り先は
/// サイドバー内になる。Windows の IME 変換窓はコンソールカーソル位置に
/// アンカーされるので、これが「変換中に一瞬サイドバーへ飛ぶ」症状になる。
pub(crate) struct FrameCursor {
    pub(crate) pos: Position,
    pub(crate) visible: bool,
}

impl FrameCursor {
    pub(crate) fn shown_at(pos: Position) -> Self {
        Self { pos, visible: true }
    }

    /// 見せないが位置は確定させる（IME のアンカーを迷子にしない）
    pub(crate) fn hidden_at(pos: Position) -> Self {
        Self { pos, visible: false }
    }
}

pub(crate) fn draw(frame: &mut Frame, app: &mut App) -> FrameCursor {
    // 最下行は横断のキーヒントバー
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(app.sidebar_width),
            Constraint::Min(1),
        ])
        .split(vert[0]);

    // 下部バー: 通知（attach 失敗等）があれば数秒それを出し、無ければキーヒント
    if let Some((msg, at)) = &app.notice {
        if at.elapsed() < Duration::from_secs(5) {
            frame.render_widget(
                ratatui::widgets::Paragraph::new(Line::from(format!(" {msg}")))
                    .style(Style::default().fg(C_FAIL)),
                vert[1],
            );
        } else {
            app.notice = None;
        }
    }
    if app.notice.is_none() {
        let mut hint_spans = vec![
            Span::styled(" app:", Style::default().fg(MUTED_FG)),
            Span::raw(" Ctrl+Q quit · Alt+←→ focus"),
        ];
        if app.focus == Focus::Sidebar {
            hint_spans.push(Span::styled("  sidebar:", Style::default().fg(MUTED_FG)));
            hint_spans.push(Span::raw(
                " ↑↓ select · Enter open · Ctrl+S group · Ctrl+X stop→delete",
            ));
        }
        // 右端: 5h/7d 使用率とリセット時刻（opt-in。statusline フック由来の公式データ）。
        // 古いデータ（10 分超更新なし）は消さず、全体を dim に落として区別する
        let mut usage_spans: Vec<Span> = Vec::new();
        if let Some(usage) = &app.usage {
            let stale = usage.stale;
            let ring = |pct: f64| ["○", "◔", "◑", "◕", "●"][(pct / 25.0).min(4.0) as usize];
            let mut push_window = |label: &str, w: Option<(f64, u64)>| {
                if let Some((pct, resets)) = w {
                    if !usage_spans.is_empty() {
                        usage_spans.push(Span::styled(" · ", Style::default().fg(ui().dim)));
                    }
                    usage_spans.push(Span::styled(
                        format!("{label} "),
                        Style::default().fg(ui().dim),
                    ));
                    let value_color = if stale { ui().dim } else { usage_color(pct) };
                    usage_spans.push(Span::styled(
                        format!("{} {}%", ring(pct), pct.round() as u32),
                        Style::default().fg(value_color),
                    ));
                    if resets > 0 {
                        usage_spans.push(Span::styled(
                            format!(" →{}", fmt_reset_at(resets)),
                            Style::default().fg(ui().dim),
                        ));
                    }
                }
            };
            push_window("5h", usage.five);
            push_window("7d", usage.seven);
            usage_spans.push(Span::raw(" "));
        }
        let usage_w = usage_spans
            .iter()
            .map(|s| s.content.chars().count() as u16)
            .sum::<u16>();
        let bar = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(1), Constraint::Length(usage_w)])
            .split(vert[1]);
        // new session 画面のヒントはペイン内に出すため、下部バーには重ねない
        frame.render_widget(
            ratatui::widgets::Paragraph::new(Line::from(hint_spans))
                .style(Style::default().fg(ui().dim)),
            bar[0],
        );
        if usage_w > 0 {
            frame.render_widget(
                ratatui::widgets::Paragraph::new(Line::from(usage_spans)),
                bar[1],
            );
        }
    }

    // サイドバー: 行の正本は agents --json（ライブ）+ state.json（summary 補完）。
    // 自分の PTY 行は「attach ウィンドウ」としてだけ出す
    let active = app.active;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    // 自分のウィンドウの表示は agents --json（attach_id で突合）→ classify で決める。
    // agents に居なければ出力ヒューリスティックへフォールバック
    let agents_snapshot = app.agents.clone();
    let views: Vec<StateView> = app
        .sessions
        .iter_mut()
        .map(|s| {
            if let Some(agent) = s
                .attach_id
                .as_deref()
                .and_then(|id| agents_snapshot.iter().find(|a| a.id == id))
            {
                classify(&agent.state, false, agent.has_pid)
            } else if !s.alive() {
                classify("stopped", false, false)
            } else {
                match s.status_heuristic() {
                    SessionStatus::Working => classify("working", false, true),
                    SessionStatus::NeedsInput => classify("blocked", false, true),
                    SessionStatus::Exited => classify("stopped", false, false),
                }
            }
        })
        .collect();

    // ---- 行データを先に組み立てる（State / Directory 両グルーピング対応）----
    struct RowData {
        action: RowAction,
        group: Group,
        cwd: String,
        glyph: &'static str,
        color: Color,
        label: String,
        is_active_window: bool,
        status_label: &'static str,
        summary: String,
        children: Vec<String>,
        age: String,
        bucket: Bucket,
    }
    // 公式の Working スピナーは点滅アニメ
    let spinner = if (now_ms / 400).is_multiple_of(2) { "✽" } else { "✻" };
    // 公式準拠: 形状 = プロセス生死（✻ 生存 / ∙ 終了）、Working は点滅
    let glyph_of = |view: &StateView| -> &'static str {
        if view.spinning {
            spinner
        } else if view.alive {
            "✻"
        } else {
            "∙"
        }
    };

    let mut data: Vec<RowData> = Vec::new();
    for (i, session) in app.sessions.iter().enumerate() {
        let view = views[i];
        let age_secs = session
            .last_output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .elapsed()
            .as_secs();
        data.push(RowData {
            action: RowAction::Open(session.attach_id.clone().unwrap_or_default()),
            group: view.group,
            cwd: session.cwd.clone(),
            glyph: glyph_of(&view),
            color: view.color,
            label: session.name.clone(),
            is_active_window: i == active && matches!(app.right_view, RightView::Sessions),
            status_label: view.label,
            summary: String::new(),
            children: Vec::new(),
            age: fmt_age(age_secs),
            bucket: view.bucket,
        });
    }
    for job in app.jobs.iter() {
        if app
            .sessions
            .iter()
            .any(|s| s.attach_id.as_deref() == Some(job.short.as_str()))
        {
            continue; // attach 中は自分のウィンドウ行が代表する（集計もウィンドウ行が担う）
        }
        // agents --json のライブ状態を優先（rename・state 変化が即時反映される）
        let agent = app.agents.iter().find(|a| a.id == job.short);
        let live_state = agent
            .map(|a| a.state.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(job.state.as_str());
        let alive = agent.map(|a| a.has_pid).unwrap_or(false);
        let view = classify(live_state, job.tempo == "blocked", alive);
        let summary = match view.label {
            "Done" | "Failed" => job.result.clone(),
            "Needs input" => job.needs.clone(),
            _ => job.detail.clone(),
        };
        // 表示名はライブ名を優先。未命名はプロンプト投入前なら "new session"、他は "bg"
        let live_name = agent
            .map(|a| a.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| job.name.clone());
        let label = if !live_name.is_empty() {
            live_name
        } else if live_state == "working" && job.result.is_empty() {
            "new session".to_string()
        } else {
            "bg".to_string()
        };
        // 公式の経過時間: 作業中 = 作成からの経過、終了後 = 総実行時間で凍結。
        // 隣に出す状態表示と同じ live_state で判定する（state.json 直読みは古い）
        let age_secs = match live_state {
            "done" | "failed" | "stopped" if job.updated_at_ms >= job.created_at_ms => {
                (job.updated_at_ms - job.created_at_ms) / 1000
            }
            _ if job.created_at_ms > 0 => now_ms.saturating_sub(job.created_at_ms) / 1000,
            _ => job.mtime.elapsed().map(|d| d.as_secs()).unwrap_or(0),
        };
        // PR 持ちで未完了なら Ready for review（公式: 完了系はそのまま Completed へ）
        let group = if !job.children.is_empty() && view.group != Group::Completed {
            Group::ReadyForReview
        } else {
            view.group
        };
        data.push(RowData {
            action: RowAction::Open(job.short.clone()),
            group,
            cwd: job.cwd.clone(),
            glyph: glyph_of(&view),
            color: view.color,
            label,
            is_active_window: false,
            status_label: view.label,
            summary,
            children: job.children.clone(),
            age: fmt_age(age_secs),
            bucket: view.bucket,
        });
    }
    // ヘッダー集計は表示行そのものから数える（分岐の複製をしない = 行数と必ず一致）
    let mut awaiting = 0usize;
    let mut working = 0usize;
    let mut completed = 0usize;
    for d in &data {
        match d.bucket {
            Bucket::Awaiting => awaiting += 1,
            Bucket::Working => working += 1,
            Bucket::Completed => completed += 1,
        }
    }

    // ---- 描画 ----
    let hovered = app.hovered_row;
    let selected = app.selected_row;
    let mut items: Vec<ListItem> = Vec::new();
    let mut rows: Vec<Option<RowAction>> = Vec::new();

    let push_data_row =
        |items: &mut Vec<ListItem>, rows: &mut Vec<Option<RowAction>>, d: &RowData| {
            let cur = rows.len();
            let highlighted = hovered == Some(cur) || selected == cur;
            let mut line_style = Style::default();
            if highlighted {
                line_style = line_style.bg(ui().hl_bg).fg(ui().emph);
            }
            let name_style = if d.is_active_window {
                Style::default().fg(ui().emph).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let mut spans = vec![
                Span::styled(
                    "☰",
                    Style::default().fg(if highlighted {
                        ui().emph
                    } else {
                        MUTED_FG
                    }),
                ),
                Span::raw(" "),
                Span::styled(d.glyph, Style::default().fg(d.color)),
                Span::raw(" "),
                Span::styled(d.label.clone(), name_style),
                Span::raw("  "),
                Span::styled(d.status_label, Style::default().fg(d.color)),
            ];
            if !d.summary.is_empty() {
                spans.push(Span::raw(" · "));
                spans.push(Span::raw(d.summary.clone()));
            }
            for child in &d.children {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(child.clone(), Style::default().fg(C_ATTENTION)));
            }
            spans.push(Span::raw(" · "));
            spans.push(Span::styled(
                d.age.clone(),
                Style::default().fg(ui().dim),
            ));
            items.push(ListItem::new(Line::from(spans).style(line_style)));
            rows.push(Some(d.action.clone()));
        };

    let inner_width = chunks[0].width.saturating_sub(2);

    // 先頭: ccdesk / claude の版行と区切り線。更新があるときだけ行全体がクリック可
    for (text, style, action) in version_rows(
        ccdesk_update_state(app),
        &app.footer.current,
        claude_update_state(app),
        inner_width,
    ) {
        let cur = rows.len();
        let mut style = style;
        if action.is_some() && (hovered == Some(cur) || selected == cur) {
            style = style.bg(ui().hl_bg).fg(ui().emph);
        }
        items.push(ListItem::new(Line::from(text).style(style)));
        rows.push(action);
    }

    // 新規セッション
    {
        let cur = rows.len();
        let highlighted = hovered == Some(cur) || selected == cur;
        let mut style = Style::default();
        if highlighted {
            style = style.bg(ui().hl_bg).fg(ui().emph);
        }
        items.push(ListItem::new(Line::from("+ new session").style(style)));
        rows.push(Some(RowAction::New));
    }
    // 区切り線: new session（アクション）とセッション一覧領域を分ける（Desktop 風）
    items.push(ListItem::new(
        Line::from(separator_text(inner_width)).style(Style::default().fg(ui().dim)),
    ));
    rows.push(None);
    // グルーピング切替（クリックで state ⇔ directory）
    {
        let cur = rows.len();
        let highlighted = hovered == Some(cur) || selected == cur;
        let mut style = Style::default().fg(ui().dim);
        if highlighted {
            style = style.bg(ui().hl_bg).fg(ui().emph);
        }
        let chosen = if app.grouping == Grouping::State {
            "state"
        } else {
            "directory"
        };
        items.push(ListItem::new(
            Line::from(vec![
                Span::raw("⊞ group: "),
                Span::styled(chosen, Style::default().fg(ui().emph)),
            ])
            .style(style),
        ));
        rows.push(Some(RowAction::ToggleGroup));
    }
    // ヘッダー集計行（公式ヘッダー相当）
    items.push(ListItem::new(
        Line::from(format!(
            "{awaiting} awaiting input · {working} working · {completed} completed"
        ))
        .style(Style::default().fg(ui().dim)),
    ));
    rows.push(None);
    // ここまでが固定ヘッダー。積んだ数をそのまま正本にする
    // （ヒットテストとスクロール計算が読む。定数と二重管理にしない）
    let header_n = rows.len();

    match app.grouping {
        Grouping::State => {
            for group in [
                Group::ReadyForReview,
                Group::NeedsInput,
                Group::Working,
                Group::Completed,
            ] {
                let members: Vec<&RowData> =
                    data.iter().filter(|d| d.group == group).collect();
                if members.is_empty() {
                    continue;
                }
                items.push(ListItem::new(Line::from("")));
                rows.push(None);
                items.push(ListItem::new(
                    Line::from(group.title()).style(Style::default().fg(ui().dim)),
                ));
                rows.push(None);
                for d in members {
                    push_data_row(&mut items, &mut rows, d);
                }
            }
        }
        Grouping::Directory => {
            // 見出しに出すフォルダと並びの決定は project_rows に閉じている。
            // 選択・stop・delete 等の操作では並び替えない
            let mut cwds: Vec<&str> = app.jobs.iter().map(|j| j.cwd.as_str()).collect();
            cwds.extend(data.iter().map(|d| d.cwd.as_str()));
            for (heading, cwd) in project_rows(&app.projects, &cwds) {
                items.push(ListItem::new(Line::from("")));
                rows.push(None);
                let cur = rows.len();
                let highlighted = hovered == Some(cur) || selected == cur;
                let mut style = Style::default().fg(ui().dim);
                if highlighted {
                    style = style.bg(ui().hl_bg).fg(ui().emph);
                }
                items.push(ListItem::new(Line::from(heading).style(style)));
                rows.push(Some(RowAction::Project(cwd.clone())));
                // 配下のセッション行。見出しの一覧と同じ同一判定を使う
                // （ここだけ厳密一致にすると大小違いのセッションが行き場を失う）
                for d in data.iter().filter(|d| same_dir(&d.cwd, &cwd)) {
                    push_data_row(&mut items, &mut rows, d);
                }
            }
        }
    }

    // 下部のフッター（区切り線 + アカウント行）を差し引いた行数が表示窓。
    // 溢れる分はスクロールで届く（ホイール / ↑↓ の選択追従）。
    // ジオメトリはクリック判定と同じ sidebar_layout を使う
    let sl = sidebar_layout(app);
    let capacity = sl.capacity;
    app.sidebar_rows = rows;
    app.sidebar_header_rows = header_n;
    // 行構成が変わって選択が浮いたら、最寄りのクリック可能行へ寄せる
    if app
        .sidebar_rows
        .get(app.selected_row)
        .map(|r| r.is_none())
        .unwrap_or(true)
    {
        app.selected_row = app
            .sidebar_rows
            .iter()
            .position(|r| r.is_some())
            .unwrap_or(0);
    }

    // ヘッダー行は固定表示。スクロールはその下（セッション一覧）にだけ効く。
    // ↑↓ 直後だけ選択行へ追従し、常に範囲内へクランプ
    let tail_capacity = capacity.saturating_sub(header_n);
    if app.sidebar_follow_sel {
        app.sidebar_follow_sel = false;
        if app.selected_row >= header_n {
            let sel_t = app.selected_row - header_n;
            if sel_t < app.sidebar_scroll {
                app.sidebar_scroll = sel_t;
            } else if tail_capacity > 0 && sel_t >= app.sidebar_scroll + tail_capacity {
                app.sidebar_scroll = sel_t + 1 - tail_capacity;
            }
        }
    }
    // items と rows は 1:1 で積むので items.len() >= header_n だが、
    // 引き算の前提を式の中で閉じておく（行の積み方を変えても破綻させない）
    app.sidebar_scroll = app
        .sidebar_scroll
        .min(items.len().saturating_sub(header_n + tail_capacity));

    // フォーカス中のペインだけ枠を少し明るく
    let focus_style = Style::default().fg(FOCUS_BORDER);
    let blur_style = Style::default().fg(ui().dim);
    let scroll = app.sidebar_scroll;
    let visible: Vec<ListItem> = items
        .into_iter()
        .enumerate()
        .filter(|(i, _)| {
            *i < header_n
                || (*i >= header_n + scroll && *i < header_n + scroll + tail_capacity)
        })
        .map(|(_, item)| item)
        .collect();
    let list = List::new(visible).block(Block::default().borders(Borders::ALL).border_style(
        if app.focus == Focus::Sidebar {
            focus_style
        } else {
            blur_style
        },
    ));
    frame.render_widget(list, chunks[0]);

    // ---- サイドバー下部フッター: 区切り線 / アカウント行 ----
    // claude の更新行はここには無い（上部の版行に集約した）
    if sl.footer_visible {
        let fx = chunks[0].x + 1;
        let fw = chunks[0].width - 2;
        let account_y = sl.account_y;
        // 区切り線（Desktop 風にフッターを本文から分ける）
        frame.render_widget(
            ratatui::widgets::Paragraph::new(
                Line::from("─".repeat(fw as usize)).style(Style::default().fg(ui().dim)),
            ),
            Rect::new(fx, account_y - 1, fw, 1),
        );
        // アカウント行（表示名 · 組織名）。文面の判断は account_row に閉じる。
        // **この行はクリックできる**（アカウントメニューの入口。当たり判定は
        // handle_mouse 側が同じ `sidebar_layout` の account_y で持つ）
        let (account, account_style) = account_row(&app.footer.account, active_unstored(app));
        frame.render_widget(
            ratatui::widgets::Paragraph::new(
                Line::from(clip_to_width(&account, fw)).style(account_style),
            ),
            Rect::new(fx, account_y, fw, 1),
        );
    }

    // コンテキストメニュー（モーダル）。矩形はクリック判定と同じ popup_rect を使う
    if let Some(popup) = &app.popup {
        let entries = popup.kind.entries(app.grouping);
        let area = popup_rect(app, popup);
        frame.render_widget(ratatui::widgets::Clear, area);
        let lines: Vec<ListItem> = entries
            .iter()
            .enumerate()
            .map(|(i, (label, enabled))| {
                let mut style = if *enabled {
                    Style::default()
                } else {
                    Style::default().fg(ui().dim)
                };
                if i == popup.selected {
                    style = style.bg(ui().hl_bg);
                    if *enabled {
                        style = style.fg(ui().emph);
                    }
                }
                ListItem::new(Line::from(format!(" {label}")).style(style))
            })
            .collect();
        frame.render_widget(
            List::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(ui().dim)),
            ),
            area,
        );
    }

    // 右ペイン: 新規セッション画面 or アクティブセッションの画面
    let terminal_focused = app.focus == Focus::Terminal;
    let starting = app.spawn_rx.is_some();
    if let RightView::New(state) = &mut app.right_view {
        return draw_new_view(frame, chunks[1], state, terminal_focused, starting);
    }
    if app.sessions.is_empty() {
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .title("no session")
                .border_style(Style::default().fg(ui().dim)),
            chunks[1],
        );
        return FrameCursor::hidden_at(pane_fallback_pos(chunks[1]));
    }
    let session = &app.sessions[app.active];
    let parser = session.parser.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let screen = parser.screen();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(session.name.clone())
        .border_style(if app.focus == Focus::Terminal {
            focus_style
        } else {
            blur_style
        });
    let inner = block.inner(chunks[1]);
    // tui-term 独自の █ カーソル描画は無効化し、ネイティブカーソル
    // （set_cursor_position = 本家と同じ点滅バー）だけを使う
    let widget = PseudoTerminal::new(screen)
        .cursor(tui_term::widget::Cursor::default().visibility(false))
        .block(block);
    frame.render_widget(widget, chunks[1]);

    // カーソル位置を反映。フォーカス外・子が非表示指定のときも「隠すだけ」で
    // 位置は必ず確定させる（描かないとサイドバーに置き去りになる。FrameCursor 参照）。
    // ペイン外へはみ出す座標はペイン内へクランプする
    let (crow, ccol) = screen.cursor_position();
    let pos = terminal_cursor_pos(chunks[1], inner, crow, ccol);
    if app.focus == Focus::Terminal && !screen.hide_cursor() {
        FrameCursor::shown_at(pos)
    } else {
        FrameCursor::hidden_at(pos)
    }
}

/// カーソルの安全な退避先。「見せるものが無い / クランプの前提が崩れた」経路は
/// すべてここへ寄せる（セッション 0 件・inner が潰れている・New 画面のフォームが
/// 収まらない）。位置を返さないと物理カーソルがサイドバーに置き去りになるので、
/// 何も指せないフレームでもペイン矩形の原点だけは必ず返す。
/// pane は Layout::split の結果なので、原点は常に端末の内側にある
pub(crate) fn pane_fallback_pos(pane: Rect) -> Position {
    Position::new(pane.x, pane.y)
}

/// ターミナルペインのカーソル位置。子（vt100）の (row, col) を pane 内の絶対座標へ移す。
/// Frame を必要としない純関数なので描画から切り離してテストできる。
///
/// inner が潰れている（幅または高さ 0）ときは width-1 クランプが枠の列＝inner の外を
/// 指し、端末幅を超える MoveTo にもなり得る。右ペインは Constraint::Min(1) なので
/// サイドバーを広げ切ると幅 1 = inner 幅 0 が起こり得るため、その場合は退避先を使う
fn terminal_cursor_pos(pane: Rect, inner: Rect, crow: u16, ccol: u16) -> Position {
    if inner.width == 0 || inner.height == 0 {
        return pane_fallback_pos(pane);
    }
    Position::new(
        inner.x + ccol.min(inner.width - 1),
        inner.y + crow.min(inner.height - 1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// pos が矩形の内側にあるか（幅・高さ 0 の矩形は「内側なし」なので常に false）
    fn contains(rect: Rect, pos: Position) -> bool {
        pos.x >= rect.x && pos.x < rect.right() && pos.y >= rect.y && pos.y < rect.bottom()
    }

    /// Borders::ALL の Block::inner と同じ 1px 縮小
    fn shrink(rect: Rect) -> Rect {
        Rect {
            x: rect.x + 1,
            y: rect.y + 1,
            width: rect.width.saturating_sub(2),
            height: rect.height.saturating_sub(2),
        }
    }

    /// 通常サイズでは子の (row, col) がそのまま inner 内の絶対座標へ移る
    #[test]
    fn terminal_cursor_maps_child_position_into_inner() {
        let pane = Rect::new(34, 0, 60, 20);
        let inner = shrink(pane);
        let pos = terminal_cursor_pos(pane, inner, 3, 7);
        assert_eq!(pos, Position::new(42, 4));
        assert!(contains(inner, pos));
    }

    /// inner をはみ出す座標は最終行・最終列へクランプされる
    #[test]
    fn terminal_cursor_clamps_out_of_range_child_position() {
        let pane = Rect::new(0, 0, 10, 5);
        let inner = shrink(pane);
        let pos = terminal_cursor_pos(pane, inner, 99, 99);
        assert_eq!(pos, Position::new(8, 3));
        assert!(contains(inner, pos));
    }

    /// 退避先はどんなペインでもその内側（幅・高さがある限り）。
    /// draw のセッション 0 件経路が返すのはこの位置そのもの
    #[test]
    fn pane_fallback_is_inside_pane() {
        for (x, y, w, h) in [(34u16, 0u16, 60u16, 20u16), (0, 0, 1, 1), (34, 5, 3, 12)] {
            let pane = Rect::new(x, y, w, h);
            let pos = pane_fallback_pos(pane);
            assert_eq!(pos, Position::new(x, y));
            assert!(contains(pane, pos), "pane {pane:?} で pos {pos:?} が外");
        }
    }

    /// 既定のサイドバー幅（34 桁）の内側。版行の幅の予算はこの桁数
    const DEFAULT_INNER: u16 = 32;

    /// 更新マーカーの表示幅は 1 桁。**この 1 という実測値に設計が乗っている**:
    /// 更新が無い行はスペース 1 個でマーカー桁を確保しており、2 桁なら桁がずれる。
    /// `☰` が 2 桁を占めるのと対照（unicode-width の判定は文字ごとに違う）
    #[test]
    fn the_update_marker_is_one_column_wide() {
        use unicode_width::UnicodeWidthStr;
        assert_eq!(UPDATE_MARK.width(), 1, "⟳ が 1 桁でない");
        assert_eq!("☰".width(), 2, "☰ は 2 桁（⟳ と同じ扱いにはできない）");
    }

    /// 更新の有無で行構成が変わらない（固定ヘッダー行数もマーカー桁の位置も動かない）。
    /// 版行 2 本 + 区切り線 1 本で必ず 3 行
    #[test]
    fn version_rows_keep_a_fixed_shape_whether_or_not_updates_exist() {
        for (ccdesk, claude) in [
            (UpdateState::Current, UpdateState::Current),
            (UpdateState::Available, UpdateState::Current),
            (UpdateState::Running, UpdateState::Available),
            (UpdateState::Restart, UpdateState::Running),
        ] {
            let rows = version_rows(ccdesk, "2.1.220", claude, DEFAULT_INNER);
            assert_eq!(rows.len(), 3, "版行 2 本 + 区切り線 1 本のはず");
            assert!(rows[0].0.contains(env!("CARGO_PKG_VERSION")), "{:?}", rows[0].0);
            assert!(rows[1].0.contains("claude v2.1.220"), "{:?}", rows[1].0);
            assert_eq!(rows[2].0, separator_text(DEFAULT_INNER));
            assert!(rows[2].2.is_none(), "区切り線はクリック不可");
        }
        // 版が未取得なら番号を出さない（誤情報を出さない）
        let rows = version_rows(UpdateState::Current, "", UpdateState::Current, DEFAULT_INNER);
        assert!(rows[1].0.contains("claude"), "{:?}", rows[1].0);
        assert!(!rows[1].0.contains(" v"), "版が無いのに v を出している: {:?}", rows[1].0);
    }

    /// 4 状態の文面。左端がマーカー桁、右端が動詞で、**新版の番号は出さない**
    #[test]
    fn version_row_spells_out_all_four_update_states() {
        let row = |state| version_row("ccdesk", "0.5.0", state, DEFAULT_INNER);
        // 最新: マーカー桁は空白、動詞なし
        let current = row(UpdateState::Current);
        assert_eq!(current, "  ccdesk v0.5.0");
        // 更新あり / 実行中 / 再起動待ち: ⟳ + 右端の動詞
        for (state, verb) in [
            (UpdateState::Available, "update"),
            (UpdateState::Running, "updating…"),
            (UpdateState::Restart, "restart"),
        ] {
            let text = row(state);
            assert!(text.starts_with(UPDATE_MARK), "{text:?}");
            assert!(text.ends_with(verb), "{text:?} が {verb:?} で終わっていない");
            assert!(text.contains("ccdesk v0.5.0"), "{text:?}");
        }
    }

    /// **最新のときもマーカー桁を確保する。** 更新が出た瞬間に名前が横へずれると、
    /// 行が変わったこと自体に気づきにくい
    #[test]
    fn version_row_keeps_the_name_column_fixed_across_states() {
        use unicode_width::UnicodeWidthStr;
        // 名前の前にある部分の表示幅（マーカー桁 + 区切りの空白）
        let name_col = |text: &str| {
            let at = text.find("ccdesk").expect("名前が無い");
            text[..at].width()
        };
        let base = name_col(&version_row("ccdesk", "0.5.0", UpdateState::Current, DEFAULT_INNER));
        assert_eq!(base, 2, "マーカー 1 桁 + 空白 1 桁のはず");
        for state in [
            UpdateState::Available,
            UpdateState::Running,
            UpdateState::Restart,
        ] {
            let text = version_row("ccdesk", "0.5.0", state, DEFAULT_INNER);
            assert_eq!(name_col(&text), base, "{state:?} で名前の桁がずれた: {text:?}");
        }
    }

    /// クリックできるのは「更新がある」行だけ。実行中・再起動待ちは押しても
    /// 意味が無いのでアクションを付けない（ハイライトも出さない）
    #[test]
    fn version_rows_are_clickable_only_when_an_update_is_available() {
        let actions = |ccdesk, claude| {
            let rows = version_rows(ccdesk, "2.1.220", claude, DEFAULT_INNER);
            (rows[0].2.clone(), rows[1].2.clone())
        };
        assert_eq!(
            actions(UpdateState::Available, UpdateState::Available),
            (Some(RowAction::UpdateCcdesk), Some(RowAction::UpdateClaude))
        );
        for state in [
            UpdateState::Current,
            UpdateState::Running,
            UpdateState::Restart,
        ] {
            assert_eq!(actions(state, state), (None, None), "{state:?}");
        }
    }

    /// 既定のサイドバー幅（34 桁 = 内側 32 桁）で切られない。
    /// 版番号は現実的な桁数まで（claude は 3 パート、ccdesk は本ビルドの版）
    #[test]
    fn version_rows_fit_the_default_sidebar_width() {
        use unicode_width::UnicodeWidthStr;
        for state in [
            UpdateState::Current,
            UpdateState::Available,
            UpdateState::Running,
            UpdateState::Restart,
        ] {
            for version in ["", "0.5.0", "2.1.220", "10.20.300"] {
                for name in ["ccdesk", "claude"] {
                    let text = version_row(name, version, state, DEFAULT_INNER);
                    assert!(
                        text.width() <= DEFAULT_INNER as usize,
                        "既定幅に収まらない: {text:?}（{} 桁 / 内側 {DEFAULT_INNER} 桁）",
                        text.width()
                    );
                }
            }
        }
    }

    /// 切り出しは表示幅で数える。アカウント名・組織名は任意の文字列なので、
    /// 文字数で切ると全角ぶんだけ枠を越える
    #[test]
    fn clip_to_width_counts_display_columns_not_characters() {
        use unicode_width::UnicodeWidthStr;
        // 全角 5 文字 = 10 桁。7 桁で切れば 3 文字（6 桁）まで
        let wide = "山田太郎田";
        assert_eq!(clip_to_width(wide, 7), "山田太");
        assert_eq!(clip_to_width(wide, 7).width(), 6);
        // 半角はそのまま桁数で切れる
        assert_eq!(clip_to_width("ooba · 1→10, Inc.", 6), "ooba ·");
        // 幅ぴったり・幅超過なしのときは全部残る
        assert_eq!(clip_to_width(wide, 10), wide);
        assert_eq!(clip_to_width(wide, 99), wide);
        assert_eq!(clip_to_width(wide, 0), "");
    }

    /// アカウント行に出るのは [`crate::accounts::Account`] の **ラベルだけ**。
    /// email は保管のキー（同一性）として持ち回すだけで、行には出さない。
    ///
    /// ジオメトリだけを見る他のフッターテストと違い、ここは実際に 1 フレーム
    /// 描いて中身を見る: 版行が上部へ移って 2 行固定になったフッターと、
    /// アカウントが `String` から `Account` になった変更が噛み合っていることは、
    /// 「その行に何が出たか」でしか固定できない。
    /// 供給元は [`DemoSource`] 既定の `App`（ファイルもネットワークも触らない）
    #[test]
    fn account_row_renders_the_label_without_the_email() {
        use crate::accounts::Account;
        use crate::poll::FooterInfo;

        let mut app = App {
            term_size: (120, 30),
            footer: FooterInfo {
                account: AccountStatus::LoggedIn(Account::new(
                    "you@example.com",
                    "you · Acme, Inc.",
                )),
                current: "2.1.220".to_string(),
                latest: None,
            },
            ..Default::default()
        };
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|frame| {
                draw(frame, &mut app);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row = |y: u16| -> String {
            (0..120).map(|x| buffer[(x, y)].symbol()).collect::<String>()
        };
        // 端末高さ 30 → サイドバー高さ 29（下部バー 1 行を除く）。y は画面座標
        let sl = sidebar_layout_of(29, 34);

        assert!(
            row(sl.account_y).contains("you · Acme, Inc."),
            "アカウント行にラベルが出ていない: {:?}",
            row(sl.account_y)
        );
        assert!(
            !row(sl.account_y).contains('@'),
            "email を行に出している: {:?}",
            row(sl.account_y)
        );
        assert!(
            row(sl.account_y - 1).contains('─'),
            "アカウント行の 1 つ上が区切り線になっていない: {:?}",
            row(sl.account_y - 1)
        );
    }

    /// フッターは「区切り線 + アカウント行」の 2 行に固定された
    /// （claude の更新行は上部の版行へ移したので、更新の有無で高さが変わらない）
    #[test]
    fn sidebar_footer_is_the_separator_and_the_account_row() {
        // 端末高さ 30 → サイドバー矩形の高さ 29（下部バー 1 行を除く）
        let sl = sidebar_layout_of(29, 34);
        assert!(sl.footer_visible);
        // 内側は 27 行（上下の枠を除く）。フッター 2 行を引いた 25 行が一覧の表示窓
        assert_eq!(sl.capacity, 25);
        // アカウント行は内側の最終行、その 1 つ上が区切り線
        assert_eq!(sl.account_y, 27);
        // 表示窓は上枠の次（y = 1）から始まるので、区切り線の 1 つ上までで尽きる
        assert_eq!(1 + sl.capacity, (sl.account_y - 1) as usize);
        // 狭い端末ではフッターを描かない = クリックも受けない
        for (h, w) in [(7u16, 34u16), (29, 4)] {
            assert!(!sidebar_layout_of(h, w).footer_visible, "h={h} w={w}");
        }
    }

    /// クリック判定はヘッダー先頭の版行に当たる。行 index は列を取らない
    /// = **行のどこを押しても同じ行**（マーカーの桁だけが当たり判定ではない）。
    /// 上枠とフッター帯は不感帯
    #[test]
    fn row_at_hits_the_version_rows_at_the_top_of_the_header() {
        let sl = sidebar_layout_of(29, 34);
        let header = version_rows(
            UpdateState::Available,
            "2.1.220",
            UpdateState::Available,
            DEFAULT_INNER,
        );
        // 版行はヘッダーの 0・1 行目（区切り線が 2 行目）
        assert_eq!(header[0].2, Some(RowAction::UpdateCcdesk));
        assert_eq!(header[1].2, Some(RowAction::UpdateClaude));
        // 画面 y=1 が ccdesk 行、y=2 が claude 行（スクロール位置に関係なく固定）
        for scroll in [0usize, 5, 99] {
            assert_eq!(row_at(1, sl.capacity, 7, scroll), 0);
            assert_eq!(row_at(2, sl.capacity, 7, scroll), 1);
        }
        // 上枠とフッター帯・下枠は不感帯
        assert_eq!(row_at(0, sl.capacity, 7, 0), usize::MAX);
        for y in [sl.account_y - 1, sl.account_y, sl.account_y + 1] {
            assert_eq!(row_at(y, sl.capacity, 7, 0), usize::MAX, "y={y}");
        }
    }

    /// 見出しの表示文字列だけを取り出す
    fn headings(projects: &[&str], cwds: &[&str]) -> Vec<String> {
        let projects: Vec<String> = projects.iter().map(|p| p.to_string()).collect();
        project_rows(&projects, cwds)
            .into_iter()
            .map(|(heading, _)| heading)
            .collect()
    }

    /// **この不具合の直接のリグレッションテスト。** 登録済みのフォルダは
    /// セッションが 0 本でも見出しが残る（＝そのフォルダで新規を開く入口が消えない）。
    /// 以前はセッションの cwd から一覧を導いていたので、最後のセッションが消えると
    /// 見出しごと消えていた
    #[test]
    fn a_registered_project_keeps_its_heading_with_zero_sessions() {
        assert_eq!(headings(&["C:\\dev\\api"], &[]), ["api"]);
        // セッションが 1 本も無くても登録の数だけ見出しが出る
        assert_eq!(
            headings(&["C:\\dev\\api", "C:\\dev\\web"], &[]),
            ["api", "web"]
        );
        // 登録が空でセッションも無ければ 1 行も出ない（空の見出しを作らない）
        assert!(headings(&[], &[]).is_empty());
    }

    /// 一覧は「登録リスト ∪ セッションの cwd」。未登録+セッションあり / 登録済み+0 本 /
    /// 両方（登録済みでセッションもある）の 3 通りが同じ 1 つの並びに出る
    #[test]
    fn project_rows_are_the_union_of_registrations_and_session_folders() {
        // 未登録だがセッションがあるフォルダは従来どおり出る
        assert_eq!(headings(&[], &["C:\\dev\\api"]), ["api"]);
        // 3 通りが混ざっても和集合。登録済みでセッションもあるフォルダは 1 度だけ
        assert_eq!(
            headings(
                &["C:\\dev\\empty", "C:\\dev\\both"],
                &["C:\\dev\\both", "C:\\dev\\unregistered"]
            ),
            ["both", "empty", "unregistered"]
        );
        // 大小・末尾の区切り違いは同じフォルダなので見出しは割れない
        assert_eq!(
            headings(&["C:\\dev\\api"], &["c:\\dev\\api\\", "C:\\DEV\\api"]),
            ["api"]
        );
        // 登録リスト自体が重複していても 1 度だけ（state.json を手で直された場合）
        assert_eq!(headings(&["C:\\dev\\api", "C:\\dev\\api"], &[]), ["api"]);
    }

    /// 並びは末端ディレクトリ名のアルファベット順・大小無視（従来仕様）。
    /// 登録とセッションのどちらの由来かで優先されない
    #[test]
    fn project_rows_sort_by_leaf_name_ignoring_case() {
        assert_eq!(
            headings(&["C:\\dev\\Zebra", "C:\\dev\\apple"], &["C:\\dev\\Mango"]),
            ["apple", "Mango", "Zebra"]
        );
        // 入力の順序を変えても同じ並び（＝走査順に依存しない）
        assert_eq!(
            headings(&["C:\\dev\\apple"], &["C:\\dev\\Mango", "C:\\dev\\Zebra"]),
            ["apple", "Mango", "Zebra"]
        );
    }

    /// **同名の末端ディレクトリが別パスに 2 つあっても混ざらない。** 見出しは同名で
    /// 並ぶが、対象フォルダはフルパスで区別され、並びはフルパスで一意に決まる
    /// （末端名だけをキーにすると入力順で入れ替わる）
    #[test]
    fn project_rows_keep_same_named_leaves_apart_by_full_path() {
        let projects = ["C:\\work\\api".to_string(), "C:\\dev\\api".to_string()];
        let rows = project_rows(&projects, &[]);
        assert_eq!(
            rows,
            [
                ("api".to_string(), "C:\\dev\\api".to_string()),
                ("api".to_string(), "C:\\work\\api".to_string()),
            ],
            "同名末端がフルパス順で並んでいない"
        );
        // 入力順を入れ替えても並びは同じ
        let flipped = ["C:\\dev\\api".to_string(), "C:\\work\\api".to_string()];
        assert_eq!(project_rows(&flipped, &[]), rows, "入力順で並びが変わった");
    }

    /// 見出しは末端ディレクトリ名だけ。**`+` は出さない**（行の動作はメニューを開くことで、
    /// 「押したら即セッションが立つ」というヒントは嘘になる）
    #[test]
    fn project_headings_carry_no_plus_hint() {
        let rows = project_rows(&["C:\\dev\\api".to_string()], &["C:\\dev\\web"]);
        for (heading, cwd) in &rows {
            assert!(!heading.contains('+'), "見出しに + が残っている: {heading:?}");
            assert_eq!(heading.trim(), heading, "見出しに余分な空白がある: {heading:?}");
            assert!(cwd.starts_with("C:\\"), "対象がフルパスでない: {cwd:?}");
        }
        assert_eq!(rows[0], ("api".to_string(), "C:\\dev\\api".to_string()));
    }

    /// 末端ディレクトリ名が取れないパス（ドライブ直下）でも見出しを落とさない。
    /// 登録は自動なので、ドライブ直下でセッションを作れば一覧に入り得る
    #[test]
    fn project_rows_fall_back_to_the_path_when_there_is_no_leaf() {
        assert_eq!(
            project_rows(&["C:\\".to_string()], &[]),
            [("C:\\".to_string(), "C:\\".to_string())]
        );
    }

    /// サイドバーを実際に描いて (行データ, その行の表示文字列) を返す。
    /// **描画を経由するのが要点**で、`project_rows` の結果が本当に画面と
    /// クリック判定へ届いているか（登録リストが draw に配線されているか）を見る
    fn render_sidebar(app: &mut App) -> Vec<(Option<RowAction>, String)> {
        let (w, h) = app.term_size;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).expect("端末が作れない");
        terminal.draw(|frame| {
            draw(frame, app);
        })
        .expect("描画に失敗");
        let buffer = terminal.backend().buffer().clone();
        // 固定ヘッダーの下はスクロール分ずれるが、このテストは行数が窓に収まる
        // 前提なので scroll = 0（描画側がクランプ済み）
        assert_eq!(app.sidebar_scroll, 0, "テストの前提（スクロール無し）が崩れている");
        app.sidebar_rows
            .iter()
            .enumerate()
            .map(|(idx, action)| {
                let y = idx as u16 + 1; // 上枠の次の行から積まれる
                let text: String = (1..app.sidebar_width.saturating_sub(1))
                    .map(|x| buffer[(x, y)].symbol())
                    .collect();
                (action.clone(), text.trim_end().to_string())
            })
            .collect()
    }

    /// **登録リストが画面に届いていることの検証（配線のテスト）。** セッションを
    /// 1 本も持たない登録フォルダの見出しが描かれ、その行はメニューを開く
    /// [`RowAction::Project`] を持ち、**`+` は付かない**
    #[test]
    fn the_directory_grouping_draws_registered_projects_without_sessions() {
        let mut app = App {
            term_size: (60, 40),
            sidebar_width: 34,
            grouping: Grouping::Directory,
            projects: vec!["C:\\dev\\empty-project".to_string()],
            ..Default::default()
        };
        let rows = render_sidebar(&mut app);
        let heading = rows
            .iter()
            .find(|(action, _)| {
                matches!(action, Some(RowAction::Project(cwd)) if cwd == "C:\\dev\\empty-project")
            })
            .expect("登録フォルダの見出しが描かれていない");
        assert_eq!(heading.1, "empty-project", "見出しの文字列が末端名だけでない");
        assert!(!heading.1.contains('+'), "見出しに + が残っている: {:?}", heading.1);
        // **見出しだけを出す**（「no sessions」等の説明行を挟まない）。セッションが
        // 0 本なので、この見出しがサイドバー最後の行になる
        let idx = rows.iter().position(|(_, t)| t == "empty-project").unwrap();
        assert_eq!(
            idx + 1,
            rows.len(),
            "見出しの下に行が積まれている: {:?}",
            &rows[idx + 1..]
        );
        assert!(
            !rows.iter().any(|(_, t)| t.contains("no session")),
            "セッション 0 本の説明行が出ている"
        );
    }

    /// 撮影用データを directory グルーピングで描いたときの見出しの並び。
    /// **`--demo` の Ctrl+S で出る画面そのもの**を固定する: セッションを持たない
    /// 登録フォルダ（infra）の見出しが残り、`+` は付かず、見出し行はメニューを開く
    #[test]
    fn the_demo_data_shows_every_project_heading_in_the_directory_grouping() {
        use crate::source::{DataSource, DemoSource};
        let mut app = App {
            term_size: (60, 40),
            sidebar_width: 34,
            grouping: Grouping::Directory,
            jobs: DemoSource.jobs(),
            projects: DemoSource.window_state().projects,
            ..Default::default()
        };
        let rows = render_sidebar(&mut app);
        let headings: Vec<&str> = rows
            .iter()
            .filter(|(action, _)| matches!(action, Some(RowAction::Project(_))))
            .map(|(_, text)| text.as_str())
            .collect();
        assert_eq!(
            headings,
            ["api", "docs", "infra", "shop-app"],
            "撮影用データの見出しの並びが変わった"
        );
        // infra は demo セッションを持たないフォルダ。見出しの直後は空行 or 次の見出しで、
        // セッション行は付かない（＝ 0 本でも見出しだけが残る）
        let infra = rows.iter().position(|(_, t)| t == "infra").unwrap();
        assert_eq!(rows[infra + 1].1, "", "infra の下にセッション行が出ている");
        assert!(
            matches!(&rows[infra + 2].0, Some(RowAction::Project(cwd)) if cwd.ends_with("shop-app")),
            "空のフォルダの次が別の見出しになっていない"
        );
    }

    /// `+ new session` 行は従来どおり残る（見出しから消した `+` と混同しない）
    #[test]
    fn the_new_session_row_keeps_its_plus() {
        let mut app = App {
            term_size: (60, 40),
            sidebar_width: 34,
            grouping: Grouping::Directory,
            projects: vec!["C:\\dev\\api".to_string()],
            ..Default::default()
        };
        let rows = render_sidebar(&mut app);
        assert!(
            rows.iter()
                .any(|(action, text)| action == &Some(RowAction::New) && text == "+ new session"),
            "+ new session 行が消えている"
        );
    }

    /// inner が潰れてもペイン矩形の外（枠の列や端末外）へ出ない。
    /// 右ペインは Constraint::Min(1) なので幅 1 = inner 幅 0 が起こり得る
    #[test]
    fn terminal_cursor_stays_in_pane_for_degenerate_inner() {
        for (w, h) in [(1u16, 20u16), (2, 20), (3, 20), (20, 1), (20, 2), (1, 1)] {
            let pane = Rect::new(34, 0, w, h);
            let inner = shrink(pane);
            let pos = terminal_cursor_pos(pane, inner, 5, 5);
            assert!(
                contains(pane, pos),
                "pane {pane:?} / inner {inner:?} で pos {pos:?} がペイン外"
            );
        }
    }

    /// 未保管警告 `⚠` の表示幅は **1 桁**。既定幅（内側 32 桁）にアカウント行を
    /// 収める前提がこの実測値に乗っているので固定する（`⟳` も 1 桁だが `☰` は
    /// 2 桁 ＝ 判定は文字ごとに違うので実測しないと分からない）
    #[test]
    fn the_warning_mark_is_one_column_wide() {
        use unicode_width::UnicodeWidthStr;
        assert_eq!(WARN_MARK.width(), 1, "⚠ が 1 桁でない");
        assert_eq!(
            WARN_MARK.chars().count(),
            1,
            "異体字セレクタが混ざっている（絵文字表示になると 2 桁になる）"
        );
    }

    /// テスト内でアカウント行の文面だけを見るための短縮
    fn row_text(status: &AccountStatus, unstored: bool) -> String {
        account_row(status, unstored).0
    }

    /// **アクティブなアカウントが未保管のときだけ `⚠` を前置する。**
    /// 保管済みなら付けない（常時出ていると警告の意味が無くなる）
    #[test]
    fn account_row_marks_only_an_unstored_active_account() {
        use crate::accounts::Account;
        let logged_in = AccountStatus::LoggedIn(Account::new("you@example.com", "you · Acme, Inc."));

        assert_eq!(row_text(&logged_in, true), "⚠ you · Acme, Inc.");
        assert_eq!(
            row_text(&logged_in, false),
            "you · Acme, Inc.",
            "保管済みなのに警告が出ている"
        );
        // 未取得は空行のまま（誤情報を出さない）。未ログインは行そのものが警告なので
        // ⚠ は前置しない ＝ ⚠ は「未保管」だけを意味する
        assert_eq!(row_text(&AccountStatus::Unknown, true), "");
        assert_eq!(row_text(&AccountStatus::LoggedOut, true), LOGGED_OUT_ROW);
        assert!(!LOGGED_OUT_ROW.contains(WARN_MARK));
    }

    /// 未ログインの行は **再ログインの手順まで出す**。保管トークンの期限切れも
    /// この状態で現れる（事前検知はしない方針なので、ここが唯一の気づきどころ）
    #[test]
    fn account_row_prompts_a_login_when_logged_out() {
        let (text, style) = account_row(&AccountStatus::LoggedOut, false);
        assert!(text.contains("not logged in"), "{text:?}");
        assert!(text.contains("/login"), "再ログインの手順が無い: {text:?}");
        assert_eq!(
            style,
            Style::default().fg(C_ATTENTION),
            "要対応の行が注意色になっていない"
        );
    }

    /// 既定のサイドバー幅（34 桁 = 内側 32 桁）でアカウント行が切られない。
    /// `⚠ ` の 2 桁ぶんが増えても、現実的なラベルなら収まることの固定
    #[test]
    fn account_row_fits_the_default_sidebar_width() {
        use crate::accounts::Account;
        use unicode_width::UnicodeWidthStr;
        // README・撮影データに出る実寸のラベルと、全角を含むラベル
        for label in ["ooba · 1→10, Inc.", "you · Acme, Inc.", "大場 · 1→10, Inc."] {
            let status = AccountStatus::LoggedIn(Account::new("you@example.com", label));
            for unstored in [false, true] {
                let text = row_text(&status, unstored);
                assert_eq!(
                    clip_to_width(&text, DEFAULT_INNER),
                    text,
                    "既定幅で切られる: {text:?}（{} 桁 / 内側 {DEFAULT_INNER} 桁）",
                    text.width()
                );
            }
        }
        // 未ログインの案内も切ってはいけない（打つ手が読めなくなる）
        assert_eq!(
            clip_to_width(LOGGED_OUT_ROW, DEFAULT_INNER),
            LOGGED_OUT_ROW,
            "{} 桁 / 内側 {DEFAULT_INNER} 桁",
            LOGGED_OUT_ROW.width()
        );
    }

    /// 実際に 1 フレーム描いた結果でも `⚠` の出方が変わる。判定は
    /// [`active_unstored`]（アクティブな email が保管の写しに居るか）なので、
    /// 保管に加えた瞬間に消えることまで含めて固定する
    #[test]
    fn the_drawn_account_row_warns_until_the_active_account_is_stored() {
        use crate::accounts::Account;
        use crate::poll::FooterInfo;

        let active = Account::new("you@example.com", "you · Acme, Inc.");
        let drawn = |accounts: Vec<Account>| -> String {
            let mut app = App {
                term_size: (120, 30),
                footer: FooterInfo {
                    account: AccountStatus::LoggedIn(active.clone()),
                    current: "2.1.220".to_string(),
                    latest: None,
                },
                accounts,
                ..Default::default()
            };
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
            terminal
                .draw(|frame| {
                    draw(frame, &mut app);
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let y = sidebar_layout_of(29, 34).account_y;
            (0..120).map(|x| buffer[(x, y)].symbol()).collect()
        };

        let unstored = drawn(Vec::new());
        assert!(
            unstored.contains(WARN_MARK) && unstored.contains("you · Acme, Inc."),
            "未保管の警告が出ていない: {unstored:?}"
        );
        // 別アカウントだけが保管されていても、アクティブな 1 件が未保管なら警告する
        let other = drawn(vec![Account::new("other@example.com", "other")]);
        assert!(other.contains(WARN_MARK), "別 email の保管で消えている: {other:?}");
        // アクティブなアカウントを保管したら消える
        let stored = drawn(vec![active.clone()]);
        assert!(
            !stored.contains(WARN_MARK) && stored.contains("you · Acme, Inc."),
            "保管済みなのに警告が残っている: {stored:?}"
        );
    }
}
