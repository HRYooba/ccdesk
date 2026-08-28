//! サイドバー・スロット群の描画と、描画／クリック判定で共有するジオメトリ計算。
pub(crate) mod new_view;
pub(crate) mod text_field;

use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem};
use ratatui::Frame;
use tui_term::widget::PseudoTerminal;

use ccdesk::{dir_key, LockExt};

use crate::app::{
    selected_enter, App, Focus, Grab, Grabbed, Popup, PopupKind, RowAction, SelfUpdate, SidebarPos,
    Slot, SidebarRow,
};
use crate::backend::Kind;
use crate::poll::{AccountStatus, State, Grouping};
use crate::sessions::SessionId;
use crate::theme::{
    ui, usage_color, FOCUS_BORDER, MUTED_FG,
};
use crate::ui::new_view::draw_new_view;
use crate::usage::{Usage, UsageInfo, UsageWindow};

/// **セッション行の行頭に並ぶ 2 つの印。**
///
/// 1 桁目は「この行が今どこかのスロットに出ているか」を答え、出ているなら
/// **どのスロットか**（フォーカス中か、他スロットで見えているだけか）まで
/// 形で分ける（[`OPEN_MARK`] / [`SCREEN_MARK`] / [`CLOSED_MARK`]）。
/// 消えている側も同じ幅の空白を取る ＝ 印が付いたり消えたりしても名前の開始桁が
/// 動かない。**状態ラベルの前ではなく行頭に置く**のが判断: 印が答えるのは
/// 「この行はどうか」なので、行を縦に流し読みするときに 1 つの桁へ揃っている方が
/// 拾える（名前の後ろに置くと名前の長さで印の位置が毎行変わる）。
///
/// 2 桁目は**ドット 1 つに 4 つの直交チャンネルを持たせた印**（組み立ては
/// [`session_row_line`] に閉じる）:
///
/// | チャンネル | 表すもの | 値 |
/// |:--|:--|:--|
/// | 形 | どの agent の行か | 丸（[`DOT_FILLED`]/[`DOT_HOLLOW`]）＝ claude / 菱（[`DIAMOND_FILLED`]/[`DIAMOND_HOLLOW`]）＝ codex |
/// | 塗り | 未読（見ていない間にその行が動いた） | 塗り ＝ 未読 / 中空 ＝ 既読 |
/// | 色 | 状態（[`crate::poll::State`]） | Waiting/Working/Idle/Stopped の 4 色 |
/// | 明滅 | Working だけ | [`crate::theme::UiTheme::blink`] のコマを順に引く |
///
/// 4 つを同じ 1 桁へ載せるのは、行を縦に流し読みするときに「どの agent か」
/// 「未読か」「どの状態か」「動いているか」を 1 箇所で拾えるようにするため
/// （別々の桁に分けると視線が散る）。**状態アイコン（かつての `✻`/`✽`/`∙`）は
/// 廃止したまま**で、ここで復活したのは形そのものではなく「形 = agent」という割当。
///
/// **形が agent を指すので、状態は形に乗せられない。** かつて Stopped だけ一回り
/// 小さい丸にしていた（明滅の谷が `dim` ＝ Stopped の色と一致していたため）が、
/// 谷は背景側へ延ばした赤になって灰色の Stopped と混ざらなくなり、加えて状態語が
/// 行末に出る（[`row_tail_spans`]）ので、形に肩代わりさせる必要が無い。
///
/// **agent を色にしなかった理由**: 状態が既に色を使っている。同じ 1 桁で色を
/// 2 つの意味に割ることはできないので、空いていた形へ回した。**記号の対応は
/// 版行が凡例になる**（[`version_rows`] が `● claude` / `◆ codex` と並べる）。
///
/// **ピン留めはここに印を持たない**: pin した行は [`PINNED_TITLE`] の節へ移るので、
/// 節に入っていること自体が表示になる（同じ知識を印と並びの 2 箇所に持たない）。
///
/// **幅 1 桁であることはテストが固定する**（`the_row_head_marks_are_one_column_wide`）。
/// 測るのは `unicode-width` の既定 ＝ East Asian Ambiguous を 1 桁と数える側で、
/// ratatui の桁計算もこれと同じものを使うので、描画と予算の答えは必ず一致する。
///
/// **ドットの丸と菱（`●○◆◇`）は Ambiguous を承知で使っている**（`width_cjk` は
/// 2 を返す）。同じ意味を持つ Ambiguous でない字形が Unicode に無いため、代わりが無い。
/// CJK ロケールで Ambiguous を 2 桁に描く端末では行がずれる ＝ 既知の制約。
/// 選べる場面（[`MENU_MARK`] のように ASCII で足りるところ）では Ambiguous を避ける。
///
/// 1 桁目: **その行が今ペインに出ているか、かつどのペインか**
/// （`❯` U+276F ＝ フォーカス中のペインが指している行、
/// `›` U+203A ＝ 他のペインに出ているだけの行）
const OPEN_MARK: &str = "❯";
const SCREEN_MARK: &str = "›";
const CLOSED_MARK: &str = " ";
/// claude の行のドット。**塗りが答えるのは未読だけ**（色と明滅は状態が決める）
const DOT_FILLED: &str = "●";
const DOT_HOLLOW: &str = "○";
/// codex の行のドット。**丸と対になる塗り/中空の組**なので、未読のチャンネルが
/// agent によって欠けることがない
const DIAMOND_FILLED: &str = "◆";
const DIAMOND_HOLLOW: &str = "◇";

/// ドットのグリフ。**形が agent、塗りが未読**。色（状態）と明滅（Working）は
/// [`session_row_line`] が別のチャンネルとして載せる。
///
/// 4 グリフの対応を知るのはここ 1 箇所なので、版行の凡例（[`agent_glyph`]）も
/// 同じ表を引く ＝ 一覧の記号と凡例の記号が食い違うことがない
fn dot_glyph(kind: Kind, unread: bool) -> &'static str {
    match kind {
        Kind::Claude => mark(unread, DOT_FILLED, DOT_HOLLOW),
        Kind::Codex => mark(unread, DIAMOND_FILLED, DIAMOND_HOLLOW),
    }
}

/// 凡例に出す agent の記号（版行・new session 画面）。**一覧のドットの塗り側**を
/// 使う: 中空だと「既読」という別のチャンネルの値を凡例が名乗ってしまう
pub(crate) fn agent_glyph(kind: Kind) -> &'static str {
    dot_glyph(kind, true)
}

/// ピン留めした行を集める節の見出し。**グルーピング（state / directory）に
/// 関係なく同じ位置（一覧の先頭）に出る**ので、pin の効き方が
/// 「どう並べているか」で変わらない
const PINNED_TITLE: &str = "pinned";

/// 行頭が食う桁 ＝ ペイン印 1 + ドット 1 + 名前との間の空白 1。
/// **[`row_name_spans`] の予算と [`MIN_SIDEBAR`] の根拠がこの値に乗る**ので、
/// 行頭に何かを足したらテスト（`the_row_head_marks_are_one_column_wide`）が落ちる。
/// `pub(crate)` なのは [`crate::source`] の撮影用サイドバー幅がここから桁を導くため
/// （手で数えた桁を別ファイルに書き写さない）
pub(crate) const HEAD_COLS: usize = 3;

/// 名前に最低限残す桁（詰め切ったサイドバーでも行を見分けられる下限）。
///
/// **agent はもう桁を食わない。** かつては行頭に `[cc]` / `[cx]`（5 桁）、
/// 次に行末へ綴りで（7 桁）出していたが、今はドットの形が答えるので 0 桁。
/// **行末ブロックはこの下限を割ってまで出さない**（[`tail_cols`]）ので、
/// [`MIN_ROW_COLS`] はこの値のままでよい
const MIN_NAME_COLS: usize = 9;

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

/// 幅を選んでいないときのサイドバー幅（[`crate::app::App`] の初期値）。
///
/// **桁の予算を数えるファイルに置く**のが要点で、短い ID を出すかの判断
/// （[`shows_short_id`]）はこの幅のときに名前が持つ桁を基準にする ＝
/// 「既定幅で名前がどれだけ出るか」という 1 つの事実を 2 箇所に書かない
pub(crate) const DEFAULT_SIDEBAR: u16 = 34;

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

/// 行頭のペイン印。**フォーカス中のペインが指しているか、他のペインが指して
/// いるだけか**を形で分ける（[`OPEN_MARK`] / [`SCREEN_MARK`]）
fn open_mark(open: bool, focused: bool) -> &'static str {
    if !open {
        CLOSED_MARK
    } else if focused {
        OPEN_MARK
    } else {
        SCREEN_MARK
    }
}

