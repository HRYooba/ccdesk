//! キー入力の VT エンコードと、マウスイベントの PTY 転送。
use std::io::Write;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use ccdesk::Parser;

use crate::app::App;

/// マウスイベントを claude が要求したプロトコル（SGR 前提）で PTY へ転送する。
/// claude は AnyMotion(1003) + SGR(1006) を有効化してくる。
pub(crate) fn forward_mouse(app: &mut App, mouse: &MouseEvent) -> anyhow::Result<()> {
    use vt100::{MouseProtocolEncoding, MouseProtocolMode};

    // 右ペイン内側（枠線 1px）基準の x 原点。**当たり判定は描画と同じ導出幅**
    // （[`crate::app::sidebar_cols`]）で、窓を借りる前に取る
    let ox = crate::app::sidebar_cols(app) + 1;
    let window = &mut app.windows[app.active];
    let (mode, encoding, size) = {
        let parser = window.parser.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let screen = parser.screen();
        (
            screen.mouse_protocol_mode(),
            screen.mouse_protocol_encoding(),
            screen.size(),
        )
    };
    if mode == MouseProtocolMode::None || encoding != MouseProtocolEncoding::Sgr {
        return Ok(()); // claude は SGR(1006) を有効化する。他エンコーディングは対象外
    }

    let oy = 1;
    if mouse.column < ox || mouse.row < oy {
        return Ok(());
    }
    let x = mouse.column - ox + 1;
    let y = mouse.row - oy + 1;
    if x > size.1 || y > size.0 {
        return Ok(());
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
        _ => return Ok(()),
    };
    // 修飾キー: Shift +4 / Alt +8 / Ctrl +16（xterm 準拠）
    let code = code
        + 4 * u8::from(mouse.modifiers.contains(KeyModifiers::SHIFT))
        + 8 * u8::from(mouse.modifiers.contains(KeyModifiers::ALT))
        + 16 * u8::from(mouse.modifiers.contains(KeyModifiers::CONTROL));

    let suffix = if release { 'm' } else { 'M' };
    let seq = format!("\x1b[<{code};{x};{y}{suffix}");
    let mut writer = window.writer.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    writer.write_all(seq.as_bytes())?;
    writer.flush()?;
    Ok(())
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

    // Alt は「ESC 前置」か「修飾コード入りシーケンス」のどちらか一方でだけ表す。
    // 両方付けると余分な孤立 ESC が子プロセスに Esc キー押下として解釈される。
    // 修飾コード入り（CSI u / CSI 27;m;c~ / CSI 1;mX / CSI n;m~）は mods に Alt を
    // 含むため前置しない。前置するのは legacy バイト列（C0・生文字・\r 等）だけ
    let mut out: Vec<u8> = Vec::new();
    match key.code {
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
        KeyCode::Enter => {
            if !functional(&mut out, 13) {
                if alt {
                    out.push(0x1b);
                }
                out.push(b'\r');
            }
        }
        KeyCode::Tab => {
            if !functional(&mut out, 9) {
                if alt {
                    out.push(0x1b);
                }
                out.push(b'\t');
            }
        }
        KeyCode::BackTab => out.extend_from_slice(b"\x1b[Z"),
        KeyCode::Backspace => {
            if !functional(&mut out, 127) {
                if alt {
                    out.push(0x1b);
                }
                out.push(0x7f);
            }
        }
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
