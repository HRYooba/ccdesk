//! ccdesk 自身の更新: リリースタグの取得・アセットのダウンロード・SHA-256 検証・
//! 実行ファイルの差し替え。更新の知識はこのモジュールに閉じる:
//! 呼び出し口は `ccdesk update`（CLI）と、サイドバー上部の版行のクリック
//! （[`install`] をバックグラウンドスレッドで呼ぶ）の 2 つ。TUI は起動時に
//! [`newer_tag`] で新しい版を調べ、あればその行に更新マーカーを出す。
//!
//! Windows では**動いている実行ファイルを上書きできない**（`Device or resource
//! busy`）が、**別名へ改名することはできる**（実測）。そのため差し替えは
//! 「新しい exe を `<exe>.new` へ置く → 現行 exe を `<exe>.old` へ退避 →
//! `<exe>.new` を元のパスへ改名」の 3 手で行う。走っているプロセスは現行版のまま
//! 動き続け、新しい版は次回起動から有効になる。
//!
//! 順序には意味がある: 重い処理（別ボリュームからのコピー）を最初の 1 手に閉じ込めると、
//! 残る 2 手は同一ディレクトリ内の rename = メタデータ操作だけになる。実行ファイルが
//! 存在しない窓がバイナリサイズに比例して開くことがなく、宛先に部分的に書かれた
//! ファイルが残る経路も無い。

/// 配布元。ダウンロード URL の基点
const REPO_URL: &str = "https://github.com/HRYooba/ccdesk";

/// 最新リリースタグの取得先（GitHub の公開 API）。
/// 起動時チェック（poll.rs）と `ccdesk update` で共有する
pub(crate) const LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/HRYooba/ccdesk/releases/latest";

/// リリースに載る実行ファイルの名前。**バージョンを含まない固定名**なので
/// タグだけからダウンロード URL を組み立てられる。
/// 生産側の対応箇所: .github/workflows/release.yml の "Upload assets"
const ASSET_NAME: &str = "ccdesk-x86_64-pc-windows-msvc.exe";

/// 差し替えの結果（表示用のパス）
#[derive(Debug)]
pub(crate) struct Installed {
    /// 新しい版を置いたパス（= 元の実行ファイルのパス）
    pub(crate) exe: std::path::PathBuf,
    /// 退避した現行版のパス。走っているプロセスが掴んでいるので今は消せない
    pub(crate) old: std::path::PathBuf,
}

/// 最新リリースタグ（"v0.3.0"）。取得・パースできなければ None。
/// タイムアウトは必須: 応答しないネットワーク（DNS シンクホール等）で
/// 呼び出し元のスレッドをぶら下げない
pub(crate) fn latest_tag() -> Option<String> {
    let out = std::process::Command::new("curl")
        .args([
            "-fsSL",
            "--connect-timeout",
            "3",
            "--max-time",
            "8",
            LATEST_RELEASE_API,
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    serde_json::from_slice::<serde_json::Value>(&out.stdout)
        .ok()?
        .get("tag_name")
        .and_then(|t| t.as_str())
        .filter(|t| is_plausible_tag(t))
        .map(str::to_string)
}

/// タグとして受け入れる形か（ASCII 英数と `._-+` だけ、64 文字以内）。
///
/// 取得したタグはダウンロード URL・`ccdesk update` の標準出力・失敗時の下部バー通知
/// （URL を含む文面）へそのまま流れる。git の ref 名規則が制御文字・空白・`..` を禁じているので
/// 正規のリリースなら必ず通るが、応答が壊れていた場合に端末制御文字や極端に長い
/// 文字列が画面へ出るのをここで止める。[`ccdesk::version_newer`] は各パートの
/// **先頭の数字だけ**を見る寛容なパーサで、後ろにゴミが付いたタグでも「新しい」と
/// 判定を通してしまうため、新旧比較の前段でふるう必要がある
fn is_plausible_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 64
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
}

/// タグ（"v0.3.0"）がこのビルドより新しいか。判定はフッターの更新判定と同じ
/// [`ccdesk::version_newer`]。ローカルビルドがリリースより新しいときに
/// 「更新できます」と誤案内しないためのガードでもある
pub(crate) fn tag_is_newer(tag: &str) -> bool {
    ccdesk::version_newer(tag.trim_start_matches('v'), env!("CARGO_PKG_VERSION"))
}

/// このビルドより新しいリリースタグ。同版・古い・取得失敗なら None
pub(crate) fn newer_tag() -> Option<String> {
    let tag = latest_tag()?;
    tag_is_newer(&tag).then_some(tag)
}

/// 直前の自己更新で退避した `<exe>.old` を消す。更新した当のプロセスが生きている
/// 間はファイルが掴まれていて消せないので、掃除は次にプロセスを起こしたときになる。
/// 呼ぶのは TUI 起動（`main`）と `doctor` の 2 箇所で、`ccdesk update` の出力も
/// その 2 つを案内する。「無い」「まだ掴まれている」はどちらも正常なので失敗は無視する
pub(crate) fn cleanup_old_exe() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(old_exe_path(&exe));
    }
}

