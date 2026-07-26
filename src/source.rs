//! 表示データの供給元。実データ（[`LiveSource`]）と撮影用の固定データ
//! （[`DemoSource`]）を型で分け、**起動時に 1 度だけ**選ぶ。
//!
//! `--demo` の分岐をこの 1 箇所に閉じ込めるための構造。呼び出し側は
//! 「今 demo か」を問わないので、新しく取得する値を足すときは
//! [`DataSource`] のメソッドが増える ＝ demo 側の実装をコンパイラが要求する。
//! 分岐の書き漏らしで実データ（実セッション名・プロジェクトパス・アカウント名・
//! 使用率）が撮影に混ざる事故を、型で防ぐのが目的。
//!
//! バックグラウンド取得の起動も供給元の責務にしてある（[`DataSource::spawn_pollers`]）。
//! demo 実装が何も起こさないので、ネットワーク・プロセス起動・ファイル読みは
//! 呼び出し側の `if !demo` ではなく構造として止まる。

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use anyhow::anyhow;

use ccdesk::{
    load_setting, load_state, load_state_list, same_dir, save_setting, save_state,
    update_state_list,
};

use crate::accounts::{Account, AccountChange, AccountStore, ActiveAccount, Outgoing};
use crate::poll::{
    read_usage, spawn_agents_poller, spawn_ccdesk_version_check, spawn_footer_poller,
    AccountStatus, AgentInfo, FooterInfo, Grouping, UsageInfo,
};
use crate::sessions::{SessionId, SessionRow, SessionStore, TitleSource};

/// 登録プロジェクト（ディレクトリ）の保持上限。
///
/// **上限を設ける判断**: 登録は自動（セッションの起動が成功した時点）なので、無制限だと
/// 「一度だけ試したフォルダ」が state.json に永久に積まれ、サイドバーの見出しも
/// 際限なく増える。溢れたら**最も長く使っていない側**から落とす ＝ 最近使った
/// フォルダが残るので、落ちたことが操作の邪魔にならない（セッションがあるフォルダは
/// 登録から落ちてもセッション由来で見出しが出続ける）。「最近使った順」を保つのは
/// 登録側（[`crate::app`] の `register_project`。使い直したフォルダを末尾へ動かす）。
/// 本数は 50: サイドバーに同時に載り得る規模を超えて持つ意味が無い
/// （セッション一覧の側に上限は無い ＝ 理由は [`crate::sessions`] の `merge_sessions`）
pub(crate) const PROJECTS_LIMIT: usize = 50;

/// 実データ側のサイドバー既定幅（保存値が無いとき）
const DEFAULT_SIDEBAR_WIDTH: u16 = 34;

/// 撮影用のサイドバー幅（桁）。**開発者の保存値は使わない**（撮影のたびに
/// 幅が変わると同じ画像が撮れない。実測で 26 桁の保存値が拾われ、
/// セッション名が全部切れた画像になっていた）。
///
/// 内側（枠の中）に収めたいものは 2 つ。桁数はセルの表示幅で数える
/// （`☰` は East Asian Ambiguous で 2 桁を占めるため、文字数では足りない）:
///
/// 1. セッション行 `☰ ␣ <グリフ> ␣ <名前>␣␣<状態>`。前置きが 5 桁で、
///    [`demo_jobs`] の最長は "add dark mode toggle"(20) + "Needs input"(11)
///    ＝ 5 + 20 + 2 + 11 = 38 桁
/// 2. 集計ヘッダー行 `1 awaiting input · 0 working · 5 completed` ＝ 42 桁。
///    語の途中で切れると画像が壊れて見えるので、こちらが実際の下限になる
///
/// List は枠の内側（幅 - 2）で切るので 42 + 2 = 44 桁。右ペインを削らないよう
/// これ以上は広げない。行末の要約・経過時間は元々溢れる前提（切っても意味が残る）。
/// 根拠は `demo_sidebar_width_fits_the_sidebar_rows` が固定する
const DEMO_SIDEBAR_WIDTH: u16 = 44;

/// 撮影用の new session 画面の初期フォルダ（実フォルダを出さない）
const DEMO_CWD: &str = "C:\\dev\\shop-app";

/// 撮影用の登録プロジェクト（実プロジェクトパスを出さない）。
///
/// 実データでは「セッションを作ったフォルダ」が自動登録されるので、撮影用も
/// [`demo_jobs`] の 3 フォルダを登録済みにしておく（demo だけ登録の意味が違う、
/// という状態を作らない）。末尾の 1 件はセッションを持たないフォルダで、
/// **セッションが 0 本でも見出しが残る**ことを directory グルーピングの撮影で見せる枠
const DEMO_PROJECTS: [&str; 4] = [
    "C:\\dev\\shop-app",
    "C:\\dev\\api",
    "C:\\dev\\docs",
    "C:\\dev\\infra",
];

/// 起動時に復元するウィンドウ状態。
/// 「どんな画面で始まるか」は撮影の再現性に直接効くので、セッションデータと同じく
/// 供給元から受け取る（demo は固定値、live は state.json / config.json）
pub(crate) struct WindowState {
    pub(crate) sidebar_width: u16,
    /// 復元するセッションの [`SessionId`]。None = new session 画面から始める
    pub(crate) last_view: Option<String>,
    /// new session の初期フォルダ
    pub(crate) dispatch_cwd: String,
    pub(crate) grouping: Grouping,
    /// 登録済みプロジェクト（ディレクトリ）の絶対パス。セッションが 0 本になっても
    /// directory グルーピングの見出しを残すための実体。
    /// **Grouping::State では表示に現れない**（state 別の並びにディレクトリ見出しが
    /// 無いため）。保持しているのは表示の都合ではなくユーザーの登録内容なので、
    /// グルーピングに関係なく読み書きする
    pub(crate) projects: Vec<String>,
}

/// 永続化するウィンドウ状態の 1 項目。
/// live は state.json / config.json へ書き、demo は捨てる
/// （撮影が開発者の設定を書き換えないため）。
/// 項目を増やすと live 側の match が非網羅になるので、保存先の指定漏れは起きない
pub(crate) enum WindowItem<'a> {
    LastView(&'a str),
    SidebarWidth(u16),
    LastFolder(&'a str),
    Grouping(Grouping),
}

/// アカウント保管への変更要求。[`WindowItem`] と同じ「1 つの enum を受ける
/// メソッド」の形にしてあるのは理由も同じで、項目を増やすと live 側の match が
/// 非網羅になる ＝ 撮影用の供給元で実ファイルを触ってしまう漏れが起きない
pub(crate) enum AccountAction {
    /// 今ログイン中のアカウントを保管へ加える
    Register(ActiveAccount),
    /// 保管アカウントへ切り替える
    Switch {
        /// 切替先（[`Account::email`]）
        email: String,
        /// 出ていく側（今の持ち主）についての観測。**そのトークンを同じロック下で
        /// 保管へ巻き取るために渡す**（[`AccountStore::switch_to`] 参照）。
        /// 日付（[`ActiveAccount::seen`]）付きで渡すのは、古い判断で
        /// 別アカウントの保管を潰さないため。**「まだ観測できていない」は
        /// この型で表せない**ので、切替はそこへ到達しない（[`Outgoing`]）
        outgoing: Outgoing,
    },
    /// 保管から外す（ログイン自体は外さない）
    Unregister(String),
}

impl AccountAction {
    /// この要求を指す語（下部バーの「アカウントの…に失敗」）。
    /// **呼び出し口で文字列を添えない**のが要点で、要求とその語彙を
    /// 同じ型の隣に置くと、要求を足したときに語を決めるのを忘れられない
    pub(crate) fn what(&self) -> &'static str {
        match self {
            Self::Register(_) => "登録",
            Self::Switch { .. } => "切替",
            Self::Unregister(_) => "登録解除",
        }
    }

    /// 実行中にアカウント行へ出す語（版行の `updating…` と同じ方鑑）。
    /// 英語なのはこれが行の表示で、他の行の語彙
    /// （`not logged in · run /login` / `updating…`）に揃えるため
    pub(crate) fn progress(&self) -> &'static str {
        match self {
            Self::Register(_) => "registering…",
            Self::Switch { .. } => "switching…",
            Self::Unregister(_) => "unregistering…",
        }
    }
}

