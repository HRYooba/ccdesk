//! 行の表示名（title）の決め方。**優先順の正本は `docs/foreground-migration.md`**。
//!
//! 優先順は CLI 本体と同じで、出どころは 3 種類に分かれる:
//!
//! | 優先 | 出どころ | [`TitleSource`] |
//! |:--|:--|:--|
//! | 1 | ccdesk のリネーム | `Custom` |
//! | 2 | transcript の `custom-title` | `Custom` |
//! | 3 | transcript の `ai-title` | `Ai` |
//! | 4 | transcript の `last-prompt` | `LastPrompt` |
//! | 5 | 起動時に渡したプロンプト（＝ そのセッションの先頭ユーザープロンプト） | `FirstPrompt` |
//! | 6 | プロンプト無しで起こしたセッション | `Derived` |
//!
//! **5・6 は起動時に ccdesk が決める**（[`crate::app`] が行を作る時点）。行はすべて
//! ccdesk が起こしたものなので先頭プロンプトを知っており、transcript の先頭を
//! 読み直す必要が無い ＝ 末尾しか読まない設計と噛み合う。
//!
//! **transcript は非公開の内部形式**（`~/.claude/projects/**/*.jsonl`）。形が変われば
//! 2〜4 が拾えなくなるが、そのときは起動時に決めた名前（5・6）が残るだけで機能は
//! 落ちない。パースは行単位で捨てるので壊れた JSON でも panic しない。

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::sessions::{SessionId, SessionRow, TitleSource};

/// 行に出す表示名の桁数。**名前の長さの正本はここ 1 箇所**
/// （起動時のプロンプト由来も transcript 由来も同じ長さに揃える）
pub(crate) const TITLE_LEN: usize = 30;

/// プロンプト無しで起こしたセッションの表示名（[`TitleSource::Derived`]）
pub(crate) const UNTITLED: &str = "new session";

/// transcript のディレクトリ名の上限。超えたら先頭 [`DIR_NAME_LIMIT`] 文字 +
/// `-` + ハッシュになる（claude 本体の規則。実測）
const DIR_NAME_LIMIT: usize = 200;

/// transcript の末尾から読む量。
///
/// **全部は読まない**: 1 セッションの `.jsonl` は 1 MB を超えることがあり、
/// 一覧の周期処理で全行パースすると行数ぶんの帯域を毎周使う。手元の 538 本を
/// 測ったとき、最後の `ai-title` は EOF から最大 55 KiB・99% が 33 KiB 以内に
/// あったので 64 KiB を取る。**足りなくても壊れない**（下位の候補へ落ちるだけ）
const TAIL_BYTES: u64 = 64 * 1024;

/// transcript の 1 行から拾う候補（型名・値のキー・出どころ）。**この配列の順序が
/// 優先順そのもの**なので、候補を増やすときに触るのはここだけ
const CANDIDATES: [(&str, &str, TitleSource); 3] = [
    ("custom-title", "customTitle", TitleSource::Custom),
    ("ai-title", "aiTitle", TitleSource::Ai),
    ("last-prompt", "lastPrompt", TitleSource::LastPrompt),
];