/// 指定タグの実行ファイルを取得して現行版と差し替える。
///
/// **SHA-256 の検証を通るまで既存の実行ファイルには一切触らない。** 検証失敗・
/// ダウンロード失敗は一時ファイルを消して Err を返すだけで、インストール済みの
/// 実行ファイルはそのまま残る
pub(crate) fn install(tag: &str) -> anyhow::Result<Installed> {
    let exe = std::env::current_exe()
        .map_err(|e| anyhow::anyhow!("could not resolve the running executable: {e}"))?;
    let (exe_url, sha_url) = asset_urls(tag);
    install_at(&exe, &exe_url, &sha_url)
}

/// 差し替えの本体。取得元 URL と差し替え先を引数で受けるので、`file://` URL と
/// 一時ファイルを渡せばダウンロード・検証・退避・復旧までそのままテストできる
fn install_at(exe: &std::path::Path, exe_url: &str, sha_url: &str) -> anyhow::Result<Installed> {
    // Drop で中身ごと消えるので、以降どの経路で抜けても一時ファイルは残らない
    let temp = TempDir::new()?;
    let new_exe = temp.0.join(ASSET_NAME);
    let sha_path = temp.0.join(format!("{ASSET_NAME}.sha256"));
    download(exe_url, &new_exe)?;
    download(sha_url, &sha_path)?;

    let published = std::fs::read_to_string(&sha_path)
        .ok()
        .as_deref()
        .and_then(parse_sha256_file)
        .ok_or_else(|| anyhow::anyhow!("could not read the published SHA-256 ({sha_url})"))?;
    // 検証と使用は同一ハンドルではない（ここでハッシュを取り、下で move する）。
    // `%TEMP%` は Windows 既定でユーザー毎なので他ユーザーは間に割り込めないが、
    // `TEMP` を共有ディレクトリに向けている環境では未検証バイナリが入りうる
    let actual = certutil_hash(&new_exe)?;
    if !actual.eq_ignore_ascii_case(&published) {
        anyhow::bail!("SHA-256 mismatch (published {published}, downloaded {actual})");
    }

    // 検証済みの中身を、まず差し替え先と**同じディレクトリ**へ置く。一時ディレクトリが
    // 別ドライブのときのコピー（数 MB）はこの時点で終わるので、実行ファイルに触る残りの
    // 2 手は同一ディレクトリ内の rename = メタデータ操作だけになる。「exe が存在しない
    // 窓」がコピー時間ぶん開かず、宛先に中途半端な内容が残る経路も無くなる
    let staged = staged_exe_path(exe);
    let _ = std::fs::remove_file(&staged);
    move_file(&new_exe, &staged).map_err(|e| {
        anyhow::anyhow!("could not stage the new exe at {}: {e}", staged.display())
    })?;

    let old = old_exe_path(exe);
    // 既存の `.old` が走っているプロセスに掴まれているとロックで rename が失敗するので、
    // 掴まれていない残骸は先に落としておく（Windows の rename は既存の宛先を
    // **置き換える**ので、消せなかった場合も結果は変わらない = 掴まれていればどちらも
    // ロックで失敗し、下の park_error がその旨を伝える）
    let _ = std::fs::remove_file(&old);
    if let Err(e) = std::fs::rename(exe, &old) {
        let _ = std::fs::remove_file(&staged);
        return Err(park_error(exe, &old, &e));
    }
    // ここから exe が一瞬存在しない。失敗したら必ず退避した現行版を戻す。
    // 直前の手が rename なので宛先は空であり、部分的に書かれたファイルは残らない
    if let Err(e) = std::fs::rename(&staged, exe) {
        let restored = std::fs::rename(&old, exe);
        let _ = std::fs::remove_file(&staged);
        if let Err(re) = restored {
            // 復旧まで失敗した = 実行ファイルが無い状態で終わる唯一の経路。
            // 手で戻せば直ることを必ず伝える（黙って落ちると復旧手段が伝わらない）
            anyhow::bail!(
                "could not install the new exe ({e}) and could not restore the previous one ({re}); \
                 ccdesk has no executable right now -- rename {} back to {} by hand to recover",
                old.display(),
                exe.display()
            );
        }
        return Err(anyhow::anyhow!("could not install the new exe: {e}"));
    }
    Ok(Installed {
        exe: exe.to_path_buf(),
        old,
    })
}

