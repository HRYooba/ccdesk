//! 行の出来事を OS の通知で知らせる（Windows のトースト。**既定では出さない**）。
//!
//! 何を知らせるかは `~/.ccdesk/config.json` の `"notify"` が決める
//! （`["waiting", "done"]` ＝ [`wanted`]）。**待ちと完了で振る舞いを変えない**:
//! どちらも同じ形で出て、同じように時間で引っ込む（[`show`]）。
//!
//! **材料は hook の出来事、撃つのは TUI。**
//!
//! 何を知らせるかは agent 自身が名乗る（[`crate::hooks::HOOK_EVENTS`] の `alert`
//! 列 ＝ 「呼んでいる」「終わった」）。**行の state の変わり目は材料にしない**:
//! state は「今どうなっているか」しか答えず、その中には**ユーザー自身が開いた
//! ダイアログ**も入る（claude は `/config` `/resume` でも `status: waiting` を
//! 書く）ので、変わり目で撃つと呼ばれてもいないのに呼び出される。
//!
//! **撃つのが hook 本体でないのは、行の表示名を持たないから**（名前は記録から
//! 導くもので、[`crate::title`] の走査が要る）。通知が答えるのは「どのセッションが
//! 呼んでいるか」なので、名前と cwd を既に持っている側 ＝ TUI が、hook が保管へ
//! 書いた出来事を読んで撃つ（[`crate::app`] の `update_notifications`）。
//!
//! **`ccdesk` を名乗るには AppUserModelID の登録が要る。** Windows はトーストの
//! 送り主をこの ID で識別し、通知に出る**名前**と「設定 > 通知」の行はそこに紐づく
//! （登録の無い ID では通知を出すこと自体ができない）。
//! 登録先は `HKCU` だけ ＝ 管理者権限もインストーラも要らない。
//!
//! **アイコンだけはそこから来ない。** 通知の左上に出る小さなアイコンは Windows が
//! **呼び出し元のコンソールホスト**から取るもので、次のどれでも変えられなかった
//! （すべて実測）: 登録の `IconUri`・実行ファイルへ埋め込んだアイコンリソース・
//! `System.AppUserModel.ID` を持つスタートメニューのショートカット。
//! **ccdesk と分かるのは本文の左に出す画像**（`appLogoOverride` ＝ [`show`]）だけで、
//! これは通知の中身なので確実に出る。
//!
//! # クリックで開く
//!
//! 押されたことは WinRT のイベントで**別スレッドへ**届く。そこで行を開くと
//! run ループの外から `App` を触ることになるので、押された行を [`CLICKED`] へ
//! 積むだけにして、**開くのは run ループが引き取ってから**にする。
//!
//! **出すのは専用スレッド**（[`sender`]）。TUI の本体スレッドから出すと
//! 通知は出るのに**押しても何も起きない**（配送先が、回していない
//! メッセージポンプになる）。実際にそうなっていた。
//!
//! **画面に出ている端末を前面へ戻すのは別の仕事**（[`raise_terminal`]）。
//! 前面へ戻す先は「ccdesk がフォーカスを得た瞬間の前面ウィンドウ」を控えて使う
//! （[`remember_terminal_window`]）＝ 端末を親プロセスから手繰る必要がない。
//! **タブは切り替えられない**: Windows Terminal のタブは同じウィンドウなので、
//! 別タブに居る ccdesk はウィンドウが前に出ても背面のタブのまま残る。
//!
//! **複数の ccdesk は同じ AppUserModelID・group・tag を共有する。** 同じ行の
//! 遷移を両方が見れば両方が撃つことになる（後のものが前を置き換えるので画面には
//! 1 枚しか残らないが、鳴るのは 2 回）＝ **撃つ側で持ち主を決める**:
//! 通知を出すのはその行を動かしている窓を持つプロセスだけで、他インスタンスの
//! 行は一覧に出るが撃たない（[`crate::app`] の `update_notifications`）。
//! プロセス間の調停は要らない ＝ 窓は 1 つの行に 1 つしか無い。
//!
//! # 失敗は画面に出さない
//!
//! 通知が出ないこと自体は作業を止めないので下部バーへは出さず、原因は
//! `~/.ccdesk/error.log` へ**同じ文面につき 1 プロセス 1 度だけ**残す（通知は turn
//! ごとに撃つので、出せない環境では毎回書くとログがそれで埋まる）。
//! **数えるのが種類ではなく文面**な理由は [`report`]。

use crate::sessions::SessionId;

