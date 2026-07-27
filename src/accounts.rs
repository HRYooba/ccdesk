//! 複数 Claude アカウントの保管と切り替え（ドメイン層。UI は含まない）。
//!
//! 切替は `~/.claude/.credentials.json` をグローバルに書き換える方式。Windows の
//! claude はこのファイルを読み直すため、**稼働中セッションも次のメッセージから
//! 新しいアカウントになる**（実測）。プロファイル毎の環境変数を撒く方式ではない。
//!
//! ここはパスを全て引数（[`Paths`]）で受ける。理由は 2 つ:
//! - テストが実ユーザーの `~/.claude` / `~/.ccdesk` を絶対に触らないため
//! - 「ファイルがどこにあるか」の知識を [`Paths::detect`] 1 箇所に閉じるため

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::anyhow;
use serde_json::{json, Value};

// **共有ファイルへの安全な書き込みは lib 側の 1 実装を使う**（advisory lock と
// tmp → rename）。ここが持つのは「どのファイルをどのロックで守り、どれだけ待つか」
// という保管ストア固有の判断だけで、書き方そのものは持たない
use ccdesk::{lock_path_for, write_json_atomically, Lock, LOCK_STALE};

/// 認証情報ファイルのうち **アカウントに属する** キー。入れ替えるのはここだけ。
///
/// **ファイル全体を差し替えてはいけない。** 同じファイルには `mcpOAuth`
/// （MCP サーバーの OAuth ログイン＝アカウントに依存しない状態）が同居しており
/// （実測: トップレベルは `mcpOAuth` と `claudeAiOauth` の 2 キー）、丸ごと置くと
/// MCP の認証が壊れる。未知のトップレベルキーも同じ理由で保つ必要があるので、
/// 保管した値から新しいファイルを組み立てるのではなく、**現行ファイルを土台に
/// 1 キーだけ上書きする**（将来 claude がキーを増やしても壊れない形）
const OAUTH_KEY: &str = "claudeAiOauth";

/// 保管ファイルのトップレベルキー（`{"accounts": {"<email>": {…}}}`）
const ACCOUNTS_KEY: &str = "accounts";
/// 保管 1 件の表示ラベル
const LABEL_KEY: &str = "label";
/// 保管 1 件の認証情報（`claudeAiOauth` の中身そのまま）
const CREDENTIALS_KEY: &str = "credentials";
/// `claudeAiOauth` の中で **これが無い写しは使えない**（[`usable_oauth`]）
const REFRESH_TOKEN_KEY: &str = "refreshToken";

/// アカウントの同一性（email）と表示（label）の対。
///
/// **キーは email。表示ラベルは使えない。** ラベルは組織名の抑制ロジック
/// （`poll::is_personal_org`）で変わるため、ラベルで同一性を判定すると
/// 表示ロジックと同一性判定に同じ知識が二重化する
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Account {
    /// `claude auth status --json` の email。email を返さない認証方式では空になり、
    /// その場合は保管できない（安定した識別子が無い）
    pub(crate) email: String,
    /// 表示用ラベル（"alice" または "alice · Acme, Inc."）
    pub(crate) label: String,
}

impl Account {
    pub(crate) fn new(email: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            email: email.into(),
            label: label.into(),
        }
    }
}

/// 認証情報ファイルが書き換わっていないかを見るための印（mtime とサイズ）。
/// 無い・読めないときは `None`（「消えた」も変化として検出できる）。
///
/// **内容のハッシュではない。** 見たいのは「ある時点で観測した状態から動いたか」
/// だけで、ポーラーが再取得の契機に使う signal（[`crate::poll`]）と同じ材料。
/// 同じ知識を 2 箇所に持たないよう、認証情報ファイルを扱うこのモジュールに置く。
///
/// **見分けられない書き換え**: 同じ時刻刻み（Windows のシステムクロックは ~15.6ms
/// 更新）の中で同サイズに書き換わった場合。認証情報の書き換えは claude の
/// トークン更新（ネットワーク往復を挟む）と `/login` しか無く、2 回が 15ms に
/// 収まることは実運用では起きない。内容ハッシュにすれば閉じるが、
/// 毎秒の signal を stat から読み込みへ変える対価に見合わないと判断した
pub(crate) type CredentialsFp = Option<(std::time::SystemTime, u64)>;

/// 認証情報ファイルの指紋。無い・読めないときは None
pub(crate) fn credentials_fingerprint(path: &Path) -> CredentialsFp {
    let md = std::fs::metadata(path).ok()?;
    Some((md.modified().ok()?, md.len()))
}

/// 「今 `.credentials.json` の持ち主はこのアカウント」という **いつの観測か付きの**
/// 判断。
///
/// **email とラベルだけでは足りない。** 持ち主の判定材料は
/// `claude auth status --json`（子プロセス。数百 ms かかる）か過去の切替結果なので、
/// **判定した瞬間と、それを使って書き込む瞬間がずれる**。ずれている間に認証情報が
/// 差し替わっていると（別端末での `/login`・claude のトークンローテーション・
/// ccdesk 自身の直前の切替）、**別アカウントのトークンをこの email の保管へ
/// 書き込む**。refreshToken は使い捨てなので、それは復旧不能な破壊になる
/// （保管された側は元のトークンを二度と得られない）。
///
/// そこで「何を見てそう判断したか」（[`Self::seen`]）を同一性と対で持ち、ドメイン側は
/// **ロックを取った後に読み直して一致を確かめてから**しか巻き取らない。
/// 対にして 1 つの値にしてあるのは、片方だけ更新される形を作らないため
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ActiveAccount {
    /// 持ち主だと判断したアカウント
    pub(crate) account: Account,
    /// その判断の材料にした認証情報ファイルの指紋
    pub(crate) seen: CredentialsFp,
}

impl ActiveAccount {
    pub(crate) fn new(account: Account, seen: CredentialsFp) -> Self {
        Self { account, seen }
    }

    /// 指紋を持たない観測。**撮影用の固定データと、照合に関心が無いテスト**のため。
    /// ドメイン側の照合では「認証情報ファイルが無い状態を見た」と同じ扱いになるので、
    /// 実ファイルがある環境では必ず不一致になり巻き取りは起きない
    /// （＝実データを黙って壊す方向へは倒れない）
    pub(crate) fn unseen(account: Account) -> Self {
        Self::new(account, None)
    }
}

/// 切替が現行の認証情報を上書きする直前の持ち主について、**呼び手が観測できたこと**。
///
/// **`Option<&ActiveAccount>` では足りなかった。** `None` が
///
/// - 「観測できていて、巻き取る対象が無い」（未ログイン・email を返さない認証方式）
/// - 「まだ観測できていない」（起動直後の ~350ms・`claude auth status` の失敗が続く間）
///
/// の**両方**を意味していたため、後者でも切替が通って `.credentials.json` を
/// 上書きしていた。登録済みアカウントが登録後にローテートした refreshToken
/// （使い捨て）を巻き取れないまま失う ＝ そのアカウントは復旧不能になる。
/// [`ActiveAccount`] と `seen` で防いだはずの破壊が、`None` 経路で素通りしていた。
///
/// **この型では「観測できていない」を表せない**のが要点で、呼び手は
/// 観測できるまで [`AccountStore::switch_to`] を呼べない
/// （[`crate::poll::AccountStatus::Unknown`] からこの値は作れない）。
/// 3 状態の判別は表示側の `AccountStatus` が持ち、そこからの変換は
/// [`crate::app`] の 1 箇所だけが行う
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outgoing {
    /// 現行の認証情報の持ち主はこのアカウントだと観測できている。
    /// email が空（保管のキーを持たない認証方式）なら巻き取れないが、
    /// **持ち主が誰かは言えている**ので切替は通す
    Known(ActiveAccount),
    /// 誰もログインしていないと観測できている ＝ 巻き取る対象が無い
    NobodyLoggedIn,
}

/// 保管への変更が「今の持ち主」に何をしたか。
///
/// **「何もしなかった」を成功と区別するために enum で返す。** 区別しないと、
/// 判断材料が古くて no-op になった場合と本当に切り替えた場合が呼び出し側で同じ形になり、
/// UI は「切替に成功した」と表示してしまう（実際に起きていた: 切替直後に元の
/// アカウントへ戻す操作が黙って無反応になり、現行は切替先のまま残る）
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AccountChange {
    /// 現行の認証情報を差し替えた。新しい持ち主はこの観測。
    /// **ccdesk 自身が書いた値なので、ポーラーの追いつきを待つ必要が無い**
    Switched(ActiveAccount),
    /// 切替を求められたが既にそのアカウントだった（現行トークンには触っていない）
    AlreadyActive,
    /// 保管一覧だけを変えた（登録・登録解除）。[`AccountStore::switch_to`] は返さない
    StoreOnly,
}

/// 依存するファイルの位置。既定値の解決は [`Paths::detect`] だけが持つ
#[derive(Clone, Debug)]
pub(crate) struct Paths {
    /// 保管ファイル（`~/.ccdesk/accounts.json`）。**トークンを含む**
    pub(crate) store: PathBuf,
    /// 現行の認証情報（`~/.claude/.credentials.json`）
    pub(crate) credentials: PathBuf,
    /// claude と共有する advisory lock の実体ディレクトリ（`~/.claude.lock`）
    pub(crate) lock: PathBuf,
    /// 使用率キャッシュ（`~/.ccdesk/usage.json`）。切替時に消す
    pub(crate) usage_cache: PathBuf,
}

impl Paths {
    /// 既定パス。claude 側は `CLAUDE_CONFIG_DIR` に追従する（[`ccdesk::claude_dir`]）
    pub(crate) fn detect() -> Option<Self> {
        let claude = ccdesk::claude_dir()?;
        Some(Self {
            store: ccdesk::accounts_store_path()?,
            credentials: claude.join(".credentials.json"),
            lock: lock_path_for(&claude),
            usage_cache: ccdesk::usage_cache_path()?,
        })
    }

    /// 保管ファイル用の advisory lock（`~/.ccdesk/accounts.json.lock`）。
    ///
    /// **claude のロックを借りない。** `~/.claude.lock` が守る対象は認証情報ファイルで、
    /// 保管ファイルの read-modify-write をそれで直列化するのは意味論がズレるうえ、
    /// ccdesk 同士の競合が claude のトークン更新を待たせることになる。
    /// 導出は claude と同じ [`lock_path_for`]（ロック名の規則を 2 通り持たない）
    fn store_lock(&self) -> PathBuf {
        lock_path_for(&self.store)
    }
}

/// ロック取得の既定待ち時間。claude 側の保持はトークンエンドポイント 1 往復ぶんなので
/// これで足りる。**無限には待たない**（取れなければ失敗を返し、壊れた状態を残さない）
const LOCK_WAIT: Duration = Duration::from_secs(9);
/// 保管ファイルの read-modify-write を直列化するロックの待ち時間。
///
/// **プロセス内 Mutex では足りない。** ccdesk は複数起動でき保管ファイルは共有なので、
/// 「インスタンス 1 の登録解除」と「インスタンス 2 の追従更新」が重なると後着が
/// 前着を無かったことにする（外したアカウントが復活する / 新しい refreshToken が
/// 落ちて保管が死んだ値へ巻き戻る）。ロックの実体はディレクトリなので同一プロセス内でも
/// 排他になり、これ 1 つで両方の直列化が足りる。
///
/// 守る区間は小さなファイル 1 本の読み書きだけ（ネットワークも子プロセスも無い）なので、
/// claude と共有するロックの待ち（[`LOCK_WAIT`]）より短くてよい
const STORE_LOCK_WAIT: Duration = Duration::from_secs(2);

