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
    codex_app_server, codex_index, AgentVersion, Backend, Candidate, Inject, Launch, Message,
    NameIndex, Span, Spawn,
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

/// codex が持つ hook イベント。**[`HOOK_EVENTS`] の部分集合**で、State への対応も
/// 同じ（実測 / codex 0.146.0）。
///
/// **一覧を持つ理由**: 知らないイベント名を `-c` に載せると codex が設定ごと
/// 読み込みに失敗し、hook が 1 つも効かなくなる。claude 専用のイベント
/// （`Notification` / `StopFailure`）をここで落とす。
///
/// (event, state) の対応そのものは [`HOOK_EVENTS`] が正本なので、ここには持たない
const SUPPORTED_EVENTS: [&str; 5] = [
    "SessionStart",
    "UserPromptSubmit",
    "PermissionRequest",
    "Stop",
    "SessionEnd",
];

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

/// `SessionEnd` の hook に codex が許すタイムアウトの上限（秒）。
///
/// **超えると codex が黙って詰めたうえで警告を 1 行出す**（実測:
/// `clamping SessionEnd hook timeout to 3s in …`）。ccdesk の hook は実測
/// 170〜190ms なので 3 秒で足り、警告を出させる理由が無い。
/// **セッションを閉じる経路なので codex 側が短く切っているのは妥当**
/// （ここで待たされると終了が遅れる）
const SESSION_END_TIMEOUT_SECS: u64 = 3;

/// この詰めが意味を持つのは共通のタイムアウトより短い間だけ。
/// **コンパイル時に固定する**（共通側を 3 秒以下へ下げたらここは要らなくなる）
const _: () = assert!(SESSION_END_TIMEOUT_SECS < HOOK_TIMEOUT_SECS);

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

    /// **持たない。** codex に claude の `status` 相当のものは無い（`state_*.sqlite`
    /// の `threads` は会話の索引であって、今どうしているかを言わない）。
    /// 代用の材料と、それが要る理由は [`Backend::has_live_status`]
    fn has_live_status(&self) -> bool {
        false
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

    fn update_program(&self) -> &'static str {
        PROGRAM
    }

    /// `codex update` は npm を通るので、**走っている codex が掴んでいる
    /// `codex.exe` を unlink できないと npm が作業ディレクトリごと諦める**
    /// （実測: `EPERM` を warn 扱いにして exit 0 を返し、`@openai/.codex-<乱数>` が
    /// 285MB 残った）。npm 自身は次の更新でも消しに来ない。
    ///
    /// 拾うのは `@openai/` 直下の**先頭ドット付き**のものだけ。npm の作業用の名前で、
    /// 正規のインストール（`@openai/codex`）とはドットの有無で必ず分かれる
    fn update_leftovers(&self) -> Vec<PathBuf> {
        ccdesk::resolve_program(PROGRAM)
            .map(|shim| npm_workdirs_beside(&shim))
            .unwrap_or_default()
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

    /// `{"type":"event_msg","payload":{"type":"user_message"|"agent_message",
    /// "message":"…"}}` から発言を取る。
    ///
    /// **`response_item` は見ない。** 表示名のときと同じ理由で、あちらには
    /// AGENTS.md や permissions の前置き・道具の出入りが同じ形で並ぶ
    /// （[`USER_MESSAGE`]）。`event_msg` は codex が画面に出した出来事なので、
    /// ここに載るのは人が読む発言だけになる
    fn message(&self, value: &serde_json::Value) -> Option<Message> {
        let payload = value.get("payload")?;
        let from_user = match payload.get("type").and_then(serde_json::Value::as_str)? {
            USER_MESSAGE => true,
            AGENT_MESSAGE => false,
            _ => return None,
        };
        let text = payload.get("message").and_then(serde_json::Value::as_str)?;
        (!text.trim().is_empty()).then(|| Message {
            from_user,
            text: text.to_string(),
        })
    }
}

/// 表示名の候補。**索引（`thread_name`）が上、これは下段**（[`Backend::title_records`]）。
///
/// 取り出しは 2 つとも同じ（rollout は打鍵を 1 種類の行でしか運ばない）で、
/// **違うのは読む範囲だけ**。並びがそのまま優先順 ＝ 末尾で拾えたら先頭は見ない
static TITLE_RECORDS: [Candidate; 2] = [
    Candidate {
        marker: USER_MESSAGE,
        text: user_prompt,
        span: Span::Appended,
    },
    Candidate {
        marker: USER_MESSAGE,
        text: user_prompt,
        span: Span::Head,
    },
];

