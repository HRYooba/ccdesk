//! バックグラウンド取得（agents --json / フッター / 使用率）と状態分類。
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ratatui::style::Color;

use ccdesk::{claude_settings_channel, version_newer};

use crate::accounts::{Account, AccountStore, ActiveAccount, CredentialsFp};
use crate::theme::{ui, C_ATTENTION, C_FAIL, C_OK, C_WORKING};

/// `claude agents --json --all` の 1 エントリ（公式のスクリプト向けライブデータ）。
///
/// **前景移行後に読むのは前景セッションを名指しできる項目だけ。** `sessionId` が
/// ccdesk の行（[`crate::sessions::SessionRow`]）と同じ鍵で、`status` が前景
/// セッションの生きた状態（`~/.claude/sessions/<pid>.json` に書かれる
/// busy|idle|waiting|shell）。bg 専用の `id`（short）・`state`・要約は読まない
/// ＝ 非公開の内部形式に依存する経路をここで断つ（`docs/foreground-migration.md`）
#[derive(Clone, Default)]
pub(crate) struct AgentInfo {
    /// transcript の `sessionId`（＝ `claude --session-id` へ渡した UUID）
    pub(crate) session_id: String,
    /// `"interactive"` | `"background"` 等。前景セッションの行だけを突き合わせる
    pub(crate) kind: String,
    /// 前景セッションが書くライブ状態（busy|idle|waiting|shell）
    pub(crate) status: String,
    /// そのセッションを動かしているプロセス（文書化: 生存中のみ pid が載る）。
    ///
    /// **`sessionId` と対で読むのが要点**: ペインの中で `/resume` すると
    /// 同じプロセスが別の `sessionId` に移るので、ccdesk が自分の子（pid は
    /// 知っている）が今どのセッションを動かしているかを知る唯一の口になる
    /// （[`crate::app`] の `live_session_of`）
    pub(crate) pid: Option<u32>,
}

impl AgentInfo {
    /// 前景（interactive）セッションか。bg エントリを行の状態の答えにしない
    pub(crate) fn is_interactive(&self) -> bool {
        self.kind == "interactive"
    }
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
                            session_id: s("sessionId"),
                            kind: s("kind"),
                            status: s("status"),
                            // 桁が u32 に収まらない値は pid として使わない
                            pid: v
                                .get("pid")
                                .and_then(serde_json::Value::as_u64)
                                .and_then(|pid| u32::try_from(pid).ok()),
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
    /// ログイン中のアカウント。同一性（email）と表示ラベルの両方を持つ。
    ///
    /// **ラベルだけでは足りない。** 複数アカウントの保管はキーに安定した識別子を
    /// 要求するが、ラベルは組織名の抑制（[`is_personal_org`]）で変わるので不適。
    /// ラベルで同一性を判定すると、表示ロジックと同一性判定に同じ知識が二重化する。
    ///
    /// **「いつの認証情報を見た判断か」まで持つ**（[`ActiveAccount`]）。この値は表示に
    /// 使われるだけでなく、保管への書き込みで「出ていくアカウント」を決める材料にも
    /// なるため、日付の無い同一性だけを渡すとポーラーの追いつき待ちの数秒間に
    /// 別アカウントのトークンを保管へ書いてしまう
    LoggedIn(ActiveAccount),
}

impl AccountStatus {
    /// 観測の指紋を後付けする。**誰がログインしているか**を決めるのはパーサ
    /// （[`parse_account`]）、**いつの認証情報を見た判断か**を知っているのは
    /// 取得側なので、それぞれの持ち場で決めて最後に合わせる
    fn seen_at(self, seen: CredentialsFp) -> Self {
        match self {
            Self::LoggedIn(active) => Self::LoggedIn(ActiveAccount::new(active.account, seen)),
            other => other,
        }
    }
}

/// claude 側のアカウント・バージョン情報。アカウントはサイドバー下部の行、
/// バージョンは上部の claude 版行に出る（`latest` は「更新がある」の有無だけを
/// 決め、新しい番号そのものは幅の都合で表示しない）。
/// アカウントは `claude auth status --json`（公式サブコマンド）、
/// 現行版は `claude --version`、最新版は Anthropic 公式配布の npm パッケージ
/// メタデータ（registry.npmjs.org/@anthropic-ai/claude-code/latest）から取る
#[derive(Clone, Default)]
pub(crate) struct FooterInfo {
    pub(crate) account: AccountStatus, // ログイン状態 + アカウント（email とラベル）
    pub(crate) current: String,        // claude の現行バージョン
    pub(crate) latest: Option<String>, // 新しい版があるときだけ Some
}

