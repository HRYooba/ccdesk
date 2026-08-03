//! codex の会話名の読み取り（`~/.codex/session_index.jsonl`）。
//!
//! **codex 側に正本がある。** claude は transcript を舐めて名前を導く必要があるが
//! （[`crate::title`]）、codex は `{"id":…,"thread_name":…,"updated_at":…}` を
//! 1 行 1 会話で持っている。**ここは読むだけ**で、ccdesk は 1 バイトも書かない。
//!
//! 非公開の内部形式なので、形が変われば名前が拾えなくなるだけ（行は
//! [`crate::title::UNTITLED`] で出る）。行単位で捨てるので壊れた JSON でも panic しない。

use std::path::PathBuf;

/// 索引ファイルの項目キー。**読みはここ 1 箇所**（綴りを 2 箇所に持たない）
const ID_KEY: &str = "id";
const NAME_KEY: &str = "thread_name";

/// `~/.codex`。**`CODEX_HOME` を尊重する**（codex 自身と同じ解決順）
pub(crate) fn codex_home() -> Option<PathBuf> {
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

/// 会話名の索引の場所と読み方（[`crate::backend::Backend::name_index`]）
pub(crate) fn name_index() -> Option<crate::backend::NameIndex> {
    Some(crate::backend::NameIndex {
        path: index_path()?,
        id_key: ID_KEY,
        name_key: NAME_KEY,
    })
}

/// 会話 ID（**UUIDv7**）が採番された時刻。epoch からの**日数**で返す。
///
/// **rollout のファイル名から時刻を組み立て直さないためにある。** ファイル名は
/// `rollout-<現地時刻>-<会話 ID>.jsonl` で、時刻部分は書いた環境の
/// タイムゾーンに依存する ＝ 組み立てた名前は環境が変わると黙って外れる。
/// UUIDv7 の先頭 48bit は生成時刻（ms）なので、そこから**日だけ**を導けば、
/// 残る仮定は「その日か前後 1 日のディレクトリに在る」に縮む。
///
/// UUIDv7 でない ID（版が変わった等）は None ＝ 記録が引けなくなるだけ
pub(crate) fn minted_at_days(conversation: &str) -> Option<i64> {
    let hex: String = conversation.chars().filter(|c| *c != '-').take(12).collect();
    if hex.len() < 12 {
        return None;
    }
    let ms = u64::from_str_radix(&hex, 16).ok()?;
    // 版の印（`0` 桁目が 7）まで見ないのは、外れていれば日付が現実離れした値に
    // なり、そのディレクトリが無いだけで済むため
    i64::try_from(ms / 86_400_000).ok()
}

/// epoch からの日数 → `YYYY/MM/DD`（rollout の日ディレクトリ）。
///
/// **暦の計算を自前で持つ**のは、この 1 箇所のためだけに日付の crate を
/// 足す価値が無いため（グレゴリオ暦の民生日付は 1 つの式で出る）
pub(crate) fn day_path(days: i64) -> Option<PathBuf> {
    // Howard Hinnant の civil_from_days（proleptic Gregorian、閏をまとめて扱う）
    let z = days.checked_add(719_468)?;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    Some(PathBuf::from(format!("{y:04}")).join(format!("{m:02}")).join(format!("{d:02}")))
}

/// `$CODEX_HOME/auth.json` の指紋（大きさと更新時刻）。
/// **読めなければ None**（周期フォールバックだけが効く）
pub(crate) fn auth_fingerprint() -> crate::poll::CredentialsFp {
    let meta = std::fs::metadata(codex_home()?.join("auth.json")).ok()?;
    Some((meta.len(), meta.modified().ok()?))
}

/// codex の会話名（会話 ID → 名前）。
///
/// 索引の読み方そのものは agent 固有ではないので
/// [`crate::title::ConversationNames`] が持つ。ここに残るのは**どこを読むか**
/// （[`name_index`]）と、codex に固有のパスの導き方だけ
#[cfg(test)]
mod tests {
    use super::*;

    /// **rollout の日ディレクトリを会話 ID から導く**（ファイル名の時刻部分を
    /// 組み立て直すとタイムゾーンに依存して黙って腐るため）。
    /// 実機の 2 本で、UUIDv7 の時刻がファイル名の日と一致することを確認済み
    #[test]
    fn the_day_directory_comes_from_the_uuid_not_from_the_file_name() {
        // 019fc236-22c1-… の先頭 48bit = 2026-08-02（実機の rollout）
        let day = minted_at_days("019fc236-22c1-7bd3-8fcc-954de8d2ea9a").expect("no timestamp");
        assert_eq!(day_path(day).unwrap(), PathBuf::from("2026").join("08").join("02"));
        // 019e2f2d-20d6-… = 2026-05-16
        let day = minted_at_days("019e2f2d-20d6-7c62-9c2a-8a3f7e5b1d04").expect("no timestamp");
        assert_eq!(day_path(day).unwrap(), PathBuf::from("2026").join("05").join("16"));

        // 暦の境目（閏日・年またぎ）を素の式で出せていること
        assert_eq!(day_path(0).unwrap(), PathBuf::from("1970").join("01").join("01"));
        assert_eq!(day_path(20_513).unwrap(), PathBuf::from("2026").join("03").join("01"));
        assert_eq!(day_path(20_512).unwrap(), PathBuf::from("2026").join("02").join("28"));

        // UUIDv7 でない ID には答えない（記録が引けなくなるだけ）
        assert_eq!(minted_at_days("short"), None);
        assert_eq!(minted_at_days("zzzzzzzz-zzzz-zzzz-zzzz-zzzzzzzzzzzz"), None);
    }

    /// 索引の場所と読み方は 1 箇所（綴りを 2 箇所に持たない）
    #[test]
    fn the_name_index_names_its_own_keys() {
        let index = name_index().expect("no index location");
        assert_eq!((index.id_key, index.name_key), ("id", "thread_name"));
        assert!(index.path.ends_with("session_index.jsonl"), "{:?}", index.path);
    }
}