/// 持ち主の再判定（[`AccountStore::confirm`]）を試す回数の上限。
///
/// **1 回では足りない**: 判定は子プロセス 1 つぶん（実測 ~370ms）かかり、その間に
/// 動いている claude がトークンを更新するとファイルが動く。動いた瞬間に諦めると、
/// セッションを複数抱えた環境では押すたびに同じ失敗になる（リトライが無かった）。
///
/// **無制限にもできない**: この再判定は claude と共有する `~/.claude.lock` を
/// **保持したまま**回る。ccdesk は保持中にロックの mtime を touch しないので
/// （[`ccdesk::Lock`] の判断）、保持が [`LOCK_STALE`] を超えると claude 側が
/// 死んだ保持者のものとして奪い、守っていたはずの区間で認証情報の書き換えが
/// 始まる。1 回 ~400ms なのでここは閾値に対して十分小さく取る
/// （残りは保管ロックの待ち [`STORE_LOCK_WAIT`] と小さなファイル 2 本の書き込み）。
///
/// 収束しないということは書き手が止まっていないということなので、
/// 回数を増やしても通らない ＝ 早く諦めて打つ手を返す方が速い
const OWNER_CHECK_ATTEMPTS: u32 = 3;

/// 保管への 1 件書き込み（[`AccountStore::upsert`]）の要求元。
///
/// **書き込みの流儀（待つか・在否を再確認するか）を要求元から導く**ための型。
/// bool を 2 つ渡す形にすると呼び出し口ごとに組み合わせを選び直すことになり、
/// 「追従更新なのに待つ」のような食い違いが黙って入る（実際にそうなっていた）。
/// 要求元は 3 つで固定なので、対応表をここ 1 箇所に持つ
#[derive(Clone, Copy)]
enum Upsert {
    /// 登録（[`AccountStore::register`]）。保管に無ければ作る
    Register,
    /// 切替時の巻き取り（[`AccountStore::switch_to`]）
    Capture,
    /// 追従更新（[`AccountStore::sync_active`]）
    FollowUp,
}

impl Upsert {
    /// 保管ロックの待ち時間。
    ///
    /// **追従更新は待ってはいけない。** `sync_active` は 1 秒周期のフッターポーラーから
    /// 繰り返し呼ばれるので、別インスタンスが保管を書いている間に [`STORE_LOCK_WAIT`]
    /// ぶん待つと 1 ティックあたりその分止まり、アカウント行と版行の更新が遅れる
    /// （あちらが claude 側のロックを `Duration::ZERO` で取っているのと同じ判断。
    /// 取り逃しても認証ファイルの変化と周期フォールバックで次の機会が来る）。
    ///
    /// **巻き取りは待つ。** ユーザーが押した切替の一部で、取り逃すと使い捨ての
    /// refreshToken を落として**そのアカウントへ戻れなくなる** ＝ 次の機会が無い
    fn store_lock_wait(self, store: &AccountStore) -> Duration {
        match self {
            Self::Register | Self::Capture => store.store_lock_wait,
            Self::FollowUp => Duration::ZERO,
        }
    }

    /// 保管に無いアカウントには何もしないか。**明示登録するまで認証情報を
    /// コピーしない**という規則で、登録そのもの以外はすべてこちら
    fn only_if_present(self) -> bool {
        match self {
            Self::Register => false,
            Self::Capture | Self::FollowUp => true,
        }
    }
}

/// 認証情報ファイルの**今の持ち主**を判定し直した結果（[`OwnerCheck`]）。
///
/// **「分からない」を表せることが要点**: 判定できないまま「別人ではない」と
/// みなすと、指紋ガードが守っていた性質（別アカウントのトークンをこの email の
/// 保管へ書かない）がそのまま抜ける
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Owner {
    /// この email がログイン中
    LoggedIn(String),
    /// 誰もログインしていない
    LoggedOut,
    /// 判定できなかった（CLI が起動できない・出力が読めない）
    Unknown,
}

/// 持ち主を判定し直す口。**実体は `claude auth status --json`**（子プロセス）で、
/// それを知っているのは表示側（[`crate::poll`]）なので注入で受ける
/// （ドメイン層が CLI の起動を持たない ＝ テストは実 CLI を叩かずに検査できる）
pub(crate) type OwnerCheck = std::sync::Arc<dyn Fn() -> Owner + Send + Sync>;

/// アカウント保管ストア。保管先とロックの位置は [`Paths`] で注入する
pub(crate) struct AccountStore {
    paths: Paths,
    lock_wait: Duration,
    lock_stale: Duration,
    store_lock_wait: Duration,
    /// 認証情報が動いていたときに持ち主を判定し直す口（[`Self::confirm`]）。
    /// 無ければ「動いた ＝ 中止」の従来どおり
    owner_check: Option<OwnerCheck>,
}

impl AccountStore {
    pub(crate) fn new(paths: Paths) -> Self {
        Self {
            paths,
            lock_wait: LOCK_WAIT,
            lock_stale: LOCK_STALE,
            store_lock_wait: STORE_LOCK_WAIT,
            owner_check: None,
        }
    }

    /// 既定パスのストア。ホームが取れない環境では None
    pub(crate) fn detect() -> Option<Self> {
        Some(Self::new(Paths::detect()?))
    }

    /// 持ち主の再判定を付ける（[`Self::confirm`]）。
    ///
    /// **付けるのはユーザーが押した操作の経路だけ**（[`crate::source::LiveSource`] の
    /// `apply_account`）: 再判定は子プロセス 1 回ぶん（~350ms）なので、
    /// 1 秒周期のポーラーから呼ばれる追従更新には付けない（あちらは
    /// 「次の機会がある」処理なので、疑わしければ見送るのが正しい）
    pub(crate) fn with_owner_check(mut self, check: OwnerCheck) -> Self {
        self.owner_check = Some(check);
        self
    }

    /// 追従更新: 登録済みアカウントがアクティブな間、現行の認証情報を保管へ反映する。
    /// 戻り値は反映したか（未登録・ロック競合・読めない = false）。
    ///
    /// **これが無いと保管は腐る。** claude がトークンを更新すると `refreshToken` が
    /// 新しい値に置き換わり、**古い refreshToken は無効になる**（使い捨て。
    /// anthropics/claude-code#31637 のコメント: "Refresh tokens are one-time use"）。
    /// 追従しないと「A を登録 → A で作業 → B に切替 → A に戻す」で A が壊れる。
    ///
    /// **未登録のアカウントは何もしない。** 明示登録するまで認証情報を勝手に
    /// コピーしない（ユーザーの決定）
    pub(crate) fn sync_active(&self, active: &ActiveAccount) -> anyhow::Result<bool> {
        let account = &active.account;
        if account.email.is_empty() || !self.is_registered(&account.email) {
            return Ok(false);
        }
        // 追従更新はポーラーから繰り返し呼ばれるので **待たない**。待つと
        // アカウント行の更新がロックの待ち時間ぶん止まる。取り逃しても
        // 認証ファイルの変化と周期フォールバックで次の機会が来る
        let Ok(_lock) = Lock::acquire(&self.paths.lock, Duration::ZERO, self.lock_stale) else {
            return Ok(false);
        };
        // **「誰のトークンか」の判定はロックの外・数百 ms 前**（`claude auth status`
        // が認証情報を読んだ時点）に済んでいる。その後に差し替わっていたら、
        // 新しいアカウントのトークンを古い email の保管へ書くことになる。
        // 追従更新はもともと「次の機会がある」処理なので、疑わしければ落とす
        if !self.still_current(active) {
            return Ok(false);
        }
        // 読めない（消えた・書き換え途中）ならエラーにしない。追従更新は
        // 「次の機会がある」処理で、失敗を報告しても打つ手が無い
        let Some(oauth) = read_oauth(&self.paths.credentials) else {
            return Ok(false);
        };
        self.upsert(account, &oauth, Upsert::FollowUp)
    }

    /// 保管ファイルに載っているか
    pub(crate) fn is_registered(&self, email: &str) -> bool {
        !email.is_empty() && read_accounts(&self.paths.store).contains_key(email)
    }

    /// 現行の認証情報の指紋。呼び出し側は「持ち主を判定した時点の状態」として持ち回り、
    /// 巻き取りの直前に照合させる（[`ActiveAccount`]）
    pub(crate) fn credentials_fingerprint(&self) -> CredentialsFp {
        credentials_fingerprint(&self.paths.credentials)
    }

    /// 呼び出し側の観測が **今もそのまま** か。**ロックを保持している間に呼ぶ**
    /// （呼んだ後に差し替わらないことがロックで保証されている区間でしか意味を持たない）
    fn still_current(&self, active: &ActiveAccount) -> bool {
        self.credentials_fingerprint() == active.seen
    }

    /// 観測が今も有効か確かめ、**有効なら今の指紋を載せ直した観測**を返す。
    /// **ロックを保持している間に呼ぶ**（[`Self::still_current`] と同じ理由）。
    ///
    /// # なぜ「変わった ＝ 中止」では駄目だったか
    ///
    /// 指紋（mtime + サイズ）は**ファイルが動いたか**しか答えない。ところが
    /// 動いている claude はトークン更新のたびにこのファイルを書くので、セッションを
    /// 複数抱えていると**メニューを開いてから押すまでの数秒で必ず動く** ＝
    /// 切替が毎回 `changed since ccdesk last checked` で弾かれた（実機で再現）。
    ///
    /// 区別すべきものは 2 つある:
    ///
    /// - **同じアカウントのトークン更新** → 無害。むしろ新しい値を保管すべき ＝ 続行
    /// - **別アカウントへの差し替え**（別端末での `/login` 等） → 中止
    ///
    /// どちらかは指紋では言えないので、**持ち主を判定し直す**（[`OwnerCheck`]）。
    ///
    /// # 判定できないときは中止する
    ///
    /// 再判定の口が無い / `Unknown` / email が空 / email が違う、のいずれも中止。
    /// 守っている性質は変わらない ＝ **別アカウントのトークンをこの email の保管へ
    /// 書かない**（refreshToken は使い捨てなので、それは復旧不能な破壊になる）。
    ///
    /// # 判定中に動いたら諦めずに取り直す
    ///
    /// 再判定の**前後で指紋が一致すること**も確かめる: 動いたなら、その判定は
    /// 今のファイルについてのものではない。ただし**そこで失敗にしてはいけない**。
    /// 判定は子プロセス 1 つぶん（~370ms）かかり、その窓に動いている claude の
    /// トークン更新が入るのは珍しくないので、1 回動いただけで諦めると
    /// 「押すたびに同じエラー」になる（リトライが無かった）。
    /// 収束するまで [`OWNER_CHECK_ATTEMPTS`] 回だけ取り直す。
    ///
    /// **取り直すのは指紋が動いた場合だけ。** 持ち主が違う・未ログイン・判定不能は
    /// もう答えが出ているので、繰り返してもロックを長く握るだけになる
    fn confirm(&self, active: &ActiveAccount) -> anyhow::Result<ActiveAccount> {
        if self.still_current(active) {
            return Ok(active.clone());
        }
        let email = &active.account.email;
        let refuse = |reason: Unconfirmed| reason.into_error(&self.paths.credentials, email);
        let Some(check) = self.owner_check.as_ref() else {
            return Err(refuse(Unconfirmed::NoOwnerCheck));
        };
        if email.is_empty() {
            return Err(refuse(Unconfirmed::NoEmail));
        }
        for _ in 0..OWNER_CHECK_ATTEMPTS {
            // 判定の材料になるファイルの状態を、判定の**前に**読む（[`crate::poll`] の
            // 取得と同じ順序。後から読むと「古い判断に新しい日付」が付く）
            let seen = self.credentials_fingerprint();
            let owner = check();
            if seen != self.credentials_fingerprint() {
                continue; // 判定中に書き換わった ＝ この答えは今のファイルのものではない
            }
            return match owner {
                Owner::LoggedIn(now) if now == *email => {
                    Ok(ActiveAccount::new(active.account.clone(), seen))
                }
                other => Err(refuse(Unconfirmed::Owner(other))),
            };
        }
        Err(refuse(Unconfirmed::KeptChanging))
    }

