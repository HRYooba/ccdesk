//! サイドバー・右ペインの描画と、描画／クリック判定で共有するジオメトリ計算。
pub(crate) mod new_view;
pub(crate) mod text_field;

use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem};
use ratatui::Frame;
use tui_term::widget::PseudoTerminal;

use ccdesk::{dir_key, LockExt};

use crate::app::{
    selected_enter, App, Focus, Popup, PopupKind, RightView, RowAction, SelfUpdate, SidebarPos,
    SidebarRow,
};
use crate::poll::{row_state, AccountStatus, Group, Grouping, Run};
use crate::sessions::SessionId;
use crate::theme::{
    ui, usage_color, C_ATTENTION, C_FAIL, C_WORKING, FOCUS_BORDER, MUTED_FG,
};
use crate::ui::new_view::draw_new_view;
use crate::usage::{Usage, UsageInfo, UsageWindow};

/// **セッション行の行頭に並ぶ 2 つの印。**
///
/// 1 桁目は「この行が今ペインに出ているか」だけを答える（[`OPEN_MARK`] / [`CLOSED_MARK`]）。
/// 消えている側も同じ幅の空白を取る ＝ 印が付いたり消えたりしても名前の開始桁が
/// 動かない。**状態ラベルの前ではなく行頭に置く**のが判断: 印が答えるのは
/// 「この行はどうか」なので、行を縦に流し読みするときに 1 つの桁へ揃っている方が
/// 拾える（名前の後ろに置くと名前の長さで印の位置が毎行変わる）。
///
/// 2 桁目は**ドット 1 つに 3 つの直交チャンネルを持たせた印**（組み立ては
/// [`session_row_line`] に閉じる）:
///
/// | チャンネル | 表すもの | 値 |
/// |:--|:--|:--|
/// | 形の大きさ | 止まっているか | 丸（[`DOT_FILLED`]/[`DOT_HOLLOW`]）/ 一回り小さい丸（[`DOT_STOPPED_FILLED`]/[`DOT_STOPPED_HOLLOW`]）＝ Stopped |
/// | 塗り | 未読（見ていない間にその行が動いた） | 塗り ＝ 未読 / 中空 ＝ 既読（大小のどちらでも保たれる） |
/// | 色 | 状態（[`crate::poll::Group`]） | Waiting/Working/Completed/Stopped の 4 色 |
/// | 明滅 | Working だけ | 400ms 周期で状態色 ↔ [`crate::theme::UiTheme::dim`]（淡色テキストと同じ色）を往復 |
///
/// 4 つを同じ 1 桁へ載せるのは、行を縦に流し読みするときに「未読か」「どの状態か」
/// 「動いているか」を 1 箇所で拾えるようにするため（別々の桁に分けると視線が散る）。
/// **状態アイコン（かつての `✻`/`✽`/`∙`）と行末の `<状態> · <経過>` テキストは廃止した**:
/// 状態は色とドットで語るので、文字での重複表現は持たない。
///
/// **ピン留めはここに印を持たない**: pin した行は [`PINNED_TITLE`] の節へ移るので、
/// 節に入っていること自体が表示になる（同じ知識を印と並びの 2 箇所に持たない）。
///
/// **幅 1 桁であることはテストが固定する**（`the_row_head_marks_are_one_column_wide`）。
/// 測るのは `unicode-width` の既定 ＝ East Asian Ambiguous を 1 桁と数える側で、
/// ratatui の桁計算もこれと同じものを使うので、描画と予算の答えは必ず一致する。
///
/// **ドットの丸（`●○•`）は Ambiguous を承知で使っている**（`width_cjk` は 2 を返す）。
/// 同じ意味を持つ Ambiguous でない丸が Unicode に無いため、代わりが無い。
/// CJK ロケールで Ambiguous を 2 桁に描く端末では行がずれる ＝ 既知の制約。
/// 選べる場面（[`MENU_MARK`] のように ASCII で足りるところ）では Ambiguous を避ける。
///
/// 1 桁目: **その行が今ペインに出ているか**（`❯` U+276F ＝ ペインが指している行）
const OPEN_MARK: &str = "❯";
const CLOSED_MARK: &str = " ";
/// ドットの塗り（未読チャンネル）。色と明滅は [`session_row_line`] が別に決める
const DOT_FILLED: &str = "●";
const DOT_HOLLOW: &str = "○";
/// 止まった行（[`Group::Stopped`]）のドット。**同じ丸のまま一回り小さい**ので、
/// 塗り（未読）はそのまま読めて「もう動かない行」だけが引っ込んで見える。
///
/// **形で分けるのが要る理由**: Stopped の色は [`crate::theme::UiTheme::dim`] で、
/// Working の明滅の谷と同じ色になる（[`session_row_line`]）。色だけに任せると
/// 「谷にいる Working」と「Stopped」が見分けられないので、そこは形が引き受ける。
/// 副産物として、モノクロ端末や色覚差でも Stopped だけは判別できる
const DOT_STOPPED_FILLED: &str = "•";
const DOT_STOPPED_HOLLOW: &str = "◦";

/// ドットのグリフ。**形の種類が「止まっているか」、塗りが「未読か」**を表す
/// （色は [`Group::color`]、明滅は [`Group::blinks`] が別に決める ＝ 4 つの
/// チャンネルがどれも他の値を書き換えない）
fn dot_glyph(group: Group, unread: bool) -> &'static str {
    if group == Group::Stopped {
        mark(unread, DOT_STOPPED_FILLED, DOT_STOPPED_HOLLOW)
    } else {
        mark(unread, DOT_FILLED, DOT_HOLLOW)
    }
}

/// ピン留めした行を集める節の見出し。**グルーピング（state / directory）に
/// 関係なく同じ位置（一覧の先頭）に出る**ので、pin の効き方が
/// 「どう並べているか」で変わらない
const PINNED_TITLE: &str = "pinned";

/// 行頭が食う桁 ＝ ペイン印 1 + ドット 1 + 名前との間の空白 1。
/// **[`row_name_and_gap`] の予算と [`crate::app::MIN_SIDEBAR`] の根拠がこの値に乗る**ので、
/// 行頭に何かを足したらテスト（`the_row_head_marks_are_one_column_wide`）が落ちる。
/// `pub(crate)` なのは [`crate::source`] の撮影用サイドバー幅がここから桁を導くため
/// （手で数えた桁を別ファイルに書き写さない）
pub(crate) const HEAD_COLS: usize = 3;

/// 名前に最低限残す桁（詰め切ったサイドバーでも行を見分けられる下限）
const MIN_NAME_COLS: usize = 4;

/// **セッション行 1 本が要る内側の桁数**（行頭 + 名前の下限 + 行末のメニュー）。
/// [`MIN_SIDEBAR`] はこれに枠の 2 桁を足したもの ＝
/// **桁の予算を持っているのはこの 1 箇所だけ**で、下限の値を別に書き写さない
pub(crate) const MIN_ROW_COLS: u16 = (HEAD_COLS + MIN_NAME_COLS + MENU_COLS) as u16;

/// サイドバー幅の下限（ドラッグで詰められる限界）。
/// 根拠は 1 行が固定で食う桁（[`MIN_ROW_COLS`]）+ 枠の左右 1 桁ずつ。
/// 「枠が 2 桁」という事実を描く側と同じファイルで数える
pub(crate) const MIN_SIDEBAR: u16 = MIN_ROW_COLS + 2;
/// 右ペインに最低限残す桁（サイドバーを広げても claude の画面が潰れない下限）
const MIN_PANE: u16 = 40;

/// **描画とヒットテストが使うサイドバー幅**（＝ 画面に出ている桁数）。
///
/// `App::sidebar_width` はユーザーが選んだ幅の正本で、ここはそれを
/// 今の端末に収まる範囲へ丸めた**導出値**。丸めた結果を保存値へ書き戻さないのが
/// 要点で、**端末が一時的に狭くなっただけでユーザーの選んだ幅を失わない**
/// （書き戻していた頃は、PTY の破棄が端末サイズ変化イベントを連れてくる Windows で
/// セッションを止めるたびにサイドバーが数桁ずつ縮み、端末が元に戻っても復元しなかった）
pub(crate) fn sidebar_cols(app: &App) -> u16 {
    fit_sidebar(app.sidebar_width, app.term_size.0)
}

/// 幅 1 つを端末幅へ収める（下限 [`MIN_SIDEBAR`]、右ペインに [`MIN_PANE`] を残す）。
/// **丸めの規則はここ 1 箇所**（導出とドラッグの確定が同じ式を見る）
pub(crate) fn fit_sidebar(width: u16, term_w: u16) -> u16 {
    let max = term_w.saturating_sub(MIN_PANE).max(MIN_SIDEBAR);
    width.clamp(MIN_SIDEBAR, max)
}

fn mark(on: bool, yes: &'static str, no: &'static str) -> &'static str {
    if on { yes } else { no }
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

/// 使用率の詳しさ。**幅が足りないときに何から落とすか**の順序そのもの
/// （左が最も詳しい）。使用率は補助情報なので、キーヒントを押し出してまで
/// 詳しく出さない
#[derive(Clone, Copy)]
enum UsageDetail {
    /// 5h / 7d / モデル別 + それぞれのリセット時刻
    Full,
    /// リセット時刻を落とす
    NoResets,
    /// モデル別も落とす（5h / 7d の数字だけ）
    WindowsOnly,
}

/// 右下の使用率行。**何も出さない状態と、出せない状態を区別する**:
///
/// | [`Usage`] | 見え方 | 理由 |
/// |:--|:--|:--|
/// | `Unknown` | 何も出さない | opt-in していない、または起動直後で判断が付いていない |
/// | `Unavailable` | 何も出さない | 枠の概念が無いアカウント ＝ **恒久的に取れない**ので警告し続けない（理由は `ccdesk doctor` が言う）。一度 `Ready` を見た後にログアウト等で取れなくなったときは `usage.rs` の裁定が前の `Ready` を保つので、ここには来ない |
/// | `Failed` | `usage —` | opt-in したのに取れていないことを出す（黙って消さない） |
/// | `Ready` | 枠の一覧 | 最後の取得が古ければ全体を dim |
///
/// クリック起点の取得中にリングの代わりに回すコマ（すべて 1 桁幅 ＝
/// どのコマでも全体の幅が変わらない）。回す理由は「押した」ことを画面で返すため:
/// 取得は 1 回 3 秒前後かかるので、静止したままだと押せていないように見える
const USAGE_SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// 今この瞬間のスピナーのコマ（回す周期はスピナーの描き直し間隔と同じ 200ms）
fn usage_spinner_frame() -> &'static str {
    USAGE_SPINNER[(ccdesk::now_ms() / 200) as usize % USAGE_SPINNER.len()]
}

/// `max_width` に収まる最も詳しい形を選ぶ（[`UsageDetail`]）。
/// `fetching`（クリック起点の取得中）はリングをスピナーに変える（幅は不変）
fn usage_line(usage: &Usage, max_width: u16, fetching: bool) -> Vec<Span<'static>> {
    let spin = fetching.then(usage_spinner_frame);
    let info = match usage {
        Usage::Unknown | Usage::Unavailable => return Vec::new(),
        Usage::Failed => {
            return vec![
                Span::styled(" usage ", Style::default().fg(ui().dim)),
                // 取れていない状態のクリック（再挑戦）にも押した反応を返す
                Span::styled(spin.unwrap_or("—"), Style::default().fg(C_FAIL)),
                Span::raw(" "),
            ]
        }
        Usage::Ready(info) => info,
    };
    // 判定する側と記録する側（usage.rs の fetched_at）で epoch の取り方を分けない
    let now = ccdesk::now_secs();
    let stale = info.is_stale(now);
    for detail in [
        UsageDetail::Full,
        UsageDetail::NoResets,
        UsageDetail::WindowsOnly,
    ] {
        let spans = usage_spans(info, stale, detail, spin);
        if span_width(&spans) <= max_width {
            return spans;
        }
    }
    // どれも収まらない幅（極端に狭い端末）では出さない
    Vec::new()
}

/// 枠 1 つ（ラベルと使用率）。**リセット時刻は付けない**（枠のグループごとに
/// 1 つだけ出すので、時刻を出すのは [`push_reset`] の役目）。
/// `spin` があればリングの代わりにそのコマを出す（取得中の表示）
fn push_window(
    spans: &mut Vec<Span<'static>>,
    label: &str,
    pct: f64,
    stale: bool,
    spin: Option<&'static str>,
) {
    let ring = spin.unwrap_or(["○", "◔", "◑", "◕", "●"][(pct / 25.0).min(4.0) as usize]);
    if !spans.is_empty() {
        spans.push(Span::styled(" · ", Style::default().fg(ui().dim)));
    }
    spans.push(Span::styled(
        format!("{label} "),
        Style::default().fg(ui().dim),
    ));
    // 古い値は色を消して dim へ落とす（古さを黙って隠さない）
    let value_color = if stale { ui().dim } else { usage_color(pct) };
    spans.push(Span::styled(
        format!("{ring} {}%", pct.round() as u32),
        Style::default().fg(value_color),
    ));
}

/// 直前までの枠に共通するリセット時刻
fn push_reset(spans: &mut Vec<Span<'static>>, resets_at: u64) {
    spans.push(Span::styled(
        format!(" →{}", fmt_reset_at(resets_at)),
        Style::default().fg(ui().dim),
    ));
}

/// [`usage_line`] の 1 形。判断（何を落とすか）は呼び手が持ち、ここは組むだけ。
///
/// **リセット時刻は枠ではなくグループに付く。** 7d 枠とモデル別枠は同じ週次枠を
/// 別の切り口で見たものなので（`limits[]` で `weekly_all` と `weekly_scoped` が
/// 同じ `group: "weekly"` に入り、`weekly_all` の時刻は `seven_day` と同一）、
/// 時刻を枠ごとに出すと同じ値が並ぶ。狭い下部バーで最も惜しいのは幅なので、
/// **週次はまとめて末尾に 1 つ**出す:
///
/// ```text
/// 5h ◔ 34% →05:35 · 7d ◑ 58% · Fable ○ 12% →7/31 07:55
/// ```
fn usage_spans(
    info: &UsageInfo,
    stale: bool,
    detail: UsageDetail,
    spin: Option<&'static str>,
) -> Vec<Span<'static>> {
    let with_resets = matches!(detail, UsageDetail::Full);
    // モデル別を落とす形では週次枠は 7d だけになる
    let models: &[(String, UsageWindow)] = if matches!(detail, UsageDetail::WindowsOnly) {
        &[]
    } else {
        &info.models
    };
    let mut spans: Vec<Span<'static>> = Vec::new();

    // 5h 枠は単独のグループ（自分の時刻を持つ）
    if let Some(w) = &info.five {
        push_window(&mut spans, "5h", w.pct, stale, spin);
        if with_resets && let Some(resets_at) = w.resets_at {
            push_reset(&mut spans, resets_at);
        }
    }

    // 週次グループ: 7d → モデル別 → 共通のリセット時刻
    if let Some(w) = &info.seven {
        push_window(&mut spans, "7d", w.pct, stale, spin);
    }
    for (name, w) in models {
        push_window(&mut spans, name, w.pct, stale, spin);
    }
    // 時刻の出どころは 7d。**モデル別しか無い形でも出せる**ように、
    // 7d が持っていなければモデル別が持つ値を使う（どれも同じ週次枠）
    if with_resets && (info.seven.is_some() || !models.is_empty())
        && let Some(resets_at) = info
            .seven
            .as_ref()
            .and_then(|w| w.resets_at)
            .or_else(|| models.iter().find_map(|(_, w)| w.resets_at))
    {
        push_reset(&mut spans, resets_at);
    }

    if !spans.is_empty() {
        spans.push(Span::raw(" "));
    }
    spans
}

