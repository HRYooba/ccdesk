//! 新規セッション画面（フォルダブラウザ + 初回プロンプト入力）。
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders};
use ratatui::Frame;

use crate::app::{start_new_session, App, RightView};
use crate::theme::{ui, C_WORKING, FOCUS_BORDER, MUTED_FG};
use crate::ui::FrameCursor;

/// カーソル付きテキストフィールド（挿入・削除・←→・Home/End・クリック位置反映、全角幅対応）
#[derive(Default)]
pub(crate) struct TextField {
    pub(crate) text: String,
    pub(crate) cursor: usize, // 文字（char）単位のカーソル位置
}

impl TextField {
    pub(crate) fn set_text(&mut self, s: &str) {
        self.text = s.to_string();
        self.cursor = self.text.chars().count();
    }

    fn char_count(&self) -> usize {
        self.text.chars().count()
    }

    /// char index → バイト位置
    fn byte_at(&self, idx: usize) -> usize {
        self.text
            .char_indices()
            .nth(idx)
            .map(|(b, _)| b)
            .unwrap_or(self.text.len())
    }

    fn insert(&mut self, c: char) {
        let b = self.byte_at(self.cursor);
        self.text.insert(b, c);
        self.cursor += 1;
    }

    pub(crate) fn insert_str(&mut self, s: &str) {
        let b = self.byte_at(self.cursor);
        self.text.insert_str(b, s);
        self.cursor += s.chars().count();
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            let b = self.byte_at(self.cursor - 1);
            self.text.remove(b);
            self.cursor -= 1;
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.char_count() {
            let b = self.byte_at(self.cursor);
            self.text.remove(b);
        }
    }

    fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.char_count());
    }

    fn home(&mut self) {
        self.cursor = 0;
    }

    fn end(&mut self) {
        self.cursor = self.char_count();
    }

    /// カーソルの表示列（全角 = 2 幅）
    fn cursor_x(&self) -> u16 {
        use unicode_width::UnicodeWidthChar;
        self.text
            .chars()
            .take(self.cursor)
            .map(|c| c.width().unwrap_or(0) as u16)
            .sum()
    }

    /// クリックされた表示列 → カーソル位置
    pub(crate) fn click(&mut self, x: u16) {
        use unicode_width::UnicodeWidthChar;
        let mut acc = 0u16;
        let mut idx = 0;
        for c in self.text.chars() {
            let w = c.width().unwrap_or(0) as u16;
            if acc + w > x {
                break;
            }
            acc += w;
            idx += 1;
        }
        self.cursor = idx;
    }

    /// フィールド共通のキー処理。処理したら true
    fn handle_key(&mut self, key: &KeyEvent) -> bool {
        match key.code {
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.insert(c);
            }
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left => self.left(),
            KeyCode::Right => self.right(),
            KeyCode::Home => self.home(),
            KeyCode::End => self.end(),
            _ => return false,
        }
        true
    }
}

/// New 画面のフォーカス対象フィールド
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum NewFocus {
    Prompt,  // 下部のプロンプト入力（初期フォーカス。Enter で起動）
    Browser, // フォルダ一覧（↑↓/→/← で移動。Enter は潜るだけ）
    Path,    // Folder 行のテキストフィールド
}

/// 新規セッション画面（フォルダブラウザ + プロンプト入力）
pub(crate) struct NewState {
    pub(crate) cur_dir: String,
    pub(crate) entries: Vec<String>, // ".." + サブディレクトリ（隠しフォルダ含む）
    pub(crate) dir_idx: usize,
    pub(crate) scroll: usize, // 表示ウィンドウ先頭（draw で更新）
    pub(crate) shown: usize,  // 直近 draw で表示した行数（マウス判定用）
    pub(crate) path: TextField,
    pub(crate) prompt: TextField,
    pub(crate) focus: NewFocus,
}

impl NewState {
    pub(crate) fn browse(dir: &str) -> Self {
        let mut path = TextField::default();
        path.set_text(dir);
        Self {
            cur_dir: dir.to_string(),
            entries: Self::list_entries(dir),
            dir_idx: 0,
            scroll: 0,
            shown: 0,
            path,
            prompt: TextField::default(),
            focus: NewFocus::Prompt,
        }
    }

