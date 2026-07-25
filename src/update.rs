//! ccdesk 自身の更新: リリースタグの取得・アセットのダウンロード・SHA-256 検証・
//! 実行ファイルの差し替え。`ccdesk update`（CLI）とサイドバーの更新行はどちらも
//! ここを通る（更新の知識をこのモジュールに 1 か所へ閉じる）。
//!
//! Windows では**動いている実行ファイルを上書きできない**（`Device or resource
//! busy`）が、**別名へ改名することはできる**（実測）。そのため差し替えは
//! 「現行 exe を `<exe>.old` へ退避 → 新しい exe を元のパスへ置く」で行う。
//! 走っているプロセスは現行版のまま動き続け、新しい版は次回起動から有効になる。

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
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

/// タグ（"v0.3.0"）がこのビルドより新しいか。判定はフッターの更新判定と同じ
/// [`ccdesk::version_newer`]。ローカルビルドがリリースより新しいときに
/// 「更新できます」と誤案内しないためのガードでもある
pub(crate) fn tag_is_newer(tag: &str) -> bool {
    ccdesk::version_newer(tag.trim_start_matches('v'), env!("CARGO_PKG_VERSION"))
}

/// 直前の自己更新で退避した `<exe>.old` を消す。更新した当のプロセスが生きている
/// 間はファイルが掴まれていて消せないので、次回起動時のこの 1 回が唯一の掃除機会。
/// 「無い」「まだ掴まれている」はどちらも正常なので失敗は無視する
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
    let actual = certutil_hash(&new_exe)?;
    if !actual.eq_ignore_ascii_case(&published) {
        anyhow::bail!("SHA-256 mismatch (published {published}, downloaded {actual})");
    }

    let old = old_exe_path(exe);
    // Windows の rename は既存の宛先を上書きしないので、前回の残骸を先に消す
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old)
        .map_err(|e| anyhow::anyhow!("could not move the current exe aside: {e}"))?;
    if let Err(e) = move_file(&new_exe, exe) {
        // 退避したまま終わると実行ファイルが無くなる。必ず戻す
        let _ = std::fs::rename(&old, exe);
        return Err(anyhow::anyhow!("could not install the new exe: {e}"));
    }
    Ok(Installed {
        exe: exe.to_path_buf(),
        old,
    })
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

/// `.sha256` ファイルからハッシュを取り出す。書式は `sha256sum` の text モードと
/// 同じ `<hex>␠␠<ファイル名>` で、先頭トークンがハッシュ（生産側は
/// .github/workflows/release.yml の "Upload assets"）
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

/// 退避先 `<exe>.old`。拡張子を置き換えず末尾に足す
/// （`ccdesk.exe` → `ccdesk.exe.old`。`with_extension` だと `ccdesk.old` になる）
fn old_exe_path(exe: &std::path::Path) -> std::path::PathBuf {
    let mut path = exe.as_os_str().to_os_string();
    path.push(".old");
    std::path::PathBuf::from(path)
}

/// ファイルを移す。一時ディレクトリと実行ファイルが別ボリュームだと rename は
/// 失敗するので、そのときはコピーしてから元を消す
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

    /// 退避先は拡張子を置き換えず末尾に足す（`ccdesk.old` にすると
    /// 掃除対象を取り違え、拡張子なしの実行ファイルも壊す）
    #[test]
    fn appends_old_suffix_without_replacing_the_extension() {
        assert_eq!(
            old_exe_path(std::path::Path::new("C:\\bin\\ccdesk.exe")),
            std::path::PathBuf::from("C:\\bin\\ccdesk.exe.old")
        );
        assert_eq!(
            old_exe_path(std::path::Path::new("C:\\bin\\ccdesk")),
            std::path::PathBuf::from("C:\\bin\\ccdesk.old")
        );
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
