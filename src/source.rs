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

use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use ccdesk::{same_dir, save_state, update_state_list, LockExt};

use crate::backend::Kind;
use crate::hooks::HookStates;
use crate::poll::{
    spawn_agents_poller, spawn_footer_poller, AccountStatus, AgentSnapshot, FooterInfo, Grouping,
    State, VersionSinks,
};
use crate::usage::{Usage, UsageInfo, UsageRefresh, UsageSlot, UsageWindow};
use crate::sessions::{SessionId, SessionRow, SessionStore};
use crate::title::Titles;

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
/// 内側（枠の中）に収めたいものはセッション行 1 つ。桁数は文字数ではなくセルの
/// 表示幅で数える（区切りの記号は 1 文字が 1 桁とは限らない）。名前に使える桁は
/// [`crate::ui::name_cols`] が答える（行頭・行末のメニュー・行末ブロックを引いた
/// 残り）。**手で数え直さない**ので、行の桁割りが変わればこの幅の検査も一緒に動く。
/// [`demo_rows`] の最長は "fix login form validation"(25)。
///
/// 名前が切れた画像は README の売り（行の名前が agent と一致する）を裏切るので、
/// 切らない幅を採る。右ペインを削らないようこれ以上は広げない
/// （状態ごとの件数を出すヘッダー行を撤去したので、下限は行の側だけになった）。
/// **ちょうど収まる幅より 2 桁広い**: 最長の名前が行末の状態語と隙間なく
/// 接すると、画像では 1 語に見えてしまう。
/// 根拠は `demo_sidebar_width_fits_the_sidebar_rows` が固定する
const DEMO_SIDEBAR_WIDTH: u16 = 42;

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
    /// スロットの並べ方
    pub(crate) layout: crate::panes::Layout,
    /// 十字の位置
    pub(crate) split: crate::panes::Split,
    /// スロットごとの復元内容（並びはスロット番号順）。
    /// 長さが `layout.slots()` と食い違っていても `App::set_layout` が揃える
    pub(crate) slots: Vec<SlotView>,
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

/// スロット 1 枚の復元内容。**保存表記との変換はこのファイルに閉じる**
/// （外は この enum で意図を表す）
#[derive(Clone, PartialEq, Debug)]
pub(crate) enum SlotView {
    Empty,
    /// new session 画面
    New,
    Session(String),
}

/// 「new session 画面」を表す保存表記。UUID と衝突しない値なら何でもよい
const LAST_VIEW_NEW: &str = "new";
/// 「空スロット」を表す保存表記
const LAST_VIEW_EMPTY: &str = "-";
/// スロットの区切り
const SLOT_SEP: char = ',';

impl SlotView {
    fn encode(&self) -> &str {
        match self {
            Self::Empty => LAST_VIEW_EMPTY,
            Self::New => LAST_VIEW_NEW,
            Self::Session(id) => id,
        }
    }

    /// **旧版の保存値（スロット 1 枚ぶんの裸の値）もそのまま読める**:
    /// 区切りが無ければ 1 要素の一覧として解釈されるため、
    /// 移行のための分岐を持たなくてよい
    fn decode(text: &str) -> Self {
        match text {
            LAST_VIEW_EMPTY | "" => Self::Empty,
            LAST_VIEW_NEW => Self::New,
            id => Self::Session(id.to_string()),
        }
    }
}

