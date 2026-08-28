//! CLI サブコマンド（doctor / logs / update）。

use crate::poll::{fetch_claude_account, AccountStatus};
use crate::update;

/// 使い方の本文。**出力先は呼び手が決める**（[`print_usage`] / [`print_usage_error`]）
fn usage_text() -> String {
    format!(
        "ccdesk {} — Claude Code session manager TUI\n\n\
         Usage:\n\
         \x20 ccdesk            launch the TUI\n\
         \x20 ccdesk doctor     diagnose the environment (claude CLI, config dir, terminal)\n\
         \x20 ccdesk logs       print the path and tail of the error log\n\
         \x20 ccdesk update     download and install the latest release\n\
         \x20 ccdesk --version  print version\n\n\
         From inside a session (for the agent running there):\n\
         \x20 ccdesk list                       this ccdesk's sessions, running or not\n\
         \x20 ccdesk send <session> <text>      type text into another session and submit it\n\
         \x20 ccdesk read <session> [-n <count>] [--screen]\n\
         \x20                                   that session's last messages, or its screen\n\
         \x20 ccdesk new [--agent <name>] [--cwd <dir>] [prompt]\n\
         \x20                                   start another session and print its id\n\
         \x20 ccdesk stop <session>             end its process, keep the row\n\
         \x20 ccdesk close <session>            end its process and drop the row",
        env!("CARGO_PKG_VERSION")
    )
}

/// `--help` で明示的に求められた使い方。**求められた出力なので stdout**
pub(crate) fn print_usage() {
    println!("{}", usage_text());
}

/// 知らない引数を渡されたときの案内。**stdout には 1 バイトも出さない。**
///
/// エラーの案内を stdout へ出すと、ccdesk を「出力を読み取る道具」として
/// 呼んでいる相手にヘルプ本文を食わせることになる。実際に踏みうる経路がある:
/// 旧版が `--settings` へ注入した `statusLine`（`ccdesk statusline-hook`）が
/// 残っている claude セッションは、ccdesk を更新した後もそれを呼び続けるので、
/// **ユーザーの statusline 行にヘルプが並ぶ**。stderr へ出せば stdout は空になり、
/// 行が空欄になるだけで済む（セッションを開き直せば注入ファイルごと消える）
pub(crate) fn print_usage_error(argument: &str) {
    eprintln!("unknown argument: {argument}\n");
    eprintln!("{}", usage_text());
}

/// 自己更新。最新リリースの実行ファイルを取得し、SHA-256 を検証してから
/// 現行の実行ファイルと差し替える（実体は [`crate::update`]）。
/// 出力は ASCII に留める: Windows コンソールのコードページ次第で
/// 非 ASCII が化けるため
pub(crate) fn update_self() -> anyhow::Result<()> {
    let Some(tag) = update::latest_tag() else {
        anyhow::bail!("failed to fetch the latest release tag from GitHub");
    };
    if !update::tag_is_newer(&tag) {
        println!(
            "ccdesk {} is up to date (latest release: {tag})",
            env!("CARGO_PKG_VERSION")
        );
        return Ok(());
    }
    println!(
        "downloading ccdesk {tag} (current: {})...",
        env!("CARGO_PKG_VERSION")
    );
    // 失敗時はここで Err を返して非ゼロ終了する。検証を通らなければ
    // インストール済みの実行ファイルには触れていない（update::install 参照）
    let installed = update::install(&tag)?;
    // 「次回起動時に消える」とだけ書くと doctor などのサブコマンドでも消えると
    // 読めてしまう。実際に掃除するのは TUI 起動と doctor なので、そう書く
    println!(
        "installed ccdesk {tag} at {}\n\
         this process keeps running {}; the new version applies on next launch\n\
         the previous exe is parked at {}; it is removed the next time you start \
         the TUI or run `ccdesk doctor`",
        installed.exe.display(),
        env!("CARGO_PKG_VERSION"),
        installed.old.display()
    );
    Ok(())
}

