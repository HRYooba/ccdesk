//! 端末クエリ応答: claude CLI は起動時に CPR（\x1b[6n）等を送り、応答が無いとブロックする。
//! 本物の端末が返す応答を vt100 の Callbacks で肩代わりし、pending に溜めて PTY へ書き戻す。

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
                        1000 => known(screen.mouse_protocol_mode() == MM::Press
                            || screen.mouse_protocol_mode() == MM::PressRelease),
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
                    let fg = self.host_fg.unwrap_or([0xcccc, 0xcccc, 0xcccc]);
                    self.pending.extend_from_slice(reply(10, fg).as_bytes());
                }
                b"11" => {
                    let bg = self.host_bg.unwrap_or([0x1e1e, 0x1e1e, 0x1e1e]);
                    self.pending.extend_from_slice(reply(11, bg).as_bytes());
                }
                _ => {}
            }
        }
    }
}

/// Responder 付き vt100 パーサ
pub type Parser = vt100::Parser<Responder>;

pub fn new_parser(rows: u16, cols: u16, scrollback: usize) -> Parser {
    vt100::Parser::new_with_callbacks(rows, cols, scrollback, Responder::default())
}

/// Claude Code の設定ディレクトリ。公式に CLAUDE_CONFIG_DIR で移動可能と明記されている
pub fn claude_dir() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Some(std::path::PathBuf::from(dir));
    }
    Some(std::path::PathBuf::from(std::env::var_os("USERPROFILE")?).join(".claude"))
}

/// claude の更新チャネル。settings.json の autoUpdatesChannel（公式に文書化された
/// 設定。"latest"(既定) / "stable"）を読む。CLAUDE_CONFIG_DIR にも追従する
pub fn claude_settings_channel() -> String {
    claude_dir()
        .map(|d| d.join("settings.json"))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
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

/// ~/.claude.json（CLAUDE_CONFIG_DIR 設定時はその配下）
pub fn claude_json_path() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Some(std::path::PathBuf::from(dir).join(".claude.json"));
    }
    Some(std::path::PathBuf::from(std::env::var_os("USERPROFILE")?).join(".claude.json"))
}