/// バックグラウンド取得の書き込み先（ポーラーが書き、run ループが dirty で取り込む）
pub(crate) struct PollSinks {
    pub(crate) agents: Arc<Mutex<Vec<AgentInfo>>>,
    pub(crate) agents_dirty: Arc<AtomicBool>,
    pub(crate) footer: Arc<Mutex<FooterInfo>>,
    pub(crate) footer_dirty: Arc<AtomicBool>,
    pub(crate) footer_refresh: Arc<AtomicBool>,
    pub(crate) ccdesk_latest: Arc<Mutex<Option<String>>>,
    pub(crate) ccdesk_latest_dirty: Arc<AtomicBool>,
}

/// 画面に出す値の供給元。実装は [`LiveSource`] と [`DemoSource`] の 2 つだけで、
/// どちらを使うかは起動時の 1 箇所で決める。
///
/// **新しい取得値を足すときはここにメソッドを足す。** そうすれば demo 側の
/// 固定値をコンパイラが要求するので、撮影に実データが漏れない
// **`Send + Sync` が要る**: 重い要求（アカウントの登録・切替）は別スレッドで
// 走るので、供給元は `Arc` で共有される（[`crate::app`] の `apply_account`）
pub(crate) trait DataSource: Send + Sync {
    /// サイドバーに並べるセッション行（周期的に呼ばれる。正本は
    /// `~/.ccdesk/sessions.json`）。
    ///
    /// **前景セッションは `~/.claude/jobs` に痕跡を残さない**ので、
    /// 「どのセッションが存在するか」はこちらが持つ
    fn sessions(&self) -> Vec<SessionRow>;

    /// セッション行の保存。**差分ではなく全量で渡し、永続化された一覧を返す**。
    ///
    /// 戻り値がある理由は [`Self::store_projects`] と同じで、保存はディスクとの
    /// マージを通るので**渡した一覧と保存された一覧は一致しない**（他インスタンスが
    /// 起こしたセッションが増える）。返さないと App 側の一覧がディスクとずれ、
    /// 画面には出続けるのに再起動で消える / 消したはずの行が戻る、が起きる。
    /// 呼び手はこれを自分の一覧として取り込む ＝ 正本は sessions.json 1 つのまま
    fn store_sessions(&self, next: &[SessionRow]) -> Vec<SessionRow>;

    /// フッター（アカウント・バージョン）の初期値。
    /// live はポーラーが後から埋めるので既定値でよい
    fn footer(&self) -> FooterInfo;

    /// 使用率（5h/7d 枠）。表示しないなら None
    fn usage(&self) -> Option<UsageInfo>;

    /// 起動時に復元するウィンドウ状態
    fn window_state(&self) -> WindowState;

