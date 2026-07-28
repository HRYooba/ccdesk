//! 使用率（5h 枠 / 7d 枠 / モデル別週次）の取得。
//!
//! claude を短命なヘッドレスプロセスとして起こし、SDK の制御チャンネルへ
//! `get_usage` を 1 往復投げて答えだけ受け取る。**プロセスを起こしてまでこの経路を
//! 通るのは、ユーザーの statusline を奪わずに済む唯一の形だから**
//! （理由は [`crate::hooks`]）。実測（v2.1.220）での性質:
//!
//! - **課金ゼロ・枠を消費しない**（モデル推論が走らない: `total_cost_usd: 0` /
//!   `total_api_duration_ms: 0`。連続 4 回叩いて使用率もリクエスト数も動かない）
//! - **transcript を作らない**（会話ではないため `~/.claude/projects` が増えない）
//! - **ユーザーの hook を発火させない**（`disableAllHooks` を渡すため）
//! - **ユーザーの設定を 1 バイトも書き換えない**
//!
//! 綴りは非公開なので [`crate::claude_format`] が持つ。外れたときは使用率行が
//! 消えるだけで、ccdesk の他の機能は影響を受けない。

use std::sync::{Arc, Mutex};

use ccdesk::LockExt;
use std::time::Duration;

use serde_json::Value;

use crate::claude_format::{
    CONTROL_RESPONSE, CONTROL_SUBTYPE_POINTER, CONTROL_SUCCESS, INHERITED_MARKERS,
    USAGE_AVAILABLE, USAGE_BODY_POINTER, USAGE_DISPLAY_NAME, USAGE_FIVE_HOUR,
    USAGE_MODEL_SCOPED, USAGE_RATE_LIMITS, USAGE_REQUEST_LINE, USAGE_RESETS_AT,
    USAGE_SEVEN_DAY, USAGE_UTILIZATION,
};

/// **保険の取得周期。** 主のトリガーはターン完了（[`Trigger::TurnFinished`]）で、
/// 使用率が動くのはそのときだけ。それでも周期取得を残すのは、イベントでは拾えない
/// 変化が 2 つあるため:
///
/// - **5h 枠は時刻で減る**（リセット）。イベント駆動だけだと、リセット後に次の
///   ターンが終わるまで古い高い数字を出し続ける
/// - **ccdesk の外の消費**（別ターミナルの claude・claude.ai・他デバイス）
///
/// イベントが主なので周期は長くてよい。何もしていない間に claude を起こす回数が
/// 2 分ごと → 15 分ごとへ落ちる
const POLL_INTERVAL: Duration = Duration::from_secs(900);

/// イベント由来の取得の最短間隔。**セッションが何本も走っていればターン完了は
/// 連発する**ので、1 回 3 秒のプロセス起動をそのまま流さない。
/// クリック（[`Trigger::Manual`]）はユーザーが明示的に求めた操作なので間引かない
const EVENT_MIN_INTERVAL: Duration = Duration::from_secs(30);

/// 取得の合図。**間引くかどうかが違う**ので種類を分けてある
#[derive(Clone, Copy, PartialEq, Debug)]
enum Trigger {
    /// ユーザーがフッターの使用率をクリックした。**必ず取得する**
    Manual,
    /// どこかのセッションがターンを終えた（使用率が動いた瞬間）。
    /// [`EVENT_MIN_INTERVAL`] で間引く
    TurnFinished,
}

/// 最後の成功からこれを超えたら表示を dim へ落とす。**古さを黙って隠さない**
/// （取得が続けて失敗しているのに前の値を平然と出すと、固まった数字を信じさせる）
pub(crate) const STALE_AFTER_SECS: u64 = 600;

/// 1 つの枠。
///
/// `resets_at` が `None` の枠は実在する（実測: `model_scoped` の要素はリセット時刻を
/// 持たなかった）ので、**無い前提で組む**
#[derive(Clone, PartialEq, Debug)]
pub(crate) struct UsageWindow {
    /// 使用率（0-100）
    pub(crate) pct: f64,
    /// リセット時刻（unix 秒）
    pub(crate) resets_at: Option<u64>,
}

/// 取れた使用率。
///
/// **古さの判定材料は `fetched_at` 1 つ**にしてある（`stale` を持たせると、
/// 「いつ取れたか」と「古いか」の 2 つが食い違う状態を作れてしまう）
#[derive(Clone, PartialEq, Debug)]
pub(crate) struct UsageInfo {
    pub(crate) five: Option<UsageWindow>,
    /// 7 日枠（全モデル集計）
    pub(crate) seven: Option<UsageWindow>,
    /// モデル別の週次枠（返った順のまま。0% も落とさない ＝ claude 本体の
    /// 使用量画面と同じ並びになる）
    pub(crate) models: Vec<(String, UsageWindow)>,
    /// 取得できた時刻（unix 秒）
    pub(crate) fetched_at: u64,
}

