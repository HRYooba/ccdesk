//! 行の表示名（title）。**正本は transcript**
//! （`~/.claude/projects/<エンコード済み cwd>/<session-id>.jsonl`）で、
//! ここは**読むだけ**（優先順は下表）。
//!
//! # 表示名は保存しない
//!
//! 表示名は行に持たず、**そのつど transcript から導く**（[`Titles::of`]）。
//! 保存していた頃は正本が 2 つあり、ズレても気づけなかった。実害:
//!
//! - 「格下げしないガード」が要る → 入れると `/rename` が反映されない、の往復
//! - 名前が変わるたびに `updated_at` が動き、行の経過時間が 0s へ戻る
//! - 保存値が `new session` のまま固定され、transcript に材料があるのに直らない
//!
//! **[`Titles`] が持つのはキャッシュであって状態ではない。** 捨てても
//! 同じ答えになる（`the_answer_does_not_depend_on_the_cache` が固定する）。
//!
//! # ccdesk は claude の内部ファイルへ 1 バイトも書かない
//!
//! 名前を変えるのはペインの中で `/rename` を打つ形に一本化してある（PTY へ
//! 打鍵を流し込む UI 自動化も、transcript への直書きも、どちらも claude 側の形に
//! 依存して黙って壊れる）。この不変条件は
//! `reading_a_transcript_never_writes_to_it` が固定する。
//!
//! # 優先順
//!
//! | 優先 | 出どころ |
//! |:--|:--|
//! | 1 | transcript の `custom-title`（claude の `/rename` と `-n`） |
//! | 2 | transcript の `ai-title` |
//! | 3 | transcript の `last-prompt` |
//! | 4 | どれも拾えない ＝ [`UNTITLED`] |
//!
//! **transcript は非公開の内部形式**（型名・パスの導出は
//! [`crate::claude_format`]）。形が変われば 1〜3 が拾えなくなるが、そのときは
//! [`UNTITLED`] へ落ちるだけで機能は落ちない。パースは行単位で捨てるので
//! 壊れた JSON でも panic しない。

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::claude_format::{
    project_dir_name, projects_dir, transcript_file_name, AI_TITLE, CUSTOM_TITLE, LAST_PROMPT,
};
use crate::git::worktrees_of;
use crate::sessions::{SessionId, SessionRow};

/// 行に出す表示名の桁数。**名前の長さの正本はここ 1 箇所**
pub(crate) const TITLE_LEN: usize = 30;

/// transcript から何も拾えないセッションの表示名。**起こしただけで 1 ターンも
/// 終わっていない行と、材料が本当に無い行が同じ名前になる**のは仕様
pub(crate) const UNTITLED: &str = "new session";

/// 「末尾」と呼ぶ量。**[`Span::Appended`] の候補にとって十分な幅**で、
/// 手元の 538 本を測ったとき、最後の `ai-title` は EOF から最大 55 KiB・
/// 99% が 33 KiB 以内にあった。
///
/// **これは全候補に効く上限ではない**（[`Span`]）。まれにしか書かれない候補は
/// この外に出るので、初回だけ先頭から読む
const TAIL_BYTES: u64 = 64 * 1024;

/// 候補が transcript の**どこに現れるか**。走査の範囲はこの性質から機械的に決まる
/// （[`first_scan_from`] / [`Titles::refresh`]）ので、候補と範囲の対応表を別に持たない。
///
/// **この区別を落とすと実害が出る**: `custom-title` を末尾 64 KiB だけで探していた
/// 頃は、長い会話の早い段階でリネームした記録が範囲の外に出て拾えず、
/// transcript 全体を読む `/resume` のピッカーと名前が食い違っていた
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Span {
    /// セッション中に繰り返し追記される。最新は必ず末尾側にあるので末尾で足りる
    Appended,
    /// まれにしか書かれない。**ファイルのどこにあるか分からない**ので全体を見る
    Rare,
}

/// transcript の 1 行から拾う表示名の候補（レコードと、現れる場所）。
/// **この配列の順序が優先順そのもの**なので、候補を増やすときに触るのはここだけ
/// （綴りの正本は [`crate::claude_format`]）
const CANDIDATES: [(crate::claude_format::Record, Span); 3] = [
    (CUSTOM_TITLE, Span::Rare),
    (AI_TITLE, Span::Appended),
    (LAST_PROMPT, Span::Appended),
];

