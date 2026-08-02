//! カラーパレット: 端末テーマに追従する。起動時に照会したホスト端末の実 fg/bg と
//! ANSI パレットから、16 色パレットでは表現できない色（うっすら帯・必ず読める淡色・
//! 明滅のコマ・使用率のグラデーション）を合成する。
//!
//! **固定 RGB を持たないのが方針。** 画面に出る色はすべて端末が答えた値から導くので、
//! ユーザーが端末のテーマを変えれば ccdesk の色も一緒に動く。端末が答えない場合だけ、
//! ANSI 名前色（＝ これもテーマに追従する）へ落ちる。
use ratatui::style::Color;
use std::io::{Read, Write};
use std::time::{Duration, Instant};

/// フォーカス中ペインの枠色 = 端末のデフォルト前景
pub(crate) const FOCUS_BORDER: Color = Color::Reset;
/// 通常テキスト = 端末のデフォルト前景（claude 本文と同じ）
pub(crate) const MUTED_FG: Color = Color::Reset;

/// 明滅 1 コマの表示時間。**描き直し間隔（[`crate::app::ANIMATION_REDRAW`]）が
/// この値から導かれる**ので、コマを増やしてもフレームは増えない
/// （どちらも 200ms という数字を別々に持たない）
pub(crate) const BLINK_TICK_MS: u64 = 200;

/// 状態色として引く ANSI パレットの索引。**照会するのはここに挙げた 4 つだけ**
/// （brightRed は明滅の明るい側、red はその暗い側、green/yellow は他の状態と
/// 使用率のグラデーション）
const PALETTE_INDICES: [u8; 4] = [1, 2, 3, 9];

/// 照会に答えない端末を待ち続けないための上限。**目印（DA1）が返れば即抜ける**ので、
/// 対応端末でこの時間まで待つことはない
const PALETTE_TIMEOUT: Duration = Duration::from_millis(500);

/// 端末の ANSI パレット実色（16bit/ch RGB）。**索引の意味を知るのはここだけ**で、
/// 呼び手は `red` / `green` のような名前で引く
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Palette {
    pub(crate) red: [u16; 3],
    pub(crate) green: [u16; 3],
    pub(crate) yellow: [u16; 3],
    pub(crate) bright_red: [u16; 3],
}

/// 起動時にホスト端末へ照会した ANSI パレット。None = 照会できなかった
/// （OSC 4 非対応 ＝ Windows Terminal 1.22 未満・旧 ConHost 等）
pub(crate) static HOST_PALETTE: std::sync::OnceLock<Option<Palette>> = std::sync::OnceLock::new();

/// 起動時のホスト端末色から合成した UI トーン。**端末色から導く色はすべてここが持つ**
/// ので、「この色はどこから来たのか」を探す場所が 1 つで済む
/// （照会失敗時は Dark+ 相当の fg/bg と ANSI 名前色になる）
pub(crate) struct UiTheme {
    pub(crate) emph: Color,  // 強調テキスト（背景の明暗で White / Black を自動選択）
    pub(crate) dim: Color,   // 淡色テキスト（fg と bg の中間 45%。どのテーマでも読める距離を保証）
    pub(crate) hl_bg: Color, // 選択・ホバーの帯（bg を fg 側へ 10% 寄せた色）
    // ---- 状態色（[`crate::poll::State::color`] が引く）----
    pub(crate) working: Color,
    pub(crate) attention: Color,
    pub(crate) ok: Color,
    pub(crate) fail: Color,
    /// working の明滅のコマ。**三角波に展開済み**なので、引く側は通し番号を
    /// 剰余で当てるだけでよい（往復の折り返しをコマを引くたびに計算しない）。
    /// パレットが取れれば 4 段階の往復（6 コマ）、取れなければ ANSI 2 色（4 コマ）
    blink: Vec<Color>,
    /// 使用率グラデーションの節（0% / 50% / 100%）。**RGB のまま持つ**ので
    /// [`usage_color`] が任意の % で補間できる
    usage_ramp: [[u16; 3]; 3],
}

impl UiTheme {
    /// 明滅のこのコマの色。`tick` は [`BLINK_TICK_MS`] 刻みの通し番号
    pub(crate) fn blink(&self, tick: u64) -> Color {
        self.blink[tick as usize % self.blink.len()]
    }

    /// 明滅が何コマで一周するか。**テストが位相を一周させるためだけにある**
    /// （描画側はコマ番号を剰余で当てるので長さを知らなくてよい）
    #[cfg(test)]
    pub(crate) fn blink_len(&self) -> usize {
        self.blink.len()
    }
}

static UI: std::sync::OnceLock<UiTheme> = std::sync::OnceLock::new();

