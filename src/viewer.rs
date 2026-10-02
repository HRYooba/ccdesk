//! 画像ビューアー。`ccdesk view <path>` が開く、ペインの上に浮かぶ窓（[`Overlay`]）。
//!
//! **持つのは画像・カメラ（倍率・中心）・窓の位置と大きさ。** いつ開くかは
//! [`crate::app`]、Sixel への書き出しは [`crate::graphics`] が持つ。
//!
//! 座標は 3 種類ある: 画像の画素・ビューポートの画素（内寸のセル × セルの画素寸法）・
//! 端末のセル。**カメラは画像の画素で持つ**ので、スロットの大きさが変わっても
//! 見ている場所は動かない（倍率は「全体が収まる倍率」に対する比で持つ）
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ratatui::layout::Rect;

use crate::graphics::Picture;

/// ホイール 1 段の倍率
const STEP: f64 = 1.1;

/// 1 フレームに効かせるホイールの段数。
/// Windows ではホイールを素早く回すと、1 回の読み取りに 10〜20 段ぶんがまとめて届く。
/// 上限が無いとその 1 回で最大・最小まで飛ぶ
pub(crate) const STEPS_PER_FRAME: u8 = 3;

/// 拡大の上限（画像 1 画素をビューポートの何画素にするか）。
/// 全体を収める倍率がこれを超える小さな画像では、収めた大きさが上限になる
const MAX_SCALE: f64 = 32.0;

/// この倍率以上は最近傍で描く（拡大したときに画素の境目を見せる）
const NEAREST_FROM: f64 = 2.0;

/// 縮小用の段（半分ずつ縮めた写し）を、長辺がこの画素数を切るまで作る
const MIN_LEVEL_SIDE: u32 = 16;

/// 開いた画像。**縮小用の段を持つ**: 縮小表示を元画像からの補間で作ると、
/// 間引かれた画素がちらつく（拡大・移動のたびに違う画素を拾う）
pub(crate) struct Image {
    pub(crate) path: PathBuf,
    /// `levels[0]` が元画像。以降は 1 つ前の半分
    levels: Vec<Picture>,
}

impl Image {
    pub(crate) fn open(path: &Path) -> Result<Self, String> {
        let decoded = image::ImageReader::open(path)
            .and_then(|reader| reader.with_guessed_format())
            .map_err(|e| format!("could not open {}: {e}", path.display()))?
            .decode()
            .map_err(|e| format!("could not decode {}: {e}", path.display()))?;
        let rgba = decoded.to_rgba8();
        Ok(Self::from_picture(
            path.to_path_buf(),
            Picture {
                width: rgba.width(),
                height: rgba.height(),
                rgba: rgba.into_raw(),
            },
        ))
    }

    pub(crate) fn from_picture(path: PathBuf, picture: Picture) -> Self {
        let mut levels = vec![picture];
        while let Some(last) = levels.last()
            && last.width.max(last.height) >= MIN_LEVEL_SIDE * 2
        {
            let half = halve(last);
            levels.push(half);
        }
        Self { path, levels }
    }

    pub(crate) fn size(&self) -> (f64, f64) {
        let p = &self.levels[0];
        (f64::from(p.width), f64::from(p.height))
    }

    /// 枠の見出しに出す名前
    pub(crate) fn name(&self) -> String {
        self.path
            .file_name()
            .map_or_else(|| self.path.display().to_string(), |n| n.to_string_lossy().to_string())
    }
}

/// 2×2 画素の平均で半分にする（奇数の端は 1 画素を繰り返す）
fn halve(p: &Picture) -> Picture {
    let (w, h) = ((p.width / 2).max(1), (p.height / 2).max(1));
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    let at = |x: u32, y: u32| ((y.min(p.height - 1) * p.width + x.min(p.width - 1)) * 4) as usize;
    for y in 0..h {
        for x in 0..w {
            let taps = [at(2 * x, 2 * y), at(2 * x + 1, 2 * y), at(2 * x, 2 * y + 1), at(2 * x + 1, 2 * y + 1)];
            let out = ((y * w + x) * 4) as usize;
            for c in 0..4 {
                let sum: u32 = taps.iter().map(|&i| u32::from(p.rgba[i + c])).sum();
                rgba[out + c] = ((sum + 2) / 4) as u8;
            }
        }
    }
    Picture { width: w, height: h, rgba }
}