/// 表示名として使える 1 行へ整える。改行・連続空白は 1 つの空白へ畳み、
/// [`TITLE_LEN`] 文字で切る。
///
/// **サイドバーは 1 行**なので、改行や制御文字をそのまま入れると行が崩れる
/// （プロンプトは複数行、transcript の値はユーザーが打った文字そのもの）
pub(crate) fn title_text(raw: &str) -> String {
    let mut out = String::new();
    let mut len = 0usize;
    let mut gap = false;
    for ch in raw.chars() {
        if ch.is_whitespace() || ch.is_control() {
            gap = len > 0; // 先頭の空白は落とす（末尾の空白は詰めた時点で消える）
            continue;
        }
        if len >= TITLE_LEN {
            break;
        }
        if gap {
            out.push(' ');
            len += 1;
            gap = false;
            if len >= TITLE_LEN {
                break;
            }
        }
        out.push(ch);
        len += 1;
    }
    // 桁の切れ目が語の切れ目に当たると、詰めた空白が末尾に残る
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// cwd から transcript のディレクトリ名を導く。**claude 本体の規則の写し**（実測）:
/// 英数字以外をすべて `-` へ置換し、[`DIR_NAME_LIMIT`] 文字を超えたら先頭
/// [`DIR_NAME_LIMIT`] 文字 + `-` + ハッシュ（Java 風 hash の絶対値の base36）。
///
/// 置換後は ASCII だけになるので、文字数・UTF-16 単位数・バイト数が一致する
/// （日本語を含む cwd でも数え方で結果が割れない）
fn project_dir_name(cwd: &str) -> String {
    let encoded: String = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    if encoded.len() <= DIR_NAME_LIMIT {
        return encoded;
    }
    format!(
        "{}-{}",
        &encoded[..DIR_NAME_LIMIT],
        base36(java_hash(&encoded).unsigned_abs())
    )
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

/// その行の transcript のパス（`~/.claude/projects/<dir>/<session-id>.jsonl`）。
/// 設定ディレクトリの位置は [`ccdesk::claude_dir`] が持つ（`CLAUDE_CONFIG_DIR` に追従）
fn transcript_path(cwd: &str, session_id: &SessionId) -> Option<PathBuf> {
    Some(
        ccdesk::claude_dir()?
            .join("projects")
            .join(project_dir_name(cwd))
            .join(format!("{session_id}.jsonl")),
    )
}

/// ファイルの末尾 `bytes` バイトを文字列で読む。
///
/// 途中から読むので**先頭行は壊れている**（行の途中・UTF-8 の途中で切れる）。
/// 壊れたバイトは lossy で受け、先頭の 1 行は丸ごと落とす
fn read_tail(path: &Path, bytes: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let from = len.saturating_sub(bytes);
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    if from == 0 {
        return Some(text);
    }
    Some(match text.find('\n') {
        Some(at) => text[at + 1..].to_string(),
        // 1 行も完結していない ＝ 拾える候補が無い
        None => String::new(),
    })
}

/// transcript の末尾から表示名を選ぶ。
///
/// 各候補は**最後に現れたものを採る**（セッション中に何度も追記され、末尾側が最新）。
/// 選ぶのは [`CANDIDATES`] の順（＝ 優先順）で、**上位が 1 つでも見つかれば
/// 下位は見ない**。壊れた行・知らない形は捨てる
fn pick_title(tail: &str) -> Option<(String, TitleSource)> {
    let mut found: [Option<String>; CANDIDATES.len()] = [None, None, None];
    for line in tail.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue; // 壊れた行（書き込みの途中で読んだ場合を含む）
        };
        let Some(kind) = value.get("type").and_then(Value::as_str) else {
            continue;
        };
        for (i, (name, key, _)) in CANDIDATES.iter().enumerate() {
            if kind != *name {
                continue;
            }
            let text = value.get(key).and_then(Value::as_str).map(title_text);
            if let Some(text) = text.filter(|t| !t.is_empty()) {
                found[i] = Some(text);
            }
        }
    }
    CANDIDATES
        .iter()
        .zip(found)
        .find_map(|((_, _, source), text)| text.map(|text| (text, *source)))
}

/// transcript を追って行の表示名を決める。
///
/// **変化した transcript だけを読む**のが要点: 一覧は 2 秒ごとに走るので、
/// 毎周すべての行の末尾を読むと、動いていないセッションのファイルまで舐め続ける。
/// 長さと更新時刻が前回と同じなら追記されていないので読まない
#[derive(Default)]
pub(crate) struct TitleWatcher {
    /// 最後に読んだ transcript の見え方（長さ・更新時刻）
    seen: HashMap<SessionId, Stamp>,
}

/// transcript が前回から変わったかの判定材料（長さ・更新時刻）
type Stamp = (u64, Option<std::time::SystemTime>);

