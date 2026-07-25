//! バックグラウンド取得（agents --json / フッター / 使用率）と状態分類。
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ratatui::style::Color;

use ccdesk::{claude_settings_channel, version_newer, BgJob};

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

/// アカウント行の状態。「未取得」と「未ログイン」を区別する
/// （取得失敗を "not logged in" と誤表示しないため）
#[derive(Clone, Default, Debug, PartialEq)]
pub(crate) enum AccountStatus {
    /// まだ取得できていない（起動直後・CLI 実行失敗・JSON 不正）。行は出さない
    #[default]
    Unknown,
    /// `loggedIn: false`。ログインを促す
    LoggedOut,
    /// 表示用ラベル（"alice" または "alice · Acme, Inc."）
    LoggedIn(String),
}

/// サイドバー下部に出すアカウント・バージョン情報。
/// アカウントは `claude auth status --json`（公式サブコマンド）、
/// 現行版は `claude --version`、最新版は Anthropic 公式配布の npm パッケージ
/// メタデータ（registry.npmjs.org/@anthropic-ai/claude-code/latest）から取る
#[derive(Clone, Default)]
pub(crate) struct FooterInfo {
    pub(crate) account: AccountStatus, // "alice · Acme, Inc."（表示名 + 組織名）
    pub(crate) current: String,        // claude の現行バージョン
    pub(crate) latest: Option<String>, // 新しい版があるときだけ Some
}

/// `claude auth status --json` の出力 → アカウント行。
///
/// profile は `.claude.json` の `oauthAccount` の (emailAddress, displayName)。
/// 表示名は公式 IF に無いのでここから best-effort で補う（内部形式・非保証）。
/// **email が一致するときだけ採用する**: `oauthAccount` は遅延取得・キャッシュ
/// （`profileFetchedAt` を持つ）なので、別アカウントへ再ログインした直後は前の
/// アカウントの displayName が残る窓があり、照合しないと名前が変わらない。
///
/// 組織名は「実在の Team/Enterprise 組織のときだけ」出す。個人アカウントでも
/// orgName は空にならず `"<email>'s Organization"` という email 由来の自動生成名が
/// 返るため（実測値）、email と同じ情報しか持たないそれは出さない。
///
/// 判定は終了コードを見ない: 未ログインは **exit 1 + 正当な JSON**（実測）なので、
/// 「JSON が読めたか」「loggedIn があるか」だけで決める
fn parse_account(auth_json: &str, profile: Option<(&str, &str)>) -> AccountStatus {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(auth_json) else {
        return AccountStatus::Unknown;
    };
    match v.get("loggedIn").and_then(|b| b.as_bool()) {
        Some(true) => {}
        Some(false) => return AccountStatus::LoggedOut,
        None => return AccountStatus::Unknown, // 想定外の形。未ログインと断定しない
    }
    let str_of = |key: &str| v.get(key).and_then(|s| s.as_str()).filter(|s| !s.is_empty());
    let email = str_of("email").unwrap_or_default();
    let name = profile
        .filter(|(profile_email, _)| *profile_email == email) // 古いプロフィールは使わない
        .map(|(_, display_name)| display_name)
        .filter(|s| !s.is_empty())
        .or_else(|| email.split('@').next().filter(|s| !s.is_empty()));
    let org = str_of("orgName").filter(|org| !is_personal_org(org, email));
    // 名前も組織も取れない（email を返さない認証方式）ときも空行にはしない。
    // 空ラベルは Unknown（未取得）と区別が付かず、ログイン済みなのに何も出ない
    let label = match (name, org) {
        (Some(name), Some(org)) => format!("{name} · {org}"),
        (Some(name), None) => name.to_string(),
        (None, Some(org)) => org.to_string(),
        (None, None) => str_of("authMethod").unwrap_or("logged in").to_string(),
    };
    AccountStatus::LoggedIn(label)
}

