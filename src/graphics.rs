//! kitty graphics を Sixel へ描き替える。
//!
//! claude（`CLAUDE_CODE_FORCE_TERMINAL_IMAGES=1`）は画像を kitty graphics の
//! **Unicode placeholder** 方式で出す: APC（`ESC _ G … ESC \`）で画像を送り、
//! 画面には U+10EEEE ＋ 行・列を表す結合文字のセルを並べる。セルの前景色が画像 ID。
//! ホスト端末（Windows Terminal）は kitty graphics を持たないが Sixel は描けるので、
//! ccdesk が APC を取り除いて画像を覚え、placeholder の並んだ矩形へ Sixel を重ねる。
//!
//! **APC が PTY を通るのは新しい ConPTY だけ**（OS 同梱の ConPTY は APC を捨てる）。
//! Microsoft 配布の ConPTY（`assets/conpty/`）を ccdesk に埋め込み、起動時に
//! [`install_conpty`] が `~/.ccdesk/conpty/` へ書き出して DLL の探し先に足す。
//! portable-pty は名前だけで `conpty.dll` を読みに行くので、それで新しい方を掴む

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

/// placeholder の文字
pub(crate) const PLACEHOLDER: char = '\u{10EEEE}';

/// 行・列の番号を表す結合文字（kitty の rowcolumn-diacritics.txt の順）
const DIACRITICS: [u32; 297] = [
    0x0305, 0x030D, 0x030E, 0x0310, 0x0312, 0x033D, 0x033E, 0x033F, 0x0346, 0x034A,
    0x034B, 0x034C, 0x0350, 0x0351, 0x0352, 0x0357, 0x035B, 0x0363, 0x0364, 0x0365,
    0x0366, 0x0367, 0x0368, 0x0369, 0x036A, 0x036B, 0x036C, 0x036D, 0x036E, 0x036F,
    0x0483, 0x0484, 0x0485, 0x0486, 0x0487, 0x0592, 0x0593, 0x0594, 0x0595, 0x0597,
    0x0598, 0x0599, 0x059C, 0x059D, 0x059E, 0x059F, 0x05A0, 0x05A1, 0x05A8, 0x05A9,
    0x05AB, 0x05AC, 0x05AF, 0x05C4, 0x0610, 0x0611, 0x0612, 0x0613, 0x0614, 0x0615,
    0x0616, 0x0617, 0x0657, 0x0658, 0x0659, 0x065A, 0x065B, 0x065D, 0x065E, 0x06D6,
    0x06D7, 0x06D8, 0x06D9, 0x06DA, 0x06DB, 0x06DC, 0x06DF, 0x06E0, 0x06E1, 0x06E2,
    0x06E4, 0x06E7, 0x06E8, 0x06EB, 0x06EC, 0x0730, 0x0732, 0x0733, 0x0735, 0x0736,
    0x073A, 0x073D, 0x073F, 0x0740, 0x0741, 0x0743, 0x0745, 0x0747, 0x0749, 0x074A,
    0x07EB, 0x07EC, 0x07ED, 0x07EE, 0x07EF, 0x07F0, 0x07F1, 0x07F3, 0x0816, 0x0817,
    0x0818, 0x0819, 0x081B, 0x081C, 0x081D, 0x081E, 0x081F, 0x0820, 0x0821, 0x0822,
    0x0823, 0x0825, 0x0826, 0x0827, 0x0829, 0x082A, 0x082B, 0x082C, 0x082D, 0x0951,
    0x0953, 0x0954, 0x0F82, 0x0F83, 0x0F86, 0x0F87, 0x135D, 0x135E, 0x135F, 0x17DD,
    0x193A, 0x1A17, 0x1A75, 0x1A76, 0x1A77, 0x1A78, 0x1A79, 0x1A7A, 0x1A7B, 0x1A7C,
    0x1B6B, 0x1B6D, 0x1B6E, 0x1B6F, 0x1B70, 0x1B71, 0x1B72, 0x1B73, 0x1CD0, 0x1CD1,
    0x1CD2, 0x1CDA, 0x1CDB, 0x1CE0, 0x1DC0, 0x1DC1, 0x1DC3, 0x1DC4, 0x1DC5, 0x1DC6,
    0x1DC7, 0x1DC8, 0x1DC9, 0x1DCB, 0x1DCC, 0x1DD1, 0x1DD2, 0x1DD3, 0x1DD4, 0x1DD5,
    0x1DD6, 0x1DD7, 0x1DD8, 0x1DD9, 0x1DDA, 0x1DDB, 0x1DDC, 0x1DDD, 0x1DDE, 0x1DDF,
    0x1DE0, 0x1DE1, 0x1DE2, 0x1DE3, 0x1DE4, 0x1DE5, 0x1DE6, 0x1DFE, 0x20D0, 0x20D1,
    0x20D4, 0x20D5, 0x20D6, 0x20D7, 0x20DB, 0x20DC, 0x20E1, 0x20E7, 0x20E9, 0x20F0,
    0x2CEF, 0x2CF0, 0x2CF1, 0x2DE0, 0x2DE1, 0x2DE2, 0x2DE3, 0x2DE4, 0x2DE5, 0x2DE6,
    0x2DE7, 0x2DE8, 0x2DE9, 0x2DEA, 0x2DEB, 0x2DEC, 0x2DED, 0x2DEE, 0x2DEF, 0x2DF0,
    0x2DF1, 0x2DF2, 0x2DF3, 0x2DF4, 0x2DF5, 0x2DF6, 0x2DF7, 0x2DF8, 0x2DF9, 0x2DFA,
    0x2DFB, 0x2DFC, 0x2DFD, 0x2DFE, 0x2DFF, 0xA66F, 0xA67C, 0xA67D, 0xA6F0, 0xA6F1,
    0xA8E0, 0xA8E1, 0xA8E2, 0xA8E3, 0xA8E4, 0xA8E5, 0xA8E6, 0xA8E7, 0xA8E8, 0xA8E9,
    0xA8EA, 0xA8EB, 0xA8EC, 0xA8ED, 0xA8EE, 0xA8EF, 0xA8F0, 0xA8F1, 0xAAB0, 0xAAB2,
    0xAAB3, 0xAAB7, 0xAAB8, 0xAABE, 0xAABF, 0xAAC1, 0xFE20, 0xFE21, 0xFE22, 0xFE23,
    0xFE24, 0xFE25, 0xFE26, 0x10A0F, 0x10A38, 0x1D185, 0x1D186, 0x1D187, 0x1D188, 0x1D189,
    0x1D1AA, 0x1D1AB, 0x1D1AC, 0x1D1AD, 0x1D242, 0x1D243, 0x1D244,
];

