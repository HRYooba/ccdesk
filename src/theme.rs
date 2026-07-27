//! カラーパレット: 端末テーマに追従する。起動時に照会したホスト端末の実 fg/bg から
//! 16 色パレットに無い「うっすら帯」「必ず読める淡色」を合成する。
use ratatui::style::Color;

/// 枠タイトルに出す ccdesk のバージョン
// ---- カラーパレット: 端末テーマに追従する ----
// 基本は ANSI 名前付き色 + Reset。ただし「うっすら帯」「必ず読める淡色」は
// 16 色パレットに存在しないため、起動時に照会したホスト端末の実 fg/bg から中間色を合成する
/// フォーカス中ペインの枠色 = 端末のデフォルト前景
pub(crate) const FOCUS_BORDER: Color = Color::Reset;
/// 通常テキスト = 端末のデフォルト前景（claude 本文と同じ）
pub(crate) const MUTED_FG: Color = Color::Reset;
// ---- 状態色（公式 Agent View のオレンジ/黄/緑/赤を ANSI パレットへ対応付け）----
/// 作業中 = brightRed（オレンジは 16 色に無いため最も近い暖色。Failed の Red とは明度で区別）
pub(crate) const C_WORKING: Color = Color::LightRed;
/// 入力待ち・PR 番号
pub(crate) const C_ATTENTION: Color = Color::Yellow;
/// 完了・起動アクション
pub(crate) const C_OK: Color = Color::Green;
/// 失敗
pub(crate) const C_FAIL: Color = Color::Red;

/// 起動時のホスト端末色から合成した UI トーン。16 色パレットで表現できない
/// 「うっすら帯」「必ず読める淡色」はここで作る（照会失敗時は Dark+ 相当の値になる）
pub(crate) struct UiTheme {
    pub(crate) emph: Color,  // 強調テキスト（背景の明暗で White / Black を自動選択）
    pub(crate) dim: Color,   // 淡色テキスト（fg と bg の中間 45%。どのテーマでも読める距離を保証）
    pub(crate) hl_bg: Color, // 選択・ホバーの帯（bg を fg 側へ 10% 寄せた色）
}

static UI: std::sync::OnceLock<UiTheme> = std::sync::OnceLock::new();

pub(crate) fn ui() -> &'static UiTheme {
    UI.get_or_init(|| {
        let (fg, bg) = HOST_COLORS.get().copied().unwrap_or((None, None));
        let fg = fg.unwrap_or([0xcccc, 0xcccc, 0xcccc]);
        let bg = bg.unwrap_or([0x1e1e, 0x1e1e, 0x1e1e]);
        let mix = |a: [u16; 3], b: [u16; 3], t: f32| -> Color {
            let ch = |i: usize| {
                let v = a[i] as f32 + (b[i] as f32 - a[i] as f32) * t;
                (v as u32 >> 8) as u8
            };
            Color::Rgb(ch(0), ch(1), ch(2))
        };
        // WCAG 相対輝度（claude 本体のテーマ判定と同じ式）で背景の明暗を判定
        let lum = (0.2126 * bg[0] as f32 + 0.7152 * bg[1] as f32 + 0.0722 * bg[2] as f32)
            / 65535.0;
        UiTheme {
            emph: if lum > 0.5 { Color::Black } else { Color::White },
            dim: mix(fg, bg, 0.45),
            hl_bg: mix(bg, fg, 0.10),
        }
    })
}

/// 起動時に照会したホスト端末の実色（16bit/ch RGB）。None = 照会失敗
pub(crate) type HostColor = Option<[u16; 3]>;

/// 起動時にホスト端末へ照会した実 (fg, bg)。Responder が claude への
/// OSC 10/11 応答へ転送する = claude 自身のテーマ自動検出（theme=auto）が正しく動く。
/// 照会失敗（WT 1.22 未満・旧 ConHost 等）は Dark+ 相当の固定値で応答する
pub(crate) static HOST_COLORS: std::sync::OnceLock<(HostColor, HostColor)> = std::sync::OnceLock::new();

/// 使用率の色（緑 → 黄 → 赤の連続グラデーション。しきい値を持たないので
/// 「何 % で色が変わるか」という設定を増やさずに済む）
pub(crate) fn usage_color(pct: f64) -> Color {
    if pct < 50.0 {
        Color::Rgb((pct * 5.1) as u8, 200, 80)
    } else {
        Color::Rgb(255, (200.0 - (pct - 50.0) * 4.0).max(0.0) as u8, 60)
    }
}
