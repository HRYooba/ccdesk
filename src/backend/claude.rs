//! claude の起こし方。
//!
//! 起動は新規なら `claude --session-id <uuid> [prompt]`、再開なら
//! `claude -r <session-id>`。**渡した UUID がそのまま transcript の `sessionId` に
//! なる**ので、一覧の行（[`crate::sessions::SessionRow`]）と claude 側の記録が
//! 同じ鍵で結びつく（codex にはこれができない ＝ `crate::backend::codex` 参照）。
//!
//! 綴りが非公開なもの（継承の印）は [`crate::claude_format`] が持つ。

use portable_pty::CommandBuilder;

use crate::backend::{AgentVersion, Backend, Inject, Launch};
use crate::claude_format::INHERITED_MARKERS;
use crate::sessions::SessionId;

/// 実行ファイル名。**PATH で解決する**（絶対パスを持たない: 自己更新で場所が
/// 変わっても追随する）
const PROGRAM: &str = "claude";

pub(crate) struct Claude;

impl Backend for Claude {
    fn command(
        &self,
        session_id: &SessionId,
        cwd: &str,
        launch: Launch<'_>,
        inject: Option<&Inject>,
    ) -> CommandBuilder {
        let mut cmd = crate::backend::program(PROGRAM);
        cmd.cwd(cwd);
        // 継承した親セッションの印を落とす（落とさないと transcript が保存されない）
        for key in INHERITED_MARKERS {
            cmd.env_remove(key);
        }
        // state を戻す hook の注入（中身は [`crate::hooks::inject_settings`]）
        if let Some(inject) = inject {
            cmd.arg("--settings");
            cmd.arg(inject.settings);
        }
        match launch {
            // **`-n <title>` は渡さない。** claude は `-n` で渡した名前を transcript の
            // `custom-title` として残す（実測）ので、ccdesk が組んだ名前を渡すと
            // 「ユーザーが付けた名前」の位置が埋まる ＝ 表示名がそこで凍る
            // （プロンプト無しなら "new session" のまま・claude 側の AI 生成名も付かない）。
            // 表示名は transcript から導く（[`crate::title`]）ので、渡す必要も無い
            Launch::New { prompt } => {
                cmd.arg("--session-id");
                cmd.arg(session_id.as_str());
                // 空プロンプトは渡さない（"idle — プロンプト待ち" で始まる）
                if !prompt.is_empty() {
                    cmd.arg(prompt);
                }
            }
            // **`--session-id` は新規採番の指定なので混ぜない**
            Launch::Resume { id } => {
                cmd.arg("-r");
                cmd.arg(id);
            }
        }
        cmd
    }

    /// **要らない。** claude は `agents --json` の `status` を遷移のたびに書き直す
    /// ので、hook を取り逃しても次の観測（2 秒周期）で必ず正しくなる。
    /// PTY の無音まで材料にすると、考え込んで出力が止まっている間を
    /// 「手が空いた」と誤って読む
    fn quiet_means_idle(&self) -> bool {
        false
    }

    /// 最新版は claude 本体の更新チェックと同じ公式配布エンドポイント
    /// （`downloads.claude.ai/claude-code-releases/<channel>` が版番号を返す。
    /// チャネルは文書化設定 `autoUpdatesChannel` に従う。既定 latest）
    fn version(&self) -> AgentVersion {
        // 現行版: "2.1.218 (Claude Code)" の先頭トークン
        let current = crate::poll::out(PROGRAM, &["--version"])
            .and_then(|s| s.split_whitespace().next().map(str::to_string))
            .unwrap_or_default();
        let channel = ccdesk::claude_settings_channel();
        // ネットワークへ出る作法（タイムアウト等）は [`crate::update::http_get`] が持つ。
        // このスレッドはアカウント取得と共用なので、応答しないネットワークで
        // ぶら下がるとアカウント行の更新まで止まる ＝ タイムアウトが必須な理由
        let latest = crate::update::http_get(&format!(
            "https://downloads.claude.ai/claude-code-releases/{channel}"
        ))
        .map(|s| s.trim().to_string())
        .filter(|l| {
            l.split('.').count() >= 3
                && !current.is_empty()
                && ccdesk::version_newer(l, &current)
        });
        AgentVersion { current, latest }
    }