/// 診断項目 1 つの結果。**ok/FAIL/warn の綴り・桁揃え・exit code への反映は
/// [`Check::report`] 1 箇所**（項目を足すときに `failed = true` を書き忘れて
/// exit 0 のまま通る、という形を作らない）
enum Check {
    Ok(String),
    /// ccdesk は動くが伝えるべきことがある（exit code には効かない）
    Warn(String),
    Fail(String),
}

impl Check {
    fn report(self, failed: &mut bool) {
        match self {
            Self::Ok(msg) => println!("ok    {msg}"),
            Self::Warn(msg) => println!("warn  {msg}"),
            Self::Fail(msg) => {
                println!("FAIL  {msg}");
                *failed = true;
            }
        }
    }
}

/// 環境診断。各項目を ok / FAIL / warn の 1 行で英語出力し、FAIL があれば exit 1。
/// TUI を起動しないので raw mode には入らず、色照会もここで直接行う
pub(crate) fn run_doctor() -> anyhow::Result<()> {
    let mut failed = false;

    // 自己更新の残骸を掃除する。TUI 起動でも消すが、更新後に TUI を開かず
    // doctor だけ叩く使い方があるため、環境を診るこの入口でも同じことをする
    // （`ccdesk update` の出力もこの 2 つを案内している）
    update::cleanup_old_exe();

    check_claude_cli().report(&mut failed);
    check_agent_leftovers().report(&mut failed);
    check_agents().report(&mut failed);
    check_codex_cli().report(&mut failed);
    check_codex_account().report(&mut failed);
    check_codex_usage().report(&mut failed);
    check_account().report(&mut failed);
    check_usage().report(&mut failed);
    check_ccdesk_dir().report(&mut failed);
    check_terminal_colors().report(&mut failed);

    if failed {
        std::process::exit(1);
    }
    Ok(())
}

/// claude CLI が PATH にあるか（バージョン文字列も表示）。
/// 取得の作法は本番（[`crate::backend::Backend::version`]）と同じ `poll::out`
fn check_claude_cli() -> Check {
    match crate::poll::out("claude", &["--version"]) {
        Some(ver) if !ver.trim().is_empty() => {
            Check::Ok(format!("claude CLI on PATH: {}", ver.trim()))
        }
        Some(_) => Check::Fail("claude CLI: `claude --version` answered nothing".to_string()),
        None => Check::Fail("claude CLI not found on PATH".to_string()),
    }
}

/// agent が置き去りにした残骸を掃除して、**観測した事実だけ**を報告する。
///
/// **診断ではなく後始末**だが doctor に置く: TUI を開かずに `ccdesk doctor` だけ
/// 叩く使い方があり、自分の `<exe>.old` を同じ入口で消しているのと理由が揃う。
///
/// **原因を断定しない。** 以前は消せなかったものを "still held by running sessions"
/// と報告していたが、ccdesk が観測しているのは削除が失敗したことだけで、
/// 誰が掴んでいるかは見ていない（削除失敗の理由は他にもある）。
///
/// 深刻度は退かせたかで分ける: 退かせたものは次の更新を塞がないので Ok、
/// 元の場所に残ったものだけが**次の更新を落とす**ので Warn
fn check_agent_leftovers() -> Check {
    let swept = crate::update::sweep_agent_leftovers(&crate::backend::Kind::ORDER);
    let mut done = Vec::new();
    if swept.deleted > 0 {
        done.push(format!("deleted {}", swept.deleted));
    }
    if swept.quarantined > 0 {
        done.push(format!("{} could not be deleted, moved aside", swept.quarantined));
    }
    match (done.is_empty(), swept.stuck) {
        (true, 0) => Check::Ok("agent leftovers: none to clear".to_string()),
        (false, 0) => Check::Ok(format!("agent leftovers: {}", done.join(", "))),
        (_, stuck) => {
            done.push(format!("{stuck} could not be deleted or moved"));
            Check::Warn(format!("agent leftovers: {}", done.join(", ")))
        }
    }
}