/// 2 色の中間（`t` = 0 で `a`、1 で `b`）。**混色はここ 1 箇所**なので、
/// 帯・淡色・明滅・使用率が別々の混ぜ方を持たない
fn mix(a: [u16; 3], b: [u16; 3], t: f32) -> [u16; 3] {
    let ch = |i: usize| (a[i] as f32 + (b[i] as f32 - a[i] as f32) * t) as u16;
    [ch(0), ch(1), ch(2)]
}

/// 16bit/ch を ratatui の 8bit/ch へ落とす
fn rgb(c: [u16; 3]) -> Color {
    Color::Rgb((c[0] >> 8) as u8, (c[1] >> 8) as u8, (c[2] >> 8) as u8)
}

/// 8bit/ch を 16bit/ch へ広げる（フォールバックの定数を書くための補助）
const fn wide(r: u8, g: u8, b: u8) -> [u16; 3] {
    [
        (r as u16) << 8 | r as u16,
        (g as u16) << 8 | g as u16,
        (b as u16) << 8 | b as u16,
    ]
}

pub(crate) fn ui() -> &'static UiTheme {
    UI.get_or_init(|| {
        let (fg, bg) = HOST_COLORS.get().copied().unwrap_or((None, None));
        // フォールバックは claude への OSC 応答（`ccdesk::Responder`）と同じ既定
        // （別の値にすると「claude に送ったテーマ」と「ccdesk の描画が仮定する
        // テーマ」がずれる）
        let fg = fg.unwrap_or(ccdesk::DEFAULT_FG);
        let bg = bg.unwrap_or(ccdesk::DEFAULT_BG);
        // WCAG 相対輝度（claude 本体のテーマ判定と同じ式）で背景の明暗を判定
        let lum = (0.2126 * bg[0] as f32 + 0.7152 * bg[1] as f32 + 0.0722 * bg[2] as f32)
            / 65535.0;
        let palette = HOST_PALETTE.get().copied().flatten();
        let (working, attention, ok, fail) = match palette {
            Some(p) => (rgb(p.bright_red), rgb(p.yellow), rgb(p.green), rgb(p.red)),
            // パレットが取れない端末でも ANSI 名前色はテーマに追従する。
            // 失うのは中間色（＝ 明滅の段階数）だけ
            None => (
                Color::LightRed,
                Color::Yellow,
                Color::Green,
                Color::Red,
            ),
        };
        UiTheme {
            emph: if lum > 0.5 { Color::Black } else { Color::White },
            dim: rgb(mix(fg, bg, 0.45)),
            hl_bg: rgb(mix(bg, fg, 0.10)),
            working,
            attention,
            ok,
            fail,
            blink: blink_ramp(palette, bg),
            usage_ramp: usage_ramp(palette),
        }
    })
}

/// working の明滅のコマ列（**三角波に展開済み**、明るい側から始まる）。
///
/// **谷を red で止めず、背景側へ延ばす**のが要点: 端末パレットの red と brightRed は
/// 明度差が小さく（Campbell では `#CD3131` と `#F14C4C`）、その間だけを刻んでも
/// 1 段ずつの差が見えない。背景側へ延ばしても色相は赤のままなので、
/// 灰色の Stopped（[`UiTheme::dim`]）と混ざることはない。
///
/// パレットが取れない端末は ANSI の 2 色で往復する。1 コマを 2 tick 保つので
/// 周期は 800ms ＝ 段階が減っても明滅の速さは変わらない
fn blink_ramp(palette: Option<Palette>, bg: [u16; 3]) -> Vec<Color> {
    let Some(p) = palette else {
        return vec![
            Color::LightRed,
            Color::LightRed,
            Color::Red,
            Color::Red,
        ];
    };
    let bright = p.bright_red;
    let trough = mix(p.red, bg, 0.45);
    // 明 → 暗を 4 段階で刻み、折り返して往復させる（6 コマ = 1200ms 周期）
    let step = |i: usize| rgb(mix(bright, trough, i as f32 / 3.0));
    vec![step(0), step(1), step(2), step(3), step(2), step(1)]
}

/// 使用率グラデーションの節（0% 緑 / 50% 黄 / 100% 赤）。
/// パレットが取れなければ、従来と同じ見た目になる固定値へ落ちる
fn usage_ramp(palette: Option<Palette>) -> [[u16; 3]; 3] {
    match palette {
        Some(p) => [p.green, p.yellow, p.red],
        None => [wide(0, 200, 80), wide(255, 200, 60), wide(255, 0, 60)],
    }
}

/// 起動時に照会したホスト端末の実色（16bit/ch RGB）。None = 照会失敗
pub(crate) type HostColor = Option<[u16; 3]>;

/// 起動時にホスト端末へ照会した実 (fg, bg)。Responder が claude への
/// OSC 10/11 応答へ転送する = claude 自身のテーマ自動検出（theme=auto）が正しく動く。
/// 照会失敗（WT 1.22 未満・旧 ConHost 等）は Dark+ 相当の固定値で応答する
pub(crate) static HOST_COLORS: std::sync::OnceLock<(HostColor, HostColor)> = std::sync::OnceLock::new();