/// リセット時刻のローカル表記。別日なら "7/29 09:00"、当日は `with_date` 次第で
/// "8/6 14:00" か "14:00"。
///
/// **当日を時刻だけで出せるのは 5h 枠だけ。** 5h は数時間おきに来るので日付は
/// ほぼ常に今日か明日で、幅を食うだけの情報になる。週次（7d / モデル別）は
/// 週に 1 度しか来ないので、当日に時刻だけで出ると 5h の時刻と見分けが付かない
/// （「時刻だけ ＝ 当日」という規則は画面に書かれていない ＝ 読み手は知らない）
fn fmt_reset_at(resets_at: u64, with_date: bool) -> String {
    use chrono::{Datelike, Local, TimeZone, Timelike};
    let Some(t) = Local.timestamp_opt(resets_at as i64, 0).single() else {
        return String::new();
    };
    let today = Local::now().date_naive();
    if !with_date && t.date_naive() == today {
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
                Span::styled(spin.unwrap_or("—"), Style::default().fg(ui().fail)),
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

/// 直前までの枠に共通するリセット時刻。`with_date` は [`fmt_reset_at`] の判断
/// （当日を時刻だけで出してよいのは 5h だけ）
fn push_reset(spans: &mut Vec<Span<'static>>, resets_at: u64, with_date: bool) {
    spans.push(Span::styled(
        format!(" →{}", fmt_reset_at(resets_at, with_date)),
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
            // 5h だけは当日を時刻だけで出す（数時間おきに来るので日付は幅の無駄）
            push_reset(&mut spans, resets_at, false);
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
        // 週次は当日でも日付を出す（週に 1 度しか来ない時刻を 5h と混同させない）
        push_reset(&mut spans, resets_at, true);
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
/// 乗った瞬間に当たり判定（[`usage_hits`]）が動かない
fn usage_footer(app: &App) -> Vec<Vec<Span<'static>>> {
    if app.notice.is_some() {
        return Vec::new();
    }
    let fetching = |kind: Kind| {
        app.usage_fetching
            .get(&kind)
            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed))
    };
    // 各行は「agent 名 + アカウント + 枠」。**アカウントはここにしか出ない**
    // （サイドバーのフッターは廃止した ＝ 同じことを 2 箇所に出さない）
    let mut lines: Vec<(Kind, Vec<Span<'static>>)> = app
        .kinds
        .iter()
        .copied()
        .map(|kind| {
            let mut spans = vec![Span::styled(
                format!(" {} ", kind.title()),
                Style::default().fg(MUTED_FG),
            )];
            let (label, style) = account_cell(&app.footer.account(kind));
            spans.push(Span::styled(label, style));
            spans.extend(usage_line(
                app.usage.get(&kind).unwrap_or(&Usage::Unknown),
                // キーヒントを押し出さないよう、使用率に渡すのは幅の半分まで
                app.term_size.0 / 2,
                fetching(kind),
            ));
            (kind, spans)
        })
        .collect();
    // ブロックとして右端へ寄せるので、中は左揃えに整える
    // （agent 名とアカウントの桁が縦に揃う ＝ 2 行が 1 つの塊に見える）
    let widest = lines.iter().map(|(_, l)| span_width(l)).max().unwrap_or(0);
    for (kind, line) in &mut lines {
        let pad = widest.saturating_sub(span_width(line)) as usize;
        if pad > 0 {
            line.push(Span::raw(" ".repeat(pad)));
        }
        // **帯が乗るのは乗っている 1 行だけ**（行ごとに取り直せるので、
        // 押せる場所と光る場所を 1 対 1 にする）
        if app.usage_hovered == Some(*kind) {
            for span in line.iter_mut() {
                span.style = span.style.bg(ui().hl_bg);
            }
        }
    }
    lines.into_iter().map(|(_, line)| line).collect()
}

/// 使用率行の左に置くアカウント表示。**未ログインは黄のまま**
/// （サイドバーから移しても気づきやすさを落とさない）
fn account_cell(status: &AccountStatus) -> (String, Style) {
    let (text, style) = account_row(status);
    if text.is_empty() {
        return (String::new(), style);
    }
    (format!("{text}  "), style)
}

/// 使用率のクリック当たり判定（右下の使用率を押すとその場で取り直す）
pub(crate) struct UsageHit {
    /// その行が出している agent（押すとこの agent だけ取り直す）
    pub(crate) kind: Kind,
    pub(crate) row: u16,
    pub(crate) columns: std::ops::Range<u16>,
}

/// 使用率が今どこに描かれているか。出していないときは None ＝ 当たらない
pub(crate) fn usage_hits(app: &App) -> Vec<UsageHit> {
    let (width, height) = app.term_size;
    if height < bottom_bar_rows(app) {
        return Vec::new();
    }
    // **枠を 1 つも出していないなら当たらない。** アカウントだけの行は
    // 押しても取り直すものが無い（使用率は opt-in。[`crate::main`]）
    if !app
        .kinds
        .iter()
        .any(|kind| !matches!(app.usage.get(kind), None | Some(Usage::Unknown)))
    {
        return Vec::new();
    }
    let lines = usage_footer(app);
    let drawn = lines.iter().map(|l| span_width(l)).max().unwrap_or(0);
    if drawn == 0 {
        return Vec::new();
    }
    // 下部バーは画面の末尾 [`bottom_bar_rows`] 行（[`draw`] の縦分割と同じ）。
    // 行の並びは [`crate::app::App::kinds`] ＝ [`usage_footer`] が積む順と同じ
    let top = height - bottom_bar_rows(app);
    app.kinds
        .iter()
        .copied()
        .zip(0..lines.len() as u16)
        .map(|(kind, offset)| UsageHit {
            kind,
            row: top + offset,
            // 右端に寄せて描くので、占めるのは末尾 `drawn` 列
            columns: width.saturating_sub(drawn)..width,
        })
        .collect()
}

/// 下部バーの行数 ＝ **出す agent の数**（使用率とアカウントは agent ごとに 1 行）。
/// キーヒントはその 1 行目の左に相乗りする。
///
/// **「右ペインはどこか」（[`pane_rect`]）もここから導く**ので、agent を切ると
/// ペインの高さ・PTY のサイズ・当たり判定が一斉に追随する ＝ codex を off に
/// すると 1 行がペインへ返る（空行が残らない）。
///
/// 下限は 1（agent を全部切ることはできない ＝ [`Kind::OPTIONAL`]）だが、
/// 式の前提を呼び手に委ねないのでここで保証する
pub(crate) fn bottom_bar_rows(app: &App) -> u16 {
    (app.kinds.len() as u16).max(1)
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
    /// 一覧に使える行数（上下の枠を除く）
    pub(crate) capacity: usize,
}

pub(crate) fn sidebar_layout(app: &App) -> SidebarLayout {
    // 下部バーを除いたサイドバー矩形は draw の chunks[0] と一致する
    sidebar_layout_of(
        app.term_size.1.saturating_sub(bottom_bar_rows(app)),
        sidebar_cols(app),
    )
}

/// [`sidebar_layout`] の本体。更新行が上部の版行に集約されたことでフッターは
/// 「区切り線 + アカウント行」の 2 行に固定され、ジオメトリはサイドバー矩形の
/// 大きさだけで決まる純関数になった（App を組まずにテストできる）
fn sidebar_layout_of(height: u16, sidebar_width: u16) -> SidebarLayout {
    // **フッターはもう無い。** アカウントは下部バーの使用率行の左へ移した
    // （同じことを 2 箇所に出さない）ので、サイドバーは枠の 2 行だけを引く
    let _ = sidebar_width;
    SidebarLayout {
        capacity: (height as usize).saturating_sub(2),
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
/// **East Asian Ambiguous でない記号だけを選ぶ。** 以前使っていたハンバーガー記号
/// （U+2630）は Ambiguous ＝ 幅の判定が端末とフォント設定で 1 桁にも 2 桁にもなる。
/// ccdesk は 2 桁と実測して桁を数えていたので、1 桁と解釈する端末では
/// **行全体が横へずれる**。縦三点（U+22EE）は Ambiguous ではなく、
/// `width` / `width_cjk` の両方が 1 を返す ＝ どのロケールでも 1 桁で確定する
/// （満たしているかはテストが `width_cjk` で直接測る）
const MENU_MARK: &str = "⋮";

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
/// ccdesk 側（[`SelfUpdate`]）と agent 側（[`crate::app::AgentUpdate`] + `FooterInfo::version`）で
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
    /// **更新は成功と報告されたのに版が動かなかった。**
    ///
    /// 5 秒で消える下部バーの通知だけでは足りないので行に残す: 押しても効かない
    /// 状態は次の更新まで続くのに、通知を見逃すと「押したのに何も起きない行」に
    /// 戻ってしまう。押せるままにしておく（agent 側が直れば次の一押しで通る）
    Stalled,
    /// 差し替え済み。反映は次回起動なので、案内だけ出して押させない。
    /// **自動再起動はしない**（コンソールを親子で奪い合いマウスが効かなくなる不具合が
    /// 実機で出たため、TUI を持ったまま自プロセスを起こす経路そのものをやめた）。
    /// 利用者が自分の好きなタイミングで ccdesk を終了・再起動して適用する。
    /// この状態になるのは ccdesk の行だけ（claude 側は次の `claude --version` が
    /// 新版を返して行が最新表示へ戻る）
    RestartPending,
}

impl UpdateState {
    /// 全状態。**テストが網羅を回す土台**（幅に収まるか・名前の桁がずれないか・
    /// 押せるかを状態ごとに見る箇所が 4 つあり、それぞれが自前の一覧を持っていた
    /// ＝ 状態を足すたびに 4 箇所へ手で足す形だった）。
    /// 状態を足したらここへ足す: [`Self::verb`] の match がコンパイラに気づかせる
    #[cfg(test)]
    const ALL: [Self; 5] = [
        Self::Current,
        Self::Available,
        Self::Running,
        Self::Stalled,
        Self::RestartPending,
    ];

    /// 右端に置く語。最新のときだけ空（やることが無い）。**押せる行だけが動詞**で、
    /// 押せない行（[`Self::action`] が `Inert`）は命令形を避けて、再起動が何を
    /// もたらすかを述べる句にする ＝ 語の形そのものが「押して意味があるか」を伝える。
    ///
    /// **新しい版の番号は出さない。** 新旧を並べた `⟳ claude v2.1.218 → v2.1.220` に
    /// 語まで足すと実測 35 桁で、既定幅（内側 32 桁）に収まらない。現行版と
    /// 「やること」のどちらも欠かせないので、落とすのは新版の番号にした
    fn verb(self) -> &'static str {
        match self {
            Self::Current => "",
            Self::Available => "update",
            Self::Running => "updating…",
            // **動詞ではなく状態**（押せる行だが、押しても同じ結果になりうる）。
            // "update" のままだと、効かなかったことが行から読めない
            Self::Stalled => "stalled",
            // **claude 本体と同じ綴り**（あちらは差し替え後に
            // "Update installed / Restart to update" と出す）。同じ出来事に同じ語を
            // 使う ＝ 利用者が 2 つの綴りを対応付けなくていい。
            //
            // 裸の "restart" にしないのは、押しても何も起きない行
            // （[`Self::action`]）なのに「押せばここから再起動できる」と読めるという
            // 指摘が利用者から来たため。`restart to update` は再起動が何をもたらすかを
            // 述べる句なので、その誤読が起きない。
            //
            // **この綴りは版番号と並べると既定幅（内側 32 桁）に入らない**ので、
            // この状態だけ版番号を出さない（[`Self::shows_version`]）
            Self::RestartPending => "restart to update",
        }
    }

    /// 版番号を行に出すか。**出さないのは差し替え済みのときだけ。**
    ///
    /// あの状態の右端の語（`restart to update`）は 17 桁あり、版番号と並べると
    /// 既定幅（内側 32 桁）で切られる。両方は置けないので、落とすのは番号にした:
    /// 伝えたいのは「新しい版はもう入っていて、あとは起動し直すだけ」で、
    /// **走っている版の番号はその判断に効かない**（新版の番号はそもそも幅の都合で
    /// どの状態でも出していない）。番号が要るときは `ccdesk --version` が答える
    fn shows_version(self) -> bool {
        self != Self::RestartPending
    }

    /// 押したときの動作（＝行に付ける [`RowAction`]）。押して意味があるのは
    /// `update`（更新の実行）だけ。**差し替え済みの行も押せない**: 自動再起動を
    /// やめたので、案内を出すだけの [`SidebarRow::Inert`] に留める
    /// （それでも行は行なので、選択・ホバーの対象からは外れない）
    fn action(self, update: RowAction) -> SidebarRow {
        match self {
            // 据え置きも押せる: agent 側の詰まりが解ければ次の一押しで通るので、
            // 押せなくすると利用者が直したあとに ccdesk を再起動する羽目になる
            Self::Available | Self::Stalled => SidebarRow::Action(update),
            _ => SidebarRow::Inert,
        }
    }

    /// 行のスタイル。最新は dim（背景情報）、やることがある行は本文色にする
    /// （dim だと更新の存在に気づかない）。差し替え済みも押せないだけで
    /// 気づいてほしい情報なので dim には落とさない
    fn style(self) -> Style {
        match self {
            Self::Current => Style::default().fg(ui().dim),
            Self::Running => Style::default().fg(ui().working),
            // 据え置きは**人の手当てが要る**ので、やることがある行より強い色で出す
            // （更新が効かないまま何日も気づかれない状態が実際に起きた）。
            // `fail` ではない: agent は成功と言っていて、失敗したのは差し替えだけ
            Self::Stalled => Style::default().fg(ui().attention),
            Self::Available | Self::RestartPending => Style::default().fg(MUTED_FG),
        }
    }
}

/// バージョン行 1 本の文面。`<マーカー> <記号> <名前> v<版>` を左に、動詞を右端へ寄せる。
///
/// `glyph` は agent の記号（[`agent_glyph`]）。**ccdesk の行は空白 1 桁**を置くので、
/// 4 つの [`UpdateState`] のどれでも、agent 行でも ccdesk 行でも名前の開始桁が動かない。
///
/// **この桁が一覧の凡例を兼ねる。** セッション行のドットは形で agent を表すが、
/// 形と綴りが並ぶ場所はここしかない（一覧の行に綴りを戻すと名前の桁を食う）。
///
/// 版が未取得（起動直後・CLI 失敗）なら番号を出さない ＝ 誤情報を出さない。
/// **差し替え済みの行も番号を出さない**（右端の語に桁を譲る。[`UpdateState::shows_version`]）
fn version_row(
    glyph: &str,
    name: &str,
    version: &str,
    state: UpdateState,
    inner_width: u16,
) -> String {
    use unicode_width::UnicodeWidthStr;
    let mark = if state == UpdateState::Current {
        " " // マーカー桁を空白で確保する（更新が出ても名前の桁が動かない）
    } else {
        UPDATE_MARK
    };
    let left = if version.is_empty() || !state.shows_version() {
        format!("{mark} {glyph} {name}")
    } else {
        format!("{mark} {glyph} {name} v{version}")
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
    agents: &[(Kind, String, UpdateState)],
    inner_width: u16,
) -> Vec<(String, Style, SidebarRow)> {
    // ccdesk 自身は agent ではないので記号を持たない（桁だけ空白で確保する）
    let mut rows = vec![(
        version_row(
            " ",
            "ccdesk",
            env!("CARGO_PKG_VERSION"),
            ccdesk,
            inner_width,
        ),
        ccdesk.style(),
        ccdesk.action(RowAction::UpdateCcdesk),
    )];
    // **agent ごとに 1 行。** 1 行へ詰めると横に長くなり、更新導線も行単位で
    // 押せなくなる（どちらの更新かを行が名乗れない）。
    // 記号と綴りが 1 行に並ぶので、この行がそのまま一覧のドットの凡例になる
    rows.extend(agents.iter().map(|(kind, version, state)| {
        (
            version_row(
                agent_glyph(*kind),
                kind.title(),
                version,
                *state,
                inner_width,
            ),
            state.style(),
            state.action(RowAction::UpdateAgent(*kind)),
        )
    }));
    rows.push((
        separator_text(inner_width),
        Style::default().fg(ui().dim),
        SidebarRow::Decoration,
    ));
    rows
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
/// （OS 通知に出すプロジェクト名も同じ材料 ＝ [`crate::notify`]）
pub(crate) fn leaf_name(cwd: &str) -> Option<String> {
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
        SelfUpdate::Done => UpdateState::RestartPending,
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

/// claude 本体の版行の状態。ccdesk 側と違って RestartPending を持たないのは、更新後に
/// `claude --version` が新しい版を返して `footer.latest` が消える ＝ 行が自然に
/// 最新表示へ戻るため。ネイティブインストールは既定で自動更新するので、
/// 何もしなくてもこの行が消えることもある（公式仕様）
fn agent_update_state(app: &App, kind: Kind) -> UpdateState {
    // **状態は agent ごと**（片方の更新中にもう片方の行まで止めない）
    let state = app.agent_update.get(&kind).map(|s| s.lock_recover());
    let version = app.footer.version(kind);
    match state.as_deref() {
        Some(crate::app::AgentUpdate::Running) => UpdateState::Running,
        // **据え置きは「押した時点の版のまま」であることが条件。** 版が動けば
        // （手で入れ直した・agent 自身の自動更新が通った）条件が外れて行が
        // 最新表示へ戻る ＝ 旗を降ろす規則をどこにも書かなくてよい
        Some(crate::app::AgentUpdate::Stalled { version: at, .. })
            if version.current == *at && version.latest.is_some() =>
        {
            UpdateState::Stalled
        }
        _ if version.latest.is_none() => UpdateState::Current,
        _ => UpdateState::Available,
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
/// - ペインに出ている ＝ 行頭 1 桁目の記号（[`OPEN_MARK`] / [`SCREEN_MARK`]）と名前の太字
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
    /// 今ペインに出ている行（フォーカス中かは問わない）
    open: bool,
    /// `open` な行のうち、出ているスロットが今フォーカス中か。
    /// **`open` が false なら意味を持たない**（呼び手は常に false を渡す）
    focused: bool,
}

impl Look {
    /// その位置の見た目。`open` / `focused` は一覧の行だけが持つ
    /// （飾りやアカウント行はどちらも false）
    fn at(app: &App, pos: SidebarPos, open: bool, focused: bool) -> Self {
        Self {
            band: app.selection == pos || app.hovered == Some(pos),
            selected: app.selection == pos,
            open,
            focused,
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
    /// 行の identity。**名前の直後に出す短い ID の材料**（[`row_name_spans`]）で、
    /// `ccdesk list` が出すものと同じ [`SessionId::short`] を読む ＝
    /// 画面で読んだ 8 桁がそのまま宛先として通る
    id: SessionId,
    /// 状態そのもの。**ドットの色・明滅も行末の語と色もここから導く**
    /// （[`State::color`] / [`State::blinks`] / [`State::title`]）ので、
    /// `group` と食い違う色や語を別に持たせられない（これを別フィールドで
    /// 持っていた頃は、`look_fixture` のような手組みの `RowData` で
    /// Stopped なのに Working の色、という矛盾を作れてしまっていた）
    group: State,
    /// どの agent の行か。**行末に出す綴りの材料**（[`Kind::title`]）。
    /// grouping が agent のときの振り分けキーでもある
    kind: Kind,
    cwd: String,
    label: String,
    /// 今ペインに出ている行（[`Look::open`] の材料）
    is_active_window: bool,
    /// 未読（[`crate::hooks::HookStates::unread`]）＝ ドットの塗り（[`dot_glyph`]）
    unread: bool,
    /// ピン留め（[`PINNED_TITLE`] の節へ移す）
    pinned: bool,
}

/// セッション行 1 本の見た目。**行の組み立てはここ 1 箇所**なので、
/// 帯（選択・ホバー）と印（ペインに出ている・ドット）の重なり方も含めて
/// [`Frame`] を用意せずに検査できる。
///
/// `blink_tick` は明滅の通し番号（[`crate::theme::BLINK_TICK_MS`] 刻み。
/// [`State::blinks`] な行だけに効く）。**時計を直接読まず引数で受ける**ので、
/// 位相を固定してテストできる（[`draw`] は 1 フレームぶんの全行に同じ番号を渡す）
fn session_row_line(d: &RowData, look: Look, inner_width: u16, blink_tick: u64) -> Line<'static> {
    let color = row_state_color(d.group, blink_tick);
    // 行頭のペイン印 + ドット + 空白（消えている側も同じ幅を取る）
    let mut spans = vec![
        Span::styled(
            open_mark(look.open, look.focused),
            Style::default().fg(ui().emph).add_modifier(Modifier::BOLD),
        ),
        Span::styled(dot_glyph(d.kind, d.unread), Style::default().fg(color)),
        Span::raw(" "),
    ];
    let name_style = if look.open {
        Style::default().fg(ui().emph).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    // **agent の綴りはここに居ない。** どの agent の行かはドットの形が答えるので、
    // 行頭の略記（かつての `[cc]` / `[cx]`）も行末の綴りも要らない。
    // 名前の直後に短い ID が並ぶ幅もある（[`row_name_spans`]）
    spans.extend(row_name_spans(d, inner_width, name_style));
    spans.extend(row_tail_spans(d, inner_width, blink_tick));
    // 行末のメニュー記号（当たり判定は [`menu_zone`] が同じ桁から導く）
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        MENU_MARK,
        Style::default().fg(if look.band { ui().emph } else { MUTED_FG }),
    ));
    Line::from(spans).style(look.band(Style::default()))
}

/// 行末ブロックのうち状態語が食う桁。前の空白 1 桁を含む。
/// **agent はここに居ない**（ドットの形が答えるので、綴りで場所を取らない）
const TAIL_STATE_COLS: usize = 1 + State::TITLE_COLS;

/// この幅で行末ブロックに何桁使うか（`body` ＝ 行頭とメニューを除いた予算）。
///
/// 落ちるのは状態語だけ。落ちても**ドットの色が状態を語り続ける**ので、
/// 一番狭いサイドバーで状態が読めなくなることはない。
/// **名前の予算とブロックの中身が同じこの関数を読む**ので、桁が食い違わない
fn tail_cols(body: usize) -> usize {
    if body >= MIN_NAME_COLS + TAIL_STATE_COLS {
        TAIL_STATE_COLS
    } else {
        0
    }
}

/// この状態の行が今このコマで使う色。**ドットと状態語が同じここを読む**ので、
/// 片方だけ明滅する（＝ 行の中で 2 つのものが別々のリズムで動く）ことがない。
///
/// 明滅するのは Working だけ（[`State::blinks`]）。コマ列は
/// [`crate::theme::UiTheme::blink`] が持つ ＝ 谷の深さも段階数もテーマ側の 1 箇所で決まる
fn row_state_color(group: State, blink_tick: u64) -> Color {
    if group.blinks() {
        ui().blink(blink_tick)
    } else {
        group.color()
    }
}

/// 行末ブロックの中身（状態語だけ）。
///
/// **状態を文字で出すのがここ 1 箇所。** 色は [`row_state_color`] と同じ 1 箇所から
/// 引くので、語と色（と明滅の位相）が食い違わない。固定桁で左詰めするのは、
/// 揃えないと右端のメニュー記号の手前が行ごとにガタつくため
fn row_tail_spans(d: &RowData, inner_width: u16, blink_tick: u64) -> Vec<Span<'static>> {
    let body = (inner_width as usize).saturating_sub(HEAD_COLS + MENU_COLS);
    if tail_cols(body) == 0 {
        return Vec::new();
    }
    vec![
        Span::raw(" "),
        Span::styled(
            format!("{:<width$}", d.group.title(), width = State::TITLE_COLS),
            Style::default().fg(row_state_color(d.group, blink_tick)),
        ),
    ]
}

/// 名前ブロック（名前 + 短い ID + 詰め物）が使える桁。
/// 「内側の幅 - 行頭 [`HEAD_COLS`] - 行末のメニュー [`MENU_COLS`] -
/// 行末ブロック [`tail_cols`]」
fn name_block_cols(inner_width: u16) -> usize {
    let body = (inner_width as usize).saturating_sub(HEAD_COLS + MENU_COLS);
    body - tail_cols(body)
}

/// 短い ID が食う桁。**名前との間の空白 1 桁を含む**
/// （桁数の正本は [`crate::sessions::SHORT_ID_COLS`]）
const SHORT_ID_BLOCK_COLS: usize = 1 + crate::sessions::SHORT_ID_COLS;

/// 既定幅（[`DEFAULT_SIDEBAR`]）のサイドバーで名前が持つ桁。
/// **ID を出してよいかの判断がこの値に乗る**（[`shows_short_id`]）
const NAME_COLS_AT_DEFAULT: usize =
    (DEFAULT_SIDEBAR as usize) - 2 - HEAD_COLS - MENU_COLS - TAIL_STATE_COLS;

/// この名前ブロックに短い ID を並べるか。
///
/// **ID は「サイドバーを広げたときだけ」出る。** 既定幅で出すと名前が 9 桁痩せる
/// （日本語のタイトルなら 4 文字ぶん消える）。ID が要るのは他のセッションを
/// 宛先として指すときだけで、名前は常に読む ＝ 常に読む方を痩せさせない。
///
/// 閾値を「既定幅のときの名前の桁」に置いたのは、**恣意的な数値を置かない**ため:
/// ID が見えているどの幅でも、名前は既定幅と同じかそれより広い。
///
/// **広げた瞬間に名前の桁が一度縮む**のは承知の上（境目をまたぐと ID が
/// 9 桁を取るため）。ID が現れることでその縮みは説明が付く
fn shows_short_id(block: usize) -> bool {
    block >= NAME_COLS_AT_DEFAULT + SHORT_ID_BLOCK_COLS
}

/// この内側幅でセッション名そのものに使える桁。**名前の予算はここ 1 箇所**で決まる。
///
/// `pub(crate)` なのは、撮影用のサイドバー幅（[`crate::source`]）が
/// 「この名前が切れない幅か」をここから導くため ＝ 行の桁割りを変えたときに
/// 撮影側の検査が黙って古くならない
pub(crate) fn name_cols(inner_width: u16) -> usize {
    let block = name_block_cols(inner_width);
    if shows_short_id(block) {
        block - SHORT_ID_BLOCK_COLS
    } else {
        block
    }
}

/// 名前ブロックの中身（名前 + 短い ID + 詰め物）。**合計は必ず
/// [`name_block_cols`] ちょうど**なので、メニュー記号は常に内側の右端に来る
/// （[`menu_zone`] の当たり判定が成り立つ前提）。
///
/// **ID は名前の直後**（行末の固定列ではない）。指したいセッションの名前を
/// 見つけたら、視線を動かさずにその宛先が読める。位置が行ごとに動くのは承知の上で、
/// ID は流し読みで拾うものではなく「この行だ」と決めた後に読むものなので、
/// 桁が揃っていることに意味が無い。
///
/// **ID は dim。** 行を縦に流し読みするときに拾うのは名前と状態なので、
/// 名前と同じ濃さで隣に並べない
fn row_name_spans(d: &RowData, inner_width: u16, name_style: Style) -> Vec<Span<'static>> {
    use unicode_width::UnicodeWidthStr;
    let block = name_block_cols(inner_width);
    let name = clip_to_width(&d.label, name_cols(inner_width) as u16);
    let mut used = name.width();
    let mut spans = vec![Span::styled(name, name_style)];
    if shows_short_id(block) {
        // 8 桁に満たない ID（壊れた保管など）でも桁が合うよう、実測した幅で数える
        let short = d.id.short();
        used += 1 + short.width();
        spans.push(Span::raw(" "));
        spans.push(Span::styled(short, Style::default().fg(ui().dim)));
    }
    spans.push(Span::raw(" ".repeat(block - used)));
    spans
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
            (LOGGED_OUT_ROW.to_string(), Style::default().fg(ui().attention))
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
/// 幅は内容が決める（[`popup_width`]）ので、サイドバーより広い
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
    let entries = popup.kind.entries(app.grouping, &app.kinds);
    let (term_w, term_h) = (app.term_size.0.max(1), app.term_size.1.max(1));
    let width = popup_width(&popup.kind, app.grouping, &app.kinds).min(term_w);
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
fn popup_width(kind: &PopupKind, grouping: Grouping, kinds: &[Kind]) -> u16 {
    use unicode_width::UnicodeWidthStr;
    let widest = kind
        .entries(grouping, kinds)
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
        if app.focus_is_new() {
            return None;
        }
        // ccdesk が取るのは予約キーだけ。残りは全部**その行の agent**が受ける
        let agent = app
            .shown_session()
            .and_then(|id| app.row(id))
            .map_or(Kind::default(), |row| row.kind);
        return Some((
            "terminal",
            format!("all keys pass through to {}", agent.title()),
        ));
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
                .style(Style::default().fg(ui().fail)),
            area,
        );
        return;
    }
    let mut hint_spans = vec![
        Span::styled(" app:", Style::default().fg(MUTED_FG)),
        Span::raw(" Ctrl+Q quit · Alt+←→ focus"),
    ];
    // スロット間の移動は**2 枚以上のときだけ出す**（1 枚では行き先が無く、
    // 押しても何も起きないキーを案内すると嘘になる ＝ [`context_hint`] と同じ規準）。
    // 枚数の正本は配置の側（[`crate::panes::Layout::slots`]）から引く
    if app.layout.slots() > 1 {
        hint_spans.push(Span::raw(" · Alt+Shift+←→↑↓ slot"));
    }
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
    if app.input_gate.is_some() && !app.focus_is_new() {
        hint_spans.push(Span::styled(
            "  starting session…",
            Style::default().fg(ui().working),
        ));
    }
    // 右端: 使用率（opt-in）。中身は [`crate::usage`] が取ったもので、
    // **statusline には一切関与しない**。
    //
    // **無言の空白を作らない**のが要点。以前は「opt-in していない」
    // 「取得が効いていない」「枠が無いアカウント」「壊れた」が全部同じ
    // 見え方（何も出ない）で、opt-in したのに出ない人へ渡せる情報が無かった
    // **当たり判定（[`usage_hits`]）と同じ導出**を通す
    let usage_lines = usage_footer(app);
    let usage_w = usage_lines.iter().map(|l| span_width(l)).max().unwrap_or(0);
    let bar = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Length(usage_w)])
        .split(area);
    // **キーヒントは下部バーの 1 行目の左。** 使用率は右側で 2 行を使うので、
    // ヒントを最下行に置くと**左上が 1 行まるごと空く**（右だけ 2 行埋まって
    // 左は下 1 行、という段違いになる）。上に詰めれば空くのは左下だけで、
    // 使用率の 2 行目（codex）と横に並ぶ
    let hint_row = Rect {
        height: 1,
        ..bar[0]
    };
    // new session 画面のヒントはペイン内に出すため、下部バーには重ねない
    frame.render_widget(
        ratatui::widgets::Paragraph::new(Line::from(hint_spans))
            .style(Style::default().fg(ui().dim)),
        hint_row,
    );
    if usage_w > 0 {
        frame.render_widget(
            ratatui::widgets::Paragraph::new(
                usage_lines.into_iter().map(Line::from).collect::<Vec<_>>(),
            ),
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
/// その行の**今のライブ状態**と、**claude がそれを書いた時刻**（`~/.claude/sessions/`
/// の interactive エントリ）。載っていなければ `("", 0)` ＝ 未観測。
///
/// **対で返すことがこの関数の存在理由。** かつて時刻の側だけを ccdesk 自身の観測時刻
/// （[`App::agents_observed_at`]）で埋めていた。その値は常に「今」なので
/// **status が hook に必ず勝ち**、陳腐化した `busy` を新しい `idle` hook で
/// 降ろせない ＝ 行が赤のまま固着した（実機で観測）。引数に観測時刻を渡さない形に
/// してあるので、同じ取り違えをもう一度書くことができない
/// **照合は会話 ID。** ライブ状態が名乗るのは claude 側の `sessionId` ＝ 会話で、
/// ccdesk の行 ID はそこに出てこない（`CCDESK_ROW` 以外のどこにも出さない）。
/// 会話を知らない行は未観測（`("", 0)`）で、状態は hook と PTY だけで決まる
pub(crate) fn pane_rect(app: &App) -> Rect {
    let (w, h) = (app.term_size.0, app.term_size.1);
    let sidebar = sidebar_cols(app).min(w);
    Rect::new(sidebar, 0, w - sidebar, h.saturating_sub(bottom_bar_rows(app)))
}

pub(crate) fn draw(frame: &mut Frame, app: &mut App) -> FrameCursor {
    // 画面の末尾は横断の下部バー（agent ごとに 1 行 + 1 行目にキーヒント）
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(bottom_bar_rows(app))])
        .split(frame.area());
    // 横の分割は pane_rect と同じ答えになる（右ペインの矩形の正本はあちら）
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(sidebar_cols(app)), Constraint::Min(1)])
        .split(vert[0]);

    // サイドバー: **行の正本は `~/.ccdesk/sessions.json`**（`app.sessions`）。
    // 生死は自分の子プロセス（`child.try_wait()`）が、生きている行のライブ状態は
    // `~/.claude/sessions/` の `status` が答える。
    // Working の明滅の位相もここで取る時刻（`ccdesk::now_ms`）から決める
    let now_ms = ccdesk::now_ms();

    // 通知もこの写しを使う。状態材料の裁定を描画だけに閉じると、承認後に
    // status が hook を追い越したとき通知だけ Waiting のまま残る
    let effective_states = crate::app::effective_row_states(app);
    // **今どれかのスロットに出ている行**（印を付ける対象）。フォーカス中の 1 枚だけ
    // ではないのが要点で、4 枚並べたら 4 行に印が付く
    let on_screen: Vec<crate::sessions::SessionId> = app
        .slots
        .iter()
        .filter_map(|slot| slot.session().cloned())
        .collect();
    // Working の明滅のコマ番号。**時計を読むのはここ 1 箇所**にして、1 フレームぶんの
    // 全行へ同じ番号を配る（[`session_row_line`] は時計を読まない）。
    // コマ列の長さ（＝ 何段階で何秒周期か）はテーマが持つので、ここは刻むだけ
    let blink_tick = now_ms / crate::theme::BLINK_TICK_MS;

    // ---- 行データを先に組み立てる（State / Directory 両グルーピング対応）----
    //
    // **切った agent の行はここで落とす。** 集計も節も選択の寄せ直しも、この後は
    // `data` から導くので、落とす判断はこの 1 箇所で済む
    // （`sessions.json` の側は触らないので、on に戻せばそのまま出てくる）
    let kinds = app.kinds.clone();
    let mut data: Vec<RowData> = Vec::new();
    for row in app.sessions.iter().filter(|row| kinds.contains(&row.kind)) {
        // **未読は状態の材料でもある**（[`crate::poll::State::group`]）: 手が要らない行は
        // 「まだ見ていない（Done）」と「見終わった（Idle）」に割れる
        let unread = app.hook_states.unread(row);
        data.push(RowData {
            action: RowAction::Open(row.session_id.clone()),
            id: row.session_id.clone(),
            group: effective_states
                .get(&row.session_id)
                .copied()
                .unwrap_or(State::Stopped),
            kind: row.kind,
            cwd: row.cwd.clone(),
            label: app.titles.of(row),
            is_active_window: on_screen.contains(&row.session_id),
            unread,
            pinned: row.pinned,
        });
    }
    // 何か動いているか（run ループがアイドル時の描き直し間隔を選ぶ材料。
    // [`crate::app::App::animating`]）。材料は 2 つ: 明滅する行（表示行そのものから
    // 導く。[`State::blinks`]）と、使用率の取得中スピナー
    app.animating = data.iter().any(|d| d.group.blinks())
        || app
            .usage_fetching
            .values()
            .any(|flag| flag.load(std::sync::atomic::Ordering::Relaxed));

    // ---- 描画 ----
    // 行の見え方の規則は [`Look`] 1 つ（帯 = 選択・ホバー / 印 = ペインに出ている）。
    // **一覧の行（下）とフッターのアカウント行（末尾）が同じ規則を読む**ので、
    // 「どこが光るか」の知識が 2 箇所に分かれない
    let inner_width = chunks[0].width.saturating_sub(2);
    let mut items: Vec<ListItem> = Vec::new();
    let mut rows: Vec<SidebarRow> = Vec::new();

    let focused_id = app.shown_session();
    let push_data_row = |items: &mut Vec<ListItem>, rows: &mut Vec<SidebarRow>, d: &RowData| {
        let cur = rows.len();
        let focused = d.is_active_window && focused_id == Some(&d.id);
        let look = Look::at(app, SidebarPos::Row(cur), d.is_active_window, focused);
        items.push(ListItem::new(session_row_line(d, look, inner_width, blink_tick)));
        rows.push(SidebarRow::Action(d.action.clone()));
    };
    // セッション行以外の 1 行を積む。**items と rows が 1:1 であること**と
    // 「帯（選択・ホバー）を載せるのは触れる行だけ」の規則を、行種ごとに
    // 書き写さずここ 1 箇所で守る（片方だけ push すると全行のヒットテストがずれる）。
    //
    // `keep_fg`: 選択時に前景色を `emph` へ差し替えないための逃げ道。版行のように
    // 前景色そのものが状態（更新中の赤など）を運ぶ行では、選択で emph に潰すと
    // 状態が読めなくなる（選択中だけ更新中の赤が消える不具合の原因だった）。
    // 帯（背景）は掛かるので「今ここ」は変わらず読める
    let push_row = |items: &mut Vec<ListItem>,
                    rows: &mut Vec<SidebarRow>,
                    line: Line<'static>,
                    base: Style,
                    kind: SidebarRow,
                    keep_fg: bool| {
        let style = if kind.selectable() {
            let banded = Look::at(app, SidebarPos::Row(rows.len()), false, false).band(base);
            match (keep_fg, base.fg) {
                (true, Some(fg)) => banded.fg(fg),
                _ => banded,
            }
        } else {
            base
        };
        items.push(ListItem::new(line.style(style)));
        rows.push(kind);
    };

    // 先頭: ccdesk と各 agent の版行、そして区切り線。
    // 更新があるときだけ行全体がクリック可
    let agents: Vec<(Kind, String, UpdateState)> = app
        .kinds
        .iter()
        .copied()
        .map(|kind| {
            (
                kind,
                app.footer.version(kind).current,
                agent_update_state(app, kind),
            )
        })
        .collect();
    for (text, style, row) in version_rows(ccdesk_update_state(app), &agents, inner_width) {
        // keep_fg = true: 版行の前景色は状態そのもの（更新中の赤など）なので、
        // 選択しても emph に潰さず保つ
        push_row(&mut items, &mut rows, Line::from(text), style, row, true);
    }

    // 新規セッション
    push_row(
        &mut items,
        &mut rows,
        Line::from("+ new session"),
        Style::default(),
        SidebarRow::Action(RowAction::New),
        false,
    );
    // 区切り線: new session（アクション）とセッション一覧領域を分ける（Desktop 風）
    push_row(
        &mut items,
        &mut rows,
        Line::from(separator_text(inner_width)),
        Style::default().fg(ui().dim),
        SidebarRow::Decoration,
        false,
    );
    // スロットの並べ方（クリックでメニューが開く）。現在値の綴りは Layout::as_str
    push_row(
        &mut items,
        &mut rows,
        Line::from(vec![
            Span::raw("▦ layout: "),
            Span::styled(app.layout.as_str(), Style::default().fg(ui().emph)),
        ]),
        Style::default().fg(ui().dim),
        SidebarRow::Action(RowAction::ChooseLayout),
        false,
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
        false,
    );
    // **状態ごとの件数は出さない。** 数は節の見出しの下に並ぶ行そのもので見えており、
    // 同じ知識をヘッダーにもう 1 本置くと幅を食うだけだった。
    //
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
        push_row(items, rows, Line::from(""), Style::default(), SidebarRow::Decoration, false);
        push_row(
            items,
            rows,
            Line::from(title.to_string()),
            Style::default().fg(ui().dim),
            SidebarRow::Decoration,
            false,
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
            for group in State::ORDER {
                let members: Vec<&RowData> = unpinned
                    .iter()
                    .copied()
                    .filter(|d| d.group == group)
                    .collect();
                push_section(&mut items, &mut rows, group.title(), &members);
            }
        }
        // **種別の節はここだけ**。他の 2 軸では claude と codex が同じ節に並ぶ
        // （同じプロジェクトの作業を種別で引き離さない）
        Grouping::Agent => {
            for kind in &kinds {
                let members: Vec<&RowData> = unpinned
                    .iter()
                    .copied()
                    .filter(|d| d.kind == *kind)
                    .collect();
                push_section(&mut items, &mut rows, kind.title(), &members);
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
                push_row(&mut items, &mut rows, Line::from(""), Style::default(), SidebarRow::Decoration, false);
                push_row(
                    &mut items,
                    &mut rows,
                    Line::from(row.heading),
                    Style::default().fg(ui().dim),
                    SidebarRow::Action(RowAction::Project(row.cwd)),
                    false,
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
    let SidebarPos::Row(selected) = app.selection;
    let selection_lost = !app
        .sidebar_rows
        .get(selected)
        .is_some_and(SidebarRow::selectable);
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
    let entries = popup.kind.entries(app.grouping, &app.kinds);
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

/// ペイン枠の見出しでタイトルと短い ID を分ける区切り。
/// **前後の空白を含めて 3 桁**（桁の予算がこの定数の表示幅に乗る）
const PANE_TITLE_SEP: &str = " · ";

/// 枠の見出しが使える桁 ＝ ペインの幅から左右の角 1 桁ずつを引いたもの。
/// ratatui は角の内側から見出しを描き、溢れたぶんは黙って落ちる ＝
/// **ここで数えておかないと ID が先に消える**（見出しの末尾に居るため）
const PANE_TITLE_MARGIN: usize = 2;

/// 枠の右上に出す閉じる印。**押すとそのスロットが配置から外れる**（[`close_zone`]）。
/// セッションは止まらない ＝ ここで閉じるのは**表示枠**で、プロセスは裏で走り続ける
/// （[`crate::panes`] の書き出し）。
///
/// **East Asian Ambiguous でない記号だけを選ぶ**のは [`MENU_MARK`] と同じ判断で、
/// U+2715 は Neutral ＝ `width` / `width_cjk` の両方が 1 を返す（テストが直接測る）
const CLOSE_MARK: &str = "✕";

/// 閉じる印が食う桁（記号 + その左の空白）。**左の空白まで数えるのは
/// [`MENU_COLS`] と同じ理由**（記号 1 桁だけだと突きにくい）で、
/// 当たり判定（[`close_zone`]）も見出しの桁予算（[`pane_title`]）も同じ 2 桁を取る
const CLOSE_COLS: u16 = 2;

/// 閉じる印の当たり判定 `(桁の範囲, 行)`。印を出さない枠では `None` ＝
/// **描く判断と押せる判断が同じ 1 箇所**（[`with_close_mark`] がこれを見て載せる）。
///
/// 右寄せの見出しは**右の角の 1 桁内側**で終わるので、印は角の列に乗らない。
/// これは**十字の掴み代（隣り合う枠線 2 列）と重ならない**ための桁で、
/// 左列・上段のスロットの印がリサイズの掴み代を食わない。
///
/// 行のほうは避けられない: 下段スロットの上辺は横の境界そのものなので、
/// 掴み代に乗る。**押しの判定でこちらを先に見る**ことで決着させてある
/// （[`crate::app`] の `handle_mouse`）
pub(crate) fn close_zone(rect: Rect) -> Option<(std::ops::RangeInclusive<u16>, u16)> {
    if close_cols(rect.width) == 0 || rect.height == 0 {
        return None;
    }
    let right = rect.x + rect.width - 2;
    Some((right + 1 - CLOSE_COLS..=right, rect.y))
}

/// 見出しが閉じる印へ譲る桁（印を出さない狭い枠では 0）。
///
/// **印を出すかは幅だけで決まる**ので、判断の式をここ 1 つに置いて
/// [`close_zone`]（押せる場所）と [`pane_title`]（見出しの桁予算）が同じ答えを見る。
/// 左右の角 1 桁ずつと印の桁が要る ＝ これより狭い枠に印は出さない
/// （出しても押せる場所が角と重なる）
const fn close_cols(pane_width: u16) -> usize {
    if pane_width < 2 + CLOSE_COLS {
        0
    } else {
        CLOSE_COLS as usize
    }
}

/// 枠に閉じる印を載せる。**セッション・空・New 画面の 3 種の枠が同じここを通る**ので、
/// 「印が出ている枠と押せる枠」が種類ごとにずれない。
///
/// **フォーカス中の枠だけ明るく出す**のはサイドバーの [`MENU_MARK`] と同じ作法
/// （今の操作対象がどれかを、押せる入口の側でも示す）
pub(crate) fn with_close_mark<'a>(block: Block<'a>, rect: Rect, focused: bool) -> Block<'a> {
    if close_zone(rect).is_none() {
        return block;
    }
    let color = if focused { ui().emph } else { MUTED_FG };
    block.title_top(Line::styled(CLOSE_MARK, Style::default().fg(color)).right_aligned())
}

/// ✕ を押したスロット（どれでもなければ `None`）。
/// **矩形の正本は [`App::slot_rects`]** ＝ 描画と同じ矩形から導く
pub(crate) fn close_hit(app: &App, column: u16, row: u16) -> Option<usize> {
    app.slot_rects().iter().position(|rect| {
        close_zone(*rect).is_some_and(|(cols, at)| row == at && cols.contains(&column))
    })
}

/// ペイン枠の見出し（`<タイトル> · <短い ID>`）。
///
/// **ID を出すのは、名前に [`MIN_NAME_COLS`] 桁残るときだけ。** サイドバーの
/// 名前ブロック（[`shows_short_id`]）と同じ規則で、「ID は名前を削ってまで
/// 出さない」を画面の 2 箇所で同じ言い方にしてある（分割して細くなった
/// スロットでは ID が消え、タイトルだけが残る）。
///
/// **切るのは表示幅**（[`clip_to_width`]）: タイトルは会話から生成されるので
/// 全角を含み得る。
///
/// **右上の閉じる印（[`close_cols`]）のぶんも先に引く。** 見出しは左寄せ・印は
/// 右寄せで同じ 1 行に載るので、ここで数えないと長いタイトルが印を押し出す
fn pane_title(name: &str, id: &SessionId, pane_width: u16) -> String {
    pane_title_parts(name, id, pane_width).text
}

/// 見出しの文字列と、**その中で短い ID が占める桁**（見出しの先頭を 0 とする）。
///
/// 桁を一緒に返すのが要点: ID は押すとクリップボードへ載る
/// （[`id_zone`] → [`crate::app`] のマウス処理）ので、**見えている場所と押せる場所を
/// 同じ 1 つの計算から出す**（描画側だけが知っている桁割りを当たり判定へ書き写すと、
/// 名前の切り方を変えた日に黙ってずれる）
struct PaneTitle {
    text: String,
    /// 見出しの先頭から数えた ID の開始桁。None ＝ この幅では ID を出していない
    id_at: Option<usize>,
}

fn pane_title_parts(name: &str, id: &SessionId, pane_width: u16) -> PaneTitle {
    use unicode_width::UnicodeWidthStr;
    let budget = (pane_width as usize)
        .saturating_sub(PANE_TITLE_MARGIN)
        .saturating_sub(close_cols(pane_width));
    let short = id.short();
    // 区切りと ID が食う桁。ID は ASCII 16 進なので文字数 = 表示桁
    let tail = PANE_TITLE_SEP.width() + short.width();
    if budget < MIN_NAME_COLS + tail {
        return PaneTitle {
            text: clip_to_width(name, budget as u16),
            id_at: None,
        };
    }
    let name = clip_to_width(name, (budget - tail) as u16);
    PaneTitle {
        id_at: Some(name.width() + PANE_TITLE_SEP.width()),
        text: format!("{name}{PANE_TITLE_SEP}{short}"),
    }
}

/// 見出しの短い ID の当たり判定 `(桁の範囲, 行)`。ID を出していない幅では `None`
/// ＝ **見えていないものは押せない**（[`close_zone`] と同じ作法）。
///
/// 見出しは枠の左上（角の 1 桁内側）から左詰めで描かれるので、桁は
/// 「矩形の左端 + 1 + [`PaneTitle::id_at`]」から決まる
pub(crate) fn id_zone(
    rect: Rect,
    name: &str,
    id: &SessionId,
) -> Option<(std::ops::RangeInclusive<u16>, u16)> {
    use unicode_width::UnicodeWidthStr;
    if rect.height == 0 {
        return None;
    }
    let at = pane_title_parts(name, id, rect.width).id_at?;
    let left = rect.x + 1 + at as u16;
    let cols = id.short().width() as u16;
    Some((left..=left + cols - 1, rect.y))
}

/// 短い ID を押したスロットのセッション（どのスロットの ID でもなければ `None`）。
/// **矩形の正本は [`App::slot_rects`]**、見出しの組み方は [`pane_title_parts`] ＝
/// 描画と同じ導出から答える
pub(crate) fn id_hit(app: &App, column: u16, row: u16) -> Option<SessionId> {
    app.slot_rects()
        .into_iter()
        .enumerate()
        .find_map(|(at, rect)| {
            let id = app.slots.get(at).and_then(Slot::session)?;
            let name = app
                .row(id)
                .map_or_else(|| crate::title::UNTITLED.to_string(), |r| app.titles.of(r));
            let (cols, at_row) = id_zone(rect, &name, id)?;
            (row == at_row && cols.contains(&column)).then(|| id.clone())
        })
}

/// 右ペイン: スロットを並べて描く。
///
/// **カーソルを返すのはフォーカススロットだけ。** 端末のカーソルは 1 本しか無いので、
/// 他のスロットは描くだけで位置を主張しない（[`FrameCursor`] 参照）
fn draw_right_pane(frame: &mut Frame, pane: Rect, app: &mut App) -> FrameCursor {
    let rects = app.layout.rects(pane, app.split);
    // フォーカススロットが無いフレームでも物理カーソルは駐車させる（FrameCursor 参照）
    let mut cursor = FrameCursor::hidden_at(pane_fallback_pos(pane));
    for (at, rect) in rects.into_iter().enumerate() {
        let focused = app.focus == Focus::Terminal && app.focus_slot == at;
        let found = draw_slot(frame, rect, app, at, focused);
        if app.focus_slot == at {
            cursor = found;
        }
    }
    draw_drop_guide(frame, app);
    cursor
}

/// **掴んでいるセッションの落とし先を塗る**（IDE のタブドロップと同じ見え方）。
///
/// 塗る範囲は [`crate::app::drop_rect`] ＝ ドロップを実行する側と同じ答えなので、
/// 「塗られた場所と違うところに入る」が構造的に起きない。
///
/// **落とせないところでは何も塗らない**（4 分割の縁・育った先が端末に入らないとき）。
/// 塗りが消えること自体が「ここへは落とせない」の返事になる ＝
/// 落としてから何も起きないより早く分かる
fn draw_drop_guide(frame: &mut Frame, app: &App) {
    let Some(drag) = app.drag.as_ref() else {
        return;
    };
    // 押しただけ（まだ動いていない）ではクリックかもしれないので塗らない
    if !drag.moved {
        return;
    }
    let Some(rect) = drag.target.and_then(|t| crate::app::drop_rect(app, t)) else {
        return;
    };
    // 半透明が無いので、下の内容は消して塗り潰す（透けさせると落とし先の
    // 輪郭が読めない ＝ ガイドの用が足りない）
    frame.render_widget(ratatui::widgets::Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(grabbed_title(app, &drag.what, rect.width))
        .border_style(Style::default().fg(ui().emph).add_modifier(Modifier::BOLD))
        .style(Style::default().bg(ui().hl_bg));
    frame.render_widget(block, rect);
}

/// ガイドの見出し ＝ **掴んでいるものの名前**（落とすと何が出るか）。
/// セッション行はペインの見出しと同じ綴り（[`pane_title`]）なので、
/// 落ちた後に出る枠と読みが変わらない
fn grabbed_title(app: &App, what: &Grab, width: u16) -> String {
    match what.shows() {
        // 掴んだのが既にある行なら、その名前（ペインの見出しと同じ綴り）
        Grabbed::Session(id) => {
            let name = app
                .row(id)
                .map_or_else(|| crate::title::UNTITLED.to_string(), |row| app.titles.of(row));
            pane_title(&name, id, width)
        }
        // メニューの `new <agent> session` は**どのフォルダで始まるか**まで出す
        // （掴み間違いが落とす前に分かる）
        Grabbed::NewIn(cwd) => {
            let leaf = leaf_name(cwd).unwrap_or_else(|| cwd.to_string());
            format!("new session in {leaf}")
        }
        Grabbed::New => "new session".to_string(),
    }
}

/// スロット 1 枚。戻り値はその中身が主張するカーソル（採るかは呼び手が決める）
fn draw_slot(frame: &mut Frame, rect: Rect, app: &mut App, at: usize, focused: bool) -> FrameCursor {
    let starting = app.input_gate.is_some() && app.focus_slot == at;
    // Esc で戻れる先（セッションの窓）があるか。**借用の前に取る**（New 画面の
    // 描画はスロットを可変で借りるため）
    let can_leave = !app.windows.is_empty();
    // 見出しは名前を引くために不変で借りるので、可変借用より先に組む。
    // **見出しには短い ID も入る**（[`pane_title`]）＝ 今どのセッションを見ているかを
    // `ccdesk list` と同じ 8 桁で名乗るので、そのまま宛先として打てる
    let title = match app.slots.get(at) {
        Some(Slot::Session(id)) => {
            let name = app
                .row(id)
                .map_or_else(|| crate::title::UNTITLED.to_string(), |row| app.titles.of(row));
            Some(pane_title(&name, id, rect.width))
        }
        _ => None,
    };
    match app.slots.get_mut(at) {
        Some(Slot::New(state)) => draw_new_view(frame, rect, state, focused, starting, can_leave),
        Some(Slot::Session(_)) => {
            let title = title.unwrap_or_else(|| crate::title::UNTITLED.to_string());
            draw_session_slot(frame, rect, app, at, focused, title)
        }
        // 空スロット（起動時・stop / close の直後）。枠だけだと「壊れている」
        // のか「ただ空」なのか見分かないので、案内を 1 行出す
        _ => {
            let block = with_close_mark(
                Block::default()
                    .borders(Borders::ALL)
                    .title("no session")
                    .border_style(border_style(focused)),
                rect,
                focused,
            );
            let inner = block.inner(rect);
            frame.render_widget(block, rect);
            let mid = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(0), Constraint::Length(1), Constraint::Min(0)])
                .split(inner)[1];
            frame.render_widget(
                ratatui::widgets::Paragraph::new("pick a session, or + new session, from the sidebar")
                    .style(Style::default().fg(ui().dim))
                    .alignment(ratatui::layout::Alignment::Center),
                mid,
            );
            FrameCursor::hidden_at(pane_fallback_pos(rect))
        }
    }
}

/// セッションを映しているスロット。窓が見つからない（起こし損ねた）ときは空扱い
fn draw_session_slot(
    frame: &mut Frame,
    rect: Rect,
    app: &App,
    at: usize,
    focused: bool,
    title: String,
) -> FrameCursor {
    let Some(window) = app
        .slots
        .get(at)
        .and_then(Slot::session)
        .and_then(|id| app.windows.iter().find(|w| &w.session_id == id))
    else {
        frame.render_widget(
            with_close_mark(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(Style::default().fg(ui().dim)),
                rect,
                focused,
            ),
            rect,
        );
        return FrameCursor::hidden_at(pane_fallback_pos(rect));
    };
    let parser = window.parser.lock_recover();
    let screen = parser.screen();
    // スクロールバックを見ている間は、新しい出力が来ても画面が動かない
    // （vt100 が見ている位置を保つ）。**止まったのか遡っているのかは
    // 見ただけでは区別が付かない**ので、枠に遡り量を出す
    let scrolled = screen.scrollback();
    let title = if scrolled > 0 {
        format!("{title} ↑{scrolled}")
    } else {
        title
    };
    let block = with_close_mark(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(border_style(focused)),
        rect,
        focused,
    );
    let inner = block.inner(rect);
    // tui-term 独自の █ カーソル描画は無効化し、ネイティブカーソル
    // （set_cursor_position = 本家と同じ点滅バー）だけを使う
    let widget = PseudoTerminal::new(screen)
        .cursor(tui_term::widget::Cursor::default().visibility(false))
        .block(block);
    frame.render_widget(widget, rect);

    // カーソル位置を反映。フォーカス外・子が非表示指定のときも「隠すだけ」で
    // 位置は必ず確定させる（描かないとサイドバーに置き去りになる。FrameCursor 参照）。
    // ペイン外へはみ出す座標はペイン内へクランプする
    let (crow, ccol) = screen.cursor_position();
    let pos = terminal_cursor_pos(rect, inner, crow, ccol);
    // 遡っている間はカーソルを出さない（座標は「今の画面」のもので、
    // 表示している行とは無関係 ＝ 出すと無関係な位置で点滅する）
    if focused && !screen.hide_cursor() && scrolled == 0 {
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
    use crate::app::live_status;
    use crate::hooks::Reported;
    use crate::poll::{row_state, state_of_status, PtyHint, Run, State};

    /// テスト用: claude の `status` の生値を [`Run::observed`] の形へ
    /// （本番でこの写しをするのは [`crate::app`] の `observed_state`）
    fn observed(status: &str, at: u64) -> Option<(State, u64)> {
        state_of_status(status).map(|state| (state, at))
    }

    // 語彙の別名（綴りの正本は [`State::as_str`]）
    const WORKING: State = State::Working;
    const WAITING: State = State::Waiting;
    const IDLE: State = State::Idle;
    const STOPPED: State = State::Stopped;
    // Color は本番コードでは型名を直接書かない（`d.group.color()` で済む）ので、
    // テストだけで使う型としてここで読み込む
    use ratatui::style::Color;

    /// **幅の下限は「短い項目しか無いメニューが痩せない」ための床**で、
    /// grouping 切替（最長 `  directory` = 11 桁）がそれに当たる。
    /// セッションのメニューは項目が増えて床を越えたので、最長項目から決まる
    #[test]
    fn menu_width_is_the_longest_entry_but_never_below_the_floor() {
        use unicode_width::UnicodeWidthStr;
        let all = Kind::ORDER;
        assert_eq!(popup_width(&PopupKind::State, Grouping::State, &all), POPUP_MIN_WIDTH);
        assert_eq!(popup_width(&PopupKind::State, Grouping::Directory, &all), POPUP_MIN_WIDTH);
        let kind = PopupKind::Session {
            id: SessionId::new("s1"),
            pinned: false,
            open: true,
        };
        let widest = kind
            .entries(Grouping::State, &all)
            .iter()
            .map(|entry| entry.label.width())
            .max()
            .unwrap() as u16;
        assert_eq!(popup_width(&kind, Grouping::State, &all), widest + POPUP_CHROME);
        assert!(
            popup_width(&kind, Grouping::State, &all) > POPUP_MIN_WIDTH,
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

    /// **使用率の行はその agent のアカウントを出す。** claude と codex は別の
    /// アカウントで、片方の名前をもう片方の行へ出すと誰の枠か分からなくなる
    /// （実際に claude の名前が codex の行にも出ていた）
    #[test]
    fn each_usage_row_shows_its_own_account() {
        let labels = [(Kind::Claude, "alice-cc"), (Kind::Codex, "alice-cx@example")];
        let app = App {
            usage: Kind::ORDER
                .into_iter()
                .map(|k| (k, crate::usage::sample_ready(Vec::new())))
                .collect(),
            footer: crate::poll::FooterInfo {
                accounts: labels
                    .iter()
                    .map(|(kind, label)| {
                        (*kind, AccountStatus::LoggedIn((*label).to_string()))
                    })
                    .collect(),
                ..Default::default()
            },
            term_size: (140, 30),
            ..Default::default()
        };
        let lines: Vec<String> = usage_footer(&app)
            .iter()
            .map(|line| line.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(lines.len(), labels.len(), "one line per agent: {lines:?}");
        for (i, (kind, label)) in labels.iter().enumerate() {
            assert!(
                lines[i].contains(label),
                "{kind:?} row does not show its own account: {:?}",
                lines[i]
            );
            // 他の agent の名前が紛れ込んでいない
            for (other, other_label) in &labels {
                if other != kind {
                    assert!(
                        !lines[i].contains(other_label),
                        "{kind:?} row shows {other:?}'s account: {:?}",
                        lines[i]
                    );
                }
            }
        }
    }

    /// **使用率はマウスが乗っている間だけ帯が乗る**（押せることを示す。
    /// 一覧の行のホバーと同じ手段）。帯は背景だけで**幅を変えない** ＝
    /// 乗った瞬間に当たり判定（[`usage_hits`]）が動かない
    #[test]
    fn the_usage_gauge_is_banded_only_while_hovered() {
        let mut app = App {
            usage: Kind::ORDER
                .into_iter()
                .map(|k| (k, crate::usage::sample_ready(Vec::new())))
                .collect(),
            term_size: (120, 30),
            ..Default::default()
        };
        let spans = |app: &App| -> Vec<Span<'static>> {
            usage_footer(app).into_iter().flatten().collect()
        };
        let plain = spans(&app);
        assert!(!plain.is_empty(), "the fixture's premise broke — nothing is drawn");
        assert!(
            plain.iter().all(|s| s.style.bg.is_none()),
            "banded before the mouse arrived"
        );

        // **乗った 1 行だけが光る**（行ごとに取り直せるので、押せる場所と
        // 光る場所を 1 対 1 にする）
        app.usage_hovered = Some(Kind::Claude);
        let banded: Vec<usize> = usage_footer(&app)
            .iter()
            .enumerate()
            .filter(|(_, line)| line.iter().any(|s| s.style.bg == Some(ui().hl_bg)))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            banded,
            vec![0],
            "the hover banded more than the row it is on"
        );
        // **帯は背景だけ ＝ 幅を変えない**（乗った瞬間に当たり判定が動かない）
        assert_eq!(
            span_width(&plain),
            span_width(&spans(&app)),
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

    /// **週次のリセットが当日でも日付を出す。** 「時刻だけ ＝ 当日」という規則は
    /// 画面のどこにも書かれていないので、週に 1 度しか来ない週次の時刻がその形で
    /// 出ると 5h の時刻と見分けが付かない。5h は数時間おきに来るので当日は時刻だけ
    /// （日付は幅を食うだけ）＝ **同じ時刻でも週次にだけ日付が付く**
    #[test]
    fn the_weekly_reset_keeps_its_date_even_when_it_falls_today() {
        use chrono::{Local, TimeZone};
        // 今日の正午（日付が変わる境目で走っても当日に収まる時刻を選ぶ）
        let today_noon = Local::now()
            .date_naive()
            .and_hms_opt(12, 0, 0)
            .and_then(|t| Local.from_local_datetime(&t).single())
            .expect("today has a noon")
            .timestamp() as u64;
        let usage = Usage::Ready(UsageInfo {
            five: Some(UsageWindow {
                pct: 18.0,
                resets_at: Some(today_noon),
            }),
            seven: Some(UsageWindow {
                pct: 55.0,
                resets_at: Some(today_noon),
            }),
            models: Vec::new(),
            fetched_at: ccdesk::now_secs(),
        });
        let text = usage_text(&usage, 200);
        assert_eq!(text.matches('\u{2192}').count(), 2, "{text}");
        // 日付は 1 つだけ ＝ 5h には付かず、週次にだけ付く
        assert_eq!(text.matches('/').count(), 1, "the date is on the wrong window: {text}");
        let date = text.find('/').expect("the weekly date is missing");
        let seven = text.find("7d").expect("the weekly window is missing");
        assert!(date > seven, "the date landed on the 5h window: {text}");
        assert!(text.contains("12:00"), "{text}");
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

    /// 既定のサイドバー幅の内側（枠の左右 1 桁ずつを引いたもの）。
    /// 版行の幅の予算はこの桁数。**幅そのものは [`DEFAULT_SIDEBAR`] から導く**
    /// ので、既定幅を動かしたときにここが黙って古くならない
    const DEFAULT_INNER: u16 = DEFAULT_SIDEBAR - 2;

    /// **行の桁の前提はすべて「記号 1 桁」に乗っている。**
    ///
    /// 行頭の 2 つの印は消えている側を同じ幅の空白で確保しており、行末の
    /// メニュー記号は内側の右端に置く（当たり判定がその桁を指す）。どれかが
    /// 2 桁になると行全体が横へずれるので、幅はここで実測して固定する。
    /// East Asian Ambiguous な記号は端末とフォント設定で 1 桁にも 2 桁にもなるため
    /// 選ばない。Ambiguous かどうかは `width_cjk`（Ambiguous を 2 と数える側）で
    /// 測れるので、ASCII かどうかではなくその値そのものを固定する。
    ///
    /// **[`HEAD_COLS`] / [`MENU_COLS`] / [`MIN_SIDEBAR`] の足し算もここで検算する**
    /// ので、行頭や行末に何かを足したらこのテストが落ちる
    #[test]
    fn the_row_head_marks_are_one_column_wide() {
        use unicode_width::UnicodeWidthStr;
        assert_eq!(UPDATE_MARK.width(), 1, "the update mark is not 1 column wide");
        assert_eq!(MENU_MARK.width(), 1, "the menu mark is not 1 column wide");
        // Ambiguous なら `width_cjk` が 2 を返す ＝ CJK ロケールで行がずれる記号
        assert_eq!(
            MENU_MARK.width_cjk(),
            1,
            "the menu mark is East Asian Ambiguous: {MENU_MARK:?}"
        );
        // ペインに出ているかの印は、消えている側も同じ 1 桁の空白
        assert_eq!(OPEN_MARK.width(), 1, "{OPEN_MARK:?} is not 1 column wide");
        assert_eq!(SCREEN_MARK.width(), 1, "{SCREEN_MARK:?} is not 1 column wide");
        assert_eq!(
            SCREEN_MARK.width_cjk(),
            1,
            "the screen mark is East Asian Ambiguous: {SCREEN_MARK:?}"
        );
        assert_eq!(CLOSED_MARK.width(), 1, "{CLOSED_MARK:?} is not 1 column wide");
        assert!(
            CLOSED_MARK.trim().is_empty(),
            "a character is showing in the empty slot: {CLOSED_MARK:?}"
        );
        // ドットは 2 グリフとも 1 桁（既読は空白ではなく ○）
        assert_eq!(DOT_FILLED.width(), 1, "{DOT_FILLED:?} is not 1 column wide");
        assert_eq!(DOT_HOLLOW.width(), 1, "{DOT_HOLLOW:?} is not 1 column wide");
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
        let mut d = look_fixture();
        d.label = "n".repeat(30);
        let name = row_name_spans(&d, MIN_SIDEBAR - 2, Style::default())[0].content.clone();
        assert_eq!(name.width(), MIN_NAME_COLS, "the narrowest sidebar lost the name column");
    }

    /// **メニュー記号は内側の右端で、当たり判定はそこと左隣の空白。**
    /// 描画とヒットテストが別々の桁を持つと「見えているのに押せない」が起きる
    #[test]
    fn the_menu_mark_sits_at_the_right_edge_where_the_click_lands() {
        use unicode_width::UnicodeWidthStr;
        let mut app = App {
            term_size: (60, 40),
            sidebar_width: 40,
            sessions: vec![named_session("a", "C:\\dev\\api", "some-session")],
            titles: fixed_titles(),
            ..Default::default()
        };
        let lines = session_lines(&mut app);
        let line = lines
            .iter()
            .find(|line| line.contains("some-session"))
            .unwrap_or_else(|| panic!("no session row: {lines:?}"))
            .clone();
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
                ("a", IDLE, 2_000),
                ("b", IDLE, 2_000),
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
        let (unread, read) = (at("fresh"), at("seen"));
        // 印はそれぞれ決まった桁に出る（ペインに出ていないので 1 桁目は空白）。
        // **ドットが答えるのは未読だけ**なので、割れるのは塗りの有無
        assert!(unread.starts_with(&format!("{CLOSED_MARK}{DOT_FILLED}")), "{unread:?}");
        assert!(read.starts_with(&format!("{CLOSED_MARK}{DOT_HOLLOW}")), "{read:?}");
        // 名前欄の開始桁は 2 本とも同じ（消えている印の桁も確保されている）
        let name_col = |line: &str, needle: &str| line[..line.find(needle).unwrap()].width();
        assert_eq!(name_col(&unread, "fresh"), HEAD_COLS);
        assert_eq!(name_col(&read, "seen"), HEAD_COLS);
    }

    /// **ライブ状態を持たない agent は、PTY の無音で Working を降ろす。**
    ///
    /// codex は Esc 中断で `Stop` を撃たない（openai/codex#22858）うえ、
    /// **PTY は Waiting を降ろす材料にならない。**
    ///
    /// 無音で降ろせば許可待ちの行が黙って消える（ユーザーが動かないと進まないのに
    /// 呼ばれなくなる）。出力で降ろすこともできない: codex の TUI は承認ダイアログを
    /// 出している間も 1 秒ごとにタイトルを書き換える（実測）ので、Writing は
    /// 「動いている」と「待たれている」の両方に付く
    #[test]
    fn no_amount_of_pty_output_clears_waiting() {
        let view = |pty| {
            row_state(Some(Run {
                hook: Some(Reported { state: WAITING, at: 1_000 }),
                observed: None,
                pty: Some(pty),
            }))
        };
        for pty in [PtyHint::Quiet, PtyHint::Writing, PtyHint::Starting] {
            assert_eq!(view(pty), WAITING, "{pty:?} cleared a permission prompt");
        }
    }

    /// **行の状態は hook（イベント）と `status`（現在値）の新しい方で決まる。**
    ///
    /// 2 つは同じ語彙へ揃えてある（[`crate::poll::state_of_status`]）ので、
    /// 突き合わせに特例は要らない。ここが崩れると、食い違うたびに裁定則を
    /// 1 本足す形へ逆戻りする
    #[test]
    fn the_newer_of_the_hook_and_the_live_status_decides_the_row() {
        let view = |hook, status, status_at| {
            row_state(Some(Run { hook, observed: observed(status, status_at), pty: None }))
        };
        // 観測の方が新しい → 観測が勝つ
        assert_eq!(view(Some(Reported { state: WORKING, at: 1_000 }), "idle", 2_000), IDLE);
        assert_eq!(view(Some(Reported { state: IDLE, at: 1_000 }), "busy", 2_000), WORKING);
        // **赤の固着そのもの**: 陳腐化した busy より新しい idle hook が勝つ
        assert_eq!(view(Some(Reported { state: IDLE, at: 2_000 }), "busy", 1_000), IDLE);
        assert_eq!(view(Some(Reported { state: State::Idle, at: 1_000 }), "waiting", 2_000), WAITING);
        // hook の方が新しい（または同時刻）→ hook が勝つ
        assert_eq!(view(Some(Reported { state: WORKING, at: 1_000 }), "idle", 999), WORKING);
        assert_eq!(view(Some(Reported { state: WORKING, at: 1_000 }), "idle", 1_000), WORKING);
        // 片方しか無いときはある方
        assert_eq!(view(Some(Reported { state: WAITING, at: 1_000 }), "", 2_000), WAITING);
        assert_eq!(view(None, "busy", 2_000), WORKING);
    }

    /// **記録から読んだ現在値も、hook と同じ土俵で新旧を比べる**（codex）。
    ///
    /// **これが Esc 中断の固着を直す**: codex は中断のとき `Stop` を撃たないので
    /// hook は `working` のまま残るが、rollout には `turn_aborted` が書かれる。
    /// 「記録の方が新しければ記録が勝つ」という同じ 1 本で降りる ＝ codex 専用の
    /// 特例は要らない
    #[test]
    fn a_state_read_from_the_record_beats_an_older_hook() {
        let view = |hook, recorded| row_state(Some(Run {
            hook: Some(hook),
            observed: Some(recorded),
            // codex の TUI は考え込んでいる間も書き続けるので、PTY は材料にならない
            pty: Some(PtyHint::Writing),
        }));
        // Esc 中断: `turn_aborted` が hook より後 ＝ 手が空いた
        assert_eq!(view(Reported { state: WORKING, at: 1_000 }, (IDLE, 2_000)), IDLE);
        // 許可に答えた後: 記録が動き出した ＝ もう待たれていない
        assert_eq!(view(Reported { state: WAITING, at: 1_000 }, (WORKING, 2_000)), WORKING);
        // **記録が古ければ hook が勝つ**（許可ダイアログは記録より後に出る ＝
        // 直前の道具呼び出しで黄が消えてはいけない）
        assert_eq!(view(Reported { state: WAITING, at: 2_000 }, (WORKING, 1_000)), WAITING);
        assert_eq!(view(Reported { state: WAITING, at: 2_000 }, (WORKING, 2_000)), WAITING);
    }

    /// **行に渡す時刻は claude が status を書いた時刻**で、ccdesk がそれを読んだ
    /// 時刻ではない。読んだ時刻を渡していた頃は、その値が常に「今」なので status が
    /// hook に必ず勝ち、陳腐化した `busy` を新しい `idle` hook で降ろせなかった
    /// （＝ 行が赤のまま固着した）
    #[test]
    fn a_rows_live_status_carries_the_time_claude_wrote_it() {
        let agents = vec![
            crate::poll::AgentInfo {
                session_id: "mine".to_string(),
                kind: crate::claude_format::AGENT_KIND_INTERACTIVE.to_string(),
                status: "busy".to_string(),
                status_at: 1_000,
            },
            // 前景でないエントリは行の答えにしない
            crate::poll::AgentInfo {
                session_id: "bg".to_string(),
                kind: "background".to_string(),
                status: "busy".to_string(),
                status_at: 9_999,
            },
        ];
        assert_eq!(live_status(&agents, Some("mine")), ("busy", 1_000));
        assert_eq!(
            live_status(&agents, Some("bg")),
            ("", 0),
            "a background entry answered for a row"
        );
        assert_eq!(live_status(&agents, Some("absent")), ("", 0));
        // **会話を知らない行は未観測。** ライブ状態が名乗るのは会話 ID なので、
        // 会話が分からなければ突き合わせる鍵が無い
        assert_eq!(live_status(&agents, None), ("", 0));
    }

    /// **観測は、閉じるイベントが来なかった hook を必ず治す。**
    ///
    /// hook はイベントなので取りこぼすと自己修復しない。実際に閉じない組が 2 つある:
    /// Esc 中断では `Stop` が飛ばず（実データで中断ターンの 91%（113/124）が未発火）、
    /// 許可プロンプトの**許可には解除を知らせるイベントがそもそも存在しない**。
    /// どちらも次の観測で解ける ＝ 遅れは最大ポーリング 1 周期に収まる
    #[test]
    fn a_status_observation_heals_a_hook_whose_closing_event_never_came() {
        let view = |hook, status| {
            row_state(Some(Run { hook, observed: observed(status, 2_000), pty: None }))
        };
        // Esc 中断: working のまま固着していた行が、次の観測で赤を降りる
        assert_eq!(view(Some(Reported { state: WORKING, at: 1_000 }), "idle"), IDLE);
        // 許可した直後: waiting のまま固着していた行が、次の観測で動き出す
        assert_eq!(view(Some(Reported { state: WAITING, at: 1_000 }), "busy"), WORKING);
        // ダイアログが開いたままなら黄のまま（claude 自身が waiting と言う）
        assert_eq!(view(Some(Reported { state: WAITING, at: 1_000 }), "waiting"), WAITING);
    }

    /// **バックグラウンド作業は「入力待ち」ではない。**
    ///
    /// `shell` ＝ 「アイドルだが未終了のバックグラウンド bash がある」で、
    /// ユーザーへの要求は何も無い。`busy` 以外を一律で入力待ちへ倒していた頃は、
    /// バックグラウンド実行中の行が黄「Needs input」を名乗って呼びつけていた。
    /// **今は idle と同じ扱い**（独立した状態にするかは別の判断）
    #[test]
    fn background_shell_work_is_not_a_request_for_input() {
        let view = |hook, status| {
            row_state(Some(Run { hook, observed: observed(status, 2_000), pty: None }))
        };
        for status in ["idle", "shell"] {
            assert_eq!(view(None, status), IDLE, "{status:?} asks for input");
            assert_eq!(view(Some(Reported { state: WORKING, at: 1_000 }), status), IDLE, "{status:?}");
        }
        // 知らない値だけは「動いているらしい」へ倒す（呼びつけるよりは害が小さい）
        assert_eq!(view(None, "something-new"), WORKING);
    }

    /// **材料が 1 つも無い行の最後の手段は PTY の出力変化だけ。**
    /// 出ていなければ「手が要らない」へ倒す（何も知らないことを
    /// 「入力待ち」と名乗ってユーザーを呼びつけない）
    #[test]
    fn a_row_with_no_hook_and_no_status_falls_back_to_the_pty_output() {
        let view = |pty| row_state(Some(Run { hook: None, observed: None, pty: Some(pty) }));
        // まだ端末を掴んでいない ＝ 起動中。**「もう終わった」ではない**
        assert_eq!(view(PtyHint::Starting), WORKING);
        assert_eq!(view(PtyHint::Writing), WORKING);
        assert_eq!(view(PtyHint::Quiet), IDLE);
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
        assert_eq!(row_state(None), STOPPED);
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
                ("a632c052", WAITING, 1_785_118_423_198),
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
    /// stop 直後はライブ状態の観測が最大 2 秒古く、kill したばかりの
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
            hook: Some(Reported { state: STOPPED, at: 0 }),
            observed: observed("idle", 0),
            pty: None,
        }));
        assert_eq!(view, STOPPED, "a fresh stopped was thrown away");
        // かつては「Stopped なのにアイコンが生存形（✻）」を `alive` フィールドで
        // 検査していたが、状態はもう色だけで語るのでその矛盾自体が作れなくなった

        // 他の state はそのまま生きている実行として出る（畳まれるのは stopped だけ）
        assert_eq!(
            row_state(Some(Run { hook: Some(Reported { state: WORKING, at: 0 }), observed: None, pty: None })),
            State::Working
        );
    }

    /// 更新の有無で行構成が変わらない（固定ヘッダー行数もマーカー桁の位置も動かない）。
    /// 版行（ccdesk + agent の数）+ 区切り線 1 本。**行数は更新の有無で変わらない**
    #[test]
    fn version_rows_keep_a_fixed_shape_whether_or_not_updates_exist() {
        let agents = |state: UpdateState, version: &str| -> Vec<(Kind, String, UpdateState)> {
            Kind::ORDER
                .into_iter()
                .map(|kind| (kind, version.to_string(), state))
                .collect()
        };
        let total = 1 + Kind::ORDER.len() + 1;
        for (ccdesk, claude) in [
            (UpdateState::Current, UpdateState::Current),
            (UpdateState::Available, UpdateState::Current),
            (UpdateState::Running, UpdateState::Available),
            (UpdateState::RestartPending, UpdateState::Running),
        ] {
            let rows = version_rows(ccdesk, &agents(claude, "2.1.220"), DEFAULT_INNER);
            assert_eq!(rows.len(), total, "expected one row per agent + 1 separator");
            // 版番号を出すかは状態が決める（差し替え済みだけ出さない ＝
            // [`UpdateState::shows_version`]）。**どちらでも行は 1 本**
            assert_eq!(
                rows[0].0.contains(env!("CARGO_PKG_VERSION")),
                ccdesk.shows_version(),
                "{:?}",
                rows[0].0
            );
            assert!(rows[0].0.contains("ccdesk"), "{:?}", rows[0].0);
            // agent 行は差し替え済みにならない（claude は次の --version で最新へ戻る）
            assert!(rows[1].0.contains("claude v2.1.220"), "{:?}", rows[1].0);
            assert!(rows[2].0.contains("codex v2.1.220"), "{:?}", rows[2].0);
            assert_eq!(rows[total - 1].0, separator_text(DEFAULT_INNER));
            // **区切り線だけが飾り。** 版行は更新の有無に関係なく行の実体がある
            assert_eq!(
                rows[total - 1].2,
                SidebarRow::Decoration,
                "the separator must not be a row you can touch"
            );
            assert!(
                rows[..total - 1].iter().all(|row| row.2.selectable()),
                "a version row dropped out of the selection at {ccdesk:?} / {claude:?}"
            );
        }
        // 版が未取得なら番号を出さない（誤情報を出さない）
        let rows = version_rows(
            UpdateState::Current,
            &agents(UpdateState::Current, ""),
            DEFAULT_INNER,
        );
        assert!(rows[1].0.contains("claude"), "{:?}", rows[1].0);
        assert!(!rows[1].0.contains(" v"), "showing a v with no version: {:?}", rows[1].0);
    }

    /// 各状態の文面。左端がマーカー桁、右端が動詞で、**新版の番号は出さない**
    #[test]
    fn version_row_spells_out_every_update_state() {
        let row = |state| version_row(" ", "ccdesk", "0.5.0", state, DEFAULT_INNER);
        // 最新: マーカー桁も記号桁も空白、動詞なし
        let current = row(UpdateState::Current);
        assert_eq!(current, "    ccdesk v0.5.0");
        // 残りは ⟳ + 右端の動詞。**動詞は状態ごとに違う**（同じ綴りが 2 つあると、
        // 行を見ても今どちらなのか分からない）
        let mut verbs = Vec::new();
        for state in UpdateState::ALL.into_iter().filter(|s| *s != UpdateState::Current) {
            let text = row(state);
            let verb = state.verb();
            assert!(!verb.is_empty(), "{state:?} has nothing to say at the right edge");
            assert!(text.starts_with(UPDATE_MARK), "{text:?}");
            assert!(text.ends_with(verb), "{text:?} does not end with {verb:?}");
            // 名前は必ず出る。番号は状態が決める（差し替え済みは右端の語へ桁を譲る）
            assert!(text.contains("ccdesk"), "{text:?}");
            assert_eq!(
                text.contains("v0.5.0"),
                state.shows_version(),
                "{state:?} disagrees with shows_version: {text:?}"
            );
            verbs.push(verb);
        }
        let unique: std::collections::BTreeSet<_> = verbs.iter().collect();
        assert_eq!(unique.len(), verbs.len(), "two states share a verb: {verbs:?}");
    }

    /// **据え置きは押せる行のまま。** agent 側の詰まりが解けたあと、ccdesk を
    /// 起動し直さずに押し直せる必要がある（更新が効かなかっただけで、
    /// やることが無くなったわけではない）
    #[test]
    fn a_stalled_row_still_offers_the_update_it_could_not_apply() {
        assert_eq!(
            UpdateState::Stalled.action(RowAction::UpdateAgent(Kind::Claude)),
            SidebarRow::Action(RowAction::UpdateAgent(Kind::Claude))
        );
        // 状態が行から読める（"update" のままだと効かなかったことが伝わらない）
        assert_ne!(UpdateState::Stalled.verb(), UpdateState::Available.verb());
    }

    /// **差し替え済みの行は claude 本体と同じ綴りで、版番号を右端の語に譲る。**
    /// 綴りを揃えるのは同じ出来事（新しい版は入った / 反映は再起動から）に 2 つの
    /// 言い方を作らないため。番号を落とすのは幅の都合で、両方は既定幅に入らない
    #[test]
    fn the_restart_pending_row_reads_like_the_agent_it_mirrors() {
        use unicode_width::UnicodeWidthStr;
        let text = version_row(" ", "ccdesk", "0.23.2", UpdateState::RestartPending, DEFAULT_INNER);
        assert!(text.ends_with("restart to update"), "{text:?}");
        assert!(text.contains("ccdesk"), "{text:?}");
        assert!(!text.contains("0.23.2"), "the version was not yielded: {text:?}");
        assert!(text.width() <= DEFAULT_INNER as usize, "{text:?} ({} columns)", text.width());
        // 版を出す状態では番号が残る（落としたのは差し替え済みだけ）
        let available = version_row(" ", "ccdesk", "0.23.2", UpdateState::Available, DEFAULT_INNER);
        assert!(available.contains("ccdesk v0.23.2"), "{available:?}");
    }

    /// **最新のときもマーカー桁を確保する。** 更新が出た瞬間に名前が横へずれると、
    /// 行が変わったこと自体に気づきにくい。
    ///
    /// **記号の桁も同じ扱い**: agent 行が `●`/`◆` を持ち ccdesk 行が空白でも、
    /// 名前は同じ桁から始まる（凡例を足したせいで版行が階段状にならない）
    #[test]
    fn version_row_keeps_the_name_column_fixed_across_states() {
        use unicode_width::UnicodeWidthStr;
        // 名前の前にある部分の表示幅（マーカー桁 + 記号桁 + 区切りの空白 2 つ）
        let name_col = |text: &str, name: &str| {
            let at = text.find(name).expect("name is missing");
            text[..at].width()
        };
        let base = name_col(
            &version_row(" ", "ccdesk", "0.5.0", UpdateState::Current, DEFAULT_INNER),
            "ccdesk",
        );
        assert_eq!(base, 4, "expected marker + glyph columns with a space each");
        for state in UpdateState::ALL {
            let text = version_row(" ", "ccdesk", "0.5.0", state, DEFAULT_INNER);
            assert_eq!(
                name_col(&text, "ccdesk"),
                base,
                "{state:?} shifted the name column: {text:?}"
            );
            // 記号を持つ agent 行でも名前の開始桁は同じ
            for kind in Kind::ORDER {
                let text = version_row(
                    agent_glyph(kind),
                    kind.title(),
                    "1.0.0",
                    state,
                    DEFAULT_INNER,
                );
                assert_eq!(
                    name_col(&text, kind.title()),
                    base,
                    "{kind:?} shifted the name column: {text:?}"
                );
            }
        }
    }

    /// 押せるのは「更新がある」行だけ（= update）。実行中・最新・差し替え済みは
    /// 押しても意味が無いので動作を付けない。**それでも行は行**なので
    /// [`SidebarRow::Inert`] ＝ 選択・ホバーの対象からは外れない。
    ///
    /// **差し替え済み（`RestartPending`）も押せない側。** 自動再起動をやめたので、
    /// ccdesk の行もクリックでは何も起きない案内に留まる
    #[test]
    fn version_rows_are_clickable_only_when_there_is_something_to_do() {
        let rows_of = |ccdesk, claude| {
            let agents: Vec<(Kind, String, UpdateState)> = Kind::ORDER
                .into_iter()
                .map(|kind| (kind, "2.1.220".to_string(), claude))
                .collect();
            let rows = version_rows(ccdesk, &agents, DEFAULT_INNER);
            (rows[0].2.clone(), rows[1].2.clone())
        };
        assert_eq!(
            rows_of(UpdateState::Available, UpdateState::Available),
            (
                SidebarRow::Action(RowAction::UpdateCcdesk),
                SidebarRow::Action(RowAction::UpdateAgent(Kind::Claude))
            )
        );
        // 押せるのは「やることが残っている」状態だけ。差し替え済み（claude は
        // この状態にならない）も実行中も最新も、押しても意味が無いので Inert
        for state in UpdateState::ALL
            .into_iter()
            .filter(|s| !matches!(s, UpdateState::Available | UpdateState::Stalled))
        {
            assert_eq!(
                rows_of(state, state),
                (SidebarRow::Inert, SidebarRow::Inert),
                "{state:?}"
            );
        }
    }

    /// **押せない行の語を裸の動詞にしない。** 差し替え済みの行が右端に "restart" と
    /// 出していた頃、押しても何も起きない行（[`UpdateState::action`]）なのに
    /// 「押せばここから再起動できる」と読めるという指摘が利用者から来た。
    /// 押せない行は「今どうなっているか」を述べる形 ＝ 語をつないだ句（`restart to update`）か
    /// 進行の省略記号付き（`updating…`）にする。**押せるかどうかは実装から引く**ので、
    /// 状態を足しても手で一覧を足す必要はない
    #[test]
    fn an_unclickable_version_row_never_words_itself_as_a_command() {
        for state in UpdateState::ALL {
            let verb = state.verb();
            if verb.is_empty() || state.action(RowAction::UpdateCcdesk) != SidebarRow::Inert {
                continue;
            }
            assert!(
                verb.contains(' ') || verb.ends_with('…'),
                "{state:?} labels an unclickable row with a bare command: {verb:?}"
            );
        }
    }

    /// 既定のサイドバー幅（34 桁 = 内側 32 桁）で切られない。
    /// 版番号は現実的な桁数まで（claude は 3 パート、ccdesk は本ビルドの版）。
    /// **凡例の記号を足したぶんも含めて収まる**ことをここで見る
    #[test]
    fn version_rows_fit_the_default_sidebar_width() {
        use unicode_width::UnicodeWidthStr;
        // ccdesk 行（記号なし）と agent 行（記号あり）の両方
        let names: Vec<(&str, &str)> = std::iter::once((" ", "ccdesk"))
            .chain(Kind::ORDER.into_iter().map(|k| (agent_glyph(k), k.title())))
            .collect();
        for state in UpdateState::ALL {
            for version in ["", "0.5.0", "2.1.220", "10.20.300"] {
                for (glyph, name) in &names {
                    let text = version_row(glyph, name, version, state, DEFAULT_INNER);
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



    /// クリック判定はヘッダー先頭の版行に当たる。行 index は列を取らない
    /// = **行のどこを押しても同じ行**（マーカーの桁だけが当たり判定ではない）。
    /// 上枠とフッター帯は不感帯
    #[test]
    fn row_at_hits_the_version_rows_at_the_top_of_the_header() {
        let sl = sidebar_layout_of(29, 34);
        let agents: Vec<(Kind, String, UpdateState)> = Kind::ORDER
            .into_iter()
            .map(|kind| (kind, "2.1.220".to_string(), UpdateState::Available))
            .collect();
        let header = version_rows(UpdateState::Available, &agents, DEFAULT_INNER);
        // 版行はヘッダーの先頭から順に並ぶ（区切り線は末尾）
        assert_eq!(header[0].2.action(), Some(&RowAction::UpdateCcdesk));
        assert_eq!(
            header[1].2.action(),
            Some(&RowAction::UpdateAgent(Kind::Claude))
        );
        // 画面 y=1 が ccdesk 行、y=2 が claude 行（スクロール位置に関係なく固定）
        for scroll in [0usize, 5, 99] {
            assert_eq!(row_at(1, sl.capacity, 7, scroll), 0);
            assert_eq!(row_at(2, sl.capacity, 7, scroll), 1);
        }
        // 上枠とフッター帯・下枠は不感帯
        assert_eq!(row_at(0, sl.capacity, 7, 0), usize::MAX);
        for y in [sl.capacity as u16 + 1, sl.capacity as u16 + 2] {
            assert_eq!(row_at(y, sl.capacity, 7, 0), usize::MAX, "y={y}");
        }
    }

    /// **1 行 = 1 画面行**（[`row_at`] と [`row_y`] が互いの逆）。
    /// 行の種類で高さが変わらないので、押した場所とメニューの出る場所がずれない
    #[test]
    fn the_hit_test_and_the_screen_row_are_inverses() {
        let at = |y| row_at(y, 10, 1, 0);
        assert_eq!(at(1), 0, "the header row moved");
        assert_eq!(at(2), 1);
        assert_eq!(at(3), 2);
        for row in 0..5usize {
            assert_eq!(at(row_y(row, 1, 0)), row, "row {row} is not its own inverse");
        }
        // ヘッダーの下だけスクロールぶんずれる（ヘッダー内は動かない）
        assert_eq!(row_at(2, 10, 1, 4), 5);
        assert_eq!(row_y(5, 1, 4), 2);
        assert_eq!(row_at(1, 10, 1, 4), 0, "the header scrolled with the list");
        assert_eq!(row_y(0, 1, 4), 1);
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

    /// サイドバーを実際に描いて (行データ, **その行の 1 段目**の表示文字列) を返す。
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

    /// その行が Stopped として描かれたか（**行末の状態語**で見る）。
    ///
    /// **色ではなく語で見ること**: 明滅の谷は [`ui`]`().dim` ＝ Stopped の色そのもの
    /// なので、色で判定すると Working の行が描いた瞬間の位相しだいで Stopped と
    /// 同じ答えを返す。実際にこれで 2 回に 1 回落ちるテストが生まれた。語は位相に依らない
    fn drawn_as_stopped(app: &mut App, needle: &str) -> bool {
        drawn_session_row(app, needle).0.contains(State::Stopped.title())
    }

    /// `needle` を名前に含むセッション行を 1 フレーム描いて
    /// (行の表示文字列, ドットの前景色) を返す。
    ///
    /// **2 つとも同じ 1 回の描画から取る**のが要点（2 回描くと、行を探す描画と
    /// 読む描画の間で一覧が組み直され、別の行の桁を読み得る）
    fn drawn_session_row(app: &mut App, needle: &str) -> (String, Color) {
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
        let text = |y: u16| -> String {
            (1..cols.saturating_sub(1))
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        };
        let idx = app
            .sidebar_rows
            .iter()
            .enumerate()
            .filter(|(_, row)| matches!(row.action(), Some(RowAction::Open(_))))
            .map(|(i, _)| i)
            .find(|i| text(*i as u16 + 1).contains(needle))
            .unwrap_or_else(|| panic!("{needle} is not on any row"));
        // 行は上枠の次から積まれ、ドットは行頭から 2 桁目（1 桁目はペイン印）
        let y = idx as u16 + 1;
        (text(y), buffer[(2, y)].fg)
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

    /// 端末を 1 フレーム描いて**全行**の文字列を返す。サイドバー・右ペイン・
    /// 下部バーを区別しないので、「どこかに出ていないこと」を見るのに使える
    fn drawn_screen(app: &mut App) -> Vec<String> {
        let (w, h) = app.term_size;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).expect("test terminal");
        terminal
            .draw(|frame| {
                draw(frame, app);
            })
            .expect("draw failed");
        let buffer = terminal.backend().buffer();
        (0..h)
            .map(|y| (0..w).map(|x| buffer[(x, y)].symbol()).collect())
            .collect()
    }

    /// **切った agent は画面のどこにも出ない。**
    ///
    /// 版行・行・grouping=agent の節・下部バーの行数が全部 [`App::kinds`] から
    /// 導かれることの検査。1 箇所でも `Kind::ORDER` を直に読んでいると、
    /// そこだけ設定に従わない ＝ 出さないはずの agent が残る
    #[test]
    fn a_disabled_agent_is_nowhere_on_the_screen() {
        let session = |id: &str, kind: Kind| crate::sessions::SessionRow {
            kind,
            ..named_session(id, "C:\\dev\\api", id)
        };
        let mut app = App {
            term_size: (120, 40),
            sidebar_width: 40,
            // **codex を出さない**（本番の既定。[`crate::backend::Kind::enabled`]）
            kinds: vec![Kind::Claude],
            sessions: vec![session("kept", Kind::Claude), session("hidden", Kind::Codex)],
            titles: fixed_titles(),
            grouping: Grouping::Agent,
            ..Default::default()
        };
        // **画面全体を見る**（サイドバーだけだと、下部バーの agent 行に綴りが
        // 残っているのを見逃す。実際この検査を広げるまで残っていた）
        let screen = drawn_screen(&mut app);
        assert!(screen.iter().any(|t| t.contains("kept")), "the claude row is gone: {screen:?}");
        for line in &screen {
            assert!(
                !line.contains(Kind::Codex.title()),
                "codex is still on the screen: {line:?}"
            );
        }
        assert!(
            !screen.iter().any(|t| t.contains("hidden")),
            "the codex session row is still listed: {screen:?}"
        );
        // 下部バーは agent 1 行ぶん ＝ 空行がペインを削らない
        assert_eq!(bottom_bar_rows(&app), 1);
        // **行は消えていない**（`sessions.json` は触らない ＝ on に戻せば戻る）
        assert_eq!(app.sessions.len(), 2, "the hidden row was dropped from the list");
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
    /// 撮影用と同じ固定表（[`crate::title::Titles::fixed`]）で与える）。
    ///
    /// **会話 ID は行 ID と同じ値**にしてある: ライブ状態との突き合わせを見る
    /// テストが、行 ID と会話 ID の綴り分けではなく突き合わせそのものを見るため
    /// （分ける必要のあるテストは行を自分で組む）
    fn named_session(id: &str, cwd: &str, title: &str) -> crate::sessions::SessionRow {
        NAMES.with(|names| {
            names
                .borrow_mut()
                .insert(crate::sessions::SessionId::new(id), title.to_string())
        });
        crate::sessions::SessionRow {
            conversation: crate::sessions::Conversation::Observed(id.to_string()),
            ..crate::sessions::SessionRow::new(crate::sessions::SessionId::new(id), cwd, 0)
        }
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

    /// **行の並びは「名前 → 状態語 → メニュー記号」。**
    ///
    /// **agent は行末に居ない**（ドットの形が答えるので綴りで桁を食わない）。
    /// 略記（かつての `[cc]` / `[cx]`）も綴りも出ないことをここで固定する。
    /// 状態が Stopped なのは、窓（PTY）を起こしていないので保管の hook が何であれ
    /// この行が止まっているため（[`a_stopped_row_is_drawn_as_stopped_and_not_as_needs_input`]）
    #[test]
    fn the_row_ends_with_the_state_then_the_menu_mark() {
        let mut app = App {
            term_size: (120, 40),
            sidebar_width: 40,
            sessions: vec![named_session("s", "C:\\dev\\api", "no-text-row")],
            hook_states: crate::hooks::HookStates::from_entries([("s", IDLE, 0)]),
            titles: fixed_titles(),
            ..Default::default()
        };
        let (line, _) = drawn_session_row(&mut app, "no-text-row");
        let after_name = &line[line.find("no-text-row").unwrap() + "no-text-row".len()..];
        assert_eq!(
            after_name.split_whitespace().collect::<Vec<_>>(),
            [State::Stopped.title(), MENU_MARK],
            "the row tail is not state then menu mark: {after_name:?}"
        );
        // 略記も綴りも残っていない（agent が桁を食っていた形は 2 度とも消した）
        assert!(!line.contains("[cc]"), "the old agent tag is back: {line:?}");
        assert!(
            !line.contains(Kind::Claude.title()),
            "the agent spelling is back in the row: {line:?}"
        );
    }

    /// **行末ブロックは名前の下限を割ってまで出さない。**
    /// 落ちるのは状態語だけで、落ちてもドットの色が状態を語り続ける。
    ///
    /// **どの幅・どの kind・どの状態でも行はちょうど内側の幅を埋める**（メニュー記号が
    /// 常に右端に来る前提）。ここを 1 通りの組み合わせでしか見ないと、綴りが
    /// たまたま桁ぴったりの状態だけで通ってしまう
    #[test]
    fn the_row_tail_gives_way_before_the_name_does() {
        let d = look_fixture();
        let tail_of = |inner: u16| -> String {
            row_tail_spans(&d, inner, 0)
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        };
        // 予算に余裕があれば状態語が出る（agent はここに居ない）
        let wide = tail_of(DEFAULT_INNER);
        assert!(wide.contains(State::Waiting.title()), "no state: {wide:?}");
        assert!(
            !wide.contains(Kind::Claude.title()),
            "the agent spelling came back to the tail: {wide:?}"
        );

        // 状態語が残る一番狭い幅（名前の下限 + 状態語ぶん）
        let fixed = (HEAD_COLS + MENU_COLS + MIN_NAME_COLS) as u16;
        let state_only = tail_of(fixed + TAIL_STATE_COLS as u16);
        assert!(
            state_only.contains(State::Waiting.title()),
            "the state word went too early: {state_only:?}"
        );
        // 一番狭いサイドバーでは行末ブロックごと消える（名前が下限を割らない）
        assert_eq!(tail_of(fixed), "", "the tail squeezed the name below its floor");

        // どの幅でも名前の予算は下限を割らず、行はちょうど内側の幅を埋める
        use unicode_width::UnicodeWidthStr as _;
        for inner in MIN_ROW_COLS..80 {
            assert!(
                name_cols(inner) >= MIN_NAME_COLS,
                "inner {inner} left only {} columns for the name",
                name_cols(inner)
            );
            // **全 kind × 全状態**で見る（綴りの長さが違っても桁は動かない）
            for kind in Kind::ORDER {
                for group in State::ORDER {
                    let mut d = look_fixture();
                    d.kind = kind;
                    d.group = group;
                    d.label = "x".repeat(80);
                    let cols = |spans: Vec<Span<'_>>| -> usize {
                        spans.iter().map(|s| s.content.width()).sum()
                    };
                    // 行頭 + 名前ブロック（名前 + ID + 詰め物）+ 行末ブロック +
                    // メニュー = ちょうど内側の幅
                    let used = HEAD_COLS
                        + cols(row_name_spans(&d, inner, Style::default()))
                        + cols(row_tail_spans(&d, inner, 0))
                        + MENU_COLS;
                    assert_eq!(
                        used, inner as usize,
                        "inner {inner} / {} / {} does not fill the row exactly",
                        kind.title(),
                        group.title()
                    );
                }
            }
        }
    }

    /// **短い ID は名前の直後に並び、「サイドバーを広げたときだけ」出る。**
    ///
    /// 既定幅で出すと名前が 9 桁痩せる。ID が要るのは他のセッションを宛先として
    /// 指すときだけで、名前は常に読む ＝ 常に読む方を痩せさせない。
    ///
    /// 閾値は「ID を出しても名前が既定幅ぶん残るか」なので、**ID が見えている
    /// どの幅でも、名前は既定幅と同じかそれより広い**。ここが崩れると
    /// 「広げたのに名前が読めなくなる」が起きる
    #[test]
    fn the_short_id_sits_next_to_the_name_once_the_sidebar_is_widened() {
        use unicode_width::UnicodeWidthStr as _;
        let mut d = look_fixture();
        d.label = "some-session".to_string();
        let short = d.id.short();
        let name_block = |inner: u16| -> String {
            row_name_spans(&d, inner, Style::default())
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        };

        // 既定幅では出ない
        let default = name_block(DEFAULT_INNER);
        assert!(
            !default.contains(&short),
            "the id ate the name at the default width: {default:?}"
        );

        // 十分広げれば出る。**名前の直後**（行末ではない）で、
        // 出るのは `ccdesk list` と同じ 8 桁
        let wide = name_block(60);
        assert_eq!(short.width(), crate::sessions::SHORT_ID_COLS, "the id is not 8 columns");
        assert!(
            wide.starts_with(&format!("{} {short}", d.label)),
            "the id is not right after the name: {wide:?}"
        );

        // 名前の桁は、ID が出ているどの幅でも既定幅を下回らない
        let mut seen_id = false;
        for inner in MIN_ROW_COLS..120 {
            if !name_block(inner).contains(&short) {
                continue;
            }
            seen_id = true;
            assert!(
                name_cols(inner) >= NAME_COLS_AT_DEFAULT,
                "inner {inner} shows the id but left the name only {} columns",
                name_cols(inner)
            );
        }
        assert!(seen_id, "the id never appears at any width");
    }

    /// **ペイン枠は今どのセッションを見ているかを名乗る。**
    /// 出す ID は `ccdesk list` と同じ 8 桁なので、読んだままを宛先として打てる。
    ///
    /// 細くなったスロットでは ID から落ちる（サイドバーの行末と同じ規則）
    #[test]
    fn the_pane_frame_names_the_session_and_gives_up_the_id_when_thin() {
        use unicode_width::UnicodeWidthStr as _;
        let id = SessionId::new("0123456789abcdef-0123");
        let short = id.short();

        // 広いペイン: タイトルと ID が並ぶ
        let wide = pane_title("api probe", &id, 60);
        assert!(wide.starts_with("api probe"), "the title is not first: {wide:?}");
        assert!(wide.ends_with(&short), "the id is not at the end: {wide:?}");

        // 長いタイトルは詰められるが、ID は残る（見出しは枠に収まる）。
        // **2 桁の文字で見る**: タイトルは会話から生成されるので全角を含み得て、
        // 文字数で切ると枠を壊す（この検査に日本語を使えないのでハングルを置く。
        // 見たいのは「1 文字 2 桁」であって字種ではない）
        for width in MIN_PANE..100 {
            let drawn = pane_title(&"\u{ac00}".repeat(80), &id, width);
            // **右上の ✕ のぶんも空けたまま収まる**（見出しが印を押し出さない）
            assert!(
                drawn.width() <= usize::from(width) - PANE_TITLE_MARGIN - close_cols(width),
                "width {width} overflows the frame: {drawn:?}"
            );
            assert!(drawn.ends_with(&short), "width {width} dropped the id: {drawn:?}");
        }

        // 分割で細くなったスロットでは ID が落ち、タイトルだけが残る
        let thin = pane_title("api probe", &id, 14);
        assert!(!thin.contains(&short), "the id squeezed the name: {thin:?}");
        assert!(!thin.is_empty(), "the title vanished: {thin:?}");
    }

    /// **閉じる印も桁の前提に乗るので実測して固定する**（[`MENU_MARK`] と同じ理由）。
    /// Ambiguous な記号（`×` U+00D7 など）を選ぶと、端末とロケールで幅が変わり
    /// 見出しと印が重なる
    #[test]
    fn the_close_mark_is_one_column_wide_in_every_locale() {
        use unicode_width::UnicodeWidthStr as _;
        assert_eq!(CLOSE_MARK.width(), 1, "the close mark is not 1 column wide");
        assert_eq!(
            CLOSE_MARK.width_cjk(),
            1,
            "the close mark is East Asian Ambiguous: {CLOSE_MARK:?}"
        );
        assert_eq!(
            CLOSE_COLS as usize,
            1 + CLOSE_MARK.width(),
            "the close budget changed"
        );
    }

    /// **見えている ✕ と押せる場所は同じ桁。** 枠の 3 種類（セッション・空・New 画面）を
    /// 実際に描いて、印が [`close_zone`] の返す桁と行に載っていることを実物で見る
    /// （導出を 1 つにしてあるので、どれか 1 種だけずれることはここで落ちる）
    #[test]
    fn the_close_mark_is_drawn_where_it_can_be_clicked() {
        for slot in [
            Slot::Empty,
            Slot::Session(SessionId::new("0123456789abcdef-0123")),
            Slot::New(new_view::NewState::browse("C:\\dev")),
        ] {
            let mut app = App {
                term_size: (120, 30),
                ..Default::default()
            };
            app.slots = vec![slot];
            let rect = app.slot_rects()[0];
            let (cols, row) = close_zone(rect).expect("the frame has no close mark");
            assert_eq!(
                drawn_cell(&mut app, *cols.end(), row),
                CLOSE_MARK,
                "the close mark is not on the column its hit test claims"
            );
        }
    }

    /// **見えている ID と押せる場所は同じ桁**（✕ と同じ作法）。
    ///
    /// 実際に描いて、[`id_zone`] が返す桁に短い ID が並んでいることを実物で見る ＝
    /// 見出しの組み方（名前の切り方・区切りの桁）を変えた日にここで落ちる。
    /// あわせて [`id_hit`] が同じ桁でその行を答えることも固定する
    /// （矩形と見出しを 2 度組む経路なので、どちらかだけずれ得る）
    #[test]
    fn the_short_id_is_drawn_where_it_can_be_copied() {
        let id = SessionId::new("0123456789abcdef-0123");
        let mut app = App {
            term_size: (120, 30),
            sessions: vec![named_session("0123456789abcdef-0123", "C:\\dev\\api", "api probe")],
            titles: fixed_titles(),
            ..Default::default()
        };
        app.slots = vec![Slot::Session(id.clone())];
        let rect = app.slot_rects()[0];
        let name = app.titles.of(&app.sessions[0]);
        let (cols, row) = id_zone(rect, &name, &id).expect("the frame shows no id");

        assert_eq!(
            drawn_span(&mut app, cols.clone(), row),
            id.short(),
            "the id is not on the columns its hit test claims"
        );
        for column in [*cols.start(), *cols.end()] {
            assert_eq!(
                id_hit(&app, column, row).as_ref(),
                Some(&id),
                "column {column} of the id does not answer with the session"
            );
        }
        // 見出しの外（区切りの左・ID の右）は当たらない
        assert!(id_hit(&app, cols.start() - 1, row).is_none(), "the separator is clickable");
        assert!(id_hit(&app, cols.end() + 1, row).is_none(), "past the id is clickable");
        // 細いスロットでは ID を出さない ＝ 押せる場所も無い（見えないものは押せない）
        assert!(
            id_zone(Rect::new(rect.x, rect.y, 14, rect.height), &name, &id).is_none(),
            "a frame too thin for the id still offers a hit zone"
        );
    }

    /// **閉じる印は縦のリサイズ掴み代を食わない。** 印は角の 1 桁内側に居るので、
    /// 隣り合う枠線 2 列（縦の境界の掴み代）とは重ならない ＝ 左列のスロットの印が
    /// あっても幅は掴める。
    ///
    /// 横の境界だけは下段スロットの上辺と同じ行なので重なる。そちらは押しの
    /// 判定順（[`crate::app`] の `handle_mouse` が印を先に見る）で決着させてある
    #[test]
    fn the_close_mark_does_not_eat_the_vertical_resize_grip() {
        let area = Rect::new(20, 0, 100, 40);
        let split = crate::panes::Split::default();
        for layout in crate::panes::Layout::ORDER {
            for rect in layout.rects(area, split) {
                let (cols, row) = close_zone(rect).expect("the slot has no close mark");
                for column in cols {
                    let (on_v, _) = layout.grab_at(area, split, column, row);
                    assert!(
                        !on_v,
                        "{layout:?}: the close mark at {column} sits on the width grip"
                    );
                }
            }
        }
    }

    /// **止めた行は `Stopped`（行末の語も Stopped）。**
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
            hook_states: crate::hooks::HookStates::from_entries([("s", WAITING, 9_999)]),
            titles: fixed_titles(),
            ..Default::default()
        };
        // 状態は行末の語が語り、ドットの色がそれに揃う
        let (line, dot) = drawn_session_row(&mut app, "stopped-row");
        assert!(
            line.contains(State::Stopped.title()),
            "a dead row is not drawn as Stopped: {line:?}"
        );
        for word in [State::Waiting, State::Working, State::Idle] {
            assert!(
                !line.contains(word.title()),
                "a dead row also shows {}: {line:?}",
                word.title()
            );
        }
        // 既読なのでドットは抜き（塗りのままでは未読の入力待ちと見紛う）
        assert!(line.starts_with(&format!("{CLOSED_MARK}{DOT_HOLLOW}")), "{line:?}");
        assert_eq!(dot, ui().dim, "a dead row does not use the Stopped color");
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
    /// **状態は特別扱いの無い `Waiting`**（明滅しない ＝ 位相を変えても色が動かない）に
    /// してある。ここを `Working` にすると、位相を上書きしないテストが黙って
    /// 「あるコマだけの見た目」を検査することになる（明滅の見え方は各テストが
    /// 自分で `group` と位相を置く）
    fn look_fixture() -> RowData {
        RowData {
            kind: Kind::Claude,
            action: RowAction::Open(SessionId::new("a")),
            id: SessionId::new("0123456789abcdef"),
            group: State::Waiting,
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

    /// 組んだセッション行を**2 段まとめて**「文字ごとの (文字, スタイル)」へ均す。
    /// 帯は 2 段の両方に掛かるので、見た目の比較は 2 段そろえて行う
    fn row_cells(d: &RowData, look: Look) -> Vec<(String, Style)> {
        // 明滅の位相はコマ列の先頭（一番明るい側）で固定。位相を見たいテストは
        // 自分で [`session_row_line`] を呼ぶ（既定を谷にすると、位相に関心の無い
        // テストが黙って「谷の見た目」を検査することになる）
        cells(&session_row_line(d, look, DEFAULT_INNER, 0))
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
            row_cells(&d, Look { band, selected, open, focused: open })
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
        let look = Look { band: false, selected: false, open: true, focused: true };
        let drawn = row_cells(&d, look);
        assert_eq!(drawn[0].0, OPEN_MARK);
        assert_eq!(drawn[1].0, DOT_FILLED);
    }

    /// **画面に出ていても、フォーカス中のペインでなければ違う印。**
    /// `on_screen`（4 枚並べたら 4 行が open になる）と `focused`（そのうち
    /// キー入力が届く 1 枚）は別の軸なので、印も別でないと見分けが付かない
    #[test]
    fn an_open_row_not_in_the_focused_pane_gets_a_different_mark() {
        let mut d = look_fixture();
        d.is_active_window = true;
        let in_focus = row_cells(&d, Look { band: false, selected: false, open: true, focused: true });
        let elsewhere = row_cells(&d, Look { band: false, selected: false, open: true, focused: false });
        assert_eq!(in_focus[0].0, OPEN_MARK);
        assert_eq!(elsewhere[0].0, SCREEN_MARK);
        assert_ne!(in_focus[0].0, elsewhere[0].0, "focused and unfocused open rows look the same");
    }

    /// **未読行はドットが塗り、既読行は抜き。** 塗りは 2 値だけで、
    /// 形（agent）・色（状態）・明滅とは独立したチャンネル。
    /// **agent が変わっても塗りのチャンネルは欠けない**（丸も菱も塗り/中空の対を持つ）
    #[test]
    fn the_dot_fill_marks_unread_rows() {
        let look = Look { band: false, selected: false, open: false, focused: false };
        let mut d = look_fixture();
        for kind in Kind::ORDER {
            d.kind = kind;
            d.unread = false;
            let read = row_cells(&d, look)[1].0.clone();
            d.unread = true;
            let unread = row_cells(&d, look)[1].0.clone();
            assert_ne!(read, unread, "{kind:?} draws the same glyph read and unread");
            assert_eq!(unread, dot_glyph(kind, true), "{kind:?} unread glyph");
            assert_eq!(read, dot_glyph(kind, false), "{kind:?} read glyph");
            // 塗りは状態に反応しない（4 状態とも同じ 2 値）
            for group in State::ORDER {
                d.group = group;
                d.unread = true;
                assert_eq!(
                    row_cells(&d, look)[1].0,
                    dot_glyph(kind, true),
                    "{} changed the fill",
                    group.title()
                );
            }
            d.group = State::Waiting;
        }
    }

    /// **ドットの形が agent を答える。** 4 グリフ（agent 2 × 塗り 2）はすべて別物で、
    /// 状態が変わっても形は動かない ＝ 形と色が互いのチャンネルを侵さない。
    ///
    /// 対応表は [`dot_glyph`] から引くので、記号を手で書き写さない
    #[test]
    fn the_dot_shape_tells_the_agent_apart() {
        let look = Look { band: false, selected: false, open: false, focused: false };
        let glyphs: Vec<&str> = Kind::ORDER
            .into_iter()
            .flat_map(|k| [dot_glyph(k, true), dot_glyph(k, false)])
            .collect();
        for i in 0..glyphs.len() {
            for j in (i + 1)..glyphs.len() {
                assert_ne!(glyphs[i], glyphs[j], "two dot glyphs collide: {glyphs:?}");
            }
        }
        // 状態を変えても形は動かない（色だけが状態に反応する）
        for kind in Kind::ORDER {
            for group in State::ORDER {
                let mut d = look_fixture();
                d.kind = kind;
                d.group = group;
                d.unread = true;
                assert_eq!(
                    row_cells(&d, look)[1].0,
                    dot_glyph(kind, true),
                    "{kind:?} changed shape at {}",
                    group.title()
                );
            }
        }
    }

    /// **凡例（版行・new 画面）と一覧のドットは同じ表を引く。**
    /// 凡例が中空（＝ 既読という別チャンネルの値）を名乗らないことも見る
    #[test]
    fn the_legend_glyph_is_the_one_the_rows_draw() {
        for kind in Kind::ORDER {
            assert_eq!(agent_glyph(kind), dot_glyph(kind, true), "{kind:?}");
            assert_ne!(agent_glyph(kind), dot_glyph(kind, false), "{kind:?} legend is hollow");
        }
    }

    /// **ドットの色は状態そのもの。** 4 状態それぞれで [`crate::poll::classify`] が
    /// 決めた `State` の色（[`State::color`]）がそのままドットへ出る。
    /// [`State::color`] を経由して取るので、対応表を手で書き写さない
    #[test]
    fn the_dot_color_matches_the_row_state() {
        let look = Look { band: false, selected: false, open: false, focused: false };
        let dot_color = |state: State| {
            let mut d = look_fixture();
            d.group = state;
            row_cells(&d, look)[1].1.fg
        };
        assert_eq!(dot_color(WAITING), Some(ui().attention), "Waiting");
        assert_eq!(dot_color(WORKING), Some(ui().working), "Working");
        assert_eq!(dot_color(IDLE), Some(ui().ok), "Idle");
        assert_eq!(dot_color(STOPPED), Some(ui().dim), "Stopped");
    }

    /// **Working 中のドットはコマ列を往復する。**
    ///
    /// コマ列は [`crate::theme::UiTheme::blink`] が持つので、ここが見るのは
    /// 「一周のうちに 2 つ以上の色が出る」「先頭コマは状態色そのもの」
    /// 「一周して戻る」の 3 つ。段階数や谷の深さはテーマ側のテストが固定する。
    ///
    /// **谷が Stopped と同色にならない**ことも見る: 谷を `dim` まで落としていた頃は、
    /// 位相しだいで止まった行と動いている行が同じ見た目になっていた
    #[test]
    fn a_working_dot_blinks_through_its_ramp() {
        let look = Look { band: false, selected: false, open: false, focused: false };
        let mut d = look_fixture();
        d.group = State::Working;
        let fg = |d: &RowData, tick: u64| {
            cells(&session_row_line(d, look, DEFAULT_INNER, tick))[1].1.fg
        };
        let len = ui().blink_len() as u64;
        let frames: Vec<_> = (0..len).map(|t| fg(&d, t)).collect();
        assert_eq!(frames[0], Some(State::Working.color()), "the ramp does not start lit");
        assert_eq!(fg(&d, len), frames[0], "the ramp does not come back around");
        assert!(
            frames.iter().collect::<std::collections::HashSet<_>>().len() >= 2,
            "the dot does not blink: {frames:?}"
        );
        assert!(
            frames.iter().all(|c| *c != Some(ui().dim)),
            "a blink frame collides with the stopped color: {frames:?}"
        );

        // 明滅しない状態は位相に関係なく自分の色のまま（色と明滅は同じ group から
        // 決まるので、「Working の色だが明滅しない」という行は型として作れない）
        d.group = State::Idle;
        for tick in 0..len {
            assert_eq!(fg(&d, tick), Some(State::Idle.color()), "a steady group moved at {tick}");
        }
    }

    /// **行末の状態語と色は同じ [`State`] から出る。** 4 状態それぞれで
    /// [`State::title`] の語が並び、その前景色が [`State::color`] と一致する。
    /// 語と色を別々に組むと、片方だけ変えたときに黙って食い違う
    #[test]
    fn the_row_tail_states_the_row_state_in_words_and_color() {
        for state in State::ORDER {
            let mut d = look_fixture();
            d.group = state;
            let drawn = row_cells(&d, Look { band: false, selected: false, open: false, focused: false });
            let text: String = drawn.iter().map(|c| c.0.as_str()).collect();
            let at = text
                .find(state.title())
                .unwrap_or_else(|| panic!("{} is not spelled out: {text:?}", state.title()));
            // 語の 1 文字目の色で見る（語全体に同じスタイルが掛かっている）
            assert_eq!(
                drawn[text[..at].chars().count()].1.fg,
                Some(state.color()),
                "{} is not drawn in its own color",
                state.title()
            );
        }
    }

    /// **状態語とドットは同じコマを引く。** 行の中で 2 つのものが別々のリズムで
    /// 動くと、1 つの呼吸ではなく雑音に見える。
    ///
    /// 位相を一周させて、どのコマでもドットと語の色が一致することを見る
    /// （片方だけ [`row_state_color`] を経由しなくなったら落ちる）
    #[test]
    fn the_state_word_blinks_in_step_with_the_dot() {
        let look = Look { band: false, selected: false, open: false, focused: false };
        let mut d = look_fixture();
        d.group = State::Working;
        for tick in 0..ui().blink_len() as u64 {
            let drawn = cells(&session_row_line(&d, look, DEFAULT_INNER, tick));
            let text: String = drawn.iter().map(|c| c.0.as_str()).collect();
            let at = text
                .find(State::Working.title())
                .unwrap_or_else(|| panic!("the state word is missing: {text:?}"));
            assert_eq!(
                drawn[text[..at].chars().count()].1.fg,
                drawn[1].1.fg,
                "the word and the dot are out of phase at tick {tick}"
            );
        }
    }

    /// **4 状態の色は互いに異なる。** 2 つの状態が同じ色になると、
    /// 語を読まない限り画面上で区別が付かなくなる
    #[test]
    fn the_four_group_colors_are_all_distinct() {
        let colors: Vec<Color> = State::ORDER.iter().map(|g| g.color()).collect();
        for i in 0..colors.len() {
            for j in (i + 1)..colors.len() {
                assert_ne!(
                    colors[i],
                    colors[j],
                    "{} and {} share a color",
                    State::ORDER[i].title(),
                    State::ORDER[j].title()
                );
            }
        }
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
            assert!(at("chosen") > at("Idle"), "{grouping:?}: {texts:?}");
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

    /// **一覧に隠し区画は無い。** アーカイブを廃止したので、行はどちらの
    /// グルーピングでも通常の一覧に出る（`close` が外すのは行だけなので、
    /// アーカイブとの差は「戻す導線があるか」しか残らず、
    /// 節を 1 つ増やす価値が無かった）
    #[test]
    fn every_row_stays_in_the_normal_list() {
        for grouping in [Grouping::State, Grouping::Directory] {
            let mut app = App {
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
        let bar = drawn_hint_bar(&mut app);
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
        let bar = drawn_hint_bar(&mut app);
        assert!(bar.contains("sidebar: ↑↓ select"), "{bar:?}");

        // メニュー表示中: 一覧のキーは全部このメニューが飲むので出さない
        let mut app = App {
            popup: Some(Popup {
                kind: crate::app::PopupKind::State,
                anchor_y: 3,
                selected: 0,
            }),
            ..base()
        };
        let bar = drawn_hint_bar(&mut app);
        assert!(bar.contains("popup: ↑↓ select · Enter run · Esc close"), "{bar:?}");
        assert!(!bar.contains("menu"), "the list keys are still listed: {bar:?}");

        // 端末: 予約キー以外は全部 claude が受ける
        let mut app = App {
            focus: Focus::Terminal,
            ..base()
        };
        let bar = drawn_hint_bar(&mut app);
        assert!(bar.contains("terminal: all keys pass through to claude"), "{bar:?}");
        assert!(!bar.contains("select"), "the sidebar keys are still listed: {bar:?}");

        // どの状態でも予約キーは出る（受け手に関係なく効く）
        for focus in [Focus::Sidebar, Focus::Terminal] {
            let mut app = App { focus, ..base() };
            let bar = drawn_hint_bar(&mut app);
            assert!(bar.contains("Ctrl+Q quit · Alt+←→ focus"), "{focus:?}: {bar:?}");
        }
    }

    /// **スロット間の移動キーは 2 枚以上のときだけ案内する。**
    /// 1 枚では行き先が無い（[`crate::panes::Layout::neighbor`] がどの向きでも
    /// `None`）ので、出すと効かないキーを案内することになる
    #[test]
    fn the_slot_move_keys_are_only_offered_once_there_is_a_second_slot() {
        let mut app = App {
            term_size: (120, 30),
            focus: Focus::Sidebar,
            ..Default::default()
        };
        let bar = drawn_hint_bar(&mut app);
        assert!(
            !bar.contains("slot"),
            "a one-slot layout still advertises the move keys: {bar:?}"
        );

        app.set_layout(crate::panes::Layout::Four);
        let bar = drawn_hint_bar(&mut app);
        assert!(bar.contains("Alt+Shift+←→↑↓ slot"), "{bar:?}");
    }

    /// 新規セッション画面は案内をペイン内に持つので、下部バーへは重ねない
    #[test]
    fn the_new_session_screen_keeps_its_hint_inside_the_pane() {
        let mut app = App {
            term_size: (120, 30),
            focus: Focus::Terminal,
            slots: vec![Slot::New(new_view::NewState::browse("."))],
            ..Default::default()
        };
        let bar = drawn_hint_bar(&mut app);
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
            Style::default().fg(ui().attention),
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

    /// そのスクリーン行に、指定した前景色のセルが 1 つでもあるか
    fn row_has_fg(app: &mut App, y: u16, fg: ratatui::style::Color) -> bool {
        let (w, h) = app.term_size;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).expect("test terminal");
        terminal
            .draw(|frame| {
                draw(frame, app);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        (0..w).any(|x| buffer[(x, y)].fg == fg)
    }

    /// **選択中でも版行の状態色（更新中の赤）は消えない。**
    ///
    /// 版行は [`Look::band`] の帯（選択・ホバー）に乗るが、前景色は
    /// [`UpdateState`] が運ぶ状態そのもの。選択のたびに `emph` へ上書きすると
    /// 「更新した本人が選択した行だけ更新中の赤が見えない」という形になっていた
    /// （クリックが hovered と selection を同じ行へ揃えるため、更新した本人は
    /// 必ずこの状態を踏む）
    #[test]
    fn a_selected_version_row_keeps_its_state_color_while_updating() {
        let mut app = App {
            term_size: (120, 30),
            agent_update: crate::app::agent_update_states(),
            ..Default::default()
        };
        *app.agent_update[&Kind::Claude].lock_recover() = crate::app::AgentUpdate::Running;
        // claude の版行はヘッダー 2 行目（y=1 が ccdesk, y=2 が claude）
        app.selection = SidebarPos::Row(1);
        assert!(
            row_has_fg(&mut app, 2, ui().working),
            "selecting the updating row lost its red (overwritten by emph)"
        );
    }

    /// **据え置きは版行に残る。** 下部バーの通知は 5 秒で消えるのに、押しても
    /// 効かない状態は次の更新まで続く ＝ 通知を見逃すと「押しても何も起きない行」に
    /// 戻ってしまう（claude が 5 日間更新されないまま気づかれなかった）。
    ///
    /// **据え置きを降ろす経路は要らない**ことも同時に見る: 状態が「押した時点の版」を
    /// 持つので、版が動けば（別経路で入った・手で入れ直した）条件が外れて
    /// 行は最新表示へ戻る
    #[test]
    fn a_stalled_update_stays_on_the_row_until_the_version_moves() {
        use crate::app::AgentUpdate;
        use crate::backend::AgentVersion;
        let mut app = App {
            agent_update: crate::app::agent_update_states(),
            ..Default::default()
        };
        let behind = AgentVersion {
            current: "2.1.225".to_string(),
            latest: Some("2.1.226".to_string()),
        };
        app.footer.versions.insert(Kind::Claude, behind.clone());
        assert_eq!(agent_update_state(&app, Kind::Claude), UpdateState::Available);

        // 更新を試して版が動かなかった ＝ 行に残る
        *app.agent_update[&Kind::Claude].lock_recover() = AgentUpdate::Stalled {
            version: "2.1.225".to_string(),
            announced: true,
        };
        assert_eq!(agent_update_state(&app, Kind::Claude), UpdateState::Stalled);

        // **状態は据え置きのまま**版が上がれば、行は最新表示へ戻る
        app.footer.versions.insert(
            Kind::Claude,
            AgentVersion {
                current: "2.1.226".to_string(),
                latest: None,
            },
        );
        assert_eq!(
            agent_update_state(&app, Kind::Claude),
            UpdateState::Current,
            "the row stayed stalled after the version actually moved"
        );

        // **版が動いたあと次の新版が出ても、押していない行は据え置きを名乗らない**
        // （据え置きは「あの版のまま」であることが条件なので、条件が外れたら消える）
        app.footer.versions.insert(
            Kind::Claude,
            AgentVersion {
                current: "2.1.226".to_string(),
                latest: Some("2.1.227".to_string()),
            },
        );
        assert_eq!(
            agent_update_state(&app, Kind::Claude),
            UpdateState::Available,
            "a row that was never pressed at this version called itself stalled"
        );

        // 押し直している間は Running が勝つ（やり直しが行から見える）
        app.footer.versions.insert(Kind::Claude, behind);
        *app.agent_update[&Kind::Claude].lock_recover() = AgentUpdate::Running;
        assert_eq!(agent_update_state(&app, Kind::Claude), UpdateState::Running);
    }

    /// **更新の状態は agent ごと。** 共有にすると、片方を更新している間に
    /// もう片方の版行まで Running になって押せなくなる（実機で踏んだ）
    #[test]
    fn updating_one_agent_leaves_the_other_agents_row_pressable() {
        let app = App {
            agent_update: crate::app::agent_update_states(),
            ..Default::default()
        };
        // claude だけ更新中にする
        *app.agent_update[&Kind::Claude].lock_recover() = crate::app::AgentUpdate::Running;

        assert_eq!(agent_update_state(&app, Kind::Claude), UpdateState::Running);
        assert_ne!(
            agent_update_state(&app, Kind::Codex),
            UpdateState::Running,
            "the other agent's row was frozen by an update that is not its own"
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

        // 区切り線は触れても光らない（位置は ccdesk 行 + agent の数 ＝ 数を書き写さない）
        let separator = 1 + Kind::ORDER.len();
        app.hovered = Some(SidebarPos::Row(separator));
        assert_eq!(
            app.sidebar_rows[separator],
            SidebarRow::Decoration,
            "row {separator} is not the separator"
        );
        assert!(
            highlighted_columns(&mut app, separator as u16 + 1).is_empty(),
            "a decoration row is highlighted"
        );
    }

    /// 1 フレーム描いて**下部バーの案内の行**を読む。位置の正本は
    /// [`bottom_bar_rows`] なので、テストが最下行を数え直さない
    /// （案内を上へ詰めたとき・agent を切って行数が変わったときに
    /// テストだけ取り残されない）
    fn drawn_hint_bar(app: &mut App) -> String {
        let y = app.term_size.1 - bottom_bar_rows(app);
        drawn_row(app, y)
    }

    /// 端末を 1 フレーム描いて、その 1 桁の文字を返す。
    /// **行の文字列（[`drawn_row`]）では桁を数えられない**（2 桁の文字が 1 つ入ると
    /// 文字数と桁がずれる）ので、桁を名指しで見る検査はこちらを使う
    fn drawn_cell(app: &mut App, x: u16, y: u16) -> String {
        drawn_span(app, x..=x, y)
    }

    /// 描いた画面の 1 行のうち、桁の範囲ぶんの表示文字列。
    /// **当たり判定が返す桁範囲をそのまま渡せる**（[`close_zone`] / [`id_zone`]）
    fn drawn_span(app: &mut App, cols: std::ops::RangeInclusive<u16>, y: u16) -> String {
        let (w, h) = app.term_size;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).expect("test terminal");
        terminal.draw(|frame| {
            draw(frame, app);
        })
        .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        cols.map(|x| buffer[(x, y)].symbol().to_string()).collect()
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
        let bar = drawn_hint_bar(&mut app);
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
        assert_eq!(
            row(rect.y + 1),
            "│ new claude session│",
            "row 1 is overwritten by the right pane"
        );
        assert_eq!(
            row(rect.y + 3),
            "│ remove project    │",
            "the last row is overwritten by the right pane"
        );
    }
}

