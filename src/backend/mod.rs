//! agent（claude / codex）ごとに違う知識の唯一の入口。
//!
//! **ここが無いと `match kind` が散る。** 起動コマンド・hook の注入・表示名の
//! 出どころ・使用率の取り方は agent ごとに違い、その知識は今まで
//! [`crate::session`] / [`crate::hooks`] / [`crate::title`] / [`crate::usage`] に
//! 分かれて置かれていた。呼び出し側が kind を見て分岐すると、agent を 1 つ足すのに
//! 4 ファイルを直すことになる（＝ 1 つの変更が 1 箇所に閉じない）。
//!
//! **呼び出し側は kind を見ない。** [`Kind::backend`] から [`Backend`] を貰い、
//! それに聞く。agent を足すときに増えるのは `backend/` のファイル 1 枚と
//! [`Kind`] の値 1 つで、コンパイラが未実装のメソッドを要求する。
//!
//! **どの agent を出すかは設定 1 箇所**（[`Kind::enabled`]）。任意の agent は
//! opt-in で、切っている間は表示も導線もポーリングもその agent へ届かない。

pub(crate) mod claude;
pub(crate) mod codex;
pub(crate) mod codex_app_server;
pub(crate) mod codex_index;

use portable_pty::CommandBuilder;

use crate::sessions::SessionId;

/// 起動の種類。**会話をどう決めるか**の 3 通りで、どれを使うかは呼び出し側が
/// 行の [`crate::sessions::Conversation`] から決める（`crate::app` の `relaunch`）
pub(crate) enum Launch<'a> {
    /// 新規の会話。`prompt` は最初のメッセージ（空なら渡さない）。
    /// **会話 ID を採番するかは agent 次第**（[`Spawn::conversation`]）
    New { prompt: &'a str },
    /// **確かめた**会話の再開。**cwd の一致が必須**（別 cwd からは会話が見つからない
    /// ＝ 記録が在る作業ツリーで開く。判断は [`crate::title::Titles::resume_cwd`]）
    Resume {
        /// 再開に使う ID。**行の ID ではない**（行 ID は `CCDESK_ROW` 以外の
        /// どこにも出ない）。hook が名乗った値だけがここへ来る
        id: &'a str,
    },
    /// **agent 自身の会話ピッカーを開く**（ID を渡さない）。
    ///
    /// 会話を確かめていない行を開くときに使う。**推測で resume しない**のが
    /// 要点で、渡す ID が違えば別の会話を開く / 見つからずに落ちる。
    /// `claude -r` は値が任意、`codex resume` は既定でピッカー
    Pick,
}

/// 起こす子プロセス 1 つぶん。
///
/// **コマンドラインと「どの会話に載るか」を対で返すことがこの型の存在理由。**
/// claude は ccdesk が UUID を採番して押し付け、codex は codex 自身が採番する
/// （`--session-id` 相当が無い）。この違いを「採番できるか」のような bool で
/// 返すと、呼び手が agent ごとに分岐して採番を代行することになり、agent を
/// 足すたびに呼び手が増える
pub(crate) struct Spawn {
    pub(crate) cmd: CommandBuilder,
    /// この起動が載る会話。**None は「起こす前には分からない」**（codex の新規と
    /// [`Launch::Pick`]）。分かるまで行は会話を持たず、hook が名乗って初めて
    /// [`crate::sessions::Conversation::Observed`] になる
    pub(crate) conversation: Option<String>,
}

/// hook を注入するための材料。
///
/// **agent ごとに載せ方が違う**ので、材料だけを渡してコマンドの形は各実装が決める
/// （claude は ccdesk が書いた settings ファイルを `--settings` で、codex は
/// 実行ファイルのパスから組んだ TOML を `-c` で渡す）。
///
/// **どちらも同じ 1 つの事実（ccdesk 実行ファイルの場所）から導かれる**が、
/// claude 側はファイルの書き出しを伴うので、書き出し済みのパスを一緒に運ぶ。
/// 書き出しは 1 プロセス 1 回（[`crate::hooks::inject_settings`]）
pub(crate) struct Inject<'a> {
    /// ccdesk 実行ファイル（`/` 区切り）。hook のコマンド文字列に埋める
    pub(crate) exe: &'a str,
    /// claude が `--settings` で読むファイル
    pub(crate) settings: &'a std::path::Path,
}

