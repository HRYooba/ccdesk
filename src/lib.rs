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

fn kv_save(path: Option<std::path::PathBuf>, key: &str, value: &str) {
    let Some(path) = path else { return };
    let _guard = KV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut v = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if !v.is_object() {
        v = serde_json::json!({});
    }
    v[key] = serde_json::Value::String(value.to_string());
    // 読み手が書きかけの JSON を見ないよう tmp → rename で置く
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, serde_json::to_string_pretty(&v).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
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

// 注: 旧実装（~/.claude/sessions レジストリ読み・roster.json・JSONL transcript パース・
// プロセス親子関係の遡り）は監査指摘により削除した。ライブ状態は正規の
// `claude agents --json` を唯一のソースとする。