/// クリックされた通知が指していた行。**積むのは WinRT のイベントスレッド、
/// 引き取るのは run ループ**（[`clicked`]）
#[cfg(windows)]
static CLICKED: std::sync::Mutex<Vec<SessionId>> = std::sync::Mutex::new(Vec::new());

/// 通知の送り主として名乗る AppUserModelID。**この値が通知の identity**で、
/// 名前・アイコン・OS 側の通知設定はすべてここに紐づく
#[cfg(windows)]
const APP_ID: &str = "HRYooba.ccdesk";

/// 同梱するアイコン。**`IconUri` はパスしか受けない**ので、埋め込んだこのバイト列を
/// `~/.ccdesk/icon.png` へ書き出して指す（`cargo install` で入れた実行ファイルの
/// 隣に `assets/` は無い ＝ ソースツリーの位置に依存させない）
#[cfg(windows)]
const ICON: &[u8] = include_bytes!("../assets/ccdesk.png");

/// 通知用スレッドへの注文（[`sender`]）
#[cfg(windows)]
struct Request {
    /// 通知の種類（題の頭に出る。[`Kind::headline`]）
    kind: Kind,
    project: String,
    session: String,
    /// **押されたときに開く行**
    id: SessionId,
}

/// 何を知らせる通知か。**取り下げは持たない**（どの通知も時間で引っ込む）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    /// ユーザーが動くまで進まない（許可待ち等）
    NeedsInput,
    /// ターンが終わった
    Finished,
}

impl Kind {
    /// **保管（`~/.ccdesk/hook-states.json`）に残る綴り。**
    /// hook は別プロセスなので、撃つ材料はファイルを通って TUI へ渡る
    /// （[`crate::hooks::HookStates::alert`]）。読みと書きが別々の綴りを持つと、
    /// 片方だけ変えたときに**通知が黙って止まる**（行の表示は何も変わらないので
    /// 気づけない種類の壊れ方）
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::NeedsInput => "needs_input",
            Self::Finished => "finished",
        }
    }

    /// 保管値からの復元。**知らない綴りは None**（＝ その行の呼び出しは撃たれない）
    pub(crate) fn parse(text: &str) -> Option<Self> {
        [Self::NeedsInput, Self::Finished]
            .into_iter()
            .find(|kind| kind.as_str() == text)
    }

    /// 通知の題の頭。**画面の状態名と同じ語**を使う（通知だけ別の呼び方をしない）
    #[cfg(windows)]
    fn headline(self) -> &'static str {
        match self {
            Self::NeedsInput => "Needs input",
            Self::Finished => "Turn finished",
        }
    }

    /// 通知の名前（tag）に混ぜる種類。**行 ID だけを tag にしてはいけない**:
    /// 同じ名前の通知は置き換えになり、**置き換えは鳴らない**（実測: 待ちの直後に
    /// 完了を出すと、画面の文字だけが黙って差し替わる）。種類を混ぜておけば、
    /// 待ちと完了は別物として鳴り、**同じ種類の繰り返しだけが置き換わる**
    #[cfg(windows)]
    fn tag_suffix(self) -> &'static str {
        match self {
            Self::NeedsInput => "waiting",
            Self::Finished => "finished",
        }
    }
}

/// どの出来事を知らせるか（`~/.ccdesk/config.json` の `"notify"`）。
///
/// **既定はどちらも false** ＝ 書いていない人には通知が飛ばない
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct Wanted {
    /// 入力待ち（許可待ち等）
    pub(crate) waiting: bool,
    /// ターンが終わった
    pub(crate) finished: bool,
}

impl Wanted {
    /// 1 つも出さないなら、突き合わせ自体を回さなくてよい
    pub(crate) fn any(self) -> bool {
        self.waiting || self.finished
    }

    /// その種類を出すか。**設定の綴りと [`Kind`] の対応はここ 1 箇所**
    pub(crate) fn wants(self, kind: Kind) -> bool {
        match kind {
            Kind::NeedsInput => self.waiting,
            Kind::Finished => self.finished,
        }
    }
}