/// 個人アカウントに自動で付く組織名か。実測では `"<email>'s Organization"`。
/// 接尾辞の表記揺れに耐えるため email の前方一致で判定する: 組織名が利用者本人の
/// email で始まるなら、既に出している email 以上の情報を持たないので出す価値が無い。
///
/// email が取れない出力では判定できないので false（= 出す）に倒す。
/// なお `.claude.json` の `oauthAccount.organizationType`（実測 `"claude_max"`）でも
/// 判別できそうだが、Team/Enterprise 側の値を実機で確認できていないため使わない
fn is_personal_org(org: &str, email: &str) -> bool {
    !email.is_empty() && org.starts_with(email)
}

/// 子プロセスの stdout を取る。不正な出力は各パーサ側で弾く。
///
/// **終了コードは意図的に見ない。** `claude auth status --json` は未ログイン時に
/// exit 1 を返しつつ正当な JSON（`{"loggedIn": false, …}`）を stdout に出す（実測）。
/// ここで `status.success()` を要求すると未ログインが「取得失敗」に化けて
/// 表示が固まるため、成否は各パーサの内容判定に委ねる
fn out(cmd: &str, args: &[&str]) -> Option<String> {
    let o = std::process::Command::new(cmd)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&o.stdout).to_string())
}

/// ログイン状態が変わったことを示す安価な signal: 認証情報ファイルの (mtime, サイズ)。
/// ログイン・ログアウト・トークン更新で書き換わるので、これを見て初めて
/// `claude auth status --json`（1 回 ~350ms のプロセス起動）を叩く。
///
/// `.claude.json` は 100KB 超で claude の通常動作でも常時書き換わるため signal に
/// 使えない。認証情報が OS の資格情報マネージャ側にある環境ではこの関数は常に
/// None を返すので、その場合は周期フォールバックだけが効く
fn auth_fingerprint() -> Option<(std::time::SystemTime, u64)> {
    file_fingerprint(ccdesk::claude_dir()?.join(".credentials.json"))
}

/// ファイルの (mtime, サイズ)。存在しない・読めないときは None。
/// 「消えた」も None への変化として検出できる
fn file_fingerprint(path: impl AsRef<std::path::Path>) -> Option<(std::time::SystemTime, u64)> {
    let md = std::fs::metadata(path).ok()?;
    Some((md.modified().ok()?, md.len()))
}

/// アカウント行の取得。表示名は `.claude.json` から best-effort で補完する
/// （`ccdesk doctor` も同じ経路で「今どう表示されるか」を出す）
pub(crate) fn fetch_account() -> AccountStatus {
    // (emailAddress, displayName) を組で取る。email は「このプロフィールが今の
    // アカウントのものか」の照合に使う（parse_account 参照）
    let profile: Option<(String, String)> = ccdesk::claude_json_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| {
            let s = |key: &str| {
                v.pointer(&format!("/oauthAccount/{key}"))
                    .and_then(|x| x.as_str())
                    .map(str::to_string)
            };
            Some((s("emailAddress")?, s("displayName")?))
        });
    let profile = profile.as_ref().map(|(e, d)| (e.as_str(), d.as_str()));
    match out("claude", &["auth", "status", "--json"]) {
        Some(json) => parse_account(&json, profile),
        None => AccountStatus::Unknown,
    }
}

/// 現行バージョンと、それより新しい配布版があれば その版番号。
/// 最新版は claude 本体の更新チェックと同じ公式配布エンドポイント
/// （downloads.claude.ai/claude-code-releases/<channel> が版番号を返す。
///  チャネルは文書化設定 autoUpdatesChannel に従う。既定 latest）
fn fetch_version() -> (String, Option<String>) {
    // 現行バージョン: "2.1.218 (Claude Code)" の先頭トークン
    let current = out("claude", &["--version"])
        .and_then(|s| s.split_whitespace().next().map(str::to_string))
        .unwrap_or_default();
    let channel = claude_settings_channel();
    let latest = out(
        "curl",
        &[
            "-fsSL",
            &format!("https://downloads.claude.ai/claude-code-releases/{channel}"),
        ],
    )
    .map(|s| s.trim().to_string())
    .filter(|l| l.split('.').count() >= 3 && !current.is_empty() && version_newer(l, &current));
    (current, latest)
}

