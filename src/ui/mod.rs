//! サイドバー・右ペインの描画と、描画／クリック判定で共有するジオメトリ計算。
pub(crate) mod new_view;
pub(crate) mod text_field;

use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem};
use ratatui::Frame;
use std::time::Duration;
use tui_term::widget::PseudoTerminal;

use ccdesk::dir_key;

use crate::app::{
    active_unstored, selected_enter, App, Focus, Popup, RightView, RowAction, SelfUpdate,
    SidebarPos, SidebarRow,
};
use crate::poll::{
    classify, foreground_state, AccountStatus, Bucket, Group, Grouping, StateView,
};
use crate::session::SessionStatus;
use crate::sessions::SessionId;
use crate::theme::{
    ui, usage_color, C_ATTENTION, C_FAIL, C_WORKING, FOCUS_BORDER, MUTED_FG,
};
use crate::ui::new_view::draw_new_view;

/// 未読マーカー（行頭のメニュー記号の右）。**既読も同じ幅を取る**ので、
/// 未読が付いたり消えたりしても名前の開始桁が動かない。
///
/// **状態ラベルの前ではなく行頭に置く**のが判断: 未読は「この行に新しいことが
/// あるか」の印なので、行を縦に流し読みするときに 1 つの桁に揃っている方が拾える
/// （状態ラベルの前だと名前の長さで印の位置が毎行変わる）
const UNREAD_MARK: &str = "● ";
const READ_MARK: &str = "  ";

fn unread_mark(unread: bool) -> &'static str {
    if unread {
        UNREAD_MARK
    } else {
        READ_MARK
    }
}

/// 1 行に出す状態を決める。**hook が主、`agents --json` が従**
/// （`docs/foreground-migration.md` のフェーズ3）。
///
/// - 死んでいる行 ＝ 最後に観測した state のまま（空 ＝ 一度も観測していない行は
///   動いていない）。生きている行の判断材料は使わない
/// - 生きている行は hook（[`crate::hooks`]）が答える。turn 単位で届くので
///   Working / Needs input / Done を取り違えない
/// - ただし**生きている行の `stopped` は捨てる**: 生死の真実は PTY の
///   `try_wait`（[`crate::session::Session::alive`]）で、再開した直後は前回の
///   `SessionEnd` が保管に残っている ＝ 次の `SessionStart` が届くまでの
///   数秒だけ「動いているのに Stopped」に見えてしまう
/// - hook が一度も来ていない行だけ `agents --json` の `status` へ落ちる
///   （ccdesk が起こしていないセッション・注入が効かなかった場合）
/// - `status` も無い（ポーラーが拾う前）間は出力の変化から推す
fn row_state(
    alive: bool,
    last_state: &str,
    hook: Option<&str>,
    status: &str,
    heuristic: Option<SessionStatus>,
) -> StateView {
    if !alive {
        let state = if last_state.is_empty() { "stopped" } else { last_state };
        return classify(state, false);
    }
    let hook = hook.filter(|state| *state != "stopped");
    match (hook, status, heuristic) {
        (Some(state), _, _) => classify(state, true),
        (None, "", Some(SessionStatus::Working)) => classify("working", true),
        (None, "", _) => classify("blocked", true),
        (None, status, _) => classify(foreground_state(status), true),
    }
}

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

/// 行 index → サイドバー内の画面 y。[`row_at`] の逆で、**同じ規則を 2 つ持たない**
/// ために対で置く（枠の 1 行を足し、固定ヘッダーより下はスクロールぶんを引く）。
///
/// 使い手は「行から生えるメニューの位置」（[`crate::app`]）と
/// 「名前を編集中の行のカーソル位置」の 2 つ。表示窓に入っているかは
/// [`row_visible`] が別に答える（y だけでは見えているか分からない）
pub(crate) fn row_y(row: usize, header_rows: usize, scroll: usize) -> u16 {
    let r = if row < header_rows {
        row
    } else {
        row.saturating_sub(scroll)
    };
    (r as u16).saturating_add(1)
}

/// 行 index が今の表示窓に入っているか。固定ヘッダーは常に見え、その下は
/// スクロール位置から `tail_capacity` 行ぶんだけが見える。
/// **描画の絞り込みとカーソルの判断が同じ式を見る**ための共有
fn row_visible(row: usize, header_rows: usize, scroll: usize, tail_capacity: usize) -> bool {
    row < header_rows
        || (row >= header_rows + scroll && row < header_rows + scroll + tail_capacity)
}

/// サイドバーを横断する区切り線のテキスト（枠の内側幅ぶん）
fn separator_text(inner_width: u16) -> String {
    "─".repeat(inner_width as usize)
}

/// 行頭のメニュー記号（クリックで二次操作のメニューが開く）。
///
/// **ASCII を選んだのは桁の曖昧さを消すため。** 以前使っていたハンバーガー記号
/// （U+2630）は East Asian Ambiguous ＝ 幅の判定が端末とフォント設定で
/// 1 桁にも 2 桁にもなる。
/// ccdesk は 2 桁と実測して桁を数えていたので、1 桁と解釈する端末では
/// **行全体が横へずれる**。`=` なら常に 1 桁で、前提そのものが消える
const MENU_MARK: &str = "=";

/// 名前の変更中の行の先頭。**通常の行と同じ「記号 + 空白」から始める**ので、
/// 編集に入っても名前の桁が横へ動かない（[`MENU_MARK`] と対であることと
/// 表示幅 2 桁であることはテストで固定する ＝ カーソル位置がこれに乗っている）
const RENAME_PREFIX: &str = "= ";

/// 更新マーカー。**表示幅は実測 1 桁**（U+27F3 / unicode-width 0.2.2 で `1`）。
/// 1 桁だと分かっているので、更新が無い行はスペース 1 個で同じ桁を確保できる。
/// 記号の幅は文字ごとに違い、しかも曖昧なものがある（[`MENU_MARK`] の判断）ので、
/// 桁の前提に乗せる記号は実測してテストで固定する
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

    /// 押して更新を始められるか（＝行に動作を付けるか）。実行中と再起動待ちは
    /// もう押す意味が無いので付けない。**それでも行は行**なので、選択・ホバーの
    /// 対象からは外れない（[`SidebarRow::Inert`]）
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
/// Frame に触らない純関数なので、4 状態の文面と当たり判定をテストで固定できる。
///
/// **更新が無い版行は [`SidebarRow::Inert`]**（押しても何も起きないが行の実体はある）。
/// 飾りは区切り線だけ ＝ 版行は更新の有無に関係なく選択・ホバーできる
fn version_rows(
    ccdesk: UpdateState,
    claude_version: &str,
    claude: UpdateState,
    inner_width: u16,
) -> Vec<(String, Style, SidebarRow)> {
    let row = |state: UpdateState, action: RowAction| {
        if state.actionable() {
            SidebarRow::Action(action)
        } else {
            SidebarRow::Inert
        }
    };
    vec![
        (
            version_row("ccdesk", env!("CARGO_PKG_VERSION"), ccdesk, inner_width),
            ccdesk.style(),
            row(ccdesk, RowAction::UpdateCcdesk),
        ),
        (
            version_row("claude", claude_version, claude, inner_width),
            claude.style(),
            row(claude, RowAction::UpdateClaude),
        ),
        (
            separator_text(inner_width),
            Style::default().fg(ui().dim),
            SidebarRow::Decoration,
        ),
    ]
}

/// directory グルーピングの見出し行。返すのは [`ProjectRow`]（表示文字列・対象フォルダ・
/// 配下の行を振り分けるための同一判定キー）。
///
/// 一覧は「登録リスト ∪ セッションの cwd」の**和集合**。登録リスト側があるので
/// セッションが 0 本になっても見出しは消えず、そのフォルダで新規を開く入口が残る
/// （以前はセッションの cwd から導出するだけだったので、最後のセッションが消えると
/// 見出しごと消えていた）。未登録でセッションだけあるフォルダも従来どおり出す。
///
/// 並びは末端ディレクトリ名のアルファベット順（大小無視）で従来どおり。**同名の末端が
/// 別パスに複数あるときはフルパスで決める**: キーが末端名だけだと安定ソートの入力順
/// （＝セッションの走査順）で並びが変わり、同じ画面が再描画で入れ替わり得る
fn project_rows(projects: &[String], session_cwds: &[&str]) -> Vec<ProjectRow> {
    // **派生値はパスごとに 1 度だけ作る。** 重複排除も並べ替えも照合の回数は入力数の
    // 2 乗で増えるので、比較のたびにキーを作ると描画 1 回で数千〜数万の String に
    // なる（この一覧は PTY の dirty ごと ＝ 秒 30 回まで描き直される）
    let mut dirs: Vec<ProjectRow> = projects
        .iter()
        .map(String::as_str)
        .chain(session_cwds.iter().copied())
        .map(ProjectRow::new)
        .collect();
    // 同じフォルダは 1 度だけ。キーで束ねてから畳むので、**残るのは先に出てきた表記**
    // ＝ 登録リストを先に積んである以上ユーザーが登録した表記が勝つ（sort_by は安定）。
    // state.json を手で直された場合の自己重複もここで落ちる
    dirs.sort_by(|a, b| a.key.cmp(&b.key));
    dirs.dedup_by(|a, b| a.key == b.key);
    dirs.sort_by(|a, b| a.sort_key.cmp(&b.sort_key).then_with(|| a.cwd.cmp(&b.cwd)));
    dirs
}

/// directory グルーピングの見出し 1 行分。**パスから作る派生値をここにまとめて持つ**
/// のが要点で、作るのはパスごとに 1 度きり（[`project_rows`] 参照）
struct ProjectRow {
    /// 見出しの表示文字列そのもの（見出しに何を出すかの知識をここだけに持つ）
    heading: String,
    /// 見出しの対象フォルダ。表記は登録リスト / セッションの cwd のまま
    cwd: String,
    /// 同一判定キー（[`dir_key`]）。**重複排除と、配下のセッション行の振り分けが
    /// 同じこの値を使う**（別々に作ると判定がずれるうえ回数も倍になる）
    key: String,
    /// 並べ替えキー（末端ディレクトリ名の小文字）。比較子の中で作ると比較のたびに
    /// アロケートするので、ここに持たせて比較は借用で済ませる
    sort_key: String,
}