/// 設定値から [`Wanted`] を作る。設定の書き方は**列挙**
/// （`"notify": ["waiting", "done"]`）で、値は次の 3 つ:
///
/// | 値 | 意味 |
/// |:--|:--|
/// | `waiting` | 入力待ちになったとき |
/// | `done` | ターンが終わったとき |
/// | `on` | `waiting` と同じ（この設定が単値の `"on"` だった頃の書き方） |
///
/// **知らない値は黙って捨てる**（綴り違いで通知が全部止まるより、書いた分だけ
/// 効く方が直しやすい）。単値で書かれた設定も呼び手が 1 要素として渡す
pub(crate) fn wanted(values: &[String]) -> Wanted {
    let mut wanted = Wanted::default();
    for value in values {
        match value.trim() {
            "waiting" | "on" => wanted.waiting = true,
            "done" => wanted.finished = true,
            _ => {}
        }
    }
    wanted
}

/// 行の出来事を 1 件知らせる。`project` は行の cwd の末端、`session` は
/// 行の表示名、`id` は**クリックされたときに開く行**。
///
/// **実際に出すのは通知用のスレッド**（[`sender`]）で、ここは渡すだけ
#[cfg(windows)]
pub(crate) fn post(kind: Kind, project: &str, session: &str, id: &SessionId) {
    send(Request {
        kind,
        project: project.to_string(),
        session: session.to_string(),
        id: id.clone(),
    });
}

/// 通知用スレッドへ積める上限。**満杯なら捨てる**（[`send`]）。
///
/// 撃つのは行の変わり目なので、通常は 1 周に多くても行数ぶんしか積まれない ＝
/// この数に届くのは worker が詰まっている状況だけ。そこで待つ（無界に積む）と
/// 描画ループが通知に引きずられ、メモリも上限を失う
#[cfg(windows)]
const QUEUE_LIMIT: usize = 64;

#[cfg(windows)]
fn send(request: Request) -> bool {
    use std::sync::mpsc::TrySendError;

    match sender().try_send(request) {
        Ok(()) => true,
        // **待たずに捨てる。** 詰まっているときに積み足しても、出るのは
        // 「もう終わった出来事」の列。捨てた事実はログに 1 度だけ残す
        Err(TrySendError::Full(_)) => {
            report("dropped a desktop notification: the worker is behind");
            false
        }
        Err(TrySendError::Disconnected(_)) => {
            report("desktop notification worker is unavailable: the channel is closed");
            false
        }
    }
}

/// 通知を出すスレッドへの口（**プロセスに 1 本**）。
///
/// **描画ループから直接 WinRT を呼んではいけない。** トーストの押下は WinRT の
/// イベントとして返ってくるが、その配送は呼び出し側のスレッドが属する
/// アパートメントに従う。TUI の本体スレッドは Windows のメッセージポンプを
/// 回さないので、そちらで出すと**通知は出るのに押しても何も起きない**
/// （実測でこうなった）。ここで MTA のスレッドを 1 本作り、
/// 出すのも受けるのもそのスレッドに閉じる。
///
/// **積むだけで待たない**（[`QUEUE_LIMIT`] で満杯なら捨てる）。表示には 10ms
/// 程度かかるので、描画ループから外れること自体にも意味がある
#[cfg(windows)]
fn sender() -> &'static std::sync::mpsc::SyncSender<Request> {
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

    static SENDER: std::sync::OnceLock<std::sync::mpsc::SyncSender<Request>> =
        std::sync::OnceLock::new();
    SENDER.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Request>(QUEUE_LIMIT);
        std::thread::spawn(move || {
            // このスレッドを MTA にする ＝ 押下は既定のスレッドプールへ配送される
            // （STA だと配送先が「回していないポンプ」になる）
            if let Err(error) = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() } {
                report(&format!(
                    "could not initialize the desktop notification worker: {error}"
                ));
                return;
            }
            while let Ok(request) = rx.recv() {
                show(request.kind, &request.project, &request.session, &request.id);
            }
        });
        tx
    })
}

/// 通知に付ける組名。tag と対で「同じ通知か」を決める（tag は行 ID ＋ 種類 ＝
/// [`try_show`]）
#[cfg(windows)]
const GROUP: &str = "sessions";

/// トーストを 1 枚出す（**通知用スレッドの中だけ**で呼ぶ）。
///
/// **どの種類も時間で引っ込む**（Windows の既定 ＝ 数十秒で通知センターへ移る）。
/// 消えないトースト（`scenario="reminder"`）は席を外していても押せる利点が
/// あるが、**取り下げる相手が無い通知**（ターン完了は Idle のまま何時間も続く）が
/// 画面に溜まる。種類ごとに消え方を変えるより、**1 つの振る舞いに揃える**方を採った。
/// 代償: 引っ込んだ後に通知センターから押しても ccdesk には届かない
/// （COM のアクティベータ登録が要る）
#[cfg(windows)]
fn show(kind: Kind, project: &str, session: &str, id: &SessionId) {
    let Some(icon) = registered() else {
        return;
    };
    if let Err(e) = try_show(kind, project, session, id, icon) {
        report(&format!("could not show a desktop notification: {e}"));
    }
}

