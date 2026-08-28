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

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::backend::{Candidate, Kind, Mark, NameIndex, Span};
use crate::poll::State;
use crate::sessions::{SessionId, SessionRow};

/// 畳んだ名前として**保持する**文字数の上限。
///
/// **表示の切り詰めはここではなく ui が表示幅で行う**（サイドバーの行は幅の
/// 予算で、ペインの見出しは枠で切れる）。ここで短く切ると、サイドバーを
/// 広げても名前がそこで止まる ＝ 「どこで切れたか」の原因が 2 ファイルに割れる。
/// 上限を持つのは、`lastPrompt`（打った文字そのもの）を丸ごと保持しないため
const TITLE_MAX_CHARS: usize = 120;

/// transcript から何も拾えないセッションの表示名。**起こしただけで 1 ターンも
/// 終わっていない行と、材料が本当に無い行が同じ名前になる**のは仕様
pub(crate) const UNTITLED: &str = "new session";

/// 「末尾」と呼ぶ量。**[`Span::Appended`] の候補にとって十分な幅**で、
/// 手元の 538 本を測ったとき、最後の `ai-title` は EOF から最大 55 KiB・
/// 99% が 33 KiB 以内にあった。
///
/// **これは全候補に効く上限ではない**（[`Span`]）。まれにしか書かれない候補は
/// この外に出るので、初回だけ先頭から読む
pub(crate) const TAIL_BYTES: u64 = 64 * 1024;

/// [`Span::Rare`] の候補を探して**末尾から遡る上限**。
///
/// **これが無いと transcript を丸ごと後方走査する。** 実測（実機 802 本 /
/// 492.8 MB、2026-08-02）: `custom-title` を持つのは 77 本だけで、
///
/// | 末尾から | 拾える |
/// |--:|--:|
/// | 64 KiB（[`TAIL_BYTES`]） | 75 / 77 |
/// | 512 KiB | 76 / 77 |
/// | **1 MiB** | **77 / 77** |
///
/// つまり無制限に遡っても 1 MiB を超えて拾える例は 1 本も無い一方、
/// 手元の 6 行では 35.6 MB を舐めていた（起動のたびに UI スレッドで
/// 予算 1 周期ぶんを 9 周期）。**上限を置いても失うものが実測で 0 本**なので置く。
///
/// 代償: 1 MiB より前のリネームは拾えず、下位の候補（`ai-title` /
/// `last-prompt`）へ落ちる。腐ったら測り直す種類の定数
pub(crate) const RARE_BYTES: u64 = 1024 * 1024;

/// 遡る上限は末尾窓より広くなければ意味が無い（同じなら初回の走査で
/// 読み終えており、遡る余地が残らない）
const _: () = assert!(RARE_BYTES > TAIL_BYTES, "the rare window must reach past the tail window");

/// [`Span::Head`] の候補を探す**先頭からの窓**。
///
/// **codex のためにある。** rollout は道具の出力が末尾を埋めるので末尾窓では
/// 最初のプロンプトに届かない（実測 214 本中 128 本 ＝ 60%）が、先頭は
/// `session_meta` → 前置き → 最初のプロンプトと形が決まっている:
///
/// | 先頭から | 拾える |
/// |--:|--:|
/// | 64 KiB | 189 / 214 |
/// | 128 KiB | 212 / 214 |
/// | **256 KiB** | **213 / 214** |
/// | 512 KiB | 214 / 214 |
///
/// 512 KiB で 100% になるが、1 本のために窓を倍にする値段（1 会話あたり
/// 予算の 12.5%）に見合わないので 256 KiB。**先頭は追記されない**ので、
/// 読むのは 1 会話につき 1 回きり
pub(crate) const HEAD_BYTES: u64 = 256 * 1024;

/// 走査 1 回で見つかった候補（agent の [`Candidate`] の並びと同じ長さ）
type Found = Vec<Option<String>>;

/// [`Span::Rare`] の候補にまだ答えが無いか（＝ 遡る走査を続ける理由があるか）
fn rare_missing(found: &Found, records: &[Candidate]) -> bool {
    records
        .iter()
        .enumerate()
        .any(|(i, c)| c.span == Span::Rare && found.get(i).is_none_or(Option::is_none))
}

/// その agent の候補ぶんの空欄
fn empty_found(records: &[Candidate]) -> Found {
    vec![None; records.len()]
}

/// 1 周期（一覧の読み直し 1 回）に読んでよいバイト数。**全行で分け合う。**
///
/// **初回の先頭側スキャン（[`Span::Rare`] ＝ リネーム記録の探索）を数周期に
/// 分けるための予算。** 予算なしで全量を読んでいた頃は、最初の描画の前に
/// 全セッションの transcript（数百 MB になり得る）を UI スレッドで読み切り、
/// 起動が数秒〜数十秒固まった。
///
/// **これは実際の読み取り量の上限。** 末尾窓・追記ぶん・先頭側の読み残しの
/// どれも、読む前に残高で切る（読んでから引くと上限にならない）。
///
/// **配り方は [`Titles::refresh_all`] が持つ**（呼び手ではない）。予算が
/// 有限である以上、足りなくなったときに何を先に捨てるかが表示に出るので、
/// その順序は行をまたいだ判断になる。理由と段の表はあちらの doc
pub(crate) const SCAN_BUDGET: u64 = 4 * 1024 * 1024;

/// **末尾窓 1 つぶんは必ず 1 周期の予算に収まる。** 割ると走査が 2 つとも止まる:
/// [`Titles::first_scan`] はどの周期でも窓を読めず名前が永久に出ないし、
/// [`Titles::append_scan`] は「行が長い」と「予算が細い」を区別できる大きさに
/// 届かず追記を 1 バイトも進められない
const _: () = assert!(
    SCAN_BUDGET >= TAIL_BYTES,
    "a tail window must fit in one cycle's budget"
);

/// 表示名として使える 1 行へ整える。改行・連続空白は 1 つの空白へ畳み、
/// [`TITLE_MAX_CHARS`] 文字で切る。
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
        if len >= TITLE_MAX_CHARS {
            break;
        }
        if gap {
            out.push(' ');
            len += 1;
            gap = false;
            if len >= TITLE_MAX_CHARS {
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

/// ファイルの `from` バイト目から `len` バイトだけを**生バイトで**読む
/// （足りなければ None ＝ 縮んだ・読めない）。
///
/// **文字列にして返さない。** 読んだ範囲は必ず文字の途中から始まる（塊の境界は
/// バイト位置で決まる）ので、`String::from_utf8_lossy` を通すと壊れたバイトが
/// U+FFFD（3 バイト）に膨らみ、**文字列上の位置が生ファイルのバイト位置と
/// ずれる**。そのずれを `scanned` や `head_pending` に足していたのが実害で、
/// 実データでは末尾窓の境界が多バイト文字を割ったセッションの `scanned` が
/// ファイル長を 4 バイト超え、`meta.len() < scanned` が毎周期成立して
/// **走査が毎回まるごとやり直しになっていた**。
/// 位置は生バイトで数え、lossy 変換は走査するスライスにだけ掛ける
/// （[`scan_bytes`]）
fn read_range(path: &Path, from: u64, len: u64) -> Option<Vec<u8>> {
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut buf = vec![0u8; usize::try_from(len).ok()?];
    file.read_exact(&mut buf).ok()?;
    Some(buf)
}

/// 最初の改行の**次**の位置（改行が無ければ末尾）。塊の先頭が行の途中のとき、
/// その半端な行を落とす量
fn after_first_newline(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .position(|&b| b == b'\n')
        .map_or(bytes.len(), |at| at + 1)
}

/// 最後の改行の**次**の位置（改行が無ければ 0）＝ 行が完結している範囲の終わり。
/// 書きかけの最終行はここから外れるので、次に読むときもう一度読める
fn end_of_complete_lines(bytes: &[u8]) -> usize {
    bytes.iter().rposition(|&b| b == b'\n').map_or(0, |at| at + 1)
}

/// その記録ファイルがこの会話のものか。**agent をまたいで同じ判定**で、
/// claude は `<会話 ID>.jsonl`、codex は `rollout-<時刻>-<会話 ID>.jsonl` ＝
/// どちらも**拡張子を除いた名前が会話 ID で終わる**
fn is_for(path: &Path, conversation: &str) -> bool {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem.ends_with(conversation))
}

/// agent 自身が記録の外に持っている会話名の写し（[`NameIndex`]）。
///
/// **ファイルの大きさが変わったときだけ読み直す。** 索引は 1 会話 1 行の追記型
/// （リネームも追記される ＝ 長さは必ず増える）で実測 11 KB 程度だが、一覧の
/// 読み直しは 2 秒ごとなので、変わっていないファイルを舐め続ける理由が無い
#[derive(Default)]
pub(crate) struct ConversationNames {
    names: HashMap<String, String>,
    /// 索引ごとの最後に読んだ大きさ。**agent ごとに別のファイル**
    seen_len: HashMap<PathBuf, u64>,
}

impl ConversationNames {
    /// 索引を読み直す（変わっていなければ何もしない）。
    /// **読めないときは前回の表を保つ**（一時的な失敗で名前が消えない）
    fn refresh(&mut self, index: &NameIndex) {
        let Ok(meta) = std::fs::metadata(&index.path) else {
            return;
        };
        if self.seen_len.get(&index.path) == Some(&meta.len()) {
            return;
        }
        let Ok(text) = std::fs::read_to_string(&index.path) else {
            return;
        };
        self.seen_len.insert(index.path.clone(), meta.len());
        // **行単位で捨てる**（1 行壊れても他は読む ＝ 索引は agent が書く外部
        // ファイルで、書き込み途中を読むことがある）。同じ会話が 2 度出てきたら
        // 後の行が勝つ（リネームは追記されるため）
        self.names.extend(text.lines().filter_map(|line| {
            let value: Value = serde_json::from_str(line).ok()?;
            let id = value.get(index.id_key)?.as_str()?;
            let name = value.get(index.name_key)?.as_str()?.trim();
            (!id.is_empty() && !name.is_empty()).then(|| (id.to_string(), title_text(name)))
        }));
    }

    fn get(&self, conversation: &str) -> Option<&str> {
        self.names.get(conversation).map(String::as_str)
    }
}

/// バイト列を走査して候補を拾う。**位置の計算は生バイトで済ませ、ここで初めて
/// 文字列にする**（壊れたバイトは lossy で受ける ＝ 途中から読んでも失敗しない）
fn scan_bytes(bytes: &[u8], records: &Records<'_>, spans: &[Span], into: &mut Picked) {
    scan_into(&String::from_utf8_lossy(bytes), records, spans, into);
}

/// 1 本の記録から拾うもの（agent が何を持つかの宣言）。**2 つを 1 つの走査で
/// 拾うためにある**: 記録は 1 MB を超えることがあり、名前のためにもう一周、
/// 状態のためにもう一周と読むと走査の回数がそのまま起動の重さになる
struct Records<'a> {
    /// 会話に名前を与えうる記録（[`crate::backend::Backend::title_records`]）
    titles: &'a [Candidate],
    /// その会話の現在値を名乗る記録（[`crate::backend::Backend::record_states`]）
    states: &'a [Mark],
    /// 記録の 1 行が書かれた時刻の読み方
    /// （[`crate::backend::Backend::record_time`]）を持つ実装
    backend: &'a dyn crate::backend::Backend,
}