impl UsageInfo {
    /// 最後の取得から [`STALE_AFTER_SECS`] を超えたか
    pub(crate) fn is_stale(&self, now: u64) -> bool {
        now.saturating_sub(self.fetched_at) > STALE_AFTER_SECS
    }
}

/// 使用率について ccdesk が言えること。
///
/// **4 つを区別するのが要点**で、以前はどれも「無言の空白」に潰れていた
/// （opt-in していない / 注入が効いていない / 枠が無いアカウント / 壊れた、が
/// 画面上で同じ見え方だった ＝ opt-in したのに出ない人へ渡せる情報が無かった）
#[derive(Clone, PartialEq, Debug, Default)]
pub(crate) enum Usage {
    /// まだ 1 度も答えが返っていない（起動直後）。**何も描かない**
    /// ＝ 判断が付く前に嘘を出さない
    #[default]
    Unknown,
    /// 枠の概念が無いアカウント（`rate_limits_available: false`）。
    /// API キー利用者など**恒久的に取れない**ので、行ごと出さない
    /// （直せないものを警告し続けない。理由は `ccdesk doctor` が言う）
    Unavailable,
    /// 取得に失敗した（claude が起きない・応答が読めない・形が変わった）。
    /// **黙って消さず、取れていないことを出す**
    Failed,
    /// 最後に取れた値
    Ready(UsageInfo),
}

/// 使用率を持ち回る共有スロット（書き手は取得スレッド 1 つ、読み手は供給元）
pub(crate) type UsageSlot = Arc<Mutex<Usage>>;

/// 取得スレッドを外から動かす口（[`spawn_poller`] が返す）
pub(crate) struct UsageRefresh(std::sync::mpsc::Sender<Trigger>);

impl UsageRefresh {
    /// **クリックされた。** その場で取り直させる（間引かない）。
    /// 連打しても取得は 1 回に畳まれる（[`spawn_poller`] が溜まった要求を捨てる）
    pub(crate) fn request(&self) {
        // スレッドが死んでいれば送れないだけ（画面は最後の値を出し続ける）
        let _ = self.0.send(Trigger::Manual);
    }

    /// **どこかのセッションがターンを終えた。** 使用率が動いた瞬間なので取り直すが、
    /// 連発するので [`EVENT_MIN_INTERVAL`] で間引く
    pub(crate) fn note_turn_finished(&self) {
        let _ = self.0.send(Trigger::TurnFinished);
    }
}