/// 走査 1 回で見つかった候補（[`CANDIDATES`] と同じ並び）
type Found = [Option<String>; CANDIDATES.len()];

/// **初回にファイルのどこから読むか。** [`CANDIDATES`] の性質から導く:
/// どこに現れるか分からない候補が 1 つでもあれば先頭から、全部が追記型なら末尾だけ
fn first_scan_from(len: u64) -> u64 {
    if CANDIDATES.iter().any(|(_, span)| *span == Span::Rare) {
        0
    } else {
        len.saturating_sub(TAIL_BYTES)
    }
}

/// 表示名として使える 1 行へ整える。改行・連続空白は 1 つの空白へ畳み、
/// [`TITLE_LEN`] 文字で切る。
///
/// **サイドバーは 1 行**なので、改行や制御文字をそのまま入れると行が崩れる
/// （transcript の値はユーザーが打った文字そのもの）
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
            .any(|((name, _), span)| wanted(span) && line.contains(name))
        {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue; // 壊れた行（書き込みの途中で読んだ場合を含む）
        };
        let Some(kind) = value.get("type").and_then(Value::as_str) else {
            continue;
        };
        for (i, ((name, key), span)) in CANDIDATES.iter().enumerate() {
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
fn pick(found: &Found) -> Option<&String> {
    found.iter().flatten().next()
}

/// すべての候補をすべての範囲で探す（走査を範囲で分けない場合の指定）
const EVERY_SPAN: [Span; 2] = [Span::Rare, Span::Appended];

/// 1 本の transcript について「どこまで読んだか」と「そこまでで見つかった候補」。
///
/// **これはキャッシュであって状態ではない。** 捨てて作り直しても同じ答えになる
/// （transcript は追記しかされないので、全体を読み直せば同じ候補が揃う）。
/// 持っているのは**読み直しを増分で済ませるため**だけ: 一覧は 2 秒ごとに走るので、
/// 毎周すべての行の全体を舐めると、動いていないセッションのファイルまで読み続ける
#[derive(Default)]
struct Scan {
    /// 最後に読んだときのファイルの見え方（長さ・更新時刻）。None ＝ 未走査
    stamp: Option<Stamp>,
    /// 走査済みの末尾位置。**必ず行の切れ目**なので、次はここから読めば
    /// 行の途中から始まらない（書きかけの最終行は次回もう一度読む）
    scanned: u64,
    /// そこまでで見つかった候補
    found: Found,
    /// どのファイルを読んでいたか（解決し直されたら走査もやり直す）
    path: PathBuf,
}

/// transcript が前回から変わったかの判定材料（長さ・更新時刻）
type Stamp = (u64, Option<std::time::SystemTime>);

/// transcript から表示名を導く。**書き込みは一切しない。**
///
/// **パスは注入で受ける**（[`Self::default`] が既定の `~/.claude/projects` を入れる）:
/// 理由は [`crate::sessions`] と同じで、テストが実ユーザーの transcript を
/// 絶対に触らないため。撮影用の供給元は [`Self::fixed`]（ファイルを 1 つも見ない）
pub(crate) struct Titles {
    /// transcript を探す根（`~/.claude/projects`）。None ＝ 何も読まない
    projects: Option<PathBuf>,
    /// 行ごとの走査のキャッシュ（[`Scan`]）
    seen: HashMap<SessionId, Scan>,
    /// 撮影用の固定表。空でなければ transcript より優先する
    /// （`--demo` は実セッションの名前を 1 つも出さない）
    fixed: HashMap<SessionId, String>,
}

impl Default for Titles {
    fn default() -> Self {
        Self {
            projects: projects_dir(),
            seen: HashMap::new(),
            fixed: HashMap::new(),
        }
    }
}

impl Titles {
    /// 撮影用: 固定の表示名だけを返す（transcript も `~/.claude` も読まない）
    pub(crate) fn fixed(names: HashMap<SessionId, String>) -> Self {
        Self {
            projects: None,
            seen: HashMap::new(),
            fixed: names,
        }
    }

    /// **行の表示名。** 純粋な引き当てで、拾えていなければ [`UNTITLED`]。
    ///
    /// 材料を用意するのは [`Self::refresh`]（一覧の読み直しと同じ周期）で、
    /// ここは引き当てるだけ ＝ 描画のたびにファイルを読まない
    pub(crate) fn of(&self, row: &SessionRow) -> String {
        if let Some(name) = self.fixed.get(&row.session_id) {
            return name.clone();
        }
        self.seen
            .get(&row.session_id)
            .and_then(|scan| pick(&scan.found))
            .cloned()
            .unwrap_or_else(|| UNTITLED.to_string())
    }

    /// 行の transcript を解決し、増えたぶんを走査する（一覧の読み直しと同じ周期）。
    ///
    /// **行を書き換えるのは `transcript` だけ**（解決した場所の記録）。表示名も
    /// `updated_at` も触らないので、**名前が変わっても行の経過時間は動かない**。
    ///
    /// 読む範囲の決め方は 2 段:
    ///
    /// - **初回**は [`first_scan_from`]（＝ [`CANDIDATES`] の性質）が決める。
    ///   先頭側ではどこに現れるか分からない候補（[`Span::Rare`]）だけを探し、末尾
    ///   [`TAIL_BYTES`] では全部を探す ＝ 追記型の候補を先頭側でパースしない
    /// - **2 回目以降**は増えたぶんだけを全候補で走査する。transcript は追記しか
    ///   されないので、これで初回と同じ答えが保たれる（`/resume` のピッカーと同じ）
    pub(crate) fn refresh(&mut self, row: &mut SessionRow) {
        // 解決できない ＝ 読む対象が消えた。**拾った値も一緒に落とす**
        // （残すと、消えたファイルから拾った名前が行に出続ける ＝ キャッシュが
        // 状態になってしまう）
        let Some(path) = self.resolve(row).filter(|path| path.is_file()) else {
            self.seen.remove(&row.session_id);
            return;
        };
        let Ok(meta) = std::fs::metadata(&path) else {
            return;
        };
        let stamp = (meta.len(), meta.modified().ok());
        let scan = self.seen.entry(row.session_id.clone()).or_default();
        // 別のファイルへ解決し直された / 縮んだ（追記ではなく作り直された）なら、
        // 覚えた範囲も候補も当てにならない
        if scan.path != path || meta.len() < scan.scanned {
            *scan = Scan {
                path: path.clone(),
                ..Scan::default()
            };
        }
        if scan.stamp == Some(stamp) {
            return;
        }
        let first = scan.stamp.is_none();
        let from = if first {
            first_scan_from(meta.len())
        } else {
            scan.scanned
        };
        let Some(mut text) = read_from(&path, from) else {
            return;
        };
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
            scan_into(&text[..split], &[Span::Rare], &mut scan.found);
            scan_into(&text[split..], &EVERY_SPAN, &mut scan.found);
        } else {
            scan_into(text, &EVERY_SPAN, &mut scan.found);
        }
    }

    /// **`claude -r` を打てる cwd**（transcript が無い ＝ 再開できない行は None）。
    ///
    /// 記録した transcript がどの作業ツリーのものかで決まる:
    /// 行の cwd から導いた置き場所に在るならその cwd、別の作業ツリーの置き場所に
    /// 在るならその作業ツリー。**作業ツリーが消えていれば None**（claude 自身も
    /// その会話を見つけられないので、`claude -r` を打っても
    /// `No conversation found` になる ＝ 新規として起こすのが正しい）
    pub(crate) fn resume_cwd(&self, row: &SessionRow) -> Option<String> {
        let path = row.transcript.as_ref()?;
        if !path.is_file() {
            return None;
        }
        let dir = path.parent()?.file_name()?.to_str()?;
        if project_dir_name(&row.cwd) == dir {
            return Some(row.cwd.clone());
        }
        worktrees_of(&row.cwd)
            .into_iter()
            .map(|tree| tree.display().to_string())
            .find(|tree| project_dir_name(tree) == dir)
    }

    /// 行の transcript の場所。**記録が生きている間は解決し直さない。**
    ///
    /// 解決の手順は claude 本体と同じ:
    ///
    /// 1. 行の cwd のプロジェクトディレクトリ（200 字超は畳んだ派生名）
    /// 2. **行の cwd の git 作業ツリー**のプロジェクトディレクトリ
    ///    （セッションは走行中に worktree へ移れる。[`crate::git`]）
    ///
    /// `~/.claude/projects` の全走査はしない（実機で 67 ディレクトリある）。
    /// 見つからなければ記録も残さない ＝ 1 ターン終わって transcript ができた
    /// 時点で次の周期が拾う
    fn resolve(&self, row: &mut SessionRow) -> Option<PathBuf> {
        if row.transcript.as_ref().is_some_and(|p| p.is_file()) {
            return row.transcript.clone();
        }
        let projects = self.projects.as_ref()?;
        let file = transcript_file_name(row.session_id.as_str());
        let at = |cwd: &str| {
            let path = projects.join(project_dir_name(cwd)).join(&file);
            path.is_file().then_some(path)
        };
        let found = at(&row.cwd).or_else(|| {
            worktrees_of(&row.cwd)
                .into_iter()
                .find_map(|tree| at(&tree.display().to_string()))
        });
        // 見つからなかったときは記録を消す（消えた worktree の記録を残さない）
        row.transcript = found.clone();
        found
    }
}