impl Records<'_> {
    fn of(kind: Kind) -> Records<'static> {
        let backend = kind.backend();
        Records {
            titles: backend.title_records(),
            states: backend.record_states(),
            backend,
        }
    }
}

/// 走査 1 回で拾えたもの
#[derive(Default)]
struct Picked {
    /// 候補ごとの表示名（[`Records::titles`] と同じ長さ）
    found: Found,
    /// 最後に見つかった現在値（[`Records::states`]。None ＝ 記録の中に無かった）
    live: Option<(State, u64)>,
    /// **記録が最後に書かれた時刻**（0 ＝ 見ていない）。
    /// 使い道と、なぜ turn の切れ目だけでは足りないのかは
    /// [`crate::backend::Backend::record_time`]
    moved_at: u64,
}

/// 範囲 `text` を走査して候補を拾う。`spans` に載る性質の候補だけを見るので、
/// 「末尾でしか探さない候補」「先頭でしか探さない候補」「遡って探す候補」を
/// 同じ 1 つの走査で表せる。
///
/// **[`Span::Head`] だけは先に見つけた値を守る**（会話の先頭に 1 度だけ書かれ、
/// **最初の値が答え**）。他は後から現れた値が前の値を上書きする（最後が最新）。
/// 壊れた行・知らない形は捨てる。
///
/// **JSON を組む前に印の文字列で弾く**のが速さの要点: 記録は 1 MB を
/// 超えることがあり、全行をパースすると走査 1 回に行数ぶんの時間がかかる
fn scan_into(text: &str, records: &Records<'_>, spans: &[Span], into: &mut Picked) {
    let wanted = |span: Span| spans.contains(&span);
    // **現在値は「末尾へ向かって読む範囲」でしか拾わない。** [`Span::Rare`] だけの
    // 走査（[`Titles::drain_head`]）は末尾から**遡って**読むので、そこで拾うと
    // 古い turn の値が新しい値を上書きする
    let live_here = wanted(Span::Appended);
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let title_here = records
            .titles
            .iter()
            .any(|c| wanted(c.span) && line.contains(c.marker));
        let mark_here =
            live_here && records.states.iter().any(|m| line.contains(m.marker));
        if !title_here && !mark_here {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue; // 壊れた行（書き込みの途中で読んだ場合を含む）
        };
        for (i, candidate) in records.titles.iter().enumerate() {
            if !wanted(candidate.span) {
                continue;
            }
            // 先頭に 1 度だけ書かれる候補は、最初に拾った値が答え
            if candidate.span == Span::Head && into.found[i].is_some() {
                continue;
            }
            let text = (candidate.text)(&value).map(title_text);
            if let Some(text) = text.filter(|t| !t.is_empty()) {
                into.found[i] = Some(text);
            }
        }
        if mark_here {
            // **後から現れた行が勝つ**（記録は追記なので、後ろほど新しい）
            if let Some(live) = records.states.iter().find_map(|m| (m.read)(&value)) {
                into.live = Some(live);
            }
        }
    }
    // **塊の最後の行の時刻だけを見る**（全行のパースにしない ＝ 記録は 1 MB を
    // 超えることがあり、そこを舐めると走査 1 回が行数に比例する）。
    // 状態を名乗る記録を持たない agent はここも空振りするだけ
    if live_here && !records.states.is_empty() {
        into.moved_at = into.moved_at.max(last_record_time(text, records));
    }
}

/// その範囲の**最後の行**が書かれた時刻（0 ＝ 読めなかった）
fn last_record_time(text: &str, records: &Records<'_>) -> u64 {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|line| line.starts_with('{'))
        .and_then(|line| serde_json::from_str::<Value>(line).ok())
        .and_then(|value| records.backend.record_time(&value))
        .unwrap_or(0)
}

/// 拾った候補から表示名を選ぶ。順は agent の [`Candidate`] の並び（＝ 優先順）で、
/// **上位が 1 つでも見つかれば下位は見ない**
fn pick(found: &Found) -> Option<&String> {
    found.iter().flatten().next()
}

/// 末尾窓で探す性質（[`Span::Head`] は先頭窓が別に読む）
const TAIL_SPANS: [Span; 2] = [Span::Rare, Span::Appended];

/// 1 周期でその行に要る読み取りの種類。**予算が足りないときに何を先に捨てるかは
/// これで決まる**（順序と根拠は [`Titles::refresh_all`] の表）。
///
/// 先頭側の読み残しはここに載せない: あれは行の状態
/// （[`Scan::head_pending`]）から分かるので、[`Titles::plan`] が決める必要が無い
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    /// まだ一度も走査していない ＝ **その行の名前がまだ無い**
    First,
    /// 走査済みの行の追記ぶん ＝ 名前は既に出ていて、新しくなるだけ
    Append,
}

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
    /// 走査済みの末尾位置。**原則として行の切れ目**なので、次はここから読めば
    /// 行の途中から始まらない（書きかけの最終行は次回もう一度読む）。
    ///
    /// **例外は 1 つだけ**: [`TAIL_BYTES`] 読んでも改行が 1 つも無い ＝ 予算では
    /// 追えない長さの 1 行に当たったとき、その行を諦めて塊の分だけ進める
    /// （[`Titles::append_scan`]）。そこだけは行の途中を指し、頭の欠けた行は
    /// JSON として捨てられる
    scanned: u64,
    /// そこまでで拾えたもの（表示名の候補と、記録が名乗った現在値）
    picked: Picked,
    /// どのファイルを読んでいたか（解決し直されたら走査もやり直す）
    path: PathBuf,
    /// 初回に読み残した先頭側の範囲（[`Span::Rare`] ＝ リネーム記録の探索）。
    /// 予算（[`SCAN_BUDGET`]）の範囲で**末尾側からさかのぼって**消化する
    /// （[`Titles::drain_head`]）。None ＝ 読み残し無し
    head_pending: Option<std::ops::Range<u64>>,
}

/// transcript が前回から変わったかの判定材料（長さ・更新時刻）
type Stamp = (u64, Option<std::time::SystemTime>);

/// 会話の記録から表示名を導く。**書き込みは一切しない。**
///
/// **パスは注入で受ける**（[`Self::default`] が各 agent の既定の根を入れる）:
/// 理由は [`crate::sessions`] と同じで、テストが実ユーザーの `~/.claude`
/// `~/.codex` を絶対に触らないため。撮影用の供給元は [`Self::fixed`]
/// （ファイルを 1 つも見ない）。
///
/// **agent ごとの知識は 1 つも持たない。** 記録の場所・候補の並び・索引の在り処は
/// すべて [`crate::backend::Backend`] に聞く（[`crate::backend::Backend::transcript_in`] /
/// [`crate::backend::Backend::title_records`] / [`crate::backend::Backend::name_index`]）ので、agent を足すときに
/// このファイルは触らない
pub(crate) struct Titles {
    /// agent ごとの記録の根。**None ＝ その agent の記録は読まない**
    roots: BTreeMap<Kind, PathBuf>,
    /// **会話**ごとの走査のキャッシュ（[`Scan`]）。
    ///
    /// **鍵が会話 ID であることがこの型の要。** 中身は特定の会話の transcript を
    /// 走った結果なので、行 ID で引くと**行が別の会話へ移った瞬間に嘘になる**
    /// （ペインの中の `/clear` `/resume`、記録を見失った行の起こし直し）。
    /// 行 ID で持っていた頃は `plan` の「パスが変わったらリセット」だけが整合を
    /// 保っており、**`refresh_all` が回るまで前の会話の名前が出続けた**。
    ///
    /// 鍵を会話にすると、会話が変わった時点でそもそも引ける値が無い ＝
    /// 無効化を呼ぶ側の責任が消える（呼び忘れても「古い名前が出る」だけなので
    /// 気づけない種類の責任だった）。
    ///
    /// 代償は「行数で有界でなくなる」こと（`/clear` のたびに会話が増える）なので、
    /// [`Self::refresh_all`] の最後にどの行も指していない会話を落とす
    scans: HashMap<String, Scan>,
    /// 撮影用の固定表。空でなければ記録より優先する
    /// （`--demo` は実セッションの名前を 1 つも出さない）
    fixed: HashMap<SessionId, String>,
    /// agent 自身が記録の外に持っている会話名（[`crate::backend::Backend::name_index`]）
    names: ConversationNames,
}

impl Default for Titles {
    fn default() -> Self {
        Self {
            roots: Kind::ORDER
                .into_iter()
                .filter_map(|kind| Some((kind, kind.backend().transcript_root()?)))
                .collect(),
            scans: HashMap::new(),
            fixed: HashMap::new(),
            names: ConversationNames::default(),
        }
    }
}

impl Titles {
    /// 撮影用: 固定の表示名だけを返す（記録も `~/.claude` `~/.codex` も読まない）
    pub(crate) fn fixed(names: HashMap<SessionId, String>) -> Self {
        Self {
            roots: BTreeMap::new(),
            scans: HashMap::new(),
            fixed: names,
            names: ConversationNames::default(),
        }
    }

    /// **行の表示名。** 純粋な引き当てで、拾えていなければ [`UNTITLED`]。
    ///
    /// 材料を用意するのは [`Self::refresh_all`]（一覧の読み直しと同じ周期）で、
    /// ここは引き当てるだけ ＝ 描画のたびにファイルを読まない
    pub(crate) fn of(&self, row: &SessionRow) -> String {
        if let Some(name) = self.fixed.get(&row.session_id) {
            return name.clone();
        }
        // **名前は会話に付く。** 会話が分からない行は名前も持てない
        // （行 ID から引いていた頃は、会話が変わっても前の名前が出続けた）
        let Some(conversation) = row.conversation.id() else {
            return UNTITLED.to_string();
        };
        // **索引が上、走査が下。** agent 自身が名前を決めているなら（codex の
        // `thread_name`）それが正本で、決めていない会話だけを記録から導く
        // ＝ claude と codex が同じ 2 段を通る（`match Kind` が要らない）
        self.names
            .get(conversation)
            .or_else(|| self.scans.get(conversation).and_then(|s| pick(&s.picked.found)).map(String::as_str))
            .unwrap_or(UNTITLED)
            .to_string()
    }

