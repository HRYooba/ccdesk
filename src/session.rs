//! 前景セッションの PTY。**この PTY がセッションそのもの**で、実体は ccdesk の
//! 子プロセスになる（窓を閉じる = プロセスを終わらせる）。
//!
//! 起動は新規なら `claude --session-id <uuid> -n <title> [prompt]`、再開なら
//! `claude -r <session-id>`。渡した UUID がそのまま transcript の `sessionId` に
//! なるので、一覧の行（[`crate::sessions::SessionRow`]）と claude 側の記録が
//! 同じ鍵で結びつく。移行の全体像は `docs/foreground-migration.md`。
//!
//! **一覧の行とは別物**: ここは「今開いている端末」、[`crate::sessions`] は
//! 「一覧に載る行」。プロセスが死んでも行は残る。
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

use ccdesk::{new_parser, Parser};

use crate::sessions::SessionId;
use crate::theme::HOST_COLORS;

// ローカルスクロール UI は無く、スクロールは claude 自身に任せる設計のため 0
const SCROLLBACK: usize = 0;

/// **子へ渡さない環境変数。** Claude Code の配下から ccdesk を起動すると、この印が
/// 継承されて子の claude が「別セッションの子」だと誤認し、transcript の保存が
/// 無効になる（実測: `⚠ Transcript saving is off — inherited
/// CLAUDE_CODE_CHILD_SESSION marker`）。
///
/// **`env_clear` は使わない**: PATH・USERPROFILE 等まで落ちて claude が起動しなく
/// なる。落とすのは実測で継承が確認できたこの一覧だけ（[`CommandBuilder::env_remove`]）
const INHERITED_MARKERS: [&str; 8] = [
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_SESSION_NAME",
    "CLAUDE_CODE_SESSION_KIND",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_PID",
    "CLAUDECODE",
    "CLAUDE_JOB_DIR",
];

pub(crate) struct Session {
    pub(crate) name: String,
    /// このウィンドウが動かしているセッション（`claude --session-id` へ渡した値）。
    /// **cwd は持たない**: 行（[`crate::sessions::SessionRow`]）が正本で、
    /// 窓に写しを置くと同じ知識が 2 箇所に増える
    pub(crate) session_id: SessionId,
    pub(crate) parser: Arc<Mutex<Parser>>,
    pub(crate) writer: Arc<Mutex<Box<dyn Write + Send>>>,
    pub(crate) master: Box<dyn MasterPty + Send>,
    pub(crate) child: Box<dyn Child + Send + Sync>,
    pub(crate) size: (u16, u16), // (rows, cols)
    pub(crate) last_output: Arc<Mutex<std::time::Instant>>,
    // PTY から新しい出力が来たら true（再描画が必要かの判定に使う）
    pub(crate) dirty: Arc<AtomicBool>,
    /// 子が一度でも出力したか（＝端末を掴んだ）。**一度立ったら戻らない**ので
    /// `dirty`（毎周降ろす）とは別に持つ。起動直後の打鍵を捨てる門番
    /// （[`crate::app`] の `input_gate`）が降りる合図に使う
    started: Arc<AtomicBool>,
}

/// 起動の種類。**新規と再開でコマンドラインが違う**ことだけをここに持たせる
/// （どちらを使うかを決めるのは呼び出し側 ＝ 行があるか）
pub(crate) enum Launch<'a> {
    /// 新規セッション。`title` は `-n`、`prompt` は最初のメッセージ（空なら渡さない）
    New { title: &'a str, prompt: &'a str },
    /// 既存セッションの再開（`claude -r`）。**cwd の一致が必須**（別 cwd からは
    /// `No conversation found` になる ＝ 行が持つ cwd で開く）
    Resume,
}

/// 出力ヒューリスティックの判定結果（agents --json に居ないときのフォールバック専用。
/// 表示への変換は classify() が一元的に行う）
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum SessionStatus {
    Working,
    NeedsInput,
    Exited,
}

/// 起こす claude のコマンドライン。**PTY を開かずに組める形にしてある**ので、
/// 引数と環境変数の除去をテストで固定できる（どちらも失敗が静かに効く:
/// 引数を間違えれば起動が落ち、除去を落とせば transcript が保存されない）
fn build_command(
    session_id: &SessionId,
    cwd: &str,
    launch: Launch<'_>,
    settings: Option<&std::path::Path>,
) -> CommandBuilder {
    let mut cmd = CommandBuilder::new("claude");
    cmd.cwd(cwd);
    // 継承した親セッションの印を落とす（落とさないと transcript が保存されない）
    for key in INHERITED_MARKERS {
        cmd.env_remove(key);
    }
    // 使用率表示（opt-in）の statusline フック注入
    if let Some(path) = settings {
        cmd.arg("--settings");
        cmd.arg(path);
    }
    match launch {
        Launch::New { title, prompt } => {
            cmd.arg("--session-id");
            cmd.arg(session_id.as_str());
            if !title.is_empty() {
                cmd.arg("-n");
                cmd.arg(title);
            }
            // 空プロンプトは渡さない（"idle — プロンプト待ち" で始まる）
            if !prompt.is_empty() {
                cmd.arg(prompt);
            }
        }
        Launch::Resume => {
            cmd.arg("-r");
            cmd.arg(session_id.as_str());
        }
    }
    cmd
}

