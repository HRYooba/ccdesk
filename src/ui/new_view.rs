//! 新規セッション画面（フォルダブラウザ + 初回プロンプト入力）。
//! **入力（キー・マウス・貼り付け）の解釈はすべてこのファイルに置く**:
//! ヒットテストのジオメトリは [`NewLayout`] と同じ場所でしか変えられない ＝
//! レイアウトを動かした変更が app 側の比較式に散らばらない
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders};
use ratatui::Frame;

use crate::app::{start_new_session, App, RightView};
use crate::backend::Kind;
use crate::theme::{ui, C_OK, C_WORKING, FOCUS_BORDER, MUTED_FG};
use crate::ui::text_field::TextField;
use crate::ui::{border_style, pane_fallback_pos, FrameCursor};

/// New 画面のフォーカス対象フィールド
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum NewFocus {
    /// AGENT 切替行（←→ / Tab で claude ⇄ codex）
    Agent,
    Prompt,  // 下部のプロンプト入力（初期フォーカス。Enter で起動）
    Browser, // フォルダ一覧（↑↓ で行移動・→← で潜る/上がる。Enter は選択行の実行）
    Path,    // Folder 行のテキストフィールド
}

/// フォルダ一覧の行。先頭の Launch は「今開いているフォルダで起動する」ボタン行。
/// 行の意味を型で持つことで、`entry == ".."` の文字列比較と
/// 「index 0 は必ず ..」という暗黙の前提を無くす
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum BrowseRow {
    /// 現在のフォルダで起動（`..` の上に常設。フィルタでも消えない）
    Launch,
    /// 親フォルダへ
    Parent,
    /// サブフォルダ（名前）
    Dir(String),
}

/// 新規セッション画面（フォルダブラウザ + プロンプト入力）
pub(crate) struct NewState {
    pub(crate) cur_dir: String,
    /// 一覧に効いている前方一致フィルタ（Folder 欄の打鍵途中の断片。小文字化済み。
    /// 空 = 絞り込み無し）。
    ///
    /// `entries` は `(cur_dir, filter)` から決まるので、この 2 つが一覧の唯一の入力に
    /// なる。フィルタを状態として持たないと「一覧が絞り込みで縮んでいる」ことを
    /// 判別できず、Folder 欄が `cur_dir` と一致した時点で一覧の作り直しが
    /// 要るのかどうかが分からない
    filter: String,
    /// `(cur_dir, filter)` から作った表示行。起動ボタン + ".." + サブディレクトリ
    /// （隠しフォルダ含む）を、フィルタが空でなければ前方一致で絞ったもの。
    /// 作り直しは [`NewState::rebuild`] だけが行う
    pub(crate) entries: Vec<BrowseRow>,
    pub(crate) dir_idx: usize,
    pub(crate) scroll: usize, // 表示ウィンドウ先頭（draw で更新）
    pub(crate) shown: usize,  // 直近 draw で表示した行数（マウス判定用）
    pub(crate) path: TextField,
    pub(crate) prompt: TextField,
    pub(crate) focus: NewFocus,
    /// 今の選択行が「一覧の作り直しで既定へ戻った結果」か（＝利用者が選んだ行ではない）。
    ///
    /// クリックの 2 段階（選択 → 再クリックで実行）を守るために要る。一覧を作り直すと
    /// `dir_idx` は 0 = 起動ボタン行に戻るが `focus` は Browser のまま残るので、これが
    /// 無いと「フォルダ行を再クリックして潜った直後の起動ボタン 1 クリック」が
    /// 再クリック扱いになり、書きかけのプロンプトでセッションが起動してしまう
    /// （送ったメッセージは取り消せない）。作り直しで立て、明示的な選択移動
    /// （↑↓・ホイール・選択を動かすクリック）で倒す
    pub(crate) selection_from_rebuild: bool,
    /// 起こす agent。**この画面の選択が正本**で、起動はこの値を読む
    pub(crate) kind: Kind,
}

impl NewState {
    /// 次の agent へ回す（`kinds` ＝ 今出す agent の並びで巡回）。
    /// **数を書き写さない**ので、agent を足しても切替の実装は変わらない。
    ///
    /// 切った agent を渡されない限りそこへは回らない ＝ off の agent を
    /// この画面から起こせない。今の選択が一覧に無ければ先頭へ戻す
    /// （設定を変えた後の最初の起動で、選択が消えた agent に残らない）
    pub(crate) fn cycle_kind(&mut self, kinds: &[Kind]) {
        let Some(&first) = kinds.first() else { return };
        let at = kinds.iter().position(|k| *k == self.kind);
        self.kind = match at {
            Some(at) => kinds[(at + 1) % kinds.len()],
            None => first,
        };
    }

    pub(crate) fn browse(dir: &str) -> Self {
        let mut path = TextField::default();
        path.set_text(dir);
        Self {
            kind: Kind::default(),
            cur_dir: dir.to_string(),
            filter: String::new(),
            entries: Self::list_entries(dir),
            dir_idx: 0,
            scroll: 0,
            shown: 0,
            path,
            prompt: TextField::default(),
            focus: NewFocus::Prompt,
            selection_from_rebuild: true,
        }
    }

    pub(crate) fn set_dir(&mut self, dir: String) {
        self.rebuild(dir, String::new());
        let cur = self.cur_dir.clone();
        self.path.set_text(&cur);
    }

    /// `(dir, filter)` から一覧を作り直す。`entries` に触るのはここだけ。
    /// `filter` が空でなければサブフォルダを前方一致で絞る（起動ボタンは常設なので
    /// 残し、".." は絞り込み中は落とす）
    fn rebuild(&mut self, dir: String, filter: String) {
        self.cur_dir = dir;
        self.filter = filter;
        self.entries = Self::list_entries(&self.cur_dir);
        if !self.filter.is_empty() {
            let frag = self.filter.clone();
            self.entries.retain(|row| match row {
                BrowseRow::Launch => true, // 起動ボタンはフィルタで消さない
                BrowseRow::Parent => false,
                BrowseRow::Dir(n) => n.to_lowercase().starts_with(&frag),
            });
        }
        // 断片を打鍵中は最初の一致フォルダを選ぶ。index 0 は常設の起動ボタン
        // なので 0 のままにすると、絞り込んだ直後の → / Enter が何も起こらない
        let idx = if self.filter.is_empty() {
            0
        } else {
            self.entries
                .iter()
                .position(|row| matches!(row, BrowseRow::Dir(_)))
                .unwrap_or(0)
        };
        self.reset_selection(idx);
    }