    pub(crate) fn set_dir(&mut self, dir: String) {
        self.cur_dir = dir;
        self.path.set_text(&self.cur_dir);
        self.entries = Self::list_entries(&self.cur_dir);
        self.dir_idx = 0;
        self.scroll = 0;
    }

    /// Folder フィールドの内容を確定: 存在するディレクトリならそこへ移動して true
    fn apply_path_input(&mut self) -> bool {
        let path = self.path.text.trim().trim_matches('"').to_string();
        if path.is_empty() {
            self.path.set_text(&self.cur_dir.clone());
            return false;
        }
        let p = std::path::Path::new(&path);
        let dir = if p.is_dir() {
            Some(path.clone())
        } else if p.is_file() {
            // ファイルを渡されたら親フォルダを使う（D&D でファイルを落とした場合）
            p.parent().map(|d| d.to_string_lossy().to_string())
        } else {
            None
        };
        let Some(dir) = dir else { return false };
        self.set_dir(dir);
        true
    }

    /// テキストから実在ディレクトリを取り出す（引用符除去 / ファイルなら親フォルダ /
    /// 既存テキストの途中に D&D パスが挿入されて壊れた場合は末尾のドライブレター以降を救済）
    pub(crate) fn extract_dir(text: &str) -> Option<String> {
        let dir_of = |s: &str| -> Option<String> {
            let p = std::path::Path::new(s);
            if p.is_dir() {
                Some(s.to_string())
            } else if p.is_file() {
                p.parent().map(|d| d.to_string_lossy().to_string())
            } else {
                None
            }
        };
        let t = text.trim().trim_matches('"').trim_end();
        if let Some(dir) = dir_of(t) {
            return Some(dir);
        }
        // 例: "C:\old案C:\dropped" → 最後の「X:」位置から末尾までを候補にする
        let i = t.rfind(':')?;
        if i < 1 || !t.is_char_boundary(i - 1) || !t.as_bytes()[i - 1].is_ascii_alphabetic() {
            return None;
        }
        dir_of(t[i - 1..].trim().trim_matches('"'))
    }

    fn list_entries(dir: &str) -> Vec<String> {
        let mut out = vec!["..".to_string()];
        if let Ok(read) = std::fs::read_dir(dir) {
            let mut subdirs: Vec<String> = read
                .flatten()
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
            subdirs.sort_by_key(|n| n.to_lowercase());
            out.extend(subdirs);
        }
        out
    }

    pub(crate) fn descend(&mut self) {
        let Some(entry) = self.entries.get(self.dir_idx) else {
            return;
        };
        let next = if entry == ".." {
            match std::path::Path::new(&self.cur_dir).parent() {
                Some(p) => p.to_string_lossy().to_string(),
                None => return,
            }
        } else {
            std::path::Path::new(&self.cur_dir)
                .join(entry)
                .to_string_lossy()
                .to_string()
        };
        self.set_dir(next);
    }

    fn go_up(&mut self) {
        self.dir_idx = 0; // ".."
        self.descend();
    }

    /// Folder 欄の変化に一覧をリアルタイム追従させる（テキスト・カーソルは触らない）。
    /// - 全体が実在パス → そのフォルダの一覧
    /// - 入力途中 → 最後の区切りまでを親フォルダとして開き、残りの断片で前方一致フィルタ
    /// - どちらでもなく末尾に実在パスが埋まっている（D&D がキー入力として既存テキストへ
    ///   挿入されたケース）→ パスごと置き換える
    pub(crate) fn refresh_from_input(&mut self) {
        let t = self.path.text.trim().trim_matches('"').to_string();
        if t.is_empty() {
            return;
        }
        if std::path::Path::new(&t).is_dir() {
            if t != self.cur_dir {
                self.cur_dir = t;
                self.entries = Self::list_entries(&self.cur_dir);
                self.dir_idx = 0;
                self.scroll = 0;
            }
            return;
        }
        if let Some(sep) = t.rfind(['\\', '/']) {
            let (parent, frag) = t.split_at(sep + 1);
            // 末尾区切りを外す。ただしルートは区切りが必須なので残す:
            // "C:" はドライブ相対（プロセスカレント）を指してしまうため "C:\" のまま、
            // "\" や "/" もそのまま
            let trimmed = parent.trim_end_matches(['\\', '/']);
            let parent = if trimmed.is_empty() || trimmed.ends_with(':') {
                parent
            } else {
                trimmed
            };
            if std::path::Path::new(parent).is_dir() {
                let frag = frag.to_lowercase();
                self.cur_dir = parent.to_string();
                self.entries = Self::list_entries(parent);
                self.entries.retain(|n| {
                    if n == ".." {
                        frag.is_empty()
                    } else {
                        n.to_lowercase().starts_with(&frag)
                    }
                });
                self.dir_idx = 0;
                self.scroll = 0;
                return;
            }
        }
        if let Some(dir) = Self::extract_dir(&self.path.text) {
            self.set_dir(dir);
        }
    }
}

