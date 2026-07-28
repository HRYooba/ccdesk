//! 端末クエリ応答: claude CLI は起動時に CPR（\x1b[6n）等を送り、応答が無いとブロックする。
//! 本物の端末が返す応答を vt100 の Callbacks で肩代わりし、pending に溜めて PTY へ書き戻す。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::anyhow;

/// vt100 が処理しないシーケンスのうち「応答を要求するクエリ」に応答を生成し、
/// 子プロセスが有効化した入力プロトコル（kitty / modifyOtherKeys / focus）を追跡する。
#[derive(Default)]
pub struct Responder {
    pending: Vec<u8>,
    /// kitty keyboard protocol の現在フラグ（CSI > flags u で push される）
    pub kitty_flags: u16,
    kitty_stack: Vec<u16>,
    /// modifyOtherKeys のレベル（CSI > 4 ; level m）
    pub modify_other_keys: u16,
    /// DECSET/DECRST 1004（focus reporting）の有効状態
    pub focus_reporting: bool,
    /// OSC 10/11 応答用のホスト端末色（16bit/ch RGB）。起動時に実端末へ照会した値を
    /// 転送する（claude のテーマ自動検出はこの応答の輝度で light/dark を判定する）。
    /// None = 照会失敗時のフォールバック（Dark+ 相当の固定値）
    pub host_fg: Option<[u16; 3]>,
    pub host_bg: Option<[u16; 3]>,
}

impl Responder {
    /// 溜まった応答バイトを取り出す（呼び出し側が PTY writer へ書き戻す）
    pub fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
}

impl vt100::Callbacks for Responder {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let first = |n: usize| params.get(n).and_then(|p| p.first()).copied();
        match (i1, i2, c) {
            // DSR 6 = CPR: カーソル位置報告（1 始まり） / DSR 5 = 状態報告
            (None, None, 'n') => match first(0) {
                Some(6) => {
                    let (row, col) = screen.cursor_position();
                    self.pending
                        .extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
                }
                Some(5) => self.pending.extend_from_slice(b"\x1b[0n"),
                _ => {}
            },
            // DA1: VT220 相当を名乗る
            (None, None, 'c') => {
                self.pending.extend_from_slice(b"\x1b[?62;22c");
            }
            // DA2: Windows Terminal 風の応答
            (Some(b'>'), None, 'c') => {
                self.pending.extend_from_slice(b"\x1b[>0;10;1c");
            }
            // XTWINOPS 18: 文字セル単位の画面サイズ報告
            (None, None, 't') if first(0) == Some(18) => {
                let (rows, cols) = screen.size();
                self.pending
                    .extend_from_slice(format!("\x1b[8;{rows};{cols}t").as_bytes());
            }
            // DECRQM: 追跡しているモードは実状態を返し、未知は 0（未認識）
            (Some(b'?'), Some(b'$'), 'p') => {
                if let Some(mode) = first(0) {
                    use vt100::MouseProtocolMode as MM;
                    let known = |on: bool| if on { 1 } else { 2 };
                    let value = match mode {
                        25 => known(!screen.hide_cursor()),
                        // X10(9) と通常(1000) は**別のモード**。X10 有効中の 1000 照会に
                        // 「有効」と答えると、応答を信じた子は X10 では送られない
                        // ボタン解放（release）を待ち続ける
                        9 => known(screen.mouse_protocol_mode() == MM::Press),
                        1000 => known(screen.mouse_protocol_mode() == MM::PressRelease),
                        1002 => known(screen.mouse_protocol_mode() == MM::ButtonMotion),
                        1003 => known(screen.mouse_protocol_mode() == MM::AnyMotion),
                        1006 => known(
                            screen.mouse_protocol_encoding()
                                == vt100::MouseProtocolEncoding::Sgr,
                        ),
                        1004 => known(self.focus_reporting),
                        1049 => known(screen.alternate_screen()),
                        2004 => known(screen.bracketed_paste()),
                        _ => 0,
                    };
                    self.pending
                        .extend_from_slice(format!("\x1b[?{mode};{value}$y").as_bytes());
                }
            }
            // DECSET/DECRST のうち vt100 が追跡しない 1004（focus reporting）を自前追跡
            (Some(b'?'), None, 'h') => {
                if params.iter().any(|p| p.contains(&1004)) {
                    self.focus_reporting = true;
                }
            }
            (Some(b'?'), None, 'l') => {
                if params.iter().any(|p| p.contains(&1004)) {
                    self.focus_reporting = false;
                }
            }
            // kitty keyboard protocol: push / pop / set / query
            (Some(b'>'), None, 'u') => {
                self.kitty_stack.push(self.kitty_flags);
                self.kitty_flags = first(0).unwrap_or(0);
            }
            (Some(b'<'), None, 'u') => {
                let count = first(0).unwrap_or(1).max(1);
                for _ in 0..count {
                    self.kitty_flags = self.kitty_stack.pop().unwrap_or(0);
                }
            }
            (Some(b'='), None, 'u') => {
                let flags = first(0).unwrap_or(0);
                match first(1).unwrap_or(1) {
                    1 => self.kitty_flags = flags,
                    2 => self.kitty_flags |= flags,
                    3 => self.kitty_flags &= !flags,
                    _ => {}
                }
            }
            (Some(b'?'), None, 'u') => {
                self.pending
                    .extend_from_slice(format!("\x1b[?{}u", self.kitty_flags).as_bytes());
            }
            // modifyOtherKeys: CSI > 4 ; level m
            (Some(b'>'), None, 'm') if first(0) == Some(4) => {
                self.modify_other_keys = first(1).unwrap_or(0);
            }
            _ => {}
        }
    }

    fn unhandled_osc(&mut self, _screen: &mut vt100::Screen, params: &[&[u8]]) {
        // OSC 10;? / 11;? = 前景色・背景色の問い合わせ（claude のテーマ検出が使う）。
        // ホスト端末から照会できた実色を転送し、できなければ Dark+ 相当の固定値
        if params.len() >= 2 && params[1] == b"?" {
            let reply = |code: u8, c: [u16; 3]| {
                format!(
                    "\x1b]{};rgb:{:04x}/{:04x}/{:04x}\x07",
                    code, c[0], c[1], c[2]
                )
            };
            match params[0] {
                b"10" => {
                    let fg = self.host_fg.unwrap_or(DEFAULT_FG);
                    self.pending.extend_from_slice(reply(10, fg).as_bytes());
                }
                b"11" => {
                    let bg = self.host_bg.unwrap_or(DEFAULT_BG);
                    self.pending.extend_from_slice(reply(11, bg).as_bytes());
                }
                _ => {}
            }
        }
    }
}

/// 色照会に失敗したときのフォールバック前景色（VS Code Dark+ 相当、16bit/ch）。
/// **claude へ返す OSC 応答（[`Responder`]）と ccdesk 自身の UI トーン合成
/// （`theme::ui`）が同じ既定を仮定する**ので、値はここ 1 箇所に置く
/// （片方だけ変えると「claude に送った既定テーマ」と「ccdesk の描画が仮定する
/// テーマ」がずれる）
pub const DEFAULT_FG: [u16; 3] = [0xcccc, 0xcccc, 0xcccc];
/// 同・背景色
pub const DEFAULT_BG: [u16; 3] = [0x1e1e, 0x1e1e, 0x1e1e];

/// Responder 付き vt100 パーサ
pub type Parser = vt100::Parser<Responder>;

pub fn new_parser(rows: u16, cols: u16, scrollback: usize) -> Parser {
    vt100::Parser::new_with_callbacks(rows, cols, scrollback, Responder::default())
}

/// ホームディレクトリ（Windows 専用ツールなので USERPROFILE）。
/// **環境変数を読む場所はここ 1 箇所**: フォールバック（`HOME` を見る等）を
/// 足すことになったとき、直す場所が散らばらない
fn home() -> Option<std::path::PathBuf> {
    Some(std::path::PathBuf::from(std::env::var_os("USERPROFILE")?))
}

/// Claude Code の設定ディレクトリ。公式に CLAUDE_CONFIG_DIR で移動可能と明記されている
pub fn claude_dir() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Some(std::path::PathBuf::from(dir));
    }
    Some(home()?.join(".claude"))
}

/// claude の更新チャネル。settings.json の autoUpdatesChannel（公式に文書化された
/// 設定。"latest"(既定) / "stable"）を読む。CLAUDE_CONFIG_DIR にも追従する
pub fn claude_settings_channel() -> String {
    claude_dir()
        .and_then(|d| read_json(&d.join("settings.json")))
        .and_then(|v| {
            v.get("autoUpdatesChannel")
                .and_then(|c| c.as_str())
                .map(str::to_string)
        })
        .filter(|c| c == "stable")
        .unwrap_or_else(|| "latest".to_string())
}

