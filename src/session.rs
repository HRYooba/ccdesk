//! bg セッションへの attach クライアント PTY。実体は常に supervisor 側にある。
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

use ccdesk::{new_parser, Parser};

use crate::theme::HOST_COLORS;

// ローカルスクロール UI は無く、スクロールは claude 自身に任せる設計のため 0
const SCROLLBACK: usize = 0;

pub(crate) struct Session {
    pub(crate) name: String,
    pub(crate) cwd: String,
    // `claude attach <id>` で開いたときの id（重複 attach 防止）
    pub(crate) attach_id: Option<String>,
    pub(crate) parser: Arc<Mutex<Parser>>,
    pub(crate) writer: Arc<Mutex<Box<dyn Write + Send>>>,
    pub(crate) master: Box<dyn MasterPty + Send>,
    pub(crate) child: Box<dyn Child + Send + Sync>,
    pub(crate) size: (u16, u16), // (rows, cols)
    pub(crate) last_output: Arc<Mutex<std::time::Instant>>,
    // PTY から新しい出力が来たら true（再描画が必要かの判定に使う）
    pub(crate) dirty: Arc<std::sync::atomic::AtomicBool>,
    // agents --json で一度でも pid を観測したか。true → pid 消失 = セッション終了
    // （終了・外部 stop 追従でウィンドウを閉じる判定。停止中への attach 復帰を誤検知しない）
    pub(crate) seen_alive: bool,
}

/// 出力ヒューリスティックの判定結果（agents --json に居ないときのフォールバック専用。
/// 表示への変換は classify() が一元的に行う）
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum SessionStatus {
    Working,
    NeedsInput,
    Exited,
}

impl Session {
    /// bg セッションへの attach クライアントとして PTY を開く。
    /// セッションの実体は常に supervisor 側（公式 Agent View と同じライフサイクル）
    pub(crate) fn spawn(name: &str, cwd: &str, rows: u16, cols: u16, attach_id: &str) -> anyhow::Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut cmd = CommandBuilder::new("claude");
        cmd.cwd(cwd);
        cmd.arg("attach");
        cmd.arg(attach_id);
        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        let parser = Arc::new(Mutex::new(new_parser(rows, cols, SCROLLBACK)));
        // ホスト端末の実色を Responder に渡す（claude の OSC 10/11 テーマ検出へ転送）
        {
            let (fg, bg) = HOST_COLORS.get().copied().unwrap_or((None, None));
            let mut p = parser
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            p.callbacks_mut().host_fg = fg;
            p.callbacks_mut().host_bg = bg;
        }
        let mut reader = pair.master.try_clone_reader()?;
        let writer: Arc<Mutex<Box<dyn Write + Send>>> =
            Arc::new(Mutex::new(pair.master.take_writer()?));