/// 下部バーに出す使用率。**描画とクリック判定がこの 1 つの導出を共有する**
/// （位置の答えが 2 つあると、クリックできる場所と見えている場所がずれる）。
/// notice を出している間は下部バーが notice に置き換わるので、使用率は出ない。
///
/// マウスが乗っている間は帯（`hl_bg`）で「押せる」ことを示す。一覧の行の
/// ホバーと同じ手段（[`Look`]）。**帯は背景だけ ＝ 幅を変えない**ので、
/// 乗った瞬間に当たり判定（[`usage_hit`]）が動かない
fn usage_footer(app: &App) -> Vec<Span<'static>> {
    if app.notice.is_some() {
        return Vec::new();
    }
    let mut spans = usage_line(
        &app.usage,
        // キーヒントを押し出さないよう、使用率に渡すのは幅の半分まで
        app.term_size.0 / 2,
        app.usage_fetching.load(std::sync::atomic::Ordering::Relaxed),
    );
    if app.usage_hovered {
        for span in &mut spans {
            span.style = span.style.bg(ui().hl_bg);
        }
    }
    spans
}

/// 使用率のクリック当たり判定（右下の使用率を押すとその場で取り直す）
pub(crate) struct UsageHit {
    pub(crate) row: u16,
    pub(crate) columns: std::ops::Range<u16>,
}

/// 使用率が今どこに描かれているか。出していないときは None ＝ 当たらない
pub(crate) fn usage_hit(app: &App) -> Option<UsageHit> {
    let (width, height) = app.term_size;
    if height == 0 {
        return None;
    }
    let drawn = span_width(&usage_footer(app));
    (drawn > 0).then(|| UsageHit {
        // 下部バーは最下行（[`draw`] の縦分割と同じ）
        row: height - 1,
        // 右端に寄せて描くので、占めるのは末尾 `drawn` 列
        columns: width.saturating_sub(drawn)..width,
    })
}

/// 表示幅（`Span` の表示幅の合計）。**文字数ではなく表示幅**で測る:
/// モデル名は claude が返す表示名なので、全角を含みうる
fn span_width(spans: &[Span<'_>]) -> u16 {
    use unicode_width::UnicodeWidthStr as _;
    spans
        .iter()
        .map(|s| s.content.width() as u16)
        .sum()
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
    sidebar_layout_of(app.term_size.1.saturating_sub(1), sidebar_cols(app))
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

/// ペイン枠の色（フォーカスの有無）。**枠色の規則はこの 1 箇所**
/// （サイドバー・右ペイン・New 画面が同じ答えを読む）
pub(crate) fn border_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(FOCUS_BORDER)
    } else {
        Style::default().fg(ui().dim)
    }
}

/// **行末のメニュー記号**（クリックで二次操作のメニューが開く）。
///
/// **右端に置くのが判断**: 行頭は「その行がどうか」を答える印
/// （[`OPEN_MARK`] 他）の場所で、押すと別の画面が出る入口を同じ並びに混ぜると、
/// 読む場所と押す場所が同じ桁に重なる。当たり判定は [`menu_zone`] が
/// 描画と同じ桁から導く。
///
/// **ASCII を選んだのは桁の曖昧さを消すため。** 以前使っていたハンバーガー記号
/// （U+2630）は East Asian Ambiguous ＝ 幅の判定が端末とフォント設定で
/// 1 桁にも 2 桁にもなる。
/// ccdesk は 2 桁と実測して桁を数えていたので、1 桁と解釈する端末では
/// **行全体が横へずれる**。`=` なら常に 1 桁で、前提そのものが消える
const MENU_MARK: &str = "=";

/// 行末のメニューが食う桁（記号 + その左の空白）。
/// **左の空白まで数えるのは、記号 1 桁だけだと突きにくいため**で、
/// 当たり判定（[`menu_zone`]）も同じ 2 桁を取る。
/// `pub(crate)` の理由は [`HEAD_COLS`] と同じ（[`crate::source`] が桁を導くため）
pub(crate) const MENU_COLS: usize = 2;

/// 行末のメニュー記号の当たり判定（画面の桁）。**描画と同じ導出**なので、
/// 見えている記号と押せる場所がずれない。
///
/// 記号は枠の内側の右端 ＝ サイドバーの右枠線の 1 桁手前で、その左の空白も含める
/// （[`MENU_COLS`]）。サイドバー幅を変えるつかみ代（右枠線の 2 桁）とは重ならない
pub(crate) fn menu_zone(sidebar_cols: u16) -> std::ops::RangeInclusive<u16> {
    let right = sidebar_cols.saturating_sub(2);
    right.saturating_sub(MENU_COLS as u16 - 1)..=right
}

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
    /// 差し替え済み。反映は次回起動なので、クリックで再起動して適用する
    /// （この状態になるのは ccdesk の行だけ。claude 側は次の `claude --version` が
    /// 新版を返して行が最新表示へ戻る）
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

    /// 押したときの動作（＝行に付ける [`RowAction`]）。`update` は更新の実行、
    /// `restart` は再起動での適用で、**動詞（[`Self::verb`]）が押した結果の名前**。
    /// 実行中と最新は押す意味が無いので付けない。**それでも行は行**なので、
    /// 選択・ホバーの対象からは外れない（[`SidebarRow::Inert`]）。
    /// `restart` は ccdesk の行にしか無い（claude は次回起動で勝手に適用される）ので、
    /// 持たない行は `None` を渡す ＝ その状態になっても押せない行に留まる
    fn action(self, update: RowAction, restart: Option<RowAction>) -> SidebarRow {
        match (self, restart) {
            (Self::Available, _) => SidebarRow::Action(update),
            (Self::Restart, Some(restart)) => SidebarRow::Action(restart),
            _ => SidebarRow::Inert,
        }
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
/// **やることの無い版行は [`SidebarRow::Inert`]**（押しても何も起きないが行の実体はある）。
/// 飾りは区切り線だけ ＝ 版行は更新の有無に関係なく選択・ホバーできる
fn version_rows(
    ccdesk: UpdateState,
    claude_version: &str,
    claude: UpdateState,
    inner_width: u16,
) -> Vec<(String, Style, SidebarRow)> {
    vec![
        (
            version_row("ccdesk", env!("CARGO_PKG_VERSION"), ccdesk, inner_width),
            ccdesk.style(),
            ccdesk.action(RowAction::UpdateCcdesk, Some(RowAction::RestartCcdesk)),
        ),
        (
            version_row("claude", claude_version, claude, inner_width),
            claude.style(),
            claude.action(RowAction::UpdateClaude, None),
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
        .lock_recover()
    {
        SelfUpdate::Running => UpdateState::Running,
        SelfUpdate::Done(_) => UpdateState::Restart,
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
    let (chars, _) = width_prefix(s, width as usize);
    s.chars().take(chars).collect()
}

/// 先頭から表示幅を積み、`cols` 桁に収まる**最長の前置き**を（文字数, 表示幅）で
/// 返す。**表示幅の境界を探す走査はここ 1 箇所**（全角 = 2 桁・幅 0 文字の扱いを、
/// 幅で切る側とクリック位置 → カーソルの側で別々に持たない ＝ 片方だけ直して
/// 「カーソル位置と描画位置が 1 桁ずれる」形を作らない）
pub(crate) fn width_prefix(text: &str, cols: usize) -> (usize, usize) {
    use unicode_width::UnicodeWidthChar;
    let (mut chars, mut used) = (0usize, 0usize);
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > cols {
            break;
        }
        used += w;
        chars += 1;
    }
    (chars, used)
}

/// **サイドバーの行がどう光るか。** 3 つの状態を 1 つの型に集めてあるので、
/// 行の種類ごとに「どこが光るか」を書き分けない（判定は [`Look::at`] 1 箇所）。
///
/// 見分け方は 3 つとも別の手段に割り当ててある:
///
/// - ホバー ＝ 帯（背景 `hl_bg`）
/// - 選択 ＝ 帯 + 前景 `emph`
/// - ペインに出ている ＝ 行頭 1 桁目の [`OPEN_MARK`] と名前の太字
///
/// **帯と印は別の軸**なので、開いている行を選択してホバーした場合も
/// 「帯 + 前景の強調 + `❯` + 太字」で 3 つとも同時に読める。
/// 色だけに頼らない区別（記号・太字）を入れてあるのは、帯の色差は
/// 端末の配色によっては潰れるため
#[derive(Clone, Copy, PartialEq, Debug)]
struct Look {
    /// 選択かホバーが指している ＝ 「今ここ」の帯
    band: bool,
    /// 選択（帯の中でホバーと区別する前景の強調）
    selected: bool,
    /// 今ペインに出ている行
    open: bool,
}

impl Look {
    /// その位置の見た目。`open` は一覧の行だけが持つ（飾りやアカウント行は false）
    fn at(app: &App, pos: SidebarPos, open: bool) -> Self {
        Self {
            band: app.selection == pos || app.hovered == Some(pos),
            selected: app.selection == pos,
            open,
        }
    }

    /// 帯をスタイルへ載せる。**どこがどう光るかの規則はここ 1 箇所**
    fn band(self, style: Style) -> Style {
        if !self.band {
            return style;
        }
        let style = style.bg(ui().hl_bg);
        if self.selected {
            style.fg(ui().emph)
        } else {
            style
        }
    }
}

/// 一覧に積むセッション行 1 本ぶんの表示材料（[`draw`] が行データから組む）。
///
/// **描画に要るものだけを持つ**ので、行の見た目は [`session_row_line`] だけで
/// 決まる ＝ 窓（PTY）を起こさずに見た目を検査できる
struct RowData {
    action: RowAction,
    /// 状態そのもの。**ドットの色・明滅もここから導く**（[`Group::color`] /
    /// [`Group::blinks`]）ので、`group` と食い違う色や明滅を別に持たせられない
    /// （これを別フィールドで持っていた頃は、`look_fixture` のような手組みの
    /// `RowData` で Stopped なのに Working の色、という矛盾を作れてしまっていた）
    group: Group,
    cwd: String,
    label: String,
    /// 今ペインに出ている行（[`Look::open`] の材料）
    is_active_window: bool,
    /// 未読（[`crate::sessions::SessionRow::unread`]）＝ ドットの塗り（[`dot_glyph`]）
    unread: bool,
    /// ピン留め（[`PINNED_TITLE`] の節へ移す）
    pinned: bool,
}

/// セッション行 1 本の見た目。**行の組み立てはここ 1 箇所**なので、
/// 帯（選択・ホバー）と印（ペインに出ている・ドット）の重なり方も含めて
/// [`Frame`] を用意せずに検査できる。
///
/// `blink_lit` は今このフレームが明滅の「点灯」位相か（[`Group::blinks`] な
/// 行だけに効く）。**時計を直接読まず引数で受ける**ので、位相を固定してテストできる
/// （[`draw`] は 1 フレームぶんの全行に同じ位相を渡す）
fn session_row_line(d: &RowData, look: Look, inner_width: u16, blink_lit: bool) -> Line<'static> {
    let dot = dot_glyph(d.group, d.unread);
    // 明滅の谷は淡色テキストと同じ [`crate::theme::UiTheme::dim`]。**画面に既にある
    // 淡さへ揃える**ので、谷の深さを決めるためだけの色を別に持たない。
    // 代価: dim は Stopped の色でもあるため、谷の瞬間だけ Working が Stopped と
    // 同じ色になる。塗り（未読）と「動いていること」自体は谷でも消えないので、
    // 谷専用の色（fg と bg の中間 80%）を足してまで避ける価値は無いと判断した
    let dot_color = if d.group.blinks() && !blink_lit { ui().dim } else { d.group.color() };
    // 行頭のペイン印 + ドット + 空白（消えている側も同じ幅を取る）
    let head = vec![
        Span::styled(
            mark(look.open, OPEN_MARK, CLOSED_MARK),
            Style::default().fg(ui().emph).add_modifier(Modifier::BOLD),
        ),
        Span::styled(dot, Style::default().fg(dot_color)),
        Span::raw(" "),
    ];
    let name_style = if look.open {
        Style::default().fg(ui().emph).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let (name, gap) = row_name_and_gap(&d.label, inner_width);
    let mut spans = head;
    spans.push(Span::styled(name, name_style));
    spans.push(Span::raw(gap));
    // 行末のメニュー記号（当たり判定は [`menu_zone`] が同じ桁から導く）
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        MENU_MARK,
        Style::default().fg(if look.band { ui().emph } else { MUTED_FG }),
    ));
    Line::from(spans).style(look.band(Style::default()))
}

/// セッション行の桁割り（名前・詰め物）。**行の予算はここ 1 箇所**で決まり、
/// 描画もテストも同じ答えを読む。
///
/// 予算は「内側の幅 - 行頭 [`HEAD_COLS`] - 行末のメニュー [`MENU_COLS`]」。
/// 名前が長い行は右で切れる（[`clip_to_width`]）。合わせると必ず予算ちょうどの
/// 桁になるので、**メニュー記号は常に内側の右端に来る**
/// （[`menu_zone`] の当たり判定が成り立つ前提）
fn row_name_and_gap(label: &str, inner_width: u16) -> (String, String) {
    use unicode_width::UnicodeWidthStr;
    let body = (inner_width as usize).saturating_sub(HEAD_COLS + MENU_COLS);
    let name = clip_to_width(label, body as u16);
    let gap = body - name.width();
    (name, " ".repeat(gap))
}

/// 未ログインのときのアカウント行。**再ログインの手順まで出す**
/// （状態だけ出しても打つ手が分からない）。
/// 文面は `ccdesk doctor` の案内と同じ語彙にそろえる
const LOGGED_OUT_ROW: &str = "not logged in · run /login";

/// アカウント行の文面とスタイル。Frame に触らない純関数なので、文面と桁数を
/// テストで固定できる。
///
/// **出すだけの行**（押しても何も起きない）。ccdesk がアカウントについて答えるのは
/// 「今サインインしているのは誰か」だけなので、警告も進行中の語も持たない
/// （切り替えを持たない理由は [`crate::poll::AccountStatus`]）
fn account_row(status: &AccountStatus) -> (String, Style) {
    match status {
        // 出すのはラベル（`alice` または `alice · Acme, Inc.`）
        AccountStatus::LoggedIn(label) => (label.clone(), Style::default().fg(ui().dim)),
        AccountStatus::LoggedOut => {
            (LOGGED_OUT_ROW.to_string(), Style::default().fg(C_ATTENTION))
        }
        // 未取得（起動直後・CLI 失敗）は誤情報を出さないため空行にする
        AccountStatus::Unknown => (String::new(), Style::default().fg(ui().dim)),
    }
}

/// モーダルの矩形。描画とクリック判定で同じ計算を共有する。
///
/// **横の位置は記号（[`MENU_MARK`]）から導く。** メニューを開く入口は行末の `=` で、
/// 当たり判定も描画も [`menu_zone`] という 1 つの規則を見ているので、矩形もそこから
/// 決める（記号の右端に矩形の右端を合わせる ＝ 押した場所から下へ開く）。
/// 記号を右端へ移したのに左端 x=1 固定のままだった頃は、画面の反対側から
/// メニューが出ていた。
///
/// 幅は内容が決める（[`crate::app::PopupKind::width`]）ので、サイドバーより広い
/// メニューは記号に右端を合わせると左へはみ出す ＝ そのときは左端で止めて
/// **右ペインへ被せる**。アカウント表示名や email を切って読めなくするより、
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
    let width = popup_width(&popup.kind, app.grouping).min(term_w);
    let height = entries.len().saturating_add(2).min(term_h as usize) as u16;
    // 記号の右端に矩形の右端を合わせる。収まらなければサイドバー内の x=1 まで
    // 左へ寄せ、それでも広ければ右ペインへ食い込ませる（端末の外へは出さない）
    let max_x = term_w - width;
    let min_x = 1u16.min(max_x);
    let mark_right = *menu_zone(sidebar_cols(app)).end();
    let x = mark_right.saturating_add(1).saturating_sub(width).clamp(min_x, max_x);
    let y = popup.anchor_y.saturating_add(1).min(term_h - height);
    Rect::new(x, y, width, height)
}

