//! 行の表示名（title）の受け渡し。**正本は transcript**
//! （`~/.claude/projects/<エンコード済み cwd>/<session-id>.jsonl`）で、
//! ここはその読みと書きの両方を持つ。優先順の正本は `docs/foreground-migration.md`。
//!
//! 優先順は CLI 本体と同じで、出どころは 2 種類に分かれる:
//!
//! | 優先 | 出どころ | [`TitleSource`] |
//! |:--|:--|:--|
//! | 1 | transcript の `custom-title`（ccdesk のリネームと claude の `/rename`） | `Custom` |
//! | 2 | transcript の `ai-title` | `Ai` |
//! | 3 | transcript の `last-prompt` | `LastPrompt` |
//! | 4 | 起動時に渡したプロンプト（＝ そのセッションの先頭ユーザープロンプト） | `FirstPrompt` |
//! | 5 | プロンプト無しで起こしたセッション | `Derived` |
//!
//! **4・5 は起動時に ccdesk が決める**（[`crate::app`] が行を作る時点）。行はすべて
//! ccdesk が起こしたものなので先頭プロンプトを知っており、transcript の先頭を
//! 読み直す必要が無い ＝ 末尾しか読まない設計と噛み合う。
//!
//! **ccdesk のリネームも 1 番へ書く**（[`Titles::set_custom`]）。書く先を claude の
//! `/rename` と同じ 1 箇所にしてあるので、どちらで名前を変えても両方に映る
//! （別の場所に持つと、同じセッションの名前が 2 つある状態になる）。
//!
//! **transcript は非公開の内部形式。** 形が変われば 1〜3 が拾えなくなるが、そのときは
//! 起動時に決めた名前（4・5）が残るだけで機能は落ちない。パースは行単位で捨てるので
//! 壊れた JSON でも panic しない。**書き込みも失敗しても落ちない**
//! （transcript は会話の履歴そのもので、`claude -r` の材料 ＝ 壊さないことが第一）。

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use ccdesk::log_error;

use crate::sessions::{SessionId, SessionRow, TitleSource};

/// 行に出す表示名の桁数。**名前の長さの正本はここ 1 箇所**
/// （起動時のプロンプト由来も transcript 由来も同じ長さに揃える）
pub(crate) const TITLE_LEN: usize = 30;

/// プロンプト無しで起こしたセッションの表示名（[`TitleSource::Derived`]）
pub(crate) const UNTITLED: &str = "new session";

/// transcript のディレクトリ名の上限。超えたら先頭 [`DIR_NAME_LIMIT`] 文字 +
/// `-` + ハッシュになる（claude 本体の規則。実測）
const DIR_NAME_LIMIT: usize = 200;

/// 「末尾」と呼ぶ量。**[`Span::Appended`] の候補にとって十分な幅**で、
/// 手元の 538 本を測ったとき、最後の `ai-title` は EOF から最大 55 KiB・
/// 99% が 33 KiB 以内にあった。
///
/// **これは全候補に効く上限ではない**（[`Span`]）。1 度しか書かれない候補は
/// この外に出るので、初回だけ先頭から読む
const TAIL_BYTES: u64 = 64 * 1024;

/// ユーザーが付けた名前の行の型名と値のキー。**読み（[`CANDIDATES`] の先頭）と
/// 書き（[`custom_title_line`]）が同じ 1 組を見る**ので、綴りが片側だけ変わらない
const CUSTOM_TITLE: (&str, &str) = ("custom-title", "customTitle");

/// 候補が transcript の**どこに現れるか**。走査の範囲はこの性質から機械的に決まる
/// （[`first_scan_from`] / [`Titles::poll`]）ので、候補と範囲の対応表を別に持たない。
///
/// **この区別を落とすと実害が出る**: `custom-title` を末尾 64 KiB だけで探していた
/// 頃は、長い会話の早い段階でリネームした記録が範囲の外に出て拾えず、
/// transcript 全体を読む `/resume` のピッカーと名前が食い違っていた
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Span {
    /// セッション中に繰り返し追記される。最新は必ず末尾側にあるので末尾で足りる
    Appended,
    /// リネームの瞬間に 1 度だけ書かれる。**ファイルのどこにあるか分からない**
    Once,
}