fn diacritic_index(c: char) -> Option<u32> {
    DIACRITICS.iter().position(|&d| d == c as u32).map(|i| i as u32)
}

/// 埋め込んだ ConPTY の版（`assets/conpty/` の中身を差し替えたら上げる）。
/// 書き出し先のフォルダ名になるので、版ごとに別の場所へ置かれる
const CONPTY_VERSION: &str = "1.24.260710001";
const CONPTY_DLL: &[u8] = include_bytes!("../assets/conpty/conpty.dll");
const OPEN_CONSOLE: &[u8] = include_bytes!("../assets/conpty/OpenConsole.exe");

static CONPTY_READY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// 埋め込んだ ConPTY を `~/.ccdesk/conpty/<版>/` へ書き出し、DLL の探し先に足す。
/// **最初の PTY を開く前に呼ぶ**（portable-pty は最初に開くときに DLL を読んで固定する）。
/// 失敗したら OS 同梱の ConPTY のまま動く（画像が出ないだけ）
pub(crate) fn install_conpty() -> bool {
    *CONPTY_READY.get_or_init(|| {
        let Some(dir) = ccdesk::ccdesk_dir().map(|d| d.join("conpty").join(CONPTY_VERSION)) else {
            return false;
        };
        let write = |name: &str, bytes: &[u8]| -> bool {
            let path = dir.join(name);
            // 動いている OpenConsole は上書きできないので、同じ大きさなら書き直さない
            if std::fs::metadata(&path).is_ok_and(|m| m.len() == bytes.len() as u64) {
                return true;
            }
            std::fs::create_dir_all(&dir).is_ok() && std::fs::write(&path, bytes).is_ok()
        };
        if !(write("conpty.dll", CONPTY_DLL) && write("OpenConsole.exe", OPEN_CONSOLE)) {
            return false;
        }
        set_dll_directory(&dir)
    })
}