/// codex CLI が PATH にあるか。**無いのは FAIL ではない**:
/// ccdesk は claude だけでも動く（codex の行を作ろうとしたときに初めて困る）
fn check_codex_cli() -> Check {
    let program = crate::backend::Kind::Codex.backend().update_program();
    match crate::poll::out(program, &["--version"]) {
        Some(ver) if !ver.trim().is_empty() => {
            Check::Ok(format!("codex CLI on PATH: {}", ver.trim()))
        }
        _ => Check::Warn(
            "codex CLI not found on PATH (codex sessions cannot be started)".to_string(),
        ),
    }
}

/// codex のアカウントが取れるか（`codex app-server` の `account/read`）。
/// **本番と同じ経路**を通す（別経路だと doctor が嘘の ok を出す）
fn check_codex_account() -> Check {
    match crate::backend::Kind::Codex.backend().account() {
        AccountStatus::LoggedIn(label) => Check::Ok(format!("codex account: {label}")),
        AccountStatus::LoggedOut => {
            Check::Warn("codex account: not logged in (run `codex login`)".to_string())
        }
        AccountStatus::Unknown => Check::Warn("codex account: could not be read".to_string()),
    }
}

/// codex の使用率が取れるか（`codex app-server` へ 1 往復）。
/// **本番と同じ経路**を通す（別経路だと doctor が嘘の ok を出す）
fn check_codex_usage() -> Check {
    match crate::backend::Kind::Codex.backend().usage() {
        crate::usage::Usage::Ready(info) => Check::Ok(format!(
            "codex usage: {}",
            info.windows()
                .map(|(label, w)| format!("{label} {:.0}%", w.pct))
                .collect::<Vec<_>>()
                .join(" · ")
        )),
        // 取れないのは codex が入っていない・未ログイン・形が変わった、のどれか。
        // どれも ccdesk 自体は動くので Warn
        other => Check::Warn(format!("codex usage: not available ({other:?})")),
    }
}

/// 前景セッションの生存記録（`~/.claude/sessions/`）が読めるか。**本番のポーラーと
/// 同じ [`crate::poll::fetch_agents`]** を通す（別経路だと poll 側だけ解釈を
/// 変えたときに doctor が嘘の ok を出す）
fn check_agents() -> Check {
    match crate::poll::fetch_agents() {
        Some(snapshot) => Check::Ok(format!(
            "~/.claude/sessions: {} live session(s)",
            snapshot.agents.len()
        )),
        None => Check::Fail("~/.claude/sessions: could not be listed".to_string()),
    }
}

/// サイドバー下部に出るアカウント行。表示が実際どうなるかをここで確認できる
/// （未ログインは FAIL ではない = ccdesk 自体は動く。ログインを促すだけ）
fn check_account() -> Check {
    match fetch_claude_account() {
        AccountStatus::LoggedIn(label) => Check::Ok(format!("claude account: {label}")),
        AccountStatus::LoggedOut => Check::Warn(
            "claude account: not logged in (run /login in a claude session)".to_string(),
        ),
        AccountStatus::Unknown => Check::Warn(
            "claude account: could not determine (`claude auth status --json`)".to_string(),
        ),
    }
}

