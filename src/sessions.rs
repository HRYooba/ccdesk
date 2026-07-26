//! セッション一覧の永続ストア（ドメイン層。UI は含まない）。
//!
//! **`~/.ccdesk/sessions.json` が一覧の正本。** 前景起動（`claude --session-id <uuid>`）は
//! `~/.claude/jobs/*/state.json` に痕跡を残さないので、「どのセッションが存在するか」を
//! ccdesk 自身が持つ必要がある。移行の全体像は `docs/foreground-migration.md`。
//!
//! **[`crate::session`]（PTY のクライアント）とは別物**: あちらは「今開いている端末」、
//! ここは「一覧に載る行」。プロセスが死んでも行は残る（最後に観測した状態のまま）。
//!
//! パスは引数で受ける（[`SessionStore::new`]）。理由は [`crate::accounts`] と同じで、
//! テストが実ユーザーの `~/.ccdesk` を絶対に触らないため。
//!
//! **共有ファイルへの安全な書き込みは lib 側の 1 実装を使う**（advisory lock と
//! tmp → rename）。ここが持つのは「どのファイルをどのロックで守り、どれだけ待つか」と
//! 「一覧をどうマージするか」だけで、書き方そのものは持たない。

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};

use ccdesk::{lock_path_for, write_json_atomically, Lock, LOCK_STALE};

/// 保管ファイルのトップレベルキー（`{"sessions": [ … ]}`）
const SESSIONS_KEY: &str = "sessions";
/// 行 1 件のキー。**読みと書きで同じ定数を使う**（片側だけ直した状態を作らない）
const ID_KEY: &str = "session_id";
const CWD_KEY: &str = "cwd";
const TITLE_KEY: &str = "title";
const TITLE_SOURCE_KEY: &str = "title_source";
const LAST_STATE_KEY: &str = "last_state";
const PINNED_KEY: &str = "pinned";
const LAST_OPENED_AT_KEY: &str = "last_opened_at";
const CREATED_AT_KEY: &str = "created_at";
const UPDATED_AT_KEY: &str = "updated_at";

/// 保管ファイルの read-modify-write を直列化するロックの待ち時間。
///
/// **プロセス内 Mutex では足りない。** ccdesk は複数起動でき `sessions.json` は
/// 共有なので、「インスタンス 1 のピン留め」と「インスタンス 2 の状態更新」が
/// 重なると後着が前着を無かったことにする（[`merge_sessions`] が守る不変条件は、
/// 読みと書きの間に他インスタンスの書き込みが挟まらないことが前提）。
///
/// 長さの判断は [`crate::accounts`] の保管ロックと同じ: 守る区間は小さなファイル
/// 1 本の読み書きだけ（ネットワークも子プロセスも無い）なので短くてよく、
/// **無限には待たない**（取れなければ書かずに諦め、次の保存でもう一度載せに行く）
const STORE_LOCK_WAIT: Duration = Duration::from_secs(2);

/// セッションの identity（claude の `sessionId` = UUID）。
///
/// **newtype にしてある理由**: 移行前の一覧は `short`（jobs ディレクトリ名）で、
/// 移行後は `sessionId` になる。どちらも素の `String` だと**半分だけ直しても
/// コンパイルが通る** ＝ 「jobs の short をセッション ID として渡す」ような
/// 取り違えが型で止まらない。
///
/// **`Deref<Target = str>` は実装しない。** 実装すると `&SessionId` が `&str` として
/// 素通りしてしまい、この型を作った目的（暗黙の混同を止める）が消える。
/// 文字列が要る場所は [`Self::as_str`] で明示的に降ろす
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct SessionId(String);

