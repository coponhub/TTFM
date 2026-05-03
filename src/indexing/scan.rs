use super::indexer::{calc_scanhash, ScanHash, TempScanEntry};
use crate::db::{Col, Tbl};
use crate::indexing::functions::ScanEntry;
use crate::types::ItemId;
use crate::util::{ExecuteSql, ParquetExt, SafeMetadata, TableCreateExt};
use anyhow::Result;
use duckdb::Connection;
use rustc_hash::FxHashMap;
use sea_query::{Expr, Iden, Query, Table};
use std::path::{Path, PathBuf};

// ========================================================
// Scan Phase Orchestrator
// ========================================================

/// スキャンスレッドからメインスレッドへ送られるメッセージ。
pub(crate) enum ScanMessage {
    /// 新規・変更あり：フルデータを一時テーブルに書く。
    Found(TempScanEntry),
    /// 変更なし：生存している既存の Item ID を一時テーブルに書く。
    Live(ItemId),
}

pub(crate) fn run_scan<F>(
    conn: &Connection,
    db_dir: &Path,
    temp_scan_path: &Path,
    temp_live_path: &Path,
    root_path: &Path,
    cache: &FxHashMap<ScanHash, ItemId>,
    on_progress: Option<&F>,
    dry_run: bool,
) -> Result<usize>
where
    F: Fn(usize) + Sync + Send,
{
    let (mut scanner, rx) = FileScanner::new(
        conn,
        root_path.to_path_buf(),
        db_dir.to_path_buf(),
        cache,
        dry_run,
        on_progress.map(|f| f as _),
    );

    scanner.prepare_tray()?;

    let count = std::thread::scope(|s| {
        scanner.scan(s);
        scanner.write(rx)
    })?;

    scanner.finalize_tables(temp_scan_path, temp_live_path)?;

    Ok(count)
}

// ========================================================
// File Scanner Implementation
// ========================================================

pub(crate) struct FileScanner<'a> {
    pub(crate) conn: &'a Connection,
    pub(crate) db_dir: PathBuf,
    pub(crate) cache: &'a FxHashMap<ScanHash, ItemId>,
    pub(crate) dry_run: bool,
    pub(crate) on_progress: Option<&'a (dyn Fn(usize) + Sync + Send)>,
    pub(crate) walker: Option<ignore::WalkParallel>,
    pub(crate) tx: Option<std::sync::mpsc::Sender<ScanMessage>>,
}

impl<'a> FileScanner<'a> {
    pub(crate) fn new(
        conn: &'a Connection,
        root_path: PathBuf,
        db_dir: PathBuf,
        cache: &'a FxHashMap<ScanHash, ItemId>,
        dry_run: bool,
        on_progress: Option<&'a (dyn Fn(usize) + Sync + Send)>,
    ) -> (Self, std::sync::mpsc::Receiver<ScanMessage>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let walker = ignore::WalkBuilder::new(root_path)
            .hidden(false)
            .git_ignore(true)
            .threads(rayon::current_num_threads())
            .build_parallel();

        let db_dir = db_dir.canonicalize().unwrap_or(db_dir);

        (
            Self {
                conn,
                db_dir: db_dir.clone(),
                cache,
                on_progress,
                dry_run,
                walker: Some(walker),
                tx: Some(tx),
            },
            rx,
        )
    }

