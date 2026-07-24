//! バックグラウンド取得（agents --json / フッター / 使用率）と状態分類。
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ratatui::style::Color;

use ccdesk::BgJob;

use crate::cli::{claude_settings_channel, version_newer};
use crate::theme::{ui, C_ATTENTION, C_FAIL, C_OK, C_WORKING};

/// `claude agents --json --all` の 1 エントリ（公式のスクリプト向けライブデータ。
/// フィールドは agent-view.md に文書化されている: state は
/// working|blocked|done|failed|stopped、pid はプロセス生存中のみ載る）
#[derive(Clone, Default)]
pub(crate) struct AgentInfo {
    pub(crate) id: String, // bg セッションの short id（interactive は空）
    pub(crate) name: String,
    pub(crate) state: String,
    pub(crate) has_pid: bool, // プロセス生存（文書化: 生存中のみ pid が載る）
}

/// agents --json は 1 回 ~900ms かかるためバックグラウンドスレッドで回す
pub(crate) fn spawn_agents_poller(
    shared: Arc<Mutex<Vec<AgentInfo>>>,
    dirty: Arc<std::sync::atomic::AtomicBool>,
) {
    std::thread::spawn(move || loop {
        let output = std::process::Command::new("claude")
            .args(["agents", "--json", "--all"])
            .stdin(std::process::Stdio::null())
            .output();
        if let Ok(output) = output
            && let Ok(serde_json::Value::Array(items)) =
                serde_json::from_slice::<serde_json::Value>(&output.stdout)
            {
                let parsed: Vec<AgentInfo> = items
                    .iter()
                    .map(|v| {
                        let s = |k: &str| {
                            v.get(k)
                                .and_then(|x| x.as_str())
                                .unwrap_or_default()
                                .to_string()
                        };
                        AgentInfo {
                            id: s("id"),
                            name: s("name"),
                            state: s("state"),
                            has_pid: v.get("pid").is_some(),
                        }
                    })
                    .collect();
                *shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = parsed;
                dirty.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        std::thread::sleep(Duration::from_secs(2));
    });
}

/// スクリーンショット撮影用の架空データ（--demo）。
/// セッション名・プロジェクト・アカウント・使用率を実データの代わりに描画する
pub(crate) fn demo_jobs() -> Vec<BgJob> {
    let rows: [(&str, &str, &str, &str, &str); 6] = [
        ("demo01", "fix login form validation", "working", "", "refining error messages"),
        ("demo02", "add dark mode toggle", "blocked", "blocked", "choose accent color?"),
        ("demo03", "refactor api client", "working", "", "extracting retry logic"),
        ("demo04", "write onboarding docs", "done", "", "docs published"),
        ("demo05", "optimize image pipeline", "done", "", "2.3x faster builds"),
        ("demo06", "migrate to vite", "stopped", "", ""),
    ];
    let projects = ["C:\\dev\\shop-app", "C:\\dev\\shop-app", "C:\\dev\\api", "C:\\dev\\docs", "C:\\dev\\api", "C:\\dev\\shop-app"];
    rows.iter()
        .zip(projects)
        .map(|((short, name, state, tempo, detail), cwd)| BgJob {
            short: short.to_string(),
            cwd: cwd.to_string(),
            state: state.to_string(),
            tempo: tempo.to_string(),
            name: name.to_string(),
            needs: "which option should I take?".to_string(),
            detail: detail.to_string(),
            result: detail.to_string(),
            children: Vec::new(),
            mtime: std::time::SystemTime::now(),
            created_at_ms: 0,
            updated_at_ms: 0,
        })
        .collect()
}

/// 使用率（5h/7d 枠）の表示用データ。statusline フックが書いた
/// ~/.ccdesk/usage.json（公式 statusline JSON の rate_limits 由来）を読む
#[derive(Clone, Copy, PartialEq)]
pub(crate) struct UsageInfo {
    pub(crate) five: Option<(f64, u64)>,  // (使用率 %, resets_at unix 秒)
    pub(crate) seven: Option<(f64, u64)>, // 同上（7 日枠・全モデル集計）
    // 最終更新から 10 分超（rate_limits はセッションの活動時レンダーにしか
    // 載らないため、活動が無いだけで古くなる）。消さずに薄く表示する
    pub(crate) stale: bool,
}

/// usage.json を読む。無い・壊れているときだけ None（古いデータは stale 付きで返す）
pub(crate) fn read_usage() -> Option<UsageInfo> {
    let text = std::fs::read_to_string(ccdesk::usage_cache_path()?).ok()?;
    let v = serde_json::from_str::<serde_json::Value>(&text).ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let written = v.get("written_at").and_then(|w| w.as_u64()).unwrap_or(0);
    let window = |key: &str| -> Option<(f64, u64)> {
        let w = v.pointer(&format!("/rate_limits/{key}"))?;
        Some((
            w.get("used_percentage").and_then(|p| p.as_f64())?,
            w.get("resets_at").and_then(|r| r.as_u64()).unwrap_or(0),
        ))
    };
    let info = UsageInfo {
        five: window("five_hour"),
        seven: window("seven_day"),
        stale: now.saturating_sub(written) > 600,
    };
    (info.five.is_some() || info.seven.is_some()).then_some(info)
}

/// サイドバー下部に出すアカウント・バージョン情報。
/// アカウントは `claude auth status --json`（公式サブコマンド）、
/// 現行版は `claude --version`、最新版は Anthropic 公式配布の npm パッケージ
/// メタデータ（registry.npmjs.org/@anthropic-ai/claude-code/latest）から取る
#[derive(Clone, Default)]
pub(crate) struct FooterInfo {
    pub(crate) account: String,        // "alice · Acme, Inc."（email ローカル部 + 組織名）
    pub(crate) current: String,        // claude の現行バージョン
    pub(crate) latest: Option<String>, // 新しい版があるときだけ Some
}

/// フッター情報のバックグラウンド取得（起動時 + 1 時間毎 + 更新後の再取得要求時）
pub(crate) fn spawn_footer_poller(
    shared: Arc<Mutex<FooterInfo>>,
    dirty: Arc<std::sync::atomic::AtomicBool>,
    refresh: Arc<std::sync::atomic::AtomicBool>,
) {
    std::thread::spawn(move || {
        let mut slept = u64::MAX / 2; // 初回は即取得
        loop {
            if slept >= 3600 || refresh.swap(false, std::sync::atomic::Ordering::Relaxed) {
                slept = 0;
                let out = |cmd: &str, args: &[&str]| -> Option<String> {
                    let o = std::process::Command::new(cmd)
                        .args(args)
                        .stdin(std::process::Stdio::null())
                        .output()
                        .ok()?;
                    Some(String::from_utf8_lossy(&o.stdout).to_string())
                };
                // 現行バージョン: "2.1.218 (Claude Code)" の先頭トークン
                let current = out("claude", &["--version"])
                    .and_then(|s| s.split_whitespace().next().map(str::to_string))
                    .unwrap_or_default();
                // アカウント: 公式 `claude auth status --json`（email・組織名）。
                // 表示名は公式 IF に無いため .claude.json の oauthAccount.displayName を
                // best-effort で補完し（内部形式・非保証）、無ければ email ローカル部
                let display_name = ccdesk::claude_json_path()
                    .and_then(|p| std::fs::read_to_string(p).ok())
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                    .and_then(|v| {
                        v.pointer("/oauthAccount/displayName")
                            .and_then(|s| s.as_str())
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                    });
                let account = out("claude", &["auth", "status", "--json"])
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                    .map(|v| {
                        if v.get("loggedIn").and_then(|b| b.as_bool()) != Some(true) {
                            return "not logged in".to_string();
                        }
                        let email = v
                            .get("email")
                            .and_then(|s| s.as_str())
                            .unwrap_or_default();
                        let name = display_name
                            .unwrap_or_else(|| email.split('@').next().unwrap_or(email).to_string());
                        match v.get("orgName").and_then(|s| s.as_str()) {
                            Some(org) if !org.is_empty() => format!("{name} · {org}"),
                            _ => name,
                        }
                    })
                    .unwrap_or_default();
                // 最新バージョン: claude 本体の更新チェックと同じ公式配布エンドポイント
                // （downloads.claude.ai/claude-code-releases/<channel> が版番号を返す。
                //  チャネルは文書化設定 autoUpdatesChannel に従う。既定 latest）
                let channel = claude_settings_channel();
                let latest = out(
                    "curl",
                    &[
                        "-fsSL",
                        &format!("https://downloads.claude.ai/claude-code-releases/{channel}"),
                    ],
                )
                .map(|s| s.trim().to_string())
                .filter(|l| {
                    l.split('.').count() >= 3
                        && !current.is_empty()
                        && version_newer(l, &current)
                });
                *shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = FooterInfo {
                    account,
                    current,
                    latest,
                };
                dirty.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            std::thread::sleep(Duration::from_secs(1));
            slept += 1;
        }
    });
}

/// 公式のグルーピング切替（Ctrl+S）。デフォルトは State 別
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Grouping {
    State,
    Directory,
}

/// 公式のグループ順: Ready for review → Needs input → Working → Completed
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Group {
    ReadyForReview,
    NeedsInput,
    Working,
    Completed,
}

impl Group {
    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::ReadyForReview => "Ready for review",
            Self::NeedsInput => "Needs input",
            Self::Working => "Working",
            Self::Completed => "Completed",
        }
    }
}