/// ccdesk 自身の版チェック（起動時 1 回の使い捨てスレッド）。
/// このビルドより新しいリリースタグがあれば共有状態へ書く（無ければ何も書かない
/// ＝版行は最新表示のまま）。値の形は claude 側の [`FooterInfo::latest`] と同じ
/// 「新しい版があるときだけ Some」。
///
/// **周期ポーリングはしない。** ccdesk のリリース頻度は低く、適用には再起動が
/// 必要なので 1 起動につき 1 回で足りる（claude のバージョン監視＝
/// [`spawn_footer_poller`] の 1 時間周期とは別物なので混ぜない）。
/// 通信に数百 ms かかるため起動はブロックしない
pub(crate) fn spawn_ccdesk_version_check(
    shared: Arc<Mutex<Option<String>>>,
    dirty: Arc<std::sync::atomic::AtomicBool>,
) {
    std::thread::spawn(move || {
        let Some(tag) = crate::update::newer_tag() else {
            return;
        };
        *shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tag);
        dirty.store(true, std::sync::atomic::Ordering::Relaxed);
    });
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
    // email は表示に使わない（サイドバーに出るのはラベル）が、アカウントの
    // 同一性として持ち回す。email を返さない認証方式では空になり、その場合は
    // 保管できない（[`crate::accounts::AccountStore::register`]）。
    // 観測の指紋は取得側が付ける（[`AccountStatus::seen_at`]）
    AccountStatus::LoggedIn(ActiveAccount::unseen(Account::new(email, label)))
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

/// ポーラーが持ち回す保管ストア。**パスの解決は 1 起動につき 1 回**。
///
/// 持ち上げてある理由: [`AccountStore::detect`] は [`crate::accounts::Paths::detect`]
/// を通り、そこで呼ぶ `accounts_store_path()` と `usage_cache_path()` がどちらも
/// `ccdesk_dir()`（＝ `create_dir_all`）を経由する。指紋読みはポーラーが**毎秒**
/// 呼ぶので、毎ティックでストアを作り直すと `metadata()` 1 回で済む処理が
/// **毎秒 create_dir_all 2 回 + stat 1 回**になる（ccdesk のインスタンスごとに永続的に）。
///
/// ホームが取れない環境では None のまま（指紋は常に None ＝ 周期フォールバックだけが
/// 効く）。起動後にホームが生える環境は無いので、取り直しはしない
struct AuthWatch(Option<AccountStore>);

impl AuthWatch {
    fn new(store: Option<AccountStore>) -> Self {
        Self(store)
    }

    fn detect() -> Self {
        Self::new(AccountStore::detect())
    }

    /// ログイン状態が変わったことを示す安価な signal: 認証情報ファイルの指紋。
    /// ログイン・ログアウト・トークン更新で書き換わるので、これを見て初めて
    /// `claude auth status --json`（1 回 ~350ms のプロセス起動）を叩く。
    ///
    /// `.claude.json` は 100KB 超で claude の通常動作でも常時書き換わるため signal に
    /// 使えない。認証情報が OS の資格情報マネージャ側にある環境ではこれは常に
    /// None を返すので、その場合は周期フォールバックだけが効く。
    ///
    /// **指紋の取り方とファイルの位置はドメイン側（[`AccountStore`]）に持たせる。**
    /// 同じ値が「再取得の契機」と「観測がまだ有効かの照合」の両方で使われるので、
    /// 2 通りの導出を持つと片方だけ直る形になる。
    /// **ここが触るのは認証情報ファイルの `metadata()` だけ**（ディレクトリは作らない）
    fn fingerprint(&self) -> CredentialsFp {
        self.0
            .as_ref()
            .and_then(AccountStore::credentials_fingerprint)
    }