/// ホスト端末の実 fg/bg を OSC 10/11 で照会する。**raw mode / alt screen に
/// 入る前に呼ぶ**（TUI 起動と doctor が同じ照会を通る ＝ doctor の ok と
/// 実際の転送結果が食い違わない）。非対応端末はヒューリスティックで即 Err に
/// なるためハングしない。失敗は (None, None) ＝ Dark+ 相当のフォールバック
pub(crate) fn query_host_colors() -> (HostColor, HostColor) {
    use terminal_colorsaurus::{color_palette, QueryOptions};
    color_palette(QueryOptions::default())
        .map(|p| {
            let c = |c: terminal_colorsaurus::Color| Some([c.r, c.g, c.b]);
            (c(p.foreground), c(p.background))
        })
        .unwrap_or((None, None))
}

/// ホスト端末の ANSI パレットを OSC 4 で照会する。[`query_host_colors`] と同じく
/// **raw mode / alt screen に入る前に呼ぶ**。
///
/// **fg/bg が取れた端末にしか投げない**（引数の `host` がその結果）。これは
/// 読み取りが止まらないための担保でもある: 応答の終わりは目印として末尾に付ける
/// DA1（`ESC [ c`）で判定するので、「OSC にも DA1 にも答えない端末」に投げると
/// 最初の read で待ち続けてしまう。OSC 10/11 に答えた端末なら DA1 には必ず答える。
///
/// OSC 4 に答えない端末（Windows Terminal 1.22 未満）でも DA1 は返るので、
/// その場合は「パレット無し」として即座に None になる
pub(crate) fn query_palette(host: (HostColor, HostColor)) -> Option<Palette> {
    if host.0.is_none() || host.1.is_none() {
        return None;
    }
    let mut term = terminal_trx::terminal().ok()?;
    let mut lock = term.lock();
    let mut raw = lock.enable_raw_mode().ok()?;

    let mut out = String::new();
    for index in PALETTE_INDICES {
        out.push_str(&format!("\x1b]4;{index};?\x1b\\"));
    }
    // 終端の目印。**OSC 4 に答えない端末でもこれには答える**
    out.push_str("\x1b[c");
    raw.write_all(out.as_bytes()).ok()?;
    raw.flush().ok()?;

    let deadline = Instant::now() + PALETTE_TIMEOUT;
    let mut seen = Vec::new();
    let mut chunk = [0u8; 512];
    while Instant::now() < deadline {
        let n = raw.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        seen.extend_from_slice(&chunk[..n]);
        if ends_with_da1(&seen) {
            break;
        }
    }
    parse_palette(&seen)
}

/// 目印（DA1 の応答 `ESC [ ? … c`）が届いたか。**目印より前に届いた OSC 応答が
/// 答えのすべて**なので、これが見えた時点で読むのをやめてよい
fn ends_with_da1(buf: &[u8]) -> bool {
    let Some(start) = buf.windows(3).position(|w| w == b"\x1b[?") else {
        return false;
    };
    buf[start..].contains(&b'c')
}

/// 応答から [`PALETTE_INDICES`] の 4 色を取り出す。**1 つでも欠けたら None**:
/// 半分だけ端末由来・半分は既定という混ざった状態を作らない
fn parse_palette(buf: &[u8]) -> Option<Palette> {
    let text = String::from_utf8_lossy(buf);
    let find = |index: u8| -> Option<[u16; 3]> {
        let head = format!("]4;{index};rgb:");
        let rest = text.split(&head).nth(1)?;
        // 応答の終端は BEL か ST（`ESC \`）。どちらでも同じところで切る
        let spec = rest.split(['\x07', '\x1b']).next()?;
        parse_rgb(spec)
    };
    Some(Palette {
        red: find(1)?,
        green: find(2)?,
        yellow: find(3)?,
        bright_red: find(9)?,
    })
}

/// `cdcd/3131/3131` を 16bit/ch へ。**桁数はチャンネルごとに 1〜4 桁あり得る**
/// （xterm の仕様）ので、桁数から最大値を導いて 16bit へ引き伸ばす
fn parse_rgb(spec: &str) -> Option<[u16; 3]> {
    let mut parts = spec.split('/');
    let mut ch = || -> Option<u16> {
        let part = parts.next()?.trim();
        if part.is_empty() || part.len() > 4 {
            return None;
        }
        let raw = u32::from_str_radix(part, 16).ok()?;
        let max = (1u32 << (4 * part.len())) - 1;
        Some((raw * 0xffff / max) as u16)
    };
    let c = [ch()?, ch()?, ch()?];
    Some(c)
}