#[cfg(windows)]
fn try_show(
    kind: Kind,
    project: &str,
    session: &str,
    id: &SessionId,
    icon: &std::path::Path,
) -> windows::core::Result<()> {
    use windows::core::{IInspectable, HSTRING};
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::Foundation::TypedEventHandler;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};

    let icon_uri = file_uri(icon)?;

    // **ccdesk と分かるのはこの `appLogoOverride` だけ。** 通知の左上に出る小さな
    // アイコンは Windows が呼び出し元のコンソールホストから取るもので、
    // アプリ側からは変えられない（module のコメント）。こちらは通知の中身なので確実
    let payload = format!(
        r#"<toast>
             <visual><binding template="ToastGeneric">
               <image placement="appLogoOverride" hint-crop="default" src="{icon}"/>
               <text>{headline} · {project}</text>
               <text>{session}</text>
             </binding></visual>
           </toast>"#,
        icon = escape(&icon_uri),
        headline = kind.headline(),
        project = escape(project),
        session = escape(session),
    );

    let xml = XmlDocument::new()?;
    xml.LoadXml(&HSTRING::from(payload))?;
    let toast = ToastNotification::CreateToastNotification(&xml)?;
    // **行と種類で 1 枚**（[`Kind::tag_suffix`]）。同じ行の同じ種類を続けて出せば
    // 置き換わり、待ち → 完了は別物として鳴る
    toast.SetTag(&HSTRING::from(format!("{}-{}", id.as_str(), kind.tag_suffix())))?;
    toast.SetGroup(&HSTRING::from(GROUP))?;

    let clicked = id.clone();
    toast.Activated(&TypedEventHandler::<ToastNotification, IInspectable>::new(
        move |_, _| {
            if let Ok(mut queue) = CLICKED.lock() {
                queue.push(clicked.clone());
            }
            Ok(())
        },
    ))?;

    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID))?.Show(&toast)?;
    Ok(())
}

/// Windows 自身の URL builder でファイルパスを percent-encoded file URI にする。
/// 空白や `#` を XML escape だけで済ませると URI の区切りとして解釈されてしまう。
#[cfg(windows)]
fn file_uri(path: &std::path::Path) -> windows::core::Result<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::UI::Shell::UrlCreateFromPathW;

    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // 最悪すべての UTF-16 code unit が `%XX` へ広がっても収まる大きさ。
    let mut output = vec![0u16; path.len().saturating_mul(3).saturating_add(16)];
    let mut length = output.len() as u32;
    unsafe {
        UrlCreateFromPathW(
            PCWSTR::from_raw(path.as_ptr()),
            PWSTR::from_raw(output.as_mut_ptr()),
            &mut length,
            0,
        )?;
    }
    let mut used = (length as usize).min(output.len());
    if used > 0 && output[used - 1] == 0 {
        used -= 1;
    }
    Ok(String::from_utf16_lossy(&output[..used]))
}


/// XML の本文へ入れる文字の逃がし（**表示名は利用者の書いた文字列**なので、
/// `&` や `<` がそのまま入ると payload が壊れて通知が出なくなる）
#[cfg(windows)]
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Windows 以外は何もしない（トーストに相当する口を持たない ＝ 呼び手に cfg が要らない）
#[cfg(not(windows))]
pub(crate) fn post(_kind: Kind, _project: &str, _session: &str, _id: &SessionId) {}

/// 押された通知が指していた行を引き取る（**引き取った分は消える**）。
/// 呼ぶのは run ループだけで、そこで初めて行を開く
#[cfg(windows)]
pub(crate) fn clicked() -> Vec<SessionId> {
    match CLICKED.lock() {
        Ok(mut queue) => std::mem::take(&mut queue),
        Err(_) => Vec::new(),
    }
}

#[cfg(not(windows))]
pub(crate) fn clicked() -> Vec<SessionId> {
    Vec::new()
}