/// 退避 rename が失敗したときのエラー文面。`PermissionDenied` は原因が 2 つあり、
/// **どちらなのかは残骸の有無で判別できる**ので対処まで伝える:
/// 消せなかった `.old` が残っているなら走っているプロセスがそのイメージを掴んでいる、
/// 残っていないなら実行ファイルのあるディレクトリに書き込み権限が無い
fn park_error(exe: &std::path::Path, old: &std::path::Path, e: &std::io::Error) -> anyhow::Error {
    if e.kind() != std::io::ErrorKind::PermissionDenied {
        return anyhow::anyhow!("could not move the current exe aside: {e}");
    }
    if old.exists() {
        anyhow::anyhow!(
            "could not move the current exe aside: {} is still held by a running ccdesk -- \
             quit it and try again ({e})",
            old.display()
        )
    } else {
        anyhow::anyhow!(
            "could not move the current exe aside: no write access to {} -- \
             run the update from an elevated shell, or install ccdesk somewhere you own ({e})",
            exe.parent().unwrap_or(exe).display()
        )
    }
}

/// タグ → (実行ファイルの URL, その `.sha256` の URL)
fn asset_urls(tag: &str) -> (String, String) {
    let exe = format!("{REPO_URL}/releases/download/{tag}/{ASSET_NAME}");
    let sha = format!("{exe}.sha256");
    (exe, sha)
}

/// 1 ファイルのダウンロード。出力は捨てる（TUI から呼ばれるので画面を汚さない）。
/// タイムアウトは poll.rs と同じ流儀で明示する。実行ファイルは数 MB あるので
/// 全体は 120s まで許す
fn download(url: &str, dest: &std::path::Path) -> anyhow::Result<()> {
    let out = std::process::Command::new("curl")
        .args(["-fsSL", "--connect-timeout", "5", "--max-time", "120", "-o"])
        .arg(dest)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| anyhow::anyhow!("could not run curl: {e}"))?;
    if !out.status.success() {
        anyhow::bail!("download failed ({}): {url}", out.status);
    }
    Ok(())
}

/// `certutil -hashfile <path> SHA256` で SHA-256 を計算する（Windows 標準搭載）。
/// 終了コードは見ない: 失敗すればハッシュ行が出ないので、パーサの判定で足りる
fn certutil_hash(path: &std::path::Path) -> anyhow::Result<String> {
    let out = std::process::Command::new("certutil")
        .arg("-hashfile")
        .arg(path)
        .arg("SHA256")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| anyhow::anyhow!("could not run certutil: {e}"))?;
    parse_certutil_hash(&String::from_utf8_lossy(&out.stdout))
        .ok_or_else(|| anyhow::anyhow!("could not read a SHA-256 hash from certutil output"))
}

