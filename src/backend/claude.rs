//! claude の起こし方。
//!
//! 起動は新規なら `claude --session-id <uuid> [prompt]`、再開なら
//! `claude -r <session-id>`。**渡した UUID がそのまま transcript の `sessionId` に
//! なる**ので、一覧の行（[`crate::sessions::SessionRow`]）と claude 側の記録が
//! 同じ鍵で結びつく（codex にはこれができない ＝ `crate::backend::codex` 参照）。
//!
//! 綴りが非公開なもの（継承の印）は [`crate::claude_format`] が持つ。

use std::path::{Path, PathBuf};

use crate::backend::{
    AgentVersion, Backend, Candidate, Inject, Launch, Garbage, Message, NameIndex, Span, Spawn,
};
use crate::claude_format::{
    AGENT_RECORD, AI_TITLE, CONTENT_KEY, CUSTOM_TITLE, INHERITED_MARKERS, LAST_PROMPT, MESSAGE_KEY,
    TEXT_BLOCK, USER_RECORD,
};

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

    /// **持つ。** `~/.claude/sessions/` の `status` を遷移のたびに書き直すので、
    /// hook を取り逃しても次の観測（2 秒周期）で必ず正しくなる ＝ 代用の材料
    /// （[`Backend::has_live_status`]）は要らない。PTY の無音まで材料にすると、
    /// 考え込んで出力が止まっている間を「手が空いた」と誤って読む
    fn has_live_status(&self) -> bool {
        true
    }

    /// 最新版は claude 本体の更新チェックと同じ公式配布エンドポイント
    /// （`downloads.claude.ai/claude-code-releases/<channel>` が版番号を返す。
    /// チャネルは文書化設定 `autoUpdatesChannel` に従う。既定 latest）
    fn version(&self) -> AgentVersion {
        let current = current_version().unwrap_or_default();
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

    fn update_argv(&self) -> (&'static str, &'static [&'static str]) {
        (PROGRAM, &["update"])
    }

    /// claude が置き去りにするものは **2 箇所**にある。**片方だけ見ていた。**
    ///
    /// 1. `<exe>.old.<ミリ秒>`（退避した実行ファイル）
    /// 2. `~/.local/share/claude/versions/<版>`（版ごとの実体）
    ///
    /// 実測は日をまたいで揺れる。2026-08-27 には `.old.*` が **0 件**で
    /// versions に 3 世代、翌 08-28 の更新（2.1.247 → 2.1.250）では
    /// **versions が現行 1 件へ刈られ、代わりに `.old.*` が 2 件出た**。
    /// つまり **claude は versions を刈ることがある**（毎回かは不明）一方で、
    /// 退避 exe が残る回もある。片方だけ見ると、その日の流儀によって
    /// **正しく実装された空振り**になる。どちらも 1 世代 200〜400MB 台。
    ///
    /// `claude.exe` は versions とは別ファイル（2026-08-28、`fsutil hardlink list`
    /// で両方ともリンク数 1 と確認）なので、versions は保管庫であり、
    /// **現行版より古いもの**は次の更新に要らない。
    ///
    /// **どちらも「塞がない」**（[`Garbage::blocks_next_update`]）:
    /// 退避名はミリ秒、versions は版名で、どちらも次の更新と衝突しない
    fn garbage(&self) -> Vec<Garbage> {
        let parked = ccdesk::resolve_program(PROGRAM)
            .and_then(|exe| parked_exes_spec(&exe))
            .into_iter();
        parked.chain(superseded_versions_spec()).collect()
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

    /// `{"type":"user"|"assistant","message":{"content":…}}` から本文を取る。
    ///
    /// **`content` は文字列 1 本のことも、ブロックの配列のこともある。**
    /// 配列のときは本文ブロック（[`TEXT_BLOCK`]）だけを繋ぐので、道具の結果を
    /// 運ぶだけの `user` 行（`tool_result` しか持たない）はここで落ちる
    fn message(&self, value: &serde_json::Value) -> Option<Message> {
        let from_user = match value.get("type").and_then(serde_json::Value::as_str)? {
            USER_RECORD => true,
            AGENT_RECORD => false,
            _ => return None,
        };
        let content = value.get(MESSAGE_KEY)?.get(CONTENT_KEY)?;
        let text = match content {
            serde_json::Value::String(text) => text.clone(),
            serde_json::Value::Array(blocks) => blocks
                .iter()
                .filter(|block| {
                    block.get("type").and_then(serde_json::Value::as_str) == Some(TEXT_BLOCK)
                })
                .filter_map(|block| block.get(TEXT_BLOCK).and_then(serde_json::Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => return None,
        };
        (!text.trim().is_empty()).then_some(Message { from_user, text })
    }
}

/// claude が更新のたびに現行版を退避する `<exe>.old.<ミリ秒>`。
///
/// **後ろが数字だけのものに限る。** その綴りは claude が付ける退避名そのもので、
/// 実行ファイル本体（`claude.exe`）とも世代を持たない `.old` とも重ならない
fn parked_exes_spec(exe: &Path) -> Option<Garbage> {
    let (dir, name) = (exe.parent()?, exe.file_name()?);
    Some(Garbage {
        dir: dir.to_path_buf(),
        prefix: format!("{}.old.", name.to_string_lossy()),
        rest_ok: Box::new(|rest| rest.chars().all(|c| c.is_ascii_digit())),
        blocks_next_update: false,
    })
}

/// `~/.local/share/claude/versions/` の、**現行版より古い**実体。
///
/// **「現行版以外」では消しすぎる。** claude の更新は「新版を versions へ落とす →
/// 実行ファイルを差し替える」の 2 手で、後半だけ失敗する状態が実在する
/// （[`crate::app::AgentUpdate::Stalled`] が検出しているのがまさにそれ）。
/// その窓では `claude --version` は旧版のままなので、「現行版以外」を消すと
/// **落としたばかりの新版**を消し、claude 自身の「次回起動で差し替える」復帰路を
/// 毎回壊す。新旧の比較は版行と同じ [`ccdesk::version_newer`]。
///
/// **現行版が読めなければ宣言そのものを作らない。** 版が空のまま比較へ回すと
/// 全世代が「より古い」に該当する ＝ 現行版まで消す。ここが返す `None` は
/// 「掃除できないだけ」で、次の掃除でもう一度来る
fn superseded_versions_spec() -> Option<Garbage> {
    let (current, dir) = (current_version()?, versions_dir()?);
    Some(Garbage {
        dir,
        prefix: String::new(),
        // **版の形をしたものだけを積極的に同定する。** 排除で選ぶと、落とし途中の
        // 一時ファイル・ロック・ccdesk 自身の隔離先まで巻き込む
        rest_ok: Box::new(move |name| {
            is_version(name) && ccdesk::version_newer(&current, name)
        }),
        blocks_next_update: false,
    })
}

/// 今入っている版。**聞き方はここ 1 箇所**（版行の表示と保管庫の掃除が
/// 別々の綴りを持つと、片方だけ直して「現行版を消す」形になる）。
///
/// `claude --version` は `"2.1.218 (Claude Code)"` を返すので先頭トークン。
/// **形まで確かめる**（[`is_version`]）: [`crate::poll::out`] は終了コードを
/// 見ないので、警告バナーやエラー文が先に出れば `"Warning:"` のような値が返る
fn current_version() -> Option<String> {
    crate::poll::out(PROGRAM, &["--version"])?
        .split_whitespace()
        .next()
        .map(str::to_string)
        .filter(|v| is_version(v))
}

/// 版番号の形か（空でない数字の並びをドットで 2 つ以上繋いだもの）。
///
/// **保管庫を触る判断はここ 1 箇所**: 現行版の妥当性と、保管庫の中身が版かどうかを
/// 別の綴りで判定すると、片方だけ緩めたときに消しすぎる
fn is_version(s: &str) -> bool {
    s.split('.').count() >= 2
        && s.split('.')
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// 版の保管庫。**`resolve_program` の隣ではない**（実行ファイルは `~/.local/bin`、
/// 実体は `~/.local/share/claude/versions`）ので、ここだけ別に組み立てる
fn versions_dir() -> Option<PathBuf> {
    // **`unwrap_or_default()` にしない**: ホームが引けないときに空のパスへ join すると
    // カレントディレクトリ配下の `.local/share/claude/versions` を指す ＝
    // 掃除が別の場所を消しに行く
    Some(
        ccdesk::home()?
            .join(".local")
            .join("share")
            .join("claude")
            .join("versions"),
    )
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

    /// **保管庫から拾うのは「現行版より古い」ものだけ。**
    ///
    /// 「現行版以外」にすると、更新の 2 手（新版を落とす → 実行ファイルを差し替える）の
    /// 後半だけ失敗した窓で**落としたばかりの新版**を消す。`claude --version` は
    /// その窓では旧版を返すので、区別できるのは新旧の比較だけ。
    ///
    /// **現行版が読めないときは 1 つも拾わない**ことも同時に固定する: ここが空文字に
    /// 縮退すると全世代が「より古い」に該当し、現行版まで消える
    #[test]
    fn the_version_store_gives_up_only_generations_older_than_the_running_one() {
        let dir = crate::testutil::TempDir::new("claude", "versions");
        for name in ["2.1.242", "2.1.246", "2.1.250", "2.1.251", ".ccdesk-held", "download.tmp"] {
            std::fs::write(dir.join(name), "BINARY").unwrap();
        }
        let collect = |current: &str| {
            let spec = Garbage {
                dir: dir.path().to_path_buf(),
                prefix: String::new(),
                rest_ok: Box::new({
                    let current = current.to_string();
                    move |name: &str| is_version(name) && ccdesk::version_newer(&current, name)
                }),
                blocks_next_update: false,
            };
            let mut found: Vec<String> = crate::backend::leftovers_in(&spec.dir, &spec.prefix, &spec.rest_ok)
                .into_iter()
                .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                .collect();
            found.sort();
            found
        };
        // **新版（2.1.251）は残す** — 差し替え待ちかもしれない
        assert_eq!(collect("2.1.250"), ["2.1.242", "2.1.246"]);
        // 版の形をしていないものには触らない（隔離先・落とし途中の一時ファイル）
        assert!(!collect("9.9.9").iter().any(|n| n.starts_with('.') || n.ends_with(".tmp")));

        // **版が読めなければ宣言そのものを作らない。** ここが `None` を返す限り、
        // 上の述語に空文字が渡る経路は存在しない
        assert!(is_version("2.1.250") && is_version("1.2"));
        for junk in ["", "Warning:", "2.1.250-beta", "abc", "2", "2..1", "2.1."] {
            assert!(!is_version(junk), "accepted {junk:?} as a version");
        }
    }

    /// **退避ファイルだけを拾い、動いているインストールには触れない。**
    ///
    /// ccdesk がセッションを常駐させるせいで、退避 exe は消える機会を失う
    /// （掴まれたまま次の更新を迎える）。拾い方を間違えると **claude 本体を消す**ので、
    /// 隣り合う紛らわしい名前を並べて固定する
    #[test]
    fn only_the_parked_generations_next_to_the_exe_are_collected() {
        let dir = crate::testutil::TempDir::new("claude", "parked-exes");
        for name in [
            "claude.exe",                   // 本体
            "claude.exe.old.1785884570678", // 退避（消す）
            "claude.exe.old.1786075360017", // 退避（消す）
            "claude.exe.old",               // 世代が無い ＝ 何か分からないので残す
            "claude.exe.old.manual",        // 人が付けた名前 ＝ 残す
            "claude.exe.new",               // 更新の途中経過 ＝ 別の話
        ] {
            std::fs::write(dir.join(name), "x").unwrap();
        }
        let spec = parked_exes_spec(&dir.join("claude.exe")).unwrap();
        let mut found: Vec<String> =
            crate::backend::leftovers_in(&spec.dir, &spec.prefix, &spec.rest_ok)
                .into_iter()
                .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                .collect();
        found.sort();
        assert_eq!(
            found,
            ["claude.exe.old.1785884570678", "claude.exe.old.1786075360017"]
        );
        // 実行ファイルが無い場所を指しても落ちない（PATH に claude が無い環境）
        let nowhere = parked_exes_spec(&dir.join("nowhere").join("claude.exe")).unwrap();
        assert!(crate::backend::leftovers_in(&nowhere.dir, &nowhere.prefix, &nowhere.rest_ok).is_empty());
    }

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

    fn message_of(line: &str) -> Option<Message> {
        Claude.message(&serde_json::from_str(line).expect("the test wrote invalid JSON"))
    }

    #[test]
    fn both_sides_of_the_conversation_are_read() {
        assert_eq!(
            message_of(r#"{"type":"user","message":{"content":"run the tests"}}"#),
            Some(Message { from_user: true, text: "run the tests".to_string() })
        );
        assert_eq!(
            message_of(
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"they pass"}]}}"#
            ),
            Some(Message { from_user: false, text: "they pass".to_string() })
        );
    }

    /// 道具の結果は `user` の行として並ぶ。**これを発言として返すと、会話が
    /// コマンド出力で埋まって読めなくなる**（`ccdesk read` の値そのものが落ちる）
    #[test]
    fn a_tool_result_is_not_a_message() {
        assert_eq!(
            message_of(
                r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#
            ),
            None
        );
    }

    /// 思考と道具呼び出しが同じ配列に混ざっても、本文だけを繋ぐ
    #[test]
    fn only_the_text_blocks_are_kept() {
        assert_eq!(
            message_of(
                r#"{"type":"assistant","message":{"content":[
                   {"type":"thinking","thinking":"hmm"},
                   {"type":"text","text":"first"},
                   {"type":"tool_use","name":"Bash"},
                   {"type":"text","text":"second"}]}}"#
            ),
            Some(Message { from_user: false, text: "first\nsecond".to_string() })
        );
    }

    #[test]
    fn other_records_are_not_messages() {
        for line in [
            r#"{"type":"summary","summary":"a recap"}"#,
            r#"{"type":"ai-title","aiTitle":"a name"}"#,
            r#"{"type":"assistant","message":{}}"#,
            r#"{"type":"user","message":{"content":"   "}}"#,
        ] {
            assert_eq!(message_of(line), None, "{line}");
        }
    }
}
