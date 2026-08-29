//! バックグラウンド取得（前景セッションのライブ状態 / フッター）と状態分類。
//!
//! 使用率は [`crate::usage`] が取得から解釈まで一手に持つ（取得の作法をここと
//! 2 箇所に分けない）。
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ratatui::style::Color;

use std::collections::BTreeMap;

use ccdesk::LockExt;

use crate::backend::{AgentVersion, Kind};

// 生存記録（`~/.claude/sessions/<pid>.json`）の項目の綴りは文書化されていないので
// [`crate::claude_format`] が持つ（外れたときに直す場所を 1 つにするため）
use crate::claude_format::{
    AGENT_KIND, AGENT_KIND_INTERACTIVE, AGENT_PID, AGENT_SESSION_ID, AGENT_STATUS,
    AGENT_STATUS_BUSY, AGENT_STATUS_IDLE, AGENT_STATUS_SHELL, AGENT_STATUS_UPDATED_AT,
    AGENT_STATUS_WAITING, SESSIONS_DIR,
};
use crate::theme::ui;

/// 生きている前景セッション 1 つ（`~/.claude/sessions/<pid>.json` 1 ファイル）。
///
/// **読むのは前景セッションを名指しできる項目だけ。** `sessionId` が ccdesk の行
/// （[`crate::sessions::SessionRow`]）と同じ鍵で、`status` が生きた状態。
/// 項目の綴りと値の一覧は [`crate::claude_format`] が正本
#[derive(Clone, Default)]
pub(crate) struct AgentInfo {
    /// transcript の `sessionId`（＝ `claude --session-id` へ渡した UUID）
    pub(crate) session_id: String,
    /// `"interactive"` | `"background"` 等。前景セッションの行だけを突き合わせる
    pub(crate) kind: String,
    /// 前景セッションが書くライブ状態の生値（値の一覧と決定条件は
    /// [`crate::claude_format`]、語彙への翻訳は [`state_of_status`]）
    pub(crate) status: String,
    /// **claude が [`Self::status`] を書いた時刻**（ms）。0 ＝ 時刻が読めなかった。
    ///
    /// hook（イベント）とどちらが新しいかを見るのはこの値で、**ccdesk 自身が
    /// 観測した時刻ではない**（[`row_state`]）。観測時刻で代用すると、その値は
    /// 常に「今」なので status が hook に必ず勝ち、陳腐化した `busy` を新しい
    /// `idle` hook で降ろせなくなる
    pub(crate) status_at: u64,
}

impl AgentInfo {
    /// 前景（interactive）セッションか。bg エントリを行の状態の答えにしない
    pub(crate) fn is_interactive(&self) -> bool {
        self.kind == AGENT_KIND_INTERACTIVE
    }
}

/// 生きている前景セッションを 1 回読む（`~/.claude/sessions/`）。
/// None ＝ ディレクトリを列挙できなかった（＝ 呼び手は前回の観測を保つ）。
///
/// **ポーラーと `ccdesk doctor` が同じこの経路を通る**: doctor は
/// 「実際どう見えるか」を確かめる道具なので、本番と別の実装を持つと
/// こちらだけ解釈を変えたときに doctor が嘘の ok を出す
pub(crate) fn fetch_agents() -> Option<AgentSnapshot> {
    read_sessions(&ccdesk::claude_dir()?.join(SESSIONS_DIR))
}

/// 1 回ぶんの観測。**「読めた時刻」を値と一緒に運ぶ**のが要点で、時刻だけが
/// 別経路で進むと古い値に新しい時刻が付く（[`poll_agents_once`]）
#[derive(Clone, Default)]
pub(crate) struct AgentSnapshot {
    pub(crate) agents: Vec<AgentInfo>,
    /// このディレクトリを読み始めた時刻（ms）。**個々の `status` の新しさでは
    /// ない**（それは [`AgentInfo::status_at`]）。使い道は「ccdesk が止めた後に
    /// 見たか」の判定 1 つだけ（[`crate::app::App::agents_observed_at`]）
    pub(crate) observed_at: u64,
}

/// [`fetch_agents`] の本体。**ディレクトリを引数で受ける**ので、テストが実ユーザーの
/// `~/.claude` を読まずに済む（置き場は bin のテスト専用モジュール `crate::testutil`）
pub(crate) fn read_sessions(dir: &std::path::Path) -> Option<AgentSnapshot> {
    let observed_at = ccdesk::now_ms();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // **まだ 1 度も claude が起きていない ＝ セッションは 0**（読めなかった
        // のではないので、前回の観測を残すとそちらの方が嘘になる）
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Some(AgentSnapshot { agents: Vec::new(), observed_at });
        }
        Err(_) => return None,
    };
    let agents = entries
        .flatten()
        .filter_map(|entry| read_session_file(&entry.path()))
        .collect();
    Some(AgentSnapshot { agents, observed_at })
}

