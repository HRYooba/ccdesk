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
    codex_app_server, codex_index, AgentVersion, Backend, Candidate, Inject, Launch, NameIndex,
    Span, Spawn,
};
use crate::hooks::{HOOK_EVENTS, HOOK_TIMEOUT_SECS};

const PROGRAM: &str = "codex";

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

    /// **要る。** codex に claude の `status` 相当のライブ状態は無く、Esc 中断では
    /// `Stop` が発火しない（[#22858](https://github.com/openai/codex/issues/22858)。
    /// 2026-08-02 時点で OPEN）。補正しないと中断した行が Working のまま固着する。
    ///
    /// 誤読の危険は小さい: codex の TUI は考えている間もスピナーを描き続けるので、
    /// 「無出力が続く」＝ ほぼ本当に止まっている
    fn quiet_means_idle(&self) -> bool {
        true
    }

    /// **最新版は codex 自身が書いた更新チェックの結果を読む**
    /// （`$CODEX_HOME/version.json` の `latest_version`）。claude のように
    /// 配布エンドポイントを自前で叩かない ＝ ネットワークへ出ない。
    ///
    /// 非公開の内部ファイルなので、形が変われば「更新あり」が出なくなるだけ
    fn version(&self) -> AgentVersion {
        // 現行版: "codex-cli 0.146.0" の末尾トークン
        let current = crate::poll::out(PROGRAM, &["--version"])
            .and_then(|s| s.split_whitespace().last().map(str::to_string))
            .unwrap_or_default();
        let latest = codex_index::latest_version()
            .filter(|l| !current.is_empty() && ccdesk::version_newer(l, &current));
        AgentVersion { current, latest }
    }

    fn update_program(&self) -> &'static str {
        PROGRAM
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
    /// 索引に載るのは名前が決まった会話だけで、実測（実機 2026-08-02）では
    /// **ターンのある rollout 211 本のうち 65 本（31%）**しか載っていない。
    /// 残りは rollout の**最初のユーザー発話**で名乗る ＝ claude が
    /// `last-prompt` へ落ちるのと同じ形。
    ///
    /// **[`Span::Head`] なのが claude との違い。** codex の rollout は道具の出力が
    /// 末尾を埋めるので末尾窓では届かない（実測 60%）が、先頭は
    /// `session_meta` → 前置き → 最初のプロンプトと決まった形で、
    /// 先頭 256 KiB に 213/214（99.5%）が収まる。**先頭は追記されない**ので
    /// 1 会話 1 回の有界読みで済む
    fn title_records(&self) -> &'static [Candidate] {
        &TITLE_RECORDS
    }

    fn name_index(&self) -> Option<NameIndex> {
        codex_index::name_index()
    }
}

/// 表示名の候補。**索引（`thread_name`）が上、これは下段**なので 1 つだけ
static TITLE_RECORDS: [Candidate; 1] = [Candidate {
    marker: USER_MESSAGE,
    text: first_prompt,
    span: Span::Head,
}];

/// rollout の中でユーザーの打鍵そのものを運ぶ行の型名。
///
/// **`role: "user"` の `response_item` は使わない。** あちらには AGENTS.md や
/// permissions の前置きも同じ形で入るので、名前にすると前置きが行に出る
const USER_MESSAGE: &str = "user_message";

/// `{"type":"event_msg","payload":{"type":"user_message","message":"…"}}` から
/// 打鍵を取り出す
fn first_prompt(value: &serde_json::Value) -> Option<&str> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::tests::argv;

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
}
