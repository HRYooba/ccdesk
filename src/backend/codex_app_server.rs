//! codex の使用率の取得（`codex app-server` の JSON-RPC）。
//!
//! **ターンを起こさずに現在値が取れる。** claude 側の `get_usage` と同じ性質を
//! 実測で確認してある（2026-08-02 / codex 0.146.0）:
//!
//! - **課金ゼロ・枠を消費しない**（モデル推論が走らない）
//! - **会話の記録を作らない**（`~/.codex/sessions` が増えない）
//! - **ユーザーの設定を 1 バイトも書き換えない**
//!
//! rollout（会話の記録）にも `rate_limits` は載るが、そちらは**最後にセッションが
//! 動いた時点の値**でしかない。実測で rollout の最新が 1%、この経路が 2% だった。
//!
//! app-server 自体は公式ドキュメントのある third-party 向けインタフェースだが、
//! **`account/*` は文書化されていない**（CLI では `[experimental]` 表記）。
//! 外れたら使用率の行が消えるだけで、ccdesk の他の機能は落ちない。

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::poll::AccountStatus;
use crate::usage::{UsageInfo, UsageWindow};

/// 応答を待つ上限。**待ち切りにしない**（応答しない版へ当たっても、使用率が
/// 出ないだけで済ませる）
const WAIT: std::time::Duration = std::time::Duration::from_secs(20);

/// 本題の JSON-RPC id。`initialize` → `initialized` → 本題 の 3 手で、
/// `initialize` を省くと本題が通らない（実測）
const RPC_ID: i64 = 2;

/// 5h 枠と 7d 枠の幅（分）。**枠の振り分けはここから導く**（幅を書き写した
/// 文字列を別に持たない）
const FIVE_HOUR_MINS: u64 = 5 * 60;
const SEVEN_DAY_MINS: u64 = 7 * 24 * 60;

/// 今サインインしているアカウント。**取れなければ [`AccountStatus::Unknown`]**
/// （「まだ分からない」と「ログインしていない」を混ぜない ＝ 一時的な失敗で
/// 表示が "not logged in" へ化けない）
pub(crate) fn account(program: &str) -> AccountStatus {
    let Some(result) = ask(program, "account/read") else {
        return AccountStatus::Unknown;
    };
    parse_account(&result)
}

/// 応答 → アカウント。**claude と違って表示名を持たない**ので、身元として
/// 出せるのはメールアドレス。プランは身元ではないので添えない
fn parse_account(result: &Value) -> AccountStatus {
    let Some(account) = result.get("account") else {
        // `account` ごと無い ＝ ログインしていない（実測: 未ログインでも
        // メソッド自体は成功する）
        return AccountStatus::LoggedOut;
    };
    match account.get("email").and_then(Value::as_str) {
        Some(email) if !email.trim().is_empty() => {
            AccountStatus::LoggedIn(email.trim().to_string())
        }
        // 形が変わって読めない ＝ 誤情報を出さない
        _ => AccountStatus::Unknown,
    }
}

/// 現在の使用率。**取れなければ None**（呼び手が Failed へ倒す）
pub(crate) fn rate_limits(program: &str, now: u64) -> Option<UsageInfo> {
    let value = ask(program, "account/rateLimits/read")?;
    parse(&value, now)
}

/// `codex app-server` を 1 往復。
///
/// **応答が返るまで stdin を開いたままにする**（閉じるとサーバーが即終了して
/// 何も返さない。実測 327ms で無応答終了した）
fn ask(program: &str, method: &str) -> Option<Value> {
    // `.cmd` のシムでも起こせるように絶対パスへ解決する（[`ccdesk::resolve_program`]）
    let mut child = Command::new(ccdesk::resolve_program(program)?)
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let stdout = child.stdout.take()?;

    let request = format!(
        concat!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"clientInfo":"#,
            r#"{{"name":"ccdesk","title":"ccdesk","version":"{}"}}}}}}"#,
            "\n",
            r#"{{"jsonrpc":"2.0","method":"initialized","params":{{}}}}"#,
            "\n",
            r#"{{"jsonrpc":"2.0","id":{},"method":"{}","params":{{}}}}"#,
            "\n",
        ),
        env!("CARGO_PKG_VERSION"),
        RPC_ID,
        method,
    );
    let wrote = stdin.write_all(request.as_bytes()).and_then(|_| stdin.flush());

    // **読み終わったら必ず殺す。** stdin を持ったままなのでサーバーは自分では
    // 終わらない（放っておくと ccdesk の寿命ぶん残る）
    let answer = wrote.ok().and_then(|_| read_answer(stdout));
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    answer
}

/// `id: RPC_ID` の応答 1 行を拾う（通知は読み飛ばす）。
/// **行単位で捨てる**ので、知らない通知が混ざっても壊れない
fn read_answer(stdout: std::process::ChildStdout) -> Option<Value> {
    let deadline = std::time::Instant::now() + WAIT;
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        if std::time::Instant::now() > deadline {
            return None;
        }
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            return None; // サーバーが閉じた
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("id").and_then(Value::as_i64) == Some(RPC_ID) {
            return value.get("result").cloned();
        }
    }
}