/// transcript の 1 行から拾う候補（型名・値のキー・出どころ・現れる場所）。
/// **この配列の順序が優先順そのもの**なので、候補を増やすときに触るのはここだけ
const CANDIDATES: [(&str, &str, TitleSource, Span); 3] = [
    (CUSTOM_TITLE.0, CUSTOM_TITLE.1, TitleSource::Custom, Span::Once),
    ("ai-title", "aiTitle", TitleSource::Ai, Span::Appended),
    ("last-prompt", "lastPrompt", TitleSource::LastPrompt, Span::Appended),
];

/// 走査 1 回で見つかった候補（[`CANDIDATES`] と同じ並び）
type Found = [Option<String>; CANDIDATES.len()];

/// **初回にファイルのどこから読むか。** [`CANDIDATES`] の性質から導く:
/// 1 度しか書かれない候補が 1 つでもあれば先頭から、全部が追記型なら末尾だけ
fn first_scan_from(len: u64) -> u64 {
    if CANDIDATES.iter().any(|(_, _, _, span)| *span == Span::Once) {
        0
    } else {
        len.saturating_sub(TAIL_BYTES)
    }
}

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

/// transcript を置くディレクトリ（`~/.claude/projects`）。設定ディレクトリの位置は
/// [`ccdesk::claude_dir`] が持つ（`CLAUDE_CONFIG_DIR` に追従）
fn projects_dir() -> Option<PathBuf> {
    Some(ccdesk::claude_dir()?.join("projects"))
}

/// ファイルの `from` バイト目から終わりまでを文字列で読む
/// （壊れたバイトは lossy で受ける ＝ 途中から読んでも失敗しない）
fn read_from(path: &Path, from: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// `want` バイト目以前で一番後ろの行の切れ目（先頭からの位置）。
/// 走査の範囲を行の途中で割らないための計算で、`\n` の位置しか見ないので
/// UTF-8 の境界を踏み外さない
fn line_boundary_before(text: &str, want: usize) -> usize {
    let mut at = 0;
    for (index, _) in text.match_indices('\n') {
        if index + 1 > want {
            break;
        }
        at = index + 1;
    }
    at
}

/// 範囲 `text` を走査して候補を拾う。`spans` に載る性質の候補だけを見るので、
/// 「末尾でしか探さない候補」と「全体で探す候補」を同じ 1 つの走査で表せる。
///
/// **後から現れた値が前の値を上書きする**（同じ候補は最後に現れたものが最新）。
/// 壊れた行・知らない形は捨てる。
///
/// **JSON を組む前に型名の文字列で弾く**のが速さの要点: transcript は 1 MB を
/// 超えることがあり、全行をパースすると走査 1 回に行数ぶんの時間がかかる
fn scan_into(text: &str, spans: &[Span], found: &mut Found) {
    let wanted = |span: &Span| spans.contains(span);
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        if !CANDIDATES
            .iter()
            .any(|(name, _, _, span)| wanted(span) && line.contains(name))
        {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue; // 壊れた行（書き込みの途中で読んだ場合を含む）
        };
        let Some(kind) = value.get("type").and_then(Value::as_str) else {
            continue;
        };
        for (i, (name, key, _, span)) in CANDIDATES.iter().enumerate() {
            if kind != *name || !wanted(span) {
                continue;
            }
            let text = value.get(key).and_then(Value::as_str).map(title_text);
            if let Some(text) = text.filter(|t| !t.is_empty()) {
                found[i] = Some(text);
            }
        }
    }
}