/// アカウントの周期フォールバック（秒）。認証ファイルを見られない環境や、
/// ファイルを経由しない状態変化のための保険
const ACCOUNT_FALLBACK_SECS: u64 = 60;
/// バージョンチェックの周期（秒）。外部ネットワークへ出るので頻繁には回さない
const VERSION_INTERVAL_SECS: u64 = 3600;

/// フッター情報のバックグラウンド取得。
/// アカウントとバージョンは変化の速さが違うので別々の周期で回す:
/// - アカウント: 認証ファイルの変化で即時 + 60s フォールバック
///   （ログイン・ログアウトを 1 時間待たずに反映するため）
/// - バージョン: 1 時間毎 + `claude update` 完了時の再取得要求
pub(crate) fn spawn_footer_poller(
    shared: Arc<Mutex<FooterInfo>>,
    dirty: Arc<std::sync::atomic::AtomicBool>,
    refresh: Arc<std::sync::atomic::AtomicBool>,
) {
    std::thread::spawn(move || {
        let mut account_age = u64::MAX / 2; // 初回は即取得
        let mut version_age = u64::MAX / 2;
        let mut last_fp = auth_fingerprint();
        loop {
            let forced = refresh.swap(false, std::sync::atomic::Ordering::Relaxed);
            let fp = auth_fingerprint();
            let auth_changed = fp != last_fp;
            last_fp = fp;
            let mut updated = false;

            if account_age >= ACCOUNT_FALLBACK_SECS || auth_changed || forced {
                account_age = 0;
                let account = fetch_account();
                // 取得に失敗した（Unknown）ときは既存表示を残す。一時的な失敗で
                // アカウント行が消えたり "not logged in" に化けたりしないため
                let mut guard = shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if account != AccountStatus::Unknown && guard.account != account {
                    guard.account = account;
                    updated = true;
                }
                drop(guard);
                // 取得中に状態が変わっていたら次のループで拾い直す
                last_fp = auth_fingerprint();
            }

            if version_age >= VERSION_INTERVAL_SECS || forced {
                version_age = 0;
                let (current, latest) = fetch_version();
                let mut guard = shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if guard.current != current || guard.latest != latest {
                    guard.current = current;
                    guard.latest = latest;
                    updated = true;
                }
            }

            if updated {
                dirty.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            std::thread::sleep(Duration::from_secs(1));
            account_age += 1;
            version_age += 1;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// PERSONAL の email（プロフィール照合のテストで使う）
    const EMAIL: &str = "taro@example.com";

    /// 実測した個人アカウントの出力（email 以外の値も実物と同じ形）
    const PERSONAL: &str = r#"{
        "loggedIn": true,
        "authMethod": "claude.ai",
        "apiProvider": "firstParty",
        "email": "taro@example.com",
        "orgId": "00000000-0000-0000-0000-000000000000",
        "orgName": "taro@example.com's Organization",
        "subscriptionType": "max"
    }"#;

    #[test]
    fn 個人アカウントの自動生成組織名は出さない() {
        assert_eq!(
            parse_account(PERSONAL, None),
            AccountStatus::LoggedIn("taro".into())
        );
    }

    #[test]
    fn 自動生成組織名の表記揺れも出さない() {
        // 接尾辞が変わっても email 前方一致で落とす
        for org in [
            "taro@example.com's Organization",
            "taro@example.com のオーガナイゼーション",
            "taro@example.com",
        ] {
            let json = PERSONAL.replace("taro@example.com's Organization", org);
            assert_eq!(
                parse_account(&json, None),
                AccountStatus::LoggedIn("taro".into()),
                "org: {org:?}"
            );
        }
    }

    #[test]
    fn 実在の組織名は出す() {
        let json = PERSONAL.replace("taro@example.com's Organization", "Acme, Inc.");
        assert_eq!(
            parse_account(&json, None),
            AccountStatus::LoggedIn("taro · Acme, Inc.".into())
        );
    }

    #[test]
    fn 表示名があればemailローカル部より優先する() {
        assert_eq!(
            parse_account(PERSONAL, Some((EMAIL, "alice"))),
            AccountStatus::LoggedIn("alice".into())
        );
        // 空の表示名は無いものとして扱う
        assert_eq!(
            parse_account(PERSONAL, Some((EMAIL, ""))),
            AccountStatus::LoggedIn("taro".into())
        );
    }

    /// 別アカウントへ再ログインした直後、`.claude.json` のプロフィールは前の
    /// アカウントのまま残る窓がある。照合しないと「名前が変わらない」ままになる
    #[test]
    fn 別アカウントの古い表示名は採用しない() {
        assert_eq!(
            parse_account(PERSONAL, Some(("hanako@example.com", "hanako"))),
            AccountStatus::LoggedIn("taro".into()),
            "email が一致しないプロフィールを使ってしまっている"
        );
    }

    #[test]
    fn 未ログインは未ログインとして扱う() {
        assert_eq!(
            parse_account(r#"{"loggedIn": false}"#, None),
            AccountStatus::LoggedOut
        );
    }

    #[test]
    fn 取得失敗を未ログインと誤判定しない() {
        // CLI が何も出さなかった / JSON でない / loggedIn が無い
        for bad in ["", "not json", "{}", r#"{"email":"a@b.c"}"#] {
            assert_eq!(
                parse_account(bad, None),
                AccountStatus::Unknown,
                "input: {bad:?}"
            );
        }
    }

    #[test]
    fn 組織名が空でもemailだけは出す() {
        let json = PERSONAL.replace("taro@example.com's Organization", "");
        assert_eq!(
            parse_account(&json, None),
            AccountStatus::LoggedIn("taro".into())
        );
    }

    /// 認証ファイルの変化検知。ログイン・ログアウトを即時反映する土台なので、
    /// 「書き換え」と「消滅」の両方が変化として見えることを固定する
    #[test]
    fn 認証ファイルの書き換えと消滅を変化として検出する() {
        let path = std::env::temp_dir().join("ccdesk-fp-test.json");
        let _ = std::fs::remove_file(&path);
        assert_eq!(file_fingerprint(&path), None, "無いファイルは None");

        std::fs::write(&path, "a").unwrap();
        let first = file_fingerprint(&path);
        assert!(first.is_some());

        // 長さが変わればサイズで検出できる（mtime の粒度に依存しない）
        std::fs::write(&path, "abcd").unwrap();
        assert_ne!(file_fingerprint(&path), first, "書き換えを検出できていない");

        // ログアウトでファイルが消えるケース
        std::fs::remove_file(&path).unwrap();
        assert_eq!(file_fingerprint(&path), None, "消滅を検出できていない");
    }

    #[test]
    fn emailが取れないときは組織名を落とさない() {
        // email 不在では自動生成組織名かを判定できないので、情報を消さない側に倒す。
        // 名前は照合できないので使わず、組織名だけを出す
        let json = r#"{"loggedIn": true, "orgName": "Acme, Inc."}"#;
        assert_eq!(
            parse_account(json, Some(("taro@example.com", "taro"))),
            AccountStatus::LoggedIn("Acme, Inc.".into())
        );
    }

    /// ログイン済みなのに空ラベルになると Unknown（未取得）と区別が付かない
    #[test]
    fn 名前も組織も取れなくても空にはしない() {
        assert_eq!(
            parse_account(r#"{"loggedIn": true, "authMethod": "claude.ai"}"#, None),
            AccountStatus::LoggedIn("claude.ai".into())
        );
        assert_eq!(
            parse_account(r#"{"loggedIn": true}"#, None),
            AccountStatus::LoggedIn("logged in".into())
        );
    }
}