/// 任意 agent を出す設定値。**綴りはここ 1 箇所**（`~/.ccdesk/config.json` の
/// `"codex": "on"`）。これ以外の値・キーが無い場合は off と読む ＝
/// 設定ファイルを持たない人には出ない
pub(crate) const ON: &str = "on";

/// どの agent の行か。**保存と表示の綴りをここ 1 箇所が持つ**
/// （[`crate::poll::State`] と同じ作り: 語彙の正本を 1 つにし、2 つの顔を生やす）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default, Hash)]
pub(crate) enum Kind {
    #[default]
    Claude,
    Codex,
}

impl Kind {
    /// **ccdesk が知っている agent の全部**（表示順）。grouping の節・New 画面の
    /// 切替行・メニューの並びもこれに従う。
    ///
    /// **「今出す agent」はこれではない**（[`Self::enabled`]）。この一覧は
    /// 保存値の復元（[`Self::parse`]）と網羅の検査が読む正本で、設定で切っても
    /// 縮まない ＝ off の agent の行を保存から読めなくなることがない
    pub(crate) const ORDER: [Self; 2] = [Self::Claude, Self::Codex];

    /// **opt-in の agent**（既定では出さない）。ここに載っていない agent は
    /// 設定に関係なく常に出る。
    ///
    /// claude は ccdesk の前提（無ければ何もできない）なので切れない。
    /// codex は任意なので opt-in ＝ 「選べるのは追加の agent だけ」という規則
    const OPTIONAL: [Self; 1] = [Self::Codex];

    /// 設定（`~/.ccdesk/config.json`）から今出す agent の一覧を組む。
    ///
    /// `setting` はその agent の綴りをキーにした値（例 `"codex"` → `"on"`）を
    /// 引く関数。**既定は off** ＝ 設定を書いていない人には claude だけが出る。
    ///
    /// **既定で出さないのは、入れていない agent のポーリングが無駄に回るから。**
    /// アカウント取得はその agent の実行ファイルを起こすので、入っていなければ
    /// 毎回失敗し、[`crate::poll`] の再試行間隔（5 秒）で起動を試み続ける。
    /// 使っている人だけが 1 行書く形なら、その空振りが誰にも起きない。
    ///
    /// 判断をこの純関数に閉じてあるので、ファイルを置かずにテストできる
    pub(crate) fn enabled(setting: impl Fn(&str) -> Option<String>) -> Vec<Self> {
        Self::ORDER
            .into_iter()
            .filter(|kind| {
                !Self::OPTIONAL.contains(kind)
                    || setting(kind.as_str()).as_deref() == Some(ON)
            })
            .collect()
    }

    /// **保存値（`sessions.json`）と CLI 引数の唯一の綴り。**
    /// 読み・書きが別々に綴りを持つと、片方だけ変えたときに保存値が読めなくなる
    /// （行が黙って claude 扱いへ戻る）
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    /// 保存値からの復元。**知らない綴りは None**（呼び手が既定を決める）
    pub(crate) fn parse(text: &str) -> Option<Self> {
        Self::ORDER.into_iter().find(|kind| kind.as_str() == text)
    }