/// 拾った候補から表示名を選ぶ。順は [`CANDIDATES`]（＝ 優先順）で、
/// **上位が 1 つでも見つかれば下位は見ない**
fn pick(found: &Found) -> Option<(String, TitleSource)> {
    CANDIDATES
        .iter()
        .zip(found)
        .find_map(|((_, _, source, _), text)| text.clone().map(|text| (text, *source)))
}

/// すべての候補をすべての範囲で探す（走査を範囲で分けない場合の指定）
const EVERY_SPAN: [Span; 2] = [Span::Once, Span::Appended];

/// ccdesk のリネームが追記する 1 行。**`sessionId` まで入れるのは claude 自身が
/// 書く形と同じにするため**（実測: `-n` で渡した名前も `/rename` もこの形で残る）。
///
/// 組み立てを serde に任せるので、名前に `"` や改行が入っても 1 行に収まる
/// （手で文字列を連結すると、その 1 つで transcript が壊れる）
fn custom_title_line(session_id: &SessionId, title: &str) -> String {
    let (kind, key) = CUSTOM_TITLE;
    json!({ "type": kind, key: title, "sessionId": session_id.as_str() }).to_string()
}

/// transcript へ 1 行追記する。**claude が同じファイルへ書いている最中に割り込む
/// 可能性がある**ので、守ることが 2 つある:
///
/// - **1 行を 1 回の write で書く**（改行まで含めて 1 つのバッファにする）。
///   行の途中で相手の書き込みが挟まると、両方の行が壊れて読めなくなる
/// - **末尾が改行で終わっていなければ先に改行を足す**（相手が書きかけの行の
///   続きに自分の JSON を貼り付けない）
fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    let mut file = std::fs::File::options().append(true).open(path)?;
    let payload = if ends_with_newline(path)? {
        format!("{line}\n")
    } else {
        format!("\n{line}\n")
    };
    file.write_all(payload.as_bytes())
}

/// 末尾が改行か（空ファイルは「改行で終わっている」扱い ＝ 足す必要が無い）
fn ends_with_newline(path: &Path) -> std::io::Result<bool> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(true);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    Ok(last[0] == b'\n')
}

/// 1 本の transcript について「どこまで読んだか」と「そこまでで見つかった候補」。
///
/// **走査の状態はこの 1 つに集める**（変化検知の材料も含める）ので、
/// 「読んだ範囲」と「拾った値」が別々に古くならない。transcript は追記しか
/// されないので、**一度読んだ範囲は二度と読まない**（初回に全体を読んでも、
/// 以降の周期で読むのは増えたぶんだけ）
#[derive(Default)]
struct Scan {
    /// 最後に読んだときのファイルの見え方（長さ・更新時刻）。None ＝ 未走査
    stamp: Option<Stamp>,
    /// 走査済みの末尾位置。**必ず行の切れ目**なので、次はここから読めば
    /// 行の途中から始まらない（書きかけの最終行は次回もう一度読む）
    scanned: u64,
    /// そこまでで見つかった候補
    found: Found,
}

/// transcript と行の表示名のやり取り（読みと書き）。
///
/// **変化した transcript だけを読む**のが読みの要点: 一覧は 2 秒ごとに走るので、
/// 毎周すべての行を舐めると、動いていないセッションのファイルまで読み続ける。
/// 長さと更新時刻が前回と同じなら追記されていないので読まない
/// （どこまで読んだかは [`Scan`]）。
///
/// **パスは注入で受ける**（[`Self::default`] が既定の `~/.claude/projects` を入れる）:
/// 理由は [`crate::sessions`] と同じで、テストが実ユーザーの transcript を
/// 絶対に触らないため
pub(crate) struct Titles {
    /// transcript を探す根（`~/.claude/projects`）。取れない環境では None ＝
    /// 読みも書きも何もしない
    projects: Option<PathBuf>,
    /// 行ごとの走査の状態（変化検知と拾った候補）
    seen: HashMap<SessionId, Scan>,
    /// **まだ transcript へ載せられていないリネーム。** transcript は 1 ターン目が
    /// 終わるまで作られないので、その間のリネームはここで持って次の [`Self::flush`]
    /// で載せに行く。
    ///
    /// **自分でファイルを作らない**のが判断: 空の transcript を置くと
    /// 「会話のあるセッション」に見えて再開の分岐（[`Self::has_transcript`]）が
    /// `claude -r` を選び、`No conversation found` になる。
    /// 諦めてもいけない（1 ターン目が `last-prompt` を書いた時点で、
    /// ユーザーが付けた名前が黙って上書きされる）ので、載せるまで持ち越す
    pending: HashMap<SessionId, String>,
}

