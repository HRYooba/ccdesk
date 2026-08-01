//! 前景セッションの PTY。**この PTY がセッションそのもの**で、実体は ccdesk の
//! 子プロセスになる（窓を閉じる = プロセスを終わらせる）。
//!
//! **どう起こすかは持たない。** コマンドラインは agent ごとに違うので
//! [`crate::backend`] が組む（ここは組み上がったものを PTY で走らせるだけ）。
//!
//! **一覧の行とは別物**: ここは「今開いている端末」、[`crate::sessions`] は
//! 「一覧に載る行」。プロセスが死んでも行は残る。
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use portable_pty::{native_pty_system, Child, MasterPty, PtySize};

use ccdesk::{new_parser, now_ms, LockExt, Parser};

// 継承させない環境変数の一覧は claude の非公開な形なので
// [`crate::claude_format`] が持つ（外れたときに直す場所を 1 つにするため）
use crate::backend::{Backend, Inject, Launch};
use crate::poll::PtyHint;
use crate::sessions::SessionId;
use crate::theme::HOST_COLORS;

// ローカルスクロール UI は無く、スクロールは claude 自身に任せる設計のため 0
const SCROLLBACK: usize = 0;

/// 描画を見送ってよい上限。**「最後に描いてからの経過」で測る**ので、この値が
/// そのまま「画面がどれだけ古くなり得るか」の上限になる。子が `\e[?2026l` を
/// 落としたまま固まっても、出力が一度も途切れなくても、ここを過ぎたら描く
/// （画面が止まる方が、途中のフレームを掴むより悪い）
pub(crate) const REDRAW_HOLD_MAX: Duration = Duration::from_millis(100);

/// PTY 出力が「静まった」とみなすまでの無出力時間。**子が DEC 2026 を出さない
/// ときのフォールバック**（実際 claude は出さない）。1 回の再描画は 1 回の
/// 読み取りに収まらず複数に分かれて届くが、その間は途切れないので、
/// 途切れ ＝ 描き終わりとみなせる
const OUTPUT_QUIET: Duration = Duration::from_millis(12);

/// このフレームを掴むのを見送るか。**保留の判断はこの関数だけが持つ**
/// （PTY を開かずに試せるよう純関数にしてある）。
///
/// 子が画面を作り替えている途中でカーソル位置を読むと、中間の位置
/// （ステータス行の末尾など）でフレームが確定する。Windows の IME 変換窓は
/// コンソールカーソルにアンカーされるので、これが「打つたびに変換窓が飛ぶ」
/// 症状になる（同じ性質の別経路が [`crate::ui::FrameCursor`] に記録してある）。
///
/// `updating` は子の宣言（DEC 2026）、`since_output` は最後の PTY 出力からの経過、
/// `since_draw` は最後に描いてからの経過
fn hold_frame(updating: bool, since_output: Duration, since_draw: Duration) -> bool {
    if since_draw >= REDRAW_HOLD_MAX {
        return false;
    }
    updating || since_output < OUTPUT_QUIET
}

/// 「まだ出力が続いている」と見なす、最後の出力からの猶予
const BUSY_QUIET: Duration = Duration::from_secs(2);

/// 出力変化ヒューリスティック。**hook も `agents --json` の status も無い行の
/// 最後の手段**（[`crate::poll::row_state`]）で、精度は低い（フォーカスの出入りや
/// 再描画でも動く）。
///
/// **`started`（一度でも出力したか）が要点。** `since_output` の起点は窓を作った
/// 時刻なので、無出力には意味の違う 2 つが混ざる: まだ端末を掴んでいない（起動中）と、
/// 掴んだうえで静か（プロンプトで待機）。真偽値 1 つで持っていた頃はこれを混同し、
/// **セッションを開き直すたびに根拠の無い赤が 2 秒出て**いた
fn pty_hint(started: bool, since_output: Duration) -> PtyHint {
    match (started, since_output < BUSY_QUIET) {
        (false, _) => PtyHint::Starting,
        (true, true) => PtyHint::Writing,
        (true, false) => PtyHint::Quiet,
    }
}