    /// **全行を 1 周期ぶん読み直す。** 戻り値は「どれかの行の `transcript` の記録を
    /// 書き換えたか」（呼び手が保存の要否に使う）。
    ///
    /// **行を書き換えるのは `transcript` だけ**（解決した場所の記録）。表示名も
    /// `updated_at` も触らない ＝ **名前が変わってもマージの後勝ち判定は動かない**
    /// （[`crate::sessions`] の `merge_sessions`。行の経過時間表示は既に廃止済みで、
    /// 今 `updated_at` が答えるのはこれだけ）。
    ///
    /// # 読む順が「どの行に名前が付くか」を決める
    ///
    /// 予算（[`SCAN_BUDGET`]）は全行で分け合うので、足りなくなったときに
    /// **何を先に捨てるか**が表示に出る。段は 3 つで、上ほど失うものが大きい:
    ///
    /// | 段 | 読むもの | その周期に落ちたとき失うもの |
    /// |:--|:--|:--|
    /// | 1 | まだ走査していない行の末尾窓 | **その行の名前そのもの**（[`UNTITLED`] のまま） |
    /// | 2 | 走査済みの行の追記ぶん | 名前の新しさ（古い名前は出ている） |
    /// | 3 | 先頭側の読み残し | リネーム記録（下位の候補へ落ちるだけ） |
    ///
    /// **段をまたいで順に配るのが要点で、行ごとに 1〜3 を回してはいけない。**
    /// 段 3 はリネーム記録が無ければファイルを舐め切るまで止まらないので、
    /// 行ごとに回すと前の行の段 3 が後ろの行の段 1 を飢えさせる。実データ
    /// 6 行 12.9 MB で、最後の行に名前が付くのが起動の約 4 秒後になっていた。
    ///
    /// **段の順序をここに閉じ込めてあるのは、呼び手に順序を守らせないため。**
    /// 段を公開メソッドに割って呼び手が並べる形にすると、段 3 を呼び忘れても
    /// 名前は出てしまう（下位の候補で埋まる）＝ リネームだけが静かに拾えなくなる
    pub(crate) fn refresh_all(&mut self, rows: &mut [SessionRow], budget: &mut u64) -> bool {
        let mut changed = false;
        // 索引は 1 本を読むだけ（走査の予算とは無関係）。**その agent の行が
        // 1 つも無ければ触らない**
        for kind in Kind::ORDER {
            if rows.iter().any(|row| row.kind == kind)
                && let Some(index) = kind.backend().name_index()
            {
                self.names.refresh(&index);
            }
        }
        // 解決と stat は行あたりここで 1 回だけ（段ごとに回すと段の数だけ増える）
        let mut plans = Vec::with_capacity(rows.len());
        for row in rows.iter_mut() {
            let (plan, moved) = self.plan(row);
            changed |= moved;
            plans.push(plan);
        }
        for stage in [Stage::First, Stage::Append] {
            let planned_here =
                |plan: &&Option<(Stage, Stamp)>| matches!(plan, Some((s, _)) if *s == stage);
            let mut left = plans.iter().filter(planned_here).count() as u64;
            for (row, plan) in rows.iter().zip(&plans) {
                let Some((planned, stamp)) = *plan else {
                    continue;
                };
                if planned != stage {
                    continue;
                }
                // 走査の鍵は会話（段の計画が立った行は必ず会話を持っている）
                let Some(conversation) = row.conversation.id() else {
                    continue;
                };
                let mut allot = Self::allowance(stage, *budget, left);
                let offered = allot;
                self.run(conversation, row.kind, planned, stamp, &mut allot);
                *budget -= offered - allot;
                left -= 1;
            }
        }
        for row in rows.iter() {
            let records = Records::of(row.kind);
            if let Some(conversation) = row.conversation.id()
                && let Some(scan) = self.scans.get_mut(conversation)
            {
                Self::drain_head(scan, &records, budget);
            }
        }
        // **どの行も指していない会話の走査結果を落とす。** 鍵が行から会話へ移った
        // 以上、ペインの中で `/clear` を繰り返すと会話は増え続ける（行は増えない）
        // ので、放っておくとキャッシュが行数で有界にならない
        let live: std::collections::HashSet<&str> =
            rows.iter().filter_map(|row| row.conversation.id()).collect();
        self.scans.retain(|id, _| live.contains(id.as_str()));
        changed
    }

    /// その行の transcript を解決し、**この周期に要る読み取りを決める**（読まない）。
    /// 戻り値の bool は「行の `transcript` 記録を書き換えたか」
    fn plan(&mut self, row: &mut SessionRow) -> (Option<(Stage, Stamp)>, bool) {
        let (path, resolved_changed) = self.resolve(row);
        // 会話が分からない行は走査する対象が無い（Scan も持っていない）
        let Some(conversation) = row.conversation.id().map(str::to_string) else {
            return (None, resolved_changed);
        };
        // 解決できない・消えた ＝ 読む対象が無い。**拾った値も一緒に落とす**
        // （残すと、消えたファイルから拾った名前が行に出続ける ＝ キャッシュが
        // 状態になってしまう）。ここが唯一の stat（resolve の確認と合わせて
        // 1 周期 2 回。以前は同じファイルを 3 回 stat していた）
        let Some((path, meta)) = path
            .and_then(|path| {
                let meta = std::fs::metadata(&path).ok().filter(|m| m.is_file())?;
                Some((path, meta))
            })
        else {
            self.scans.remove(&conversation);
            return (None, resolved_changed);
        };
        let stamp = (meta.len(), meta.modified().ok());
        let records = Records::of(row.kind);
        let scan = self.scans.entry(conversation).or_default();
        // 別のファイルへ解決し直された / 縮んだ（追記ではなく作り直された）／
        // 候補の数が変わった（版が上がった）なら、覚えた範囲も候補も当てにならない
        if scan.path != path
            || meta.len() < scan.scanned
            || scan.picked.found.len() != records.titles.len()
        {
            *scan = Scan {
                path: path.clone(),
                picked: Picked {
                    found: empty_found(records.titles),
                    ..Picked::default()
                },
                ..Scan::default()
            };
        }
        let stage = match scan.stamp {
            None => Some(Stage::First),
            Some(seen) if seen != stamp => Some(Stage::Append),
            Some(_) => None, // 前回から見え方が変わっていない ＝ 読むものが無い
        };
        (stage.map(|stage| (stage, stamp)), resolved_changed)
    }

    /// **その会話の記録が最後に名乗った現在値**と、それが記録された時刻
    /// （None ＝ 記録が状態を語らない agent、または末尾窓にまだ出ていない）。
    ///
    /// **時刻は記録自身のもの**（ccdesk が読んだ時刻ではない）。読み手
    /// （[`crate::poll::row_state`]）は hook の時刻と比べて新しい方を採るので、
    /// 読んだ時刻で代用するとその値は常に「今」になり、**記録が hook に必ず勝つ**
    /// ＝ 0 遅延の hook を 1 周期遅れの走査が毎回上書きすることになる。
    ///
    /// **かつてここは `grew_since`（伸びたかどうか）だった。** 状態を言わない
    /// 材料から状態を推していたので、「伸びた ＝ 待たれていない」という 1 方向の
    /// 救済にしか使えず、Working の固着は別の材料（PTY の無音）に任せるほか
    /// なかった。記録が状態そのものを持つと分かった今は、両方向がこの 1 つで足りる
    pub(crate) fn live_state(&self, conversation: &str) -> Option<(State, u64)> {
        let picked = &self.scans.get(conversation)?.picked;
        let (state, at) = picked.live?;
        Some(match state {
            // **turn の途中は「最後に書かれた時刻」まで進める。** turn の切れ目の
            // 時刻で止めると、その後に来た `PermissionRequest` の hook に永久に
            // 負ける ＝ 許可に答えても黄「入力待ち」が turn の終わりまで残る
            // （報告された症状）。記録は許可を待っている間だけ伸びが止まるので、
            // 「最後に書かれた時刻」がそのまま「その時刻には動いていた」の証拠
            State::Working => (state, at.max(picked.moved_at)),
            // **終わった turn の時刻は進めない。** 記録は turn の外でも
            // （設定の適用などで）伸びるので、そこまで進めると次の打鍵の
            // hook を追い越して一瞬 Idle に見える
            _ => (state, at),
        })
    }

    /// その段でこの 1 行に許す読み取り量（`left` ＝ その段に残っている行数）。
    ///
    /// **段によって分け方が違うのは、読み取りが分割できるかどうかが違うから**:
    ///
    /// - 段 1（末尾窓）は**分けない**。窓は丸ごと読むか読まないかで、等分すると
    ///   どの行も窓に届かず**全部の行が名前を失う**。先着順で構わないのは、
    ///   読めた行はその周期で走査済みになり次の周期には段 1 から抜けるため
    ///   ＝ 行数ぶんの周期で必ず全部に行き渡る
    /// - 段 2（追記）は**等分する**。分割して読めるので、先頭の行が抱えた
    ///   backlog に周期を丸ごと取られると後ろの行と段 3 が進まない。
    ///   下限を [`TAIL_BYTES`] に置くのは、それより細かく割ると
    ///   [`Self::append_scan`] が行の切れ目に届かず読み直しが空回りするため
    ///
    /// **下限があるぶん、等分しきれる行数には上限がある**（`SCAN_BUDGET /
    /// TAIL_BYTES` ＝ 64 行）。それを超える行が**毎周期ずっと**追記され続けると、
    /// 並びの後ろの行は追記を読めない ＝ 名前は残るが古いまま止まる。
    /// 段 1 と違って自然には解消しない（順序が固定なので同じ行が後ろに居続ける）。
    /// 直すならラウンドロビンの状態を持つことになるので、64 セッションが同時に
    /// 出力し続ける状況が実際に起きてから入れる
    ///
    /// **残高で頭打ちするのもここ**（呼び手と分け合わない ＝ 「この行が読んでよい量」
    /// の答えが 1 箇所に収まる）
    fn allowance(stage: Stage, budget: u64, left: u64) -> u64 {
        match stage {
            Stage::First => budget,
            Stage::Append => budget.div_ceil(left.max(1)).max(TAIL_BYTES).min(budget),
        }
    }