impl Session {
    /// 前景セッションを PTY で起こす。**セッションの実体はこの子プロセス**
    /// （ccdesk を閉じると終わる。行は `sessions.json` に残る）
    pub(crate) fn spawn(
        session_id: &SessionId,
        name: &str,
        cwd: &str,
        rows: u16,
        cols: u16,
        launch: Launch<'_>,
        settings: Option<&std::path::Path>,
    ) -> anyhow::Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let child = pair
            .slave
            .spawn_command(build_command(session_id, cwd, launch, settings))?;
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
        let dirty = Arc::new(AtomicBool::new(true));
        let started = Arc::new(AtomicBool::new(false));
        let parser_clone = parser.clone();
        let writer_clone = writer.clone();
        let last_output_clone = last_output.clone();
        let dirty_clone = dirty.clone();
        let started_clone = started.clone();
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
                        dirty_clone.store(true, Ordering::Relaxed);
                        started_clone.store(true, Ordering::Relaxed);
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
            session_id: session_id.clone(),
            parser,
            writer,
            master: pair.master,
            child,
            size: (rows, cols),
            last_output,
            dirty,
            started,
        })
    }

    /// 子が端末を掴んだか（一度でも出力したか。理由は `started` フィールド）
    pub(crate) fn started(&self) -> bool {
        self.started.load(Ordering::Relaxed)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 組み立てたコマンドラインの引数（先頭のプログラム名は落とす）
    fn argv(cmd: &CommandBuilder) -> Vec<String> {
        cmd.get_argv()
            .iter()
            .skip(1)
            .map(|a| a.to_string_lossy().to_string())
            .collect()
    }

    fn id() -> SessionId {
        SessionId::new("8a1c0f52-0b3e-4a6d-9f11-2c7d5e8b0a34")
    }

    /// 新規は `--session-id <uuid> -n <title> [prompt]`。**空プロンプトは渡さない**
    /// （渡すと空メッセージを送ったセッションになる）
    #[test]
    fn a_new_session_passes_its_uuid_title_and_prompt() {
        let cmd = build_command(
            &id(),
            "C:\\dev\\app",
            Launch::New {
                title: "fix login",
                prompt: "fix login form validation",
            },
            None,
        );
        assert_eq!(
            argv(&cmd),
            [
                "--session-id",
                id().as_str(),
                "-n",
                "fix login",
                "fix login form validation",
            ]
        );
        assert_eq!(
            cmd.get_cwd().map(|c| c.to_string_lossy().to_string()),
            Some("C:\\dev\\app".to_string()),
            "cwd is not passed through"
        );

        let cmd = build_command(
            &id(),
            "C:\\dev\\app",
            Launch::New {
                title: "new session",
                prompt: "",
            },
            None,
        );
        assert_eq!(argv(&cmd), ["--session-id", id().as_str(), "-n", "new session"]);
    }

    /// 再開は `-r <session-id>` だけ（`--session-id` は新規採番の指定なので混ぜない）
    #[test]
    fn resuming_passes_only_the_session_id() {
        let cmd = build_command(&id(), "C:\\dev\\app", Launch::Resume, None);
        assert_eq!(argv(&cmd), ["-r", id().as_str()]);
    }

    /// 使用率表示（opt-in）の settings は起動の種類に関係なく前に付く
    #[test]
    fn the_injected_settings_are_passed_when_usage_display_is_on() {
        let path = std::path::Path::new("C:\\Users\\me\\.ccdesk\\inject-settings.json");
        let cmd = build_command(&id(), "C:\\dev\\app", Launch::Resume, Some(path));
        assert_eq!(
            argv(&cmd),
            [
                "--settings",
                path.to_string_lossy().as_ref(),
                "-r",
                id().as_str(),
            ]
        );
    }

    /// **継承した親セッションの印は 1 つ残らず落とす。**
    ///
    /// 残すと子の claude が「別セッションの子」だと誤認して transcript を保存しない
    /// （実測。[`INHERITED_MARKERS`]）。`env_clear` ではなく個別除去なので、
    /// **PATH 等の通常の環境変数は残っている**ことも併せて固定する。
    ///
    /// **親のプロセス環境を一時的に触る**（そうしないと CI のように印が居ない環境で
    /// 検査が空振りする）。触るのはこの一覧の名前だけで、他のテストが読む変数
    /// （`USERPROFILE` 等）とは重ならない。復元は組み立ての直後に行い、
    /// アサートが失敗しても残さない
    #[test]
    fn the_inherited_session_markers_are_removed_but_the_rest_of_the_env_is_kept() {
        for key in INHERITED_MARKERS {
            unsafe { std::env::set_var(key, "1") };
        }
        let cmd = build_command(&id(), "C:\\dev\\app", Launch::Resume, None);
        for key in INHERITED_MARKERS {
            unsafe { std::env::remove_var(key) };
        }
        for key in INHERITED_MARKERS {
            assert_eq!(cmd.get_env(key), None, "{key} is inherited by the child");
        }
        // 個別除去なので、通常の環境変数は落ちない（env_clear ではない）
        assert!(
            cmd.iter_full_env_as_str().any(|(k, _)| k.eq_ignore_ascii_case("PATH")),
            "PATH was dropped too — claude cannot start"
        );
    }
}