/// ビューポートの大きさ（画素）
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Viewport {
    pub(crate) w: f64,
    pub(crate) h: f64,
}

impl Viewport {
    /// 内寸のセル矩形とセルの画素寸法から
    pub(crate) fn of(area: Rect, (cw, ch): (u16, u16)) -> Self {
        Self {
            w: f64::from(area.width) * f64::from(cw),
            h: f64::from(area.height) * f64::from(ch),
        }
    }
}

/// スロット 1 枚ぶんのビューアー
pub(crate) struct ImageView {
    pub(crate) image: Arc<Image>,
    /// 全体が収まる倍率に対する比（1 ＝ 全体が収まる）
    zoom: f64,
    /// ビューポートの中心が指す画像の画素
    center: (f64, f64),
    /// このフレームで残っているホイールの段数（[`STEPS_PER_FRAME`]）
    wheel_left: u8,
}

impl ImageView {
    pub(crate) fn new(image: Image) -> Self {
        let (w, h) = image.size();
        Self {
            image: Arc::new(image),
            zoom: 1.0,
            center: (w / 2.0, h / 2.0),
            wheel_left: STEPS_PER_FRAME,
        }
    }

    /// カメラ `(倍率, 中心)`（検査用）
    #[cfg(test)]
    pub(crate) fn camera(&self) -> (f64, (f64, f64)) {
        (self.zoom, self.center)
    }

    fn fit(&self, vp: Viewport) -> f64 {
        let (w, h) = self.image.size();
        (vp.w / w).min(vp.h / h)
    }

    /// 画像 1 画素がビューポートの何画素になるか
    pub(crate) fn scale(&self, vp: Viewport) -> f64 {
        self.fit(vp) * self.zoom
    }

    fn max_zoom(&self, vp: Viewport) -> f64 {
        (MAX_SCALE / self.fit(vp)).max(1.0)
    }

    /// フレームを描くたびに呼ぶ（ホイールの段数の上限を数え直す）
    pub(crate) fn begin_frame(&mut self) {
        self.wheel_left = STEPS_PER_FRAME;
    }

    /// ホイール 1 段。`at` はビューポート内のカーソル位置（画素）で、
    /// **その下にある画像の点を動かさずに**拡大・縮小する
    pub(crate) fn wheel(&mut self, zoom_in: bool, at: (f64, f64), vp: Viewport) {
        if self.wheel_left == 0 {
            return;
        }
        self.wheel_left -= 1;
        let before = self.scale(vp);
        let factor = if zoom_in { STEP } else { 1.0 / STEP };
        self.zoom = (self.zoom * factor).clamp(1.0, self.max_zoom(vp));
        let after = self.scale(vp);
        let (dx, dy) = (at.0 - vp.w / 2.0, at.1 - vp.h / 2.0);
        // カーソルの下の画像の点 ＝ center + d / scale。倍率を変えてもこれを保つ
        self.center.0 += dx / before - dx / after;
        self.center.1 += dy / before - dy / after;
        self.clamp(vp);
    }

    /// ドラッグで動かす。`delta` はカーソルが動いた量（ビューポートの画素）
    pub(crate) fn pan(&mut self, delta: (f64, f64), vp: Viewport) {
        let s = self.scale(vp);
        self.center.0 -= delta.0 / s;
        self.center.1 -= delta.1 / s;
        self.clamp(vp);
    }

    /// 画像が見えなくなるところまでは動かさない。ビューポートより小さい向きは
    /// 中央に置く（端に寄せる理由が無い）
    fn clamp(&mut self, vp: Viewport) {
        self.zoom = self.zoom.clamp(1.0, self.max_zoom(vp));
        let s = self.scale(vp);
        let (w, h) = self.image.size();
        let axis = |c: f64, size: f64, view: f64| {
            let half = view / s / 2.0;
            if half * 2.0 >= size {
                size / 2.0
            } else {
                c.clamp(half, size - half)
            }
        };
        self.center = (axis(self.center.0, w, vp.w), axis(self.center.1, h, vp.h));
    }

    /// このフレームで描くもの。`area` は内寸（端末の絶対セル座標）
    pub(crate) fn shot(&mut self, area: Rect, cell: (u16, u16), background: [u8; 3]) -> Shot {
        self.clamp(Viewport::of(area, cell));
        Shot {
            image: Arc::clone(&self.image),
            area,
            zoom: self.zoom,
            center: self.center,
            background,
        }
    }
}