/// バージョン文字列 "2.1.218" の数値比較（比較不能なら等価扱い）
pub fn version_newer(latest: &str, current: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> {
        s.split('.')
            .map(|p| {
                p.chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0)
            })
            .collect()
    };
    parse(latest) > parse(current)
}

/// ~/.claude.json（CLAUDE_CONFIG_DIR 設定時はその配下）。
/// 既定の置き場は **ホーム直下**であって `.claude` の中ではないので、
/// CLAUDE_CONFIG_DIR の分岐だけを [`claude_dir`] へ寄せる
pub fn claude_json_path() -> Option<std::path::PathBuf> {
    if std::env::var_os("CLAUDE_CONFIG_DIR").is_some() {
        return Some(claude_dir()?.join(".claude.json"));
    }
    Some(home()?.join(".claude.json"))
}

/// ccdesk 自身のデータ置き場 ~/.ccdesk/（config.json と error.log）。無ければ作る。
/// doctor の書き込み可否チェックで参照するため公開する。
///
/// **成功したら以降はキャッシュを返す**: run ループの毎周回（33ms）から
/// hook-states の stamp 読み経由で呼ばれるので、環境変数読みと
/// `create_dir_all` を毎回やり直さない。失敗はキャッシュしない
/// （一時的に作れなかっただけなら次の呼び出しで再試行する）
pub fn ccdesk_dir() -> Option<std::path::PathBuf> {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    if let Some(dir) = DIR.get() {
        return Some(dir.clone());
    }
    let dir = home()?.join(".ccdesk");
    std::fs::create_dir_all(&dir).ok()?;
    let _ = DIR.set(dir.clone());
    Some(dir)
}

/// エラーログ ~/.ccdesk/error.log のパス（logs コマンドで参照）
pub fn error_log_path() -> Option<std::path::PathBuf> {
    Some(ccdesk_dir()?.join("error.log"))
}

/// ccdesk のユーザー設定（グルーピング選択・使用率表示の opt-in 等）。~/.ccdesk/config.json
fn settings_path() -> Option<std::path::PathBuf> {
    Some(ccdesk_dir()?.join("config.json"))
}

/// 前回の画面状態（サイドバー幅・最後に開いていたセッション・new session のフォルダ等）。
/// 設定と違いユーザーが編集する想定のないデータなので config.json とは分ける
fn state_path() -> Option<std::path::PathBuf> {
    Some(ccdesk_dir()?.join("state.json"))
}

/// 撤去したアカウント切り替えが使っていた保管ファイルの名前。
/// **トークンを含む**ので、残っていれば起動時に消す（[`purge_account_store`]）
const ACCOUNT_STORE: &str = "accounts.json";

/// 撤去したアカウント切り替えの名残（`~/.ccdesk/accounts.json` とその付随物）を
/// 消す。**呼ぶのは `main` の起動列だけ**（[`enable_error_log`] と同じ扱いで、
/// `main` を通らないプロセス ＝ テストの実行ファイルは構造上ここへ到達しない）。
///
/// **無ければ何もしない**（ログも出さない）。失敗しても起動は止めない:
/// 消せなかったところで ccdesk はもうこのファイルを読まないので、
/// 起動を止める理由にならない
pub fn purge_account_store() {
    let Some(store) = ccdesk_dir().map(|dir| dir.join(ACCOUNT_STORE)) else {
        return;
    };
    if remove_account_store(&store) {
        // **1 行だけ残す。** 消えた理由が分からないと、保管を頼りにしていた人が
        // 「壊れた」と読む（`ccdesk logs` に出る）
        log_error("removed the account store (account switching was dropped)");
    }
}

/// 保管ファイル本体・そのロック・書きかけの `.tmp` を消す。
/// 戻り値は**本体を消したか**（付随物だけが残っていた場合はログを出さない）。
///
/// **パスを引数で受ける**のはテストが実ユーザーの `~/.ccdesk` を触らないため。
/// tmp は古さ（[`TMP_KEEP`]）を見ずに消す: このファイルを書く者はもう居ないので、
/// 「今まさに書いている別インスタンスの tmp」は存在しない
fn remove_account_store(store: &Path) -> bool {
    let removed = std::fs::remove_file(store).is_ok();
    // ロックの実体はディレクトリ（[`Lock`]）
    let _ = std::fs::remove_dir_all(lock_path_for(store));
    // tmp の回収は他の書き手と同じ 1 実装（[`reap_tmp_in`]）。古さは見ない:
    // このファイルを書く者はもう居ないので、「今まさに書いている別インスタンスの
    // tmp」は存在しない
    if let (Some(dir), Some(name)) = (store.parent(), store.file_name().and_then(|n| n.to_str())) {
        reap_tmp_in(dir, &[name], None);
    }
    removed
}

/// セッション一覧の正本 ~/.ccdesk/sessions.json。
/// 前景セッション（`claude --session-id <uuid>`）は `~/.claude/jobs` に痕跡を残さないので、
/// 「どのセッションが存在するか」は ccdesk 自身が持つ
pub fn sessions_store_path() -> Option<std::path::PathBuf> {
    Some(ccdesk_dir()?.join("sessions.json"))
}

/// hook が書いた state の受け渡し先 ~/.ccdesk/hook-states.json。
/// 子の claude へ `--settings` で注入した hook（`ccdesk hook <event>`）が書き、
/// TUI が周期的に読む（`crate::hooks` が形式の正本）
pub fn hook_states_path() -> Option<std::path::PathBuf> {
    Some(ccdesk_dir()?.join("hook-states.json"))
}

/// 現在時刻の epoch ms。**行の時刻・hook の時刻はすべてこの単位**
/// （`SessionRow` と `hook-states.json` が同じ物差しを使う）
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 現在時刻の epoch 秒。**使用率のリセット時刻・鮮度判定はこの単位**
/// （[`now_ms`] と同じ物差しから導く ＝ 記録する側と判定する側で epoch の
/// 取り方が分かれない）
pub fn now_secs() -> u64 {
    now_ms() / 1000
}

/// エラーログを書いてよいか。**決めるのは `main` だけ**（[`enable_error_log`]）。
///
/// **既定は「書かない」。** ログの出力先はプロセス全体で 1 つしかない隠れた
/// グローバルなので、注入で受けると呼び出し口の数だけ渡し忘れが作れる。代わりに
/// 「起動時に 1 度だけ有効化する」形にしてある ＝ `main` を通らないプロセス
/// （テストの実行ファイル）は構造上ここへ到達しない。
///
/// これが無かった頃、単体テストの失敗（一時ディレクトリのパスを含むもの）が
/// **実ユーザーの `~/.ccdesk/error.log` へ**溜まっていた（`cargo test` を回すたびに増える）
static ERROR_LOG_ENABLED: AtomicBool = AtomicBool::new(false);

/// エラーログを有効にする。**呼ぶのは `main` の先頭だけ**（[`ERROR_LOG_ENABLED`]）
pub fn enable_error_log() {
    ERROR_LOG_ENABLED.store(true, Ordering::Relaxed);
}

/// エラーの集約先 ~/.ccdesk/error.log へ時刻付きで追記する。
/// panic（TUI は画面ごと消えて読めない）と実行時エラー（attach 失敗等）の両方が集まる。
/// **有効化されていないプロセスでは何も書かない**（[`ERROR_LOG_ENABLED`]）
pub fn log_error(msg: &str) {
    if !ERROR_LOG_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    // 書き先は `ccdesk logs` が読む先と同じ関数から取る（別の式で組むと、
    // 名前を変えたときに「書いた場所と読む場所が違う」状態を作れる）
    let Some(path) = error_log_path() else {
        return;
    };
    use std::io::Write as _;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "[{}] {msg}", now_iso());
    }
}

