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

/// スクリーンショット撮影用の架空フッター（--demo）。実アカウントを出さない。
/// demo ではフッターのポーラーを起動しないので、これが最終値になる
/// （`latest` が None なので更新ボタン行は出ず、`current` は描画に現れない）
pub(crate) fn demo_footer() -> FooterInfo {
    FooterInfo {
        account: AccountStatus::LoggedIn("you · Acme, Inc.".to_string()),
        current: String::new(),
        latest: None,
    }
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
/// 返るため（実測値）、email と同じ情報しか持たないそれは出さない。落とすのは
/// 「組織名自体が自動生成の形をしている」ときだけ。詳細は [`is_personal_org`]。
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
        // 古いプロフィールは使わない。email が空同士の「一致」も認めない
        // （email を返さない認証方式 + emailAddress が空の `.claude.json` で、
        //   無関係な displayName を採用してしまう）
        .filter(|(profile_email, _)| !email.is_empty() && *profile_email == email)
        .map(|(_, display_name)| display_name)
        .filter(|s| !s.is_empty())
        .or_else(|| email.split('@').next().filter(|s| !s.is_empty()));
    let subscription_type = str_of("subscriptionType");
    let org = str_of("orgName").filter(|org| !is_personal_org(org, email, subscription_type));
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

/// 個人アカウントに自動で付く組織名か。
///
/// **不変条件: 組織名それ自体が email 以上の情報を持たないときだけ落とす。**
/// どちらの規則も「自動生成名の形をしているか」を必ず経由するので、`Acme, Inc.`
/// のような実在の名前はどの入力でも隠れない。誤りの向きとして、余計な組織名を
/// 1 行出すより情報を消す方が悪い。
///
/// 次の **どちらか**が成り立てば落とす:
///
/// 1. **email 前方一致**: 実測では個人アカウントの orgName は
///    `"<email>'s Organization"`。接尾辞の表記揺れに耐えるため前方一致で判定する
///    （大小文字は無視する。email と orgName の表記差で漏らさないため）。
///    組織名が利用者本人の email で始まるなら、既に出している email 以上の
///    情報を持たないので出す価値が無い
/// 2. **自動生成名の形 + 既知の個人プラン**: `"…'s Organization"` という所有格の
///    形（実測値と同じ形）をしていて、かつ `subscriptionType` が既知の個人プラン。
///    個人プランに実在の Team/Enterprise 組織は無いので、email 由来でない
///    自動生成名（表示名由来・大小文字違い）も落とせる。
///    照合は所有格まで含めて見る: `"Organization"` で終わるだけを条件にすると
///    `"Contoso Organization"` のような実在の組織名を個人プランで消してしまい、
///    「組織名自体が情報を持たないときだけ落とす」不変条件を破る
///
/// 2 の個人プランは「既知の値の**ホワイトリスト**」であって、team 系の値を並べた
/// ブラックリストではない。ブラックリストにすると、未知の値（将来のプラン名や
/// 別表記）を個人扱いすることになる。手元にあるのは個人 Max のアカウントだけで、
/// Team/Enterprise の `subscriptionType` の値は**実機で未確認**なので推測で
/// 書かない。加えて `subscriptionType` が選択中の組織由来か利用者由来かも
/// 未確認なので、プラン単独では落とさず組織名の形も必ず見る
fn is_personal_org(org: &str, email: &str, subscription_type: Option<&str>) -> bool {
    /// 個人プランの `subscriptionType`。`"max"` は実測値、`"free"` / `"pro"` は
    /// 公表されている個人プラン名。Team/Enterprise の値は未確認なので載せない
    const PERSONAL_PLANS: [&str; 3] = ["free", "pro", "max"];

    let email_derived = !email.is_empty()
        && org
            .to_ascii_lowercase()
            .starts_with(&email.to_ascii_lowercase());
    // 自動生成名の形（実測は "<email>'s Organization"）。email 由来でない
    // 変種を規則 2 で拾うための条件。所有格 "'s" まで含めて見るので、
    // "Contoso Organization" のような実在の組織名は形が一致しない
    let auto_shaped = org.to_ascii_lowercase().contains("'s organization");
    let personal_plan =
        auto_shaped && subscription_type.is_some_and(|t| PERSONAL_PLANS.contains(&t));
    email_derived || personal_plan
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

/// ファイルの (mtime, サイズ)。読めない・存在しないときは None
type AuthFp = Option<(std::time::SystemTime, u64)>;

/// ログイン状態が変わったことを示す安価な signal: 認証情報ファイルの (mtime, サイズ)。
/// ログイン・ログアウト・トークン更新で書き換わるので、これを見て初めて
/// `claude auth status --json`（1 回 ~350ms のプロセス起動）を叩く。
///
/// `.claude.json` は 100KB 超で claude の通常動作でも常時書き換わるため signal に
/// 使えない。認証情報が OS の資格情報マネージャ側にある環境ではこの関数は常に
/// None を返すので、その場合は周期フォールバックだけが効く
fn auth_fingerprint() -> AuthFp {
    file_fingerprint(ccdesk::claude_dir()?.join(".credentials.json"))
}

/// ファイルの (mtime, サイズ)。存在しない・読めないときは None。
/// 「消えた」も None への変化として検出できる
fn file_fingerprint(path: impl AsRef<std::path::Path>) -> AuthFp {
    let md = std::fs::metadata(path).ok()?;
    Some((md.modified().ok()?, md.len()))
}

/// `.claude.json` の `oauthAccount` から (emailAddress, displayName) を読む。
/// email は「このプロフィールが今のアカウントのものか」の照合に使う
/// （[`parse_account`] 参照）。
///
/// このファイルは 100KB 超で全 claude セッションが常時書き換えるため、読み取りは
/// 普通に失敗しうる（書き換え途中のパース失敗、Windows では共有モードの都合で
/// open 自体の失敗）
fn read_profile() -> Option<(String, String)> {
    let v = ccdesk::claude_json_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())?;
    let s = |key: &str| {
        v.pointer(&format!("/oauthAccount/{key}"))
            .and_then(|x| x.as_str())
            .map(str::to_string)
    };
    Some((s("emailAddress")?, s("displayName")?))
}

/// アカウント行の取得。最後に読めたプロフィールを持ち越す。
///
/// `.claude.json` の一時的な読み取り失敗でプロフィールが落ちると、表示名は email
/// ローカル部へ退化する（`alice` → `taro`）。これは `Unknown` ではなく `LoggedIn`
/// なので「Unknown では上書きしない」規則では守れず、ラベルが 2 周期のあいだ
/// 目に見えて揺れる。前回読めた値を使い回して防ぐ。
/// 採否の判断は [`parse_account`] の email 照合に委ねるので、別アカウントへ
/// 再ログインした後に古いプロフィールが使われることはない
#[derive(Default)]
struct AccountFetcher {
    last_profile: Option<(String, String)>,
}

impl AccountFetcher {
    fn fetch(&mut self) -> AccountStatus {
        if let Some(profile) = read_profile() {
            self.last_profile = Some(profile);
        }
        let profile = self
            .last_profile
            .as_ref()
            .map(|(email, display_name)| (email.as_str(), display_name.as_str()));
        match out("claude", &["auth", "status", "--json"]) {
            Some(json) => parse_account(&json, profile),
            None => AccountStatus::Unknown,
        }
    }
}

/// アカウント行の 1 回取得（`ccdesk doctor` 用。ポーラーと同じ経路で
/// 「今どう表示されるか」を出す）
pub(crate) fn fetch_account() -> AccountStatus {
    AccountFetcher::default().fetch()
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
    // タイムアウトは必須: このスレッドはアカウント取得と共用なので、curl が
    // 応答しないネットワーク（DNS シンクホール・blackhole されたプロキシ）で
    // ぶら下がるとアカウント行の更新まで止まる。返るのは版番号 1 行だけなので
    // 接続 3s・全体 8s あれば十分で、失敗しても次は 1 時間後に再試行する
    let latest = out(
        "curl",
        &[
            "-fsSL",
            "--connect-timeout",
            "3",
            "--max-time",
            "8",
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
/// 取得失敗後の再試行間隔（秒）。失敗（`Unknown`）は認証ファイルを書き換えないので、
/// 通常のフォールバックで待つと 1 回の空振り（起動直後に `claude` の起動が
/// 間に合わない等）でアカウント行が 60 秒間空のまま残る。失敗だけは短く再試行する
const ACCOUNT_RETRY_SECS: u64 = 5;
/// バージョンチェックの周期（秒）。外部ネットワークへ出るので頻繁には回さない
const VERSION_INTERVAL_SECS: u64 = 3600;

/// 取得した値を表示へ反映するか。反映しないなら None。
///
/// - `Unknown`（取得失敗）は既存表示を上書きしない。一時的な失敗でアカウント行が
///   消えたり "not logged in" に化けたりしないため
/// - `LoggedOut` は上書きする（ログアウトを反映しないと嘘の表示になる）
/// - 同値なら None（無駄な再描画をしない）
fn next_account(shown: &AccountStatus, fetched: AccountStatus) -> Option<AccountStatus> {
    if fetched == AccountStatus::Unknown || *shown == fetched {
        return None;
    }
    Some(fetched)
}

/// 再取得の契機があるか。`changed` は監視対象の変化（認証ファイルの書き換え）、
/// `forced` は明示要求（`claude update` 完了時など）。どちらも無ければ周期待ち
fn refetch_due(age: u64, interval: u64, changed: bool, forced: bool) -> bool {
    age >= interval || changed || forced
}

/// アカウントポーリングの持ち越し状態
struct AccountPollState {
    /// 前回 **取得の前に** 観測した認証ファイルの fingerprint
    last_fp: AuthFp,
    /// 最後の取得からの経過秒
    age: u64,
    /// 直前の取得が失敗（`Unknown`）だったか。次の待ち時間を選ぶのに使う
    last_failed: bool,
}

impl AccountPollState {
    /// 起動直後は 1 度取得する（認証ファイルが無い環境では fingerprint が
    /// 変化せず、周期フォールバックまでアカウント行が出ないため）
    fn new() -> Self {
        Self {
            last_fp: None,
            age: u64::MAX / 2,
            last_failed: false,
        }
    }

    /// 次の取得までの待ち時間。失敗直後だけ短くする（[`ACCOUNT_RETRY_SECS`]）
    fn interval(&self) -> u64 {
        if self.last_failed {
            ACCOUNT_RETRY_SECS
        } else {
            ACCOUNT_FALLBACK_SECS
        }
    }
}

/// アカウント 1 周分の判定。表示を差し替えるなら新しい値を返す。
/// IO を引数（`read_fp` / `fetch`）で受けるので単体テストできる。
///
/// **fingerprint は取得の前に読んで `last_fp` に残す。** 取得後に読み直すと、
/// 取得中（子プロセスが認証情報を読んだ後）に入ったログインが「もう反映済み」に
/// 見えて次の周で拾えず、古い表示が周期フォールバック（60s）まで残る。
/// `claude auth status` 自身は認証ファイルを書き換えない（実行前後で mtime・
/// サイズが同一と実測）ので、取得起因の空振りは起きない
fn account_step(
    state: &mut AccountPollState,
    shown: &AccountStatus,
    forced: bool,
    read_fp: impl Fn() -> AuthFp,
    fetch: impl FnOnce() -> AccountStatus,
) -> Option<AccountStatus> {
    let fp = read_fp();
    let auth_changed = fp != state.last_fp;
    state.last_fp = fp;
    if !refetch_due(state.age, state.interval(), auth_changed, forced) {
        return None;
    }
    state.age = 0;
    let fetched = fetch();
    // 失敗はフォールバック全体（60s）を消費させない。認証ファイルは変わっていないので
    // 次の契機が周期しか無く、そのままではアカウント行が空で固まる
    state.last_failed = fetched == AccountStatus::Unknown;
    next_account(shown, fetched)
}

/// フッター情報のバックグラウンド取得。
/// アカウントとバージョンは変化の速さが違うので別々の周期で回す:
/// - アカウント: 認証ファイルの変化で即時 + 60s フォールバック
///   （ログイン・ログアウトを 1 時間待たずに反映するため。取得失敗の直後だけ
///   5s で再試行する）
/// - バージョン: 1 時間毎 + `claude update` 完了時の再取得要求
pub(crate) fn spawn_footer_poller(
    shared: Arc<Mutex<FooterInfo>>,
    dirty: Arc<std::sync::atomic::AtomicBool>,
    refresh: Arc<std::sync::atomic::AtomicBool>,
) {
    std::thread::spawn(move || {
        let mut account = AccountPollState::new();
        let mut fetcher = AccountFetcher::default();
        // 共有側へ最後に書いたアカウント。書き手はこのスレッドだけなので、
        // 毎秒ロックを取らずに手元の写しと比べられる
        let mut shown = AccountStatus::default();
        let mut version_age = u64::MAX / 2; // 初回は即取得
        loop {
            let forced = refresh.swap(false, std::sync::atomic::Ordering::Relaxed);
            let mut updated = false;

            if let Some(next) = account_step(&mut account, &shown, forced, auth_fingerprint, || {
                fetcher.fetch()
            }) {
                shown = next.clone();
                shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .account = next;
                updated = true;
            }

            if refetch_due(version_age, VERSION_INTERVAL_SECS, false, forced) {
                version_age = 0;
                let (current, latest) = fetch_version();
                // 取得に失敗した（current が空）ときは書かない。1 回の失敗で
                // バージョン表記と更新ボタン行が 1 時間消えるのを防ぐ
                if !current.is_empty() {
                    let mut guard = shared
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if guard.current != current || guard.latest != latest {
                        guard.current = current;
                        guard.latest = latest;
                        updated = true;
                    }
                }
            }

            if updated {
                dirty.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            std::thread::sleep(Duration::from_secs(1));
            account.age += 1;
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

    /// PERSONAL の自動生成組織名（差し替えの土台）
    const PERSONAL_ORG: &str = "taro@example.com's Organization";

    /// 実測した個人アカウントの出力（email・orgId は架空の値に差し替え済み。
    /// フィールドの並びと種類は実物と同じ）
    const PERSONAL: &str = r#"{
        "loggedIn": true,
        "authMethod": "claude.ai",
        "apiProvider": "firstParty",
        "email": "taro@example.com",
        "orgId": "00000000-0000-0000-0000-000000000000",
        "orgName": "taro@example.com's Organization",
        "subscriptionType": "max"
    }"#;

    /// PERSONAL と同じ形で orgName / subscriptionType だけを差し替えた出力を作る。
    /// `subscription` が None なら `subscriptionType` フィールド自体を落とす
    /// （＝プランが分からない出力。組織名の形だけで判定させるケース）
    fn auth_json(org: &str, subscription: Option<&str>) -> String {
        let mut v = serde_json::json!({
            "loggedIn": true,
            "authMethod": "claude.ai",
            "apiProvider": "firstParty",
            "email": EMAIL,
            "orgId": "00000000-0000-0000-0000-000000000000",
            "orgName": org,
        });
        if let Some(subscription) = subscription {
            v["subscriptionType"] = subscription.into();
        }
        v.to_string()
    }

    /// テスト用の fingerprint。値そのものに意味は無く「変わったか」だけを見る
    fn fp_of(size: u64) -> AuthFp {
        Some((std::time::UNIX_EPOCH, size))
    }

    /// スコープを抜けるときに必ずファイルを消す番人。アサート失敗で
    /// パニックしても Drop は走るので、一時ファイルを残さない
    struct TempFile(std::path::PathBuf);

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    impl TempFile {
        /// 並列実行・別チェックアウトの同時実行と衝突しないよう、
        /// テスト名とプロセス ID でパスを一意にする
        fn new(test_name: &str) -> Self {
            Self(std::env::temp_dir().join(format!(
                "ccdesk-{test_name}-{}.json",
                std::process::id()
            )))
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    #[test]
    fn suppresses_auto_generated_org_name() {
        assert_eq!(
            parse_account(PERSONAL, None),
            AccountStatus::LoggedIn("taro".into())
        );
    }

    /// email 前方一致（規則 1）だけで落とせることの固定。`subscriptionType` を
    /// 落とした出力＝プラン不明でも、組織名が email 由来なら出さない
    #[test]
    fn suppresses_email_derived_org_name_without_subscription_type() {
        // 接尾辞の表記が変わっても email 前方一致で落とす
        for org in [
            PERSONAL_ORG,
            "taro@example.com のオーガナイゼーション",
            "taro@example.com",
        ] {
            assert_eq!(
                parse_account(&auth_json(org, None), None),
                AccountStatus::LoggedIn("taro".into()),
                "org: {org:?}"
            );
        }
    }

    /// 規則 1 は大小文字を無視する。email と orgName の表記差で
    /// 自動生成名を漏らさない
    #[test]
    fn suppresses_email_derived_org_name_ignoring_case() {
        assert_eq!(
            parse_account(&auth_json("TARO@EXAMPLE.COM's Organization", None), None),
            AccountStatus::LoggedIn("taro".into())
        );
    }

    /// 実在の組織名は出す。プランが分からない出力（`subscriptionType` 不在）では
    /// 規則 1 しか効かず、email 由来でない組織名は情報として残す
    #[test]
    fn keeps_real_org_name() {
        assert_eq!(
            parse_account(&auth_json("Acme, Inc.", None), None),
            AccountStatus::LoggedIn("taro · Acme, Inc.".into())
        );
    }

    /// 不変条件: 個人プランでも、組織名自体が情報を持つなら隠さない。
    /// `subscriptionType` が選択中の組織由来か利用者由来かは未確認なので、
    /// プランだけを根拠に実在の組織名を消してはいけない
    #[test]
    fn keeps_real_org_name_on_personal_plan() {
        for plan in ["free", "pro", "max"] {
            assert_eq!(
                parse_account(&auth_json("Acme, Inc.", Some(plan)), None),
                AccountStatus::LoggedIn("taro · Acme, Inc.".into()),
                "plan: {plan:?}"
            );
        }
    }

    /// 規則 2: 自動生成名の形 + 既知の個人プランなら、email 由来でなくても落とす
    /// （表示名由来の変種を規則 1 の網から漏らさない）
    #[test]
    fn suppresses_auto_shaped_org_name_on_personal_plan() {
        for plan in ["free", "pro", "max"] {
            assert_eq!(
                parse_account(&auth_json("Alice's Organization", Some(plan)), None),
                AccountStatus::LoggedIn("taro".into()),
                "plan: {plan:?}"
            );
        }
        // 規則 2 は 2 条件が揃って初めて効く。プランが分からなければ落とさない
        assert_eq!(
            parse_account(&auth_json("Alice's Organization", None), None),
            AccountStatus::LoggedIn("taro · Alice's Organization".into())
        );
    }

    /// 規則 2 は所有格 "'s Organization" の形だけを自動生成名と見なす。
    /// `"Organization"` で終わるだけの実在組織名は、個人プランでも消さない
    #[test]
    fn keeps_real_org_name_ending_with_organization_on_personal_plan() {
        for plan in ["free", "pro", "max"] {
            assert_eq!(
                parse_account(&auth_json("Contoso Organization", Some(plan)), None),
                AccountStatus::LoggedIn("taro · Contoso Organization".into()),
                "plan: {plan:?}"
            );
        }
    }

    /// ホワイトリストの要: 未知の `subscriptionType` は落とす根拠にしない。
    /// 落としてしまうと実在の Team/Enterprise 組織名を隠すことになる
    /// （Team/Enterprise 側の値は実機で未確認なので、未知として扱われる）
    #[test]
    fn keeps_real_org_name_for_unknown_subscription_type() {
        for plan in ["team", "enterprise", "", "MAX"] {
            assert_eq!(
                parse_account(&auth_json("Acme, Inc.", Some(plan)), None),
                AccountStatus::LoggedIn("taro · Acme, Inc.".into()),
                "plan: {plan:?}"
            );
        }
    }

    #[test]
    fn prefers_display_name_over_email_local_part() {
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
    fn ignores_stale_display_name_from_another_account() {
        assert_eq!(
            parse_account(PERSONAL, Some(("hanako@example.com", "hanako"))),
            AccountStatus::LoggedIn("taro".into()),
            "email が一致しないプロフィールを使ってしまっている"
        );
    }

    /// email を返さない認証方式 + emailAddress が空のプロフィール。
    /// 空同士を「一致」と見なして無関係な displayName を採用してはいけない
    #[test]
    fn ignores_profile_when_email_is_empty() {
        assert_eq!(
            parse_account(
                r#"{"loggedIn": true, "authMethod": "claude.ai"}"#,
                Some(("", "alice"))
            ),
            AccountStatus::LoggedIn("claude.ai".into())
        );
    }

    #[test]
    fn treats_logged_out_output_as_logged_out() {
        assert_eq!(
            parse_account(r#"{"loggedIn": false}"#, None),
            AccountStatus::LoggedOut
        );
    }

    #[test]
    fn treats_unparsable_output_as_unknown() {
        // CLI が何も出さなかった / JSON でない / loggedIn が無い
        for bad in ["", "not json", "{}", r#"{"email":"a@b.c"}"#] {
            assert_eq!(
                parse_account(bad, None),
                AccountStatus::Unknown,
                "input: {bad:?}"
            );
        }
    }

    /// 空の orgName は「無い」ものとして扱う（`"taro · "` にしない）。
    /// プラン不在の出力を使う: 個人プランの出力だと組織名が別の規則でも落ちるため、
    /// 空文字フィルタが効いているかを固定できない
    #[test]
    fn falls_back_to_email_local_part_when_org_name_is_empty() {
        assert_eq!(
            parse_account(&auth_json("", None), None),
            AccountStatus::LoggedIn("taro".into())
        );
    }

    /// 認証ファイルの変化検知。ログイン・ログアウトを即時反映する土台なので、
    /// 「書き換え」と「消滅」の両方が変化として見えることを固定する
    #[test]
    fn detects_credential_file_rewrite_and_removal() {
        let temp = TempFile::new("detects_credential_file_rewrite_and_removal");
        let path = temp.path();
        assert_eq!(file_fingerprint(path), None, "無いファイルは None");

        std::fs::write(path, "a").unwrap();
        let first = file_fingerprint(path);
        assert!(first.is_some());

        // 長さが変わればサイズで検出できる（mtime の粒度に依存しない）
        std::fs::write(path, "abcd").unwrap();
        assert_ne!(file_fingerprint(path), first, "書き換えを検出できていない");

        // ログアウトでファイルが消えるケース
        std::fs::remove_file(path).unwrap();
        assert_eq!(file_fingerprint(path), None, "消滅を検出できていない");
    }

    #[test]
    fn keeps_org_name_when_email_is_missing() {
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
    fn never_produces_an_empty_label() {
        assert_eq!(
            parse_account(r#"{"loggedIn": true, "authMethod": "claude.ai"}"#, None),
            AccountStatus::LoggedIn("claude.ai".into())
        );
        assert_eq!(
            parse_account(r#"{"loggedIn": true}"#, None),
            AccountStatus::LoggedIn("logged in".into())
        );
    }

    /// 取得失敗（Unknown）で既存表示を消さない。一時的な失敗でアカウント行が
    /// 消えたり "not logged in" に化けたりしないため
    #[test]
    fn unknown_does_not_overwrite_known_account() {
        let shown = AccountStatus::LoggedIn("taro".into());
        assert_eq!(next_account(&shown, AccountStatus::Unknown), None);
    }

    /// 起動直後の失敗も同じ扱い（Unknown のまま = 行を出さない）
    #[test]
    fn unknown_on_the_first_cycle_leaves_state_unknown() {
        assert_eq!(
            next_account(&AccountStatus::Unknown, AccountStatus::Unknown),
            None
        );
    }

    /// ログアウトは反映しないと嘘の表示になるので上書きする
    #[test]
    fn logged_out_overwrites_known_account() {
        let shown = AccountStatus::LoggedIn("taro".into());
        assert_eq!(
            next_account(&shown, AccountStatus::LoggedOut),
            Some(AccountStatus::LoggedOut)
        );
    }

    /// 同値なら再描画しない
    #[test]
    fn identical_account_produces_no_update() {
        let shown = AccountStatus::LoggedIn("taro".into());
        assert_eq!(next_account(&shown, shown.clone()), None);
    }

    #[test]
    fn refetch_is_due_on_age_change_or_force() {
        const FALLBACK: u64 = ACCOUNT_FALLBACK_SECS;
        assert!(
            refetch_due(FALLBACK, FALLBACK, false, false),
            "周期フォールバック"
        );
        assert!(refetch_due(0, FALLBACK, true, false), "認証ファイルの変化");
        assert!(refetch_due(0, FALLBACK, false, true), "明示要求");
        assert!(
            !refetch_due(FALLBACK - 1, FALLBACK, false, false),
            "契機が無い"
        );
    }

    /// 取得失敗（`Unknown`）はフォールバック全体（60s）を消費しない。
    /// 認証ファイルは変わらないので周期しか契機が無く、そのままでは
    /// 起動直後の 1 回の空振りでアカウント行が 60 秒間空のまま残る
    #[test]
    fn failed_fetch_retries_before_the_fallback_interval() {
        let mut state = AccountPollState::new();
        let account = AccountStatus::LoggedIn("taro".into());

        // 1 周目: 起動直後の取得が失敗（claude の起動に失敗した等）
        assert_eq!(
            account_step(
                &mut state,
                &AccountStatus::Unknown,
                false,
                || None,
                || AccountStatus::Unknown
            ),
            None
        );

        // 失敗直後は短い間隔で再試行する（認証ファイルは変化していない）
        state.age = ACCOUNT_RETRY_SECS;
        assert_eq!(
            account_step(&mut state, &AccountStatus::Unknown, false, || None, || {
                account.clone()
            }),
            Some(account.clone()),
            "失敗後の再試行がフォールバック（60s）まで来ていない"
        );

        // 成功した後は通常の周期に戻す（短周期で claude を叩き続けない）
        state.age = ACCOUNT_RETRY_SECS;
        assert_eq!(
            account_step(&mut state, &account, false, || None, || panic!(
                "成功後も短周期で再取得している"
            )),
            None
        );
    }

    /// 取得中（子プロセスが認証情報を読んだ後）にログインが入るケース。
    /// fingerprint を取得の **前に** 記録するので、次の周で変化として拾える。
    /// 取得後に読み直すと「もう反映済み」に見えて古い表示が 60s 残る
    #[test]
    fn login_during_fetch_is_picked_up_on_the_next_cycle() {
        let fp = std::cell::Cell::new(fp_of(1));
        let mut state = AccountPollState::new();
        let old = AccountStatus::LoggedIn("taro".into());
        let new = AccountStatus::LoggedIn("hanako".into());

        // 1 周目: 取得中に認証ファイルが書き換わる。子プロセスは変更前の
        // 認証情報を読んでいるので、古いアカウントが返る
        let first = account_step(
            &mut state,
            &AccountStatus::Unknown,
            false,
            || fp.get(),
            || {
                fp.set(fp_of(2));
                old.clone()
            },
        );
        assert_eq!(first, Some(old.clone()));

        // 2 周目: age は 0 に戻っているので、fingerprint の変化だけが契機になる
        let second = account_step(&mut state, &old, false, || fp.get(), || new.clone());
        assert_eq!(
            second,
            Some(new),
            "取得中に入ったログインを次の周で拾えていない"
        );
    }
}