impl SessionId {
    pub(crate) fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// 空の ID は行の identity になれない（読みで捨てる判断に使う）
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 表示名がどこから来たか。
///
/// **優先順は CLI 本体と同じ** `customTitle` > `aiTitle` > `lastPrompt` > `firstPrompt`
/// で、ccdesk のリネームは `customTitle` の位置（＝ transcript の `custom-title` 行）に
/// 入る。正本は `docs/foreground-migration.md` の title の行。[`Self::Derived`] は
/// 上のどれでもない場合（cwd 等から組んだ名前・読めなかった保存値）で、
/// **未知の保存値もここへ倒す**
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TitleSource {
    /// ユーザーが付けた名前（ccdesk のリネーム）
    Custom,
    /// claude が生成した名前
    Ai,
    /// 最後のプロンプト
    LastPrompt,
    /// 最初のプロンプト
    FirstPrompt,
    /// 上のどれでもない（既定）
    #[default]
    Derived,
}

impl TitleSource {
    /// 保存表記。**読み（[`Self::parse`]）と対で持つ**ので、増やしたときに
    /// 片側だけ直した状態にならない
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Custom => "custom",
            Self::Ai => "ai",
            Self::LastPrompt => "last_prompt",
            Self::FirstPrompt => "first_prompt",
            Self::Derived => "derived",
        }
    }

    /// 保存表記からの復元。**未知の値は [`Self::Derived`]**（読みは寛容に:
    /// 手編集や将来の版が書いた値で起動を止めない）
    pub(crate) fn parse(text: &str) -> Self {
        match text {
            "custom" => Self::Custom,
            "ai" => Self::Ai,
            "last_prompt" => Self::LastPrompt,
            "first_prompt" => Self::FirstPrompt,
            _ => Self::Derived,
        }
    }
}

/// 一覧に載る 1 行。**プロセスが死んでも残る**のがライブ状態（`claude agents --json`）
/// との違いで、行の生死と表示する状態は別の知識になる
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SessionRow {
    /// 行の identity（[`SessionId`]）
    pub(crate) session_id: SessionId,
    pub(crate) cwd: String,
    /// 表示名の**表示用キャッシュ**。正本は transcript（`custom-title` /
    /// `ai-title` / `last-prompt`。[`crate::title`]）で、ここに置くのは
    /// transcript を読む前（起動直後・1 ターン前）でも行に名前が出るようにするため
    pub(crate) title: String,
    /// `title` がどこから来たか（[`TitleSource`]）
    pub(crate) title_source: TitleSource,
    /// **最後に観測した** state。プロセスが死んでも消さない
    /// （消すと「終わった」のか「見えなくなった」のか行から読めなくなる）
    pub(crate) last_state: String,
    pub(crate) pinned: bool,
    /// 未読判定用（ms）。`updated_at > last_opened_at` が未読
    pub(crate) last_opened_at: u64,
    pub(crate) created_at: u64,
    /// **マージの後勝ち判定にも使う**（[`merge_sessions`]）ので、行の内容を
    /// 変えたら必ず進める
    pub(crate) updated_at: u64,
}

impl SessionRow {
    /// 新しい行。`now` は epoch ms。**時計は引数で受ける**（呼び出し側が 1 度読んだ
    /// 値を行に揃えられる ＝ 同じ操作で作った行の時刻がばらけない）
    pub(crate) fn new(
        session_id: SessionId,
        cwd: impl Into<String>,
        title: impl Into<String>,
        title_source: TitleSource,
        now: u64,
    ) -> Self {
        Self {
            session_id,
            cwd: cwd.into(),
            title: title.into(),
            title_source,
            last_state: String::new(),
            pinned: false,
            // 作った時点では未読にしない（作ったのはユーザー自身の操作）
            last_opened_at: now,
            created_at: now,
            updated_at: now,
        }
    }

    /// 未読か（行頭に `●` を出す判定）。
    ///
    /// **判定は行の意味論なのでここが持つ**: `updated_at` を進める側（[`crate::app`]
    /// の `touch`）・既読にする側（同 `mark_opened`）・描画（[`crate::ui`]）が
    /// どれもこの 1 つの式を見る
    pub(crate) fn unread(&self) -> bool {
        self.updated_at > self.last_opened_at
    }

    fn to_json(&self) -> Value {
        json!({
            ID_KEY: self.session_id.as_str(),
            CWD_KEY: self.cwd,
            TITLE_KEY: self.title,
            TITLE_SOURCE_KEY: self.title_source.as_str(),
            LAST_STATE_KEY: self.last_state,
            PINNED_KEY: self.pinned,
            LAST_OPENED_AT_KEY: self.last_opened_at,
            CREATED_AT_KEY: self.created_at,
            UPDATED_AT_KEY: self.updated_at,
        })
    }