    /// ウィンドウ状態の保存（demo は書かない）
    fn save_window(&self, item: WindowItem<'_>);

    /// 登録プロジェクト一覧の保存。**差分ではなく全量で渡し、永続化された一覧を返す**。
    ///
    /// [`WindowItem`] から外して独立したメソッドにしてあるのは**戻り値がある**ため:
    /// 保存はディスクとのマージと上限の適用を通るので、渡した一覧と保存された一覧は
    /// 一致しない（他インスタンスの登録が増え、上限を超えた分は落ちる）。返さないと
    /// App 側の一覧がディスクとずれ、**画面には出続けるのに再起動で消える**登録が
    /// できてしまう（[`LiveSource::store_projects`]）。呼び手はこれを自分の一覧として
    /// 取り込む ＝ 登録一覧の正本は state.json 1 つのまま。
    /// メソッドである以上 demo 側の実装もコンパイラが要求するので、
    /// 保存先の指定漏れが起きない点は [`WindowItem`] と同じ
    fn store_projects(&self, next: &[String]) -> Vec<String>;

    /// バックグラウンド取得の開始。**demo は 1 本も起こさない**
    fn spawn_pollers(&self, sinks: PollSinks);

    /// 保管済みアカウントの一覧（アカウント行のメニューに並べる中身と、
    /// 「今のアカウントが保管されているか」＝ ⚠ の判定の両方がこれを見る）
    fn accounts(&self) -> Vec<Account>;

    /// 保管への変更（登録・切替・登録解除）。失敗は呼び出し側が下部バーへ出す。
    /// 戻り値で「今の持ち主」がどうなったかを返す（[`AccountChange`]）ので、
    /// UI はポーラーの追いつきを待たずにアカウント行を確定値へ更新できる
    fn apply_account(&self, action: AccountAction) -> anyhow::Result<AccountChange>;

    /// 新規セッションの要求で実際に claude の PTY を起こすか。
    /// **撮影用データは起こさない**（架空のセッション一覧に本物のセッションが混ざると、
    /// 撮影の再現性が壊れるうえ開発者の環境にセッションが残る）。起こさない供給元では
    /// 新規セッションの要求はフォルダの登録と初期値の更新までで止まる
    fn spawns_sessions(&self) -> bool;
}

/// [`AccountAction`] とドメイン API の対応表。**ストアを引数で受ける**ので、
/// 一時ディレクトリのストアに対してテストできる（実ユーザーの
/// `~/.claude` / `~/.ccdesk` を触らずに「どの動作がどの API へ行くか」を固定する）。
///
/// 登録・登録解除は現行の認証情報を差し替えないので
/// [`AccountChange::StoreOnly`]（＝今の持ち主は変わらない）に畳む
pub(crate) fn apply_account_action(
    store: &AccountStore,
    action: AccountAction,
) -> anyhow::Result<AccountChange> {
    match action {
        AccountAction::Register(active) => {
            store.register(&active).map(|()| AccountChange::StoreOnly)
        }
        AccountAction::Switch { email, outgoing } => store.switch_to(&email, &outgoing),
        AccountAction::Unregister(email) => {
            store.unregister(&email).map(|()| AccountChange::StoreOnly)
        }
    }
}

/// 登録プロジェクトの保存内容を、**ディスク上の一覧と突き合わせて**決める。
///
/// **なぜマージするか**: ccdesk は複数起動でき state.json は共有なので、メモリ上の
/// 写しをそのまま書くと、その間に別のインスタンスが登録したフォルダが消える
/// （A 起動 → B 起動 → B が登録 → A が何か登録して自分の写しを書く、で B の登録が
/// 落ちる）。サイドバー幅のようなスカラーも後勝ちだが、こちらは設定ではなく
/// **ユーザーのデータ**なので黙って捨てられない。
///
/// **意味論**（最近使った順 = LRU の扱いをどう決めたか）:
/// - 同一性は [`same_dir`]（表記違いは同じフォルダ。判定は lib 1 箇所のまま）
/// - `baseline` は「ディスクはこうなっている」とこのインスタンスが最後に判断した
///   一覧。`next` との差が**このインスタンスの操作**なので、足した / 外したを
///   区別できる（全量の写しだけでは「外したから無い」と「知らないから無い」が
///   同じ形になり、マージのしようがない）
/// - `baseline` に居て `next` に居ないフォルダは**このインスタンスが外した**ので、
///   ディスクに残っていても落とす（remove project が**このインスタンスの以降の
///   書き込み**で復活しない）
/// - どちらにも居ない ＝ ディスクにしか居ないフォルダは他インスタンスの登録。
///   **`next` の後ろへ足す**: 自分が baseline を取った後に書かれた登録なので、
///   自分が知っている登録より新しいと見なすのが妥当で、上限で追い出されるのも
///   自分の古い登録が先になる（他インスタンスの登録を消さないための修正なのに、
///   前に足して真っ先に追い出したら意味が無い）
/// - 上限は最後にかける（追い出しは先頭 ＝ 最も長く使っていない側から）
///
/// 単独起動なら `disk` は `baseline` と一致するので、結果は `next` そのもの
/// （＝ マージが入っても通常の 1 プロセス動作は何も変わらない）
///
/// **守れないこと**: 「外した登録が二度と復活しない」保証は無い。他インスタンスも
/// 書くときにディスクの内容を取り込むが（[`DataSource::store_projects`] の戻り値）、
/// 取り込みは**足す方向だけ**で、相手の一覧に居るフォルダは落ちない。よって A が
/// 外した登録は、それを一覧に持ったままの B の次の書き込みでディスクへ戻る
/// （A・B が `[P,Q]` を読んだ状態で A が P を外す → B が別のフォルダを登録すると、
/// B の `next` にまだ居る P が書き戻り、再起動で見出しも戻る）。止めるには
/// ディスクの一覧を周期的に読んで**一覧から消す**方向の反映が要るが、それは
/// 「登録はこのインスタンスのユーザー操作の記録」という扱いを崩す（他インスタンスの
/// remove project が自分の一覧を黙って削る）ので持たない ＝ 復活したら
/// remove project をもう一度押せば済む頻度の問題として割り切っている
fn merge_projects(disk: &[String], baseline: &[String], next: &[String]) -> Vec<String> {
    let mut merged: Vec<String> = next.to_vec();
    for entry in disk {
        // baseline に居る ＝ このインスタンスが外したフォルダなので足さない。
        // merged に居る ＝ 既に入っている（ディスク側の自己重複もここで落ちる）
        if merged.iter().chain(baseline).any(|p| same_dir(p, entry)) {
            continue;
        }
        merged.push(entry.clone());
    }
    let excess = merged.len().saturating_sub(PROJECTS_LIMIT);
    merged.drain(..excess);
    merged
}

/// 保存 1 回分: 読み直した `disk` から**書く内容**を決め、**書けたことが確認できてから**
/// 次のマージの基準を進める（[`merge_projects`] の結果 ＝ 上限適用後）。
/// 戻り値は呼び手が自分の一覧として取り込む内容（[`DataSource::store_projects`]）。
///
/// `persist` は「マージ関数を受けて実際に置き換え、**ディスクへ載ったか**を返す」もの。
/// マージがディスクの読みと同じロックの下で走らなければならないので
/// （読みと書きの間に別インスタンスの書き込みを挟ませない）、書き込み側の
/// 手続きに畳み込む形で受け取る。
///
/// **基準を書く前に進めてはいけない**: `kv_edit` の tmp 書き込み / rename は失敗しうる。
/// 進めてしまうと「P を外した」がディスクに載っていないのに基準からは消え、
/// 次の保存で P は `merged` にも `baseline` にも居ない ＝ [`merge_projects`] が
/// 「他インスタンスの登録」と分類して**外したフォルダを復活させる**。
///
/// **基準を「上限適用前の `next`」にしないのも同じ根**: 上限で削った分はディスクから
/// 消えるのに基準には残る ＝ 「こう書いた」の記録が実際と食い違い、
/// 保存するたび同じ登録が落ち続ける（呼び手の一覧にだけ残る）。
/// 他インスタンスの登録を含む書いた内容が次の `next` から落ちないのは、
/// 呼び手が戻り値を取り込むため。
///
/// **crate 内へ出しているのはテスト用の供給元も同じ手順を通すため**
/// （保存の意味論をテスト側へ写し取ると、live だけが壊れても気づけない）
pub(crate) fn persist_projects(
    baseline: &mut Vec<String>,
    next: &[String],
    persist: impl FnOnce(&mut dyn FnMut(Vec<String>) -> Vec<String>) -> bool,
) -> Vec<String> {
    let mut merged: Option<Vec<String>> = None;
    let wrote = {
        let baseline = &*baseline;
        persist(&mut |disk| {
            let written = merge_projects(&disk, baseline, next);
            merged = Some(written.clone());
            written
        })
    };
    // 書けたことが確認できたときだけ基準を進める。書けていないならディスクは
    // 動いていないので、基準も「最後に確認できたディスクの内容」のままにする
    // （呼び手には自分の一覧をそのまま返す ＝ 画面はユーザーの操作を反映し続け、
    //  次の保存で同じ差分をもう一度ディスクへ載せに行く）
    match merged.filter(|_| wrote) {
        Some(written) => {
            *baseline = written.clone();
            written
        }
        None => next.to_vec(),
    }
}

/// 実データ。~/.claude と ~/.ccdesk を読み、ポーラーで claude CLI と
/// 公式配布エンドポイントを叩く
pub(crate) struct LiveSource {
    /// 使用率表示の opt-in（config.json の usage_display = "on"）
    usage_display: bool,
    /// 「ディスク上の登録プロジェクトはこうなっている」とこのインスタンスが最後に
    /// 判断した一覧。**書き込みのマージの基準**（[`merge_projects`]）で、
    /// 起動時の読み込みと、**実際にディスクへ書いた内容**で更新する
    projects_baseline: Mutex<Vec<String>>,
    /// セッション一覧のストア。**アカウント保管（毎回 `detect` する）と違い
    /// 持ち回る**のは、マージの基準を呼び出しを跨いで保つ必要があるため
    /// （基準はストアの中にある。[`SessionStore`]）。
    /// ホームが取れない環境では None ＝ 一覧を持たない（読みは空・保存は素通し）
    sessions: Option<SessionStore>,
}

impl LiveSource {
    pub(crate) fn new(usage_display: bool) -> Self {
        // 前回の異常終了が残した書きかけの `.tmp`（**中身はトークン**）を回収する。
        // 実データ側だけで行う ＝ 撮影（[`DemoSource`]）は実ファイルに触らない、
        // という約束を「今 demo か」の分岐を足さずに守れる置き場所
        if let Some(store) = AccountStore::detect() {
            store.cleanup_leftover_tmp();
        }
        // ウィンドウ状態・設定側も同じ理由で回収する（tmp 名が一意なので、
        // rename 前に死んだ分は上書きされずに積もる）
        ccdesk::reap_leftover_kv_tmp();
        let sessions = SessionStore::detect();
        // セッション一覧の tmp も同じ理由で回収する（中身はトークンではないが、
        // rename の前に死んだ分は誰にも消されずに積もる）
        if let Some(store) = &sessions {
            store.cleanup_leftover_tmp();
        }
        Self {
            usage_display,
            projects_baseline: Mutex::new(Vec::new()),
            sessions,
        }
    }
}

impl DataSource for LiveSource {
    /// 起動時と周期的に読む。**読むたびにマージの基準が進む**（[`SessionStore::list`]）
    fn sessions(&self) -> Vec<SessionRow> {
        self.sessions
            .as_ref()
            .map(SessionStore::list)
            .unwrap_or_default()
    }