    /// 現行の認証情報をロック下で読んで保管する。ロックを取るのは、claude の
    /// トークン更新の途中（読む → ネットワーク → 保存）の値を保管しないため。
    /// 使い捨ての refreshToken を古い値で保管すると、そのアカウントは切替時に
    /// 復元できない
    fn capture_current(&self, active: &ActiveAccount) -> anyhow::Result<()> {
        let _lock = Lock::acquire(&self.paths.lock, self.lock_wait, self.lock_stale)?;
        // 観測が古いなら **別アカウントの現行トークンをこの email として保管しうる**
        // （切替直後の `register current` で実際に起きていた）。動いていても
        // 持ち主が同じなら続ける（[`Self::confirm`]）
        let active = &self.confirm(active)?;
        let oauth = read_oauth(&self.paths.credentials).ok_or_else(|| {
            anyhow!(
                "{} has no usable {OAUTH_KEY}: either no account is logged in, \
                 or claude keeps the credentials outside this file \
                 (OS credential manager), in which case ccdesk cannot store it",
                self.paths.credentials.display()
            )
        })?;
        self.upsert(&active.account, &oauth, Upsert::Register)?;
        Ok(())
    }

    /// 保管ファイルへ 1 件書く。
    ///
    /// **要求元（[`Upsert`]）で流儀が変わる**のが要点で、待ち時間と在否の再確認は
    /// どちらも「ユーザーが押した操作か、ポーラー契機の追従更新か」で決まる。
    /// 2 つの引数に割ると片方だけ渡し忘れる形ができるので 1 つの型で受ける
    fn upsert(&self, account: &Account, oauth: &Value, kind: Upsert) -> anyhow::Result<bool> {
        let _guard = self.lock_store(kind.store_lock_wait(self))?;
        let mut accounts = read_accounts(&self.paths.store);
        if kind.only_if_present() && !accounts.contains_key(&account.email) {
            return Ok(false);
        }
        // **2 つの保管が同じ refreshToken を指す状態を作らない**（[`other_holder`]）
        if let Some(other) = other_holder(&accounts, &account.email, oauth) {
            return Err(anyhow!(
                "refusing to store credentials for {}: the stored entry for {other} already holds \
                 the same {REFRESH_TOKEN_KEY}, so one of the two is wrong; \
                 unregister the wrong one and register it again",
                account.email
            ));
        }
        accounts.insert(
            account.email.clone(),
            json!({ LABEL_KEY: account.label, CREDENTIALS_KEY: oauth }),
        );
        write_json_atomically(&self.paths.store, &json!({ ACCOUNTS_KEY: accounts }))?;
        Ok(true)
    }

    /// 保管ファイルの read-modify-write を守るロック。**書き手 4 つ（登録・切替の
    /// 巻き取り・追従更新・登録解除）が全てこれを通る**ことが不変条件で、
    /// 1 つでも外れると多重起動で書き込みが消える。
    /// 待ちは要求元が決める（[`Upsert::store_lock_wait`] / [`STORE_LOCK_WAIT`]）
    fn lock_store(&self, wait: Duration) -> anyhow::Result<Lock> {
        Lock::acquire(&self.paths.store_lock(), wait, self.lock_stale)
    }

    /// 起動時の掃除: [`write_json_atomically`] が rename する前にプロセスが死ぬと、
    /// **トークン入りの `.tmp` が誰にも消されずに残る**（README が
    /// 「`accounts.json` は `.credentials.json` と同じ扱いをせよ」と案内している
    /// 対象の外にファイルが増える）。
    ///
    /// **どう回収するかは [`ccdesk::reap_leftover_tmp`]**（tmp の名前を決める側と
    /// 同じ場所）。ここが持つのは「保管ストアが守る 2 本」という対象の指定だけ
    pub(crate) fn cleanup_leftover_tmp(&self) {
        for target in [&self.paths.store, &self.paths.credentials] {
            ccdesk::reap_leftover_tmp(target);
        }
    }
}

/// 観測が古く、持ち主を確かめ直せなかった理由（[`AccountStore::confirm`]）。
///
/// **経路ごとに違う文言を出すためだけに在る型。** 拒む理由はここに並ぶだけあるのに
/// 全部が同じ 1 文（「変わったのでメニューを開き直せ」）を返していたので、実機で
/// 失敗したときにログを見ても**どの経路で落ちたのか分からなかった** ＝ 原因の
/// 切り分けができない。打つ手も経路ごとに違う（待ち直す・ログインし直す・
/// claude を起動できるようにする）ので、1 文にまとめられる情報ではない。
///
/// **打つ手を必ず書く**のは元の文と同じ方針。トークンは載せない
/// （載るのはパスと email だけ）
enum Unconfirmed {
    /// 再判定の口が付いていない経路（[`AccountStore::with_owner_check`] を通らない）。
    /// 追従更新はここに来ない（あちらは失敗ではなく見送り）ので、
    /// 実際に出るのは口を付け忘れた呼び出し口だけ ＝ 文言でそれと分かる必要がある
    NoOwnerCheck,
    /// 観測されたアカウントが email を持たない ＝ 再判定の結果と照合できない
    NoEmail,
    /// 再判定のたびに認証情報が書き換わり、[`OWNER_CHECK_ATTEMPTS`] 回で収束しなかった
    KeptChanging,
    /// 判定し直した持ち主が観測と食い違う（別 email・未ログイン・判定不能）
    Owner(Owner),
}

impl Unconfirmed {
    /// `expected` は観測されていた持ち主の email（[`Self::NoEmail`] では空）
    fn into_error(self, credentials: &Path, expected: &str) -> anyhow::Error {
        let path = credentials.display();
        match self {
            Self::NoOwnerCheck => anyhow!(
                "{path} changed since ccdesk last checked which account is logged in, \
                 and this code path cannot re-check the owner; \
                 reopen the account menu and try again"
            ),
            Self::NoEmail => anyhow!(
                "{path} changed since ccdesk last checked which account is logged in, \
                 and the logged-in account has no email to re-check it against; \
                 reopen the account menu and try again"
            ),
            Self::KeptChanging => anyhow!(
                "{path} was rewritten during each of the {OWNER_CHECK_ATTEMPTS} owner \
                 re-checks, so ccdesk never saw who owns a settled file; \
                 let the running claude sessions go idle and try again"
            ),
            Self::Owner(Owner::LoggedIn(now)) => anyhow!(
                "{path} now belongs to {now}, not {expected}, so ccdesk left it alone; \
                 reopen the account menu and try again"
            ),
            Self::Owner(Owner::LoggedOut) => anyhow!(
                "{path} changed and no account is logged in now, \
                 so ccdesk cannot tell what belongs to {expected}; \
                 log in and try again"
            ),
            Self::Owner(Owner::Unknown) => anyhow!(
                "{path} changed and ccdesk could not run claude to see who owns it now; \
                 make sure claude runs from this shell and try again"
            ),
        }
    }
}

/// UI（アカウント切替ポップアップ）向けの公開 API。
/// 呼び出し元は後続の別作業が入れるため、この repo にはまだ無い
#[allow(dead_code)]
impl AccountStore {
    /// 保管済み一覧。キーが email なので並びは email 昇順で安定する
    /// （`serde_json::Map` は既定で `BTreeMap`）
    pub(crate) fn list(&self) -> Vec<Account> {
        read_accounts(&self.paths.store)
            .iter()
            .map(|(email, entry)| Account::new(email.clone(), entry_label(entry, email)))
            .collect()
    }

    /// 登録: 現行の認証情報の `claudeAiOauth` を email をキーに保管する。
    /// `active` は「今ログイン中のアカウント」の観測で、**保管する前に
    /// ロック下で照合する**（古ければ別アカウントのトークンを保管しかねない）
    pub(crate) fn register(&self, active: &ActiveAccount) -> anyhow::Result<()> {
        if active.account.email.is_empty() {
            // 表示ラベルで代用してはいけない（同一性の判定に表示ロジックが混ざる）
            return Err(anyhow!(
                "this account has no email, so there is no stable key to store it under"
            ));
        }
        self.capture_current(active)
    }

    /// 登録解除: 保管を消すだけ。**ログイン自体は外さない**
    /// （現行の `.credentials.json` には触らない）
    pub(crate) fn unregister(&self, email: &str) -> anyhow::Result<()> {
        let _guard = self.lock_store(self.store_lock_wait)?;
        let mut accounts = read_accounts(&self.paths.store);
        if accounts.remove(email).is_none() {
            return Ok(()); // 既に無い＝目的は達成されている
        }
        write_json_atomically(&self.paths.store, &json!({ ACCOUNTS_KEY: accounts }))
    }

    /// 切替: 保管した `claudeAiOauth` を現行ファイルへ書き戻す。
    /// `mcpOAuth` と未知のトップレベルキーは保つ（[`OAUTH_KEY`] 参照）。
    ///
    /// `outgoing` は今の持ち主についての観測（[`Outgoing`]）。**出ていく
    /// アカウントの認証情報を、上書きする前に同じロックの下で保管へ取り込む**:
    /// 追従更新（[`AccountStore::sync_active`]）はポーリング契機なので直前の
    /// トークン更新を取り逃す窓があり、使い捨ての refreshToken をそこで落とすと
    /// そのアカウントには戻れなくなる。
    ///
    /// **持ち主を観測できていない状態はこの引数で表せない**（[`Outgoing`]）ので、
    /// 「誰の認証情報か分からないまま上書きする」経路はここに入って来ない。
    ///
    /// **観測が古ければ書かずに失敗する。** 巻き取り先を決める材料が古いと
    /// 「A の保管に B のトークンを書く」＝ A も B も復旧不能、という壊し方をする
    /// （[`ActiveAccount`]）。切替自体を諦めるのは、諦めれば次の操作で必ず正しく
    /// やり直せるのに対し、書いてしまうと取り返しがつかないため
    pub(crate) fn switch_to(
        &self,
        email: &str,
        outgoing: &Outgoing,
    ) -> anyhow::Result<AccountChange> {
        // 保管の読みは **意図的にロックの外**。`~/.claude.lock` が守るのは claude と
        // 共有する認証情報ファイルで、こちらの保管ファイルはその対象ではない
        // （ロック下に入れると、claude の保持時間ぶん自分の読みも待つことになる）。
        // 許容している穴: ccdesk を複数起動していると、読んだ後に別インスタンスが
        // `unregister` する窓がある。書き込みは tmp + rename で原子的なので、
        // 最悪でも「登録解除したはずのアカウントに切り替わる」だけでファイルは壊れない
        let (label, stored) = self.stored_entry(email)?;
        let _lock = Lock::acquire(&self.paths.lock, self.lock_wait, self.lock_stale)?;
        // 判断材料が動いていたら持ち主を判定し直す（[`Self::confirm`]）。
        // 別人へ変わっていれば巻き取りも上書きもしない
        let outgoing = match outgoing {
            Outgoing::Known(active) => Some(self.confirm(active)?),
            // 誰もログインしていないと観測できている ＝ 巻き取る対象が無い
            Outgoing::NobodyLoggedIn => None,
        };
        let capture = match &outgoing {
            // 同じアカウントへの「切替」は何もしない。書き戻すと、保管より新しい
            // 可能性のある現行トークンを古い写しで上書きしてしまい、使い捨ての
            // refreshToken が無効な値に戻って **今のログインを壊す**
            Some(active) if !active.account.email.is_empty() && active.account.email == email => {
                return Ok(AccountChange::AlreadyActive)
            }
            // email を持たないアカウント（email を返さない認証方式）は保管の
            // キーが無いので巻き取れない。切替自体は通す
            Some(active) => Some(&active.account).filter(|a| !a.email.is_empty()),
            None => None,
        };
        let mut current = self.current_document()?;
        if let Some(capture) = capture
            && let Some(oauth) = current.get(OAUTH_KEY).filter(|o| usable_oauth(o)).cloned()
        {
            // 未登録のアカウントには何もしない（[`Upsert::only_if_present`]）。
            // 明示登録するまで認証情報をコピーしない規則は切替でも同じ
            self.upsert(capture, &oauth, Upsert::Capture)?;
        }
        current[OAUTH_KEY] = stored;
        write_json_atomically(&self.paths.credentials, &current)?;
        // 使用率キャッシュは **どのアカウントの数字か記録していない**（statusline へ
        // 渡される公式 JSON にアカウント情報が無いので、識別子を後から足せない）。
        // stale 判定は 10 分経過のみなので、消さないと切替後も前アカウントの残量を
        // 最大 10 分表示して嘘になる
        let _ = std::fs::remove_file(&self.paths.usage_cache);
        // **新しい持ち主はここで確定する。** 書いたのは自分なので、ポーラーが
        // `claude auth status` で追いつくのを待つ必要が無い（待つと、その 1〜2 秒に
        // 入った次の操作が古い持ち主を材料に走る）。ラベルは保管のものなので、
        // ポーラーが追いついた時点で live の表記へ揃う
        Ok(AccountChange::Switched(ActiveAccount::new(
            Account::new(email, label),
            self.credentials_fingerprint(),
        )))
    }