    /// [`Self::plan`] が決めた読み取りを実行する
    fn run(&mut self, conversation: &str, kind: Kind, stage: Stage, stamp: Stamp, budget: &mut u64) {
        let records = Records::of(kind);
        let Some(scan) = self.scans.get_mut(conversation) else {
            return;
        };
        match stage {
            Stage::First => Self::first_scan(scan, &records, stamp, budget),
            Stage::Append => Self::append_scan(scan, &records, stamp, budget),
        }
    }

    /// 初回の走査: 末尾 [`TAIL_BYTES`] を全候補で読み、そこから [`RARE_BYTES`] まで
    /// 遡る範囲を読み残しとして記録する。
    ///
    /// **末尾窓は分割して読まない**（予算が窓ぶんに満たない周期は何もせず次へ回す）。
    /// 先頭側の消化（[`Self::drain_head`]）が探すのは [`Span::Rare`] だけなので、
    /// 窓を半端に読むと追記型の候補（ai-title / last-prompt）が読み残し側へ落ちて
    /// **どの段も拾わない** ＝ その行の名前が永久に出ない。
    /// 窓が予算より大きくなることは無い（[`SCAN_BUDGET`] の直下で固定してある）
    fn first_scan(scan: &mut Scan, records: &Records<'_>, stamp: Stamp, budget: &mut u64) {
        let len = stamp.0;
        // **先頭窓は、先頭にしか現れない候補を持つ agent だけが読む**（codex）。
        // 先頭は追記されないので、読むのは 1 会話につきこの 1 回きり
        if records.titles.iter().any(|c| c.span == Span::Head) {
            let want = HEAD_BYTES.min(len);
            if *budget < want {
                return; // 次の周期へ（窓は必ず丸ごと読む）
            }
            if let Some(bytes) = read_range(&scan.path, 0, want) {
                *budget = budget.saturating_sub(want);
                let complete = end_of_complete_lines(&bytes);
                scan_bytes(&bytes[..complete], records, &[Span::Head], &mut scan.picked);
            }
        }
        let from = len.saturating_sub(TAIL_BYTES);
        let want = len - from;
        if *budget < want {
            return; // 次の周期へ（窓は必ず丸ごと読む）
        }
        let Some(bytes) = read_range(&scan.path, from, want) else {
            return;
        };
        *budget = budget.saturating_sub(want);
        // 行の途中から読んだときだけ、半端な先頭行を落とす（その行は遡る側の
        // 読み残しに含まれるので、取りこぼしにはならない）
        let skip = if from > 0 { after_first_newline(&bytes) } else { 0 };
        let complete = end_of_complete_lines(&bytes[skip..]);
        scan_bytes(&bytes[skip..skip + complete], records, &TAIL_SPANS, &mut scan.picked);
        let start = from + skip as u64;
        scan.stamp = Some(stamp);
        scan.scanned = start + complete as u64;
        // **遡るのは末尾から [`RARE_BYTES`] まで。** ファイルの先頭まで残すと、
        // 22 MB の transcript 1 本で予算 1 周期ぶんを 6 周期使い切る（実機で
        // 6 行 35.6 MB。しかもその 6 行は `custom-title` を 1 つも持っていない）
        let rare_from = len.saturating_sub(RARE_BYTES);
        scan.head_pending = records
            .titles
            .iter()
            .any(|c| c.span == Span::Rare)
            .then(|| (start > rare_from).then_some(rare_from..start))
            .flatten();
    }

    /// 追記ぶんの走査（常に行の切れ目 ＝ `scanned` から始まる）。
    ///
    /// **残り予算で切る。** EOF まで届かなかった周期は `stamp` を据え置くので、
    /// 次の周期が同じ続きから読む ＝ 読み残しが消えない。据え置かずに
    /// EOF まで読み切っていた頃は、予算がいくら残っていようと 1 行の追記が
    /// 数十 MB あればそのぶん丸ごと UI スレッドで読んでいた。
    ///
    /// **細切れには読まない。** 塊が行の切れ目に届かなかったとき、それが
    /// 「予算では追えない長さの 1 行」なのか「残り予算が細かっただけ」なのかは、
    /// **塊の大きさでしか区別できない**。区別が付かない大きさ（[`TAIL_BYTES`] 未満）
    /// なら読まずに次の周期へ回す。区別せずに塊を飛ばしていた頃は、周期の終わりに
    /// 当たった**ごく普通の長さの行**が候補を持っていると二度と拾えなかった
    /// （次の周期は行の途中から読み始め、頭の欠けた行は JSON として捨てられる）
    fn append_scan(scan: &mut Scan, records: &Records<'_>, stamp: Stamp, budget: &mut u64) {
        let delta = stamp.0.saturating_sub(scan.scanned);
        if delta == 0 {
            scan.stamp = Some(stamp); // 中身は増えていない（更新時刻だけ動いた）
            return;
        }
        let take = (*budget).min(delta);
        if take < delta && take < TAIL_BYTES {
            return; // 次の周期へ（予算切れ take == 0 もここに入る）
        }
        let Some(bytes) = read_range(&scan.path, scan.scanned, take) else {
            return;
        };
        *budget = budget.saturating_sub(take);
        // 走査するのは行が完結している範囲まで（書きかけの最終行は次回に回す）
        let complete = end_of_complete_lines(&bytes);
        // 追記ぶんに [`Span::Head`] は現れない（先頭に 1 度だけ書かれる候補）
        scan_bytes(&bytes[..complete], records, &TAIL_SPANS, &mut scan.picked);
        if take < delta {
            // まだ EOF に届いていない。**[`TAIL_BYTES`] 読んで改行が 1 つも無い ＝
            // 予算では追えない長さの 1 行**なので、その行は諦めて先へ進む
            // （進めないと同じ塊を毎周期読み直して永久に止まる。
            // [`Self::drain_head`] と同じ判断）
            scan.scanned += if complete > 0 { complete as u64 } else { take };
        } else {
            scan.scanned += complete as u64;
            scan.stamp = Some(stamp);
        }
    }

    /// 先頭側の読み残し（[`Span::Rare`] の探索）を、予算の範囲で**末尾側から
    /// さかのぼって**消化する。
    ///
    /// Rare の答えは「ファイル中で最後に現れた値」なので、末尾に近い塊から読めば
    /// **最初に見つかった値が答え**になり、見つかった時点で残りは読まずに済む
    /// （末尾スキャンで既に見つかっていれば先頭側は 1 バイトも読まない）
    fn drain_head(scan: &mut Scan, records: &Records<'_>, budget: &mut u64) {
        while let Some(range) = scan.head_pending.clone() {
            if range.is_empty() || !rare_missing(&scan.picked.found, records.titles) {
                scan.head_pending = None;
                return;
            }
            if *budget == 0 {
                return; // 続きは次の周期（予算は呼び手が配る）
            }
            let take = (*budget).min(range.end - range.start);
            let from = range.end - take;
            let Some(bytes) = read_range(&scan.path, from, take) else {
                // **範囲は捨てずに次の周期へ回す。** 一時的に読めなかっただけの
                // ことがある（Windows では別プロセスのロックで普通に起きる）。
                // 捨てると、ファイルが在って見え方も変わらない限りどの段も
                // 読み直さない ＝ リネーム記録が永久に拾えなくなる。
                // ファイルごと消えた場合は行の記録ごと落ちる（[`Self::plan`]）
                return;
            };
            *budget = budget.saturating_sub(take);
            // 塊の先頭が行の途中なら、その行は次（さらに前）の塊が読む
            let skip = if from > range.start {
                after_first_newline(&bytes)
            } else {
                0
            };
            let mut fresh = Picked {
                found: empty_found(records.titles),
                ..Picked::default()
            };
            scan_bytes(&bytes[skip..], records, &[Span::Rare], &mut fresh);
            for (i, candidate) in records.titles.iter().enumerate() {
                // 既にある値（末尾側 ＝ より新しい塊で見つけたもの）は上書きしない
                if candidate.span == Span::Rare && scan.picked.found[i].is_none() {
                    scan.picked.found[i] = fresh.found[i].take();
                }
            }
            let new_end = if skip == bytes.len() && from > range.start {
                // 塊の中に行の切れ目が無い（予算より長い 1 行）。その行の走査は
                // 諦めて先へ進む（リネーム記録は短い行なので実害は無い）
                from
            } else {
                from + skip as u64
            };
            scan.head_pending = (range.start < new_end).then_some(range.start..new_end);
        }
    }

    /// **その会話を再開できる cwd**（見つからない行は None ＝ 新規として起こす）。
    ///
    /// 判断は agent が持つ（[`crate::backend::Backend::resume_cwd`]）。ここが渡すのは行の cwd と
    /// 解決済みの記録の場所だけで、**「その会話を確かめたか」は見ない**
    /// （判断は呼び手 1 箇所 ＝ `crate::app` の `relaunch`）
    pub(crate) fn resume_cwd(&self, row: &SessionRow) -> Option<String> {
        row.kind
            .backend()
            .resume_cwd(&row.cwd, row.transcript.as_deref())
    }

    /// 行の会話の記録の場所。**記録が生きている間は解決し直さない。**
    ///
    /// 探し方そのものは agent が持つ（[`crate::backend::Backend::transcript_in`]）。ここが持つのは
    /// 「いつ探し直すか」と「行に書き戻すか」だけ。
    ///
    /// 戻り値の bool は「行の `transcript` 記録を書き換えたか」（呼び手が
    /// 保存の要否に使う ＝ 変化検出のためだけの clone を呼び手に持たせない）。
    ///
    /// **記録は「今の会話のものか」まで見る。** ファイル名には会話 ID が入るので、
    /// ペインの中で `/clear` を打った行の記録は**そのファイルが在るまま**古い
    /// 会話を指す。存在だけで済ませていた頃は、`/clear` の後も前の会話の名前が
    /// サイドバーに残り続けた（行 ID と会話 ID が同じ値だった間は起こり得なかった）
    fn resolve(&self, row: &mut SessionRow) -> (Option<PathBuf>, bool) {
        // 会話が分からない行は記録も持てない（残っていれば落とす）
        let Some(conversation) = row.conversation.id() else {
            return (None, row.transcript.take().is_some());
        };
        if row
            .transcript
            .as_ref()
            .is_some_and(|p| is_for(p, conversation) && p.is_file())
        {
            return (row.transcript.clone(), false);
        }
        let Some(root) = self.roots.get(&row.kind) else {
            // 根が無い ＝ 何も読まない供給元（撮影用）。記録は触らない
            return (None, false);
        };
        let found = row
            .kind
            .backend()
            .transcript_in(root, conversation, &row.cwd);
        // 見つからなかったときは記録を消す（消えた worktree の記録を残さない）
        let changed = row.transcript != found;
        row.transcript = found.clone();
        (found, changed)
    }
}