/// `certutil -hashfile` の出力からハッシュを取り出す。
///
/// 出力は「見出し行 → ハッシュ → 完了行」の 3 行で、見出しと完了行は**ロケールで
/// 文言が変わる**ため行位置や文言に頼らない。加えて環境によってハッシュが
/// 2 桁ずつ空白区切りで出るので、空白を除いてから 64 桁の 16 進数かで判定する。
/// 比較を取り違えないよう小文字へ正規化して返す
fn parse_certutil_hash(out: &str) -> Option<String> {
    out.lines()
        .map(|line| {
            line.chars()
                .filter(|c| !c.is_whitespace())
                .collect::<String>()
        })
        .find(|s| is_sha256_hex(s))
        .map(|s| s.to_ascii_lowercase())
}

/// `.sha256` ファイルからハッシュを取り出す。
///
/// 受け入れる書式は **「先頭の空白区切りトークンが 64 桁の 16 進数」だけ**で、それ以降は
/// 問わない。`sha256sum` は同じ内容でもモードで区切りが変わる（text は `<hex>␠␠<名前>`、
/// binary は `<hex>␠*<名前>`。Windows の GNU coreutils は **binary が既定**なので
/// 生産側が実際に出すのは後者）ため、区切りやファイル名に依存させない。
/// 生産側は .github/workflows/release.yml の "Upload assets"
fn parse_sha256_file(text: &str) -> Option<String> {
    text.split_whitespace()
        .next()
        .map(str::to_ascii_lowercase)
        .filter(|h| is_sha256_hex(h))
}

/// SHA-256 の 16 進表記か（64 桁の 16 進数）
fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// 実行ファイルの隣に置く作業用パス `<exe><suffix>`。拡張子は**置き換えず末尾に足す**
/// （`ccdesk.exe` + ".old" → `ccdesk.exe.old`。`with_extension` だと `ccdesk.old` に
/// なり、拡張子なしの実行ファイルでは本体そのものを指してしまう）
fn sibling_path(exe: &std::path::Path, suffix: &str) -> std::path::PathBuf {
    let mut path = exe.as_os_str().to_os_string();
    path.push(suffix);
    std::path::PathBuf::from(path)
}

/// 退避先 `<exe>.old`。更新した当のプロセスが掴んでいるので更新直後は消せない
fn old_exe_path(exe: &std::path::Path) -> std::path::PathBuf {
    sibling_path(exe, ".old")
}

/// 差し替え直前の置き場 `<exe>.new`。差し替え先と同じディレクトリなので、ここへ
/// 置いた後はボリューム内の rename だけで差し替えが終わる。
/// 差し替えの途中で kill されたときだけ残るが、次回の更新が置く前に消すので
/// 溜まり続けることはない（起動時の [`cleanup_old_exe`] では消さない:
/// 別シェルで進行中の更新からステージ済みのファイルを奪ってしまうため）
fn staged_exe_path(exe: &std::path::Path) -> std::path::PathBuf {
    sibling_path(exe, ".new")
}

/// ファイルを移す。一時ディレクトリと実行ファイルが別ボリュームだと rename は
/// 失敗するので、そのときはコピーしてから元を消す。
/// 実行ファイル本体には使わない（宛先は必ず `<exe>.new`）ので、コピーが途中で
/// 失敗しても壊れるのはステージ用の一時ファイルだけ
fn move_file(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to)?;
    let _ = std::fs::remove_file(from);
    Ok(())
}

/// スコープを抜けるときに一時ディレクトリを丸ごと消す番人。
/// 検証失敗・パニックを含めどの経路でもダウンロードした中間ファイルを残さない
struct TempDir(std::path::PathBuf);