/// メニュー枠が食う桁数: 左右の枠線 2 + 項目行の先頭空白 1
/// （[`draw_popup`] が `" {label}"` を出す）。**枠と空白を出す側と同じファイル**で
/// 数える: 別の場所に置くと、描画に印や桁を足したときにこの数だけが古くなり、
/// メニューの右端が黙って 1 桁切れる
const POPUP_CHROME: u16 = 3;
/// メニュー幅の下限。短い項目だけのメニュー（grouping 切替）が細く痩せて
/// 押しにくくならないようにするための床で、**広げる側の判断ではない**
/// （項目が長ければ [`popup_width`] がそちらを採る）
const POPUP_MIN_WIDTH: u16 = 14;

/// メニュー幅。**項目の表示幅から決める**ので、動的な項目でも切れない。
/// 種類ごとに固定値を置くと項目を足した時点で嘘になる。端末へ収める責任は
/// [`popup_rect`] 側
fn popup_width(kind: &PopupKind, grouping: Grouping) -> u16 {
    use unicode_width::UnicodeWidthStr;
    let widest = kind
        .entries(grouping)
        .iter()
        .map(|entry| entry.label.width().min(u16::MAX as usize) as u16)
        .max()
        .unwrap_or(0);
    widest.saturating_add(POPUP_CHROME).max(POPUP_MIN_WIDTH)
}

/// 枠内に入りきらないメニューの表示開始位置。**選択が常に見える**よう選択へ
/// 追従する（描画とクリック判定が同じ計算を共有する）。
/// 追従しない頃は、極端に低い端末で**描かれていない項目を Enter で実行できた**
pub(crate) fn popup_scroll(selected: usize, total: usize, visible: usize) -> usize {
    if visible == 0 {
        return 0;
    }
    selected
        .saturating_sub(visible - 1)
        .min(total.saturating_sub(visible))
}