/// 使用率の取得（**opt-in を切っていても実際に 1 回叩く** ＝ 入れる前に
/// 何が返るか確かめられる。取得は課金ゼロ・枠を消費しない。[`crate::usage`]）。
///
/// **これが無いと「opt-in したのに出ない」人へ渡せる情報が無い。** 以前の方式は
/// 開発者の環境でだけ通る 1 本を踏んでいて、他人の環境で無言に空になっていた。
/// 環境差でしか壊れないものは、他人自身が 1 コマンドで確かめられる必要がある
fn check_usage() -> Check {
    let opt_in = if ccdesk::load_setting("usage_display").as_deref() == Some("on") {
        "on"
    } else {
        "off"
    };
    match crate::usage::diagnose() {
        Ok(crate::usage::Usage::Ready(info)) => {
            // 枠のラベルと並びは画面と同じ配り口（`UsageInfo::windows`）
            let parts: Vec<String> = info
                .windows()
                .map(|(label, w)| format!("{label} {}%", w.pct.round() as u32))
                .collect();
            Check::Ok(format!("usage ({opt_in}): {}", parts.join(" · ")))
        }
        // 枠の概念が無いアカウント（API キー・Bedrock・Vertex 等）。
        // ccdesk は動くので FAIL ではない。**恒久的に取れないことを言う**
        Ok(crate::usage::Usage::Unavailable) => Check::Warn(format!(
            "usage ({opt_in}): this account has no rate-limit windows \
             (subscription plans only); the gauge stays hidden"
        )),
        Ok(_) => Check::Warn(format!(
            "usage ({opt_in}): claude answered but no window could be read \
             (its shape may have changed)"
        )),
        Err(e) => Check::Warn(format!("usage ({opt_in}): {e}")),
    }
}

/// ~/.ccdesk/ が書き込み可能か（試し書きして消す）
fn check_ccdesk_dir() -> Check {
    let Some(dir) = ccdesk::ccdesk_dir() else {
        return Check::Fail("~/.ccdesk: could not resolve path (USERPROFILE unset?)".to_string());
    };
    let probe = dir.join(".doctor-write-test");
    match std::fs::write(&probe, b"ok") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Check::Ok(format!("~/.ccdesk writable: {}", dir.display()))
        }
        Err(e) => Check::Fail(format!("~/.ccdesk not writable: {} ({e})", dir.display())),
    }
}

/// ターミナルの色照会（OSC 10/11）。取れれば fg/bg を hex 表示、失敗なら warn
/// （パイプ実行など実端末でない場合は失敗して当然。テーマ転送は dark にフォールバック）。
/// 照会は TUI 起動と同じ 1 実装（theme 側）を通る ＝ doctor の ok が本番と同じ経路の答えになる
fn check_terminal_colors() -> Check {
    let hex = |c: [u16; 3]| {
        format!(
            "#{:02x}{:02x}{:02x}",
            (c[0] >> 8) as u8,
            (c[1] >> 8) as u8,
            (c[2] >> 8) as u8
        )
    };
    let host = crate::theme::query_host_colors();
    let (Some(fg), Some(bg)) = host else {
        return Check::Warn(
            "terminal color query failed; theme forwarding falls back to dark".to_string(),
        );
    };
    // パレット（OSC 4）は fg/bg が取れた端末にだけ聞く ＝ 本番と同じ順序・同じ条件。
    // 取れなければ状態色は ANSI 名前色へ落ちる（テーマ追従は保たれ、明滅の段階だけ減る）
    match crate::theme::query_palette(host) {
        Some(p) => Check::Ok(format!(
            "terminal color query: fg {} bg {} · palette {} {} {} {}",
            hex(fg),
            hex(bg),
            hex(p.red),
            hex(p.green),
            hex(p.yellow),
            hex(p.bright_red)
        )),
        None => Check::Warn(format!(
            "terminal color query: fg {} bg {}; palette query (OSC 4) unanswered, \
             state colors fall back to the ANSI palette",
            hex(fg),
            hex(bg)
        )),
    }
}

/// ~/.ccdesk/error.log のパスを表示し、末尾 50 行を出力する。
/// ファイルが無ければ "no errors logged" と表示
pub(crate) fn show_logs() -> anyhow::Result<()> {
    let Some(path) = ccdesk::error_log_path() else {
        println!("no errors logged");
        return Ok(());
    };
    println!("{}", path.display());
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let lines: Vec<&str> = text.lines().collect();
            let start = lines.len().saturating_sub(50);
            for line in &lines[start..] {
                println!("{line}");
            }
        }
        Err(_) => println!("no errors logged"),
    }
    Ok(())
}
