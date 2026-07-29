//! バックグラウンド取得（agents --json / フッター）と状態分類。
//!
//! 使用率は [`crate::usage`] が取得から解釈まで一手に持つ（取得の作法をここと
//! 2 箇所に分けない）。
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ratatui::style::Color;

use ccdesk::{claude_settings_channel, version_newer, LockExt};

// `agents --json` の項目の綴りは文書化されていないので
// [`crate::claude_format`] が持つ（外れたときに直す場所を 1 つにするため）
use crate::claude_format::{
    AGENT_KIND, AGENT_KIND_INTERACTIVE, AGENT_PID, AGENT_SESSION_ID, AGENT_STATUS,
};
use crate::theme::{ui, C_ATTENTION, C_OK, C_WORKING};

/// `claude agents --json --all` の 1 エントリ（公式のスクリプト向けライブデータ）。
///
/// **前景移行後に読むのは前景セッションを名指しできる項目だけ。** `sessionId` が
/// ccdesk の行（[`crate::sessions::SessionRow`]）と同じ鍵で、`status` が前景
/// セッションの生きた状態（`~/.claude/sessions/<pid>.json` に書かれる
/// busy|idle|waiting|shell）。bg 専用の `id`（short）・`state`・要約は読まない
/// ＝ 非公開の内部形式に依存する経路をここで断つ
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
        self.kind == AGENT_KIND_INTERACTIVE
    }
}

/// `claude agents --json --all` を 1 回叩いて解釈する。
/// None ＝ 起動できなかった、または応答が JSON 配列でない。
///
/// **ポーラーと `ccdesk doctor` が同じこの経路を通る**: doctor は
/// 「実際どう見えるか」を確かめる道具なので、本番と別の実装を持つと
/// こちらだけ引数や解釈を変えたときに doctor が嘘の ok を出す
pub(crate) fn fetch_agents() -> Option<Vec<AgentInfo>> {
    let json = out("claude", &["agents", "--json", "--all"])?;
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str::<serde_json::Value>(&json)
    else {
        return None;
    };
    let parsed = items
        .iter()
        .map(|v| {
            let s = |k: &str| {
                v.get(k)
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string()
            };
            AgentInfo {
                session_id: s(AGENT_SESSION_ID),
                kind: s(AGENT_KIND),
                status: s(AGENT_STATUS),
                // 桁が u32 に収まらない値は pid として使わない
                pid: v
                    .get(AGENT_PID)
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|pid| u32::try_from(pid).ok()),
            }
        })
        .collect();
    Some(parsed)
}

