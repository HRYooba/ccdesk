//! codex の起こし方。
//!
//! **claude と決定的に違うのは ID の採番。** codex に `--session-id` 相当は無く
//! （`codex --help` の全項目で確認）、セッション ID は codex 自身が起動時に決める。
//! だから codex の新規起動は会話 ID を名乗れない（[`Spawn::conversation`] が None）。
//!
//! 行との相関は環境変数（[`crate::hooks::ROW_ENV`]）で、**立てるのは共通の口**
//! （[`crate::backend::Kind::spawn_command`]）。hook の子プロセスがそれを読んで
//! 「どの行の出来事か」を名乗る。**env は codex を経由して hook まで継承される**（実測）。
//!
//! 再開は `codex resume <uuid>` で、この `<uuid>` は **codex が採番した方**
//! （[`Launch::Resume`] の `id`）。ccdesk の行 ID ではない。
//!
//! **codex は opt-in**（`~/.ccdesk/config.json` の `"codex": "on"`。
//! [`crate::backend::Kind::enabled`]）。off の間はこのファイルの経路を
//! 1 度も通らない ＝ codex を入れていない環境でプロセスを起こさない。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::backend::{
    codex_app_server, codex_index, AgentVersion, Backend, Candidate, Inject, Kind, Launch, Garbage,
    Message, NameIndex, Span, Spawn,
};
use crate::hooks::{HOOK_EVENTS, HOOK_TIMEOUT_SECS};

const PROGRAM: &str = "codex";

/// 最新版の取得先。**npm registry の dist-tag `latest`** を見る
/// （`codex` は `@openai/codex` として配布され、`/latest` はその安定版 1 件を返す。
/// alpha 版は別の dist-tag なのでここには出ない）。
///
/// GitHub の releases ではなくこちらを使うのは、返る値が `codex --version` と
/// **同じ形**だから（GitHub のタグは `rust-v0.146.0` で、剥がし方が黙って腐る）
const LATEST_VERSION_API: &str = "https://registry.npmjs.org/@openai/codex/latest";

/// trust の確認を飛ばすフラグ。
///
/// **これを使わないとユーザーの設定を書き換えることになる。** codex には
/// `--settings` に相当する「この起動限りの注入口」が無く、`-c` で渡した hook にも
/// ハッシュ単位の trust が要る。承認すると `~/.codex/config.toml` に
/// `[hooks.state]` が書かれる（実測）。ccdesk は claude 側で「ユーザーの設定を
/// 1 バイトも書き換えない」を守っており、codex でも同じにする。
///
/// 代償: ペインに警告が毎回出る。ユーザー自身の hook も無審査で走る
const BYPASS_TRUST: &str = "--dangerously-bypass-hook-trust";

/// 終了系の hook に codex が許すタイムアウトの上限（秒）。
///
/// **超えると codex が黙って詰めたうえで警告を 1 行出す**（実測:
/// `clamping Interrupt hook timeout to 3s in …` / `clamping SessionEnd hook
/// timeout to 3s in …`）。ccdesk の hook は実測
/// 170〜190ms なので 3 秒で足り、警告を出させる理由が無い。
/// **中断・終了の経路なので codex 側が短く切っているのは妥当**
/// （ここで待たされると操作や終了が遅れる）
const TERMINAL_HOOK_TIMEOUT_SECS: u64 = 3;

/// この詰めが意味を持つのは共通のタイムアウトより短い間だけ。
/// **コンパイル時に固定する**（共通側を 3 秒以下へ下げたらここは要らなくなる）
const _: () = assert!(TERMINAL_HOOK_TIMEOUT_SECS < HOOK_TIMEOUT_SECS);

pub(crate) struct Codex;

impl Backend for Codex {
    fn command(&self, cwd: &str, launch: Launch<'_>, inject: Option<&Inject>) -> Spawn {
        let mut cmd = crate::backend::program(PROGRAM);
        let mut conversation = None;
        cmd.cwd(cwd);
        if let Some(inject) = inject
            && let Some(toml) = hook_toml(inject.exe)
        {
            cmd.arg("-c");
            cmd.arg(toml);
            cmd.arg(BYPASS_TRUST);
        }
        match launch {
            // **ID は渡さない**（`--session-id` 相当が codex に無い）。会話 ID は
            // codex が採番し、最初のターンまで存在しない ＝ 起こす前には知れない
            Launch::New { prompt } => {
                // 空プロンプトは渡さない（claude と同じ: 空メッセージを送った
                // セッションにしない）
                if !prompt.is_empty() {
                    cmd.arg(prompt);
                }
            }
            // **サブコマンドなので大域フラグより後ろ**（`codex -c … resume <id>`）
            Launch::Resume { id } => {
                cmd.arg("resume");
                cmd.arg(id);
                conversation = Some(id.to_string());
            }
            // 値なしの `resume` は codex 自身のピッカー（公式: "Resume a previous
            // interactive session (picker by default; use --last …)"）
            Launch::Pick => cmd.arg("resume"),
        }
        Spawn { cmd, conversation }
    }