#[cfg(test)]
impl Titles {
    /// テスト用: transcript の根を差し替える（実ユーザーの transcript を絶対に触らない）
    pub(crate) fn with_projects(projects: PathBuf) -> Self {
        Self {
            projects: Some(projects),
            seen: HashMap::new(),
            fixed: HashMap::new(),
        }
    }

    /// テスト用: その cwd の置き場所へ transcript を作る（**パスの導出は本番と同じ**）
    pub(crate) fn write_transcript_for(&self, row: &SessionRow, cwd: &str, contents: &str) {
        let path = self
            .projects
            .as_ref()
            .expect("no transcript root")
            .join(project_dir_name(cwd))
            .join(transcript_file_name(row.session_id.as_str()));
        std::fs::create_dir_all(path.parent().expect("no parent")).expect("mkdir failed");
        std::fs::write(&path, contents).expect("write failed");
    }

    /// テスト用: 行の cwd の置き場所へ transcript を作る
    pub(crate) fn write_transcript(&self, row: &SessionRow, contents: &str) {
        self.write_transcript_for(row, &row.cwd.clone(), contents);
    }

    /// テスト用: 解決 + 走査 + 引き当てを 1 度に通す（本番は
    /// [`Self::refresh`] が周期で、[`Self::of`] が描画で走る）
    pub(crate) fn title_now(&mut self, row: &mut SessionRow) -> String {
        self.refresh(row);
        self.of(row)
    }
}