        // 裏セッションでも読み続ける（切替時に画面を即座に復元するため）。
        // 端末クエリ（CPR 等）への応答もここで PTY へ書き戻す。
        let last_output = Arc::new(Mutex::new(std::time::Instant::now()));
        let dirty = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let parser_clone = parser.clone();
        let writer_clone = writer.clone();
        let last_output_clone = last_output.clone();
        let dirty_clone = dirty.clone();
        let reader_thread = std::thread::Builder::new().name("pty-reader".to_string());
        let _ = reader_thread.spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        *last_output_clone
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            std::time::Instant::now();
                        dirty_clone.store(true, std::sync::atomic::Ordering::Relaxed);
                        // vt100 0.16 は「右端の全角文字 + リサイズ」で内部 unwrap が
                        // panic する既知バグがある。捕捉してパーサを作り直し継続する
                        // （claude は全面再描画するので画面はすぐ復元される）
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                            || {
                                let mut parser = parser_clone
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                parser.process(&buf[..n]);
                                parser.callbacks_mut().take()
                            },
                        ));
                        match result {
                            Ok(response) => {
                                if !response.is_empty() {
                                    let mut writer = writer_clone
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                                    let _ = writer.write_all(&response);
                                    let _ = writer.flush();
                                }
                            }
                            Err(_) => {
                                let mut guard = parser_clone
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                // 端末モードを退避してから作り直す（claude は画面は再描画するが
                                // モード再送はしないため、失うとマウス・ペースト等が死ぬ）
                                let (rows, cols) = guard.screen().size();
                                let mut replay = String::new();
                                {
                                    use vt100::{MouseProtocolEncoding, MouseProtocolMode};
                                    let s = guard.screen();
                                    if s.alternate_screen() {
                                        replay.push_str("\x1b[?1049h");
                                    }
                                    match s.mouse_protocol_mode() {
                                        MouseProtocolMode::Press => replay.push_str("\x1b[?9h"),
                                        MouseProtocolMode::PressRelease => {
                                            replay.push_str("\x1b[?1000h");
                                        }
                                        MouseProtocolMode::ButtonMotion => {
                                            replay.push_str("\x1b[?1002h");
                                        }
                                        MouseProtocolMode::AnyMotion => {
                                            replay.push_str("\x1b[?1003h");
                                        }
                                        MouseProtocolMode::None => {}
                                    }
                                    if s.mouse_protocol_encoding() == MouseProtocolEncoding::Sgr {
                                        replay.push_str("\x1b[?1006h");
                                    }
                                    if s.bracketed_paste() {
                                        replay.push_str("\x1b[?2004h");
                                    }
                                    if s.application_cursor() {
                                        replay.push_str("\x1b[?1h");
                                    }
                                    if s.hide_cursor() {
                                        replay.push_str("\x1b[?25l");
                                    }
                                }
                                let kitty_flags = guard.callbacks().kitty_flags;
                                let modify_other_keys = guard.callbacks().modify_other_keys;
                                let focus_reporting = guard.callbacks().focus_reporting;
                                let host_fg = guard.callbacks().host_fg;
                                let host_bg = guard.callbacks().host_bg;
                                *guard = new_parser(rows, cols, SCROLLBACK);
                                guard.process(replay.as_bytes());
                                guard.callbacks_mut().kitty_flags = kitty_flags;
                                guard.callbacks_mut().modify_other_keys = modify_other_keys;
                                guard.callbacks_mut().focus_reporting = focus_reporting;
                                guard.callbacks_mut().host_fg = host_fg;
                                guard.callbacks_mut().host_bg = host_bg;
                                let _ = guard.callbacks_mut().take();
                            }
                        }
                    }
                }
            }
        });

        Ok(Self {
            name: name.to_string(),
            cwd: cwd.to_string(),
            attach_id: Some(attach_id.to_string()),
            parser,
            writer,
            master: pair.master,
            child,
            size: (rows, cols),
            last_output,
            dirty,
            seen_alive: false,
        })
    }

    /// フォーカス変化を PTY へ通知する（DECSET 1004 を有効化した子にだけ送る）
    pub(crate) fn send_focus(&mut self, gained: bool) {
        let wants_focus = self
            .parser
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .callbacks()
            .focus_reporting;
        if !wants_focus {
            return;
        }
        let seq: &[u8] = if gained { b"\x1b[I" } else { b"\x1b[O" };
        let mut writer = self.writer.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = writer.write_all(seq);
        let _ = writer.flush();
    }

    /// 出力変化ヒューリスティックによるステータス判定（registry に居ないときのフォールバック）
    pub(crate) fn status_heuristic(&mut self) -> SessionStatus {
        if !self.alive() {
            return SessionStatus::Exited;
        }
        if self.last_output.lock().unwrap_or_else(std::sync::PoisonError::into_inner).elapsed() < Duration::from_secs(2) {
            SessionStatus::Working
        } else {
            SessionStatus::NeedsInput
        }
    }


    pub(crate) fn resize(&mut self, rows: u16, cols: u16) {
        if self.size == (rows, cols) {
            return;
        }
        self.size = (rows, cols);
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        self.parser.lock().unwrap_or_else(std::sync::PoisonError::into_inner).screen_mut().set_size(rows, cols);
    }

    pub(crate) fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}
