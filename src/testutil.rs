//! テスト共通の一時ディレクトリ。
//!
//! **「テストは実ユーザーの `~/.ccdesk` / `~/.claude` を絶対に触らない」という
//! 規律の実装を 1 つ**にする。各テストモジュールが同じ定型（テスト名 + pid +
//! 連番で一意・Drop で丸ごと消す）を別々に書いていた頃は 9 実装あり、
//! 1 箇所書き漏らせば開発者のホームを踏んだ。
//! ドメインの知識（どのファイル名を置くか・何を組むか）は各モジュールの
//! ラッパーが持ち、ここは「安全な置き場」だけを提供する。
//!
//! lib 側（`ccdesk` クレート）のテストからは見えない（bin のテスト専用。
//! lib のテストは自前の `temp_json` を持つ）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

pub(crate) struct TempDir(PathBuf);

impl TempDir {
    /// `prefix` は所属モジュール、`test` はテスト名。pid + 連番で
    /// 並列実行・別チェックアウトの同時実行とも衝突しない。
    /// Drop で丸ごと消すので、アサート失敗でパニックしても残らない
    pub(crate) fn new(prefix: &str, test: &str) -> Self {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "ccdesk-{prefix}-{test}-{}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("mkdir failed");
        Self(root)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    pub(crate) fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
