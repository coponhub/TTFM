use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use ttfm::{
    db::Store,
    edit::{edit, EditResponse, QueryType, WriteOptions},
    indexing::Indexer,
    response::Item,
    tag::TagRegistry,
    SearchOptions,
};

fn setup(files: &[&str]) -> (Store, TagRegistry, TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = base.join("files");
    for name in files {
        let p = root.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, *name).unwrap();
    }
    let registry = TagRegistry::with_standard();
    let store = Store::open(base.join("db")).unwrap();
    Indexer::new(&store, &registry).initialize_tables().unwrap();
    Indexer::new(&store, &registry)
        .run(&[&root], None::<&fn(usize)>, false)
        .unwrap();
    (store, registry, dir, root)
}

fn find(store: &Store, registry: &TagRegistry, q: &str) -> Vec<Item> {
    ttfm::search::search_nowarn(store, registry, q, SearchOptions::default())
        .unwrap()
        .results
}

fn run(
    store: &Store,
    registry: &TagRegistry,
    q: &str,
    e: &str,
    t: QueryType,
) -> anyhow::Result<EditResponse> {
    edit(
        store,
        registry,
        q,
        Some(e),
        t,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )
}

fn expected_epoch(local: &str) -> i64 {
    use chrono::TimeZone;
    let naive =
        chrono::NaiveDateTime::parse_from_str(local, "%Y-%m-%dT%H:%M:%S")
            .unwrap();
    chrono::Local
        .from_local_datetime(&naive)
        .unwrap()
        .timestamp()
}

fn second_device_dir() -> Option<PathBuf> {
    let cand = std::env::var("TTFM_TEST_XDEV_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/dev/shm"));
    let dev = |p: &Path| std::fs::metadata(p).map(|m| m.dev()).ok();
    (cand.is_dir() && dev(&cand) != dev(&std::env::temp_dir())).then_some(cand)
}

fn warn_skip() {
    writeln!(
        std::io::stderr(),
        "skipped cross-device test: set TTFM_TEST_XDEV_DIR to a directory on \
         another filesystem, e.g. /dev/shm or a tmpfs mount"
    )
    .ok();
}

#[test]
fn rename_by_stem_moves_the_file_and_updates_the_index() {
    let (store, registry, _d, root) = setup(&["a.txt"]);
    run(
        &store,
        &registry,
        "filename:a.txt",
        "stem:b",
        QueryType::Tag,
    )
    .unwrap();
    assert!(root.join("b.txt").exists() && !root.join("a.txt").exists());
    assert_eq!(find(&store, &registry, "filename:b.txt").len(), 1);
}

#[test]
fn parentdir_change_across_devices_keeps_the_item() {
    let Some(other) = second_device_dir() else {
        return warn_skip();
    };
    let dest = other.join("ttfm_xdev_probe");
    std::fs::remove_dir_all(&dest).ok();
    std::fs::create_dir_all(&dest).unwrap();
    let (store, registry, _d, root) = setup(&["a.txt"]);
    let id = find(&store, &registry, "filename:a.txt")[0].id.clone();
    let mtime = std::fs::metadata(root.join("a.txt")).unwrap().mtime();

    let e = format!("parentdir:{}", dest.display());
    run(&store, &registry, "filename:a.txt", &e, QueryType::Tag).unwrap();

    assert!(dest.join("a.txt").exists() && !root.join("a.txt").exists());
    assert_eq!(
        std::fs::metadata(dest.join("a.txt")).unwrap().mtime(),
        mtime
    );
    assert_eq!(find(&store, &registry, "filename:a.txt")[0].id, id);
    std::fs::remove_dir_all(&dest).ok();
}

#[test]
fn mtime_edit_sets_the_file_attribute_and_reindexes() {
    let (store, registry, _d, root) = setup(&["a.txt"]);
    let resp = run(
        &store,
        &registry,
        "filename:a.txt",
        "mtime:2020-01-01",
        QueryType::Tag,
    )
    .unwrap();
    assert_eq!(resp.fs_ops, 1);
    assert!(
        !resp.has_skipped,
        "has_skipped should be false for successful attr edit"
    );
    let on_disk = std::fs::metadata(root.join("a.txt")).unwrap().mtime();
    assert_eq!(on_disk, expected_epoch("2020-01-01T00:00:00"));
    assert_eq!(find(&store, &registry, "mtime:2020").len(), 1);
}

#[test]
fn colliding_target_is_reported_as_an_error() {
    let (store, registry, _d, root) = setup(&["a.txt", "b.txt"]);
    let err = run(
        &store,
        &registry,
        "filename:a.txt",
        "stem:b",
        QueryType::Tag,
    )
    .unwrap_err();
    assert!(err.to_string().contains("b.txt"));
    assert!(root.join("a.txt").exists());
}

#[test]
fn path_untag_warns_and_reports_skipped_without_deleting() {
    let (store, registry, _d, root) = setup(&["a.txt"]);
    let resp = run(
        &store,
        &registry,
        "filename:a.txt",
        "path:",
        QueryType::Untag,
    )
    .unwrap();
    assert!(resp.has_skipped);
    assert!(root.join("a.txt").exists());
}

#[test]
fn partial_location_untag_is_rejected() {
    let (store, registry, _d, _root) = setup(&["a.txt"]);
    assert!(run(
        &store,
        &registry,
        "filename:a.txt",
        "stem:",
        QueryType::Untag
    )
    .is_err());
    assert!(run(
        &store,
        &registry,
        "filename:a.txt",
        "mtime:",
        QueryType::Untag
    )
    .is_err());
}

#[test]
fn overlapping_location_tags_are_rejected() {
    let (store, registry, _d, root) = setup(&["a.txt"]);
    let e = format!("path:{}/z.txt stem:y", root.display());
    assert!(
        run(&store, &registry, "filename:a.txt", &e, QueryType::Tag).is_err()
    );
}
