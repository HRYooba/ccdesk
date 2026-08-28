//! ccdesk 自身の更新: リリースタグの取得・アセットのダウンロード・SHA-256 検証・
//! 実行ファイルの差し替え。更新の知識はこのモジュールに閉じる:
//! 呼び出し口は `ccdesk update`（CLI）と、サイドバー上部の版行のクリック
//! （[`install`] をバックグラウンドスレッドで呼ぶ）の 2 つ。
//!
//! 新しい版があるかの周期チェックは [`crate::poll`] が持つ（claude の版チェックと
//! **同じゲートで回す**ため）。ここが提供するのは [`latest_tag`] と
//! [`tag_is_newer`] の 2 つで、**組み合わせない**: 「取得できなかった」と
//! 「新しい版が無い」を呼び手が区別できる必要があるため（1 回の通信失敗で
//! 更新マーカーを 1 時間消してはいけない）。
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

/// 小さなテキスト 1 本の HTTP GET（版番号・リリース JSON）。
/// **ネットワークへ出る作法（curl のフラグ・タイムアウト）はここ 1 箇所**:
/// タイムアウトは必須で、応答しないネットワーク（DNS シンクホール・blackhole
/// されたプロキシ）で呼び出し元のスレッドをぶら下げない。返るのは短いテキスト
/// なので接続 3s・全体 8s で足りる。失敗しても呼び手が周期で再試行する
pub(crate) fn http_get(url: &str) -> Option<String> {
    let out = std::process::Command::new("curl")
        .args(["-fsSL", "--connect-timeout", "3", "--max-time", "8", url])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// 最新リリースタグ（"v0.3.0"）。取得・パースできなければ None
pub(crate) fn latest_tag() -> Option<String> {
    serde_json::from_str::<serde_json::Value>(&http_get(LATEST_RELEASE_API)?)
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

/// 直前の自己更新で退避した `<exe>.old` を消す。更新した当のプロセスが生きている
/// 間はファイルが掴まれていて消せないので、掃除は次にプロセスを起こしたときになる。
/// 呼ぶのは TUI 起動（`main`）と `doctor` の 2 箇所で、`ccdesk update` の出力も
/// その 2 つを案内する。「無い」「まだ掴まれている」はどちらも正常なので失敗は無視する
pub(crate) fn cleanup_old_exe() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(old_exe_path(&exe));
    }
}

/// agent が置き去りにした残骸を消す（[`crate::backend::Garbage`] の宣言に従う）。
///
/// **自分の `<exe>.old` を消すのと同じ扱い**にする（[`cleanup_old_exe`] の隣で
/// 呼ぶ）。ccdesk がセッションを常駐させるせいで agent 側の掃除が空振りする以上、
/// 後始末は ccdesk の仕事になる。
///
/// **消せなかったものも数は捨てない**（[`Swept`]）。消せない理由は見ていないので
/// 断定しないが、次の更新を塞ぐかどうかは backend が答えるので、塞ぐものだけ
/// 場所を空ける。消せないこと自体は異常ではない: 次の起動でもう一度来る。
/// 報告するのは `doctor` だけ（TUI の起動列は黙って進む）
pub(crate) fn sweep_agent_leftovers(kinds: &[crate::backend::Kind]) -> Swept {
    let specs: Vec<_> = kinds
        .iter()
        .flat_map(|kind| kind.backend().garbage())
        .collect();
    let mut swept = Swept { deleted: 0, quarantined: 0, stuck: 0 };
    // **退かした置き場を先に掃く。** 同じ sweep の中で新しく退かしたものを
    // すぐ再走査しないためで、掴みが解けているものはここで消える
    for held in held_dirs(&specs) {
        for path in entries_in(&held) {
            swept.add(collect(&path, false));
        }
        // 空になった置き場は畳む（`remove_dir` は空のときだけ通るので、
        // 中身が残っているかを別に確かめなくてよい）
        let _ = std::fs::remove_dir(&held);
    }
    for spec in &specs {
        for path in crate::backend::leftovers_in(&spec.dir, &spec.prefix, &spec.rest_ok) {
            swept.add(collect(&path, spec.blocks_next_update));
        }
    }
    swept
}