/// ~/.claude/jobs/<short>/state.json の読み取り。パス自体は公式ドキュメントに記載があるが
/// **フィールドは非公開の内部形式**。ライブ状態（name/state/生存）は正規の
/// `claude agents --json` を正とし、ここからは正規 IF に存在しない補完情報
/// （要約文・PR 番号・時刻）だけを best-effort で拾う。
/// state.json の無い jobs ディレクトリは pre-warmed worker のため表示対象外。
pub struct BgJob {
    pub short: String,
    pub cwd: String,
    pub state: String, // "working" | "done" | "failed" | "stopped" 等
    pub tempo: String, // "blocked" = Needs input / "idle" 等
    pub name: String,  // Haiku 生成のセッション名（無ければ intent）
    pub needs: String, // Needs input 時の質問サマリー
    pub detail: String, // Working 時の状況説明
    pub result: String, // 完了時の要約
    pub children: Vec<String>, // "#15" 等の PR 表示用
    pub mtime: std::time::SystemTime,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// "2026-07-23T06:42:38.487Z" 形式を epoch ms へ（依存を増やさない最小実装）
pub fn iso_to_epoch_ms(s: &str) -> Option<u64> {
    let bytes = s.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let num = |range: std::ops::Range<usize>| -> Option<i64> {
        s.get(range)?.parse::<i64>().ok()
    };
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let millis = if bytes.len() >= 23 && bytes[19] == b'.' {
        num(20..23)?
    } else {
        0
    };
    // Howard Hinnant の days_from_civil
    let y_adj = if m <= 2 { y - 1 } else { y };
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = y_adj - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let secs = days * 86400 + hh * 3600 + mm * 60 + ss;
    u64::try_from(secs * 1000 + millis).ok()
}

pub fn scan_jobs(limit: usize) -> Vec<BgJob> {
    let mut out = Vec::new();
    let Some(dir) = claude_dir().map(|d| d.join("jobs")) else {
        return out;
    };
    let Ok(dirs) = std::fs::read_dir(&dir) else {
        return out;
    };
    for job_dir in dirs.flatten() {
        let state_path = job_dir.path().join("state.json");
        let Ok(meta) = std::fs::metadata(&state_path) else {
            continue; // spare worker
        };
        let Ok(text) = std::fs::read_to_string(&state_path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let str_of = |key: &str| {
            v.get(key)
                .and_then(|s| s.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let children = v
            .get("children")
            .and_then(|c| c.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|c| c.get("id").and_then(|i| i.as_str()))
                    .map(|id| format!("#{id}"))
                    .collect()
            })
            .unwrap_or_default();
        let name = {
            let n = str_of("name");
            if !n.is_empty() {
                n
            } else {
                str_of("intent").chars().take(40).collect()
            }
        };
        out.push(BgJob {
            short: job_dir.file_name().to_string_lossy().to_string(),
            cwd: str_of("cwd"),
            state: str_of("state"),
            tempo: str_of("tempo"),
            name,
            needs: str_of("needs"),
            detail: str_of("detail"),
            result: v
                .pointer("/output/result")
                .and_then(|s| s.as_str())
                .unwrap_or_default()
                .to_string(),
            children,
            mtime: meta.modified().unwrap_or(std::time::UNIX_EPOCH),
            created_at_ms: v
                .get("createdAt")
                .and_then(|s| s.as_str())
                .and_then(iso_to_epoch_ms)
                .unwrap_or(0),
            updated_at_ms: v
                .get("updatedAt")
                .and_then(|s| s.as_str())
                .and_then(iso_to_epoch_ms)
                .unwrap_or(0),
        });
    }
    out.sort_by_key(|b| std::cmp::Reverse(b.mtime));
    out.truncate(limit);
    out
}

/// ccdesk 自身のデータ置き場 ~/.ccdesk/（config.json と error.log）。無ければ作る。
/// doctor の書き込み可否チェックで参照するため公開する
pub fn ccdesk_dir() -> Option<std::path::PathBuf> {
    let dir = std::path::PathBuf::from(std::env::var_os("USERPROFILE")?).join(".ccdesk");
    std::fs::create_dir_all(&dir).ok()?;
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

/// 使用率キャッシュ ~/.ccdesk/usage.json（statusline フックが書き、TUI が読む）
pub fn usage_cache_path() -> Option<std::path::PathBuf> {
    Some(ccdesk_dir()?.join("usage.json"))
}

/// 複数アカウントの保管先 ~/.ccdesk/accounts.json。
/// **トークンを含む**ので、ログやエラーメッセージに中身を出さないこと
pub fn accounts_store_path() -> Option<std::path::PathBuf> {
    Some(ccdesk_dir()?.join("accounts.json"))
}

/// エラーの集約先 ~/.ccdesk/error.log へ時刻付きで追記する。
/// panic（TUI は画面ごと消えて読めない）と実行時エラー（attach 失敗等）の両方が集まる
pub fn log_error(msg: &str) {
    let Some(path) = ccdesk_dir().map(|d| d.join("error.log")) else {
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
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
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

/// kv_save の read-modify-write を直列化するプロセス内ロック
/// （UI スレッドとディスパッチスレッドの同時書込みでキーが消えるのを防ぐ）
static KV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn kv_load(path: Option<std::path::PathBuf>, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(path?).ok()?;
    let v = serde_json::from_str::<serde_json::Value>(&text).ok()?;
    Some(v.get(key)?.as_str()?.to_string())
}

/// 文字列配列の読み取り。**読みは寛容**で、値の形が想定と違えば「保存が無い」ものとして
/// 扱う（ファイル無し・壊れた JSON・オブジェクトでない・キー無し・配列でない・
/// 要素が文字列でない）。state.json はユーザーが手で直す想定のファイルではないので、
/// 壊れていたら起動を止めるより既定値で先へ進むのが唯一の親切な選択になる
fn kv_load_list(path: Option<std::path::PathBuf>, key: &str) -> Vec<String> {
    let Some(path) = path else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    v.get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| e.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// キー 1 つの read-modify-write。**書き込みの作法（プロセス内ロック・オブジェクト以外の
/// 上書き・tmp → rename）をここ 1 箇所に持つ**ので、値が文字列でも配列でも同じ保証になる
fn kv_put(path: Option<std::path::PathBuf>, key: &str, value: serde_json::Value) {
    let Some(path) = path else { return };
    let _guard = KV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut v = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if !v.is_object() {
        v = serde_json::json!({});
    }
    v[key] = value;
    // 読み手が書きかけの JSON を見ないよう tmp → rename で置く
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, serde_json::to_string_pretty(&v).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn kv_save(path: Option<std::path::PathBuf>, key: &str, value: &str) {
    kv_put(path, key, serde_json::Value::String(value.to_string()));
}

fn kv_save_list(path: Option<std::path::PathBuf>, key: &str, values: &[String]) {
    kv_put(
        path,
        key,
        serde_json::Value::Array(
            values
                .iter()
                .map(|v| serde_json::Value::String(v.clone()))
                .collect(),
        ),
    );
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

pub fn save_state_list(key: &str, values: &[String]) {
    kv_save_list(state_path(), key, values);
}

/// 2 つのパスが同じフォルダを指すか。**「同じフォルダか」の判断はここ 1 箇所だけ**に置く
/// （登録リストの重複排除・登録解除の対象照合・セッション行をどの見出しへ入れるかの
/// 振り分けが別々の答えを出すと、見出しが 2 つに割れたり登録解除が空振りする）。
///
/// 大小・区切りの種類・末尾の区切りを無視するのは、突き合わせる文字列の出自が違うため:
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
/// **残すのは区切り 1 個だけ**で、`C:\\` のように重複した表記も同じキーへ丸める
/// （素朴な join で作られ得る形で、別扱いにすると見出しが 2 つに割れて登録解除も
/// 空振りする ＝ [`same_dir`] が 1 箇所で持つ不変条件が崩れる）。区切りが元から
/// 無い `C:` は丸めない ＝ ドライブ指定はドライブ直下と別物のまま
pub fn dir_key(path: &str) -> String {
    let unified = path.replace('/', "\\").to_lowercase();
    let trimmed = unified.trim_end_matches('\\');
    // trimmed は unified の先頭からの部分なので、長さの差 = 落とした区切りの個数
    let root_with_separator = (trimmed.is_empty() || trimmed.ends_with(':'))
        && trimmed.len() < unified.len();
    if root_with_separator {
        format!("{trimmed}\\")
    } else {
        trimmed.to_string()
    }
}

// 注: 旧実装（~/.claude/sessions レジストリ読み・roster.json・JSONL transcript パース・
// プロセス親子関係の遡り）は監査指摘により削除した。ライブ状態は正規の
// `claude agents --json` を唯一のソースとする。

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト専用の JSON ファイル。~/.ccdesk は触らない（開発者の state.json を踏まない）
    fn temp_json(name: &str, contents: Option<&str>) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ccdesk-test-{}-{name}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("state.json");
        match contents {
            Some(text) => std::fs::write(&path, text).expect("テスト用ファイルが書けない"),
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
        path
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
                "{name} で配列読みが空にならない"
            );
            // 単値読みも同じ寛容さ。旧形式（文字列が入っていた頃）だけは値として読めるので
            // ケースを分ける（ケース名で例外を作ると何を保証しているのか読めなくなる）
            let single = kv_load(Some(path), "projects");
            if name == "legacy-string" {
                assert_eq!(single.as_deref(), Some("C:\\dev\\a"), "旧形式の単値が読めない");
            } else {
                assert!(single.is_none(), "{name} で単値読みが None にならない");
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
        kv_save(some(&path), "sidebar_width", "33");
        let projects = vec!["C:\\dev\\a".to_string(), "C:\\dev\\b".to_string()];
        kv_save_list(some(&path), "projects", &projects);
        assert_eq!(kv_load(some(&path), "sidebar_width").as_deref(), Some("33"));
        assert_eq!(kv_load_list(some(&path), "projects"), projects);
        // 空配列も「0 件」として保存できる（キーごと消す実装だと未保存と区別できない）
        kv_save_list(some(&path), "projects", &[]);
        assert!(kv_load_list(some(&path), "projects").is_empty());
        assert_eq!(
            kv_load(some(&path), "sidebar_width").as_deref(),
            Some("33"),
            "配列の書き込みが他のキーを消している"
        );
        // オブジェクトでないファイルは作り直す（壊れた state.json で保存が死なない）
        let broken = temp_json("write-over-broken", Some("[1,2,3]"));
        kv_save_list(some(&broken), "projects", &projects);
        assert_eq!(kv_load_list(some(&broken), "projects"), projects);
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
            assert!(same_dir(a, b), "{a:?} と {b:?} が同じフォルダにならない");
        }
        // 別フォルダは別。末端名が同じでも親が違えば別（見出しが混ざってはいけない）
        for (a, b) in [
            ("C:\\dev\\api", "C:\\dev\\api2"),
            ("C:\\work\\api", "C:\\dev\\api"),
            ("C:\\dev\\api", ""),
        ] {
            assert!(!same_dir(a, b), "{a:?} と {b:?} が同じフォルダ扱いになった");
        }
    }

    /// ドライブ直下は区切りを落とさない。`C:\` を `C:` に丸めると Windows では
    /// 「そのドライブのカレントディレクトリ」を指す別物になる
    #[test]
    fn same_dir_keeps_the_drive_root_separator() {
        assert_eq!(dir_key("C:\\"), "c:\\");
        assert_eq!(dir_key("C:/"), "c:\\", "区切りの種類はキーに残さない");
        assert!(!same_dir("C:\\", "C:"), "ドライブ直下とドライブ指定を同一視している");
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
        assert!(same_dir("C:\\", "C:\\\\"), "重複区切りのドライブ直下が別扱いになった");
        assert!(same_dir("C:\\\\", "c://"), "区切りの種類と個数が混ざると別扱いになる");
        assert!(same_dir("\\\\", "\\"), "ルートの重複区切りが別扱いになった");
        // 区切りが元から無い `C:` はドライブ指定なので、丸めた結果と同一視しない
        assert!(!same_dir("C:", "C:\\\\"), "ドライブ指定がドライブ直下と同一視された");
        // 末端まであるパスは従来どおり（重複区切りは落ちる）
        assert_eq!(dir_key("C:\\dev\\api\\\\"), "c:\\dev\\api");
    }
}