/// 新しい ConPTY で PTY を開くか（＝ claude に画像を求めてよいか）
pub(crate) fn images_enabled() -> bool {
    CONPTY_READY.get().copied().unwrap_or(false)
}

#[cfg(windows)]
fn set_dll_directory(dir: &std::path::Path) -> bool {
    use std::os::windows::ffi::OsStrExt as _;
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: wide は NUL 終端の UTF-16 で、呼び出しの間生きている
    unsafe { windows::Win32::System::LibraryLoader::SetDllDirectoryW(windows::core::PCWSTR(wide.as_ptr())) }
        .is_ok()
}

#[cfg(not(windows))]
fn set_dll_directory(_dir: &std::path::Path) -> bool {
    false
}

/// PTY の出力から kitty graphics の APC を抜き出す。読み取りの境目で
/// 途切れた APC も続きから拾えるよう、状態を持ち越す
#[derive(Default)]
pub(crate) struct ApcFilter {
    state: FilterState,
    apc: Vec<u8>,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum FilterState {
    #[default]
    Text,
    Esc,
    Apc,
    ApcEsc,
}

impl ApcFilter {
    /// `input` のうち APC 以外を `out` へ、`G` で始まる APC の中身を `commands` へ。
    /// `G` 以外の APC は捨てる（vt100 も読まない）
    pub(crate) fn feed(&mut self, input: &[u8], out: &mut Vec<u8>, commands: &mut Vec<Vec<u8>>) {
        for &b in input {
            match self.state {
                FilterState::Text => {
                    if b == 0x1b {
                        self.state = FilterState::Esc;
                    } else {
                        out.push(b);
                    }
                }
                FilterState::Esc => {
                    if b == b'_' {
                        self.state = FilterState::Apc;
                        self.apc.clear();
                    } else {
                        out.push(0x1b);
                        if b != 0x1b {
                            out.push(b);
                            self.state = FilterState::Text;
                        }
                    }
                }
                FilterState::Apc => {
                    if b == 0x1b {
                        self.state = FilterState::ApcEsc;
                    } else {
                        self.apc.push(b);
                    }
                }
                FilterState::ApcEsc => {
                    if b == b'\\' {
                        if self.apc.first() == Some(&b'G') {
                            commands.push(self.apc[1..].to_vec());
                        }
                        self.apc.clear();
                        self.state = FilterState::Text;
                    } else {
                        self.apc.push(0x1b);
                        self.apc.push(b);
                        self.state = FilterState::Apc;
                    }
                }
            }
        }
    }
}

/// 画素（RGBA）
pub(crate) struct Picture {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) rgba: Vec<u8>,
}

type Keys = Vec<(String, String)>;

/// 1 つの子（窓）が送ってきた画像と、その仮想配置の大きさ（セル）
#[derive(Default)]
pub(crate) struct Graphics {
    pictures: HashMap<u32, Arc<Picture>>,
    /// 画像 ID → (列, 行)。placeholder が指す矩形の大きさ
    placements: HashMap<u32, (u16, u16)>,
    /// 分割送信（`m=1`）の途中: 最初の APC の鍵と、ここまでの本体
    pending: Option<(u32, Keys, Vec<u8>)>,
}