    fn update_program(&self) -> &'static str {
        PROGRAM
    }

    /// 取得から解釈まで [`crate::usage`] が一手に持つ（claude を短命な
    /// ヘッドレスプロセスとして起こし、SDK の制御チャンネルへ 1 往復投げる）
    fn usage(&self) -> crate::usage::Usage {
        crate::usage::fetch_claude()
    }

    fn account(&self) -> crate::poll::AccountStatus {
        crate::poll::fetch_claude_account()
    }

    fn auth_fingerprint(&self) -> crate::poll::CredentialsFp {
        crate::poll::claude_auth_fingerprint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::tests::argv;

    fn id() -> SessionId {
        SessionId::new("8a1c0f52-0b3e-4a6d-9f11-2c7d5e8b0a34")
    }

    fn build(launch: Launch<'_>, inject: Option<&Inject>) -> CommandBuilder {
        Claude.command(&id(), "C:\\dev\\app", launch, inject)
    }

    /// 新規は `--session-id <uuid> [prompt]`。**空プロンプトは渡さない**
    /// （渡すと空メッセージを送ったセッションになる）。
    ///
    /// **`-n <title>` は 1 つも渡さない**: claude は `-n` の名前を transcript の
    /// `custom-title` として残すので、ccdesk が組んだ名前を渡すと表示名が
    /// そこで凍る（`new session` のまま張り付く実害があった）
    #[test]
    fn a_new_session_passes_its_uuid_and_prompt_but_never_a_name() {
        let cmd = build(
            Launch::New {
                prompt: "fix login form validation",
            },
            None,
        );
        assert_eq!(
            argv(&cmd),
            ["--session-id", id().as_str(), "fix login form validation"]
        );
        assert_eq!(
            cmd.get_cwd().map(|c| c.to_string_lossy().to_string()),
            Some("C:\\dev\\app".to_string()),
            "cwd is not passed through"
        );

        // プロンプト無しは UUID だけ（`claude --session-id <uuid>`）
        let cmd = build(Launch::New { prompt: "" }, None);
        assert_eq!(argv(&cmd), ["--session-id", id().as_str()]);
        assert!(
            !argv(&cmd).contains(&"-n".to_string()),
            "the name argument came back: {:?}",
            argv(&cmd)
        );
    }

    /// 再開は `-r <session-id>` だけ（`--session-id` は新規採番の指定なので混ぜない）
    #[test]
    fn resuming_passes_only_the_session_id() {
        let id = id();
        let resume = Launch::Resume { id: id.as_str() };
        assert_eq!(argv(&build(resume, None)), ["-r", id.as_str()]);
    }

    /// 注入する settings（state を戻す hook）は起動の種類に関係なく前に付く
    #[test]
    fn the_injected_settings_are_passed_before_the_launch_arguments() {
        let path = std::path::Path::new("C:\\Users\\me\\.ccdesk\\inject-settings.json");
        let inject = Inject {
            exe: "C:/Users/me/ccdesk.exe",
            settings: path,
        };
        let row = id();
        let cmd = build(Launch::Resume { id: row.as_str() }, Some(&inject));
        assert_eq!(
            argv(&cmd),
            [
                "--settings",
                path.to_string_lossy().as_ref(),
                "-r",
                id().as_str(),
            ]
        );
    }

    /// **継承した親セッションの印は 1 つ残らず落とす。**
    ///
    /// 残すと子の claude が「別セッションの子」だと誤認して transcript を保存しない
    /// （実測。[`INHERITED_MARKERS`]）。`env_clear` ではなく個別除去なので、
    /// **PATH 等の通常の環境変数は残っている**ことも併せて固定する。
    ///
    /// **親のプロセス環境を一時的に触る**（そうしないと CI のように印が居ない環境で
    /// 検査が空振りする）。触るのはこの一覧の名前だけで、他のテストが読む変数
    /// （`USERPROFILE` 等）とは重ならない。復元は組み立ての直後に行い、
    /// アサートが失敗しても残さない
    #[test]
    fn the_inherited_session_markers_are_removed_but_the_rest_of_the_env_is_kept() {
        for key in INHERITED_MARKERS {
            unsafe { std::env::set_var(key, "1") };
        }
        let row = id();
        let cmd = build(Launch::Resume { id: row.as_str() }, None);
        for key in INHERITED_MARKERS {
            unsafe { std::env::remove_var(key) };
        }
        for key in INHERITED_MARKERS {
            assert_eq!(cmd.get_env(key), None, "{key} is inherited by the child");
        }
        // 個別除去なので、通常の環境変数は落ちない（env_clear ではない）
        assert!(
            cmd.iter_full_env_as_str()
                .any(|(k, _)| k.eq_ignore_ascii_case("PATH")),
            "PATH was dropped too — claude cannot start"
        );
    }
}