    /// 一覧の入力 `(dir, filter)` が変わったときだけ作り直す。
    /// 変わっていなければ選択・スクロールを保つ（Folder 欄での ←→ など、
    /// 一覧に影響しない打鍵で選択を既定へ飛ばさない）
    fn rebuild_if_changed(&mut self, dir: String, filter: String) {
        if dir != self.cur_dir || filter != self.filter {
            self.rebuild(dir, filter);
        }
    }

    /// Folder 欄の編集を取り消して現在のフォルダへ戻す（Esc）。テキストだけ戻すと
    /// 「有効なディレクトリを表示しているのに一覧は絞り込み後」の食い違いが残るので、
    /// 一覧の絞り込みもここで解除する
    pub(crate) fn cancel_path_edit(&mut self) {
        let cur = self.cur_dir.clone();
        self.path.set_text(&cur);
        self.rebuild_if_changed(cur, String::new());
    }

    /// 一覧を作り直したときの選択リセット。`selection_from_rebuild` の立て忘れを
    /// 防ぐため、作り直し側の `dir_idx` 代入はすべてここを通す
    fn reset_selection(&mut self, idx: usize) {
        self.dir_idx = idx;
        self.scroll = 0;
        self.selection_from_rebuild = true;
    }

    /// 利用者の明示操作で選択行を動かす（↑↓・ホイール・選択を動かすクリック）。
    /// ここを通った選択だけが「再クリックで実行」の対象になる
    pub(crate) fn select(&mut self, idx: usize) {
        self.dir_idx = idx;
        self.selection_from_rebuild = false;
    }

    /// 1 行上へ
    pub(crate) fn select_prev(&mut self) {
        self.select(self.dir_idx.saturating_sub(1));
    }

    /// 1 行下へ（末尾で止まる）
    pub(crate) fn select_next(&mut self) {
        self.select((self.dir_idx + 1).min(self.entries.len().saturating_sub(1)));
    }

    /// `idx` 行のクリックがその行のアクション（起動 / 潜る）を実行してよいか。
    /// 実行するのは「利用者が選んだ行を、一覧にフォーカスしたまま再クリックした」
    /// ときだけ。一覧の作り直しで既定へ戻った選択は対象外
    /// （[`NewState::selection_from_rebuild`] 参照）
    fn click_activates(&self, idx: usize) -> bool {
        self.focus == NewFocus::Browser && self.dir_idx == idx && !self.selection_from_rebuild
    }

    /// Folder フィールドの内容を確定: 存在するディレクトリならそこへ移動する。
    /// 判定は D&D と同じ [`dir_of`]（Enter と貼り付けで開けるものが食い違わない）
    fn apply_path_input(&mut self) {
        let path = self.path.text.trim().trim_matches('"').to_string();
        if path.is_empty() {
            self.path.set_text(&self.cur_dir.clone());
            return;
        }
        if let Some(dir) = dir_of(&path) {
            self.set_dir(dir);
        }
    }