fn keys(control: &str) -> Keys {
    control
        .split(',')
        .filter_map(|kv| kv.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
        .collect()
}

fn get<'a>(keys: &'a [(String, String)], key: &str) -> Option<&'a str> {
    keys.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn num(keys: &[(String, String)], key: &str) -> Option<u32> {
    get(keys, key).and_then(|v| v.parse().ok())
}

impl Graphics {
    /// 1 つの APC を適用し、子へ返す応答があれば返す
    pub(crate) fn apply(&mut self, command: &[u8]) -> Option<Vec<u8>> {
        let text = String::from_utf8_lossy(command);
        let (control, payload) = text.split_once(';').unwrap_or((&text, ""));
        let mut k = keys(control);
        let mut data = payload.as_bytes().to_vec();
        // 分割送信の続きは最初の APC の鍵を引き継ぐ（続きは m= しか持たないことがある）
        let more = num(&k, "m") == Some(1);
        if let Some((_, first, mut head)) = self.pending.take() {
            head.append(&mut data);
            data = head;
            k = first;
        }
        let id = num(&k, "i").unwrap_or(0);
        if more {
            self.pending = Some((id, k, data));
            return None;
        }
        let action = get(&k, "a").unwrap_or("t").to_string();
        let quiet = num(&k, "q").unwrap_or(0);
        let reply = |ok: bool, msg: &str| -> Option<Vec<u8>> {
            if (ok && quiet >= 1) || quiet >= 2 || id == 0 {
                return None;
            }
            Some(format!("\x1b_Gi={id};{msg}\x1b\\").into_bytes())
        };
        match action.as_str() {
            "q" => reply(true, "OK"),
            "d" => {
                match get(&k, "d").unwrap_or("a") {
                    "i" | "I" => {
                        self.pictures.remove(&id);
                        self.placements.remove(&id);
                    }
                    "a" | "A" => {
                        self.pictures.clear();
                        self.placements.clear();
                    }
                    _ => {}
                }
                None
            }
            "p" => {
                self.place(&k, id);
                None
            }
            "t" | "T" => match load(&k, &data) {
                Some(picture) => {
                    self.pictures.insert(id, Arc::new(picture));
                    if action == "T" {
                        self.place(&k, id);
                    }
                    reply(true, "OK")
                }
                None => reply(false, "EINVAL:could not load"),
            },
            _ => None,
        }
    }

    fn place(&mut self, k: &[(String, String)], id: u32) {
        if let (Some(c), Some(r)) = (num(k, "c"), num(k, "r")) {
            self.placements.insert(id, (c as u16, r as u16));
        }
    }

    /// 画像と、その配置の大きさ（列, 行）
    pub(crate) fn picture(&self, id: u32) -> Option<(Arc<Picture>, (u16, u16))> {
        Some((self.pictures.get(&id)?.clone(), *self.placements.get(&id)?))
    }
}

/// 送られてきた画像を RGBA にする。`t=f`（ファイル名）と `t=d`（本体）、
/// `f=100`（PNG）・`f=32`（RGBA）・`f=24`（RGB）を扱う
fn load(k: &[(String, String)], payload: &[u8]) -> Option<Picture> {
    use base64::Engine as _;
    let raw = base64::engine::general_purpose::STANDARD.decode(payload).ok()?;
    let bytes = match get(k, "t").unwrap_or("d") {
        "d" => raw,
        "f" | "t" => std::fs::read(String::from_utf8(raw).ok()?).ok()?,
        _ => return None,
    };
    match num(k, "f").unwrap_or(32) {
        100 => {
            let image = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png).ok()?;
            let rgba = image.to_rgba8();
            Some(Picture { width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() })
        }
        32 => {
            let (width, height) = (num(k, "s")?, num(k, "v")?);
            (bytes.len() == (width * height * 4) as usize).then_some(Picture { width, height, rgba: bytes })
        }
        24 => {
            let (width, height) = (num(k, "s")?, num(k, "v")?);
            if bytes.len() != (width * height * 3) as usize {
                return None;
            }
            let rgba = bytes.chunks(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect();
            Some(Picture { width, height, rgba })
        }
        _ => None,
    }
}