/// 使用率の色（緑 → 黄 → 赤の連続グラデーション。しきい値を持たないので
/// 「何 % で色が変わるか」という設定を増やさずに済む）。
/// **節の 3 色は端末パレット由来**なので、この帯もテーマに追従する
pub(crate) fn usage_color(pct: f64) -> Color {
    let [low, mid, high] = ui().usage_ramp;
    let pct = pct.clamp(0.0, 100.0) as f32;
    if pct < 50.0 {
        rgb(mix(low, mid, pct / 50.0))
    } else {
        rgb(mix(mid, high, (pct - 50.0) / 50.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 実測した Windows Terminal（Campbell）の応答をそのまま流す。
    /// **応答の形をテストが固定する**ので、桁数や終端の扱いを変えたときに気づける
    #[test]
    fn parses_a_real_terminal_reply() {
        let reply = concat!(
            "\x1b]4;1;rgb:cdcd/3131/3131\x1b\\",
            "\x1b]4;2;rgb:0d0d/bcbc/7979\x1b\\",
            "\x1b]4;3;rgb:e5e5/e5e5/1010\x1b\\",
            "\x1b]4;9;rgb:f1f1/4c4c/4c4c\x1b\\",
            "\x1b[?61;6;7;22;23;24;28;32;42c",
        );
        let p = parse_palette(reply.as_bytes()).expect("the reply did not parse");
        assert_eq!(p.red, [0xcdcd, 0x3131, 0x3131]);
        assert_eq!(p.green, [0x0d0d, 0xbcbc, 0x7979]);
        assert_eq!(p.yellow, [0xe5e5, 0xe5e5, 0x1010]);
        assert_eq!(p.bright_red, [0xf1f1, 0x4c4c, 0x4c4c]);
        assert!(ends_with_da1(reply.as_bytes()), "the sentinel was not found");
    }

    /// BEL 終端（xterm の古い作法）と短い桁数でも同じ値になる
    #[test]
    fn accepts_bel_terminated_and_short_replies() {
        let reply = concat!(
            "\x1b]4;1;rgb:ff/00/00\x07",
            "\x1b]4;2;rgb:0/f/0\x07",
            "\x1b]4;3;rgb:ffff/ffff/0000\x07",
            "\x1b]4;9;rgb:fff/000/000\x07",
            "\x1b[?1;2c",
        );
        let p = parse_palette(reply.as_bytes()).expect("the reply did not parse");
        // 1 桁でも 4 桁でも同じ「満たした」値へ伸びる
        assert_eq!(p.red[0], 0xffff);
        assert_eq!(p.green[1], 0xffff);
        assert_eq!(p.yellow[2], 0x0000);
        assert_eq!(p.bright_red[0], 0xffff);
    }

    /// OSC 4 に答えない端末（DA1 だけ返る）は None。**部分的な答えも None**:
    /// 半分だけ端末由来という混ざった状態を作らない
    #[test]
    fn a_terminal_that_only_answers_the_sentinel_yields_no_palette() {
        assert_eq!(parse_palette(b"\x1b[?61;6c"), None);
        let partial = "\x1b]4;1;rgb:cdcd/3131/3131\x1b\\\x1b[?61c";
        assert_eq!(parse_palette(partial.as_bytes()), None);
    }

    /// 明滅のコマ列は往復する ＝ 先頭が一番明るく、折り返しが中央にある。
    /// **パレットが無くても列は空にならない**（明滅が止まらない）
    #[test]
    fn the_blink_ramp_is_a_round_trip() {
        let bg = ccdesk::DEFAULT_BG;
        let p = Palette {
            red: [0xcdcd, 0x3131, 0x3131],
            green: [0x0d0d, 0xbcbc, 0x7979],
            yellow: [0xe5e5, 0xe5e5, 0x1010],
            bright_red: [0xf1f1, 0x4c4c, 0x4c4c],
        };
        let ramp = blink_ramp(Some(p), bg);
        assert_eq!(ramp.len(), 6, "the ramp is not a 4-step round trip");
        assert_eq!(ramp[0], rgb(p.bright_red), "the ramp does not start lit");
        assert_eq!(ramp[1], ramp[5], "the ramp is not symmetric");
        assert_eq!(ramp[2], ramp[4], "the ramp is not symmetric");
        assert_ne!(ramp[0], ramp[3], "the ramp has no trough");
        // 谷は赤のまま（背景側へ延ばしても色相を失わない ＝ 灰色の Stopped と混ざらない）
        let Color::Rgb(r, g, b) = ramp[3] else {
            panic!("the trough is not an RGB color");
        };
        assert!(r > g && r > b, "the trough lost its hue: {r} {g} {b}");

        let fallback = blink_ramp(None, bg);
        assert!(!fallback.is_empty(), "the fallback ramp is empty");
        assert_eq!(fallback[0], Color::LightRed);
    }
}