/// 新規セッション画面のキー処理。
/// フィールド制: Tab で Prompt ⇔ Browser を切替え、キーはフォーカス中のフィールドにだけ効く。
/// 起動は Prompt での Enter のみ（Browser の Enter は移動専用）
pub(crate) fn handle_new_view_key(app: &mut App, key: &KeyEvent) -> anyhow::Result<()> {
    let RightView::New(state) = &mut app.right_view else {
        return Ok(());
    };
    // 共通キー
    match key.code {
        KeyCode::Tab => {
            state.focus = match state.focus {
                NewFocus::Prompt => NewFocus::Path,
                NewFocus::Path => NewFocus::Browser,
                NewFocus::Browser => NewFocus::Prompt,
            };
            return Ok(());
        }
        KeyCode::Esc => {
            match state.focus {
                NewFocus::Path => {
                    // 編集を破棄して現在のフォルダに戻す
                    let cur = state.cur_dir.clone();
                    state.path.set_text(&cur);
                    state.focus = NewFocus::Prompt;
                }
                _ if !app.sessions.is_empty() => {
                    app.right_view = RightView::Sessions;
                }
                _ => {}
            }
            return Ok(());
        }
        _ => {}
    }
    match state.focus {
        NewFocus::Path => {
            if key.code == KeyCode::Enter {
                state.apply_path_input();
            } else if state.path.handle_key(key) {
                // 手打ちで実在パスになった時点で下の一覧を追従させる
                state.refresh_from_input();
            }
        }
        NewFocus::Prompt => {
            if key.code == KeyCode::Enter {
                start_new_session(app)?;
            } else {
                state.prompt.handle_key(key);
            }
        }
        NewFocus::Browser => match key.code {
            KeyCode::Up => state.dir_idx = state.dir_idx.saturating_sub(1),
            KeyCode::Down => {
                state.dir_idx =
                    (state.dir_idx + 1).min(state.entries.len().saturating_sub(1));
            }
            // Enter = 現在のフォルダで起動（潜るのは →）
            KeyCode::Enter => start_new_session(app)?,
            KeyCode::Right => state.descend(),
            KeyCode::Left => state.go_up(),
            _ => {}
        },
    }
    Ok(())
}

/// New 画面のジオメトリ。描画とマウスのヒットテストが同じ座標を共有するための単一計算点。
/// これまで draw と handle_mouse に散っていた行番号のマジックナンバー（row==1・row-3・
/// term_height-4 等）をここへ集約する。値はすべて絶対スクリーン座標（Rect と同じ原点）。
pub(crate) struct NewLayout {
    pub(crate) inner: Rect,       // 枠の内側
    pub(crate) folder_hd_y: u16,  // "FOLDER" セクション見出し
    pub(crate) path_y: u16,       // パス値の行
    pub(crate) sep_y: u16,        // FOLDER セクションの ┄ 区切り
    pub(crate) list_top: u16,     // フォルダ一覧の先頭行
    pub(crate) list_height: u16,  // 一覧に割ける行数（縦が足りなければここが縮む）
    pub(crate) prompt_hd_y: u16,  // "PROMPT" セクション見出し
    pub(crate) prompt_box: Rect,  // プロンプト入力枠（Borders::ALL・高さ3）
    pub(crate) input_y: u16,      // 枠内の入力行
    pub(crate) hint_y: u16,       // ペイン内ヒント行
    pub(crate) path_text_x: u16,  // パス値のテキスト開始列（左余白の直後）
    pub(crate) input_text_x: u16, // 入力行のテキスト開始列（枠内 " ❯ " の直後）
    pub(crate) ok: bool,          // フォーム全体 + 一覧 1 行が収まる高さか
}