    /// テキストから実在ディレクトリを取り出す（引用符除去 / ファイルなら親フォルダ /
    /// 既存テキストの途中に D&D パスが挿入されて壊れた場合は末尾のドライブレター以降を救済）
    pub(crate) fn extract_dir(text: &str) -> Option<String> {
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

    /// この画面のマウス処理（[`handle_new_view_key`] のマウス版）。
    /// ヒットテストは描画と同じ [`NewLayout`]。戻り値の [`NewAction::Launch`] は
    /// 「選択済みの起動ボタンを再クリックした」で、起動の実行（`start_new_session`）は
    /// App を持つ呼び手が行う
    pub(crate) fn handle_mouse(
        &mut self,
        pane: Rect,
        mouse: &MouseEvent,
        kinds: &[Kind],
    ) -> Option<NewAction> {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let layout = NewLayout::compute(pane);
                let box_bottom = layout.prompt_box.y + layout.prompt_box.height;
                if !layout.ok {
                    // ペインが小さすぎて未描画。フィールド判定はしない
                } else if mouse.row == layout.agent_y {
                    // AGENT 行はクリックで次の agent へ回す（項目ごとの当たり判定を
                    // 持たない ＝ 桁の計算を描画と 2 箇所で持たない）
                    self.focus = NewFocus::Agent;
                    self.cycle_kind(kinds);
                } else if mouse.row >= layout.folder_hd_y && mouse.row <= layout.sep_y {
                    // FOLDER セクション（見出し・パス値・┄ 区切り）クリック → パスフィールド。
                    // パス値の行ならカーソルも移動、他はカーソル位置維持
                    self.focus = NewFocus::Path;
                    if mouse.row == layout.path_y {
                        let text_x = mouse.column.saturating_sub(layout.path_text_x);
                        self.path.click(text_x);
                    }
                } else if mouse.row >= layout.prompt_hd_y && mouse.row < box_bottom {
                    // PROMPT セクション（見出し + 入力枠 3 行）クリック → プロンプト欄
                    self.focus = NewFocus::Prompt;
                    if mouse.row == layout.input_y {
                        let text_x = mouse.column.saturating_sub(layout.input_text_x);
                        self.prompt.click(text_x);
                    }
                } else if mouse.row >= layout.list_top
                    && mouse.row < layout.list_top + layout.list_height
                {
                    // フォルダ一覧エリア（空白部分も含む）→ 一覧フォーカス。
                    // 実在する行の上なら選択も動かし、選択済み行の再クリックで実行する
                    let row_in = (mouse.row - layout.list_top) as usize;
                    if row_in < self.shown {
                        let idx = self.scroll + row_in;
                        // 起動ボタン行もフォルダ行と同じ 2 段階（選択 → 再クリック）にする。
                        // 1 クリックで起動すると、プロンプト入力中に一覧へフォーカスを
                        // 移すだけのクリックが書きかけのプロンプトでセッションを起動して
                        // しまう（送ったメッセージは取り消せない）。
                        // 判定はクリックで選択を動かす前に取る（動かした後では
                        // 常に dir_idx == idx になり 2 段階が崩れる）
                        let reclick = self.click_activates(idx);
                        self.select(idx);
                        self.focus = NewFocus::Browser;
                        if reclick {
                            if self.selected_is_launch() {
                                return Some(NewAction::Launch);
                            }
                            self.descend(); // 選択済みを再クリック = 潜る
                        }
                    } else {
                        self.focus = NewFocus::Browser;
                    }
                }
                None
            }
            MouseEventKind::ScrollUp => {
                self.focus = NewFocus::Browser;
                self.select_prev();
                None
            }
            MouseEventKind::ScrollDown => {
                self.focus = NewFocus::Browser;
                self.select_next();
                None
            }
            _ => None,
        }
    }

    /// この画面の貼り付け / D&D。フォーカス中のフィールドで受ける:
    /// Folder → フォルダ切替（一覧も更新）/ それ以外 → プロンプトへ挿入
    /// （パスを最初のメッセージ本文に書きたいケースがあるため）
    pub(crate) fn handle_paste(&mut self, text: &str) {
        if self.focus == NewFocus::Path {
            if let Some(dir) = Self::extract_dir(text) {
                self.set_dir(dir); // パスは丸ごと置き換える
            } else {
                self.path.insert_str(text.trim());
                self.refresh_from_input();
            }
        } else {
            self.prompt.insert_str(text.trim());
            self.focus = NewFocus::Prompt;
        }
    }

    fn list_entries(dir: &str) -> Vec<BrowseRow> {
        let mut out = vec![BrowseRow::Launch, BrowseRow::Parent];
        if let Ok(read) = std::fs::read_dir(dir) {
            let mut subdirs: Vec<String> = read
                .flatten()
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
            subdirs.sort_by_key(|n| n.to_lowercase());
            out.extend(subdirs.into_iter().map(BrowseRow::Dir));
        }
        out
    }

    /// 選択行が指すフォルダへ移動する。起動ボタン行は移動対象ではない
    pub(crate) fn descend(&mut self) {
        let next = match self.entries.get(self.dir_idx) {
            Some(BrowseRow::Parent) => match std::path::Path::new(&self.cur_dir).parent() {
                Some(p) => p.to_string_lossy().to_string(),
                None => return, // ドライブ直下
            },
            Some(BrowseRow::Dir(name)) => std::path::Path::new(&self.cur_dir)
                .join(name)
                .to_string_lossy()
                .to_string(),
            Some(BrowseRow::Launch) | None => return,
        };
        self.set_dir(next);
    }

    /// 親フォルダへ。一覧の index 前提を持たないので、フィルタで `..` 行が
    /// 消えている状態でも正しく上がれる
    fn go_up(&mut self) {
        let Some(parent) = std::path::Path::new(&self.cur_dir).parent() else {
            return; // ドライブ直下
        };
        let parent = parent.to_string_lossy().to_string();
        self.set_dir(parent);
    }

    /// 一覧にフォルダ行（`..` / サブフォルダ）が 1 つも無いか。
    /// 起動ボタンは常設なので `entries.is_empty()` では判定できない
    pub(crate) fn no_folder_rows(&self) -> bool {
        !self
            .entries
            .iter()
            .any(|row| matches!(row, BrowseRow::Parent | BrowseRow::Dir(_)))
    }

    /// 選択行が起動ボタンか（クリック 1 回で起動するかの判定に使う）
    pub(crate) fn selected_is_launch(&self) -> bool {
        matches!(self.entries.get(self.dir_idx), Some(BrowseRow::Launch))
    }

    /// Folder 欄の変化に一覧をリアルタイム追従させる（テキスト・カーソルは触らない）。
    /// - 全体が実在パス → そのフォルダの一覧（絞り込み無し）
    /// - 入力途中 → 最後の区切りまでを親フォルダとして開き、残りの断片で前方一致フィルタ
    /// - どちらでもなく末尾に実在パスが埋まっている（D&D がキー入力として既存テキストへ
    ///   挿入されたケース）→ パスごと置き換える
    ///
    /// やることはテキストから一覧の入力 `(dir, filter)` を決めることだけで、
    /// 作り直すかどうかの判断は [`NewState::rebuild_if_changed`] に任せる。
    /// 判断材料をフォルダだけにすると、Folder 欄が `cur_dir` と一致した時点で
    /// 「絞り込み中だから作り直しが要る」ケースを取りこぼす
    pub(crate) fn refresh_from_input(&mut self) {
        let t = self.path.text.trim().trim_matches('"').to_string();
        if t.is_empty() {
            return;
        }
        if std::path::Path::new(&t).is_dir() {
            self.rebuild_if_changed(t, String::new());
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
                self.rebuild_if_changed(parent.to_string(), frag.to_lowercase());
                return;
            }
        }
        if let Some(dir) = Self::extract_dir(&self.path.text) {
            self.set_dir(dir);
        }
    }
}

