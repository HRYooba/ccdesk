// ccdesk: Claude Code 用のセッション管理 TUI。portable-pty で claude を起動 →
// vt100 でパース → tui-term で描画。
// マウス主体の操作: クリックでセッション切替・フォーカス / 行頭の = で二次操作のメニュー /
// 境界線ドラッグで幅変更。ターミナルフォーカス中のキーは PTY へ素通し。
// 予約は Ctrl+Q（終了）と Alt+←→（ペインフォーカス移動）のみ

use std::sync::{Arc, Mutex};

use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture,
};
use ccdesk::{load_setting, log_error};

mod app;
mod claude_format;
mod cli;
mod git;
mod hooks;
mod keys;
mod poll;
mod session;
mod sessions;
mod source;
mod theme;
mod title;
mod ui;
mod update;
mod usage;

use app::{open_session, run, App, Focus, RightView, SelfUpdate};
use cli::{print_usage, print_usage_error, run_doctor, show_logs, update_self};
use poll::FooterInfo;
use source::{DataSource, DemoSource, LiveSource};
use theme::HOST_COLORS;

fn main() -> anyhow::Result<()> {
    // **エラーログの出力先を決めるのはここだけ。** 決めていないプロセス（＝ テスト）
    // では `log_error` は何も書かない ＝ `cargo test` がユーザーの
    // `~/.ccdesk/error.log` を汚さない（[`ccdesk::enable_error_log`]）
    ccdesk::enable_error_log();
    // アカウント切り替えを撤去したので、残っている保管ファイル（**トークンを含む**）を
    // 消す。**エラーログを有効にした直後**に置くのは、消したことを 1 行残すため
    // （[`ccdesk::purge_account_store`]）
    ccdesk::purge_account_store();
    let mut demo = false;
    // フラグ/サブコマンドは TUI 起動前に処理
    match std::env::args().nth(1).as_deref() {
        Some("--version" | "-V" | "version") => {
            println!("ccdesk {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("doctor") => return run_doctor(),
        Some("logs") => return show_logs(),
        // セッションの状態を受け取る内部フック（`--settings` で注入し、
        // 子の claude が turn ごとに `ccdesk hook <event>` として起こす）
        Some("hook") => {
            return hooks::run_hook(std::env::args().nth(2).unwrap_or_default().as_str())
        }
        Some("update") => return update_self(),
        Some("--help" | "-h" | "help") => {
            print_usage();
            return Ok(());
        }
        // スクリーンショット撮影用: セッション・アカウント等を架空データで描画（非公開フラグ）
        Some("--demo") => demo = true,
        // **stdout へ出さない**（理由は [`cli::print_usage_error`]）
        Some(other) => {
            print_usage_error(other);
            std::process::exit(2);
        }
        None => {}
    }

    // 自己更新で退避した <exe>.old を掃除する。更新した当のプロセスが掴んでいる
    // 間は消せないので、掃除は次にプロセスを起こしたときになる（失敗は無視する）。
    // ここと doctor の 2 箇所で行い、`ccdesk update` の出力もその 2 つを案内する。
    // TUI 初期化より前に済ませて画面に影響させない
    update::cleanup_old_exe();

    // 使用率表示の opt-in（`~/.ccdesk/config.json` の `"usage_display": "on"`）。
    //
    // **既定で取らないのは資源の話ではない。** 取得は課金ゼロ・枠を消費せず、周期 2 分は
    // 既存の `claude agents --json`（2 秒ごと）の 1/60 なので、負荷を理由に切る意味は無い。
    // 切ってあるのは、これが **ccdesk で唯一「無人で Anthropic のサーバーへ出る」経路**
    // だから（他のポーリングはローカルのファイルとプロセスしか見ない）。Consumer Terms
    // 第 3 節は API キー以外での自動アクセスを禁じており、公式 CLI の文書化された機能を
    // 本人のサブスクで本人のために使う形は「explicitly permit」側に収まると読めるが、
    // **断定はできない**。であれば、無人の通信を始めるかどうかは利用者が決めるべきで、
    // ccdesk が全員に代わって決めてよいものではない。
    //
    // **判断はここ 1 箇所**で、以降は供給元の中に閉じる
    let usage_display = load_setting("usage_display").as_deref() == Some("on");
    // 使用率の更新を run ループへ伝える旗と、取得中かどうか。供給元と App が同じものを持つ
    let usage_dirty = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let usage_fetching = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // demo / 実データの選択はこの 1 箇所だけ。以降のコードは供給元を通すので
    // 「今 demo か」を問う分岐を持たない（＝分岐の書き漏らしで実データが漏れない）
    let source: Arc<dyn DataSource> = if demo {
        Arc::new(DemoSource)
    } else {
        Arc::new(LiveSource::new(
            usage_display,
            Arc::clone(&usage_dirty),
            Arc::clone(&usage_fetching),
        ))
    };
    // セッション一覧・フッター・ウィンドウ状態はすべて供給元から受け取る
    let sessions = source.sessions();
    // hook（子の claude が書く state）の写しも起動時に 1 度読む。
    // 以降は一覧の読み直しと同じ周期で取り直す（app::adopt_hook_states）
    let hook_states = source.hook_states();
    // 撮影用の固定 state（実データでは空）。窓を持たない行を「動いている」ものとして
    // 描くための表で、起動時に 1 度受け取れば足りる（撮影データは動かない）
    let fixed_states = source.fixed_states();
    let footer = source.footer();
    let window = source.window_state();

    // ホスト端末の実 fg/bg を OSC 10/11 で照会（raw mode に入る前。
    // 照会の作法は theme 側の 1 実装 ＝ doctor と同じ経路を通る）
    let _ = HOST_COLORS.set(theme::query_host_colors());

    let mut terminal = ratatui::init();
    // panic は ~/.ccdesk/error.log へ記録（TUI は画面ごと消えて panic 表示が読めない）。
    // PTY リーダースレッドの捕捉済み panic（vt100 バグ）以外は
    // ratatui の復旧 hook（raw mode / alt screen の解除）へ連鎖させる
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log_error(&format!(
            "panic (thread: {:?}): {info}\n{}",
            std::thread::current().name(),
            std::backtrace::Backtrace::force_capture()
        ));
        if std::thread::current().name() != Some("pty-reader") {
            // ratatui の復旧 hook は raw mode 解除 + alt screen 離脱**だけ**なので、
            // main 末尾の正常経路が解除している 3 モードはここでも戻す
            // （unwind ではあの 3 行に到達しない ＝ 戻さないとクラッシュ後の
            // シェルにマウスエスケープ列 `<35;12;5M` 等が流れ込み続ける）
            let _ = crossterm::execute!(
                std::io::stdout(),
                DisableFocusChange,
                DisableMouseCapture,
                DisableBracketedPaste
            );
            prev_hook(info);
        }
    }));
    crossterm::execute!(
        std::io::stdout(),
        EnableBracketedPaste,
        EnableMouseCapture,
        EnableFocusChange
    )?;

    let area = terminal.get_frame().area();
    let mut app = App {
        windows: Vec::new(),
        active: 0,
        agents: Vec::new(),
        agents_shared: Arc::new(Mutex::new(Vec::new())),
        agents_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        sessions,
        hook_states,
        fixed_states,
        // 起動時の見え方を先に控える（run ループの 1 周目が「変わった」と誤解しない）
        hook_stamp: source.hook_stamp(),
        titles: source.titles(),
        last_scan: std::time::Instant::now(),
        last_live_scan: std::time::Instant::now(),
        // 保存値はそのまま持つ（画面に出す桁数は端末幅から導くので丸めない ＝
        // 狭い端末で一度起動しただけでユーザーの選んだ幅が失われない）
        sidebar_width: window.sidebar_width,
        dragging: false,
        last_drag_resize: std::time::Instant::now(),
        term_size: (area.width, area.height),
        sidebar_rows: Vec::new(),
        // 初回 draw が実際に積んだ数で上書きする（それまでは行が無いので 0）
        sidebar_header_rows: 0,
        sidebar_scroll: 0,
        sidebar_follow_sel: false,
        hovered: None,
        selection: app::SidebarPos::Row(0),
        // 起動時の復元でペインが指す行へは最初の描画で揃う（None ＝ まだ揃えていない）
        pane_shown: None,
        dispatch_cwd: window.dispatch_cwd,
        right_view: RightView::Sessions,
        footer,
        footer_shared: Arc::new(Mutex::new(FooterInfo::default())),
        footer_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        footer_refresh: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        claude_updating: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        ccdesk_update: Arc::new(Mutex::new(SelfUpdate::Idle)),
        ccdesk_latest: None,
        ccdesk_latest_shared: Arc::new(Mutex::new(None)),
        ccdesk_latest_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        // 起動時の値は供給元から 1 度受け取る（撮影用は固定値、実データは
        // まだ取れていないので Unknown ＝ 何も描かない）
        usage: source.usage(),
        usage_dirty,
        usage_fetching,
        input_gate: None,
        notice: None,
        grouping: window.grouping,
        projects: window.projects,
        popup: None,
        focus: Focus::Terminal,
        spinner_active: false,
        source,
    };
    // 既にあるセッションのフォルダを登録へ埋め戻す（以前から使っているフォルダの
    // 見出しが、最後のセッションを消した時点で消えないように）。一覧を読んだ後・
    // 画面を組む前のこの位置に置く: 埋め戻しは初回の一覧に効く必要がある
    app::backfill_projects(&mut app);
    // **最初の描画より前に transcript を解決して名前を読む。** 走査の結果を持つのは
    // Titles のキャッシュだけなので、ここで 1 度走らせないと最初の周期（2 秒）まで
    // 全部の行が `new session` に見える。未記録の行の解決し直しも同じ 1 回で済む
    app::refresh_transcripts(&mut app);
    // バックグラウンド取得の起動。撮影用の供給元は 1 本も起こさないので、
    // ここに `if !demo` は要らない
    app.source.spawn_pollers(app.poll_sinks());
    // 前回開いていた画面を復元: セッションを見ていたなら `claude -r` で再開、
    // それ以外は new session 画面
    match window.last_view.map(sessions::SessionId::new) {
        Some(id) if app.row(&id).is_some() => {
            open_session(&mut app, &id);
            if app.windows.is_empty() {
                app.open_new_view(); // 再開に失敗したときのフォールバック
            }
        }
        _ => app.open_new_view(),
    }

    let result = run(&mut terminal, &mut app);

    // 終了時に子プロセスを残さない。**行は残す**（`sessions.json` はそのまま ＝
    // 次の起動で一覧に出て `claude -r` で再開できる）
    app::kill_sessions_on_exit(&mut app);
    let _ = crossterm::execute!(
        std::io::stdout(),
        DisableFocusChange,
        DisableMouseCapture,
        DisableBracketedPaste
    );
    ratatui::restore();
    // ratatui::restore() は raw mode 解除 + alt screen 離脱だけで DECTCEM を戻さない。
    // カーソルの表示状態を戻すのは Terminal の Drop（hidden_cursor が立っているときだけ
    // `?25h` を出す）。alt screen を出た後に出さないと通常画面に効かない端末があるため、
    // restore() の後で明示的に drop する（この順序は意味を持つので暗黙の drop に任せない）
    drop(terminal);
    result
}
