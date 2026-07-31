//! `src/**/*.rs` の**非コメント**部分に日本語（ひらがな・カタカナ・漢字）が
//! 残っていないことを検証する。
//!
//! 方針は「コメントは日本語・画面に出る文字列は英語」。TUI の通知は `set_notice`
//! 経由で `error.log` にも残るため、日本語が混じると `ccdesk logs` の出力まで
//! 日本語になる。ここで機械的に止める。
//!
//! 記号（`·` `●○•◦` `⟳` `⚠` `─` `❯` `┄` `▸` `→` `=` `⊞` `○◔◑◕`）は表示に
//! 使うので「非 ASCII 禁止」にはできない。見るのは日本語の 3 用字だけ。
//! （`✽` はかつての Working スピナーの記号で、状態をドットの色と明滅で語る
//! 設計に変えたときに使わなくなったのでこの例からも外した）
//!
//! 許可リストを持たない代わりに、コメント/文字列/raw string/char リテラルを
//! 区別する簡易トークナイザで走査する。許可リストは「例外を書けば通る」ため
//! 黙って腐るが、トークナイザなら判定根拠がコードに閉じる。
//!
//! **表示幅 2 の文字がテストの入力として要るとき**（`src/ui/mod.rs` の
//! アカウント行の幅）は `"\u{5927}"` のようにエスケープで書く。源が ASCII なら
//! ここは通り、意味は隣の日本語コメントが持つ ＝ 例外表を作らずに逃がせる。

use std::path::{Path, PathBuf};

/// 走査中の位置づけ。Rust のブロックコメントは入れ子にできるので深さを持つ。
enum Scan {
    Code,
    LineComment,
    BlockComment(usize),
    /// `"..."` / `b"..."`。`\"` のエスケープを見る
    Str,
    /// `r#"..."#`。エスケープは無く、同数の `#` を伴う `"` だけが閉じる
    RawStr(usize),
    /// `'x'` / `'\n'`。ライフタイム `'a` とは呼び出し側で区別する
    Char,
}

fn is_japanese(c: char) -> bool {
    matches!(c,
        '\u{3040}'..='\u{309f}'   // ひらがな
        | '\u{30a0}'..='\u{30ff}' // カタカナ
        | '\u{31f0}'..='\u{31ff}' // カタカナ拡張
        | '\u{3400}'..='\u{4dbf}' // 漢字（拡張 A）
        | '\u{4e00}'..='\u{9fff}' // 漢字
        | '\u{f900}'..='\u{faff}' // 漢字（互換）
        | '\u{ff66}'..='\u{ff9f}' // 半角カタカナ
    )
}

/// `'` がライフタイム（`'a` / `'static`）かどうか。char リテラルなら `'` は
/// 2〜数文字先で閉じるが、ライフタイムは閉じない。閉じ `'` を先読みして決める。
fn opens_char_literal(rest: &[char]) -> bool {
    // rest[0] == '\'' 前提。`'\''` のようなエスケープを含めて先を見る
    let mut i = 1;
    if rest.get(i) == Some(&'\\') {
        i += 1;
        // `'\u{3042}'` のような長い形もあるので閉じ `'` を素直に探す
        while i < rest.len() && i < 12 {
            if rest[i] == '\'' {
                return true;
            }
            i += 1;
        }
        return false;
    }
    matches!(rest.get(i + 1), Some('\'')) && rest.get(i).is_some()
}