/// **ccdesk を載せている端末のウィンドウを控える。** 呼ぶのは端末がフォーカスを
/// 得たとき（`FocusGained`）だけ ＝ そのとき前面に居るウィンドウは ccdesk を
/// 映している当のウィンドウなので、**親プロセスを手繰らずに特定できる**
/// （ConPTY 環境では `GetConsoleWindow` が返すのは隠しウィンドウで使えない）。
///
/// 一度もフォーカスを得ていなければ控えは空のまま ＝ [`raise_terminal`] は何もしない
#[cfg(windows)]
pub(crate) fn remember_terminal_window() {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_invalid() {
        return;
    }
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    let Some(started_at) = process_started_at(pid) else {
        report("could not identify the terminal window owner");
        return;
    };
    if let Ok(mut remembered) = terminal_window().lock() {
        *remembered = Some(WindowIdentity {
            hwnd: hwnd.0 as isize,
            pid,
            started_at,
        });
    }
}

#[cfg(not(windows))]
pub(crate) fn remember_terminal_window() {}

/// 控えておいた端末のウィンドウを前面へ戻す（通知から行を開いた直後）。
/// **控えが無ければ何もしない**（間違ったウィンドウを前へ出すより出さない方がよい）
#[cfg(windows)]
pub(crate) fn raise_terminal() {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        FlashWindowEx, GetWindowThreadProcessId, IsIconic, IsWindow, SetForegroundWindow,
        ShowWindow, FLASHWINFO, FLASHW_ALL, FLASHW_TIMERNOFG, SW_RESTORE,
    };

    let identity = terminal_window().lock().ok().and_then(|window| *window);
    let Some(identity) = identity else {
        return;
    };
    let hwnd = HWND(identity.hwnd as *mut core::ffi::c_void);
    unsafe {
        let mut owner = 0;
        let valid = IsWindow(Some(hwnd)).as_bool()
            && GetWindowThreadProcessId(hwnd, Some(&mut owner)) != 0
            && owner == identity.pid
            && process_started_at(owner) == Some(identity.started_at);
        if !valid {
            if let Ok(mut remembered) = terminal_window().lock() {
                *remembered = None;
            }
            report("the remembered terminal window is no longer owned by the same process");
            return;
        }
        // 最小化されていると前面化だけでは戻らない
        if IsIconic(hwnd).as_bool() {
            // 戻り値は直前に可視だったかで、失敗値ではない。復元後の状態を検証する
            let _was_visible = ShowWindow(hwnd, SW_RESTORE).as_bool();
            if IsIconic(hwnd).as_bool() {
                report("could not restore the terminal window");
            }
        }
        if !SetForegroundWindow(hwnd).as_bool() {
            let flash = FLASHWINFO {
                cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
                hwnd,
                dwFlags: FLASHW_ALL | FLASHW_TIMERNOFG,
                uCount: 3,
                dwTimeout: 0,
            };
            // 戻り値は呼び出し前にアクティブだったかを表し、成否値ではない
            let _was_active = FlashWindowEx(&flash).as_bool();
            report("could not foreground the terminal window; flashed it instead");
        }
    }
}

#[cfg(not(windows))]
pub(crate) fn raise_terminal() {}

#[cfg(windows)]
#[derive(Clone, Copy)]
struct WindowIdentity {
    /// `HWND` は `Send` ではないので生の値で持つ
    hwnd: isize,
    pid: u32,
    /// PID 再利用まで見分けるプロセス作成時刻（FILETIME の 100ns tick）
    started_at: u64,
}

#[cfg(windows)]
fn terminal_window() -> &'static std::sync::Mutex<Option<WindowIdentity>> {
    static WINDOW: std::sync::OnceLock<std::sync::Mutex<Option<WindowIdentity>>> =
        std::sync::OnceLock::new();
    WINDOW.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(windows)]