/// テスト用: 文字列を丸ごと走査して表示名を選ぶ（本番の [`Titles::refresh`] は
/// 範囲を分けて走るので、範囲の話を抜きにした優先順の検査はこれを使う）
#[cfg(test)]
fn pick_title(text: &str) -> Option<String> {
    let mut found = Found::default();
    scan_into(text, &EVERY_SPAN, &mut found);
    pick(&found).cloned()
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

    /// **実データを写した作業ツリー**（実機の
    /// `<repo>\.git\worktrees\<名前>\gitdir` / `commondir` と
    /// `<repo>\.claude\worktrees\<名前>` をそのまま作る）。
    ///
    /// 実例: 行の cwd が `…\claude-kaizen` なのに transcript は
    /// `projects/C--Users-admin-Documents-Work-claude-kaizen--claude-worktrees-fix-kaizen-window-and-design/`
    /// に在った（セッションが `EnterWorktree` で移った）
    struct TempRepo(PathBuf);

    impl TempRepo {
        fn new(test: &str) -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir()
                .join(format!("ccdesk-repo-{test}-{}-{seq}", std::process::id()));
            std::fs::create_dir_all(root.join(".git")).expect("mkdir failed");
            Self(root)
        }

        fn cwd(&self) -> String {
            self.0.display().to_string()
        }

        fn add_worktree(&self, name: &str) -> String {
            let tree = self.0.join(".claude").join("worktrees").join(name);
            std::fs::create_dir_all(&tree).expect("mkdir failed");
            let admin = self.0.join(".git").join("worktrees").join(name);
            std::fs::create_dir_all(&admin).expect("mkdir failed");
            let git_file = tree.join(".git");
            std::fs::write(
                admin.join("gitdir"),
                format!("{}\n", git_file.display().to_string().replace('\\', "/")),
            )
            .expect("write failed");
            std::fs::write(admin.join("commondir"), "../..\n").expect("write failed");
            std::fs::write(&git_file, format!("gitdir: {}\n", admin.display()))
                .expect("write failed");
            tree.display().to_string()
        }

        /// 作業ツリーを消す（`ExitWorktree` で片付けた後の形）
        fn remove_worktree(&self, name: &str) {
            let _ = std::fs::remove_dir_all(self.0.join(".claude").join("worktrees").join(name));
            let _ = std::fs::remove_dir_all(self.0.join(".git").join("worktrees").join(name));
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// テスト用の行（cwd は transcript のディレクトリ名を決める材料）
    fn row(id: &str) -> SessionRow {
        SessionRow::new(SessionId::new(id), "C:\\dev\\app", 1_000)
    }

    /// **材料が無い行は [`UNTITLED`]**（起こしただけで 1 ターンも終わっていない行と、
    /// `last-prompt` に値が入っていない行が同じ名前になるのは仕様）
    #[test]
    fn a_row_with_nothing_to_read_is_untitled() {
        let temp = TempProjects::new("a_row_with_nothing_to_read_is_untitled");
        let mut titles = temp.titles();
        let mut row = row("33333333-3333-4333-8333-333333333333");
        assert_eq!(titles.title_now(&mut row), UNTITLED, "invented a name out of nothing");
        assert_eq!(row.transcript, None);
        assert_eq!(titles.resume_cwd(&row), None);

        // 実測: `lastPrompt` を持たない `last-prompt` 行だけの transcript
        titles.write_transcript(&row, "{\"type\":\"last-prompt\"}\n");
        assert_eq!(titles.title_now(&mut row), UNTITLED);
        // transcript はあるので再開はできる
        assert_eq!(titles.resume_cwd(&row).as_deref(), Some(row.cwd.as_str()));
    }

    /// **表示名はキャッシュに依存しない。** 増分走査を積み重ねた [`Titles`] と、
    /// まっさらな [`Titles`] が同じ答えを出す（キャッシュが状態でないことの担保）
    #[test]
    fn the_answer_does_not_depend_on_the_cache() {
        let temp = TempProjects::new("the_answer_does_not_depend_on_the_cache");
        let mut warm = temp.titles();
        let mut row = row("11111111-1111-4111-8111-111111111111");
        let custom = line("custom-title", "customTitle", "named early on");
        let prompt = line("last-prompt", "lastPrompt", "a later prompt");
        // 追記を重ねて、増分走査の状態を積む
        titles_write(&warm, &row, &format!("{custom}\n"));
        assert_eq!(warm.title_now(&mut row), "named early on");
        titles_write(&warm, &row, &format!("{custom}\n{prompt}\n"));
        assert_eq!(warm.title_now(&mut row), "named early on");
        let ai = line("ai-title", "aiTitle", "a generated name");
        titles_write(&warm, &row, &format!("{custom}\n{prompt}\n{ai}\n"));
        let warm_answer = warm.title_now(&mut row);

        // キャッシュを丸ごと捨てて解決からやり直す
        let mut cold = temp.titles();
        let mut cold_row = SessionRow {
            transcript: None,
            ..row.clone()
        };
        assert_eq!(cold.title_now(&mut cold_row), warm_answer, "the cache changed the answer");
        assert_eq!(warm_answer, "named early on");
    }

    /// **git worktree へ移った会話も見つける**（実機で起きた形そのまま）。
    /// 記録した場所は行に残るので、次の周期以降は探し直さない
    #[test]
    fn a_transcript_that_moved_into_a_worktree_is_found_through_the_worktree_list() {
        let temp = TempProjects::new("a_transcript_that_moved_into_a_worktree_is_found");
        let repo = TempRepo::new("moved_into_a_worktree");
        let worktree = repo.add_worktree("fix+kaizen-window-and-design");
        let mut titles = temp.titles();
        let mut row = SessionRow {
            cwd: repo.cwd(),
            ..row("84a3d2c8-029c-472d-9180-6e1e2e304242")
        };
        titles.write_transcript_for(
            &row,
            &worktree,
            &format!("{}\n", line("ai-title", "aiTitle", "named in the worktree")),
        );

        assert_eq!(titles.title_now(&mut row), "named in the worktree");
        assert_eq!(
            row.transcript.as_deref().and_then(Path::parent).and_then(Path::file_name),
            Some(std::ffi::OsStr::new(&project_dir_name(&worktree))),
            "did not record where the transcript actually is"
        );
        // 再開は移った先から打つ（行の cwd では claude が会話を見つけられない）
        assert_eq!(titles.resume_cwd(&row).as_deref(), Some(worktree.as_str()));
    }

    /// **記録したパスが消えていたら解決し直す**（worktree の削除・再作成）。
    /// 消えたまま残った記録は再開先にもならない
    #[test]
    fn a_recorded_path_that_disappeared_is_resolved_again() {
        let temp = TempProjects::new("a_recorded_path_that_disappeared_is_resolved_again");
        let repo = TempRepo::new("recorded_path_disappeared");
        let worktree = repo.add_worktree("fix+one");
        let mut titles = temp.titles();
        let mut row = SessionRow {
            cwd: repo.cwd(),
            ..row("22222222-2222-4222-8222-222222222222")
        };
        titles.write_transcript_for(
            &row,
            &worktree,
            &format!("{}\n", line("ai-title", "aiTitle", "in the worktree")),
        );
        assert_eq!(titles.title_now(&mut row), "in the worktree");
        let recorded = row.transcript.clone().expect("nothing was recorded");

        // 作業ツリーごと片付けられ、transcript も消えた
        std::fs::remove_file(&recorded).unwrap();
        repo.remove_worktree("fix+one");
        assert_eq!(titles.title_now(&mut row), UNTITLED, "kept a name from a file that is gone");
        assert_eq!(row.transcript, None, "kept a record that points at nothing");
        assert_eq!(titles.resume_cwd(&row), None);

        // 同じ会話が行の cwd 側に現れたら、そちらへ解決し直す
        titles.write_transcript(&row, &format!("{}\n", line("ai-title", "aiTitle", "back home")));
        assert_eq!(titles.title_now(&mut row), "back home");
        assert_eq!(titles.resume_cwd(&row).as_deref(), Some(row.cwd.as_str()));
    }

    /// **解決は毎周期走らない。** 記録が生きている間はディレクトリを 1 つも
    /// 読まない（`~/.claude/projects` は実機で 67 ディレクトリある）
    #[test]
    fn resolving_does_not_run_on_every_tick() {
        let temp = TempProjects::new("resolving_does_not_run_on_every_tick");
        let repo = TempRepo::new("resolving_does_not_run_on_every_tick");
        let worktree = repo.add_worktree("fix+two");
        let mut titles = temp.titles();
        let mut row = SessionRow {
            cwd: repo.cwd(),
            ..row("44444444-4444-4444-8444-444444444444")
        };
        titles.write_transcript_for(
            &row,
            &worktree,
            &format!("{}\n", line("ai-title", "aiTitle", "resolved once")),
        );
        assert_eq!(titles.title_now(&mut row), "resolved once");

        // 探索の材料（作業ツリーの台帳）を消しても、記録が生きているので影響しない
        repo.remove_worktree("fix+two");
        assert!(row.transcript.as_ref().unwrap().is_file(), "the premise broke");
        titles.refresh(&mut row);
        assert_eq!(titles.of(&row), "resolved once", "resolved again despite a live record");
        assert!(row.transcript.is_some());
    }

    /// **読むだけで書かない。** ccdesk は claude の内部ファイルへ 1 バイトも
    /// 書き込まない（名前の変更はペインの中の `/rename` に一本化してある）
    #[test]
    fn reading_a_transcript_never_writes_to_it() {
        let temp = TempProjects::new("reading_a_transcript_never_writes_to_it");
        let mut titles = temp.titles();
        let mut row = row("99999999-9999-4999-8999-999999999999");
        let body = format!(
            "{}\n{}\n",
            line("last-prompt", "lastPrompt", "the first prompt"),
            line("ai-title", "aiTitle", "a name")
        );
        titles.write_transcript(&row, &body);
        let path = temp
            .0
            .join(project_dir_name(&row.cwd))
            .join(transcript_file_name(row.session_id.as_str()));

        assert_eq!(titles.title_now(&mut row), "a name");
        assert!(titles.resume_cwd(&row).is_some());

        assert_eq!(std::fs::read_to_string(&path).unwrap(), body, "the transcript changed");
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
        assert_eq!(title_text(&"\u{ff21}".repeat(TITLE_LEN + 10)).chars().count(), TITLE_LEN);
        // 切れ目に空白が来ても、詰めた空白で桁が溢れない
        let words = "ab ".repeat(TITLE_LEN);
        let folded = title_text(&words);
        assert!(folded.chars().count() <= TITLE_LEN, "width overflowed: {folded:?}");
        assert!(!folded.ends_with(' '), "trailing whitespace remains: {folded:?}");
    }

    /// **優先順どおりに選ぶ**（上位が居れば下位は見ない）
    #[test]
    fn the_title_follows_the_priority_of_its_sources() {
        let custom = line("custom-title", "customTitle", "hand-written title");
        let ai = line("ai-title", "aiTitle", "ai-written title");
        let prompt = line("last-prompt", "lastPrompt", "last prompt");
        assert_eq!(
            pick_title(&format!("{prompt}\n{ai}\n{custom}\n")).as_deref(),
            Some("hand-written title")
        );
        assert_eq!(
            pick_title(&format!("{prompt}\n{ai}\n")).as_deref(),
            Some("ai-written title")
        );
        assert_eq!(pick_title(&prompt).as_deref(), Some("last prompt"));
        // **順序ではなく優先順で決まる**（上位が先に書かれていても上位が勝つ）
        assert_eq!(
            pick_title(&format!("{custom}\n{ai}\n{prompt}\n")).as_deref(),
            Some("hand-written title")
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
        assert_eq!(pick_title(&tail).as_deref(), Some("new name"));
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
        assert_eq!(pick_title(&tail).as_deref(), Some("ai-written title"));
        // 拾えるものが 1 つも無ければ None（呼び手は [`UNTITLED`] を出す）
        for tail in ["", "not json", r#"{"type":"user","message":{}}"#] {
            assert_eq!(pick_title(tail), None, "built a title out of {tail:?}");
        }
    }

    /// **まれにしか書かれない候補は、末尾 [`TAIL_BYTES`] より前にあっても拾う。**
    ///
    /// これが実機で起きた食い違いの直接の再現: 長い会話の早い段階でリネームすると
    /// `custom-title` は末尾の範囲から出るので、末尾しか読まない実装では
    /// `last-prompt` へ落ちる一方、transcript 全体を読む `/resume` のピッカーは
    /// リネームした名前を出す
    #[test]
    fn a_rename_before_the_tail_window_is_still_found() {
        let temp = TempProjects::new("a_rename_before_the_tail_window_is_still_found");
        let mut titles = temp.titles();
        let mut row = row("55555555-5555-4555-8555-555555555555");
        // 1 行目にリネーム、そのあと末尾の範囲を越える量の会話を積む
        let custom = line("custom-title", "customTitle", "named early on");
        let filler = line("assistant", "text", &"x".repeat(2_000));
        let bulk = std::iter::repeat_n(filler.as_str(), (TAIL_BYTES as usize / 2_000) + 8)
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = line("last-prompt", "lastPrompt", "the latest prompt");
        titles.write_transcript(&row, &format!("{custom}\n{bulk}\n{prompt}\n"));

        assert_eq!(
            titles.title_now(&mut row),
            "named early on",
            "the rename outside the tail window was not found"
        );
        assert!(
            std::fs::metadata(row.transcript.as_ref().unwrap()).unwrap().len() > TAIL_BYTES,
            "the premise broke — the transcript fits in the tail window"
        );
    }

    /// **2 回目以降は増えたぶんだけを読む**（transcript は追記しかされない）。
    /// 初回に拾った候補は覚えているので、**追記に含まれない上位の候補が
    /// 下位へ落ちない**（リネーム済みの行が次の発話で名前を失わない）
    #[test]
    fn later_refreshes_only_read_what_was_appended() {
        let temp = TempProjects::new("later_refreshes_only_read_what_was_appended");
        let mut titles = temp.titles();
        let mut row = row("66666666-6666-4666-8666-666666666667");
        let custom = line("custom-title", "customTitle", "kept name");
        titles.write_transcript(&row, &format!("{custom}\n"));
        assert_eq!(titles.title_now(&mut row), "kept name");

        // 追記（上位の候補は含まれない）。覚えているので上位のまま
        let prompt = line("last-prompt", "lastPrompt", "a later prompt");
        titles.write_transcript(&row, &format!("{custom}\n{prompt}\n"));
        assert_eq!(
            titles.title_now(&mut row),
            "kept name",
            "the remembered custom title was lost when only the appended part was read"
        );

        // 追記に上位の候補が含まれれば、そちらへ更新される（セッション内の `/rename`）
        let renamed = line("custom-title", "customTitle", "renamed later");
        titles.write_transcript(&row, &format!("{custom}\n{prompt}\n{renamed}\n"));
        assert_eq!(titles.title_now(&mut row), "renamed later");
    }

    /// **書きかけの最終行は走査済みにしない**（claude が書いている途中で読むと
    /// 行が途中で切れる）。次の周期で改行まで届いたら、そこで初めて拾える
    #[test]
    fn a_half_written_last_line_is_read_again_next_time() {
        let temp = TempProjects::new("a_half_written_last_line_is_read_again_next_time");
        let mut titles = temp.titles();
        let mut row = row("66666666-6666-4666-8666-666666666666");
        let prompt = line("last-prompt", "lastPrompt", "first prompt");
        let custom = line("custom-title", "customTitle", "arrives in two writes");
        let (head, tail) = custom.split_at(custom.len() / 2);
        titles.write_transcript(&row, &format!("{prompt}\n{head}"));
        assert_eq!(titles.title_now(&mut row), "first prompt");
        // 残りが届いた（行が完結した）
        titles.write_transcript(&row, &format!("{prompt}\n{head}{tail}\n"));
        assert_eq!(titles.title_now(&mut row), "arrives in two writes");
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
        let mut row = row("77777777-7777-4777-8777-777777777777");
        let filler = line("assistant", "text", &"x".repeat(1_000));
        let bulk = std::iter::repeat_n(filler.as_str(), 1_200)
            .collect::<Vec<_>>()
            .join("\n");
        titles.write_transcript(
            &row,
            &format!("{bulk}\n{}\n", line("ai-title", "aiTitle", "big one")),
        );
        let started = std::time::Instant::now();
        assert_eq!(titles.title_now(&mut row), "big one");
        let took = started.elapsed();
        assert!(
            std::fs::metadata(row.transcript.as_ref().unwrap()).unwrap().len() > 1_000_000,
            "the premise broke — the transcript is not large"
        );
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

    /// 撮影用の固定表は transcript を 1 つも読まずに名前を返す
    #[test]
    fn the_fixed_table_answers_without_reading_anything() {
        let staged = row("88888888-8888-4888-8888-888888888888");
        let titles = Titles::fixed(HashMap::from([(
            staged.session_id.clone(),
            "a staged name".to_string(),
        )]));
        assert_eq!(titles.of(&staged), "a staged name");
        // 表に無い行は既定名（撮影でも実データは 1 つも出さない）
        assert_eq!(titles.of(&row("00000000-0000-4000-8000-000000000000")), UNTITLED);
    }

    /// テストの書き味を揃えるための小さな別名（本番の経路は通さない）
    fn titles_write(titles: &Titles, row: &SessionRow, contents: &str) {
        titles.write_transcript(row, contents);
    }
}