impl TempDir {
    /// パスはプロセス ID + 呼び出し毎の連番で一意にする。Drop がディレクトリごと
    /// 消すので、2 つの呼び出しが同じパスを共有すると片方の後始末が
    /// もう片方の作業ファイルを消してしまう（プロセス ID だけでは同一プロセス内の
    /// 同時呼び出しを分けられない）
    fn new() -> anyhow::Result<Self> {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ccdesk-update-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&dir)
            .map_err(|e| anyhow::anyhow!("could not create a temp dir: {e}"))?;
        Ok(Self(dir))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 検証に使う架空のハッシュ（64 桁）
    const HASH: &str = "9f2c1b8e4d5a6073f1e2d3c4b5a6978877665544332211ffeeddccbbaa998877";

    /// URL はタグからだけ組み立つ（アセット名にバージョンが入らないことの固定）
    #[test]
    fn builds_download_urls_from_a_tag() {
        let (exe, sha) = asset_urls("v0.3.0");
        assert_eq!(
            exe,
            "https://github.com/HRYooba/ccdesk/releases/download/v0.3.0/ccdesk-x86_64-pc-windows-msvc.exe"
        );
        assert_eq!(sha, format!("{exe}.sha256"));
        // タグが変わってもアセット名は変わらない（固定名が前提）
        let (other, _) = asset_urls("v9.9.9");
        assert!(other.ends_with(ASSET_NAME) && !ASSET_NAME.contains("0.3.0"));
    }

    /// 実際の certutil 出力の形（見出し → ハッシュ → 完了行）
    #[test]
    fn parses_certutil_hash_output() {
        let out = format!(
            "SHA256 hash of C:\\Users\\x\\ccdesk.exe:\r\n{}\r\nCertUtil: -hashfile command completed successfully.\r\n",
            HASH.to_ascii_uppercase()
        );
        assert_eq!(parse_certutil_hash(&out).as_deref(), Some(HASH));
    }

    /// 2 桁ずつ空白区切りで出る環境。行位置・文言・区切りに依存しない
    #[test]
    fn parses_space_separated_certutil_hash() {
        let spaced: Vec<String> = HASH
            .as_bytes()
            .chunks(2)
            .map(|c| String::from_utf8_lossy(c).to_string())
            .collect();
        let out = format!(
            "SHA256 ハッシュ (対象 C:\\x\\ccdesk.exe):\r\n{}\r\nCertUtil: 完了しました。\r\n",
            spaced.join(" ")
        );
        assert_eq!(parse_certutil_hash(&out).as_deref(), Some(HASH));
    }

    /// ハッシュ行が無い出力（実行失敗・別アルゴリズム）は None。
    /// 見出し行や完了行をハッシュと誤読してはいけない
    #[test]
    fn rejects_certutil_output_without_a_hash() {
        for bad in [
            "",
            "CertUtil: -hashfile command FAILED: 0x80070002\r\n",
            "SHA256 hash of C:\\x\\ccdesk.exe:\r\nnot-a-hash\r\n",
            // MD5（32 桁）を SHA-256 と間違えない
            "MD5 hash of file:\r\n0123456789abcdef0123456789abcdef\r\n",
        ] {
            assert_eq!(parse_certutil_hash(bad), None, "input: {bad:?}");
        }
    }

    /// `.sha256` は sha256sum の text モード書式（`<hex>␠␠<ファイル名>`）
    #[test]
    fn parses_sha256_file_in_sha256sum_format() {
        assert_eq!(
            parse_sha256_file(&format!("{HASH}  {ASSET_NAME}\n")).as_deref(),
            Some(HASH)
        );
        // 改行なし・バイナリモードの `*` 印・前後の空行にも耐える
        for text in [
            format!("{HASH}  {ASSET_NAME}"),
            format!("{HASH} *{ASSET_NAME}\r\n"),
            format!("\r\n{HASH}  {ASSET_NAME}\r\n"),
        ] {
            assert_eq!(parse_sha256_file(&text).as_deref(), Some(HASH), "{text:?}");
        }
    }