/// 周期取得を始め、手動取得の口を返す。
///
/// 取得結果は共有スロットへ書くだけで、画面へ伝わるのは供給元
/// （[`crate::source::LiveSource::usage`]）経由。
///
/// **待ちは `recv_timeout` 1 つ**にしてある: 保険の周期と 2 種類の要求を同じ場所で
/// 受けるので、「旗を短い sleep で覗きに行く」形にならない（クリックしてから
/// 周期いっぱい待たされる形も避けられる）。
///
/// 取得そのものにタイムアウトは持たない: `claude agents --json` の周期取得
/// （[`crate::poll`]）と同じ扱いで、待ちに入るのはこのスレッドだけなので TUI は
/// 止まらない。**待ちの作法を 2 通り持たない**ためにあちらへ揃えてある
pub(crate) fn spawn_poller(
    slot: UsageSlot,
    dirty: Arc<std::sync::atomic::AtomicBool>,
    fetching: Arc<std::sync::atomic::AtomicBool>,
) -> UsageRefresh {
    let (tx, rx) = std::sync::mpsc::channel::<Trigger>();
    std::thread::spawn(move || {
        loop {
            // 取得中であることを出す（1 回 3 秒前後かかるので、押したことが
            // 画面に出ないと壊れているように見える）
            fetching.store(true, std::sync::atomic::Ordering::Relaxed);
            dirty.store(true, std::sync::atomic::Ordering::Relaxed);
            let next = fetch();
            fetching.store(false, std::sync::atomic::Ordering::Relaxed);
            let fetched_at = std::time::Instant::now();

            let mut guard = slot.lock_recover();
            // 取得に失敗しても、一度取れた値は捨てない（1 回の失敗で使用率行が
            // 消えるのを防ぐ）。古さは `fetched_at` に出るので嘘にはならない
            let keep_previous =
                matches!(next, Usage::Failed) && matches!(*guard, Usage::Ready(_));
            if !keep_previous {
                *guard = next;
            }
            // 値が据え置きでも描き直させる（取得中の表示を戻す・古さは時間で変わる）
            dirty.store(true, std::sync::atomic::Ordering::Relaxed);
            // **枠の概念が無いアカウントでは周期取得をやめる。** 恒久的に取れないので
            // 保険の周期で claude を起こす意味が無い。要求だけは受け続ける
            // （アカウントを切り替えたときに戻ってこられる経路を残す）
            let permanent = matches!(*guard, Usage::Unavailable);
            drop(guard);

            // 次に取得する合図を待つ。**間引きはここ 1 箇所**
            loop {
                let received = if permanent {
                    rx.recv().map_err(|_| ())
                } else {
                    match rx.recv_timeout(POLL_INTERVAL.saturating_sub(fetched_at.elapsed())) {
                        Ok(trigger) => Ok(trigger),
                        // 保険の周期が来た
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(()),
                    }
                };
                let Ok(trigger) = received else {
                    return; // 送り手が居なくなった（TUI 終了）
                };
                match trigger {
                    // ユーザーが押した ＝ 間引かない（アカウントを切り替えたときに
                    // Unavailable から戻ってこられる唯一の経路でもある）
                    Trigger::Manual => break,
                    // **枠の概念が無いアカウントではターン完了を無視する。** 恒久的に
                    // 取れないのに、ターンのたびに claude を起こし続ける意味が無い
                    // （README の「stop polling entirely (click still re-checks)」が
                    // この挙動の正本）
                    Trigger::TurnFinished if permanent => continue,
                    // ターン完了は連発するので、直前の取得から間を置く。
                    // 間引いた分は待ち直すだけ（取得は次の合図か保険の周期で起きる）
                    Trigger::TurnFinished if fetched_at.elapsed() >= EVENT_MIN_INTERVAL => {
                        break
                    }
                    Trigger::TurnFinished => continue,
                }
            }
            // 待っている間に溜まった要求は捨てる（連打の回数だけ取得しない）
            while rx.try_recv().is_ok() {}
        }
    });
    UsageRefresh(tx)
}

/// `--settings` へ渡す JSON。**ファイルを置かずに文字列で渡す**（`--settings` は
/// 「パスまたは JSON 文字列」を受ける。公式）。
///
/// `disableAllHooks` が要る理由: これを渡さないとユーザー自身の `SessionStart` /
/// `SessionEnd` hook が**取得ごとに発火する**（実測。渡すと発火 0 件）。
/// 使用率を見るためにユーザーのフックを周期実行するのは筋が通らない
const PROBE_SETTINGS: &str = r#"{"disableAllHooks":true}"#;

/// ヘッドレスで制御チャンネルを開く引数。`-p` / `--input-format` /
/// `--output-format` / `--verbose` / `--settings` はいずれも公式
const PROBE_ARGS: [&str; 8] = [
    "-p",
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
    // `-p` と `--output-format=stream-json` の併用には必須（付けないと claude が拒否する）
    "--verbose",
    "--settings",
    PROBE_SETTINGS,
];

/// 取得が落ちた段。**`ccdesk doctor` が「どこで落ちたか」を言えるようにするため**に
/// 段を分けてある（表示側は [`Usage::Failed`] に潰れるが、診断はここを出す）
pub(crate) enum ProbeError {
    /// claude を起こせなかった（PATH に無い等）
    Spawn(String),
    /// 起きたが応答を読めなかった
    NoResponse,
    /// 制御応答が 1 行も無かった（引数や制御プロトコルが変わった）
    NoControlResponse,
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 出力は ASCII に留める（Windows コンソールのコードページ次第で化ける）
        match self {
            Self::Spawn(e) => write!(f, "could not run claude ({e})"),
            Self::NoResponse => write!(f, "claude started but produced no readable output"),
            Self::NoControlResponse => {
                write!(f, "no control_response in the output (protocol changed?)")
            }
        }
    }
}