/// 窓の最小の外寸（枠を含む）。これより小さくすると掴む場所が無くなる
const MIN_COLS: u16 = 16;
const MIN_ROWS: u16 = 6;

/// 浮かぶ窓としてのビューアー。**スロットの配置には入らない**: セッションの
/// 並びを崩さず、その上に重ねる。位置と大きさはペイン（右側の矩形）に対する比で
/// 持つので、端末やサイドバーの幅が変わっても同じ辺りに留まる
pub(crate) struct Overlay {
    pub(crate) view: ImageView,
    /// ペインに対する比 `(x, y, 幅, 高さ)`
    place: (f64, f64, f64, f64),
    /// 押されてから他を押すまで。`Esc` で閉じるのはこの間だけ
    /// （それ以外の `Esc` はセッションの agent のもの）
    pub(crate) focused: bool,
}

/// 窓のどこを押したか
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Hit {
    Close,
    /// 上辺（見出し）を掴んで動かす
    Move,
    /// 左・右・下の辺（と下の角）を掴んで大きさを変える
    Resize { left: bool, right: bool, bottom: bool },
    /// 内側（ホイールで拡大縮小・ドラッグで画像を動かす）
    Inside,
}

impl Overlay {
    /// 初めて開くときの置き場所 ＝ `slot`（呼んだセッションのスロット）の**右上**、
    /// スロットの枠の内側。**全部は隠さない**: 幅も高さもスロットの半分までに留め、
    /// 画像の縦横比に詰める（claude の出力は左寄せなので、右上がいちばん読む邪魔をしない）
    pub(crate) fn new(view: ImageView, pane: Rect, slot: Rect, cell: (u16, u16)) -> Self {
        let (cw, ch) = (f64::from(cell.0), f64::from(cell.1));
        // スロットの枠の内側。ここに窓の外寸を収める
        let room = Rect::new(slot.x + 1, slot.y + 1, slot.width.saturating_sub(2), slot.height.saturating_sub(2));
        // 窓の枠の 2 桁・2 行を除いた内寸の上限
        let max_cols = (f64::from(room.width) * 0.5 - 2.0).max(1.0);
        let max_rows = (f64::from(room.height) * 0.5 - 2.0).max(1.0);
        let (iw, ih) = view.image.size();
        let fit = (max_cols * cw / iw).min(max_rows * ch / ih);
        let cols = ((iw * fit / cw).ceil() as u16 + 2).max(MIN_COLS).min(pane.width);
        let rows = ((ih * fit / ch).ceil() as u16 + 2).max(MIN_ROWS).min(pane.height);
        let x = room.right().saturating_sub(cols).max(pane.x);
        let y = room.y.min(pane.bottom().saturating_sub(rows));
        let mut overlay = Self {
            view,
            place: (0.0, 0.0, 0.0, 0.0),
            focused: false,
        };
        overlay.set_rect(Rect::new(x, y, cols, rows), pane);
        overlay
    }

    /// 今の外寸（端末の絶対セル座標）。ペインの内側へ収め、最小の大きさを守る
    pub(crate) fn rect(&self, pane: Rect) -> Rect {
        let (x, y, w, h) = self.place;
        let size = |frac: f64, total: u16, min: u16| ((frac * f64::from(total)).round() as u16).clamp(min.min(total), total);
        let (width, height) = (size(w, pane.width, MIN_COLS), size(h, pane.height, MIN_ROWS));
        let at = |frac: f64, total: u16, size: u16| ((frac * f64::from(total)).round() as u16).min(total - size);
        Rect::new(pane.x + at(x, pane.width, width), pane.y + at(y, pane.height, height), width, height)
    }

    /// 外寸を置き直す（ペインに対する比へ戻して持つ）
    pub(crate) fn set_rect(&mut self, rect: Rect, pane: Rect) {
        if pane.width == 0 || pane.height == 0 {
            return;
        }
        let (pw, ph) = (f64::from(pane.width), f64::from(pane.height));
        self.place = (
            f64::from(rect.x.saturating_sub(pane.x)) / pw,
            f64::from(rect.y.saturating_sub(pane.y)) / ph,
            f64::from(rect.width) / pw,
            f64::from(rect.height) / ph,
        );
    }

