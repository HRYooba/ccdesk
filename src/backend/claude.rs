//! claude の起こし方。
//!
//! 起動は新規なら `claude --session-id <uuid> [prompt]`、再開なら
//! `claude -r <session-id>`。**渡した UUID がそのまま transcript の `sessionId` に
//! なる**ので、一覧の行（[`crate::sessions::SessionRow`]）と claude 側の記録が
//! 同じ鍵で結びつく（codex にはこれができない ＝ `crate::backend::codex` 参照）。
//!
//! 綴りが非公開なもの（継承の印）は [`crate::claude_format`] が持つ。

use std::path::{Path, PathBuf};

use crate::backend::{AgentVersion, Backend, Candidate, Inject, Launch, NameIndex, Span, Spawn};
use crate::claude_format::{AI_TITLE, CUSTOM_TITLE, INHERITED_MARKERS, LAST_PROMPT};

/// 実行ファイル名。**PATH で解決する**（絶対パスを持たない: 自己更新で場所が
/// 変わっても追随する）
const PROGRAM: &str = "claude";

pub(crate) struct Claude;

impl Backend for Claude {
    fn command(&self, cwd: &str, launch: Launch<'_>, inject: Option<&Inject>) -> Spawn {
        let mut cmd = crate::backend::program(PROGRAM);
        let mut conversation = None;
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
                // **行 ID とは別に採番する。** 会話 ID は claude 側の世界の値で、
                // ペインの中の `/clear` `/resume` で行を変えずに移り変わる。
                // 行 ID をここへ出すと、その 1 回目の会話にだけ行 ID が焼き付き、
                // 2 回目以降の会話と扱いが揃わない
                let id = uuid::Uuid::new_v4().to_string();
                cmd.arg("--session-id");
                cmd.arg(&id);
                // 空プロンプトは渡さない（"idle — プロンプト待ち" で始まる）
                if !prompt.is_empty() {
                    cmd.arg(prompt);
                }
                conversation = Some(id);
            }
            // **`--session-id` は新規採番の指定なので混ぜない**
            Launch::Resume { id } => {
                cmd.arg("-r");
                cmd.arg(id);
                conversation = Some(id.to_string());
            }
            // 値なしの `-r` はピッカー（公式: "Resume a conversation by session ID,
            // or …"）。どの会話になるかはユーザーが選ぶので、ccdesk は知らない
            Launch::Pick => cmd.arg("-r"),
        }
        Spawn { cmd, conversation }
    }

    /// **要らない。** claude は `~/.claude/sessions/` の `status` を遷移のたびに書き直す
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

    fn transcript_root(&self) -> Option<PathBuf> {
        crate::claude_format::projects_dir()
    }

    /// 解決の手順は claude 本体と同じ:
    ///
    /// 1. cwd のプロジェクトディレクトリ（200 字超は畳んだ派生名）
    /// 2. **cwd の git 作業ツリー**のプロジェクトディレクトリ
    ///    （セッションは走行中に worktree へ移れる。[`crate::git`]）
    ///
    /// 根の全走査はしない（実機で 67 ディレクトリある）。見つからなければ None ＝
    /// 1 ターン終わって記録ができた時点で次の周期が拾う
    fn transcript_in(&self, root: &Path, conversation: &str, cwd: &str) -> Option<PathBuf> {
        let file = crate::claude_format::transcript_file_name(conversation);
        let at = |cwd: &str| {
            let path = root
                .join(crate::claude_format::project_dir_name(cwd))
                .join(&file);
            path.is_file().then_some(path)
        };
        at(cwd).or_else(|| {
            crate::git::worktrees_of(cwd)
                .into_iter()
                .find_map(|tree| at(&tree.display().to_string()))
        })
    }

    /// 記録した transcript が**どの作業ツリーのもの**かで決まる: 行の cwd から
    /// 導いた置き場所に在るならその cwd、別の作業ツリーの置き場所に在るなら
    /// その作業ツリー。
    ///
    /// **作業ツリーが消えていれば None**（claude 自身もその会話を見つけられず、
    /// `claude -r` は `No conversation found` になる ＝ 新規で起こすのが正しい）
    fn resume_cwd(&self, cwd: &str, transcript: Option<&Path>) -> Option<String> {
        let path = transcript?;
        if !path.is_file() {
            return None;
        }
        let dir = path.parent()?.file_name()?.to_str()?;
        if crate::claude_format::project_dir_name(cwd) == dir {
            return Some(cwd.to_string());
        }
        crate::git::worktrees_of(cwd)
            .into_iter()
            .map(|tree| tree.display().to_string())
            .find(|tree| crate::claude_format::project_dir_name(tree) == dir)
    }

    /// **transcript の中で名前が決まる**（索引は持たない ＝ [`Self::name_index`]）。
    /// 綴りの正本は [`crate::claude_format`]
    fn title_records(&self) -> &'static [Candidate] {
        &TITLE_RECORDS
    }

    /// **持たない。** claude の名前は transcript の中にある
    fn name_index(&self) -> Option<NameIndex> {
        None
    }
}