    /// 壊れた `.sha256` を通してはいけない（検証が形だけになる）
    #[test]
    fn rejects_malformed_sha256_file() {
        for bad in [
            "",
            "\n",
            "Not Found",
            "<html>404</html>",
            "0123456789abcdef  ccdesk.exe",             // 桁不足
            "zzzz1b8e4d5a6073f1e2d3c4b5a6978877665544332211ffeeddccbbaa998877  x", // 非 16 進
        ] {
            assert_eq!(parse_sha256_file(bad), None, "input: {bad:?}");
        }
    }

    /// certutil（大文字）と `.sha256`（小文字）が同じ値なら一致すること。
    /// 両パーサが同じ正規化を通ることの固定
    #[test]
    fn hashes_from_both_sources_compare_equal() {
        let from_certutil =
            parse_certutil_hash(&format!("h:\r\n{}\r\nok\r\n", HASH.to_ascii_uppercase())).unwrap();
        let from_file = parse_sha256_file(&format!("{HASH}  {ASSET_NAME}\n")).unwrap();
        assert!(from_certutil.eq_ignore_ascii_case(&from_file));
    }

    /// 作業用パスは拡張子を置き換えず末尾に足す（`ccdesk.old` にすると
    /// 掃除対象を取り違え、拡張子なしの実行ファイルも壊す）
    #[test]
    fn appends_work_suffixes_without_replacing_the_extension() {
        assert_eq!(
            old_exe_path(std::path::Path::new("C:\\bin\\ccdesk.exe")),
            std::path::PathBuf::from("C:\\bin\\ccdesk.exe.old")
        );
        assert_eq!(
            old_exe_path(std::path::Path::new("C:\\bin\\ccdesk")),
            std::path::PathBuf::from("C:\\bin\\ccdesk.old")
        );
        // ステージ先も同じ流儀。退避先とは別のパスであること（同じだと退避を潰す）
        assert_eq!(
            staged_exe_path(std::path::Path::new("C:\\bin\\ccdesk.exe")),
            std::path::PathBuf::from("C:\\bin\\ccdesk.exe.new")
        );
        assert_ne!(
            staged_exe_path(std::path::Path::new("C:\\bin\\ccdesk.exe")),
            old_exe_path(std::path::Path::new("C:\\bin\\ccdesk.exe"))
        );
    }

    /// 資産名は生産側（ワークフロー）と一字一句一致していないと、以後の
    /// `ccdesk update` が全件 404 になる。リリースを打つまで気付けない類の破損なので
    /// ここで静的に縛る（`include_str!` はコンパイル時に解決されるので、
    /// ワークフローを消した・移した場合もビルドが割れて気付ける）
    #[test]
    fn the_workflow_uploads_the_asset_name_the_updater_downloads() {
        let yml = include_str!("../.github/workflows/release.yml");
        assert!(
            yml.contains(&format!("asset={ASSET_NAME}")),
            "release.yml がアップロードする資産名が ASSET_NAME ({ASSET_NAME}) とずれている"
        );
        // `.sha256` の URL は実行ファイル URL への接尾で組み立てる（asset_urls）ので、
        // 生産側も同じ名前 + ".sha256" で上げていること
        assert!(
            yml.contains("\"$asset.sha256\""),
            "release.yml が <資産名>.sha256 以外の名前でチェックサムを上げている"
        );
    }

    /// タグは URL・画面・stdout の 3 箇所へ流れるので、素朴な版文字列以外は入口で弾く
    #[test]
    fn rejects_tags_that_are_not_plain_version_strings() {
        assert!(is_plausible_tag("v0.3.0"));
        assert!(is_plausible_tag("v1.2.3-rc.1"));
        assert!(is_plausible_tag("0.3.0+build.1"));
        for bad in [
            "",
            "v1.0.0\u{1b}[2J",  // 端末制御文字（version_newer は通してしまう）
            "v1.0.0\nv2.0.0",   // 改行
            "v1.0/../../evil",  // パス区切り
            "v1.0.0 ",          // 空白
            "ｖ１.０.０",       // 非 ASCII
        ] {
            assert!(!is_plausible_tag(bad), "input: {bad:?}");
        }
        // 長すぎるタグ（画面とログを埋めない）
        assert!(!is_plausible_tag(&"v1.0.0".repeat(20)));
    }