    /// 追従更新の呼び出し口。**登録済みアカウントの保管を現行の認証情報に追従させる**
    /// （claude はトークン更新で refreshToken を使い捨てにするため、放置すると保管が
    /// 腐って切替で復元できなくなる。詳細は
    /// [`crate::accounts::AccountStore::sync_active`]）。
    ///
    /// 契機は既存の signal（[`Self::fingerprint`] の変化 + 周期フォールバック）に乗せる。
    /// **新しいポーリングは足さない。** 未登録アカウントには何もしないので、
    /// 「明示登録するまでコピーしない」規則はストア側で守られる。
    ///
    /// 渡すのは **取得の前に読んだ指紋を持った観測**（[`ActiveAccount`]）。
    /// 「誰のトークンか」はここに来るより数百 ms 前（`claude auth status` が
    /// 認証情報を読んだ時点）に決まっているので、ストア側がロック下で照合する
    fn sync(&self, active: &ActiveAccount) {
        let Some(store) = self.0.as_ref() else {
            return;
        };
        // 失敗しても表示は続ける（保管の追従はアカウント行の表示より優先度が低い）。
        // ログにはパスとエラーだけが出る（トークンは載せない）
        if let Err(e) = store.sync_active(active) {
            ccdesk::log_error(&format!("account store sync failed: {e}"));
        }
    }
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
    /// `seen` は **この取得の前に読んだ** 認証情報ファイルの指紋。取得結果は
    /// 「その状態のファイルを見て決めた持ち主」なので、判断とその材料を対で返す
    fn fetch(&mut self, seen: CredentialsFp) -> AccountStatus {
        if let Some(profile) = read_profile() {
            self.last_profile = Some(profile);
        }
        let profile = self
            .last_profile
            .as_ref()
            .map(|(email, display_name)| (email.as_str(), display_name.as_str()));
        match out("claude", &["auth", "status", "--json"]) {
            Some(json) => parse_account(&json, profile).seen_at(seen),
            None => AccountStatus::Unknown,
        }
    }
}

