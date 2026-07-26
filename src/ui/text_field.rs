//! カーソル付きテキストフィールド（挿入・削除・←→・Home/End・クリック位置反映、全角幅対応）。
//!
//! **入力の作法をここ 1 つに持つ。** 使い手は新規セッション画面（フォルダ・プロンプト）と
//! サイドバーの名前変更で、どちらも「打った文字がどう入るか」は同じでなければならない
//! （別々に持つと片方だけ全角の桁がずれる・片方だけ Home が効かない、が起きる）。
//!
//! 画面の**どこに何桁で描くか**は持たない（それは使い手ごとに違う知識）。
//! ここが答えるのは文字列とカーソルだけで、[`TextField::cursor_x`] が返すのも
//! 「先頭からカーソルまでの表示幅」＝ 位置ではなく相対の桁数

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// カーソル付きテキストフィールド
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

    pub(crate) fn delete(&mut self) {
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
    pub(crate) fn cursor_x(&self) -> u16 {
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
    pub(crate) fn handle_key(&mut self, key: &KeyEvent) -> bool {
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
