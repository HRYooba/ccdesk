//! 前景セッションの PTY。**この PTY がセッションそのもの**で、実体は ccdesk の
//! 子プロセスになる（窓を閉じる = プロセスを終わらせる）。
//!
//! 起動は新規なら `claude --session-id <uuid> [prompt]`、再開なら
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

use ccdesk::{new_parser, now_ms, Parser};

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
    /// **この窓が claude を起こした時刻**（epoch ms）。hook が書いた state が
    /// 今の実行のものか前回の実行の残骸かを決める材料
    /// （[`crate::hooks::HookStates::get`]）。
    ///
    /// **正本をここに置く理由**: 前景セッションの実体はこの子プロセスなので、
    /// 「いつ起こしたか」を正確に知っているのはここだけ。`claude agents --json` の
    /// `startedAt` は 2 秒周期の観測で、再開直後は前回の実行の値が残っているうえ、
    /// 自分の子の pid が載らない環境（npm 版）では値そのものが来ない
    pub(crate) started_at: u64,
    pub(crate) last_output: Arc<Mutex<std::time::Instant>>,
    // PTY から新しい出力が来たら true（再描画が必要かの判定に使う）
    pub(crate) dirty: Arc<AtomicBool>,
    /// 子が一度でも出力したか（＝端末を掴んだ）。**一度立ったら戻らない**ので
    /// `dirty`（毎周降ろす）とは別に持つ。起動直後の打鍵を捨てる門番
    /// （[`crate::app`] の `input_gate`）が降りる合図に使う
    started: Arc<AtomicBool>,
}