fn process_started_at(pid: u32) -> Option<u64> {
    use windows::Win32::Foundation::{CloseHandle, FILETIME};
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()? };
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let read = unsafe {
        GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user).is_ok()
    };
    unsafe {
        let _ = CloseHandle(process);
    }
    read.then_some((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

/// AppUserModelID の登録（**1 プロセスに 1 度**）。戻り値は書き出したアイコンの場所で、
/// **None は「通知を出せない」**。
///
/// 登録が要るのは名前のため: 通知に出る送り主の名前はこの `DisplayName` から来る
/// （**アイコンは来ない** ＝ [`show`]）。登録の無い ID では通知そのものが出せない
#[cfg(windows)]
fn registered() -> Option<&'static std::path::Path> {
    static DONE: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    DONE.get_or_init(|| match register() {
        Ok(icon) => Some(icon),
        Err(e) => {
            report(&format!(
                "could not register the notification app id: {e}; \
                 desktop notifications stay off for this run"
            ));
            None
        }
    })
    .as_deref()
}

#[cfg(windows)]
fn register() -> anyhow::Result<std::path::PathBuf> {
    let icon = icon_file()?;
    let key = windows_registry::CURRENT_USER
        .create(format!(r"Software\Classes\AppUserModelId\{APP_ID}"))?;
    key.set_string("DisplayName", "ccdesk")?;
    // **通知の絵はここからは来ない**（実測）。それでも書くのは「設定 > 通知」と
    // 通知センターの一覧が読む先だから
    key.set_string("IconUri", icon.to_string_lossy().as_ref())?;
    // 「設定 > 通知」に ccdesk の行を出す ＝ **OS の側で切れる**
    // （ccdesk の設定を知らなくても止められる口を残す）
    key.set_u32("ShowInSettings", 1)?;
    Ok(icon)
}

/// 同梱アイコンの実体を `~/.ccdesk/icon.png` に置いてそのパスを返す。
/// **大きさが違えば書き直す** ＝ ccdesk を更新すればアイコンも入れ替わる
#[cfg(windows)]
fn icon_file() -> anyhow::Result<std::path::PathBuf> {
    let path = ccdesk::ccdesk_dir()
        .ok_or_else(|| anyhow::anyhow!("no home directory"))?
        .join("icon.png");
    if std::fs::metadata(&path).map(|m| m.len()).ok() != Some(ICON.len() as u64) {
        std::fs::write(&path, ICON)?;
    }
    Ok(path)
}

/// 書いた文面の上限。届いたら以後は何も書かない。
///
/// 上限が要るのは、通知が turn ごとに撃たれるため ＝ 毎回違う文面になる失敗
/// （OS のエラーコードが混ざる等）で `error.log` を埋めない
#[cfg(windows)]
const REPORT_LIMIT: usize = 32;

/// 通知の失敗を `~/.ccdesk/error.log` へ書く（**同じ文面は 1 プロセス 1 度**）。
///
/// **数えるのは文面で、失敗の種類ではない。** 種類ごとに 1 度だと、同じ種類の中の
/// **別の原因**が最初の 1 件に隠れて永久に出ない（前面化なら「OS に拒否された」と
/// 「控えた窓が別プロセスのものになった」、表示ならペイロード・tag・notifier の
/// どれで落ちたか ＝ 実際に 1 つの旗を共有していた）。
///
/// 通知が出ないこと自体は作業を止めないので、下部バーへは出さない（module の頭）
#[cfg(windows)]
fn report(message: &str) {
    use std::collections::BTreeSet;
    use std::sync::Mutex;

    static SEEN: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
    let Ok(mut seen) = SEEN.lock() else {
        return;
    };
    if seen.len() >= REPORT_LIMIT || !seen.insert(message.to_string()) {
        return;
    }
    ccdesk::log_error(message);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **設定の綴りは 3 つだけ。** 単値だった頃の `"on"` は入力待ちの意味で
    /// 残す（この設定を先に書いた人の config を黙って無効にしない）
    #[test]
    fn the_config_lists_which_events_are_announced() {
        let of = |values: &[&str]| wanted(&values.iter().map(|v| v.to_string()).collect::<Vec<_>>());

        assert_eq!(of(&[]), Wanted { waiting: false, finished: false });
        assert!(!of(&[]).any());
        assert_eq!(of(&["waiting"]), Wanted { waiting: true, finished: false });
        assert_eq!(of(&["done"]), Wanted { waiting: false, finished: true });
        assert_eq!(of(&["waiting", "done"]), Wanted { waiting: true, finished: true });
        // 旧い単値の書き方
        assert_eq!(of(&["on"]), Wanted { waiting: true, finished: false });
        // **知らない値は捨てるだけ**（隣に書いた正しい値は効く）
        assert_eq!(of(&["nonsense", "done"]), Wanted { waiting: false, finished: true });
        // 前後の空白は書き手の都合
        assert_eq!(of(&[" done "]), Wanted { waiting: false, finished: true });
    }

    /// URI の予約文字はパスの一部として符号化され、区切り文字へ化けない。
    #[test]
    fn icon_path_is_built_as_a_percent_encoded_file_uri() {
        let uri = file_uri(std::path::Path::new(r"C:\folder name\icon #.png"))
            .expect("could not build a file URI");
        assert_eq!(uri, "file:///C:/folder%20name/icon%20%23.png");
    }
}