    /// **rollout の末尾が現在値。** codex に claude の `status` 相当のファイルは
    /// 無いが、rollout は turn の始まりと終わりを順に持つ（[`LIFECYCLE`]）。
    ///
    /// **これが無いと Esc 中断が永久に赤で残る**（報告された症状）: 中断のとき
    /// codex は `Stop` を撃たず（[codex#22858](https://github.com/openai/codex/issues/22858)）、
    /// hook はイベントなので誰も降ろせない。記録は turn が終われば必ず
    /// `turn_aborted` を書くので、次の走査で必ず正しくなる
    fn record_states(&self) -> &'static [crate::backend::Mark] {
        &LIFECYCLE
    }

    /// rollout は**全行が同じ封筒**（`{"timestamp":…,"type":…,"payload":…}`）を
    /// 持つので、行の種類を問わず 1 つの読み方で時刻が出る
    fn record_time(&self, value: &serde_json::Value) -> Option<u64> {
        record_time(value.get("timestamp")?.as_str()?)
    }

    /// 最新版は codex の配布元（npm registry の `@openai/codex`、dist-tag `latest`）
    /// へ**毎回問い合わせる**。claude と同じ流儀で、取れなければ「更新あり」を出さない。
    ///
    /// **`$CODEX_HOME/version.json` は読まない。** あれは codex 自身が更新チェックを
    /// した時刻の値で、codex を起こさない限り古いまま止まる ＝ 新しい版が出ても
    /// 版行が黙る。鮮度を判定する材料（`last_checked_at`）を足すより、claude と
    /// 同じく取得のたびにネットワークへ出るほうが知識源が 1 つで済む
    fn version(&self) -> AgentVersion {
        // 現行版: "codex-cli 0.146.0" の末尾トークン
        let current = crate::poll::out(PROGRAM, &["--version"])
            .and_then(|s| s.split_whitespace().last().map(str::to_string))
            .unwrap_or_default();
        // ネットワークへ出る作法（タイムアウト等）は [`crate::update::http_get`] が持つ
        let latest = crate::update::http_get(LATEST_VERSION_API)
            .as_deref()
            .and_then(parse_latest_version)
            .filter(|l| !current.is_empty() && ccdesk::version_newer(l, &current));
        AgentVersion { current, latest }
    }

    fn update_argv(&self) -> (&'static str, &'static [&'static str]) {
        (PROGRAM, &["update"])
    }

    /// `codex update` は npm を通るので、**走っている codex が掴んでいる
    /// `codex.exe` を unlink できないと npm が作業ディレクトリごと諦める**
    /// （実測 2026-08-27: `EPERM` を warn 扱いにして exit 0 を返し、
    /// `@openai/.codex-<ハッシュ>` が数百 MB 残った）。npm 自身は次の更新でも
    /// 消しに来ない。
    ///
    /// 拾うのは `@openai/` 直下の**先頭ドット付き**のものだけ。npm の作業用の
    /// 名前で、正規のインストール（`@openai/codex`）とはドットの有無で必ず分かれる。
    ///
    /// **塞ぐ**（[`Garbage::blocks_next_update`]）
    fn garbage(&self) -> Vec<Garbage> {
        ccdesk::resolve_program(PROGRAM)
            .and_then(|shim| npm_scope_beside(&shim))
            .map(|dir| Garbage {
                dir,
                prefix: ".codex-".to_string(),
                rest_ok: Box::new(|rest| rest.chars().all(|c| c.is_ascii_alphanumeric())),
                blocks_next_update: true,
            })
            .into_iter()
            .collect()
    }

    /// app-server へ 1 往復（[`codex_app_server`]）。**rollout は読まない**
    /// （あちらは最後にセッションが動いた時点の値で、現在値ではない）
    fn usage(&self) -> crate::usage::Usage {
        match codex_app_server::rate_limits(PROGRAM, ccdesk::now_secs()) {
            Some(info) => crate::usage::Usage::Ready(info),
            None => crate::usage::Usage::Failed,
        }
    }

    /// app-server の `account/read`（使用率と同じ経路）。**claude と違って
    /// 表示名を持たない**ので、身元として出せるのはメールアドレス
    fn account(&self) -> crate::poll::AccountStatus {
        codex_app_server::account(PROGRAM)
    }

    /// `$CODEX_HOME/auth.json` の指紋。ログイン・ログアウト・トークン更新で
    /// 書き換わる（実測でも `last_refresh` が載る）
    fn auth_fingerprint(&self) -> crate::poll::CredentialsFp {
        codex_index::auth_fingerprint()
    }

    fn transcript_root(&self) -> Option<PathBuf> {
        Some(codex_index::codex_home()?.join("sessions"))
    }

    /// rollout は `sessions/YYYY/MM/DD/rollout-<現地時刻>-<会話 ID>.jsonl`。
    ///
    /// **ファイル名の時刻部分を組み立て直さない。** 書いた時のタイムゾーンに
    /// 依存するので、組み立てた名前は環境が変わると黙って外れる。会話 ID は
    /// **UUIDv7**（先頭 48bit が生成時刻の ms）なので、そこから**日のディレクトリ
    /// だけ**を導き、その日と前後 1 日の中から末尾が `-<会話 ID>.jsonl` の
    /// ファイルを探す。時計の仮定は「どの日か」だけに縮む。
    ///
    /// 前後 1 日も見るのは、ファイル名が**現地時刻**でディレクトリを決めるため
    /// （UTC から導いた日と最大 1 日ずれる）
    fn transcript_in(&self, root: &Path, conversation: &str, _cwd: &str) -> Option<PathBuf> {
        let suffix = format!("-{conversation}.jsonl");
        let day = codex_index::minted_at_days(conversation)?;
        // その日 → 前日 → 翌日 の順（同名は在り得ないので見つかった時点で確定）
        [0i64, -1, 1].into_iter().find_map(|shift| {
            let dir = root.join(codex_index::day_path(day.checked_add(shift)?)?);
            std::fs::read_dir(dir)
                .ok()?
                .flatten()
                .map(|entry| entry.path())
                .find(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.ends_with(&suffix))
                })
        })
    }

    /// **行の cwd でそのまま再開できる**（`codex resume <uuid>` は会話を ID で
    /// 名指しするので、打つ場所を選ばない）。記録の場所も見ない
    fn resume_cwd(&self, cwd: &str, _transcript: Option<&Path>) -> Option<String> {
        Some(cwd.to_string())
    }

    /// **索引で拾えない会話のための下段**（[`Self::name_index`] が主）。
    ///
    /// 索引に載るのは `/rename` された会話だけ（実機で確認: 索引ファイルは
    /// リネームが 1 度も無いと存在すらしない）。残りは rollout の
    /// **ユーザー発話**で名乗る ＝ claude が `last-prompt` へ落ちるのと同じ形。
    ///
    /// **同じ記録を 2 つの範囲で読む**のが codex の事情:
    ///
    /// | 順 | 範囲 | 拾えるもの |
    /// |:--|:--|:--|
    /// | 1 | [`Span::Appended`]（末尾窓） | **最後の**打鍵 ＝ 今やらせていること |
    /// | 2 | [`Span::Head`]（先頭窓） | 最初の打鍵（末尾窓に発話が無いとき） |
    ///
    /// **1 が無いと名前が最初の打鍵で固定される。** claude の行は打つたびに
    /// 最新へ動くのに codex の行だけ動かず、「名前が変わらない」として報告された。
    ///
    /// **2 を捨てられないのは、末尾窓に発話が届かない会話があるから**: codex の
    /// rollout は道具の出力が末尾を埋める（実測 60%）。先頭は `session_meta` →
    /// 前置き → 最初のプロンプトと決まった形で、先頭 256 KiB に 213/214（99.5%）が
    /// 収まる。**先頭は追記されない**ので 1 会話 1 回の有界読みで済む
    fn title_records(&self) -> &'static [Candidate] {
        &TITLE_RECORDS
    }

    fn name_index(&self) -> Option<NameIndex> {
        codex_index::name_index()
    }

    /// 記録の 1 行から発言を取る。**2 つの形を両方読む**（理由は [`TITLE_RECORDS`]）。
    ///
    /// **`response_item` は見ない。** 表示名のときと同じ理由で、あちらには
    /// AGENTS.md や permissions の前置き・道具の出入りが同じ形で並ぶ。
    /// `event_msg` は codex が画面に出した出来事なので、ここに載るのは人が読む
    /// 発言だけになる
    fn message(&self, value: &serde_json::Value) -> Option<Message> {
        let payload = value.get("payload")?;
        let (from_user, text) = match payload.get("type").and_then(serde_json::Value::as_str)? {
            // 今の形（0.150〜）: 発言は完了した item として載る
            ITEM_COMPLETED => {
                let item = payload.get("item")?;
                let from_user = match item.get("type").and_then(serde_json::Value::as_str)? {
                    USER_ITEM => true,
                    AGENT_ITEM => false,
                    _ => return None,
                };
                (from_user, item_text(item)?)
            }
            // 旧い形（〜0.149）: 発言が payload に平らに載る
            USER_MESSAGE => (true, payload.get("message")?.as_str()?.to_string()),
            AGENT_MESSAGE => (false, payload.get("message")?.as_str()?.to_string()),
            _ => return None,
        };
        (!text.trim().is_empty()).then_some(Message { from_user, text })
    }
}