/// transcript が前回から変わったかの判定材料（長さ・更新時刻）
type Stamp = (u64, Option<std::time::SystemTime>);

impl Default for Titles {
    fn default() -> Self {
        Self {
            projects: projects_dir(),
            seen: HashMap::new(),
            pending: HashMap::new(),
        }
    }
}

impl Titles {
    /// その行の transcript のパス。**導出はここ 1 箇所**（読み・書き・存在確認が
    /// 同じ計算を見る）
    fn path(&self, row: &SessionRow) -> Option<PathBuf> {
        Some(
            self.projects
                .as_ref()?
                .join(project_dir_name(&row.cwd))
                .join(format!("{}.jsonl", row.session_id)),
        )
    }

    /// その行に transcript があるか ＝ **1 ターン以上会話したか**。
    ///
    /// 再開の分岐に使う（[`crate::app`] の `open_session`）: 前景セッションは
    /// 1 ターン終わるまで transcript を作らないので、無い行に `claude -r` を
    /// 打つと `No conversation found` になる
    pub(crate) fn has_transcript(&self, row: &SessionRow) -> bool {
        self.path(row).is_some_and(|path| path.is_file())
    }

    /// 行の transcript から表示名を拾う。**読む必要が無い（追記されていない）**
    /// / transcript が無い / 候補が見つからない、のいずれも None。
    ///
    /// 読む範囲の決め方は 2 段:
    ///
    /// - **初回**は [`first_scan_from`]（＝ [`CANDIDATES`] の性質）が決める。
    ///   先頭側では 1 度しか書かれない候補（[`Span::Once`]）だけを探し、末尾
    ///   [`TAIL_BYTES`] では全部を探す ＝ 追記型の候補を先頭側でパースしない
    /// - **2 回目以降**は増えたぶんだけを全候補で走査する。transcript は追記しか
    ///   されないので、これで初回と同じ答えが保たれる（`/resume` のピッカーと同じ）
    pub(crate) fn poll(&mut self, row: &SessionRow) -> Option<(String, TitleSource)> {
        let path = self.path(row)?;
        let meta = std::fs::metadata(&path).ok()?;
        let stamp = (meta.len(), meta.modified().ok());
        let scan = self.seen.entry(row.session_id.clone()).or_default();
        if scan.stamp == Some(stamp) {
            return None;
        }
        // 縮んだ ＝ 追記ではなく作り直された。覚えた範囲も候補も当てにならない
        if meta.len() < scan.scanned {
            *scan = Scan::default();
        }
        let first = scan.stamp.is_none();
        let from = if first {
            first_scan_from(meta.len())
        } else {
            scan.scanned
        };
        let mut text = read_from(&path, from)?;
        let mut start = from;
        // 行の途中から読んだ初回だけ、壊れた先頭行を落とす
        // （追記ぶんの読み直しは常に行の切れ目から始まる）
        if first && from > 0 {
            let at = text.find('\n').map_or(text.len(), |at| at + 1);
            text = text.split_off(at);
            start += at as u64;
        }
        // 走査するのは行が完結している範囲まで（書きかけの最終行は次回に回す）
        let complete = text.rfind('\n').map_or(0, |at| at + 1);
        let text = &text[..complete];
        scan.stamp = Some(stamp);
        scan.scanned = start + complete as u64;
        if first {
            let split = line_boundary_before(text, text.len().saturating_sub(TAIL_BYTES as usize));
            scan_into(&text[..split], &[Span::Once], &mut scan.found);
            scan_into(&text[split..], &EVERY_SPAN, &mut scan.found);
        } else {
            scan_into(text, &EVERY_SPAN, &mut scan.found);
        }
        pick(&scan.found)
    }

