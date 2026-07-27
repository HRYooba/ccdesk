//! 知らない引数を渡したとき、**stdout に 1 バイトも出さない**ことを検証する。
//!
//! # なぜ機械で止めるか
//!
//! ccdesk は「出力を読み取る道具」として呼ばれる経路を持つ。実際に踏みうるのは
//! 旧版が `--settings` へ注入した `statusLine`（`ccdesk statusline-hook`）で、
//! その設定を抱えた claude セッションは ccdesk を更新した後もそれを呼び続ける。
//! ここで stdout にヘルプ本文を出すと、**ユーザーの statusline 行にヘルプが並ぶ**。
//!
//! stderr へ出しておけば stdout は空になり、行が空欄になるだけで済む
//! （セッションを開き直せば注入ファイルごと消える）。
//!
//! 出力先はコードを読んでも取り違えやすく（`println!` と `eprintln!` は 1 文字違い）、
//! 壊れても TUI のテストには一切現れない。**実際にプロセスを起こして stdout を
//! 数える**のがこの不変条件を守る唯一の形。

use std::path::PathBuf;
use std::process::Command;

/// テスト対象の実行ファイル。`CARGO_BIN_EXE_<name>` は cargo がテスト時に
/// 用意する環境変数で、ビルド済みバイナリの絶対パスが入る
fn ccdesk() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_ccdesk"))
}

/// 知らない引数は **stdout を汚さず** stderr へ案内し、非ゼロで終わる
#[test]
fn an_unknown_argument_writes_nothing_to_stdout() {
    // 旧版が注入していた呼び方をそのまま使う（この経路が現に踏まれるため）
    for argument in ["statusline-hook", "--nope", "typo"] {
        let out = Command::new(ccdesk())
            .arg(argument)
            .output()
            .expect("could not run ccdesk");

        assert!(
            out.stdout.is_empty(),
            "{argument}: wrote {} byte(s) to stdout: {:?}",
            out.stdout.len(),
            String::from_utf8_lossy(&out.stdout)
        );
        // 案内そのものは出す（黙って終わると、打ち間違えた人に何も伝わらない）
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(argument),
            "{argument}: stderr does not name the argument: {stderr:?}"
        );
        assert!(
            stderr.contains("Usage:"),
            "{argument}: stderr has no usage text: {stderr:?}"
        );
        assert_eq!(out.status.code(), Some(2), "{argument}: unexpected exit code");
    }
}

/// **`--help` は stdout のまま。** 求められた出力を stderr へ移してしまうと、
/// `ccdesk --help | less` のような普通の使い方が壊れる
#[test]
fn explicit_help_still_goes_to_stdout() {
    for argument in ["--help", "-h", "help"] {
        let out = Command::new(ccdesk())
            .arg(argument)
            .output()
            .expect("could not run ccdesk");

        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("Usage:"),
            "{argument}: usage text is not on stdout: {stdout:?}"
        );
        assert!(out.status.success(), "{argument}: should exit zero");
    }
}