    /// スコープを抜けるときにディレクトリごと消す作業場。
    /// アサート失敗でパニックしても Drop は走るので一時ファイルを残さない
    struct Workspace(std::path::PathBuf);

    impl Drop for Workspace {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl Workspace {
        /// 並列実行・別チェックアウトと衝突しないようテスト名とプロセス ID で一意にする
        fn new(test_name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "ccdesk-test-{test_name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, name: &str, body: &str) -> std::path::PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, body).unwrap();
            path
        }

        /// ローカルファイルを curl に渡せる `file://` URL にする
        fn url(&self, name: &str) -> String {
            format!(
                "file:///{}",
                self.0.join(name).display().to_string().replace('\\', "/")
            )
        }
    }

    /// 「配布物と同じ形の .sha256」を作る（certutil で計算した実ハッシュ +
    /// sha256sum の text 書式）
    fn sha256_of(path: &std::path::Path) -> String {
        certutil_hash(path).unwrap()
    }

    /// 正常系: 取得 → 検証 → 現行 exe を退避 → 新しい exe を元のパスへ。
    /// 実際に curl（file://）と certutil を通すので、ネットワーク以外の
    /// 差し替え経路がそのまま動くことを確認できる
    #[test]
    fn installs_a_verified_download_and_parks_the_current_exe() {
        let ws = Workspace::new("install-ok");
        let exe = ws.write("ccdesk.exe", "OLD BINARY");
        let asset = ws.write(ASSET_NAME, "NEW BINARY");
        ws.write(
            &format!("{ASSET_NAME}.sha256"),
            &format!("{}  {ASSET_NAME}\n", sha256_of(&asset)),
        );

        let installed = install_at(&exe, &ws.url(ASSET_NAME), &ws.url(&format!("{ASSET_NAME}.sha256")))
            .expect("検証済みの更新が適用されていない");

        assert_eq!(std::fs::read_to_string(&installed.exe).unwrap(), "NEW BINARY");
        assert_eq!(
            std::fs::read_to_string(&installed.old).unwrap(),
            "OLD BINARY",
            "現行 exe が <exe>.old へ退避されていない"
        );
        assert_eq!(installed.old, old_exe_path(&exe));
        assert!(
            !staged_exe_path(&exe).exists(),
            "ステージ用の <exe>.new が残っている"
        );
    }

    /// SHA-256 が合わないときはインストール済みの exe に触らない。
    /// 退避（.old）も作らない = 何も起きなかった状態で終わる
    #[test]
    fn leaves_the_installed_exe_untouched_on_checksum_mismatch() {
        let ws = Workspace::new("install-mismatch");
        let exe = ws.write("ccdesk.exe", "OLD BINARY");
        ws.write(ASSET_NAME, "NEW BINARY");
        // 形は正しいが中身が違うハッシュ（書式チェックだけを通す）
        ws.write(&format!("{ASSET_NAME}.sha256"), &format!("{HASH}  {ASSET_NAME}\n"));

        let err = install_at(&exe, &ws.url(ASSET_NAME), &ws.url(&format!("{ASSET_NAME}.sha256")))
            .expect_err("ハッシュ不一致で差し替えてしまっている");
        assert!(err.to_string().contains("mismatch"), "{err}");
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "OLD BINARY");
        assert!(!old_exe_path(&exe).exists(), "退避ファイルが残っている");
        assert!(
            !staged_exe_path(&exe).exists(),
            "検証前にステージしている（検証を通るまで exe の隣に何も置かない）"
        );
    }