impl ProjectRow {
    fn new(cwd: &str) -> Self {
        // 末端ディレクトリ名は表示と並べ替えの共通の材料なので 1 度だけ取り出す。
        // 取れないのはドライブ直下（`C:\`）等
        let leaf = leaf_name(cwd);
        // 末端が取れないパスの並べ替えキーは空（見出しに出すパスとは別扱い ＝
        // 従来の並びを変えない）
        let sort_key = leaf.as_deref().map(str::to_lowercase).unwrap_or_default();
        Self {
            // 見出しはプロジェクト名（末端ディレクトリ名）だけ。**ここに `+` は出さない**:
            // 行の動作はメニューを開くことなので、「押したら即セッションが立つ」という
            // ヒントは嘘になる。末端が取れないときはパスをそのまま出す
            // （ホーム短縮が効く形にはならない）
            heading: leaf.unwrap_or_else(|| cwd.to_string()),
            cwd: cwd.to_string(),
            key: dir_key_of(cwd),
            sort_key,
        }
    }
}

/// 末端ディレクトリ名。表示名と並べ替えキーの共通の材料
fn leaf_name(cwd: &str) -> Option<String> {
    #[cfg(test)]
    count_key_call(&LEAF_NAME_CALLS);
    std::path::Path::new(cwd)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
}

/// 同一判定キーを作る唯一の入口（中身は [`dir_key`] ＝ 判定は lib 側 1 箇所のまま）。
/// **テスト時だけ呼び出し回数を数える**: 「パスごとに 1 度」が崩れると照合が
/// 入力数の 2 乗でアロケートし始めるので、回数そのものをテストで固定する
fn dir_key_of(cwd: &str) -> String {
    #[cfg(test)]
    count_key_call(&DIR_KEY_CALLS);
    dir_key(cwd)
}