/// 日本語を見つけた**文字位置**を返す。
///
/// **行番号はここで数えない。** 数えると、エスケープや `//` を読み飛ばす
/// `i += 2` が改行をまたいだ瞬間に黙ってずれる（実際にずれていた）。
/// 位置 → 行番号の変換は [`line_of`] が 1 度だけ行い、この関数は
/// 「今どの構文の中か」だけを見る。
fn japanese_positions(chars: &[char]) -> Vec<usize> {
    let mut state = Scan::Code;
    let mut hits = Vec::new();
    let mut i = 0usize;

    while i < chars.len() {
        let c = chars[i];
        match state {
            Scan::LineComment => {
                if c == '\n' {
                    state = Scan::Code;
                }
            }
            Scan::BlockComment(depth) => {
                if c == '/' && chars.get(i + 1) == Some(&'*') {
                    state = Scan::BlockComment(depth + 1);
                    i += 2;
                    continue;
                }
                if c == '*' && chars.get(i + 1) == Some(&'/') {
                    state = if depth <= 1 {
                        Scan::Code
                    } else {
                        Scan::BlockComment(depth - 1)
                    };
                    i += 2;
                    continue;
                }
            }
            Scan::Str => {
                if c == '\\' {
                    i += 2;
                    continue;
                }
                if c == '"' {
                    state = Scan::Code;
                } else if is_japanese(c) {
                    hits.push(i);
                }
            }
            Scan::RawStr(hashes) => {
                if c == '"' && chars[i + 1..].iter().take(hashes).all(|&h| h == '#') {
                    state = Scan::Code;
                    i += 1 + hashes;
                    continue;
                }
                if is_japanese(c) {
                    hits.push(i);
                }
            }
            Scan::Char => {
                if c == '\\' {
                    i += 2;
                    continue;
                }
                if c == '\'' {
                    state = Scan::Code;
                } else if is_japanese(c) {
                    hits.push(i);
                }
            }
            Scan::Code => {
                if c == '/' && chars.get(i + 1) == Some(&'/') {
                    state = Scan::LineComment;
                    i += 2;
                    continue;
                }
                if c == '/' && chars.get(i + 1) == Some(&'*') {
                    state = Scan::BlockComment(1);
                    i += 2;
                    continue;
                }
                if c == 'r' {
                    // `r"..."` / `r#"..."#`。直前が識別子文字なら変数名の一部
                    let prev_is_ident = i
                        .checked_sub(1)
                        .and_then(|p| chars.get(p))
                        .is_some_and(|&p| p.is_alphanumeric() || p == '_');
                    if !prev_is_ident {
                        let mut hashes = 0usize;
                        while chars.get(i + 1 + hashes) == Some(&'#') {
                            hashes += 1;
                        }
                        if chars.get(i + 1 + hashes) == Some(&'"') {
                            state = Scan::RawStr(hashes);
                            i += 2 + hashes;
                            continue;
                        }
                    }
                }
                if c == '"' {
                    state = Scan::Str;
                    i += 1;
                    continue;
                }
                if c == '\'' && opens_char_literal(&chars[i..]) {
                    state = Scan::Char;
                    i += 1;
                    continue;
                }
                if is_japanese(c) {
                    hits.push(i);
                }
            }
        }
        i += 1;
    }

    hits
}

/// 文字位置 → 1 始まりの行番号。行頭の位置表を 1 度作って二分探索する。
fn line_of(line_starts: &[usize], pos: usize) -> usize {
    line_starts.partition_point(|&start| start <= pos)
}