    /// **読みは寛容**（[`ccdesk::load_state_list`] と同じ方針）: 型が違う項目は
    /// 既定値として読む。**identity を持たない行だけは捨てる**
    /// （`session_id` が無い行はマージのキーが無く、残しても何も指せない）
    fn from_json(value: &Value) -> Option<Self> {
        let text = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let flag = |key: &str| value.get(key).and_then(Value::as_bool).unwrap_or(false);
        let ms = |key: &str| value.get(key).and_then(Value::as_u64).unwrap_or(0);
        let session_id = SessionId::new(text(ID_KEY));
        if session_id.is_empty() {
            return None;
        }
        Some(Self {
            session_id,
            cwd: text(CWD_KEY),
            title: text(TITLE_KEY),
            title_source: TitleSource::parse(&text(TITLE_SOURCE_KEY)),
            last_state: text(LAST_STATE_KEY),
            pinned: flag(PINNED_KEY),
            last_opened_at: ms(LAST_OPENED_AT_KEY),
            created_at: ms(CREATED_AT_KEY),
            updated_at: ms(UPDATED_AT_KEY),
        })
    }
}

/// セッション一覧の保管ストア。保管先は引数で注入する（[`Self::new`]）
pub(crate) struct SessionStore {
    /// 保管ファイル（`~/.ccdesk/sessions.json`）
    store: PathBuf,
    lock_wait: Duration,
    lock_stale: Duration,
    /// 「ディスク上の一覧はこうなっている」とこのインスタンスが最後に判断した内容。
    /// **書き込みのマージの基準**（[`merge_sessions`]）で、読み（[`Self::list`]）と
    /// **実際にディスクへ書いた内容**（[`Self::store`]）で更新する。
    ///
    /// **ストアが持つ**のは、基準を進めてよい瞬間（＝ 書けたことが確認できた瞬間）が
    /// ロックの内側にしか無いため。呼び出し側に持たせると「書けたか」と「基準」が
    /// 別の場所に分かれ、片方だけ進んだ状態を作れてしまう
    baseline: Mutex<Vec<SessionRow>>,
}

impl SessionStore {
    pub(crate) fn new(store: PathBuf) -> Self {
        Self {
            store,
            lock_wait: STORE_LOCK_WAIT,
            lock_stale: LOCK_STALE,
            baseline: Mutex::new(Vec::new()),
        }
    }

    /// 既定パスのストア。ホームが取れない環境では None
    pub(crate) fn detect() -> Option<Self> {
        Some(Self::new(ccdesk::sessions_store_path()?))
    }

    /// 保管ファイル用の advisory lock（`~/.ccdesk/sessions.json.lock`）。
    /// 導出は claude と同じ [`lock_path_for`]（ロック名の規則を 2 通り持たない）
    fn store_lock(&self) -> PathBuf {
        lock_path_for(&self.store)
    }

    /// 一覧を読む。**ロックを取らない**のは [`crate::accounts::AccountStore::list`] と
    /// 同じ判断で、書き込みが tmp → rename で原子的なので中途の JSON は読めないため
    /// （読みのたびに待つと、周期的に呼ぶ側が他インスタンスの書き込みで止まる）。
    ///
    /// **読んだ内容が以降の書き込みでマージする基準になる**（[`merge_sessions`]）
    pub(crate) fn list(&self) -> Vec<SessionRow> {
        let rows = read_rows(&self.store);
        *self.baseline() = rows.clone();
        rows
    }

    /// 一覧の保存。**差分ではなく全量で渡し、永続化された一覧を返す**
    /// （渡した一覧と保存された一覧は一致しない ＝ 他インスタンスの行が増える）。
    ///
    /// 書けなかったとき（ロックが取れない・tmp 書き込みや rename の失敗）は
    /// **基準を動かさず渡された一覧を返す**: ディスクが動いていないのに
    /// 「こう書いた」と記録すると、消したはずの行が次の保存で
    /// 「他インスタンスが作った行」と分類されて復活する
    pub(crate) fn store(&self, next: &[SessionRow]) -> Vec<SessionRow> {
        // **基準を保持したままファイルのロックを取る**（順序はここと [`Self::list`] で
        // 1 通り ＝ 逆順を作らない）。こうすると読みと書きが基準を同時に動かさないので、
        // 「読んだ内容」と「書いた内容」のどちらが基準になったか分からない状態が消える
        let mut baseline = self.baseline();
        let Ok(_guard) = Lock::acquire(&self.store_lock(), self.lock_wait, self.lock_stale) else {
            return next.to_vec();
        };
        // 読みと書きの間に他インスタンスの書き込みを挟ませない（ロックの内側で読む）
        let merged = merge_sessions(&read_rows(&self.store), &baseline, next);
        let document = json!({
            SESSIONS_KEY: merged.iter().map(SessionRow::to_json).collect::<Vec<_>>()
        });
        if write_json_atomically(&self.store, &document).is_err() {
            return next.to_vec();
        }
        *baseline = merged.clone();
        merged
    }