    /// 保管 1 件の (ラベル, `claudeAiOauth`)。**戻せない写しは失敗にする**
    /// （手編集や旧版の残骸で `refreshToken` を持たない写しを書き戻すと、
    /// 今のログインを壊すだけで切替先へは行けない）
    fn stored_entry(&self, email: &str) -> anyhow::Result<(String, Value)> {
        let entry = read_accounts(&self.paths.store)
            .get(email)
            .cloned()
            .ok_or_else(|| anyhow!("no stored credentials for {email}"))?;
        let credentials = entry
            .get(CREDENTIALS_KEY)
            .filter(|c| usable_oauth(c))
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "the stored credentials for {email} have no {REFRESH_TOKEN_KEY}; \
                     switch to that account elsewhere and register it again"
                )
            })?;
        Ok((entry_label(&entry, email).to_string(), credentials))
    }

    /// 差し替えの土台になる現行ファイル。無い・空なら新規（`{}`）から作る。
    ///
    /// **中身があるのに JSON として読めないときは失敗させる。** そこで `{}` に
    /// 倒すと、読めなかっただけの `mcpOAuth` を消してしまう。
    ///
    /// **「無い」と言えるのは `NotFound` だけ。** 存在するが読めない（権限・
    /// ウイルス対策やバックアップの共有違反・I/O エラー）を未ログインと同じ扱いに
    /// すると、rename が通る限り `mcpOAuth` と未知のトップレベルキーを丸ごと失う
    /// ＝ 壊れた JSON を失敗にしている理由がそのまま当てはまる
    fn current_document(&self) -> anyhow::Result<Value> {
        let path = &self.paths.credentials;
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(json!({})); // 未ログイン（ファイルが無い）
            }
            Err(e) => return Err(anyhow!("could not read {}: {e}", path.display())),
        };
        if text.trim().is_empty() {
            return Ok(json!({}));
        }
        let value = serde_json::from_str::<Value>(&text)
            .map_err(|e| anyhow!("{} is not valid JSON: {e}", path.display()))?;
        if !value.is_object() {
            return Err(anyhow!("{} is not a JSON object", path.display()));
        }
        Ok(value)
    }
}

/// JSON を読む。無い・壊れている・書き換え途中はすべて None
fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// 認証情報ファイルの `claudeAiOauth`（保管に使えない形なら無い扱い）
fn read_oauth(path: &Path) -> Option<Value> {
    let value = read_json(path)?;
    value.get(OAUTH_KEY).filter(|o| usable_oauth(o)).cloned()
}

/// 保管・復元してよい `claudeAiOauth` か。
///
/// **キー集合は固定しない。** 固定すると、将来 claude が増やしたキーを保管の
/// 時点で落としてしまう（[`OAUTH_KEY`] の方針と同じ）。見るのは `refreshToken` が
/// あるかだけ: 保管の目的は「そのアカウントへ戻れること」で、refreshToken の無い
/// 写し（手編集・旧版の残骸・別方式の認証情報）は戻しても何も得られず、
/// 現行のログインを壊すだけになる
fn usable_oauth(value: &Value) -> bool {
    value.is_object() && refresh_token(value).is_some()
}

/// 保管 1 件の表示ラベル。**ラベルが失われていても空にはしない**
/// （識別子で代替する。空行はメニューで選べない行に見える）
fn entry_label<'a>(entry: &'a Value, email: &'a str) -> &'a str {
    entry
        .get(LABEL_KEY)
        .and_then(|l| l.as_str())
        .filter(|l| !l.is_empty())
        .unwrap_or(email)
}

/// `oauth` と**同じ `refreshToken` を既に持っている別の email**（無ければ None）。
///
/// # なぜ検出するか
///
/// refreshToken は使い捨てなので、2 つの保管が同じ値を指した瞬間に**両方が壊れる**
/// （片方を使うと他方の値は無効になる）。実機で「2 つのアカウントが同じトークンを
/// 持ち、どちらへ switch しても何も起きない」状態が起きていた。
///
/// # どうやってその状態になるか
///
/// 書き手は [`AccountStore::upsert`] 1 つだけなので、必ず「email E のつもりで
/// アカウント F のトークンを書いた」ときに起きる。持ち主の判定材料は
/// `claude auth status --json` で、**認証情報ファイルを差し替えた直後は
/// そこが前のアカウントを答えうる**（実測で `.claude.json` の `oauthAccount` は
/// 遅延取得のキャッシュで、切替直後は前のアカウントの値が残る窓がある）。
/// 指紋（[`CredentialsFp`]）はファイルが動いたかしか見ないので、
/// ccdesk 自身が書いた直後の「動いていない」状態では通ってしまう。
///
/// **そこで保管そのものに聞く。** 書こうとしているトークンを別の email が既に
/// 持っているなら、その判定は間違っている ＝ 書かない
/// （諦めれば次の操作でやり直せるが、書いてしまうと取り返しがつかない）
fn other_holder<'a>(
    accounts: &'a serde_json::Map<String, Value>,
    email: &str,
    oauth: &Value,
) -> Option<&'a str> {
    let token = refresh_token(oauth)?;
    accounts
        .iter()
        .find(|(other, entry)| {
            other.as_str() != email
                && entry.get(CREDENTIALS_KEY).and_then(refresh_token) == Some(token)
        })
        .map(|(other, _)| other.as_str())
}

/// `claudeAiOauth` の `refreshToken`（無い・空なら None）
fn refresh_token(oauth: &Value) -> Option<&str> {
    oauth
        .get(REFRESH_TOKEN_KEY)
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
}