/// claude を 1 回起こして制御応答の JSON を取る。**プロセスを起こすのはここだけ**で、
/// 解釈は [`parse_response`] にある（形の判断をテストで固定できる）
fn probe() -> Result<Value, ProbeError> {
    let mut command = std::process::Command::new("claude");
    command
        .args(PROBE_ARGS)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    // セッションを起こすときと同じ印を落とす（[`INHERITED_MARKERS`]）。
    // このプロセスは transcript を作らないので保存の心配は無いが、
    // 「継承した印は落とす」という知識を 2 通り持たないために揃えてある
    for key in INHERITED_MARKERS {
        command.env_remove(key);
    }
    let mut child = command
        .spawn()
        .map_err(|e| ProbeError::Spawn(e.to_string()))?;
    // 1 行書いて閉じる（EOF で claude が終了する）
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write as _;
        let _ = stdin.write_all(USAGE_REQUEST_LINE.as_bytes());
        let _ = stdin.write_all(b"\n");
    }
    let output = child.wait_with_output().map_err(|_| ProbeError::NoResponse)?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|v| v.get("type").and_then(Value::as_str) == Some(CONTROL_RESPONSE))
        .ok_or(ProbeError::NoControlResponse)
}

/// 1 回取得する（取得スレッド用。落ちた段は表示に出さないので潰す）
fn fetch() -> Usage {
    match probe() {
        Ok(v) => parse_response(&v, ccdesk::now_secs()),
        Err(_) => Usage::Failed,
    }
}

/// `ccdesk doctor` 用。**取得経路をユーザー自身が 1 コマンドで確かめられる**ように、
/// 落ちた段をそのまま返す（開発者の環境では再現しない不具合をユーザー側で切り分けるため）
pub(crate) fn diagnose() -> Result<Usage, ProbeError> {
    probe().map(|v| parse_response(&v, ccdesk::now_secs()))
}

/// `control_response` 1 個から [`Usage`] を組む。**プロセスを起こさずに検査できる**
/// ように分けてある（形が変わったときに何が起きるかをテストで固定するため）
fn parse_response(v: &Value, now: u64) -> Usage {
    if v.pointer(CONTROL_SUBTYPE_POINTER).and_then(Value::as_str) != Some(CONTROL_SUCCESS) {
        return Usage::Failed;
    }
    let Some(body) = v.pointer(USAGE_BODY_POINTER) else {
        return Usage::Failed;
    };
    // **恒久的に取れないアカウントを故障と混ぜない。** ここが明示的に false の
    // ときだけ Unavailable にする（キーが無い版では枠の有無で判断する）
    let unavailable = body.get(USAGE_AVAILABLE).and_then(Value::as_bool) == Some(false);
    let limits = body.get(USAGE_RATE_LIMITS);
    let five = limits.and_then(|l| window(l.get(USAGE_FIVE_HOUR)));
    let seven = limits.and_then(|l| window(l.get(USAGE_SEVEN_DAY)));
    let models = limits.map(model_windows).unwrap_or_default();
    if five.is_none() && seven.is_none() && models.is_empty() {
        // 枠が 1 つも取れなかった。アカウントの都合なら Unavailable、
        // そうでなければ形が変わったので Failed
        return if unavailable {
            Usage::Unavailable
        } else {
            Usage::Failed
        };
    }
    Usage::Ready(UsageInfo {
        five,
        seven,
        models,
        fetched_at: now,
    })
}

/// 枠 1 つ。`null` や欠落、`utilization` が数値でない形はすべて `None` へ落とす
fn window(v: Option<&Value>) -> Option<UsageWindow> {
    let v = v?;
    let pct = v.get(USAGE_UTILIZATION)?.as_f64()?;
    Some(UsageWindow {
        pct,
        resets_at: v
            .get(USAGE_RESETS_AT)
            .and_then(Value::as_str)
            .and_then(parse_timestamp),
    })
}