/// 新規セッション画面のキー処理。
/// フィールド制: Tab で Prompt → Path → Browser と巡回し、キーはフォーカス中のフィールドにだけ効く。
/// 起動は 2 手段: Prompt での Enter と、一覧先頭の起動ボタン行での Enter。
/// Browser の Enter は「選択行の実行」なので、フォルダ行では移動になる。
/// 起動ボタン行の Enter は 1 打鍵で起動する（明示的な操作なので確認を挟まない）。
/// マウスは同じ扱いにしない: 誤クリックで起動しないよう、クリックはフォルダ行と同じ
/// 文字列から実在ディレクトリを解決する（ディレクトリならそれ、ファイルなら
/// 親フォルダ ＝ D&D でファイルを落とした場合）。
/// **Enter 確定（`apply_path_input`）と D&D（`extract_dir`）が同じ規則を読む**:
/// 別々に持つと、片方だけ挙動を足して「貼り付けでは開けるのに Enter では
/// 開けない」形の食い違いになる。引用符の除去は呼び手が済ませる
fn dir_of(text: &str) -> Option<String> {
    let p = std::path::Path::new(text);
    if p.is_dir() {
        Some(text.to_string())
    } else if p.is_file() {
        p.parent().map(|d| d.to_string_lossy().to_string())
    } else {
        None
    }
}

/// New 画面のマウス処理が呼び手（App を持つ側）へ返す指示。
/// 起動そのもの（`start_new_session`）は state の借用を抜けてから実行する
#[derive(PartialEq, Debug)]
pub(crate) enum NewAction {
    /// 選択済みの起動ボタンを再クリックした ＝ セッションを起こす
    Launch,
}

