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
use std::sync::{Mutex, PoisonError};
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
                return Err(anyhow!(
                    "another process is holding the Claude credentials lock ({}); try again",
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

/// ロックの経過時間。mtime が未来のとき（時刻のずれ）は None＝stale ではない扱い
fn lock_age(path: &Path) -> Option<Duration> {
    lock_mtime(path)?.elapsed().ok()
}

/// 保管ファイルの read-modify-write を直列化するプロセス内ロック。
/// UI スレッドの登録操作とポーラーの追従更新が同時に走るため、
/// これが無いと片方の書き込みが消える
static STORE_LOCK: Mutex<()> = Mutex::new(());

/// アカウント保管ストア。保管先とロックの位置は [`Paths`] で注入する
pub(crate) struct AccountStore {
    paths: Paths,
    lock_wait: Duration,
    lock_stale: Duration,
}

impl AccountStore {
    pub(crate) fn new(paths: Paths) -> Self {
        Self {
            paths,
            lock_wait: LOCK_WAIT,
            lock_stale: LOCK_STALE,
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
    pub(crate) fn sync_active(&self, account: &Account) -> anyhow::Result<bool> {
        if account.email.is_empty() || !self.is_registered(&account.email) {
            return Ok(false);
        }
        // 追従更新はポーラーから繰り返し呼ばれるので **待たない**。待つと
        // アカウント行の更新がロックの待ち時間ぶん止まる。取り逃しても
        // 認証ファイルの変化と周期フォールバックで次の機会が来る
        let Ok(_lock) = Lock::acquire(&self.paths.lock, Duration::ZERO, self.lock_stale) else {
            return Ok(false);
        };
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

    /// 現行の認証情報をロック下で読んで保管する。ロックを取るのは、claude の
    /// トークン更新の途中（読む → ネットワーク → 保存）の値を保管しないため。
    /// 使い捨ての refreshToken を古い値で保管すると、そのアカウントは切替時に
    /// 復元できない
    fn capture_current(&self, account: &Account) -> anyhow::Result<()> {
        let _lock = Lock::acquire(&self.paths.lock, self.lock_wait, self.lock_stale)?;
        let oauth = read_oauth(&self.paths.credentials).ok_or_else(|| {
            anyhow!(
                "no {OAUTH_KEY} in {} (not logged in?)",
                self.paths.credentials.display()
            )
        })?;
        self.upsert(account, &oauth, false)?;
        Ok(())
    }

    /// 保管ファイルへ 1 件書く。`only_if_present` は追従更新用
    /// （プロセス内ロックの下で在否を再確認するので、直前の登録解除と競合しない）
    fn upsert(
        &self,
        account: &Account,
        oauth: &Value,
        only_if_present: bool,
    ) -> anyhow::Result<bool> {
        let _guard = STORE_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
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
            .map(|(email, entry)| {
                let label = entry
                    .get(LABEL_KEY)
                    .and_then(|l| l.as_str())
                    .filter(|l| !l.is_empty())
                    // ラベルが失われていても空行にはしない（識別子で代替する）
                    .unwrap_or(email);
                Account::new(email.clone(), label)
            })
            .collect()
    }

    /// 登録: 現行の認証情報の `claudeAiOauth` を email をキーに保管する
    pub(crate) fn register(&self, account: &Account) -> anyhow::Result<()> {
        if account.email.is_empty() {
            // 表示ラベルで代用してはいけない（同一性の判定に表示ロジックが混ざる）
            return Err(anyhow!(
                "this account has no email, so there is no stable key to store it under"
            ));
        }
        self.capture_current(account)
    }

    /// 登録解除: 保管を消すだけ。**ログイン自体は外さない**
    /// （現行の `.credentials.json` には触らない）
    pub(crate) fn unregister(&self, email: &str) -> anyhow::Result<()> {
        let _guard = STORE_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut accounts = read_accounts(&self.paths.store);
        if accounts.remove(email).is_none() {
            return Ok(()); // 既に無い＝目的は達成されている
        }
        write_json_atomically(&self.paths.store, &json!({ ACCOUNTS_KEY: accounts }))
    }

    /// 切替: 保管した `claudeAiOauth` を現行ファイルへ書き戻す。
    /// `mcpOAuth` と未知のトップレベルキーは保つ（[`OAUTH_KEY`] 参照）。
    ///
    /// `active` は今ログイン中のアカウント（分からなければ None）。**出ていく
    /// アカウントの認証情報を、上書きする前に同じロックの下で保管へ取り込む**:
    /// 追従更新（[`AccountStore::sync_active`]）はポーリング契機なので直前の
    /// トークン更新を取り逃す窓があり、使い捨ての refreshToken をそこで落とすと
    /// そのアカウントには戻れなくなる
    pub(crate) fn switch_to(&self, email: &str, active: Option<&Account>) -> anyhow::Result<()> {
        // 同じアカウントへの「切替」は何もしない。書き戻すと、保管より新しい
        // 可能性のある現行トークンを古い写しで上書きしてしまい、使い捨ての
        // refreshToken が無効な値に戻って **今のログインを壊す**
        if active.is_some_and(|a| !a.email.is_empty() && a.email == email) {
            return Ok(());
        }
        // 保管の読みは **意図的にロックの外**。`~/.claude.lock` が守るのは claude と
        // 共有する認証情報ファイルで、こちらの保管ファイルはその対象ではない
        // （ロック下に入れると、claude の保持時間ぶん自分の読みも待つことになる）。
        // 許容している穴: ccdesk を複数起動していると、読んだ後に別インスタンスが
        // `unregister` する窓がある。書き込みは tmp + rename で原子的なので、
        // 最悪でも「登録解除したはずのアカウントに切り替わる」だけでファイルは壊れない
        let stored = read_accounts(&self.paths.store)
            .get(email)
            .and_then(|entry| entry.get(CREDENTIALS_KEY))
            .filter(|c| c.is_object())
            .cloned()
            .ok_or_else(|| anyhow!("no stored credentials for {email}"))?;
        let _lock = Lock::acquire(&self.paths.lock, self.lock_wait, self.lock_stale)?;
        let mut current = self.current_document()?;
        if let Some(outgoing) = active.filter(|a| !a.email.is_empty())
            && let Some(oauth) = current.get(OAUTH_KEY).filter(|o| o.is_object()).cloned()
        {
            // 未登録のアカウントには何もしない（`only_if_present`）。
            // 明示登録するまで認証情報をコピーしない規則は切替でも同じ
            self.upsert(outgoing, &oauth, true)?;
        }
        current[OAUTH_KEY] = stored;
        write_json_atomically(&self.paths.credentials, &current)?;
        // 使用率キャッシュは **どのアカウントの数字か記録していない**（statusline へ
        // 渡される公式 JSON にアカウント情報が無いので、識別子を後から足せない）。
        // stale 判定は 10 分経過のみなので、消さないと切替後も前アカウントの残量を
        // 最大 10 分表示して嘘になる
        let _ = std::fs::remove_file(&self.paths.usage_cache);
        Ok(())
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

/// 認証情報ファイルの `claudeAiOauth`（オブジェクトでなければ無い扱い）
fn read_oauth(path: &Path) -> Option<Value> {
    let value = read_json(path)?;
    value.get(OAUTH_KEY).filter(|o| o.is_object()).cloned()
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
/// 名前は pid + 連番で一意にする（同じパスへの同時書き込みで tmp を共有しない）
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
    std::fs::write(&tmp, text).map_err(|e| anyhow!("could not write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp); // 中間ファイルを残さない
        anyhow!("could not replace {}: {e}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMAIL_A: &str = "taro@example.com";
    const EMAIL_B: &str = "hanako@example.com";

    /// テスト専用の擬似ホーム。**実ユーザーの `~/.claude` / `~/.ccdesk` を
    /// 絶対に触らない**ための境界で、パスは全て [`Paths`] 経由で注入する。
    /// Drop で丸ごと消すので、アサート失敗でパニックしても残らない
    struct TempHome(PathBuf);

    impl TempHome {
        /// パスはテスト名 + pid + 連番で一意にする（並列実行・別チェックアウトの
        /// 同時実行と衝突させない。Drop がディレクトリごと消すので共有は事故になる）
        fn new(test: &str) -> Self {
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
        fn paths(&self) -> Paths {
            let claude = self.0.join(".claude");
            Paths {
                store: self.0.join(".ccdesk").join("accounts.json"),
                credentials: claude.join(".credentials.json"),
                lock: lock_path_for(&claude),
                usage_cache: self.0.join(".ccdesk").join("usage.json"),
            }
        }

        fn store(&self) -> AccountStore {
            AccountStore::new(self.paths())
        }

        /// 待ち時間を詰めたストア（ロック競合を有界時間でテストするため）
        fn store_with_short_wait(&self) -> AccountStore {
            let mut store = self.store();
            store.lock_wait = Duration::from_millis(50);
            store
        }

        fn write_credentials(&self, value: &Value) {
            std::fs::write(
                self.paths().credentials,
                serde_json::to_string_pretty(value).unwrap(),
            )
            .unwrap();
        }

        fn read_credentials(&self) -> Value {
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
    fn credentials_doc(access: &str, refresh: &str) -> Value {
        json!({
            "mcpOAuth": {
                "linear-server": { "accessToken": "mcp-token", "expiresAt": 1_800_000_000_u64 }
            },
            OAUTH_KEY: oauth(access, refresh),
        })
    }

    fn oauth(access: &str, refresh: &str) -> Value {
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

    /// 保管された `claudeAiOauth`（トークン比較用）
    fn stored_oauth(store: &AccountStore, email: &str) -> Option<Value> {
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
        store.register(&Account::new(EMAIL_A, "taro")).unwrap();

        // B へログインし直した状態（mcpOAuth は claude 側が更新した別の値）
        let mut with_b = credentials_doc("access-b", "refresh-b");
        with_b["mcpOAuth"] = json!({ "notion": { "accessToken": "mcp-notion" } });
        home.write_credentials(&with_b);

        store.switch_to(EMAIL_A, None).unwrap();

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
        store.register(&Account::new(EMAIL_A, "taro")).unwrap();

        let mut future = credentials_doc("access-b", "refresh-b");
        future["someFutureKey"] = json!({ "nested": [1, 2, 3] });
        future["anotherKey"] = json!("value");
        home.write_credentials(&future);

        store.switch_to(EMAIL_A, None).unwrap();

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
        store.register(&Account::new(EMAIL_A, "taro")).unwrap();

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
        assert!(store.register(&Account::new("", "claude.ai")).is_err());
        assert!(store.list().is_empty());
    }

    /// 未ログイン（`claudeAiOauth` が無い）状態では登録しても保管しない
    #[test]
    fn register_fails_without_current_credentials() {
        let home = TempHome::new("register_fails_without_current_credentials");
        let store = home.store();
        assert!(store.register(&Account::new(EMAIL_A, "taro")).is_err());
        home.write_credentials(&json!({ "mcpOAuth": {} }));
        assert!(store.register(&Account::new(EMAIL_A, "taro")).is_err());
        assert!(store.list().is_empty());
    }

    /// 一覧は email をキーに保管され、ラベルも保つ
    #[test]
    fn list_returns_stored_accounts_by_email() {
        let home = TempHome::new("list_returns_stored_accounts_by_email");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&Account::new(EMAIL_A, "taro")).unwrap();
        home.write_credentials(&credentials_doc("access-b", "refresh-b"));
        store
            .register(&Account::new(EMAIL_B, "hanako · Acme, Inc."))
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
        store.register(&Account::new(EMAIL_A, "taro")).unwrap();
        home.write_credentials(&credentials_doc("access-b", "refresh-b"));
        store.register(&Account::new(EMAIL_B, "hanako")).unwrap();
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
        store.register(&Account::new(EMAIL_A, "taro")).unwrap();
        std::fs::write(home.paths().usage_cache, r#"{"written_at":1}"#).unwrap();

        store.switch_to(EMAIL_A, None).unwrap();

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
        store.register(&Account::new(EMAIL_A, "taro")).unwrap();
        std::fs::remove_file(home.paths().credentials).unwrap();

        store.switch_to(EMAIL_A, None).unwrap();

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
        store.register(&Account::new(EMAIL_B, "hanako")).unwrap();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&Account::new(EMAIL_A, "taro")).unwrap();
        // A で作業してトークンが更新された（追従更新はまだ走っていない）
        home.write_credentials(&credentials_doc("access-a3", "refresh-a3"));

        store
            .switch_to(EMAIL_B, Some(&Account::new(EMAIL_A, "taro")))
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
        store.register(&Account::new(EMAIL_A, "taro")).unwrap();
        // 未登録の B でログイン中に A へ切り替える
        home.write_credentials(&credentials_doc("access-b", "refresh-b"));

        store
            .switch_to(EMAIL_A, Some(&Account::new(EMAIL_B, "hanako")))
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
        store.register(&Account::new(EMAIL_A, "taro")).unwrap();
        home.write_credentials(&credentials_doc("access-a4", "refresh-a4"));
        std::fs::write(home.paths().usage_cache, r#"{"written_at":1}"#).unwrap();

        store
            .switch_to(EMAIL_A, Some(&Account::new(EMAIL_A, "taro")))
            .unwrap();

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

        assert!(store.switch_to(EMAIL_B, None).is_err());
        assert_eq!(std::fs::read(home.paths().credentials).unwrap(), before);
    }

    /// 読めない現行ファイルを `{}` に倒すと `mcpOAuth` を消す。
    /// 壊れた状態を残さないため、書かずに失敗する
    #[test]
    fn switch_refuses_to_clobber_an_unreadable_credentials_file() {
        let home = TempHome::new("switch_refuses_to_clobber_an_unreadable_credentials_file");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&Account::new(EMAIL_A, "taro")).unwrap();
        std::fs::write(home.paths().credentials, "{ this is not json").unwrap();

        assert!(store.switch_to(EMAIL_A, None).is_err());
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
        let account = Account::new(EMAIL_A, "taro");
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&account).unwrap();

        // claude がトークンを更新した（refreshToken が新しい値に置き換わる）
        home.write_credentials(&credentials_doc("access-a2", "refresh-a2"));
        assert!(store.sync_active(&account).unwrap(), "追従していない");

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
        store.register(&Account::new(EMAIL_A, "taro")).unwrap();

        store
            .sync_active(&Account::new(EMAIL_A, "taro · Acme, Inc."))
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

        assert!(!store.sync_active(&Account::new(EMAIL_B, "hanako")).unwrap());
        // email を持たないアカウントも同じ（キーが無いので保管できない）
        assert!(!store.sync_active(&Account::new("", "claude.ai")).unwrap());

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
        let account = Account::new(EMAIL_A, "taro");
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&account).unwrap();

        // 書き換え途中（壊れた JSON）を読んだケース
        std::fs::write(home.paths().credentials, "{ partial").unwrap();
        assert!(!store.sync_active(&account).unwrap());
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
        store.register(&Account::new(EMAIL_A, "taro")).unwrap();
        home.write_credentials(&credentials_doc("access-b", "refresh-b"));
        let before = std::fs::read(home.paths().credentials).unwrap();

        // claude 相当の保持者（mkdir されたばかりなので stale ではない）
        let held = Lock::acquire(&home.paths().lock, Duration::ZERO, LOCK_STALE).unwrap();

        let short = home.store_with_short_wait();
        let started = Instant::now();
        assert!(short.switch_to(EMAIL_A, None).is_err(), "ロックを取らずに書いている");
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
        assert!(short.register(&Account::new(EMAIL_B, "hanako")).is_err());
        // 追従更新は待たずに諦める（ポーラーを止めない）
        assert!(!short.sync_active(&Account::new(EMAIL_A, "taro")).unwrap());

        drop(held);
        // 解放後は通常どおり書ける
        short.switch_to(EMAIL_A, None).unwrap();
        assert_eq!(home.read_credentials()[OAUTH_KEY], oauth("access-a", "refresh-a"));
    }

    /// 書き終えたらロックを解放する（次の claude のトークン更新を待たせない）
    #[test]
    fn switch_releases_the_lock_afterwards() {
        let home = TempHome::new("switch_releases_the_lock_afterwards");
        let store = home.store();
        home.write_credentials(&credentials_doc("access-a", "refresh-a"));
        store.register(&Account::new(EMAIL_A, "taro")).unwrap();
        assert!(!home.paths().lock.exists(), "登録がロックを残している");

        store.switch_to(EMAIL_A, None).unwrap();
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
}
