//! codex の起こし方。
//!
//! **claude と決定的に違うのは ID の採番。** codex に `--session-id` 相当は無く
//! （`codex --help` の全項目で確認）、セッション ID は codex 自身が起動時に決める。
//! そこで ccdesk は自分の行 ID を環境変数（[`crate::hooks::ROW_ENV`]）で渡し、
//! hook の子プロセスがそれを読んで「どの行の出来事か」を名乗る。**env は codex を
//! 経由して hook まで継承される**（実測）。
//!
//! 再開は `codex resume <uuid>` で、この `<uuid>` は **codex が採番した方**
//! （[`Launch::Resume`] の `id`）。ccdesk の行 ID ではない。
//!
//! 計測の前提と経緯は `docs/codex-support.md`。

use std::collections::BTreeMap;

use portable_pty::CommandBuilder;

use crate::backend::{codex_app_server, codex_index, AgentVersion, Backend, Inject, Launch};
use crate::hooks::{HOOK_EVENTS, HOOK_TIMEOUT_SECS, ROW_ENV};
use crate::sessions::SessionId;

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
    fn command(
        &self,
        session_id: &SessionId,
        cwd: &str,
        launch: Launch<'_>,
        inject: Option<&Inject>,
    ) -> CommandBuilder {
        let mut cmd = crate::backend::program(PROGRAM);
        cmd.cwd(cwd);
        // **行の相関はここだけ。** codex が採番する ID を ccdesk は前もって知れないので、
        // 自分の行 ID を env で渡し、hook 側が payload の session_id と一緒に記録する
        cmd.env(ROW_ENV, session_id.as_str());
        if let Some(inject) = inject
            && let Some(toml) = hook_toml(inject.exe)
        {
            cmd.arg("-c");
            cmd.arg(toml);
            cmd.arg(BYPASS_TRUST);
        }
        match launch {
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
            }
        }
        cmd
    }

    /// **要る。** codex に `agents --json` 相当のライブ状態は無く、Esc 中断では
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
}

/// `-c` に渡す hook の定義（TOML）。**注入できない形なら None**（hook 無しで
/// 起動する ＝ 行の状態が縮退するだけで、セッション自体は動く）。
///
/// **TOML のリテラル文字列（`'…'`）で組む。** 二重引用符を使うと
/// npm shim → cmd → exe のどこかで食われ、値が TOML として解釈されず
/// 「ただの文字列」として渡る（実測。codex は
/// `invalid type: string …, expected struct HooksToml` で落ちる）。
/// リテラル文字列にはエスケープが無いので、パスに `'` を含む環境では組めない
fn hook_toml(exe: &str) -> Option<String> {
    if exe.contains('\'') {
        return None;
    }
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
            "{{hooks=[{{type='command',command='\"{exe}\" hook {} {}',timeout={timeout}}}]}}",
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::tests::argv;

    /// ccdesk の行 ID（codex が採番する ID ではない）
    fn row() -> SessionId {
        SessionId::new("8a1c0f52-0b3e-4a6d-9f11-2c7d5e8b0a34")
    }

    fn inject() -> Inject<'static> {
        Inject {
            exe: "C:/Users/me/ccdesk.exe",
            settings: std::path::Path::new("C:\\Users\\me\\.ccdesk\\inject-settings.json"),
        }
    }

    fn build(launch: Launch<'_>, inject: Option<&Inject>) -> CommandBuilder {
        Codex.command(&row(), "C:\\dev\\app", launch, inject)
    }

    /// 新規はプロンプトだけ。**`--session-id` に相当するものは渡さない**
    /// （codex に無い ＝ 渡すと起動が落ちる）
    #[test]
    fn a_new_session_passes_only_the_prompt() {
        let cmd = build(
            Launch::New {
                prompt: "fix login form validation",
            },
            None,
        );
        assert_eq!(argv(&cmd), ["fix login form validation"]);
        assert_eq!(
            cmd.get_cwd().map(|c| c.to_string_lossy().to_string()),
            Some("C:\\dev\\app".to_string()),
            "cwd is not passed through"
        );
        assert!(argv(&build(Launch::New { prompt: "" }, None)).is_empty());
    }

    /// 再開は **codex が採番した ID**（行 ID ではない）。取り違えると
    /// `codex resume` が別の会話を開く / 見つからずに落ちる
    #[test]
    fn resuming_uses_the_id_codex_minted_not_the_row_id() {
        let cmd = build(Launch::Resume { id: "019fbe35-5ffa" }, None);
        assert_eq!(argv(&cmd), ["resume", "019fbe35-5ffa"]);
        assert!(
            !argv(&cmd).contains(&row().as_str().to_string()),
            "the ccdesk row id leaked into the resume arguments"
        );
    }

    /// **行の相関は env の 1 本だけ。** これが落ちると hook が「どの行の出来事か」を
    /// 名乗れず、codex の行は永久に状態が付かない
    #[test]
    fn the_row_id_is_handed_over_through_the_environment() {
        let cmd = build(Launch::New { prompt: "" }, None);
        let found = cmd
            .iter_full_env_as_str()
            .find(|(key, _)| *key == ROW_ENV)
            .map(|(_, value)| value.to_string());
        assert_eq!(found, Some(row().as_str().to_string()));
    }

    /// hook は `-c` で 1 起動限り渡し、trust の確認を飛ばす（ユーザーの
    /// `~/.codex/config.toml` を書き換えないため）。**サブコマンドより前**に置く
    #[test]
    fn the_hooks_are_injected_before_the_subcommand() {
        let inject = inject();
        let args = argv(&build(Launch::Resume { id: "abc" }, Some(&inject)));
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

    /// 二重引用符で組むと npm shim → cmd → exe のどこかで食われ、値が TOML として
    /// 解釈されない（実測）。**外側はリテラル文字列**でなければならない
    #[test]
    fn the_injected_value_never_relies_on_double_quotes_as_toml_syntax() {
        let toml = hook_toml("C:/ccdesk.exe").expect("no hooks were built");
        // 二重引用符が出てよいのは exe を囲む中身だけ（TOML の構文としては使わない）
        assert!(
            !toml.contains("=\"") && !toml.contains("{\""),
            "a double-quoted TOML string slipped in: {toml}"
        );
        assert!(toml.contains("type='command'"), "{toml}");
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

    /// リテラル文字列にエスケープは無い。**組めないなら hook 無しで起動する**
    /// （壊れた TOML を渡して codex ごと起動不能にしない）
    #[test]
    fn a_path_that_cannot_be_quoted_drops_the_injection_instead_of_breaking_it() {
        assert_eq!(hook_toml("C:/it's here/ccdesk.exe"), None);
        let inject = Inject {
            exe: "C:/it's here/ccdesk.exe",
            settings: std::path::Path::new("x"),
        };
        assert!(argv(&build(Launch::New { prompt: "" }, Some(&inject))).is_empty());
    }
}