    /// ユーザーが付けた名前を transcript へ載せる（claude の `/rename` と同じ場所）。
    /// **transcript が無い間は持ち越す**（[`Self::pending`]）
    pub(crate) fn set_custom(&mut self, row: &SessionRow, title: &str) {
        self.pending
            .insert(row.session_id.clone(), title.to_string());
        self.write_pending(row);
    }

    /// 持ち越したリネームを載せに行く（一覧の読み直しと同じ周期で呼ぶ）。
    /// 持ち越しが無ければ**ファイルを 1 つも触らない**
    pub(crate) fn flush(&mut self, rows: &[SessionRow]) {
        if self.pending.is_empty() {
            return;
        }
        for row in rows {
            if self.pending.contains_key(&row.session_id) {
                self.write_pending(row);
            }
        }
    }

    /// 1 行ぶんの持ち越しを書く。**書けたか諦めたかのどちらかで持ち越しを降ろす**
    /// （書き込みが失敗し続ける環境で毎周期叩き続けない。名前はストア側の
    /// 表示用キャッシュに残る）
    fn write_pending(&mut self, row: &SessionRow) {
        let Some(title) = self.pending.get(&row.session_id) else {
            return;
        };
        let Some(path) = self.path(row) else {
            self.pending.remove(&row.session_id);
            return;
        };
        if !path.is_file() {
            return; // まだ 1 ターンも終わっていない ＝ 次の周期で載せる
        }
        if let Err(err) = append_line(&path, &custom_title_line(&row.session_id, title)) {
            // transcript は会話の履歴そのものなので、書けなかったことを記録して諦める
            log_error(&format!("failed to write the title to {path:?}: {err}"));
        }
        self.pending.remove(&row.session_id);
    }
}

/// テスト用: 文字列を丸ごと走査して表示名を選ぶ（本番の [`Titles::poll`] は
/// 範囲を分けて走るので、範囲の話を抜きにした優先順の検査はこれを使う）
#[cfg(test)]
fn pick_title(text: &str) -> Option<(String, TitleSource)> {
    let mut found = Found::default();
    scan_into(text, &EVERY_SPAN, &mut found);
    pick(&found)
}

#[cfg(test)]
impl Titles {
    /// テスト用: transcript の根を差し替える（実ユーザーの transcript を絶対に触らない）
    pub(crate) fn with_projects(projects: PathBuf) -> Self {
        Self {
            projects: Some(projects),
            seen: HashMap::new(),
            pending: HashMap::new(),
        }
    }

    /// テスト用: その行の transcript を作る（**パスの導出は本番と同じ
    /// [`Self::path`]** ＝ テストが自分で組み立てた別のパスを見ない）
    pub(crate) fn write_transcript(&self, row: &SessionRow, contents: &str) {
        let path = self.path(row).expect("no transcript root");
        std::fs::create_dir_all(path.parent().expect("no parent")).expect("mkdir failed");
        std::fs::write(&path, contents).expect("write failed");
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn line(kind: &str, key: &str, value: &str) -> String {
        format!(r#"{{"type":"{kind}","{key}":"{value}"}}"#)
    }

    /// テスト専用の transcript 置き場（`~/.claude/projects` の代わり）。
    /// **実ユーザーの transcript を絶対に触らない**ための境界で、Drop で丸ごと消す。
    /// [`crate::app`] のテスト（起こし直し方の判断）も同じ道具を使う ＝
    /// transcript を作る手順を 2 通り持たない
    pub(crate) struct TempProjects(PathBuf);

    impl TempProjects {
        pub(crate) fn new(test: &str) -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "ccdesk-projects-{test}-{}-{seq}",
                std::process::id()
            ));
            std::fs::create_dir_all(&root).expect("mkdir failed");
            Self(root)
        }