/// 1 ファイルを走査し、日本語を含む非コメント行の (行番号, 行) を返す。
fn scan(src: &str) -> Vec<(usize, String)> {
    let chars: Vec<char> = src.chars().collect();
    let mut line_starts = vec![0usize];
    for (i, c) in chars.iter().enumerate() {
        if *c == '\n' {
            line_starts.push(i + 1);
        }
    }
    let text: Vec<&str> = src.lines().collect();

    let mut lines: Vec<usize> = japanese_positions(&chars)
        .into_iter()
        .map(|pos| line_of(&line_starts, pos))
        .collect();
    lines.dedup();
    lines
        .into_iter()
        .map(|n| (n, text.get(n - 1).unwrap_or(&"").trim().to_string()))
        .collect()
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .map(|e| e.expect("dir entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn src_has_no_japanese_outside_comments() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    assert!(!files.is_empty(), "no .rs file found under src/");

    let mut report = String::new();
    let mut count = 0usize;
    for path in &files {
        let src = std::fs::read_to_string(path).expect("read source");
        for (line, text) in scan(&src) {
            count += 1;
            let rel = path.strip_prefix(root.parent().unwrap()).unwrap_or(path);
            report.push_str(&format!("{}:{line}: {text}\n", rel.display()));
        }
    }
    assert!(
        report.is_empty(),
        "{count} line(s) carry Japanese outside comments. Comments stay Japanese; \
         everything else (strings, identifiers) must be English:\n{report}"
    );
}

// 以下はトークナイザ自身の検証。誤検知（コメントを拾う）と見逃し（文字列を
// 見落とす）の両方を止める。テスト用の日本語はコメントではなく検査対象の
// 「入力データ」なので、エスケープで組み立てる（この関数自身が対象にならない）。
#[test]
fn tokenizer_separates_comments_from_code() {
    let jp = "\u{65e5}\u{672c}\u{8a9e}";
    let quote = '"';

    // コメントは全て見逃す: 行頭 / 行末 / ブロック / 入れ子 / doc
    for src in [
        format!("// {jp}\n"),
        format!("let x = 1; // {jp}\n"),
        format!("/// {jp}\n"),
        format!("//! {jp}\n"),
        format!("/* {jp} */\n"),
        format!("/* outer /* {jp} */ still */\n"),
        format!("let s = {quote}ok{quote}; /* {jp} */\n"),
    ] {
        assert!(
            scan(&src).is_empty(),
            "comment must be ignored, got a hit for: {src}"
        );
    }

    // 文字列は全て拾う: 通常 / エスケープ入り / raw / `//` を含む文字列
    for src in [
        format!("let s = {quote}{jp}{quote};\n"),
        format!("let s = {quote}\\{quote}{jp}\\{quote}{quote};\n"),
        format!("let s = r#{quote}{jp}{quote}#;\n"),
        format!("let s = {quote}http://x{jp}{quote};\n"),
        format!("let c = '{jp}';\n"),
    ] {
        assert_eq!(scan(&src).len(), 1, "string must be caught: {src}");
    }

    // `//` を含む文字列のあとのコメントも、コメントのままでいる
    let mixed = format!("let s = {quote}http://x{quote}; // {jp}\n");
    assert!(
        scan(&mixed).is_empty(),
        "a `//` inside a string must not end the string"
    );

    // ライフタイムは char リテラルの開始ではない。`'a` を開始と誤ると
    // 次の `'` までが全部リテラル扱いになり、その間のコメントを見逃す
    let lifetime = format!("fn f<'a>(x: &'a str) {{}} // {jp}\n");
    assert!(
        scan(&lifetime).is_empty(),
        "a lifetime must not open a char literal"
    );
    let lifetime_then_string = format!("fn f<'a>(x: &'a str) {{ let s = {quote}{jp}{quote}; }}\n");
    assert_eq!(
        scan(&lifetime_then_string).len(),
        1,
        "a string after a lifetime must still be scanned"
    );

    // `'\"'` を char リテラルとして閉じられないと、以降の文字列判定がずれる
    let quote_char = format!("let c = '\\{quote}'; // {jp}\n");
    assert!(
        scan(&quote_char).is_empty(),
        "an escaped quote char literal must not open a string"
    );

    // 識別子の一部の `r` は raw string の始まりではない
    let ident_r = format!("let var = {quote}ok{quote}; // {jp}\n");
    assert!(
        scan(&ident_r).is_empty(),
        "ident `r` must not open a raw string"
    );

    // 表示幅 2 の文字がテストの入力として要るときの逃がし方。エスケープなら
    // 源は ASCII なので通る（意味は隣の日本語コメントが持つ）
    let escaped = format!("let wide = {quote}\\u{{5927}}{quote};\n");
    assert!(
        scan(&escaped).is_empty(),
        "an escaped code point keeps the source ASCII"
    );
}

/// **報告する行番号がずれないこと。**
///
/// 走査は `//` やエスケープを `i += 2` で読み飛ばす。読み飛ばしと行数え上げを
/// 同じループで持つと、読み飛ばしが改行をまたいだ瞬間に行番号が黙ってずれる
/// （src/app.rs で 3 行ずれた実績がある）。行番号は文字位置から後で引くので
/// ずれない ＝ その保証をここで固定する。
#[test]
fn the_reported_line_number_survives_every_skip() {
    let jp = "\u{65e5}\u{672c}\u{8a9e}";
    let quote = '"';

    // 読み飛ばしを通る行（コメント / エスケープ入り文字列 / raw string /
    // ブロックコメント / char リテラル）を前に積んでから、最後の行で当てる
    let src = format!(
        "// {jp}\n\
         let a = {quote}C:\\\\dev\\\\api{quote};\n\
         let b = r#{quote}raw{quote}#;\n\
         /* {jp}\n\
            {jp} */\n\
         let c = '\\n';\n\
         let d = {quote}{jp}{quote};\n"
    );
    assert_eq!(
        scan(&src).iter().map(|(n, _)| *n).collect::<Vec<_>>(),
        vec![7],
        "the hit is on line 7:\n{src}"
    );

    // 改行を含む文字列（raw string）でもずれない
    let multi = format!("let a = r#{quote}one\ntwo{quote}#;\nlet b = {quote}{jp}{quote};\n");
    assert_eq!(
        scan(&multi).iter().map(|(n, _)| *n).collect::<Vec<_>>(),
        vec![3],
        "the hit is on line 3:\n{multi}"
    );

    // 報告に載る本文は、その行そのもの
    let (_, text) = scan(&multi).into_iter().next().expect("one hit");
    assert_eq!(text, format!("let b = {quote}{jp}{quote};"));
}