/// 応答 → [`UsageInfo`]。**プロセスを起こさずに検査できる**ように分けてある
fn parse(result: &Value, now: u64) -> Option<UsageInfo> {
    let limits = result.get("rateLimits")?;
    let mut five = None;
    let mut seven = None;
    let mut models = Vec::new();
    for key in ["primary", "secondary"] {
        let Some(window) = limits.get(key).and_then(window_of) else {
            continue;
        };
        // **枠の幅で振り分ける**（codex は primary / secondary としか名乗らないので、
        // どちらが 5h でどちらが 7d かは名前から分からない。幅で判断する）
        match window.0 {
            FIVE_HOUR_MINS => five = Some(window.1),
            SEVEN_DAY_MINS => seven = Some(window.1),
            // 知らない幅の枠も落とさない（時間で名前を付けて並べる）
            mins => models.push((label_for(mins), window.1)),
        }
    }
    (five.is_some() || seven.is_some() || !models.is_empty()).then_some(UsageInfo {
        five,
        seven,
        models,
        fetched_at: now,
    })
}

/// 枠 1 つ → (幅の分, 枠)。使用率が読めない項目は捨てる
fn window_of(value: &Value) -> Option<(u64, UsageWindow)> {
    let pct = value.get("usedPercent")?.as_f64()?;
    let mins = value
        .get("windowDurationMins")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let resets_at = value.get("resetsAt").and_then(Value::as_u64);
    Some((mins, UsageWindow { pct, resets_at }))
}

/// 知らない幅の枠に付ける名前（`90m` / `12h` / `3d`）
fn label_for(mins: u64) -> String {
    if mins > 0 && mins.is_multiple_of(24 * 60) {
        format!("{}d", mins / (24 * 60))
    } else if mins > 0 && mins.is_multiple_of(60) {
        format!("{}h", mins / 60)
    } else {
        format!("{mins}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 実測の応答（2026-08-02 / codex 0.146.0）をそのまま読む
    fn sample() -> Value {
        serde_json::json!({
            "rateLimits": {
                "limitId": "codex",
                "primary": {
                    "usedPercent": 2,
                    "windowDurationMins": 10080,
                    "resetsAt": 1786191975u64
                },
                "secondary": null,
                "credits": {"hasCredits": false, "unlimited": false, "balance": null},
                "planType": "team"
            }
        })
    }

    #[test]
    fn the_weekly_window_lands_in_the_seven_day_slot() {
        let info = parse(&sample(), 1_000).expect("no usage was built");
        assert_eq!(info.seven.as_ref().map(|w| w.pct), Some(2.0));
        assert_eq!(info.seven.and_then(|w| w.resets_at), Some(1786191975));
        assert!(info.five.is_none(), "a 7d window landed in the 5h slot");
        assert!(info.models.is_empty());
        assert_eq!(info.fetched_at, 1_000);
    }

    /// **枠の振り分けは幅で決める。** codex は primary / secondary としか
    /// 名乗らないので、名前で 5h / 7d を決めると入れ替わったときに黙って嘘になる
    #[test]
    fn windows_are_sorted_by_their_width_not_by_their_name() {
        let value = serde_json::json!({
            "rateLimits": {
                "primary": {"usedPercent": 34, "windowDurationMins": 300},
                "secondary": {"usedPercent": 58, "windowDurationMins": 10080}
            }
        });
        let info = parse(&value, 0).expect("no usage was built");
        assert_eq!(info.five.map(|w| w.pct), Some(34.0));
        assert_eq!(info.seven.map(|w| w.pct), Some(58.0));
    }

    /// 知らない幅の枠も落とさず、幅から名前を付けて並べる
    #[test]
    fn a_window_of_an_unknown_width_is_kept_under_a_name_built_from_it() {
        let value = serde_json::json!({
            "rateLimits": {
                "primary": {"usedPercent": 7, "windowDurationMins": 720}
            }
        });
        let info = parse(&value, 0).expect("no usage was built");
        assert_eq!(
            info.models.iter().map(|(n, w)| (n.as_str(), w.pct)).collect::<Vec<_>>(),
            [("12h", 7.0)]
        );
    }

    /// 枠が 1 つも読めない応答は None（空の使用率行を描かない）
    #[test]
    fn an_answer_with_no_window_builds_nothing() {
        assert!(parse(&serde_json::json!({"rateLimits": {}}), 0).is_none());
        assert!(parse(&serde_json::json!({}), 0).is_none());
    }

    #[test]
    fn a_width_becomes_the_shortest_name_that_says_it() {
        assert_eq!(label_for(10080), "7d");
        assert_eq!(label_for(720), "12h");
        assert_eq!(label_for(90), "90m");
    }
}