/// ヘッダー集計（N awaiting input · N working · N completed）の分類先
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Bucket {
    Awaiting,
    Working,
    Completed,
}

/// 状態 → 表示（グループ・ラベル・色・スピナー・集計先）の単一マッピング。
/// draw 内に同じ分岐を複製しない（集計と行表示のずれを防ぐ）
#[derive(Clone, Copy)]
pub(crate) struct StateView {
    pub(crate) group: Group,
    pub(crate) label: &'static str,
    pub(crate) color: Color,
    pub(crate) spinning: bool, // Working スピナーの対象
    pub(crate) alive: bool,    // プロセス生存（アイコン形状 ✻/∙）
    pub(crate) bucket: Bucket,
}

/// 文書化された state 値（working|blocked|done|failed|stopped）+ tempo + 生死から表示を決める
pub(crate) fn classify(live_state: &str, tempo_blocked: bool, alive: bool) -> StateView {
    let needs_input = StateView {
        group: Group::NeedsInput,
        label: "Needs input",
        color: C_ATTENTION,
        spinning: false,
        alive,
        bucket: Bucket::Awaiting,
    };
    match live_state {
        "done" => StateView {
            group: Group::Completed,
            label: "Done",
            color: C_OK,
            spinning: false,
            alive,
            bucket: Bucket::Completed,
        },
        "failed" => StateView {
            group: Group::Completed,
            label: "Failed",
            color: C_FAIL,
            spinning: false,
            alive,
            bucket: Bucket::Completed,
        },
        "stopped" => StateView {
            group: Group::Completed,
            label: "Stopped",
            color: ui().dim,
            spinning: false,
            alive,
            bucket: Bucket::Completed,
        },
        "blocked" => needs_input,
        _ if tempo_blocked => needs_input,
        _ if alive => StateView {
            group: Group::Working,
            label: "Working",
            color: C_WORKING,
            spinning: true,
            alive,
            bucket: Bucket::Working,
        },
        // プロセスが居ない working / 不明値: グループと集計を Completed 側に揃える
        _ => StateView {
            group: Group::Completed,
            label: "Idle",
            color: ui().dim,
            spinning: false,
            alive,
            bucket: Bucket::Completed,
        },
    }
}
