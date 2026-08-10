//! ペインの配置。**スロットの「形」だけ**を持ち、中身（どのセッションを映すか）は持たない。
//!
//! **tmux / zellij の split は持ち込まない。** あちらのペインは「プロセスの入れ物」で、
//! 作る＝新しいプロセスが生まれ、閉じる＝プロセスが死ぬ。ccdesk のペインは
//! **表示スロット**で、プロセス（[`crate::session::Session`]）は分割と無関係に
//! 裏で走り続ける。だから「分割する」という操作には意味が無く、
//! **「スロットを何枚どう並べるか」を選ぶ**形にしてある。
//!
//! その結果、分割の方向・順序・入れ子といった状態が 1 つも要らない:
//! 配置は [`Layout`] の 8 値と十字の位置（[`Split`]）だけで完全に決まる。
use ratatui::layout::Rect;

/// 2×2 グリッド上の矩形 ＝ スロット 1 枚が占める範囲。
/// `col` / `row` は 0 か 1、`cols` / `rows` は 1 か 2
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Span {
    col: u16,
    row: u16,
    cols: u16,
    rows: u16,
}

impl Span {
    const fn new(col: u16, row: u16, cols: u16, rows: u16) -> Self {
        Self {
            col,
            row,
            cols,
            rows,
        }
    }
}

/// 十字の位置（百分率）。境界ドラッグで動く。
///
/// **1 本しか持たない**のが要点で、これが「1 列に 3 枚以上並べられない」の正体。
/// 最大 4 スロットではその形を作れないので、持たせても使い道が無い
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Split {
    /// 縦の区切り（左列が占める割合）
    pub(crate) v: u16,
    /// 横の区切り（上行が占める割合）
    pub(crate) h: u16,
}

impl Default for Split {
    fn default() -> Self {
        Self { v: 50, h: 50 }
    }
}

impl Split {
    /// 端へ寄せ切れない下限・上限（片側が潰れると掴み直せなくなる）
    const MIN: u16 = 15;
    const MAX: u16 = 85;

    pub(crate) fn clamped(self) -> Self {
        Self {
            v: self.v.clamp(Self::MIN, Self::MAX),
            h: self.h.clamp(Self::MIN, Self::MAX),
        }
    }
}

/// スロットを割る軸（[`Layout::split_slot`]）。**割ってできた 2 枚のどちらを使うかは
/// 含まない**（それは掴んだ座標が決めることで、割り方の性質ではない）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Axis {
    /// 縦に割る ＝ 左右 2 枚になる
    Vertical,
    /// 横に割る ＝ 上下 2 枚になる
    Horizontal,
}

/// 移動の向き（`Alt+Shift+←→↑↓`）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Dir {
    Left,
    Right,
    Up,
    Down,
}

/// 選べる配置。**この 8 つが全部**。
///
/// 2×2 のセルを土台に、スロット = 隣接セルの矩形集合として表す
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum Layout {
    #[default]
    One,
    TwoColumns,
    TwoRows,
    ThreeTallLeft,
    ThreeTallRight,
    ThreeWideTop,
    ThreeWideBottom,
    Four,
}

impl Layout {
    /// 表示順（メニューの項目の並びもこれに従う）
    pub(crate) const ORDER: [Self; 8] = [
        Self::One,
        Self::TwoColumns,
        Self::TwoRows,
        Self::ThreeTallLeft,
        Self::ThreeTallRight,
        Self::ThreeWideTop,
        Self::ThreeWideBottom,
        Self::Four,
    ];

    /// スロット 1 枚に要る最小の外寸 `(rows, cols)`（枠を含む）。
    ///
    /// **New 画面が出せる最小サイズから来ている**（枠 2 行 + 内側 2 行 /
    /// 枠 2 桁 + 内側 4 桁）。この対応は
    /// [`crate::ui::new_view`] 側のテストが機械で確かめるので、
    /// 片方だけ変えれば落ちる
    pub(crate) const MIN_SLOT: (u16, u16) = (4, 6);