    /// 幅に余裕のある場所（版行・使用率行・grouping の見出し）に出す名前。
    ///
    /// **公式の印は使わない。** claude 公式の印は `✻`（1 桁）、codex 公式の印は `>_`
    /// （2 桁）で幅が揃わず、列が崩れる。両方を 1 画面に並べる慣習も見当たらない。
    /// セッション行が agent を表すのに使っている記号は公式の印ではなく、
    /// **幅の揃う丸と菱**（[`crate::ui`] の `dot_glyph`）で、綴りとの対応は
    /// 版行がこの `title` と並べて出す
    pub(crate) fn title(self) -> &'static str {
        self.as_str()
    }

    /// この kind の実装。**`&'static` にしてある**ので、行やコマンドを組む側は
    /// 寿命を気にせず持ち回せる
    pub(crate) fn backend(self) -> &'static dyn Backend {
        match self {
            Self::Claude => &claude::Claude,
            Self::Codex => &codex::Codex,
        }
    }

    /// 行を 1 つ起こすコマンド。**セッションを起こす唯一の口**
    /// （[`Backend::command`] を直に呼ぶのはここだけ）。
    ///
    /// **`CCDESK_ROW` を立てるのがここ 1 箇所であることが、この関数の存在理由。**
    /// 行 ID は hook の子プロセスへ env でしか渡らず（argv にも transcript 名にも
    /// 出さない）、立て忘れた agent の行は**無音で状態を失う**
    /// ＝ 起動が落ちるわけでもエラーが出るわけでもないので気づけない。
    /// 各 backend に任せていた頃、実際に codex だけが立てていた
    pub(crate) fn spawn_command(
        self,
        row: &SessionId,
        cwd: &str,
        launch: Launch<'_>,
        inject: Option<&Inject>,
    ) -> Spawn {
        let mut spawn = self.backend().command(cwd, launch, inject);
        spawn.cmd.env(crate::hooks::ROW_ENV, row.as_str());
        // **どの ccdesk の子か**（[`crate::relay::INSTANCE_ENV`]）。行 ID と対で
        // 立てるので、立てる場所も同じ 1 箇所にする。`ccdesk send` はこの 2 つが
        // 揃って初めて「自分は誰で、誰へ送れるか」を答えられる
        spawn
            .cmd
            .env(crate::relay::INSTANCE_ENV, std::process::id().to_string());
        spawn
    }
}

/// 起こす実行ファイルの [`CommandBuilder`]。
///
/// **PATH の解決を自前でやる**（[`ccdesk::resolve_program`]）。portable-pty も
/// PATH を探すが、npm が並べて置く拡張子なしのシム（`codex`）を先に掴んで
/// `CreateProcessW` が落ちる（実機で踏んだ）。解決できなければ名前のまま渡す
/// ＝ 従来どおりの挙動へ落ちる
fn program(name: &str) -> CommandBuilder {
    match ccdesk::resolve_program(name) {
        Some(path) => CommandBuilder::new(path),
        None => CommandBuilder::new(name),
    }
}