    /// **書く前にディスクを読み直してマージし、書いた内容を返す**（意味論は
    /// [`crate::sessions`] の `merge_sessions`）。書けなかったときは渡された一覧を
    /// そのまま返す ＝ ディスクが動いていないので「こう書いた」と記録しない
    fn store_sessions(&self, next: &[SessionRow]) -> Vec<SessionRow> {
        match &self.sessions {
            Some(store) => store.store(next),
            None => next.to_vec(),
        }
    }

    fn footer(&self) -> FooterInfo {
        FooterInfo::default() // 実値は spawn_footer_poller が書く
    }

    fn usage(&self) -> Option<UsageInfo> {
        // opt-in が off なら usage.json も読まない
        self.usage_display.then(read_usage).flatten()
    }

    fn window_state(&self) -> WindowState {
        // **存在しないディレクトリも落とさない**（dispatch_cwd の is_dir と対照的）:
        // リムーバブルドライブ・ネットワークドライブ・未マウントの作業領域は
        // 「今この瞬間見えない」だけで、消えたわけではない。ここで黙って隠すと
        // ドライブを挿し直したときに見出しが復活する理由が読めないし、
        // 登録を外す操作（remove project）も出せなくなる。
        // 見えないフォルダで new session を選んだ場合は claude の起動が
        // 失敗して下部バーに出るので、間違いは操作した時点で伝わる
        let projects = load_state_list("projects");
        // **読んだ内容が以降の書き込みでマージする基準になる**（[`merge_projects`]）
        *self
            .projects_baseline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = projects.clone();
        WindowState {
            // 旧版は config.json に保存していたため、state.json に無ければそちらへフォールバック
            sidebar_width: load_state("sidebar_width")
                .or_else(|| load_setting("sidebar_width"))
                .and_then(|s| s.parse().ok())
                .unwrap_or(DEFAULT_SIDEBAR_WIDTH),
            // "new" は new session 画面を意味する保存値（＝復元するセッションは無い）
            last_view: load_state("last_view").filter(|view| view != "new"),
            // 前回使ったフォルダを復元（無ければ起動ディレクトリ）
            dispatch_cwd: load_state("last_folder")
                .filter(|p| std::path::Path::new(p).is_dir())
                .unwrap_or_else(|| {
                    std::env::current_dir()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_default()
                }),
            // デフォルトは公式 Agent View と同じ State 別グルーピング
            grouping: match load_setting("grouping").as_deref() {
                Some("directory") => Grouping::Directory,
                _ => Grouping::State,
            },
            projects,
        }
    }

    fn save_window(&self, item: WindowItem<'_>) {
        match item {
            WindowItem::LastView(view) => save_state("last_view", view),
            WindowItem::SidebarWidth(width) => save_state("sidebar_width", &width.to_string()),
            WindowItem::LastFolder(cwd) => save_state("last_folder", cwd),
            // グルーピングだけはユーザー設定なので config.json 側
            WindowItem::Grouping(grouping) => save_setting(
                "grouping",
                match grouping {
                    Grouping::Directory => "directory",
                    Grouping::State => "state",
                },
            ),
        }
    }

    /// **書く前にディスクを読み直してマージし、書いた内容を返す**
    /// （マージの意味論は [`merge_projects`]）。
    ///
    /// 返すのは**実際にディスクへ書いた一覧**そのもの。これがそのまま次のマージの
    /// 基準（`baseline`）にもなり、呼び手の一覧にもなるので、**3 者が常に一致する**
    /// ＝ 「上限で削られた登録が App 側にだけ残る」ずれが起きない。
    ///
    /// 呼び手が取り込むことが前提の形で、これは以前の判断（`baseline` を
    /// マージ結果で更新すると、App の一覧に無い他インスタンスの登録が次の書き込みで
    /// 「このインスタンスが外した」と読めて消える）と矛盾しない: 他インスタンスの登録は
    /// 戻り値に入るので呼び手の一覧にも入り、次の `next` から落ちないため。
    ///
    /// 書けなかったとき（ホームが取れない・tmp 書き込みや rename の失敗）は
    /// `baseline` を動かさず渡された一覧を返す ＝ ディスクが動いていないので
    /// 「こう書いた」と記録しない（[`persist_projects`]）
    fn store_projects(&self, next: &[String]) -> Vec<String> {
        let mut baseline = self
            .projects_baseline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        persist_projects(&mut baseline, next, |merge| {
            update_state_list("projects", merge)
        })
    }

    fn spawns_sessions(&self) -> bool {
        true
    }

    fn spawn_pollers(&self, sinks: PollSinks) {
        spawn_agents_poller(sinks.agents, sinks.agents_dirty);
        spawn_footer_poller(sinks.footer, sinks.footer_dirty, sinks.footer_refresh);
        // ccdesk 自身の版チェックは起動時 1 回だけ（周期ポーリングしない）
        spawn_ccdesk_version_check(sinks.ccdesk_latest, sinks.ccdesk_latest_dirty);
    }

    fn accounts(&self) -> Vec<Account> {
        // ホームが取れない環境（[`AccountStore::detect`] が None）は保管 0 件と同じ扱い。
        // 一覧が空でも `register current` だけのメニューとして成立する
        AccountStore::detect()
            .map(|store| store.list())
            .unwrap_or_default()
    }

    fn apply_account(&self, action: AccountAction) -> anyhow::Result<AccountChange> {
        let store = AccountStore::detect()
            .ok_or_else(|| anyhow!("could not locate the home directory for the account store"))?;
        apply_account_action(&store, action)
    }
}

/// スクリーンショット撮影用の固定データ（`--demo`）。
///
/// 実セッション・実アカウント・実使用率・保存済みのウィンドウ状態を **一切読まない**。
/// ファイルもネットワークも触らないので、~/.ccdesk が無い環境でも同じ画面になり、
/// スクリプトから何度撮っても同じ画像が得られる
pub(crate) struct DemoSource;

impl DataSource for DemoSource {
    fn sessions(&self) -> Vec<SessionRow> {
        demo_sessions()
    }

    fn store_sessions(&self, next: &[SessionRow]) -> Vec<SessionRow> {
        // 撮影は開発者の `~/.ccdesk/sessions.json` を踏み潰さない。書かない ＝
        // ディスクとのマージも起きないので、渡された一覧をそのまま返す
        // （撮影中の行が保存の有無で動かない）
        next.to_vec()
    }

    fn footer(&self) -> FooterInfo {
        demo_footer()
    }

    fn usage(&self) -> Option<UsageInfo> {
        Some(demo_usage())
    }

