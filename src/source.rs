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

use ccdesk::{
    load_setting, load_state, load_state_list, save_setting, save_state, save_state_list,
    scan_jobs, BgJob,
};

use crate::poll::{
    read_usage, spawn_agents_poller, spawn_ccdesk_version_check, spawn_footer_poller,
    AccountStatus, AgentInfo, FooterInfo, Grouping, UsageInfo,
};

/// サイドバーに載せるセッション数の上限（state.json の走査本数）
pub(crate) const JOBS_LIMIT: usize = 50;

/// 登録プロジェクト（ディレクトリ）の保持上限。
///
/// **上限を設ける判断**: 登録は自動（セッションを作った時点）なので、無制限だと
/// 「一度だけ試したフォルダ」が state.json に永久に積まれ、サイドバーの見出しも
/// 際限なく増える。溢れたら古い側から落とす ＝ 最近使ったフォルダが残るので、
/// 落ちたことが操作の邪魔にならない（セッションがあるフォルダは登録から落ちても
/// セッション由来で見出しが出続ける）。本数はセッション上限（[`JOBS_LIMIT`]）と
/// 同じ 50 に揃える: サイドバーに同時に載り得る規模を超えて持つ意味が無い
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
    /// 復元するセッションの short id。None = new session 画面から始める
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
    /// 登録プロジェクトの一覧。**差分ではなく全量で渡す**（正本は App 側の一覧で、
    /// 「今の全量」を書き切る形にしておけば追加と削除で保存経路が分かれない）
    Projects(&'a [String]),
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
pub(crate) trait DataSource {
    /// サイドバーに並べるセッション一覧（周期的に呼ばれる）
    fn jobs(&self) -> Vec<BgJob>;

    /// フッター（アカウント・バージョン）の初期値。
    /// live はポーラーが後から埋めるので既定値でよい
    fn footer(&self) -> FooterInfo;

    /// 使用率（5h/7d 枠）。表示しないなら None
    fn usage(&self) -> Option<UsageInfo>;

    /// 起動時に復元するウィンドウ状態
    fn window_state(&self) -> WindowState;

    /// ウィンドウ状態の保存（demo は書かない）
    fn save_window(&self, item: WindowItem<'_>);

    /// バックグラウンド取得の開始。**demo は 1 本も起こさない**
    fn spawn_pollers(&self, sinks: PollSinks);
}

/// 実データ。~/.claude と ~/.ccdesk を読み、ポーラーで claude CLI と
/// 公式配布エンドポイントを叩く
pub(crate) struct LiveSource {
    /// 使用率表示の opt-in（config.json の usage_display = "on"）
    usage_display: bool,
}

impl LiveSource {
    pub(crate) fn new(usage_display: bool) -> Self {
        Self { usage_display }
    }
}

impl DataSource for LiveSource {
    fn jobs(&self) -> Vec<BgJob> {
        scan_jobs(JOBS_LIMIT)
    }

    fn footer(&self) -> FooterInfo {
        FooterInfo::default() // 実値は spawn_footer_poller が書く
    }

    fn usage(&self) -> Option<UsageInfo> {
        // opt-in が off なら usage.json も読まない
        self.usage_display.then(read_usage).flatten()
    }

    fn window_state(&self) -> WindowState {
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
            // **存在しないディレクトリも落とさない**（dispatch_cwd の is_dir と対照的）:
            // リムーバブルドライブ・ネットワークドライブ・未マウントの作業領域は
            // 「今この瞬間見えない」だけで、消えたわけではない。ここで黙って隠すと
            // ドライブを挿し直したときに見出しが復活する理由が読めないし、
            // 登録を外す操作（remove project）も出せなくなる。
            // 見えないフォルダで new session を選んだ場合は `claude --bg` が
            // 失敗して下部バーに出るので、間違いは操作した時点で伝わる
            projects: load_state_list("projects"),
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
            WindowItem::Projects(projects) => save_state_list("projects", projects),
        }
    }

    fn spawn_pollers(&self, sinks: PollSinks) {
        spawn_agents_poller(sinks.agents, sinks.agents_dirty);
        spawn_footer_poller(sinks.footer, sinks.footer_dirty, sinks.footer_refresh);
        // ccdesk 自身の版チェックは起動時 1 回だけ（周期ポーリングしない）
        spawn_ccdesk_version_check(sinks.ccdesk_latest, sinks.ccdesk_latest_dirty);
    }
}

/// スクリーンショット撮影用の固定データ（`--demo`）。
///
/// 実セッション・実アカウント・実使用率・保存済みのウィンドウ状態を **一切読まない**。
/// ファイルもネットワークも触らないので、~/.ccdesk が無い環境でも同じ画面になり、
/// スクリプトから何度撮っても同じ画像が得られる
pub(crate) struct DemoSource;

impl DataSource for DemoSource {
    fn jobs(&self) -> Vec<BgJob> {
        demo_jobs()
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
        // （サイドバー幅・最後に開いた画面・グルーピング・プロジェクト一覧を踏み潰さない）
    }

    fn spawn_pollers(&self, _sinks: PollSinks) {
        // 固定値をそのまま出すので、claude CLI 起動・ネットワーク・
        // ファイル監視のスレッドは 1 本も起こさない
    }
}

/// 撮影用の架空セッション。実セッション名・実プロジェクトパスを出さない
fn demo_jobs() -> Vec<BgJob> {
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

/// 撮影用の架空アカウント・架空 claude 版。実アカウント・実インストールを出さない。
/// demo はフッターのポーラーを起動しないので、これが最終値になる。
/// `current` はサイドバー上部の claude 版行にそのまま出るので**架空でも埋める**
/// （空だと版番号なしの行になり、撮影が「取得前」の状態に見える）。
/// `latest` は None なので更新マーカーと動詞は出ない = 最新の見た目で撮れる
fn demo_footer() -> FooterInfo {
    FooterInfo {
        account: AccountStatus::LoggedIn("you · Acme, Inc.".to_string()),
        current: "2.1.220".to_string(),
        latest: None,
    }
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
        let jobs = DemoSource.jobs();
        assert_eq!(
            jobs.iter().map(|j| j.short.as_str()).collect::<Vec<_>>(),
            ["demo01", "demo02", "demo03", "demo04", "demo05", "demo06"]
        );
        assert_eq!(jobs[0].name, "fix login form validation");
        // プロジェクトパスは架空のものだけ（実パスの断片が混ざっていない）
        for job in &jobs {
            assert!(job.cwd.starts_with("C:\\dev\\"), "cwd: {:?}", job.cwd);
        }

        assert_eq!(
            DemoSource.footer().account,
            AccountStatus::LoggedIn("you · Acme, Inc.".to_string())
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
        let jobs = DemoSource.jobs();
        for job in &jobs {
            assert!(
                projects.contains(&job.cwd),
                "セッションのあるフォルダが未登録: {:?}",
                job.cwd
            );
        }
        let empty: Vec<&String> = projects
            .iter()
            .filter(|p| !jobs.iter().any(|j| &j.cwd == *p))
            .collect();
        assert_eq!(
            empty.len(),
            1,
            "セッション 0 本の登録フォルダがちょうど 1 件でない: {empty:?}"
        );
    }

    /// 撮影はプロジェクト一覧も書かない（開発者の登録を踏み潰さない）
    #[test]
    fn demo_does_not_persist_projects() {
        let before = load_state_list("projects");
        DemoSource.save_window(WindowItem::Projects(&[
            "C:\\demo-must-not-write".to_string()
        ]));
        assert_eq!(
            load_state_list("projects"),
            before,
            "demo が projects を書き換えている"
        );
    }

    /// 撮影は開発者の設定を書き換えない。保存要求を投げても state.json は変わらない
    /// （万一漏れても実害が小さい値を渡す: 存在しない last_view は
    ///  次回起動で new session 画面へフォールバックするだけ）
    #[test]
    fn demo_does_not_persist_window_state() {
        let before = load_state("last_view");
        DemoSource.save_window(WindowItem::LastView("demo-must-not-write"));
        assert_eq!(
            load_state("last_view"),
            before,
            "demo が state.json を書き換えている"
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
        // 名前より前の固定部分 `☰ ␣ <グリフ> ␣`。demo は agents ポーラーを
        // 起こさないので生存プロセスは無く、グリフは常に停止形（∙）
        let prefix = "☰ ∙ ".width();
        let mut widest = DEMO_HEADER.width();
        let (mut awaiting, mut working, mut completed) = (0, 0, 0);
        for job in demo_jobs() {
            let view = classify(&job.state, job.tempo == "blocked", false);
            match view.bucket {
                Bucket::Awaiting => awaiting += 1,
                Bucket::Working => working += 1,
                Bucket::Completed => completed += 1,
            }
            let need = prefix + job.name.width() + 2 + view.label.width();
            assert!(
                need <= inner,
                "{:?} + {:?} に {need} 桁必要（内側 {inner} 桁）",
                job.name,
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
}