/// 画面に見えている placeholder の塊（1 画像につき 1 つ）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Visible {
    pub(crate) id: u32,
    /// 見えている範囲の左上（子の画面のセル座標）
    pub(crate) row: u16,
    pub(crate) col: u16,
    /// 見えている範囲の左上が、画像の何行目・何列目にあたるか
    pub(crate) image_row: u16,
    pub(crate) image_col: u16,
    /// 見えている範囲の大きさ（セル）
    pub(crate) rows: u16,
    pub(crate) cols: u16,
}

/// 画面の placeholder を拾い、画像ごとの見えている矩形にまとめる。
/// 結合文字が省かれたセルは、左隣の続き（同じ行・次の列）として読む（kitty の規約）
pub(crate) fn visible(screen: &vt100::Screen) -> Vec<Visible> {
    let (rows, cols) = screen.size();
    let mut found: HashMap<u32, Visible> = HashMap::new();
    for y in 0..rows {
        let mut prev: Option<(u32, u32, u32)> = None; // (id, 画像の行, 画像の列)
        for x in 0..cols {
            let Some(cell) = screen.cell(y, x) else {
                prev = None;
                continue;
            };
            let contents = cell.contents();
            let mut chars = contents.chars();
            if chars.next() != Some(PLACEHOLDER) {
                prev = None;
                continue;
            }
            let low = match cell.fgcolor() {
                vt100::Color::Rgb(r, g, b) => (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b),
                vt100::Color::Idx(i) => u32::from(i),
                vt100::Color::Default => 0,
            };
            let marks: Vec<u32> = chars.filter_map(diacritic_index).collect();
            let (image_row, image_col, high) = match (marks.first(), marks.get(1), prev) {
                (Some(&r), Some(&c), _) => (r, c, marks.get(2).copied()),
                (Some(&r), None, Some((_, pr, pc))) if pr == r => (r, pc + 1, None),
                (None, None, Some((_, pr, pc))) => (pr, pc + 1, None),
                (Some(&r), None, _) => (r, 0, None),
                _ => (0, 0, None),
            };
            let id = high.map_or(low, |h| (h << 24) | low);
            prev = Some((id, image_row, image_col));
            let entry = found.entry(id).or_insert(Visible {
                id,
                row: y,
                col: x,
                image_row: image_row as u16,
                image_col: image_col as u16,
                rows: 0,
                cols: 0,
            });
            // 上の行から走査するので row は最小。列は行ごとに左端がずれ得るので寄せる
            if x < entry.col {
                entry.image_col = entry.image_col.saturating_sub(entry.col - x);
                entry.cols += entry.col - x;
                entry.col = x;
            }
            entry.rows = entry.rows.max(y - entry.row + 1);
            entry.cols = entry.cols.max(x - entry.col + 1);
        }
    }
    let mut out: Vec<Visible> = found.into_values().collect();
    out.sort_by_key(|v| (v.row, v.col, v.id));
    out
}

/// 1 フレームで描く画像。`visible` の座標は**端末の絶対座標**へ移してある
#[derive(Clone)]
pub(crate) struct Paint {
    pub(crate) picture: Arc<Picture>,
    pub(crate) visible: Visible,
    /// 画像全体の大きさ（列, 行）
    pub(crate) size: (u16, u16),
}

type PaintKey = (usize, Visible, (u16, u16));

impl Paint {
    fn key(&self) -> PaintKey {
        (Arc::as_ptr(&self.picture) as usize, self.visible, self.size)
    }
}

/// 画面に出した Sixel の記録。**同じ物を毎フレーム送らない**（Sixel は重い）
#[derive(Default)]
pub(crate) struct Painter {
    shown: Vec<PaintKey>,
    cache: HashMap<(PaintKey, (u16, u16)), Arc<String>>,
}

