//! **claude Code の文書化されていない形**への依存を集めた層。
//!
//! ここに置くのは「claude が更新されたら黙って外れうる知識」だけで、実体は
//! **定数と純関数**しかない（ファイルもプロセスも触らない）。使う側の責務
//! （[`crate::title`] が表示名を決める・[`crate::hooks`] が hook を受ける・
//! [`crate::session`] が PTY を起こす）はそのまま各モジュールに残り、
//! **移してあるのは形の知識だけ**。
//!
//! # なぜ 1 箇所に集めるか
//!
//! 非公開の形への依存が散っていると、claude が変わったときに**探す場所が
//! ファイルの数だけ**ある。ここに集めてあれば、直す場所は 1 つになる。
//!
//! # 公式に文書化されたものはここに入れない
//!
//! `--session-id` / `-r` / `--settings` / hook のイベント名は公式のインタフェース
//! なので、[`crate::session`] や [`crate::hooks`] にそのまま置く。混ぜると
//! 「どこが脆いか」が読めなくなる。
//!
//! # 外れたときにどう縮退するか
//!
//! 各入口の doc に **「公式か」** と **「壊れたときにどう縮退するか」** を書く。
//! 全体としては、ここが外れても ccdesk は動き続ける（表示名が `new session` に
//! 戻る・行の状態がヒューリスティックへ落ちる、といった degradation で止まる）。

use std::path::PathBuf;

// ---------------------------------------------------------------------------
// transcript の置き場所（**非公開**）
//
// 縮退: パスの導出が外れると transcript が見つからず、表示名は起動時に決めた値
// （`new session` / 先頭プロンプト）のままになる。再開（`claude -r`）は claude
// 自身が探すので影響を受けない ＝ 機能は落ちない。
// ---------------------------------------------------------------------------

/// transcript を置くディレクトリ（`~/.claude/projects`）。**非公開の配置**。
/// 設定ディレクトリの位置は [`ccdesk::claude_dir`] が持つ（`CLAUDE_CONFIG_DIR` に追従）
pub(crate) fn projects_dir() -> Option<PathBuf> {
    Some(ccdesk::claude_dir()?.join("projects"))
}

/// transcript のファイル名（`<session-id>.jsonl`）。**非公開の規則**
pub(crate) fn transcript_file_name(session_id: &str) -> String {
    format!("{session_id}.jsonl")
}

/// transcript のディレクトリ名の上限。超えたら先頭 [`DIR_NAME_LIMIT`] 文字 +
/// `-` + ハッシュになる（claude 本体の規則。実測）
const DIR_NAME_LIMIT: usize = 200;

/// cwd から transcript のディレクトリ名を導く。**claude 本体の規則の写し**（実測）:
/// 英数字以外をすべて `-` へ置換し、[`DIR_NAME_LIMIT`] 文字を超えたら先頭
/// [`DIR_NAME_LIMIT`] 文字 + `-` + ハッシュ（Java 風 hash の絶対値の base36）。
///
/// 置換後は ASCII だけになるので、文字数・UTF-16 単位数・バイト数が一致する
/// （日本語を含む cwd でも数え方で結果が割れない）
pub(crate) fn project_dir_name(cwd: &str) -> String {
    let encoded = encode_cwd(cwd);
    if encoded.len() <= DIR_NAME_LIMIT {
        return encoded;
    }
    format!(
        "{}-{}",
        &encoded[..DIR_NAME_LIMIT],
        base36(java_hash(&encoded).unsigned_abs())
    )
}

