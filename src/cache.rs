// Copyright (C) 2026 The TTFM Project Contributors
// See the CONTRIBUTORS file at the top-level directory of this distribution
// for a list of copyright holders.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use crate::types::Progress;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// キャッシュファイルのメタデータキー
pub const META_QUERY: &str = "ttfm.query";
pub const META_INDEX_VERSION: &str = "ttfm.index_version";
pub const META_CREATED_AT: &str = "ttfm.created_at";

/// 検索結果のキャッシュ（ResultCache）を管理する構造体。
pub struct CacheManager {
    cache_dir: PathBuf,
    max_size_bytes: i64,
}

impl CacheManager {
    pub fn new(cache_dir: PathBuf, max_size_bytes: i64) -> Self {
        if !cache_dir.exists() {
            let _ = std::fs::create_dir_all(&cache_dir);
        }
        Self {
            cache_dir,
            max_size_bytes,
        }
    }

    /// 指定された CID に対応するキャッシュファイルのパスを返します。
    pub fn path_for(&self, cid: &str) -> PathBuf {
        self.cache_dir.join(format!("{}.parquet", cid))
    }

    /// キャッシュディレクトリ内の古いファイルを削除し、合計サイズを制限以下に保ちます（LRU）。
    pub fn cleanup(&self) -> Result<()> {
        let mut files = Vec::new();
        let entries = std::fs::read_dir(&self.cache_dir)?;

        for entry in entries {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_file()
                && entry.path().extension().and_then(|s| s.to_str())
                    == Some("parquet")
            {
                let accessed = metadata.accessed().unwrap_or_else(|_| {
                    metadata
                        .modified()
                        .unwrap_or_else(|_| std::time::SystemTime::now())
                });
                files.push((entry.path(), metadata.len(), accessed));
            }
        }

        // アクセス日時が古い順にソート
        files.sort_by_key(|f| f.2);

        let total_size: u64 = files.iter().map(|f| f.1).sum();
        let mut current_size = total_size;

        for (path, size, _) in files {
            if current_size <= self.max_size_bytes as u64 {
                break;
            }
            std::fs::remove_file(&path)
                .context(format!("Failed to remove old cache: {:?}", path))?;
            current_size -= size;
        }

        Ok(())
    }

    /// キャッシュファイルの進捗状況（メタデータ）を確認します。
    /// キャッシュファイルの進捗状況（メタデータ）を確認します。
    pub fn get_progress(&self, cid: &str) -> Result<Progress> {
        let path = self.path_for(cid);
        // 生成中の一時的な .tmp ファイルがあれば、まだ完了していない
        // path.exists() より先にチェックしないと、生成中に "存在しない(=default)" と判定されてしまう
        let tmp_path = format!("{}.tmp", path.to_string_lossy());
        if Path::new(&tmp_path).exists() {
            return Ok(Progress {
                current: 0,
                total: None,
                is_done: false,
            });
        }

        if !path.exists() {
            return Ok(Progress::default());
        }

        // 完了済みなら全件数を取得
        Ok(Progress {
            current: 1, // ダミー
            total: Some(1),
            is_done: true,
        })
    }

    /// 全てのキャッシュファイルを物理的に削除します。
    pub fn clear(&self) -> Result<()> {
        if self.cache_dir.exists() {
            std::fs::remove_dir_all(&self.cache_dir)?;
            std::fs::create_dir_all(&self.cache_dir)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_cache_path_for() {
        let dir = tempdir().unwrap();
        let cm = CacheManager::new(dir.path().to_path_buf(), 1024);
        let path = cm.path_for("test-cid");
        assert!(path.to_string_lossy().contains("test-cid.parquet"));
    }

    #[test]
    fn test_cache_cleanup_lru() {
        let dir = tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        // 100バイト制限
        let cm = CacheManager::new(cache_dir.clone(), 100);

        // ファイル作成 (1つ 60バイト)
        let f1 = cache_dir.join("a.parquet");
        let f2 = cache_dir.join("b.parquet");
        fs::write(&f1, vec![0u8; 60]).unwrap();

        // 少し時間を空けるか、アクセス時刻を細工する（ここでは単純に作成順）
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(&f2, vec![0u8; 60]).unwrap();

        // 合計120バイトなので、古い f1 が消えるはず
        cm.cleanup().unwrap();

        assert!(!f1.exists(), "Oldest file should be removed");
        assert!(f2.exists(), "Newer file should be kept");
    }

    #[test]
    fn test_cache_get_progress() {
        let dir = tempdir().unwrap();
        let cm = CacheManager::new(dir.path().to_path_buf(), 1024);
        let cid = "test-progress";
        let path = cm.path_for(cid);
        let tmp_path = format!("{}.tmp", path.to_string_lossy());

        // 1. 存在しない場合 -> デフォルト (0/None)
        let prog = cm.get_progress(cid).unwrap();
        assert_eq!(prog.current, 0);
        assert_eq!(prog.total, None);

        // 2. .tmp ファイルがある場合 -> 生成中 (0/None)
        fs::write(&tmp_path, "generating").unwrap();
        let prog = cm.get_progress(cid).unwrap();
        assert_eq!(prog.current, 0);
        assert_eq!(prog.total, None);

        // 3. 完了済みの場合 (.parquetのみ)
        fs::remove_file(&tmp_path).unwrap();
        fs::write(&path, "finished").unwrap();
        let prog = cm.get_progress(cid).unwrap();
        assert_eq!(prog.current, 1);
        assert_eq!(prog.total, Some(1));
    }

    #[test]
    fn test_cache_clear() {
        let dir = tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        let cm = CacheManager::new(cache_dir.clone(), 1024);

        let f1 = cache_dir.join("a.parquet");
        fs::write(&f1, "data").unwrap();
        assert!(f1.exists());

        cm.clear().unwrap();
        assert!(!f1.exists(), "Cache should be cleared");
        assert!(cache_dir.exists(), "Cache directory should be recreated");
    }

    #[test]
    fn test_cache_cleanup_ignores_non_parquet() {
        let dir = tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        // 10バイト制限
        let cm = CacheManager::new(cache_dir.clone(), 10);

        let f_parquet = cache_dir.join("a.parquet");
        let f_other = cache_dir.join("important.txt");

        // 20バイトのファイルをそれぞれ作成
        fs::write(&f_parquet, vec![0u8; 20]).unwrap();
        fs::write(&f_other, vec![0u8; 20]).unwrap();

        cm.cleanup().unwrap();

        // パケットファイルはサイズオーバーで削除されるはずだが、他は無視されるべき
        assert!(!f_parquet.exists(), "Parquet should be cleaned up");
        assert!(
            f_other.exists(),
            "Non-parquet file should be ignored by cleanup"
        );
    }
}