    /// 内寸（枠の内側 ＝ Sixel を置く範囲）
    pub(crate) fn inner(&self, pane: Rect) -> Rect {
        let r = self.rect(pane);
        Rect::new(r.x + 1, r.y + 1, r.width.saturating_sub(2), r.height.saturating_sub(2))
    }

    /// `(column, row)` が窓のどこか（窓の外なら `None`）
    pub(crate) fn hit(&self, pane: Rect, column: u16, row: u16) -> Option<Hit> {
        let r = self.rect(pane);
        if !r.contains(ratatui::layout::Position::new(column, row)) {
            return None;
        }
        if crate::ui::close_zone(r).is_some_and(|(cols, at)| row == at && cols.contains(&column)) {
            return Some(Hit::Close);
        }
        let (left, right, bottom) = (column == r.x, column == r.right() - 1, row == r.bottom() - 1);
        if row == r.y {
            return Some(Hit::Move);
        }
        if left || right || bottom {
            return Some(Hit::Resize { left, right, bottom });
        }
        Some(Hit::Inside)
    }
}

/// 辺や見出しを掴んで `(dx, dy)` セル動かした後の外寸。`start` は掴んだときの外寸。
/// **ペインの外へは出さず、最小の大きさより小さくしない**（掴む場所が消えない）
pub(crate) fn dragged(start: Rect, hit: Hit, (dx, dy): (i32, i32), pane: Rect) -> Rect {
    let (px0, py0) = (i32::from(pane.x), i32::from(pane.y));
    let (px1, py1) = (px0 + i32::from(pane.width), py0 + i32::from(pane.height));
    let (mut x0, mut y0) = (i32::from(start.x), i32::from(start.y));
    let (mut x1, mut y1) = (x0 + i32::from(start.width), y0 + i32::from(start.height));
    let (min_w, min_h) = (i32::from(MIN_COLS).min(px1 - px0), i32::from(MIN_ROWS).min(py1 - py0));
    match hit {
        Hit::Move => {
            let w = x1 - x0;
            let h = y1 - y0;
            x0 = (x0 + dx).clamp(px0, px1 - w);
            y0 = (y0 + dy).clamp(py0, py1 - h);
            x1 = x0 + w;
            y1 = y0 + h;
        }
        Hit::Resize { left, right, bottom } => {
            if left {
                x0 = (x0 + dx).clamp(px0, x1 - min_w);
            }
            if right {
                x1 = (x1 + dx).clamp(x0 + min_w, px1);
            }
            if bottom {
                y1 = (y1 + dy).clamp(y0 + min_h, py1);
            }
        }
        Hit::Close | Hit::Inside => {}
    }
    Rect::new(x0 as u16, y0 as u16, (x1 - x0) as u16, (y1 - y0) as u16)
}

/// 1 フレームぶんのビューアーの絵（カメラの写し）
#[derive(Clone)]
pub(crate) struct Shot {
    image: Arc<Image>,
    area: Rect,
    zoom: f64,
    center: (f64, f64),
    /// 画像の載らない余白の色
    background: [u8; 3],
}

/// 同じ絵かどうか（[`crate::graphics::Painter`] が描き直しを省く判断）
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct ShotKey {
    image: usize,
    area: (u16, u16, u16, u16),
    zoom: u64,
    center: (u64, u64),
    background: [u8; 3],
}

/// 描いた絵。`row` / `col` は Sixel を置く端末のセル（左上）
pub(crate) struct Rendered {
    pub(crate) row: u16,
    pub(crate) col: u16,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) rgba: Vec<u8>,
}

impl Shot {
    pub(crate) fn key(&self) -> ShotKey {
        ShotKey {
            image: Arc::as_ptr(&self.image) as usize,
            area: (self.area.x, self.area.y, self.area.width, self.area.height),
            zoom: self.zoom.to_bits(),
            center: (self.center.0.to_bits(), self.center.1.to_bits()),
            background: self.background,
        }
    }