/// 表示名の候補。**索引（`thread_name`）が上、これは下段**（[`Backend::title_records`]）。
///
/// **rollout の形が 0.150 で変わった**（実測 2026-08-28 / codex 0.150.1）。
/// ユーザーの打鍵は
/// `{"payload":{"type":"user_message","message":…}}` から
/// `{"payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"text":…}]}}}`
/// へ移り、**旧い型名は 1 件も出なくなった**（直近 2 日の rollout 46 本で
/// `user_message` は 0 件）＝ 索引に載らない会話（`/rename` していないもの）は
/// **全部 `new session` のまま**になっていた。
///
/// **両方を残す。** 記録は消えないので、古い会話は古い形のまま在り続ける。
/// 取り出しはどちらも [`user_prompt`] が吸収するので、候補が分かれるのは
/// **足切りの印が別の文字列だから**だけ（[`Candidate::marker`] は 1 本しか持てない）。
///
/// 並びがそのまま優先順 ＝ 末尾（今やらせていること）で拾えたら先頭は見ない
static TITLE_RECORDS: [Candidate; 4] = [
    Candidate { marker: USER_ITEM, text: user_prompt, span: Span::Appended },
    Candidate { marker: USER_MESSAGE, text: user_prompt, span: Span::Appended },
    Candidate { marker: USER_ITEM, text: user_prompt, span: Span::Head },
    Candidate { marker: USER_MESSAGE, text: user_prompt, span: Span::Head },
];

/// 発言を運ぶ**今の**行の型名（0.150〜）。中身は [`USER_ITEM`] / [`AGENT_ITEM`]
const ITEM_COMPLETED: &str = "item_completed";

/// ユーザーの打鍵を運ぶ item の型名（0.150〜）。
///
/// **`role: "user"` の `response_item` は使わない。** あちらには AGENTS.md や
/// permissions の前置きも同じ形で入るので、名前にすると前置きが行に出る
const USER_ITEM: &str = "UserMessage";

/// codex の答えを運ぶ item の型名（0.150〜）。**表示名には使わない**
/// （名前は打鍵から採る）が、会話を読むには要る（[`Backend::message`]）
const AGENT_ITEM: &str = "AgentMessage";

/// 旧い形（〜0.149）でユーザーの打鍵を運んだ行の型名
const USER_MESSAGE: &str = "user_message";

/// 旧い形（〜0.149）で codex の答えを運んだ行の型名
const AGENT_MESSAGE: &str = "agent_message";

