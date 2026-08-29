// ccdesk: Claude Code 用のセッション管理 TUI。portable-pty で claude を起動 →
// vt100 でパース → tui-term で描画。
// マウス主体の操作: クリックでセッション切替・フォーカス / 行頭の = で二次操作のメニュー /
// 境界線ドラッグで幅変更・スロットの十字移動。ターミナルフォーカス中のキーは PTY へ素通し。
// 予約は Ctrl+Q（終了）、Alt+←→（サイドバー ⇄ メインビュー）、
// Alt+Shift+←→↑↓（スロット間の移動）のみ

use std::sync::{Arc, Mutex};

use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture,
};
use ccdesk::{load_setting, log_error};

mod app;
mod backend;
mod claude_format;
mod cli;
mod git;
mod hooks;
mod keys;
mod notify;
mod panes;
mod poll;
mod relay;
mod session;
mod sessions;
mod source;
#[cfg(test)]
mod testutil;
mod theme;
mod title;
mod ui;
mod update;
mod usage;

use app::{run, App, Focus, SelfUpdate};
use cli::{print_usage, print_usage_error, run_doctor, show_logs, update_self};
use poll::FooterInfo;
use source::{DataSource, DemoSource, LiveSource};
use theme::{HOST_COLORS, HOST_PALETTE};

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
        // セッションの状態を受け取る内部フック（`--settings` / `-c` で注入し、
        // 子の agent が turn ごとに `ccdesk hook <event> <state> <alert>` として
        // 起こす。引数が足りないのは更新前の ccdesk が注入した設定で起きている
        // セッションからの呼び出し ＝ 受け口側が補う）
        Some("hook") => {
            let event = std::env::args().nth(2).unwrap_or_default();
            let state = std::env::args().nth(3);
            let alert = std::env::args().nth(4);
            return hooks::run_hook(&event, state.as_deref(), alert.as_deref());
        }
        // セッション間の受け渡し（[`crate::relay`]）。**セッションの中の agent が叩く**
        // 前提なので、TUI を起こさずここで終わる
        Some("list") => return relay::run_list(),
        Some("send") => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            let Some((target, text)) = args.split_first() else {
                anyhow::bail!("usage: ccdesk send <session> <text>");
            };
            // **残りを全部 1 つの本文として繋ぐ。** 引用符を付け忘れた
            // `ccdesk send docs run the tests` を「宛先 docs へ run the tests」と
            // 読む（シェルの引用符の作法を agent に要求しない）
            return relay::run_send(target, &text.join(" "));
        }
        Some("read") => return read_session(),
        Some("new") => return new_session(),
        Some(verb @ ("stop" | "close")) => {
            let target = std::env::args()
                .nth(2)
                .ok_or_else(|| anyhow::anyhow!("usage: ccdesk {verb} <session>"))?;
            return if verb == "stop" {
                relay::run_stop(&target)
            } else {
                relay::run_close(&target)
            };
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

    // セッション間の受け渡しの残骸を掃除する（[`crate::relay`]）。
    // **自分の pid ぶんを先に消す**: pid は再利用されるので、落ちた前回の
    // 未消化の送信がこのインスタンスの窓へ打ち込まれるのを止める
    relay::unpublish(std::process::id());
    relay::reap();

    // 自己更新で退避した <exe>.old を掃除する。更新した当のプロセスが掴んでいる
    // 間は消せないので、掃除は次にプロセスを起こしたときになる（失敗は無視する）。
    // ここと doctor の 2 箇所で行い、`ccdesk update` の出力もその 2 つを案内する。
    // TUI 初期化より前に済ませて画面に影響させない
    update::cleanup_old_exe();
    // agent 側の残骸も同じタイミングで掃除する。**ccdesk がセッションを常駐させる
    // せいで agent 自身の掃除が空振りする**（掴まれていて消せない）ので、後始末は
    // こちらの仕事になる。ここが一番よく消せる瞬間でもある: 前回のセッションは
    // もう終わっていて、今回のセッションはまだ起こしていない。
    //
    // **設定で出していない agent も見る。** codex を off にした人の環境に残った
    // 残骸を取り逃す理由が無い。
    //
    // **別スレッドで走らせる。** 走査は read_dir だけでは済まず、agent へ版を
    // 聞きに行くものがある（claude の保管庫は「現行版より古いもの」しか消せない
    // ＝ 現行版を知る必要がある）。前景でやると agent の起動 1 回ぶん TUI の
    // 表示が遅れる。掃除は何度やっても同じ結果なので、遅れて終わって困らない
    std::thread::spawn(|| {
        let _ = update::sweep_agent_leftovers(&backend::Kind::ORDER);
    });

    // 使用率表示の opt-in（`~/.ccdesk/config.json` の `"usage_display": "on"`）。
    //
    // **既定で取らないのは資源の話ではない。** 取得は課金ゼロ・枠を消費せず、周期 2 分は
    // 既存のライブ状態のポーリング（2 秒ごと）の 1/60 なので、負荷を理由に切る意味は無い。
    // 切ってあるのは、これが **ccdesk で唯一「無人で Anthropic のサーバーへ出る」経路**
    // だから（他のポーリングはローカルのファイルとプロセスしか見ない）。Consumer Terms
    // 第 3 節は API キー以外での自動アクセスを禁じており、公式 CLI の文書化された機能を
    // 本人のサブスクで本人のために使う形は「explicitly permit」側に収まると読めるが、
    // **断定はできない**。であれば、無人の通信を始めるかどうかは利用者が決めるべきで、
    // ccdesk が全員に代わって決めてよいものではない。
    //
    // **判断はここ 1 箇所**で、以降は供給元の中に閉じる
    let usage_display = load_setting("usage_display").as_deref() == Some("on");
    // 出す agent（`~/.ccdesk/config.json` の `"codex": "on"` で足す）。
    //
    // **既定で出さないのは無駄なポーリングを誰にも起こさないため。** codex CLI が
    // 無い環境ではアカウント取得が毎回失敗し、5 秒ごと（[`crate::poll`] の再試行
    // 間隔）に codex のプロセス起動を試み続ける。使っている人だけが 1 行書く形なら、
    // その空振りが起きる人がいない。
    //
    // **判断はここ 1 箇所**で、以降は `App::kinds` とポーラーへ渡した一覧が答える
    // **設定を読むのはここだけ。** 以降は供給元（`source.kinds()`）が答えるので、
    // 撮影用の供給元は設定に触れずに全 agent を返せる
    let live_kinds = backend::Kind::enabled(load_setting);
    // OS の通知で何を知らせるか（`~/.ccdesk/config.json` の
    // `"notify": ["waiting", "done"]`）。値の意味は [`crate::notify::wanted`]。
    //
    // **既定で出さないのは、通知が端末の外へ出る唯一の出力だから。** 画面の中の
    // 表示と違い、他の作業へ割り込む ＝ 入れた覚えのない割り込みを誰にも起こさない。
    // **撮影用の供給元では常に off**（画面を撮っている最中にトーストを出さない）。
    //
    // 設定の読み方（単値も配列も受ける）は [`crate::notify::configured`] が持つ ＝
    // `ccdesk doctor` の診断と同じ答えを見る。
    // **撮影用かどうかの判断はここ 1 箇所**で、以降は `App::notify` が答える
    let notify = if demo {
        notify::Wanted::default()
    } else {
        notify::configured()
    };
    // 使用率の更新を run ループへ伝える旗と、クリック起点の取得が進行中か。
    // 供給元と App が同じものを持つ
    let usage_dirty = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // 取得中スピナーの旗は agent ごと（押した行だけが回る）
    let usage_fetching = app::per_agent_flags();
    // demo / 実データの選択はこの 1 箇所だけ。以降のコードは供給元を通すので
    // 「今 demo か」を問う分岐を持たない（＝分岐の書き漏らしで実データが漏れない）
    let source: Arc<dyn DataSource> = if demo {
        Arc::new(DemoSource)
    } else {
        Arc::new(LiveSource::new(
            usage_display,
            live_kinds,
            Arc::clone(&usage_dirty),
            usage_fetching.clone(),
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

    // ホスト端末の実 fg/bg を OSC 10/11 で、ANSI パレットを OSC 4 で照会
    // （どちらも raw mode に入る前。照会の作法は theme 側の 1 実装 ＝
    // doctor と同じ経路を通る）。**パレットは fg/bg が取れた端末にだけ聞く**
    // ので、照会に答えない端末へ投げて待つことがない
    let host = theme::query_host_colors();
    let _ = HOST_COLORS.set(host);
    let _ = HOST_PALETTE.set(theme::query_palette(host));

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
        layout: window.layout,
        split: window.split,
        // 中身は下の復元が入れる（長さは App::set_layout が保つ）
        slots: Vec::new(),
        focus_slot: 0,
        agents: Vec::new(),
        agents_observed_at: 0,
        agents_shared: Arc::new(Mutex::new(poll::AgentSnapshot::default())),
        agents_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        stopped_at: std::collections::HashMap::new(),
        sessions,
        hook_states,
        fixed_states,
        notify,
        announced: std::collections::HashMap::new(),
        // 起動時の見え方を先に控える（run ループの 1 周目が「変わった」と誤解しない）
        hook_stamp: source.hook_stamp(),
        titles: source.titles(),
        last_scan: std::time::Instant::now(),
        last_live_scan: std::time::Instant::now(),
        // 保存値はそのまま持つ（画面に出す桁数は端末幅から導くので丸めない ＝
        // 狭い端末で一度起動しただけでユーザーの選んだ幅が失われない）
        sidebar_width: window.sidebar_width,
        dragging: false,
        cross_drag: None,
        drag: None,
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
        footer,
        footer_shared: Arc::new(Mutex::new(FooterInfo::default())),
        footer_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        footer_refresh: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        agent_update: app::agent_update_states(),
        ccdesk_update: Arc::new(Mutex::new(SelfUpdate::Idle)),
        ccdesk_latest: None,
        ccdesk_latest_shared: Arc::new(Mutex::new(None)),
        ccdesk_latest_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        // 起動時の値は供給元から 1 度受け取る（撮影用は固定値、実データは
        // まだ取れていないので Unknown ＝ 何も描かない）
        // **出す agent は供給元に聞く**（撮影は設定を読まない ＝ `--demo` の
        // 見た目が撮る人の `config.json` で変わらない）
        usage: source
            .kinds()
            .into_iter()
            .map(|kind| (kind, source.usage(kind)))
            .collect(),
        usage_dirty,
        usage_fetching,
        usage_hovered: None,
        input_gate: None,
        held_input: Vec::new(),
        notice: None,
        grouping: window.grouping,
        kinds: source.kinds(),
        projects: window.projects,
        popup: None,
        // **復元の間はサイドバー側にしておく。** 端末側のまま復元すると、
        // スロットを 1 枚開くたびにその窓へ focus in が飛び、最後の 1 枚以外は
        // 「自分が端末を持っている」と思い込んだまま残る。
        // 復元し終えてから `set_focus` で 1 枚だけに伝える（下の復元の直後）
        focus: Focus::Sidebar,
        animating: false,
        // 起動時は何も公開していない（run ループの 1 周目が必ず書く）
        published_sessions: Vec::new(),
        pending_submit: Vec::new(),
        source,
    };
    // バックグラウンド取得の起動。**起動列の重い処理（埋め戻し・transcript の
    // 初回読み）より先に起こす**: ポーラーが取りに行くもの（ライブ状態・
    // バージョン）は起動列と独立なので、後回しにすると初回のライブ状態と
    // アカウント行の表示がその分だけ遅れる。撮影用の供給元は 1 本も起こさないので、
    // ここに `if !demo` は要らない
    app.source.spawn_pollers(app.poll_sinks());
    // 既にあるセッションのフォルダを登録へ埋め戻す（以前から使っているフォルダの
    // 見出しが、最後のセッションを消した時点で消えないように）。一覧を読んだ後・
    // 画面を組む前のこの位置に置く: 埋め戻しは初回の一覧に効く必要がある
    app::backfill_projects(&mut app);
    // **最初の描画より前に transcript を解決して名前を読む。** 走査の結果を持つのは
    // Titles のキャッシュだけなので、ここで 1 度走らせないと最初の周期（2 秒）まで
    // 全部の行が `new session` に見える。未記録の行の解決し直しも同じ 1 回で済む
    // （読む量は予算で有界。[`crate::title::SCAN_BUDGET`]）
    app::refresh_transcripts(&mut app);
    // 前回の並びを復元（枚数を配置へ合わせてから中身を戻す）。
    // 何を戻し、どこで New 画面を出すかは [`app::restore_slots`]
    app.set_layout(window.layout);
    app::restore_slots(&mut app, window.slots);
    // 端末フォーカスを伝えるのはここ 1 回だけ（受け取るのはフォーカススロットの窓）
    app.set_focus(Focus::Terminal);

    // **起動時にも端末のウィンドウを控える**（通知クリックで前面へ戻す先。
    // [`crate::notify`]）。`FocusGained` だけに任せると、**前面の端末で起動した
    // 場合はフォーカスが変わらないので一度も届かない** ＝ 起動してすぐ来た通知が
    // 画面を前へ出せない。ここの推測が外れていても、次に端末がフォーカスを
    // 得た時点で正しい窓に置き換わる
    notify::remember_terminal_window();

    // panic でも通知・子プロセス・端末を通常終了と同じ順序で片付ける。
    // 強制終了だけは OS が unwind を許さないため、この経路では捕捉できない
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run(&mut terminal, &mut app)
    }));

    // 終了時に子プロセスを残さない。**行は残す**（`sessions.json` はそのまま ＝
    // 次の起動で一覧に出て `claude -r` で再開できる）
    app::kill_sessions_on_exit(&mut app);
    // 窓の公開をやめる。**pid は再利用される**ので、残すと次に同じ pid を得た
    // プロセスの子が、死んだインスタンスの窓一覧を自分のものとして読む
    relay::unpublish(std::process::id());
    drop(app);
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
    match result {
        Ok(result) => result,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

/// `ccdesk read <session> [-n <count>] [--screen]`。
///
/// **手で読む**のは他のサブコマンドと同じ作法（引数の解析に crate を足さない）。
/// 知らない旗は黙って無視せず落とす: 打ち間違いが「既定で読めた」ように見えると、
/// agent は `-n 100` を指定したつもりで 20 件しか見ていないことに気づけない
fn read_session() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let mut target = None;
    let mut last = relay::READ_DEFAULT;
    let mut screen = false;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--screen" => screen = true,
            "-n" | "--last" => {
                let value = rest
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("{arg} needs a count"))?;
                last = value
                    .parse()
                    .map_err(|_| anyhow::anyhow!("`{value}` is not a count"))?;
            }
            other if other.starts_with('-') => anyhow::bail!("unknown option `{other}`"),
            other if target.is_none() => target = Some(other.to_string()),
            other => anyhow::bail!("unexpected argument `{other}`"),
        }
    }
    let target = target
        .ok_or_else(|| anyhow::anyhow!("usage: ccdesk read <session> [-n <count>] [--screen]"))?;
    relay::run_read(&target, last, screen)
}

/// `ccdesk new [--agent <name>] [--cwd <dir>] [prompt]`。
///
/// **旗より後ろは全部プロンプト**（`ccdesk new write the release notes` が
/// そのまま通る ＝ 引用符の作法を agent に要求しない）
fn new_session() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let mut kind = None;
    let mut cwd = None;
    let mut prompt: Vec<String> = Vec::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        // **プロンプトが始まったら旗の解釈をやめる。** やめないと、プロンプトの中の
        // `--cwd` のような語を旗として食う
        if !prompt.is_empty() {
            prompt.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--agent" | "--cwd" => {
                let value = rest
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("{arg} needs a value"))?;
                if arg == "--cwd" {
                    cwd = Some(value.clone());
                } else {
                    kind = Some(backend::Kind::parse(value).ok_or_else(|| {
                        anyhow::anyhow!("`{value}` is not an agent ccdesk knows")
                    })?);
                }
            }
            other if other.starts_with("--") => anyhow::bail!("unknown option `{other}`"),
            other => prompt.push(other.to_string()),
        }
    }
    relay::run_new(kind, cwd.as_deref(), &prompt.join(" "))
}