impl TitleWatcher {
    /// 行の transcript から表示名を拾う。**読む必要が無い（追記されていない）**
    /// / transcript が無い / 候補が見つからない、のいずれも None
    pub(crate) fn poll(&mut self, row: &SessionRow) -> Option<(String, TitleSource)> {
        let path = transcript_path(&row.cwd, &row.session_id)?;
        let meta = std::fs::metadata(&path).ok()?;
        let stamp = (meta.len(), meta.modified().ok());
        if self.seen.insert(row.session_id.clone(), stamp) == Some(stamp) {
            return None;
        }
        pick_title(&read_tail(&path, TAIL_BYTES)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(kind: &str, key: &str, value: &str) -> String {
        format!(r#"{{"type":"{kind}","{key}":"{value}"}}"#)
    }

    /// 表示名は 1 行に畳んで [`TITLE_LEN`] 文字で切る
    /// （改行入りのプロンプトがそのまま行に入るとサイドバーが崩れる）
    #[test]
    fn a_title_is_folded_into_one_line_and_cut_to_the_display_width() {
        assert_eq!(title_text("  fix login\n\tform  validation  "), "fix login form validation");
        assert_eq!(title_text(""), "");
        assert_eq!(title_text(" \n\t "), "");
        // ちょうど / 超過。切るのは文字数（バイト数ではない）
        let exact = "a".repeat(TITLE_LEN);
        assert_eq!(title_text(&exact), exact);
        assert_eq!(title_text(&"a".repeat(TITLE_LEN + 10)).chars().count(), TITLE_LEN);
        // 日本語ではなく全角ラテンを使う（マルチバイト文字であることを検証したいだけで、
        // tests/no_japanese_in_code.rs のチェック対象を避けるため）
        assert_eq!(title_text(&"Ａ".repeat(TITLE_LEN + 10)).chars().count(), TITLE_LEN);
        // 切れ目に空白が来ても、詰めた空白で桁が溢れない
        let words = "ab ".repeat(TITLE_LEN);
        let folded = title_text(&words);
        assert!(folded.chars().count() <= TITLE_LEN, "width overflowed: {folded:?}");
        assert!(!folded.ends_with(' '), "trailing whitespace remains: {folded:?}");
    }

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
        assert_eq!(project_dir_name("C:\\ＡＢ\\app"), "C-----app");
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
        assert_eq!(head, &encoded[..DIR_NAME_LIMIT], "head 200 chars differ from the replaced string");
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

    /// **優先順どおりに選ぶ**（上位が居れば下位は見ない）
    #[test]
    fn the_title_follows_the_priority_of_its_sources() {
        let custom = line("custom-title", "customTitle", "hand-written title");
        let ai = line("ai-title", "aiTitle", "ai-written title");
        let prompt = line("last-prompt", "lastPrompt", "last prompt");
        let all = format!("{prompt}\n{ai}\n{custom}\n");
        assert_eq!(
            pick_title(&all),
            Some(("hand-written title".to_string(), TitleSource::Custom))
        );
        assert_eq!(
            pick_title(&format!("{prompt}\n{ai}\n")),
            Some(("ai-written title".to_string(), TitleSource::Ai))
        );
        assert_eq!(
            pick_title(&prompt),
            Some(("last prompt".to_string(), TitleSource::LastPrompt))
        );
        // **順序ではなく優先順で決まる**（上位が先に書かれていても上位が勝つ）
        assert_eq!(
            pick_title(&format!("{custom}\n{ai}\n{prompt}\n")),
            Some(("hand-written title".to_string(), TitleSource::Custom))
        );
    }

    /// 同じ種類が何度も追記されるので、**最後に現れたものが最新**
    #[test]
    fn the_last_occurrence_of_each_source_wins() {
        let tail = [
            line("ai-title", "aiTitle", "old name"),
            line("ai-title", "aiTitle", "new name"),
        ]
        .join("\n");
        assert_eq!(
            pick_title(&tail),
            Some(("new name".to_string(), TitleSource::Ai))
        );
    }

    /// **壊れた行・知らない形は捨てて、拾えるものだけ拾う**（transcript は非公開の
    /// 内部形式なので、形が変わっても title が下位へ落ちるだけで済ませる）
    #[test]
    fn broken_and_unknown_lines_fall_back_instead_of_failing() {
        let ai = line("ai-title", "aiTitle", "ai-written title");
        let tail = [
            "{\"type\":\"ai-title\",\"aiTitle\":\"cut off partway",
            "not json at all",
            "",
            r#"{"type":"assistant","message":{}}"#,
            r#"{"type":"ai-title"}"#,
            r#"{"type":"ai-title","aiTitle":""}"#,
            r#"{"type":"ai-title","aiTitle":7}"#,
            &ai,
        ]
        .join("\n");
        assert_eq!(
            pick_title(&tail),
            Some(("ai-written title".to_string(), TitleSource::Ai))
        );
        // 拾えるものが 1 つも無ければ None（呼び手は起動時に決めた名前を保つ）
        for tail in ["", "not json", r#"{"type":"user","message":{}}"#] {
            assert_eq!(pick_title(tail), None, "built a title out of {tail:?}");
        }
    }

    /// 末尾読みは**先頭の壊れた行を落とす**。読んだ範囲に候補が無ければ None
    #[test]
    fn reading_the_tail_drops_the_partial_first_line() {
        let dir = std::env::temp_dir().join(format!(
            "ccdesk-title-tail-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let ai = line("ai-title", "aiTitle", "tail name");
        std::fs::write(&path, format!("{}\n{ai}\n", line("ai-title", "aiTitle", "head name")))
            .unwrap();

        // 全部読めば先頭も見える（が、最後に現れた方を採る）
        assert_eq!(
            pick_title(&read_tail(&path, 4096).unwrap()),
            Some(("tail name".to_string(), TitleSource::Ai))
        );
        // 末尾だけを読むと先頭の行は（壊れているので）落ちる。
        // 最後の行 + その手前の改行だけが入る量にする
        let tail = read_tail(&path, ai.len() as u64 + 2).unwrap();
        assert!(!tail.contains("head name"), "kept the broken head line: {tail:?}");
        assert_eq!(
            pick_title(&tail),
            Some(("tail name".to_string(), TitleSource::Ai))
        );
        // 1 行も完結しない量しか読めなければ、拾える候補は無い
        assert_eq!(pick_title(&read_tail(&path, 3).unwrap()), None);
        assert!(read_tail(&dir.join("missing.jsonl"), 4096).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
