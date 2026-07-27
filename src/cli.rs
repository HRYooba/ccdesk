//! CLI サブコマンド（doctor / logs / update）。

use crate::poll::{fetch_account, AccountStatus};
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
         \x20 ccdesk --version  print version",
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

/// 環境診断。各項目を ok / FAIL / warn の 1 行で英語出力し、FAIL があれば exit 1。
/// TUI を起動しないので raw mode には入らず、色照会もここで直接行う
pub(crate) fn run_doctor() -> anyhow::Result<()> {
    let mut failed = false;

    // 自己更新の残骸を掃除する。TUI 起動でも消すが、更新後に TUI を開かず
    // doctor だけ叩く使い方があるため、環境を診るこの入口でも同じことをする
    // （`ccdesk update` の出力もこの 2 つを案内している）
    update::cleanup_old_exe();

    // claude CLI が PATH にあるか（バージョン文字列も表示）
    match std::process::Command::new("claude")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
    {
        Ok(o) if o.status.success() => {
            let ver = String::from_utf8_lossy(&o.stdout);
            println!("ok    claude CLI on PATH: {}", ver.trim());
        }
        Ok(o) => {
            println!("FAIL  claude CLI: `claude --version` exited with {}", o.status);
            failed = true;
        }
        Err(e) => {
            println!("FAIL  claude CLI not found on PATH ({e})");
            failed = true;
        }
    }

    // `claude agents --json --all` が JSON 配列を返すか
    match std::process::Command::new("claude")
        .args(["agents", "--json", "--all"])
        .stdin(std::process::Stdio::null())
        .output()
    {
        Ok(o) => match serde_json::from_slice::<serde_json::Value>(&o.stdout) {
            Ok(serde_json::Value::Array(items)) => {
                println!("ok    claude agents --json: {} session(s)", items.len());
            }
            Ok(_) => {
                println!("FAIL  claude agents --json: expected a JSON array");
                failed = true;
            }
            Err(e) => {
                println!("FAIL  claude agents --json: invalid JSON ({e})");
                failed = true;
            }
        },
        Err(e) => {
            println!("FAIL  claude agents --json: failed to run ({e})");
            failed = true;
        }
    }

    // サイドバー下部に出るアカウント行。表示が実際どうなるかをここで確認できる
    // （未ログインは FAIL ではない = ccdesk 自体は動く。ログインを促すだけ）
    match fetch_account() {
        AccountStatus::LoggedIn(label) => println!("ok    claude account: {label}"),
        AccountStatus::LoggedOut => {
            println!("warn  claude account: not logged in (run /login in a claude session)");
        }
        AccountStatus::Unknown => {
            println!("warn  claude account: could not determine (`claude auth status --json`)");
        }
    }

    // 使用率の取得（**opt-in を切っていても実際に 1 回叩く** ＝ 入れる前に
    // 何が返るか確かめられる。取得は課金ゼロ・枠を消費しない。[`crate::usage`]）。
    //
    // **これが無いと「opt-in したのに出ない」人へ渡せる情報が無い。** 以前の方式は
    // 開発者の環境でだけ通る 1 本を踏んでいて、他人の環境で無言に空になっていた。
    // 環境差でしか壊れないものは、他人自身が 1 コマンドで確かめられる必要がある
    {
        let opt_in = if ccdesk::load_setting("usage_display").as_deref() == Some("on") {
            "on"
        } else {
            "off"
        };
        match crate::usage::diagnose() {
            Ok(crate::usage::Usage::Ready(info)) => {
                let show = |label: &str, w: Option<&crate::usage::UsageWindow>| {
                    w.map_or_else(
                        || format!("{label} -"),
                        |w| format!("{label} {}%", w.pct.round() as u32),
                    )
                };
                let mut parts = vec![show("5h", info.five.as_ref()), show("7d", info.seven.as_ref())];
                parts.extend(
                    info.models
                        .iter()
                        .map(|(name, w)| format!("{name} {}%", w.pct.round() as u32)),
                );
                println!("ok    usage ({opt_in}): {}", parts.join(" · "));
            }
            // 枠の概念が無いアカウント（API キー・Bedrock・Vertex 等）。
            // ccdesk は動くので FAIL ではない。**恒久的に取れないことを言う**
            Ok(crate::usage::Usage::Unavailable) => {
                println!(
                    "warn  usage ({opt_in}): this account has no rate-limit windows \
                     (subscription plans only); the gauge stays hidden"
                );
            }
            Ok(_) => {
                println!(
                    "warn  usage ({opt_in}): claude answered but no window could be read \
                     (its shape may have changed)"
                );
            }
            Err(e) => println!("warn  usage ({opt_in}): {e}"),
        }
    }

    // ~/.ccdesk/ が書き込み可能か（試し書きして消す）
    match ccdesk::ccdesk_dir() {
        Some(dir) => {
            let probe = dir.join(".doctor-write-test");
            match std::fs::write(&probe, b"ok") {
                Ok(()) => {
                    let _ = std::fs::remove_file(&probe);
                    println!("ok    ~/.ccdesk writable: {}", dir.display());
                }
                Err(e) => {
                    println!("FAIL  ~/.ccdesk not writable: {} ({e})", dir.display());
                    failed = true;
                }
            }
        }
        None => {
            println!("FAIL  ~/.ccdesk: could not resolve path (USERPROFILE unset?)");
            failed = true;
        }
    }

    // ターミナルの色照会（OSC 10/11）。取れれば fg/bg を hex 表示、失敗なら warn
    // （パイプ実行など実端末でない場合は失敗して当然。テーマ転送は dark にフォールバック）
    {
        use terminal_colorsaurus::{color_palette, QueryOptions};
        let hex = |c: terminal_colorsaurus::Color| {
            format!("#{:02x}{:02x}{:02x}", (c.r >> 8) as u8, (c.g >> 8) as u8, (c.b >> 8) as u8)
        };
        match color_palette(QueryOptions::default()) {
            Ok(p) => println!(
                "ok    terminal color query: fg {} bg {}",
                hex(p.foreground),
                hex(p.background)
            ),
            Err(e) => println!(
                "warn  terminal color query failed ({e}); theme forwarding falls back to dark"
            ),
        }
    }

    if failed {
        std::process::exit(1);
    }
    Ok(())
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
