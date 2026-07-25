// ccdesk: Claude Desktop の TUI 版。portable-pty で claude を起動 → vt100 でパース → tui-term で描画
// マウス主体の操作: クリックでセッション切替・フォーカス / ☰ = stop・delete メニュー /
// 境界線ドラッグで幅変更。ターミナルフォーカス中のキーは PTY へ素通し。
// 予約は Ctrl+Q（終了）と Alt+←→（ペインフォーカス移動）のみ

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture,
};
use ccdesk::{load_setting, log_error};

mod accounts;
mod app;
mod cli;
mod keys;
mod poll;
mod session;
mod source;
mod theme;
mod ui;
mod update;

use app::{clamp_sidebar, instant_ago, open_short, run, App, Focus, RightView};
use cli::{print_usage, run_doctor, show_logs, statusline_hook, update_self};
use poll::FooterInfo;
use source::{DataSource, DemoSource, LiveSource};
use theme::HOST_COLORS;

fn main() -> anyhow::Result<()> {
    let mut demo = false;
    // フラグ/サブコマンドは TUI 起動前に処理
    match std::env::args().nth(1).as_deref() {
        Some("--version" | "-V" | "version") => {
            println!("ccdesk {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("doctor") => return run_doctor(),
        Some("logs") => return show_logs(),
        // 使用率表示（opt-in）が attach セッションへ注入する内部フック
        Some("statusline-hook") => return statusline_hook(),
        Some("update") => return update_self(),
        Some("--help" | "-h" | "help") => {
            print_usage();
            return Ok(());
        }
        // スクリーンショット撮影用: セッション・アカウント等を架空データで描画（非公開フラグ）
        Some("--demo") => demo = true,
        Some(other) => {
            eprintln!("unknown argument: {other}\n");
            print_usage();
            std::process::exit(2);
        }
        None => {}
    }

    // 自己更新で退避した <exe>.old を掃除する。更新した当のプロセスが掴んでいる
    // 間は消せないので、掃除は次にプロセスを起こしたときになる（失敗は無視する）。
    // ここと doctor の 2 箇所で行い、`ccdesk update` の出力もその 2 つを案内する。
    // TUI 初期化より前に済ませて画面に影響させない
    update::cleanup_old_exe();

    // 使用率表示の opt-in。使用率そのものを読むかは供給元が判断し、
    // ここでは dispatch 時の statusline フック注入の可否として使う
    let usage_display = load_setting("usage_display").as_deref() == Some("on");
    // demo / 実データの選択はこの 1 箇所だけ。以降のコードは供給元を通すので
    // 「今 demo か」を問う分岐を持たない（＝分岐の書き漏らしで実データが漏れない）
    let source: Box<dyn DataSource> = if demo {
        Box::new(DemoSource)
    } else {
        Box::new(LiveSource::new(usage_display))
    };
    // セッション一覧・フッター・ウィンドウ状態はすべて供給元から受け取る
    let jobs = source.jobs();
    let footer = source.footer();
    let window = source.window_state();

    // ホスト端末の実 fg/bg を OSC 10/11 で照会。
    // raw mode / alt screen に入る前に行う。非対応端末はヒューリスティックで
    // 即 Err になるためハングしない（その場合は Dark+ 相当の固定値で claude に応答）
    {
        use terminal_colorsaurus::{color_palette, QueryOptions};
        let host = color_palette(QueryOptions::default())
            .map(|p| {
                let c = |c: terminal_colorsaurus::Color| [c.r, c.g, c.b];
                (Some(c(p.foreground)), Some(c(p.background)))
            })
            .unwrap_or((None, None));
        let _ = HOST_COLORS.set(host);
    }

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
        sessions: Vec::new(),
        active: 0,
        agents: Vec::new(),
        agents_shared: Arc::new(Mutex::new(Vec::new())),
        agents_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        jobs,
        last_scan: std::time::Instant::now(),
        last_live_scan: std::time::Instant::now(),
        rescan_hot_until: None,
        sidebar_width: window.sidebar_width,
        dragging: false,
        last_drag_resize: std::time::Instant::now(),
        term_size: (area.width, area.height),
        sidebar_rows: Vec::new(),
        // 初回 draw が実際に積んだ数で上書きする（それまでは行が無いので 0）
        sidebar_header_rows: 0,
        sidebar_scroll: 0,
        sidebar_follow_sel: false,
        hovered_row: None,
        selected_row: 0,
        dispatch_cwd: window.dispatch_cwd,
        right_view: RightView::Sessions,
        footer,
        footer_shared: Arc::new(Mutex::new(FooterInfo::default())),
        footer_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        footer_refresh: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        claude_updating: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        ccdesk_latest: None,
        ccdesk_latest_shared: Arc::new(Mutex::new(None)),
        ccdesk_latest_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        usage_display,
        usage: None,
        last_usage_read: instant_ago(Duration::from_secs(60)),
        pending_delete: None,
        spawn_rx: None,
        notice: None,
        grouping: window.grouping,
        popup: None,
        focus: Focus::Terminal,
        source,
    };
    clamp_sidebar(&mut app); // 保存値が現在の端末幅を超えていたら丸める
    // バックグラウンド取得の起動。撮影用の供給元は 1 本も起こさないので、
    // ここに `if !demo` は要らない
    app.source.spawn_pollers(app.poll_sinks());
    // 前回開いていた画面を復元: セッションを見ていたなら再 attach、それ以外は new session 画面
    match window.last_view {
        Some(short) if app.jobs.iter().any(|j| j.short == short) => {
            open_short(&mut app, &short);
            if app.sessions.is_empty() {
                app.open_new_view(); // attach 失敗時のフォールバック
            }
        }
        _ => app.open_new_view(),
    }

    let result = run(&mut terminal, &mut app);

    // 終了時に子プロセスを残さない
    for session in &mut app.sessions {
        let _ = session.child.kill();
    }
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