    /// 内寸の全面を描く。**余白も不透明に塗る**: ビューアーはセッションの上に
    /// 浮かぶので、透かすと下のセッションの画像（Sixel）が覗く
    pub(crate) fn render(&self, (cw, ch): (u16, u16)) -> Option<Rendered> {
        let vp = Viewport::of(self.area, (cw, ch));
        let (width, height) = (vp.w as u32, vp.h as u32);
        if width == 0 || height == 0 {
            return None;
        }
        let (iw, ih) = self.image.size();
        let s = (vp.w / iw).min(vp.h / ih) * self.zoom;
        // 画像の左上がビューポートのどこに来るか（画素）
        let x0 = vp.w / 2.0 - self.center.0 * s;
        let y0 = vp.h / 2.0 - self.center.1 * s;
        let level = self.level(s);
        let picture = &self.image.levels[level];
        // 段の画素へ写す比（段は丸めて半分にしているので、元の大きさとの比で取る）
        let (lx, ly) = (f64::from(picture.width) / iw, f64::from(picture.height) / ih);
        let nearest = s >= NEAREST_FROM;
        let [r, g, b] = self.background;
        let mut rgba: Vec<u8> = [r, g, b, 255].repeat((width * height) as usize);
        for oy in 0..height {
            let v = (f64::from(oy) + 0.5 - y0) / s;
            if v < 0.0 || v >= ih {
                continue;
            }
            for ox in 0..width {
                let u = (f64::from(ox) + 0.5 - x0) / s;
                if u < 0.0 || u >= iw {
                    continue;
                }
                let out = ((oy * width + ox) * 4) as usize;
                let pixel = if nearest {
                    sample_nearest(picture, u * lx, v * ly)
                } else {
                    sample_bilinear(picture, u * lx, v * ly)
                };
                // 画像そのものの透明は背景の上に合成する（透かすと下が覗く）
                let a = u32::from(pixel[3]);
                for c in 0..3 {
                    let bg = u32::from(self.background[c]);
                    rgba[out + c] = ((u32::from(pixel[c]) * a + bg * (255 - a) + 127) / 255) as u8;
                }
            }
        }
        Some(Rendered {
            row: self.area.y,
            col: self.area.x,
            width,
            height,
            rgba,
        })
    }

    /// 倍率 `s` の縮小に使う段。**段の上での倍率が 1/2 を下回らない**いちばん小さい段
    fn level(&self, s: f64) -> usize {
        let mut level = 0;
        let mut at = s;
        while at * 2.0 <= 1.0 && level + 1 < self.image.levels.len() {
            at *= 2.0;
            level += 1;
        }
        level
    }
}

fn texel(p: &Picture, x: i64, y: i64) -> [u8; 4] {
    let x = x.clamp(0, i64::from(p.width) - 1) as usize;
    let y = y.clamp(0, i64::from(p.height) - 1) as usize;
    let i = (y * p.width as usize + x) * 4;
    [p.rgba[i], p.rgba[i + 1], p.rgba[i + 2], p.rgba[i + 3]]
}

fn sample_nearest(p: &Picture, u: f64, v: f64) -> [u8; 4] {
    texel(p, u.floor() as i64, v.floor() as i64)
}