/// 現在時刻の UTC ISO 表記（Howard Hinnant の civil_from_days。依存を増やさない最小実装）
fn now_iso() -> String {
    let secs = now_secs() as i64;
    let (days, sod) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

// ---------------------------------------------------------------------------
// 複数プロセスで共有するファイルへの書き込み（advisory lock + tmp → rename）。
//
// **ここに置いてある理由**: 守る対象が複数ある（`~/.ccdesk/sessions.json` =
// ドメイン層の sessions モジュール、`~/.ccdesk/hook-states.json` = hooks モジュール、
// `~/.ccdesk/state.json` と `config.json` = このファイルの kv 群）。
// 「どう安全に書くか」の知識を対象ごとに持つと、片方だけ直した状態が生まれる
// （実際に起きた: 一方は pid + 連番の tmp 名で同時書き込みを避けているのに、
// state.json 側は全インスタンス共通の `state.json.tmp` を使っていた）。**仕組みは 1 つ、守る対象ごとに違うのは
// 「どのロックを使うか」と「どれだけ待つか」だけ**にしてある。
// ---------------------------------------------------------------------------

/// proper-lockfile のロック名は `<target>.lock`（拡張子の置換ではなく **付加**）。
/// ディレクトリを対象にしても同じ規則で、`<dir>.lock` が隣に並ぶ
pub fn lock_path_for(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    target.with_file_name(name)
}

/// mtime がこれより古いロックは死んだ保持者のものとして奪う（proper-lockfile の既定と同じ）
pub const LOCK_STALE: Duration = Duration::from_secs(10);
/// 取得の再試行間隔
const LOCK_RETRY: Duration = Duration::from_millis(100);
/// stale ロックを奪う回数の上限。奪った直後に別プロセスが取り直したなら
/// それは正当な競合なので、以降は通常の待ちに落とす（奪い合いで回り続けない）
const LOCK_MAX_STEALS: u32 = 3;

/// ファイル 1 本を守る advisory lock（RAII）。
///
/// 守るのは **ccdesk 自身のファイル**（`sessions.json` / `hook-states.json` /
/// `state.json` / `config.json`）。claude は触らないが、ccdesk を複数起動すると
/// 読み書きが交差する。仕組みを 1 つに保つのは、「どう排他するか」の知識を
/// 2 通り持つと片方だけ直した状態が生まれるため。違いは**どのファイルを守り、
/// どれだけ待つか**だけで、それは呼び手（守る対象を知っている側）が決める。
///
/// **プロトコルは npm `proper-lockfile`**（claude Code が OAuth トークン更新を
/// 保護しているのと同じもの）に合わせてある。自作せずに借りたのは、
/// 実装が枯れていて分解能や stale の扱いを実測で確かめられるため:
/// - ロックの実体は **ディレクトリ** `<target>.lock`。`mkdir` の原子性が mutex
/// - mtime が 10 秒より古いロックは stale とみなして奪ってよい
/// - 保持者は 5 秒ごとに mtime を touch して生存を示す
///
/// # 解放は所有権を確認してから行う
///
/// 取得した瞬間の mtime を所有権の印として持ち、[`Drop`] では **それが今も
/// 一致するときだけ** `rmdir` する。無条件に消すと、自分のロックが stale 判定で
/// 奪われた後（奪取は rmdir → mkdir なので mtime が変わる）に **奪った側の
/// ロックを消してしまい**、守っていたはずの区間に第三者が入れる状態を作る
/// ＝ このロックがまさに防ごうとしている上書きそのものになる。
///
/// 印に mtime を選んだ理由:
/// - **proper-lockfile も同じ基準で所有権を見ている。** 取得時の mtime と
///   現在の mtime が違えば "compromised" と判定する実装
/// - **ロックディレクトリの中に印のファイルは置けない。** 奪う側は `rmdir` で
///   消すが、非空ディレクトリの `rmdir` は `ENOTEMPTY` で失敗する ＝
///   中身を置くと stale ロックを誰も回収できなくなる
///
/// mtime の分解能（NTFS は 100ns 刻み）より短い間隔で奪われると判別できないが、
/// 奪取は「mtime が 10 秒より古い」ことが前提なので実運用では起きない。
///
/// **mtime を更新するスレッドは持たない。** ここでの保持は小さなファイル 1 本の
/// 読み書き（ミリ秒）で、stale 閾値 10 秒に対して十分短い。仮に環境要因
/// （ウイルス対策のスキャン・スリープ復帰）で 10 秒を超えて奪われても、
/// 上の所有権確認があるので他者のロックを消すことはなく、こちらの書き込みが
/// 失敗するだけで済む（touch スレッドを足しても「奪われた後に消す」経路は
/// 消えないので、守りとしては所有権確認の方が単純かつ確実）
#[derive(Debug)] // 取れなかったことをテストで `expect_err` するため
pub struct Lock {
    path: PathBuf,
    /// 取得した瞬間の mtime＝所有権の印。取れなかった（None）ときは所有を
    /// 証明できないので解放しない（stale 化して誰かが奪うのに任せる。
    /// 他者のロックを消す危険より、10 秒待たせる方が軽い）
    mtime: Option<std::time::SystemTime>,
}

impl Lock {
    /// `wait` まで待って取る。`stale` より古いロックは奪う
    pub fn acquire(path: &Path, wait: Duration, stale: Duration) -> anyhow::Result<Self> {
        // ロックの置き場所が無いと mkdir は必ず失敗する。`~/.ccdesk` 配下の
        // ロックは初回起動（そのディレクトリがまだ無い）で実際にこの状況になる
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let deadline = Instant::now() + wait;
        let mut steals = 0;
        loop {
            match std::fs::create_dir(path) {
                Ok(()) => {
                    return Ok(Self {
                        path: path.to_path_buf(),
                        mtime: lock_mtime(path),
                    })
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => {
                    return Err(anyhow!(
                        "could not create the lock at {}: {e}",
                        path.display()
                    ))
                }
            }
            // 死んだ保持者のロックは奪う。奪えたら即座に取り直しへ戻る
            // （待ち時間を消費させない: 誰も生きていないのだから待つ理由が無い）
            let stolen = steals < LOCK_MAX_STEALS
                && lock_age(path).is_some_and(|age| age >= stale)
                && std::fs::remove_dir(path).is_ok();
            if stolen {
                steals += 1;
                continue;
            }
            if Instant::now() >= deadline {
                // **打つ手まで書く。** 時計の巻き戻し・スリープ復帰・ネットワーク
                // ドライブの skew でロックの mtime が未来に付くと [`lock_age`] は
                // 永久に stale と判定しないので、"try again" は何度やっても通らない。
                // ロックの実体が空ディレクトリで、保持者が居なければ消してよいことは
                // ここでしか伝わらない（未ログイン行が `run /login` まで書くのと同じ方針）
                return Err(anyhow!(
                    "another process is holding the lock at {}; \
                     if no claude session and no other ccdesk window is running, \
                     this leftover lock is an empty directory and can be deleted",
                    path.display()
                ));
            }
            std::thread::sleep(LOCK_RETRY);
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // 所有権の確認（[`Lock`] の解説を参照）。奪われている・確認できないなら
        // 何もしない。ここで無条件に消すと他者のロックを外すことになる
        if self.mtime.is_some() && lock_mtime(&self.path) == self.mtime {
            let _ = std::fs::remove_dir(&self.path);
        }
    }
}

/// ロックの mtime（所有権の印）。無い・読めないときは None
fn lock_mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// ロックの経過時間。mtime が未来のとき（時刻のずれ）は None＝stale ではない扱い。
///
/// **未来の mtime を「十分古い」側に倒さない。** 未来に付くのは保持者の時計ではなく
/// ファイルシステム側の時刻がずれているときで、そうなると経過時間そのものが
/// 信用できない ＝ 生きている claude のロックを奪う判断材料にはできない。
/// 代わりに、取得できなかったときのエラー文が「消してよい」ことを案内する
/// （[`Lock::acquire`]）
fn lock_age(path: &Path) -> Option<Duration> {
    lock_mtime(path)?.elapsed().ok()
}


/// `<target>.<pid>-<連番>.tmp` の形か（[`write_json_atomically`] が付ける名前）。
/// pid と連番の形まで見るのは、無関係な `.tmp`（claude や他ツールのもの）を
/// 消さないため
pub fn is_leftover_tmp(name: &str, target: &str) -> bool {
    let Some(rest) = name.strip_prefix(&format!("{target}.")) else {
        return false;
    };
    let Some((pid, seq)) = rest.strip_suffix(".tmp").and_then(|m| m.split_once('-')) else {
        return false;
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    digits(pid) && digits(seq)
}


/// ファイルを読んで JSON にする。**無い・読めない・壊れている・書き換え途中は
/// すべて None**（呼び手は既定値で先へ進む）。この寛容さは共有ファイルの読み全部に
/// 共通の契約なので、実装をここ 1 箇所に持つ ＝ 1 箇所だけ厳格に変わって
/// 「起動が止まる」側へ倒れることがない
pub fn read_json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// ファイルの「見え方」（長さ + mtime）。**中身を読まずに「変わったか」だけを
/// 見る指紋**で、変化検出（hook-states・認証情報・transcript）が同じ型を使う。
/// 無い・読めないときは None（「消えた」も変化として検出できる）
pub fn file_stamp(path: &Path) -> Option<(u64, std::time::SystemTime)> {
    let md = std::fs::metadata(path).ok()?;
    Some((md.len(), md.modified().ok()?))
}

/// Mutex のポイズン回復。**「パニックしたスレッドが居ても値は使い続ける」方針を
/// ここ 1 箇所に持つ**（呼び出し約 40 箇所が同じ長い式を書き写さない ＝
/// 1 箇所だけ `unwrap()` に戻って方針が黙って割れることがない）
pub trait LockExt<T> {
    fn lock_recover(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> LockExt<T> for std::sync::Mutex<T> {
    fn lock_recover(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// tmp → rename で置く（読み手が書きかけの JSON を見ないため）。
/// tmp は同じディレクトリに作る（別ボリュームだと rename が失敗する）。
/// 名前は pid + 連番で一意にする（同じパスへの同時書き込みで tmp を共有しない）。
/// **rename 前に取り残された tmp は起動時に回収する**（[`reap_leftover_tmp`]）
pub fn write_json_atomically(path: &Path, value: &serde_json::Value) -> anyhow::Result<()> {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| anyhow!("could not create {}: {e}", dir.display()))?;
    }
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}-{seq}.tmp", std::process::id()));
    let tmp = path.with_file_name(name);
    let text = serde_json::to_string_pretty(value)?;
    // **rename の前に中身をディスクへ確定させる。** rename 自体は NTFS の
    // メタデータジャーナルで守られるが、tmp の中身は守られない。電源断で
    // 0 バイトの `sessions.json` が残ると、一覧が丸ごと飛ぶ。
    // 小さなファイル 1 本なので代償は小さい
    if let Err(e) = write_and_sync(&tmp, text.as_bytes()) {
        let _ = std::fs::remove_file(&tmp); // 中間ファイルを残さない
        return Err(anyhow!("could not write {}: {e}", tmp.display()));
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        anyhow!("could not replace {}: {e}", path.display())
    })
}

/// 書いて fsync する（[`write_json_atomically`] 用）
fn write_and_sync(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// 回収してよい `.tmp` の古さ（[`reap_leftover_tmp`]）。
/// 書いている最中の別インスタンスの tmp を消さないため、十分に古いものだけを消す
pub const TMP_KEEP: Duration = Duration::from_secs(3600);

/// `target` の書きかけ `.tmp` を回収する。**[`write_json_atomically`] が
/// rename する前にプロセスが死ぬと、その tmp は誰にも消されずに残る**。
/// `update::cleanup_old_exe` と同じ
/// 「次にプロセスを起こしたときに片付ける」方式。
///
/// **tmp の名前を決めるのと同じ場所に置いてある**のが要点で、名前の形
/// （[`is_leftover_tmp`]）を知らずに回収はできない ＝ 書き手ごとに回収を書くと
/// 名前の規則が 2 通りに分かれる。消すのは自分たちが付ける形の名前で、かつ
/// 十分に古いもの（[`TMP_KEEP`]）だけ: 今まさに書いている別インスタンスの tmp や
/// 無関係な `.tmp` を消さないため。失敗は無視する（掃除は次の起動でまた来る）
pub fn reap_leftover_tmp(target: &Path) {
    let (Some(dir), Some(name)) = (target.parent(), target.file_name().and_then(|n| n.to_str()))
    else {
        return;
    };
    reap_tmp_in(dir, &[name], Some(TMP_KEEP));
}

/// 回収の実体: `dir` を 1 度だけ列挙し、`targets` のいずれかの書きかけ `.tmp` を消す。
/// `min_age` が Some のときは十分に古いものだけを消す（今まさに書いている
/// 別インスタンスの tmp を消さないため）。None は無条件（書く者が居ない対象専用）
fn reap_tmp_in(dir: &Path, targets: &[&str], min_age: Option<Duration>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(file) = file_name.to_str() else {
            continue;
        };
        if !targets.iter().any(|target| is_leftover_tmp(file, target)) {
            continue;
        }
        let old = min_age.is_none_or(|keep| {
            entry
                .metadata()
                .and_then(|md| md.modified())
                .ok()
                .and_then(|m| m.elapsed().ok())
                .is_some_and(|age| age >= keep)
        });
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// 起動時の掃除: `~/.ccdesk` 配下の**全書き手**（ウィンドウ状態・設定・セッション
/// 一覧・hook の受け渡し）の書きかけ `.tmp` を、**1 回の走査**でまとめて回収する。
/// 対象は各書き手のパス関数から導く（名前を書き写すと、置き場を変えたときに
/// 片方だけ古い名前を掃除し続ける）
pub fn reap_startup_leftovers() {
    let Some(dir) = ccdesk_dir() else { return };
    let names: Vec<String> = [
        state_path(),
        settings_path(),
        sessions_store_path(),
        hook_states_path(),
    ]
    .into_iter()
    .flatten()
    .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_string))
    .collect();
    let targets: Vec<&str> = names.iter().map(String::as_str).collect();
    reap_tmp_in(&dir, &targets, Some(TMP_KEEP));
}

/// kv_save の read-modify-write を直列化するプロセス内ロック
/// （UI スレッドとディスパッチスレッドの同時書込みでキーが消えるのを防ぐ）
static KV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn kv_load(path: Option<std::path::PathBuf>, key: &str) -> Option<String> {
    let v = read_json(&path?)?;
    Some(v.get(key)?.as_str()?.to_string())
}

/// 文字列配列の読み取り。**読みは寛容**で、値の形が想定と違えば「保存が無い」ものとして
/// 扱う（ファイル無し・壊れた JSON・オブジェクトでない・キー無し・配列でない・
/// 要素が文字列でない）。state.json はユーザーが手で直す想定のファイルではないので、
/// 壊れていたら起動を止めるより既定値で先へ進むのが唯一の親切な選択になる
fn kv_load_list(path: Option<std::path::PathBuf>, key: &str) -> Vec<String> {
    let Some(v) = path.as_deref().and_then(read_json) else {
        return Vec::new();
    };
    value_strings(v.get(key))
}

/// JSON 値を文字列配列として読む（配列でない / 要素が文字列でない分は捨てる）。
/// 読みとマージの両方がここを通るので、寛容さの範囲が 2 通りにならない
fn value_strings(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| e.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// state.json / config.json の read-modify-write を守る advisory lock の待ち。
///
/// **[`KV_LOCK`] では足りない。** あちらはプロセス内 Mutex なので、ccdesk を
/// 複数起動していると「A が読む → B が読む → A が書く → B が書く」を許す
/// ＝ A の変更が消える。それは `update_state_list` の戻り値
/// （[`crate::update_state_list`]）が防いだはずの状態そのもので、A は「書けた」と
/// 記録したのにディスクには B の内容が乗る ＝ **次の保存で外したフォルダが
/// 「他インスタンスの登録」と分類されて復活する**。
///
/// 待ちが短いのは **UI スレッドから呼ばれる**ため（サイドバー幅・見出しの登録・
/// 最後に見た画面）。守る区間は小さなファイル 1 本の読み書きだけで、競合していても
/// 数 ms で空くので、フリーズとして感じられる長さを待つ理由が無い。取れなければ
/// 諦めて `false` を返す（呼び手は基準を進めない ＝ 次の保存でもう一度載せに行く）
const KV_LOCK_WAIT: Duration = Duration::from_millis(500);

/// キー 1 つの read-modify-write。**書き込みの作法（ロック・オブジェクト以外の
/// 上書き・原子的な置き換え）をここ 1 箇所に持つ**ので、値が文字列でも配列でも
/// 同じ保証になる。
///
/// `edit` は**同じロックの下でディスク上の今の値**を受け取る（読みと書きの間に
/// 別の書き込みを挟ませないため）＝ 呼び手は「今の値の上に載せる」判断ができる。
/// ロックは 2 段で、プロセス内（[`KV_LOCK`]）と**インスタンス間**
/// （[`KV_LOCK_WAIT`]）の両方を閉じる: 前者だけでは多重起動で読み書きが交差する。
///
/// 置き換えは [`write_json_atomically`]（他の書き手と同じ 1 実装）。
/// **自前で tmp を書かない**のが要点で、以前ここは全インスタンス共通の
/// `state.json.tmp` を使っており、同時保存で他インスタンスの tmp を rename して
/// 「成功したのに自分の内容が乗っていない」状態を作れた。
///
/// **戻り値は「ディスクへ載ったか」。** ロックの取得・tmp 書き込み・rename は
/// 失敗しうる（他インスタンス・ディスク満杯・権限・ウイルス対策のロック）。
/// 黙って捨てると、呼び手が「こう書いた」と記録したのに実際は書かれていない状態に
/// なり、次の書き込みの判断材料が嘘になる
/// （[`crate::update_state_list`] の呼び手が持つマージの基準）
#[must_use]
fn kv_edit(
    path: Option<std::path::PathBuf>,
    key: &str,
    edit: impl FnOnce(Option<&serde_json::Value>) -> serde_json::Value,
) -> bool {
    let Some(path) = path else { return false };
    let _guard = KV_LOCK.lock_recover();
    // 別インスタンスとの直列化。**ファイルごとに別のロック**（state.json と
    // config.json は別物で、片方の保存が他方を待つ理由が無い）
    let Ok(_shared) = Lock::acquire(&lock_path_for(&path), KV_LOCK_WAIT, LOCK_STALE) else {
        return false;
    };
    let mut v = read_json(&path).unwrap_or_else(|| serde_json::json!({}));
    if !v.is_object() {
        v = serde_json::json!({});
    }
    v[key] = edit(v.get(key));
    write_json_atomically(&path, &v).is_ok()
}

fn kv_save(path: Option<std::path::PathBuf>, key: &str, value: &str) {
    // 単値（サイドバー幅・最後に開いた画面）は失敗しても次の保存で上書きされる
    // ＝ 呼び手に判断させるものが無いので、成否は返さない
    let _ = kv_edit(path, key, |_| {
        serde_json::Value::String(value.to_string())
    });
}

#[must_use]
fn kv_update_list(
    path: Option<std::path::PathBuf>,
    key: &str,
    merge: impl FnOnce(Vec<String>) -> Vec<String>,
) -> bool {
    kv_edit(path, key, |current| {
        serde_json::Value::Array(
            merge(value_strings(current))
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        )
    })
}

/// 起動時にまとめて読むためのスナップショット。**1 度だけ読み、以降のキー引きは
/// メモリ上**で行う（`load_state` / `load_setting` はキーごとにファイルを
/// 読み直すので、起動列で 5 キー引くと同じファイルを 5 回読む）。
/// 値の解釈（文字列・文字列配列）は単発読みと同じ寛容さ
pub struct KvSnapshot(serde_json::Value);

impl KvSnapshot {
    pub fn string(&self, key: &str) -> Option<String> {
        Some(self.0.get(key)?.as_str()?.to_string())
    }

    pub fn list(&self, key: &str) -> Vec<String> {
        value_strings(self.0.get(key))
    }
}

/// state.json のスナップショット（[`KvSnapshot`]）
pub fn state_snapshot() -> KvSnapshot {
    kv_snapshot(state_path())
}

/// config.json のスナップショット（[`KvSnapshot`]）
pub fn settings_snapshot() -> KvSnapshot {
    kv_snapshot(settings_path())
}

fn kv_snapshot(path: Option<std::path::PathBuf>) -> KvSnapshot {
    KvSnapshot(
        path.as_deref()
            .and_then(read_json)
            .unwrap_or_else(|| serde_json::json!({})),
    )
}

pub fn load_setting(key: &str) -> Option<String> {
    kv_load(settings_path(), key)
}

pub fn save_setting(key: &str, value: &str) {
    kv_save(settings_path(), key, value);
}

pub fn load_state(key: &str) -> Option<String> {
    kv_load(state_path(), key)
}

pub fn save_state(key: &str, value: &str) {
    kv_save(state_path(), key, value);
}

/// 複数値の状態（プロジェクト一覧）。単値と同じ state.json に、同じ書き込み作法で置く
/// ＝ 保存経路を増やさない
pub fn load_state_list(key: &str) -> Vec<String> {
    kv_load_list(state_path(), key)
}

/// 配列の状態を**ディスク上の今の値から**作り直す。`merge` はディスクの一覧を受けて
/// 保存する一覧を返す。
///
/// **全量で上書きする書き方を持たない**のが要点: ccdesk は複数起動でき state.json は
/// 共有なので、メモリ上の写しをそのまま書くと、その間に別のインスタンスが足した
/// 要素が消える（[`kv_edit`] は他のキーは保つが、同じキーへの同時編集は保たない）。
/// 単値（サイドバー幅など）も後勝ちだが、あちらは設定でこちらはユーザーのデータなので
/// 黙って捨ててはいけない。マージの意味論は呼び手（保存する値の持ち主）が決める。
///
/// **戻り値は「ディスクへ載ったか」**（[`kv_edit`]）。呼び手は次のマージの基準を
/// これで進めるかどうかを決める ＝ 書けていないのに「こう書いた」と記録すると、
/// 外したはずの要素が次の保存で復活する
#[must_use]
pub fn update_state_list(key: &str, merge: impl FnOnce(Vec<String>) -> Vec<String>) -> bool {
    kv_update_list(state_path(), key, merge)
}

/// 2 つのパスが同じフォルダを指すか。**「同じフォルダか」の判断はここ 1 箇所だけ**に置く
/// （登録リストの重複排除・登録解除の対象照合・セッション行をどの見出しへ入れるかの
/// 振り分けが別々の答えを出すと、見出しが 2 つに割れたり登録解除が空振りする）。
///
/// 大小・区切りの種類・区切りの重複を無視するのは、突き合わせる文字列の出自が違うため:
/// 登録リストは ccdesk が保存した文字列、セッションの cwd は claude が記録した文字列、
/// 新規セッションのフォルダはユーザーが打った文字列。Windows 専用ツールなので
/// `C:\dev\api` と `c:/dev/api\` は同じフォルダであり、別扱いにする理由が無い
/// （`/` も Windows の正当な区切りで、フォルダ欄に打ち込める）。
///
/// 正規化はこの範囲に留める（`canonicalize` を使わない）: 実在しないフォルダも
/// 突き合わせの対象になるうえ、ディスクを触る比較を描画のたびに走らせられない。
///
/// **1 対 1 の照合はこれで、総当たりになる場所は [`dir_key`] を使う**
/// （キーを作り直さずに持ち回るため。判定の中身はどちらも dir_key 1 箇所）
pub fn same_dir(a: &str, b: &str) -> bool {
    dir_key(a) == dir_key(b)
}

/// [`same_dir`] の比較キー。**同じフォルダかの判断の実体はこの関数だけ**が持つので、
/// 総当たりの照合（描画のたびに走る見出しの重複排除・セッション行の振り分け）は
/// パスごとにこのキーを 1 度だけ作って持ち回る ＝ 比較のたびに文字列を作らない。
///
/// `C:\` のようなルートは末尾の区切りを落とさない（落とすと
/// ドライブ指定 `C:` になり、Windows では「そのドライブのカレント」を指す別物になる）。
///
/// **連続する区切りは末尾でも内部でも 1 個へ畳む**（`C:\\` `C:\dev\\api` は
/// `C:\` `C:\dev\api` と同じキー）。素朴な join・貼り付け・ドラッグ&ドロップで
/// 入ってくる形で、Windows はこれを同じフォルダとして開く（`Path::is_dir` も通る ＝
/// 新規セッション画面の存在確認では弾かれない）。別扱いにすると見出しが 2 つに割れて
/// 登録解除も空振りする ＝ [`same_dir`] が 1 箇所で持つ不変条件が崩れる。
/// 区切りが元から無い `C:` は丸めない ＝ ドライブ指定はドライブ直下と別物のまま。
///
/// **例外は先頭の `\\`（UNC = `\\server\share`）**: Windows では区切りの重複ではなく
/// 「ネットワーク上」を表す記号なので、1 個に畳むと `\server\share`
/// ＝ ローカルのルート直下の別フォルダを指してしまう。記号として働くのは後ろに
/// サーバー名が続くときだけなので、続かない `\\` は畳めるルート表記として扱う
pub fn dir_key(path: &str) -> String {
    let unified = path.replace('/', "\\").to_lowercase();
    let body = unified.trim_start_matches('\\');
    // 先頭の区切りだけは畳み方が違う（UNC は 2 個で意味を持つ）ので、本体と分けて持つ
    let head = match unified.len() - body.len() {
        0 => "",
        // 2 個以上でもサーバー名が続かなければ UNC ではない ＝ ルートとして 1 個へ
        1 => "\\",
        _ if body.is_empty() => "\\",
        _ => "\\\\",
    };
    if body.is_empty() {
        // ルートだけの表記（`\` `\\`）。空入力はそのまま空のキーになる
        return head.to_string();
    }
    let mut collapsed = String::with_capacity(body.len());
    for ch in body.chars() {
        if ch == '\\' && collapsed.ends_with('\\') {
            continue;
        }
        collapsed.push(ch);
    }
    // 畳んだ後の末尾の区切りは 1 個以下。trimmed は collapsed の先頭からの部分なので、
    // 長さの差が「末尾に区切りがあった」ことを表す
    let trimmed = collapsed.trim_end_matches('\\');
    if trimmed.ends_with(':') && trimmed.len() < collapsed.len() {
        // ドライブ直下は区切りを落とさない（上記のとおり `C:` は別物）
        format!("{head}{trimmed}\\")
    } else {
        format!("{head}{trimmed}")
    }
}

// 注: 旧実装（~/.claude/sessions レジストリ読み・roster.json・JSONL transcript パース・
// プロセス親子関係の遡り）は監査指摘により削除した。ライブ状態は正規の
// `claude agents --json` を唯一のソースとする。

#[cfg(test)]
mod tests {
    use super::*;

    /// **テストは実ユーザーの `~/.ccdesk/error.log` へ 1 バイトも書かない。**
    ///
    /// ログの出力先はプロセス全体で 1 つの隠れたグローバルなので、注入で受けると
    /// 呼び出し口の数だけ渡し忘れが作れる。代わりに「起動時に有効化する」形にしてある
    /// （[`enable_error_log`] を呼ぶのは `main` だけ）＝ テストの実行ファイルは
    /// 構造上そこへ到達しない。実際、これが無かった頃は一時ディレクトリのパスを含む
    /// 失敗が `cargo test` のたびに実ログへ溜まっていた
    #[test]
    fn logging_writes_nothing_until_it_is_enabled() {
        assert!(
            !ERROR_LOG_ENABLED.load(Ordering::Relaxed),
            "a test enabled the error log — the real ~/.ccdesk/error.log is now in play"
        );
        let size = || error_log_path().and_then(|p| std::fs::metadata(p).ok()).map(|m| m.len());
        let before = size();
        log_error("this line must never reach the user's log");
        assert_eq!(size(), before, "wrote to the real error log from a test");
    }

    /// テスト専用の JSON ファイル。~/.ccdesk は触らない（開発者の state.json を踏まない）
    fn temp_json(name: &str, contents: Option<&str>) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ccdesk-test-{}-{name}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("state.json");
        match contents {
            Some(text) => std::fs::write(&path, text).expect("failed to write test file"),
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
        path
    }

    /// **撤去したアカウント切り替えの名残を、起動時に一式まとめて消す。**
    ///
    /// 消すのは保管ファイル本体だけでは足りない: 書きかけの `.tmp` にも
    /// **同じトークンが入っている**ので、本体だけ消しても隣に残り続ける。
    /// ロック（ディレクトリ）も、誰も取らなくなった以上ただのごみになる。
    ///
    /// **実ユーザーの `~/.ccdesk` は触らない**（パスを引数で受ける形にしてある理由）
    #[test]
    fn purging_the_account_store_takes_its_lock_and_tmp_files_with_it() {
        let dir = std::env::temp_dir().join(format!(
            "ccdesk-test-{}-account-store-purge",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = dir.join(ACCOUNT_STORE);

        // 何も無ければ何もしない（＝ ログを出す理由が無い）
        assert!(!remove_account_store(&store), "reported a removal that did not happen");

        std::fs::write(&store, "{\"accounts\": {}}").unwrap();
        std::fs::create_dir_all(lock_path_for(&store)).unwrap();
        // tmp 名は書き手が付ける形をそのまま使う（名前の規則を書き写さない）
        let tmp = dir.join(format!("{ACCOUNT_STORE}.{}-0.tmp", std::process::id()));
        std::fs::write(&tmp, "{}").unwrap();
        // 無関係なファイルは巻き込まない
        let other = dir.join("sessions.json");
        std::fs::write(&other, "{}").unwrap();

        assert!(remove_account_store(&store), "did not report removing the store");
        assert!(!store.exists(), "the account store is still there");
        assert!(!lock_path_for(&store).exists(), "the lock directory is still there");
        assert!(!tmp.exists(), "a tmp file with tokens in it is still there");
        assert!(other.exists(), "removed a file that is not the account store");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 壊れた / 古い形式の state.json でも読みは失敗しない（＝起動が止まらない）。
    /// 保存形式を足したときに一番壊れやすいのがここなので、想定外の形を並べて固定する
    #[test]
    fn state_reads_tolerate_missing_broken_and_legacy_shapes() {
        let cases = [
            ("missing", None),
            ("empty", Some("")),
            ("broken", Some("{\"projects\": [\"C:\\\\dev\\\\a\"")), // 閉じ括弧なし
            ("not-object", Some("[1, 2, 3]")),
            ("no-key", Some("{\"sidebar_width\": \"33\"}")),
            // 旧形式: 単値しか無かった頃の state.json に配列キーが無い / 型が違う
            ("legacy-string", Some("{\"projects\": \"C:\\\\dev\\\\a\"}")),
            ("mixed-array", Some("{\"projects\": [1, null, {}]}")),
        ];
        for (name, contents) in cases {
            let path = temp_json(name, contents);
            assert!(
                kv_load_list(Some(path.clone()), "projects").is_empty(),
                "{name}: list read was not empty"
            );
            // 単値読みも同じ寛容さ。旧形式（文字列が入っていた頃）だけは値として読めるので
            // ケースを分ける（ケース名で例外を作ると何を保証しているのか読めなくなる）
            let single = kv_load(Some(path), "projects");
            if name == "legacy-string" {
                assert_eq!(single.as_deref(), Some("C:\\dev\\a"), "legacy single value not readable");
            } else {
                assert!(single.is_none(), "{name}: single-value read was not None");
            }
        }
        // 配列の中に文字列と非文字列が混ざっていたら、文字列だけを拾う
        let path = temp_json("partial-array", Some("{\"projects\": [\"C:\\\\dev\\\\a\", 7]}"));
        assert_eq!(kv_load_list(Some(path), "projects"), ["C:\\dev\\a"]);
    }

    /// 書きは単値・配列が同じファイルに共存し、読み直しても他のキーを壊さない。
    /// 旧形式の単値が入っていたキーを配列で上書きしても読めることまで含める
    #[test]
    fn state_writes_keep_other_keys_and_round_trip() {
        let path = temp_json("round-trip", Some("{\"projects\": \"legacy\"}"));
        let some = |p: &std::path::PathBuf| Some(p.clone());
        let write = |p: &std::path::PathBuf, values: &[String]| {
            assert!(
                kv_update_list(some(p), "projects", |_| values.to_vec()),
                "did not report success"
            );
        };
        kv_save(some(&path), "sidebar_width", "33");
        let projects = vec!["C:\\dev\\a".to_string(), "C:\\dev\\b".to_string()];
        write(&path, &projects);
        assert_eq!(kv_load(some(&path), "sidebar_width").as_deref(), Some("33"));
        assert_eq!(kv_load_list(some(&path), "projects"), projects);
        // 空配列も「0 件」として保存できる（キーごと消す実装だと未保存と区別できない）
        write(&path, &[]);
        assert!(kv_load_list(some(&path), "projects").is_empty());
        assert_eq!(
            kv_load(some(&path), "sidebar_width").as_deref(),
            Some("33"),
            "writing the array erased other keys"
        );
        // オブジェクトでないファイルは作り直す（壊れた state.json で保存が死なない）
        let broken = temp_json("write-over-broken", Some("[1,2,3]"));
        write(&broken, &projects);
        assert_eq!(kv_load_list(broken.clone().into(), "projects"), projects);
    }

    /// 配列の書きは**ディスク上の今の値を受け取ってから**新しい値を作る
    /// （別のインスタンスが書いた要素の上に載せられる ＝ マージの前提）。
    /// 想定外の形（旧形式の単値・配列でない）が入っていても空の一覧として渡す
    #[test]
    fn list_writes_see_the_value_on_disk_first() {
        let path = temp_json("merge-base", Some("{\"projects\": [\"C:\\\\dev\\\\disk\"]}"));
        let some = || Some(path.clone());
        let mut seen = Vec::new();
        let wrote = kv_update_list(some(), "projects", |disk| {
            seen = disk.clone();
            let mut next = disk;
            next.push("C:\\dev\\mine".to_string());
            next
        });
        assert!(wrote, "did not report success");
        assert_eq!(seen, ["C:\\dev\\disk"], "value on disk was not passed through");
        assert_eq!(
            kv_load_list(some(), "projects"),
            ["C:\\dev\\disk", "C:\\dev\\mine"]
        );
        // 配列でない値は「保存が無い」扱い（読みと同じ寛容さ）
        let legacy = temp_json("merge-legacy", Some("{\"projects\": \"legacy\"}"));
        let mut seen_legacy = vec!["dirty".to_string()];
        assert!(
            kv_update_list(Some(legacy.clone()), "projects", |disk| {
                seen_legacy = disk;
                Vec::new()
            }),
            "did not report success"
        );
        assert!(seen_legacy.is_empty(), "legacy single value was passed through as a list");
    }

    /// **書けなかったことを黙って飲まない。** tmp 書き込み / rename は失敗しうる
    /// （ディスク満杯・権限・ウイルス対策のロック）。呼び手はこの戻り値で
    /// 「こう書いた」と記録するかどうかを決めるので、失敗を成功と報告すると
    /// 外したはずの要素が次の保存で復活する（[`crate::update_state_list`]）
    #[test]
    fn a_write_that_cannot_land_reports_failure() {
        // 書けないパス ＝ **保存先がディレクトリ**（作成そのものが失敗する）。
        // 「親ディレクトリが無いパス」では書けてしまう: 保存は置き場所を作ってから
        // 書く（`ccdesk_dir` / `Lock::acquire` / `write_json_atomically` のいずれも
        // 先に `create_dir_all` する ＝ 初回起動で ~/.ccdesk が無くても保存できる）
        let blocked = temp_json("blocked", None).with_file_name("blocked-dir.json");
        std::fs::create_dir_all(&blocked).unwrap();
        assert!(
            !kv_update_list(Some(blocked.clone()), "projects", |_| vec![
                "C:\\dev\\a".to_string()
            ]),
            "reported success despite failing to write"
        );
        assert!(blocked.is_dir(), "the write-failure precondition is broken");
        // 置き場所そのものが分からないとき（ホームが取れない）も同じ
        assert!(!kv_update_list(None, "projects", |_| Vec::new()));
        // 書きかけの tmp を残さない（次の読み手が中途の JSON を拾わない）。
        // tmp 名はインスタンスごとに一意なので、名前を組み立てずに走査で見る
        let path = temp_json("tmp-cleanup", Some("{}"));
        assert!(kv_update_list(Some(path.clone()), "projects", |_| vec![
            "C:\\dev\\a".to_string()
        ]));
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "tmp files left behind: {leftovers:?}");
    }

    /// **他インスタンスの書きかけ tmp と名前を共有しない。**
    ///
    /// tmp が全インスタンス共通（`state.json.tmp`）だと、同時保存で
    /// 「A が tmp を書く → B が同じ tmp を上書き → A が rename して成功を返す」
    /// が成立し、**成功を返した A の内容ではなく B の内容がディスクに乗る**。
    /// 他の書き手は pid + 連番の tmp 名でこれを塞いでいたのに、
    /// こちらは塞いでいなかった ＝ 同じ危険に対策が 2 通りあった。
    ///
    /// 状況は **共有を許さないハンドル**で作る: 別インスタンスが tmp を掴んでいる間、
    /// 名前を共有していればこちらの書き込みは共有違反で失敗する
    #[test]
    fn a_save_does_not_share_its_tmp_with_another_instance() {
        use std::os::windows::fs::OpenOptionsExt;

        let path = temp_json("tmp-collision", Some("{}"));
        // 修正前の tmp 名（全インスタンス共通）を別インスタンスが掴んでいる状態
        let shared = path.with_extension("json.tmp");
        let held = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .share_mode(0) // 読みも書きも共有しない
            .open(&shared)
            .unwrap();

        let wrote = kv_update_list(Some(path.clone()), "projects", |_| {
            vec!["C:\\dev\\mine".to_string()]
        });
        drop(held);
        let _ = std::fs::remove_file(&shared);

        assert!(
            wrote,
            "sharing tmp name with another instance blocked our own save"
        );
        assert_eq!(
            kv_load_list(Some(path), "projects"),
            ["C:\\dev\\mine"],
            "reported success but our content did not land on disk"
        );
    }

    /// **別インスタンスが保存中なら諦める（読みと書きを交差させない）。**
    ///
    /// [`KV_LOCK`] はプロセス内 Mutex なので、多重起動では
    /// 「A が読む → B が読む → A が書く → B が書く」を許す ＝ A は成功を返したのに
    /// ディスクには A の変更を含まない B の内容が乗り、A は自分のマージ結果を
    /// 基準として記録する。次の保存でその差が「他インスタンスの登録」と分類され、
    /// **外したフォルダが復活する**（[`crate::update_state_list`] が防ぐはずの状態）。
    ///
    /// 待ちが有界であることも同時に見る（保存は UI スレッドから呼ばれる）
    #[test]
    fn a_save_gives_up_while_another_instance_is_writing() {
        let path = temp_json("kv-lock", Some("{\"projects\": [\"C:\\\\dev\\\\disk\"]}"));
        let before = std::fs::read(&path).unwrap();

        let held = Lock::acquire(&lock_path_for(&path), Duration::ZERO, LOCK_STALE).unwrap();
        let started = Instant::now();
        let wrote = kv_update_list(Some(path.clone()), "projects", |_| {
            vec!["C:\\dev\\mine".to_string()]
        });
        let waited = started.elapsed();

        assert!(!wrote, "wrote while another instance was writing");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "changed the disk by crossing with another instance's write"
        );
        assert!(
            waited < KV_LOCK_WAIT * 2,
            "UI thread kept waiting ({waited:?})"
        );

        // 解放後は通常どおり書けて、**その内容がディスクに乗る**
        // （ロックが理由で保存が死んだままにならない）
        drop(held);
        assert!(
            kv_update_list(Some(path.clone()), "projects", |disk| {
                assert_eq!(disk, ["C:\\dev\\disk"], "value read under the lock is stale");
                vec!["C:\\dev\\mine".to_string()]
            }),
            "cannot write even after release"
        );
        assert_eq!(kv_load_list(Some(path), "projects"), ["C:\\dev\\mine"]);
    }

    /// ロック検査用のパス（実ユーザーのホームは触らない）。
    /// 前回の残りがあれば消してから返す（Drop で消すのはロック自身の仕事）
    fn temp_lock_path(name: &str) -> std::path::PathBuf {
        let path = temp_json(name, None).with_file_name(format!("{name}.lock"));
        let _ = std::fs::remove_dir(&path);
        path
    }

    /// 他者がロックを保持していたら待つ（claude 側も 1〜2 秒のジッタ付きで
    /// 5 回リトライするので、短時間の保持は協調的に待ち合わせられる）
    #[test]
    fn acquire_waits_until_the_holder_releases() {
        let path = temp_lock_path("acquire_waits_until_the_holder_releases");
        let held = Lock::acquire(&path, Duration::ZERO, LOCK_STALE).unwrap();
        let holder = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(held);
        });

        let started = Instant::now();
        let mine = Lock::acquire(&path, Duration::from_secs(5), LOCK_STALE)
            .expect("failed to acquire even after release");
        assert!(
            started.elapsed() >= Duration::from_millis(150),
            "acquired while still held: {:?}",
            started.elapsed()
        );
        drop(mine);
        holder.join().unwrap();
    }

    /// **奪われた後の Drop で他者のロックを消してはいけない。**
    /// 消すと、奪った側が守っている区間へ第三者が入れる状態になり、
    /// このロック機構が防ごうとしている上書きそのものが起きる
    #[test]
    fn drop_keeps_a_lock_that_another_holder_took_over() {
        let path = temp_lock_path("drop_keeps_a_lock_that_another_holder_took_over");
        let mine = Lock::acquire(&path, Duration::ZERO, LOCK_STALE).unwrap();

        // 自分のロックが stale 判定で奪われた状況（他者の rmdir → mkdir）を作る。
        // 所有権の印は mtime なので、奪い直しが元と同じ刻（Windows のシステム
        // クロックは ~15ms 刻み）に収まると判別できない。実運用では奪取は
        // 取得から 10 秒以上経ってからしか起きないので衝突しないが、
        // テストは同じ刻を踏み得るため mtime が変わるまで作り直す
        let mtime_of = || std::fs::metadata(&path).unwrap().modified().unwrap();
        let mine_mtime = mtime_of();
        for _ in 0..500 {
            std::fs::remove_dir(&path).unwrap();
            std::fs::create_dir(&path).unwrap();
            if mtime_of() != mine_mtime {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_ne!(mtime_of(), mine_mtime, "could not create a takeover (precondition broken)");

        drop(mine);

        assert!(
            path.exists(),
            "Drop after being taken over deleted another holder's lock (leaves claude's token update unprotected)"
        );
    }

    /// stale 閾値より古いロックは奪う（保持者が死んで残った `.lock` で
    /// 永久に書けなくなるのを防ぐ）。閾値を注入して mtime の経過を待たずに固定する
    #[test]
    fn acquire_steals_a_stale_lock_but_not_a_fresh_one() {
        let path = temp_lock_path("acquire_steals_a_stale_lock_but_not_a_fresh_one");
        std::fs::create_dir(&path).unwrap(); // 死んだ保持者が残したロック

        // 新しいロックは奪わない: 有界時間で諦める
        let started = Instant::now();
        assert!(Lock::acquire(&path, Duration::from_millis(50), LOCK_STALE).is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(path.exists(), "deleted another holder's lock despite giving up");

        // mtime が閾値より古ければ奪える
        let stolen = Lock::acquire(&path, Duration::ZERO, Duration::ZERO)
            .expect("failed to steal a stale lock");
        drop(stolen);
        assert!(!path.exists(), "not released");
    }

    /// ロックが取れなかったときのエラーは **打つ手まで言う**。
    /// 時計のずれで mtime が未来に付いたロックは stale 判定に掛からず、
    /// 「もう一度試す」では永久に通らない（[`lock_age`]）。実体が空ディレクトリで
    /// 保持者が居なければ消してよいことは、この文面でしか伝わらない
    #[test]
    fn a_lock_we_cannot_take_says_how_to_recover() {
        let path = temp_lock_path("a_lock_we_cannot_take_says_how_to_recover");
        std::fs::create_dir(&path).unwrap();

        let err = Lock::acquire(&path, Duration::from_millis(20), LOCK_STALE)
            .expect_err("acquired despite being held")
            .to_string();

        assert!(
            err.contains(&path.display().to_string()),
            "does not say which lock: {err}"
        );
        assert!(
            err.contains("empty directory") && err.contains("deleted"),
            "does not say what to do (that it's safe to delete): {err}"
        );
    }

    /// **DECRQM のマウスモード照会は X10(9) と通常(1000) を区別する。**
    /// X10 有効中に 1000 を「有効」と答えると、応答を信じた子は
    /// X10 では送られないボタン解放（release）を待ち続ける
    #[test]
    fn decrqm_distinguishes_x10_from_normal_mouse_mode() {
        let mut parser = new_parser(24, 80, 0);
        parser.process(b"\x1b[?9h\x1b[?9$p\x1b[?1000$p");
        let reply = String::from_utf8(parser.callbacks_mut().take()).unwrap();
        assert!(reply.contains("\x1b[?9;1$y"), "X10 not reported as set: {reply:?}");
        assert!(
            reply.contains("\x1b[?1000;2$y"),
            "mode 1000 reported as set while only X10 is: {reply:?}"
        );

        // 通常モード（1000）を有効化した子には従来どおり「有効」と答える
        let mut parser = new_parser(24, 80, 0);
        parser.process(b"\x1b[?1000h\x1b[?1000$p");
        let reply = String::from_utf8(parser.callbacks_mut().take()).unwrap();
        assert!(reply.contains("\x1b[?1000;1$y"), "mode 1000 not reported as set: {reply:?}");
    }

    /// フォルダの同一判定。**大小と末尾の区切りは無視する**（登録リスト・claude が記録した
    /// cwd・ユーザーの打鍵という出自の違う 3 種類を突き合わせるため）
    #[test]
    fn same_dir_ignores_case_and_trailing_separators() {
        for (a, b) in [
            ("C:\\dev\\api", "c:\\dev\\api"),
            ("C:\\dev\\api", "C:\\dev\\api\\"),
            ("C:\\dev\\api\\", "c:\\DEV\\Api"),
            ("C:\\dev\\api", "C:/dev/api/"), // / も Windows の正当な区切り
            ("C:/dev/api", "c:\\DEV\\api\\"),
            ("C:\\", "c:/"),
        ] {
            assert!(same_dir(a, b), "{a:?} and {b:?} should be the same folder");
        }
        // 別フォルダは別。末端名が同じでも親が違えば別（見出しが混ざってはいけない）
        for (a, b) in [
            ("C:\\dev\\api", "C:\\dev\\api2"),
            ("C:\\work\\api", "C:\\dev\\api"),
            ("C:\\dev\\api", ""),
        ] {
            assert!(!same_dir(a, b), "{a:?} and {b:?} were treated as the same folder");
        }
    }

    /// ドライブ直下は区切りを落とさない。`C:\` を `C:` に丸めると Windows では
    /// 「そのドライブのカレントディレクトリ」を指す別物になる
    #[test]
    fn same_dir_keeps_the_drive_root_separator() {
        assert_eq!(dir_key("C:\\"), "c:\\");
        assert_eq!(dir_key("C:/"), "c:\\", "separator style should not remain in the key");
        assert!(!same_dir("C:\\", "C:"), "treated the drive root and the drive designation as the same");
        // 末尾を落として空になる入力でも panic せず、そのまま比較キーになる
        assert_eq!(dir_key("\\"), "\\");
        assert_eq!(dir_key("/"), "\\");
        assert_eq!(dir_key(""), "");
    }

    /// ルートの区切りが重複した表記（`C:\\`）も同じフォルダ。素朴な join
    /// （`format!("{dir}\\{name}")` に `dir = "C:\"` を渡す等）で作られ得る形で、
    /// 別扱いにすると**見出しが 2 つに割れて登録解除も空振りする** ＝
    /// この関数が 1 箇所で持っているはずの不変条件が崩れる
    #[test]
    fn same_dir_collapses_repeated_root_separators() {
        assert_eq!(dir_key("C:\\\\"), "c:\\");
        assert!(same_dir("C:\\", "C:\\\\"), "duplicated-separator drive root was treated as different");
        assert!(same_dir("C:\\\\", "c://"), "mixing separator style and count was treated as different");
        assert!(same_dir("\\\\", "\\"), "duplicated root separators were treated as different");
        // 区切りが元から無い `C:` はドライブ指定なので、丸めた結果と同一視しない
        assert!(!same_dir("C:", "C:\\\\"), "drive designation was treated as the same as the drive root");
        // 末端まであるパスは従来どおり（重複区切りは落ちる）
        assert_eq!(dir_key("C:\\dev\\api\\\\"), "c:\\dev\\api");
    }

    /// **パスの内部**の重複区切り（`C:\dev\\api`）も同じフォルダ。貼り付けや
    /// ドラッグ&ドロップで入ってくる形で、Windows はこれを同じフォルダとして開く
    /// （`Path::is_dir` も通る ＝ 新規セッション画面の存在確認では弾かれない）。
    /// 別扱いにすると同じフォルダの見出しが 2 つ出て、片方の登録解除が空振りする
    #[test]
    fn same_dir_collapses_repeated_separators_inside_the_path() {
        assert_eq!(dir_key("C:\\dev\\\\api"), "c:\\dev\\api");
        assert_eq!(dir_key("C:\\\\dev\\api"), "c:\\dev\\api");
        assert!(same_dir("C:\\dev\\\\api", "C:\\dev\\api"), "duplicated separators inside the path were treated as different");
        assert!(same_dir("C://dev///api//", "c:\\dev\\api"), "mixing separator style and count was treated as different");
        // 畳むのは区切りだけ。区切りを消して階層を潰したりはしない
        assert!(!same_dir("C:\\dev\\\\api", "C:\\api"), "collapsing duplicated separators erased the parent");
        assert!(!same_dir("C:\\dev\\\\api", "C:\\devapi"), "the separator itself disappeared");
        // ルートの丸めは変えない（区切り 1 個を残す / 区切りが元から無い
        // ドライブ指定は別物のまま ＝ 内部を畳んでも同じ判断になる）
        assert_eq!(dir_key("C://"), "c:\\");
        assert_eq!(dir_key("C:///"), "c:\\");
        assert_eq!(dir_key("C:"), "c:");
    }

    /// UNC（`\\server\share`）の**先頭 2 個は畳まない**。Windows では区切りの重複ではなく
    /// 「ネットワーク上」を表す記号で、1 個に畳むと `\server\share`
    /// ＝ ローカルのルート直下の別フォルダを指してしまう
    #[test]
    fn same_dir_keeps_the_unc_prefix() {
        assert_eq!(dir_key("\\\\server\\share"), "\\\\server\\share");
        assert_eq!(dir_key("//server/share/"), "\\\\server\\share", "separator style and trailing separator are normalized");
        assert!(
            !same_dir("\\\\server\\share", "\\server\\share"),
            "UNC path was treated as the same as a local path"
        );
        assert!(same_dir("\\\\server\\share\\", "//SERVER/share"), "the same UNC path was treated as different");
        // 先頭より後ろの重複は畳む（UNC でも「同じフォルダか」の判断は 1 つ）
        assert!(same_dir("\\\\server\\\\share", "\\\\server\\share"), "duplicated separators inside the UNC path remained");
        // サーバー名が続かない `\\` は UNC の記号として働かないので、ルートとして丸める
        // （既存の `same_dir("\\\\", "\\")` と同じ判断）
        assert_eq!(dir_key("\\\\"), "\\");
        assert_eq!(dir_key("//"), "\\");
    }
}