/// 起動の種類。**新規と再開でコマンドラインが違う**ことだけをここに持たせる
/// （どちらを使うかを決めるのは呼び出し側 ＝ transcript があるか）
pub(crate) enum Launch<'a> {
    /// 新規セッション。`prompt` は最初のメッセージ（空なら渡さない）
    New { prompt: &'a str },
    /// 既存セッションの再開（`claude -r`）。**cwd の一致が必須**（別 cwd からは
    /// `No conversation found` になる ＝ 行が持つ cwd で開く）。
    /// **transcript が無い行には使えない**（会話が無いので `-r` が見つけられない）
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
        // **`-n <title>` は渡さない。** claude は `-n` で渡した名前を transcript の
        // `custom-title` として残す（実測）ので、ccdesk が組んだ名前を渡すと
        // 「ユーザーが付けた名前」の位置が埋まる ＝ 表示名がそこで凍る
        // （プロンプト無しなら "new session" のまま・claude 側の AI 生成名も付かない）。
        // 表示名は ccdesk 自身が行に持つので、渡す必要も無い
        Launch::New { prompt } => {
            cmd.arg("--session-id");
            cmd.arg(session_id.as_str());
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

/// **claude の入力欄の行頭に描かれる文字。**
///
/// 正本は実際に描かれた画面で、推測ではない: v2.1.220 を PTY で起こして vt100 の
/// スクリーンを読むと、入力行は `❯`（U+276F）+ U+00A0（no-break space）で始まる。
/// 枠付きの入力欄（`│ > `）だった頃の ASCII の `>` も残してあるのは、claude の版で
/// この文字が変わり得るため。
///
/// **候補はこの 1 つの表だけが持つ**（[`input_is_empty`] が引く）ので、
/// 版が変わって文字が増えてもここへ足すだけで済む。**表に無い文字で描かれた場合は
/// 「入力欄が見つからない」＝ 送らない側へ倒れる**ので、外したときの害は
/// 「名前が transcript 経由になる」だけで、打ちかけを消す方へは倒れない
const PROMPT_MARKS: [char; 2] = ['❯', '>'];

/// カーソルより左の 1 行から「入力欄が空か」を読む（[`Session::input_line_is_empty`] の
/// 判断。画面を組まずに検査できる）。
///
/// 最後の [`PROMPT_MARKS`] の文字より右に文字が無ければ空。**プロンプトが無ければ
/// 空とは言わない** ＝ 判断がつかないときは「送れない」側へ倒れる。
///
/// 空白の判定に `trim` を使うのは、実測の入力行がプロンプトの直後に U+00A0 を
/// 置くため（Unicode の White_Space なので `trim` が落とす ＝ ASCII 空白だけを
/// 見る書き方だと「空でない」に化ける）
fn input_is_empty(line_up_to_cursor: &str) -> bool {
    match line_up_to_cursor
        .char_indices()
        .rev()
        .find(|(_, ch)| PROMPT_MARKS.contains(ch))
    {
        Some((at, mark)) => line_up_to_cursor[at + mark.len_utf8()..].trim().is_empty(),
        None => false,
    }
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
            started_at: now_ms(),
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

    /// **claude の入力欄が空か**（＝ こちらから 1 行送っても打ちかけを壊さないか）。
    ///
    /// 判定は画面のカーソル行だけを見る: 行内の最後のプロンプト文字
    /// （[`PROMPT_MARKS`]）からカーソル位置までに文字が無ければ空。
    ///
    /// **この判定が守るのは 1 つだけ ＝ ユーザーの打ちかけを消さないこと。**
    /// 実測（v2.1.220）では応答生成中もカーソルは空の入力行に居るので、
    /// そのとき送った行は claude が次の turn へ送るぶんとして受け取る（消えない）。
    /// 選択肢を並べるダイアログはカーソル行がプロンプト行ではない（トラストの
    /// 確認画面では `Enter to confirm · Esc to cancel` の行）か、行に選択肢の文字が
    /// 続くかのどちらかなので、どちらも「空ではない」側へ落ちる。
    ///
    /// **claude の画面の形に依存する判定なので、外したときに倒れる向きを選んである**:
    /// 形が変わってプロンプトが見つからなくなっても、起きるのは「PTY へ送らず
    /// transcript へ書く」＝ 従来どおりの経路で、ユーザーの打ちかけを消す方へは倒れない
    pub(crate) fn input_line_is_empty(&self) -> bool {
        let parser = self
            .parser
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let screen = parser.screen();
        let (row, col) = screen.cursor_position();
        input_is_empty(&screen.contents_between(row, 0, row, col))
    }

    /// 入力欄へ 1 行送る（`/rename <名前>` のようなスラッシュコマンド）。
    ///
    /// **改行まで含めて 1 回の write** にするのは、reader スレッドが書き戻す端末応答と
    /// 行の途中で混ざらないため（[`Self::send_focus`] と同じ作り）。
    /// 行末は打鍵と同じ `\r`（[`crate::keys::encode_key`] の Enter）
    pub(crate) fn send_line(&mut self, line: &str) {
        let payload = format!("{line}\r");
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = writer.write_all(payload.as_bytes());
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

    /// 新規は `--session-id <uuid> [prompt]`。**空プロンプトは渡さない**
    /// （渡すと空メッセージを送ったセッションになる）。
    ///
    /// **`-n <title>` は 1 つも渡さない**: claude は `-n` の名前を transcript の
    /// `custom-title` として残すので、ccdesk が組んだ名前を渡すと表示名が
    /// そこで凍る（`new session` のまま張り付く実害があった）
    #[test]
    fn a_new_session_passes_its_uuid_and_prompt_but_never_a_name() {
        let cmd = build_command(
            &id(),
            "C:\\dev\\app",
            Launch::New {
                prompt: "fix login form validation",
            },
            None,
        );
        assert_eq!(
            argv(&cmd),
            ["--session-id", id().as_str(), "fix login form validation"]
        );
        assert_eq!(
            cmd.get_cwd().map(|c| c.to_string_lossy().to_string()),
            Some("C:\\dev\\app".to_string()),
            "cwd is not passed through"
        );

        // プロンプト無しは UUID だけ（`claude --session-id <uuid>`）
        let cmd = build_command(&id(), "C:\\dev\\app", Launch::New { prompt: "" }, None);
        assert_eq!(argv(&cmd), ["--session-id", id().as_str()]);
        assert!(
            !argv(&cmd).contains(&"-n".to_string()),
            "the name argument came back: {:?}",
            argv(&cmd)
        );
    }

    /// 再開は `-r <session-id>` だけ（`--session-id` は新規採番の指定なので混ぜない）
    #[test]
    fn resuming_passes_only_the_session_id() {
        let cmd = build_command(&id(), "C:\\dev\\app", Launch::Resume, None);
        assert_eq!(argv(&cmd), ["-r", id().as_str()]);
    }

    /// **入力欄が空だと言えるのは、プロンプト文字が見えていてその右が空のときだけ。**
    ///
    /// 入力は**実際に描かれた画面の写し**を使う（claude v2.1.220 を PTY で起こし、
    /// vt100 のカーソル行を `contents_between` で読んだもの）。理想化した文字列で
    /// 検査していた頃は、この関数が ASCII の `>` を探していて実機では一度も
    /// 真にならないのに、テストだけが通っていた
    #[test]
    fn the_input_is_only_empty_when_the_prompt_is_visible_and_nothing_follows_it() {
        // 実測: 入力行は `❯` + U+00A0 で始まる（カーソルはその直後）
        assert!(input_is_empty("\u{276f}\u{a0}"));
        // 実測: 打ちかけの文字があるとプロンプトの右に残る ＝ 送ると混ざる
        assert!(!input_is_empty("\u{276f}\u{a0}half-typed"));
        assert!(!input_is_empty("\u{276f}\u{a0}/ren"));
        // 実測: 選択肢のダイアログは同じ `❯` を選択マーカーに使うが、右に文字が続く
        assert!(!input_is_empty("\u{276f} 1. Yes, I trust this folder"));
        // 実測: ダイアログ表示中のカーソル行（プロンプトが見えない）
        assert!(!input_is_empty(" Enter to confirm \u{b7} Esc to cancel"));
        // 枠付きの入力欄だった頃の形（[`PROMPT_MARKS`] に残してある ASCII の `>`）
        assert!(input_is_empty("│ > "));
        assert!(input_is_empty(">"));
        assert!(!input_is_empty("│ > half-typed message"));
        // プロンプトが見えない行はすべて「空ではない」側
        for line in ["", "  ", "✻ Thinking…", "│ 1. Yes, allow once"] {
            assert!(!input_is_empty(line), "claimed the input is empty for {line:?}");
        }
        // 出力に `>` が出ていても、その右に文字があれば空とは言わない
        assert!(!input_is_empty("  => result"));
    }

    /// 送る 1 行は**改行と制御文字を含まない**（PTY へ生で流すので、混ざると
    /// 別の打鍵として解釈される）。畳むのは [`crate::title::title_text`] 1 箇所
    #[test]
    fn a_line_sent_to_the_pty_carries_no_control_characters() {
        let folded = crate::title::title_text("new\nname\twith\rbreaks");
        let line = format!("/rename {folded}");
        assert_eq!(line, "/rename new name with breaks");
        assert!(
            !line.chars().any(|c| c.is_control()),
            "a control character survived into the line: {line:?}"
        );
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
