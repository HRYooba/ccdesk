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
//! 設計の背景は `docs/codex-support.md`。

pub(crate) mod claude;
pub(crate) mod codex;
pub(crate) mod codex_index;

use portable_pty::CommandBuilder;

use crate::sessions::SessionId;

/// 起動の種類。**新規と再開でコマンドラインが違う**ことだけをここに持たせる
/// （どちらを使うかを決めるのは呼び出し側 ＝ 会話の記録があるか）
pub(crate) enum Launch<'a> {
    /// 新規セッション。`prompt` は最初のメッセージ（空なら渡さない）
    New { prompt: &'a str },
    /// 既存セッションの再開。**cwd の一致が必須**（別 cwd からは会話が見つからない
    /// ＝ 記録が在る作業ツリーで開く。判断は
    /// [`crate::title::Titles::resume_cwd`]）。
    /// **会話の記録が無い行には使えない**
    Resume {
        /// 再開に使う ID。**行の ID とは限らない。** claude は ccdesk が採番した
        /// 値をそのまま使うので一致するが、codex は codex 自身が採番した値で、
        /// ccdesk の行 ID では会話が見つからない
        id: &'a str,
    },
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

/// どの agent の行か。**保存と表示の綴りをここ 1 箇所が持つ**
/// （[`crate::poll::State`] と同じ作り: 語彙の正本を 1 つにし、2 つの顔を生やす）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default, Hash)]
pub(crate) enum Kind {
    #[default]
    Claude,
    Codex,
}

impl Kind {
    /// 表示順（grouping の節・New 画面の切替行・メニューの並びもこれに従う）
    pub(crate) const ORDER: [Self; 2] = [Self::Claude, Self::Codex];

    /// [`Self::tag`] の桁数。**全 kind で同じ**（`every_tag_is_the_same_width` が
    /// 固定する）。サイドバーの桁予算がこの値に乗る
    pub(crate) const TAG_COLS: usize = 4;

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
    /// **記号は使わない。** claude 公式の印は `✻`（1 桁）、codex 公式の印は `>_`
    /// （2 桁）で幅が揃わず、列が崩れる。両方を 1 画面に並べる慣習も見当たらない。
    /// 加えて ccdesk は状態アイコン（かつての `✻`/`✽`/`∙`）を廃止した経緯があり、
    /// 同じ記号を別の意味で復活させると読み手の中で衝突する
    pub(crate) fn title(self) -> &'static str {
        self.as_str()
    }

    /// サイドバーの行に出す略記。**幅が足りないのはここだけ**なので、
    /// 略記もここだけ（[`Self::title`] が入らない場所の代替）。
    /// **全 kind で同じ桁**（揃っていないと行ごとに名前の開始位置がずれる）
    pub(crate) fn tag(self) -> &'static str {
        match self {
            Self::Claude => "[cc]",
            Self::Codex => "[cx]",
        }
    }

    /// この kind の実装。**`&'static` にしてある**ので、行やコマンドを組む側は
    /// 寿命を気にせず持ち回せる
    pub(crate) fn backend(self) -> &'static dyn Backend {
        match self {
            Self::Claude => &claude::Claude,
            Self::Codex => &codex::Codex,
        }
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
    /// 起こす ＝ 行の状態が縮退するだけで、セッション自体は動く
    fn command(
        &self,
        session_id: &SessionId,
        cwd: &str,
        launch: Launch<'_>,
        inject: Option<&Inject>,
    ) -> CommandBuilder;
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

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

    /// 略記は**サイドバーの桁予算に乗る**（`MIN_NAME_COLS` の根拠）ので、
    /// 長さが揃っていないと行ごとに名前の開始位置がずれる
    #[test]
    fn every_tag_is_the_same_width() {
        let widths: Vec<usize> = Kind::ORDER
            .iter()
            .map(|k| unicode_width::UnicodeWidthStr::width(k.tag()))
            .collect();
        assert!(
            widths.iter().all(|w| *w == Kind::TAG_COLS),
            "a tag is not {} columns wide: {widths:?}",
            Kind::TAG_COLS
        );
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
            let cmd = kind.backend().command(
                &SessionId::new("row"),
                "C:\\dev\\app",
                Launch::New { prompt: "" },
                None,
            );
            let program = cmd.get_argv()[0].to_string_lossy().to_string();
            assert_eq!(program, kind.as_str(), "{kind:?} launches the wrong program");
        }
    }
}