/// DEC private mode 2026（synchronized output）
const SYNC_MODE: u32 = 2026;

/// CSI の走査位置。DEC 2026 の宣言だけを拾うので、これ以上の状態は持たない
#[derive(Default)]
enum ScanState {
    #[default]
    Ground,
    Esc,
    Csi,
}

/// PTY 出力から DEC 2026（synchronized output）の開始・終了だけを拾う走査器。
///
/// **vt100 0.16 は mode 2026 を追わない**（`decset`/`decrst` が並べるモードに
/// 2026 が無く `unhandled_csi` へ落ちる）ため、パーサの状態からは読めない。
/// なので生バイトをここで見る。読み取りは固定長のバッファ単位で、
/// **シーケンスがその境界をまたぐ**ので状態を持つ
#[derive(Default)]
struct SyncScan {
    state: ScanState,
    /// `?` 付きの私用シーケンスか（DECSET/DECRST はこれ）
    private: bool,
    /// DECSET/DECRST ではないと分かった（中間バイト付き ＝ DECRQM など、
    /// あるいは `<` `=` `>` の別の私用マーカー）
    rejected: bool,
    /// 組み立て中のパラメータ
    param: u32,
    /// 今のシーケンスのパラメータに 2026 が居たか
    saw_mode: bool,
    /// 子が宣言している「更新中」の現在値
    updating: bool,
}

impl SyncScan {
    /// 読み取った塊を 1 つ流す。戻り値 ＝ **この塊の中で更新の開始を見たか**
    /// （呼び出し側は取り込みより先にフラグを立てるのに使う）
    fn feed(&mut self, bytes: &[u8]) -> bool {
        let mut began = false;
        for &b in bytes {
            match self.state {
                ScanState::Ground => {
                    if b == 0x1b {
                        self.state = ScanState::Esc;
                    }
                }
                ScanState::Esc => {
                    if b == b'[' {
                        self.state = ScanState::Csi;
                        self.private = false;
                        self.rejected = false;
                        self.param = 0;
                        self.saw_mode = false;
                    } else if b != 0x1b {
                        self.state = ScanState::Ground;
                    }
                }
                ScanState::Csi => match b {
                    0x1b => self.state = ScanState::Esc,
                    b'0'..=b'9' => {
                        self.param = self
                            .param
                            .saturating_mul(10)
                            .saturating_add(u32::from(b - b'0'));
                    }
                    b';' | b':' => self.end_param(),
                    b'?' => self.private = true,
                    // `<` `=` `>` は別の私用マーカー、0x20..=0x2f は中間バイト
                    0x3c..=0x3e | 0x20..=0x2f => self.rejected = true,
                    0x40..=0x7e => {
                        self.end_param();
                        if self.private && !self.rejected && self.saw_mode {
                            match b {
                                b'h' => {
                                    self.updating = true;
                                    began = true;
                                }
                                b'l' => self.updating = false,
                                _ => {}
                            }
                        }
                        self.state = ScanState::Ground;
                    }
                    _ => {}
                },
            }
        }
        began
    }

    /// 組み立て終わったパラメータを 1 つ確定させる
    fn end_param(&mut self) {
        if self.param == SYNC_MODE {
            self.saw_mode = true;
        }
        self.param = 0;
    }
}

pub(crate) struct Session {
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
    /// 子が DEC 2026 で「画面を作り替えている最中」と宣言している間 true
    /// （[`SyncScan`]）。宣言しない子のことは [`hold_frame`] が出力の途切れで見る
    updating: Arc<AtomicBool>,
}