    fn baseline(&self) -> std::sync::MutexGuard<'_, Vec<SessionRow>> {
        self.baseline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 起動時の掃除: [`write_json_atomically`] が rename する前にプロセスが死ぬと、
    /// その `.tmp` は誰にも消されずに残る。
    ///
    /// **どう回収するかは [`ccdesk::reap_leftover_tmp`]**（tmp の名前を決める側と
    /// 同じ場所）。ここが持つのは対象の指定だけ
    pub(crate) fn cleanup_leftover_tmp(&self) {
        ccdesk::reap_leftover_tmp(&self.store);
    }
}

/// 保管ファイルの行一覧（無い・壊れている・書き換え途中はすべて空）。
/// identity を持たない行は捨てる（[`SessionRow::from_json`]）
fn read_rows(path: &Path) -> Vec<SessionRow> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    value
        .get(SESSIONS_KEY)
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter_map(SessionRow::from_json).collect())
        .unwrap_or_default()
}

/// 保存する一覧を、**ディスク上の一覧と突き合わせて**決める。
///
/// **なぜマージするか**: ccdesk は複数起動でき `sessions.json` は共有なので、
/// メモリ上の写しをそのまま書くと、その間に別のインスタンスが起こしたセッションが
/// 一覧から消える（＝ そのセッションはどのウィンドウからも開けなくなる。
/// プロセスは生きているのに行だけ消えるので、ユーザーには「消えた」としか見えない）。
/// [`crate::source`] の登録プロジェクトと同じ問題で、同じ形で解く。
///
/// **意味論**:
/// - 同一性は [`SessionId`]（行の identity。表示名や cwd では判定しない）
/// - `baseline` は「ディスクはこうなっている」とこのインスタンスが最後に判断した一覧。
///   `next` との差が**このインスタンスの操作**なので、消した / 知らないを区別できる
/// - **両方に居る行は `updated_at` が新しい方**を採る。行の中身（title・状態・
///   ピン留め・既読）は最後に触った側が正しく、こちらの写しが古いなら
///   他インスタンスの変更を踏み潰してはいけない
/// - `baseline` に居て `next` に居ない行は**このインスタンスが削除した**ので、
///   ディスクに残っていても落とす（削除がこのインスタンスの以降の書き込みで復活しない）
/// - どちらにも居ない ＝ ディスクにしか居ない行は他インスタンスが作ったセッション。
///   **`next` の後ろへ足す**（自分が基準を取った後に作られた ＝ 自分の行より新しい）
///
/// 単独起動なら `disk` は `baseline` と一致するので、結果は `next` そのもの
/// （＝ マージが入っても通常の 1 プロセス動作は何も変わらない）。
///
/// **上限は設けない。** 登録プロジェクト（自動登録なので溢れる）と違い、行が増えるのは
/// ユーザーがセッションを起こしたときだけで、減らす手段（削除）もある。
/// 上限で押し出すと**ユーザーが起こしたセッションが黙って一覧から消える**
///
/// **守れないこと**: 「削除した行が二度と復活しない」保証は無い。他インスタンスが
/// 自分の一覧にその行を持ったまま書けば戻る（[`crate::source`] の `merge_projects` と
/// 同じ性質）。削除をもう一度押せば済む頻度の問題として割り切っている
fn merge_sessions(
    disk: &[SessionRow],
    baseline: &[SessionRow],
    next: &[SessionRow],
) -> Vec<SessionRow> {
    let mut merged: Vec<SessionRow> = next.to_vec();
    for row in disk {
        match merged
            .iter_mut()
            .find(|mine| mine.session_id == row.session_id)
        {
            // 両方が知っている行 ＝ 後に触った側の内容を採る
            Some(mine) if row.updated_at > mine.updated_at => *mine = row.clone(),
            Some(_) => {}
            // baseline に居る ＝ このインスタンスが削除した行なので足さない
            None if baseline.iter().any(|b| b.session_id == row.session_id) => {}
            None => merged.push(row.clone()),
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;
    // 取り残し tmp の判定と保持期間は lib 側（tmp 名を決める場所）が持つ
    use ccdesk::{is_leftover_tmp, TMP_KEEP};

    /// テスト専用の保管先。**実ユーザーの `~/.ccdesk` を絶対に触らない**ための境界。
    /// Drop で丸ごと消すので、アサート失敗でパニックしても残らない。
    ///
    /// [`crate::accounts::tests::TempHome`] と同じ規律（テスト名 + pid + 連番で一意）だが、
    /// あちらは認証情報ファイルと保管ファイルの**対**を作る道具で、こちらが要るのは
    /// 1 本だけ。あちらの [`crate::accounts::Paths`] にセッションの保管先を足すのは
    /// 「アカウント保管が依存するファイル」という意味を壊すので、ここは分けて持つ
    struct TempStore(PathBuf);

    impl TempStore {
        fn new(test: &str) -> Self {
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "ccdesk-sessions-{test}-{}-{seq}",
                std::process::id()
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn path(&self) -> PathBuf {
            self.0.join("sessions.json")
        }

        fn store(&self) -> SessionStore {
            SessionStore::new(self.path())
        }

        /// 待ち時間を詰めたストア（ロック競合を有界時間でテストするため）
        fn store_with_short_wait(&self) -> SessionStore {
            let mut store = self.store();
            store.lock_wait = Duration::from_millis(50);
            store
        }
    }

    impl Drop for TempStore {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// テスト用の行。`updated_at` はマージの後勝ち判定に効くので明示で受ける
    fn row(id: &str, title: &str, updated_at: u64) -> SessionRow {
        SessionRow {
            updated_at,
            ..SessionRow::new(
                SessionId::new(id),
                "C:\\dev\\app",
                title,
                TitleSource::Derived,
                1_000,
            )
        }
    }

    fn ids(rows: &[SessionRow]) -> Vec<&str> {
        rows.iter().map(|r| r.session_id.as_str()).collect()
    }

    /// **行の identity は [`SessionId`] だけ。** 表示名・cwd が同じでも別の行になり、
    /// 逆に中身が違っても ID が同じなら同じ行（マージが同一視する単位）
    #[test]
    fn a_row_is_identified_by_its_session_id() {
        let a = row("11111111-1111-4111-8111-111111111111", "same title", 1);
        let mut b = a.clone();
        b.session_id = SessionId::new("22222222-2222-4222-8222-222222222222");
        assert_ne!(a, b, "became the same row despite different IDs");
        assert_eq!(a.session_id.as_str(), a.session_id.to_string());

        let disk = [row("a", "disk title", 2)];
        let next = [row("a", "local title", 1)];
        assert_eq!(
            merge_sessions(&disk, &[], &next).len(),
            1,
            "same-ID row split into two"
        );
    }

    /// 未読は `updated_at > last_opened_at`。新しく作った行は未読にしない
    /// （作ったのはユーザー自身の操作）
    #[test]
    fn a_row_is_unread_when_it_changed_after_it_was_opened() {
        let mut row = SessionRow::new(
            SessionId::new("s"),
            "C:\\dev\\app",
            "title",
            TitleSource::Derived,
            1_000,
        );
        assert!(!row.unread(), "freshly created row is marked unread");
        row.updated_at = 1_001;
        assert!(row.unread(), "changed row did not become unread");
        row.last_opened_at = row.updated_at;
        assert!(!row.unread(), "still unread after being opened");
    }

    /// **他インスタンスが起こしたセッションを消さない**（マージが要る理由そのもの）。
    /// ディスクにしか居ない行は自分が基準を取った後に作られたので、末尾へ回して残す
    #[test]
    fn merging_keeps_sessions_started_by_another_instance() {
        let baseline = [row("shared", "shared", 1)];
        let disk = [row("shared", "shared", 1), row("from-b", "started by B", 2)];
        let next = [row("shared", "shared", 1), row("from-a", "started by A", 3)];
        assert_eq!(
            ids(&merge_sessions(&disk, &baseline, &next)),
            ["shared", "from-a", "from-b"],
            "another instance's session is missing from the list"
        );
    }

    /// 単独起動なら結果はメモリ上の写しそのもの（マージが通常動作を変えない）。
    /// ディスクが読めなかった場合（空で渡る）も同じ
    #[test]
    fn merging_is_a_no_op_for_a_single_instance() {
        let next = [row("a", "A", 1), row("b", "B", 2)];
        assert_eq!(merge_sessions(&next, &next, &next), next);
        assert_eq!(merge_sessions(&[], &next, &next), next);
    }

    /// **両方が知っている行は後に触った側が勝つ。** 他インスタンスが状態や名前を
    /// 更新した行を、こちらの古い写しで踏み潰さない（逆にこちらが新しければ残す）
    #[test]
    fn merging_takes_the_more_recently_updated_row() {
        let baseline = [row("s", "original title", 1)];
        let disk = [row("s", "renamed by B", 5)];
        let next = [row("s", "original title", 1)];
        let merged = merge_sessions(&disk, &baseline, &next);
        assert_eq!(merged[0].title, "renamed by B", "clobbered another instance's update");

        // こちらの方が新しければこちらが残る（自分の操作が保存の往復で巻き戻らない）
        let next = [row("s", "renamed by A", 9)];
        let merged = merge_sessions(&disk, &baseline, &next);
        assert_eq!(merged[0].title, "renamed by A", "own change got rolled back");
    }

    /// **削除した行は、このインスタンスの以降の書き込みでは復活しない。** baseline に
    /// 居て next に居ない行は「このインスタンスが削除した」ので、ディスクに残っていても
    /// 落とす（全量の写しだけでは「削除した」と「知らない」が区別できない ＝
    /// マージの基準が要る理由）
    #[test]
    fn merging_keeps_a_deleted_row_out_of_this_instances_own_writes() {
        let baseline = [row("keep", "kept", 1), row("dropped", "deleted", 1)];
        let disk = [row("keep", "kept", 1), row("dropped", "deleted", 9)];
        let next = [row("keep", "kept", 1)];
        assert_eq!(
            ids(&merge_sessions(&disk, &baseline, &next)),
            ["keep"],
            "deleted row came back (even though the disk side is newer)"
        );
    }

    /// 保存して読み直すと同じ行が返る（全項目が往復する）。
    /// **保存表記の読み書きが対で揃っているか**をここで固定する
    #[test]
    fn rows_round_trip_through_the_file() {
        let temp = TempStore::new("rows_round_trip_through_the_file");
        let store = temp.store();
        let mut written = SessionRow::new(
            SessionId::new("8a1c0f52-0b3e-4a6d-9f11-2c7d5e8b0a34"),
            "C:\\dev\\shop-app",
            "add dark mode toggle",
            TitleSource::Custom,
            1_700_000_000_000,
        );
        written.last_state = "blocked".to_string();
        written.pinned = true;
        written.last_opened_at = 1_700_000_000_500;
        written.updated_at = 1_700_000_001_000;

        assert_eq!(store.store(&[written.clone()]), [written.clone()]);
        // 読み直しは別のストア（起動しなおしと同じ ＝ メモリ上の写しを見ていない）
        assert_eq!(temp.store().list(), [written]);

        // 未知の title_source は既定へ倒す（手編集・将来の版で起動が止まらない）
        std::fs::write(
            temp.path(),
            r#"{"sessions":[{"session_id":"s","title_source":"from-the-future"}]}"#,
        )
        .unwrap();
        assert_eq!(temp.store().list()[0].title_source, TitleSource::Derived);
    }

    /// 出どころの保存表記は**読みと書きが対**（片方だけ増えた状態にならない）。
    ///
    /// **優先順はここが持たない**: 正本は transcript から候補を選ぶ側
    /// （[`crate::title`] の `CANDIDATES` の並び）1 箇所で、行はどこから来たかを
    /// 覚えているだけ（順位を数でも持つと、同じ知識が 2 箇所になる）
    #[test]
    fn title_sources_round_trip_through_their_stored_spelling() {
        for source in [
            TitleSource::Derived,
            TitleSource::FirstPrompt,
            TitleSource::LastPrompt,
            TitleSource::Ai,
            TitleSource::Custom,
        ] {
            assert_eq!(TitleSource::parse(source.as_str()), source);
        }
        // 保存表記は互いに重ならない（別の出どころが同じ綴りにならない）
        let spellings = [
            TitleSource::Derived.as_str(),
            TitleSource::FirstPrompt.as_str(),
            TitleSource::LastPrompt.as_str(),
            TitleSource::Ai.as_str(),
            TitleSource::Custom.as_str(),
        ];
        let mut unique = spellings.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), spellings.len(), "two sources share one spelling");
    }

    /// 壊れた / 想定外の形でも読みは失敗しない（＝起動が止まらない）。
    /// **identity を持たない行だけは捨てる**（マージのキーが無く、残しても何も指せない）
    #[test]
    fn reads_tolerate_missing_broken_and_unexpected_shapes() {
        let temp = TempStore::new("reads_tolerate_missing_broken_and_unexpected_shapes");
        let cases = [
            ("missing", None),
            ("empty", Some("")),
            ("broken", Some(r#"{"sessions":[{"session_id":"s"}"#)),
            ("not-object", Some("[1,2,3]")),
            ("no-key", Some(r#"{"other":1}"#)),
            ("not-array", Some(r#"{"sessions":"nope"}"#)),
            ("no-id", Some(r#"{"sessions":[{"title":"title only"},{"session_id":""}]}"#)),
            ("wrong-types", Some(r#"{"sessions":[{"session_id":7,"title":[]}]}"#)),
        ];
        for (name, contents) in cases {
            match contents {
                Some(text) => std::fs::write(temp.path(), text).unwrap(),
                None => {
                    let _ = std::fs::remove_file(temp.path());
                }
            }
            assert!(temp.store().list().is_empty(), "did not become empty for {name}");
        }
        // 型が違う項目は既定値として読む（行そのものは残す）
        std::fs::write(
            temp.path(),
            r#"{"sessions":[{"session_id":"s","pinned":"yes","updated_at":"soon"}]}"#,
        )
        .unwrap();
        let rows = temp.store().list();
        assert_eq!(ids(&rows), ["s"], "dropped a row that has an identity");
        assert!(!rows[0].pinned);
        assert_eq!(rows[0].updated_at, 0);
    }

    /// **書き込みはロックの下でしか起きない。** 別インスタンスが保管を書いている間は
    /// 有界時間で諦め、ディスクを動かさない（部分的に書いた状態を残さない）
    #[test]
    fn writes_take_the_store_lock_and_give_up_in_bounded_time() {
        let temp = TempStore::new("writes_take_the_store_lock_and_give_up_in_bounded_time");
        let store = temp.store();
        store.store(&[row("a", "A", 1)]);
        let before = std::fs::read(temp.path()).unwrap();

        // 別インスタンス相当の保持者（mkdir されたばかりなので stale ではない）
        let held = Lock::acquire(&store.store_lock(), Duration::ZERO, LOCK_STALE).unwrap();
        let short = temp.store_with_short_wait();
        let started = Instant::now();
        let returned = short.store(&[row("a", "A", 1), row("b", "B", 2)]);
        let waited = started.elapsed();
        drop(held);

        assert!(waited < Duration::from_secs(5), "wait was not bounded: {waited:?}");
        assert_eq!(
            std::fs::read(temp.path()).unwrap(),
            before,
            "wrote to the store despite not holding the lock"
        );
        assert_eq!(
            ids(&returned),
            ["a", "b"],
            "when the write fails, returns the user's list unchanged (does not roll back the screen)"
        );

        // 解放後は通常どおり書ける（＝ロックが理由で壊れているわけではない）
        assert_eq!(ids(&short.store(&[row("a", "A", 1), row("b", "B", 2)])), ["a", "b"]);
        assert!(!store.store_lock().exists(), "left the store file's lock behind");
    }

    /// **書けなかったら次のマージの基準を進めない。**
    ///
    /// 進めてしまうと、削除した行がディスクには残っているのに基準からは消え、
    /// 次の保存で [`merge_sessions`] が「他インスタンスが作った行」と分類して
    /// **復活させる**
    #[test]
    fn a_failed_write_keeps_the_baseline_and_the_removal() {
        let temp = TempStore::new("a_failed_write_keeps_the_baseline_and_the_removal");
        let store = temp.store_with_short_wait();
        store.store(&[row("p", "P", 1), row("q", "Q", 1)]);

        // 書けない状態（別インスタンスが保管を書いている）で削除を保存する
        let held = Lock::acquire(&store.store_lock(), Duration::ZERO, LOCK_STALE).unwrap();
        let next = [row("q", "Q", 1)]; // P を削除した
        assert_eq!(ids(&store.store(&next)), ["q"], "the user's action got rolled back from the screen");
        drop(held);

        // 次の保存は書ける。P は基準に居るので「このインスタンスが削除した」と読める
        assert_eq!(ids(&store.store(&next)), ["q"], "deleted row came back");
        assert_eq!(ids(&temp.store().list()), ["q"], "deletion did not land on disk");
    }

    /// **保存はディスクを落ち着かせる**（同じ状態で 2 度保存してもディスクが動かない）。
    /// 基準も戻り値も「実際に書いた内容」になるので、取り込んだ他インスタンスの行が
    /// 次の保存で「自分が削除した」と読まれない
    #[test]
    fn a_second_save_of_the_same_state_leaves_the_disk_unchanged() {
        let temp = TempStore::new("a_second_save_of_the_same_state_leaves_the_disk_unchanged");
        let store = temp.store();
        // 他インスタンスが起こしたセッションが既にディスクに居る状態
        temp.store().store(&[row("from-b", "started by B", 5)]);

        let first = store.store(&[row("mine", "mine", 1)]);
        assert_eq!(ids(&first), ["mine", "from-b"]);
        let second = store.store(&first);
        assert_eq!(second, first, "list gets rewritten on every save");
        assert_eq!(temp.store().list(), first, "disk content differs from the returned value");
    }

    /// 書き込みは tmp → rename（[`write_json_atomically`]）。
    /// **書きかけの `.tmp` を残さない**（次の読み手が中途の JSON を拾わない）
    #[test]
    fn writes_land_atomically_without_leaving_a_tmp() {
        let temp = TempStore::new("writes_land_atomically_without_leaving_a_tmp");
        temp.store().store(&[row("a", "A", 1)]);

        // tmp 名はインスタンスごとに一意なので、名前を組み立てずに走査で見る
        let leftovers: Vec<_> = std::fs::read_dir(&temp.0)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "leftover partial tmp file remains: {leftovers:?}");
        // 置いてあるのは完全な JSON（rename で丸ごと差し替わっている）
        assert_eq!(ids(&temp.store().list()), ["a"]);
    }

    /// rename の前に死んだプロセスが残した `.tmp` は起動時に回収する。
    /// 消すのは自分たちが付ける形の名前で、かつ十分に古いものだけ
    /// （書いている最中の別インスタンスの tmp を消さない）
    #[test]
    fn leftover_tmp_files_are_reclaimed_at_startup() {
        assert!(is_leftover_tmp("sessions.json.1234-0.tmp", "sessions.json"));
        assert!(!is_leftover_tmp("sessions.json.tmp", "sessions.json"));
        assert!(!is_leftover_tmp("sessions.json.1234-0.tmp", "accounts.json"));

        let temp = TempStore::new("leftover_tmp_files_are_reclaimed_at_startup");
        let old = temp.path().with_file_name("sessions.json.4242-7.tmp");
        let fresh = temp.path().with_file_name("sessions.json.4243-0.tmp");
        let other = temp.path().with_file_name("something-else.tmp");
        for path in [&old, &fresh, &other] {
            std::fs::write(path, "{}").unwrap();
        }
        // 古い側だけ mtime を閾値の外へ動かす（経過を待たずに固定する）
        let handle = std::fs::File::options().write(true).open(&old).unwrap();
        handle
            .set_times(std::fs::FileTimes::new().set_modified(
                std::time::SystemTime::now() - TMP_KEEP - Duration::from_secs(60),
            ))
            .unwrap();
        drop(handle);

        temp.store().cleanup_leftover_tmp();

        assert!(!old.exists(), "did not reclaim the old tmp");
        assert!(fresh.exists(), "removed a tmp that might still be in progress");
        assert!(other.exists(), "removed an unrelated tmp");
    }
}