impl NewLayout {
    /// 左右の余白（モック準拠の 2 桁）
    const MARGIN: u16 = 2;
    /// 入力枠内の先頭 " ❯ "（先頭スペース + ❯ + スペース）の表示幅
    const INPUT_LEAD: u16 = 3;
    /// ヘッダー行数（上パディング + FOLDER 見出し + パス値 + ┄ 区切り）
    const HEAD_ROWS: u16 = 4;
    /// フッター行数（spacer + PROMPT 見出し + 枠上 + 入力 + 枠下 + 空行 + ヒント）
    const FOOT_ROWS: u16 = 7;

    /// 右ペイン矩形（枠を含む）からフォーム型レイアウトを導く。draw は chunks[1] を、
    /// ヒットテストは同じ矩形を再構成して渡すことで両者のジオメトリを一致させる。
    pub(crate) fn compute(pane: Rect) -> Self {
        // Block::inner(Borders::ALL) と同じ四辺 1px の縮小
        let inner = Rect {
            x: pane.x + 1,
            y: pane.y + 1,
            width: pane.width.saturating_sub(2),
            height: pane.height.saturating_sub(2),
        };
        let bottom = inner.y + inner.height; // 内側の下端（排他）
        // 上から: 空行 / FOLDER 見出し / パス値 / ┄ 区切り / 一覧…
        let folder_hd_y = inner.y + 1;
        let path_y = inner.y + 2;
        let sep_y = inner.y + 3;
        let list_top = inner.y + Self::HEAD_ROWS;
        // 下から: ヒント / 空行 / 枠下 / 入力 / 枠上 / PROMPT 見出し / spacer
        let hint_y = bottom.saturating_sub(1);
        let input_y = bottom.saturating_sub(4);
        let box_top_y = bottom.saturating_sub(5);
        let prompt_hd_y = bottom.saturating_sub(6);
        let list_height = inner
            .height
            .saturating_sub(Self::HEAD_ROWS + Self::FOOT_ROWS);
        let prompt_box = Rect {
            x: inner.x + Self::MARGIN,
            y: box_top_y,
            width: inner.width.saturating_sub(Self::MARGIN * 2),
            height: 3,
        };
        NewLayout {
            inner,
            folder_hd_y,
            path_y,
            sep_y,
            list_top,
            list_height,
            prompt_hd_y,
            input_y,
            hint_y,
            // パス値は左余白の直後から
            path_text_x: inner.x + Self::MARGIN,
            // 入力は枠内側（+1）の " ❯ "（+INPUT_LEAD）の後ろ
            input_text_x: prompt_box.x + 1 + Self::INPUT_LEAD,
            prompt_box,
            // フォーム全体（HEAD + FOOT）+ 一覧 1 行が要る。狭い幅では ┄/枠が崩れるため下限も設ける
            ok: inner.height > Self::HEAD_ROWS + Self::FOOT_ROWS && inner.width >= 10,
        }
    }

    /// PROMPT 入力枠の内側（Borders::ALL の 1px 内側）。入力行の描画とカーソル位置で
    /// 同じ値を共有するため、Block::inner の再計算をここへ集約する
    pub(crate) fn prompt_inner(&self) -> Rect {
        Rect {
            x: self.prompt_box.x + 1,
            y: self.prompt_box.y + 1,
            width: self.prompt_box.width.saturating_sub(2),
            height: self.prompt_box.height.saturating_sub(2),
        }
    }
}