    /// ダウンロード失敗（存在しないアセット = 新リリースに .exe が無い等）でも同じ。
    /// 一時ファイルは TempDir が消すので後始末も要らない
    #[test]
    fn leaves_the_installed_exe_untouched_when_the_download_fails() {
        let ws = Workspace::new("install-404");
        let exe = ws.write("ccdesk.exe", "OLD BINARY");

        let err = install_at(&exe, &ws.url("missing.exe"), &ws.url("missing.exe.sha256"))
            .expect_err("取得に失敗したのに差し替えている");
        assert!(err.to_string().contains("download failed"), "{err}");
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "OLD BINARY");
        assert!(!old_exe_path(&exe).exists());
        assert!(!staged_exe_path(&exe).exists());
    }

    /// ステージ（`<exe>.new` への配置）に失敗しても実行ファイルには触らない。
    /// ステージは実行ファイルを退避する**前**の手なので、ここで折り返せば
    /// 「退避したのに新版が入らない」状態には入らない。
    /// 失敗の作り方: `<exe>.new` をディレクトリにして rename もコピーも通らなくする
    #[test]
    fn leaves_the_installed_exe_untouched_when_staging_fails() {
        let ws = Workspace::new("install-stage-fail");
        let exe = ws.write("ccdesk.exe", "OLD BINARY");
        let asset = ws.write(ASSET_NAME, "NEW BINARY");
        ws.write(
            &format!("{ASSET_NAME}.sha256"),
            &format!("{}  {ASSET_NAME}\n", sha256_of(&asset)),
        );
        std::fs::create_dir_all(staged_exe_path(&exe)).unwrap();

        let err = install_at(
            &exe,
            &ws.url(ASSET_NAME),
            &ws.url(&format!("{ASSET_NAME}.sha256")),
        )
        .expect_err("ステージに失敗したのに成功を返している");
        assert!(err.to_string().contains("could not stage"), "{err}");
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "OLD BINARY");
        assert!(!old_exe_path(&exe).exists(), "退避まで進んでしまっている");
    }

    /// 退避（`<exe>.old` への改名）に失敗しても実行ファイルは残り、ステージ済みの
    /// `<exe>.new` も片付ける（失敗のたびに数 MB を置き去りにしない）。
    /// 失敗の作り方: `<exe>.old` をディレクトリにして改名先を塞ぐ。実運用で塞がるのは
    /// 「走っている ccdesk が前回の `.old` を掴んでいる」ケース
    #[test]
    fn keeps_the_exe_and_clears_the_stage_when_parking_fails() {
        let ws = Workspace::new("install-park-fail");
        let exe = ws.write("ccdesk.exe", "OLD BINARY");
        let asset = ws.write(ASSET_NAME, "NEW BINARY");
        ws.write(
            &format!("{ASSET_NAME}.sha256"),
            &format!("{}  {ASSET_NAME}\n", sha256_of(&asset)),
        );
        std::fs::create_dir_all(old_exe_path(&exe)).unwrap();

        let err = install_at(
            &exe,
            &ws.url(ASSET_NAME),
            &ws.url(&format!("{ASSET_NAME}.sha256")),
        )
        .expect_err("退避に失敗したのに成功を返している");
        assert!(
            err.to_string().contains("could not move the current exe aside"),
            "{err}"
        );
        assert_eq!(
            std::fs::read_to_string(&exe).unwrap(),
            "OLD BINARY",
            "退避に失敗したのに実行ファイルが失われている"
        );
        assert!(
            !staged_exe_path(&exe).exists(),
            "ステージした <exe>.new が残っている"
        );
    }

    /// ローカルビルドがリリースより新しいときに更新を勧めない
    #[test]
    fn only_reports_tags_newer_than_this_build() {
        let current = env!("CARGO_PKG_VERSION");
        assert!(!tag_is_newer(&format!("v{current}")), "同版で更新を勧めている");
        assert!(!tag_is_newer("v0.0.1"));
        assert!(tag_is_newer("v999.0.0"));
    }
}
