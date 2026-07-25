//! CLI サブコマンド（doctor / logs / update / statusline-hook）。

use crate::poll::{fetch_account, AccountStatus};
use crate::update;

pub(crate) fn print_usage() {
    println!(
        "ccdesk {} — Claude Code session manager TUI\n\n\
         Usage:\n\
         \x20 ccdesk            launch the TUI\n\
         \x20 ccdesk doctor     diagnose the environment (claude CLI, config dir, terminal)\n\
         \x20 ccdesk logs       print the path and tail of the error log\n\
         \x20 ccdesk update     download and install the latest release\n\
         \x20 ccdesk --version  print version",
        env!("CARGO_PKG_VERSION")
    );
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
    println!(
        "installed ccdesk {tag} at {}\n\
         this process keeps running {}; the new version applies on next launch\n\
         the previous exe is parked at {} and is deleted at the next launch",
        installed.exe.display(),
        env!("CARGO_PKG_VERSION"),
        installed.old.display()
    );
    Ok(())
}

/// statusline フック（使用率表示 opt-in 時に --settings で注入される。ユーザーは直接使わない）。
/// claude が statusline へ渡す公式 JSON から rate_limits を ~/.ccdesk/usage.json に保存し、
/// ユーザー自身の statusline 設定があれば同じ入力でそのまま実行して出力を透過する。
/// fail-open: フック側で何が起きてもユーザー statusline の実行と出力は必ず通す
pub(crate) fn statusline_hook() -> anyhow::Result<()> {
    use std::io::Read as _;
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);

    // rate_limits の保存は best-effort（失敗しても透過実行へ進む）
    let _ = std::panic::catch_unwind(|| {
        let Some(v) = serde_json::from_str::<serde_json::Value>(&input).ok() else {
            return;
        };
        let Some(rl) = v.get("rate_limits") else {
            return;
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let out = serde_json::json!({
            "rate_limits": rl,
            "written_at": now,
        });
        if let Some(path) = ccdesk::usage_cache_path() {
            // 読み手が中途半端な JSON を見ないよう tmp → rename で置く
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, out.to_string()).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    });

    // ユーザー自身の statusline 設定（ユーザースコープのみ）へ透過する。
    // プロジェクトローカルの .claude/settings.json は読まない: リポジトリ由来の
    // コマンドを信頼確認なしに実行すると、悪意あるリポジトリを開くだけで
    // 任意コマンド実行になるため（claude 本体の信頼プロンプト相当を持たないうちは
    // ユーザースコープに限定する）
    let read_statusline = |path: std::path::PathBuf| -> Option<String> {
        let s = std::fs::read_to_string(path).ok()?;
        let v = serde_json::from_str::<serde_json::Value>(&s).ok()?;
        let sl = v.get("statusLine")?;
        if sl.get("type").and_then(|t| t.as_str()) != Some("command") {
            return None;
        }
        sl.get("command").and_then(|c| c.as_str()).map(str::to_string)
    };
    let user_cmd = ccdesk::claude_dir().and_then(|d| read_statusline(d.join("settings.json")));
    if let Some(cmd) = user_cmd {
        // 自己参照ガード: 誤って自分自身が設定されていても無限再帰させない
        if cmd.contains("statusline-hook") {
            return Ok(());
        }
        use std::io::Write as _;
        // claude 本体と同じく bash で実行する（bash 前提のコマンドを壊さない）。
        // bash が無い環境だけ cmd /C にフォールバックし、~/ は手動でホームへ展開
        let child = std::process::Command::new("bash")
            .arg("-c")
            .arg(&cmd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::null())
            .spawn()
            .or_else(|_| {
                let home = std::env::var("USERPROFILE").unwrap_or_default();
                let cmd = cmd.replace("~/", &format!("{}/", home.replace('\\', "/")));
                std::process::Command::new("cmd")
                    .args(["/C", &cmd])
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::inherit())
                    .stderr(std::process::Stdio::null())
                    .spawn()
            });
        if let Ok(mut child) = child {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(input.as_bytes());
            }
            let _ = child.wait();
        }
    }
    Ok(())
}

/// 環境診断。各項目を ok / FAIL / warn の 1 行で英語出力し、FAIL があれば exit 1。
/// TUI を起動しないので raw mode には入らず、色照会もここで直接行う
pub(crate) fn run_doctor() -> anyhow::Result<()> {
    let mut failed = false;

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