// キー作りの呼び出し回数カウンタ。スレッドローカルなのでテストの並列実行で混ざらない
#[cfg(test)]
thread_local! {
    static DIR_KEY_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static LEAF_NAME_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn count_key_call(counter: &'static std::thread::LocalKey<std::cell::Cell<usize>>) {
    counter.with(|c| c.set(c.get() + 1));
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
/// 桁数をテストで固定できる。`unstored` は [`active_unstored`] の判定。
///
/// `pending` は進行中のアカウント操作の語（[`crate::app::AccountJob`]）。
/// **進行中は他の何よりこれを出す**: 操作は別スレッドで走り最大 11 秒かかりうるので、
/// 何も出さないと「押したのに変わらない行」に見える（版行が `updating…` を出すのと
/// 同じ方針で、語彙は要求の側が持つ）。進行中の値は「今の持ち主」ではないので、
/// `⚠` も dim も付けない（判断材料が確定していない間の見た目を作り分けない）
fn account_row(status: &AccountStatus, unstored: bool, pending: Option<&str>) -> (String, Style) {
    if let Some(progress) = pending {
        return (progress.to_string(), Style::default().fg(ui().dim));
    }
    match status {
        // 未保管のときは `⚠` を前置し、色も dim から注意色へ上げる。dim のままだと
        // 登録し忘れに気づけず、次の /login で前のアカウントの認証情報が
        // 上書きされて失われる（`.credentials.json` は常に 1 アカウント分だけ）
        AccountStatus::LoggedIn(active) if unstored => (
            format!("{WARN_MARK} {}", active.account.label),
            Style::default().fg(C_ATTENTION),
        ),
        // 出すのはラベルだけ（email は同一性の保持用で、行には出さない）
        AccountStatus::LoggedIn(active) => {
            (active.account.label.clone(), Style::default().fg(ui().dim))
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
/// 被せて全部読ませる方を選んだ。**この意図は描画順に依存する**（[`draw`] が
/// 右ペインの後にメニューを描く）: 逆順にすると被った列が右ペインに塗り潰され、
/// クリック判定だけがここの矩形に残る ＝ 見えない場所のクリックが効く不具合になる。
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

/// 下部バーの文脈セクション ＝ (見出し, 打鍵の案内)。
/// **今この瞬間に打鍵が届く先で効くキーだけ**を出す（効かないキーを出すと嘘になる）。
///
/// 判断の順序は run ループのキー配りと同じ（フォーカス → 名前の入力 → メニュー →
/// 一覧）。順序が別々だと、案内と実際の受け手がずれる。
///
/// `None` は新規セッション画面だけ ＝ 案内をペイン内に持つので下部バーへ重ねない
fn context_hint(app: &App) -> Option<(&'static str, String)> {
    if app.focus == Focus::Terminal {
        if matches!(app.right_view, RightView::New(_)) {
            return None;
        }
        // ccdesk が取るのは予約キーだけ。残りは全部 claude が受ける
        return Some(("terminal", "all keys pass through to claude".to_string()));
    }
    if app.rename.is_some() {
        // 名前の入力中は入力の作法だけ（一覧の操作は全部この入力へ渡っている）
        return Some(("rename", "Enter rename · Esc cancel".to_string()));
    }
    if app.popup.is_some() {
        // メニューは開いている間すべてのキーを飲む ＝ 一覧のキーは出さない
        return Some(("popup", "↑↓ select · Enter run · Esc close".to_string()));
    }
    Some(("sidebar", sidebar_hint(app)))
}

/// サイドバーの案内。**選択行で本当に効くキーだけ**を並べる。
///
/// `↑↓` はどの行でも効くが、`Enter` が何をするかは行の種類で違う
/// （メニュー / 新規セッション / 更新 / 何もしない）。その語は
/// [`crate::app::Enter::label`] ＝ **動作の名前の正本**から取るので、
/// 「行の種類 → 案内」の対応表をここに持たない: 種類を足したときに
/// 案内だけが黙って古くなることが起きない。
///
/// 押しても何も起きない行（更新の無い版行）では `Enter` を出さない
fn sidebar_hint(app: &App) -> String {
    match selected_enter(app) {
        Some(enter) => format!("↑↓ select · Enter {}", enter.label()),
        None => "↑↓ select".to_string(),
    }
}

/// 最下行の横断バー。**通知があれば数秒それを出し、無ければキーヒント**を出す。
///
/// **呼ぶのはサイドバーを積んだ後**（[`draw`] の並び）: 案内は選択行の種類で
/// 変わり、その選択行は [`App::sidebar_rows`] を組んだ結果に依るので、
/// 先に描くと 1 フレーム古い行の案内が出る
fn draw_bottom_bar(frame: &mut Frame, area: Rect, app: &mut App) {
    // 下部バー: 通知（起動失敗等）があれば数秒それを出し、無ければキーヒント
    if let Some((msg, at)) = &app.notice {
        if at.elapsed() < Duration::from_secs(5) {
            frame.render_widget(
                ratatui::widgets::Paragraph::new(Line::from(format!(" {msg}")))
                    .style(Style::default().fg(C_FAIL)),
                area,
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
        if let Some((label, keys)) = context_hint(app) {
            hint_spans.push(Span::styled(
                format!("  {label}:"),
                Style::default().fg(MUTED_FG),
            ));
            hint_spans.push(Span::raw(format!(" {keys}")));
        }
        // 起こした子がまだ端末を掴んでいないことを出す。**見出しメニューの
        // new session は右ペインの表示を変えない**ので、ここに出さないと無反応に見える。
        // 判定は `input_gate` 1 つ（起動中かどうかの正本を増やさない）。
        // New 画面は入力欄に自前の starting 表示を持つので、そこでは二重に出さない
        if app.input_gate.is_some() && !matches!(app.right_view, RightView::New(_)) {
            hint_spans.push(Span::styled(
                "  starting session…",
                Style::default().fg(C_WORKING),
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
            .split(area);
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

    // サイドバー: **行の正本は `~/.ccdesk/sessions.json`**（`app.sessions`）。
    // 生死は自分の子プロセス（`child.try_wait()`）が、生きている行のライブ状態は
    // `claude agents --json` の `status` が答える
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    // 窓ごとの観測を**先に**確定させる（生死と出力ヒューリスティックは可変借用が要る）。
    // 以降は行の一覧を不変で回せるので、行の組み立ては 1 本のループで済む
    struct WindowView {
        session_id: crate::sessions::SessionId,
        alive: bool,
        heuristic: SessionStatus,
        idle_secs: u64,
    }
    let windows: Vec<WindowView> = app
        .windows
        .iter_mut()
        .map(|w| WindowView {
            session_id: w.session_id.clone(),
            alive: w.alive(),
            heuristic: w.status_heuristic(),
            idle_secs: w
                .last_output
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .elapsed()
                .as_secs(),
        })
        .collect();
    let active = app.active;

    // ---- 行データを先に組み立てる（State / Directory 両グルーピング対応）----
    struct RowData {
        action: RowAction,
        group: Group,
        cwd: String,
        glyph: &'static str,
        color: Color,
        label: String,
        is_active_window: bool,
        /// 未読（[`crate::sessions::SessionRow::unread`]）＝ 行頭のメニュー記号の右に `●`
        unread: bool,
        status_label: &'static str,
        age: String,
        bucket: Bucket,
        /// ピン留め（グループ内で先頭へ寄せる）
        pinned: bool,
        /// 名前の変更中なら入力中の文字列。**行そのものが入力欄に化ける**
        /// （[`crate::app::Rename`]）ので、ここが Some の行は通常の描画をしない
        editing: Option<String>,
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
    for row in &app.sessions {
        let window = windows
            .iter()
            .enumerate()
            .find(|(_, w)| w.session_id == row.session_id);
        let alive = window.is_some_and(|(_, w)| w.alive);
        // 生きている行のライブ状態は `agents --json` の interactive エントリが答える
        let status = app
            .agents
            .iter()
            .find(|a| a.is_interactive() && a.session_id == row.session_id.as_str())
            .map(|a| a.status.as_str())
            .unwrap_or_default();
        let view = row_state(
            alive,
            &row.last_state,
            app.hook_states.get(&row.session_id),
            status,
            window.map(|(_, w)| w.heuristic),
        );
        // 経過時間: 動いている窓は最後の出力から、止まっている行は最後の更新から
        let age_secs = match window {
            Some((_, w)) if alive => w.idle_secs,
            _ => now_ms.saturating_sub(row.updated_at) / 1000,
        };
        data.push(RowData {
            action: RowAction::Open(row.session_id.clone()),
            group: view.group,
            cwd: row.cwd.clone(),
            glyph: glyph_of(&view),
            color: view.color,
            label: row.title.clone(),
            is_active_window: window.is_some_and(|(i, _)| i == active)
                && matches!(app.right_view, RightView::Sessions),
            unread: row.unread(),
            status_label: view.label,
            age: fmt_age(age_secs),
            bucket: view.bucket,
            pinned: row.pinned,
            editing: app
                .rename
                .as_ref()
                .filter(|r| r.id == row.session_id)
                .map(|r| r.field.text.clone()),
        });
    }
    // ヘッダー集計は表示行そのものから数える（分岐の複製をしない = 行数と必ず一致）
    let mut awaiting = 0usize;
    let mut working = 0usize;
    let mut completed = 0usize;
    for d in data.iter() {
        match d.bucket {
            Bucket::Awaiting => awaiting += 1,
            Bucket::Working => working += 1,
            Bucket::Completed => completed += 1,
        }
    }

    // ---- 描画 ----
    // ハイライトの規則はこれ 1 つ: 選択かホバーがその位置を指していれば光る。
    // **一覧の行（下）とフッターのアカウント行（末尾）が同じ規則を読む**ので、
    // 「どこが光るか」の知識が 2 箇所に分かれない
    let (selection, hovered) = (app.selection, app.hovered);
    let is_highlighted = |pos: SidebarPos| selection == pos || hovered == Some(pos);
    let mut items: Vec<ListItem> = Vec::new();
    let mut rows: Vec<SidebarRow> = Vec::new();

    let push_data_row =
        |items: &mut Vec<ListItem>, rows: &mut Vec<SidebarRow>, d: &RowData| {
            let cur = rows.len();
            let highlighted = is_highlighted(SidebarPos::Row(cur));
            let mut line_style = Style::default();
            if highlighted {
                line_style = line_style.bg(ui().hl_bg).fg(ui().emph);
            }
            // 名前の変更中は行そのものが入力欄。**アクションは残す**
            // （行数と当たり判定を通常時と揃える ＝ 編集中に一覧の座標がずれない）
            if let Some(text) = &d.editing {
                items.push(ListItem::new(
                    Line::from(format!("{RENAME_PREFIX}{text}"))
                        .style(line_style.bg(ui().hl_bg).fg(ui().emph)),
                ));
                rows.push(SidebarRow::Action(d.action.clone()));
                return;
            }
            let name_style = if d.is_active_window {
                Style::default().fg(ui().emph).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let mut spans = vec![
                Span::styled(
                    MENU_MARK,
                    Style::default().fg(if highlighted {
                        ui().emph
                    } else {
                        MUTED_FG
                    }),
                ),
                Span::raw(" "),
                // 未読マーカー。既読も同じ幅なので**名前の開始桁は動かない**
                Span::styled(unread_mark(d.unread), Style::default().fg(ui().emph)),
                Span::styled(d.glyph, Style::default().fg(d.color)),
                Span::raw(" "),
                Span::styled(d.label.clone(), name_style),
                Span::raw(" "),
                Span::styled(d.status_label, Style::default().fg(d.color)),
            ];
            spans.push(Span::raw(" · "));
            spans.push(Span::styled(
                d.age.clone(),
                Style::default().fg(ui().dim),
            ));
            items.push(ListItem::new(Line::from(spans).style(line_style)));
            rows.push(SidebarRow::Action(d.action.clone()));
        };

    let inner_width = chunks[0].width.saturating_sub(2);

    // 先頭: ccdesk / claude の版行と区切り線。更新があるときだけ行全体がクリック可
    for (text, style, row) in version_rows(
        ccdesk_update_state(app),
        &app.footer.current,
        claude_update_state(app),
        inner_width,
    ) {
        let cur = rows.len();
        let mut style = style;
        // ハイライトの条件は他の行と同じ「実体のある行か」だけ
        // （更新が無い版行も選択・ホバーできる ＝ 触れる行と光る行がずれない）
        if row.selectable() && is_highlighted(SidebarPos::Row(cur)) {
            style = style.bg(ui().hl_bg).fg(ui().emph);
        }
        items.push(ListItem::new(Line::from(text).style(style)));
        rows.push(row);
    }

    // 新規セッション
    {
        let cur = rows.len();
        let highlighted = is_highlighted(SidebarPos::Row(cur));
        let mut style = Style::default();
        if highlighted {
            style = style.bg(ui().hl_bg).fg(ui().emph);
        }
        items.push(ListItem::new(Line::from("+ new session").style(style)));
        rows.push(SidebarRow::Action(RowAction::New));
    }
    // 区切り線: new session（アクション）とセッション一覧領域を分ける（Desktop 風）
    items.push(ListItem::new(
        Line::from(separator_text(inner_width)).style(Style::default().fg(ui().dim)),
    ));
    rows.push(SidebarRow::Decoration);
    // グルーピング切替（クリックで state ⇔ directory）
    {
        let cur = rows.len();
        let highlighted = is_highlighted(SidebarPos::Row(cur));
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
        rows.push(SidebarRow::Action(RowAction::ToggleGroup));
    }
    // ヘッダー集計行（公式ヘッダー相当）
    items.push(ListItem::new(
        Line::from(format!(
            "{awaiting} awaiting input · {working} working · {completed} completed"
        ))
        .style(Style::default().fg(ui().dim)),
    ));
    rows.push(SidebarRow::Decoration);
    // ここまでが固定ヘッダー。積んだ数をそのまま正本にする
    // （ヒットテストとスクロール計算が読む。定数と二重管理にしない）
    let header_n = rows.len();

    // 見出しの下に並べる順。**ピン留めが先頭**で、それ以外は元の並びのまま
    // （安定ソートなので、ピン留め以外の相対順は動かない ＝ 並び替えの知識を
    // 増やさずに「ピンは上」だけを足す）
    fn ordered(mut members: Vec<&RowData>) -> Vec<&RowData> {
        members.sort_by_key(|d| !d.pinned);
        members
    }
    match app.grouping {
        Grouping::State => {
            for group in [Group::NeedsInput, Group::Working, Group::Completed] {
                let members = ordered(data.iter().filter(|d| d.group == group).collect());
                if members.is_empty() {
                    continue;
                }
                items.push(ListItem::new(Line::from("")));
                rows.push(SidebarRow::Decoration);
                items.push(ListItem::new(
                    Line::from(group.title()).style(Style::default().fg(ui().dim)),
                ));
                rows.push(SidebarRow::Decoration);
                for d in members {
                    push_data_row(&mut items, &mut rows, d);
                }
            }
        }
        Grouping::Directory => {
            // 見出しに出すフォルダと並びの決定は project_rows に閉じている。
            // 選択・close・delete 等の操作では並び替えない
            let cwds: Vec<&str> = data.iter().map(|d| d.cwd.as_str()).collect();
            // セッション行の振り分けキーも**行ごとに 1 度だけ**作る（見出し × 行の
            // 総当たりになるので、突き合わせのたびに作ると描画 1 回で数千の String に
            // なる。見出し側のキーは project_rows が持っている）
            let data_keys: Vec<String> = data.iter().map(|d| dir_key_of(&d.cwd)).collect();
            for row in project_rows(&app.projects, &cwds) {
                items.push(ListItem::new(Line::from("")));
                rows.push(SidebarRow::Decoration);
                let cur = rows.len();
                let highlighted = is_highlighted(SidebarPos::Row(cur));
                let mut style = Style::default().fg(ui().dim);
                if highlighted {
                    style = style.bg(ui().hl_bg).fg(ui().emph);
                }
                items.push(ListItem::new(Line::from(row.heading).style(style)));
                rows.push(SidebarRow::Action(RowAction::Project(row.cwd)));
                // 配下のセッション行。見出しの一覧と同じ同一判定キーで振り分ける
                // （ここだけ厳密一致にすると大小違いのセッションが行き場を失う）
                let members = ordered(
                    data.iter()
                        .zip(&data_keys)
                        .filter(|(_, key)| **key == row.key)
                        .map(|(d, _)| d)
                        .collect(),
                );
                for d in members {
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
    // 選択が浮いたら（行構成が変わった / 狭くてフッターが消えた）先頭の
    // 触れる行へ寄せる。**実体の無い位置に選択を残さない**
    let selection_lost = match app.selection {
        SidebarPos::Row(row) => !app.sidebar_rows.get(row).is_some_and(SidebarRow::selectable),
        SidebarPos::Account => !sl.footer_visible,
    };
    if selection_lost {
        app.selection = SidebarPos::Row(
            app.sidebar_rows
                .iter()
                .position(SidebarRow::selectable)
                .unwrap_or(0),
        );
    }

    // ヘッダー行は固定表示。スクロールはその下（セッション一覧）にだけ効く。
    // ↑↓ 直後だけ選択行へ追従し、常に範囲内へクランプ
    // （アカウント行はフッターに固定なのでスクロールに関係しない）
    let tail_capacity = capacity.saturating_sub(header_n);
    if app.sidebar_follow_sel {
        app.sidebar_follow_sel = false;
        if let Some(row) = app.selection.row().filter(|row| *row >= header_n) {
            let sel_t = row - header_n;
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
        .filter(|(i, _)| row_visible(*i, header_n, scroll, tail_capacity))
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

    // 名前の変更中はカーソルを編集位置へ置く。**行が表示窓から出ていれば置かない**
    // （スクロールで見えていない行の位置へカーソルだけが飛ぶと、IME の変換窓が
    // 無関係な場所に開く。判断は描画の絞り込みと同じ [`row_visible`]）
    let rename_cursor = app.rename.as_ref().and_then(|rename| {
        use unicode_width::UnicodeWidthStr;
        let row = row_of_session(&app.sidebar_rows, &rename.id)?;
        row_visible(row, header_n, scroll, tail_capacity).then(|| {
            let x = chunks[0].x + 1 + RENAME_PREFIX.width() as u16;
            FrameCursor::shown_at(Position::new(
                // 入力が枠を越えたらカーソルは内側の右端で止める（枠の外へ出さない）
                x.saturating_add(rename.field.cursor_x())
                    .min(chunks[0].x + chunks[0].width.saturating_sub(2)),
                row_y(row, header_n, scroll),
            ))
        })
    });

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
        // **この行はクリックでもキーボードでも押せる**（アカウントメニューの入口。
        // 当たり判定は handle_mouse 側が同じ `sidebar_layout` の account_y で持ち、
        // キーボードの選択は [`SidebarPos::Account`]）
        let (account, mut account_style) = account_row(
            &app.footer.account,
            active_unstored(app),
            app.account_job.as_ref().map(|job| job.progress),
        );
        // 選択中・ホバー中は一覧の行とまったく同じ見え方にする
        // （キーボードで降りてもマウスを乗せても「今ここ」が同じ帯で分かる）
        if is_highlighted(SidebarPos::Account) {
            account_style = account_style.bg(ui().hl_bg).fg(ui().emph);
        }
        // **スタイルは `Line` ではなく `Paragraph` へ載せる。** `Paragraph` は
        // 自分のスタイルを矩形全体へ塗ってから文字を書くので、帯が一覧の行と同じ
        // 行幅いっぱいまで伸びる。`Line` に載せると塗られるのは文字が占める桁だけで、
        // 一覧の行（`ListItem` ＝ ratatui がリスト幅まで埋める）より短い帯になる
        frame.render_widget(
            ratatui::widgets::Paragraph::new(Line::from(clip_to_width(&account, fw)))
                .style(account_style),
            Rect::new(fx, account_y, fw, 1),
        );
    }

    // 最下行の横断バー。**サイドバーを積んだ後に描くのが要点**で、案内は選択行の
    // 種類で変わるため、先に描くと 1 フレーム前の行の案内が出る（選択を動かした
    // フレームで案内が追いつかない）。矩形は重ならないので描く順序は見た目に影響しない
    draw_bottom_bar(frame, vert[1], app);


    // 右ペイン → コンテキストメニューの順で描く。**この順序が意味を持つ**:
    // メニューの幅は内容が決めるので、サイドバーが狭いと矩形が右ペインへ食い込む
    // （[`popup_rect`]「被せて全部読ませる」の意図）。先に描くと食い込んだ列が
    // 右ペインに塗り潰され、ラベルが割れたまま**クリック判定だけが残る**
    // （見た目は claude の画面なのに押すと new session が走る）。
    // クリック判定と描画は同じ [`popup_rect`] を見ているので、最後に描けば
    // 「見えているものが効く」が回復する
    let cursor = draw_right_pane(frame, chunks[1], app);
    draw_popup(frame, app);
    // 名前の入力中はそこが打鍵の宛先なので、右ペインのカーソルより優先する
    rename_cursor.unwrap_or(cursor)
}

/// そのセッションの行 index（[`RowAction::Open`] を持つ行）。
/// 名前の編集中のカーソルを置く行を、**描画が積んだ一覧そのもの**から引く
/// （行の並びは編集中も読み直しで動くので、開始時の index を覚えない）
fn row_of_session(rows: &[SidebarRow], id: &SessionId) -> Option<usize> {
    rows.iter()
        .position(|row| matches!(row.action(), Some(RowAction::Open(row_id)) if row_id == id))
}

/// コンテキストメニュー（モーダル）。矩形はクリック判定と同じ [`popup_rect`] を使う。
/// **右ペインより後に描く**（呼び出し側の順序に理由を書いてある）
fn draw_popup(frame: &mut Frame, app: &App) {
    let Some(popup) = &app.popup else { return };
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

/// 右ペイン: 新規セッション画面 or アクティブセッションの画面。
/// 終端カーソルの決定はこの中に閉じる（[`FrameCursor`] 参照）
fn draw_right_pane(frame: &mut Frame, pane: Rect, app: &mut App) -> FrameCursor {
    let focus_style = Style::default().fg(FOCUS_BORDER);
    let blur_style = Style::default().fg(ui().dim);
    let terminal_focused = app.focus == Focus::Terminal;
    let starting = app.input_gate.is_some();
    // Esc で戻れる先（セッションの窓）があるか。**借用の前に取る**（New 画面の
    // 描画は right_view を可変で借りるため）
    let can_leave = !app.windows.is_empty();
    if let RightView::New(state) = &mut app.right_view {
        return draw_new_view(frame, pane, state, terminal_focused, starting, can_leave);
    }
    if app.windows.is_empty() {
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .title("no session")
                .border_style(Style::default().fg(ui().dim)),
            pane,
        );
        return FrameCursor::hidden_at(pane_fallback_pos(pane));
    }
    let window = &app.windows[app.active];
    let parser = window.parser.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let screen = parser.screen();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(window.name.clone())
        .border_style(if app.focus == Focus::Terminal {
            focus_style
        } else {
            blur_style
        });
    let inner = block.inner(pane);
    // tui-term 独自の █ カーソル描画は無効化し、ネイティブカーソル
    // （set_cursor_position = 本家と同じ点滅バー）だけを使う
    let widget = PseudoTerminal::new(screen)
        .cursor(tui_term::widget::Cursor::default().visibility(false))
        .block(block);
    frame.render_widget(widget, pane);

    // カーソル位置を反映。フォーカス外・子が非表示指定のときも「隠すだけ」で
    // 位置は必ず確定させる（描かないとサイドバーに置き去りになる。FrameCursor 参照）。
    // ペイン外へはみ出す座標はペイン内へクランプする
    let (crow, ccol) = screen.cursor_position();
    let pos = terminal_cursor_pos(pane, inner, crow, ccol);
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
            assert!(contains(pane, pos), "pane {pane:?}: pos {pos:?} is outside");
        }
    }

    /// 既定のサイドバー幅（34 桁）の内側。版行の幅の予算はこの桁数
    const DEFAULT_INNER: u16 = 32;

    /// 行頭に置く記号の表示幅は 1 桁。**この 1 という実測値に設計が乗っている**:
    /// 更新が無い版行はスペース 1 個でマーカー桁を確保しており、2 桁なら桁がずれる。
    ///
    /// メニュー記号が ASCII なのはこの前提を測らずに済ませるため
    /// （U+2630 は East Asian Ambiguous で、端末によって 1 桁にも 2 桁にもなる）。
    /// 名前の変更中の行頭はその記号と対で、通常の行と同じ桁から名前が始まる
    #[test]
    fn the_row_head_marks_are_one_column_wide() {
        use unicode_width::UnicodeWidthStr;
        assert_eq!(UPDATE_MARK.width(), 1, "⟳ is not 1 column wide");
        assert_eq!(MENU_MARK.width(), 1, "the menu mark is not 1 column wide");
        assert!(MENU_MARK.is_ascii(), "reverted to an ambiguous-width mark");
        assert_eq!(
            RENAME_PREFIX,
            format!("{MENU_MARK} "),
            "the renaming row head is out of step with a normal row"
        );
        assert_eq!(RENAME_PREFIX.width(), 2);
    }

    /// **未読マーカーは既読と同じ幅を取る。** 幅が違うと、未読が付いたり消えたり
    /// するたびに名前が横へずれる（行が変わったこと自体が読み取りにくくなる）
    #[test]
    fn the_unread_marker_and_its_empty_slot_are_the_same_width() {
        use unicode_width::UnicodeWidthStr;
        assert_eq!(unread_mark(true), UNREAD_MARK);
        assert_eq!(unread_mark(false), READ_MARK);
        assert_eq!(
            UNREAD_MARK.width(),
            READ_MARK.width(),
            "unread and read marks differ in width: {UNREAD_MARK:?} / {READ_MARK:?}"
        );
        assert!(READ_MARK.trim().is_empty(), "a character is showing in the read slot");
    }

    /// **未読の印は行頭のメニュー記号の右**（状態ラベルの前ではない）で、
    /// **名前の開始桁は未読／既読で動かない**。
    ///
    /// 比べるのは 2 本の行そのもの（未読 1 本・既読 1 本を同じフレームに描く）で、
    /// 桁数を式で書き写さない ＝ 行の組み立てを変えたらここが落ちる。
    /// 行頭が食う桁は [`MIN_SIDEBAR`] の下限の根拠でもあるので、同時に固定する
    #[test]
    fn the_unread_mark_sits_next_to_the_menu_mark_without_shifting_the_name() {
        use crate::app::MIN_SIDEBAR;
        use unicode_width::UnicodeWidthStr;
        let mut app = App {
            term_size: (60, 40),
            sidebar_width: 34,
            sessions: vec![
                crate::sessions::SessionRow {
                    // 見ていない間に動いた行（`updated_at > last_opened_at`）
                    last_opened_at: 1_000,
                    updated_at: 2_000,
                    ..named_session("a", "C:\\dev\\api", "fresh-row")
                },
                named_session("b", "C:\\dev\\api", "seen-row"),
            ],
            ..Default::default()
        };
        let lines = session_lines(&mut app);
        let at = |needle: &str| {
            lines
                .iter()
                .find(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("{needle} is not on any row: {lines:?}"))
                .clone()
        };
        let (unread, read) = (at("fresh-row"), at("seen-row"));

        // 行頭は `= ` の次が未読の桁（既読はそこが空白）
        let head = format!("{MENU_MARK} ");
        assert!(unread.starts_with(&format!("{head}{}", UNREAD_MARK.trim())), "{unread:?}");
        assert!(read.starts_with(&format!("{head}{READ_MARK}")), "{read:?}");
        // 名前の開始桁は 2 本で同じ（未読の桁が空白でも確保されている）
        let name_col = |line: &str, name: &str| line[..line.find(name).unwrap()].width();
        assert_eq!(
            name_col(&unread, "fresh-row"),
            name_col(&read, "seen-row"),
            "the unread mark shifted the name column: {unread:?} / {read:?}"
        );
        // 行頭が食う桁 = メニュー記号 + 空白 + 未読の桁 + 状態アイコン + 空白
        let head_cols = MENU_MARK.width() + 1 + UNREAD_MARK.width() + 1 + 1;
        assert_eq!(name_col(&read, "seen-row"), head_cols, "the row head changed width");
        // 一番狭いサイドバー（[`MIN_SIDEBAR`]）でも行頭 + 名前 1 桁は残る
        assert!(
            usize::from(MIN_SIDEBAR - 2) > head_cols,
            "the narrowest sidebar no longer fits the row head ({head_cols} columns)"
        );
    }

    /// **生きている行の状態は hook が主、`agents --json` が従。**
    /// hook は turn 単位で届くので Done を区別できるが、`status` からは出せない
    #[test]
    fn a_live_row_prefers_the_hook_state_over_the_live_status() {
        let label = |hook, status, heuristic| {
            row_state(true, "stopped", hook, status, heuristic).label
        };
        // hook が居れば status も出力ヒューリスティックも見ない
        assert_eq!(label(Some("done"), "busy", Some(SessionStatus::Working)), "Done");
        assert_eq!(label(Some("working"), "idle", None), "Working");
        assert_eq!(label(Some("blocked"), "busy", None), "Needs input");
        // hook が一度も来ていない行は status から導く
        assert_eq!(label(None, "busy", None), "Working");
        assert_eq!(label(None, "idle", None), "Needs input");
        // status も無い間は出力の変化から推す
        assert_eq!(label(None, "", Some(SessionStatus::Working)), "Working");
        assert_eq!(label(None, "", Some(SessionStatus::NeedsInput)), "Needs input");
        assert_eq!(label(None, "", None), "Needs input");
    }

    /// **生きている行の `stopped` は捨てる。** 再開した直後は前回の `SessionEnd` が
    /// 保管に残っているので、そのまま出すと「動いているのに Stopped」になる
    /// （生死の真実は PTY の `try_wait`）
    #[test]
    fn a_live_row_ignores_a_stale_stopped_from_the_previous_run() {
        assert_eq!(row_state(true, "", Some("stopped"), "busy", None).label, "Working");
        assert_eq!(row_state(true, "", Some("stopped"), "idle", None).label, "Needs input");
        // 死んでいる行では捨てない（そこは本当に止まっている）
        assert_eq!(row_state(false, "stopped", None, "", None).label, "Stopped");
    }

    /// **死んでいる行は最後に観測した state のまま**（生きている行の材料は使わない）。
    /// 一度も観測していない行 ＝ 動いていない
    #[test]
    fn a_dead_row_shows_the_last_state_it_was_seen_in() {
        let label = |last_state| row_state(false, last_state, Some("working"), "busy", None).label;
        assert_eq!(label("done"), "Done");
        assert_eq!(label("stopped"), "Stopped");
        assert_eq!(label(""), "Stopped", "a never-observed row looks like it is running");
        // 死んでいる行はアイコンの形も生死を表す（✻ ではなく ∙）
        assert!(!row_state(false, "done", None, "", None).alive);
        assert!(row_state(true, "", Some("done"), "", None).alive);
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
            assert_eq!(rows.len(), 3, "expected 2 version rows + 1 separator");
            assert!(rows[0].0.contains(env!("CARGO_PKG_VERSION")), "{:?}", rows[0].0);
            assert!(rows[1].0.contains("claude v2.1.220"), "{:?}", rows[1].0);
            assert_eq!(rows[2].0, separator_text(DEFAULT_INNER));
            // **区切り線だけが飾り。** 版行は更新の有無に関係なく行の実体がある
            assert_eq!(
                rows[2].2,
                SidebarRow::Decoration,
                "the separator must not be a row you can touch"
            );
            assert!(
                rows[0].2.selectable() && rows[1].2.selectable(),
                "a version row dropped out of the selection at {ccdesk:?} / {claude:?}"
            );
        }
        // 版が未取得なら番号を出さない（誤情報を出さない）
        let rows = version_rows(UpdateState::Current, "", UpdateState::Current, DEFAULT_INNER);
        assert!(rows[1].0.contains("claude"), "{:?}", rows[1].0);
        assert!(!rows[1].0.contains(" v"), "showing a v with no version: {:?}", rows[1].0);
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
            assert!(text.ends_with(verb), "{text:?} does not end with {verb:?}");
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
            let at = text.find("ccdesk").expect("name is missing");
            text[..at].width()
        };
        let base = name_col(&version_row("ccdesk", "0.5.0", UpdateState::Current, DEFAULT_INNER));
        assert_eq!(base, 2, "expected 1 marker column + 1 space column");
        for state in [
            UpdateState::Available,
            UpdateState::Running,
            UpdateState::Restart,
        ] {
            let text = version_row("ccdesk", "0.5.0", state, DEFAULT_INNER);
            assert_eq!(name_col(&text), base, "{state:?} shifted the name column: {text:?}");
        }
    }

    /// 押して更新できるのは「更新がある」行だけ。実行中・再起動待ちは押しても
    /// 意味が無いので動作を付けない。**それでも行は行**なので
    /// [`SidebarRow::Inert`] ＝ 選択・ホバーの対象からは外れない
    #[test]
    fn version_rows_are_clickable_only_when_an_update_is_available() {
        let rows_of = |ccdesk, claude| {
            let rows = version_rows(ccdesk, "2.1.220", claude, DEFAULT_INNER);
            (rows[0].2.clone(), rows[1].2.clone())
        };
        assert_eq!(
            rows_of(UpdateState::Available, UpdateState::Available),
            (
                SidebarRow::Action(RowAction::UpdateCcdesk),
                SidebarRow::Action(RowAction::UpdateClaude)
            )
        );
        for state in [
            UpdateState::Current,
            UpdateState::Running,
            UpdateState::Restart,
        ] {
            assert_eq!(
                rows_of(state, state),
                (SidebarRow::Inert, SidebarRow::Inert),
                "{state:?}"
            );
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
                        "does not fit the default width: {text:?} ({} columns / inner {DEFAULT_INNER} columns)",
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
        // 全角（East Asian Wide）5 文字 = 10 桁。7 桁で切れば 3 文字（6 桁）まで。
        // 日本語ではなく全角ラテン文字を使うのは、ここで要るのは「全角幅（幅 2）」という
        // 性質だけで、日本語そのものは tests/no_japanese_in_code.rs の検査対象になるため
        let wide = "ＡＢＣＤＥ";
        assert_eq!(clip_to_width(wide, 7), "ＡＢＣ");
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
        use crate::accounts::{Account, ActiveAccount};
        use crate::poll::FooterInfo;

        let mut app = App {
            term_size: (120, 30),
            footer: FooterInfo {
                account: AccountStatus::LoggedIn(ActiveAccount::unseen(Account::new(
                    "you@example.com",
                    "you · Acme, Inc.",
                ))),
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
            "the account row has no label: {:?}",
            row(sl.account_y)
        );
        assert!(
            !row(sl.account_y).contains('@'),
            "the row shows the email: {:?}",
            row(sl.account_y)
        );
        assert!(
            row(sl.account_y - 1).contains('─'),
            "the row above the account row is not a separator: {:?}",
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
        assert_eq!(header[0].2.action(), Some(&RowAction::UpdateCcdesk));
        assert_eq!(header[1].2.action(), Some(&RowAction::UpdateClaude));
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
            .map(|row| row.heading)
            .collect()
    }

    /// 見出し行の (表示文字列, 対象フォルダ) を取り出す
    fn heading_pairs(projects: &[String], cwds: &[&str]) -> Vec<(String, String)> {
        project_rows(projects, cwds)
            .into_iter()
            .map(|row| (row.heading, row.cwd))
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
        let rows = heading_pairs(&projects, &[]);
        assert_eq!(
            rows,
            [
                ("api".to_string(), "C:\\dev\\api".to_string()),
                ("api".to_string(), "C:\\work\\api".to_string()),
            ],
            "same-named leaves are not sorted by full path"
        );
        // 入力順を入れ替えても並びは同じ
        let flipped = ["C:\\dev\\api".to_string(), "C:\\work\\api".to_string()];
        assert_eq!(heading_pairs(&flipped, &[]), rows, "order changed with input order");
    }

    /// 見出しは末端ディレクトリ名だけ。**`+` は出さない**（行の動作はメニューを開くことで、
    /// 「押したら即セッションが立つ」というヒントは嘘になる）
    #[test]
    fn project_headings_carry_no_plus_hint() {
        let rows = heading_pairs(&["C:\\dev\\api".to_string()], &["C:\\dev\\web"]);
        for (heading, cwd) in &rows {
            assert!(!heading.contains('+'), "a + is left in the heading: {heading:?}");
            assert_eq!(heading.trim(), heading, "the heading has extra whitespace: {heading:?}");
            assert!(cwd.starts_with("C:\\"), "the target is not a full path: {cwd:?}");
        }
        assert_eq!(rows[0], ("api".to_string(), "C:\\dev\\api".to_string()));
    }

    /// 末端ディレクトリ名が取れないパス（ドライブ直下）でも見出しを落とさない。
    /// 登録は自動なので、ドライブ直下でセッションを作れば一覧に入り得る
    #[test]
    fn project_rows_fall_back_to_the_path_when_there_is_no_leaf() {
        assert_eq!(
            heading_pairs(&["C:\\".to_string()], &[]),
            [("C:\\".to_string(), "C:\\".to_string())]
        );
    }

    /// **同一判定キーがパスごとに 1 度しか作られないことの固定。** ここは PTY の
    /// dirty ごと（秒 30 回まで）に走る経路で、以前は照合のたびにキーを作っていたので
    /// 呼び出しが入力数の 2 乗で増えていた（上限 50+50 の実測で 1 描画あたり
    /// dir_key 12,550 回 ＝ String 約 2.5 万個）。回数そのものを式で固定して
    /// 「うっかり比較子の中でキーを作る」変更が入ったら落ちるようにする
    #[test]
    fn the_directory_grouping_builds_each_key_once_per_path() {
        for n in [1usize, 10, 50] {
            let projects: Vec<String> = (0..n).map(|i| format!("C:\\dev\\p{i}")).collect();
            let cwds: Vec<String> = (0..n).map(|i| format!("c:\\dev\\p{i}\\")).collect();
            let cwds: Vec<&str> = cwds.iter().map(String::as_str).collect();
            let (dir_keys, leaves) = key_calls(|| {
                let rows = project_rows(&projects, &cwds);
                // 表記違いは同じフォルダなので見出しは n 本（重複排除も効いている）
                assert_eq!(rows.len(), n, "n={n}: heading count does not match");
            });
            // 入力 2n 本ぶんだけ ＝ 線形。2 乗なら n=50 で 1 万回を超える
            assert_eq!(dir_keys, 2 * n, "n={n}: dir_key was called more than the input count");
            assert_eq!(leaves, 2 * n, "n={n}: leaf-name extraction exceeded the input count");
        }
    }

    /// 描画 1 フレーム全体でも線形。見出しの重複排除と**セッション行の振り分け**の
    /// 両方を通した回数を固定する（振り分けは見出し × 行の総当たりなので、
    /// キーを持ち回らないとここが一番効く）
    #[test]
    fn a_directory_grouped_frame_builds_keys_linearly() {
        for n in [1usize, 10, 50] {
            let mut app = App {
                term_size: (120, 250),
                sidebar_width: 34,
                grouping: Grouping::Directory,
                projects: (0..n).map(|i| format!("C:\\dev\\p{i}")).collect(),
                sessions: (0..n)
                    .map(|i| session_in(&format!("C:\\dev\\p{i}")))
                    .collect(),
                ..Default::default()
            };
            let (dir_keys, leaves) = key_calls(|| {
                let rows = render_sidebar(&mut app);
                let headings = rows
                    .iter()
                    .filter(|(row, _)| matches!(row.action(), Some(RowAction::Project(_))))
                    .count();
                assert_eq!(headings, n, "n={n}: heading count does not match");
            });
            // project_rows へ渡るのは登録 n + セッション行 n で、
            // セッション行の振り分けキーがさらに n。末端名は project_rows のぶんだけ
            assert_eq!(dir_keys, 3 * n, "n={n}: dir_key calls are not proportional to the input count");
            assert_eq!(leaves, 2 * n, "n={n}: leaf-name extraction is not proportional to the input count");
        }
    }

    /// キー作りの呼び出し回数を (dir_key, 末端名) で数える。カウンタはスレッド
    /// ローカルで、テストは 1 本ごとに別スレッドなので並列実行でも混ざらない
    fn key_calls(body: impl FnOnce()) -> (usize, usize) {
        DIR_KEY_CALLS.set(0);
        LEAF_NAME_CALLS.set(0);
        body();
        (DIR_KEY_CALLS.get(), LEAF_NAME_CALLS.get())
    }

    /// そのフォルダのセッション行 1 本
    fn session_in(cwd: &str) -> crate::sessions::SessionRow {
        crate::sessions::SessionRow::new(
            crate::sessions::SessionId::new(format!("s-{cwd}")),
            cwd,
            "session",
            crate::sessions::TitleSource::Derived,
            0,
        )
    }

    /// **キーを持ち回る形にしても一覧の中身・並び・重複排除が変わらないことの固定。**
    /// 表記違い（大小・区切りの種類・末尾の重複区切り）・同名末端の別パス・末端の
    /// 取れないドライブ直下を 1 つの入力に混ぜて、出る行を丸ごと固定する
    #[test]
    fn project_rows_keep_their_content_and_order_for_mixed_notations() {
        let projects = [
            "C:\\dev\\Zebra".to_string(),
            "C:\\work\\api".to_string(),
            "C:\\dev\\api".to_string(),
            "C:\\".to_string(),
        ];
        let cwds = [
            "c:/dev/zebra/",     // 登録済みと同じフォルダ（表記違い）
            "C:\\\\",            // ドライブ直下の重複区切り
            "C:\\dev\\API\\",    // 登録済みと同じフォルダ（大小・末尾違い）
            "C:\\dev\\mango",    // 未登録でセッションだけ
        ];
        assert_eq!(
            heading_pairs(&projects, &cwds),
            [
                // 末端が取れないドライブ直下は並べ替えキーが空なので先頭
                ("C:\\".to_string(), "C:\\".to_string()),
                // 同名末端はフルパス順。表記は**先に出てきた登録リスト側**が残る
                ("api".to_string(), "C:\\dev\\api".to_string()),
                ("api".to_string(), "C:\\work\\api".to_string()),
                ("mango".to_string(), "C:\\dev\\mango".to_string()),
                ("Zebra".to_string(), "C:\\dev\\Zebra".to_string()),
            ],
            "content or order changed when notation variants were mixed together"
        );
    }

    /// サイドバーを実際に描いて (行データ, その行の表示文字列) を返す。
    /// **描画を経由するのが要点**で、`project_rows` の結果が本当に画面と
    /// クリック判定へ届いているか（登録リストが draw に配線されているか）を見る
    fn render_sidebar(app: &mut App) -> Vec<(SidebarRow, String)> {
        let (w, h) = app.term_size;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).expect("failed to create terminal");
        terminal.draw(|frame| {
            draw(frame, app);
        })
        .expect("draw failed");
        let buffer = terminal.backend().buffer().clone();
        // 固定ヘッダーの下はスクロール分ずれるが、このテストは行数が窓に収まる
        // 前提なので scroll = 0（描画側がクランプ済み）
        assert_eq!(app.sidebar_scroll, 0, "test precondition (no scroll) broke down");
        app.sidebar_rows
            .iter()
            .enumerate()
            .map(|(idx, row)| {
                let y = idx as u16 + 1; // 上枠の次の行から積まれる
                let text: String = (1..app.sidebar_width.saturating_sub(1))
                    .map(|x| buffer[(x, y)].symbol())
                    .collect();
                (row.clone(), text.trim_end().to_string())
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
            .find(|(row, _)| {
                matches!(row.action(), Some(RowAction::Project(cwd)) if cwd == "C:\\dev\\empty-project")
            })
            .expect("the registered project's heading was not drawn");
        assert_eq!(heading.1, "empty-project", "the heading text is not just the leaf name");
        assert!(!heading.1.contains('+'), "a + is left in the heading: {:?}", heading.1);
        // **見出しだけを出す**（「no sessions」等の説明行を挟まない）。セッションが
        // 0 本なので、この見出しがサイドバー最後の行になる
        let idx = rows.iter().position(|(_, t)| t == "empty-project").unwrap();
        assert_eq!(
            idx + 1,
            rows.len(),
            "rows are stacked below the heading: {:?}",
            &rows[idx + 1..]
        );
        assert!(
            !rows.iter().any(|(_, t)| t.contains("no session")),
            "a zero-sessions explanation row is showing"
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
            sessions: DemoSource.sessions(),
            projects: DemoSource.window_state().projects,
            ..Default::default()
        };
        let rows = render_sidebar(&mut app);
        let headings: Vec<&str> = rows
            .iter()
            .filter(|(row, _)| matches!(row.action(), Some(RowAction::Project(_))))
            .map(|(_, text)| text.as_str())
            .collect();
        assert_eq!(
            headings,
            ["api", "docs", "infra", "shop-app"],
            "the heading order of the demo data changed"
        );
        // infra は demo セッションを持たないフォルダ。見出しの直後は空行 or 次の見出しで、
        // セッション行は付かない（＝ 0 本でも見出しだけが残る）
        let infra = rows.iter().position(|(_, t)| t == "infra").unwrap();
        assert_eq!(rows[infra + 1].1, "", "a session row is showing under infra");
        assert!(
            matches!(rows[infra + 2].0.action(), Some(RowAction::Project(cwd)) if cwd.ends_with("shop-app")),
            "the row after the empty folder is not another heading"
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
                .any(|(row, text)| row.action() == Some(&RowAction::New) && text == "+ new session"),
            "the + new session row is gone"
        );
    }

    // ── ピン留め / 名前の変更が一覧に効いていることの検証 ──────
    //
    // どれも「行が持っている値」→「画面に出る並び」の配線なので、
    // 実際に 1 フレーム描いて（[`render_sidebar`]）出た行そのものを見る

    /// 名前つきのセッション行 1 本
    fn named_session(id: &str, cwd: &str, title: &str) -> crate::sessions::SessionRow {
        crate::sessions::SessionRow::new(
            crate::sessions::SessionId::new(id),
            cwd,
            title,
            crate::sessions::TitleSource::Derived,
            0,
        )
    }

    /// セッション行（[`MENU_MARK`] で始まる行）の表示文字列だけを、描かれた順に取り出す
    fn session_lines(app: &mut App) -> Vec<String> {
        render_sidebar(app)
            .into_iter()
            .filter(|(row, _)| matches!(row.action(), Some(RowAction::Open(_))))
            .map(|(_, text)| text)
            .collect()
    }

    /// 一覧の行の並びをそのまま返す（見出し・空行も含む）
    fn sidebar_texts(app: &mut App) -> Vec<String> {
        render_sidebar(app).into_iter().map(|(_, t)| t).collect()
    }

    /// **ピン留めした行はグループ内の先頭へ。** 並べ替えは安定なので、
    /// ピン留め以外の相対順は動かない（どちらのグルーピングでも同じ規則）
    #[test]
    fn a_pinned_row_comes_first_inside_its_group() {
        for grouping in [Grouping::State, Grouping::Directory] {
            let mut app = App {
                term_size: (60, 40),
                sidebar_width: 34,
                grouping,
                sessions: vec![
                    named_session("a", "C:\\dev\\api", "first"),
                    named_session("b", "C:\\dev\\api", "second"),
                    crate::sessions::SessionRow {
                        pinned: true,
                        ..named_session("c", "C:\\dev\\api", "pinned")
                    },
                ],
                ..Default::default()
            };
            let lines = session_lines(&mut app);
            assert_eq!(lines.len(), 3, "{grouping:?}: a row is missing");
            assert!(lines[0].contains("pinned"), "{grouping:?}: {lines:?}");
            // 残りは元の並びのまま（ピン留めが並べ替えの規則を増やしていない）
            assert!(lines[1].contains("first"), "{grouping:?}: {lines:?}");
            assert!(lines[2].contains("second"), "{grouping:?}: {lines:?}");
        }
    }

    /// **一覧に隠し区画は無い。** アーカイブを廃止したので、行はどちらの
    /// グルーピングでも通常の一覧に出て集計にも数えられる（`delete` が消すのは
    /// 行だけなので、アーカイブとの差は「戻す導線があるか」しか残らず、
    /// 節を 1 つ増やす価値が無かった）
    #[test]
    fn every_row_stays_in_the_normal_list_and_is_counted() {
        for grouping in [Grouping::State, Grouping::Directory] {
            let mut app = App {
                term_size: (60, 40),
                sidebar_width: 34,
                grouping,
                sessions: vec![
                    named_session("a", "C:\\dev\\api", "first row"),
                    named_session("b", "C:\\dev\\api", "second row"),
                ],
                ..Default::default()
            };
            let texts = sidebar_texts(&mut app);
            assert!(
                !texts.iter().any(|t| t.contains("Archived")),
                "{grouping:?}: an Archived section came back: {texts:?}"
            );
            for row in ["first row", "second row"] {
                assert!(
                    texts.iter().any(|t| t.contains(row)),
                    "{grouping:?}: {row} is missing from the list: {texts:?}"
                );
            }
            // 集計は一覧に出る行を全部数える（隠す行が無いので数と行数が一致する）
            let counts = texts
                .iter()
                .find(|t| t.contains("awaiting input"))
                .expect("the summary row is missing")
                .clone();
            // （末尾はサイドバー幅で切られるので、数の出るところまでを見る）
            assert!(
                counts.starts_with("0 awaiting input · 0 working · 2"),
                "{grouping:?}: not every row is counted: {counts:?}"
            );
        }
    }

    /// 名前の変更中は**その行が入力欄に化ける**。名前の桁は通常の行と同じ位置から
    /// 始まり（[`RENAME_PREFIX`] の 2 桁ぶん）、カーソルはその行の入力位置に立つ
    #[test]
    fn the_renamed_row_becomes_an_inline_input_with_the_cursor_on_it() {
        use unicode_width::UnicodeWidthStr;
        let mut field = crate::ui::text_field::TextField::default();
        field.set_text("new name");
        let mut app = App {
            term_size: (60, 40),
            sidebar_width: 34,
            sessions: vec![named_session("s", "C:\\dev\\api", "old name")],
            rename: Some(crate::app::Rename {
                id: crate::sessions::SessionId::new("s"),
                field,
            }),
            ..Default::default()
        };
        let (w, h) = app.term_size;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        let mut cursor = None;
        terminal
            .draw(|frame| {
                cursor = Some(draw(frame, &mut app));
            })
            .unwrap();
        let cursor = cursor.expect("did not draw");
        let buffer = terminal.backend().buffer().clone();

        // 入力中の文字列が行に出て、確定前の名前は出ていない
        let row = app
            .sidebar_rows
            .iter()
            .position(|row| matches!(row.action(), Some(RowAction::Open(_))))
            .expect("no session row");
        let y = row_y(row, app.sidebar_header_rows, app.sidebar_scroll);
        let text: String = (1..app.sidebar_width - 1)
            .map(|x| buffer[(x, y)].symbol())
            .collect();
        assert!(text.contains("new name"), "the text being typed is not on the row: {text:?}");
        assert!(!text.contains("old name"), "the old name is still on the row before the rename is committed: {text:?}");
        // カーソルはその行の、入力の末尾（行頭 2 桁 + 8 桁）に立つ
        assert!(cursor.visible, "the cursor is hidden while a name is being typed");
        assert_eq!(
            cursor.pos,
            Position::new(
                1 + RENAME_PREFIX.width() as u16 + "new name".width() as u16,
                y
            )
        );
    }

    /// 下部バーの案内。**撤去した打鍵は載せない**（載っていれば嘘になる）。
    /// 名前の入力中はその作法だけを出す
    #[test]
    fn the_sidebar_hint_only_mentions_keys_that_still_exist() {
        let mut app = App {
            term_size: (120, 30),
            focus: Focus::Sidebar,
            ..Default::default()
        };
        let bar = drawn_row(&mut app, 29);
        assert!(bar.contains("Ctrl+Q quit"), "{bar:?}");
        assert!(bar.contains("Alt+←→ focus"), "{bar:?}");
        assert!(bar.contains("↑↓ select"), "{bar:?}");
        // サイドバーから撤去した打鍵（`←` メニュー / `→` 開く）は案内にも残さない。
        // `Alt+←→` は残るので、方向キー単体の案内だけを見る
        assert!(!bar.contains("← menu"), "the hint still offers ← as the menu key: {bar:?}");
        assert!(!bar.contains("Enter/→"), "the hint still offers → as the open key: {bar:?}");
        assert!(!bar.contains("Ctrl+S"), "the hint still lists a key that was removed: {bar:?}");
        assert!(!bar.contains("Ctrl+X"), "the hint still lists a key that was removed: {bar:?}");

        app.sessions = vec![named_session("s", "C:\\dev\\api", "session")];
        app.rename = Some(crate::app::Rename {
            id: crate::sessions::SessionId::new("s"),
            field: crate::ui::text_field::TextField::default(),
        });
        let bar = drawn_row(&mut app, 29);
        assert!(bar.contains("Enter rename"), "{bar:?}");
        assert!(bar.contains("Esc cancel"), "{bar:?}");
    }

    /// 下部バーは**打鍵が届く先で効くキーだけ**を出す。受け手は 4 つ
    /// （一覧 / メニュー / 名前の入力 / 端末）で、他所のキーを混ぜない。
    /// `app:` の予約キーはどの状態でも出る（受け手に関係なく効く唯一の打鍵）
    #[test]
    fn the_bottom_bar_follows_whoever_receives_the_keys() {
        let base = || App {
            term_size: (120, 30),
            focus: Focus::Sidebar,
            sessions: vec![named_session("s", "C:\\dev\\api", "session")],
            ..Default::default()
        };
        // 一覧: `↑↓` と、選択行の `Enter` が何をするか（既定の選択は先頭の版行）
        let mut app = base();
        let bar = drawn_row(&mut app, 29);
        assert!(bar.contains("sidebar: ↑↓ select"), "{bar:?}");

        // メニュー表示中: 一覧のキーは全部このメニューが飲むので出さない
        let mut app = App {
            popup: Some(Popup {
                kind: crate::app::PopupKind::Group,
                anchor_y: 3,
                selected: 0,
            }),
            ..base()
        };
        let bar = drawn_row(&mut app, 29);
        assert!(bar.contains("popup: ↑↓ select · Enter run · Esc close"), "{bar:?}");
        assert!(!bar.contains("menu"), "the list keys are still listed: {bar:?}");

        // 名前の入力中: 入力の作法だけ
        let mut app = App {
            rename: Some(crate::app::Rename {
                id: crate::sessions::SessionId::new("s"),
                field: crate::ui::text_field::TextField::default(),
            }),
            ..base()
        };
        let bar = drawn_row(&mut app, 29);
        assert!(bar.contains("rename: Enter rename · Esc cancel"), "{bar:?}");
        assert!(!bar.contains("select"), "the list keys are still listed: {bar:?}");

        // 端末: 予約キー以外は全部 claude が受ける
        let mut app = App {
            focus: Focus::Terminal,
            ..base()
        };
        let bar = drawn_row(&mut app, 29);
        assert!(bar.contains("terminal: all keys pass through to claude"), "{bar:?}");
        assert!(!bar.contains("select"), "the sidebar keys are still listed: {bar:?}");

        // どの状態でも予約キーは出る（受け手に関係なく効く）
        for focus in [Focus::Sidebar, Focus::Terminal] {
            let mut app = App { focus, ..base() };
            let bar = drawn_row(&mut app, 29);
            assert!(bar.contains("Ctrl+Q quit · Alt+←→ focus"), "{focus:?}: {bar:?}");
        }
    }

    /// 新規セッション画面は案内をペイン内に持つので、下部バーへは重ねない
    #[test]
    fn the_new_session_screen_keeps_its_hint_inside_the_pane() {
        let mut app = App {
            term_size: (120, 30),
            focus: Focus::Terminal,
            right_view: RightView::New(new_view::NewState::browse(".")),
            ..Default::default()
        };
        let bar = drawn_row(&mut app, 29);
        assert!(bar.contains("Ctrl+Q quit"), "{bar:?}");
        assert!(!bar.contains("terminal:"), "the pane hint is repeated on the bottom bar: {bar:?}");
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
                "pos {pos:?} is outside the pane for pane {pane:?} / inner {inner:?}"
            );
        }
    }

    /// 未保管警告 `⚠` の表示幅は **1 桁**。既定幅（内側 32 桁）にアカウント行を
    /// 収める前提がこの実測値に乗っているので固定する（幅の判定は文字ごとに違い、
    /// 中には端末によって変わる曖昧なものもある ＝ 実測しないと分からない）
    #[test]
    fn the_warning_mark_is_one_column_wide() {
        use unicode_width::UnicodeWidthStr;
        assert_eq!(WARN_MARK.width(), 1, "WARN_MARK is not 1 column wide");
        assert_eq!(
            WARN_MARK.chars().count(),
            1,
            "a variation selector slipped in — emoji presentation makes it 2 columns wide"
        );
    }

    /// テスト内でアカウント行の文面だけを見るための短縮
    fn row_text(status: &AccountStatus, unstored: bool) -> String {
        account_row(status, unstored, None).0
    }

    /// **アクティブなアカウントが未保管のときだけ `⚠` を前置する。**
    /// 保管済みなら付けない（常時出ていると警告の意味が無くなる）
    #[test]
    fn account_row_marks_only_an_unstored_active_account() {
        use crate::accounts::{Account, ActiveAccount};
        let logged_in = AccountStatus::LoggedIn(ActiveAccount::unseen(Account::new(
            "you@example.com",
            "you · Acme, Inc.",
        )));

        assert_eq!(row_text(&logged_in, true), "⚠ you · Acme, Inc.");
        assert_eq!(
            row_text(&logged_in, false),
            "you · Acme, Inc.",
            "a stored account still shows the warning"
        );
        // 未取得は空行のまま（誤情報を出さない）。未ログインは行そのものが警告なので
        // ⚠ は前置しない ＝ ⚠ は「未保管」だけを意味する
        assert_eq!(row_text(&AccountStatus::Unknown, true), "");
        assert_eq!(row_text(&AccountStatus::LoggedOut, true), LOGGED_OUT_ROW);
        assert!(!LOGGED_OUT_ROW.contains(WARN_MARK));
    }

    /// **進行中のアカウント操作は行に出る**（版行の `updating…` と同じ方針）。
    /// 操作は別スレッドで走り最大 11 秒かかりうるので、出さないと「押したのに
    /// 変わらない行」に見える。⚠ より優先するのは、進行中の値がまだ
    /// 「今の持ち主」ではないため（確定していない間の見た目を作り分けない）
    #[test]
    fn the_account_row_shows_a_running_account_action() {
        use crate::accounts::{Account, ActiveAccount};
        let active = AccountStatus::LoggedIn(ActiveAccount::unseen(Account::new(
            "a@example.com",
            "you",
        )));
        let (text, _) = account_row(&active, true, Some("switching…"));
        assert_eq!(text, "switching…", "the running action is not shown on the row");
        assert!(!text.contains(WARN_MARK), "a value that is not settled yet is marked with ⚠");
    }

    /// 未ログインの行は **再ログインの手順まで出す**。保管トークンの期限切れも
    /// この状態で現れる（事前検知はしない方針なので、ここが唯一の気づきどころ）
    #[test]
    fn account_row_prompts_a_login_when_logged_out() {
        let (text, style) = account_row(&AccountStatus::LoggedOut, false, None);
        assert!(text.contains("not logged in"), "{text:?}");
        assert!(text.contains("/login"), "the row does not say how to log back in: {text:?}");
        assert_eq!(
            style,
            Style::default().fg(C_ATTENTION),
            "a row that needs action is not in the attention color"
        );
    }

    /// 既定のサイドバー幅（34 桁 = 内側 32 桁）でアカウント行が切られない。
    /// `⚠ ` の 2 桁ぶんが増えても、現実的なラベルなら収まることの固定
    #[test]
    fn account_row_fits_the_default_sidebar_width() {
        use crate::accounts::{Account, ActiveAccount};
        use unicode_width::UnicodeWidthStr;
        // README・撮影データに出る実寸のラベルと、表示幅 2 の文字（全角）を含む
        // ラベル。`⚠ ` の 2 桁が乗っても切れないことを見たいので幅 2 の文字が要る。
        // 源を ASCII に保つため \u エスケープで書く（表示幅 4 桁の 2 文字）
        let wide = format!("{} · 1→10, Inc.", "\u{5927}\u{5834}");
        for label in ["ooba · 1→10, Inc.", "you · Acme, Inc.", wide.as_str()] {
            let status =
                AccountStatus::LoggedIn(ActiveAccount::unseen(Account::new("you@example.com", label)));
            for unstored in [false, true] {
                let text = row_text(&status, unstored);
                assert_eq!(
                    clip_to_width(&text, DEFAULT_INNER),
                    text,
                    "clipped at the default width: {text:?} ({} cols / inner {DEFAULT_INNER} cols)",
                    text.width()
                );
            }
        }
        // 未ログインの案内も切ってはいけない（打つ手が読めなくなる）
        assert_eq!(
            clip_to_width(LOGGED_OUT_ROW, DEFAULT_INNER),
            LOGGED_OUT_ROW,
            "{} cols / inner {DEFAULT_INNER} cols",
            LOGGED_OUT_ROW.width()
        );
    }

    /// 実際に 1 フレーム描いた結果でも `⚠` の出方が変わる。判定は
    /// [`active_unstored`]（アクティブな email が保管の写しに居るか）なので、
    /// 保管に加えた瞬間に消えることまで含めて固定する
    #[test]
    fn the_drawn_account_row_warns_until_the_active_account_is_stored() {
        use crate::accounts::Account;

        let active = active_account();
        let drawn = |accounts: Vec<Account>| -> String {
            let mut app = app_with_account_row(accounts);
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
            "an unstored active account is not warned about: {unstored:?}"
        );
        // 別アカウントだけが保管されていても、アクティブな 1 件が未保管なら警告する
        let other = drawn(vec![Account::new("other@example.com", "other")]);
        assert!(other.contains(WARN_MARK), "storing a different email cleared the warning: {other:?}");
        // アクティブなアカウントを保管したら消える
        let stored = drawn(vec![active.clone()]);
        assert!(
            !stored.contains(WARN_MARK) && stored.contains("you · Acme, Inc."),
            "the warning is still there after the active account was stored: {stored:?}"
        );
    }

    /// アカウント行に出るアクティブなアカウント
    fn active_account() -> crate::accounts::Account {
        crate::accounts::Account::new("you@example.com", "you · Acme, Inc.")
    }

    /// [`active_account`] でログイン済みの `App`。保管の写し（⚠ の出方を決める）は
    /// テストごとに変わるので引数で受ける
    fn app_with_account_row(accounts: Vec<crate::accounts::Account>) -> App {
        use crate::accounts::ActiveAccount;
        use crate::poll::FooterInfo;

        App {
            term_size: (120, 30),
            footer: FooterInfo {
                account: AccountStatus::LoggedIn(ActiveAccount::unseen(active_account())),
                current: "2.1.220".to_string(),
                latest: None,
            },
            accounts,
            ..Default::default()
        }
    }

    /// アカウント行の描画（文字と色）を 1 フレーム描いて取り出す。
    /// **見た目が同じか**を突き合わせたいので、帯の色を式で持たずに実描画を比べる
    fn drawn_account_row(app: &mut App) -> Vec<(String, Color, Color)> {
        let (w, h) = app.term_size;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).expect("test terminal");
        terminal
            .draw(|frame| {
                draw(frame, app);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let y = sidebar_layout(app).account_y;
        (0..w)
            .map(|x| {
                let cell = &buffer[(x, y)];
                (cell.symbol().to_string(), cell.fg, cell.bg)
            })
            .collect()
    }

    /// **アカウント行はマウスを乗せると一覧の行と同じ帯でハイライトされる。**
    /// フッターに描かれる行なので一覧の行 index では表せず、以前はホバーから
    /// 除外されていた（[`SidebarPos::Account`] を指せるようになったことの固定）。
    ///
    /// 「同じ見た目」は帯の色を書き写さずに**キーボード選択中の描画と突き合わせて**
    /// 見る（選択の見え方が変われば、ホバーも一緒に追う）
    #[test]
    fn the_account_row_is_highlighted_while_hovered() {
        let mut app = app_with_account_row(vec![active_account()]);
        let plain = drawn_account_row(&mut app);

        app.selection = SidebarPos::Account;
        let selected = drawn_account_row(&mut app);
        assert_ne!(plain, selected, "the premise broke — selection no longer highlights the row");

        app.selection = SidebarPos::Row(0);
        app.hovered = Some(SidebarPos::Account);
        assert_eq!(
            drawn_account_row(&mut app),
            selected,
            "hovering the account row does not look like the selected row"
        );

        // 外れたら消える（帯が残らない）
        app.hovered = Some(SidebarPos::Row(0));
        assert_eq!(
            drawn_account_row(&mut app),
            plain,
            "the highlight stays after the mouse left the account row"
        );
        app.hovered = None;
        assert_eq!(drawn_account_row(&mut app), plain);
    }

    /// 1 フレーム描いて、指定行で**帯（ハイライト背景）が乗っている桁**を返す。
    ///
    /// 帯の色や幅を式で書き写さずに「どの桁が塗られたか」だけを取り出すので、
    /// **行どうしの見た目を突き合わせる**のに使える（見え方を変えたら両方が一緒に動く）
    fn highlighted_columns(app: &mut App, y: u16) -> Vec<u16> {
        let (w, h) = app.term_size;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).expect("test terminal");
        terminal
            .draw(|frame| {
                draw(frame, app);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        (0..w).filter(|x| buffer[(*x, y)].bg == ui().hl_bg).collect()
    }

    /// **アカウント行の帯は一覧の行と同じ行幅いっぱいに出る。**
    ///
    /// アカウント行は `Paragraph` + `Rect`、一覧の行は `List` の `ListItem` で描かれる。
    /// `Paragraph` はスタイルを `Line` に載せると**文字が占める桁だけ**が塗られるので、
    /// リスト幅まで埋める `ListItem` より短い帯になっていた（実機のスクリーンショットで
    /// `+ new session` は幅いっぱい・アカウント行は文字幅だけ）。
    ///
    /// 「同じ幅」は桁数を書き写さず、**同じ App の一覧の行と突き合わせて**見る
    #[test]
    fn the_account_row_band_is_as_wide_as_a_list_row() {
        let mut app = app_with_account_row(vec![active_account()]);
        // 突き合わせる相手は `+ new session`（文字が短いので帯を埋めているかが出る）。
        // 行 index は描画結果から引く
        highlighted_columns(&mut app, 0);
        let new_row = app
            .sidebar_rows
            .iter()
            .position(|row| row.action() == Some(&RowAction::New))
            .expect("the + new session row was not stacked");
        let new_y = row_y(new_row, app.sidebar_header_rows, app.sidebar_scroll);
        let account_y = sidebar_layout(&app).account_y;

        app.selection = SidebarPos::Row(new_row);
        let list_band = highlighted_columns(&mut app, new_y);
        assert!(
            list_band.len() > "+ new session".len(),
            "the premise broke — the list row's band stops at its text: {list_band:?}"
        );

        app.selection = SidebarPos::Account;
        assert_eq!(
            highlighted_columns(&mut app, account_y),
            list_band,
            "the account row's band is not the same width as a list row's"
        );
    }

    /// **更新の無い版行も、触れれば他の行と同じ帯が出る。** 以前は動作の無い行を
    /// 区切り線と同じ扱いにしていたので、選択もホバーもハイライトも全部から漏れていた。
    /// 「同じ見え方」は色を書き写さず、**更新のある版行の帯と突き合わせて**見る
    #[test]
    fn a_version_row_without_an_update_is_highlighted_like_any_other_row() {
        // ccdesk = 更新あり（行 0）、claude = 更新なし（行 1）。
        // 版行は固定ヘッダーの先頭 2 行なので画面 y は上枠の次から 2 行ぶん
        let mut app = App {
            term_size: (120, 30),
            ccdesk_latest: Some("v9.9.9".to_string()),
            ..Default::default()
        };
        let (actionable_y, inert_y) = (1u16, 2u16);

        app.selection = SidebarPos::Row(0);
        let actionable = highlighted_columns(&mut app, actionable_y);
        assert!(
            actionable.len() > 1,
            "the premise broke — an actionable version row has no band: {actionable:?}"
        );
        assert_eq!(app.sidebar_rows[1], SidebarRow::Inert, "row 1 is not the inert version row");

        // 選択で光る
        app.selection = SidebarPos::Row(1);
        assert_eq!(
            highlighted_columns(&mut app, inert_y),
            actionable,
            "selecting a version row without an update draws no band"
        );

        // ホバーでも光る（選択は別の行に置いたまま）
        app.selection = SidebarPos::Row(0);
        app.hovered = Some(SidebarPos::Row(1));
        assert_eq!(
            highlighted_columns(&mut app, inert_y),
            actionable,
            "hovering a version row without an update draws no band"
        );

        // 区切り線（行 2 = 画面 y 3）は触れても光らない
        app.hovered = Some(SidebarPos::Row(2));
        assert_eq!(app.sidebar_rows[2], SidebarRow::Decoration, "row 2 is not the separator");
        assert!(
            highlighted_columns(&mut app, 3).is_empty(),
            "a decoration row is highlighted"
        );
    }

    /// 端末を 1 フレーム描いて、指定行の文字列を返す
    fn drawn_row(app: &mut App, y: u16) -> String {
        let (w, h) = app.term_size;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).expect("test terminal");
        terminal.draw(|frame| {
            draw(frame, app);
        })
        .expect("draw");
        let buffer = terminal.backend().buffer();
        (0..w).map(|x| buffer[(x, y)].symbol()).collect()
    }

    /// **起動処理中であることを下部バーへ出す。** 見出しメニューの new session は
    /// 右ペインの表示を変えないので、ここに出さないと起動から表示までまったく
    /// 無反応に見える。判定は `input_gate` 1 つ（起動中の正本を増やさない）
    #[test]
    fn the_bottom_bar_shows_that_a_session_is_starting() {
        let mut app = App {
            term_size: (120, 30),
            input_gate: Some(std::time::Instant::now()),
            ..Default::default()
        };
        // 下部バーは最下行
        let bar = drawn_row(&mut app, 29);
        assert!(bar.contains("starting session…"), "the bottom bar does not say a session is starting: {bar:?}");
    }

    /// **サイドバーより広いメニューは右ペインに被せて全部読ませる**（[`popup_rect`] の
    /// 意図）。描画順が右ペインより前だと、サイドバー幅を超える列が右ペインに
    /// 塗り潰されてラベルが割られる（実測: サイドバー 12 桁で `│ new sessi│n   │`）。
    /// クリック判定は同じ矩形を見るので、**見た目は claude の画面なのにクリックすると
    /// メニューが動く**状態になる
    #[test]
    fn a_menu_wider_than_the_sidebar_is_drawn_over_the_right_pane() {
        use crate::app::{PopupKind, MIN_SIDEBAR};
        let mut app = App {
            term_size: (60, 20),
            sidebar_width: MIN_SIDEBAR,
            grouping: Grouping::Directory,
            popup: Some(Popup {
                kind: PopupKind::Project {
                    cwd: "C:\\dev\\api".to_string(),
                    has_sessions: false,
                },
                anchor_y: 1,
                selected: 0,
            }),
            ..Default::default()
        };
        let rect = popup_rect(&app, app.popup.as_ref().expect("a menu is open"));
        assert!(
            rect.right() > MIN_SIDEBAR,
            "the premise broke — the menu is no longer wider than the sidebar: {rect:?}"
        );
        let (w, h) = app.term_size;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).expect("test terminal");
        terminal.draw(|frame| {
            draw(frame, &mut app);
        })
        .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        // 矩形の列だけを読む（クリック判定が見るのと同じ範囲）
        let row = |y: u16| -> String {
            (rect.x..rect.right()).map(|x| buffer[(x, y)].symbol()).collect()
        };
        assert_eq!(row(rect.y + 1), "│ new session   │", "row 1 is overwritten by the right pane");
        assert_eq!(row(rect.y + 2), "│ remove project│", "row 2 is overwritten by the right pane");
    }
}