        /// その置き場を見る [`Titles`]
        pub(crate) fn titles(&self) -> Titles {
            Titles::with_projects(self.0.clone())
        }
    }

    impl Drop for TempProjects {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// テスト用の行（cwd は transcript のディレクトリ名を決める材料）
    fn row(id: &str) -> SessionRow {
        SessionRow::new(
            SessionId::new(id),
            "C:\\dev\\app",
            "start name",
            TitleSource::FirstPrompt,
            1_000,
        )
    }

    /// **リネームは transcript へ `custom-title` の 1 行として載り、読み直しで
    /// 同じ値が返る**（ccdesk のリネームと claude の `/rename` が同じ場所を使う）。
    ///
    /// 併せて固定するのは 2 つ: **前の行を壊さない**（末尾が改行で終わっていない
    /// transcript でも、既存の行がそのまま読める）ことと、**追記が 1 行で収まる**こと
    #[test]
    fn a_rename_lands_in_the_transcript_as_a_custom_title_line() {
        let temp = TempProjects::new("a_rename_lands_in_the_transcript_as_a_custom_title_line");
        let mut titles = temp.titles();
        let row = row("11111111-1111-4111-8111-111111111111");
        // 末尾が改行で終わっていない transcript（claude が書いている途中の形）
        let existing = line("last-prompt", "lastPrompt", "the first prompt");
        titles.write_transcript(&row, &existing);
        assert!(titles.has_transcript(&row));

        titles.set_custom(&row, "hand-written name");

        let path = titles.path(&row).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "the append did not land as exactly one line: {text:?}");
        assert_eq!(lines[0], existing, "the line that was already there got broken");
        assert!(text.ends_with('\n'), "the appended line has no terminator: {text:?}");
        // 読み直すと同じ値が返る（読みと書きが同じ綴りを見ている）
        assert_eq!(
            pick_title(&text),
            Some(("hand-written name".to_string(), TitleSource::Custom))
        );
        // 追記した後の 1 行だけでも読める（`sessionId` まで claude と同じ形で入る）
        let written: Value = serde_json::from_str(lines[1]).expect("not valid json");
        assert_eq!(written["sessionId"], row.session_id.as_str());
    }

    /// **1 ターン前のリネームは持ち越して、transcript ができてから載せる。**
    ///
    /// 自分で transcript を作らないのが要点: 会話が無いのにファイルがあると
    /// 再開の分岐（[`Titles::has_transcript`]）が `claude -r` を選び、
    /// `No conversation found` になる。諦めるのも駄目で、1 ターン目が
    /// `last-prompt` を書いた時点でユーザーが付けた名前が黙って消える
    #[test]
    fn a_rename_before_the_first_turn_is_held_until_the_transcript_exists() {
        let temp =
            TempProjects::new("a_rename_before_the_first_turn_is_held_until_the_transcript_exists");
        let mut titles = temp.titles();
        let row = row("22222222-2222-4222-8222-222222222222");
        assert!(!titles.has_transcript(&row), "the premise (no transcript) broke");

        titles.set_custom(&row, "named before the first turn");
        assert!(
            !titles.has_transcript(&row),
            "created a transcript for a session that has no conversation"
        );
        // transcript が無いままの flush でも作らない（何度呼んでも同じ）
        titles.flush(std::slice::from_ref(&row));
        assert!(!titles.has_transcript(&row));

        // 1 ターン目が終わって claude が transcript を作った
        let first_turn = line("last-prompt", "lastPrompt", "the first prompt");
        titles.write_transcript(&row, &format!("{first_turn}\n"));
        titles.flush(std::slice::from_ref(&row));
        let text = std::fs::read_to_string(titles.path(&row).unwrap()).unwrap();
        assert_eq!(
            pick_title(&text),
            Some(("named before the first turn".to_string(), TitleSource::Custom)),
            "the held rename never landed: {text:?}"
        );

        // 載ったら持ち越しは降りる（同じ行を二度書かない）
        let before = text.lines().count();
        titles.flush(std::slice::from_ref(&row));
        let after = std::fs::read_to_string(titles.path(&row).unwrap())
            .unwrap()
            .lines()
            .count();
        assert_eq!(after, before, "the rename was appended twice");
    }

    /// **transcript が無い行は「まだ会話が無い」** ＝ 起こし直しは `claude -r` に
    /// できない（判断は [`crate::app`] の `relaunch`）。ディレクトリ名は cwd から
    /// 決まるので、cwd が違えば別のセッションの transcript を指さない
    #[test]
    fn the_transcript_of_a_row_is_found_by_its_cwd_and_id() {
        let temp = TempProjects::new("the_transcript_of_a_row_is_found_by_its_cwd_and_id");
        let titles = temp.titles();
        let row = row("33333333-3333-4333-8333-333333333333");
        assert!(!titles.has_transcript(&row));
        titles.write_transcript(&row, "");
        assert!(titles.has_transcript(&row), "the file that was just written is not found");
        // 同じ ID でも cwd が違えば別のパス（別プロジェクトの会話を指さない）
        let elsewhere = SessionRow {
            cwd: "C:\\dev\\other".to_string(),
            ..row.clone()
        };
        assert!(!titles.has_transcript(&elsewhere));
        assert_ne!(titles.path(&row), titles.path(&elsewhere));
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

    /// **1 度しか書かれない候補は、末尾 [`TAIL_BYTES`] より前にあっても拾う。**
    ///
    /// これが実機で起きた食い違いの直接の再現: 長い会話の早い段階でリネームすると
    /// `custom-title` は末尾の範囲から出るので、末尾しか読まない実装では
    /// `last-prompt` へ落ちる一方、transcript 全体を読む `/resume` のピッカーは
    /// リネームした名前を出す
    #[test]
    fn a_rename_before_the_tail_window_is_still_found() {
        let temp = TempProjects::new("a_rename_before_the_tail_window_is_still_found");
        let mut titles = temp.titles();
        let row = row("44444444-4444-4444-8444-444444444444");
        // 1 行目にリネーム、そのあと末尾の範囲を越える量の会話を積む
        let custom = line("custom-title", "customTitle", "named early on");
        let filler = line("assistant", "text", &"x".repeat(2_000));
        let bulk = std::iter::repeat_n(filler.as_str(), (TAIL_BYTES as usize / 2_000) + 8)
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = line("last-prompt", "lastPrompt", "the latest prompt");
        titles.write_transcript(&row, &format!("{custom}\n{bulk}\n{prompt}\n"));
        let path = titles.path(&row).unwrap();
        assert!(
            std::fs::metadata(&path).unwrap().len() > TAIL_BYTES,
            "the premise broke — the transcript fits in the tail window"
        );

        assert_eq!(
            titles.poll(&row),
            Some(("named early on".to_string(), TitleSource::Custom)),
            "the rename outside the tail window was not found"
        );
    }

    /// **2 回目以降は増えたぶんだけを読む**（transcript は追記しかされない）。
    /// 初回に拾った候補は覚えているので、**追記に含まれない上位の候補が
    /// 下位へ落ちない**（リネーム済みの行が次の発話で名前を失わない）
    #[test]
    fn later_polls_only_read_what_was_appended() {
        let temp = TempProjects::new("later_polls_only_read_what_was_appended");
        let mut titles = temp.titles();
        let row = row("55555555-5555-4555-8555-555555555555");
        let custom = line("custom-title", "customTitle", "kept name");
        titles.write_transcript(&row, &format!("{custom}\n"));
        assert_eq!(
            titles.poll(&row),
            Some(("kept name".to_string(), TitleSource::Custom))
        );
        // 変わっていなければ読まない
        assert_eq!(titles.poll(&row), None, "re-read a transcript that did not change");

        // 追記（上位の候補は含まれない）。覚えているので上位のまま
        let path = titles.path(&row).unwrap();
        let prompt = line("last-prompt", "lastPrompt", "a later prompt");
        append_line(&path, &prompt).unwrap();
        assert_eq!(
            titles.poll(&row),
            Some(("kept name".to_string(), TitleSource::Custom)),
            "the remembered custom title was lost when only the appended part was read"
        );

        // 追記に上位の候補が含まれれば、そちらへ更新される（リネーム直後）
        append_line(&path, &line("custom-title", "customTitle", "renamed later")).unwrap();
        assert_eq!(
            titles.poll(&row),
            Some(("renamed later".to_string(), TitleSource::Custom))
        );
    }

    /// **書きかけの最終行は走査済みにしない**（claude が書いている途中で読むと
    /// 行が途中で切れる）。次の周期で改行まで届いたら、そこで初めて拾える
    #[test]
    fn a_half_written_last_line_is_read_again_next_time() {
        let temp = TempProjects::new("a_half_written_last_line_is_read_again_next_time");
        let mut titles = temp.titles();
        let row = row("66666666-6666-4666-8666-666666666666");
        let prompt = line("last-prompt", "lastPrompt", "first prompt");
        let custom = line("custom-title", "customTitle", "arrives in two writes");
        let (head, tail) = custom.split_at(custom.len() / 2);
        titles.write_transcript(&row, &format!("{prompt}\n{head}"));
        assert_eq!(
            titles.poll(&row),
            Some(("first prompt".to_string(), TitleSource::LastPrompt))
        );
        // 残りが届いた（行が完結した）
        titles.write_transcript(&row, &format!("{prompt}\n{head}{tail}\n"));
        assert_eq!(
            titles.poll(&row),
            Some(("arrives in two writes".to_string(), TitleSource::Custom))
        );
    }

    /// **大きい transcript でも走査は現実的な時間で終わる**（型名の文字列で
    /// 先に弾くので、行数ぶんの JSON パースをしない）。
    ///
    /// 上限は実測の 30 倍以上に取ってある ＝ 遅い CI でも揺れないが、
    /// 「全行をパースする実装へ戻した」ときは超える
    #[test]
    fn a_large_transcript_is_scanned_quickly() {
        let temp = TempProjects::new("a_large_transcript_is_scanned_quickly");
        let mut titles = temp.titles();
        let row = row("77777777-7777-4777-8777-777777777777");
        let filler = line("assistant", "text", &"x".repeat(1_000));
        let bulk = std::iter::repeat_n(filler.as_str(), 1_200)
            .collect::<Vec<_>>()
            .join("\n");
        titles.write_transcript(
            &row,
            &format!("{bulk}\n{}\n", line("ai-title", "aiTitle", "big one")),
        );
        assert!(
            std::fs::metadata(titles.path(&row).unwrap()).unwrap().len() > 1_000_000,
            "the premise broke — the transcript is not large"
        );
        let started = std::time::Instant::now();
        assert_eq!(
            titles.poll(&row),
            Some(("big one".to_string(), TitleSource::Ai))
        );
        let took = started.elapsed();
        assert!(took < std::time::Duration::from_secs(2), "scanning took {took:?}");
    }

    /// 走査の範囲を行の途中で割らない（`\n` の位置だけを見る）
    #[test]
    fn the_scan_range_ends_on_a_line_boundary() {
        let text = "aaa\nbbb\nccc\n";
        assert_eq!(line_boundary_before(text, 0), 0);
        assert_eq!(line_boundary_before(text, 4), 4);
        assert_eq!(line_boundary_before(text, 7), 4);
        assert_eq!(line_boundary_before(text, 8), 8);
        assert_eq!(line_boundary_before(text, 999), 12);
        // 改行が 1 つも無ければ「切れ目は先頭だけ」
        assert_eq!(line_boundary_before("no newline here", 5), 0);
    }
}
