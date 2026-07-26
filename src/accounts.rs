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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use serde_json::{json, Value};

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

/// proper-lockfile のロック名は `<target>.lock`（拡張子の置換ではなく **付加**）。
/// 設定ホーム `~/.claude` に対しては `~/.claude.lock` になる
pub(crate) fn lock_path_for(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    target.with_file_name(name)
}

/// ロック取得の既定待ち時間。claude 側の保持はトークンエンドポイント 1 往復ぶんなので
/// これで足りる。**無限には待たない**（取れなければ失敗を返し、壊れた状態を残さない）
const LOCK_WAIT: Duration = Duration::from_secs(9);
/// mtime がこれより古いロックは死んだ保持者のものとして奪う（proper-lockfile の既定と同じ）
const LOCK_STALE: Duration = Duration::from_secs(10);
/// 取得の再試行間隔
const LOCK_RETRY: Duration = Duration::from_millis(100);
/// stale ロックを奪う回数の上限。奪った直後に別プロセスが取り直したなら
/// それは正当な競合なので、以降は通常の待ちに落とす（奪い合いで回り続けない）
const LOCK_MAX_STEALS: u32 = 3;

/// claude と共有する advisory lock（RAII）。
///
/// claude Code は OAuth トークン更新を npm `proper-lockfile` で保護している。
/// 合わせる必要があるプロトコル:
/// - ロックの実体は **ディレクトリ** `<target>.lock`。`mkdir` の原子性が mutex
/// - mtime が 10 秒より古いロックは stale とみなして奪ってよい
/// - 保持者は 5 秒ごとに mtime を touch して生存を示す
/// - claude は取れないとき 1〜2 秒のジッタ付きで 5 回リトライしてから諦める
///   （＝短時間の保持は協調的で、待たせても壊れない）
///
/// ロックを取らずに書くと、claude のトークン更新（読む → ネットワーク更新 → 保存を
/// `~/.claude.lock` の下で行う）と衝突し、**差し替えた認証情報が旧アカウントの
/// 更新済みトークンで上書きされる**。
///
/// # 解放は所有権を確認してから行う
///
/// 取得した瞬間の mtime を所有権の印として持ち、[`Drop`] では **それが今も
/// 一致するときだけ** `rmdir` する。無条件に消すと、自分のロックが stale 判定で
/// 奪われた後（奪取は rmdir → mkdir なので mtime が変わる）に **奪った側＝claude の
/// ロックを消してしまい**、トークン更新の最中に第三者が入れる状態を作る。
/// それはこのロックがまさに防ごうとしている上書きそのもので、使い捨ての
/// refreshToken が壊れるとログインは復旧不能になりうる。
///
/// 印に mtime を選んだ理由:
/// - **claude 側（proper-lockfile）も同じ基準で所有権を見ている。** 取得時の mtime と
///   現在の mtime が違えば "compromised" と判定する実装で、claude のバイナリにも
///   その痕跡（`mtimePrecision` / `ECOMPROMISED` / `onCompromised` /
///   `Unable to update lock within the stale threshold` の文字列）がある
/// - **ロックディレクトリの中に印のファイルは置けない。** 奪う側は `rmdir` で
///   消すが、非空ディレクトリの `rmdir` は `ENOTEMPTY` で失敗する（この定数も
///   claude のバイナリにある）。中身を置くと claude が stale ロックを回収できず、
///   トークン更新が永久に失敗しうる
///
/// mtime の分解能（NTFS は 100ns 刻み）より短い間隔で奪われると判別できないが、
/// 奪取は「mtime が 10 秒より古い」ことが前提なので実運用では起きない。
///
/// **mtime を更新するスレッドは持たない。** ここでの保持は小さなファイル 2 本の
/// 読み書き（ミリ秒）で、stale 閾値 10 秒に対して十分短い。仮に環境要因
/// （ウイルス対策のスキャン・スリープ復帰）で 10 秒を超えて奪われても、
/// 上の所有権確認があるので他者のロックを消すことはなく、こちらの書き込みが
/// 失敗するだけで済む（touch スレッドを足しても「奪われた後に消す」経路は
/// 消えないので、守りとしては所有権確認の方が単純かつ確実）
#[derive(Debug)] // 取れなかったことをテストで `expect_err` するため
struct Lock {
    path: PathBuf,
    /// 取得した瞬間の mtime＝所有権の印。取れなかった（None）ときは所有を
    /// 証明できないので解放しない（stale 化して誰かが奪うのに任せる。
    /// 他者のロックを消す危険より、10 秒待たせる方が軽い）
    mtime: Option<std::time::SystemTime>,
}