/// agents --json は 1 回 ~900ms かかるためバックグラウンドスレッドで回す
pub(crate) fn spawn_agents_poller(
    shared: Arc<Mutex<Vec<AgentInfo>>>,
    dirty: Arc<std::sync::atomic::AtomicBool>,
) {
    std::thread::spawn(move || loop {
        if let Some(parsed) = fetch_agents() {
            *shared.lock_recover() = parsed;
            dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        std::thread::sleep(Duration::from_secs(2));
    });
}

/// アカウント行の状態。「未取得」と「未ログイン」を区別する
/// （取得失敗を "not logged in" と誤表示しないため）。
///
/// **持つのは表示ラベルだけ。** アカウントの切り替えを撤去した今、ccdesk が
/// アカウントについて答えるのは「今サインインしているのは誰か」の 1 行だけなので、
/// email のような同一性の識別子を持ち回す先が無い。
///
/// **切り替えを再び作らないこと。** `claude auth status --json` が返す email の出所は
/// `~/.claude.json` の遅延取得キャッシュで、その更新契機は ccdesk から見えない
/// （`auth status` は同ファイルを書かず、動いている claude が数秒おきに書き換え続けるので
/// mtime も答えの新しさを語らない）。「今このトークンが誰のものか」を確実に知れないまま
/// 保管へ書けば別アカウントのトークンを混ぜ、refreshToken は使い捨てなので
/// **復旧不能な破壊**になる。待ち時間や自己修復は誤る確率を下げるだけで、
/// 被害が復旧不能である以上は機能として成立しない
#[derive(Clone, Default, Debug, PartialEq)]
pub(crate) enum AccountStatus {
    /// まだ取得できていない（起動直後・CLI 実行失敗・JSON 不正）。行は出さない
    #[default]
    Unknown,
    /// `loggedIn: false`。ログインを促す
    LoggedOut,
    /// ログイン中のアカウントの表示ラベル（`alice` または `alice · Acme, Inc.`）
    LoggedIn(String),
}

/// claude 側のアカウント・バージョン情報。アカウントはサイドバー下部の行、
/// バージョンは上部の claude 版行に出る（`latest` は「更新がある」の有無だけを
/// 決め、新しい番号そのものは幅の都合で表示しない）。
/// アカウントは `claude auth status --json`（公式サブコマンド）、
/// 現行版は `claude --version`、最新版は claude 本体の更新チェックと同じ
/// 配布エンドポイント（**取得元の正本は [`fetch_version`]**。ここに URL を
/// 書き写すと、変えたときに片方だけ古いままになる）
#[derive(Clone, Default)]
pub(crate) struct FooterInfo {
    pub(crate) account: AccountStatus, // ログイン状態 + 表示ラベル
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
/// 版行 2 本（claude と ccdesk）の更新チェックの書き込み先。
///
/// **2 本を 1 つの構造体で受けるのは、周期を分けないため**（[`spawn_footer_poller`] が
/// 同じゲートで両方を取る）。片方だけ「起動時 1 回」にしていた頃は、ccdesk を
/// 開いたままにしていると自分の更新に何日も気づけなかった
pub(crate) struct VersionSinks {
    /// claude 側（現行版と、新しい配布版があればその番号）
    pub(crate) claude: Arc<Mutex<FooterInfo>>,
    pub(crate) claude_dirty: Arc<std::sync::atomic::AtomicBool>,
    /// ccdesk 側（自分より新しいリリースタグ。無ければ None）
    pub(crate) ccdesk: Arc<Mutex<Option<String>>>,
    pub(crate) ccdesk_dirty: Arc<std::sync::atomic::AtomicBool>,
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
    // email は行に出さない（出るのはラベル）。ここまでで使い切る
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
/// **TUI 内から起こす子は必ず `stdin(null)` にする**（付け忘れると子が端末を
/// 掴んでハングする）。この作法ごと共有するため doctor もここを通る。
///
/// **終了コードは意図的に見ない。** `claude auth status --json` は未ログイン時に
/// exit 1 を返しつつ正当な JSON（`{"loggedIn": false, …}`）を stdout に出す（実測）。
/// ここで `status.success()` を要求すると未ログインが「取得失敗」に化けて
/// 表示が固まるため、成否は各パーサの内容判定に委ねる
pub(crate) fn out(cmd: &str, args: &[&str]) -> Option<String> {
    let o = std::process::Command::new(cmd)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    // `into_owned` は正常な UTF-8（Borrowed）でも確保 1 回で済む
    // （`.to_string()` は Owned のときに二重確保になる）
    Some(String::from_utf8_lossy(&o.stdout).into_owned())
}

/// 認証情報ファイルが書き換わっていないかを見るための印（サイズと mtime）。
/// 無い・読めないときは `None`（「消えた」も変化として検出できる）。
///
/// **内容のハッシュではない。** 見たいのは「アカウント行を取り直す契機があるか」
/// だけなので、指紋の実体は [`ccdesk::file_stamp`]（変化検出の型を 1 つに保つ）
type CredentialsFp = Option<(u64, std::time::SystemTime)>;

/// ポーラーが見張る認証情報ファイル。**パスの解決は 1 起動につき 1 回**
/// （指紋読みは毎秒走るので、毎ティックで環境変数からパスを組み直さない）。
///
/// ホームが取れない環境では None のまま（指紋は常に None ＝ 周期フォールバックだけが
/// 効く）。起動後にホームが生える環境は無いので、取り直しはしない
struct AuthWatch(Option<std::path::PathBuf>);

impl AuthWatch {
    fn detect() -> Self {
        Self(ccdesk::claude_dir().map(|dir| dir.join(".credentials.json")))
    }

    /// ログイン状態が変わったことを示す安価な signal: 認証情報ファイルの指紋。
    /// ログイン・ログアウト・トークン更新で書き換わるので、これを見て初めて
    /// `claude auth status --json`（1 回 ~350ms のプロセス起動）を叩く。
    ///
    /// `.claude.json` は 100KB 超で claude の通常動作でも常時書き換わるため signal に
    /// 使えない。認証情報が OS の資格情報マネージャ側にある環境ではこれは常に
    /// None を返すので、その場合は周期フォールバックだけが効く。
    ///
    /// **触るのは `metadata()` だけ。** 認証情報の中身は読まない（ccdesk が
    /// このファイルに対して持つ関心は「アカウント行を取り直す契機」1 つだけになった）
    fn fingerprint(&self) -> CredentialsFp {
        ccdesk::file_stamp(self.0.as_ref()?)
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
        .as_deref()
        .and_then(ccdesk::read_json)?;
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
    // ネットワークへ出る作法（タイムアウト等）は [`crate::update::http_get`] が持つ。
    // このスレッドはアカウント取得と共用なので、応答しないネットワークで
    // ぶら下がるとアカウント行の更新まで止まる ＝ タイムアウトが必須な理由
    let latest = crate::update::http_get(&format!(
        "https://downloads.claude.ai/claude-code-releases/{channel}"
    ))
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

    /// 1 秒進める。**進めるのはループで、判断は [`account_step`]**
    /// （`age` を 0 に戻すのは取得した側 ＝ 状態の進み方を 2 箇所に分けない）
    fn tick(&mut self) {
        self.age += 1;
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
    read_fp: impl Fn() -> CredentialsFp,
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
/// - バージョン: **claude と ccdesk の 2 本を同じゲートで**、1 時間毎 +
///   `claude update` 完了時の再取得要求。どちらも**起動時に 1 度取る**
///   （`version_age` の初期値が周期を超えているため）
pub(crate) fn spawn_footer_poller(
    versions: VersionSinks,
    refresh: Arc<std::sync::atomic::AtomicBool>,
) {
    let VersionSinks {
        claude: shared,
        claude_dirty: dirty,
        ccdesk,
        ccdesk_dirty,
    } = versions;
    std::thread::spawn(move || {
        let mut account = AccountPollState::new();
        let mut fetcher = AccountFetcher::default();
        // **ループの外で 1 度だけ解決する**（理由は [`AuthWatch`]）
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
                || fetcher.fetch(),
            ) {
                shown = next.clone();
                shared
                    .lock_recover()
                    .account = next;
                updated = true;
            }

            // **版行 2 本を同じゲートで取る。** 周期を 2 つ持つと、片方だけ
            // 「起動時 1 回」のような別の規則へ流れる（実際そうなっていた）
            if refetch_due(version_age, VERSION_INTERVAL_SECS, false, forced) {
                version_age = 0;
                let (current, latest) = fetch_version();
                // 取得に失敗した（current が空）ときは書かない。1 回の失敗で
                // バージョン表記と更新ボタン行が 1 時間消えるのを防ぐ
                if !current.is_empty() {
                    let mut guard = shared
                        .lock_recover();
                    if guard.current != current || guard.latest != latest {
                        guard.current = current;
                        guard.latest = latest;
                        updated = true;
                    }
                }
                // ccdesk 自身の版。**取得できなかった回は書かない**（claude 側と
                // 同じ判断。1 回の空振りで版行の ⟳ が 1 時間消えるのを防ぐ）。
                // 更新済みで新しい版が無くなったときは None を書いて ⟳ を消す
                if let Some(next) = crate::update::latest_tag() {
                    let next = crate::update::tag_is_newer(&next).then_some(next);
                    let mut guard = ccdesk
                        .lock_recover();
                    if *guard != next {
                        *guard = next;
                        ccdesk_dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }

            if updated {
                dirty.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            std::thread::sleep(Duration::from_secs(1));
            account.tick();
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

impl Grouping {
    /// 表示順（メニューの項目の並びもこれに従う）
    pub(crate) const ORDER: [Self; 2] = [Self::State, Self::Directory];

    /// **保存値（config.json）と画面表示の唯一の綴り**。
    /// 読み・書き・メニュー・現在値表示が別々に綴りを持つと、片方だけ変えたときに
    /// 保存値が読めなくなる（設定が黙って既定へ戻る）
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::State => "state",
            Self::Directory => "directory",
        }
    }

    /// 保存値からの復元。未知の値は既定（State）へ倒す
    pub(crate) fn parse(text: &str) -> Self {
        Self::ORDER
            .into_iter()
            .find(|g| g.as_str() == text)
            .unwrap_or(Self::State)
    }
}

/// 行の状態。**行のラベル・節の見出し・集計の項目がこの 1 つ**で、
/// 並びは緊急度の順（上ほどユーザーの手が要る）。
///
/// **語彙を 1 つに保つのが要点。** 以前は同じ分類を 3 通りの言葉で持っていた
/// （行ラベル `Needs input` / `Done`、節の見出し `Needs input` / `Completed`、
/// 集計 `awaiting input` / `completed`）ので、画面の 3 箇所で別の語が出ていた。
/// [`Self::title`] が唯一の綴りで、集計行はそれを小文字にして使う。
///
/// **`Ready for review` は持たない**: 判定材料（PR 番号）は bg の state.json
/// （非公開の内部形式）にしか無く、前景セッションは書かない。正本を
/// `sessions.json` 1 つにする代償として落とした
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Group {
    /// claude がユーザーの操作を待って止まっている（許可・応答待ち・プロンプト待ち）
    Waiting,
    /// claude が動いている
    Working,
    /// ターンが終わった
    Completed,
    /// プロセスが終了した（行は残る）。**`Completed` と混ぜない**:
    /// 止められたセッションを「完了」と呼ぶのは嘘になる
    Stopped,
}

impl Group {
    /// **この 4 語が画面に出る唯一の綴り**（行ラベル・節の見出し・集計）
    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Waiting => "Waiting",
            Self::Working => "Working",
            Self::Completed => "Completed",
            Self::Stopped => "Stopped",
        }
    }

    /// この状態の色。**行のドット・集計・節が同じこの 1 箇所を読む**ので、
    /// 状態を増やしたときに色の対応を書き忘れる場所が増えない
    /// （以前は `StateView.color` が別に持っていて、`group` と食い違う値を
    /// 作れてしまっていた）
    pub(crate) fn color(self) -> Color {
        match self {
            Self::Waiting => C_ATTENTION,
            Self::Working => C_WORKING,
            Self::Completed => C_OK,
            Self::Stopped => ui().dim,
        }
    }

    /// ドットが明滅するか。**動いている状態だけ**（`Group::Working` と同義）
    pub(crate) fn blinks(self) -> bool {
        self == Self::Working
    }

    /// 表示順（節の並びと集計の並び）。**[`Ord`] と同じ順を 1 箇所で配る**ので、
    /// 節を足したときに並びの書き漏らしが起きない
    pub(crate) const ORDER: [Self; 4] = [
        Self::Waiting,
        Self::Working,
        Self::Completed,
        Self::Stopped,
    ];
}

/// **実行が終わった**ことを表す state 値。書く側（hook の `SessionEnd` ＝
/// [`crate::hooks`]）と、読む側（[`classify`] と行の状態の導出 ＝ [`crate::ui`]）が
/// 同じ綴りを見るための 1 箇所。綴りを 2 通り持つと「hook が言った `stopped`」と
/// 「表示の Stopped」が黙って別物になる
pub(crate) const STOPPED: &str = "stopped";

/// 生きている前景セッションのライブ状態（`agents --json` の `status`）を、
/// [`classify`] が読む state 値へ写す。**呼ぶのは status が空でないときだけ**
/// （空 ＝ まだ書かれていない・拾えていないので、判断の材料が無い）。
///
/// **これは従の経路**: 状態の主は
/// hook（[`crate::hooks`]）で、ここへ落ちるのは hook が一度も来ていない行だけ
/// （ccdesk が起こしていないセッション・注入が効かなかった場合）。
/// hook のように「ターンが終わった」を区別できないので、出るのは Working か
/// Waiting の 2 つ。`busy` 以外を Waiting へ倒すのは、前景セッションが `busy` で
/// ないならユーザーの入力を待っているため（`idle` = プロンプト待ち、
/// `waiting` = 確認待ち）
pub(crate) fn foreground_state(status: &str) -> &'static str {
    if status == STATUS_BUSY { WORKING } else { WAITING }
}

/// `agents --json` の `status` が「claude が動いている」を表す値。
/// [`foreground_state`] と [`row_state`] の裁定則が同じ綴りを見る
const STATUS_BUSY: &str = "busy";

/// state 値。**書く側（hook）と読む側（[`classify`]）が同じ綴りを見る**ための 1 箇所。
/// 語は [`Group`] の小文字（画面に出る語と内部の値を別にしない）
pub(crate) const WAITING: &str = "waiting";
pub(crate) const WORKING: &str = "working";
pub(crate) const COMPLETED: &str = "completed";

/// state 値（waiting|working|completed|stopped）+ 生死から表示を決める。
///
/// **未知の値は state として扱わない。** 生きているなら Working（まだ何も
/// 報告していないセッションは動いている可能性が高い）、死んでいるなら Stopped。
/// 以前は死んだ行に `Idle` という 5 番目の語を出していた
pub(crate) fn classify(live_state: &str, alive: bool) -> Group {
    match live_state {
        COMPLETED => Group::Completed,
        STOPPED => Group::Stopped,
        WAITING => Group::Waiting,
        // `working` と未知の値をまとめて扱う（どちらも「動いているらしい」）
        _ if alive => Group::Working,
        // プロセスが居ない ＝ 動いていないので、report された state に関わらず Stopped
        _ => Group::Stopped,
    }
}

/// その行を**今動かしている実行**の観測。窓 1 つが実行 1 つで、他インスタンスの
/// 実行は `agents --json` の status 経由で、撮影用の供給元は固定表で名乗る
/// （材料をどこから集めるかは描画側 ＝ [`crate::ui`] が持つ）
pub(crate) struct Run<'a> {
    /// その実行が hook で報告した最新の (state, 記録時刻)（一度も来ていなければ None）。
    /// 前回の実行の残骸を捨てる判断は [`crate::hooks::HookStates::get`] が
    /// 窓の起動時刻で済ませてあるので、ここへ来るのは今の実行が書いたものだけ
    pub(crate) hook: Option<(&'a str, u64)>,
    /// `agents --json` の `status`（hook が一度も来ていない行の従経路と、
    /// waiting の裁定（[`row_state`]）の材料。空 ＝ ポーラーがまだ拾っていない）
    pub(crate) status: &'a str,
    /// `status` を観測した時刻（ms）。hook の記録時刻との新旧裁定の材料。
    /// 0 ＝ 一度も観測していない（裁定は起きない）
    pub(crate) status_at: u64,
    /// PTY の出力から推した「動いているらしい」（`status` も無い間の最後の手段。
    /// フォーカスの出入りや再描画でも動くので、**裁定の材料にはしない**）
    pub(crate) busy: bool,
}

/// 1 行に出す状態を決める。**行に保存せず、そのつど導く。**
///
/// ```text
/// state(row) = 動かしている実行がある ? その実行が報告した最新 : Stopped
/// ```
///
/// この形から出る性質が 3 つあり、どれも**構造的に**成り立つ:
///
/// - **ccdesk の起動直後は窓が 1 つも無いので必ず全部 Stopped**（保存値が
///   「動いていた頃の state」を出し続けることが起こり得ない ＝ ccdesk が
///   異常終了しても次の起動で正しくなる）
/// - `stop` / `/clear` / `/resume` の**どれで止まっても同じ表示**（止まる ＝
///   その行を動かす実行が無くなる、の 1 通りしかない）
/// - **`Stopped` の行が Working の色・明滅を帯びるという矛盾が作れない**: `stopped` は
///   「実行が終わった」の言い換えなので、hook がそう言った実行は実行として扱わない
///   ＝ Stopped は必ず `classify(STOPPED, _)` の 1 分岐だけを通って `Group::Stopped` を
///   返し、色（[`Group::color`]）も明滅（[`Group::blinks`]）もその 1 つの値から導く
///
/// 実行があるときの中身は **hook が主、`agents --json` が従**:
/// hook は turn 単位で届くので
/// Working / Waiting / Completed を取り違えない。hook が一度も来ていない行
/// （ccdesk が起こしていないセッション・注入が効かなかった場合）だけ `status` へ落ち、
/// `status` も無い間は出力の変化から推す。
///
/// # 例外 ＝ waiting の裁定則
///
/// **hook の `waiting` だけは、より新しい `busy` 観測に負ける。**
/// `waiting` は「ユーザーが動くまで claude は進まない」という主張なので、
/// その後に観測された「動いている」はその主張の反証になる（許可プロンプトの
/// 許可のように**「解除された」を知らせる hook イベントが存在しない**操作が
/// あり、イベントの列挙では状態機械が閉じない。裁定則は原因が何であれ
/// 最大ポーリング 1 周期の遅れで自己修復する）。
///
/// waiting 以外は status で覆さない: `busy` は「このターンの続き」と「次のターン」を
/// 区別できないので、`completed` を覆すと Done の意味が壊れる（次のターンが
/// 始まれば `UserPromptSubmit` hook が即 `working` を書くので、覆す必要も無い）。
/// 逆向き（busy でない観測で `working` を waiting へ落とす）もしない:
/// 「動いていない」は idle_prompt の誤検知と同じ轍で、ターンを終えた行が
/// 時間経過で入力待ちへ落ちる
pub(crate) fn row_state(run: Option<Run<'_>>) -> Group {
    let Some(run) = run.filter(|run| run.hook.map(|(state, _)| state) != Some(STOPPED)) else {
        return classify(STOPPED, false);
    };
    match run.hook {
        Some((WAITING, at)) if run.status == STATUS_BUSY && run.status_at > at => {
            classify(WORKING, true)
        }
        Some((state, _)) => classify(state, true),
        None if run.status.is_empty() && run.busy => classify(WORKING, true),
        None if run.status.is_empty() => classify(WAITING, true),
        None => classify(foreground_state(run.status), true),
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
        Some((size, std::time::UNIX_EPOCH))
    }

    /// 期待値の組み立て
    fn logged_in(label: &str) -> AccountStatus {
        AccountStatus::LoggedIn(label.to_string())
    }

    /// email も組織名も取れない認証方式でも空行にはしない（未取得と見分けが付かない）
    #[test]
    fn falls_back_to_the_auth_method_when_there_is_no_name() {
        assert_eq!(
            parse_account(r#"{"loggedIn": true, "authMethod": "claude.ai"}"#, None),
            logged_in("claude.ai")
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
            logged_in("claude.ai")
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
            logged_in("Acme, Inc.")
        );
    }

    /// ログイン済みなのに空ラベルになると Unknown（未取得）と区別が付かない
    #[test]
    fn never_produces_an_empty_label() {
        assert_eq!(
            parse_account(r#"{"loggedIn": true, "authMethod": "claude.ai"}"#, None),
            logged_in("claude.ai")
        );
        assert_eq!(
            parse_account(r#"{"loggedIn": true}"#, None),
            logged_in("logged in")
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
            "a retry after a failed fetch waits for the whole 60s fallback"
        );

        // 成功した後は通常の周期に戻す（短周期で claude を叩き続けない）
        state.age = ACCOUNT_RETRY_SECS;
        assert_eq!(
            account_step(&mut state, &account, false, || None, || panic!(
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
        let new = logged_in("hanako");

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
            "a login that landed during the fetch is not picked up on the next cycle"
        );
    }

}