/// 記録の 1 行から打鍵を取り出す。**新旧どちらの形でも同じ答えを返す**
/// （[`TITLE_RECORDS`]）
fn user_prompt(value: &serde_json::Value) -> Option<&str> {
    let payload = value.get("payload")?;
    match payload.get("type")?.as_str()? {
        ITEM_COMPLETED => {
            let item = payload.get("item")?;
            (item.get("type")?.as_str()? == USER_ITEM).then(|| first_text(item))?
        }
        USER_MESSAGE => payload.get("message")?.as_str(),
        _ => None,
    }
}

/// item の本文ブロックのうち**最初の中身のある `text`**。
///
/// **ブロックの型名では選ばない。** codex は同じ `content` の中で綴りを揃えて
/// いない（実測: ユーザー側は `"type":"text"`、codex 側は `"type":"Text"`）ので、
/// 型名で絞ると片側だけが拾えなくなる。持っている値（`text`）で選ぶ
fn first_text(item: &serde_json::Value) -> Option<&str> {
    item.get("content")?
        .as_array()?
        .iter()
        .filter_map(|block| block.get("text")?.as_str())
        .find(|text| !text.trim().is_empty())
}

/// item の本文ブロックを**全部**つないだもの（[`Backend::message`] 用）。
/// 表示名は 1 行に畳むので先頭 1 つで足りるが、会話を読むほうは落とせない
fn item_text(item: &serde_json::Value) -> Option<String> {
    let joined = item
        .get("content")?
        .as_array()?
        .iter()
        .filter_map(|block| block.get("text")?.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    (!joined.trim().is_empty()).then_some(joined)
}

/// rollout が turn の切れ目に書く行（[`Backend::record_states`]）。
///
/// **`task_started` と、その turn が終わったことを言う 2 つ**で閉じる（実測
/// 2026-08-28 / codex 0.150.1: 直近 2 日で 46 = 36 + 10 と過不足なく対応した）。
/// 中断（`turn_aborted`）も**手が空いた**という意味では完了と同じ Idle:
/// 「終わったのか止めたのか」は通知が見る区別で、それは hook の表
/// （[`HOOK_EVENTS`]）が持つ ＝ ここは色が答えることだけを答える
static LIFECYCLE: [crate::backend::Mark; 3] = [
    crate::backend::Mark { marker: TASK_STARTED, read: task_started },
    crate::backend::Mark { marker: TASK_COMPLETE, read: task_complete },
    crate::backend::Mark { marker: TURN_ABORTED, read: turn_aborted },
];

/// turn が始まった
const TASK_STARTED: &str = "task_started";
/// turn が最後まで終わった
const TASK_COMPLETE: &str = "task_complete";
/// turn が中断された（`reason: "interrupted"` ＝ Esc）
const TURN_ABORTED: &str = "turn_aborted";

fn task_started(value: &serde_json::Value) -> Option<(crate::poll::State, u64)> {
    lifecycle(value, TASK_STARTED, crate::poll::State::Working)
}

fn task_complete(value: &serde_json::Value) -> Option<(crate::poll::State, u64)> {
    lifecycle(value, TASK_COMPLETE, crate::poll::State::Idle)
}

fn turn_aborted(value: &serde_json::Value) -> Option<(crate::poll::State, u64)> {
    lifecycle(value, TURN_ABORTED, crate::poll::State::Idle)
}

/// その行が `payload.type == kind` なら (state, 記録された時刻)。
///
/// **時刻は封筒の `timestamp`**（RFC3339）で、payload の中の `started_at` /
/// `completed_at` ではない: あちらは行ごとに綴りも粒度（秒）も違うが、封筒は
/// rollout の全行が同じ形で持つ ＝ 1 つの読み方で全部に効く
fn lifecycle(
    value: &serde_json::Value,
    kind: &str,
    state: crate::poll::State,
) -> Option<(crate::poll::State, u64)> {
    (value.get("payload")?.get("type")?.as_str()? == kind).then_some(())?;
    Some((state, record_time(value.get("timestamp")?.as_str()?)?))
}

/// rollout の封筒の時刻（`"2026-08-28T03:38:32.547Z"`）→ epoch ms。
/// 読めなければ None ＝ その行は現在値として使わない（誤った時刻で hook に
/// 勝たせるより、材料を 1 つ落とすほうが害が小さい）
fn record_time(text: &str) -> Option<u64> {
    let at = chrono::DateTime::parse_from_rfc3339(text).ok()?;
    u64::try_from(at.timestamp_millis()).ok()
}

/// `-c` に渡す hook の定義（TOML）。**注入できない形なら None**（hook 無しで
/// 起動する ＝ 行の状態が縮退するだけで、セッション自体は動く）。
///
/// **TOML のリテラル文字列（`'…'`）で組む。** 二重引用符で囲むと
/// npm shim → cmd → exe のどこかで食われ、値が TOML として解釈されず
/// 「ただの文字列」として渡る（実測。codex は
/// `invalid type: string …, expected struct HooksToml` で落ちる）。
/// リテラル文字列にはエスケープが無いので、パスに `'` を含む環境では組めない
fn hook_toml(exe: &str) -> Option<String> {
    let exe = command_word(exe)?;
    let exe = exe.as_str();
    // イベント名をキーに配列へ足し込む（同名イベントが 2 枚あっても後着が
    // 前着を潰さない。並びを固定するため BTreeMap を使う ＝ 同じ入力で同じ文字列）
    let mut by_event: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for row in HOOK_EVENTS.iter().filter(|row| row.has(Kind::Codex)) {
        // 中断・終了系は codex 側の上限が短い
        // （[`TERMINAL_HOOK_TIMEOUT_SECS`]）
        let timeout = if matches!(row.event, "Interrupt" | "SessionEnd") {
            HOOK_TIMEOUT_SECS.min(TERMINAL_HOOK_TIMEOUT_SECS)
        } else {
            HOOK_TIMEOUT_SECS
        };
        // **alert まで運ぶ。** 同じ (event, state) でも呼び出しは agent ごとに
        // 違う（`PermissionRequest` は codex だけが開く）＝ hook の子プロセスは
        // 自分がどちらの下に居るかを知らないので、注入する側が答えを載せる
        // （[`crate::hooks::Alert`]）
        by_event.entry(row.event).or_default().push(format!(
            "{{hooks=[{{type='command',command='{exe} hook {} {} {}',timeout={timeout}}}]}}",
            row.event,
            row.state.as_str(),
            row.alert.as_str(),
        ));
    }
    if by_event.is_empty() {
        return None;
    }
    let body = by_event
        .into_iter()
        .map(|(event, entries)| format!("{event}=[{}]", entries.join(",")))
        .collect::<Vec<_>>()
        .join(",");
    Some(format!("hooks={{{body}}}"))
}

/// hook のコマンド行の先頭に**裸で**置ける形の exe パス。置けなければ None。
///
/// **二重引用符で囲めない。** 囲むと npm の `.cmd` シムを通る間に `""` へ
/// 二重化され（実測 2026-08-02 / codex 0.146.0）、codex はその名前の
/// プログラムを起こそうとして失敗する（ペインに
/// `hook exited with code 1` が並び、hook は 1 度も起動されない）。
/// codex 側は配列（argv）も受けない（`invalid type: sequence, expected a string`）。
///
/// 囲めない以上、パスに空白があってはいけない。空白があるときは Windows の
/// 8.3 短縮名へ落とす（[`ccdesk::short_path`]）。それでも残るなら諦める ＝
/// hook 無しで起動する（行の状態が縮退するだけで、セッションは動く）。
/// `'` はリテラル文字列を閉じてしまうので同じく諦める
fn command_word(exe: &str) -> Option<String> {
    let usable = |path: &str| !path.contains(' ') && !path.contains('\'');
    if usable(exe) {
        return Some(exe.to_string());
    }
    ccdesk::short_path(exe).filter(|short| usable(short))
}

/// [`LATEST_VERSION_API`] の応答から版番号を取り出す。
///
/// **3 パート以上の版番号だけを通す**（claude 側の同じ判定と揃える）。応答が
/// エラー JSON・プロキシの HTML に化けたときに、それを「新しい版」として
/// 版行へ出さないための入口の関門
fn parse_latest_version(body: &str) -> Option<String> {
    let version = serde_json::from_str::<serde_json::Value>(body)
        .ok()?
        .get("version")?
        .as_str()?
        .trim()
        .to_string();
    (version.split('.').count() >= 3).then_some(version)
}

/// npm のツリーで `@openai/` に当たるディレクトリ。**組み立てはここ 1 箇所**
/// （残骸の走査と、退かした置き場の走査が別々の綴りを持たない）。
/// npm はシムの隣に node_modules を置く（`npm prefix -g` の中身そのもの）
fn npm_scope_beside(shim: &Path) -> Option<PathBuf> {
    Some(shim.parent()?.join("node_modules").join("@openai"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::tests::argv;

    /// **npm の作業ディレクトリだけを拾い、正規のインストールには触れない。**
    ///
    /// `codex update` が EPERM で諦めると `@openai/.codex-<ハッシュ>` が残る。
    /// 拾い方を間違えると **codex 本体を消す**ので、隣に正規のインストールを
    /// 置いた状態で固定する（両者はドットの有無だけで分かれる）
    #[test]
    fn only_the_npm_workdirs_beside_the_real_install_are_collected() {
        let dir = crate::testutil::TempDir::new("codex", "npm-workdirs");
        let scope = dir.join("node_modules").join("@openai");
        for name in [
            "codex",           // 正規のインストール
            "codex-win32-x64", // 正規のプラットフォーム別パッケージ
            ".codex-g3ieL94X", // npm の作業場（消す）
            ".codex-AbC123",   // 同上
            ".codex-",         // ハッシュが無い ＝ 何か分からないので残す
            ".codex-has.dot",  // ハッシュでない ＝ 残す
            ".ccdesk-held",    // 退かした残骸の置き場（掃除が別で見る）
        ] {
            std::fs::create_dir_all(scope.join(name)).unwrap();
        }
        let spec = Garbage {
            dir: npm_scope_beside(&dir.join("codex.cmd")).unwrap(),
            prefix: ".codex-".to_string(),
            rest_ok: Box::new(|rest| rest.chars().all(|c| c.is_ascii_alphanumeric())),
            blocks_next_update: true,
        };
        let mut found: Vec<String> =
            crate::backend::leftovers_in(&spec.dir, &spec.prefix, &spec.rest_ok)
                .into_iter()
                .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                .collect();
        found.sort();
        assert_eq!(found, [".codex-AbC123", ".codex-g3ieL94X"]);
        // npm ツリーが無い置き方（PATH に codex が無い / 別の入れ方）でも落ちない
        let missing = npm_scope_beside(&dir.join("elsewhere").join("codex.cmd")).unwrap();
        assert!(crate::backend::leftovers_in(&missing, ".codex-", |_| true).is_empty());
    }

    fn inject() -> Inject<'static> {
        Inject {
            exe: "C:/Users/me/ccdesk.exe",
            settings: std::path::Path::new("C:\\Users\\me\\.ccdesk\\inject-settings.json"),
        }
    }

    fn build(launch: Launch<'_>, inject: Option<&Inject>) -> Spawn {
        Codex.command("C:\\dev\\app", launch, inject)
    }

    /// 新規はプロンプトだけ。**`--session-id` に相当するものは渡さない**
    /// （codex に無い ＝ 渡すと起動が落ちる）。会話 ID は codex が採番するので、
    /// **起こす前には名乗れない**（hook が来るまで行は会話を持たない）
    #[test]
    fn a_new_session_passes_only_the_prompt_and_claims_no_conversation() {
        let spawn = build(
            Launch::New {
                prompt: "fix login form validation",
            },
            None,
        );
        assert_eq!(argv(&spawn.cmd), ["fix login form validation"]);
        assert_eq!(spawn.conversation, None);
        assert_eq!(
            spawn.cmd.get_cwd().map(|c| c.to_string_lossy().to_string()),
            Some("C:\\dev\\app".to_string()),
            "cwd is not passed through"
        );
        assert!(argv(&build(Launch::New { prompt: "" }, None).cmd).is_empty());
    }

    /// 再開は **codex が採番した ID**（行 ID ではない）。取り違えると
    /// `codex resume` が別の会話を開く / 見つからずに落ちる
    #[test]
    fn resuming_uses_the_id_codex_minted() {
        let spawn = build(Launch::Resume { id: "019fbe35-5ffa" }, None);
        assert_eq!(argv(&spawn.cmd), ["resume", "019fbe35-5ffa"]);
        assert_eq!(spawn.conversation.as_deref(), Some("019fbe35-5ffa"));
    }

    /// **ピッカーには ID を渡さない**（claude と同じ扱い）。値なしの `resume` が
    /// codex 自身のピッカーで、選ぶのはユーザー
    #[test]
    fn picking_passes_no_id_and_claims_no_conversation() {
        let spawn = build(Launch::Pick, None);
        assert_eq!(argv(&spawn.cmd), ["resume"]);
        assert_eq!(spawn.conversation, None);
    }

    /// hook は `-c` で 1 起動限り渡し、trust の確認を飛ばす（ユーザーの
    /// `~/.codex/config.toml` を書き換えないため）。**サブコマンドより前**に置く
    #[test]
    fn the_hooks_are_injected_before_the_subcommand() {
        let inject = inject();
        let args = argv(&build(Launch::Resume { id: "abc" }, Some(&inject)).cmd);
        assert_eq!(args[0], "-c");
        assert!(args[1].starts_with("hooks={"), "{args:?}");
        assert_eq!(args[2], BYPASS_TRUST);
        assert_eq!(&args[3..], ["resume", "abc"]);
    }

    /// **claude 専用のイベントを載せない。** 知らない名前があると codex は設定ごと
    /// 読み込みに失敗し、hook が 1 つも効かなくなる。
    ///
    /// **誰が持つかの正本は [`HOOK_EVENTS`] の 1 表**（かつてはここに部分集合の
    /// 一覧を別に持っていて、表と 2 箇所で食い違い得た）。
    ///
    /// **問うのは行ごとではなくイベントごと。** 同じイベントが agent 別に
    /// 2 枚並ぶことがある（`PermissionRequest` は claude 用と codex 用で
    /// `alert` だけが違う）ので、行ごとに聞くと codex 用の 1 枚で載った
    /// イベントを claude 用の 1 枚が「載っていないはず」と言うことになる
    #[test]
    fn only_the_events_codex_has_are_injected() {
        let toml = hook_toml("C:/ccdesk.exe").expect("no hooks were built");
        for row in HOOK_EVENTS {
            let present = toml.contains(&format!("{}=[", row.event));
            let wanted = HOOK_EVENTS
                .iter()
                .any(|other| other.event == row.event && other.has(Kind::Codex));
            assert_eq!(
                present,
                wanted,
                "{} is {} in the injected TOML",
                row.event,
                if present { "present" } else { "missing" }
            );
        }
    }

    /// **codex の `PermissionRequest` は呼び出しを開いたまま**（`needs_input` を
    /// 引数として運ぶ）。claude 側は 6 秒ゲート付きの `Notification` が呼び出しを
    /// 開くのでこの hook を `silent` へ降ろしたが、codex にその受け皿は無く、
    /// ここを黙らせると**許可待ちが 1 度も通知されない**
    #[test]
    fn the_codex_permission_hook_still_opens_a_call() {
        let row = HOOK_EVENTS
            .iter()
            .find(|row| row.event == "PermissionRequest" && row.has(Kind::Codex))
            .expect("codex lost its permission hook");
        assert_eq!(
            row.alert.kind(),
            Some(crate::notify::Kind::NeedsInput),
            "codex's permission wait no longer calls the user"
        );
        // その答えが実際にコマンドへ載る（載らなければ受け口は静かな方を採る）
        let toml = hook_toml("C:/ccdesk.exe").expect("no hooks were built");
        assert!(
            toml.contains(&format!(
                "hook {} {} {}",
                row.event,
                row.state.as_str(),
                row.alert.as_str()
            )),
            "{toml}"
        );
    }

    /// **Esc 中断は `Interrupt` が名乗る**（codex 0.150 で増えたイベント）。
    /// これが落ちると中断の 0 遅延の材料が消え、記録の走査（1 周期の遅れ）
    /// だけが Working を降ろすことになる
    #[test]
    fn the_interrupt_event_is_injected() {
        let toml = hook_toml("C:/ccdesk.exe").expect("no hooks were built");
        assert!(toml.contains("Interrupt=["), "{toml}");
    }

    /// **注入する値に二重引用符を 1 つも出さない。**
    ///
    /// 理由が 2 つあり、どちらも実測:
    ///
    /// 1. TOML の構文として使うと、npm shim → cmd → exe のどこかで食われて値が
    ///    文字列として渡り、codex が `invalid type: string …` で落ちる
    /// 2. exe を囲む用途で使うと、`.cmd` のシムを通る間に `""` へ二重化され、
    ///    codex はその名前のプログラムを起こそうとして失敗する（ペインに
    ///    `hook exited with code 1` が並び、hook は 1 度も起動されない）
    ///
    /// 「構文としては使っていない」では 2 を防げないので、**1 つも出さない**で固定する
    #[test]
    fn the_injected_value_contains_no_double_quote_at_all() {
        let toml = hook_toml("C:/ccdesk.exe").expect("no hooks were built");
        assert!(
            !toml.contains('"'),
            "a double quote slipped in; the .cmd shim doubles it: {toml}"
        );
        assert!(toml.contains("type='command'"), "{toml}");
        assert!(toml.contains("command='C:/ccdesk.exe hook "), "{toml}");
    }

    /// **空白のある exe パスを裸で置かない。** 囲む手段が無い以上、置けば
    /// コマンドが途中で切れて別のプログラムを起こそうとする。
    /// 8.3 短縮名へ落とせなければ注入ごと諦める（[`command_word`]）
    #[test]
    fn a_path_with_a_space_is_never_emitted_as_it_is() {
        // 実在しないパスは短縮名も取れない ＝ 注入しない
        assert_eq!(hook_toml("C:/no such dir/ccdesk.exe"), None);
        // 実在して空白を含むパスは、空白の無い別名になって初めて出る
        if let Some(short) = ccdesk::short_path(&spaced_exe()) {
            let toml = hook_toml(&spaced_exe()).expect("a shortenable path was refused");
            assert!(!toml.contains(' ') || toml.contains(&short), "{toml}");
            assert!(
                !toml.contains("/ccdesk with space/"),
                "the raw path with a space was emitted: {toml}"
            );
        }
    }

    /// 空白を含む実在のパス（8.3 が無効なボリュームでは短縮名が取れない ＝
    /// そのときこのテストは注入を諦める側だけを見る）
    fn spaced_exe() -> String {
        let dir = std::env::temp_dir().join("ccdesk with space");
        let _ = std::fs::create_dir_all(&dir);
        let exe = dir.join("ccdesk.exe");
        let _ = std::fs::write(&exe, b"stub");
        exe.to_string_lossy().replace('\\', "/")
    }

    /// **codex が詰め直す形で渡さない。** 上限を超えると codex は黙って詰めたうえで
    /// 警告を 1 行出し、それがセッションのたびにペインへ残る（実測:
    /// `clamping Interrupt hook timeout to 3s in …` / `clamping SessionEnd hook
    /// timeout to 3s in …`）
    #[test]
    fn terminal_hooks_stay_within_the_timeout_codex_allows() {
        let toml = hook_toml("C:/ccdesk.exe").expect("no hooks were built");
        for event in ["Interrupt", "SessionEnd"] {
            let at = toml
                .find(&format!("{event}="))
                .unwrap_or_else(|| panic!("{event} is not injected"));
            assert!(
                toml[at..].contains(&format!("timeout={TERMINAL_HOOK_TIMEOUT_SECS}")),
                "{event} asks for a timeout codex will clamp: {}",
                &toml[at..]
            );
        }
    }

    /// リテラル文字列にエスケープは無い（`'` は文字列を閉じてしまう）。
    /// **組めないなら hook 無しで起動する**（壊れた TOML を渡して codex ごと
    /// 起動不能にしない）
    #[test]
    fn a_path_that_cannot_be_placed_bare_drops_the_injection_instead_of_breaking_it() {
        assert_eq!(hook_toml("C:/it's here/ccdesk.exe"), None);
        let inject = Inject {
            exe: "C:/it's here/ccdesk.exe",
            settings: std::path::Path::new("x"),
        };
        assert!(argv(&build(Launch::New { prompt: "" }, Some(&inject)).cmd).is_empty());
    }

    fn message_of(line: &str) -> Option<Message> {
        Codex.message(&serde_json::from_str(line).expect("the test wrote invalid JSON"))
    }

    /// **0.150 の形**（実測 2026-08-28 / codex 0.150.1）。発話は完了した item として
    /// 載り、本文ブロックの型名は**ユーザー側と codex 側で綴りが違う**
    /// （`"text"` と `"Text"`）ので、型名ではなく `text` の有無で選ぶ
    #[test]
    fn the_current_shape_of_a_rollout_is_read() {
        assert_eq!(
            message_of(
                r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"text","text":"run the tests","text_elements":[]}]}}}"#
            ),
            Some(Message { from_user: true, text: "run the tests".to_string() })
        );
        assert_eq!(
            message_of(
                r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","content":[{"type":"Text","text":"they pass"}]}}}"#
            ),
            Some(Message { from_user: false, text: "they pass".to_string() })
        );
        // 本文ブロックを持たない item は発言ではない（道具・思考は同じ形で並ぶ）
        for line in [
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"Reasoning","summary_text":[],"raw_content":[]}}}"#,
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","command":"ls"}}}"#,
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"text","text":"  "}]}}}"#,
        ] {
            assert_eq!(message_of(line), None, "{line}");
        }
    }

    /// **表示名は新旧どちらの形でも打鍵から採れる。**
    ///
    /// 0.150 で `user_message` が消え（直近 2 日の rollout 46 本で 0 件）、
    /// 索引（`session_index.jsonl`）に載るのは `/rename` した会話だけなので、
    /// ここが外れると **`/rename` していない codex の行は全部 `new session`** に
    /// なる（報告された症状）。記録は消えないので旧い形も読み続ける
    #[test]
    fn a_prompt_is_found_in_both_shapes_of_the_record() {
        let prompt = |line: &str| {
            let value: serde_json::Value = serde_json::from_str(line).expect("invalid JSON");
            user_prompt(&value).map(str::to_string)
        };
        assert_eq!(
            prompt(r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"text","text":"fix the login form"}]}}}"#)
                .as_deref(),
            Some("fix the login form")
        );
        assert_eq!(
            prompt(r#"{"type":"event_msg","payload":{"type":"user_message","message":"fix the login form","images":[]}}"#)
                .as_deref(),
            Some("fix the login form")
        );
        // codex の答えは名前にしない（名前は打鍵から採る）
        assert_eq!(
            prompt(r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","content":[{"type":"Text","text":"done"}]}}}"#),
            None
        );
        // **両方の形の印が候補に載っている**（印は 1 候補 1 本しか持てないので、
        // どちらかを落とすとその形の記録が丸ごと拾えなくなる）
        for marker in [USER_ITEM, USER_MESSAGE] {
            for span in [Span::Appended, Span::Head] {
                assert!(
                    TITLE_RECORDS
                        .iter()
                        .any(|c| c.marker == marker && c.span == span),
                    "{marker} is not searched in {span:?}"
                );
            }
        }
    }

    /// **turn の切れ目が現在値になる。** 時刻は封筒の `timestamp` から採る
    /// （payload 側の `started_at` / `completed_at` は行ごとに綴りも粒度も違う）。
    ///
    /// 中断（`turn_aborted`）が Idle なのが要点で、codex は中断のとき `Stop` を
    /// 撃たない ＝ これが無いと行が永久に Working で固着する（報告された症状）
    #[test]
    fn the_turn_boundaries_in_a_rollout_are_the_current_state() {
        let read = |line: &str| {
            let value: serde_json::Value = serde_json::from_str(line).expect("invalid JSON");
            LIFECYCLE.iter().find_map(|mark| (mark.read)(&value))
        };
        assert_eq!(
            read(r#"{"timestamp":"2026-08-28T03:38:31.000Z","type":"event_msg","payload":{"type":"task_started","turn_id":"t","started_at":1787888311}}"#),
            Some((crate::poll::State::Working, 1_787_888_311_000))
        );
        assert_eq!(
            read(r#"{"timestamp":"2026-08-28T03:38:32.547Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"t","last_agent_message":"done"}}"#),
            Some((crate::poll::State::Idle, 1_787_888_312_547))
        );
        assert_eq!(
            read(r#"{"timestamp":"2026-08-28T03:38:33.000Z","type":"event_msg","payload":{"type":"turn_aborted","turn_id":"t","reason":"interrupted"}}"#),
            Some((crate::poll::State::Idle, 1_787_888_313_000))
        );
        // 状態を語らない行・時刻の読めない行は現在値にしない
        for line in [
            r#"{"timestamp":"2026-08-28T03:38:31.000Z","type":"event_msg","payload":{"type":"token_count","info":{}}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"t"}}"#,
            r#"{"timestamp":"not a time","type":"event_msg","payload":{"type":"task_complete","turn_id":"t"}}"#,
        ] {
            assert_eq!(read(line), None, "{line}");
        }
    }

    /// 旧い形（〜0.149）の発言も読み続ける（記録は消えない）
    #[test]
    fn both_sides_of_the_conversation_are_read() {
        assert_eq!(
            message_of(
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"run the tests","images":[]}}"#
            ),
            Some(Message { from_user: true, text: "run the tests".to_string() })
        );
        assert_eq!(
            message_of(
                r#"{"type":"event_msg","payload":{"type":"agent_message","message":"they pass"}}"#
            ),
            Some(Message { from_user: false, text: "they pass".to_string() })
        );
    }

    /// **`response_item` は読まない。** ここには AGENTS.md や permissions の
    /// 前置きが同じ `role: "user"` の形で入るので、発言として返すと会話の頭が
    /// 前置きで埋まる（表示名がこれを避けているのと同じ理由）
    #[test]
    fn a_preamble_carried_as_a_response_item_is_not_a_message() {
        assert_eq!(
            message_of(
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"permissions preamble"}]}}"#
            ),
            None
        );
    }

    #[test]
    fn other_events_are_not_messages() {
        for line in [
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{}}}"#,
            r#"{"type":"session_meta","payload":{"id":"019fc236"}}"#,
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"  "}}"#,
        ] {
            assert_eq!(message_of(line), None, "{line}");
        }
    }

    /// 実際の registry 応答（`version` 以外に多くのキーを持つ）から版番号だけを拾う。
    /// **`codex --version` と同じ形で返ること**が要点（前後に接頭辞が付かない）
    #[test]
    fn the_published_version_comes_out_shaped_like_codex_version() {
        let body = r#"{"name":"@openai/codex","version":"0.146.0","bin":{"codex":"bin/codex.js"}}"#;
        assert_eq!(parse_latest_version(body).as_deref(), Some("0.146.0"));
        // 前後の空白は落とす（配布側が整形を変えても比較が壊れない）
        assert_eq!(
            parse_latest_version(r#"{"version":" 0.147.0 "}"#).as_deref(),
            Some("0.147.0")
        );
    }

    /// 版番号として読めない応答は通さない。通すと版行が「更新あり」を出し、
    /// クリックすると要らない `codex update` が走る
    #[test]
    fn rejects_responses_that_are_not_a_version() {
        for bad in [
            "",
            "<html>502 Bad Gateway</html>",
            r#"{"error":"Not found"}"#,          // registry のエラー応答
            r#"{"version":""}"#,                 // 空
            r#"{"version":"0.146"}"#,            // パート不足
            r#"{"version":{"latest":"0.146.0"}}"#, // 文字列でない
        ] {
            assert_eq!(parse_latest_version(bad), None, "input: {bad:?}");
        }
    }
}