/// 1 フレーム終端のカーソル状態。**位置は可視性に関係なく必ず返す**。
///
/// ratatui は位置が None だとカーソル非表示コマンドしか出さず MoveTo を出さない。
/// 一方で差分描画は「変更セルごとに MoveTo」なので、位置を渡さないフレームでは
/// 物理カーソルが最終変更セルに置き去りになる。日本語変換中は右ペインに差分が出ず
/// サイドバー（Working の点滅ドット・400ms 周期）だけが変わるため、その置き去り先は
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
fn draw_bottom_bar(frame: &mut Frame, area: Rect, app: &App) {
    // 下部バー: 通知（起動失敗等）があれば数秒それを出し、無ければキーヒント。
    // **通知の失効は run ループが持つ**（ここで落とすと、期限切れでも描画が
    // 走らない間は notice が残り、使用率クリックの当たり判定まで殺し続ける）
    if let Some((msg, _)) = &app.notice {
        frame.render_widget(
            ratatui::widgets::Paragraph::new(Line::from(format!(" {msg}")))
                .style(Style::default().fg(C_FAIL)),
            area,
        );
        return;
    }
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
    // 右端: 使用率（opt-in）。中身は [`crate::usage`] が取ったもので、
    // **statusline には一切関与しない**。
    //
    // **無言の空白を作らない**のが要点。以前は「opt-in していない」
    // 「取得が効いていない」「枠が無いアカウント」「壊れた」が全部同じ
    // 見え方（何も出ない）で、opt-in したのに出ない人へ渡せる情報が無かった
    // **当たり判定（[`usage_hit`]）と同じ導出**を通す
    let usage_spans = usage_footer(app);
    let usage_w = span_width(&usage_spans);
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

/// 右ペインの矩形（枠を含む。最下行の横断バーは含まない）。
///
/// **「右ペインはどこか」の答えはこの 1 つ**: 描画（[`draw`]）・PTY サイズ
/// （`App::pane_size`）・New 画面のヒットテスト・マウス転送の座標変換
/// （`keys::forward_mouse`）が全部ここから導く。別々の式で持つと、下部バーの
/// 行数や枠を変えたときに「見えている場所と押せる場所」が黙ってずれる
pub(crate) fn pane_rect(app: &App) -> Rect {
    let (w, h) = (app.term_size.0, app.term_size.1);
    let sidebar = sidebar_cols(app).min(w);
    Rect::new(sidebar, 0, w - sidebar, h.saturating_sub(1))
}

pub(crate) fn draw(frame: &mut Frame, app: &mut App) -> FrameCursor {
    // 最下行は横断のキーヒントバー
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());
    // 横の分割は pane_rect と同じ答えになる（右ペインの矩形の正本はあちら）
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(sidebar_cols(app)), Constraint::Min(1)])
        .split(vert[0]);

    // サイドバー: **行の正本は `~/.ccdesk/sessions.json`**（`app.sessions`）。
    // 生死は自分の子プロセス（`child.try_wait()`）が、生きている行のライブ状態は
    // `claude agents --json` の `status` が答える。
    // Working の点滅位相もここで取る時刻（`ccdesk::now_ms`）から決める
    let now_ms = ccdesk::now_ms();

    // 窓ごとの観測を**先に**確定させる（生死と出力ヒューリスティックは可変借用が要る）。
    // 以降は行の一覧を不変で回せるので、行の組み立ては 1 本のループで済む
    struct WindowView {
        session_id: crate::sessions::SessionId,
        alive: bool,
        busy: bool,
        /// この窓が claude を起こした時刻（hook の新旧判断の材料）
        launched_at: u64,
    }
    let windows: Vec<WindowView> = app
        .windows
        .iter_mut()
        .map(|w| WindowView {
            session_id: w.session_id.clone(),
            // 生死の観測（try_wait = プロセス状態の syscall）は**窓 1 つにつき
            // 1 フレーム 1 回**。looks_busy は出力時刻を見るだけで生死を見ない
            alive: w.alive(),
            busy: w.looks_busy(),
            launched_at: w.started_at,
        })
        .collect();
    let active = app.active;
    // Working の明滅の位相（400ms 周期）。**時計を読むのはここ 1 箇所**にして、
    // 1 フレームぶんの全行へ同じ位相を配る（[`session_row_line`] は時計を読まない）
    let blink_lit = (now_ms / 400).is_multiple_of(2);

    // ---- 行データを先に組み立てる（State / Directory 両グルーピング対応）----
    let mut data: Vec<RowData> = Vec::new();
    for row in &app.sessions {
        let window = windows
            .iter()
            .enumerate()
            .find(|(_, w)| w.session_id == row.session_id);
        // 生きている行のライブ状態は `agents --json` の interactive エントリが答える
        let status = app
            .agents
            .iter()
            .find(|a| a.is_interactive() && a.session_id == row.session_id.as_str())
            .map(|a| a.status.as_str())
            .unwrap_or_default();
        // **その行を動かしている実行**。自分の窓（＝ ccdesk の子プロセス）が
        // 生きている行が主で、無ければ撮影用の固定表、それも無ければ
        // `agents --json` が拾っている前景セッション（＝ 別インスタンスや
        // ccdesk の外で動いている実行）を実行として扱う
        let run = window
            .filter(|(_, w)| w.alive)
            .map(|(_, w)| Run {
                hook: app.hook_states.get(&row.session_id, Some(w.launched_at)),
                status,
                status_at: app.agents_observed_at,
                busy: w.busy,
            })
            .or_else(|| {
                app.fixed_states.get(&row.session_id).map(|state| Run {
                    // 撮影用の固定 state は時刻を持たない（status も空なので
                    // 裁定則（[`crate::poll::row_state`]）は起き得ない）
                    hook: Some((state.as_str(), 0)),
                    status: "",
                    status_at: 0,
                    busy: false,
                })
            })
            .or_else(|| {
                // 別インスタンスで動いている行を Stopped と描かない（Stopped の行は
                // open で `claude -r` を起こすので、嘘の Stopped は二重再開の入口になる）。
                // hook の記録は起動時刻で新旧を判断できない（他人の窓）ので使わない。
                //
                // **ただし、ccdesk 自身がこの行を止めた直後は例外。** `agents --json`
                // の観測は最大 2 秒古く、今 kill したばかりの自分のセッションがまだ
                // 載っていることがある。観測時刻（[`App::agents_observed_at`]）が
                // 停止時刻（[`App::stopped_at`]）より新しくなるまでは、この救済を
                // 「他インスタンスの実行」として採らない ＝ Stopped のまま描く
                // （採ってしまうと、次のポーリングで残像が消えるまでの一瞬だけ
                // Waiting 等を経由してから Stopped になる）
                let stopped_at = app.stopped_at.get(&row.session_id).copied().unwrap_or(0);
                (!status.is_empty() && app.agents_observed_at > stopped_at).then_some(Run {
                    hook: None,
                    status,
                    status_at: app.agents_observed_at,
                    busy: false,
                })
            });
        let group = row_state(run);
        data.push(RowData {
            action: RowAction::Open(row.session_id.clone()),
            group,
            cwd: row.cwd.clone(),
            label: app.titles.of(row),
            is_active_window: window.is_some_and(|(i, _)| i == active)
                && matches!(app.right_view, RightView::Sessions),
            unread: app.hook_states.unread(row),
            pinned: row.pinned,
        });
    }
    // 集計は表示行そのものから数える（分岐の複製をしない = 行数と必ず一致）。
    // **節と同じ [`Group::ORDER`] を回す**ので、並びと語が集計行とずれない
    let counts: Vec<(Group, usize)> = Group::ORDER
        .iter()
        .map(|group| (*group, data.iter().filter(|d| d.group == *group).count()))
        .collect();
    // 何か動いているか（run ループがアイドル時の描き直し間隔を選ぶ材料。
    // [`crate::app::App::animating`]）。材料は 2 つ: 明滅する行（集計と同じ表示行から
    // 導く。[`Group::blinks`]）と、使用率の取得中スピナー
    app.animating = data.iter().any(|d| d.group.blinks())
        || app
            .usage_fetching
            .load(std::sync::atomic::Ordering::Relaxed);

    // ---- 描画 ----
    // 行の見え方の規則は [`Look`] 1 つ（帯 = 選択・ホバー / 印 = ペインに出ている）。
    // **一覧の行（下）とフッターのアカウント行（末尾）が同じ規則を読む**ので、
    // 「どこが光るか」の知識が 2 箇所に分かれない
    let inner_width = chunks[0].width.saturating_sub(2);
    let mut items: Vec<ListItem> = Vec::new();
    let mut rows: Vec<SidebarRow> = Vec::new();

    let push_data_row = |items: &mut Vec<ListItem>, rows: &mut Vec<SidebarRow>, d: &RowData| {
        let cur = rows.len();
        let look = Look::at(app, SidebarPos::Row(cur), d.is_active_window);
        items.push(ListItem::new(session_row_line(d, look, inner_width, blink_lit)));
        rows.push(SidebarRow::Action(d.action.clone()));
    };
    // セッション行以外の 1 行を積む。**items と rows が 1:1 であること**と
    // 「帯（選択・ホバー）を載せるのは触れる行だけ」の規則を、行種ごとに
    // 書き写さずここ 1 箇所で守る（片方だけ push すると全行のヒットテストがずれる）
    let push_row = |items: &mut Vec<ListItem>,
                    rows: &mut Vec<SidebarRow>,
                    line: Line<'static>,
                    base: Style,
                    kind: SidebarRow| {
        let style = if kind.selectable() {
            Look::at(app, SidebarPos::Row(rows.len()), false).band(base)
        } else {
            base
        };
        items.push(ListItem::new(line.style(style)));
        rows.push(kind);
    };

    // 先頭: ccdesk / claude の版行と区切り線。更新があるときだけ行全体がクリック可
    for (text, style, row) in version_rows(
        ccdesk_update_state(app),
        &app.footer.current,
        claude_update_state(app),
        inner_width,
    ) {
        push_row(&mut items, &mut rows, Line::from(text), style, row);
    }

    // 新規セッション
    push_row(
        &mut items,
        &mut rows,
        Line::from("+ new session"),
        Style::default(),
        SidebarRow::Action(RowAction::New),
    );
    // 区切り線: new session（アクション）とセッション一覧領域を分ける（Desktop 風）
    push_row(
        &mut items,
        &mut rows,
        Line::from(separator_text(inner_width)),
        Style::default().fg(ui().dim),
        SidebarRow::Decoration,
    );
    // グルーピング切替（クリックでメニューが開く）。現在値の綴りは Grouping::as_str
    push_row(
        &mut items,
        &mut rows,
        Line::from(vec![
            Span::raw("⊞ group: "),
            Span::styled(app.grouping.as_str(), Style::default().fg(ui().emph)),
        ]),
        Style::default().fg(ui().dim),
        SidebarRow::Action(RowAction::ToggleGroup),
    );
    // 集計行。**語は節の見出しの小文字**（[`Group::title`] が唯一の綴り）なので、
    // 見出し・行ラベル・集計で別の語が出ることがない。
    //
    // **0 件の項目は出さない。** 語が 4 つになって行が長くなったので、
    // `0 stopped` のような情報を持たない項目で幅を使わない（1 本も無ければ空行）
    push_row(
        &mut items,
        &mut rows,
        Line::from(
            counts
                .iter()
                .filter(|(_, n)| *n > 0)
                .map(|(group, n)| format!("{n} {}", group.title().to_lowercase()))
                .collect::<Vec<_>>()
                .join(" · "),
        ),
        Style::default().fg(ui().dim),
        SidebarRow::Decoration,
    );
    // ここまでが固定ヘッダー。積んだ数をそのまま正本にする
    // （ヒットテストとスクロール計算が読む。定数と二重管理にしない）
    let header_n = rows.len();

    // 節 1 つ（空行 + 見出し + 行）を積む。**節の積み方はここ 1 箇所**なので、
    // pin の節もグループの節も見出しの見え方と行数が揃う
    let push_section = |items: &mut Vec<ListItem>,
                            rows: &mut Vec<SidebarRow>,
                            title: &str,
                            members: &[&RowData]| {
        if members.is_empty() {
            return;
        }
        push_row(items, rows, Line::from(""), Style::default(), SidebarRow::Decoration);
        push_row(
            items,
            rows,
            Line::from(title.to_string()),
            Style::default().fg(ui().dim),
            SidebarRow::Decoration,
        );
        for d in members {
            push_data_row(items, rows, d);
        }
    };

    // **ピン留めした行は一覧の先頭の節へ「移す」**（グループには残さない ＝
    // 同じ行が 2 箇所に出ない）。行に印を足すのではなく節ごと分けるのは
    // Claude Desktop と同じ形で、**pin が 0 本なら節ごと出ない**。
    //
    // グルーピング（state / directory）より先に分けるので、**どちらの並べ方でも
    // pin の節は同じ位置**に出る（pin の効き方が並べ方で変わらない）
    let (pinned, unpinned): (Vec<&RowData>, Vec<&RowData>) =
        data.iter().partition(|d| d.pinned);
    push_section(&mut items, &mut rows, PINNED_TITLE, &pinned);
    match app.grouping {
        Grouping::State => {
            for group in Group::ORDER {
                let members: Vec<&RowData> = unpinned
                    .iter()
                    .copied()
                    .filter(|d| d.group == group)
                    .collect();
                push_section(&mut items, &mut rows, group.title(), &members);
            }
        }
        Grouping::Directory => {
            // 見出しに出すフォルダと並びの決定は project_rows に閉じている。
            // 選択・stop・close 等の操作では並び替えない
            // **見出しに出すのは pin の節へ移していない行の cwd**（pin した行は
            // 上の節に出ているので、その行だけのためにフォルダの見出しは作らない）
            let cwds: Vec<&str> = unpinned.iter().map(|d| d.cwd.as_str()).collect();
            // セッション行の振り分けキーも**行ごとに 1 度だけ**作る（見出し × 行の
            // 総当たりになるので、突き合わせのたびに作ると描画 1 回で数千の String に
            // なる。見出し側のキーは project_rows が持っている）
            let data_keys: Vec<String> = unpinned.iter().map(|d| dir_key_of(&d.cwd)).collect();
            for row in project_rows(&app.projects, &cwds) {
                push_row(&mut items, &mut rows, Line::from(""), Style::default(), SidebarRow::Decoration);
                push_row(
                    &mut items,
                    &mut rows,
                    Line::from(row.heading),
                    Style::default().fg(ui().dim),
                    SidebarRow::Action(RowAction::Project(row.cwd)),
                );
                // 配下のセッション行。見出しの一覧と同じ同一判定キーで振り分ける
                // （ここだけ厳密一致にすると大小違いのセッションが行き場を失う）
                for (d, _) in unpinned
                    .iter()
                    .zip(&data_keys)
                    .filter(|(_, key)| **key == row.key)
                {
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
    follow_pane(app, app.shown_session().cloned());
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

    let scroll = app.sidebar_scroll;
    let visible: Vec<ListItem> = items
        .into_iter()
        .enumerate()
        .filter(|(i, _)| row_visible(*i, header_n, scroll, tail_capacity))
        .map(|(_, item)| item)
        .collect();
    let list = List::new(visible).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style(app.focus == Focus::Sidebar)),
    );
    frame.render_widget(list, chunks[0]);

    // ---- サイドバー下部フッター: 区切り線 / アカウント行 ----
    // claude の更新行はここには無い（上部の版行に集約した）
    if sl.footer_visible {
        let fx = chunks[0].x + 1;
        let fw = chunks[0].width - 2;
        let account_y = sl.account_y;
        // 区切り線（Desktop 風にフッターを本文から分ける。線の文字は separator_text が正本）
        frame.render_widget(
            ratatui::widgets::Paragraph::new(
                Line::from(separator_text(fw)).style(Style::default().fg(ui().dim)),
            ),
            Rect::new(fx, account_y - 1, fw, 1),
        );
        // アカウント行（表示名 · 組織名）。文面の判断は account_row に閉じる。
        // **選択とホバーはできるが押しても何も起きない行**（当たり判定は
        // handle_mouse 側が同じ `sidebar_layout` の account_y で持ち、
        // キーボードの選択は [`SidebarPos::Account`]）
        let (account, mut account_style) = account_row(&app.footer.account);
        // 選択中・ホバー中は一覧の行とまったく同じ見え方にする
        // （キーボードで降りてもマウスを乗せても「今ここ」が同じ帯で分かる）
        account_style = Look::at(app, SidebarPos::Account, false).band(account_style);
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
    cursor
}

/// **ペインが指すセッションの行へ選択を寄せる。**
///
/// 揃えるのは「開く操作が起きたとき」だけ ＝ ペインが指すセッションが**変わった
/// フレーム**でしか動かさないので、`↑↓` で選択だけを動かしている間は触らない
/// （選択とペインは別物のまま。逆向き ＝ 選択がペインを動かすことも無い）。
///
/// **判断をここ 1 箇所に置けるのが要点**: 開く経路は 4 つある（行のクリック /
/// 選択行のメニューの `open` / `+ new session` からの起動 / ペインの中の
/// `/resume` による張り替え）が、どれも最後は「ペインが指すセッション」に
/// 集まるので、経路ごとに選択を動かす処理を足さなくていい。
///
/// 行がまだ積まれていない周期（一覧の読み直しが追いついていない）は見送り、
/// 次のフレームで揃える。
///
/// `shown` は今ペインが指しているセッション（[`App::shown_session`]）。引数で
/// 受けるのは**窓（PTY）を起こさずに追従の規則そのものを検査できる**ようにするため
fn follow_pane(app: &mut App, shown: Option<SessionId>) {
    if shown == app.pane_shown {
        return;
    }
    let Some(id) = &shown else {
        // ペインがセッションを出していない（新規セッション画面）＝ 揃える先が無い
        app.pane_shown = None;
        return;
    };
    if let Some(row) = row_of_session(&app.sidebar_rows, id) {
        app.selection = SidebarPos::Row(row);
        // 選択が表示窓の外にあってもスクロールで見える位置まで連れてくる
        app.sidebar_follow_sel = true;
        app.pane_shown = shown;
    }
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
    // 枠内に入りきらないときは選択が見える範囲だけを描く（クリック判定も
    // 同じ [`popup_scroll`] を通るので、見えている項目と押せる項目が一致する）
    let visible = area.height.saturating_sub(2) as usize;
    let offset = popup_scroll(popup.selected, entries.len(), visible);
    frame.render_widget(ratatui::widgets::Clear, area);
    let lines: Vec<ListItem> = entries
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .map(|(i, entry)| {
            let mut style = if entry.enabled {
                Style::default()
            } else {
                Style::default().fg(ui().dim)
            };
            if i == popup.selected {
                style = style.bg(ui().hl_bg);
                if entry.enabled {
                    style = style.fg(ui().emph);
                }
            }
            ListItem::new(Line::from(format!(" {}", entry.label)).style(style))
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
    // ペインの見出しもサイドバーと同じ導出（名前の正本は transcript 1 つ）
    let title = app
        .row(&window.session_id)
        .map_or_else(|| crate::title::UNTITLED.to_string(), |row| app.titles.of(row));
    let parser = window.parser.lock_recover();
    let screen = parser.screen();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(border_style(terminal_focused));
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
pub(crate) mod tests {
    use super::*;
    use crate::poll::{classify, COMPLETED, STOPPED, WAITING, WORKING};
    // Color は本番コードでは型名を直接書かない（`d.group.color()` で済む）ので、
    // テストだけで使う型としてここで読み込む
    use ratatui::style::Color;

    /// **幅の下限は「短い項目しか無いメニューが痩せない」ための床**で、
    /// grouping 切替（最長 `  directory` = 11 桁）がそれに当たる。
    /// セッションのメニューは項目が増えて床を越えたので、最長項目から決まる
    #[test]
    fn menu_width_is_the_longest_entry_but_never_below_the_floor() {
        use unicode_width::UnicodeWidthStr;
        assert_eq!(popup_width(&PopupKind::Group, Grouping::State), POPUP_MIN_WIDTH);
        assert_eq!(popup_width(&PopupKind::Group, Grouping::Directory), POPUP_MIN_WIDTH);
        let kind = PopupKind::Session {
            id: SessionId::new("s1"),
            pinned: false,
            open: true,
        };
        let widest = kind
            .entries(Grouping::State)
            .iter()
            .map(|entry| entry.label.width())
            .max()
            .unwrap() as u16;
        assert_eq!(popup_width(&kind, Grouping::State), widest + POPUP_CHROME);
        assert!(
            popup_width(&kind, Grouping::State) > POPUP_MIN_WIDTH,
            "the floor must not clip the longest entry"
        );
    }

    /// **枠内に入りきらないメニューは選択が常に見える範囲を描く**（描画と
    /// クリック判定が同じ計算を共有する）。追従しない頃は、極端に低い端末で
    /// 描かれていない項目を Enter で実行できた
    #[test]
    fn the_popup_scroll_keeps_the_selection_visible() {
        // 5 項目・4 行しか描けない: 先頭 4 つの間はスクロールしない
        assert_eq!(popup_scroll(0, 5, 4), 0);
        assert_eq!(popup_scroll(3, 5, 4), 0);
        // 最後の項目を選ぶと 1 行ずれて見える
        assert_eq!(popup_scroll(4, 5, 4), 1);
        // 全部描けるならずらさない
        assert_eq!(popup_scroll(4, 5, 5), 0);
        // 高さ 0（枠しか無い）でも落ちない
        assert_eq!(popup_scroll(4, 5, 0), 0);
    }

    /// 使用率行を 1 本の文字列にして中身を見る（描画の検査用）
    fn usage_text(usage: &Usage, max_width: u16) -> String {
        usage_line(usage, max_width, false)
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    fn ready(models: Vec<(String, UsageWindow)>) -> Usage {
        crate::usage::sample_ready(models)
    }

    /// **4 つの状態が画面上で区別できる。** 以前はどれも「無言の空白」に潰れていて、
    /// opt-in したのに出ない人へ渡せる情報が無かった
    #[test]
    fn the_usage_line_distinguishes_why_it_has_nothing_to_show() {
        // opt-in していない / 起動直後 — 判断が付く前に何も言わない
        assert_eq!(usage_text(&Usage::Unknown, 80), "");
        // 枠の概念が無いアカウント — 恒久的に取れないので警告し続けない
        assert_eq!(usage_text(&Usage::Unavailable, 80), "");
        // 取れていない — 黙って消さず、取れていないことを出す
        assert!(
            usage_text(&Usage::Failed, 80).contains("usage"),
            "a failed fetch must be visible"
        );
    }

    /// **使用率はマウスが乗っている間だけ帯が乗る**（押せることを示す。
    /// 一覧の行のホバーと同じ手段）。帯は背景だけで**幅を変えない** ＝
    /// 乗った瞬間に当たり判定（[`usage_hit`]）が動かない
    #[test]
    fn the_usage_gauge_is_banded_only_while_hovered() {
        let mut app = App {
            usage: crate::usage::sample_ready(Vec::new()),
            term_size: (120, 30),
            ..Default::default()
        };
        let plain = usage_footer(&app);
        assert!(!plain.is_empty(), "the fixture's premise broke — nothing is drawn");
        assert!(
            plain.iter().all(|s| s.style.bg.is_none()),
            "banded before the mouse arrived"
        );

        app.usage_hovered = true;
        let hovered = usage_footer(&app);
        assert!(
            hovered.iter().all(|s| s.style.bg == Some(ui().hl_bg)),
            "the hover puts no band on the gauge"
        );
        assert_eq!(
            span_width(&plain),
            span_width(&hovered),
            "the band changed the width"
        );
    }

    /// **取得中はリングがスピナーに変わる**（押したことを画面で返す）。
    /// 値も幅もそのまま ＝ 回っている間もクリック位置と読める情報が動かない。
    /// 取れていない状態（`—`）の再挑戦クリックにも同じ反応を返す
    #[test]
    fn a_fetch_in_flight_spins_the_rings_without_moving_anything() {
        let usage = ready(vec![(
            "Fable".to_string(),
            UsageWindow {
                pct: 12.0,
                resets_at: None,
            },
        )]);
        let plain = usage_line(&usage, 200, false);
        let spinning = usage_line(&usage, 200, true);
        let text = |spans: &[Span<'_>]| {
            spans.iter().map(|s| s.content.as_ref()).collect::<String>()
        };
        // リングは 1 つ残らずスピナーへ（枠の数だけ回る）
        assert!(
            !text(&spinning).contains(['○', '◔', '◑', '◕', '●']),
            "a ring survived the fetch: {}",
            text(&spinning)
        );
        assert!(
            USAGE_SPINNER.iter().any(|frame| text(&spinning).contains(frame)),
            "no spinner frame appeared: {}",
            text(&spinning)
        );
        // 値と幅はそのまま
        assert!(text(&spinning).contains("18%"), "{}", text(&spinning));
        assert_eq!(span_width(&plain), span_width(&spinning), "the width moved");

        // Failed の「—」も取得中はスピナー（幅は同じ 1 桁）
        let failed_plain = usage_line(&Usage::Failed, 80, false);
        let failed_spinning = usage_line(&Usage::Failed, 80, true);
        assert!(!text(&failed_spinning).contains('—'), "{}", text(&failed_spinning));
        assert_eq!(span_width(&failed_plain), span_width(&failed_spinning));
    }

    /// 3 つの枠（5h / 7d / モデル別）が並び、リセット時刻まで出る
    #[test]
    fn all_three_windows_appear_when_there_is_room() {
        let text = usage_text(
            &ready(vec![(
                "Fable".to_string(),
                UsageWindow {
                    pct: 12.0,
                    resets_at: None,
                },
            )]),
            120,
        );
        assert!(text.contains("5h"), "{text}");
        assert!(text.contains("18%"), "{text}");
        assert!(text.contains("7d"), "{text}");
        assert!(text.contains("55%"), "{text}");
        assert!(text.contains("Fable"), "{text}");
        assert!(text.contains("12%"), "{text}");
        assert!(text.contains('\u{2192}'), "reset time is missing: {text}");
    }

    /// **週次のリセット時刻は 1 つだけ、モデル別の後ろに出る。**
    ///
    /// 7d 枠とモデル別枠は同じ週次枠を別の切り口で見たものなので、枠ごとに時刻を
    /// 出すと同じ値が並ぶ（狭い下部バーで最も惜しいのは幅）。
    /// 出るのは 5h の 1 つと週次の 1 つで**合計 2 つ**
    #[test]
    fn the_weekly_reset_time_is_shown_once_after_the_model_windows() {
        let text = usage_text(
            &ready(vec![
                (
                    "Fable".to_string(),
                    UsageWindow {
                        pct: 12.0,
                        resets_at: None,
                    },
                ),
                (
                    "Opus".to_string(),
                    UsageWindow {
                        pct: 3.0,
                        resets_at: None,
                    },
                ),
            ]),
            200,
        );
        assert_eq!(
            text.matches('\u{2192}').count(),
            2,
            "expected one reset time for 5h and one for the weekly group: {text}"
        );
        // 週次の時刻は最後のモデル別枠より後ろ（＝ グループ全体に付いている）
        let last_model = text.rfind("Opus").expect("the model window is missing");
        let last_reset = text.rfind('\u{2192}').expect("the reset time is missing");
        assert!(last_reset > last_model, "the weekly reset time is not last: {text}");
        // 7d とモデル別の間に時刻が挟まっていない
        let seven = text.find("7d").expect("the weekly window is missing");
        let fable = text.find("Fable").expect("the model window is missing");
        assert!(
            !text[seven..fable].contains('\u{2192}'),
            "a reset time sits between 7d and the model windows: {text}"
        );
    }

    /// モデル別枠しか無い形でも週次の時刻を出せる（時刻の出どころが 7d だけに
    /// 縛られていないこと。7d が欠ける形は公式に「各枠が独立に欠けうる」と明記されている）
    #[test]
    fn the_weekly_reset_time_can_come_from_a_model_window() {
        let now = ccdesk::now_secs();
        let usage = Usage::Ready(UsageInfo {
            five: None,
            seven: None,
            models: vec![(
                "Fable".to_string(),
                UsageWindow {
                    pct: 4.0,
                    resets_at: Some(now + 86_400),
                },
            )],
            fetched_at: now,
        });
        let text = usage_text(&usage, 200);
        assert!(text.contains("Fable"), "{text}");
        assert_eq!(text.matches('\u{2192}').count(), 1, "{text}");
    }

    /// **狭い端末では詳しさから落とす。** 落とす順はリセット時刻 → モデル別 →
    /// （それでも入らなければ）表示しない。使用率は補助情報なので、
    /// キーヒントを押し出してまで出さない
    #[test]
    fn a_narrow_footer_drops_detail_before_it_overflows() {
        let usage = ready(vec![(
            "Fable".to_string(),
            UsageWindow {
                pct: 12.0,
                resets_at: None,
            },
        )]);
        for max_width in [0_u16, 4, 8, 12, 16, 20, 24, 32, 48, 64, 120] {
            let spans = usage_line(&usage, max_width, false);
            assert!(
                span_width(&spans) <= max_width,
                "overflowed {max_width}: {:?}",
                spans.iter().map(|s| s.content.as_ref()).collect::<String>()
            );
        }
        // 幅を削ると、まずリセット時刻が消え、次にモデル別が消える。
        // **境界の幅は数え上げず、その形の実寸から引く**（書式を変えたときに
        // テストが嘘にならないように）
        let Usage::Ready(info) = &usage else {
            panic!("built a Ready value");
        };
        let width_of = |detail| span_width(&usage_spans(info, false, detail, None));
        // 古い値・取得中は見た目だけ変わって**幅は変わらない**
        // （dim やスピナーに変わった瞬間にクリック位置が動かない）
        for detail in [
            UsageDetail::Full,
            UsageDetail::NoResets,
            UsageDetail::WindowsOnly,
        ] {
            assert_eq!(
                span_width(&usage_spans(info, true, detail, None)),
                span_width(&usage_spans(info, false, detail, None)),
                "the width changed when the value went stale"
            );
            assert_eq!(
                span_width(&usage_spans(info, false, detail, Some("⠋"))),
                span_width(&usage_spans(info, false, detail, None)),
                "the width changed while fetching"
            );
        }

        let full = usage_text(&usage, width_of(UsageDetail::Full));
        assert!(full.contains('\u{2192}'), "{full}");
        assert!(full.contains("Fable"), "{full}");

        // Full には 1 桁足りない幅 = リセット時刻が落ちる
        let no_resets = usage_text(&usage, width_of(UsageDetail::Full) - 1);
        assert!(!no_resets.contains('\u{2192}'), "{no_resets}");
        assert!(no_resets.contains("Fable"), "{no_resets}");

        // NoResets にも 1 桁足りない幅 = モデル別が落ちる
        let windows_only = usage_text(&usage, width_of(UsageDetail::NoResets) - 1);
        assert!(!windows_only.contains("Fable"), "{windows_only}");
        assert!(
            windows_only.contains("5h") && windows_only.contains("7d"),
            "{windows_only}"
        );

        // 一番簡素な形にも足りない幅 = 何も出さない（はみ出させない）
        assert_eq!(
            usage_text(&usage, width_of(UsageDetail::WindowsOnly) - 1),
            ""
        );
    }

    /// モデル別枠が無いアカウントでも 5h / 7d は出る
    #[test]
    fn the_line_works_without_model_windows() {
        let text = usage_text(&ready(Vec::new()), 120);
        assert!(text.contains("5h") && text.contains("7d"), "{text}");
    }

    /// pos が矩形の内側にあるか（幅・高さ 0 の矩形は「内側なし」なので常に false ＝
    /// `Rect::contains` を使わない理由）。new_view のテストも同じ 1 実装を使う
    pub(crate) fn contains(rect: Rect, pos: Position) -> bool {
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

    /// **行の桁の前提はすべて「記号 1 桁」に乗っている。**
    ///
    /// 行頭の 2 つの印は消えている側を同じ幅の空白で確保しており、行末の
    /// メニュー記号は内側の右端に置く（当たり判定がその桁を指す）。どれかが
    /// 2 桁になると行全体が横へずれるので、幅はここで実測して固定する。
    /// East Asian Ambiguous な記号は端末とフォント設定で 1 桁にも 2 桁にもなるため
    /// 選ばない（`=` が ASCII なのはこの前提を測らずに済ませるため）。
    ///
    /// **[`HEAD_COLS`] / [`MENU_COLS`] / [`MIN_SIDEBAR`] の足し算もここで検算する**
    /// ので、行頭や行末に何かを足したらこのテストが落ちる
    #[test]
    fn the_row_head_marks_are_one_column_wide() {
        use unicode_width::UnicodeWidthStr;
        assert_eq!(UPDATE_MARK.width(), 1, "the update mark is not 1 column wide");
        assert_eq!(MENU_MARK.width(), 1, "the menu mark is not 1 column wide");
        assert!(MENU_MARK.is_ascii(), "reverted to an ambiguous-width mark");
        // ペインに出ているかの印は、消えている側も同じ 1 桁の空白
        assert_eq!(OPEN_MARK.width(), 1, "{OPEN_MARK:?} is not 1 column wide");
        assert_eq!(CLOSED_MARK.width(), 1, "{CLOSED_MARK:?} is not 1 column wide");
        assert!(
            CLOSED_MARK.trim().is_empty(),
            "a character is showing in the empty slot: {CLOSED_MARK:?}"
        );
        // ドットは 4 グリフとも 1 桁（既読は空白ではなく ○、Stopped は一回り小さい丸）
        assert_eq!(DOT_FILLED.width(), 1, "{DOT_FILLED:?} is not 1 column wide");
        assert_eq!(DOT_HOLLOW.width(), 1, "{DOT_HOLLOW:?} is not 1 column wide");
        assert_eq!(
            DOT_STOPPED_FILLED.width(),
            1,
            "{DOT_STOPPED_FILLED:?} is not 1 column wide"
        );
        assert_eq!(
            DOT_STOPPED_HOLLOW.width(),
            1,
            "{DOT_STOPPED_HOLLOW:?} is not 1 column wide"
        );
        // 行頭 = ペイン印 1 + ドット 1 + 空白 1
        assert_eq!(
            HEAD_COLS,
            OPEN_MARK.width() + DOT_FILLED.width() + 1,
            "the row head budget is out of step with the marks"
        );
        // 行末 = 空白 1 + メニュー記号
        assert_eq!(MENU_COLS, 1 + MENU_MARK.width(), "the menu budget changed");
        // 一番狭いサイドバーでも名前に [`MIN_NAME_COLS`] 桁が残る（下限の根拠）
        assert_eq!(
            usize::from(MIN_ROW_COLS),
            HEAD_COLS + MIN_NAME_COLS + MENU_COLS,
            "the row budget is out of step with its parts"
        );
        assert_eq!(MIN_SIDEBAR, MIN_ROW_COLS + 2, "the sidebar floor lost the border columns");
        // 下限の幅で描いても名前に [`MIN_NAME_COLS`] 桁が残る（式ではなく実物で見る）
        let (name, _) = row_name_and_gap(&"n".repeat(30), MIN_SIDEBAR - 2);
        assert_eq!(name.width(), MIN_NAME_COLS, "the narrowest sidebar lost the name column");
    }

    /// **メニュー記号は内側の右端で、当たり判定はそこと左隣の空白。**
    /// 描画とヒットテストが別々の桁を持つと「見えているのに押せない」が起きる
    #[test]
    fn the_menu_mark_sits_at_the_right_edge_where_the_click_lands() {
        use unicode_width::UnicodeWidthStr;
        let mut app = App {
            term_size: (60, 40),
            sidebar_width: 34,
            sessions: vec![named_session("a", "C:\\dev\\api", "some-session")],
            titles: fixed_titles(),
            ..Default::default()
        };
        let line = session_lines(&mut app)
            .into_iter()
            .find(|line| line.contains("some-session"))
            .expect("no session row");
        assert!(line.ends_with(MENU_MARK), "the menu mark is not at the end: {line:?}");
        // 行は内側の幅ちょうど（＝ 記号は内側の右端の桁に来る）
        let drawn = sidebar_cols(&app);
        assert_eq!(
            line.width(),
            usize::from(drawn - 2),
            "the row does not fill the inner width: {line:?}"
        );
        // 当たり判定はその桁（枠の 1 桁内側から数えて内側の右端）と左隣を含む
        let zone = menu_zone(drawn);
        assert_eq!(*zone.end(), drawn - 2, "the click zone is not where the mark is drawn");
        // 幅を変えるつかみ代（右枠線の 2 桁）とは重ならない
        assert!(*zone.end() < drawn - 1, "the menu zone overlaps the resize grip");
        assert_eq!(zone.count(), MENU_COLS, "the click zone is not the drawn width");
    }

    /// **行頭の印は名前の開始桁を動かさない。**
    ///
    /// 比べるのは行そのもの（印の有無で 2 本を同じフレームに描く）で、
    /// 桁数を式で書き写さない ＝ 行の組み立てを変えたらここが落ちる
    #[test]
    fn the_row_head_marks_never_shift_the_name_column() {
        use unicode_width::UnicodeWidthStr;
        let mut app = App {
            term_size: (60, 40),
            sidebar_width: 34,
            sessions: vec![
                crate::sessions::SessionRow {
                    // 見ていない間に claude が何か言った行（hook の `at` > `last_opened_at`）
                    last_opened_at: 1_000,
                    ..named_session("a", "C:\\dev\\api", "fresh-row")
                },
                crate::sessions::SessionRow {
                    last_opened_at: 3_000,
                    ..named_session("b", "C:\\dev\\api", "seen-row")
                },
            ],
            hook_states: crate::hooks::HookStates::from_entries([
                ("a", "done", 2_000),
                ("b", "done", 2_000),
            ]),
            titles: fixed_titles(),
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
        // 印はそれぞれ決まった桁に出る（ペインに出ていないので 1 桁目は空白）。
        // 窓が無い行 ＝ Stopped なので丸は小さい側、塗りは未読/既読で割れる
        assert!(unread.starts_with(&format!("{CLOSED_MARK}{DOT_STOPPED_FILLED}")), "{unread:?}");
        assert!(read.starts_with(&format!("{CLOSED_MARK}{DOT_STOPPED_HOLLOW}")), "{read:?}");
        // 名前の開始桁は 2 本とも同じ（消えている印の桁も確保されている）
        let name_col = |line: &str, name: &str| line[..line.find(name).unwrap()].width();
        assert_eq!(name_col(&unread, "fresh-row"), HEAD_COLS);
        assert_eq!(name_col(&read, "seen-row"), HEAD_COLS);
    }

    /// **動いている行の状態は hook が主、`agents --json` が従。**
    /// hook は turn 単位で届くので Done を区別できるが、`status` からは出せない
    #[test]
    fn a_live_row_prefers_the_hook_state_over_the_live_status() {
        let label = |hook, status, busy| {
            row_state(Some(Run { hook, status, status_at: 0, busy })).title()
        };
        // hook が居れば status も出力ヒューリスティックも見ない
        assert_eq!(label(Some((crate::poll::COMPLETED, 0)), "busy", true), "Completed");
        assert_eq!(label(Some((WORKING, 0)), "idle", false), "Working");
        assert_eq!(label(Some((WAITING, 0)), "busy", false), "Waiting");
        // hook が一度も来ていない行は status から導く
        assert_eq!(label(None, "busy", false), "Working");
        assert_eq!(label(None, "idle", false), "Waiting");
        // status も無い間は出力の変化から推す
        assert_eq!(label(None, "", true), "Working");
        assert_eq!(label(None, "", false), "Waiting");
    }

    /// **hook の `waiting` だけは、より新しい `busy` 観測に負ける（裁定則）。**
    ///
    /// `waiting` は「ユーザーが動くまで進まない」という主張なので、その後に
    /// 観測された「動いている」は反証になる。許可プロンプトの許可のように
    /// 「解除された」を知らせる hook イベントが存在しない操作があり、
    /// イベントの列挙では状態機械が閉じない ＝ この 1 本が安全網になる
    #[test]
    fn a_newer_busy_observation_overrules_a_waiting_hook() {
        let view = |hook, status, status_at| {
            row_state(Some(Run { hook, status, status_at, busy: false }))
        };
        // waiting(at=1000) より新しい busy 観測 → Working（スピナーも回る）
        let promoted = view(Some((WAITING, 1_000)), "busy", 2_000);
        assert_eq!(promoted.title(), "Working");
        assert!(promoted.blinks(), "the promoted row does not blink");
        // 古い busy 観測は前の状態の名残 ＝ waiting のまま（同時刻も採らない）
        assert_eq!(view(Some((WAITING, 1_000)), "busy", 999).title(), "Waiting");
        assert_eq!(view(Some((WAITING, 1_000)), "busy", 1_000).title(), "Waiting");
        // 新しい観測でも「動いていない」は waiting を覆さない（入力待ちの表示を守る）
        assert_eq!(view(Some((WAITING, 1_000)), "idle", 2_000).title(), "Waiting");
        assert_eq!(view(Some((WAITING, 1_000)), "waiting", 2_000).title(), "Waiting");
        assert_eq!(view(Some((WAITING, 1_000)), "", 2_000).title(), "Waiting");
        // completed は覆さない（busy は「このターンの続き」と「次のターン」を
        // 区別できないので、completed を覆すと Done の意味が壊れる）。
        // working が非 busy 観測に負ける逆向きは
        // `a_newer_non_busy_observation_overrules_a_working_hook` が固定する
        assert_eq!(view(Some((crate::poll::COMPLETED, 1_000)), "busy", 2_000).title(), "Completed");
    }

    /// **逆向きの裁定則: hook の `working` も、より新しい非 `busy` 観測に負ける。**
    ///
    /// claude は Esc 中断のとき `Stop` hook を撃たない（実データで確認済み:
    /// 中断ターンの 91%（113/124）で `Stop` 未発火）ので、`working` は自己修復する
    /// 手段を持たず、次のターンを完走するか窓を閉じるまで赤・明滅のまま固着していた。
    /// 材料が `agents --json` の `status`（claude 自身がポーリングのたびに上書きする
    /// 現在値）である点が、以前避けていた `idle_prompt`（時間経過だけの誤検知）とは
    /// 違う、という判断の根拠は [`row_state`] の doc を参照
    #[test]
    fn a_newer_non_busy_observation_overrules_a_working_hook() {
        let view = |hook, status, status_at| {
            row_state(Some(Run { hook, status, status_at, busy: false }))
        };
        // working(at=1000) より新しい非 busy 観測 → Waiting（実測した 3 値すべて）
        for status in ["idle", "waiting", "shell"] {
            assert_eq!(
                view(Some((WORKING, 1_000)), status, 2_000).title(),
                "Waiting",
                "a newer {status:?} observation did not demote a working hook"
            );
        }
        // 古い（またはターン開始と同時刻の）観測は前の状態の名残 ＝ working のまま
        assert_eq!(view(Some((WORKING, 1_000)), "idle", 999).title(), "Working");
        assert_eq!(view(Some((WORKING, 1_000)), "idle", 1_000).title(), "Working");
        // busy 観測はもちろん覆さない（ターンが進行中）
        assert_eq!(view(Some((WORKING, 1_000)), "busy", 2_000).title(), "Working");
        // status が空（一度も観測していない）は「busy でないと確認できた」ではない
        // ので覆さない（`Run::status_at` の doc が言う「0 ＝ 裁定は起きない」と同じ理由）
        assert_eq!(view(Some((WORKING, 1_000)), "", 2_000).title(), "Working");
    }

    /// **動かしているものが無い行は、hook が何を言っていても Stopped。**
    ///
    /// 実データで起きていた食い違いがこれで消える。行ごとに新旧が逆の 3 本
    /// （保管 `blocked` / hook `stopped` 11:14:06・保管 `stopped` / hook `blocked`
    /// 11:13:43・保管も hook も `stopped`）が、**ccdesk を起動し直せば必ず全部
    /// Stopped**になる ＝ 窓が 1 つも無い ＝ 実行がどれにも無いため。
    /// `stop` / `/clear` / `/resume` のどれで止まっても同じ表示になるのも同じ理由
    #[test]
    fn a_row_with_no_run_is_stopped_whatever_the_hooks_say() {
        let view = row_state(None);
        assert_eq!(view.title(), "Stopped");
        assert!(view == Group::Stopped, "a stopped row is not in the last group");
        assert!(!view.blinks());
        // かつては「Stopped なのに生存形（✻）」という矛盾を `alive` フィールドで
        // 検査していたが、状態はもう文字のアイコンで語らない（色だけ）ので、
        // その矛盾自体が作れなくなり検査ごと不要になった

        // 実データの 3 本（保管と hook が食い違い、しかも新旧が行ごとに逆だった）を、
        // 窓が 1 つも無い状態 ＝ ccdesk の起動直後として描く
        let mut app = App {
            term_size: (140, 40),
            sidebar_width: 60,
            sessions: vec![
                named_session("8d162272", "C:\\dev\\api", "hook-newer"),
                named_session("25bf4b8f", "C:\\dev\\api", "both-agree"),
                named_session("a632c052", "C:\\dev\\api", "store-newer"),
            ],
            hook_states: crate::hooks::HookStates::from_entries([
                ("8d162272", STOPPED, 1_785_118_446_410),
                ("25bf4b8f", STOPPED, 1_785_118_379_396),
                ("a632c052", "blocked", 1_785_118_423_198),
            ]),
            titles: fixed_titles(),
            ..Default::default()
        };
        // 状態は文字ではなくドットが語るので、ドットで回帰を検査する
        for needle in ["hook-newer", "both-agree", "store-newer"] {
            assert!(
                drawn_as_stopped(&mut app, needle),
                "a row with no window is not drawn as Stopped: {needle}"
            );
        }
    }

    /// **ccdesk が今止めたセッションの残像は、次の観測が届くまで実行として拾わない。**
    ///
    /// stop 直後は `agents --json` の観測が最大 2 秒古く、kill したばかりの
    /// 自分のセッションがまだ busy 等で載っている。ここを「別インスタンスの実行」
    /// （run の 3 つ目の分岐）と誤認すると、Stopped になるはずの行が一瞬 Waiting
    /// 等を経由してから Stopped になってしまう（今回のバグ）。観測時刻が
    /// 停止時刻より新しくなるまでは Stopped のまま描く
    #[test]
    fn a_row_ccdesk_just_stopped_ignores_a_stale_agents_observation() {
        let mut app = App {
            term_size: (140, 40),
            sidebar_width: 60,
            sessions: vec![named_session("dead-beef", "C:\\dev\\api", "just-stopped")],
            titles: fixed_titles(),
            // 「今 kill した」の記録。観測が同時刻以下の間は救済させない
            stopped_at: [(crate::sessions::SessionId::new("dead-beef"), 5_000)].into(),
            agents_observed_at: 5_000,
            agents: vec![crate::poll::AgentInfo {
                session_id: "dead-beef".to_string(),
                kind: crate::claude_format::AGENT_KIND_INTERACTIVE.to_string(),
                status: "busy".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(
            drawn_as_stopped(&mut app, "just-stopped"),
            "a just-stopped row believed a stale agents observation"
        );
    }

    /// **自然死（`/exit`・クラッシュ）の直後も、`stop` と同じ扱いになる。**
    ///
    /// `App::stopped_at` を刻む場所は今 [`crate::app`] の `remove_window` 1 箇所に
    /// 一本化されている。以前はそこを経由しない自然死の掃除ループ（生死スキャンが
    /// 拾う `!w.alive()`）だけ刻み忘れており、`stopped_at` が 0（未記録）のまま
    /// 描かれていた。0 は「守るべき時刻が無い」なので、ガード
    /// （`agents_observed_at > stopped_at`）が素通りし、最大 `LIVE_SCAN_INTERVAL`
    /// 秒古い busy 観測を実行と誤認して、Stopped になるはずの行が数秒
    /// Working/Waiting に見えていた（実機で観測されたバグ）。
    ///
    /// **この時刻を刻むのが `remove_window` 経由の実プロセス生死判定なので、
    /// ここでは疑似 PTY を起こさずに「掃除された直後」の状態（窓が無く、
    /// `stopped_at` が今の時刻を持つ）を直接組んで、そこから先の描画側の
    /// ガードが `a_row_ccdesk_just_stopped_ignores_a_stale_agents_observation` と
    /// 同じく正しく働くことを固定する**（`remove_window` 自体が刻むことは
    /// `close_window_of`・生死スキャン・`open_session` の死んだ窓の片付けの
    /// 3 箇所すべてで呼び出し元を確認済み。呼び出し元の確認は報告に書く）
    #[test]
    fn a_naturally_dead_row_is_stopped_despite_a_stale_agents_observation() {
        let mut app = App {
            term_size: (140, 40),
            sidebar_width: 60,
            sessions: vec![named_session("cafe-face", "C:\\dev\\api", "exited-naturally")],
            titles: fixed_titles(),
            // remove_window が掃除の瞬間に刻んだ想定の記録（窓は既に無い）
            stopped_at: [(crate::sessions::SessionId::new("cafe-face"), 5_000)].into(),
            // 生死スキャンの周期（最大 LIVE_SCAN_INTERVAL 秒）だけ古い busy 観測が
            // まだ残っている
            agents_observed_at: 5_000,
            agents: vec![crate::poll::AgentInfo {
                session_id: "cafe-face".to_string(),
                kind: crate::claude_format::AGENT_KIND_INTERACTIVE.to_string(),
                status: "busy".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(
            drawn_as_stopped(&mut app, "exited-naturally"),
            "a naturally-dead row believed a stale agents observation"
        );
    }

    /// 対で固定する: 停止時刻より**新しい**観測に載っていれば、それは自分の残像
    /// ではなく本当に別インスタンス（または ccdesk の外）で動いている実行 ＝
    /// 別インスタンス救済はこれまで通り働く
    #[test]
    fn a_newer_agents_observation_still_rescues_another_instance() {
        let mut app = App {
            term_size: (140, 40),
            sidebar_width: 60,
            sessions: vec![named_session("dead-beef", "C:\\dev\\api", "elsewhere")],
            titles: fixed_titles(),
            stopped_at: [(crate::sessions::SessionId::new("dead-beef"), 5_000)].into(),
            agents_observed_at: 5_001, // 停止より後の観測 ＝ 本当に動いている
            agents: vec![crate::poll::AgentInfo {
                session_id: "dead-beef".to_string(),
                kind: crate::claude_format::AGENT_KIND_INTERACTIVE.to_string(),
                status: "busy".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        // **色ではなく記号で見る**: 明滅の谷は Stopped と同じ色なので、色で
        // 「Stopped ではない」を主張すると Working の行が位相しだいで落ちる
        assert!(
            !drawn_as_stopped(&mut app, "elsewhere"),
            "the rescue for another instance stopped working"
        );
    }

    /// **`stopped` と言った実行は実行として扱わない。**
    ///
    /// pid の消失は 2 秒周期でしか届かないので、`SessionEnd` が飛んだ直後は
    /// 「窓は生きて見えているが実行は終わっている」周期がある。ここで hook の
    /// `stopped` をそのまま `classify(_, alive = true)` へ通すと **Stopped なのに
    /// Working の色になる**（矛盾）。実行の終わりは実行が無いことと同じに畳む
    #[test]
    fn a_stopped_hook_ends_the_run_instead_of_labelling_a_live_one() {
        let view = row_state(Some(Run {
            hook: Some((STOPPED, 0)),
            status: "idle",
            status_at: 0,
            busy: false,
        }));
        assert_eq!(view.title(), "Stopped", "a fresh stopped was thrown away");
        assert!(!view.blinks());
        // かつては「Stopped なのにアイコンが生存形（✻）」を `alive` フィールドで
        // 検査していたが、状態はもう色だけで語るのでその矛盾自体が作れなくなった

        // 他の state はそのまま生きている実行として出る（Working として扱われる）
        assert!(
            row_state(Some(Run { hook: Some(("done", 0)), status: "", status_at: 0, busy: false }))
                == Group::Working
        );
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

    /// 押せるのは「やることがある」行だけ: 更新あり = update、ccdesk の
    /// 差し替え済み = restart（動詞の通りに動く）。実行中・最新は押しても
    /// 意味が無いので動作を付けない。**それでも行は行**なので
    /// [`SidebarRow::Inert`] ＝ 選択・ホバーの対象からは外れない
    #[test]
    fn version_rows_are_clickable_only_when_there_is_something_to_do() {
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
        // 差し替え済み: ccdesk は restart で適用できる。claude はこの状態に
        // ならない（版チェックが新版を返して行が最新表示へ戻る）ので、
        // 仮になっても押せない行に留まる
        assert_eq!(
            rows_of(UpdateState::Restart, UpdateState::Restart),
            (
                SidebarRow::Action(RowAction::RestartCcdesk),
                SidebarRow::Inert
            )
        );
        for state in [UpdateState::Current, UpdateState::Running] {
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

    /// **今サインインしているアカウントがフッターに出る。**
    ///
    /// ジオメトリだけを見る他のフッターテストと違い、ここは実際に 1 フレーム
    /// 描いて中身を見る: 版行が上部へ移って 2 行固定になったフッターと、
    /// アカウント行が噛み合っていることは「その行に何が出たか」でしか固定できない。
    /// 供給元は [`DemoSource`] 既定の `App`（ファイルもネットワークも触らない）
    #[test]
    fn the_account_row_shows_the_signed_in_account() {
        use crate::poll::FooterInfo;

        let mut app = App {
            term_size: (120, 30),
            footer: FooterInfo {
                account: AccountStatus::LoggedIn("you · Acme, Inc.".to_string()),
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
                // 読む桁は**描画と同じ導出幅**（[`sidebar_cols`]）の内側。保存値を
                // そのまま使うと、狭い端末では枠の外まで読んでしまう
                let text: String = (1..sidebar_cols(app).saturating_sub(1))
                    .map(|x| buffer[(x, y)].symbol())
                    .collect();
                (row.clone(), text.trim_end().to_string())
            })
            .collect()
    }

    /// その行が Stopped として描かれたか（ドットの記号で見る）。
    /// 判定は [`drawn_dot`] が返す記号だけに乗るので、明滅の位相に左右されない
    fn drawn_as_stopped(app: &mut App, needle: &str) -> bool {
        let (glyph, _) = drawn_dot(app, needle);
        glyph == DOT_STOPPED_FILLED || glyph == DOT_STOPPED_HOLLOW
    }

    /// セッション行のドット（行頭 2 桁目）を 1 フレーム描いて (記号, 前景色) で取り出す。
    /// **状態は文字ラベルではなくこのドットが語る**ので、実データの回帰検査には
    /// [`render_sidebar`] のテキストではなくこれを読む。
    ///
    /// **「その行が Stopped か」は色ではなく記号で見ること**（[`drawn_as_stopped`]）:
    /// 明滅の谷は [`ui`]`().dim` ＝ Stopped の色そのものなので、色で判定すると
    /// Working の行が描いた瞬間の位相しだいで Stopped と同じ答えを返す。
    /// 実際にこれで 2 回に 1 回落ちるテストが生まれた。記号は位相に依らない。
    ///
    /// 記号・色・行の index は**同じ 1 回の描画**から取る（2 回描くと、行を探す描画と
    /// 読む描画の間で一覧が組み直され、別の行の桁を読み得る）
    fn drawn_dot(app: &mut App, needle: &str) -> (String, Color) {
        let (w, h) = app.term_size;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).expect("test terminal");
        terminal
            .draw(|frame| {
                draw(frame, app);
            })
            .expect("draw failed");
        let buffer = terminal.backend().buffer();
        // 固定ヘッダーの下はスクロール分ずれるが、このテストは行数が窓に収まる
        // 前提なので scroll = 0（描画側がクランプ済み。[`render_sidebar`] と同じ前提）
        assert_eq!(app.sidebar_scroll, 0, "test precondition (no scroll) broke down");
        let cols = sidebar_cols(app);
        let idx = app
            .sidebar_rows
            .iter()
            .enumerate()
            .find(|(i, row)| {
                matches!(row.action(), Some(RowAction::Open(_)))
                    && (1..cols.saturating_sub(1))
                        .map(|x| buffer[(x, *i as u16 + 1)].symbol())
                        .collect::<String>()
                        .contains(needle)
            })
            .map(|(i, _)| i)
            .unwrap_or_else(|| panic!("{needle} is not on any row"));
        // 行は上枠の次から積まれ、ドットは行頭から 2 桁目（1 桁目はペイン印）
        let cell = &buffer[(2, idx as u16 + 1)];
        (cell.symbol().to_string(), cell.fg)
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

    /// 名前つきのセッション行 1 本（**名前は行が持たない**ので、表示名は
    /// 撮影用と同じ固定表（[`crate::title::Titles::fixed`]）で与える）
    fn named_session(id: &str, cwd: &str, title: &str) -> crate::sessions::SessionRow {
        NAMES.with(|names| {
            names
                .borrow_mut()
                .insert(crate::sessions::SessionId::new(id), title.to_string())
        });
        crate::sessions::SessionRow::new(crate::sessions::SessionId::new(id), cwd, 0)
    }

    thread_local! {
        /// [`named_session`] が積んだ「行 → 表示名」。テストの [`App`] は
        /// [`fixed_titles`] でこれを読む（本番と同じ [`Titles::of`] を通る）
        static NAMES: std::cell::RefCell<
            std::collections::HashMap<crate::sessions::SessionId, String>,
        > = std::cell::RefCell::new(std::collections::HashMap::new());
    }

    /// [`named_session`] で積んだ名前を返す [`Titles`]（transcript は読まない）
    fn fixed_titles() -> crate::title::Titles {
        crate::title::Titles::fixed(NAMES.with(|names| names.borrow().clone()))
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

    /// 集計行を探す。**語で探さない**（語を変えるたびにテストを直す形にしない）:
    /// 集計行は「数字 + 空白 + 語」で始まる唯一の行
    fn summary_row(texts: &[String]) -> String {
        texts
            .iter()
            .find(|t| {
                let mut chars = t.chars();
                chars.next().is_some_and(|c| c.is_ascii_digit())
                    && chars.next() == Some(' ')
            })
            .expect("the summary row is missing")
            .clone()
    }

    /// **行に状態テキストと経過時間は出ない。** 状態はドットの色が語るので、
    /// 文字での重複表現は持たない（廃止に伴い、経過時間のテキストを検査していた
    /// 旧テストは丸ごと不要になった）
    #[test]
    fn a_row_shows_no_status_text_or_age() {
        let mut app = App {
            term_size: (120, 40),
            sidebar_width: 40,
            sessions: vec![named_session("s", "C:\\dev\\api", "no-text-row")],
            hook_states: crate::hooks::HookStates::from_entries([("s", "done", 0)]),
            titles: fixed_titles(),
            ..Default::default()
        };
        let line = session_lines(&mut app)
            .into_iter()
            .find(|l| l.contains("no-text-row"))
            .expect("the row was not drawn");
        for word in ["Waiting", "Working", "Completed", "Stopped"] {
            assert!(
                !line.contains(word),
                "the row still shows a status word: {line:?}"
            );
        }
        // 名前の後ろは詰め物とメニュー記号だけ（状態・経過の文字は無い）
        let after_name = &line[line.find("no-text-row").unwrap() + "no-text-row".len()..];
        assert_eq!(
            after_name.trim(),
            MENU_MARK,
            "the row shows more than padding and the menu mark after the name: {after_name:?}"
        );
    }

    /// **止めた行は `Stopped`（Stopped グループ・dim のドット）。**
    ///
    /// 実機では `stop` の直後に一瞬 `Stopped` になってから `Needs input` へ戻っていた
    /// （保管に残った停止前の hook が載り直していた）。**残骸の hook を持っていても**
    /// 窓が無い行は Stopped として描かれることを固定する
    #[test]
    fn a_stopped_row_is_drawn_as_stopped_and_not_as_needs_input() {
        let mut app = App {
            term_size: (120, 40),
            sidebar_width: 34,
            sessions: vec![crate::sessions::SessionRow {
                // 既読の行にしておく（見たいのは状態の描かれ方で、未読の印ではない）
                last_opened_at: 10_000,
                ..named_session("s", "C:\\dev\\api", "stopped-row")
            }],
            hook_states: crate::hooks::HookStates::from_entries([("s", "blocked", 9_999)]),
            titles: fixed_titles(),
            ..Default::default()
        };
        // 状態は文字ではなくドットが語る（記号は Stopped の小さい丸、色は dim）
        let (glyph, color) = drawn_dot(&mut app, "stopped-row");
        assert_eq!(glyph, DOT_STOPPED_HOLLOW, "a dead row is not drawn as Stopped");
        assert_eq!(color, ui().dim, "a dead row does not use the Stopped color");
        let texts = sidebar_texts(&mut app);
        let row = texts
            .iter()
            .find(|t| t.contains("stopped-row"))
            .expect("the row was not drawn");
        // 既読なのでドットは抜き（塗りのままでは未読の入力待ちと見紛う）。
        // 止まった行なので丸は一回り小さい側
        assert!(row.starts_with(&format!("{CLOSED_MARK}{DOT_STOPPED_HOLLOW}")), "{row:?}");
        // 集計もその 1 本を Stopped として数える（0 件の項目は出さない）
        let counts = summary_row(&texts);
        assert_eq!(
            counts, "1 stopped",
            "a stopped row was counted as something else"
        );
    }

    /// **狭い端末はサイドバーを縮めて描くが、ユーザーが選んだ幅を忘れない。**
    /// 端末が広がれば同じ幅で描き直される（実機で「セッションを止めると縮んだまま
    /// 戻らない」として出た症状の描画側の固定）
    #[test]
    fn a_narrow_terminal_draws_a_narrower_sidebar_without_forgetting_the_width() {
        let mut app = App {
            term_size: (120, 20),
            sidebar_width: 34,
            ..Default::default()
        };
        // 右ペインの左枠線がサイドバー幅の位置に立つ（そこが境目）
        let border_x = |app: &mut App| {
            let (w, h) = app.term_size;
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h))
                .expect("test terminal");
            terminal.draw(|frame| {
                draw(frame, app);
            })
            .expect("draw");
            let buffer = terminal.backend().buffer().clone();
            // 1 つ目はサイドバー自身の角、2 つ目が右ペインの角 ＝ そこが境目
            (0..w)
                .filter(|x| buffer[(*x, 0)].symbol() == "┌")
                .nth(1)
                .expect("the right pane has no top-left corner")
        };
        assert_eq!(border_x(&mut app), 34, "the chosen width is not what gets drawn");
        app.term_size = (60, 20);
        assert_eq!(border_x(&mut app), 20, "the narrow terminal did not shrink the sidebar");
        app.term_size = (120, 20);
        assert_eq!(border_x(&mut app), 34, "the sidebar stayed narrow after there was room again");
    }

    /// 見た目を比べるためのセッション行 1 本ぶんの材料。
    ///
    /// **状態は特別扱いの無い `Waiting`**（明滅もせず、丸も小さくならない）にしてある。
    /// ここを `Stopped` にすると、状態を上書きしないテストが黙って「止まった行だけの
    /// 見た目」を検査することになる（状態別の見え方は各テストが自分で `group` を置く）
    fn look_fixture() -> RowData {
        RowData {
            action: RowAction::Open(SessionId::new("a")),
            group: Group::Waiting,
            cwd: "C:\\dev\\api".to_string(),
            label: "the-row".to_string(),
            is_active_window: false,
            unread: false,
            pinned: false,
        }
    }

    /// 行を「1 文字ずつの (文字, スタイル)」へ均す（色の値を書き写さずに比べるため）
    fn cells(line: &Line<'_>) -> Vec<(String, Style)> {
        line.spans
            .iter()
            .flat_map(|span| {
                let style = line.style.patch(span.style);
                span.content
                    .chars()
                    .map(move |ch| (ch.to_string(), style))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// **選択・ホバー・ペインに出ている の 3 つが、重なっても見分けられる。**
    ///
    /// 帯（選択・ホバー）と印（ペインに出ている）は別の手段なので、重なっても
    /// 互いを消さない。色を書き写さず**組んだ行どうしを比べる**ので、
    /// 帯の色や記号を変えてもこの関係が保たれていれば通る
    #[test]
    fn the_three_row_states_stay_distinguishable_even_when_they_overlap() {
        let row = |open: bool, band: bool, selected: bool| {
            let mut d = look_fixture();
            d.is_active_window = open;
            // d.spinning は常に false（このテストの関心は帯と印の重なりだけ）なので
            // 位相はどちらを渡っても結果は変わらない
            cells(&session_row_line(&d, Look { band, selected, open }, DEFAULT_INNER, true))
        };
        let plain = row(false, false, false);
        let hovered = row(false, true, false);
        let selected = row(false, true, true);
        let open = row(true, false, false);
        // 3 つとも素の行とは違い、互いにも違う
        for (name, drawn) in [("hover", &hovered), ("selection", &selected), ("open", &open)] {
            assert_ne!(*drawn, plain, "{name} is invisible");
        }
        assert_ne!(hovered, selected, "hover and selection look the same");
        assert_ne!(selected, open, "selection and the open row look the same");
        assert_ne!(hovered, open, "hover and the open row look the same");

        // 帯（背景）は選択もホバーも同じ ＝ 「今ここ」の示し方は 1 つ
        let bg = |drawn: &[(String, Style)]| drawn.iter().map(|c| c.1.bg).collect::<Vec<_>>();
        assert_eq!(bg(&hovered), bg(&selected), "hover and selection use different bands");
        // **開いている行は帯を使わない**（別の軸）ので、素の行と背景が同じ
        assert_eq!(bg(&open), bg(&plain), "the open row stole the band");
        // 印は行頭 1 桁目 ＝ 色ではなく文字で読める
        assert_eq!(open[0].0, OPEN_MARK, "the open row has no mark at its head");
        assert_eq!(plain[0].0, CLOSED_MARK);
        assert_eq!(selected[0].0, CLOSED_MARK);

        // **3 つが重なっても全部読める**（帯 + 前景の強調 + 印）
        let all = row(true, true, true);
        assert_eq!(all[0].0, OPEN_MARK, "the open mark is lost when selected and hovered");
        assert_eq!(bg(&all), bg(&selected), "the band is lost on an open row");
        assert_ne!(all, selected, "the open mark makes no difference while selected");
        assert_ne!(all, open, "the band makes no difference on an open row");
    }

    /// **未読とペインの印は別の桁**（同じ行で両方点いても互いを消さない）。
    /// 名前の開始桁も動かない
    #[test]
    fn the_head_marks_do_not_compete_for_the_same_column() {
        let mut d = look_fixture();
        d.unread = true;
        d.is_active_window = true;
        let look = Look { band: false, selected: false, open: true };
        let drawn = cells(&session_row_line(&d, look, DEFAULT_INNER, true));
        assert_eq!(drawn[0].0, OPEN_MARK);
        assert_eq!(drawn[1].0, DOT_FILLED);
    }

    /// **未読行はドットが塗り（[`DOT_FILLED`]）、既読行は抜き（[`DOT_HOLLOW`]）。**
    /// 塗りは 2 値だけで、色や明滅とは独立したチャンネル
    #[test]
    fn the_dot_fill_marks_unread_rows() {
        let look = Look { band: false, selected: false, open: false };
        let mut d = look_fixture();
        d.unread = false;
        let read = cells(&session_row_line(&d, look, DEFAULT_INNER, true));
        d.unread = true;
        let unread = cells(&session_row_line(&d, look, DEFAULT_INNER, true));
        assert_eq!(read[1].0, DOT_HOLLOW, "a read row does not show the hollow dot");
        assert_eq!(unread[1].0, DOT_FILLED, "an unread row does not show the filled dot");
    }

    /// **止まった行だけ丸が一回り小さくなる。塗り（未読）はそのまま。**
    ///
    /// これは色の肩代わり: Stopped の色は Working の明滅の谷と同じ `dim` なので、
    /// **色だけでは「谷にいる Working」と「Stopped」が区別できない**。形が
    /// その区別を引き受ける。ここでは 4 通り（大小 × 塗り）が全部違う記号に
    /// なることを見るので、片方のチャンネルがもう片方を潰したら落ちる
    #[test]
    fn a_stopped_row_shows_a_smaller_dot_without_losing_its_fill() {
        let look = Look { band: false, selected: false, open: false };
        let glyph = |group: Group, unread: bool| {
            let mut d = look_fixture();
            d.group = group;
            d.unread = unread;
            cells(&session_row_line(&d, look, DEFAULT_INNER, true))[1].0.clone()
        };
        assert_eq!(glyph(Group::Stopped, true), DOT_STOPPED_FILLED, "unread stopped");
        assert_eq!(glyph(Group::Stopped, false), DOT_STOPPED_HOLLOW, "read stopped");
        // 止まっていない状態は大きい丸のまま（Stopped だけが小さくなる）
        for group in [Group::Waiting, Group::Working, Group::Completed] {
            assert_eq!(glyph(group, true), DOT_FILLED, "{} shrank", group.title());
            assert_eq!(glyph(group, false), DOT_HOLLOW, "{} shrank", group.title());
        }
        // 4 記号が全部別物 = 大小と塗りのどちらも相手を潰していない
        let all = [DOT_FILLED, DOT_HOLLOW, DOT_STOPPED_FILLED, DOT_STOPPED_HOLLOW];
        for i in 0..all.len() {
            for j in i + 1..all.len() {
                assert_ne!(all[i], all[j], "two dot glyphs are the same character");
            }
        }
    }

    /// **ドットの色は状態そのもの。** 4 状態それぞれで [`crate::poll::classify`] が
    /// 決めた `Group` の色（[`Group::color`]）がそのままドットへ出る。
    /// `classify` を経由して色を取るので、対応表を手で書き写さない
    #[test]
    fn the_dot_color_matches_the_row_state() {
        let look = Look { band: false, selected: false, open: false };
        let dot_color = |state: &str, alive: bool| {
            let mut d = look_fixture();
            d.group = classify(state, alive);
            cells(&session_row_line(&d, look, DEFAULT_INNER, true))[1].1.fg
        };
        assert_eq!(dot_color(WAITING, true), Some(C_ATTENTION), "Waiting");
        assert_eq!(dot_color(WORKING, true), Some(C_WORKING), "Working");
        assert_eq!(dot_color(COMPLETED, true), Some(crate::theme::C_OK), "Completed");
        assert_eq!(dot_color(STOPPED, false), Some(ui().dim), "Stopped");
    }

    /// **4 状態の色は互いに異なる。** 新設計では色だけが状態を語るので、
    /// 2 つの状態が同じ色になると画面上で区別が付かなくなる
    #[test]
    fn the_four_group_colors_are_all_distinct() {
        let colors: Vec<Color> = Group::ORDER.iter().map(|g| g.color()).collect();
        for i in 0..colors.len() {
            for j in (i + 1)..colors.len() {
                assert_ne!(
                    colors[i],
                    colors[j],
                    "{} and {} share a color",
                    Group::ORDER[i].title(),
                    Group::ORDER[j].title()
                );
            }
        }
    }

    /// **Working 中のドットは 400ms の位相で状態色と dim を往復する。**
    ///
    /// 谷は淡色テキストと同じ [`ui().dim`]（谷の深さを決めるためだけの色を別に持たない）。
    /// **谷が Stopped の色と一致するのは承知の上**なので、ここでは「谷 = dim」を
    /// 名指しで固定して、別の淡色へ黙って差し替わらないようにする。
    /// 明滅しない状態（例: Completed）は位相に関係なく自分の色のまま
    #[test]
    fn a_working_dot_blinks_between_its_color_and_dim() {
        let look = Look { band: false, selected: false, open: false };
        let mut d = look_fixture();
        d.group = Group::Working;
        let lit = cells(&session_row_line(&d, look, DEFAULT_INNER, true))[1].1.fg;
        let dark = cells(&session_row_line(&d, look, DEFAULT_INNER, false))[1].1.fg;
        assert_eq!(
            lit,
            Some(Group::Working.color()),
            "the lit phase does not show the state color"
        );
        assert_eq!(dark, Some(ui().dim), "the dark phase does not fall back to dim");
        assert_ne!(lit, dark, "the dot does not blink");

        // 明滅しない状態は位相に関係なく自分の色のまま（色と明滅は同じ group から
        // 決まるので、「Working の色だが明滅しない」という行は型として作れない）
        d.group = Group::Completed;
        let steady_lit = cells(&session_row_line(&d, look, DEFAULT_INNER, true))[1].1.fg;
        let steady_dark = cells(&session_row_line(&d, look, DEFAULT_INNER, false))[1].1.fg;
        assert_eq!(steady_lit, steady_dark, "a non-blinking group changed with the phase");
        assert_eq!(steady_lit, Some(Group::Completed.color()), "the steady color is wrong");
    }

    /// ペイン追従を見るための一覧（セッション 3 本）。**窓（PTY）は起こさない**ので、
    /// 「ペインが指すセッション」は [`App::pane_shown`] の突き合わせ相手として
    /// 直接与える（[`follow_pane`] の入口は 1 つなので、経路の違いは問わない）
    fn follow_fixture() -> App {
        App {
            term_size: (60, 40),
            sidebar_width: 34,
            sessions: vec![
                named_session("a", "C:\\dev\\api", "row-a"),
                named_session("b", "C:\\dev\\api", "row-b"),
                named_session("c", "C:\\dev\\api", "row-c"),
            ],
            titles: fixed_titles(),
            ..Default::default()
        }
    }

    /// その行の index（描画が積んだ一覧から引く）
    fn row_index(app: &App, id: &str) -> usize {
        row_of_session(&app.sidebar_rows, &SessionId::new(id)).expect("no row")
    }

    /// **ペインが指すセッションが変わったら、選択もその行へ移る。**
    ///
    /// 開く経路（クリック / メニューの `open` / 新規起動 / ペイン内の `/resume`）は
    /// どれも「ペインが指すセッション」に集まるので、追従の判断は 1 箇所で足りる。
    /// ここではその 1 箇所（[`follow_pane`]）が経路を問わずに揃えることを見る
    #[test]
    fn the_selection_moves_to_whatever_the_pane_shows() {
        let mut app = follow_fixture();
        let _ = sidebar_texts(&mut app); // 行を積む
        app.selection = SidebarPos::Row(0);

        // ペインが row-c を指した（どの経路で開いたかは問わない）
        app.pane_shown = None;
        let target = SessionId::new("c");
        follow_pane(&mut app, Some(target.clone()));
        assert_eq!(
            app.selection,
            SidebarPos::Row(row_index(&app, "c")),
            "the selection did not follow the pane"
        );
        assert!(app.sidebar_follow_sel, "the scroll does not bring the row into view");

        // 同じセッションを指したままなら、選択は動かせる（`↑↓` を邪魔しない）
        app.selection = SidebarPos::Row(row_index(&app, "a"));
        follow_pane(&mut app, Some(target));
        assert_eq!(
            app.selection,
            SidebarPos::Row(row_index(&app, "a")),
            "the selection was dragged back while the pane did not change"
        );
    }

    /// **ペインがセッションを出していない（新規セッション画面）なら揃える先が無い。**
    /// 戻ってきたときにまた揃う
    #[test]
    fn a_pane_without_a_session_leaves_the_selection_alone() {
        let mut app = follow_fixture();
        let _ = sidebar_texts(&mut app);
        follow_pane(&mut app, Some(SessionId::new("b")));
        let at_b = app.selection;
        // 新規セッション画面へ移った
        follow_pane(&mut app, None);
        assert_eq!(app.selection, at_b, "the selection jumped when the pane left the sessions");
        // 同じセッションへ戻ると、もう一度揃う
        follow_pane(&mut app, Some(SessionId::new("b")));
        assert_eq!(app.selection, at_b);
    }


    fn pinned_fixture(grouping: Grouping, pinned: bool) -> App {
        App {
            term_size: (60, 40),
            sidebar_width: 34,
            grouping,
            sessions: vec![
                named_session("a", "C:\\dev\\api", "first"),
                named_session("b", "C:\\dev\\api", "second"),
                crate::sessions::SessionRow {
                    pinned,
                    ..named_session("c", "C:\\dev\\api", "chosen")
                },
            ],
            titles: fixed_titles(),
            ..Default::default()
        }
    }

    /// **ピン留めした行は上部の `pinned` 節へ「移る」**（グループには残らない）。
    ///
    /// 行に印を足すのではなく節ごと分けるのが判断で、固定するのは
    /// 「同じ行が 2 箇所に出ない」こと ＝ pin の表示が節と印の 2 箇所に
    /// 分かれていない
    #[test]
    fn a_pinned_row_moves_into_the_pinned_section() {
        for grouping in [Grouping::State, Grouping::Directory] {
            let mut app = pinned_fixture(grouping, true);
            let texts = sidebar_texts(&mut app);
            let section = texts
                .iter()
                .position(|t| t == PINNED_TITLE)
                .unwrap_or_else(|| panic!("{grouping:?}: no pinned section: {texts:?}"));
            // 節の直後がその行（節に入っていること自体が pin の表示）
            assert!(texts[section + 1].contains("chosen"), "{grouping:?}: {texts:?}");
            // **一覧全体で 1 度だけ**（元のグループに複製されていない）
            assert_eq!(
                texts.iter().filter(|t| t.contains("chosen")).count(),
                1,
                "{grouping:?}: the pinned row is in two places: {texts:?}"
            );
            // 残りの行は元のグループのまま（節に吸い込まれていない）
            let lines = session_lines(&mut app);
            assert_eq!(lines.len(), 3, "{grouping:?}: a row is missing: {lines:?}");
            assert!(lines[0].contains("chosen"), "{grouping:?}: {lines:?}");
            assert!(lines[1].contains("first"), "{grouping:?}: {lines:?}");
            assert!(lines[2].contains("second"), "{grouping:?}: {lines:?}");
        }
    }

    /// **pin が 0 本なら節ごと出ない**（見出しだけが残らない）。
    /// unpin すれば行は元のグループへ戻る
    #[test]
    fn the_pinned_section_disappears_when_nothing_is_pinned() {
        for grouping in [Grouping::State, Grouping::Directory] {
            let mut app = pinned_fixture(grouping, false);
            let texts = sidebar_texts(&mut app);
            assert!(
                !texts.iter().any(|t| t == PINNED_TITLE),
                "{grouping:?}: an empty pinned section is drawn: {texts:?}"
            );
            // 行は元のグループの中に居る（pin を外したら戻る）
            let at = |needle: &str| texts.iter().position(|t| t.contains(needle));
            assert!(at("chosen") > at("Completed"), "{grouping:?}: {texts:?}");
        }
    }

    /// **pin の節はどちらのグルーピングでも同じ位置**（一覧の先頭）に出る。
    /// pin の効き方が「どう並べているか」で変わらないことの固定
    #[test]
    fn the_pinned_section_sits_at_the_same_place_in_both_groupings() {
        let mut places = Vec::new();
        for grouping in [Grouping::State, Grouping::Directory] {
            let mut app = pinned_fixture(grouping, true);
            let texts = sidebar_texts(&mut app);
            let at = texts.iter().position(|t| t == PINNED_TITLE).expect("no section");
            // 固定ヘッダーのすぐ下（＝ 一覧の先頭の節）。空行 1 本を挟む
            assert_eq!(at, app.sidebar_header_rows + 1, "{grouping:?}: {texts:?}");
            places.push(at);
        }
        assert_eq!(places[0], places[1], "the pinned section moved with the grouping");
    }

    /// **`↑↓` は pin の節の行も通る。** 節を作ったせいで触れなくなる行があると、
    /// キーボードだけでは pin した行を開けなくなる
    #[test]
    fn the_arrow_keys_reach_the_rows_in_the_pinned_section() {
        let mut app = pinned_fixture(Grouping::State, true);
        // 行の構成は描画が積む（選択の巡回はその結果を読む）
        let _ = sidebar_texts(&mut app);
        let row_of = |app: &App, id: &str| {
            row_of_session(&app.sidebar_rows, &SessionId::new(id)).expect("no row")
        };
        let pinned_row = row_of(&app, "c");
        // 節の中の行は選択できる（飾りではない）
        assert!(app.sidebar_rows[pinned_row].selectable());
        app.selection = SidebarPos::Row(pinned_row);
        // 下へ進むと節の外の行へ抜ける（節の中で止まらない）
        let reached: Vec<usize> = (0..app.sidebar_rows.len())
            .scan(pinned_row, |_, _| {
                crate::app::move_selection(&mut app, 1);
                app.selection.row()
            })
            .collect();
        for id in ["a", "b"] {
            let row = row_of(&app, id);
            assert!(reached.contains(&row), "{id} is not reachable with the arrow keys");
        }
    }

    /// **集計は pin した行も数える。** pin は「隠す」操作ではなく「上へ寄せる」
    /// 操作なので、数えないと一覧に見えている行と数が合わなくなる
    #[test]
    fn the_summary_counts_pinned_rows_too() {
        let mut app = App {
            // 集計行が切られない幅の端末（このテストの関心は数だけ）
            term_size: (120, 40),
            ..pinned_fixture(Grouping::State, true)
        };
        let texts = sidebar_texts(&mut app);
        assert_eq!(
            summary_row(&texts),
            "3 stopped",
            "the pinned row was not counted"
        );
    }

    /// **一覧に隠し区画は無い。** アーカイブを廃止したので、行はどちらの
    /// グルーピングでも通常の一覧に出て集計にも数えられる（`close` が外すのは
    /// 行だけなので、アーカイブとの差は「戻す導線があるか」しか残らず、
    /// 節を 1 つ増やす価値が無かった）
    #[test]
    fn every_row_stays_in_the_normal_list_and_is_counted() {
        for grouping in [Grouping::State, Grouping::Directory] {
            let mut app = App {
                // 集計行が切られない幅の端末（このテストの関心は行数と集計の一致）
                term_size: (120, 40),
                sidebar_width: 34,
                grouping,
                sessions: vec![
                    named_session("a", "C:\\dev\\api", "first row"),
                    named_session("b", "C:\\dev\\api", "second row"),
                ],
                titles: fixed_titles(),
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
            let counts = summary_row(&texts);
            assert_eq!(
                counts, "2 stopped",
                "{grouping:?}: not every row is counted"
            );
        }
    }

    /// 下部バーの案内。**撤去した打鍵は載せない**（載っていれば嘘になる）
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

    }

    /// 下部バーは**打鍵が届く先で効くキーだけ**を出す。受け手は 3 つ
    /// （一覧 / メニュー / 端末）で、他所のキーを混ぜない。
    /// `app:` の予約キーはどの状態でも出る（受け手に関係なく効く唯一の打鍵）
    #[test]
    fn the_bottom_bar_follows_whoever_receives_the_keys() {
        let base = || App {
            term_size: (120, 30),
            focus: Focus::Sidebar,
            sessions: vec![named_session("s", "C:\\dev\\api", "session")],
            titles: fixed_titles(),
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

    /// テスト内でアカウント行の文面だけを見るための短縮
    fn row_text(status: &AccountStatus) -> String {
        account_row(status).0
    }

    /// **行に出るのは今サインインしているアカウントのラベルだけ。**
    /// 未取得は空行のまま（誤情報を出さない）
    #[test]
    fn account_row_shows_the_label_as_is() {
        assert_eq!(
            row_text(&AccountStatus::LoggedIn("you · Acme, Inc.".to_string())),
            "you · Acme, Inc."
        );
        assert_eq!(row_text(&AccountStatus::Unknown), "");
        assert_eq!(row_text(&AccountStatus::LoggedOut), LOGGED_OUT_ROW);
    }

    /// 未ログインの行は **再ログインの手順まで出す**（状態だけでは打つ手が分からない）
    #[test]
    fn account_row_prompts_a_login_when_logged_out() {
        let (text, style) = account_row(&AccountStatus::LoggedOut);
        assert!(text.contains("not logged in"), "{text:?}");
        assert!(text.contains("/login"), "the row does not say how to log back in: {text:?}");
        assert_eq!(
            style,
            Style::default().fg(C_ATTENTION),
            "a row that needs action is not in the attention color"
        );
    }

    /// 既定のサイドバー幅（34 桁 = 内側 32 桁）でアカウント行が切られない
    #[test]
    fn account_row_fits_the_default_sidebar_width() {
        use unicode_width::UnicodeWidthStr;
        // README・撮影データに出る実寸のラベルと、表示幅 2 の文字（全角）を含む
        // ラベル。源を ASCII に保つため \u エスケープで書く（表示幅 4 桁の 2 文字）
        let wide = format!("{} · 1→10, Inc.", "\u{5927}\u{5834}");
        for label in ["ooba · 1→10, Inc.", "you · Acme, Inc.", wide.as_str()] {
            let text = row_text(&AccountStatus::LoggedIn(label.to_string()));
            assert_eq!(
                clip_to_width(&text, DEFAULT_INNER),
                text,
                "clipped at the default width: {text:?} ({} cols / inner {DEFAULT_INNER} cols)",
                text.width()
            );
        }
        // 未ログインの案内も切ってはいけない（打つ手が読めなくなる）
        assert_eq!(
            clip_to_width(LOGGED_OUT_ROW, DEFAULT_INNER),
            LOGGED_OUT_ROW,
            "{} cols / inner {DEFAULT_INNER} cols",
            LOGGED_OUT_ROW.width()
        );
    }

    /// ログイン済みのアカウント行を持つ `App`
    fn app_with_account_row() -> App {
        use crate::poll::FooterInfo;

        App {
            term_size: (120, 30),
            footer: FooterInfo {
                account: AccountStatus::LoggedIn("you · Acme, Inc.".to_string()),
                current: "2.1.220".to_string(),
                latest: None,
            },
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

    /// **アカウント行はマウスを乗せても帯でハイライトされる。**
    /// フッターに描かれる行なので一覧の行 index では表せず、以前はホバーから
    /// 除外されていた（[`SidebarPos::Account`] を指せるようになったことの固定）。
    ///
    /// **選択とホバーは同じ帯だが同じ見た目ではない**（[`Look`]）: 帯は
    /// 「今ここを指している」で共通、前景の強調が付くのは選択だけ。
    /// 帯の色は書き写さず、**3 つの描画を突き合わせて**関係だけを見る
    #[test]
    fn the_account_row_is_highlighted_while_hovered() {
        let bg_of = |row: &[(String, Color, Color)]| {
            row.iter().map(|(_, _, bg)| *bg).collect::<Vec<_>>()
        };
        let mut app = app_with_account_row();
        let plain = drawn_account_row(&mut app);

        app.selection = SidebarPos::Account;
        let selected = drawn_account_row(&mut app);
        assert_ne!(plain, selected, "the premise broke — selection no longer highlights the row");

        app.selection = SidebarPos::Row(0);
        app.hovered = Some(SidebarPos::Account);
        let hovered = drawn_account_row(&mut app);
        assert_ne!(hovered, plain, "hovering the account row does not highlight it");
        // 帯（背景）は選択と同じ ＝ 「今ここ」は同じ手段で示す
        assert_eq!(bg_of(&hovered), bg_of(&selected), "hover uses a different band");
        // 前景の強調は選択だけ ＝ 選択とホバーが見分けられる
        assert_ne!(hovered, selected, "hover and selection are indistinguishable");

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
        let mut app = app_with_account_row();
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
        use crate::app::PopupKind;
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