/// モデル別の週次枠。**配列だけを読む**（`seven_day_opus` のような枠名を並べない:
/// 実測では未公開の枠名が null で多数並んでおり、名前を写せば claude 側の増減で腐る）。
/// 名前が空の要素は表示できないので落とす
fn model_windows(limits: &Value) -> Vec<(String, UsageWindow)> {
    limits
        .get(USAGE_MODEL_SCOPED)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let name = item.get(USAGE_DISPLAY_NAME).and_then(Value::as_str)?;
                    if name.is_empty() {
                        return None;
                    }
                    Some((name.to_string(), window(Some(item))?))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// ISO8601 → unix 秒。**statusline 側の `resets_at` は unix 秒だがこちらは文字列**なので、
/// 変換はここ 1 箇所に置く。読めない形は `None`（リセット時刻を出さないだけ）
fn parse_timestamp(text: &str) -> Option<u64> {
    let parsed = chrono::DateTime::parse_from_rfc3339(text).ok()?;
    u64::try_from(parsed.timestamp()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 実測した応答（v2.1.220）の形。**未公開の枠名は落とさずそのまま置いてある**:
    /// 決め打ちで読んでいないことをこのテストが示す
    fn sample() -> Value {
        serde_json::json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": "ccdesk-usage",
                "response": {
                    "subscription_type": "max",
                    "rate_limits_available": true,
                    "rate_limits": {
                        "five_hour": {
                            "utilization": 18,
                            "resets_at": "2026-07-27T17:30:00.301784+00:00"
                        },
                        "seven_day": {
                            "utilization": 55,
                            "resets_at": "2026-08-01T16:59:59.301816+00:00"
                        },
                        "seven_day_opus": null,
                        "tangelo": null,
                        "model_scoped": [
                            {"display_name": "Fable", "utilization": 0, "resets_at": null}
                        ]
                    }
                }
            }
        })
    }

    /// 実測の応答から 3 つの枠が取れる（リセット時刻は ISO8601 から unix 秒へ）
    #[test]
    fn the_measured_response_yields_all_three_windows() {
        let Usage::Ready(info) = parse_response(&sample(), 1_000) else {
            panic!("did not parse as Ready");
        };
        assert_eq!(info.five.as_ref().map(|w| w.pct), Some(18.0));
        assert_eq!(
            info.five.as_ref().and_then(|w| w.resets_at),
            Some(1_785_173_400)
        );
        assert_eq!(info.seven.as_ref().map(|w| w.pct), Some(55.0));
        assert_eq!(info.models.len(), 1);
        assert_eq!(info.models[0].0, "Fable");
        assert_eq!(info.models[0].1.pct, 0.0);
        // モデル別枠はリセット時刻を持たない（実測）ので、無いまま扱える
        assert_eq!(info.models[0].1.resets_at, None);
        assert_eq!(info.fetched_at, 1_000);
    }

    /// **枠が無いアカウントは故障ではない。** 恒久的に取れないので Unavailable へ
    /// 落とし、警告を出し続けない
    #[test]
    fn an_account_without_windows_is_unavailable_not_failed() {
        let v = serde_json::json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "response": { "rate_limits_available": false, "rate_limits": {} }
            }
        });
        assert_eq!(parse_response(&v, 0), Usage::Unavailable);
    }

    /// 形が変わった・エラー応答・封筒が違う — どれも Failed（黙って空にしない）
    #[test]
    fn a_broken_response_is_a_failure() {
        // 枠が 1 つも無いが、取れないアカウントだとも言っていない
        let shape_changed = serde_json::json!({
            "type": "control_response",
            "response": { "subtype": "success", "response": { "rate_limits": {} } }
        });
        assert_eq!(parse_response(&shape_changed, 0), Usage::Failed);

        let error = serde_json::json!({
            "type": "control_response",
            "response": { "subtype": "error", "error": "unknown subtype" }
        });
        assert_eq!(parse_response(&error, 0), Usage::Failed);

        let no_body = serde_json::json!({
            "type": "control_response",
            "response": { "subtype": "success" }
        });
        assert_eq!(parse_response(&no_body, 0), Usage::Failed);
    }

    /// 枠が片方だけ返る形（公式ドキュメントは各枠が独立に欠けうると明記）でも、
    /// 取れた枠は出す
    #[test]
    fn one_window_is_enough() {
        let v = serde_json::json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "response": {
                    "rate_limits_available": true,
                    "rate_limits": { "seven_day": {"utilization": 7} }
                }
            }
        });
        let Usage::Ready(info) = parse_response(&v, 0) else {
            panic!("did not parse as Ready");
        };
        assert!(info.five.is_none());
        assert_eq!(info.seven.as_ref().map(|w| w.pct), Some(7.0));
        // リセット時刻が無くても枠自体は成立する
        assert_eq!(info.seven.and_then(|w| w.resets_at), None);
    }

    /// 古さの判定は取得時刻 1 つから出る
    #[test]
    fn staleness_comes_from_the_fetch_time() {
        let info = UsageInfo {
            five: None,
            seven: None,
            models: Vec::new(),
            fetched_at: 1_000,
        };
        assert!(!info.is_stale(1_000 + STALE_AFTER_SECS));
        assert!(info.is_stale(1_000 + STALE_AFTER_SECS + 1));
        // 時計が巻き戻っても「未来だから古い」にはしない
        assert!(!info.is_stale(0));
    }

    /// 読めない時刻は落とすだけ（枠は生きる）
    #[test]
    fn an_unreadable_timestamp_is_dropped() {
        assert_eq!(parse_timestamp("2026-07-27T17:30:00+00:00"), Some(1_785_173_400));
        assert_eq!(parse_timestamp("Jul 28, 2:30am"), None);
        assert_eq!(parse_timestamp(""), None);
    }
}