/// 退かした残骸の置き場（[`quarantine`] の行き先と同じ集合）。
///
/// **重複を潰す。** `%TEMP%` の置き場は agent に属さないので、宣言ごとに掃くと
/// 同じ中身を宣言の数だけ数える（実測: 3 件が "6" と報告された）。
/// 同一ボリュームの置き場も、2 つの agent が同じディレクトリを指せば重なる
fn held_dirs(specs: &[crate::backend::Garbage]) -> std::collections::BTreeSet<std::path::PathBuf> {
    specs
        .iter()
        // **塞がない宣言には置き場ができない**（[`collect`] が退かさない）ので見に行かない
        .filter(|spec| spec.blocks_next_update)
        .map(|spec| spec.dir.join(HELD_DIR))
        .chain([held_dir()])
        .collect()
}

/// ディレクトリ直下の全て。**述語を持たない**: 退かした置き場に居るのは
/// ccdesk が自分で動かしたものだけなので、名前から正体を推し量る必要が無い
fn entries_in(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect()
}

/// 退かした残骸の置き場の名前。
///
/// **先頭ドット**にしてあるのは、置き場を agent のツリーの直下に作るため:
/// パッケージ・設定として読まれる名前だと、agent 側のツールが中身を解釈しに来る
const HELD_DIR: &str = ".ccdesk-held";

/// 同一ボリュームに置けなかったときの行き先（[`quarantine`]）
fn held_dir() -> std::path::PathBuf {
    std::env::temp_dir().join(HELD_DIR)
}

/// [`sweep_agent_leftovers`] の結果。**3 つに分ける**:
/// 消せた / 退かした（消せていないが次の更新は塞がない）/ 元の場所に残った。
///
/// 2 値だった頃は「動かしたが消せていない」と「元の場所に残った」が混ざっていた。
/// **前者は無害で、後者だけが次の更新を壊す**ので、混ぜると doctor が
/// 深刻さを取り違える
pub(crate) struct Swept {
    /// 消せた数
    pub(crate) deleted: usize,
    /// 消せなかったが、次の更新を塞がない場所へ動かした数
    pub(crate) quarantined: usize,
    /// 消せず、動かせもしなかった数（元の場所に残っている）
    pub(crate) stuck: usize,
}

impl Swept {
    /// 残骸 1 つぶんの結果を足し込む
    fn add(&mut self, one: Collected) {
        match one {
            Collected::Deleted => self.deleted += 1,
            Collected::Quarantined => self.quarantined += 1,
            Collected::Stuck => self.stuck += 1,
        }
    }
}

/// 残骸 1 つの後始末の結果（[`Swept`] の 1 件ぶん）
enum Collected {
    Deleted,
    Quarantined,
    Stuck,
}

/// 残骸 1 つを片付ける。
///
/// 消せなかったとき、**塞ぐものだけ**退かす（[`crate::backend::Garbage`]）。
/// 塞がないものを動かしても、述語で正体が分かる場所から出るだけで得が無い。
///
/// **退かした置き場の中身は必ず `blocks = false` で来る**（呼び手が渡す）ので、
/// 退かしたものが退かし直されて名前が伸びる経路は存在しない
fn collect(path: &std::path::Path, blocks_next_update: bool) -> Collected {
    if delete(path) {
        return Collected::Deleted;
    }
    if !blocks_next_update {
        return Collected::Stuck;
    }
    // 動かしたうえでもう一度消しに行く（掴まれているのは中の 1 本だけのことが
    // 多く、残りは消える）。**動かせただけでは deleted と数えない**:
    // 消えていないものを「片付いた」と報告すると doctor が嘘をつく
    match quarantine(path) {
        Some(moved) => match delete(&moved) {
            true => Collected::Deleted,
            false => Collected::Quarantined,
        },
        None => Collected::Stuck,
    }
}