#[cfg(test)]
impl Titles {
    /// テスト用: 記録の根を差し替える（実ユーザーの `~/.claude` `~/.codex` を
    /// 絶対に触らない）。**agent ごとに別の根**
    pub(crate) fn with_root(kind: Kind, root: PathBuf) -> Self {
        Self {
            roots: [(kind, root)].into_iter().collect(),
            scans: HashMap::new(),
            fixed: HashMap::new(),
            names: ConversationNames::default(),
        }
    }

    /// テスト用: claude の記録の根を差し替える
    pub(crate) fn with_projects(projects: PathBuf) -> Self {
        Self::with_root(Kind::Claude, projects)
    }

    /// テスト用: その cwd の置き場所へ claude の transcript を作る
    /// （**パスの導出は本番と同じ**）。ファイル名は**会話 ID**（行 ID ではない
    /// ＝ [`Self::resolve`] と同じ材料）
    pub(crate) fn write_transcript_for(&self, row: &SessionRow, cwd: &str, contents: &str) {
        let conversation = row.conversation.id().expect("the row has no conversation");
        let path = self
            .roots
            .get(&row.kind)
            .expect("no transcript root")
            .join(crate::claude_format::project_dir_name(cwd))
            .join(crate::claude_format::transcript_file_name(conversation));
        std::fs::create_dir_all(path.parent().expect("no parent")).expect("mkdir failed");
        std::fs::write(&path, contents).expect("write failed");
    }

    /// テスト用: 行の cwd の置き場所へ transcript を作る
    pub(crate) fn write_transcript(&self, row: &SessionRow, contents: &str) {
        self.write_transcript_for(row, &row.cwd.clone(), contents);
    }

    /// テスト用: 解決 + 走査 + 引き当てを 1 行ぶん通す（本番は
    /// [`Self::refresh_all`] が周期で、[`Self::of`] が描画で走る）。
    /// 予算は無制限 ＝ 1 回で読み切る（予算の配り方そのものを見るテストは
    /// 予算を明示して [`Self::refresh_all`] を呼ぶ）
    pub(crate) fn title_now(&mut self, row: &mut SessionRow) -> String {
        let mut budget = u64::MAX;
        self.refresh_all(std::slice::from_mut(row), &mut budget);
        self.of(row)
    }
}