/// agent ごとに違う振る舞い。
///
/// **メソッドを足すと全 agent が実装を要求される**（それがこの trait の目的）。
/// 「claude では要らない」ものも既定実装で逃がさず、明示的に書かせる
pub(crate) trait Backend: Send + Sync {
    /// 起こす子プロセスのコマンドライン。**PTY を開かずに組める形にしてある**ので、
    /// 引数と環境変数の除去をテストで固定できる（どちらも失敗が静かに効く:
    /// 引数を間違えれば起動が落ち、除去を落とせば会話の記録が保存されない）。
    ///
    /// `inject` は state を戻す hook の材料（[`Inject`]）。None なら hook 無しで
    /// 起こす ＝ 行の状態が縮退するだけで、セッション自体は動く。
    ///
    /// **行 ID を受け取らない。** 行 ID を argv や会話名へ出さないのが今の設計で、
    /// hook へ渡す env は共通の口（[`Kind::spawn_command`]）が立てる
    fn command(&self, cwd: &str, launch: Launch<'_>, inject: Option<&Inject>) -> Spawn;

    /// **この agent は「今どうしているか」を外から読める現在値を持つか。**
    ///
    /// claude は持つ（`~/.claude/sessions/` の `status`。遷移のたびに上書きされる）。
    /// codex は持たない。
    ///
    /// **hook はイベントなので取りこぼすと自己修復しない。** ライブ状態がある側は
    /// 次の観測で必ず正しくなるが、無い側は誰も直せず固着する。実際に 2 通り出た:
    ///
    /// | 固着 | 起きる理由 | 持たない agent の直し方 |
    /// |:--|:--|:--|
    /// | Working が残る | Esc 中断で `Stop` が飛ばない（[codex#22858](https://github.com/openai/codex/issues/22858)） | PTY の無音 |
    /// | Waiting が残る | **許可が解除されたことを知らせる hook がそもそも無い** | 記録が伸びたこと |
    ///
    /// **代用の材料が 2 つに割れるのは、片方では区別が付かないから。** codex の TUI は
    /// 承認ダイアログを出している間も 1 秒ごとにタイトルを書き換える（実測）ので、
    /// PTY の出力では「動いている」と「待たれている」を分けられない。逆に記録
    /// （rollout）は承認待ちの間だけ伸びが止まる（実測: 20 秒の停止）ので、
    /// そちらが Waiting を降ろす材料になる。
    ///
    /// 判断そのものは [`crate::poll::row_state`]（この bool を読むのはあそこだけ）
    fn has_live_status(&self) -> bool;

    /// この agent の現行版と、それより新しい版があればその番号。
    ///
    /// **取得元の URL は agent ごとに違う**（claude は公式配布エンドポイント、
    /// codex は npm registry）が、**どちらも取得のたびにネットワークへ出る**。
    /// agent 自身が残した更新チェックの結果を読む実装にしてはいけない:
    /// その agent を起こさない限り値が古いまま止まり、版行が黙って嘘をつく。
    /// ネットワークへ出るので、呼ぶのは周期取得のスレッドだけ
    fn version(&self) -> AgentVersion;

    /// 更新を走らせるコマンド（`<program> update`）。**版行の更新導線**
    fn update_program(&self) -> &'static str;

    /// この agent が更新のたびに置き去りにする残骸（今あるものだけ）。
    ///
    /// **これは ccdesk が引き取るべき後始末。** agent の更新は古い実行ファイルを
    /// 消そうとするが、ccdesk はセッションを常駐させるので**そのファイルを掴んだ
    /// プロセスが残り続ける**。ターミナルで 1 本起動して閉じる使い方なら次の更新で
    /// 消えるものが、ccdesk の下でだけ溜まり続ける（実測: claude 側 1.1GB /
    /// codex 側 285MB）。作るのは agent でも、消えない状況を作っているのは ccdesk。
    ///
    /// **消してよいと確信できるものだけを返す。** 判断は各実装が持ち、迷うものは
    /// 返さない: 掃除しそこねてもディスクが減らないだけだが、取り違えれば
    /// 動いているインストールを壊す。掃除そのものは [`crate::update::sweep`]
    fn update_leftovers(&self) -> Vec<std::path::PathBuf>;

    /// 使用率（枠の残り）。**取得の作法は agent ごとに違う**が、どちらも
    /// ターンを起こさず・課金せず・記録を残さない経路を通る
    fn usage(&self) -> crate::usage::Usage;

    /// 今サインインしているアカウント。**agent ごとに別のアカウント**なので、
    /// 使用率の行もそれぞれ自分のものを出す（claude の名前を codex の行に
    /// 出していた時期があった）
    fn account(&self) -> crate::poll::AccountStatus;

    /// 認証情報ファイルの指紋（安価な変化 signal）。
    ///
    /// **これが変わったときだけ [`Self::account`] を叩く**（取得はプロセス起動を
    /// 伴うので毎周は回さない）。読めない環境では None ＝ 周期フォールバックだけが効く
    fn auth_fingerprint(&self) -> crate::poll::CredentialsFp;

    /// 会話の記録（claude の transcript / codex の rollout）を探す根。
    ///
    /// **根だけを返し、探すのは [`Self::transcript_in`] に分けてある。**
    /// [`crate::title::Titles`] が根を保持して差し替えられるようにするためで、
    /// テストが実ユーザーの `~/.claude` `~/.codex` を絶対に触らないという
    /// この repo の約束がそこに乗っている
    fn transcript_root(&self) -> Option<std::path::PathBuf>;

    /// その根の下で、この会話の記録がどこにあるか。
    ///
    /// **`cwd` を受けるのは claude のため**（記録の置き場所が作業ツリーから
    /// 決まる）。codex は会話 ID だけで決まるので使わない
    fn transcript_in(
        &self,
        root: &std::path::Path,
        conversation: &str,
        cwd: &str,
    ) -> Option<std::path::PathBuf>;

    /// **その会話を再開できる cwd**（`transcript` は解決済みの記録の場所）。
    ///
    /// 別 cwd から打つと会話が見つからない agent があるので、「どこで打つか」まで
    /// 答える必要がある。見つからなければ None ＝ 呼び手は新規として起こす
    fn resume_cwd(&self, cwd: &str, transcript: Option<&std::path::Path>) -> Option<String>;

    /// 会話に名前を与えうる記録。**この並びが優先順そのもの**
    fn title_records(&self) -> &'static [Candidate];

    /// 記録の外に agent 自身が持っている会話名の索引
    /// （None ＝ この agent は記録の中で名前を持つ）
    fn name_index(&self) -> Option<NameIndex>;

    /// 記録の 1 行から**会話の 1 発言**を取り出す（None ＝ 発言ではない行）。
    ///
    /// **[`Self::title_records`] とは読む目的が違う。** あちらは会話 1 つに
    /// 名前を 1 つ与える候補を探すので、拾えるのは特定の 1 種類でよく、
    /// 範囲も絞れる（[`Span`]）。こちらは `ccdesk read` が会話の中身を並べる
    /// ためのもので、**ユーザーと agent の両方**を、**出た順のまま**返す。
    ///
    /// **道具・思考・前置きは落とす。** 記録には tool の呼び出しと結果、
    /// permissions の前置きなども同じ形で並ぶが、それらは「発言」ではない
    /// （読んだ agent が会話として辿れることがこの関数の目的）
    fn message(&self, value: &serde_json::Value) -> Option<Message>;
}

/// 会話の 1 発言。**agent の違いはここまでで吸収する**ので、
/// [`crate::relay`] は claude と codex の記録の形を知らない
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Message {
    /// 打った人か（false ＝ agent が答えた）
    pub(crate) from_user: bool,
    pub(crate) text: String,
}

impl Message {
    /// 表示に出す話者。**綴りの正本はここ 1 箇所**
    pub(crate) fn speaker(&self) -> &'static str {
        if self.from_user { "user" } else { "agent" }
    }
}