impl Painter {
    /// 前のフレームと描く物が変わったか
    pub(crate) fn changed(&self, paints: &[Paint]) -> bool {
        paints.iter().map(Paint::key).ne(self.shown.iter().copied())
    }

    /// 端末に Sixel を出したままか（出していれば、描き直さないと跡が残る）
    pub(crate) fn has_shown(&self) -> bool {
        !self.shown.is_empty()
    }

    /// 画像を Sixel で送る（queue のみ）。`cell` はセルの画素寸法 (幅, 高さ)
    pub(crate) fn paint(&mut self, out: &mut impl Write, paints: &[Paint], cell: (u16, u16)) {
        self.shown = paints.iter().map(Paint::key).collect();
        if self.cache.len() > 64 {
            self.cache.clear();
        }
        for p in paints {
            let sixel = self
                .cache
                .entry((p.key(), cell))
                .or_insert_with(|| Arc::new(encode(p, cell).unwrap_or_default()))
                .clone();
            if sixel.is_empty() {
                continue;
            }
            let _ = write!(out, "\x1b7\x1b[{};{}H{}\x1b8", p.visible.row + 1, p.visible.col + 1, sixel);
        }
    }
}

/// 画像を配置の大きさ（セル × セル画素）へ縮め、見えている部分だけ切り出して Sixel にする
fn encode(p: &Paint, (cw, ch): (u16, u16)) -> Option<String> {
    let (cols, rows) = p.size;
    let (w, h) = (u32::from(cols) * u32::from(cw), u32::from(rows) * u32::from(ch));
    let source = image::RgbaImage::from_raw(p.picture.width, p.picture.height, p.picture.rgba.clone())?;
    let scaled = image::imageops::resize(&source, w, h, image::imageops::FilterType::Triangle);
    let v = p.visible;
    let (x, y) = (u32::from(v.image_col) * u32::from(cw), u32::from(v.image_row) * u32::from(ch));
    let (vw, vh) = (u32::from(v.cols) * u32::from(cw), u32::from(v.rows) * u32::from(ch));
    let (vw, vh) = (vw.min(w.saturating_sub(x)), vh.min(h.saturating_sub(y)));
    if vw == 0 || vh == 0 {
        return None;
    }
    let crop = image::imageops::crop_imm(&scaled, x, y, vw, vh).to_image();
    icy_sixel::SixelImage::from_rgba(crop.into_raw(), vw as usize, vh as usize).encode().ok()
}

/// ホスト端末のセルの画素寸法（起動時に [`query_cell_pixels`] で聞いた値）
pub(crate) static CELL_PIXELS: std::sync::OnceLock<(u16, u16)> = std::sync::OnceLock::new();

/// セルの画素寸法を聞けなかったときの仮の値（Windows Terminal 既定の字の大きさ相当）
pub(crate) const FALLBACK_CELL: (u16, u16) = (9, 19);

/// ホスト端末のセルの画素寸法を聞く（CSI 16 t。答えなければ CSI 14 t を桁数で割る）。
/// **raw mode / alt screen に入る前に呼ぶ**（[`crate::theme::query_palette`] と同じ作法。
/// 終わりの目印は DA1）
pub(crate) fn query_cell_pixels() -> Option<(u16, u16)> {
    use std::io::Read;
    let mut term = terminal_trx::terminal().ok()?;
    let mut lock = term.lock();
    let mut raw = lock.enable_raw_mode().ok()?;
    raw.write_all(b"\x1b[16t\x1b[14t\x1b[c").ok()?;
    raw.flush().ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
    let mut seen = Vec::new();
    let mut chunk = [0u8; 256];
    while std::time::Instant::now() < deadline {
        let n = raw.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        seen.extend_from_slice(&chunk[..n]);
        if seen.windows(3).any(|w| w == b"\x1b[?") && seen.ends_with(b"c") {
            break;
        }
    }
    drop(raw);
    parse_cell_pixels(&String::from_utf8_lossy(&seen), crossterm::terminal::size().ok()?)
}