/// アカウント行の 1 回取得（`ccdesk doctor` 用。ポーラーと同じ経路で
/// 「今どう表示されるか」を出す）
pub(crate) fn fetch_account() -> AccountStatus {
    AccountFetcher::default().fetch(AuthWatch::detect().fingerprint())
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
    last_fp: CredentialsFp,
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
/// サイズが同一と実測）ので、取得起因の空振りは起きない。
///
/// **その fingerprint を `fetch` へ渡す。** 取得結果は「その状態のファイルを見て
/// 決めた持ち主」なので、再取得の契機に使う値と観測の日付は同じ 1 回の読みで足りる
/// （2 回読むと、間に入った書き換えで「古い判断に新しい日付」が付きうる）
fn account_step(
    state: &mut AccountPollState,
    shown: &AccountStatus,
    forced: bool,
    read_fp: impl Fn() -> CredentialsFp,
    fetch: impl FnOnce(CredentialsFp) -> AccountStatus,
) -> Option<AccountStatus> {
    let fp = read_fp();
    let auth_changed = fp != state.last_fp;
    state.last_fp = fp;
    if !refetch_due(state.age, state.interval(), auth_changed, forced) {
        return None;
    }
    state.age = 0;
    let fetched = fetch(fp);
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
        // **ループの外で 1 度だけ解決する**（毎ティックのディレクトリ操作をなくす。
        // 理由は [`AuthWatch`]）
        let auth = AuthWatch::detect();
        // 共有側へ最後に書いたアカウント。書き手はこのスレッドだけなので、
        // 毎秒ロックを取らずに手元の写しと比べられる
        let mut shown = AccountStatus::default();
        let mut version_age = u64::MAX / 2; // 初回は即取得
        loop {
            let forced = refresh.swap(false, std::sync::atomic::Ordering::Relaxed);
            let mut updated = false;

            if let Some(next) = account_step(
                &mut account,
                &shown,
                forced,
                || auth.fingerprint(),
                |fp| {
                    let fetched = fetcher.fetch(fp);
                    // 追従更新は「取得できた度」に見る。トークン更新はラベルを変えない
                    // ので、表示の差分（account_step の戻り値）では拾えない
                    if let AccountStatus::LoggedIn(active) = &fetched {
                        auth.sync(active);
                    }
                    fetched
                },
            ) {
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

/// グルーピング（切替の入口はサイドバーの ⊞ group 行のメニュー）。デフォルトは State 別
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Grouping {
    State,
    Directory,
}

/// グループ順: Needs input → Working → Completed。
///
/// **Ready for review は持たない**: 判定材料（PR 番号）は bg の state.json
/// （非公開の内部形式）にしか無く、前景セッションは書かない。正本を
/// `sessions.json` 1 つにする代償として落とした（`docs/foreground-migration.md`）
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Group {
    NeedsInput,
    Working,
    Completed,
}

impl Group {
    pub(crate) fn title(self) -> &'static str {
        match self {
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

/// 生きている前景セッションのライブ状態（`agents --json` の `status`）を、
/// [`classify`] が読む state 値へ写す。**呼ぶのは status が空でないときだけ**
/// （空 ＝ まだ書かれていない・拾えていないので、判断の材料が無い）。
///
/// **これは従の経路**（`docs/foreground-migration.md` のフェーズ3）: 状態の主は
/// hook（[`crate::hooks`]）で、ここへ落ちるのは hook が一度も来ていない行だけ
/// （ccdesk が起こしていないセッション・注入が効かなかった場合）。
/// hook のように Done を区別できないので、出るのは Working か Needs input の 2 つ。
/// `busy` 以外を Needs input へ倒すのは、前景セッションが `busy` でないなら
/// ユーザーの入力を待っているため（`idle` = プロンプト待ち、`waiting` = 確認待ち）
pub(crate) fn foreground_state(status: &str) -> &'static str {
    if status == "busy" { "working" } else { "blocked" }
}

/// state 値（working|blocked|done|failed|stopped）+ 生死から表示を決める
pub(crate) fn classify(live_state: &str, alive: bool) -> StateView {
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
    fn fp_of(size: u64) -> CredentialsFp {
        Some((std::time::UNIX_EPOCH, size))
    }

    /// 期待値の組み立て。ラベル生成規則のテストは PERSONAL の email を使う
    fn logged_in(label: &str) -> AccountStatus {
        logged_in_as(EMAIL, label)
    }

    /// email を明示する版（email を返さない認証方式のケース用）。
    /// 観測の指紋は付けない（パーサは「誰か」だけを決める。日付は
    /// [`AccountStatus::seen_at`] が後から付ける）
    fn logged_in_as(email: &str, label: &str) -> AccountStatus {
        AccountStatus::LoggedIn(ActiveAccount::unseen(Account::new(email, label)))
    }

    /// 保管のキーになる email を落とさないこと。**ラベルとは独立**なので、
    /// 表示名で上書きされても・組織名が抑制されても email は素の値のまま残る
    /// （ラベルで同一性を判定すると表示ロジックと知識が二重化する）
    #[test]
    fn keeps_the_email_as_the_stable_identity() {
        let AccountStatus::LoggedIn(active) = parse_account(PERSONAL, Some((EMAIL, "alice")))
        else {
            panic!("not interpreted as logged in");
        };
        assert_eq!(active.account.email, EMAIL);
        assert_eq!(
            active.account.label, "alice",
            "label generation rule changed"
        );

        // 組織名が出るケースでも email はそのまま
        let AccountStatus::LoggedIn(active) = parse_account(&auth_json("Acme, Inc.", None), None)
        else {
            panic!("not interpreted as logged in");
        };
        assert_eq!(active.account.email, EMAIL);
    }

    /// email を返さない認証方式では email は空（＝保管できないアカウント）。
    /// ラベルで代用しないことの固定
    #[test]
    fn leaves_the_email_empty_when_the_auth_method_has_none() {
        assert_eq!(
            parse_account(r#"{"loggedIn": true, "authMethod": "claude.ai"}"#, None),
            logged_in_as("", "claude.ai")
        );
    }

    #[test]
    fn suppresses_auto_generated_org_name() {
        assert_eq!(
            parse_account(PERSONAL, None),
            logged_in("taro")
        );
    }

    /// email 前方一致（規則 1）だけで落とせることの固定。`subscriptionType` を
    /// 落とした出力＝プラン不明でも、組織名が email 由来なら出さない
    #[test]
    fn suppresses_email_derived_org_name_without_subscription_type() {
        // 接尾辞の表記が変わっても（訳語・別綴りでも）email 前方一致で落とす
        for org in [
            PERSONAL_ORG,
            "taro@example.com's Workspace",
            "taro@example.com",
        ] {
            assert_eq!(
                parse_account(&auth_json(org, None), None),
                logged_in("taro"),
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
            logged_in("taro")
        );
    }

    /// 実在の組織名は出す。プランが分からない出力（`subscriptionType` 不在）では
    /// 規則 1 しか効かず、email 由来でない組織名は情報として残す
    #[test]
    fn keeps_real_org_name() {
        assert_eq!(
            parse_account(&auth_json("Acme, Inc.", None), None),
            logged_in("taro · Acme, Inc.")
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
                logged_in("taro · Acme, Inc."),
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
                logged_in("taro"),
                "plan: {plan:?}"
            );
        }
        // 規則 2 は 2 条件が揃って初めて効く。プランが分からなければ落とさない
        assert_eq!(
            parse_account(&auth_json("Alice's Organization", None), None),
            logged_in("taro · Alice's Organization")
        );
    }

    /// 規則 2 は所有格 "'s Organization" の形だけを自動生成名と見なす。
    /// `"Organization"` で終わるだけの実在組織名は、個人プランでも消さない
    #[test]
    fn keeps_real_org_name_ending_with_organization_on_personal_plan() {
        for plan in ["free", "pro", "max"] {
            assert_eq!(
                parse_account(&auth_json("Contoso Organization", Some(plan)), None),
                logged_in("taro · Contoso Organization"),
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
                logged_in("taro · Acme, Inc."),
                "plan: {plan:?}"
            );
        }
    }

    #[test]
    fn prefers_display_name_over_email_local_part() {
        assert_eq!(
            parse_account(PERSONAL, Some((EMAIL, "alice"))),
            logged_in("alice")
        );
        // 空の表示名は無いものとして扱う
        assert_eq!(
            parse_account(PERSONAL, Some((EMAIL, ""))),
            logged_in("taro")
        );
    }

    /// 別アカウントへ再ログインした直後、`.claude.json` のプロフィールは前の
    /// アカウントのまま残る窓がある。照合しないと「名前が変わらない」ままになる
    #[test]
    fn ignores_stale_display_name_from_another_account() {
        assert_eq!(
            parse_account(PERSONAL, Some(("hanako@example.com", "hanako"))),
            logged_in("taro"),
            "a profile whose email does not match is being used"
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
            logged_in_as("", "claude.ai")
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
            logged_in("taro")
        );
    }

    #[test]
    fn keeps_org_name_when_email_is_missing() {
        // email 不在では自動生成組織名かを判定できないので、情報を消さない側に倒す。
        // 名前は照合できないので使わず、組織名だけを出す
        let json = r#"{"loggedIn": true, "orgName": "Acme, Inc."}"#;
        assert_eq!(
            parse_account(json, Some(("taro@example.com", "taro"))),
            logged_in_as("", "Acme, Inc.")
        );
    }

    /// ログイン済みなのに空ラベルになると Unknown（未取得）と区別が付かない
    #[test]
    fn never_produces_an_empty_label() {
        assert_eq!(
            parse_account(r#"{"loggedIn": true, "authMethod": "claude.ai"}"#, None),
            logged_in_as("", "claude.ai")
        );
        assert_eq!(
            parse_account(r#"{"loggedIn": true}"#, None),
            logged_in_as("", "logged in")
        );
    }

    /// 取得失敗（Unknown）で既存表示を消さない。一時的な失敗でアカウント行が
    /// 消えたり "not logged in" に化けたりしないため
    #[test]
    fn unknown_does_not_overwrite_known_account() {
        let shown = logged_in("taro");
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
        let shown = logged_in("taro");
        assert_eq!(
            next_account(&shown, AccountStatus::LoggedOut),
            Some(AccountStatus::LoggedOut)
        );
    }

    /// 同値なら再描画しない
    #[test]
    fn identical_account_produces_no_update() {
        let shown = logged_in("taro");
        assert_eq!(next_account(&shown, shown.clone()), None);
    }

    #[test]
    fn refetch_is_due_on_age_change_or_force() {
        const FALLBACK: u64 = ACCOUNT_FALLBACK_SECS;
        assert!(
            refetch_due(FALLBACK, FALLBACK, false, false),
            "periodic fallback"
        );
        assert!(refetch_due(0, FALLBACK, true, false), "auth file changed");
        assert!(refetch_due(0, FALLBACK, false, true), "explicit request");
        assert!(
            !refetch_due(FALLBACK - 1, FALLBACK, false, false),
            "no trigger"
        );
    }

    /// 取得失敗（`Unknown`）はフォールバック全体（60s）を消費しない。
    /// 認証ファイルは変わらないので周期しか契機が無く、そのままでは
    /// 起動直後の 1 回の空振りでアカウント行が 60 秒間空のまま残る
    #[test]
    fn failed_fetch_retries_before_the_fallback_interval() {
        let mut state = AccountPollState::new();
        let account = logged_in("taro");

        // 1 周目: 起動直後の取得が失敗（claude の起動に失敗した等）
        assert_eq!(
            account_step(
                &mut state,
                &AccountStatus::Unknown,
                false,
                || None,
                |_| AccountStatus::Unknown
            ),
            None
        );

        // 失敗直後は短い間隔で再試行する（認証ファイルは変化していない）
        state.age = ACCOUNT_RETRY_SECS;
        assert_eq!(
            account_step(&mut state, &AccountStatus::Unknown, false, || None, |_| {
                account.clone()
            }),
            Some(account.clone()),
            "a retry after a failed fetch waits for the whole 60s fallback"
        );

        // 成功した後は通常の周期に戻す（短周期で claude を叩き続けない）
        state.age = ACCOUNT_RETRY_SECS;
        assert_eq!(
            account_step(&mut state, &account, false, || None, |_| panic!(
                "still refetching on the short retry interval after a success"
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
        let old = logged_in("taro");
        let new = logged_in_as("hanako@example.com", "hanako");

        // 1 周目: 取得中に認証ファイルが書き換わる。子プロセスは変更前の
        // 認証情報を読んでいるので、古いアカウントが返る
        let first = account_step(
            &mut state,
            &AccountStatus::Unknown,
            false,
            || fp.get(),
            |_| {
                fp.set(fp_of(2));
                old.clone()
            },
        );
        assert_eq!(first, Some(old.clone()));

        // 2 周目: age は 0 に戻っているので、fingerprint の変化だけが契機になる
        let second = account_step(&mut state, &old, false, || fp.get(), |_| new.clone());
        assert_eq!(
            second,
            Some(new),
            "a login that landed during the fetch is not picked up on the next cycle"
        );
    }

    /// **取得へ渡る指紋は「取得の前に読んだ値」。** 取得結果は「その状態の
    /// 認証情報を見て決めた持ち主」なので、取得後の値を渡すと *古い判断に新しい日付*
    /// が付き、ロック下の照合（[`crate::accounts::AccountStore::sync_active`]）を
    /// 通してしまう ＝ 別アカウントのトークンを古い email の保管へ書く
    #[test]
    fn the_fetched_account_is_dated_with_the_fingerprint_read_before_it() {
        let fp = std::cell::Cell::new(fp_of(1));
        let mut state = AccountPollState::new();

        let fetched = account_step(
            &mut state,
            &AccountStatus::Unknown,
            false,
            || fp.get(),
            |seen| {
                // 取得中（子プロセスが認証情報を読んだ後）に書き換わる
                fp.set(fp_of(2));
                logged_in("taro").seen_at(seen)
            },
        );

        assert_eq!(
            fetched,
            Some(AccountStatus::LoggedIn(ActiveAccount::new(
                Account::new(EMAIL, "taro"),
                fp_of(1)
            ))),
            "the account is dated with the fingerprint read after the fetch"
        );
    }

    /// **毎ティックの指紋読みはディレクトリを触らない。**
    ///
    /// [`AccountStore::detect`] は `~/.ccdesk` を作る（`accounts_store_path()` と
    /// `usage_cache_path()` がどちらも `ccdesk_dir()` ＝ `create_dir_all` を通る）。
    /// フッターのポーラーは指紋を**毎秒**読むので、そこで detect を通していた頃は
    /// `metadata()` 1 回だった処理が毎秒 create_dir_all 2 回 + stat 1 回になっていた。
    /// ストアを持ち上げてあること（[`AuthWatch`]）を、ディレクトリを消してから
    /// 何度も読んで確かめる ＝ 呼び出し回数を数えずに構造を固定する
    #[test]
    fn reading_the_auth_fingerprint_creates_no_directories() {
        use crate::accounts::tests::{credentials_doc, TempHome};

        let home = TempHome::new("reading_the_auth_fingerprint_creates_no_directories");
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        let auth = AuthWatch::new(Some(home.store()));
        // ストアの置き場所（`~/.ccdesk` 相当）を消す。持ち上げてあれば作り直されない
        let store_dir = home.paths().store.parent().unwrap().to_path_buf();
        std::fs::remove_dir_all(&store_dir).unwrap();

        for _ in 0..3 {
            assert!(auth.fingerprint().is_some(), "the credentials fingerprint cannot be read");
            assert!(
                !store_dir.exists(),
                "reading the fingerprint alone creates the directory — create_dir_all would run every second"
            );
        }
    }
}
