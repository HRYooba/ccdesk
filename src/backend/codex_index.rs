//! codex の会話名の読み取り（`~/.codex/session_index.jsonl`）。
//!
//! **codex 側に正本がある。** claude は transcript を舐めて名前を導く必要があるが
//! （[`crate::title`]）、codex は `{"id":…,"thread_name":…,"updated_at":…}` を
//! 1 行 1 会話で持っている。**ここは読むだけ**で、ccdesk は 1 バイトも書かない。
//!
//! 非公開の内部形式なので、形が変われば名前が拾えなくなるだけ（行は
//! [`crate::title::UNTITLED`] で出る）。行単位で捨てるので壊れた JSON でも panic しない。

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::Value;

/// 索引ファイルの項目キー。**読みはここ 1 箇所**（綴りを 2 箇所に持たない）
const ID_KEY: &str = "id";
const NAME_KEY: &str = "thread_name";

/// `~/.codex`。**`CODEX_HOME` を尊重する**（codex 自身と同じ解決順）
fn codex_home() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("CODEX_HOME")
        && !home.trim().is_empty()
    {
        return Some(PathBuf::from(home));
    }
    Some(dirs_home()?.join(".codex"))
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

fn index_path() -> Option<PathBuf> {
    Some(codex_home()?.join("session_index.jsonl"))
}

/// `$CODEX_HOME/auth.json` の指紋（大きさと更新時刻）。
/// **読めなければ None**（周期フォールバックだけが効く）
pub(crate) fn auth_fingerprint() -> crate::poll::CredentialsFp {
    let meta = std::fs::metadata(codex_home()?.join("auth.json")).ok()?;
    Some((meta.len(), meta.modified().ok()?))
}

/// codex が自分で書いた更新チェックの結果（`version.json` の `latest_version`）。
///
/// **自前で配布エンドポイントを叩かない。** codex は起動時に自分で確認して
/// この値を残すので、ccdesk はそれを読むだけで足りる（ネットワークへ出ない）。
/// 非公開の内部ファイルなので、形が変われば「更新あり」が出なくなるだけ
pub(crate) fn latest_version() -> Option<String> {
    let path = codex_home()?.join("version.json");
    let value: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let latest = value.get("latest_version")?.as_str()?.trim();
    (!latest.is_empty()).then(|| latest.to_string())
}

/// codex の会話名（`agent_session_id` → 名前）。
///
/// **ファイルの大きさが変わったときだけ読み直す。** 索引は 1 会話 1 行の追記型で
/// 実測 11 KB 程度だが、一覧の読み直しは 2 秒ごとなので、変わっていないファイルを
/// 舐め続ける理由が無い。
#[derive(Default)]
pub(crate) struct CodexNames {
    names: HashMap<String, String>,
    /// 最後に読んだときのファイルサイズ。**None は「まだ読んでいない」**
    seen_len: Option<u64>,
}

impl CodexNames {
    /// 索引を読み直す（変わっていなければ何もしない）。
    /// **読めないときは前回の表を保つ**（一時的な失敗で名前が消えない）
    pub(crate) fn refresh(&mut self) {
        let Some(path) = index_path() else {
            return;
        };
        let Ok(meta) = std::fs::metadata(&path) else {
            return;
        };
        if self.seen_len == Some(meta.len()) {
            return;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        self.seen_len = Some(meta.len());
        self.names = parse(&text);
    }

    pub(crate) fn get(&self, agent_session_id: &str) -> Option<&str> {
        self.names.get(agent_session_id).map(String::as_str)
    }
}

/// 索引本文 → `id` → 名前。**行単位で捨てる**（1 行壊れても他は読む）
fn parse(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|line| {
            let value: Value = serde_json::from_str(line).ok()?;
            let id = value.get(ID_KEY)?.as_str()?;
            let name = value.get(NAME_KEY)?.as_str()?.trim();
            (!id.is_empty() && !name.is_empty())
                .then(|| (id.to_string(), name.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_read_for_each_thread() {
        let text = concat!(
            r#"{"id":"a","thread_name":"first thread","updated_at":"2026-06-11T04:41:43Z"}"#,
            "\n",
            r#"{"id":"b","thread_name":"second","updated_at":"2026-06-11T04:41:49Z"}"#,
            "\n",
        );
        let names = parse(text);
        assert_eq!(names.get("a").map(String::as_str), Some("first thread"));
        assert_eq!(names.get("b").map(String::as_str), Some("second"));
    }

    /// **1 行壊れても他の行は読む**（索引は codex が書く外部ファイルで、
    /// 書き込み途中を読むことがある）
    #[test]
    fn a_broken_line_does_not_take_the_rest_of_the_index_with_it() {
        let text = concat!(
            "{not json\n",
            r#"{"id":"b","thread_name":"kept"}"#,
            "\n",
            r#"{"thread_name":"no id"}"#,
            "\n",
            r#"{"id":"c","thread_name":"   "}"#,
            "\n",
        );
        let names = parse(text);
        assert_eq!(names.get("b").map(String::as_str), Some("kept"));
        assert_eq!(names.len(), 1, "an unusable row was kept: {names:?}");
    }
}