/// 保管ファイルの `accounts`（無い・壊れていれば空）
fn read_accounts(path: &Path) -> serde_json::Map<String, Value> {
    let Some(value) = read_json(path) else {
        return serde_json::Map::new();
    };
    value
        .get(ACCOUNTS_KEY)
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;
    // 取り残し tmp の判定と保持期間は lib 側（tmp 名を決める場所）が持つ
    use ccdesk::{is_leftover_tmp, TMP_KEEP};

    const EMAIL_A: &str = "taro@example.com";
    const EMAIL_B: &str = "hanako@example.com";

    /// テスト専用の擬似ホーム。**実ユーザーの `~/.claude` / `~/.ccdesk` を
    /// 絶対に触らない**ための境界で、パスは全て [`Paths`] 経由で注入する。
    /// Drop で丸ごと消すので、アサート失敗でパニックしても残らない。
    ///
    /// **他モジュールのテストからも使う**（[`crate::source`] の
    /// 「UI の動作 → ドメイン API」の対応表テスト）。フィクスチャを複製すると
    /// 「実ホームを触らない」境界の知識が 2 箇所に分かれるので、ここ 1 つに保つ
    pub(crate) struct TempHome(PathBuf);

    impl TempHome {
        /// パスはテスト名 + pid + 連番で一意にする（並列実行・別チェックアウトの
        /// 同時実行と衝突させない。Drop がディレクトリごと消すので共有は事故になる）
        pub(crate) fn new(test: &str) -> Self {
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "ccdesk-accounts-{test}-{}-{seq}",
                std::process::id()
            ));
            std::fs::create_dir_all(root.join(".claude")).unwrap();
            std::fs::create_dir_all(root.join(".ccdesk")).unwrap();
            Self(root)
        }

        /// 実運用と同じ導出（`lock_path_for`）を通す
        pub(crate) fn paths(&self) -> Paths {
            let claude = self.0.join(".claude");
            Paths {
                store: self.0.join(".ccdesk").join("accounts.json"),
                credentials: claude.join(".credentials.json"),
                lock: lock_path_for(&claude),
                usage_cache: self.0.join(".ccdesk").join("usage.json"),
            }
        }

        pub(crate) fn store(&self) -> AccountStore {
            AccountStore::new(self.paths())
        }

        /// 持ち主の再判定が固定の答えを返すストア（実 CLI を叩かない）。
        /// 実運用では `claude auth status --json` が答える（[`crate::poll::current_owner`]）
        fn store_that_sees(&self, owner: Owner) -> AccountStore {
            self.store_that_checks(move || owner.clone())
        }

        /// 持ち主の再判定に**副作用を持たせられる**ストア。実運用の再判定は
        /// 子プロセス 1 つぶん（~370ms）かかるので、その最中に動いている claude が
        /// トークンを更新する状況が起きる ＝ それをここで作る
        /// （[`AccountStore::confirm`] のリトライ）
        fn store_that_checks(
            &self,
            check: impl Fn() -> Owner + Send + Sync + 'static,
        ) -> AccountStore {
            self.store().with_owner_check(std::sync::Arc::new(check))
        }

        /// 待ち時間を詰めたストア（ロック競合を有界時間でテストするため）
        pub(crate) fn store_with_short_wait(&self) -> AccountStore {
            let mut store = self.store();
            store.lock_wait = Duration::from_millis(50);
            store.store_lock_wait = Duration::from_millis(50);
            store
        }

        /// 「今の持ち主はこのアカウント」という観測を **今のファイル状態で** 作る。
        ///
        /// 実運用ではポーラーが `claude auth status` の**前**に指紋を読んで作る値
        /// （[`ActiveAccount`]）で、テストからは「UI がその状態を見ていた」に相当する。
        /// **これを取った後に `write_credentials` すると観測は古くなる** ＝
        /// 別端末での `/login` やトークンローテーションと同じ状況が作れる
        pub(crate) fn active(&self, email: &str, label: &str) -> ActiveAccount {
            ActiveAccount::new(
                Account::new(email, label),
                credentials_fingerprint(&self.paths().credentials),
            )
        }

        pub(crate) fn write_credentials(&self, value: &Value) {
            write_credentials_at(&self.paths().credentials, value);
        }

        pub(crate) fn read_credentials(&self) -> Value {
            read_json(&self.paths().credentials).expect("failed to read credentials file")
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 認証情報ファイルを書く。**[`TempHome`] を借りずに書ける形**にしてあるのは、
    /// 持ち主の再判定（`'static` なクロージャ）の中から claude のトークン更新を
    /// 模すため。書き方の知識は 1 箇所（[`TempHome::write_credentials`] もこれを通る）
    fn write_credentials_at(path: &Path, value: &Value) {
        std::fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
    }

    /// 実測した認証情報ファイルの形（トークンは架空。トップレベルに
    /// `mcpOAuth` が同居し、`claudeAiOauth` に email は入らない）
    pub(crate) fn credentials_doc(access: &str, refresh: &str) -> Value {
        json!({
            "mcpOAuth": {
                "linear-server": { "accessToken": "mcp-token", "expiresAt": 1_800_000_000_u64 }
            },
            OAUTH_KEY: oauth(access, refresh),
        })
    }

    pub(crate) fn oauth(access: &str, refresh: &str) -> Value {
        json!({
            "accessToken": access,
            "refreshToken": refresh,
            "expiresAt": 1_900_000_000_u64,
            "refreshTokenExpiresAt": 1_950_000_000_u64,
            "scopes": ["user:inference", "user:profile"],
            "subscriptionType": "max",
            "rateLimitTier": "default_claude_max_20x",
        })
    }

    /// 認証情報を「外から」書き換える前に挟む待ち。
    ///
    /// 指紋は (mtime, サイズ) なので、**同じ時刻刻みの中で同サイズに書き換えると
    /// 変化として見えない**（[`CredentialsFp`]）。実運用の書き換え（トークン更新・
    /// 別端末での `/login`）は必ず刻みを跨ぐので、テストでも同じ条件を作る。
    /// これを省くとテストが「たまたま検出できない」で落ちる
    fn wait_for_a_new_mtime() {
        std::thread::sleep(Duration::from_millis(40));
    }

    /// 保管された `claudeAiOauth`（トークン比較用）。
    /// **他モジュールのテストからも使う**（[`crate::app`] の操作列テストが
    /// 「保管が別アカウントのトークンで潰れていないか」を見る）
    pub(crate) fn stored_oauth(store: &AccountStore, email: &str) -> Option<Value> {
        read_accounts(&store.paths.store)
            .get(email)?
            .get(CREDENTIALS_KEY)
            .cloned()
    }

    /// 切替は `claudeAiOauth` だけを入れ替える。`mcpOAuth` は
    /// アカウントに依存しない状態なので、消すと MCP の認証が壊れる
    #[test]
    fn switch_replaces_only_the_claude_oauth_key() {
        let home = TempHome::new("switch_replaces_only_the_claude_oauth_key");
        let store = home.store();

        // A でログイン中に A を登録
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&home.active(EMAIL_A, "taro")).unwrap();

        // B へログインし直した状態（mcpOAuth は claude 側が更新した別の値）
        let mut with_b = credentials_doc("access-b", "refresh-b");
        with_b["mcpOAuth"] = json!({ "notion": { "accessToken": "mcp-notion" } });
        home.write_credentials(&with_b);

        store.switch_to(EMAIL_A, &Outgoing::NobodyLoggedIn).unwrap();

        let after = home.read_credentials();
        assert_eq!(
            after[OAUTH_KEY],
            oauth("access-a", "refresh-a"),
            "stored claudeAiOauth was not restored"
        );
        assert_eq!(
            after["mcpOAuth"], with_b["mcpOAuth"],
            "mcpOAuth was not preserved (breaks MCP auth)"
        );
    }

    /// 将来 claude がトップレベルにキーを増やしても壊れない形か。
    /// 保管した値から作り直すのではなく、現行ファイルを土台にすることの固定
    #[test]
    fn switch_preserves_unknown_top_level_keys() {
        let home = TempHome::new("switch_preserves_unknown_top_level_keys");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&home.active(EMAIL_A, "taro")).unwrap();

        let mut future = credentials_doc("access-b", "refresh-b");
        future["someFutureKey"] = json!({ "nested": [1, 2, 3] });
        future["anotherKey"] = json!("value");
        home.write_credentials(&future);

        store.switch_to(EMAIL_A, &Outgoing::NobodyLoggedIn).unwrap();

        let after = home.read_credentials();
        assert_eq!(after["someFutureKey"], future["someFutureKey"]);
        assert_eq!(after["anotherKey"], future["anotherKey"]);
        assert_eq!(
            after.as_object().unwrap().len(),
            future.as_object().unwrap().len(),
            "top-level key count changed"
        );
    }

    /// 保管にはアカウントに属するキーだけを入れる（`mcpOAuth` を持ち込むと、
    /// 切替のたびに他アカウント時点の MCP 認証を復元してしまう）
    #[test]
    fn register_stores_only_the_account_scoped_key() {
        let home = TempHome::new("register_stores_only_the_account_scoped_key");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&home.active(EMAIL_A, "taro")).unwrap();

        let stored = stored_oauth(&store, EMAIL_A).expect("not stored");
        assert_eq!(stored, oauth("access-a", "refresh-a"));
        assert!(stored.get("mcpOAuth").is_none());
    }

    /// email が無いアカウント（email を返さない認証方式）は保管できない。
    /// 表示ラベルで代用すると同一性の判定に表示ロジックが混ざる
    #[test]
    fn register_requires_an_email_as_the_key() {
        let home = TempHome::new("register_requires_an_email_as_the_key");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        assert!(store.register(&home.active("", "claude.ai")).is_err());
        assert!(store.list().is_empty());
    }

    /// 未ログイン（`claudeAiOauth` が無い）状態では登録しても保管しない
    #[test]
    fn register_fails_without_current_credentials() {
        let home = TempHome::new("register_fails_without_current_credentials");
        let store = home.store();
        assert!(store.register(&home.active(EMAIL_A, "taro")).is_err());
        home.write_credentials(&json!({ "mcpOAuth": {} }));
        assert!(store.register(&home.active(EMAIL_A, "taro")).is_err());
        assert!(store.list().is_empty());
    }

    /// 一覧は email をキーに保管され、ラベルも保つ
    #[test]
    fn list_returns_stored_accounts_by_email() {
        let home = TempHome::new("list_returns_stored_accounts_by_email");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&home.active(EMAIL_A, "taro")).unwrap();
        home.write_credentials(&credentials_doc("access-b", "refresh-b"));
        store
            .register(&home.active(EMAIL_B, "hanako · Acme, Inc."))
            .unwrap();

        assert_eq!(
            store.list(),
            vec![
                Account::new(EMAIL_B, "hanako · Acme, Inc."), // email 昇順（h < t）
                Account::new(EMAIL_A, "taro"),
            ]
        );
        assert!(store.is_registered(EMAIL_A) && store.is_registered(EMAIL_B));
        assert!(!store.is_registered("nobody@example.com"));
    }

    /// 登録解除は保管だけを消す。**ログインは外さない**ので
    /// 現行の `.credentials.json` は 1 バイトも変わらない
    #[test]
    fn unregister_removes_the_store_entry_but_not_the_login() {
        let home = TempHome::new("unregister_removes_the_store_entry_but_not_the_login");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&home.active(EMAIL_A, "taro")).unwrap();
        home.write_credentials(&credentials_doc("access-b", "refresh-b"));
        store.register(&home.active(EMAIL_B, "hanako")).unwrap();
        let before = std::fs::read(home.paths().credentials).unwrap();

        store.unregister(EMAIL_A).unwrap();

        assert_eq!(store.list(), vec![Account::new(EMAIL_B, "hanako")]);
        assert_eq!(
            std::fs::read(home.paths().credentials).unwrap(),
            before,
            "unregister changed the current credentials (logged the user out)"
        );
        // 二重の登録解除は成功扱い（目的は既に達成されている）
        store.unregister(EMAIL_A).unwrap();
    }

    /// 切替は使用率キャッシュを消す。どのアカウントの数字か記録していないので、
    /// 残すと切替後も前アカウントの残量を最大 10 分表示して嘘になる
    #[test]
    fn switch_clears_the_usage_cache() {
        let home = TempHome::new("switch_clears_the_usage_cache");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&home.active(EMAIL_A, "taro")).unwrap();
        std::fs::write(home.paths().usage_cache, r#"{"written_at":1}"#).unwrap();

        store.switch_to(EMAIL_A, &Outgoing::NobodyLoggedIn).unwrap();

        assert!(
            !home.paths().usage_cache.exists(),
            "usage.json still exists (would show the previous account's remaining usage)"
        );
    }

    /// 未ログイン（ファイルが無い）状態への切替は新規に作る
    #[test]
    fn switch_creates_the_credentials_file_when_missing() {
        let home = TempHome::new("switch_creates_the_credentials_file_when_missing");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&home.active(EMAIL_A, "taro")).unwrap();
        std::fs::remove_file(home.paths().credentials).unwrap();

        store.switch_to(EMAIL_A, &Outgoing::NobodyLoggedIn).unwrap();

        assert_eq!(home.read_credentials()[OAUTH_KEY], oauth("access-a", "refresh-a"));
    }

    /// 出ていくアカウントの認証情報は、上書きする前に保管へ取り込む。
    /// 追従更新はポーリング契機なので直前のトークン更新を取り逃す窓があり、
    /// 使い捨ての refreshToken をそこで落とすと A へ戻れなくなる
    #[test]
    fn switch_captures_the_outgoing_account_before_overwriting_it() {
        let home = TempHome::new("switch_captures_the_outgoing_account_before_overwriting_it");
        let store = home.store();
        // A と B を登録（登録時点の写しが保管に入る）
        home.write_credentials(&credentials_doc("access-b", "refresh-b"));
        store.register(&home.active(EMAIL_B, "hanako")).unwrap();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&home.active(EMAIL_A, "taro")).unwrap();
        // A で作業してトークンが更新された（追従更新はまだ走っていない）
        home.write_credentials(&credentials_doc("access-a3", "refresh-a3"));

        store
            .switch_to(EMAIL_B, &Outgoing::Known(home.active(EMAIL_A, "taro")))
            .unwrap();

        assert_eq!(
            stored_oauth(&store, EMAIL_A),
            Some(oauth("access-a3", "refresh-a3")),
            "did not capture the token right before overwriting (A becomes unreachable)"
        );
        assert_eq!(
            home.read_credentials()[OAUTH_KEY],
            oauth("access-b", "refresh-b")
        );
    }

    /// 未登録のアカウントは切替の巻き取りでも保管しない
    /// （明示登録するまでコピーしない規則は切替でも同じ）
    #[test]
    fn switch_does_not_capture_an_unregistered_outgoing_account() {
        let home = TempHome::new("switch_does_not_capture_an_unregistered_outgoing_account");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&home.active(EMAIL_A, "taro")).unwrap();
        // 未登録の B でログイン中に A へ切り替える
        home.write_credentials(&credentials_doc("access-b", "refresh-b"));

        store
            .switch_to(EMAIL_A, &Outgoing::Known(home.active(EMAIL_B, "hanako")))
            .unwrap();

        assert_eq!(store.list(), vec![Account::new(EMAIL_A, "taro")]);
    }

    /// 今ログイン中のアカウントへの「切替」は何もしない。書き戻すと、保管より
    /// 新しい現行トークンを古い写しで上書きして今のログインを壊す
    #[test]
    fn switch_to_the_active_account_leaves_the_live_tokens_alone() {
        let home = TempHome::new("switch_to_the_active_account_leaves_the_live_tokens_alone");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&home.active(EMAIL_A, "taro")).unwrap();
        home.write_credentials(&credentials_doc("access-a4", "refresh-a4"));
        std::fs::write(home.paths().usage_cache, r#"{"written_at":1}"#).unwrap();

        assert_eq!(
            store
                .switch_to(EMAIL_A, &Outgoing::Known(home.active(EMAIL_A, "taro")))
                .unwrap(),
            AccountChange::AlreadyActive,
            "reported as switched even though nothing changed"
        );

        assert_eq!(
            home.read_credentials()[OAUTH_KEY],
            oauth("access-a4", "refresh-a4"),
            "overwrote the current token with a stale copy (breaks the login)"
        );
        assert!(
            home.paths().usage_cache.exists(),
            "cleared the usage cache even though the account did not change"
        );
    }

    /// 保管に無い相手へは切り替えない（現行ファイルにも触らない）
    #[test]
    fn switch_to_an_unstored_account_fails() {
        let home = TempHome::new("switch_to_an_unstored_account_fails");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        let before = std::fs::read(home.paths().credentials).unwrap();

        assert!(store.switch_to(EMAIL_B, &Outgoing::NobodyLoggedIn).is_err());
        assert_eq!(std::fs::read(home.paths().credentials).unwrap(), before);
    }

    /// 読めない現行ファイルを `{}` に倒すと `mcpOAuth` を消す。
    /// 壊れた状態を残さないため、書かずに失敗する
    #[test]
    fn switch_refuses_to_clobber_an_unreadable_credentials_file() {
        let home = TempHome::new("switch_refuses_to_clobber_an_unreadable_credentials_file");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&home.active(EMAIL_A, "taro")).unwrap();
        std::fs::write(home.paths().credentials, "{ this is not json").unwrap();

        assert!(store.switch_to(EMAIL_A, &Outgoing::NobodyLoggedIn).is_err());
        assert_eq!(
            std::fs::read_to_string(home.paths().credentials).unwrap(),
            "{ this is not json",
            "overwrote the broken file (a path that loses mcpOAuth)"
        );
    }

    /// **「読めない」を「まだログインしていない」に倒さない。**
    ///
    /// 壊れた JSON は失敗にしているのに、**存在するが読めない**（権限・
    /// ウイルス対策やバックアップの共有違反・I/O エラー）を「ファイルが無い」と
    /// 同じ扱いにすると、土台が `{}` になって `mcpOAuth` と未知のトップレベルキーを
    /// 丸ごと失う。許すのは `NotFound` だけ。
    ///
    /// 状況は **FILE_SHARE_DELETE だけを許した開きっぱなしのハンドル**で作る:
    /// 読みは共有違反で失敗するが rename での差し替えは通る ＝ 実際に上書きが
    /// 起きうる条件をそのまま再現できる（読みも rename も止まる `share_mode(0)`
    /// では「壊す経路」が作れない）
    #[test]
    fn switch_refuses_to_clobber_credentials_it_cannot_read() {
        use std::os::windows::fs::OpenOptionsExt;
        /// FILE_SHARE_DELETE のみ（読みは拒否し、rename での差し替えは許す）
        const FILE_SHARE_DELETE: u32 = 0x4;

        let home = TempHome::new("switch_refuses_to_clobber_credentials_it_cannot_read");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&home.active(EMAIL_A, "taro")).unwrap();
        let before = std::fs::read(home.paths().credentials).unwrap();

        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_DELETE)
            .open(home.paths().credentials)
            .unwrap();
        assert!(
            std::fs::read_to_string(home.paths().credentials).is_err(),
            "test precondition broken (the file was readable)"
        );
        let result = store.switch_to(EMAIL_A, &Outgoing::NobodyLoggedIn);
        drop(held);

        assert!(
            result.is_err(),
            "treated an unreadable file as logged-out and replaced it"
        );
        assert_eq!(
            std::fs::read(home.paths().credentials).unwrap(),
            before,
            "overwrote an unreadable file (a path that loses mcpOAuth and unknown keys)"
        );
    }

    /// 追従更新: 登録済みアカウントのトークンが更新されたら保管も更新する。
    /// refreshToken は使い捨てなので、追従しないと保管が腐って復元できなくなる
    #[test]
    fn sync_follows_a_rotated_refresh_token_for_a_registered_account() {
        let home = TempHome::new("sync_follows_a_rotated_refresh_token_for_a_registered_account");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&home.active(EMAIL_A, "taro")).unwrap();

        // claude がトークンを更新した（refreshToken が新しい値に置き換わる）
        home.write_credentials(&credentials_doc("access-a2", "refresh-a2"));
        assert!(
            store.sync_active(&home.active(EMAIL_A, "taro")).unwrap(),
            "did not follow"
        );

        assert_eq!(
            stored_oauth(&store, EMAIL_A),
            Some(oauth("access-a2", "refresh-a2")),
            "store still has the old token (switching would no longer restore it)"
        );
    }

    /// **追従更新は保管ロックでも待たない。**
    ///
    /// `sync_active` は claude 側のロックを `Duration::ZERO` で取る（ポーラーから
    /// 繰り返し呼ばれるので待てない）のに、続く保管ロックだけ
    /// [`STORE_LOCK_WAIT`] ぶん待っていた ＝ 別インスタンスが保管を書いている間、
    /// 1 秒周期のフッターポーラーが**1 ティックあたり最大 2 秒**止まり、
    /// アカウント行と版行の更新が遅れる。
    ///
    /// **待ち時間を詰めていないストア**（実運用と同じ [`STORE_LOCK_WAIT`]）で計るのが
    /// 要点で、詰めると「待たない」ことを見られない
    #[test]
    fn sync_does_not_wait_for_the_store_lock() {
        let home = TempHome::new("sync_does_not_wait_for_the_store_lock");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&home.active(EMAIL_A, "taro")).unwrap();
        let stored_before = std::fs::read(home.paths().store).unwrap();

        // 別インスタンスが保管を書いている状態
        let held = Lock::acquire(&home.paths().store_lock(), Duration::ZERO, LOCK_STALE).unwrap();
        let started = Instant::now();
        let result = store.sync_active(&home.active(EMAIL_A, "taro"));
        let waited = started.elapsed();
        drop(held);

        assert!(result.is_err(), "wrote despite failing to take the lock");
        assert!(
            waited < STORE_LOCK_WAIT / 2,
            "waited for the store lock ({waited:?}) = the poller would stall by that much every tick"
        );
        assert_eq!(
            std::fs::read(home.paths().store).unwrap(),
            stored_before,
            "wrote the store despite not holding the lock"
        );
    }

    /// 追従更新はラベルの変化（組織名が付いた等）も反映する
    #[test]
    fn sync_updates_the_stored_label() {
        let home = TempHome::new("sync_updates_the_stored_label");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&home.active(EMAIL_A, "taro")).unwrap();

        store
            .sync_active(&home.active(EMAIL_A, "taro · Acme, Inc."))
            .unwrap();

        assert_eq!(
            store.list(),
            vec![Account::new(EMAIL_A, "taro · Acme, Inc.")]
        );
    }

    /// **未登録のアカウントは保管しない。** 明示登録するまで認証情報を
    /// 勝手にコピーしない（ユーザーの決定）
    #[test]
    fn sync_never_stores_an_unregistered_account() {
        let home = TempHome::new("sync_never_stores_an_unregistered_account");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-b", "refresh-b"));

        assert!(!store.sync_active(&home.active(EMAIL_B, "hanako")).unwrap());
        // email を持たないアカウントも同じ（キーが無いので保管できない）
        assert!(!store.sync_active(&home.active("", "claude.ai")).unwrap());

        assert!(store.list().is_empty());
        assert!(
            !home.paths().store.exists(),
            "created the store file despite being unregistered"
        );
    }

    /// 追従更新は現行ファイルが読めないときも壊れない（次の機会に任せる）
    #[test]
    fn sync_leaves_the_store_intact_when_credentials_are_unreadable() {
        let home = TempHome::new("sync_leaves_the_store_intact_when_credentials_are_unreadable");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&home.active(EMAIL_A, "taro")).unwrap();

        // 書き換え途中（壊れた JSON）を読んだケース
        std::fs::write(home.paths().credentials, "{ partial").unwrap();
        assert!(!store.sync_active(&home.active(EMAIL_A, "taro")).unwrap());
        assert_eq!(
            stored_oauth(&store, EMAIL_A),
            Some(oauth("access-a", "refresh-a")),
            "corrupted the store with the unreadable value"
        );
    }

    /// 書き込みの前にロックを取っていること。claude が保持している間は
    /// 書かずに失敗し、現行ファイルを壊さない（有界時間で諦める）
    #[test]
    fn switch_fails_without_writing_while_another_holder_has_the_lock() {
        let home = TempHome::new("switch_fails_without_writing_while_another_holder_has_the_lock");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&home.active(EMAIL_A, "taro")).unwrap();
        home.write_credentials(&credentials_doc("access-b", "refresh-b"));
        let before = std::fs::read(home.paths().credentials).unwrap();

        // claude 相当の保持者（mkdir されたばかりなので stale ではない）
        let held = Lock::acquire(&home.paths().lock, Duration::ZERO, LOCK_STALE).unwrap();

        let short = home.store_with_short_wait();
        let started = Instant::now();
        assert!(short.switch_to(EMAIL_A, &Outgoing::NobodyLoggedIn).is_err(), "wrote without taking the lock");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "wait is not bounded: {:?}",
            started.elapsed()
        );
        assert_eq!(
            std::fs::read(home.paths().credentials).unwrap(),
            before,
            "wrote despite failing to acquire (leaves a broken state)"
        );
        // 登録も同じ（現行の認証情報をロック下で読む）
        assert!(short.register(&home.active(EMAIL_B, "hanako")).is_err());
        // 追従更新は待たずに諦める（ポーラーを止めない）
        assert!(!short.sync_active(&home.active(EMAIL_A, "taro")).unwrap());

        drop(held);
        // 解放後は通常どおり書ける
        short.switch_to(EMAIL_A, &Outgoing::NobodyLoggedIn).unwrap();
        assert_eq!(home.read_credentials()[OAUTH_KEY], oauth("access-a", "refresh-a"));
    }

    /// 書き終えたらロックを解放する（次の claude のトークン更新を待たせない）
    #[test]
    fn switch_releases_the_lock_afterwards() {
        let home = TempHome::new("switch_releases_the_lock_afterwards");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&home.active(EMAIL_A, "taro")).unwrap();
        assert!(!home.paths().lock.exists(), "register left the lock behind");

        store.switch_to(EMAIL_A, &Outgoing::NobodyLoggedIn).unwrap();
        assert!(!home.paths().lock.exists(), "switch left the lock behind");
    }

    /// **切替は「新しい持ち主」を返す。** 自分が書いた値なので確定しており、
    /// 呼び出し側は `claude auth status` の追いつき（1〜2 秒）を待たずに
    /// 次の操作の材料にできる。返る観測は書いた直後のファイルと一致する
    #[test]
    fn switch_returns_the_new_owner_with_a_fresh_view() {
        let home = TempHome::new("switch_returns_the_new_owner_with_a_fresh_view");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-b", "refresh-b"));
        store.register(&home.active(EMAIL_B, "hanako")).unwrap();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&home.active(EMAIL_A, "taro")).unwrap();

        let change = store
            .switch_to(EMAIL_B, &Outgoing::Known(home.active(EMAIL_A, "taro")))
            .unwrap();

        assert_eq!(
            change,
            AccountChange::Switched(home.active(EMAIL_B, "hanako")),
            "the post-switch owner was not returned as the settled value"
        );
    }

    /// **古い観測で巻き取ってはいけない**（バグの本体）。ccdesk が持ち主を判定した
    /// 後に認証情報が差し替わっていると、`active` に入っている email の保管へ
    /// **別アカウントの現行トークン**を書き込む。refreshToken は使い捨てなので、
    /// 保管された側は二度と復元できず、両者が同じ refreshToken を指すため
    /// どちらか一方を使った瞬間に他方も死ぬ。
    ///
    /// 検出できる（ロック下で指紋を読み直せば一致しない）ので、書かずに失敗させる
    #[test]
    fn switch_refuses_to_act_on_a_stale_view_of_the_active_account() {
        let home = TempHome::new("switch_refuses_to_act_on_a_stale_view_of_the_active_account");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-b", "refresh-b"));
        store.register(&home.active(EMAIL_B, "hanako")).unwrap();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&home.active(EMAIL_A, "taro")).unwrap();
        // ccdesk が「今は A」と判定した時点の観測
        let stale = home.active(EMAIL_A, "taro");
        // その後 B へ切り替わった（ccdesk 自身の切替でも、別端末の /login でも同じ）
        wait_for_a_new_mtime();
        home.write_credentials(&credentials_doc("access-b2", "refresh-b2"));

        let err = store
            .switch_to(EMAIL_B, &Outgoing::Known(stale.clone()))
            .expect_err("switch succeeded despite a stale observation");

        assert!(
            err.to_string().contains("try again"),
            "does not convey that retrying would work: {err}"
        );
        assert_eq!(
            stored_oauth(&store, EMAIL_A),
            Some(oauth("access-a", "refresh-a")),
            "A's stored entry was clobbered by B's token (A is unrecoverable)"
        );
        assert_eq!(
            home.read_credentials()[OAUTH_KEY],
            oauth("access-b2", "refresh-b2"),
            "rewrote the current credentials without confirming ownership"
        );
    }

    /// **同じアカウントのトークン更新では切替を止めない**（実機で出たバグの本体）。
    ///
    /// 指紋（mtime + サイズ）は「ファイルが動いたか」しか答えないが、動いている
    /// claude はトークン更新のたびにこのファイルを書く ＝ セッションを複数抱えて
    /// いると、メニューを開いてから押すまでの数秒で必ず動く。動いたことだけを
    /// 理由に中止していた頃は `changed since ccdesk last checked` で毎回弾かれた。
    ///
    /// 持ち主を判定し直して同じなら続行し、**新しい値を保管する**
    /// （巻き取りの目的そのもの: 使い捨ての refreshToken を落とさない）
    #[test]
    fn a_token_refresh_by_the_same_account_does_not_block_the_switch() {
        let home = TempHome::new("a_token_refresh_by_the_same_account_does_not_block_the_switch");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-b", "refresh-b"));
        store.register(&home.active(EMAIL_B, "hanako")).unwrap();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&home.active(EMAIL_A, "taro")).unwrap();
        // ccdesk が「今は A」と判定した時点の観測
        let seen = home.active(EMAIL_A, "taro");
        // その後 claude が A のトークンを更新した（持ち主は A のまま）
        wait_for_a_new_mtime();
        home.write_credentials(&credentials_doc("access-a2", "refresh-a2"));

        let switching = home.store_that_sees(Owner::LoggedIn(EMAIL_A.to_string()));
        assert_eq!(
            switching
                .switch_to(EMAIL_B, &Outgoing::Known(seen))
                .expect("a token refresh by the same account blocked the switch"),
            AccountChange::Switched(home.active(EMAIL_B, "hanako"))
        );
        assert_eq!(
            stored_oauth(&store, EMAIL_A),
            Some(oauth("access-a2", "refresh-a2")),
            "captured the token from before the refresh (A becomes unreachable)"
        );
        assert_eq!(
            home.read_credentials()[OAUTH_KEY],
            oauth("access-b", "refresh-b")
        );
    }

    /// **再判定の最中に動いても諦めない**（実機で出たバグの残り半分）。
    ///
    /// 再判定は子プロセス 1 つぶん（実測 ~370ms）かかるので、その窓に動いている
    /// claude のトークン更新が入るのは珍しくない。1 回動いただけで失敗にしていた
    /// 頃は、セッションを複数抱えた環境で押すたびに同じエラーになった。
    /// 収束するまで有界回数だけ取り直す（[`OWNER_CHECK_ATTEMPTS`]）
    #[test]
    fn a_rewrite_during_the_owner_check_is_retried_instead_of_failing() {
        let home = TempHome::new("a_rewrite_during_the_owner_check_is_retried_instead_of_failing");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-b", "refresh-b"));
        store.register(&home.active(EMAIL_B, "hanako")).unwrap();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&home.active(EMAIL_A, "taro")).unwrap();
        let seen = home.active(EMAIL_A, "taro");
        // 押す前に 1 度更新された（ここまでは指紋だけで分かる）
        home.write_credentials(&credentials_doc("access-a2", "refresh-a2"));

        let credentials = home.paths().credentials;
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let switching = home.store_that_checks(move || {
            // 1 回目の判定中に claude がもう一度トークンを更新する
            if counted.fetch_add(1, Ordering::Relaxed) == 0 {
                write_credentials_at(&credentials, &credentials_doc("access-a33", "refresh-a33"));
            }
            Owner::LoggedIn(EMAIL_A.to_string())
        });

        assert_eq!(
            switching
                .switch_to(EMAIL_B, &Outgoing::Known(seen))
                .expect("gave up on the first rewrite during the owner check"),
            AccountChange::Switched(home.active(EMAIL_B, "hanako"))
        );
        assert_eq!(calls.load(Ordering::Relaxed), 2, "did not re-check the owner");
        assert_eq!(
            stored_oauth(&store, EMAIL_A),
            Some(oauth("access-a33", "refresh-a33")),
            "captured a token from before the rewrite (A becomes unreachable)"
        );
        assert_eq!(
            home.read_credentials()[OAUTH_KEY],
            oauth("access-b", "refresh-b")
        );
    }

    /// **リトライは有界。** この再判定は claude と共有するロックを保持したまま
    /// 回るので、収束しない相手（書き続けている claude）を無制限に待つと
    /// [`LOCK_STALE`] を超えて claude 側にロックを奪われる ＝ 守っている区間が
    /// 守られなくなる。回数で打ち切り、**何も書かずに**失敗する
    #[test]
    fn an_owner_check_that_never_settles_gives_up_without_writing() {
        let home = TempHome::new("an_owner_check_that_never_settles_gives_up_without_writing");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-b", "refresh-b"));
        store.register(&home.active(EMAIL_B, "hanako")).unwrap();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&home.active(EMAIL_A, "taro")).unwrap();
        let seen = home.active(EMAIL_A, "taro");
        home.write_credentials(&credentials_doc("access-a2", "refresh-a2"));
        let stored_before = std::fs::read(home.paths().store).unwrap();

        let credentials = home.paths().credentials;
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let switching = home.store_that_checks(move || {
            // 判定のたびに書き換わる。**書くたびに長さを変える**ので、指紋の
            // mtime 側（同じ時刻刻みだと動いて見えない）に依らず必ず検出される
            // ＝ [`wait_for_a_new_mtime`] を挟まずに決定的にできる
            let n = counted.fetch_add(1, Ordering::Relaxed) + 2;
            let tail = "y".repeat(n);
            write_credentials_at(
                &credentials,
                &credentials_doc(&format!("access-a{tail}"), &format!("refresh-a{tail}")),
            );
            Owner::LoggedIn(EMAIL_A.to_string())
        });

        let err = switching
            .switch_to(EMAIL_B, &Outgoing::Known(seen))
            .expect_err("switched without ever seeing a settled file");

        assert!(
            err.to_string().contains("try again"),
            "does not convey that retrying would work: {err}"
        );
        assert_eq!(
            calls.load(Ordering::Relaxed),
            OWNER_CHECK_ATTEMPTS as usize,
            "the retry is not bounded (holds the shared lock past its stale threshold)"
        );
        assert_eq!(
            std::fs::read(home.paths().store).unwrap(),
            stored_before,
            "wrote the store without confirming the owner"
        );
        assert_eq!(
            home.read_credentials()[OAUTH_KEY],
            oauth("access-ayyyy", "refresh-ayyyy"),
            "overwrote the current credentials without confirming the owner"
        );
    }

    /// **どの経路で拒んだかが文言で分かること。**
    ///
    /// 4 経路が同じ 1 文を返していたので、実機で失敗してもログから原因を
    /// 切り分けられなかった（打つ手も経路ごとに違う: 待ち直す・ログインし直す・
    /// claude を起動できるようにする）。**同じ文言を 2 つ作らない**ことを固定する
    #[test]
    fn each_way_of_refusing_a_stale_view_says_which_one_it_was() {
        let path = Path::new("C:/home/.claude/.credentials.json");
        let reasons = [
            Unconfirmed::NoOwnerCheck,
            Unconfirmed::NoEmail,
            Unconfirmed::KeptChanging,
            Unconfirmed::Owner(Owner::LoggedIn(EMAIL_B.to_string())),
            Unconfirmed::Owner(Owner::LoggedOut),
            Unconfirmed::Owner(Owner::Unknown),
        ];
        let messages: Vec<String> = reasons
            .into_iter()
            .map(|reason| reason.into_error(path, EMAIL_A).to_string())
            .collect();

        for message in &messages {
            // 打つ手を書く方針は経路が増えても変えない
            assert!(message.contains("try again"), "no way forward: {message}");
            // トークンは載せないが、どのファイルの話かは必ず言う
            assert!(message.contains(".credentials.json"), "no path: {message}");
        }
        for (i, message) in messages.iter().enumerate() {
            assert!(
                !messages[..i].contains(message),
                "two paths share one message, so the log cannot tell them apart: {message}"
            );
        }

        // 実際の 2 経路が別の文言で出ることまで見る（型と振る舞いを繋ぐ）
        let home = TempHome::new("each_way_of_refusing_a_stale_view_says_which_one_it_was");
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        home.store()
            .register(&home.active(EMAIL_A, "taro"))
            .unwrap();
        let seen = home.active(EMAIL_A, "taro");
        home.write_credentials(&credentials_doc("access-b", "refresh-b"));

        let without_check = home
            .store()
            .register(&seen)
            .expect_err("registered on a stale view")
            .to_string();
        let wrong_owner = home
            .store_that_sees(Owner::LoggedIn(EMAIL_B.to_string()))
            .register(&seen)
            .expect_err("registered despite another owner")
            .to_string();
        assert_ne!(
            without_check, wrong_owner,
            "the two paths are indistinguishable in the log"
        );
        assert!(
            wrong_owner.contains(EMAIL_B) && wrong_owner.contains(EMAIL_A),
            "does not say who owns it now vs who was expected: {wrong_owner}"
        );
    }

    /// **持ち主が変わっていたら中止する**（守っている性質は変わらない）。
    /// 判定できない（CLI が起動できない・未ログイン）ときも同じ ＝
    /// 「分からない」を「同じ人だ」に倒さない
    #[test]
    fn a_switch_stops_when_the_owner_is_not_the_one_that_was_observed() {
        let home = TempHome::new("a_switch_stops_when_the_owner_is_not_the_one_that_was_observed");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-b", "refresh-b"));
        store.register(&home.active(EMAIL_B, "hanako")).unwrap();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&home.active(EMAIL_A, "taro")).unwrap();
        let seen = home.active(EMAIL_A, "taro");
        // 別端末で B へログインし直された
        wait_for_a_new_mtime();
        home.write_credentials(&credentials_doc("access-b2", "refresh-b2"));

        for owner in [
            Owner::LoggedIn(EMAIL_B.to_string()),
            Owner::LoggedOut,
            Owner::Unknown,
        ] {
            let switching = home.store_that_sees(owner.clone());
            assert!(
                switching
                    .switch_to(EMAIL_B, &Outgoing::Known(seen.clone()))
                    .is_err(),
                "switched despite the owner being {owner:?}"
            );
        }
        assert_eq!(
            stored_oauth(&store, EMAIL_A),
            Some(oauth("access-a", "refresh-a")),
            "A's stored entry was clobbered by B's token (A is unrecoverable)"
        );
        assert_eq!(
            home.read_credentials()[OAUTH_KEY],
            oauth("access-b2", "refresh-b2"),
            "rewrote the current credentials without confirming ownership"
        );
    }

    /// **2 つの保管が同じ refreshToken を指す状態を作らない**（[`other_holder`]）。
    ///
    /// 実機で「2 つのアカウントが同じトークンを持ち、どちらへ switch しても何も
    /// 起きない」状態が起きていた。refreshToken は使い捨てなので、その状態は
    /// 片方を使った瞬間に両方が死ぬ ＝ 書く前に止めるしかない
    #[test]
    fn storing_a_token_that_another_account_already_holds_is_refused() {
        let home = TempHome::new("storing_a_token_that_another_account_already_holds_is_refused");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-b", "refresh-b"));
        store.register(&home.active(EMAIL_B, "hanako")).unwrap();
        let before = std::fs::read(home.paths().store).unwrap();

        // 「今は A」という誤った判定のまま、実際は B のトークンを保管しようとする
        // （切替直後は `claude auth status` が前のアカウントを答えうる）
        let err = store
            .register(&home.active(EMAIL_A, "taro"))
            .expect_err("stored one refresh token under two accounts");
        assert!(
            err.to_string().contains(EMAIL_B) && err.to_string().contains(REFRESH_TOKEN_KEY),
            "does not say which entry collides: {err}"
        );
        assert!(
            !err.to_string().contains("refresh-b"),
            "the error message leaked a token: {err}"
        );
        assert_eq!(
            std::fs::read(home.paths().store).unwrap(),
            before,
            "wrote the colliding entry anyway"
        );

        // 同じ email への上書き（トークン更新の追従）は当然通る
        home.write_credentials(&credentials_doc("access-b2", "refresh-b2"));
        assert!(store.sync_active(&home.active(EMAIL_B, "hanako")).unwrap());
    }

    /// **保管が同一トークンになる経路そのものを塞げているか**（[`other_holder`]）。
    ///
    /// 実機で起きた状態を作る手順で見る: A → B へ切り替えた直後、`claude auth status`
    /// はまだ A を答えうる（[`other_holder`] のドキュメント）。その答えを材料に
    /// 追従更新が走ると **B のトークンを A の保管へ書く** ＝ 2 つの保管が同じ
    /// refreshToken を指し、どちらへ switch しても何も起きない状態になる。
    /// 指紋は「ccdesk 自身が書いた直後」なので動いておらず、そこでは止まらない ＝
    /// **保管そのものに聞く側でしか止められない**
    #[test]
    fn a_lagging_owner_cannot_store_the_new_accounts_token_under_the_old_email() {
        let home =
            TempHome::new("a_lagging_owner_cannot_store_the_new_accounts_token_under_the_old_email");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-b", "refresh-b"));
        store.register(&home.active(EMAIL_B, "hanako")).unwrap();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&home.active(EMAIL_A, "taro")).unwrap();

        store
            .switch_to(EMAIL_B, &Outgoing::Known(home.active(EMAIL_A, "taro")))
            .unwrap();

        // 切替直後にポーラーが「今も A」と答えた（指紋は自分が書いたままで動いていない）
        let lagging = home.active(EMAIL_A, "taro");
        let err = store
            .sync_active(&lagging)
            .expect_err("stored B's token under A (both accounts become unusable)");

        assert!(
            err.to_string().contains(EMAIL_B) && err.to_string().contains(REFRESH_TOKEN_KEY),
            "does not say which entry collides: {err}"
        );
        assert_eq!(
            stored_oauth(&store, EMAIL_A),
            Some(oauth("access-a", "refresh-a")),
            "A's stored entry now holds B's token"
        );
        assert_eq!(
            stored_oauth(&store, EMAIL_B),
            Some(oauth("access-b", "refresh-b")),
            "B's stored entry changed"
        );
    }

    /// 登録も同じ根。切替直後（ccdesk の表示がまだ前のアカウント）に
    /// `register current` を押すと、**現行 = B のトークンを A として保管**していた
    #[test]
    fn register_refuses_a_stale_view_of_the_active_account() {
        let home = TempHome::new("register_refuses_a_stale_view_of_the_active_account");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&home.active(EMAIL_A, "taro")).unwrap();
        let stale = home.active(EMAIL_A, "taro");
        wait_for_a_new_mtime();
        home.write_credentials(&credentials_doc("access-b", "refresh-b"));

        assert!(
            store.register(&stale).is_err(),
            "register succeeded despite a stale observation"
        );
        assert_eq!(
            stored_oauth(&store, EMAIL_A),
            Some(oauth("access-a", "refresh-a")),
            "A's stored entry was clobbered by B's token"
        );
    }

    /// 追従更新も同じ根だが、こちらは **失敗ではなく見送り**（次の機会がある）。
    /// `claude auth status`（子プロセス、数百 ms）が認証情報を読んだ後にトークンが
    /// 差し替わると、新しいアカウントのトークンを古い email の保管へ書きうる
    #[test]
    fn sync_skips_the_upsert_when_the_credentials_changed_after_the_fetch() {
        let home = TempHome::new("sync_skips_the_upsert_when_the_credentials_changed_after_the_fetch");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&home.active(EMAIL_A, "taro")).unwrap();
        // 「今は A」と判定した時点の観測（ポーラーは取得の前に指紋を読む）
        let stale = home.active(EMAIL_A, "taro");
        // 取得中に別アカウントへ切り替わった
        wait_for_a_new_mtime();
        home.write_credentials(&credentials_doc("access-b", "refresh-b"));

        assert!(
            !store.sync_active(&stale).unwrap(),
            "synced despite a stale observation"
        );
        assert_eq!(
            stored_oauth(&store, EMAIL_A),
            Some(oauth("access-a", "refresh-a")),
            "A's stored entry was clobbered by B's token"
        );
    }

    /// **保管の read-modify-write は 4 つの書き手すべてが同じロックを通る。**
    /// 1 つでも外れると多重起動で書き込みが消える: `unregister` が外れていると、
    /// 「インスタンス 1 が A を登録解除」と「インスタンス 2 が B を追従更新」が
    /// 重なったとき、後着が前着を無かったことにする（外した A が復活する /
    /// 新しい refreshToken が落ちて保管が死んだ値へ巻き戻る）
    #[test]
    fn every_writer_of_the_store_takes_the_store_lock() {
        let home = TempHome::new("every_writer_of_the_store_takes_the_store_lock");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&home.active(EMAIL_A, "taro")).unwrap();
        home.write_credentials(&credentials_doc("access-b", "refresh-b"));
        store.register(&home.active(EMAIL_B, "hanako")).unwrap();
        let store_before = std::fs::read(home.paths().store).unwrap();
        let credentials_before = std::fs::read(home.paths().credentials).unwrap();

        // 別インスタンス相当の保持者（保管ファイル用のロック。claude のものではない）
        let held = Lock::acquire(&home.paths().store_lock(), Duration::ZERO, LOCK_STALE).unwrap();
        let short = home.store_with_short_wait();

        assert!(short.register(&home.active(EMAIL_B, "hanako")).is_err(), "register");
        assert!(short.unregister(EMAIL_A).is_err(), "unregister");
        assert!(short.sync_active(&home.active(EMAIL_B, "hanako")).is_err(), "sync");
        // 切替は「出ていく側の巻き取り」で保管へ書くので、そこで諦める。
        // **現行の認証情報も書き換えない**（巻き取れないまま上書きするとログインが飛ぶ）
        assert!(
            short
                .switch_to(EMAIL_A, &Outgoing::Known(home.active(EMAIL_B, "hanako")))
                .is_err(),
            "switch's capture of the outgoing account"
        );

        assert_eq!(
            std::fs::read(home.paths().store).unwrap(),
            store_before,
            "wrote the store despite not holding the lock"
        );
        assert_eq!(
            std::fs::read(home.paths().credentials).unwrap(),
            credentials_before,
            "overwrote the current credentials without capturing the outgoing account"
        );

        drop(held);
        // 解放後は通常どおり書ける（＝ロックが理由で壊れているわけではない）
        short.unregister(EMAIL_A).unwrap();
        assert_eq!(short.list(), vec![Account::new(EMAIL_B, "hanako")]);
        assert!(
            !home.paths().store_lock().exists(),
            "left the store file's lock behind"
        );
    }

    /// `refreshToken` を持たない保管（手編集・旧版の残骸）は書き戻さない。
    /// 戻しても切替先へは行けず、**今のログインだけが壊れる**。
    /// キー集合は固定しない（将来 claude が増やすキーを落とさない）
    #[test]
    fn switch_refuses_a_stored_entry_without_a_refresh_token() {
        let home = TempHome::new("switch_refuses_a_stored_entry_without_a_refresh_token");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&home.active(EMAIL_A, "taro")).unwrap();
        // 手編集で refreshToken が落ちた保管
        std::fs::write(
            home.paths().store,
            serde_json::to_string_pretty(&json!({
                ACCOUNTS_KEY: {
                    EMAIL_A: { LABEL_KEY: "taro", CREDENTIALS_KEY: { "accessToken": "access-a" } }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let before = std::fs::read(home.paths().credentials).unwrap();

        let err = store
            .switch_to(EMAIL_A, &Outgoing::NobodyLoggedIn)
            .expect_err("switched using a copy that cannot be restored");

        assert!(
            err.to_string().contains(REFRESH_TOKEN_KEY),
            "does not say what is missing: {err}"
        );
        assert_eq!(std::fs::read(home.paths().credentials).unwrap(), before);
        // 未知のキーが増えた将来の写しは通す（キー集合を固定しない）
        let mut future = oauth("access-a", "refresh-a");
        future["someFutureKey"] = json!("value");
        assert!(usable_oauth(&future));
    }

    /// 書きかけの `.tmp` は **トークンを含む**ので放置しない。消すのは自分たちが
    /// 付ける形の名前で、かつ十分に古いものだけ（書いている最中の別インスタンスの
    /// tmp を消さない）
    #[test]
    fn leftover_tmp_files_are_reclaimed_at_startup() {
        assert!(is_leftover_tmp("accounts.json.1234-0.tmp", "accounts.json"));
        assert!(!is_leftover_tmp("accounts.json.tmp", "accounts.json"));
        assert!(!is_leftover_tmp("accounts.json.abc-0.tmp", "accounts.json"));
        assert!(!is_leftover_tmp("accounts.json.1234-0.tmp", ".credentials.json"));
        assert!(!is_leftover_tmp("accounts.json", "accounts.json"));

        let home = TempHome::new("leftover_tmp_files_are_reclaimed_at_startup");
        let paths = home.paths();
        let old = paths.store.with_file_name("accounts.json.4242-7.tmp");
        let fresh = paths
            .credentials
            .with_file_name(".credentials.json.4243-0.tmp");
        let other = paths.store.with_file_name("something-else.tmp");
        for path in [&old, &fresh, &other] {
            std::fs::write(path, "{}").unwrap();
        }
        // 古い側だけ mtime を閾値の外へ動かす（経過を待たずに固定する）
        let handle = std::fs::File::options().write(true).open(&old).unwrap();
        handle
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::SystemTime::now() - TMP_KEEP - Duration::from_secs(60)),
            )
            .unwrap();
        drop(handle);

        home.store().cleanup_leftover_tmp();

        assert!(!old.exists(), "did not reclaim the old tmp file (leaves a token behind)");
        assert!(fresh.exists(), "deleted a tmp file that might still be being written");
        assert!(other.exists(), "deleted an unrelated tmp file");
    }

    /// 認証情報ファイルの指紋: 書き換えと消滅を検出できる。
    /// **ポーラーの再取得契機と、観測がまだ有効かの照合が同じ値を使う**ので、
    /// この性質はここ 1 箇所で固定する
    #[test]
    fn the_credentials_fingerprint_detects_writes_and_deletion() {
        let home = TempHome::new("the_credentials_fingerprint_detects_writes_and_deletion");
        let path = home.paths().credentials;
        assert_eq!(credentials_fingerprint(&path), None, "a missing file should be None");

        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        let first = credentials_fingerprint(&path);
        assert!(first.is_some());

        // 長さが変わればサイズで検出できる（時刻の刻みに依存しない）
        home.write_credentials(&credentials_doc("access-a-longer", "refresh-a-longer"));
        let second = credentials_fingerprint(&path);
        assert_ne!(second, first, "did not detect the size change");

        // 同じ長さでも刻みを跨げば mtime で検出できる（トークン入れ替えがこの形）
        wait_for_a_new_mtime();
        home.write_credentials(&credentials_doc("access-b-longer", "refresh-b-longer"));
        assert_ne!(
            credentials_fingerprint(&path),
            second,
            "did not detect a same-size rewrite"
        );

        std::fs::remove_file(&path).unwrap();
        assert_eq!(credentials_fingerprint(&path), None, "did not detect deletion");
    }
}
