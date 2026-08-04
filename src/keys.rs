//! キー入力・貼り付けの VT エンコードと、マウスイベントの PTY 転送。
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use ccdesk::{LockExt, Parser};

use crate::app::App;

/// マウスイベントを claude が要求したプロトコル（SGR 前提）で PTY へ転送する。
/// claude は AnyMotion(1003) + SGR(1006) を有効化してくる。
/// 書き込みの失敗は落とす（マウスは高頻度で、取りこぼしても次のイベントが来る。
/// 壊れた PTY の窓はキー入力側の経路が閉じる）
pub(crate) fn forward_mouse(app: &mut App, mouse: &MouseEvent) {
    use vt100::{MouseProtocolEncoding, MouseProtocolMode};

    // フォーカススロットの内側（枠線 1px）基準の原点。**矩形の正本は描画と同じ
    // [`crate::app::App::slot_rects`]**（窓を借りる前に取る）
    let rects = app.slot_rects();
    let Some(pane) = rects.get(app.focus_slot).copied() else {
        return;
    };
    let (ox, oy) = (pane.x + 1, pane.y + 1);
    let Some(at) = app.focused_window() else {
        return;
    };
    let window = &mut app.windows[at];
    let (mode, encoding, size) = {
        let parser = window.parser.lock_recover();
        let screen = parser.screen();
        (
            screen.mouse_protocol_mode(),
            screen.mouse_protocol_encoding(),
            screen.size(),
        )
    };
    if mode == MouseProtocolMode::None || encoding != MouseProtocolEncoding::Sgr {
        return; // claude は SGR(1006) を有効化する。他エンコーディングは対象外
    }

    if mouse.column < ox || mouse.row < oy {
        return;
    }
    let x = mouse.column - ox + 1;
    let y = mouse.row - oy + 1;
    if x > size.1 || y > size.0 {
        return;
    }

    let button_code = |b: MouseButton| match b {
        MouseButton::Left => 0u8,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    let (code, release) = match mouse.kind {
        MouseEventKind::Down(b) => (button_code(b), false),
        MouseEventKind::Up(b) if mode != MouseProtocolMode::Press => (button_code(b), true),
        MouseEventKind::Drag(b)
            if matches!(
                mode,
                MouseProtocolMode::ButtonMotion | MouseProtocolMode::AnyMotion
            ) =>
        {
            (button_code(b) + 32, false)
        }
        MouseEventKind::Moved if mode == MouseProtocolMode::AnyMotion => (32 + 3, false),
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        MouseEventKind::ScrollLeft => (66, false),
        MouseEventKind::ScrollRight => (67, false),
        _ => return,
    };
    // 修飾キー: Shift +4 / Alt +8 / Ctrl +16（xterm 準拠）
    let code = code
        + 4 * u8::from(mouse.modifiers.contains(KeyModifiers::SHIFT))
        + 8 * u8::from(mouse.modifiers.contains(KeyModifiers::ALT))
        + 16 * u8::from(mouse.modifiers.contains(KeyModifiers::CONTROL));

    let suffix = if release { 'm' } else { 'M' };
    let seq = format!("\x1b[<{code};{x};{y}{suffix}");
    let _ = window.send(seq.as_bytes());
}

/// crossterm のキーイベントを VT シーケンスへ変換する。
/// DECCKM（application cursor）に追従し、修飾キーは子プロセスが有効化した
/// プロトコル（kitty > modifyOtherKeys > xterm legacy 修飾）でエンコードする。
pub(crate) fn encode_key(key: &KeyEvent, parser: &Parser) -> Vec<u8> {
    let screen = parser.screen();
    let app_cursor = screen.application_cursor();
    let kitty = parser.callbacks().kitty_flags > 0;
    let modify_other = parser.callbacks().modify_other_keys >= 2;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    // xterm 修飾コード: 1 + Shift(1) + Alt(2) + Ctrl(4)
    let mods = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);

    // 修飾つき機能キー（Enter/Tab/BS 等）: kitty → modifyOtherKeys の順で表現
    let functional = |out: &mut Vec<u8>, code: u32| -> bool {
        if mods == 1 {
            return false;
        }
        if kitty {
            out.extend_from_slice(format!("\x1b[{code};{mods}u").as_bytes());
            true
        } else if modify_other {
            out.extend_from_slice(format!("\x1b[27;{mods};{code}~").as_bytes());
            true
        } else {
            false
        }
    };
    // 修飾つきカーソル/編集キー: xterm 標準の CSI 1;{mod}X / CSI {n};{mod}~
    let cursor_key = |out: &mut Vec<u8>, final_ch: char| {
        if mods > 1 {
            out.extend_from_slice(format!("\x1b[1;{mods}{final_ch}").as_bytes());
        } else if app_cursor && matches!(final_ch, 'A' | 'B' | 'C' | 'D' | 'H' | 'F') {
            out.extend_from_slice(format!("\x1bO{final_ch}").as_bytes());
        } else {
            out.extend_from_slice(format!("\x1b[{final_ch}").as_bytes());
        }
    };
    let tilde_key = |out: &mut Vec<u8>, n: u8| {
        if mods > 1 {
            out.extend_from_slice(format!("\x1b[{n};{mods}~").as_bytes());
        } else {
            out.extend_from_slice(format!("\x1b[{n}~").as_bytes());
        }
    };
    // legacy バイト（Enter/Tab/BS）。修飾つきは functional（kitty →
    // modifyOtherKeys）が表し、どちらも無効なら Alt の ESC 前置 + 素のバイト。
    // **「Alt の前置は legacy 経路だけ」の規則を 3 つのキーへ書き写さない**
    let legacy = |out: &mut Vec<u8>, code: u32, byte: u8| {
        if functional(out, code) {
            return;
        }
        if alt {
            out.push(0x1b);
        }
        out.push(byte);
    };

    // Alt は「ESC 前置」か「修飾コード入りシーケンス」のどちらか一方でだけ表す。
    // 両方付けると余分な孤立 ESC が子プロセスに Esc キー押下として解釈される。
    // 修飾コード入り（CSI u / CSI 27;m;c~ / CSI 1;mX / CSI n;m~）は mods に Alt を
    // 含むため前置しない。前置するのは legacy バイト列（C0・生文字・\r 等）だけ
    let mut out: Vec<u8> = Vec::new();
    match key.code {
        // kitty disambiguate（flag 1）: Ctrl/Alt を含む文字キーは CSI u 形式で送る。
        // **対応を名乗っている（Responder が `?u` クエリに flags を返す）以上、
        // 形式でも送る**: C0 へ畳むと Ctrl+P と Ctrl+Shift+P が同じバイトに潰れ
        // （Shift 消失）、Alt の ESC 前置は Esc 押下 + 文字と誤解釈され得る。
        // コードポイントはシフト無しの字（kitty の規約）
        KeyCode::Char(c) if kitty && (ctrl || alt) => {
            let code = c.to_lowercase().next().unwrap_or(c) as u32;
            out.extend_from_slice(format!("\x1b[{code};{mods}u").as_bytes());
        }
        KeyCode::Char(c) if ctrl => {
            // Ctrl+英字/記号 → C0 制御コード（xterm 準拠）
            let lower = c.to_ascii_lowercase();
            let code: Option<u8> = if lower.is_ascii_lowercase() {
                Some(lower as u8 - b'a' + 1)
            } else {
                match c {
                    ' ' | '@' => Some(0x00),
                    '[' => Some(0x1b),
                    '\\' => Some(0x1c),
                    ']' => Some(0x1d),
                    '^' => Some(0x1e),
                    '_' | '-' => Some(0x1f),
                    '?' => Some(0x7f),
                    _ => None, // 表現できない組み合わせは何も送らない
                }
            };
            if let Some(code) = code {
                if alt {
                    out.push(0x1b);
                }
                out.push(code);
            }
        }
        KeyCode::Char(c) => {
            if alt {
                out.push(0x1b);
            }
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
        KeyCode::Enter => legacy(&mut out, 13, b'\r'),
        KeyCode::Tab => legacy(&mut out, 9, b'\t'),
        KeyCode::BackTab => out.extend_from_slice(b"\x1b[Z"),
        KeyCode::Backspace => legacy(&mut out, 127, 0x7f),
        KeyCode::Esc => {
            // kitty flag 1（disambiguate）有効時は Esc 単押しも CSI u 形式が仕様
            if kitty {
                if mods > 1 {
                    out.extend_from_slice(format!("\x1b[27;{mods}u").as_bytes());
                } else {
                    out.extend_from_slice(b"\x1b[27u");
                }
            } else if !functional(&mut out, 27) {
                if alt {
                    out.push(0x1b); // legacy Alt+Esc = ESC ESC
                }
                out.push(0x1b);
            }
        }
        KeyCode::Up => cursor_key(&mut out, 'A'),
        KeyCode::Down => cursor_key(&mut out, 'B'),
        KeyCode::Right => cursor_key(&mut out, 'C'),
        KeyCode::Left => cursor_key(&mut out, 'D'),
        KeyCode::Home => cursor_key(&mut out, 'H'),
        KeyCode::End => cursor_key(&mut out, 'F'),
        KeyCode::PageUp => tilde_key(&mut out, 5),
        KeyCode::PageDown => tilde_key(&mut out, 6),
        KeyCode::Delete => tilde_key(&mut out, 3),
        KeyCode::Insert => tilde_key(&mut out, 2),
        // F1-F12（修飾つきは xterm 形式）
        KeyCode::F(n @ 1..=4) => {
            let final_ch = (b'P' + (n - 1)) as char;
            if mods > 1 {
                out.extend_from_slice(format!("\x1b[1;{mods}{final_ch}").as_bytes());
            } else {
                out.extend_from_slice(format!("\x1bO{final_ch}").as_bytes());
            }
        }
        KeyCode::F(n @ 5..=12) => {
            let code = match n {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                _ => 24,
            };
            tilde_key(&mut out, code);
        }
        _ => {}
    }
    out
}

/// 貼り付けを PTY へ流すバイト列にする。
///
/// **sanitize と bracketed paste の包みは「入力を VT バイト列にする」知識**なので
/// [`encode_key`] と同じここに置く（run ループに書くと、キー側だけプロトコルを
/// 直して貼り付けが取り残される）。除去するのは制御文字 ＝ 特に ESC
/// （`\x1b[201~` でペースト終端を偽装され、続きが生のキー入力として届く）
pub(crate) fn encode_paste(text: &str, parser: &Parser) -> Vec<u8> {
    let sanitized: String = text
        .chars()
        .filter(|c| matches!(c, '\n' | '\r' | '\t') || !c.is_control())
        .collect();
    if parser.screen().bracketed_paste() {
        let mut out = Vec::with_capacity(sanitized.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(sanitized.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        sanitized.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `setup` のシーケンスを食わせた後のパーサ（子プロセスがプロトコルを
    /// 有効化した状態を作る）
    fn parser_after(setup: &str) -> Parser {
        let mut parser = ccdesk::new_parser(24, 80, 0);
        parser.process(setup.as_bytes());
        parser
    }

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    /// **kitty を名乗っている間、Ctrl/Alt を含む文字キーは CSI u 形式で届く。**
    /// C0 に畳むと Ctrl+P と Ctrl+Shift+P が同じ 0x10 に潰れ、Alt の ESC 前置は
    /// Esc 押下 + 文字と誤解釈され得る（Responder が対応を名乗る以上、形式で送る）
    #[test]
    fn kitty_mode_sends_ctrl_and_alt_chars_in_csi_u_form() {
        let kitty = parser_after("\x1b[>1u"); // 子が disambiguate を push した状態
        assert_eq!(
            encode_key(&key(KeyCode::Char('p'), KeyModifiers::CONTROL), &kitty),
            b"\x1b[112;5u"
        );
        // Shift が消えない（mods に載る）
        assert_eq!(
            encode_key(
                &key(KeyCode::Char('P'), KeyModifiers::CONTROL | KeyModifiers::SHIFT),
                &kitty
            ),
            b"\x1b[112;6u"
        );
        // Alt+文字も ESC 前置ではなく形式で
        assert_eq!(
            encode_key(&key(KeyCode::Char('b'), KeyModifiers::ALT), &kitty),
            b"\x1b[98;3u"
        );
        // 修飾なしの文字はそのまま本文（disambiguate はテキストを変えない）
        assert_eq!(encode_key(&key(KeyCode::Char('a'), KeyModifiers::NONE), &kitty), b"a");
        // 子が kitty を pop したら legacy（C0）へ戻る
        let plain = parser_after("\x1b[>1u\x1b[<u");
        assert_eq!(
            encode_key(&key(KeyCode::Char('p'), KeyModifiers::CONTROL), &plain),
            [0x10]
        );
    }

    /// 貼り付けは sanitize（ESC = ペースト終端の偽装を除去）+
    /// bracketed paste の包み（有効化した子にだけ）
    #[test]
    fn paste_is_sanitized_and_wrapped_only_when_bracketed() {
        let bracketed = parser_after("\x1b[?2004h");
        assert_eq!(
            encode_paste("hi\x1b[201~!\r\n", &bracketed),
            b"\x1b[200~hi[201~!\r\n\x1b[201~"
        );
        // 包みを有効化していない子には素のまま（sanitize は常に効く）
        let plain = parser_after("");
        assert_eq!(encode_paste("hi\x1b!", &plain), b"hi!");
    }
}