/// New 画面のカーソル位置と可視性。Frame を必要としない純関数なので描画から
/// 切り離してテストできる。pane = 右ペイン矩形（枠を含む）。
///
/// フォーカス外・一覧操作中も「隠すだけ」で位置は必ず返す（位置を返さないと物理
/// カーソルがサイドバーに置き去りになる。FrameCursor 参照）。戻り値の位置は
/// pane がどんなサイズでも pane 矩形の内側に収まる
pub(crate) fn new_view_cursor(
    pane: Rect,
    focus: NewFocus,
    path_cursor_x: u16,
    prompt_cursor_x: u16,
    focused: bool,
) -> FrameCursor {
    let layout = NewLayout::compute(pane);
    if !layout.ok {
        // フォームが収まらない（inner が潰れている場合も含む）。inner 基準のクランプは
        // 使えないので、確実に画面内であるペイン矩形の原点へ退避する
        return FrameCursor::hidden_at(Position::new(pane.x, pane.y));
    }
    // ここに来た時点で layout.ok なので inner.width >= 10 が保証され、
    // inner（幅 >= 10）も prompt_inner（幅 >= 4）も幅 0 になり得ない
    // （= right() - 1 のクランプは inner の外を指さない）
    let inner = layout.inner;
    let prompt_inner = layout.prompt_inner();
    let (pos, in_field) = match focus {
        NewFocus::Path => (
            Position::new(
                (layout.path_text_x + path_cursor_x).min(inner.right() - 1),
                layout.path_y,
            ),
            true,
        ),
        NewFocus::Prompt => (
            Position::new(
                (layout.input_text_x + prompt_cursor_x).min(prompt_inner.right() - 1),
                layout.input_y,
            ),
            true,
        ),
        NewFocus::Browser => (Position::new(layout.input_text_x, layout.input_y), false),
    };
    if focused && in_field {
        FrameCursor::shown_at(pos)
    } else {
        FrameCursor::hidden_at(pos)
    }
}

