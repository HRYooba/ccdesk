//! git の作業ツリーの列挙。**読むのは git のリポジトリレイアウトだけ**
//! （`gitrepository-layout` に文書化された `.git/worktrees/<名前>/gitdir` と
//! `commondir`）で、`git` コマンドは起こさない: これはセッション 1 本ごとに
//! 呼ばれうる問い合わせで、子プロセスを起こすと 1 回あたり数十 ms かかる。
//!
//! **なぜ ccdesk がこれを知る必要があるか**: claude のセッションは走行中に
//! git worktree へ移れて（`EnterWorktree`）、移ると transcript も移動先の cwd から
//! 導かれるディレクトリへ移る。**移動の記録は移った先のファイルの中にしかない**
//! （元の場所に印は残らない・実測）ので、探すには「この cwd の作業ツリーはどれか」を
//! 先に知る必要がある。これは `claude -r` が会話を探す範囲と同じ規則でもある。
//!
//! 縮退: レイアウトが読めなければ空を返す ＝ 移った会話が見つからないだけで、
//! 行そのものは残る（表示名が既定へ落ちる）。

use std::path::{Path, PathBuf};

/// `cwd` が属するリポジトリの**すべての作業ツリー**（主ツリーと各 worktree）。
/// リポジトリの外・読めないときは空。
///
/// 並びは「主ツリー → `.git/worktrees` の並び」で、同じ入力なら同じ順になる
pub(crate) fn worktrees_of(cwd: &str) -> Vec<PathBuf> {
    // **絶対パスだけを見る。** 相対パスを上へ辿ると、行の cwd ではなく
    // ccdesk 自身のカレントディレクトリのリポジトリを答えてしまう
    let root = Path::new(cwd);
    if !root.is_absolute() {
        return Vec::new();
    }
    let Some(common) = common_git_dir(root) else {
        return Vec::new();
    };
    // 主ツリーは `.git` の親（bare リポジトリには作業ツリーが無いので None）
    let mut trees: Vec<PathBuf> = common.parent().map(Path::to_path_buf).into_iter().collect();
    let Ok(entries) = std::fs::read_dir(common.join("worktrees")) else {
        return trees;
    };
    let mut linked: Vec<PathBuf> = entries
        .flatten()
        // `<name>/gitdir` は作業ツリーの `.git`（ファイル）の場所 ＝ その親が作業ツリー
        .filter_map(|entry| read_path(&entry.path().join("gitdir")))
        .filter_map(|git_file| git_file.parent().map(Path::to_path_buf))
        .collect();
    linked.sort();
    trees.append(&mut linked);
    trees
}

/// `path` から上へ辿って見つかる `.git` を、**主リポジトリの `.git` ディレクトリ**へ
/// 解決する。作業ツリーの `.git` は `gitdir: <path>` を書いたファイルで、その先の
/// `commondir` が主リポジトリの `.git` への相対パスを持つ
fn common_git_dir(path: &Path) -> Option<PathBuf> {
    let marker = path.ancestors().map(|dir| dir.join(".git")).find(|p| p.exists())?;
    if marker.is_dir() {
        return Some(marker);
    }
    // `gitdir: <path>` ＝ `<主リポジトリ>/.git/worktrees/<名前>`
    let text = std::fs::read_to_string(&marker).ok()?;
    let linked = PathBuf::from(text.strip_prefix("gitdir:")?.trim());
    let common = read_path(&linked.join("commondir"))?;
    let joined = if common.is_absolute() {
        common
    } else {
        linked.join(common)
    };
    // `../..` を含んだままだと名前の突き合わせに使えないので畳む
    // （`canonicalize` は UNC 表記（`\\?\`）を混ぜるので使わない）
    Some(normalize(&joined))
}

/// 1 行のパスを書いたファイルを読む（前後の空白・改行は落とす）。
///
/// **区切りは `\` へ揃える**: git は Windows でも `/` 区切りで書くが、返した値は
/// transcript のディレクトリ名の材料であり `claude -r` の起動先にもなる。
/// 表記が混ざると、同じフォルダなのに別の文字列として画面や保存値に現れる
/// （[`ccdesk::dir_key`] が吸収する範囲の話を、出どころで 1 つに揃えておく）
fn read_path(path: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(path).ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| PathBuf::from(trimmed.replace('/', "\\")))
}

/// `.` と `..` を辿らずに畳む（ディスクを触らない ＝ 消えた作業ツリーでも計算できる）
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // fixture は title 側と共有する（`.git/worktrees` のレイアウト解釈を直すとき、
    // 追随すべき fixture が 2 つあると片方だけ古い形のまま通ってしまう）
    use crate::title::tests::TempRepo;

    fn names(trees: &[PathBuf]) -> Vec<String> {
        trees
            .iter()
            .map(|p| p.display().to_string().replace('/', "\\").to_lowercase())
            .collect()
    }

    /// 主ツリーからも作業ツリーからも、**同じ一覧**が返る
    /// （どちらの cwd で起こしたセッションでも同じ範囲を探せる）
    #[test]
    fn every_worktree_of_the_repository_is_listed_from_any_of_them() {
        let repo = TempRepo::new("every_worktree_of_the_repository_is_listed");
        let a = PathBuf::from(repo.add_worktree("fix+one"));
        let b = PathBuf::from(repo.add_worktree("docs+two"));

        let from_main = names(&worktrees_of(&repo.root().display().to_string()));
        assert_eq!(
            from_main,
            names(&[repo.root().to_path_buf(), b.clone(), a.clone()]),
            "the main tree and both worktrees must be listed"
        );
        // 作業ツリーの中から見ても同じ（`.git` がファイルの側）
        assert_eq!(names(&worktrees_of(&a.display().to_string())), from_main);
    }

    /// リポジトリでない場所・作業ツリーが 1 本も無いリポジトリでも落ちない
    #[test]
    fn a_directory_outside_a_repository_lists_nothing() {
        let outside = std::env::temp_dir().join("ccdesk-git-not-a-repo");
        let _ = std::fs::create_dir_all(&outside);
        // 一時ディレクトリの親が git 管理下でないことを前提にできないので、
        // 「見つかっても自分自身は含まない」ではなく「落ちない」ことだけを見る
        let _ = worktrees_of(&outside.display().to_string());
        assert!(worktrees_of("").is_empty(), "an empty path must not resolve to a repository");

        let repo = TempRepo::new("a_directory_outside_a_repository_lists_nothing");
        assert_eq!(
            names(&worktrees_of(&repo.root().display().to_string())),
            names(std::slice::from_ref(&repo.root().to_path_buf())),
            "a repository with no linked worktree is just its main tree"
        );
    }

    /// `..` を含む `commondir` を畳む（畳まないと名前の突き合わせに使えない）
    #[test]
    fn a_relative_common_dir_is_folded() {
        assert_eq!(
            normalize(Path::new("C:\\a\\b\\.git\\worktrees\\w\\..\\..")),
            PathBuf::from("C:\\a\\b\\.git")
        );
        assert_eq!(normalize(Path::new("C:\\a\\.\\b")), PathBuf::from("C:\\a\\b"));
    }
}