    /// **保存値（config.json）と画面表示の唯一の綴り**。
    /// 読み・書き・メニュー・現在値表示が別々に綴りを持つと、片方だけ変えたときに
    /// 保存値が読めなくなる（設定が黙って既定へ戻る）
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::One => "1 pane",
            Self::TwoColumns => "2 columns",
            Self::TwoRows => "2 rows",
            Self::ThreeTallLeft => "3 tall left",
            Self::ThreeTallRight => "3 tall right",
            Self::ThreeWideTop => "3 wide top",
            Self::ThreeWideBottom => "3 wide bottom",
            Self::Four => "4 grid",
        }
    }

    /// 保存値からの復元。未知の値は既定（1 枚）へ倒す
    pub(crate) fn parse(text: &str) -> Self {
        Self::ORDER
            .into_iter()
            .find(|l| l.as_str() == text)
            .unwrap_or_default()
    }

    /// スロットの占有範囲。**並びは読み順**（占有セルの左上を (row, col) で見た昇順）で、
    /// これがそのままスロット番号になる。方向つき移動が候補を 2 つ持つときに
    /// 「読み順で先」を採れるのは、この並びが保証されているため
    pub(crate) fn spans(self) -> &'static [Span] {
        // 各配置の占有範囲。`&[..]` を式のまま返すと一時値になるので const で置く
        const ONE: [Span; 1] = [Span::new(0, 0, 2, 2)];
        const TWO_COLUMNS: [Span; 2] = [Span::new(0, 0, 1, 2), Span::new(1, 0, 1, 2)];
        const TWO_ROWS: [Span; 2] = [Span::new(0, 0, 2, 1), Span::new(0, 1, 2, 1)];
        const TALL_LEFT: [Span; 3] = [
            Span::new(0, 0, 1, 2),
            Span::new(1, 0, 1, 1),
            Span::new(1, 1, 1, 1),
        ];
        const TALL_RIGHT: [Span; 3] = [
            Span::new(0, 0, 1, 1),
            Span::new(1, 0, 1, 2),
            Span::new(0, 1, 1, 1),
        ];
        const WIDE_TOP: [Span; 3] = [
            Span::new(0, 0, 2, 1),
            Span::new(0, 1, 1, 1),
            Span::new(1, 1, 1, 1),
        ];
        const WIDE_BOTTOM: [Span; 3] = [
            Span::new(0, 0, 1, 1),
            Span::new(1, 0, 1, 1),
            Span::new(0, 1, 2, 1),
        ];
        const FOUR: [Span; 4] = [
            Span::new(0, 0, 1, 1),
            Span::new(1, 0, 1, 1),
            Span::new(0, 1, 1, 1),
            Span::new(1, 1, 1, 1),
        ];
        match self {
            Self::One => &ONE,
            Self::TwoColumns => &TWO_COLUMNS,
            Self::TwoRows => &TWO_ROWS,
            Self::ThreeTallLeft => &TALL_LEFT,
            Self::ThreeTallRight => &TALL_RIGHT,
            Self::ThreeWideTop => &WIDE_TOP,
            Self::ThreeWideBottom => &WIDE_BOTTOM,
            Self::Four => &FOUR,
        }
    }

    /// スロットの枚数
    pub(crate) fn slots(self) -> usize {
        self.spans().len()
    }

    /// 各スロットの矩形（並びは [`Self::spans`] と同じ ＝ スロット番号順）
    pub(crate) fn rects(self, area: Rect, split: Split) -> Vec<Rect> {
        let split = split.clamped();
        let (w0, w1) = cut(area.width, split.v);
        let (h0, h1) = cut(area.height, split.h);
        self.spans()
            .iter()
            .map(|s| Rect {
                x: area.x + if s.col == 0 { 0 } else { w0 },
                y: area.y + if s.row == 0 { 0 } else { h0 },
                width: span_len(s.col, s.cols, area.width, w0, w1),
                height: span_len(s.row, s.rows, area.height, h0, h1),
            })
            .collect()
    }

    /// 十字の交点 `(縦の区切りの x, 横の区切りの y)`。その向きに区切りが無ければ `None`。
    ///
    /// **境界ドラッグの掴み代の正本**で、返すのは「右（下）側のスロットが始まる座標」。
    /// 掴み代はその 1 つ手前と合わせた 2 列（行）＝ 枠線が 2 本重なって見える幅
    pub(crate) fn cross(self, area: Rect, split: Split) -> (Option<u16>, Option<u16>) {
        let split = split.clamped();
        let has_v = self.spans().iter().any(|s| s.cols == 1);
        let has_h = self.spans().iter().any(|s| s.rows == 1);
        (
            has_v.then(|| area.x + cut(area.width, split.v).0),
            has_h.then(|| area.y + cut(area.height, split.h).0),
        )
    }

    /// その座標が十字の掴み代に乗っているか `(縦を動かせるか, 横を動かせるか)`。
    ///
    /// **座標 1 つでは足りない。** 3 分割では境界が「途中で消える」ため、
    /// 列（行）が合っているだけでは掴み代にならない。例えば `3 tall left` の
    /// 左スロットは全高で、その内側に横の境界は無い ＝ そこを押したら
    /// リサイズではなく claude へ届かなければいけない。
    ///
    /// なので**その半分にそもそも境界があるか**をセルの持ち主で確かめる:
    /// 縦の境界は「その行の左右が別のスロットか」、横の境界は
    /// 「その列の上下が別のスロットか」で決まる
    pub(crate) fn grab_at(self, area: Rect, split: Split, column: u16, row: u16) -> (bool, bool) {
        // 矩形の外（サイドバー・下部バー）は掴み代にしない
        if column < area.x
            || row < area.y
            || column >= area.x + area.width
            || row >= area.y + area.height
        {
            return (false, false);
        }
        let (vx, hy) = self.cross(area, split);
        // 境界の座標 `a` に対する掴み代は `a - 1` と `a` の 2 つ
        let near = |at: Option<u16>, v: u16| at.is_some_and(|a| v + 1 >= a && v <= a);
        // その座標が 2×2 のどちら側の半分にいるか（区切りが無い向きは常に 0 側）
        let half = |at: Option<u16>, v: u16| u16::from(at.is_some_and(|a| v >= a));
        let (c, r) = (half(vx, column), half(hy, row));
        let differs = |a: Option<usize>, b: Option<usize>| a != b;
        (
            near(vx, column) && differs(self.slot_at(0, r), self.slot_at(1, r)),
            near(hy, row) && differs(self.slot_at(c, 0), self.slot_at(c, 1)),
        )
    }

    /// その配置をこの矩形で出せるか（どのスロットも [`Self::MIN_SLOT`] を割らない）。
    /// **メニューで押せるかの判断がここ 1 つ**なので、押せるのに崩れる項目が出ない
    pub(crate) fn fits(self, area: Rect, split: Split) -> bool {
        self.rects(area, split)
            .iter()
            .all(|r| r.height >= Self::MIN_SLOT.0 && r.width >= Self::MIN_SLOT.1)
    }

    /// `from` から `dir` の向きにある隣のスロット（無ければ `None`）。
    ///
    /// **候補が 2 つあるとき（例: `3 wide top` の上段から下へ）は読み順で先を採る。**
    /// 覚えておいて往復を可換にする実装もあり得るが、状態が増えるうえ 4 枚しか
    /// 無い盤面では迷子にならないので、決定的な規則 1 つで済ませる
    pub(crate) fn neighbor(self, from: usize, dir: Dir) -> Option<usize> {
        let me = *self.spans().get(from)?;
        let cells: Vec<(u16, u16)> = match dir {
            Dir::Left => {
                if me.col == 0 {
                    return None;
                }
                (me.row..me.row + me.rows).map(|r| (me.col - 1, r)).collect()
            }
            Dir::Right => {
                if me.col + me.cols >= 2 {
                    return None;
                }
                (me.row..me.row + me.rows)
                    .map(|r| (me.col + me.cols, r))
                    .collect()
            }
            Dir::Up => {
                if me.row == 0 {
                    return None;
                }
                (me.col..me.col + me.cols).map(|c| (c, me.row - 1)).collect()
            }
            Dir::Down => {
                if me.row + me.rows >= 2 {
                    return None;
                }
                (me.col..me.col + me.cols)
                    .map(|c| (c, me.row + me.rows))
                    .collect()
            }
        };
        // spans が読み順なので、最小の添字がそのまま「読み順で先」
        cells
            .into_iter()
            .filter_map(|(c, r)| self.slot_at(c, r))
            .min()
    }

    /// そのセルを占めているスロット
    fn slot_at(self, col: u16, row: u16) -> Option<usize> {
        self.spans().iter().position(|s| {
            col >= s.col && col < s.col + s.cols && row >= s.row && row < s.row + s.rows
        })
    }

    /// スロット `at` を `axis` で 2 枚に割った配置（割れないなら `None`）。
    ///
    /// **対応表を持たずに導出する。** 割った結果の占有範囲の集合と一致する配置を
    /// [`Self::ORDER`] から引くので、`spans` を足し引きしても「どれへ成長するか」の
    /// 知識が古びない（手で書いた `One + 右 → 2 columns` の表は、
    /// 配置を 1 つ足した日に黙って嘘になる）。
    ///
    /// **割れるのは 2 セルぶんある向きだけ。** 1 セルまで割れたスロットはこれ以上
    /// 分けられない ＝ そこが 8 値の enum で表せる限界で、`None` がその境界を返す
    pub(crate) fn split_slot(self, at: usize, axis: Axis) -> Option<Grown> {
        let me = *self.spans().get(at)?;
        let (first, second) = match axis {
            Axis::Vertical if me.cols == 2 => (
                Span::new(me.col, me.row, 1, me.rows),
                Span::new(me.col + 1, me.row, 1, me.rows),
            ),
            Axis::Horizontal if me.rows == 2 => (
                Span::new(me.col, me.row, me.cols, 1),
                Span::new(me.col, me.row + 1, me.cols, 1),
            ),
            _ => return None,
        };
        // 割った後の占有範囲。**並びは作らない**（スロット番号は見つけた配置の
        // 読み順が決めるので、ここで順を決めると 2 つの正本ができる）
        let mut want: Vec<Span> = self.spans().to_vec();
        want.remove(at);
        want.extend([first, second]);
        let layout = Self::ORDER.into_iter().find(|l| same_cells(l.spans(), &want))?;
        let index = |s: Span| layout.spans().iter().position(|g| *g == s);
        // **割らなかったスロットの番号は動き得る**（読み順が変わるため。例:
        // `2 rows` の上を割ると、下段は 1 番から 2 番へずれる）。番号を
        // 振り直すのは占有範囲の一致だけで、呼び手が数え直さなくていい
        let mut moved = Vec::new();
        for (old, span) in self.spans().iter().enumerate() {
            if old == at {
                continue;
            }
            moved.push((old, index(*span)?));
        }
        Some(Grown {
            layout,
            moved,
            halves: [index(first)?, index(second)?],
        })
    }
}