/// テスト用: 文字列を丸ごと走査して表示名を選ぶ（本番の [`Titles::refresh_all`] は
/// 範囲を分けて走るので、範囲の話を抜きにした優先順の検査はこれを使う）
#[cfg(test)]
fn pick_title(text: &str) -> Option<String> {
    let records = Records::of(Kind::Claude);
    let mut picked = Picked {
        found: empty_found(records.titles),
        ..Picked::default()
    };
    scan_into(text, &records, &TAIL_SPANS, &mut picked);
    pick(&picked.found).cloned()
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
    pub(crate) struct TempProjects(crate::testutil::TempDir);

    impl TempProjects {
        pub(crate) fn new(test: &str) -> Self {
            Self(crate::testutil::TempDir::new("projects", test))
        }

        /// その置き場を見る [`Titles`]
        pub(crate) fn titles(&self) -> Titles {
            Titles::with_projects(self.0.path().to_path_buf())
        }
    }

    /// **実データを写した作業ツリー**（実機の
    /// `<repo>\.git\worktrees\<名前>\gitdir` / `commondir` と
    /// `<repo>\.claude\worktrees\<名前>` をそのまま作る）。
    ///
    /// 実例: 行の cwd が `…\claude-kaizen` なのに transcript は
    /// `projects/C--Users-admin-Documents-Work-claude-kaizen--claude-worktrees-fix-kaizen-window-and-design/`
    /// に在った（セッションが `EnterWorktree` で移った）。
    /// **[`crate::git`] のテストも同じ fixture を使う**（git のレイアウト解釈を
    /// 直すとき、追随すべき fixture が 2 つあると片方だけ古い形のまま通る）
    pub(crate) struct TempRepo(crate::testutil::TempDir);

    impl TempRepo {
        pub(crate) fn new(test: &str) -> Self {
            let dir = crate::testutil::TempDir::new("repo", test);
            std::fs::create_dir_all(dir.join(".git")).expect("mkdir failed");
            Self(dir)
        }

        /// 主ツリーのパス（git 側のテストが一覧と突き合わせる）
        pub(crate) fn root(&self) -> &Path {
            self.0.path()
        }

        fn cwd(&self) -> String {
            self.root().display().to_string()
        }

        pub(crate) fn add_worktree(&self, name: &str) -> String {
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


    /// テスト用の行（cwd は transcript のディレクトリ名を決める材料）。
    ///
    /// **会話 ID を行 ID とわざと別の値にしてある。** transcript を行 ID で
    /// 引いていた実装でも通ってしまうテストにしないため（引いていた頃は、
    /// ペインの中で `/clear` した行に古い名前が残り続けた）
    fn row(id: &str) -> SessionRow {
        SessionRow {
            conversation: crate::sessions::Conversation::Observed(format!("conv-{id}")),
            ..SessionRow::new(SessionId::new(id), "C:\\dev\\app", 1_000)
        }
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

    /// **行が別の会話へ移ったら、前の会話の名前も記録も残らない。**
    ///
    /// ペインの中で `/clear` を打つと行はそのまま会話だけが変わる
    /// （`crate::app` の `adopt_conversations`）。transcript は**会話 ID の
    /// ファイル名**なので、前の会話のファイルはその場に**在り続ける** ＝
    /// 「記録したパスが在るなら解決し直さない」だけでは古い名前が張り付く。
    ///
    /// 行 ID と会話 ID が同じ値だった間はこの形が作れず、
    /// **切り離した瞬間に生きたバグになる**種類の穴だった
    #[test]
    fn moving_the_row_to_another_conversation_drops_the_previous_name_and_record() {
        let temp = TempProjects::new("moving_the_row_to_another_conversation");
        let mut titles = temp.titles();
        let mut row = row("44444444-4444-4444-8444-444444444444");
        titles.write_transcript(&row, &format!("{}\n", line("ai-title", "aiTitle", "before clear")));
        assert_eq!(titles.title_now(&mut row), "before clear");
        let before = row.transcript.clone().expect("the first conversation was not resolved");

        // `/clear`: 同じ行が新しい会話へ移る（前の会話のファイルは残ったまま）
        row.conversation = crate::sessions::Conversation::Observed("conv-after-clear".to_string());
        assert_eq!(
            titles.title_now(&mut row),
            UNTITLED,
            "the name of the conversation from before the /clear stuck to the row"
        );
        assert_ne!(row.transcript.as_ref(), Some(&before), "the row still points at the old record");
        assert!(before.is_file(), "the test premise broke: the old transcript is gone");

        // 新しい会話が 1 ターン終えたら、その名前が出る
        titles.write_transcript(&row, &format!("{}\n", line("ai-title", "aiTitle", "after clear")));
        assert_eq!(titles.title_now(&mut row), "after clear");

        // **`refresh_all` を待たずに古い名前が消える。** ここが `of()` 単体なのが
        // 要点で、`title_now`（＝ refresh_all + of）で見ると「周期が直してくれる」
        // 実装でも通ってしまう。会話を変えた経路（`app` の `open_session` は
        // 周期の外で会話を差し替える）が名前を巻き戻さないことを固定する
        row.conversation = crate::sessions::Conversation::Observed("conv-yet-another".to_string());
        assert_eq!(
            titles.of(&row),
            UNTITLED,
            "the name of the previous conversation survived the switch until the next cycle"
        );

        // 会話そのものが分からなくなったら記録も落とす（消えたファイルから
        // 拾った名前を出し続けない）
        row.conversation = crate::sessions::Conversation::Unknown;
        assert_eq!(titles.title_now(&mut row), UNTITLED);
        assert_eq!(row.transcript, None, "kept a record for a row with no conversation");
    }

    /// **索引に載らない codex の会話も名前を持ち、打つたびに最新へ動く。**
    ///
    /// codex の索引（`session_index.jsonl`）に載るのは `/rename` された会話だけ
    /// （実機で確認: リネームが 1 度も無いとファイルごと存在しない）。残りは
    /// rollout の発話で名乗る ＝ claude が `last-prompt` へ落ちるのと同じ形。
    ///
    /// **最新の発話が答え**（[`crate::backend::codex::TITLE_RECORDS`]）。
    /// 最初の発話だけを採っていた頃は、claude の行が打つたびに動くのに codex の
    /// 行だけ固まったままで、「名前が変わらない」として報告された
    #[test]
    fn a_codex_conversation_that_is_not_in_the_index_is_named_by_its_latest_prompt() {
        let temp = crate::testutil::TempDir::new("title", "codex_latest_prompt");
        let conversation = "019fc236-22c1-7bd3-8fcc-954de8d2ea9a";
        // 実機の形そのまま: session_meta → 前置き（AGENTS.md 等）→ 最初の発話
        let body = concat!(
            r#"{"timestamp":"2026-08-02T11:22:35Z","type":"session_meta","payload":{"id":"019fc236-22c1-7bd3-8fcc-954de8d2ea9a"}}"#, "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"permissions preamble"}]}}"#, "\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"fix the login form","images":[]}}"#, "\n",
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"working on it"}}"#, "\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"now do the signup form","images":[]}}"#, "\n",
        );
        write_rollout(temp.path(), conversation, "2026-08-02T20-22-35", body);

        let mut titles = Titles::with_root(Kind::Codex, temp.path().to_path_buf());
        let mut row = SessionRow {
            kind: Kind::Codex,
            conversation: crate::sessions::Conversation::Observed(conversation.to_string()),
            ..SessionRow::new(SessionId::new("row"), "C:\\dev\\app", 1_000)
        };
        assert_eq!(
            titles.title_now(&mut row),
            "now do the signup form",
            "the name did not follow the latest prompt"
        );
        // 記録の場所は**会話 ID から日ディレクトリを導いて**見つける
        // （ファイル名の時刻部分を組み立て直すとタイムゾーンで腐る）
        assert!(
            row.transcript.as_ref().is_some_and(|p| is_for(p, conversation)),
            "did not record where the rollout is: {:?}",
            row.transcript
        );
        // 会話が変われば名前も落ちる（claude と同じ扱い）
        row.conversation = crate::sessions::Conversation::Observed("019fc299-0000-7000-8000-000000000000".to_string());
        assert_eq!(titles.of(&row), UNTITLED);
    }

    /// **末尾窓に発話が 1 つも無い会話は、先頭の発話で名乗る。**
    ///
    /// codex の rollout は道具の出力が末尾を埋めるので、[`TAIL_BYTES`] では
    /// 発話に届かない会話が実測 60% ある。最新の発話だけを見る形にすると、
    /// その 60% が `new session` へ戻る ＝ 先頭窓の候補は捨てられない
    #[test]
    fn a_codex_conversation_whose_tail_is_all_tool_output_still_uses_its_first_prompt() {
        let temp = crate::testutil::TempDir::new("title", "codex_head_fallback");
        let conversation = "019fc236-22c1-7bd3-8fcc-954de8d2ea9a";
        let noise = line("response_item", "payload", &"x".repeat(1024));
        let mut body = String::new();
        body.push_str(r#"{"type":"event_msg","payload":{"type":"user_message","message":"fix the login form"}}"#);
        body.push('\n');
        // 末尾窓を道具の出力だけで埋める（発話が窓の外へ出る）
        while body.len() < (TAIL_BYTES as usize) * 2 {
            body.push_str(&noise);
            body.push('\n');
        }
        write_rollout(temp.path(), conversation, "2026-08-02T20-22-35", &body);

        let mut titles = Titles::with_root(Kind::Codex, temp.path().to_path_buf());
        let mut row = SessionRow {
            kind: Kind::Codex,
            conversation: crate::sessions::Conversation::Observed(conversation.to_string()),
            ..SessionRow::new(SessionId::new("row"), "C:\\dev\\app", 1_000)
        };
        assert_eq!(
            titles.title_now(&mut row),
            "fix the login form",
            "the head window stopped naming a rollout whose tail is all tool output"
        );
    }

    /// **記録の末尾が現在値になる**（codex）。
    ///
    /// hook はイベントなので取りこぼすと自己修復しないが、rollout は turn の
    /// 切れ目を順に書くので、**次の走査で必ず正しくなる**。実機で固着していたのは
    /// Esc 中断（`Stop` が飛ばない）で、そこは `turn_aborted` が答える。
    ///
    /// **時刻は記録自身のもの**（走査した時刻ではない）。走査時刻で代用すると
    /// 値が常に「今」になり、0 遅延の hook を 1 周期遅れの走査が毎回上書きする
    #[test]
    fn the_tail_of_a_codex_record_is_the_current_state() {
        let temp = crate::testutil::TempDir::new("title", "codex_lifecycle");
        let conversation = "019fc236-22c1-7bd3-8fcc-954de8d2ea9a";
        let started = concat!(
            r#"{"timestamp":"2026-08-02T11:22:35.000Z","type":"session_meta","payload":{"id":"019fc236-22c1-7bd3-8fcc-954de8d2ea9a"}}"#, "
",
            r#"{"timestamp":"2026-08-02T11:22:36.000Z","type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}"#, "
",
        );
        write_rollout(temp.path(), conversation, "2026-08-02T20-22-35", started);

        let mut titles = Titles::with_root(Kind::Codex, temp.path().to_path_buf());
        let mut row = SessionRow {
            kind: Kind::Codex,
            conversation: crate::sessions::Conversation::Observed(conversation.to_string()),
            ..SessionRow::new(SessionId::new("row"), r"C:\dev\app", 1_000)
        };
        titles.title_now(&mut row);
        assert_eq!(
            titles.live_state(conversation),
            Some((State::Working, 1_785_669_756_000)),
            "a started turn is not read as working"
        );

        // **中断も「手が空いた」**（`Stop` は飛ばないので、これが無いと赤が固着する）
        let aborted = format!(
            "{started}{}
",
            r#"{"timestamp":"2026-08-02T11:23:00.000Z","type":"event_msg","payload":{"type":"turn_aborted","turn_id":"t1","reason":"interrupted"}}"#
        );
        write_rollout(temp.path(), conversation, "2026-08-02T20-22-35", &aborted);
        titles.title_now(&mut row);
        assert_eq!(
            titles.live_state(conversation),
            Some((State::Idle, 1_785_669_780_000)),
            "an interrupted turn stayed working"
        );

        // 完了も同じ経路（後から現れた行が勝つ）
        let done = format!(
            "{aborted}{}
",
            r#"{"timestamp":"2026-08-02T11:24:00.000Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"t2","last_agent_message":"done"}}"#
        );
        write_rollout(temp.path(), conversation, "2026-08-02T20-22-35", &done);
        titles.title_now(&mut row);
        assert_eq!(titles.live_state(conversation), Some((State::Idle, 1_785_669_840_000)));
    }

    /// **turn の途中は「記録が最後に書かれた時刻」まで進む。**
    ///
    /// 許可を待つ hook（`PermissionRequest`）は `task_started` より後に来るので、
    /// turn の切れ目の時刻で止めると記録は永久に hook に負ける ＝ 許可に答えても
    /// 黄「入力待ち」が turn の終わりまで残る（報告された症状）。記録は
    /// **許可を待っている間だけ**伸びが止まるので、伸びた時刻がそのまま
    /// 「その時刻には動いていた」の証拠になる
    #[test]
    fn a_turn_in_progress_is_current_as_of_the_last_line_written() {
        let temp = crate::testutil::TempDir::new("title", "codex_turn_moves");
        let conversation = "019fc236-22c1-7bd3-8fcc-954de8d2ea9a";
        let started = concat!(
            r#"{"timestamp":"2026-08-02T11:22:36.000Z","type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}"#, "
",
        );
        let mut titles = Titles::with_root(Kind::Codex, temp.path().to_path_buf());
        let mut row = SessionRow {
            kind: Kind::Codex,
            conversation: crate::sessions::Conversation::Observed(conversation.to_string()),
            ..SessionRow::new(SessionId::new("row"), r"C:\dev\app", 1_000)
        };
        write_rollout(temp.path(), conversation, "2026-08-02T20-22-35", started);
        titles.title_now(&mut row);
        assert_eq!(titles.live_state(conversation), Some((State::Working, 1_785_669_756_000)));

        // 許可に答えた後: 道具の出力が続く ＝ その時刻には動いていた
        let moved = format!(
            "{started}{}
",
            r#"{"timestamp":"2026-08-02T11:25:00.000Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","command":"ls"}}}"#
        );
        write_rollout(temp.path(), conversation, "2026-08-02T20-22-35", &moved);
        titles.title_now(&mut row);
        assert_eq!(
            titles.live_state(conversation),
            Some((State::Working, 1_785_669_900_000)),
            "a turn in progress did not move with the record"
        );

        // **終わった turn の時刻は進めない**（turn の外でも記録は伸びるので、
        // そこまで進めると次の打鍵の hook を追い越して一瞬 Idle に見える）
        let ended = format!(
            "{moved}{}
{}
",
            r#"{"timestamp":"2026-08-02T11:26:00.000Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","last_agent_message":"done"}}"#,
            r#"{"timestamp":"2026-08-02T11:27:00.000Z","type":"event_msg","payload":{"type":"thread_settings_applied","thread_settings":{}}}"#
        );
        write_rollout(temp.path(), conversation, "2026-08-02T20-22-35", &ended);
        titles.title_now(&mut row);
        assert_eq!(
            titles.live_state(conversation),
            Some((State::Idle, 1_785_669_960_000)),
            "a finished turn drifted past its own end"
        );
    }

    /// **記録が状態を語らない agent は None**（claude の現在値は transcript では
    /// なく `~/.claude/sessions/` にある）。ここが値を返すと、同じ現在値を
    /// 2 系統で導くことになる
    #[test]
    fn a_claude_record_never_claims_a_state() {
        let temp = TempProjects::new("claude_has_no_record_state");
        let mut titles = temp.titles();
        let mut row = row("77777777-7777-4777-8777-777777777777");
        let conversation = row.conversation.id().expect("no conversation").to_string();
        titles.write_transcript(&row, &format!("{}
", line("last-prompt", "lastPrompt", "hi")));
        titles.title_now(&mut row);
        assert_eq!(titles.live_state(&conversation), None);
    }

    /// テスト用: codex の rollout を本番と同じ形で置く
    /// （`sessions/YYYY/MM/DD/rollout-<現地時刻>-<会話 ID>.jsonl`）
    fn write_rollout(root: &Path, conversation: &str, stamp: &str, body: &str) {
        let day = crate::backend::codex_index::minted_at_days(conversation).expect("not a uuid v7");
        let dir = root.join(crate::backend::codex_index::day_path(day).expect("no day"));
        std::fs::create_dir_all(&dir).expect("mkdir failed");
        std::fs::write(dir.join(format!("rollout-{stamp}-{conversation}.jsonl")), body)
            .expect("write failed");
    }

    /// **agent 自身が名前を決めているなら、それが走査より上。**
    /// codex が `thread_name` を書いた会話は、rollout を読まずにその名前が出る
    #[test]
    fn a_name_the_agent_recorded_wins_over_the_scan() {
        let temp = crate::testutil::TempDir::new("title", "name_index_wins");
        let path = temp.join("session_index.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"id":"a","thread_name":"named by the agent"}"#, "\n",
                "{not json\n",
                r#"{"thread_name":"no id"}"#, "\n",
                r#"{"id":"c","thread_name":"   "}"#, "\n",
                // リネームは**追記**される（実測: 同じ id が 2 行並ぶ）ので後が勝つ
                r#"{"id":"a","thread_name":"renamed later"}"#, "\n",
            ),
        )
        .expect("write failed");
        let index = NameIndex { path: path.clone(), id_key: "id", name_key: "thread_name" };

        let mut names = ConversationNames::default();
        names.refresh(&index);
        assert_eq!(names.get("a"), Some("renamed later"), "the rename was not picked up");
        assert_eq!(names.get("c"), None, "an empty name was kept");
        assert_eq!(names.get("unknown"), None);

        // **読めないときは前回の表を保つ**（一時的な失敗で名前が消えない）
        std::fs::remove_file(&path).expect("remove failed");
        names.refresh(&index);
        assert_eq!(names.get("a"), Some("renamed later"), "a transient read failure erased the names");
    }

    /// **どの行も指していない会話の走査結果は溜めない。**
    ///
    /// 鍵が行から会話へ移ったので、ペインの中で `/clear` を繰り返すと会話は
    /// 増え続ける（行は増えない）。回収しないとキャッシュが行数で有界にならず、
    /// 走査結果（候補の文字列と読み位置）が起動中ずっと積もる
    #[test]
    fn scans_of_conversations_no_row_points_at_are_reclaimed() {
        let temp = TempProjects::new("scans_of_conversations_no_row_points_at");
        let mut titles = temp.titles();
        let mut row = row("66666666-6666-4666-8666-666666666666");
        titles.write_transcript(&row, &format!("{}\n", line("ai-title", "aiTitle", "first")));
        assert_eq!(titles.title_now(&mut row), "first");
        assert_eq!(titles.scans.len(), 1);

        // `/clear` を 3 回。行は 1 本のままなので、残る Scan も 1 本であるべき
        for i in 0..3 {
            row.conversation = crate::sessions::Conversation::Observed(format!("conv-cleared-{i}"));
            titles.write_transcript(&row, &format!("{}\n", line("ai-title", "aiTitle", "later")));
            titles.title_now(&mut row);
        }
        assert_eq!(
            titles.scans.len(),
            1,
            "the scans of abandoned conversations piled up: {:?}",
            titles.scans.keys().collect::<Vec<_>>()
        );
        assert!(titles.scans.contains_key("conv-cleared-2"), "reclaimed the live conversation");
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
        warm.write_transcript(&row, &format!("{custom}\n"));
        assert_eq!(warm.title_now(&mut row), "named early on");
        warm.write_transcript(&row, &format!("{custom}\n{prompt}\n"));
        assert_eq!(warm.title_now(&mut row), "named early on");
        let ai = line("ai-title", "aiTitle", "a generated name");
        warm.write_transcript(&row, &format!("{custom}\n{prompt}\n{ai}\n"));
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
            Some(std::ffi::OsStr::new(&crate::claude_format::project_dir_name(&worktree))),
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
        titles.title_now(&mut row);
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
            .join(&crate::claude_format::project_dir_name(&row.cwd))
            .join(crate::claude_format::transcript_file_name(row.conversation.id().expect("no conversation")));

        assert_eq!(titles.title_now(&mut row), "a name");
        assert!(titles.resume_cwd(&row).is_some());

        assert_eq!(std::fs::read_to_string(&path).unwrap(), body, "the transcript changed");
    }

    /// 表示名は 1 行に畳んで [`TITLE_MAX_CHARS`] 文字で切る
    /// （改行入りのプロンプトがそのまま行に入るとサイドバーが崩れる）
    #[test]
    fn a_title_is_folded_into_one_line_and_cut_to_the_display_width() {
        assert_eq!(title_text("  fix login\n\tform  validation  "), "fix login form validation");
        assert_eq!(title_text(""), "");
        assert_eq!(title_text(" \n\t "), "");
        // ちょうど / 超過。切るのは文字数（バイト数ではない）
        let exact = "a".repeat(TITLE_MAX_CHARS);
        assert_eq!(title_text(&exact), exact);
        assert_eq!(title_text(&"a".repeat(TITLE_MAX_CHARS + 10)).chars().count(), TITLE_MAX_CHARS);
        // 日本語ではなく全角ラテンを使う（マルチバイト文字であることを検証したいだけで、
        // tests/no_japanese_in_code.rs のチェック対象を避けるため）
        assert_eq!(title_text(&"\u{ff21}".repeat(TITLE_MAX_CHARS + 10)).chars().count(), TITLE_MAX_CHARS);
        // 切れ目に空白が来ても、詰めた空白で桁が溢れない
        let words = "ab ".repeat(TITLE_MAX_CHARS);
        let folded = title_text(&words);
        assert!(folded.chars().count() <= TITLE_MAX_CHARS, "width overflowed: {folded:?}");
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

    /// **遡りは末尾から [`RARE_BYTES`] で打ち切る。**
    ///
    /// 上限が無かった頃は transcript を丸ごと後方走査していた（実機 6 行で
    /// 35.6 MB、しかも探している `custom-title` は 1 つも無かった）。
    /// 実測では 1 MiB を超えて拾える例が 802 本中 0 本なので、失うものは無い。
    ///
    /// **読んだ量まで見る**のが要点で、「名前が下位候補へ落ちる」だけを見ると、
    /// 全部読んだうえで拾えなかった実装でも通ってしまう
    #[test]
    fn the_backward_scan_stops_at_the_rare_window_instead_of_reading_the_whole_file() {
        let temp = TempProjects::new("the_backward_scan_stops_at_the_rare_window");
        let mut titles = temp.titles();
        let mut row = row("77777777-7777-4777-8777-777777777777");
        // 先頭にリネーム、その後ろに RARE_BYTES を超える詰め物、末尾に下位候補
        let filler = format!("{}\n", line("noise", "text", &"x".repeat(2_000)))
            .repeat((RARE_BYTES / 2_000) as usize * 2);
        let body = format!(
            "{}\n{filler}{}\n",
            line("custom-title", "customTitle", "renamed long ago"),
            line("last-prompt", "lastPrompt", "the latest prompt"),
        );
        assert!(
            body.len() as u64 > RARE_BYTES + TAIL_BYTES,
            "the premise broke - the rename is inside the rare window"
        );
        titles.write_transcript(&row, &body);

        // 予算を潤沢にしても、遡りは窓で止まる
        let mut budget = u64::MAX;
        titles.refresh_all(std::slice::from_mut(&mut row), &mut budget);
        for _ in 0..8 {
            let mut budget = u64::MAX;
            titles.refresh_all(std::slice::from_mut(&mut row), &mut budget);
        }
        assert_eq!(
            titles.of(&row),
            "the latest prompt",
            "found a rename that is further back than the rare window"
        );
        let read = u64::MAX - budget;
        assert!(
            read <= RARE_BYTES + TAIL_BYTES,
            "read {read} bytes for a window of {RARE_BYTES}; the whole file is {} bytes",
            body.len()
        );
    }

    /// **初回の先頭側スキャンは予算で数周期に分かれても答えが変わらない。**
    /// 末尾から遠い位置のリネーム記録は、予算が尽きた周期では拾えず、
    /// 続きの周期（同じ Titles への次の refresh）で拾える
    #[test]
    fn the_head_scan_spreads_across_refreshes_within_the_budget() {
        let temp = TempProjects::new("the_head_scan_spreads_across_refreshes");
        let mut titles = temp.titles();
        let mut row = row("55555555-5555-4555-8555-555555555555");
        // 先頭にリネーム記録、その後ろに末尾窓（TAIL_BYTES）を超える詰め物
        let filler = format!("{}\n", line("noise", "text", &"x".repeat(200))).repeat(1_000);
        assert!(filler.len() as u64 > TAIL_BYTES, "the premise broke — filler fits in the tail");
        titles.write_transcript(
            &row,
            &format!("{}\n{filler}", line("custom-title", "customTitle", "named at the top")),
        );

        // 1 周期目: 予算が末尾ぶんしか無い ＝ 先頭のリネームまで届かない
        let mut budget = TAIL_BYTES;
        titles.refresh_all(std::slice::from_mut(&mut row), &mut budget);
        assert_eq!(titles.of(&row), UNTITLED, "found the rename without reading the head");

        // 2 周期目以降: 読み残しが予算の範囲で消化され、答えが揃う
        for _ in 0..64 {
            if titles.of(&row) != UNTITLED {
                break;
            }
            let mut budget = TAIL_BYTES;
            titles.refresh_all(std::slice::from_mut(&mut row), &mut budget);
        }
        assert_eq!(titles.of(&row), "named at the top", "the head scan never finished");
    }

    /// **末尾窓の境界が多バイト文字を割っても、覚える位置が生ファイルとずれない。**
    ///
    /// 位置を lossy 変換後の文字列で数えていた頃は、割れたバイトが U+FFFD
    /// （3 バイト）へ膨らんだぶんだけ `scanned` が実ファイル長を追い越した。
    /// すると次の周期の「縮んだ？」判定（`meta.len() < scanned`）が毎回成立し、
    /// **その行の走査が 2 秒ごとにまるごとやり直しになり続ける**。
    /// 実データでも 6 本中 1 本で起きていた（4 バイト超過）
    #[test]
    fn a_tail_window_that_splits_a_multibyte_character_keeps_its_offsets_honest() {
        let temp = TempProjects::new("a_tail_window_that_splits_a_multibyte_character");
        let mut titles = temp.titles();
        let mut row = row("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
        // 日本語ではなく全角ラテン（3 バイト文字であることだけが要る。
        // tests/no_japanese_in_code.rs の検査対象を避ける）
        let wide = "\u{ff21}".repeat(300);
        // 末尾窓を必ず超える量にする（TAIL_BYTES を動かしても前提が崩れない）
        let filler = format!("{}\n", line("noise", "text", &wide));
        let body = filler.repeat(TAIL_BYTES as usize / filler.len() + 8);
        // 末尾窓の境界が文字の途中に落ちる長さを探す（末尾に詰め物を足して長さを動かす。
        // 先頭に足すと本文ごと同じだけずれて境界の当たる位置が変わらない）
        let head = line("custom-title", "customTitle", "named at the top");
        let split = (0..8).find_map(|pad| {
            let text = format!("{head}\n{body}{}\n", "x".repeat(pad));
            let bytes = text.as_bytes();
            let at = bytes.len() - TAIL_BYTES as usize;
            // UTF-8 の継続バイト（0b10xxxxxx）＝ 文字の途中
            ((bytes[at] & 0xC0) == 0x80).then_some(text)
        });
        let text = split.expect("no padding made the tail window split a character");
        titles.write_transcript(&row, &text);
        let len = text.len() as u64;
        assert!(len > TAIL_BYTES, "the premise broke - the file fits in the tail window");

        let mut budget = SCAN_BUDGET;
        titles.refresh_all(std::slice::from_mut(&mut row), &mut budget);
        assert_eq!(titles.of(&row), "named at the top");
        let scanned = titles.scans[row.conversation.id().expect("no conversation")].scanned;
        assert!(scanned <= len, "remembered {scanned} bytes of a {len} byte file");

        // 見え方が変わっていない 2 周期目は 1 バイトも読まない
        // （やり直しが起きていれば、ここで予算が減る）
        let mut budget = SCAN_BUDGET;
        titles.refresh_all(std::slice::from_mut(&mut row), &mut budget);
        assert_eq!(budget, SCAN_BUDGET, "the scan started over on an unchanged file");
        assert_eq!(titles.of(&row), "named at the top");
    }

    /// **先頭側の読み取りが失敗しても、未走査の範囲は捨てない。**
    ///
    /// 捨てていた頃は、ファイルが在って見え方も変わらない限りどの段も読み直さない
    /// ので、**そのセッションのリネーム記録が二度と拾えなくなった**
    /// （コメントは「次の周期の解決やり直しに任せる」と言っていたが、解決は
    /// 記録が生きている間は走らない）
    #[test]
    fn a_failed_head_read_keeps_the_range_for_the_next_cycle() {
        let temp = TempProjects::new("a_failed_head_read_keeps_the_range");
        // 実在するが範囲に足りないファイル ＝ 読み取りが必ず失敗する
        let path = temp.0.join("short.jsonl");
        std::fs::write(&path, "{}\n").expect("write failed");
        let records = Records::of(Kind::Claude);
        let mut scan = Scan {
            path,
            head_pending: Some(0..10_000),
            picked: Picked {
                found: empty_found(records.titles),
                ..Picked::default()
            },
            ..Scan::default()
        };

        let mut budget = 10_000;
        Titles::drain_head(&mut scan, &records, &mut budget);

        assert_eq!(
            scan.head_pending,
            Some(0..10_000),
            "the unread range was thrown away on a read that may succeed next time"
        );
        assert_eq!(budget, 10_000, "budget was spent on a read that failed");
    }

    /// **1 周期に読む量は予算を超えない。**
    ///
    /// 追記ぶんを EOF まで読み切っていた頃は、予算がいくら残っていようと
    /// 追記の大きさぶん UI スレッドで読んでいた（スリープ復帰や長時間の放置で、
    /// 1 周期の追記が数十 MB になることがある）。
    /// **読み切れなかった続きは失わない**（次の周期が同じ位置から読む）
    #[test]
    fn one_cycle_never_reads_more_than_its_budget() {
        let temp = TempProjects::new("one_cycle_never_reads_more_than_its_budget");
        let mut titles = temp.titles();
        let mut row = row("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb");
        let start = format!("{}\n", line("last-prompt", "lastPrompt", "the first prompt"));
        titles.write_transcript(&row, &start);
        assert_eq!(titles.title_now(&mut row), "the first prompt");

        // 予算よりずっと大きい追記（末尾に答えがある）
        let filler = format!("{}\n", line("noise", "text", &"x".repeat(1_000))).repeat(600);
        let appended = format!("{start}{filler}{}\n", line("ai-title", "aiTitle", "the new name"));
        titles.write_transcript(&row, &appended);
        // 1 周期の予算は末尾窓以上にする（下回ると追記は 1 バイトも進まない。
        // 本番は SCAN_BUDGET >= TAIL_BYTES が const assert で保証する）
        let budget_per_cycle = TAIL_BYTES + 20_000;
        assert!(
            (appended.len() - start.len()) as u64 > budget_per_cycle * 3,
            "the premise broke - the append fits in a few cycles' budget"
        );

        let mut budget = budget_per_cycle;
        titles.refresh_all(std::slice::from_mut(&mut row), &mut budget);
        assert_eq!(
            titles.of(&row),
            "the first prompt",
            "read past the budget and reached the end of the append in one cycle"
        );

        // 続きは次の周期以降で消化され、最後には答えが揃う
        for _ in 0..64 {
            if titles.of(&row) == "the new name" {
                break;
            }
            let mut budget = budget_per_cycle;
            titles.refresh_all(std::slice::from_mut(&mut row), &mut budget);
        }
        assert_eq!(titles.of(&row), "the new name", "the append was never finished");
        assert_eq!(
            titles.scans[row.conversation.id().expect("no conversation")].scanned,
            appended.len() as u64,
            "the append was not read to the end"
        );
    }

    /// **行の切れ目に届かなかった塊を、行が長いと決めつけて捨てない。**
    ///
    /// 読む量は残り予算で切るので、周期の終わりでは**ごく普通の長さの行**でも
    /// 塊に改行が入らない。そこで塊ごと飛ばしていた頃は、その行が候補
    /// （長いプロンプトの `last-prompt` など）だと**二度と拾えなかった**:
    /// 次の周期は行の途中から読み始め、頭の欠けた行は JSON として捨てられる
    #[test]
    fn a_chunk_too_small_to_reach_a_line_break_is_not_thrown_away() {
        let temp = TempProjects::new("a_chunk_too_small_to_reach_a_line_break");
        let mut titles = temp.titles();
        let mut row = row("eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee");
        let start = format!("{}\n", line("last-prompt", "lastPrompt", "the first prompt"));
        titles.write_transcript(&row, &start);
        assert_eq!(titles.title_now(&mut row), "the first prompt");

        // 追記は「候補を持つ 1 行」だけ。普通に読めば拾えるが、行の途中で
        // 切った塊を捨てると頭が欠けて拾えなくなる
        let long_name = "z".repeat(10_000);
        let appended = format!("{start}{}\n", line("ai-title", "aiTitle", &long_name));
        titles.write_transcript(&row, &appended);
        let thin = 3_000u64;
        assert!(
            thin < (appended.len() - start.len()) as u64 && thin < TAIL_BYTES,
            "the premise broke - the thin budget does not cut the line in half"
        );

        // 残り予算が細い周期を挟んでも、答えは変わらない
        let mut budget = thin;
        titles.refresh_all(std::slice::from_mut(&mut row), &mut budget);
        for _ in 0..8 {
            let mut budget = SCAN_BUDGET;
            titles.refresh_all(std::slice::from_mut(&mut row), &mut budget);
        }
        assert_eq!(
            titles.of(&row),
            title_text(&long_name),
            "the appended line was thrown away because a thin cycle could not reach its line break"
        );
    }

    /// **名前がまだ無い行を、名前が古くなるだけの行より先に読む。**
    ///
    /// 予算は全行で分け合うので、読む順が「どの行に名前が付くか」を決める。
    /// 走査済みの行の追記を先に配ると、まだ一度も読んでいない行が
    /// [`UNTITLED`] のまま置き去りになる（失うものの大きさが違う）
    #[test]
    fn a_row_with_no_name_yet_is_read_before_rows_that_only_go_stale() {
        let temp = TempProjects::new("a_row_with_no_name_yet_is_read_first");
        let mut titles = temp.titles();
        let mut busy = row("cccccccc-cccc-4ccc-8ccc-cccccccccccc");
        let start = format!("{}\n", line("ai-title", "aiTitle", "the busy one"));
        titles.write_transcript(&busy, &start);
        assert_eq!(titles.title_now(&mut busy), "the busy one");

        // 走査済みの行に予算を食い切る量の追記
        let filler = format!("{}\n", line("noise", "text", &"x".repeat(1_000))).repeat(300);
        titles.write_transcript(&busy, &format!("{start}{filler}"));
        // まだ一度も走査していない小さな行
        let mut fresh = row("dddddddd-dddd-4ddd-8ddd-dddddddddddd");
        titles.write_transcript(&fresh, &format!("{}\n", line("ai-title", "aiTitle", "the new one")));

        // 追記ぶんには足りず、未走査の行の末尾窓には足りる予算。
        // **並びは busy が先**（順に配ると fresh に届かない）
        let mut budget = 50_000;
        let mut rows = [busy, fresh];
        titles.refresh_all(&mut rows, &mut budget);

        assert_eq!(
            titles.of(&rows[1]),
            "the new one",
            "the append of an already-named row starved the row that had no name at all"
        );
        busy = rows[0].clone();
        fresh = rows[1].clone();
        assert_eq!(titles.of(&busy), "the busy one", "the named row lost its name");
        assert!(titles.of(&fresh) != UNTITLED);
    }

    /// **予算では追えない長さの 1 行は、諦めて先へ進む。**
    ///
    /// 諦めないと同じ塊を毎周期読み直して、その行から先が永久に進まない
    /// （[`Scan::scanned`] が行の途中を指す唯一の場合）。捨てるのはその 1 行だけで、
    /// 後ろの行は普通に拾える
    #[test]
    fn a_line_longer_than_the_tail_window_is_skipped_instead_of_stalling() {
        let temp = TempProjects::new("a_line_longer_than_the_tail_window_is_skipped");
        let mut titles = temp.titles();
        let mut row = row("ffffffff-ffff-4fff-8fff-ffffffffffff");
        let start = format!("{}\n", line("last-prompt", "lastPrompt", "the first prompt"));
        titles.write_transcript(&row, &start);
        assert_eq!(titles.title_now(&mut row), "the first prompt");

        // 末尾窓より長い 1 行（この行は捨てられる）＋ その後ろの普通の行
        let huge = "z".repeat(TAIL_BYTES as usize * 2);
        let appended = format!(
            "{start}{}\n{}\n",
            line("ai-title", "aiTitle", &huge),
            line("ai-title", "aiTitle", "the line after the huge one")
        );
        titles.write_transcript(&row, &appended);

        // 予算を末尾窓ぶんずつ配る ＝ 巨大な行は毎周期「改行なし」に当たる
        for _ in 0..32 {
            if titles.of(&row) == "the line after the huge one" {
                break;
            }
            let mut budget = TAIL_BYTES;
            titles.refresh_all(std::slice::from_mut(&mut row), &mut budget);
        }
        assert_eq!(
            titles.of(&row),
            "the line after the huge one",
            "the scan stalled on a line it could never fit in one cycle"
        );
    }

    /// **backlog を抱えた 1 行が、周期を丸ごと取らない。**
    ///
    /// 段 2（追記）は分割して読めるので、行ごとに上限を配る。先着順のままだと、
    /// スリープ復帰などで先頭の行に数十 MB たまっているあいだ、後ろの行の名前も
    /// 段 3（リネーム記録の探索）も進まなかった
    #[test]
    fn a_row_with_a_backlog_does_not_take_the_whole_cycle() {
        let temp = TempProjects::new("a_row_with_a_backlog_does_not_take_the_whole_cycle");
        let mut titles = temp.titles();
        let mut rows = [
            row("11111111-2222-4333-8444-555555555555"),
            row("66666666-7777-4888-8999-aaaaaaaaaaaa"),
        ];
        let start = |name: &str| format!("{}\n", line("ai-title", "aiTitle", name));
        for (row, name) in rows.iter_mut().zip(["the backlogged one", "the quiet one"]) {
            titles.write_transcript(row, &start(name));
            assert_eq!(titles.title_now(row), name);
        }

        // 先頭の行に予算より大きい backlog、後ろの行には小さな追記
        let filler = format!("{}\n", line("noise", "text", &"x".repeat(1_000))).repeat(300);
        titles.write_transcript(
            &rows[0].clone(),
            &format!("{}{filler}", start("the backlogged one")),
        );
        titles.write_transcript(
            &rows[1].clone(),
            &format!("{}{}\n", start("the quiet one"), line("ai-title", "aiTitle", "caught up")),
        );
        let mut budget = 150_000u64;
        assert!(
            filler.len() as u64 > budget,
            "the premise broke - the backlog fits in one cycle"
        );

        titles.refresh_all(&mut rows, &mut budget);

        assert_eq!(
            titles.of(&rows[1]),
            "caught up",
            "the backlogged row took the whole cycle and the quiet row never got read"
        );
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
}