    fn window_state(&self) -> WindowState {
        WindowState {
            sidebar_width: DEMO_SIDEBAR_WIDTH,
            last_view: None, // 撮影は必ず new session 画面から始める
            dispatch_cwd: DEMO_CWD.to_string(),
            grouping: Grouping::State,
            projects: DEMO_PROJECTS.iter().map(|p| p.to_string()).collect(),
        }
    }

    fn save_window(&self, _item: WindowItem<'_>) {
        // 撮影が開発者の state.json / config.json を書き換えない
        // （サイドバー幅・最後に開いた画面・グルーピングを踏み潰さない）
    }

    fn store_projects(&self, next: &[String]) -> Vec<String> {
        // 撮影は開発者の登録プロジェクトも踏み潰さない。書かない ＝ ディスクとの
        // マージも上限の適用も起きないので、渡された一覧をそのまま返す
        // （呼び手の一覧は変わらない ＝ 撮影中の見出しが保存の有無で動かない）
        next.to_vec()
    }

    fn spawns_sessions(&self) -> bool {
        false
    }

    fn spawn_pollers(&self, _sinks: PollSinks) {
        // 固定値をそのまま出すので、claude CLI 起動・ネットワーク・
        // ファイル監視のスレッドは 1 本も起こさない
    }

    fn accounts(&self) -> Vec<Account> {
        demo_accounts()
    }