impl Lock {
    /// `wait` まで待って取る。`stale` より古いロックは奪う
    fn acquire(path: &Path, wait: Duration, stale: Duration) -> anyhow::Result<Self> {
        // ロックの置き場所が無いと mkdir は必ず失敗する。保管ファイル用のロックは
        // 初回起動（`~/.ccdesk` がまだ無い）で実際にこの状況になる
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let deadline = Instant::now() + wait;
        let mut steals = 0;
        loop {
            match std::fs::create_dir(path) {
                Ok(()) => {
                    return Ok(Self {
                        path: path.to_path_buf(),
                        mtime: lock_mtime(path),
                    })
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => {
                    return Err(anyhow!(
                        "could not create the lock at {}: {e}",
                        path.display()
                    ))
                }
            }
            // 死んだ保持者のロックは奪う。奪えたら即座に取り直しへ戻る
            // （待ち時間を消費させない: 誰も生きていないのだから待つ理由が無い）
            let stolen = steals < LOCK_MAX_STEALS
                && lock_age(path).is_some_and(|age| age >= stale)
                && std::fs::remove_dir(path).is_ok();
            if stolen {
                steals += 1;
                continue;
            }
            if Instant::now() >= deadline {
                // **打つ手まで書く。** 時計の巻き戻し・スリープ復帰・ネットワーク
                // ドライブの skew でロックの mtime が未来に付くと [`lock_age`] は
                // 永久に stale と判定しないので、"try again" は何度やっても通らない。
                // ロックの実体が空ディレクトリで、保持者が居なければ消してよいことは
                // ここでしか伝わらない（未ログイン行が `run /login` まで書くのと同じ方針）
                return Err(anyhow!(
                    "another process is holding the lock at {}; \
                     if no claude session and no other ccdesk window is running, \
                     this leftover lock is an empty directory and can be deleted",
                    path.display()
                ));
            }
            std::thread::sleep(LOCK_RETRY);
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // 所有権の確認（[`Lock`] の解説を参照）。奪われている・確認できないなら
        // 何もしない。ここで無条件に消すと他者のロックを外すことになる
        if self.mtime.is_some() && lock_mtime(&self.path) == self.mtime {
            let _ = std::fs::remove_dir(&self.path);
        }
    }
}

/// ロックの mtime（所有権の印）。無い・読めないときは None
fn lock_mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// ロックの経過時間。mtime が未来のとき（時刻のずれ）は None＝stale ではない扱い。
///
/// **未来の mtime を「十分古い」側に倒さない。** 未来に付くのは保持者の時計ではなく
/// ファイルシステム側の時刻がずれているときで、そうなると経過時間そのものが
/// 信用できない ＝ 生きている claude のロックを奪う判断材料にはできない。
/// 代わりに、取得できなかったときのエラー文が「消してよい」ことを案内する
/// （[`Lock::acquire`]）
fn lock_age(path: &Path) -> Option<Duration> {
    lock_mtime(path)?.elapsed().ok()
}

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

/// 起動時に回収する `.tmp` の古さ（[`AccountStore::cleanup_leftover_tmp`]）。
/// 書いている最中の別インスタンスの tmp を消さないため、十分に古いものだけを消す
const TMP_KEEP: Duration = Duration::from_secs(3600);

/// アカウント保管ストア。保管先とロックの位置は [`Paths`] で注入する
pub(crate) struct AccountStore {
    paths: Paths,
    lock_wait: Duration,
    lock_stale: Duration,
    store_lock_wait: Duration,
}

impl AccountStore {
    pub(crate) fn new(paths: Paths) -> Self {
        Self {
            paths,
            lock_wait: LOCK_WAIT,
            lock_stale: LOCK_STALE,
            store_lock_wait: STORE_LOCK_WAIT,
        }
    }

    /// 既定パスのストア。ホームが取れない環境では None
    pub(crate) fn detect() -> Option<Self> {
        Some(Self::new(Paths::detect()?))
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
        self.upsert(account, &oauth, true)
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

    /// 現行の認証情報をロック下で読んで保管する。ロックを取るのは、claude の
    /// トークン更新の途中（読む → ネットワーク → 保存）の値を保管しないため。
    /// 使い捨ての refreshToken を古い値で保管すると、そのアカウントは切替時に
    /// 復元できない
    fn capture_current(&self, active: &ActiveAccount) -> anyhow::Result<()> {
        let _lock = Lock::acquire(&self.paths.lock, self.lock_wait, self.lock_stale)?;
        // 観測が古いなら **別アカウントの現行トークンをこの email として保管しうる**
        // （切替直後の `register current` で実際に起きていた）。書かずに失敗させる
        if !self.still_current(active) {
            return Err(stale_active_error(&self.paths.credentials));
        }
        let oauth = read_oauth(&self.paths.credentials).ok_or_else(|| {
            anyhow!(
                "{} has no usable {OAUTH_KEY}: either no account is logged in, \
                 or claude keeps the credentials outside this file \
                 (OS credential manager), in which case ccdesk cannot store it",
                self.paths.credentials.display()
            )
        })?;
        self.upsert(&active.account, &oauth, false)?;
        Ok(())
    }

    /// 保管ファイルへ 1 件書く。`only_if_present` は追従更新用
    /// （ロックの下で在否を再確認するので、直前の登録解除と競合しない）
    fn upsert(
        &self,
        account: &Account,
        oauth: &Value,
        only_if_present: bool,
    ) -> anyhow::Result<bool> {
        let _guard = self.lock_store()?;
        let mut accounts = read_accounts(&self.paths.store);
        if only_if_present && !accounts.contains_key(&account.email) {
            return Ok(false);
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
    /// 1 つでも外れると多重起動で書き込みが消える（[`STORE_LOCK_WAIT`]）
    fn lock_store(&self) -> anyhow::Result<Lock> {
        Lock::acquire(
            &self.paths.store_lock(),
            self.store_lock_wait,
            self.lock_stale,
        )
    }

    /// 起動時の掃除: [`write_json_atomically`] が rename する前にプロセスが死ぬと、
    /// **トークン入りの `.tmp` が誰にも消されずに残る**（README が
    /// 「`accounts.json` は `.credentials.json` と同じ扱いをせよ」と案内している
    /// 対象の外にファイルが増える）。`update::cleanup_old_exe` と同じ
    /// 「次にプロセスを起こしたときに片付ける」方式で回収する。
    ///
    /// 消すのは **自分たちが付ける形の名前**（[`is_leftover_tmp`]）で、かつ十分に
    /// 古いもの（[`TMP_KEEP`]）だけ。今まさに書いている別インスタンスの tmp や、
    /// 無関係な `.tmp` を消さないため。失敗は無視する（掃除は次の起動でまた来る）
    pub(crate) fn cleanup_leftover_tmp(&self) {
        for target in [&self.paths.store, &self.paths.credentials] {
            let (Some(dir), Some(name)) = (target.parent(), target.file_name().and_then(|n| n.to_str()))
            else {
                continue;
            };
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                if !entry
                    .file_name()
                    .to_str()
                    .is_some_and(|file| is_leftover_tmp(file, name))
                {
                    continue;
                }
                let old = entry
                    .metadata()
                    .and_then(|md| md.modified())
                    .ok()
                    .and_then(|m| m.elapsed().ok())
                    .is_some_and(|age| age >= TMP_KEEP);
                if old {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
}

/// 観測が古かったときのエラー。**打つ手を書く**: もう一度メニューを開けば
/// ポーラーが取り直した新しい観測で通る（＝ ccdesk 側が誰の認証情報かを
/// 確かめられる状態に戻る）
fn stale_active_error(credentials: &Path) -> anyhow::Error {
    anyhow!(
        "{} changed since ccdesk last checked which account is logged in; \
         reopen the account menu and try again",
        credentials.display()
    )
}

/// `<target>.<pid>-<連番>.tmp` の形か（[`write_json_atomically`] が付ける名前）。
/// pid と連番の形まで見るのは、無関係な `.tmp`（claude や他ツールのもの）を
/// 消さないため
fn is_leftover_tmp(name: &str, target: &str) -> bool {
    let Some(rest) = name.strip_prefix(&format!("{target}.")) else {
        return false;
    };
    let Some((pid, seq)) = rest.strip_suffix(".tmp").and_then(|m| m.split_once('-')) else {
        return false;
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    digits(pid) && digits(seq)
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
        let _guard = self.lock_store()?;
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
        let capture = match outgoing {
            // 判断材料が古い（ccdesk が見た後に claude か別端末が書き換えた）。
            // 今の持ち主が誰かを言えないので、巻き取りも上書きもしない
            Outgoing::Known(active) if !self.still_current(active) => {
                return Err(stale_active_error(&self.paths.credentials))
            }
            // 同じアカウントへの「切替」は何もしない。書き戻すと、保管より新しい
            // 可能性のある現行トークンを古い写しで上書きしてしまい、使い捨ての
            // refreshToken が無効な値に戻って **今のログインを壊す**
            Outgoing::Known(active)
                if !active.account.email.is_empty() && active.account.email == email =>
            {
                return Ok(AccountChange::AlreadyActive)
            }
            // email を持たないアカウント（email を返さない認証方式）は保管の
            // キーが無いので巻き取れない。切替自体は通す
            Outgoing::Known(active) => Some(&active.account).filter(|a| !a.email.is_empty()),
            // 誰もログインしていないと観測できている ＝ 巻き取る対象が無い
            Outgoing::NobodyLoggedIn => None,
        };
        let mut current = self.current_document()?;
        if let Some(capture) = capture
            && let Some(oauth) = current.get(OAUTH_KEY).filter(|o| usable_oauth(o)).cloned()
        {
            // 未登録のアカウントには何もしない（`only_if_present`）。
            // 明示登録するまで認証情報をコピーしない規則は切替でも同じ
            self.upsert(capture, &oauth, true)?;
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
    /// 倒すと、読めなかっただけの `mcpOAuth` を消してしまう
    fn current_document(&self) -> anyhow::Result<Value> {
        let path = &self.paths.credentials;
        let Ok(text) = std::fs::read_to_string(path) else {
            return Ok(json!({})); // 未ログイン（ファイルが無い）
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
    value.is_object()
        && value
            .get(REFRESH_TOKEN_KEY)
            .and_then(|t| t.as_str())
            .is_some_and(|t| !t.is_empty())
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

/// tmp → rename で置く（読み手が書きかけの JSON を見ないため）。
/// tmp は同じディレクトリに作る（別ボリュームだと rename が失敗する）。
/// 名前は pid + 連番で一意にする（同じパスへの同時書き込みで tmp を共有しない）。
/// **rename 前に取り残された tmp は起動時に回収する**
/// （[`AccountStore::cleanup_leftover_tmp`]。中身はトークンなので放置しない）
fn write_json_atomically(path: &Path, value: &Value) -> anyhow::Result<()> {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| anyhow!("could not create {}: {e}", dir.display()))?;
    }
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}-{seq}.tmp", std::process::id()));
    let tmp = path.with_file_name(name);
    let text = serde_json::to_string_pretty(value)?;
    // **rename の前に中身をディスクへ確定させる。** rename 自体は NTFS の
    // メタデータジャーナルで守られるが、tmp の中身は守られない。電源断で
    // 0 バイトの `.credentials.json` が残ると、claude 本体から見て全アカウントの
    // ログインが飛ぶ（保管ファイル側なら全アカウントの保管が飛ぶ）。
    // 小さなファイル 1 本なので代償は小さい
    if let Err(e) = write_and_sync(&tmp, text.as_bytes()) {
        let _ = std::fs::remove_file(&tmp); // 中間ファイルを残さない
        return Err(anyhow!("could not write {}: {e}", tmp.display()));
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        anyhow!("could not replace {}: {e}", path.display())
    })
}

/// 書いて fsync する（[`write_json_atomically`] 用）
fn write_and_sync(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

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
            std::fs::write(
                self.paths().credentials,
                serde_json::to_string_pretty(value).unwrap(),
            )
            .unwrap();
        }

        pub(crate) fn read_credentials(&self) -> Value {
            read_json(&self.paths().credentials).expect("認証情報ファイルが読めない")
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
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
            "保管した claudeAiOauth が復元されていない"
        );
        assert_eq!(
            after["mcpOAuth"], with_b["mcpOAuth"],
            "mcpOAuth が保たれていない（MCP の認証が壊れる）"
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
            "トップレベルのキーが増減している"
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

        let stored = stored_oauth(&store, EMAIL_A).expect("保管されていない");
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
            "登録解除で現行の認証情報が変わっている（ログインを外してしまっている）"
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
            "usage.json が残っている（前アカウントの残量を表示してしまう）"
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
            "上書き直前のトークンを取り込めていない（A へ戻れなくなる）"
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
            "何もしていないのに切替として返している"
        );

        assert_eq!(
            home.read_credentials()[OAUTH_KEY],
            oauth("access-a4", "refresh-a4"),
            "現行トークンを古い写しで上書きしている（ログインが壊れる）"
        );
        assert!(
            home.paths().usage_cache.exists(),
            "アカウントは変わっていないのに使用率キャッシュを消している"
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
            "壊れたファイルを上書きしている（mcpOAuth を失う経路）"
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
            "追従していない"
        );

        assert_eq!(
            stored_oauth(&store, EMAIL_A),
            Some(oauth("access-a2", "refresh-a2")),
            "保管が古いトークンのまま（切替で復元できなくなる）"
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
            "未登録なのに保管ファイルを作っている"
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
            "読めなかった値で保管を壊している"
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
        assert!(short.switch_to(EMAIL_A, &Outgoing::NobodyLoggedIn).is_err(), "ロックを取らずに書いている");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "待ちが有界でない: {:?}",
            started.elapsed()
        );
        assert_eq!(
            std::fs::read(home.paths().credentials).unwrap(),
            before,
            "取れなかったのに書いている（壊れた状態を残している）"
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
        assert!(!home.paths().lock.exists(), "登録がロックを残している");

        store.switch_to(EMAIL_A, &Outgoing::NobodyLoggedIn).unwrap();
        assert!(!home.paths().lock.exists(), "切替がロックを残している");
    }

    /// 他者がロックを保持していたら待つ（claude 側も 1〜2 秒のジッタ付きで
    /// 5 回リトライするので、短時間の保持は協調的に待ち合わせられる）
    #[test]
    fn acquire_waits_until_the_holder_releases() {
        let home = TempHome::new("acquire_waits_until_the_holder_releases");
        let path = home.paths().lock;
        let held = Lock::acquire(&path, Duration::ZERO, LOCK_STALE).unwrap();
        let holder = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(held);
        });

        let started = Instant::now();
        let mine = Lock::acquire(&path, Duration::from_secs(5), LOCK_STALE)
            .expect("解放されたのに取れていない");
        assert!(
            started.elapsed() >= Duration::from_millis(150),
            "保持中に取れてしまっている: {:?}",
            started.elapsed()
        );
        drop(mine);
        holder.join().unwrap();
    }

    /// **奪われた後の Drop で他者のロックを消してはいけない。**
    /// 消すと、claude がトークン更新の最中（`~/.claude.lock` の下で
    /// 読む → ネットワーク更新 → 保存を行う）に第三者が入れる状態になり、
    /// このロック機構が防ごうとしている上書きそのものが起きる
    #[test]
    fn drop_keeps_a_lock_that_another_holder_took_over() {
        let home = TempHome::new("drop_keeps_a_lock_that_another_holder_took_over");
        let path = home.paths().lock;
        let mine = Lock::acquire(&path, Duration::ZERO, LOCK_STALE).unwrap();

        // 自分のロックが stale 判定で奪われた状況（他者の rmdir → mkdir）を作る。
        // 所有権の印は mtime なので、奪い直しが元と同じ刻（Windows のシステム
        // クロックは ~15ms 刻み）に収まると判別できない。実運用では奪取は
        // 取得から 10 秒以上経ってからしか起きないので衝突しないが、
        // テストは同じ刻を踏み得るため mtime が変わるまで作り直す
        let mtime_of = || std::fs::metadata(&path).unwrap().modified().unwrap();
        let mine_mtime = mtime_of();
        for _ in 0..500 {
            std::fs::remove_dir(&path).unwrap();
            std::fs::create_dir(&path).unwrap();
            if mtime_of() != mine_mtime {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_ne!(mtime_of(), mine_mtime, "奪取を作れていない（前提が崩れている）");

        drop(mine);

        assert!(
            path.exists(),
            "奪われた後の Drop が他者のロックを消している（claude のトークン更新を無防備にする）"
        );
    }

    /// stale 閾値より古いロックは奪う（保持者が死んで残った `.lock` で
    /// 永久に書けなくなるのを防ぐ）。閾値を注入して mtime の経過を待たずに固定する
    #[test]
    fn acquire_steals_a_stale_lock_but_not_a_fresh_one() {
        let home = TempHome::new("acquire_steals_a_stale_lock_but_not_a_fresh_one");
        let path = home.paths().lock;
        std::fs::create_dir(&path).unwrap(); // 死んだ保持者が残したロック

        // 新しいロックは奪わない: 有界時間で諦める
        let started = Instant::now();
        assert!(Lock::acquire(&path, Duration::from_millis(50), LOCK_STALE).is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(path.exists(), "諦めたのに他者のロックを消している");

        // mtime が閾値より古ければ奪える
        let stolen = Lock::acquire(&path, Duration::ZERO, Duration::ZERO)
            .expect("stale ロックを奪えていない");
        drop(stolen);
        assert!(!path.exists(), "解放されていない");
    }

    /// ロックが取れなかったときのエラーは **打つ手まで言う**。
    /// 時計のずれで mtime が未来に付いたロックは stale 判定に掛からず、
    /// 「もう一度試す」では永久に通らない（[`lock_age`]）。実体が空ディレクトリで
    /// 保持者が居なければ消してよいことは、この文面でしか伝わらない
    #[test]
    fn a_lock_we_cannot_take_says_how_to_recover() {
        let home = TempHome::new("a_lock_we_cannot_take_says_how_to_recover");
        let path = home.paths().lock;
        std::fs::create_dir(&path).unwrap();

        let err = Lock::acquire(&path, Duration::from_millis(20), LOCK_STALE)
            .expect_err("取れてしまっている")
            .to_string();

        assert!(
            err.contains(&path.display().to_string()),
            "どのロックか分からない: {err}"
        );
        assert!(
            err.contains("empty directory") && err.contains("deleted"),
            "打つ手（消してよいこと）が書かれていない: {err}"
        );
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
            "切替後の持ち主が確定値で返っていない"
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
            .expect_err("古い観測のまま切替が通っている");

        assert!(
            err.to_string().contains("try again"),
            "やり直せることが伝わらない: {err}"
        );
        assert_eq!(
            stored_oauth(&store, EMAIL_A),
            Some(oauth("access-a", "refresh-a")),
            "A の保管が B のトークンで潰れている（A は復旧不能）"
        );
        assert_eq!(
            home.read_credentials()[OAUTH_KEY],
            oauth("access-b2", "refresh-b2"),
            "確認できていないのに現行の認証情報を書き換えている"
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
            "古い観測のまま登録が通っている"
        );
        assert_eq!(
            stored_oauth(&store, EMAIL_A),
            Some(oauth("access-a", "refresh-a")),
            "A の保管が B のトークンで潰れている"
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
            "古い観測のまま追従更新している"
        );
        assert_eq!(
            stored_oauth(&store, EMAIL_A),
            Some(oauth("access-a", "refresh-a")),
            "A の保管が B のトークンで潰れている"
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

        assert!(short.register(&home.active(EMAIL_B, "hanako")).is_err(), "登録");
        assert!(short.unregister(EMAIL_A).is_err(), "登録解除");
        assert!(short.sync_active(&home.active(EMAIL_B, "hanako")).is_err(), "追従更新");
        // 切替は「出ていく側の巻き取り」で保管へ書くので、そこで諦める。
        // **現行の認証情報も書き換えない**（巻き取れないまま上書きするとログインが飛ぶ）
        assert!(
            short
                .switch_to(EMAIL_A, &Outgoing::Known(home.active(EMAIL_B, "hanako")))
                .is_err(),
            "切替の巻き取り"
        );

        assert_eq!(
            std::fs::read(home.paths().store).unwrap(),
            store_before,
            "ロックを取れていないのに保管を書いている"
        );
        assert_eq!(
            std::fs::read(home.paths().credentials).unwrap(),
            credentials_before,
            "巻き取れないまま現行の認証情報を上書きしている"
        );

        drop(held);
        // 解放後は通常どおり書ける（＝ロックが理由で壊れているわけではない）
        short.unregister(EMAIL_A).unwrap();
        assert_eq!(short.list(), vec![Account::new(EMAIL_B, "hanako")]);
        assert!(
            !home.paths().store_lock().exists(),
            "保管ファイルのロックを残している"
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
            .expect_err("戻せない写しで切り替えている");

        assert!(
            err.to_string().contains(REFRESH_TOKEN_KEY),
            "何が足りないか分からない: {err}"
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

        assert!(!old.exists(), "古い tmp を回収していない（トークンが残る）");
        assert!(fresh.exists(), "書いている最中かもしれない tmp を消している");
        assert!(other.exists(), "無関係な tmp を消している");
    }

    /// 認証情報ファイルの指紋: 書き換えと消滅を検出できる。
    /// **ポーラーの再取得契機と、観測がまだ有効かの照合が同じ値を使う**ので、
    /// この性質はここ 1 箇所で固定する
    #[test]
    fn the_credentials_fingerprint_detects_writes_and_deletion() {
        let home = TempHome::new("the_credentials_fingerprint_detects_writes_and_deletion");
        let path = home.paths().credentials;
        assert_eq!(credentials_fingerprint(&path), None, "無いファイルは None");

        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        let first = credentials_fingerprint(&path);
        assert!(first.is_some());

        // 長さが変わればサイズで検出できる（時刻の刻みに依存しない）
        home.write_credentials(&credentials_doc("access-a-longer", "refresh-a-longer"));
        let second = credentials_fingerprint(&path);
        assert_ne!(second, first, "サイズの変化を検出できていない");

        // 同じ長さでも刻みを跨げば mtime で検出できる（トークン入れ替えがこの形）
        wait_for_a_new_mtime();
        home.write_credentials(&credentials_doc("access-b-longer", "refresh-b-longer"));
        assert_ne!(
            credentials_fingerprint(&path),
            second,
            "同サイズの書き換えを検出できていない"
        );

        std::fs::remove_file(&path).unwrap();
        assert_eq!(credentials_fingerprint(&path), None, "消滅を検出できていない");
    }
}