/// 会話に名前を与えうる記録が、その会話の記録ファイルの**どこに現れるか**。
/// 走査の範囲はこの性質から機械的に決まる（[`crate::title::Titles::refresh_all`]）
/// ので、候補と範囲の対応表を別に持たない。
///
/// **この区別を落とすと実害が出る**: claude の `custom-title` を末尾 64 KiB
/// だけで探していた頃は、長い会話の早い段階でリネームした記録が範囲の外に出て
/// 拾えず、記録全体を読む `/resume` のピッカーと名前が食い違っていた
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Span {
    /// セッション中に繰り返し追記される。**最新が答え**なので末尾で足りる
    Appended,
    /// 会話の先頭に 1 度だけ書かれ、**最初の値が答え**。追記されない範囲なので
    /// 1 会話 1 回の有界読みで済む（codex の最初のプロンプトがこれ）
    Head,
    /// まれにしか書かれない。末尾から [`crate::title::RARE_BYTES`] まで遡って探す
    Rare,
}

/// 会話に名前を与えうる記録 1 種類。
///
/// **取り出しを関数で持つのは、agent ごとに記録の形が違うから。** claude は平ら
/// （`{"type":"last-prompt","lastPrompt":…}`）だが、codex は入れ子
/// （`{"type":"event_msg","payload":{"type":"user_message","message":…}}`）で、
/// (型名, キー) の組では codex を表せない
pub(crate) struct Candidate {
    /// JSON を組む前の足切りに使う文字列。**走査の速さの要点**で、
    /// 記録は 1 MB を超えることがあり全行をパースすると走査 1 回が行数に比例する
    pub(crate) marker: &'static str,
    /// 解析済みの 1 行から表示名を取り出す（None ＝ この候補ではない）
    pub(crate) text: fn(&serde_json::Value) -> Option<&str>,
    pub(crate) span: Span,
}

/// agent 自身が**記録の外**に持っている会話名の索引（1 行 1 会話の JSONL）。
///
/// **「索引を持つか」の bool ではなく、どこをどう読むかを返す。** 呼び手は
/// None なら索引を持たないだけで、agent ごとの分岐を書かない
pub(crate) struct NameIndex {
    pub(crate) path: std::path::PathBuf,
    /// 会話 ID のキー
    pub(crate) id_key: &'static str,
    /// 表示名のキー
    pub(crate) name_key: &'static str,
}