impl Session {
    /// 前景セッションを PTY で起こす。**セッションの実体はこの子プロセス**
    /// （ccdesk を閉じると終わる。行は `sessions.json` に残る）
    pub(crate) fn spawn(
        backend: &dyn Backend,
        session_id: &SessionId,
        cwd: &str,
        rows: u16,
        cols: u16,
        launch: Launch<'_>,
        inject: Option<&Inject>,
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
            .spawn_command(backend.command(session_id, cwd, launch, inject))?;
        drop(pair.slave);

        let parser = Arc::new(Mutex::new(new_parser(rows, cols, SCROLLBACK)));
        // ホスト端末の実色を Responder に渡す（claude の OSC 10/11 テーマ検出へ転送）
        {
            let (fg, bg) = HOST_COLORS.get().copied().unwrap_or((None, None));
            let mut p = parser
                .lock_recover();
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
        let updating = Arc::new(AtomicBool::new(false));
        let parser_clone = parser.clone();
        let writer_clone = writer.clone();
        let last_output_clone = last_output.clone();
        let dirty_clone = dirty.clone();
        let started_clone = started.clone();
        let updating_clone = updating.clone();
        let reader_thread = std::thread::Builder::new().name("pty-reader".to_string());
        let _ = reader_thread.spawn(move || {
            let mut buf = [0u8; 8192];
            let mut scan = SyncScan::default();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        *last_output_clone
                            .lock_recover() =
                            std::time::Instant::now();
                        dirty_clone.store(true, Ordering::Relaxed);
                        started_clone.store(true, Ordering::Relaxed);
                        // 「更新中」の宣言を追う。**開始は取り込みより先に立てる**
                        // （取り込みの途中で UI が覗いても保留が効くように）。
                        // 終了は取り込みの後（画面が宣言に追いついてから降ろす）
                        if scan.feed(&buf[..n]) {
                            updating_clone.store(true, Ordering::Relaxed);
                        }
                        // vt100 0.16 は「右端の全角文字 + リサイズ」で内部 unwrap が
                        // panic する既知バグがある。捕捉してパーサを作り直し継続する
                        // （claude は全面再描画するので画面はすぐ復元される）
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                            || {
                                let mut parser = parser_clone
                                    .lock_recover();
                                parser.process(&buf[..n]);
                                parser.callbacks_mut().take()
                            },
                        ));
                        match result {
                            Ok(response) => {
                                if !response.is_empty() {
                                    let mut writer = writer_clone
                                        .lock_recover();
                                    let _ = writer.write_all(&response);
                                    let _ = writer.flush();
                                }
                            }
                            Err(_) => {
                                let mut guard = parser_clone
                                    .lock_recover();
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
                        updating_clone.store(scan.updating, Ordering::Relaxed);
                    }
                }
            }
        });

        Ok(Self {
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
            updating,
        })
    }

    /// 子が端末を掴んだか（一度でも出力したか。理由は `started` フィールド）
    pub(crate) fn started(&self) -> bool {
        self.started.load(Ordering::Relaxed)
    }

    /// この窓を描くのを見送るか（判断は [`hold_frame`]）。
    /// `since_draw` は呼び出し側が持つ「最後に描いてからの経過」
    pub(crate) fn holds_frame(&self, since_draw: Duration) -> bool {
        let since_output = self
            .last_output
            .lock_recover()
            .elapsed();
        hold_frame(
            self.updating.load(Ordering::Relaxed),
            since_output,
            since_draw,
        )
    }

    /// PTY へバイト列を書く。**この窓への書き込み（キー・貼り付け・マウス・
    /// フォーカス通知）はすべてここを通す**: 書き方（lock → write → flush）と
    /// 失敗の報告を 1 箇所に保つ。失敗をどう扱うかは呼び手が決める
    /// （キー入力は窓を閉じる、フォーカス通知やマウスは落としてよい）
    pub(crate) fn send(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut writer = self.writer.lock_recover();
        writer.write_all(bytes)?;
        writer.flush()
    }

    /// フォーカス変化を PTY へ通知する（DECSET 1004 を有効化した子にだけ送る）
    pub(crate) fn send_focus(&mut self, gained: bool) {
        let wants_focus = self
            .parser
            .lock_recover()
            .callbacks()
            .focus_reporting;
        if !wants_focus {
            return;
        }
        // 通知の取りこぼしは害が小さい（次のフォーカス変化で上書きされる）
        let _ = self.send(if gained { b"\x1b[I" } else { b"\x1b[O" });
    }

    /// 出力変化ヒューリスティック: 直近 2 秒に出力があれば「動いているらしい」
    /// （hook も `agents --json` の status も無い行の最後の手段）。
    /// **生死は見ない**: 生死の観測（try_wait）は呼び手が別に持っていて、
    /// ここでも呼ぶと同じ syscall が 1 フレームに 2 回走る。
    ///
    /// 判断そのものは [`pty_hint`]（PTY を開かずに試せる純関数）
    pub(crate) fn pty_hint(&self) -> PtyHint {
        pty_hint(self.started(), self.last_output.lock_recover().elapsed())
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
        self.parser.lock_recover().screen_mut().set_size(rows, cols);
    }

    pub(crate) fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **無出力には意味の違う 2 つが混ざる。**
    ///
    /// 経過時間の起点は窓を作った時刻。まだ端末を掴んでいない子は「起動中」で、
    /// 掴んだうえで静かなら「プロンプトで待機」。真偽値 1 つで持っていた頃は
    /// この 2 つを混同し、開き直すたびに根拠の無い赤が 2 秒出ていた
    #[test]
    fn an_unwritten_child_is_starting_not_finished() {
        let fresh = Duration::from_millis(0);
        // まだ 1 バイトも出していない ＝ 起動中（猶予を過ぎていても変わらない）
        assert_eq!(pty_hint(false, fresh), PtyHint::Starting);
        assert_eq!(pty_hint(false, BUSY_QUIET * 10), PtyHint::Starting);
        // 掴んだ後は出力の勢いで割れる
        assert_eq!(pty_hint(true, fresh), PtyHint::Writing);
        assert_eq!(pty_hint(true, BUSY_QUIET - Duration::from_millis(1)), PtyHint::Writing);
        assert_eq!(pty_hint(true, BUSY_QUIET), PtyHint::Quiet);
    }

    /// **症状の素**を固定する。子の 1 回の再描画が複数の読み取りに分かれると、
    /// その途中でカーソル位置を読んだ側は中間の位置を掴む。
    ///
    /// ここでは「ステータス行を描いた直後」と「入力欄へ戻した後」を別の塊にして、
    /// 前者だけを取り込んだ時点の `cursor_position()` がステータス行の末尾を
    /// 指すことを見せる。Windows の IME 変換窓はコンソールカーソルにアンカーされる
    /// ので、これがそのまま「打つたびに変換窓がステータス行へ飛ぶ」症状になる
    #[test]
    fn a_redraw_split_across_reads_exposes_an_intermediate_cursor_position() {
        let mut parser = ccdesk::new_parser(10, 40, SCROLLBACK);
        // 1 つ目の塊: 最下部のステータス行を描いたところまで
        parser.process(b"\x1b[?2026h\x1b[9;1Hclaude 1m 33s");
        assert_eq!(
            parser.screen().cursor_position(),
            (8, 13),
            "the status line is not where the cursor was left"
        );
        // 2 つ目の塊: 入力欄へカーソルを戻して更新を閉じる
        parser.process(b"\x1b[3;3H\x1b[?2026l");
        assert_eq!(parser.screen().cursor_position(), (2, 2));
    }

    /// **vt100 は mode 2026 を追わない**ので、パーサの状態からは「更新中」を
    /// 読めない。これが [`SyncScan`] を生バイト側に置いている理由なので、
    /// 前提が変わったら（vt100 が追うようになったら）ここが落ちる
    #[test]
    fn the_parser_alone_cannot_tell_a_synchronized_update_from_a_plain_one() {
        let mut declared = ccdesk::new_parser(4, 8, SCROLLBACK);
        declared.process(b"\x1b[?2026h\x1b[2;2Hx");
        let mut plain = ccdesk::new_parser(4, 8, SCROLLBACK);
        plain.process(b"\x1b[2;2Hx");
        assert_eq!(
            declared.screen().contents(),
            plain.screen().contents(),
            "the parser started tracking mode 2026 — read it from there instead"
        );
        assert_eq!(
            declared.screen().cursor_position(),
            plain.screen().cursor_position()
        );
    }

    /// 宣言の開始と終了を拾い、**読み取りの境界をまたいでも**見失わない
    #[test]
    fn the_scan_follows_a_synchronized_update_split_across_reads() {
        let mut scan = SyncScan::default();
        assert!(!scan.feed(b"\x1b[?20"), "a half sequence began an update");
        assert!(!scan.updating);
        assert!(scan.feed(b"26h\x1b[9;1Hclaude"), "the opening was missed");
        assert!(scan.updating);
        // 閉じるまでは更新中のまま（間の描画では降りない）
        assert!(!scan.feed(b"\x1b[3;3H"));
        assert!(scan.updating);
        assert!(!scan.feed(b"\x1b[?2026l"));
        assert!(!scan.updating, "the closing was missed");
    }

    /// **2026 以外は動かさない。** 他のモード・DECRQM（`CSI ? 2026 $ p` ＝
    /// 状態の問い合わせ）・私用マーカー違いを更新の宣言と取り違えると、
    /// 描画が止まったまま上限まで待つ周が増える
    #[test]
    fn the_scan_ignores_everything_that_is_not_a_2026_toggle() {
        for bytes in [
            &b"\x1b[?2004h"[..],  // bracketed paste
            &b"\x1b[?1049h"[..],  // alternate screen
            &b"\x1b[?2026$p"[..], // DECRQM: 問い合わせであって設定ではない
            &b"\x1b[>2026h"[..],  // 別の私用マーカー
            &b"\x1b[2026h"[..],   // `?` の無い ANSI モード
        ] {
            let mut scan = SyncScan::default();
            assert!(!scan.feed(bytes), "{bytes:?} was taken for an update");
            assert!(!scan.updating, "{bytes:?} was taken for an update");
        }
        // 他のモードと相乗りした 2026 は拾う（DECSET は複数パラメータを取る）
        let mut scan = SyncScan::default();
        assert!(scan.feed(b"\x1b[?1049;2026h"));
        assert!(scan.updating);
    }

    /// 保留の判断。**宣言があるか、出力がまだ静まっていない間は見送る**
    #[test]
    fn a_frame_is_held_while_the_child_is_still_writing_it() {
        let fresh = Duration::from_millis(0);
        // 子が宣言している間は、出力が静まっていても見送る
        assert!(hold_frame(true, Duration::from_secs(1), fresh));
        // 宣言が無くても、出力が途切れていなければ見送る（フォールバック）
        assert!(hold_frame(false, OUTPUT_QUIET / 2, fresh));
        // 静まっていて宣言も無ければ掴む
        assert!(!hold_frame(false, OUTPUT_QUIET, fresh));
    }

    /// **見送りは必ず終わる。** `\e[?2026l` が来なくても、出力が一度も途切れなくても、
    /// 最後に描いてから上限を過ぎたら描く（画面が止まる方が症状より悪い）
    #[test]
    fn holding_gives_up_once_the_screen_would_go_stale() {
        assert!(!hold_frame(true, Duration::from_millis(0), REDRAW_HOLD_MAX));
        assert!(!hold_frame(
            true,
            Duration::from_millis(0),
            REDRAW_HOLD_MAX + Duration::from_secs(10)
        ));
    }
}