/// 永続化するウィンドウ状態の 1 項目。
/// live は **state.json へ書き**、demo は捨てる
/// （撮影が開発者の設定を書き換えないため）。
///
/// **ここに並ぶものは 1 つ残らず state.json 側**: どれもユーザーが選んだ設定ではなく
/// 「最後にどうなっていたか」の記録で、操作するたびに ccdesk が書き換える。
/// config.json はユーザーが手で書くファイルなので、書いた端から上書きされる値を
/// 置くと「書いても効かない設定」に見える（旧版は layout / grouping / sidebar_width を
/// あちらへ置いていた ＝ [`LiveSource::window_state`] が引き取る）。
///
/// 項目を増やすと live 側の match が非網羅になるので、保存先の指定漏れは起きない
pub(crate) enum WindowItem<'a> {
    /// 次回起動で開くスロットの中身（並びはスロット番号順）
    Slots(&'a [SlotView]),
    /// スロットの並べ方
    Layout(crate::panes::Layout),
    /// 十字の位置
    Split(crate::panes::Split),
    SidebarWidth(u16),
    LastFolder(&'a str),
    Grouping(Grouping),
}

/// バックグラウンド取得の書き込み先（ポーラーが書き、run ループが dirty で取り込む）
pub(crate) struct PollSinks {
    /// **値と観測時刻を 1 つの箱で運ぶ**（時刻だけを別の atomic で送っていた頃は、
    /// 取得に失敗したときに古い値へ新しい時刻が付いた。理由は
    /// [`crate::poll::AgentSnapshot`]）
    pub(crate) agents: Arc<Mutex<AgentSnapshot>>,
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
// **`Send + Sync` が要る**: 供給元は `Arc` で持ち回り、run ループの外
// （バックグラウンド取得の起動）からも触れる
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

    /// hook（子の claude へ `--settings` で注入したもの）が書いた state の写し。
    /// **生きている行の state はこれが主**で、hook が一度も来ていない行だけ
    /// `~/.claude/sessions/` の `status` へ落ちる（[`crate::hooks`]）
    fn hook_states(&self) -> HookStates;

    /// **窓を持たない行に与える固定の state**（`session_id` → state）。
    ///
    /// 行の状態は「その行を動かしている実行があるか」から導く（[`crate::ui`]）ので、
    /// セッションを 1 本も起こさない撮影ではすべて Stopped になり、
    /// state グルーピングが写らない。撮影用の供給元だけが、窓の代わりに
    /// 「動いている実行」をこの表で名乗る。**実データ側は必ず空**
    /// （実データの生死を答えるのは自分の子プロセスだけ）。
    /// 名前を [`Titles::fixed`] で差し替えるのと同じ形
    fn fixed_states(&self) -> std::collections::HashMap<SessionId, State>;

    /// hook の受け渡しファイルの見え方（長さ・更新時刻）。**中身を読まずに
    /// 「変わったか」だけを答える**口で、run ループが毎周見て、変わった周だけ
    /// 一覧を読み直す（ペイン内の `/resume` `/clear` に周期を待たずに気づく）。
    /// 追いかけるものが無い供給元は None
    fn hook_stamp(&self) -> Option<(u64, std::time::SystemTime)>;

    /// フッター（アカウント・バージョン）の初期値。
    /// live はポーラーが後から埋めるので既定値でよい
    fn footer(&self) -> FooterInfo;

    /// その agent の使用率。**まだ答えが無いなら [`Usage::Unknown`]**
    /// （「まだ分からない」と「取れなかった」を混ぜない）
    fn usage(&self, kind: Kind) -> Usage;

    /// 使用率をその場で取り直す（フッターの使用率をクリックしたとき）。
    /// **実際に取り直しを頼んだかを返す**: 呼び手はこれで取得中スピナーを
    /// 始めるので、取得しない供給元（撮影用）が true を返すと永遠に回る。
    /// 取得しない供給元では何もしない ＝ false
    fn refresh_usage(&self, _kind: Kind) -> bool {
        false
    }

    /// **どこかのセッションがターンを終えた。** 使用率が動いた瞬間なので取り直す
    /// （実際に取るかは供給元が間引く。[`crate::usage::spawn_poller`]）
    fn note_turn_finished(&self) {}

    /// 起動時に復元するウィンドウ状態
    fn window_state(&self) -> WindowState;

    /// 画面に出す agent。**設定を読むのは live 側だけ** ＝ 撮影
    /// （[`DemoSource`]）は開発者の `config.json` に左右されない。
    ///
    /// ここを通さず [`crate::backend::Kind::enabled`] を直に呼ぶと、`--demo` の
    /// 見た目が撮る人の設定で変わる（撮影データは半分が codex なので、
    /// codex を切っている環境では 3 行消えた画面が撮れてしまう）
    fn kinds(&self) -> Vec<Kind>;

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

    /// 表示名の供給元。**行は名前を持たない**（正本は transcript）ので、
    /// 名前を導く側をここで選ぶ: live は transcript を読み、撮影用は固定表を返す
    /// （メソッドである以上コンパイラが demo 側の実装も要求する ＝
    /// 撮影に実セッションの名前が漏れない）
    fn titles(&self) -> Titles;

    /// 新規セッションの要求で実際に claude の PTY を起こすか。
    /// **撮影用データは起こさない**（架空のセッション一覧に本物のセッションが混ざると、
    /// 撮影の再現性が壊れるうえ開発者の環境にセッションが残る）。起こさない供給元では
    /// 新規セッションの要求はフォルダの登録と初期値の更新までで止まる
    fn spawns_sessions(&self) -> bool;
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
    /// 使用率の共有スロットと手動取得の口。**opt-in していないなら None** ＝
    /// 取得スレッドを起こさず、[`DataSource::usage`] は常に [`Usage::Unknown`]
    /// （＝ 何も描かない）を返す。
    ///
    /// opt-in を要求する理由は資源ではなく、これが ccdesk で唯一「無人で Anthropic の
    /// サーバーへ出る」経路だから（判断とその根拠は [`crate::main`] にある）。
    /// **切っている人の環境では claude プロセスが 1 つも増えない**
    usage: Option<BTreeMap<Kind, (UsageSlot, UsageRefresh)>>,
    /// 「ディスク上の登録プロジェクトはこうなっている」とこのインスタンスが最後に
    /// 判断した一覧。**書き込みのマージの基準**（[`merge_projects`]）で、
    /// 起動時の読み込みと、**実際にディスクへ書いた内容**で更新する
    projects_baseline: Mutex<Vec<String>>,
    /// セッション一覧のストア。**持ち回る**のは、マージの基準を呼び出しを跨いで
    /// 保つ必要があるため（基準はストアの中にある。[`SessionStore`]）。
    /// ホームが取れない環境では None ＝ 一覧を持たない（読みは空・保存は素通し）
    sessions: Option<SessionStore>,
    /// ポーリングして回る agent（[`Kind::enabled`]）。**切った agent の
    /// プロセスは 1 回も起こさない** ＝ off が「表示だけ消える」に留まらない
    kinds: Vec<Kind>,
}

impl LiveSource {
    /// `usage_dirty` は使用率が更新されたことを run ループへ伝える合図で、
    /// `usage_fetching` はクリック起点の取得が進行中か（スピナーの材料）
    pub(crate) fn new(
        usage_display: bool,
        kinds: Vec<Kind>,
        usage_dirty: Arc<std::sync::atomic::AtomicBool>,
        usage_fetching: BTreeMap<Kind, Arc<std::sync::atomic::AtomicBool>>,
    ) -> Self {
        // 前回の異常終了が残した書きかけの `.tmp`（ウィンドウ状態・設定・
        // セッション一覧・hook の受け渡し）を 1 回の走査でまとめて回収する。
        // 実データ側だけで行う ＝ 撮影（[`DemoSource`]）は実ファイルに触らない、
        // という約束を「今 demo か」の分岐を足さずに守れる置き場所
        ccdesk::reap_startup_leftovers();
        let sessions = SessionStore::detect();
        // **opt-in の分岐はここ 1 箇所。** off なら取得スレッドを起こさない。
        // **出す agent ごとに 1 本ずつ**（[`Kind::enabled`] が絞った一覧）＝
        // 切った agent のためにスレッドもプロセスも起こさない
        let usage = usage_display.then(|| {
            kinds
                .iter()
                .copied()
                .map(|kind| {
                    let slot: UsageSlot = Arc::new(Mutex::new(Usage::default()));
                    let refresh = crate::usage::spawn_poller(
                        kind,
                        Arc::clone(&slot),
                        Arc::clone(&usage_dirty),
                        usage_fetching
                            .get(&kind)
                            .cloned()
                            .unwrap_or_default(),
                    );
                    (kind, (slot, refresh))
                })
                .collect()
        });
        Self {
            usage,
            projects_baseline: Mutex::new(Vec::new()),
            sessions,
            kinds,
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

    fn hook_states(&self) -> HookStates {
        crate::hooks::read_states()
    }

    fn fixed_states(&self) -> std::collections::HashMap<SessionId, State> {
        // 実データの行が「動いている」と言えるのは自分の子プロセスが生きている
        // ときだけ ＝ ここから状態を足す経路は持たない
        std::collections::HashMap::new()
    }

    fn hook_stamp(&self) -> Option<(u64, std::time::SystemTime)> {
        crate::hooks::states_stamp()
    }

    fn footer(&self) -> FooterInfo {
        FooterInfo::default() // 実値は spawn_footer_poller が書く
    }

    /// 起動時に設定から組んだ一覧をそのまま返す（ポーラーへ渡したものと同じ）
    fn kinds(&self) -> Vec<Kind> {
        self.kinds.clone()
    }

    fn usage(&self, kind: Kind) -> Usage {
        // opt-in していなければスロットが無い ＝ 取得もしないし何も描かない
        self.usage
            .as_ref()
            .and_then(|slots| slots.get(&kind))
            .map_or(Usage::Unknown, |(slot, _)| slot.lock_recover().clone())
    }

    /// **押した行の agent だけを取り直す**（使用率の行は agent ごとに 1 本ずつ
    /// 出ていて、それぞれが自分の取得を持つ）
    fn refresh_usage(&self, kind: Kind) -> bool {
        let Some((_, refresh)) = self.usage.as_ref().and_then(|slots| slots.get(&kind)) else {
            return false;
        };
        refresh.request();
        true
    }

    fn note_turn_finished(&self) {
        let Some(slots) = &self.usage else {
            return;
        };
        for (_, refresh) in slots.values() {
            refresh.note_turn_finished();
        }
    }

    fn window_state(&self) -> WindowState {
        // 起動列なので state.json / config.json は 1 度だけ読む
        // （キーごとの単発読みだと同じファイルを 5 回読み直す）
        let state = ccdesk::state_snapshot();
        // **config.json を画面状態のために読むのはここが最後**。旧版はこちらへ
        // 書いていたので、値が残っている環境からは読んで引き取る（[`adopt_legacy_keys`]）
        let legacy = ccdesk::settings_snapshot();
        // **存在しないディレクトリも落とさない**（dispatch_cwd の is_dir と対照的）:
        // リムーバブルドライブ・ネットワークドライブ・未マウントの作業領域は
        // 「今この瞬間見えない」だけで、消えたわけではない。ここで黙って隠すと
        // ドライブを挿し直したときに見出しが復活する理由が読めないし、
        // 登録を外す操作（remove project）も出せなくなる。
        // 見えないフォルダで new session を選んだ場合は claude の起動が
        // 失敗して下部バーに出るので、間違いは操作した時点で伝わる
        let projects = state.list("projects");
        // **読んだ内容が以降の書き込みでマージする基準になる**（[`merge_projects`]）
        *self.projects_baseline.lock_recover() = projects.clone();
        // **旧版の値は読むだけでなく引き取って降ろす**（読み側のフォールバックだけ
        // 残すと、ccdesk が二度と書かないキーが config.json に居座り続ける）
        adopt_legacy_keys(|key| state.string(key), |key| legacy.string(key));
        WindowState {
            // 旧版は config.json に保存していたため、state.json に無ければそちらへフォールバック
            sidebar_width: state
                .string("sidebar_width")
                .or_else(|| legacy.string("sidebar_width"))
                .and_then(|s| s.parse().ok())
                .unwrap_or(DEFAULT_SIDEBAR_WIDTH),
            // 綴りの正本は [`crate::panes::Layout::as_str`]
            layout: state
                .string("layout")
                .or_else(|| legacy.string("layout"))
                .as_deref()
                .map(crate::panes::Layout::parse)
                .unwrap_or_default(),
            split: crate::panes::Split {
                v: state
                    .string("split_v")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(crate::panes::Split::default().v),
                h: state
                    .string("split_h")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(crate::panes::Split::default().h),
            },
            // 区切りが無い旧版の値（セッション 1 つ）も 1 要素として読める
            slots: state
                .string("last_view")
                .map(|text| text.split(SLOT_SEP).map(SlotView::decode).collect())
                .unwrap_or_default(),
            // 前回使ったフォルダを復元（無ければ起動ディレクトリ）
            dispatch_cwd: state
                .string("last_folder")
                .filter(|p| std::path::Path::new(p).is_dir())
                .unwrap_or_else(|| {
                    std::env::current_dir()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_default()
                }),
            // デフォルトは公式 Agent View と同じ State 別グルーピング。
            // 綴りの正本は [`Grouping::as_str`]
            grouping: state
                .string("grouping")
                .or_else(|| legacy.string("grouping"))
                .as_deref()
                .map(Grouping::parse)
                .unwrap_or(Grouping::State),
            projects,
        }
    }

    fn save_window(&self, item: WindowItem<'_>) {
        match item {
            // 符号化はこの match と復号（[`Self::window_state`]）の 2 箇所 ＝
            // このファイルに閉じる
            WindowItem::Slots(slots) => save_state(
                "last_view",
                &slots
                    .iter()
                    .map(SlotView::encode)
                    .collect::<Vec<_>>()
                    .join(&SLOT_SEP.to_string()),
            ),
            WindowItem::Layout(layout) => save_state("layout", layout.as_str()),
            WindowItem::Split(split) => {
                save_state("split_v", &split.v.to_string());
                save_state("split_h", &split.h.to_string());
            }
            WindowItem::SidebarWidth(width) => save_state("sidebar_width", &width.to_string()),
            WindowItem::LastFolder(cwd) => save_state("last_folder", cwd),
            WindowItem::Grouping(grouping) => save_state("grouping", grouping.as_str()),
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
            .lock_recover();
        persist_projects(&mut baseline, next, |merge| {
            update_state_list("projects", merge)
        })
    }

    fn spawns_sessions(&self) -> bool {
        true
    }

    fn spawn_pollers(&self, sinks: PollSinks) {
        spawn_agents_poller(sinks.agents, sinks.agents_dirty);
        // 版行 2 本（claude / ccdesk）の更新チェックは**同じポーラーの同じゲート**で
        // 回す（周期を分けると片方だけ別の規則へ流れる。[`VersionSinks`]）
        spawn_footer_poller(
            self.kinds.clone(),
            VersionSinks {
                claude: sinks.footer,
                claude_dirty: sinks.footer_dirty,
                ccdesk: sinks.ccdesk_latest,
                ccdesk_dirty: sinks.ccdesk_latest_dirty,
            },
            sinks.footer_refresh,
        );
    }

    fn titles(&self) -> Titles {
        Titles::default()
    }

}

/// 旧版が config.json へ書いていた画面状態のキー。
///
/// **この並びは過去の事実なので増えない。** 画面状態の保存先は state.json 一本
/// （[`WindowItem`]）になったので、新しい項目がここへ足されることはない。
/// 逆に減らすこともできない: 一度も起動していない旧版の環境が残っている限り、
/// 引き取る相手が居る
const LEGACY_SETTING_KEYS: [&str; 3] = ["layout", "grouping", "sidebar_width"];

/// 旧版の値を state.json へ引き取り、config.json から降ろす。
///
/// **読むだけのフォールバックでは足りない。** ccdesk が二度と書かないキーが
/// config.json に残ると、手で書けば効く設定に見える（実際には次の操作で
/// state.json 側の値に負ける）。引き取りと掃除まで済ませて初めて移行が終わる。
///
/// 読みを関数で受けるのは、判断（[`legacy_adoptions`]）を実ユーザーの
/// `~/.ccdesk` に触らずテストできるようにするため
fn adopt_legacy_keys(
    state: impl Fn(&str) -> Option<String>,
    legacy: impl Fn(&str) -> Option<String>,
) {
    for (key, adopted) in legacy_adoptions(state, legacy) {
        if let Some(value) = adopted {
            save_state(key, &value);
        }
        ccdesk::forget_setting(key);
    }
}

/// config.json に残っていたキーと、state.json へ引き取る値。
///
/// **state.json に既に値があるキーは引き取らない**（`Some(key, None)`）: あちらは
/// この版で書かれた新しい記録で、旧版の値で上書きすると「前回終了時の状態」が
/// 一世代前へ巻き戻る。それでも config.json からは降ろす ＝ 戻り値に残す
fn legacy_adoptions(
    state: impl Fn(&str) -> Option<String>,
    legacy: impl Fn(&str) -> Option<String>,
) -> Vec<(&'static str, Option<String>)> {
    LEGACY_SETTING_KEYS
        .into_iter()
        .filter_map(|key| {
            let value = legacy(key)?;
            Some((key, state(key).is_none().then_some(value)))
        })
        .collect()
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

    fn hook_states(&self) -> HookStates {
        // 撮影は実セッションの hook を読まない（未読も経過時間も動かない）。
        // 行の状態は [`Self::fixed_states`] だけで決まる ＝ 固定の画面になる
        HookStates::default()
    }

    fn fixed_states(&self) -> std::collections::HashMap<SessionId, State> {
        demo_rows()
            .into_iter()
            .filter_map(|(row, _, state)| state.map(|state| (row.session_id, state)))
            .collect()
    }

    fn hook_stamp(&self) -> Option<(u64, std::time::SystemTime)> {
        // 撮影は hook を読まないので、追いかけるファイルも無い
        None
    }

    fn footer(&self) -> FooterInfo {
        demo_footer()
    }

    fn usage(&self, _kind: Kind) -> Usage {
        Usage::Ready(demo_usage())
    }

    fn window_state(&self) -> WindowState {
        WindowState {
            sidebar_width: DEMO_SIDEBAR_WIDTH,
            layout: crate::panes::Layout::One,
            split: crate::panes::Split::default(),
            // 撮影は必ず new session 画面 1 枚から始める
            slots: vec![SlotView::New],
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

    /// **撮影は常に全 agent**（設定を読まない）。撮影データは半分が codex なので、
    /// ここで設定を見ると撮る人の環境しだいで行が消えた画面が残る
    fn kinds(&self) -> Vec<Kind> {
        Kind::ORDER.to_vec()
    }

    fn titles(&self) -> Titles {
        // 撮影は transcript も `~/.claude` も読まない（固定表だけを返す）
        Titles::fixed(
            demo_rows()
                .into_iter()
                .map(|(row, name, _)| (row.session_id, name))
                .collect(),
        )
    }

}

/// 撮影用の架空セッション行。実セッション名・実プロジェクトパス・実 ID を出さない。
///
/// ID は架空の UUID。時刻は「今から N 分前」なので、いつ撮っても同じ見た目になる
/// （[`demo_usage`] と同じ理由）。**未読は出さない**（撮影は hook を読まないので、
/// 未読の材料そのものが無い）
fn demo_sessions() -> Vec<SessionRow> {
    demo_rows().into_iter().map(|(row, _, _)| row).collect()
}

/// 撮影用の行と、その行に出す表示名・状態。
///
/// **名前も状態も行が持たない**（正本はそれぞれ transcript と「動いている実行」）
/// ので、撮影は [`Titles::fixed`] と [`DataSource::fixed_states`] へ渡す表として持つ。
/// 状態が None の行は**動かしている実行が無い** ＝ Stopped
fn demo_rows() -> Vec<(SessionRow, String, Option<State>)> {
    // **agent を混ぜる**（同じフォルダに claude と codex が並ぶ形が撮れる）
    let rows: [(&str, Option<State>, &str, Kind); 6] = [
        ("fix login form validation", Some(State::Working), "C:\\dev\\shop-app", Kind::Claude),
        ("add dark mode toggle", Some(State::Waiting), "C:\\dev\\shop-app", Kind::Codex),
        ("refactor api client", Some(State::Working), "C:\\dev\\api", Kind::Claude),
        ("write onboarding docs", Some(State::Idle), "C:\\dev\\docs", Kind::Codex),
        ("optimize image pipeline", Some(State::Idle), "C:\\dev\\api", Kind::Claude),
        ("migrate to vite", None, "C:\\dev\\shop-app", Kind::Codex),
    ];
    let now = ccdesk::now_ms();
    rows.iter()
        .enumerate()
        .map(|(i, (title, state, cwd, kind))| {
            let minutes = (i as u64 + 1) * 7;
            let updated = now.saturating_sub(minutes * 60_000);
            // 架空の UUID（実セッションの ID を出さない）
            let id = SessionId::new(format!("demo0000-0000-4000-8000-{:012}", i + 1));
            (
                SessionRow {
                    updated_at: updated,
                    kind: *kind,
                    // 撮影でも会話を持たせる（表示名は固定表から出るが、行が会話を
                    // 持たない状態は撮影で見せたい姿ではない）
                    conversation: crate::sessions::Conversation::Observed(id.to_string()),
                    ..SessionRow::new(id, *cwd, updated)
                },
                (*title).to_string(),
                *state,
            )
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
        // 撮影は agent を 1 つも起こさないので、アカウントも架空のラベル
        accounts: Kind::ORDER
            .into_iter()
            .map(|kind| {
                (
                    kind,
                    AccountStatus::LoggedIn(match kind {
                        Kind::Claude => "you · Acme, Inc.".to_string(),
                        Kind::Codex => "you@acme.example".to_string(),
                    }),
                )
            })
            .collect(),
        // 撮影は agent を 1 つも起こさないので版も架空（`latest` は None なので
        // 更新マーカーと動詞は出ない = 最新の見た目で撮れる）
        versions: [(Kind::Claude, "2.1.220"), (Kind::Codex, "0.146.0")]
            .into_iter()
            .map(|(kind, current)| {
                (
                    kind,
                    crate::backend::AgentVersion {
                        current: current.to_string(),
                        latest: None,
                    },
                )
            })
            .collect(),
    }
}

/// 撮影用の架空使用率。リセット時刻は「今から N 時間後」なので、
/// いつ撮っても同じ見た目（残り時間の相対値）になる。
/// `fetched_at` も今にしておく ＝ 撮影で dim（古い表示）にならない
fn demo_usage() -> UsageInfo {
    let now = ccdesk::now_secs();
    let window = |pct: f64, resets_at: u64| UsageWindow {
        pct,
        resets_at: Some(resets_at),
    };
    UsageInfo {
        five: Some(window(34.0, now + 2 * 3600 + 40 * 60)),
        seven: Some(window(58.0, now + 3 * 86400 + 5 * 3600)),
        // モデル別枠はリセット時刻を持たない形（実測）で撮る
        models: vec![(
            "Fable".to_string(),
            UsageWindow {
                pct: 12.0,
                resets_at: None,
            },
        )],
        fetched_at: now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccdesk::{load_state, load_state_list};

    /// キー引きを表で与える（実ユーザーの `~/.ccdesk` を読まずに移行の判断を試す）
    fn reader(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    /// 旧版の値（config.json）は state.json へ引き取り、**引き取れなくても降ろす**
    #[test]
    fn legacy_window_keys_move_out_of_the_settings_file() {
        // state 側が空 ＝ 全部引き取る
        assert_eq!(
            legacy_adoptions(reader(&[]), reader(&[("layout", "1 pane"), ("grouping", "directory")])),
            [
                ("layout", Some("1 pane".to_string())),
                ("grouping", Some("directory".to_string())),
            ]
        );
        // state 側に既にある値は**引き取らない**が、config からは降ろす（None）。
        // 引き取ると「前回終了時の状態」が旧版の値へ巻き戻る
        assert_eq!(
            legacy_adoptions(
                reader(&[("layout", "2 panes")]),
                reader(&[("layout", "1 pane")])
            ),
            [("layout", None)]
        );
        // config に無いキーは触らない（無いキーへ掃除をかけに行かない）
        assert!(legacy_adoptions(reader(&[("layout", "2 panes")]), reader(&[])).is_empty());
    }

    /// 移行の対象は**旧版が config.json へ書いていたキーだけ**。
    /// 現に書いている項目（[`WindowItem`]）を混ぜると、書いた端から掃除される
    #[test]
    fn only_keys_the_old_version_wrote_to_settings_are_migrated() {
        assert_eq!(LEGACY_SETTING_KEYS, ["layout", "grouping", "sidebar_width"]);
        for key in ["last_view", "last_folder", "split_v", "split_h", "projects"] {
            assert!(
                !LEGACY_SETTING_KEYS.contains(&key),
                "{key} was never a setting -- migrating it would delete a live state key"
            );
        }
        // 設定として残るキー（config.json の正当な住人）を巻き込まない
        for key in ["codex", "usage_display"] {
            assert!(
                !LEGACY_SETTING_KEYS.contains(&key),
                "{key} is a real setting and must stay in config.json"
            );
        }
    }

    /// 撮影データは固定。実セッション・実アカウント・実使用率が混ざらないことを、
    /// 中身そのもので固定する（描画側はこの値をそのまま出す）
    #[test]
    fn demo_source_yields_fixed_fake_data() {
        // **agent ごとに別のアカウント**（片方の名前をもう片方の行へ出さない）
        assert_eq!(
            DemoSource.footer().account(Kind::Claude),
            AccountStatus::LoggedIn("you · Acme, Inc.".to_string())
        );
        assert_ne!(
            DemoSource.footer().account(Kind::Codex),
            DemoSource.footer().account(Kind::Claude),
            "both agents show the same account"
        );
        // **撮影は常に全 agent。** 撮影データは半分が codex の行なので、ここが
        // 設定（`"codex": "on"`）を読むと、切っている環境で 3 行消えた画面が
        // README に残る。撮る人の `config.json` で結果が変わってはいけない
        assert_eq!(
            DemoSource.kinds(),
            Kind::ORDER,
            "the screenshot would depend on whoever runs it"
        );
        let shown = DemoSource.kinds();
        for (row, _, _) in demo_rows() {
            assert!(
                shown.contains(&row.kind),
                "a demo row's agent is not among the ones demo draws"
            );
        }
        // agent の版行は架空の版で埋める。更新マーカーは出さない（最新の見た目で撮る）
        for kind in Kind::ORDER {
            let version = DemoSource.footer().version(kind);
            assert!(!version.current.is_empty(), "{kind:?} has no version to draw");
            assert!(
                version.latest.is_none(),
                "demo does not show an update marker"
            );
        }

        let Usage::Ready(usage) = DemoSource.usage(Kind::Claude) else {
            panic!("usage gauge is always present in demo data");
        };
        assert_eq!(usage.five.as_ref().map(|w| w.pct), Some(34.0));
        assert_eq!(usage.seven.as_ref().map(|w| w.pct), Some(58.0));
        assert_eq!(usage.models, vec![("Fable".to_string(), UsageWindow { pct: 12.0, resets_at: None })]);
        // 撮影データは常に「今」取れたことにする（dim で撮れてしまわない）
        assert!(!usage.is_stale(ccdesk::now_secs()));
    }

    /// 撮影用のセッション行は固定。実 ID・実パスは出さず、未読も出さない
    /// （撮った時刻で見た目が動かない）
    #[test]
    fn demo_sessions_are_fixed_fake_rows() {
        let sessions = DemoSource.sessions();
        let titles = DemoSource.titles();
        assert_eq!(
            sessions.iter().map(|s| titles.of(s)).collect::<Vec<_>>(),
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
                "looks like a real session ID: {}",
                session.session_id
            );
            assert!(session.cwd.starts_with("C:\\dev\\"), "cwd: {:?}", session.cwd);
            assert!(
                !DemoSource.hook_states().unread(session),
                "unread marker shows up in the demo"
            );
            assert!(!session.pinned);
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
            0,
        )];
        assert_eq!(
            DemoSource.store_sessions(&asked),
            asked,
            "demo returned a different list than what was passed"
        );
        let after = crate::sessions::SessionStore::detect()
            .map(|store| store.list())
            .unwrap_or_default();
        assert!(
            !after.iter().any(|row| row.session_id == asked[0].session_id),
            "demo wrote to sessions.json"
        );
    }

    /// 撮影用のウィンドウ状態はディスクを読まない。
    /// この機体の state.json / config.json に何が入っていても固定値になる
    /// （幅 26 桁が拾われて名前が切れた画像になる事故の再発防止）
    #[test]
    fn demo_window_state_does_not_come_from_disk() {
        let window = DemoSource.window_state();
        assert_eq!(window.sidebar_width, DEMO_SIDEBAR_WIDTH);
        assert_eq!(
            window.slots,
            vec![SlotView::New],
            "demo always starts from the new session screen"
        );
        assert_eq!(window.layout, crate::panes::Layout::One);
        assert_eq!(window.dispatch_cwd, DEMO_CWD);
        assert_eq!(window.grouping, Grouping::State);
        assert_eq!(window.projects, DEMO_PROJECTS, "registered projects are fixed too");
    }

    /// 撮影用の登録プロジェクトは実パスを含まず、demo セッションのフォルダを
    /// 全部含み（自動登録の結果として整合する）、セッション 0 本のフォルダも 1 件持つ
    /// （directory グルーピングで見出しだけが残る見た目を撮れる）
    #[test]
    fn demo_projects_cover_every_demo_session_folder_plus_an_empty_one() {
        let projects = DemoSource.window_state().projects;
        for path in &projects {
            assert!(path.starts_with("C:\\dev\\"), "looks like a real path registration: {path:?}");
        }
        let sessions = DemoSource.sessions();
        for session in &sessions {
            assert!(
                projects.contains(&session.cwd),
                "folder with a session is not registered: {:?}",
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
            "not exactly one registered folder with zero sessions: {empty:?}"
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
            "demo returned a different list than what was passed"
        );
        assert_eq!(
            load_state_list("projects"),
            before,
            "demo overwrote projects"
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
            "another instance's registration is missing"
        );
        // 表記違いは同じフォルダなので二重にしない（同一判定は same_dir 1 箇所）
        let disk = paths(&["c:/dev/shared/", "C:\\DEV\\from-a"]);
        assert_eq!(
            merge_projects(&disk, &baseline, &next),
            next,
            "differently-written folder path got duplicated"
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
        assert_eq!(merged.len(), PROJECTS_LIMIT, "exceeded the limit");
        assert_eq!(merged.first().map(String::as_str), Some("C:\\dev\\p1"));
        assert_eq!(
            merged.last().map(String::as_str),
            Some("C:\\dev\\from-b"),
            "another instance's registration got evicted"
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
        assert_eq!(baseline, first, "baseline does not match what was actually written");
        assert_eq!(disk, first, "disk content differs from the returned value");
        // 2 度目: ディスクも自分の一覧も 1 度目に書いた内容（呼び手が取り込んだ後）
        let second = persist_projects(&mut baseline, &first, write_to(&mut disk, true));
        assert_eq!(second, first, "disk gets rewritten on every save");
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
            "recording \"this was written\" even though it wasn't"
        );
        assert_eq!(disk, ["C:\\dev\\p", "C:\\dev\\q"], "the can't-write precondition no longer holds");
        assert_eq!(returned, next, "the user's action got rolled back from the screen");

        // 次の保存は書ける。P は基準に居るので「このインスタンスが外した」と読める
        let written = persist_projects(&mut baseline, &next, write_to(&mut disk, true));
        assert_eq!(written, next, "deleted folder came back");
        assert_eq!(disk, next, "deletion did not land on disk");
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
        DemoSource.save_window(WindowItem::Slots(&[SlotView::Session(view.clone())]));
        DemoSource.save_window(WindowItem::LastFolder(&folder));
        assert_ne!(
            load_state("last_view").as_deref(),
            Some(view.as_str()),
            "demo overwrote state.json (last_view)"
        );
        assert_ne!(
            load_state("last_folder").as_deref(),
            Some(folder.as_str()),
            "demo overwrote state.json (last_folder)"
        );
    }

    /// 撮影用サイドバー幅の根拠を固定する。demo データを増やしたらここで落ちる。
    /// 幅は文字数ではなく表示幅で数える
    #[test]
    fn demo_sidebar_width_fits_the_sidebar_rows() {
        use unicode_width::UnicodeWidthStr;

        let inner = usize::from(DEMO_SIDEBAR_WIDTH - 2);
        // 名前に使える桁は**描画と同じ導出**（[`crate::ui::name_cols`]）から取る。
        // 行頭とメニューだけを手で引いていた頃は、行末に状態語と agent を足しても
        // この検査が気づかず、名前が切れた画像が README に残った
        let name_cols = crate::ui::name_cols(inner as u16);
        let mut widest = 0;
        for (_, title, _) in demo_rows() {
            assert!(
                title.width() <= name_cols,
                "{title:?} is {} cols but the name budget is {name_cols}",
                title.width()
            );
            widest = widest.max(inner - name_cols + title.width());
        }
        // 右ペインを削らないよう、必要以上に広げない（余りは 2 桁まで）
        assert!(
            inner - widest <= 2,
            "sidebar is wider than necessary (needs {widest} cols / inner {inner} cols)"
        );
    }
}