/// 表示名の候補（**この並びが優先順**）。
///
/// `custom-title` だけ [`Span::Rare`] なのは、ユーザーが `/rename` したときにしか
/// 書かれず、長い会話では末尾窓の外へ出るため（実測: 802 本中 77 本が持ち、
/// うち 75 本は末尾 64 KiB 以内）
static TITLE_RECORDS: [Candidate; 3] = [
    Candidate { marker: CUSTOM_TITLE.0, text: flat::<0>, span: Span::Rare },
    Candidate { marker: AI_TITLE.0, text: flat::<1>, span: Span::Appended },
    Candidate { marker: LAST_PROMPT.0, text: flat::<2>, span: Span::Appended },
];

/// 平らな 1 行（`{"type":"<型名>","<キー>":"…"}`）から値を取り出す。
///
/// **添字で表を引く**のは、型名とキーの組を 2 度書かないため（`Candidate` の
/// `marker` と食い違うと、足切りは通るのに値が取れない状態が作れてしまう）
fn flat<const AT: usize>(value: &serde_json::Value) -> Option<&str> {
    let (kind, key) = [CUSTOM_TITLE, AI_TITLE, LAST_PROMPT][AT];
    (value.get("type").and_then(serde_json::Value::as_str) == Some(kind))
        .then(|| value.get(key).and_then(serde_json::Value::as_str))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::tests::argv;

    fn build(launch: Launch<'_>, inject: Option<&Inject>) -> Spawn {
        Claude.command("C:\\dev\\app", launch, inject)
    }

    /// 新規は `--session-id <uuid> [prompt]`。**空プロンプトは渡さない**
    /// （渡すと空メッセージを送ったセッションになる）。
    ///
    /// **`-n <title>` は 1 つも渡さない**: claude は `-n` の名前を transcript の
    /// `custom-title` として残すので、ccdesk が組んだ名前を渡すと表示名が
    /// そこで凍る（`new session` のまま張り付く実害があった）
    #[test]
    fn a_new_session_passes_its_uuid_and_prompt_but_never_a_name() {
        let spawn = build(
            Launch::New {
                prompt: "fix login form validation",
            },
            None,
        );
        let id = spawn.conversation.clone().expect("no conversation was minted");
        assert_eq!(
            argv(&spawn.cmd),
            ["--session-id", &id, "fix login form validation"]
        );
        assert_eq!(
            spawn.cmd.get_cwd().map(|c| c.to_string_lossy().to_string()),
            Some("C:\\dev\\app".to_string()),
            "cwd is not passed through"
        );

        // プロンプト無しは UUID だけ（`claude --session-id <uuid>`）
        let spawn = build(Launch::New { prompt: "" }, None);
        let args = argv(&spawn.cmd);
        assert_eq!(args[0], "--session-id");
        assert_eq!(args.len(), 2);
        assert!(!args.contains(&"-n".to_string()), "the name argument came back: {args:?}");
    }

    /// **採番した UUID は毎回違う。** 使い回すと、`/clear` の後に開き直した行が
    /// 前の会話を上書きしに行く
    #[test]
    fn every_new_session_gets_its_own_conversation_id() {
        let first = build(Launch::New { prompt: "" }, None).conversation;
        let second = build(Launch::New { prompt: "" }, None).conversation;
        assert!(first.is_some() && first != second, "{first:?} == {second:?}");
    }

    /// 再開は `-r <session-id>` だけ（`--session-id` は新規採番の指定なので混ぜない）
    #[test]
    fn resuming_passes_only_the_session_id() {
        let spawn = build(Launch::Resume { id: "8a1c0f52-0b3e" }, None);
        assert_eq!(argv(&spawn.cmd), ["-r", "8a1c0f52-0b3e"]);
        assert_eq!(spawn.conversation.as_deref(), Some("8a1c0f52-0b3e"));
    }

    /// **ピッカーには ID を渡さない。** 会話を確かめていない行を推測で resume
    /// すると、別の会話を開くか見つからずに落ちる。値なしの `-r` は claude 自身の
    /// ピッカーで、選ぶのはユーザー ＝ ccdesk はどの会話になるか知らない
    #[test]
    fn picking_passes_no_id_and_claims_no_conversation() {
        let spawn = build(Launch::Pick, None);
        assert_eq!(argv(&spawn.cmd), ["-r"]);
        assert_eq!(spawn.conversation, None);
    }

    /// 注入する settings（state を戻す hook）は起動の種類に関係なく前に付く
    #[test]
    fn the_injected_settings_are_passed_before_the_launch_arguments() {
        let path = std::path::Path::new("C:\\Users\\me\\.ccdesk\\inject-settings.json");
        let inject = Inject {
            exe: "C:/Users/me/ccdesk.exe",
            settings: path,
        };
        let spawn = build(Launch::Resume { id: "8a1c0f52-0b3e" }, Some(&inject));
        assert_eq!(
            argv(&spawn.cmd),
            [
                "--settings",
                path.to_string_lossy().as_ref(),
                "-r",
                "8a1c0f52-0b3e",
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
        let cmd = build(Launch::Resume { id: "8a1c0f52-0b3e" }, None).cmd;
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