    pub(crate) fn prepare_tray(&self) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }

        // 変更ありエントリ用テーブル
        Table::create()
            .table(Tbl::Scan)
            .temporary()
            .add_columns(TempScanEntry::columns_with_type())
            .execute(self.conn)?;

        // 生存 ID 用テーブル (item_id カラムのみ)
        Table::create()
            .table(Tbl::Live)
            .temporary()
            .col(sea_query::ColumnDef::new(Col::ItemId).big_integer())
            .execute(self.conn)?;

        Ok(())
    }

    pub(crate) fn scan<'s, 'e>(&mut self, s: &'s std::thread::Scope<'s, 'e>)
    where
        'a: 's,
    {
        let walker = self.walker.take().expect("Walker already consumed");
        let tx = self.tx.take().expect("Sender already consumed");
        let db_dir = self.db_dir.clone();
        let cache = self.cache;

        s.spawn(move || {
            // ignore クレートの API に合わせ、Visitor (クロージャ) を生成する工場を渡す
            walker.run(|| {
                let tx = tx.clone();
                let db_dir = db_dir.clone();
                Box::new(move |res| process_entry(res, &db_dir, cache, &tx))
            });
            // 走査スレッドが終了した際、キャプチャしていた tx を明示的にドロップ。
            // これにより、チャンネルの送信機が全て消え、受信側(write)がループを抜けられる。
            drop(tx);
        });
    }

    pub(crate) fn write(
        &self,
        rx: std::sync::mpsc::Receiver<ScanMessage>,
    ) -> Result<usize> {
        let mut current_count = 0;
        let mut app_scan = (!self.dry_run)
            .then(|| {
                self.conn.appender(&Tbl::Scan.to_string().replace('"', ""))
            })
            .transpose()?;
        let mut app_live = (!self.dry_run)
            .then(|| {
                self.conn.appender(&Tbl::Live.to_string().replace('"', ""))
            })
            .transpose()?;

        for msg in rx {
            match msg {
                ScanMessage::Found(temp_entry) => {
                    if let Some(ref mut app) = app_scan {
                        app.append_row(&*temp_entry.params())?;
                    }
                }
                ScanMessage::Live(id) => {
                    if let Some(ref mut app) = app_live {
                        app.append_row(&[&id])?;
                    }
                }
            }

            current_count += 1;
            if let Some(cb) = self.on_progress {
                if current_count % 1000 == 0 {
                    cb(current_count);
                }
            }
        }

        if let Some(cb) = self.on_progress {
            cb(current_count);
        }

        Ok(current_count)
    }

    pub(crate) fn finalize_tables(
        &self,
        scan_path: &Path,
        live_path: &Path,
    ) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }

        // Tbl::Scan を Parquet に保存
        Query::select()
            .expr(Expr::cust("*"))
            .from(Tbl::Scan)
            .save_parquet(self.conn, scan_path)?;

        // Tbl::Live を Parquet に保存
        Query::select()
            .expr(Expr::cust("*"))
            .from(Tbl::Live)
            .save_parquet(self.conn, live_path)?;

        Table::drop().table(Tbl::Scan).execute(self.conn).ok();
        Table::drop().table(Tbl::Live).execute(self.conn).ok();
        Ok(())
    }
}

// --- Helper Functions for Scanning ---

/// 個別のファイルエントリを処理する独立した関数。
/// ネストを浅く保ち、テスタビリティを向上させるために scan メソッドから切り出されました。
fn process_entry(
    res: Result<ignore::DirEntry, ignore::Error>,
    db_dir: &Path,
    cache: &FxHashMap<ScanHash, ItemId>,
    tx: &std::sync::mpsc::Sender<ScanMessage>,
) -> ignore::WalkState {
    let e = match res {
        Ok(entry) => entry,
        Err(_) => return ignore::WalkState::Continue,
    };

    // DBディレクトリ内はスキップ
    if is_db_dir(e.path(), db_dir) {
        return ignore::WalkState::Continue;
    }

    // メタデータの取得
    let m = match e.metadata() {
        Ok(real_m) => SafeMetadata::new(&real_m),
        Err(err) if crate::util::is_not_found_err(&err) => {
            return ignore::WalkState::Continue
        }
        Err(_) => SafeMetadata::recovered(),
    };

    // ScanEntry の作成
    let Ok(entry) = ScanEntry::from_path_metadata(e.path(), &m) else {
        return ignore::WalkState::Continue;
    };

    let hash = calc_scanhash(
        &entry.path.value,
        entry.mtime.value.0,
        entry.size.value.0,
    );

    // ハッシュがキャッシュにあれば生存 ID として報告
    if let Some(id) = cache.get(&hash) {
        if tx.send(ScanMessage::Live(id.clone())).is_err() {
            return ignore::WalkState::Quit;
        }
        return ignore::WalkState::Continue;
    }

    // 変更ありならフルデータを送信
    if tx
        .send(ScanMessage::Found(TempScanEntry { entry, hash }))
        .is_err()
    {
        return ignore::WalkState::Quit;
    }

    ignore::WalkState::Continue
}

/// 指定されたパスが DB ディレクトリ内かどうかを判定します。
fn is_db_dir(path: &Path, db_dir: &Path) -> bool {
    path.canonicalize()
        .map(|p| p.starts_with(db_dir))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_is_db_dir() {
        let dir = tempdir().unwrap();
        let db_dir = dir.path().join(".ttfm/db");
        std::fs::create_dir_all(&db_dir).unwrap();

        let db_file = db_dir.join("entities.parquet");
        std::fs::write(&db_file, "data").unwrap();

        let normal_file = dir.path().join("normal.txt");
        std::fs::write(&normal_file, "content").unwrap();

        let db_dir_abs = db_dir.canonicalize().unwrap();

        assert!(is_db_dir(&db_file, &db_dir_abs));
        assert!(!is_db_dir(&normal_file, &db_dir_abs));
    }
}