/// rollout の中でユーザーの打鍵そのものを運ぶ行の型名。
///
/// **`role: "user"` の `response_item` は使わない。** あちらには AGENTS.md や
/// permissions の前置きも同じ形で入るので、名前にすると前置きが行に出る
const USER_MESSAGE: &str = "user_message";

/// codex の答えそのものを運ぶ行の型名。**表示名には使わない**（名前は打鍵から
/// 採る）が、会話を読むには要る（[`Backend::message`]）
const AGENT_MESSAGE: &str = "agent_message";

/// `{"type":"event_msg","payload":{"type":"user_message","message":"…"}}` から
/// 打鍵を取り出す
fn user_prompt(value: &serde_json::Value) -> Option<&str> {
    let payload = value.get("payload")?;
    (payload.get("type").and_then(serde_json::Value::as_str) == Some(USER_MESSAGE))
        .then(|| payload.get("message").and_then(serde_json::Value::as_str))?
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
    for row in HOOK_EVENTS
        .iter()
        .filter(|row| SUPPORTED_EVENTS.contains(&row.event))
    {
        // SessionEnd だけ codex 側の上限が短い（[`SESSION_END_TIMEOUT_SECS`]）
        let timeout = if row.event == "SessionEnd" {
            HOOK_TIMEOUT_SECS.min(SESSION_END_TIMEOUT_SECS)
        } else {
            HOOK_TIMEOUT_SECS
        };
        by_event.entry(row.event).or_default().push(format!(
            "{{hooks=[{{type='command',command='{exe} hook {} {}',timeout={timeout}}}]}}",
            row.event,
            row.state.as_str(),
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

/// `shim` の隣の npm ツリーに残った作業ディレクトリ `@openai/.codex-<乱数>`。
///
/// `codex update` は npm を通るので、**走っている codex が掴んでいる `codex.exe` を
/// unlink できないと npm が作業ディレクトリごと諦める**（実測: `EPERM` を warn 扱いに
/// して exit 0 を返し、285MB が残った）。npm 自身は次の更新でも消しに来ない。
///
/// **先頭ドット付きだけを拾う。** npm の作業用の名前で、正規のインストール
/// （`@openai/codex`）とはドットの有無で必ず分かれる。
/// **パスを引数で受ける**ので、テストが実ユーザーの npm ツリーを見ずに済む
fn npm_workdirs_beside(shim: &Path) -> Vec<PathBuf> {
    // npm はシムの隣に node_modules を置く（`npm prefix -g` の中身そのもの）
    let Some(dir) = shim.parent() else {
        return Vec::new();
    };
    let scope = dir.join("node_modules").join("@openai");
    crate::backend::leftovers_in(&scope, ".codex-", |rest| {
        rest.chars().all(|c| c.is_ascii_alphanumeric())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::tests::argv;

    /// **npm の作業ディレクトリだけを拾い、正規のインストールには触れない。**
    ///
    /// `codex update` が EPERM で諦めると `@openai/.codex-<乱数>` が 285MB 残る。
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
            ".codex-",         // 乱数が無い ＝ 何か分からないので残す
            ".codex-has.dot",  // 乱数でない ＝ 残す
        ] {
            std::fs::create_dir_all(scope.join(name)).unwrap();
        }
        let mut found: Vec<String> = npm_workdirs_beside(&dir.join("codex.cmd"))
            .into_iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        found.sort();
        assert_eq!(found, [".codex-AbC123", ".codex-g3ieL94X"]);
        // npm ツリーが無い置き方（PATH に codex が無い / 別の入れ方）でも落ちない
        assert!(npm_workdirs_beside(&dir.join("elsewhere").join("codex.cmd")).is_empty());
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
    /// 読み込みに失敗し、hook が 1 つも効かなくなる
    #[test]
    fn only_the_events_codex_has_are_injected() {
        let toml = hook_toml("C:/ccdesk.exe").expect("no hooks were built");
        for row in HOOK_EVENTS {
            let present = toml.contains(&format!("{}=[", row.event));
            assert_eq!(
                present,
                SUPPORTED_EVENTS.contains(&row.event),
                "{} is {} in the injected TOML",
                row.event,
                if present { "present" } else { "missing" }
            );
        }
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
    /// `clamping SessionEnd hook timeout to 3s in …`）
    #[test]
    fn the_session_end_hook_stays_within_the_timeout_codex_allows() {
        let toml = hook_toml("C:/ccdesk.exe").expect("no hooks were built");
        let at = toml.find("SessionEnd=").expect("SessionEnd is not injected");
        assert!(
            toml[at..].contains(&format!("timeout={SESSION_END_TIMEOUT_SECS}")),
            "SessionEnd asks for a timeout codex will clamp: {}",
            &toml[at..]
        );
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