/// agent 1 つぶんの版。**「新しい版があるときだけ Some」**という形は
/// claude 側から引き継いだ（読み手が「更新があるか」を latest の有無で判断する）
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub(crate) struct AgentVersion {
    /// 現行版（取得できなければ空。空は「まだ分からない」）
    pub(crate) current: String,
    /// これより新しい版があるときだけ Some
    pub(crate) latest: Option<String>,
}

/// `dir` の直下で、名前が `prefix` で始まり**残りが `rest_ok` を満たす**ものを集める。
///
/// **残骸を拾う走査はここ 1 実装**（[`Backend::update_leftovers`] の材料）。
/// 前置きだけで拾わないのが要点で、`claude.exe.old.` の後ろは世代を表す数字、
/// npm の `.codex-` の後ろはランダムな英数と決まっている。ここを緩めると
/// **動いているインストールを巻き込む**（`claude.exe` そのもの、正規の `codex`）ので、
/// 呼び手は必ず「残りがどうあるべきか」まで指定する。
///
/// 読めないディレクトリは空を返す（掃除できないだけで、困る人はいない）
pub(crate) fn leftovers_in(
    dir: &std::path::Path,
    prefix: &str,
    rest_ok: impl Fn(&str) -> bool,
) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            // 前置きぴったりで終わる名前は残骸ではない（世代を持たない ＝ 本体）
            let rest = name.strip_prefix(prefix).filter(|r| !r.is_empty())?;
            rest_ok(rest).then(|| e.path())
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// 走査は**前置きだけでは拾わない**。残骸の掃除は削除なので、
    /// 動いているインストール（`claude.exe` 本体・正規の `codex`）を
    /// 巻き込まないことをここで固定する
    #[test]
    fn the_leftover_scan_needs_more_than_a_matching_prefix() {
        let dir = crate::testutil::TempDir::new("backend", "leftovers");
        for name in [
            "claude.exe",            // 本体（前置きの手前で終わる）
            "claude.exe.old",        // 世代を持たない ＝ 判断がつかないので拾わない
            "claude.exe.old.17858",  // 残骸
            "claude.exe.old.nope",   // 数字でない ＝ 別物
        ] {
            std::fs::write(dir.join(name), "x").unwrap();
        }
        let found: Vec<String> = leftovers_in(dir.path(), "claude.exe.old.", |rest| {
            rest.chars().all(|c| c.is_ascii_digit())
        })
        .into_iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
        assert_eq!(found, ["claude.exe.old.17858"]);
        // 読めないディレクトリでも落ちない（掃除できないだけ）
        assert!(leftovers_in(&dir.join("missing"), "x", |_| true).is_empty());
    }

    /// 組んだコマンドラインの引数（実行ファイル名を除く）。**各 backend の
    /// test が共有する**ので、引数の読み方が実装ごとにぶれない
    pub(crate) fn argv(cmd: &portable_pty::CommandBuilder) -> Vec<String> {
        cmd.get_argv()
            .iter()
            .skip(1)
            .map(|a| a.to_string_lossy().to_string())
            .collect()
    }

    /// 保存値の綴りは**外部ファイル（`sessions.json`）に残る**ので、
    /// 変えると既存の行が読めなくなる。ここで文字列そのものを固定する
    /// （`parse(as_str())` の往復は `parse` が `as_str` を走査して実装されている
    /// ため恒真になり、綴りの変更を捕まえられない）
    #[test]
    fn the_stored_spelling_of_every_kind_is_fixed() {
        let spellings: Vec<&str> = Kind::ORDER.iter().map(|k| k.as_str()).collect();
        assert_eq!(spellings, ["claude", "codex"]);
    }

    /// **任意の agent は opt-in。** 設定を持たない人には claude だけが出る。
    ///
    /// 既定を on 側にすると、codex を入れていない全員の環境でアカウント取得が
    /// 空振りし続ける（実行ファイルが無く、5 秒ごとに起動を試みる）。
    /// **`"on"` 以外は全部 off** ＝ 綴り違いで黙って有効にならない
    #[test]
    fn an_optional_agent_shows_up_only_when_the_setting_says_on() {
        let fixed = |value: Option<&str>| {
            let value = value.map(str::to_string);
            move |_: &str| value.clone()
        };
        assert_eq!(Kind::enabled(fixed(None)), [Kind::Claude], "the default is not claude only");
        assert_eq!(Kind::enabled(fixed(Some(ON))), Kind::ORDER, "\"on\" did not add the agent");
        // 綴り違い・空・off はすべて出さない側（曖昧な値で黙って有効にしない）
        for value in ["", "off", "ON", "true", "yes", "on "] {
            assert_eq!(
                Kind::enabled(fixed(Some(value))),
                [Kind::Claude],
                "{value:?} turned an optional agent on"
            );
        }
        // **claude は設定で消せない**（無ければ ccdesk が何もできない）
        assert!(
            !Kind::OPTIONAL.contains(&Kind::Claude),
            "claude became switchable, so a setting could leave ccdesk with no agent"
        );
        // キーは agent の綴りそのもの（`"codex"`）＝ 設定の綴りを別に持たない
        let by_key = |key: &str| (key == Kind::Codex.as_str()).then(|| ON.to_string());
        assert_eq!(Kind::enabled(by_key), Kind::ORDER);
    }

    /// **行 ID は `CCDESK_ROW` にしか出ない。** hook はこの env でしか
    /// 「どの行の出来事か」を知れず、立て忘れた agent の行は無音で状態を失う
    /// （起動は成功し、エラーも出ない）。だから全 kind をここでまとめて固定する。
    ///
    /// 併せて **argv へ漏れていない**ことも見る: 行 ID を引数に出すと、その値が
    /// agent 側の世界（transcript 名・会話 ID）へ流れ込み、行と会話をもう一度
    /// 結び付けてしまう
    #[test]
    fn every_kind_hands_the_row_id_over_through_the_environment_and_nowhere_else() {
        let row = SessionId::new("11111111-1111-4111-8111-111111111111");
        for kind in Kind::ORDER {
            for launch in [Launch::New { prompt: "" }, Launch::Resume { id: "conv" }, Launch::Pick] {
                let cmd = kind.spawn_command(&row, "C:\\dev\\app", launch, None).cmd;
                let found = cmd
                    .iter_full_env_as_str()
                    .find(|(key, _)| *key == crate::hooks::ROW_ENV)
                    .map(|(_, value)| value.to_string());
                assert_eq!(found.as_deref(), Some(row.as_str()), "{kind:?} does not hand over the row id");
                assert!(
                    !argv(&cmd).iter().any(|arg| arg.contains(row.as_str())),
                    "{kind:?} leaked the row id into its arguments: {:?}",
                    argv(&cmd)
                );
            }
        }
    }

    #[test]
    fn an_unknown_spelling_is_not_guessed_at() {
        assert_eq!(Kind::parse("claude"), Some(Kind::Claude));
        assert_eq!(Kind::parse("codex"), Some(Kind::Codex));
        assert_eq!(Kind::parse("Claude"), None);
        assert_eq!(Kind::parse(""), None);
    }

    /// 取り違えたまま気づかないのを防ぐ（`Kind::Codex.backend()` が claude を
    /// 返しても、行の状態が付かなくなるまで誰も気づかない）。
    /// **観測できるもの（起こす実行ファイル名）で見る**
    #[test]
    fn every_kind_resolves_to_a_backend_that_launches_its_own_program() {
        for kind in Kind::ORDER {
            let cmd = kind
                .spawn_command(
                    &SessionId::new("row"),
                    "C:\\dev\\app",
                    Launch::New { prompt: "" },
                    None,
                )
                .cmd;
            // **拡張子とディレクトリは環境で変わる**（PATH の解決を通すので、
            // 入っていれば `C:\…\codex.cmd`、入っていなければ `codex`）。
            // 見るのは「どの名前の実行ファイルを起こすか」だけ
            let program = std::path::PathBuf::from(cmd.get_argv()[0].to_string_lossy().to_string());
            let stem = program
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            assert_eq!(stem, kind.as_str(), "{kind:?} launches the wrong program");
        }
    }
}