/// 消す 1 手。**ファイルとディレクトリの両方が来る**: claude は退避した
/// 実行ファイルと版ごとの実体を残し、npm は作業ディレクトリごと残す。
/// 片方の消し方しか持たないと、もう片方が毎回失敗して静かに溜まり続ける
fn delete(path: &std::path::Path) -> bool {
    match path.is_dir() {
        true => std::fs::remove_dir_all(path).is_ok(),
        false => std::fs::remove_file(path).is_ok(),
    }
}

/// 消せなかった残骸を隔離先へ動かす（動かせたら新しいパス）。
///
/// **Windows は掴まれたファイルを消せないが、改名（別ディレクトリへの移動を
/// 含む）はできる**（実測 2026-08-27）。ccdesk 自身の実行ファイル差し替え
/// （[`install_at`]）が既に頼っている性質で、走っているプロセスは掴んだ
/// イメージのまま動き続ける。
///
/// 行き先は**同一ボリューム優先**（残骸の隣）。`rename` はボリュームをまたげない
/// ので、`%TEMP%` を別ドライブへ向けている環境で `%TEMP%` だけを狙うと退避が
/// 黙って失敗し、次の更新が落ち続ける形へ戻る。どちらも駄目なら動かさない。
///
/// 名前はプロセス ID + 呼び出し毎の連番。**既にある名前は使わない**
/// （ccdesk を起動し直すと pid が再利用され連番も 0 から始まるので、
/// 確かめずに `rename` すると前回退かした残骸を上書きしうる）
fn quarantine(path: &std::path::Path) -> Option<std::path::PathBuf> {
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let leaf = path.file_name()?.to_string_lossy().into_owned();
    let dirs = [path.parent()?.join(HELD_DIR), held_dir()];
    dirs.into_iter().find_map(|dir| {
        std::fs::create_dir_all(&dir).ok()?;
        (0..16).find_map(|_| {
            let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let moved = dir.join(format!("{}-{seq}-{leaf}", std::process::id()));
            (!moved.exists() && std::fs::rename(path, &moved).is_ok()).then_some(moved)
        })
    })
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
            "SHA256 digest for target C:\\x\\ccdesk.exe:\r\n{}\r\nCertUtil: finished successfully.\r\n",
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
            "release.yml uploads an asset name that doesn't match ASSET_NAME ({ASSET_NAME})"
        );
        // `.sha256` の URL は実行ファイル URL への接尾で組み立てる（asset_urls）ので、
        // 生産側も同じ名前 + ".sha256" で上げていること
        assert!(
            yml.contains("\"$asset.sha256\""),
            "release.yml uploads the checksum under a name other than <asset-name>.sha256"
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

    /// スコープを抜けるときにディレクトリごと消す作業場
    /// （安全な置き場の実装は [`crate::testutil::TempDir`] 1 つ）
    struct Workspace(crate::testutil::TempDir);

    impl Workspace {
        fn new(test_name: &str) -> Self {
            Self(crate::testutil::TempDir::new("update", test_name))
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
            .expect("verified update was not applied");

        assert_eq!(std::fs::read_to_string(&installed.exe).unwrap(), "NEW BINARY");
        assert_eq!(
            std::fs::read_to_string(&installed.old).unwrap(),
            "OLD BINARY",
            "current exe was not parked to <exe>.old"
        );
        assert_eq!(installed.old, old_exe_path(&exe));
        assert!(
            !staged_exe_path(&exe).exists(),
            "staged <exe>.new remains"
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
            .expect_err("replaced despite a hash mismatch");
        assert!(err.to_string().contains("mismatch"), "{err}");
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "OLD BINARY");
        assert!(!old_exe_path(&exe).exists(), "parked file remains");
        assert!(
            !staged_exe_path(&exe).exists(),
            "staged before verification (nothing should be placed next to the exe until verification passes)"
        );
    }

    /// ダウンロード失敗（存在しないアセット = 新リリースに .exe が無い等）でも同じ。
    /// 一時ファイルは TempDir が消すので後始末も要らない
    #[test]
    fn leaves_the_installed_exe_untouched_when_the_download_fails() {
        let ws = Workspace::new("install-404");
        let exe = ws.write("ccdesk.exe", "OLD BINARY");

        let err = install_at(&exe, &ws.url("missing.exe"), &ws.url("missing.exe.sha256"))
            .expect_err("replaced despite the download failing");
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
        .expect_err("returned success despite staging failing");
        assert!(err.to_string().contains("could not stage"), "{err}");
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "OLD BINARY");
        assert!(!old_exe_path(&exe).exists(), "proceeded to parking anyway");
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
        .expect_err("returned success despite parking failing");
        assert!(
            err.to_string().contains("could not move the current exe aside"),
            "{err}"
        );
        assert_eq!(
            std::fs::read_to_string(&exe).unwrap(),
            "OLD BINARY",
            "exe was lost even though parking failed"
        );
        assert!(
            !staged_exe_path(&exe).exists(),
            "staged <exe>.new remains"
        );
    }

    /// 残骸はファイルとディレクトリの**両方**が来る。片方の消し方しか持たないと、
    /// もう片方（npm の作業場はディレクトリ）が毎回失敗して溜まり続ける
    #[test]
    fn leftovers_are_removed_whether_they_are_files_or_directories() {
        let ws = Workspace::new("sweep");
        let file = ws.write("claude.exe.old.1785884570678", "PARKED BINARY");
        let dir = ws.0.join(".codex-g3ieL94X");
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        std::fs::write(dir.join("node_modules").join("big.bin"), "x").unwrap();

        assert!(matches!(collect(&dir, true), Collected::Deleted));
        assert!(matches!(collect(&file, false), Collected::Deleted));
        assert!(!file.exists() && !dir.exists());
        // 無いものを消しても落ちない（消せなかった場合と同じ扱い）
        assert!(matches!(collect(&file, false), Collected::Stuck));
    }

    /// **次の更新を塞ぐ残骸だけ場所を空ける。** npm の退避先は `retire-path.js` が
    /// パスの sha1 から導く決定論的な名前なので、掴まれた実行ファイル 1 本が
    /// そこに残ると次の `codex update` が `EBUSY` で落ちる（実測）。
    /// 消せないままでも「元の場所から居なくなる」ことをここで固定する
    #[test]
    fn a_blocking_leftover_that_cannot_be_deleted_is_moved_out_of_the_way() {
        let ws = Workspace::new("sweep_quarantine");
        let dir = ws.0.join(".codex-g3ieL94X");
        std::fs::create_dir_all(&dir).unwrap();
        let running = RunningImage::spawn_in(&dir);

        // 消せていないので Deleted にはならない。**それでも退避先は空く**
        assert!(matches!(collect(&dir, true), Collected::Quarantined));
        assert!(!dir.exists(), "the retire path is still occupied");
        // 行き先は**同一ボリューム優先** ＝ 残骸の隣（`%TEMP%` ではない）
        let beside = ws.0.join(HELD_DIR);
        assert_eq!(entries_in(&beside).len(), 1, "the leftover did not land beside its own tree");
        assert!(entries_in(&held_dir()).iter().all(|p| !p.starts_with(ws.0.path())));

        // **退かしたものは退かし直さない**（掃除のたびに名前が伸びる経路を作らない）。
        // 呼び手は置き場の中身を必ず「塞がない」として渡す
        let moved = entries_in(&beside).pop().unwrap();
        assert!(matches!(collect(&moved, false), Collected::Stuck));
        assert_eq!(entries_in(&beside).len(), 1, "the quarantined leftover multiplied");

        // 掴んでいたセッションが終われば消える
        drop(running);
        assert!(matches!(collect(&moved, false), Collected::Deleted));
        assert!(entries_in(&beside).is_empty());
    }

    /// **塞がない残骸は動かさない。** claude の退避 exe は名前にミリ秒を持つので
    /// 次の更新と衝突しない ＝ 動かしても、述語で正体が分かる場所から出るだけで
    /// 何も得られない。消せなければ元の場所で次の掃除を待つ
    #[test]
    fn a_leftover_that_does_not_block_the_next_update_stays_where_it_is() {
        let ws = Workspace::new("sweep_held");
        let held_one = ws.write("claude.exe.old.2", "STILL RUNNING");
        // 「そのイメージで動いているセッションがいる」を再現する:
        // 削除共有なしで開いている間、remove_file は共有違反で失敗する
        use std::os::windows::fs::OpenOptionsExt;
        let _handle = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1) // FILE_SHARE_READ（削除を許さない）
            .open(&held_one)
            .unwrap();

        assert!(matches!(collect(&held_one, false), Collected::Stuck));
        assert!(held_one.exists(), "a leftover that blocks nothing was moved for no gain");
        assert!(entries_in(&ws.0.join(HELD_DIR)).is_empty());
    }

    /// **置き場は 1 度しか掃かない。** `%TEMP%` の置き場は agent に属さないので、
    /// 宣言ごとに掃くと同じ中身を宣言の数だけ数える（実測: 3 件が "6" と報告された）。
    /// **塞がない宣言には置き場ができない**ので、そこは見に行かない
    #[test]
    fn the_quarantine_directories_are_visited_once_each() {
        let spec = |dir: &str, blocks| crate::backend::Garbage {
            dir: std::path::PathBuf::from(dir),
            prefix: String::new(),
            rest_ok: Box::new(|_| true),
            blocks_next_update: blocks,
        };
        // 同じ場所を指す 2 つの宣言（2 agent が同じツリーに居る場合）
        let dirs = held_dirs(&[spec("C:/a", true), spec("C:/a", true), spec("C:/b", false)]);
        assert_eq!(
            dirs,
            [std::path::PathBuf::from("C:/a").join(HELD_DIR), held_dir()]
                .into_iter()
                .collect(),
            "a quarantine directory was visited twice, or a non-blocking one was visited at all"
        );
    }

    /// 掴まれた実行ファイルを**本物のプロセス**で作る。
    ///
    /// **共有指定を手で真似られない**: 走っているプロセスのイメージは
    /// 「消せないが改名できる」組み合わせで開かれていて、
    /// `OpenOptions::share_mode` では再現できない（`FILE_SHARE_DELETE` を
    /// 落とすと改名も拒まれ、入れると POSIX 意味論で消せてしまう）。
    /// Drop で確実に終わらせるので、テストが落ちても子が残らない
    struct RunningImage(std::process::Child);

    impl RunningImage {
        /// `dir` の中へ実行ファイルを 1 本置いて起こす。中身は何でもよく、
        /// 「そこそこ生き続ける」「stdin を読まない」ことだけが要る
        fn spawn_in(dir: &std::path::Path) -> Self {
            let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
            let exe = dir.join("codex.exe");
            std::fs::copy(
                std::path::Path::new(&root).join("System32").join("PING.EXE"),
                &exe,
            )
            .expect("could not stage an executable to hold");
            let child = std::process::Command::new(&exe)
                .args(["-n", "60", "127.0.0.1"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("could not start the process that holds the image");
            Self(child)
        }
    }

    impl Drop for RunningImage {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// ローカルビルドがリリースより新しいときに更新を勧めない
    #[test]
    fn only_reports_tags_newer_than_this_build() {
        let current = env!("CARGO_PKG_VERSION");
        assert!(!tag_is_newer(&format!("v{current}")), "recommended an update for the same version");
        assert!(!tag_is_newer("v0.0.1"));
        assert!(tag_is_newer("v999.0.0"));
    }
}