/// [`Layout::split_slot`] の答え。**番号の振り直しまで含める**ので、
/// 呼び手は「割った後どのスロットが何番になったか」を自分で数え直さない
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Grown {
    /// 割った後の配置
    pub(crate) layout: Layout,
    /// 割らなかったスロットの `(旧番号, 新番号)`
    pub(crate) moved: Vec<(usize, usize)>,
    /// 割ってできた 2 枚 `[左または上, 右または下]`。
    /// **どちらへ落とすかは呼び手が座標で決める**
    pub(crate) halves: [usize; 2],
}

/// 占有範囲の集合が同じか。**並びは見ない**（スロット番号の付け方は配置側が持つ）。
/// 同じ配置に同一の範囲は 2 つと無いので、枚数の一致と包含で集合の一致になる
fn same_cells(a: &[Span], b: &[Span]) -> bool {
    a.len() == b.len() && a.iter().all(|s| b.contains(s))
}

/// 1 辺を十字で 2 つに割る。**どちらも 0 にしない**（0 幅のスロットは掴めない）
fn cut(total: u16, pct: u16) -> (u16, u16) {
    if total == 0 {
        return (0, 0);
    }
    let first = (u32::from(total) * u32::from(pct) / 100) as u16;
    let first = first.clamp(1, total.saturating_sub(1).max(1));
    (first, total - first)
}