/// 「選択 → 再クリックで実行」の 2 段階（判定は [`NewState::click_activates`]）
pub(crate) fn handle_new_view_key(app: &mut App, key: &KeyEvent) -> anyhow::Result<()> {
    // 画面より先に控える（`right_view` を可変で借りると `app` を読めなくなる）
    let kinds = app.kinds.clone();
    let RightView::New(state) = &mut app.right_view else {
        return Ok(());
    };
    // 共通キー
    match key.code {
        KeyCode::Tab => {
            state.focus = match state.focus {
                NewFocus::Prompt => NewFocus::Agent,
                NewFocus::Agent => NewFocus::Path,
                NewFocus::Path => NewFocus::Browser,
                NewFocus::Browser => NewFocus::Prompt,
            };
            return Ok(());
        }
        KeyCode::Esc => {
            match state.focus {
                NewFocus::Path => {
                    // 編集を破棄して現在のフォルダに戻す（一覧の絞り込みも解除する）
                    state.cancel_path_edit();
                    state.focus = NewFocus::Prompt;
                }
                _ if !app.windows.is_empty() => {
                    app.right_view = RightView::Sessions;
                }
                _ => {}
            }
            return Ok(());
        }
        _ => {}
    }
    match state.focus {
        // ←→ / Enter / Space のどれでも次の agent へ回す（打鍵を覚えさせない）
        NewFocus::Agent => {
            if matches!(
                key.code,
                KeyCode::Left | KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ')
            ) {
                state.cycle_kind(&kinds);
            }
        }
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
            KeyCode::Up => state.select_prev(),
            KeyCode::Down => state.select_next(),
            // Enter = 選択行の実行。起動ボタン行なら起動、フォルダ行なら → と同じく移動
            KeyCode::Enter => {
                if state.selected_is_launch() {
                    start_new_session(app)?;
                } else {
                    state.descend();
                }
            }
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
    pub(crate) agent_y: u16,      // AGENT 切替行
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
    /// ヘッダー行数（上パディング + AGENT 行 + FOLDER 見出し + パス値 + ┄ 区切り）
    const HEAD_ROWS: u16 = 5;
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
        // 上から: 空行 / AGENT / FOLDER 見出し / パス値 / ┄ 区切り / 一覧…
        let agent_y = inner.y + 1;
        let folder_hd_y = inner.y + 2;
        let path_y = inner.y + 3;
        let sep_y = inner.y + 4;
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
            agent_y,
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
            x: self.prompt_box.x.saturating_add(1),
            y: self.prompt_box.y.saturating_add(1),
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
        // 使えないので共通の退避先へ寄せる
        return FrameCursor::hidden_at(pane_fallback_pos(pane));
    }
    // ここに来た時点で layout.ok なので inner.width >= 10 が保証され、
    // inner（幅 >= 10）も prompt_inner（幅 >= 4）も幅 0 になり得ない
    // （= right() - 1 のクランプは inner の外を指さない）。
    // cursor_x は入力テキストの表示幅なので加算は飽和させる
    // （素の + だと極端に長い入力で debug ビルドが panic する）
    let inner = layout.inner;
    let prompt_inner = layout.prompt_inner();
    let (pos, in_field) = match focus {
        // 入力欄ではないのでカーソルは出さない（位置だけ確定させる）
        NewFocus::Agent => (Position::new(inner.x, layout.agent_y), false),
        NewFocus::Path => (
            Position::new(
                layout
                    .path_text_x
                    .saturating_add(path_cursor_x)
                    .min(inner.right() - 1),
                layout.path_y,
            ),
            true,
        ),
        NewFocus::Prompt => (
            Position::new(
                layout
                    .input_text_x
                    .saturating_add(prompt_cursor_x)
                    .min(prompt_inner.right() - 1),
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

/// 一覧に描ける行数。0 件メッセージ（"no matching folders"）を出す場合は
/// その 1 行を必ず確保する。
///
/// `no_folder_rows` が真なら一覧は起動ボタン 1 行だけなので、`list_height >= 2` では
/// 両方が収まり従来どおり。問題は `list_height == 1`（`layout.ok` が許す下限）で、
/// メッセージを行の後ろへ足すと ratatui の Paragraph に切られて「なぜ一覧が空か」が
/// 見えなくなる。そのときはメッセージを優先する: 起動ボタンは PROMPT 欄の Enter と
/// 同じ `cur_dir` での起動なので、失うのは重複した導線だけ。一方メッセージは
/// 他のどこにも出ない情報
fn list_rows_for_message(no_folder_rows: bool, list_height: u16) -> usize {
    let height = list_height as usize;
    if no_folder_rows {
        height.saturating_sub(1)
    } else {
        height
    }
}

/// ペイン内ヒント 1 行。**フォーカス中の欄で意味が変わるキーを出し分ける**
/// （[`handle_new_view_key`] が正本）:
///
/// - `Enter`: Prompt = 起動 / Path = パスの適用 / Browser = 選択行の実行
/// - `Esc`: Path = 編集の取り消し / それ以外 = セッション一覧へ戻る
///
/// `can_leave` ＝ 戻れる窓があるか。窓が 1 つも無いときは Esc が何もしないので
/// **出さない**（効かないキーを案内しない）
fn new_view_hint(focus: NewFocus, can_leave: bool) -> &'static str {
    match focus {
        NewFocus::Agent => "Tab: next field · ←→: switch agent",
        NewFocus::Prompt if can_leave => "Tab: next field · Enter: start · Esc: back to sessions",
        NewFocus::Prompt => "Tab: next field · Enter: start",
        // Path の Esc は戻る先に関係なく編集の取り消し（一覧へは戻らない）
        NewFocus::Path => "Tab: next field · Enter: apply path · Esc: cancel edit",
        NewFocus::Browser if can_leave => {
            "Tab: next field · ↑↓ select · Enter: run row · ←→ move · Esc: back to sessions"
        }
        NewFocus::Browser => "Tab: next field · ↑↓ select · Enter: run row · ←→ move",
    }
}

/// 新規セッション画面の描画（フォルダブラウザ + 初回チャット入力）。
/// starting = 起こした子がまだ端末を掴んでいない: プロンプト欄に進行中表示を出す。
/// can_leave = Esc で戻れるセッションの窓があるか（[`new_view_hint`]）
pub(crate) fn draw_new_view(
    frame: &mut Frame,
    area: Rect,
    state: &mut NewState,
    focused: bool,
    starting: bool,
    can_leave: bool,
) -> FrameCursor {
    let block = Block::default()
        .borders(Borders::ALL)
        .title("new session")
        .border_style(border_style(focused));
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
    // 0 件メッセージは起動ボタン行より優先する（[`list_rows_for_message`] 参照）
    let max_visible = list_rows_for_message(state.no_folder_rows(), layout.list_height);
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

    // AGENT 切替行。**選んだものだけ強調**（記号は使わない理由は [`Kind::title`]）
    let agent_focused = state.focus == NewFocus::Agent;
    let mut agent_spans = vec![
        Span::raw(pad.clone()),
        Span::styled("AGENT ", heading_style(agent_focused)),
    ];
    for kind in Kind::ORDER {
        let chosen = kind == state.kind;
        agent_spans.push(Span::raw(" "));
        agent_spans.push(Span::styled(
            kind.title().to_string(),
            if chosen {
                Style::default()
                    .fg(ui().emph)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                Style::default().fg(MUTED_FG)
            },
        ));
    }
    frame.render_widget(
        ratatui::widgets::Paragraph::new(Line::from(agent_spans)),
        Rect::new(inner.x, layout.agent_y, inner.width, 1),
    );

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

    // フォルダ一覧（一覧インデント）。先頭は「このフォルダで起動」ボタン行。
    // フィルタでフォルダ行が全部消えた場合はその状態も明示する（起動ボタンは残る）
    let list_indent = " ".repeat(margin + 1);
    // 起動先をラベルに出す: Folder 欄を打鍵中は cur_dir が親フォルダへ巻き戻るため、
    // 「どこで起動するのか」がテキスト欄の見た目と食い違うことがある
    let launch_leaf = std::path::Path::new(&state.cur_dir)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| state.cur_dir.clone());
    let mut list_lines: Vec<Line> = Vec::new();
    for virt in state.scroll..state.scroll + shown {
        let selected = virt == state.dir_idx;
        let marker = if selected { "▸ " } else { "  " };
        let is_launch = state.entries[virt] == BrowseRow::Launch;
        let label = match &state.entries[virt] {
            BrowseRow::Launch => format!("+ start in {launch_leaf}"),
            BrowseRow::Parent => "..".to_string(),
            BrowseRow::Dir(name) => name.clone(),
        };
        // 起動ボタンはアクション色。起動処理中（starting）は dim にして連打が無効な
        // ことを見せる。starting 判定を先に置くのは、起動時に focus が Browser へ
        // 移る = browser_focused が真になり、後段だと MUTED_FG に負けて dim が
        // 一度も効かないため。dim にするのは起動ボタン行だけ: 多重ディスパッチで
        // 止まるのは起動だけで、フォルダ行の移動（↑↓ →← / クリック）は生きている
        let base = if is_launch && starting {
            ui().dim
        } else if is_launch {
            C_OK
        } else if browser_focused {
            MUTED_FG
        } else {
            ui().dim
        };
        let mut style = Style::default().fg(base);
        if selected {
            style = style.add_modifier(Modifier::BOLD);
            if browser_focused {
                style = style.bg(ui().hl_bg);
                if !is_launch {
                    style = style.fg(ui().emph);
                }
            }
        }
        list_lines.push(Line::from(format!("{list_indent}{marker}{label}")).style(style));
    }
    if state.no_folder_rows() {
        list_lines.push(
            Line::from(format!("{list_indent}  no matching folders"))
                .style(Style::default().fg(ui().dim)),
        );
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
    let hint = new_view_hint(state.focus, can_leave);
    frame.render_widget(
        ratatui::widgets::Paragraph::new(
            Line::from(format!("{pad}{hint}")).style(Style::default().fg(ui().dim)),
        ),
        Rect::new(inner.x, layout.hint_y, inner.width, 1),
    );

    cursor
}

#[cfg(test)]
mod tests {
    use super::*;
    // 矩形の内包判定（幅 0 の扱いを含む）は ui 側の 1 実装を使う
    use crate::ui::tests::contains;

    const FOCUSES: [NewFocus; 3] = [NewFocus::Prompt, NewFocus::Path, NewFocus::Browser];

    /// 十分な広さのペインなら、どのフィールドにフォーカスしていても位置はペイン内
    #[test]
    fn cursor_stays_in_pane_for_every_focus() {
        let pane = Rect::new(34, 0, 80, 30);
        for focus in FOCUSES {
            let cursor = new_view_cursor(pane, focus, 0, 0, true);
            assert!(
                contains(pane, cursor.pos),
                "pos {:?} is outside the pane after a focus switch",
                cursor.pos
            );
        }
    }

    /// 長い入力（= 大きな cursor_x）でも枠の外へ出ない。狭いペインでも同じ。
    /// u16::MAX は加算の飽和も兼ねて見る（素の + だと debug ビルドで panic する）
    #[test]
    fn cursor_stays_in_pane_for_long_field_text() {
        for width in [10u16, 12, 20, 80] {
            let pane = Rect::new(34, 0, width, 30);
            for cursor_x in [500u16, u16::MAX - 1, u16::MAX] {
                for focus in FOCUSES {
                    let cursor = new_view_cursor(pane, focus, cursor_x, cursor_x, true);
                    assert!(
                        contains(pane, cursor.pos),
                        "width {width} / cursor_x {cursor_x}: pos {:?} is outside the pane",
                        cursor.pos
                    );
                }
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
                            "{w}x{h} did not fall back to the pane origin"
                        );
                    } else {
                        assert!(
                            contains(pane, cursor.pos),
                            "{w}x{h}: pos {:?} is outside the pane",
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
            assert!(contains(pane, cursor.pos), "the position must be set even while hidden");
        }
    }

    /// テスト用の一時ディレクトリ（安全な置き場の実装は
    /// [`crate::testutil::TempDir`] 1 つ。ここが持つのは sub_a / sub_b の準備だけ）
    struct TempDir(crate::testutil::TempDir);

    impl TempDir {
        /// sub_a / sub_b を持つ一時ディレクトリを作る
        fn new(tag: &str) -> Self {
            let dir = crate::testutil::TempDir::new("new-view", tag);
            std::fs::create_dir_all(dir.join("sub_a")).unwrap();
            std::fs::create_dir_all(dir.join("sub_b")).unwrap();
            Self(dir)
        }

        fn path(&self) -> &std::path::Path {
            self.0.path()
        }
    }

    #[test]
    fn lists_launch_row_and_parent_first() {
        let tmp = TempDir::new("list");
        let root = tmp.path();
        let state = NewState::browse(&root.to_string_lossy());
        assert_eq!(
            state.entries,
            vec![
                BrowseRow::Launch,
                BrowseRow::Parent,
                BrowseRow::Dir("sub_a".into()),
                BrowseRow::Dir("sub_b".into()),
            ]
        );
        // 初期選択は起動ボタン。Enter の意味（現在のフォルダで起動）と一致する
        assert!(state.selected_is_launch());
    }

    #[test]
    fn does_not_descend_on_launch_row() {
        let tmp = TempDir::new("no-descend");
        let root = tmp.path();
        let mut state = NewState::browse(&root.to_string_lossy());
        // 初期選択は起動ボタン行。→ / Enter でフォルダが変わってはいけない
        state.descend();
        assert_eq!(state.cur_dir, root.to_string_lossy());
    }

    #[test]
    fn descends_into_subdir_and_back_to_parent() {
        let tmp = TempDir::new("move");
        let root = tmp.path();
        let mut state = NewState::browse(&root.to_string_lossy());
        state.dir_idx = 2; // sub_a
        state.descend();
        assert_eq!(state.cur_dir, root.join("sub_a").to_string_lossy());
        state.dir_idx = 1; // ..
        state.descend();
        assert_eq!(state.cur_dir, root.to_string_lossy());
    }

    #[test]
    fn goes_up_even_when_parent_row_is_filtered_out() {
        let tmp = TempDir::new("go-up");
        let root = tmp.path();
        let mut state = NewState::browse(&root.to_string_lossy());
        // "…/sub" まで打鍵 = 断片フィルタ。".." 行は落ち、起動ボタンは残る
        state.path.set_text(&root.join("sub").to_string_lossy());
        state.refresh_from_input();
        assert_eq!(
            state.entries,
            vec![
                BrowseRow::Launch,
                BrowseRow::Dir("sub_a".into()),
                BrowseRow::Dir("sub_b".into()),
            ]
        );
        // go_up は一覧の index を見ず cur_dir の親を直接引くので、".." 行が
        // 消えていても正しく上がれる。旧実装は「index 0 = ..」前提で
        // entries[0] を実行していたため、ここで sub_a へ潜ってしまっていた
        state.go_up();
        let parent = root.parent().unwrap().to_string_lossy().to_string();
        assert_eq!(state.cur_dir, parent);
    }

    #[test]
    fn selects_first_match_when_filtering_so_descend_works() {
        let tmp = TempDir::new("filter-select");
        let root = tmp.path();
        let mut state = NewState::browse(&root.to_string_lossy());
        // "…/sub" まで打鍵 = 断片フィルタ（sub_a / sub_b が一致）。選択は常設の
        // 起動ボタン（index 0）ではなく最初の一致フォルダに載る。載っていないと
        // 絞り込んだ直後の → / Enter が空振りする
        state.path.set_text(&root.join("sub").to_string_lossy());
        state.refresh_from_input();
        assert_eq!(state.entries[state.dir_idx], BrowseRow::Dir("sub_a".into()));
        assert!(!state.selected_is_launch());
        // その選択のまま潜れる（↓ を 1 回押させない）
        state.descend();
        assert_eq!(state.cur_dir, root.join("sub_a").to_string_lossy());
    }

    #[test]
    fn keeps_selection_in_range_when_filter_has_no_dir_row() {
        let tmp = TempDir::new("filter-select-none");
        let root = tmp.path();
        let mut state = NewState::browse(&root.to_string_lossy());
        // 一致フォルダが無い断片。選ぶ先が無いので起動ボタン（index 0）へ落とす
        state.path.set_text(&root.join("zzz").to_string_lossy());
        state.refresh_from_input();
        assert_eq!(state.dir_idx, 0);
        assert!(state.selected_is_launch());
    }

    /// 一覧を作り直した直後の選択（既定の起動ボタン行）はクリック 1 回で実行しない。
    /// 「フォルダ行を再クリックして潜る → その位置で起動ボタンを 1 回クリック」で
    /// 書きかけのプロンプトのままセッションが起動してしまうのを防ぐ
    #[test]
    fn rebuilt_selection_needs_an_explicit_click_before_activating() {
        let tmp = TempDir::new("click-guard");
        let root = tmp.path();
        let mut state = NewState::browse(&root.to_string_lossy());
        state.focus = NewFocus::Browser;
        // 初期表示の選択も「利用者が選んだ行」ではない
        assert!(state.selected_is_launch());
        assert!(!state.click_activates(0));

        // フォルダ行のクリックで選択が動く → 再クリックで潜れる
        state.select(2);
        assert_eq!(state.entries[state.dir_idx], BrowseRow::Dir("sub_a".into()));
        assert!(state.click_activates(2));
        state.descend();

        // 潜った先では dir_idx が 0 = 起動ボタンへ戻るが focus は Browser のまま。
        // ここが 1 クリックで起動すると取り消せない誤発火になる
        assert_eq!(state.cur_dir, root.join("sub_a").to_string_lossy());
        assert!(state.selected_is_launch());
        assert!(
            !state.click_activates(0),
            "the launch button right after a rebuild fires on a single click"
        );

        // 起動ボタン行を明示的にクリック（1 回目）した後は次のクリックで起動する
        state.select(0);
        assert!(state.click_activates(0));
    }

    /// 一覧の作り直しはすべて選択を無効化する（set_dir / refresh_from_input の両分岐）。
    /// 明示的な選択移動（↑↓・ホイール・選択を動かすクリック）だけが有効化する
    #[test]
    fn every_list_rebuild_invalidates_the_selection() {
        let tmp = TempDir::new("click-guard-rebuild");
        let root = tmp.path();
        let mut state = NewState::browse(&root.to_string_lossy());

        // 断片フィルタでの作り直し
        state.select(2);
        assert!(!state.selection_from_rebuild);
        state.path.set_text(&root.join("sub").to_string_lossy());
        state.refresh_from_input();
        assert!(state.selection_from_rebuild, "rebuild triggered by the filter");
        assert!(!state.click_activates(state.dir_idx));

        // 実在パス全体を入れた作り直し
        state.select(1);
        state.path.set_text(&root.join("sub_b").to_string_lossy());
        state.refresh_from_input();
        assert!(state.selection_from_rebuild, "rebuild triggered by committing the path");

        // set_dir（→ / Enter での移動もここを通る）
        state.select(0);
        state.set_dir(root.to_string_lossy().to_string());
        assert!(state.selection_from_rebuild, "set_dir");

        // 明示操作で倒れる
        state.select_next();
        assert!(!state.selection_from_rebuild, "down arrow / wheel down");
        state.select_prev();
        assert!(!state.selection_from_rebuild, "up arrow / wheel up");
    }

    /// 0 件メッセージ用の 1 行を確保する。list_height == 1（layout.ok の下限）では
    /// 起動ボタン行より優先する
    #[test]
    fn reserves_a_line_for_the_no_match_message() {
        assert_eq!(list_rows_for_message(false, 1), 1);
        assert_eq!(list_rows_for_message(false, 5), 5);
        assert_eq!(
            list_rows_for_message(true, 1),
            0,
            "with only 1 row available, the message wins"
        );
        assert_eq!(list_rows_for_message(true, 5), 4);
        assert_eq!(list_rows_for_message(true, 0), 0);
    }

    /// 一覧が 1 行しか取れない高さでも「なぜ空なのか」が画面に出る。
    /// メッセージを行の後ろへ足していた実装では Paragraph に切られて消えていた
    #[test]
    fn renders_no_match_message_in_the_shortest_pane() {
        let tmp = TempDir::new("short-pane");
        let root = tmp.path();
        let mut state = NewState::browse(&root.to_string_lossy());
        // 一致するサブフォルダが無い断片 = 起動ボタン行だけが残る
        state.path.set_text(&root.join("zzz").to_string_lossy());
        state.refresh_from_input();
        assert!(state.no_folder_rows());

        // 一覧に 1 行だけ割ける最小の高さ（ヘッダーとフッターの行数から導く ＝
        // AGENT 行のような行が増減しても、この test が測る条件は変わらない）
        let pane = Rect::new(0, 0, 40, NewLayout::HEAD_ROWS + NewLayout::FOOT_ROWS + 3);
        assert_eq!(NewLayout::compute(pane).list_height, 1);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(pane.width, pane.height))
                .unwrap();
        terminal
            .draw(|frame| {
                draw_new_view(frame, pane, &mut state, true, false, false);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text: String = (0..pane.height)
            .flat_map(|y| (0..pane.width).map(move |x| (x, y)))
            .filter_map(|(x, y)| buffer.cell((x, y)).map(|c| c.symbol().to_string()))
            .collect();
        assert!(
            text.contains("no matching folders"),
            "the reason for zero results is not rendered: {text:?}"
        );
    }

    /// 絞り込み中に Folder 欄が `cur_dir` と完全一致まで戻ったら、一覧も
    /// `cur_dir` 直下の全フォルダへ戻る（Issue #1）。
    /// 旧実装は `t != cur_dir` だけを再構築の条件にしていたため、
    /// 「一覧が絞り込みで縮んでいる」状態を区別できず一覧が古いまま残っていた
    #[test]
    fn clears_the_filter_when_input_returns_to_cur_dir() {
        let tmp = TempDir::new("filter-back-to-cur-dir");
        let root = tmp.path().to_string_lossy().to_string();
        let mut state = NewState::browse(&root);

        // 一致フォルダの無い断片で絞り込む: cur_dir は root のまま、一覧は起動ボタンだけ
        state.path.set_text(&format!("{root}\\zzz"));
        state.refresh_from_input();
        assert_eq!(state.cur_dir, root);
        assert_eq!(state.entries, vec![BrowseRow::Launch]);

        // テキストが cur_dir と完全一致まで戻った = 絞り込み解除
        state.path.set_text(&root);
        state.refresh_from_input();
        assert_eq!(
            state.entries,
            vec![
                BrowseRow::Launch,
                BrowseRow::Parent,
                BrowseRow::Dir("sub_a".into()),
                BrowseRow::Dir("sub_b".into()),
            ],
            "the listing is still filtered even though the text matches cur_dir"
        );
        assert!(!state.no_folder_rows());
    }

    /// Issue #1 の再現手順そのまま: `X\zzz` の `\zzz` を Delete で 1 文字ずつ消す。
    /// 途中のテキスト（`Xzzz` 等）がどの分岐に入るかに依らず、`X` へ戻った時点で
    /// X 直下のサブフォルダ一覧が出ていること
    #[test]
    fn restores_listing_while_deleting_the_fragment_char_by_char() {
        let tmp = TempDir::new("filter-delete-keys");
        let root = tmp.path().to_string_lossy().to_string();
        let mut state = NewState::browse(&root);

        state.path.set_text(&format!("{root}\\zzz"));
        state.refresh_from_input();
        // カーソルを X の直後（= 区切り文字の手前）へ置き、"\zzz" を Delete で消す
        state.path.cursor = root.chars().count();
        for _ in 0..4 {
            state.path.delete();
            state.refresh_from_input();
        }

        assert_eq!(state.path.text, root);
        assert_eq!(state.cur_dir, root);
        assert_eq!(
            state.entries,
            vec![
                BrowseRow::Launch,
                BrowseRow::Parent,
                BrowseRow::Dir("sub_a".into()),
                BrowseRow::Dir("sub_b".into()),
            ]
        );
    }

    /// 一覧は常に `(cur_dir, filter)` の関数であること。この不変条件が保たれている限り
    /// 「テキストは有効なディレクトリなのに一覧が絞り込み後のまま」は起こり得ない
    #[test]
    fn entries_always_match_cur_dir_and_filter() {
        let tmp = TempDir::new("filter-invariant");
        let root = tmp.path().to_string_lossy().to_string();
        let mut state = NewState::browse(&root);

        // 打鍵の各段階（絞り込み → 0 件 → 解除 → 別フォルダ）で不変条件を見る
        for text in [
            format!("{root}\\sub"),
            format!("{root}\\sub_a"),
            format!("{root}\\zzz"),
            root.clone(),
            format!("{root}\\"),
            format!("{root}\\sub_b"),
        ] {
            state.path.set_text(&text);
            state.refresh_from_input();
            let mut expected = NewState::list_entries(&state.cur_dir);
            if !state.filter.is_empty() {
                let frag = state.filter.clone();
                expected.retain(|row| match row {
                    BrowseRow::Launch => true,
                    BrowseRow::Parent => false,
                    BrowseRow::Dir(n) => n.to_lowercase().starts_with(&frag),
                });
            }
            assert_eq!(
                state.entries, expected,
                "text {text:?}: entries do not match (cur_dir {:?}, filter {:?})",
                state.cur_dir, state.filter
            );
        }
    }

    /// ペイン内ヒントは**フォーカス中の欄で意味が変わる `Esc` を出し分ける**
    /// （[`handle_new_view_key`] の分岐が正本）。戻る窓が無いときは Esc が
    /// 何もしないので出さない
    #[test]
    fn the_pane_hint_spells_out_what_esc_means_in_the_focused_field() {
        for focus in [NewFocus::Prompt, NewFocus::Browser] {
            assert!(
                new_view_hint(focus, true).ends_with("Esc: back to sessions"),
                "{focus:?}: cannot tell that Esc returns to the session list"
            );
            assert!(
                !new_view_hint(focus, false).contains("Esc"),
                "{focus:?}: hints an Esc that does nothing"
            );
        }
        // Folder 欄の Esc は編集の取り消し（戻る窓の有無に関係なく効く）
        for can_leave in [true, false] {
            assert!(
                new_view_hint(NewFocus::Path, can_leave).ends_with("Esc: cancel edit"),
                "can_leave={can_leave}: Esc means something different in the Folder field"
            );
        }
    }

    /// Esc（Folder 欄の編集取り消し）はテキストと一覧の両方を現在のフォルダへ戻す。
    /// テキストだけ戻すと一覧が絞り込み後のまま残る
    #[test]
    fn cancel_path_edit_restores_text_and_listing() {
        let tmp = TempDir::new("cancel-path-edit");
        let root = tmp.path().to_string_lossy().to_string();
        let mut state = NewState::browse(&root);

        state.path.set_text(&format!("{root}\\zzz"));
        state.refresh_from_input();
        assert!(state.no_folder_rows());

        state.cancel_path_edit();
        assert_eq!(state.path.text, root);
        assert_eq!(state.cur_dir, root);
        assert!(!state.no_folder_rows(), "the listing is still filtered even after Esc");
    }

    /// 一覧の入力 `(cur_dir, filter)` が変わらない打鍵（Folder 欄での ←→ 等）では
    /// 一覧を作り直さず、選択を既定へ飛ばさない
    #[test]
    fn keeps_selection_when_list_input_is_unchanged() {
        let tmp = TempDir::new("no-op-refresh");
        let root = tmp.path().to_string_lossy().to_string();
        let mut state = NewState::browse(&root);

        state.select(3); // sub_b
        state.path.cursor = 0; // ← / Home 相当（テキストは変わらない）
        state.refresh_from_input();
        assert_eq!(state.dir_idx, 3);
        assert!(!state.selection_from_rebuild);
    }

    #[test]
    fn detects_zero_folder_rows_when_filter_matches_nothing() {
        let tmp = TempDir::new("empty");
        let root = tmp.path();
        let mut state = NewState::browse(&root.to_string_lossy());
        // 一致するサブフォルダが無い断片。起動ボタンだけが残る
        state.path.set_text(&root.join("zzz").to_string_lossy());
        state.refresh_from_input();
        assert_eq!(state.entries, vec![BrowseRow::Launch]);
        // entries は空でないので、0 件判定は no_folder_rows() でしか出せない
        assert!(state.no_folder_rows());
    }
}