fn parse_cell_pixels(text: &str, (cols, rows): (u16, u16)) -> Option<(u16, u16)> {
    let report = |code: &str| -> Option<(u16, u16)> {
        let rest = text.split(&format!("\x1b[{code};")).nth(1)?;
        let body = rest.split('t').next()?;
        let (h, w) = body.split_once(';')?;
        Some((w.parse().ok()?, h.parse().ok()?))
    };
    if let Some(cell) = report("6").filter(|&(w, h)| w > 0 && h > 0) {
        return Some(cell);
    }
    let (pw, ph) = report("4")?;
    (cols > 0 && rows > 0 && pw >= cols && ph >= rows).then(|| (pw / cols, ph / rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_pulls_out_graphics_apc_split_across_reads() {
        let mut f = ApcFilter::default();
        let (mut out, mut cmds) = (Vec::new(), Vec::new());
        f.feed(b"ab\x1b[1mc\x1b_Ga=q,i=", &mut out, &mut cmds);
        f.feed(b"7;AAAA\x1b", &mut out, &mut cmds);
        f.feed(b"\\d", &mut out, &mut cmds);
        assert_eq!(out, b"ab\x1b[1mcd");
        assert_eq!(cmds, vec![b"a=q,i=7;AAAA".to_vec()]);
    }

    #[test]
    fn query_is_answered_ok() {
        let mut g = Graphics::default();
        assert_eq!(g.apply(b"i=31,s=1,v=1,a=q,t=d,f=24;AAAA"), Some(b"\x1b_Gi=31;OK\x1b\\".to_vec()));
    }

    #[test]
    fn chunked_rgba_transmission_is_joined() {
        use base64::Engine as _;
        let data = base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3, 4, 5, 6, 7, 8]);
        let (a, b) = data.split_at(4);
        let mut g = Graphics::default();
        assert_eq!(g.apply(format!("a=T,U=1,q=2,f=32,s=2,v=1,i=9,c=4,r=2,m=1;{a}").as_bytes()), None);
        assert_eq!(g.apply(format!("m=0;{b}").as_bytes()), None);
        let (picture, size) = g.picture(9).expect("stored");
        assert_eq!((picture.width, picture.height, size), (2, 1, (4, 2)));
    }

    #[test]
    fn placeholders_group_into_one_rect_per_image() {
        let mut parser = vt100::Parser::new(5, 20, 0);
        // 画像 ID 0x0a0b0c、2 行 × 3 列。2 行目は 2 セル目から結合文字を省く
        let d = |i: usize| char::from_u32(DIACRITICS[i]).unwrap();
        let mut s = String::from("\x1b[38;2;10;11;12m\x1b[2;3H");
        for c in 0..3 {
            s.push(PLACEHOLDER);
            s.push(d(0));
            s.push(d(c));
        }
        s.push_str("\x1b[3;3H");
        s.push(PLACEHOLDER);
        s.push(d(1));
        s.push(d(0));
        s.push(PLACEHOLDER);
        s.push(PLACEHOLDER);
        parser.process(s.as_bytes());
        assert_eq!(
            visible(parser.screen()),
            vec![Visible { id: 0x0a0b0c, row: 1, col: 2, image_row: 0, image_col: 0, rows: 2, cols: 3 }]
        );
    }

    #[test]
    fn cell_pixels_prefer_the_cell_report() {
        assert_eq!(parse_cell_pixels("\x1b[6;20;10t\x1b[4;400;800t\x1b[?61c", (80, 20)), Some((10, 20)));
        assert_eq!(parse_cell_pixels("\x1b[4;400;800t\x1b[?61c", (80, 20)), Some((10, 20)));
        assert_eq!(parse_cell_pixels("\x1b[?61c", (80, 20)), None);
    }
}