/// 画素の中心を (x + 0.5, y + 0.5) とする双線形補間
fn sample_bilinear(p: &Picture, u: f64, v: f64) -> [u8; 4] {
    let (x, y) = (u - 0.5, v - 0.5);
    let (x0, y0) = (x.floor(), y.floor());
    let (fx, fy) = (x - x0, y - y0);
    let (x0, y0) = (x0 as i64, y0 as i64);
    let taps = [
        (texel(p, x0, y0), (1.0 - fx) * (1.0 - fy)),
        (texel(p, x0 + 1, y0), fx * (1.0 - fy)),
        (texel(p, x0, y0 + 1), (1.0 - fx) * fy),
        (texel(p, x0 + 1, y0 + 1), fx * fy),
    ];
    let mut out = [0u8; 4];
    for (c, slot) in out.iter_mut().enumerate() {
        let sum: f64 = taps.iter().map(|(t, w)| f64::from(t[c]) * w).sum();
        *slot = sum.round().clamp(0.0, 255.0) as u8;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 横長の単色画像（200×100）
    fn view() -> ImageView {
        let picture = Picture {
            width: 200,
            height: 100,
            rgba: [200, 200, 200, 255].repeat(200 * 100),
        };
        ImageView::new(Image::from_picture(PathBuf::from("C:/x/wide.png"), picture))
    }

    const VP: Viewport = Viewport { w: 400.0, h: 400.0 };

    /// カーソルの下にある画像の点（画素）
    fn under(v: &ImageView, at: (f64, f64)) -> (f64, f64) {
        let s = v.scale(VP);
        (v.center.0 + (at.0 - VP.w / 2.0) / s, v.center.1 + (at.1 - VP.h / 2.0) / s)
    }

    #[test]
    fn zooming_keeps_the_point_under_the_cursor() {
        let mut v = view();
        // 先に寄せておく（全体表示の近くでは、収まっている向きが中央へ戻される）
        v.zoom = 4.0;
        let at = (300.0, 250.0);
        for _ in 0..3 {
            v.begin_frame();
            let before = under(&v, at);
            v.wheel(true, at, VP);
            let after = under(&v, at);
            assert!((before.0 - after.0).abs() < 1e-9 && (before.1 - after.1).abs() < 1e-9, "{before:?} -> {after:?}");
        }
        assert!(v.zoom > 4.0);
    }

    /// **1 フレームに効く段数には上限がある**（まとめて届いた段で端まで飛ばない）
    #[test]
    fn a_burst_of_wheel_steps_is_capped_per_frame() {
        let mut v = view();
        v.begin_frame();
        for _ in 0..20 {
            v.wheel(true, (200.0, 200.0), VP);
        }
        let expected = STEP.powi(i32::from(STEPS_PER_FRAME));
        assert!((v.zoom - expected).abs() < 1e-9, "zoom {} after a burst", v.zoom);
        v.begin_frame();
        v.wheel(true, (200.0, 200.0), VP);
        assert!(v.zoom > expected, "the next frame did not take a step");
    }

    #[test]
    fn zoom_stops_at_fit_and_at_the_largest_scale() {
        let mut v = view();
        v.begin_frame();
        v.wheel(false, (200.0, 200.0), VP);
        assert_eq!(v.zoom, 1.0, "zoomed out past the whole image");
        for _ in 0..200 {
            v.begin_frame();
            v.wheel(true, (200.0, 200.0), VP);
        }
        assert!((v.scale(VP) - MAX_SCALE).abs() < 1e-9, "scale {}", v.scale(VP));
    }

    /// 収まっている向きは中央から動かない。はみ出している向きは端までしか動かない
    #[test]
    fn panning_stops_at_the_edges() {
        let mut v = view();
        v.pan((1000.0, 1000.0), VP);
        assert_eq!(v.center, (100.0, 50.0), "a fitted image moved");
        for _ in 0..30 {
            v.begin_frame();
            v.wheel(true, (200.0, 200.0), VP);
        }
        v.pan((1.0e6, 1.0e6), VP);
        let half = VP.w / v.scale(VP) / 2.0;
        assert!((v.center.0 - half).abs() < 1e-9, "x {} not at the left edge {half}", v.center.0);
        assert!((v.center.1 - half).abs() < 1e-9, "y {} not at the top edge", v.center.1);
    }

    /// 内寸を全面描く。画像の載らない余白は背景色で不透明に塗る
    /// （浮かぶ窓の下にあるセッションの画像を覗かせない）
    #[test]
    fn a_render_covers_the_whole_inside_with_the_background_around_the_image() {
        let mut v = view();
        let area = Rect::new(10, 5, 40, 20); // 400×400 px（10×20 のセル）
        let r = v.shot(area, (10, 20), [1, 2, 3]).render((10, 20)).expect("nothing drawn");
        assert_eq!((r.col, r.row, r.width, r.height), (10, 5, 400, 400));
        let at = |x: usize, y: usize| &r.rgba[(y * 400 + x) * 4..(y * 400 + x) * 4 + 4];
        // 画像は 400×200 で縦 100..300 px に載る
        assert_eq!(at(200, 50), [1, 2, 3, 255], "the margin is not the background");
        assert_eq!(at(200, 200), [200, 200, 200, 255], "the image is missing");
        assert!(r.rgba.chunks(4).all(|p| p[3] == 255), "part of the render is transparent");
    }

    fn overlay() -> Overlay {
        Overlay::new(view(), PANE, PANE, CELL)
    }

    const PANE: Rect = Rect { x: 30, y: 0, width: 100, height: 50 };
    const CELL: (u16, u16) = (10, 20);

    /// 初めの窓はスロットの枠の内側の**右上**に、画像の縦横比に詰めて出る。
    /// 200×100 の画像を内寸 47 桁（470 px）に収めると高さ 235 px ＝ 12 行 + 枠
    #[test]
    fn a_new_overlay_sits_in_the_top_right_of_the_slot_trimmed_to_the_image() {
        assert_eq!(overlay().rect(PANE), Rect::new(80, 1, 49, 14));
    }

    /// 呼んだスロットが左列なら、その左列の右上（右列には出ない）
    #[test]
    fn a_new_overlay_stays_inside_the_callers_slot() {
        let left_slot = Rect::new(30, 0, 50, 50);
        let r = Overlay::new(view(), PANE, left_slot, CELL).rect(PANE);
        assert_eq!((r.right(), r.y), (left_slot.right() - 1, 1), "{r:?} is not at the top-right");
        assert!(r.x > left_slot.x && r.width <= left_slot.width / 2, "{r:?} covers too much");
    }

    #[test]
    fn the_overlay_tells_its_title_edges_and_inside_apart() {
        let o = overlay();
        let r = o.rect(PANE);
        assert_eq!(o.hit(PANE, r.x + 3, r.y), Some(Hit::Move));
        assert_eq!(o.hit(PANE, r.x, r.y + 3), Some(Hit::Resize { left: true, right: false, bottom: false }));
        assert_eq!(
            o.hit(PANE, r.right() - 1, r.bottom() - 1),
            Some(Hit::Resize { left: false, right: true, bottom: true })
        );
        assert_eq!(o.hit(PANE, r.x + 3, r.y + 3), Some(Hit::Inside));
        let (cols, row) = crate::ui::close_zone(r).unwrap();
        assert_eq!(o.hit(PANE, *cols.end(), row), Some(Hit::Close));
        assert_eq!(o.hit(PANE, r.x - 1, r.y + 3), None);
    }

    /// 動かしても大きさを変えても、ペインの外へ出ず最小より小さくならない
    #[test]
    fn dragging_the_frame_stays_inside_the_pane_and_above_the_minimum() {
        let start = Rect::new(80, 5, 40, 30);
        assert_eq!(dragged(start, Hit::Move, (-5, 2), PANE), Rect::new(75, 7, 40, 30));
        assert_eq!(dragged(start, Hit::Move, (100, 100), PANE), Rect::new(90, 20, 40, 30));
        let grow = Hit::Resize { left: true, right: false, bottom: true };
        assert_eq!(dragged(start, grow, (-10, 4), PANE), Rect::new(70, 5, 50, 34));
        let shrink = Hit::Resize { left: false, right: true, bottom: true };
        let r = dragged(start, shrink, (-100, -100), PANE);
        assert_eq!((r.width, r.height), (MIN_COLS, MIN_ROWS));
    }

    /// 位置と大きさはペインに対する比で残る（端末が広がれば一緒に広がる）
    #[test]
    fn the_overlay_keeps_its_place_relative_to_the_pane() {
        let mut o = overlay();
        o.set_rect(Rect::new(40, 10, 50, 25), PANE);
        assert_eq!(o.rect(PANE), Rect::new(40, 10, 50, 25));
        let wider = Rect::new(30, 0, 200, 100);
        assert_eq!(o.rect(wider), Rect::new(50, 20, 100, 50));
    }

    /// 縮小は半分ずつの段から拾う（元画像を間引かない）
    #[test]
    fn shrinking_reads_from_a_smaller_level() {
        let v = view();
        let shot = Shot { image: Arc::clone(&v.image), area: Rect::new(0, 0, 1, 1), zoom: 1.0, center: (0.0, 0.0), background: [0; 3] };
        assert_eq!(shot.level(1.0), 0);
        assert_eq!(shot.level(0.6), 0);
        assert_eq!(shot.level(0.5), 1);
        assert_eq!(shot.level(0.2), 2);
        // 段は長辺が下限を切る手前で止まる
        assert_eq!(shot.level(1.0e-6), v.image.levels.len() - 1);
        assert!(v.image.levels.last().unwrap().width >= MIN_LEVEL_SIDE);
    }

    #[test]
    fn halving_averages_two_by_two_blocks() {
        let p = Picture {
            width: 2,
            height: 2,
            rgba: [[0, 0, 0, 255], [100, 0, 0, 255], [0, 200, 0, 255], [100, 200, 0, 255]].concat(),
        };
        let h = halve(&p);
        assert_eq!((h.width, h.height, h.rgba), (1, 1, vec![50, 100, 0, 255]));
    }
}