/// 英数字以外をすべて `-` にする（畳む前のディレクトリ名）
fn encode_cwd(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Java / JavaScript 風の文字列ハッシュ（`h = h * 31 + 符号単位`、32bit で巻く）
fn java_hash(text: &str) -> i32 {
    text.encode_utf16()
        .fold(0i32, |h, unit| h.wrapping_mul(31).wrapping_add(i32::from(unit)))
}

/// 36 進表記（0-9a-z）
fn base36(mut value: u32) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if value == 0 {
        return "0".to_string();
    }
    let mut out = Vec::new();
    while value > 0 {
        out.push(DIGITS[(value % 36) as usize]);
        value /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// transcript のレコード（**非公開**）
//
// 縮退: 型名や値のキーが変わると、その候補が拾えなくなるだけ。表示名は下位の
// 候補へ落ち、最後は起動時に決めた名前が残る（[`crate::title`]）。
// ---------------------------------------------------------------------------

/// transcript の 1 行から拾う値の (レコードの型名, 値のキー)。
/// **綴りの正本はこの 4 つ**で、読み手はここしか見ない
pub(crate) type Record = (&'static str, &'static str);

/// ユーザーが付けた名前（claude の `/rename` と `-n` が書く）
pub(crate) const CUSTOM_TITLE: Record = ("custom-title", "customTitle");
/// claude が生成した名前
pub(crate) const AI_TITLE: Record = ("ai-title", "aiTitle");
/// 直近のユーザープロンプト
pub(crate) const LAST_PROMPT: Record = ("last-prompt", "lastPrompt");

// ---------------------------------------------------------------------------
// 子プロセスへ渡さない環境変数（**非公開**）
// ---------------------------------------------------------------------------

/// **子へ渡さない環境変数。** Claude Code の配下から ccdesk を起動すると、この印が
/// 継承されて子の claude が「別セッションの子」だと誤認し、transcript の保存が
/// 無効になる（実測: `⚠ Transcript saving is off — inherited
/// CLAUDE_CODE_CHILD_SESSION marker`）。
///
/// **`env_clear` は使わない**: PATH・USERPROFILE 等まで落ちて claude が起動しなく
/// なる。落とすのは実測で継承が確認できたこの一覧だけ。
///
/// 縮退: 印が増えると再び transcript が保存されなくなる（表示名が付かない・
/// 再開できない行になる）ので、**外れたことは画面に出る**
pub(crate) const INHERITED_MARKERS: [&str; 8] = [
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_SESSION_NAME",
    "CLAUDE_CODE_SESSION_KIND",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_PID",
    "CLAUDECODE",
    "CLAUDE_JOB_DIR",
];

/// hook を呼んだ claude のプロセス ID が入る環境変数（**非公開**。
/// hook のイベント名と入力 JSON は公式なので [`crate::hooks`] 側に置く）。
///
/// 縮退: 読めないと「その state がどの実行のものか」を pid で言えなくなり、
/// 行の状態は時刻の突き合わせだけに落ちる（[`crate::hooks::HookStates`]）
pub(crate) const CLAUDE_PID_ENV: &str = "CLAUDE_PID";

// ---------------------------------------------------------------------------
// `claude agents --json --all` の出力（**半公式**: サブコマンドは `--help` に
// あるが、項目の綴りは文書化されていない）
//
// 縮退: 読めないと行の状態は hook が書いた state と出力ヒューリスティックだけに
// なる（[`crate::poll::classify`]）。一覧そのものは `sessions.json` が正本なので
// 行は消えない。
// ---------------------------------------------------------------------------

/// transcript の `sessionId`（＝ `claude --session-id` へ渡した UUID）
pub(crate) const AGENT_SESSION_ID: &str = "sessionId";
/// `"interactive"` | `"background"` 等
pub(crate) const AGENT_KIND: &str = "kind";
/// 前景セッションが書くライブ状態（busy|idle|waiting|shell）
pub(crate) const AGENT_STATUS: &str = "status";
/// そのセッションを動かしているプロセス（生存中のみ載る）
pub(crate) const AGENT_PID: &str = "pid";
/// 前景セッションを表す [`AGENT_KIND`] の値
pub(crate) const AGENT_KIND_INTERACTIVE: &str = "interactive";

// ---------------------------------------------------------------------------
// SDK 制御チャンネルの `get_usage`（**非公開**）
//
// `-p` / `--input-format stream-json` / `--output-format stream-json` /
// `--settings` は公式に文書化されているが、**制御リクエストのワイヤー形式と
// `get_usage` というサブタイプは文書化されていない**（公式 SDK の公開 API にも
// 見当たらない）。実測で得た綴りなのでここに置く。
//
// **statusline の JSON とは綴りが違う。** 公式に文書化された statusline 側は
// `rate_limits.five_hour.used_percentage`（0-100）と `resets_at`（unix 秒）だが、
// こちらは `utilization`（0-100）と `resets_at`（ISO8601 文字列）。混ぜてはいけない。
//
// 縮退: 綴りが外れると使用率が取れなくなり、フッターの使用率行が消える
// （[`crate::usage`] が None を返す）。ccdesk の他の機能は影響を受けない。
// ---------------------------------------------------------------------------

/// 制御リクエスト 1 行（stdin へ書いて control_response を待つ）。
/// `request_id` は 1 回のプロセスで 1 往復しかしないので固定値でよい
pub(crate) const USAGE_REQUEST_LINE: &str =
    r#"{"type":"control_request","request_id":"ccdesk-usage","request":{"subtype":"get_usage"}}"#;

/// 応答行を見分ける `type` の値
pub(crate) const CONTROL_RESPONSE: &str = "control_response";
/// 応答の中身までの道（`/response/response` の 2 段。外側が制御プロトコルの
/// 封筒で、内側が `get_usage` の戻り値）
pub(crate) const USAGE_BODY_POINTER: &str = "/response/response";
/// 封筒の成否（`"success"` 以外はエラー応答）
pub(crate) const CONTROL_SUBTYPE_POINTER: &str = "/response/subtype";
/// [`CONTROL_SUBTYPE_POINTER`] が成功を表す値
pub(crate) const CONTROL_SUCCESS: &str = "success";

/// 枠の一覧が載るオブジェクト
pub(crate) const USAGE_RATE_LIMITS: &str = "rate_limits";
/// 枠が取れるアカウントかどうか（サブスク以外では false）。
/// **「取れない」と「壊れている」を区別する唯一の手がかり**
pub(crate) const USAGE_AVAILABLE: &str = "rate_limits_available";
/// 5 時間枠のキー
pub(crate) const USAGE_FIVE_HOUR: &str = "five_hour";
/// 7 日枠（全モデル集計）のキー
pub(crate) const USAGE_SEVEN_DAY: &str = "seven_day";
/// モデル別の週次枠の配列。**`seven_day_opus` のような枠名を決め打ちしない**:
/// 実測では未公開の枠名（`tangelo` 等）が null で多数並んでおり、名前を並べると
/// claude 側の増減で黙って腐る
pub(crate) const USAGE_MODEL_SCOPED: &str = "model_scoped";
/// [`USAGE_MODEL_SCOPED`] の要素が持つモデル名
pub(crate) const USAGE_DISPLAY_NAME: &str = "display_name";
/// 使用率（0-100）
pub(crate) const USAGE_UTILIZATION: &str = "utilization";
/// 枠のリセット時刻（**ISO8601 文字列**。statusline 側の unix 秒とは違う）
pub(crate) const USAGE_RESETS_AT: &str = "resets_at";

#[cfg(test)]
mod tests {
    use super::*;

    /// **ディレクトリ名は cwd の英数字以外をすべて `-` にしたもの**（claude 本体の規則）
    #[test]
    fn the_transcript_directory_comes_from_the_working_directory() {
        assert_eq!(
            project_dir_name("C:\\Users\\admin\\Documents\\Work\\ccdesk"),
            "C--Users-admin-Documents-Work-ccdesk"
        );
        // 区切り・記号・非 ASCII はすべて 1 文字 1 つの `-` になる
        assert_eq!(project_dir_name("/home/me/my.app"), "-home-me-my-app");
        // 日本語ではなく全角ラテンを使う（非 ASCII であることを検証したいだけで、
        // tests/no_japanese_in_code.rs のチェック対象を避けるため）
        assert_eq!(project_dir_name("C:\\\u{ff21}\u{ff22}\\app"), "C-----app");
        assert_eq!(project_dir_name(""), "");
    }

    /// 上限を超える cwd は**先頭 200 文字 + `-` + ハッシュ**へ畳む。
    /// 畳んだ後も cwd ごとに違う名前になる（別プロジェクトの transcript を指さない）
    #[test]
    fn a_long_working_directory_is_folded_with_a_hash() {
        let long = format!("C:\\{}", "a".repeat(DIR_NAME_LIMIT));
        let encoded = format!("C--{}", "a".repeat(DIR_NAME_LIMIT));
        let name = project_dir_name(&long);
        assert!(name.len() > DIR_NAME_LIMIT, "not folded: {}", name.len());
        let (head, hash) = name.split_at(DIR_NAME_LIMIT);
        assert_eq!(
            head,
            &encoded[..DIR_NAME_LIMIT],
            "head 200 chars differ from the replaced string"
        );
        assert!(hash.starts_with('-'), "missing separator: {hash:?}");
        assert!(
            hash[1..].bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
            "not base36: {hash:?}"
        );

        // 上限ちょうどまでは畳まない（境界で名前が変わらない）
        let exact = "a".repeat(DIR_NAME_LIMIT);
        assert_eq!(project_dir_name(&exact), exact);

        // 先頭 200 文字が同じでも、後ろが違えば別の名前になる
        let a = format!("{}x", "a".repeat(DIR_NAME_LIMIT));
        let b = format!("{}y", "a".repeat(DIR_NAME_LIMIT));
        assert_ne!(project_dir_name(&a), project_dir_name(&b));
    }

    /// base36 と Java 風ハッシュ（畳んだ名前の後ろ半分を決める材料）
    #[test]
    fn the_folded_name_uses_a_base36_java_style_hash() {
        assert_eq!(base36(0), "0");
        assert_eq!(base36(35), "z");
        assert_eq!(base36(36), "10");
        // Java の "abc".hashCode() は 96354
        assert_eq!(java_hash("abc"), 96354);
        assert_eq!(java_hash(""), 0);
    }

    /// **git worktree は別のディレクトリになる。** 実機で拾えなかった実例と
    /// 同じ組み合わせ（行の cwd はリポジトリの根、transcript は worktree 側）
    #[test]
    fn a_worktree_gets_a_directory_of_its_own() {
        let repo = "C:\\Users\\admin\\Documents\\Work\\claude-kaizen";
        let worktree = format!("{repo}\\.claude\\worktrees\\fix+kaizen-window-and-design");
        assert_eq!(
            project_dir_name(repo),
            "C--Users-admin-Documents-Work-claude-kaizen"
        );
        assert_eq!(
            project_dir_name(&worktree),
            "C--Users-admin-Documents-Work-claude-kaizen--claude-worktrees-fix-kaizen-window-and-design"
        );
        assert_ne!(project_dir_name(repo), project_dir_name(&worktree));
    }

    /// transcript のファイル名は `<session-id>.jsonl`
    #[test]
    fn a_transcript_is_named_after_its_session() {
        assert_eq!(
            transcript_file_name("84a3d2c8-029c-472d-9180-6e1e2e304242"),
            "84a3d2c8-029c-472d-9180-6e1e2e304242.jsonl"
        );
    }
}
