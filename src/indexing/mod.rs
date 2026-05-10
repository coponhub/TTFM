pub mod diff;
pub mod indexer;
pub mod merge;
pub mod scan;
pub mod triage;

pub use indexer::Indexer;
pub use indexer::{DynamicRow, TagRow, TaggingResult};

crate::define_scan_entry! {
    path:  crate::tag::PathFn,
    inode: crate::tag::FileIdFn,
    size:  crate::tag::SizeFn,
    mtime: crate::tag::MtimeFn,
}