/// 新規セッション画面の描画（フォルダブラウザ + 初回チャット入力）。
/// starting = `claude --bg` 実行中（別スレッド）: プロンプト欄に進行中表示を出す
pub(crate) fn draw_new_view(
    frame: &mut Frame,
    area: Rect,
    state: &mut NewState,
    focused: bool,
    starting: bool,
) -> FrameCursor {
    let border = if focused {
        Style::default().fg(FOCUS_BORDER)
    } else {
        Style::default().fg(ui().dim)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title("new session")
        .border_style(border);
    frame.render_widget(block, area);

    // 描画とマウス判定で同一のジオメトリを使う（フォーム型レイアウト）
    let layout = NewLayout::compute(area);
    // カーソル位置は描画結果に依存しないので先に決める（!ok の早期 return と共有する）
    let cursor = new_view_cursor(
        area,
        state.focus,
        state.path.cursor_x(),
        state.prompt.cursor_x(),
        focused,
    );
    if !layout.ok {
        return cursor;
    }
    let inner = layout.inner;

    // 一覧のスクロールウィンドウを更新（縦が足りなければ list_height 側が縮む）
    let max_visible = layout.list_height as usize;
    if state.dir_idx < state.scroll {
        state.scroll = state.dir_idx;
    } else if max_visible > 0 && state.dir_idx >= state.scroll + max_visible {
        state.scroll = state.dir_idx + 1 - max_visible;
    }
    let shown = state
        .entries
        .len()
        .saturating_sub(state.scroll)
        .min(max_visible);
    state.shown = shown;

    let browser_focused = state.focus == NewFocus::Browser;
    let path_focused = state.focus == NewFocus::Path;
    let prompt_focused = state.focus == NewFocus::Prompt;
    // セクション見出しは、そのセクションにフォーカスがあるときだけ emph+BOLD で「今どこ」を示す
    let folder_focused = matches!(state.focus, NewFocus::Path | NewFocus::Browser);
    let heading_style = |on: bool| {
        if on {
            Style::default().fg(ui().emph).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(ui().dim)
        }
    };
    let margin = NewLayout::MARGIN as usize;
    let pad = " ".repeat(margin);

    // FOLDER セクション見出し
    frame.render_widget(
        ratatui::widgets::Paragraph::new(
            Line::from(format!("{pad}FOLDER")).style(heading_style(folder_focused)),
        ),
        Rect::new(inner.x, layout.folder_hd_y, inner.width, 1),
    );

    // パス値（見出し下の独立行。編集可・下線がフィールドの手掛かり。フォーカス中は emph）
    let value_style = Style::default()
        .add_modifier(Modifier::UNDERLINED)
        .fg(if path_focused { ui().emph } else { MUTED_FG });
    frame.render_widget(
        ratatui::widgets::Paragraph::new(Line::from(vec![
            Span::raw(pad.clone()),
            Span::styled(state.path.text.clone(), value_style),
        ])),
        Rect::new(inner.x, layout.path_y, inner.width, 1),
    );

    // ┄ 区切り（左右 2 桁余白を残す）
    frame.render_widget(
        ratatui::widgets::Paragraph::new(
            Line::from(format!(
                "{pad}{}",
                "┄".repeat(inner.width.saturating_sub(NewLayout::MARGIN * 2) as usize)
            ))
            .style(Style::default().fg(ui().dim)),
        ),
        Rect::new(inner.x, layout.sep_y, inner.width, 1),
    );

    // フォルダ一覧（一覧インデント。フィルタ0件は状態を明示）
    let list_indent = " ".repeat(margin + 1);
    let mut list_lines: Vec<Line> = Vec::new();
    if shown == 0 {
        list_lines.push(
            Line::from(format!("{list_indent}no matching folders"))
                .style(Style::default().fg(ui().dim)),
        );
    } else {
        for virt in state.scroll..state.scroll + shown {
            let selected = virt == state.dir_idx;
            let marker = if selected { "▸ " } else { "  " };
            let mut style = Style::default().fg(if browser_focused {
                MUTED_FG
            } else {
                ui().dim
            });
            if selected {
                style = style.add_modifier(Modifier::BOLD);
                if browser_focused {
                    style = style.fg(ui().emph).bg(ui().hl_bg);
                }
            }
            list_lines.push(
                Line::from(format!("{list_indent}{marker}{}", state.entries[virt])).style(style),
            );
        }
    }
    frame.render_widget(
        ratatui::widgets::Paragraph::new(list_lines),
        Rect::new(inner.x, layout.list_top, inner.width, layout.list_height),
    );

    // PROMPT セクション見出し
    frame.render_widget(
        ratatui::widgets::Paragraph::new(
            Line::from(format!("{pad}PROMPT")).style(heading_style(prompt_focused)),
        ),
        Rect::new(inner.x, layout.prompt_hd_y, inner.width, 1),
    );

    // PROMPT 入力枠（フォーカス中は FOCUS_BORDER、非フォーカスは dim）
    let box_border = if prompt_focused {
        Style::default().fg(FOCUS_BORDER)
    } else {
        Style::default().fg(ui().dim)
    };
    let prompt_block = Block::default()
        .borders(Borders::ALL)
        .border_style(box_border);
    let prompt_inner = layout.prompt_inner();
    frame.render_widget(prompt_block, layout.prompt_box);
    // 入力行（starting 表示も枠内に出す）
    let marker_style = if prompt_focused {
        Style::default().fg(ui().emph).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(MUTED_FG)
    };
    let input_line = if starting {
        Line::from(vec![
            Span::raw(" "),
            Span::styled("❯ ", marker_style),
            Span::styled("starting session…", Style::default().fg(C_WORKING)),
        ])
    } else if state.prompt.text.is_empty() {
        Line::from(vec![
            Span::raw(" "),
            Span::styled("❯ ", marker_style),
            Span::styled("describe a task…", Style::default().fg(ui().dim)),
        ])
    } else {
        Line::from(vec![
            Span::raw(" "),
            Span::styled("❯ ", marker_style),
            Span::raw(state.prompt.text.clone()),
        ])
    };
    frame.render_widget(
        ratatui::widgets::Paragraph::new(input_line),
        Rect::new(prompt_inner.x, layout.input_y, prompt_inner.width, 1),
    );

    // ペイン内ヒント（下部バーの "new session:" セグメントはここへ移設して重複を避ける）
    frame.render_widget(
        ratatui::widgets::Paragraph::new(
            Line::from(format!("{pad}Tab: next field · Enter: start"))
                .style(Style::default().fg(ui().dim)),
        ),
        Rect::new(inner.x, layout.hint_y, inner.width, 1),
    );

    cursor
}

#[cfg(test)]
mod tests {
    use super::*;

    /// pos が矩形の内側にあるか（幅・高さ 0 の矩形は「内側なし」なので常に false）
    fn contains(rect: Rect, pos: Position) -> bool {
        pos.x >= rect.x && pos.x < rect.right() && pos.y >= rect.y && pos.y < rect.bottom()
    }

    const FOCUSES: [NewFocus; 3] = [NewFocus::Prompt, NewFocus::Path, NewFocus::Browser];

    /// 十分な広さのペインなら、どのフィールドにフォーカスしていても位置はペイン内
    #[test]
    fn cursor_stays_in_pane_for_every_focus() {
        let pane = Rect::new(34, 0, 80, 30);
        for focus in FOCUSES {
            let cursor = new_view_cursor(pane, focus, 0, 0, true);
            assert!(
                contains(pane, cursor.pos),
                "focus 切替で pos {:?} がペイン外",
                cursor.pos
            );
        }
    }

    /// 長い入力（= 大きな cursor_x）でも枠の外へ出ない。狭いペインでも同じ
    #[test]
    fn cursor_stays_in_pane_for_long_field_text() {
        for width in [10u16, 12, 20, 80] {
            let pane = Rect::new(34, 0, width, 30);
            for focus in FOCUSES {
                let cursor = new_view_cursor(pane, focus, 500, 500, true);
                assert!(
                    contains(pane, cursor.pos),
                    "幅 {width} / focus 変化で pos {:?} がペイン外",
                    cursor.pos
                );
            }
        }
    }

    /// フォームが収まらない高さ（layout.ok == false）でも位置はペイン内
    #[test]
    fn cursor_stays_in_pane_when_layout_does_not_fit() {
        let pane = Rect::new(34, 0, 80, 6); // HEAD_ROWS + FOOT_ROWS に足りない
        assert!(!NewLayout::compute(pane).ok);
        for focus in FOCUSES {
            let cursor = new_view_cursor(pane, focus, 0, 0, true);
            assert_eq!(cursor.pos, Position::new(pane.x, pane.y));
            assert!(contains(pane, cursor.pos));
        }
    }

    /// 退化サイズでもペイン外へ出ない（Finding 3 の回帰テスト）。
    /// 幅・高さのどちらかが 0 のペインには「内側」が存在しないので、
    /// その場合だけはペイン原点に落ちることを確認する
    #[test]
    fn cursor_stays_in_pane_for_degenerate_pane_sizes() {
        for w in [0u16, 1, 2, 3, 9, 10] {
            for h in [0u16, 1, 2, 11, 12] {
                let pane = Rect::new(34, 5, w, h);
                for focus in FOCUSES {
                    let cursor = new_view_cursor(pane, focus, 42, 42, true);
                    if w == 0 || h == 0 {
                        assert_eq!(
                            cursor.pos,
                            Position::new(pane.x, pane.y),
                            "{w}x{h} でペイン原点に落ちていない"
                        );
                    } else {
                        assert!(
                            contains(pane, cursor.pos),
                            "{w}x{h} で pos {:?} がペイン外",
                            cursor.pos
                        );
                    }
                }
            }
        }
    }

    /// 可視になるのは「ペインにフォーカスがあり、かつテキストフィールド上」のときだけ。
    /// 隠すときも位置は必ず入っている（IME のアンカーを迷子にしない）
    #[test]
    fn cursor_visible_only_when_pane_focused_and_field_active() {
        let pane = Rect::new(34, 0, 80, 30);
        assert!(new_view_cursor(pane, NewFocus::Prompt, 0, 0, true).visible);
        assert!(new_view_cursor(pane, NewFocus::Path, 0, 0, true).visible);
        // 一覧操作中は入力欄に居ないので隠す
        assert!(!new_view_cursor(pane, NewFocus::Browser, 0, 0, true).visible);
        // ペイン自体が非フォーカス（サイドバーにフォーカス）なら常に隠す
        for focus in FOCUSES {
            let cursor = new_view_cursor(pane, focus, 0, 0, false);
            assert!(!cursor.visible);
            assert!(contains(pane, cursor.pos), "非表示でも位置は確定させる");
        }
    }
}
