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

use ccdesk::{scan_jobs, BgJob};

use crate::poll::{
    read_usage, spawn_agents_poller, spawn_ccdesk_version_check, spawn_footer_poller,
    AccountStatus, AgentInfo, FooterInfo, UsageInfo,
};

/// サイドバーに載せるセッション数の上限（state.json の走査本数）
pub(crate) const JOBS_LIMIT: usize = 50;

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

    fn spawn_pollers(&self, sinks: PollSinks) {
        spawn_agents_poller(sinks.agents, sinks.agents_dirty);
        spawn_footer_poller(sinks.footer, sinks.footer_dirty, sinks.footer_refresh);
        // ccdesk 自身の版チェックは起動時 1 回だけ（周期ポーリングしない）
        spawn_ccdesk_version_check(sinks.ccdesk_latest, sinks.ccdesk_latest_dirty);
    }
}

/// スクリーンショット撮影用の固定データ（`--demo`）。
///
/// 実セッション・実アカウント・実使用率を **一切読まない**。
/// ポーラーも起こさないので、ネットワークにもプロセス起動にも出ない
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

/// 撮影用の架空フッター。実アカウントを出さない。
/// demo はフッターのポーラーを起動しないので、これが最終値になる
/// （`latest` が None なので更新ボタン行は出ず、`current` は描画に現れない）
fn demo_footer() -> FooterInfo {
    FooterInfo {
        account: AccountStatus::LoggedIn("you · Acme, Inc.".to_string()),
        current: String::new(),
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
        assert!(DemoSource.footer().latest.is_none(), "更新行は出さない");

        let usage = DemoSource.usage().expect("使用率ゲージは常に出す");
        assert_eq!(usage.five.map(|(pct, _)| pct), Some(34.0));
        assert_eq!(usage.seven.map(|(pct, _)| pct), Some(58.0));
        assert!(!usage.stale);
    }
}