/// 1 ファイル → 1 エントリ。**読めない・死んでいるものは黙って落とす**
/// （1 つの壊れたファイルで観測全体を失わない）
fn read_session_file(path: &std::path::Path) -> Option<AgentInfo> {
    let value = ccdesk::read_json(path)?;
    let s = |k: &str| {
        value
            .get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let num = |k: &str| value.get(k).and_then(serde_json::Value::as_u64);
    // 桁が u32 に収まらない値は pid として使わない
    let pid = num(AGENT_PID).and_then(|pid| u32::try_from(pid).ok())?;
    // **死んだ pid の残骸を読まない。** これを落とすと、止めた行が残骸の
    // `busy` を読んで Working のまま残る（理由は [`ccdesk::process_alive`]）
    if !ccdesk::process_alive(pid) {
        return None;
    }
    Some(AgentInfo {
        session_id: s(AGENT_SESSION_ID),
        kind: s(AGENT_KIND),
        status: s(AGENT_STATUS),
        // 時刻の項目が無い版のために、ファイルの更新時刻へ落とす。0 のままに
        // すると hook が必ず勝ち、hook を取り逃した行を status で直せなくなる
        status_at: num(AGENT_STATUS_UPDATED_AT).unwrap_or_else(|| modified_ms(path)),
    })
}

/// ファイルの更新時刻（ms）。読めなければ 0
fn modified_ms(path: &std::path::Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

/// 取得 1 回ぶん。**値と観測時刻を 1 つの [`AgentSnapshot`] として入れ替える**のが
/// 要点で、失敗したときは丸ごと前回のまま残る。
///
/// 以前は時刻だけを別の atomic で運び、しかも取得の**前**に無条件で刻んでいた。
/// 取得が失敗すると**古い値に新しい時刻が付く**ので、陳腐化した `busy` が
/// 「たった今の観測」を名乗って hook に勝ち続ける（＝ 赤が固着して直らない）。
/// 2 つを 1 つの値へ束ねると、この食い違いが型として作れなくなる。
///
/// **取得そのものを引数で受ける**（`fetch_agents` を直接呼ばない）。
/// 失敗しても入れ替わらないことを、実ファイルを用意せずに確かめられるようにするため
fn poll_agents_once(
    shared: &Mutex<AgentSnapshot>,
    dirty: &std::sync::atomic::AtomicBool,
    fetch: impl FnOnce() -> Option<AgentSnapshot>,
) {
    if let Some(parsed) = fetch() {
        *shared.lock_recover() = parsed;
        dirty.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// 前景セッションの観測をバックグラウンドで回す。
///
/// **1 周期は ~1ms**（`~/.claude/sessions/` を読むだけ）。`claude agents --json` を
/// 起こしていた頃は 1 回 ~900ms かかっていた
pub(crate) fn spawn_agents_poller(
    shared: Arc<Mutex<AgentSnapshot>>,
    dirty: Arc<std::sync::atomic::AtomicBool>,
) {
    std::thread::spawn(move || loop {
        poll_agents_once(&shared, &dirty, fetch_agents);
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

/// agent ごとのアカウント・バージョン情報。アカウントはサイドバー下部の行、
/// バージョンは上部の版行に出る（`latest` は「更新がある」の有無だけを
/// 決め、新しい番号そのものは幅の都合で表示しない）。
///
/// **どこから取るかの正本は [`crate::backend::Backend`]**（`account` / `version`）。
/// ここにコマンド名や URL を書き写すと、変えたときに片方だけ古いままになる
#[derive(Clone, Default)]
pub(crate) struct FooterInfo {
    /// agent ごとのログイン状態 + 表示ラベル。**agent ごとに別のアカウント**
    pub(crate) accounts: BTreeMap<Kind, AccountStatus>,
    /// agent ごとの版（[`crate::backend::Kind::ORDER`] と同じ並び）。
    /// **kind ごとに 1 本ずつ**なので、agent を足すと版行も自動で増える
    pub(crate) versions: BTreeMap<Kind, AgentVersion>,
}

impl FooterInfo {
    /// その agent の版（まだ取れていなければ既定 ＝ 現行版が空）
    pub(crate) fn version(&self, kind: Kind) -> AgentVersion {
        self.versions.get(&kind).cloned().unwrap_or_default()
    }

    /// その agent のアカウント（まだ取れていなければ [`AccountStatus::Unknown`]）
    pub(crate) fn account(&self, kind: Kind) -> AccountStatus {
        self.accounts.get(&kind).cloned().unwrap_or_default()
    }
}

/// ccdesk 自身の版チェック（起動時 1 回の使い捨てスレッド）。
/// このビルドより新しいリリースタグがあれば共有状態へ書く（無ければ何も書かない
/// ＝版行は最新表示のまま）。値の形は claude 側の [`crate::backend::AgentVersion::latest`] と同じ
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
    // **PATH の解決は自前でやる**（Windows の `Command::new` は `.cmd` を
    // 見つけない。理由は [`ccdesk::resolve_program`]）
    let program = ccdesk::resolve_program(cmd)?;
    let o = std::process::Command::new(program)
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
pub(crate) type CredentialsFp = Option<(u64, std::time::SystemTime)>;

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
pub(crate) fn fetch_claude_account() -> AccountStatus {
    AccountFetcher::default().fetch()
}

/// claude の認証情報ファイルの指紋（[`AuthWatch`]）。**解決は 1 起動 1 回**
pub(crate) fn claude_auth_fingerprint() -> CredentialsFp {
    static WATCH: std::sync::OnceLock<AuthWatch> = std::sync::OnceLock::new();
    WATCH.get_or_init(AuthWatch::detect).fingerprint()
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

/// 取得できた版を共有側へ **kind ごとに** 取り込む。1 つでも書き換えたら true。
///
/// **丸ごと代入にしない。** 取得に失敗した agent は呼び手の filter で `fetched` から
/// 落ちているので、代入すると**落ちた側の項目まで消える** ＝ その版行が
/// 「版番号なし・更新マーカーなし」で次の周期（1 時間）まで固まる。
/// filter で防いだつもりだったことを、代入が元に戻していた
fn merge_versions(
    current: &mut BTreeMap<Kind, AgentVersion>,
    fetched: BTreeMap<Kind, AgentVersion>,
) -> bool {
    let mut changed = false;
    for (kind, version) in fetched {
        if current.get(&kind) != Some(&version) {
            current.insert(kind, version);
            changed = true;
        }
    }
    changed
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
    kinds: Vec<Kind>,
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
        // **出す agent ごとに 1 本ずつ**（アカウントは agent ごとに別物）。
        // 共有側へ最後に書いた値も手元に持つ ＝ 毎秒ロックを取らずに比べられる。
        // 一覧は [`Kind::enabled`] が絞ったもの ＝ **切った agent の実行ファイルは
        // 1 回も起こさない**（入れていない agent のアカウント取得は毎回失敗し、
        // [`ACCOUNT_RETRY_SECS`] ごとに起動を試み続けるため）
        let mut accounts: BTreeMap<Kind, (AccountPollState, AccountStatus)> = kinds
            .iter()
            .map(|kind| (*kind, (AccountPollState::new(), AccountStatus::default())))
            .collect();
        let mut version_age = u64::MAX / 2; // 初回は即取得
        loop {
            let forced = refresh.swap(false, std::sync::atomic::Ordering::Relaxed);
            let mut updated = false;

            for (kind, (state, shown)) in accounts.iter_mut() {
                let backend = kind.backend();
                let Some(next) = account_step(
                    state,
                    shown,
                    forced,
                    || backend.auth_fingerprint(),
                    || backend.account(),
                ) else {
                    continue;
                };
                *shown = next.clone();
                shared.lock_recover().accounts.insert(*kind, next);
                updated = true;
            }

            // **版行 2 本を同じゲートで取る。** 周期を 2 つ持つと、片方だけ
            // 「起動時 1 回」のような別の規則へ流れる（実際そうなっていた）
            if refetch_due(version_age, VERSION_INTERVAL_SECS, false, forced) {
                version_age = 0;
                // **出す agent ごとに 1 本ずつ取る。** 一覧はアカウントと同じ
                // [`Kind::enabled`] の結果なので、版行とアカウント行がずれない
                let fetched: BTreeMap<Kind, AgentVersion> = kinds
                    .iter()
                    .map(|kind| (*kind, kind.backend().version()))
                    // 取得に失敗した（current が空）ものは載せない。1 回の失敗で
                    // バージョン表記と更新ボタン行が 1 時間消えるのを防ぐ
                    .filter(|(_, v)| !v.current.is_empty())
                    .collect();
                if !fetched.is_empty() {
                    let mut guard = shared
                        .lock_recover();
                    updated |= merge_versions(&mut guard.versions, fetched);
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
            for (state, _) in accounts.values_mut() {
                state.tick();
            }
            version_age += 1;
        }
    });
}

/// グルーピング（切替の入口はサイドバーの ⊞ group 行のメニュー）。デフォルトは State 別
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Grouping {
    State,
    Directory,
    /// agent 種別で分ける（claude / codex）
    Agent,
}

impl Grouping {
    /// 表示順（メニューの項目の並びもこれに従う）
    pub(crate) const ORDER: [Self; 3] = [Self::State, Self::Directory, Self::Agent];

    /// **保存値（config.json）と画面表示の唯一の綴り**。
    /// 読み・書き・メニュー・現在値表示が別々に綴りを持つと、片方だけ変えたときに
    /// 保存値が読めなくなる（設定が黙って既定へ戻る）
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::State => "state",
            Self::Directory => "directory",
            Self::Agent => "agent",
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

/// **行の状態。これ 1 つが語彙の正本**で、3 つの顔を持つ:
///
/// - **保存と CLI 引数**（[`Self::as_str`] / [`Self::parse`]）: hook は
///   hook の引数として飛び（形は [`crate::hooks::HOOK_EVENTS`] が決める）、
///   `hook-states.json` に文字列で残る
/// - **画面**（[`Self::title`] / [`Self::color`] / [`Self::blinks`]）: 行のドット・
///   節の見出しが同じここを読む
/// - **並び**（[`Self::ORDER`] と [`Ord`]）: 上ほどユーザーがすることがある
///
/// **表示用の型を別に持たない。** かつては `State`（hook の語彙）と `State`（節）が
/// 別の enum で、写す関数が 1 本挟まっていた。値が 1 対 1 に対応していたので、
/// その関数は恒等写像 ＝ **同じ知識を 2 箇所で維持していた**だけだった。
/// 綴りの違い（保存は `working`、画面は `Working`）は同じ型の 2 つのメソッドで足りる。
///
/// **`Completed` は持たない。** 「ターンが終わった」は状態ではなくイベントで、
/// 終わった後のセッションはプロンプトで待機している ＝ [`Self::Idle`]。claude 自身も
/// `completed` にあたる status を持たない。値として分けていた頃の唯一の用途は
/// 使用率取得の合図（[`crate::hooks::HookStates::any_row_went_idle_since`]）だったが、
/// それは「状態の語彙」に置く理由にならなかった。
///
/// **`Ready for review` も持たない**: 判定材料（PR 番号）は bg の state.json
/// （非公開の内部形式）にしか無く、前景セッションは書かない。正本を
/// `sessions.json` 1 つにする代償として落とした
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum State {
    /// claude がユーザーの決定を待って止まっている（許可・確認ダイアログ）。
    ///
    /// **判定規則はこれ 1 行**: 「あなたが動かないと claude が進めない」。
    /// claude が動いておらず、あなたも待たれていないなら [`Self::Idle`]
    Waiting,
    /// claude が動いている
    Working,
    /// **この行にすることは無い。** プロンプトで待機・ターンを終えた直後・
    /// 起動しただけ・バックグラウンド作業が走っているだけ、が全部ここに入る
    Idle,
    /// プロセスが終了した（行は残る）。**[`Self::Idle`] と混ぜない**: 止まっているのと
    /// 手が空いているのは別の話で、開くときの動き（`claude -r` で起こし直すか）が違う
    Stopped,
}

impl State {
    /// 表示順（節の並び）＝ [`Ord`] と同じ順。
    /// **[`Self::parse`] もこれを舐める**ので、語を足したら綴りも並びも同時に効く
    pub(crate) const ORDER: [Self; 4] = [Self::Waiting, Self::Working, Self::Idle, Self::Stopped];

    /// 保存と CLI 引数の綴り。**[`Self::parse`] と対で、綴りを知るのはここだけ**
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Working => "working",
            Self::Idle => "idle",
            Self::Stopped => "stopped",
        }
    }

    /// 外から来た綴り（保管ファイル・hook の CLI 引数）。**知らない語は `None`**。
    /// 呼び手は「読めなかった」を自分の文脈で decide する
    /// （保管なら項目を捨てる、hook なら何も記録しない）
    pub(crate) fn parse(text: &str) -> Option<Self> {
        Self::ORDER.into_iter().find(|state| state.as_str() == text)
    }

    /// **この 4 語が画面に出る唯一の綴り**（行ラベル・節の見出し）。
    ///
    /// **小文字なのは、出る箇所を 1 つの綴りで賄うため。** かつては大文字始まりで
    /// 持ち、一部だけが `to_lowercase` を掛けていた ＝ 同じ語が 2 つの姿を持っていた。
    /// ピン留めの節（[`crate::ui`] の `PINNED_TITLE`）が元から小文字なので、
    /// 揃える先は小文字側になる。
    ///
    /// **綴りの実体は [`Self::as_str`]**（[`crate::backend::Kind::title`] と同じ作り）。
    /// 小文字になった今、保存の綴りと画面の綴りは同じ語なので、表を 2 つ持つと
    /// 片方だけ直したときに黙ってずれる。[`Self::parse`] も `as_str` を舐めるので、
    /// 語を変えれば画面・保存・読み戻しが同時に動く
    pub(crate) fn title(self) -> &'static str {
        self.as_str()
    }

    /// [`Self::title`] を縦に揃えるための桁。**サイドバーの行末がこれで揃う**
    /// （状態語の右端が揃っていないと、行ごとにメニュー記号の手前がガタつく）。
    /// 値は綴りから導くのではなく固定し、`every_title_fits_its_column` が
    /// 全状態の収まりを見る
    pub(crate) const TITLE_COLS: usize = 7;

    /// この状態の色。**行のドット・行末の状態語・節が同じこの 1 箇所を読む**
    /// ので、状態を増やしたときに色の対応を書き忘れる場所が増えない
    /// （以前は `StateView.color` が別に持っていて、食い違う値を作れてしまっていた）
    pub(crate) fn color(self) -> Color {
        match self {
            Self::Waiting => ui().attention,
            // 明滅する行のドットと語は [`crate::theme::UiTheme::blink`] のコマを引く。
            // ここが返すのはコマ列の一番明るい側 ＝ 節の見出しが代表色として使う
            Self::Working => ui().working,
            Self::Idle => ui().ok,
            Self::Stopped => ui().dim,
        }
    }

    /// ドットが明滅するか。**動いている状態だけ**
    pub(crate) fn blinks(self) -> bool {
        self == Self::Working
    }
}

/// `~/.claude/sessions/` の `status`（claude 自身が書く**現在値**）を [`State`] へ写す。
/// `None` ＝ **まだ観測できていない**（空文字。値が載らないセッションも実在するので、
/// 呼び手はこの経路を必ず持つ）。
///
/// **翻訳はここ 1 箇所**。claude 側の綴りと決定条件は [`crate::claude_format`] が持ち、
/// ここから先は hook が書く値と同じ型になる ＝ [`row_state`] は「どちらが新しいか」
/// だけを見ればよくなる（両者を別語彙のまま突き合わせていた頃は、食い違うたびに
/// 特例を 1 本足す形になっていた）。
///
/// 未知の値は Working へ倒す: 知らない状態を「入力待ち」と名乗って呼び出しを促すより、
/// 「動いているらしい」の方が害が小さい
pub(crate) fn state_of_status(status: &str) -> Option<State> {
    match status {
        "" => None,
        AGENT_STATUS_BUSY => Some(State::Working),
        AGENT_STATUS_WAITING => Some(State::Waiting),
        // **shell を idle と同じに写すのは意図した判断**（理由は
        // [`AGENT_STATUS_SHELL`]）。ここを変えるだけで独立した状態にできる
        AGENT_STATUS_IDLE | AGENT_STATUS_SHELL => Some(State::Idle),
        _ => Some(State::Working),
    }
}

/// その行を**今動かしている実行**の観測。窓 1 つが実行 1 つで、他インスタンスの
/// 実行は `~/.claude/sessions/` の status 経由で、撮影用の供給元は固定表で名乗る
/// （材料をどこから集めるかは描画側 ＝ [`crate::ui`] が持つ）
pub(crate) struct Run {
    /// その実行が hook で報告した最新の 1 件（一度も来ていなければ None）。
    /// 前回の実行の残骸を捨てる判断は [`crate::hooks::HookStates::get`] が
    /// 窓の起動時刻で済ませてあるので、ここへ来るのは今の実行が書いたものだけ
    pub(crate) hook: Option<crate::hooks::Reported>,
    /// **agent 自身が名乗っている現在値と、それが書かれた時刻**（None ＝ まだ
    /// 観測できていない）。
    ///
    /// **出どころは agent ごとに違うが、ここへ来る形は 1 つ。** claude は
    /// `~/.claude/sessions/` の `status`（[`state_of_status`]）、codex は rollout の
    /// 末尾（[`crate::backend::Backend::record_states`]）。**どちらを読むかは
    /// 呼び手が決めない**: 両方を引いて新しい方を採れば、agent ごとの分岐が要らない
    /// （行は片方しか持たないので、実際には必ず一方が None になる）。
    ///
    /// **hook との違いは自己修復するかどうか。** hook はイベントなので取りこぼすと
    /// 誰も直せないが、こちらは現在値なので次の観測で必ず正しくなる
    pub(crate) observed: Option<(State, u64)>,
    /// PTY の出力の様子。**hook も現在値も無い行だけの最後の手段**
    /// （フォーカスの出入りや再描画でも動くので精度は低い）。
    /// `None` ＝ この行に窓が無い（材料は必ず他にあるので、この値は読まれない）
    pub(crate) pty: Option<PtyHint>,
}

/// 窓の PTY から見た様子。**2 値では足りない**のが要点で、「まだ 1 バイトも
/// 出していない」と「出したがいま静か」は同じ無出力でも意味が正反対になる:
/// 前者は起動中（＝動いている）、後者はプロンプトで待機（＝手が要らない）。
///
/// 1 つの真偽値で持っていた頃、材料が何も無い行はこの 2 つを区別できず、
/// **起動中の数秒を「もう終わった」と描いていた**（`claude` の起動は認証待ちや
/// ウイルススキャンで秒単位かかることがある。打鍵の門番が最大 10 秒待つのと同じ理由）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PtyHint {
    /// 子がまだ端末を掴んでいない ＝ 起動の途中
    Starting,
    /// 直近に出力があった
    Writing,
    /// 出力したことはあるが、いまは静か
    Quiet,
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
/// - **`Stopped` の行が Working の色・明滅を帯びるという矛盾が作れない**:
///   [`State::Stopped`] は「実行が終わった」の言い換えなので、hook がそう言った実行は
///   実行として扱わない ＝ Stopped は必ず下の早期 return 1 本だけを通り、
///   色（[`State::color`]）も明滅（[`State::blinks`]）もその 1 つの値から導く
///
/// 実行があるときの中身は **hook（イベント）と現在値（[`Run::observed`]）の
/// 新しい方**。2 つは同じ語彙（[`State`]）へ揃えてあるので、判断は新旧の比較だけで
/// 足りる。どちらも無い間だけ PTY の出力変化から推す。
///
/// # なぜ「新しい方」の 1 本だけで足りるのか
///
/// hook は**イベント**（「その瞬間こうなった」）なので、取りこぼすと自己修復しない:
/// claude は Esc 中断のとき `Stop` を撃たない（実データで中断ターンの 91%（113/124）が
/// 未発火）し、許可プロンプトの**許可には「解除された」を知らせるイベントが存在しない**。
/// 対して現在値は agent 自身が遷移のたびに上書きするので、次の観測が来れば必ず
/// 正しくなる。
///
/// **以前はこの 2 つを別々の語彙のまま突き合わせていた**（hook は
/// `working|waiting|completed`、`status` は `busy|waiting|shell|idle`）。
/// 突き合わせようがないので食い違うたびに特例を 1 本足すことになり、双方向 2 本
/// （`waiting` は新しい `busy` に負ける／`working` は新しい非 `busy` に負ける）まで
/// 増えていた。しかも `shell`（idle だがバックグラウンド bash が走っている）が
/// 「非 `busy`」に含まれるため、**バックグラウンド実行中の行が入力待ちへ落ちる**という
/// 副作用が付いていた。語彙を揃えると特例は 0 本になり、残るのは新旧の比較だけ。
///
/// # かつてここに居た codex 専用の救済 2 本
///
/// codex は現在値を持たないものとして扱われていたので、hook を取り逃した行を
/// 直す材料を代わりに 2 つ持っていた: **PTY の無音**で Working を降ろし、
/// **記録が伸びたこと**で Waiting を降ろす。どちらも「状態を言わない材料から
/// 状態を推す」形で、前者は codex の TUI が 1 秒ごとに書き続けるせいで
/// **一度も発火しなかった**（Esc 中断が永久に赤のまま ＝ 報告された症状）。
///
/// 今は codex も現在値を持つ（rollout の
/// [`crate::backend::Backend::record_states`]）ので、両方とも消えた。
/// **状態は状態を名乗る材料からだけ導く。**
///
/// 遅れは構造的に有界: hook は 0 遅延で入るので押した瞬間に色が変わり、hook を
/// 取り逃しても最大 1 周期で観測が追い越す
pub(crate) fn row_state(run: Option<Run>) -> State {
    let Some(run) = run.filter(|run| run.hook.map(|hook| hook.state) != Some(State::Stopped))
    else {
        return State::Stopped;
    };
    // 同時刻なら hook を採る（hook はそのセッション自身が turn の境目で書くので、
    // 後から拾い直す観測よりも出所が確か）
    let newest = match (run.hook, run.observed) {
        (Some(hook), Some(observed @ (_, seen))) if seen > hook.at => Some(observed),
        (Some(hook), _) => Some((hook.state, hook.at)),
        (None, observed) => observed,
    };
    match newest {
        Some((state, _)) => state,
        // 材料が 1 つも無い行 ＝ **自分の窓を起こした直後**（他インスタンスの行は
        // 現在値を、撮影用の行は hook を必ず持つ）。だから「まだ何も出していない」は
        // 起動中であって、終わったわけではない
        None => match run.pty {
            Some(PtyHint::Starting | PtyHint::Writing) => State::Working,
            // 窓が無い行はここへ来ない（来たとしても、知らないことを「入力待ち」と
            // 名乗ってユーザーを呼びつけるよりは静かな方が害が小さい）
            Some(PtyHint::Quiet) | None => State::Idle,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::testutil::TempDir;

    /// **取得に失敗した周回は、値も観測時刻も動かさない。**
    ///
    /// これが赤の固着の正体だった: かつて観測時刻は値と別の atomic で運ばれ、
    /// しかも取得の**前**に無条件で刻まれていた。取得が失敗すると古い値に新しい
    /// 時刻が付き、陳腐化した `busy` が「たった今の観測」を名乗って hook に勝ち
    /// 続ける ＝ 新しい `idle` hook では二度と降ろせない。
    ///
    /// 今は 1 つの [`AgentSnapshot`] を丸ごと入れ替える形なので食い違いを作れない。
    /// **時刻の経路をもう一度分けたらここが落ちる**のがこのテストの役目
    #[test]
    fn a_failed_read_moves_neither_the_value_nor_its_timestamp() {
        let shared = Mutex::new(AgentSnapshot {
            agents: vec![AgentInfo {
                session_id: "kept".to_string(),
                status: AGENT_STATUS_BUSY.to_string(),
                ..AgentInfo::default()
            }],
            observed_at: 1_000,
        });
        let dirty = std::sync::atomic::AtomicBool::new(false);

        poll_agents_once(&shared, &dirty, || None);

        let after = shared.lock_recover();
        assert_eq!(after.observed_at, 1_000, "a failed read advanced the clock");
        assert_eq!(after.agents.len(), 1, "a failed read dropped the last value");
        assert!(
            !dirty.load(std::sync::atomic::Ordering::Relaxed),
            "a failed read asked for a redraw"
        );
    }

    /// 1 ファイル置く（`pid` は呼び手が決める ＝ 生存判定を効かせられる）
    fn write_session(dir: &TempDir, name: &str, body: serde_json::Value) {
        std::fs::write(dir.join(name), body.to_string()).expect("could not write a session file");
    }

    /// **status を書いた時刻は claude が書いた値を使う**（ccdesk の観測時刻ではない）。
    /// ここを観測時刻に戻すと、値は常に「今」になって hook に必ず勝つ
    #[test]
    fn the_status_carries_the_time_claude_wrote_it() {
        let dir = TempDir::new("poll", "status-time");
        write_session(
            &dir,
            "1.json",
            serde_json::json!({
                AGENT_PID: std::process::id(),
                AGENT_SESSION_ID: "s1",
                AGENT_KIND: AGENT_KIND_INTERACTIVE,
                AGENT_STATUS: AGENT_STATUS_BUSY,
                AGENT_STATUS_UPDATED_AT: 1_700_000_000_123u64,
            }),
        );
        let snapshot = read_sessions(dir.path()).expect("the directory could not be listed");
        let agent = snapshot.agents.first().expect("no session was read");
        assert_eq!(agent.session_id, "s1");
        assert_eq!(agent.status, AGENT_STATUS_BUSY);
        assert_eq!(agent.status_at, 1_700_000_000_123);
    }

    /// 時刻の項目が無い版のために、ファイルの更新時刻へ落とす。
    /// 0 のままにすると hook が必ず勝ち、hook を取り逃した行を status で直せない
    #[test]
    fn a_status_without_a_time_falls_back_to_the_files_own() {
        let dir = TempDir::new("poll", "status-time-fallback");
        write_session(
            &dir,
            "1.json",
            serde_json::json!({
                AGENT_PID: std::process::id(),
                AGENT_SESSION_ID: "s1",
                AGENT_KIND: AGENT_KIND_INTERACTIVE,
                AGENT_STATUS: AGENT_STATUS_IDLE,
            }),
        );
        let snapshot = read_sessions(dir.path()).expect("the directory could not be listed");
        let agent = snapshot.agents.first().expect("no session was read");
        assert!(
            agent.status_at > 0,
            "a status with no timestamp was left at 0, so the hook can never lose to it"
        );
    }

    /// **死んだ pid の残骸を読まない。** 読むと、止めた行が残骸の `busy` を
    /// 拾って Working のまま残る（Stopped にならない）
    #[test]
    fn the_leftovers_of_a_dead_process_are_not_read() {
        let dir = TempDir::new("poll", "dead-pid");
        write_session(
            &dir,
            "1.json",
            serde_json::json!({
                AGENT_PID: std::process::id(),
                AGENT_SESSION_ID: "alive",
                AGENT_KIND: AGENT_KIND_INTERACTIVE,
                AGENT_STATUS: AGENT_STATUS_IDLE,
            }),
        );
        write_session(
            &dir,
            "2.json",
            serde_json::json!({
                AGENT_PID: u32::MAX,
                AGENT_SESSION_ID: "dead",
                AGENT_KIND: AGENT_KIND_INTERACTIVE,
                AGENT_STATUS: AGENT_STATUS_BUSY,
            }),
        );
        let snapshot = read_sessions(dir.path()).expect("the directory could not be listed");
        let read: Vec<&str> = snapshot
            .agents
            .iter()
            .map(|a| a.session_id.as_str())
            .collect();
        assert_eq!(read, ["alive"]);
    }

    /// **1 つの壊れたファイルで観測を丸ごと失わない**（読めないものだけ落とす）
    #[test]
    fn a_file_that_cannot_be_read_only_costs_its_own_entry() {
        let dir = TempDir::new("poll", "broken-file");
        std::fs::write(dir.join("1.json"), b"{ not json").expect("could not write");
        write_session(
            &dir,
            "2.json",
            serde_json::json!({
                AGENT_PID: std::process::id(),
                AGENT_SESSION_ID: "ok",
                AGENT_KIND: AGENT_KIND_INTERACTIVE,
            }),
        );
        let snapshot = read_sessions(dir.path()).expect("one broken file lost the whole read");
        assert_eq!(snapshot.agents.len(), 1);
    }

    /// **まだ 1 度も claude が起きていない ＝ セッションは 0**。
    /// ここを「読めなかった」（None）にすると、前回の観測が残り続ける
    #[test]
    fn a_directory_that_does_not_exist_means_no_sessions() {
        let dir = TempDir::new("poll", "missing-dir");
        let snapshot =
            read_sessions(&dir.join("not-here")).expect("a missing directory read as a failure");
        assert!(snapshot.agents.is_empty());
        assert!(snapshot.observed_at > 0, "the read was not stamped");
    }

    /// **綴りそのものを固定する。** これは保存（`hook-states.json`）と hook の
    /// CLI 引数のワイヤ形式で、**変えると 2 つが同時に壊れる**: 前の版が書いた
    /// 保管が読めなくなり、更新前に起こした claude が呼ぶコマンドも通らなくなる。
    ///
    /// **往復（`parse(as_str())`）だけでは守れない**: [`State::parse`] は
    /// [`State::as_str`] を舐めて実装してあるので、綴りを丸ごと変えても往復は成立する。
    /// だからここは字面を直接置く（[`State::ORDER`] への載せ忘れは網羅の検査で落ちる）
    #[test]
    fn the_stored_spelling_of_every_state_is_fixed() {
        let spelled: Vec<&str> = State::ORDER.iter().map(|state| state.as_str()).collect();
        assert_eq!(spelled, ["waiting", "working", "idle", "stopped"]);
        // 綴りは claude 側の値とも重なる（[`state_of_status`] が同じ語へ写す）
        for state in State::ORDER {
            assert_eq!(State::parse(state.as_str()), Some(state), "{state:?}");
        }
        // 語彙に無い綴りは受けない（呼び手が「読めなかった」を自分で decide する）
        // `completed` は語彙から外した語（旧版の保管に残る）
        for unknown in ["", "blocked", "done", "completed", "Working", "idle "] {
            assert_eq!(State::parse(unknown), None, "{unknown:?}");
        }
    }

    /// 状態語は**サイドバーの行末で縦に揃える**ので、桁に収まらないと
    /// 右隣のメニュー記号の手前が行ごとにガタつく
    #[test]
    fn every_title_fits_its_column() {
        let widths: Vec<usize> = State::ORDER
            .iter()
            .map(|state| unicode_width::UnicodeWidthStr::width(state.title()))
            .collect();
        assert!(
            widths.iter().all(|w| *w <= State::TITLE_COLS),
            "a title does not fit in {} columns: {widths:?}",
            State::TITLE_COLS
        );
        // **桁を余らせない**（一番長い語が幅そのもの）＝ 語を短くしたら定数も下がる
        assert_eq!(
            widths.iter().copied().max(),
            Some(State::TITLE_COLS),
            "the column is wider than the longest state word"
        );
    }

    /// claude の `status` の写し先。**`shell` を `idle` と同じに畳むのは意図した判断**で、
    /// ここが黄（Waiting）へ倒れていた頃はバックグラウンド実行中の行が
    /// 「Needs input」を名乗ってユーザーを呼びつけていた
    #[test]
    fn a_live_status_maps_to_the_shared_vocabulary() {
        assert_eq!(state_of_status("busy"), Some(State::Working));
        assert_eq!(state_of_status("waiting"), Some(State::Waiting));
        assert_eq!(state_of_status("idle"), Some(State::Idle));
        assert_eq!(state_of_status("shell"), Some(State::Idle));
        // 空 ＝ まだ観測していない（値を載せないセッションも実在する）
        assert_eq!(state_of_status(""), None);
        // 知らない値は「動いているらしい」へ倒す（呼びつけるよりは害が小さい）
        assert_eq!(state_of_status("something-new"), Some(State::Working));
    }

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

    /// **1 周で取れなかった agent の版行を消さない。** 版の取得は agent ごとに
    /// 独立して失敗しうる（`--version` が一時的に転ける・npm のシムが差し替え中）。
    /// 呼び手は失敗した分を落として渡すので、取り込み側が丸ごと代入すると
    /// 落ちた側の版番号と更新マーカーが次の周期（1 時間）まで消える
    #[test]
    fn a_version_that_failed_this_round_keeps_the_one_already_shown() {
        let v = |current: &str, latest: Option<&str>| AgentVersion {
            current: current.to_string(),
            latest: latest.map(str::to_string),
        };
        let mut shown: BTreeMap<Kind, AgentVersion> = [
            (Kind::Claude, v("2.1.237", None)),
            (Kind::Codex, v("0.148.0", Some("0.149.0"))),
        ]
        .into();
        // claude だけ取れた周（codex は current が空だったので呼び手が落とした）
        let changed = merge_versions(&mut shown, [(Kind::Claude, v("2.1.238", None))].into());
        assert!(changed);
        assert_eq!(shown.get(&Kind::Claude), Some(&v("2.1.238", None)));
        assert_eq!(
            shown.get(&Kind::Codex),
            Some(&v("0.148.0", Some("0.149.0"))),
            "the agent that failed this round lost the version row it already had"
        );
        // 同じ値なら書き換えたと言わない（描き直しの合図を無駄に立てない）
        assert!(!merge_versions(&mut shown, [(Kind::Claude, v("2.1.238", None))].into()));
        // 空の周でも消さない
        assert!(!merge_versions(&mut shown, BTreeMap::new()));
        assert_eq!(shown.len(), 2);
    }
}
