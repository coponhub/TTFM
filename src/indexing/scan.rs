use crate::db::{Tbl};
use crate::functions::{ScanEntry};
use crate::util::{ExecuteSql, ParquetExt, TableCreateExt, SafeMetadata};
use anyhow::Result;
use duckdb::{Connection, Appender};
use sea_query::{Query, Expr, Table, Iden};
use std::path::{Path, PathBuf};

// ========================================================
// Scan Phase Orchestrator
// ========================================================

pub(crate) fn run_scan<F>(
    conn: &Connection,
    db_dir: &Path,
    temp_scan_path: &Path,
    root_path: &Path,
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
        dry_run,
        on_progress.map(|f| f as _),
    );

    scanner.prepare_tray()?;

    let count = std::thread::scope(|s| {
        scanner.scan(s);
        scanner.write(rx)
    })?;

    scanner.finalize_table(temp_scan_path)?;

    Ok(count)
}

// ========================================================
// File Scanner Implementation
// ========================================================

pub(crate) struct FileScanner<'a> {
    pub(crate) conn: &'a Connection,
    pub(crate) db_dir: PathBuf,
    pub(crate) dry_run: bool,
    pub(crate) on_progress: Option<&'a (dyn Fn(usize) + Sync + Send)>,
    pub(crate) walker: Option<ignore::WalkParallel>,
    pub(crate) tx: Option<std::sync::mpsc::Sender<ScanEntry>>,
}

impl<'a> FileScanner<'a> {
    pub(crate) fn new(
        conn: &'a Connection,
        root_path: PathBuf,
        db_dir: PathBuf,
        dry_run: bool,
        on_progress: Option<&'a (dyn Fn(usize) + Sync + Send)>,
    ) -> (Self, std::sync::mpsc::Receiver<ScanEntry>) {
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

        Table::create()
            .table(Tbl::Scan)
            .temporary()
            .add_columns(ScanEntry::columns_with_type())
            .execute(self.conn)
    }

    pub(crate) fn scan<'s, 'e>(&mut self, s: &'s std::thread::Scope<'s, 'e>) {
        let walker = self.walker.take().expect("Walker already consumed");
        let tx = self.tx.take().expect("Sender already consumed");
        let db_dir = self.db_dir.clone();

        s.spawn(move || {
            let factory = move || ScanWalker::create(tx.clone(), db_dir.clone());
            walker.run(factory);
        });
    }

    pub(crate) fn write(&self, rx: std::sync::mpsc::Receiver<ScanEntry>) -> Result<usize> {
        let mut current_count = 0;
        let mut appender: Option<Appender<'_>> = if !self.dry_run {
            let table_name = Tbl::Scan.to_string().replace('"', "");
            Some(self.conn.appender(&table_name)?)
        } else {
            None
        };

        for entry in rx {
            if let Some(ref mut app) = appender {
                app.append_row(&*entry.as_params())?;
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

    pub(crate) fn finalize_table(&self, path: &Path) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }

        Query::select()
            .expr(Expr::cust("*"))
            .from(Tbl::Scan)
            .save_parquet(self.conn, path)?;

        Table::drop().table(Tbl::Scan).execute(self.conn).ok();
        Ok(())
    }
}

// --- Walker Implementation ---

pub(crate) struct ScanWalker {
    pub(crate) tx: std::sync::mpsc::Sender<ScanEntry>,
    pub(crate) db_dir: PathBuf,
}

impl ScanWalker {
    pub(crate) fn create(
        tx: std::sync::mpsc::Sender<ScanEntry>,
        db_dir: PathBuf,
    ) -> Box<dyn FnMut(Result<ignore::DirEntry, ignore::Error>) -> ignore::WalkState + Send> {
        let mut walker = Self { tx, db_dir };
        Box::new(move |res| walker.visit(res))
    }

    pub(crate) fn is_db_dir(&self, path: &Path) -> bool {
        path.canonicalize()
            .map(|p| p.starts_with(&self.db_dir))
            .unwrap_or(false)
    }

    fn try_create_entry(
        &self,
        res: Result<ignore::DirEntry, ignore::Error>,
    ) -> Option<ScanEntry> {
        let e = res.ok()?;
        if self.is_db_dir(e.path()) {
            return None;
        }

        let m = match e.metadata() {
            Ok(real_m) => SafeMetadata::new(&real_m),
            Err(err) if crate::util::is_not_found_err(&err) => return None,
            Err(_) => SafeMetadata::recovered(),
        };

        ScanEntry::from_path_metadata(e.path(), &m).ok()
    }

    fn visit(&mut self, res: Result<ignore::DirEntry, ignore::Error>) -> ignore::WalkState {
        let Some(entry) = self.try_create_entry(res) else {
            return ignore::WalkState::Continue;
        };

        if self.tx.send(entry).is_err() {
            return ignore::WalkState::Quit;
        }

        ignore::WalkState::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_scan_walker_is_db_dir() {
        let dir = tempdir().unwrap();
        let db_dir = dir.path().join(".ttfm/db");
        std::fs::create_dir_all(&db_dir).unwrap();
        
        let db_file = db_dir.join("entities.parquet");
        std::fs::write(&db_file, "data").unwrap();
        
        let normal_file = dir.path().join("normal.txt");
        std::fs::write(&normal_file, "content").unwrap();
        
        let walker = ScanWalker { 
            tx: std::sync::mpsc::channel().0, 
            db_dir: db_dir.canonicalize().unwrap() 
        };
        
        assert!(walker.is_db_dir(&db_file));
        assert!(!walker.is_db_dir(&normal_file));
    }
}