/// スロット 1 枚が 1 辺で取る長さ。2 セルぶんなら全長、1 セルなら側の長さ
fn span_len(at: u16, len: u16, total: u16, first: u16, second: u16) -> u16 {
    if len == 2 {
        total
    } else if at == 0 {
        first
    } else {
        second
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect {
        x: 10,
        y: 3,
        width: 80,
        height: 40,
    };

    /// **保存値と表示の綴りは 1 つ**（往復できないと設定が黙って既定へ戻る）
    #[test]
    fn every_layout_survives_a_save_and_load_round_trip() {
        for layout in Layout::ORDER {
            assert_eq!(Layout::parse(layout.as_str()), layout, "{layout:?}");
        }
        assert_eq!(Layout::parse("something else"), Layout::One);
    }

    /// **スロットは重ならず、コンテナを余さず埋める。**
    /// 隙間が残る配置があると、そこに前のフレームの描き残りが出る
    #[test]
    fn slots_tile_the_container_without_gaps_or_overlap() {
        for layout in Layout::ORDER {
            let rects = layout.rects(AREA, Split::default());
            assert_eq!(rects.len(), layout.slots(), "{layout:?}");
            let mut covered = vec![0u8; (AREA.width as usize) * (AREA.height as usize)];
            for r in &rects {
                for y in r.y..r.y + r.height {
                    for x in r.x..r.x + r.width {
                        let i = (y - AREA.y) as usize * AREA.width as usize + (x - AREA.x) as usize;
                        covered[i] += 1;
                    }
                }
            }
            assert!(
                covered.iter().all(|n| *n == 1),
                "{layout:?}: the slots overlap or leave a gap"
            );
        }
    }

    /// **どの配置でも全スロットが相互に行き来できる。**
    /// 到達できないスロットがあると、キーボードだけの利用者がそこへ入れない
    #[test]
    fn every_slot_is_reachable_from_every_other_slot() {
        for layout in Layout::ORDER {
            let n = layout.slots();
            for start in 0..n {
                let mut seen = vec![false; n];
                let mut queue = vec![start];
                seen[start] = true;
                while let Some(at) = queue.pop() {
                    for dir in [Dir::Left, Dir::Right, Dir::Up, Dir::Down] {
                        if let Some(next) = layout.neighbor(at, dir)
                            && !seen[next]
                        {
                            seen[next] = true;
                            queue.push(next);
                        }
                    }
                }
                assert!(
                    seen.iter().all(|s| *s),
                    "{layout:?}: slot {start} cannot reach every other slot"
                );
            }
        }
    }

    /// 移動は必ず自分以外へ行く（同じスロットに留まる「動いたつもり」が無い）
    #[test]
    fn a_move_never_lands_on_the_slot_it_started_from() {
        for layout in Layout::ORDER {
            for from in 0..layout.slots() {
                for dir in [Dir::Left, Dir::Right, Dir::Up, Dir::Down] {
                    assert_ne!(layout.neighbor(from, dir), Some(from), "{layout:?} {dir:?}");
                }
            }
        }
    }

    /// 盤の端では行き先が無い（1 枚のときは 4 方向とも行き先が無い）
    #[test]
    fn the_edges_of_the_board_have_no_neighbour() {
        for dir in [Dir::Left, Dir::Right, Dir::Up, Dir::Down] {
            assert_eq!(Layout::One.neighbor(0, dir), None, "{dir:?}");
        }
        // 左右 2 枚は上下に行き先が無く、左右にだけ動ける
        assert_eq!(Layout::TwoColumns.neighbor(0, Dir::Up), None);
        assert_eq!(Layout::TwoColumns.neighbor(0, Dir::Right), Some(1));
        assert_eq!(Layout::TwoColumns.neighbor(1, Dir::Left), Some(0));
        assert_eq!(Layout::TwoColumns.neighbor(1, Dir::Right), None);
    }

    /// **候補が 2 つある向きは読み順で先を採る。**
    /// `3 wide top` の上段から下へ降りると、下段の左（＝添字の小さい方）に着く
    #[test]
    fn an_ambiguous_move_takes_the_first_slot_in_reading_order() {
        assert_eq!(Layout::ThreeWideTop.neighbor(0, Dir::Down), Some(1));
        assert_eq!(Layout::ThreeTallLeft.neighbor(0, Dir::Right), Some(1));
        assert_eq!(Layout::ThreeWideBottom.neighbor(2, Dir::Up), Some(0));
        assert_eq!(Layout::ThreeTallRight.neighbor(1, Dir::Left), Some(0));
    }

    /// 十字は端へ寄せ切れない（潰れたスロットは掴み直せない）
    #[test]
    fn the_cross_cannot_be_pushed_all_the_way_to_an_edge() {
        let extreme = Split { v: 0, h: 100 }.clamped();
        assert_eq!(extreme.v, Split::MIN);
        assert_eq!(extreme.h, Split::MAX);
        for r in Layout::Four.rects(AREA, Split { v: 0, h: 0 }) {
            assert!(r.width > 0 && r.height > 0);
        }
    }

    /// **掴み代の位置は描かれた境界と一致する。**
    /// `cross` が返す座標は「右（下）側のスロットが始まる位置」で、
    /// これがずれると見えている線と掴める場所が食い違う
    #[test]
    fn the_cross_sits_exactly_where_the_slots_meet() {
        let split = Split { v: 40, h: 70 };
        let (vx, hy) = Layout::Four.cross(AREA, split);
        let rects = Layout::Four.rects(AREA, split);
        // 4 分割の右上（添字 1）は縦の境界から始まり、左下（添字 2）は横の境界から始まる
        assert_eq!(vx, Some(rects[1].x));
        assert_eq!(hy, Some(rects[2].y));
        // 区切りが無い向きは掴み代も無い
        assert_eq!(Layout::One.cross(AREA, split), (None, None));
        assert_eq!(Layout::TwoColumns.cross(AREA, split).1, None);
        assert_eq!(Layout::TwoRows.cross(AREA, split).0, None);
    }

    /// **境界が無いところは掴めない。**
    ///
    /// `3 tall left` の左スロットは全高なので、その内側に横の境界は無い。
    /// 列（行）が合っているかだけで判定していた頃は、ここを押すと claude へ
    /// 届かずリサイズが始まっていた
    #[test]
    fn a_boundary_that_does_not_exist_there_cannot_be_grabbed() {
        let split = Split::default();
        for (layout, full) in [
            (Layout::ThreeTallLeft, 0usize),   // 左が全高
            (Layout::ThreeTallRight, 1),       // 右が全高
            (Layout::ThreeWideTop, 0),         // 上が全幅
            (Layout::ThreeWideBottom, 2),      // 下が全幅
        ] {
            let rect = layout.rects(AREA, split)[full];
            let (vx, hy) = layout.cross(AREA, split);
            // 全高スロットの内側で「横の境界の行」を押しても掴めない
            if let Some(hy) = hy
                && rect.height == AREA.height
            {
                let x = rect.x + rect.width / 2;
                assert_eq!(
                    layout.grab_at(AREA, split, x, hy),
                    (false, false),
                    "{layout:?}: grabbed a horizontal boundary inside a full-height slot"
                );
            }
            // 全幅スロットの内側で「縦の境界の列」を押しても掴めない
            if let Some(vx) = vx
                && rect.width == AREA.width
            {
                let y = rect.y + rect.height / 2;
                assert_eq!(
                    layout.grab_at(AREA, split, vx, y),
                    (false, false),
                    "{layout:?}: grabbed a vertical boundary inside a full-width slot"
                );
            }
        }
    }

    /// 矩形の外（サイドバー・下部バー）は掴み代にならない
    #[test]
    fn nothing_outside_the_container_is_a_grab_zone() {
        let split = Split::default();
        let (vx, hy) = Layout::Four.cross(AREA, split);
        let (vx, hy) = (vx.expect("no cross"), hy.expect("no cross"));
        for (column, row) in [
            (vx, AREA.y + AREA.height), // 下部バーの行
            (vx, AREA.y.saturating_sub(1)),
            (AREA.x.saturating_sub(1), hy), // サイドバーの列
            (AREA.x + AREA.width, hy),
        ] {
            assert_eq!(
                Layout::Four.grab_at(AREA, split, column, row),
                (false, false),
                "({column},{row}) outside the container was a grab zone"
            );
        }
    }

    /// 4 分割の交点は縦横の両方を掴む（十字の中心）
    #[test]
    fn the_intersection_takes_both_axes() {
        let split = Split::default();
        let (vx, hy) = Layout::Four.cross(AREA, split);
        assert_eq!(
            Layout::Four.grab_at(AREA, split, vx.unwrap(), hy.unwrap()),
            (true, true)
        );
    }

    /// 小さすぎる矩形では、枚数の多い配置が選べなくなる
    #[test]
    fn a_layout_stops_fitting_once_its_slots_would_be_too_small() {
        let split = Split::default();
        let roomy = Rect::new(0, 0, 80, 40);
        for layout in Layout::ORDER {
            assert!(layout.fits(roomy, split), "{layout:?} does not fit 80x40");
        }
        // 1 枚ぶんしか無い大きさ: 分割した配置はどれも入らない
        let tiny = Rect::new(0, 0, Layout::MIN_SLOT.1, Layout::MIN_SLOT.0);
        assert!(Layout::One.fits(tiny, split));
        for layout in Layout::ORDER.into_iter().filter(|l| l.slots() > 1) {
            assert!(!layout.fits(tiny, split), "{layout:?} fits a one-slot area");
        }
    }

    /// **割った先はどれも既定の配置になる。** 2 セルぶんある向きを割った結果は
    /// 必ず [`Layout::ORDER`] の中に在り、枚数はちょうど 1 枚増える
    #[test]
    fn splitting_a_two_cell_slot_always_lands_on_a_known_layout() {
        let mut splittable = 0;
        for layout in Layout::ORDER {
            for at in 0..layout.slots() {
                for axis in [Axis::Vertical, Axis::Horizontal] {
                    let Some(grown) = layout.split_slot(at, axis) else {
                        continue;
                    };
                    splittable += 1;
                    assert_eq!(
                        grown.layout.slots(),
                        layout.slots() + 1,
                        "{layout:?} slot {at} {axis:?} did not add exactly one slot"
                    );
                    // 割ってできた 2 枚と、割らなかったスロットの行き先で全部が埋まる
                    let mut seen: Vec<usize> =
                        grown.moved.iter().map(|(_, new)| *new).collect();
                    seen.extend(grown.halves);
                    seen.sort_unstable();
                    seen.dedup();
                    assert_eq!(
                        seen.len(),
                        grown.layout.slots(),
                        "{layout:?} slot {at} {axis:?} left a slot unaccounted for"
                    );
                }
            }
        }
        assert!(splittable > 0, "nothing was splittable — the fixture is broken");
    }

    /// **割れるのは 2 セルぶんある向きだけ。** 4 分割はどのスロットも 1 セルなので
    /// もう割れない ＝ ここが 8 値の enum で表せる限界
    #[test]
    fn a_one_cell_slot_cannot_be_split() {
        for at in 0..Layout::Four.slots() {
            for axis in [Axis::Vertical, Axis::Horizontal] {
                assert_eq!(
                    Layout::Four.split_slot(at, axis),
                    None,
                    "4 grid slot {at} {axis:?} claimed to split"
                );
            }
        }
        // 1 枚を縦に割ったら左右にしか割れない（できた枚はもう縦に割れない）
        let two = Layout::One.split_slot(0, Axis::Vertical).unwrap();
        assert_eq!(two.layout, Layout::TwoColumns);
        assert_eq!(two.layout.split_slot(0, Axis::Vertical), None);
    }

    /// **割り方と行き先の対応**（画面で見える結果そのもの）。
    /// 落とした縁がどの配置へ育つかは、この 1 本が読めば分かる
    #[test]
    fn splitting_grows_into_the_layout_the_drop_edge_implies() {
        let cases = [
            // (元, 割るスロット, 軸, 育った先)
            (Layout::One, 0, Axis::Vertical, Layout::TwoColumns),
            (Layout::One, 0, Axis::Horizontal, Layout::TwoRows),
            // 2 列の右を横に割る = 右側が上下に分かれる ＝ 左が全高の 3 枚
            (Layout::TwoColumns, 1, Axis::Horizontal, Layout::ThreeTallLeft),
            (Layout::TwoColumns, 0, Axis::Horizontal, Layout::ThreeTallRight),
            (Layout::TwoRows, 1, Axis::Vertical, Layout::ThreeWideTop),
            (Layout::TwoRows, 0, Axis::Vertical, Layout::ThreeWideBottom),
            (Layout::ThreeTallLeft, 0, Axis::Horizontal, Layout::Four),
            (Layout::ThreeWideTop, 0, Axis::Vertical, Layout::Four),
        ];
        for (from, at, axis, want) in cases {
            let grown = from
                .split_slot(at, axis)
                .unwrap_or_else(|| panic!("{from:?} slot {at} {axis:?} refused to split"));
            assert_eq!(grown.layout, want, "{from:?} slot {at} {axis:?}");
        }
    }

    /// **割らなかったスロットの番号は動き得る。** 読み順が変わるので、
    /// 中身を写す側が番号を数え直さなくていいように `moved` が答える
    #[test]
    fn the_untouched_slots_carry_their_new_numbers() {
        // 2 rows の上段を割ると、下段は 1 番から 2 番へずれる
        let grown = Layout::TwoRows.split_slot(0, Axis::Vertical).unwrap();
        assert_eq!(grown.layout, Layout::ThreeWideBottom);
        assert_eq!(grown.moved, vec![(1, 2)], "the bottom row did not follow its cells");
        assert_eq!(grown.halves, [0, 1], "the halves are not left-then-right");
        // 下段を割る側は、上段が 0 番のまま動かない
        let grown = Layout::TwoRows.split_slot(1, Axis::Vertical).unwrap();
        assert_eq!(grown.layout, Layout::ThreeWideTop);
        assert_eq!(grown.moved, vec![(0, 0)]);
        assert_eq!(grown.halves, [1, 2]);
    }

    /// **割ってできた 2 枚は「左/上 が先」。** 落とした縁がどちらの側かで
    /// 呼び手が選べる ＝ 右の縁へ落としたのに左へ入る、が起きない
    #[test]
    fn the_halves_are_ordered_by_the_side_they_sit_on() {
        let split = Split::default();
        let area = Rect::new(0, 0, 80, 40);
        for (from, at, axis) in [
            (Layout::One, 0, Axis::Vertical),
            (Layout::One, 0, Axis::Horizontal),
            (Layout::TwoColumns, 1, Axis::Horizontal),
            (Layout::TwoRows, 0, Axis::Vertical),
        ] {
            let grown = from.split_slot(at, axis).unwrap();
            let rects = grown.layout.rects(area, split);
            let [first, second] = grown.halves;
            let (a, b) = (rects[first], rects[second]);
            match axis {
                Axis::Vertical => assert!(a.x < b.x, "{from:?} {at} {axis:?}: halves are swapped"),
                Axis::Horizontal => {
                    assert!(a.y < b.y, "{from:?} {at} {axis:?}: halves are swapped")
                }
            }
        }
    }
}