    fn apply_account(&self, _action: AccountAction) -> anyhow::Result<AccountChange> {
        // 撮影は `~/.ccdesk/accounts.json` も `~/.claude/.credentials.json` も
        // 書かない（実アカウントのトークンを触らせない）。成功を返すのは、
        // 失敗の通知が下部バーに出て撮影の見た目が変わるのを避けるため。
        // `StoreOnly` ＝ アカウント行は固定値のまま（撮影の見た目が操作で動かない）
        Ok(AccountChange::StoreOnly)
    }
}

/// 撮影用の架空セッション行。実セッション名・実プロジェクトパス・実 ID を出さない。
///
/// ID は架空の UUID。時刻は「今から N 分前」なので、いつ撮っても同じ見た目になる
/// （[`demo_usage`] と同じ理由）。**未読は出さない**
/// （`last_opened_at` = `updated_at`）: 撮影の見た目を撮った時刻で変えないため。
///
/// **どの行も生きた PTY を持たない**（撮影はセッションを起こさない）ので、
/// 状態は `last_state` から決まる ＝ ここに書いた state がそのまま画面に出る
fn demo_sessions() -> Vec<SessionRow> {
    let rows: [(&str, &str, &str); 6] = [
        ("fix login form validation", "working", "C:\\dev\\shop-app"),
        ("add dark mode toggle", "blocked", "C:\\dev\\shop-app"),
        ("refactor api client", "working", "C:\\dev\\api"),
        ("write onboarding docs", "done", "C:\\dev\\docs"),
        ("optimize image pipeline", "done", "C:\\dev\\api"),
        ("migrate to vite", "stopped", "C:\\dev\\shop-app"),
    ];
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    rows.iter()
        .enumerate()
        .map(|(i, (title, state, cwd))| {
            let minutes = (i as u64 + 1) * 7;
            let updated = now.saturating_sub(minutes * 60_000);
            SessionRow {
                last_state: (*state).to_string(),
                updated_at: updated,
                ..SessionRow::new(
                    // 架空の UUID（実セッションの ID を出さない）
                    SessionId::new(format!("demo0000-0000-4000-8000-{:012}", i + 1)),
                    *cwd,
                    *title,
                    TitleSource::Ai,
                    updated,
                )
            }
        })
        .collect()
}

/// 撮影用の架空アカウント・架空 claude 版。実アカウント・実インストールを出さない。
/// demo はフッターのポーラーを起動しないので、これが最終値になる。
/// `current` はサイドバー上部の claude 版行にそのまま出るので**架空でも埋める**
/// （空だと版番号なしの行になり、撮影が「取得前」の状態に見える）。
/// `latest` は None なので更新マーカーと動詞は出ない = 最新の見た目で撮れる
fn demo_footer() -> FooterInfo {
    FooterInfo {
        // 指紋を持たない観測（[`ActiveAccount::unseen`]）: 撮影は認証情報ファイルを
        // 読まないので観測の日付が無く、ドメイン側の照合にも通らない
        // ＝ 万一ドメインへ流れても実ファイルを書き換える方向へは倒れない
        account: AccountStatus::LoggedIn(ActiveAccount::unseen(Account::new(
            "you@example.com",
            "you · Acme, Inc.",
        ))),
        current: "2.1.220".to_string(),
        latest: None,
    }
}

/// 撮影用の架空の保管一覧。実 email・実ラベルを出さない。
///
/// **先頭は [`demo_footer`] のアクティブアカウントと同じ email**。こうしておくと
/// アカウントメニューにアクティブ印（`●`）が付いた状態で撮れて、同時に
/// 「アクティブなのに未保管」の警告（`⚠`）が出ない見た目になる。
/// 並びは [`AccountStore::list`] と同じ email 昇順（撮り直しても順序が動かない）
fn demo_accounts() -> Vec<Account> {
    vec![
        Account::new("you@example.com", "you · Acme, Inc."),
        Account::new("you@personal.example", "you"),
    ]
}

/// 撮影用の架空使用率。リセット時刻は「今から N 時間後」なので、
/// いつ撮っても同じ見た目（残り時間の相対値）になる
fn demo_usage() -> UsageInfo {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    UsageInfo {
        five: Some((34.0, now + 2 * 3600 + 40 * 60)),
        seven: Some((58.0, now + 3 * 86400 + 5 * 3600)),
        stale: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::poll::{classify, Bucket};

    /// 撮影データは固定。実セッション・実アカウント・実使用率が混ざらないことを、
    /// 中身そのもので固定する（描画側はこの値をそのまま出す）
    #[test]
    fn demo_source_yields_fixed_fake_data() {
        assert_eq!(
            DemoSource.footer().account,
            AccountStatus::LoggedIn(ActiveAccount::unseen(Account::new(
                "you@example.com",
                "you · Acme, Inc."
            )))
        );
        // claude 版行は架空の版で埋める。更新マーカーは出さない（最新の見た目で撮る）
        assert_eq!(DemoSource.footer().current, "2.1.220");
        assert!(
            DemoSource.footer().latest.is_none(),
            "撮影で更新マーカーを出さない"
        );

        let usage = DemoSource.usage().expect("使用率ゲージは常に出す");
        assert_eq!(usage.five.map(|(pct, _)| pct), Some(34.0));
        assert_eq!(usage.seven.map(|(pct, _)| pct), Some(58.0));
        assert!(!usage.stale);
    }

    /// 撮影用のセッション行は固定。実 ID・実パスは出さず、未読も出さない
    /// （撮った時刻で見た目が動かない）
    #[test]
    fn demo_sessions_are_fixed_fake_rows() {
        let sessions = DemoSource.sessions();
        assert_eq!(
            sessions.iter().map(|s| s.title.as_str()).collect::<Vec<_>>(),
            [
                "fix login form validation",
                "add dark mode toggle",
                "refactor api client",
                "write onboarding docs",
                "optimize image pipeline",
                "migrate to vite",
            ]
        );
        for session in &sessions {
            assert!(
                session.session_id.as_str().starts_with("demo0000-"),
                "実セッションらしい ID: {}",
                session.session_id
            );
            assert!(session.cwd.starts_with("C:\\dev\\"), "cwd: {:?}", session.cwd);
            assert!(!session.unread(), "撮影に未読マーカーが写る");
            assert!(!session.archived && !session.pinned);
        }
    }

    /// 撮影はセッション一覧も書かない（開発者の `~/.ccdesk/sessions.json` を
    /// 踏み潰さない）。書かない ＝ マージも起きないので、戻り値は渡した一覧そのまま。
    ///
    /// 検査は「番人の行が実ファイルに現れたか」だけを見る（[`write_sentinel`]）。
    /// 実ファイルの前後を比べる形は、開発者が ccdesk を使っているだけで落ちる
    #[test]
    fn demo_does_not_persist_sessions() {
        // 漏れても実害が無い架空の行（存在しないフォルダ・番人の ID）
        let asked = vec![crate::sessions::SessionRow::new(
            crate::sessions::SessionId::new(write_sentinel("session")),
            "C:\\demo-must-not-write",
            "demo",
            crate::sessions::TitleSource::Derived,
            0,
        )];
        assert_eq!(
            DemoSource.store_sessions(&asked),
            asked,
            "demo が渡した一覧と違うものを返している"
        );
        let after = crate::sessions::SessionStore::detect()
            .map(|store| store.list())
            .unwrap_or_default();
        assert!(
            !after.iter().any(|row| row.session_id == asked[0].session_id),
            "demo が sessions.json へ書いている"
        );
    }

    /// 撮影用のウィンドウ状態はディスクを読まない。
    /// この機体の state.json / config.json に何が入っていても固定値になる
    /// （幅 26 桁が拾われて名前が切れた画像になる事故の再発防止）
    #[test]
    fn demo_window_state_does_not_come_from_disk() {
        let window = DemoSource.window_state();
        assert_eq!(window.sidebar_width, DEMO_SIDEBAR_WIDTH);
        assert!(window.last_view.is_none(), "撮影は new session 画面から");
        assert_eq!(window.dispatch_cwd, DEMO_CWD);
        assert_eq!(window.grouping, Grouping::State);
        assert_eq!(window.projects, DEMO_PROJECTS, "登録プロジェクトも固定値");
    }

    /// 撮影用の登録プロジェクトは実パスを含まず、demo セッションのフォルダを
    /// 全部含み（自動登録の結果として整合する）、セッション 0 本のフォルダも 1 件持つ
    /// （directory グルーピングで見出しだけが残る見た目を撮れる）
    #[test]
    fn demo_projects_cover_every_demo_session_folder_plus_an_empty_one() {
        let projects = DemoSource.window_state().projects;
        for path in &projects {
            assert!(path.starts_with("C:\\dev\\"), "実パスらしい登録: {path:?}");
        }
        let sessions = DemoSource.sessions();
        for session in &sessions {
            assert!(
                projects.contains(&session.cwd),
                "セッションのあるフォルダが未登録: {:?}",
                session.cwd
            );
        }
        let empty: Vec<&String> = projects
            .iter()
            .filter(|p| !sessions.iter().any(|s| &s.cwd == *p))
            .collect();
        assert_eq!(
            empty.len(),
            1,
            "セッション 0 本の登録フォルダがちょうど 1 件でない: {empty:?}"
        );
    }

    /// 撮影はプロジェクト一覧も書かない（開発者の登録を踏み潰さない）。
    /// 書かない ＝ マージも上限も起きないので、戻り値は渡した一覧そのまま
    /// （呼び手の一覧が撮影中に動かない）
    #[test]
    fn demo_does_not_persist_projects() {
        let before = load_state_list("projects");
        let asked = paths(&["C:\\demo-must-not-write"]);
        assert_eq!(
            DemoSource.store_projects(&asked),
            asked,
            "demo が渡した一覧と違うものを返している"
        );
        assert_eq!(
            load_state_list("projects"),
            before,
            "demo が projects を書き換えている"
        );
    }

    /// テスト内でパス一覧を組む短縮
    fn paths(list: &[&str]) -> Vec<String> {
        list.iter().map(|p| p.to_string()).collect()
    }

    /// **2 つのインスタンスの登録が両方残る（この不具合の直接のリグレッションテスト）。**
    /// A 起動 → B 起動 → B が別フォルダを登録 → A が自分の登録を足して写しを書く、で
    /// B の登録が消えていた。ディスクにしか居ないフォルダは他インスタンスの登録なので、
    /// 最近使った順の末尾（自分の登録の後ろ）へ回して残す
    #[test]
    fn merging_projects_keeps_registrations_from_another_instance() {
        let baseline = paths(&["C:\\dev\\shared"]); // 両方が起動時に読んだ内容
        let disk = paths(&["C:\\dev\\shared", "C:\\dev\\from-b"]); // B の登録後
        let next = paths(&["C:\\dev\\shared", "C:\\dev\\from-a"]); // A のメモリ上の一覧
        assert_eq!(
            merge_projects(&disk, &baseline, &next),
            ["C:\\dev\\shared", "C:\\dev\\from-a", "C:\\dev\\from-b"],
            "他インスタンスの登録が消えている"
        );
        // 表記違いは同じフォルダなので二重にしない（同一判定は same_dir 1 箇所）
        let disk = paths(&["c:/dev/shared/", "C:\\DEV\\from-a"]);
        assert_eq!(
            merge_projects(&disk, &baseline, &next),
            next,
            "表記違いのフォルダが二重に積まれている"
        );
    }

    /// 単独起動なら結果はメモリ上の写しそのもの（マージが通常動作を変えない）。
    /// ディスクが読めなかった場合（空で渡る）も同じ
    #[test]
    fn merging_projects_is_a_no_op_for_a_single_instance() {
        let next = paths(&["C:\\dev\\a", "C:\\dev\\b"]);
        assert_eq!(merge_projects(&next, &next, &next), next);
        assert_eq!(merge_projects(&[], &next, &next), next);
    }

    /// **外した登録は、このインスタンスの以降の書き込みでは復活しない。** baseline に
    /// 居て next に居ないフォルダは「このインスタンスが remove project した」ので、
    /// ディスクに残っていても落とす（全量の写しだけでは「外した」と「知らない」が
    /// 区別できない ＝ マージの基準が要る理由そのもの）。
    /// **他インスタンスの古い写しからの復活は止められない**（[`merge_projects`] の
    /// 「守れないこと」）ので、名前でその範囲まで主張しない
    #[test]
    fn merging_projects_keeps_a_removed_folder_out_of_this_instances_own_writes() {
        let baseline = paths(&["C:\\dev\\keep", "C:\\dev\\dropped"]);
        let disk = paths(&["C:\\dev\\keep", "C:\\dev\\dropped"]);
        let next = paths(&["C:\\dev\\keep"]);
        assert_eq!(merge_projects(&disk, &baseline, &next), ["C:\\dev\\keep"]);
        // 表記違いで残っていても復活しない
        let disk = paths(&["C:\\dev\\keep", "c:/dev/dropped/"]);
        assert_eq!(merge_projects(&disk, &baseline, &next), ["C:\\dev\\keep"]);
    }

    /// 上限はマージの後にかける。追い出しは先頭（最も長く使っていない側）からで、
    /// 他インスタンスの登録は末尾に居るので残る
    #[test]
    fn merging_projects_applies_the_limit_last() {
        let baseline = paths(&[]);
        let next: Vec<String> = (0..PROJECTS_LIMIT).map(|i| format!("C:\\dev\\p{i}")).collect();
        let disk = paths(&["C:\\dev\\from-b"]);
        let merged = merge_projects(&disk, &baseline, &next);
        assert_eq!(merged.len(), PROJECTS_LIMIT, "上限を超えて積まれている");
        assert_eq!(merged.first().map(String::as_str), Some("C:\\dev\\p1"));
        assert_eq!(
            merged.last().map(String::as_str),
            Some("C:\\dev\\from-b"),
            "他インスタンスの登録が追い出されている"
        );
    }

    /// **保存はディスクを落ち着かせる（同じ状態で 2 度保存してもディスクが動かない）。**
    /// 基準も呼び手の一覧も「実際に書いた内容」になるので、2 度目のマージは同じ一覧を
    /// 返す ＝ 上限で落ちた自分の登録が毎回落ち直したり、他インスタンスの登録が毎回
    /// 末尾へ積み直されたりしない。**取り込んだ他インスタンスの登録を、次の保存で
    /// 「自分が外した」と読ませない**ことも同時に固定している
    /// （基準に他インスタンスの登録が入るので、呼び手が取り込まないと消える）
    #[test]
    fn a_second_save_of_the_same_state_leaves_the_disk_unchanged() {
        let mut baseline = paths(&["C:\\dev\\mine"]);
        let mine = paths(&["C:\\dev\\mine"]);
        let mut disk = paths(&["C:\\dev\\mine", "C:\\dev\\from-b"]);
        let first = persist_projects(&mut baseline, &mine, write_to(&mut disk, true));
        assert_eq!(first, ["C:\\dev\\mine", "C:\\dev\\from-b"]);
        assert_eq!(baseline, first, "基準が実際に書いた内容になっていない");
        assert_eq!(disk, first, "ディスクに載った内容と戻り値が違う");
        // 2 度目: ディスクも自分の一覧も 1 度目に書いた内容（呼び手が取り込んだ後）
        let second = persist_projects(&mut baseline, &first, write_to(&mut disk, true));
        assert_eq!(second, first, "保存するたびディスクが書き換わる");
    }

    /// メモリ上のディスクへの保存 1 回分（[`persist_projects`] の `persist`）。
    /// `wrote` を false にすると **tmp 書き込み / rename の失敗**（`kv_edit` が
    /// 報告する形）と同じになる ＝ マージは走るがディスクは動かない
    fn write_to(
        disk: &mut Vec<String>,
        wrote: bool,
    ) -> impl FnOnce(&mut dyn FnMut(Vec<String>) -> Vec<String>) -> bool + '_ {
        move |merge| {
            let written = merge(disk.clone());
            if wrote {
                *disk = written;
            }
            wrote
        }
    }

    /// **書けなかったら次のマージの基準を進めない。**
    ///
    /// 進めてしまうと、外したフォルダがディスクには残っているのに基準からは消え、
    /// 次の保存で [`merge_projects`] が「他インスタンスの登録」と分類して
    /// **`app.projects` とディスクの両方へ復活させる**（`kv_edit` の tmp 書き込み /
    /// rename の失敗が黙って無視されていたときに起きていた）
    #[test]
    fn a_failed_write_keeps_the_baseline_and_the_removal() {
        let mut baseline = paths(&["C:\\dev\\p", "C:\\dev\\q"]);
        let mut disk = paths(&["C:\\dev\\p", "C:\\dev\\q"]);
        let next = paths(&["C:\\dev\\q"]); // P を remove project した

        let returned = persist_projects(&mut baseline, &next, write_to(&mut disk, false));

        assert_eq!(
            baseline,
            ["C:\\dev\\p", "C:\\dev\\q"],
            "書けていないのに「こう書いた」と記録している"
        );
        assert_eq!(disk, ["C:\\dev\\p", "C:\\dev\\q"], "書けない前提が崩れている");
        assert_eq!(returned, next, "ユーザーの操作が画面から巻き戻っている");

        // 次の保存は書ける。P は基準に居るので「このインスタンスが外した」と読める
        let written = persist_projects(&mut baseline, &next, write_to(&mut disk, true));
        assert_eq!(written, next, "削除したフォルダが復活している");
        assert_eq!(disk, next, "削除がディスクへ載っていない");
    }

    /// このテストしか書き得ない番人の値。**「実ファイルが変わっていないこと」を
    /// 検査に使わないための道具**で、テストが投げた保存要求の値が実ファイルに
    /// 現れたかどうかだけを見る。
    ///
    /// プロセス ID で一意にするのが要点で、これが 2 つの落とし穴を同時に閉じる:
    /// - **同時に走る書き手で落ちない。** 開発者が ccdesk を使っていれば、
    ///   セッションを切り替えるたびに live 側が `WindowItem::LastView` を保存する。
    ///   前後の一致を検査する形は、その書き込みが 2 回の読み取りの間に挟まるだけで
    ///   落ちる（＝偶然ではなく構造的に落ちるテストになる）
    /// - **過去の実行が後の実行を落とさない。** 万一漏れて書かれた値が残っても、
    ///   次の実行は別の pid を使うので古い値には反応しない
    fn write_sentinel(kind: &str) -> String {
        format!("demo-must-not-write-{kind}-{}", std::process::id())
    }

    /// 撮影は開発者の設定を書き換えない。保存要求を投げても、その値は
    /// state.json に現れない（漏れても実害が小さい値を渡す: 存在しない last_view は
    /// 次回起動で new session 画面へフォールバックし、last_folder も
    /// 実在しないパスなら起動ディレクトリへ落ちるだけ）。
    /// 検査の形の理由は [`write_sentinel`] を参照
    #[test]
    fn demo_does_not_persist_window_state() {
        let view = write_sentinel("view");
        let folder = write_sentinel("folder");
        DemoSource.save_window(WindowItem::LastView(&view));
        DemoSource.save_window(WindowItem::LastFolder(&folder));
        assert_ne!(
            load_state("last_view").as_deref(),
            Some(view.as_str()),
            "demo が state.json（last_view）を書き換えている"
        );
        assert_ne!(
            load_state("last_folder").as_deref(),
            Some(folder.as_str()),
            "demo が state.json（last_folder）を書き換えている"
        );
    }

    /// 撮影用サイドバー幅の根拠を固定する。demo データを増やしたらここで落ちる。
    /// 幅は文字数ではなく表示幅で数える（`☰` は 2 桁を占める）
    #[test]
    fn demo_sidebar_width_fits_the_sidebar_rows() {
        use unicode_width::UnicodeWidthStr;

        // 集計ヘッダー行（ui::draw が組む文面）。demo データではこの 1 通りに定まる
        const DEMO_HEADER: &str = "1 awaiting input · 0 working · 5 completed";

        let inner = usize::from(DEMO_SIDEBAR_WIDTH - 2);
        // 名前より前の固定部分 `☰ ␣ <グリフ> ␣`。撮影はセッションを起こさないので
        // 生きた PTY は無く、グリフは常に停止形（∙）
        let prefix = "☰ ∙ ".width();
        let mut widest = DEMO_HEADER.width();
        let (mut awaiting, mut working, mut completed) = (0, 0, 0);
        for session in demo_sessions() {
            let view = classify(&session.last_state, false);
            match view.bucket {
                Bucket::Awaiting => awaiting += 1,
                Bucket::Working => working += 1,
                Bucket::Completed => completed += 1,
            }
            let need = prefix + session.title.width() + 2 + view.label.width();
            assert!(
                need <= inner,
                "{:?} + {:?} に {need} 桁必要（内側 {inner} 桁）",
                session.title,
                view.label
            );
            widest = widest.max(need);
        }
        // ヘッダー行の文面（= 上の DEMO_HEADER）が demo データと合っていること
        assert_eq!((awaiting, working, completed), (1, 0, 5));
        assert!(
            DEMO_HEADER.width() <= inner,
            "集計ヘッダー行が切れる（{} 桁 / 内側 {inner} 桁）",
            DEMO_HEADER.width()
        );
        // 右ペインを削らないよう、必要以上に広げない（余りは 2 桁まで）
        assert!(
            inner - widest <= 2,
            "サイドバーが必要幅より広い（必要 {widest} 桁 / 内側 {inner} 桁）"
        );
    }

    /// 撮影の保管一覧も固定。**アクティブアカウント（[`demo_footer`]）が
    /// 一覧に居る**ので、アクティブ印が付き未保管警告は出ない見た目になる
    #[test]
    fn demo_accounts_are_fixed_and_contain_the_active_one() {
        let accounts = DemoSource.accounts();
        assert_eq!(
            accounts,
            vec![
                Account::new("you@example.com", "you · Acme, Inc."),
                Account::new("you@personal.example", "you"),
            ]
        );
        let AccountStatus::LoggedIn(active) = DemoSource.footer().account else {
            panic!("撮影用のフッターがログイン済みでない");
        };
        assert!(
            accounts.iter().any(|a| a.email == active.account.email),
            "アクティブアカウントが保管一覧に無い（撮影に ⚠ が写る）"
        );
        // 並びは AccountStore::list と同じ email 昇順（撮り直しで順序が動かない）
        let mut sorted = accounts.clone();
        sorted.sort_by(|a, b| a.email.cmp(&b.email));
        assert_eq!(accounts, sorted);
    }

    /// **撮影はアカウントファイルを書かない。** 保管の変更要求を投げても、
    /// その email は実 `accounts.json` に現れない（実アカウントのトークンを触らせない）。
    ///
    /// 検査の形は [`write_sentinel`] と同じ理由で「番人の値が現れたか」だけを見る。
    /// 実ファイルの状態（存在・中身）を前後で比べる形は、開発者がアカウントを
    /// 登録・切替したタイミングで落ちる ＝ 検査したい事実と関係のない理由で落ちる。
    ///
    /// 保管に無い相手への `Switch` も混ぜてある: 実データならこの要求は必ず
    /// `Err`（no stored credentials）になるので、`Ok` で返ること自体が
    /// **ストアを引きに行っていない**ことの証拠になる（ファイルを見ない検査）
    #[test]
    fn demo_does_not_write_the_account_store() {
        let email = format!("{}@invalid", write_sentinel("account"));
        // 撮影は認証情報ファイルを読まないので、観測に指紋は無い
        let account = ActiveAccount::unseen(Account::new(&email, "demo"));
        for action in [
            AccountAction::Register(account.clone()),
            AccountAction::Switch {
                email: email.clone(),
                outgoing: Outgoing::Known(account.clone()),
            },
            AccountAction::Unregister(email.clone()),
        ] {
            DemoSource.apply_account(action).expect("撮影で失敗を出さない");
        }
        // 読むのはドメイン API 経由（email とラベルだけ。トークンは手元に取らない）
        let stored = AccountStore::detect()
            .map(|store| store.list())
            .unwrap_or_default();
        assert!(
            !stored.iter().any(|a| a.email == email),
            "demo が保管ファイルへ書いている"
        );
    }

    /// UI の 3 つの動作がどのドメイン API へ行くかの対応表。**一時ディレクトリの
    /// ストア**に対して実物を通すので、実ユーザーのファイルは触らない
    #[test]
    fn each_ui_action_reaches_its_domain_api() {
        use crate::accounts::tests::{credentials_doc, oauth, TempHome};

        let home = TempHome::new("each_ui_action_reaches_its_domain_api");
        let store = home.store();
        let taro = Account::new("taro@example.com", "taro");
        let hanako = Account::new("hanako@example.com", "hanako");

        // register: 現行の認証情報がそのアカウントとして保管される。
        // 観測（[`ActiveAccount`]）は「今のファイルを見た」もの ＝ UI が
        // メニューを開いた時点でポーラーが持っていた値に相当する
        home.write_credentials(&credentials_doc("access-t", "refresh-t"));
        apply_account_action(&store, AccountAction::Register(home.active(&taro.email, &taro.label)))
            .unwrap();
        home.write_credentials(&credentials_doc("access-h", "refresh-h"));
        apply_account_action(
            &store,
            AccountAction::Register(home.active(&hanako.email, &hanako.label)),
        )
        .unwrap();
        assert_eq!(store.list(), vec![hanako.clone(), taro.clone()]); // email 昇順

        // switch: 保管した認証情報が現行ファイルへ戻り、出ていく側は巻き取られる
        apply_account_action(
            &store,
            AccountAction::Switch {
                email: taro.email.clone(),
                outgoing: Outgoing::Known(home.active(&hanako.email, &hanako.label)),
            },
        )
        .unwrap();
        assert_eq!(
            home.read_credentials()["claudeAiOauth"],
            oauth("access-t", "refresh-t"),
            "切替先の認証情報が復元されていない"
        );

        // unregister: 一覧から消えるが、ログイン（現行の認証情報）は残る
        apply_account_action(&store, AccountAction::Unregister(hanako.email.clone())).unwrap();
        assert_eq!(store.list(), vec![taro.clone()]);
        assert_eq!(
            home.read_credentials()["claudeAiOauth"],
            oauth("access-t", "refresh-t"),
            "登録解除がログインを外している"
        );
    }

    /// 同じアカウントへの switch は**何もしない**。UI は「切替先 = アクティブ」の
    /// 組をそのまま渡すので（`app::tests::switching_to_the_active_account_passes_it_as_both_target_and_active`）、
    /// この経路で現行トークンが古い写しに巻き戻らないことを実物で確かめる
    #[test]
    fn switching_to_the_active_account_changes_nothing() {
        use crate::accounts::tests::{credentials_doc, TempHome};

        let home = TempHome::new("switching_to_the_active_account_changes_nothing");
        let store = home.store();
        let taro = Account::new("taro@example.com", "taro");
        home.write_credentials(&credentials_doc("access-t", "refresh-t"));
        apply_account_action(&store, AccountAction::Register(home.active(&taro.email, &taro.label)))
            .unwrap();
        // 保管より後にトークンが更新された状態（使い捨ての refreshToken が進む）
        home.write_credentials(&credentials_doc("access-t2", "refresh-t2"));
        let before = std::fs::read(home.paths().credentials).unwrap();

        assert_eq!(
            apply_account_action(
                &store,
                AccountAction::Switch {
                    email: taro.email.clone(),
                    outgoing: Outgoing::Known(home.active(&taro.email, &taro.label)),
                },
            )
            .unwrap(),
            AccountChange::AlreadyActive,
            "何もしていないのに切替として返している"
        );

        assert_eq!(
            std::fs::read(home.paths().credentials).unwrap(),
            before,
            "現行の認証情報を古い写しで上書きしている（今のログインが壊れる）"
        );
    }
}
