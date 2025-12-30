# TTFM (Typed Tag File Manager) Roadmap

## Project Overview
TTFM is a high-performance, query-based file manager utilizing DuckDB and Parquet for indexing. It supports boolean logic search (AND/OR/NOT) and typed tags (e.g., `ext:rs`, `parent:src`).

## Current Status (v0.1.0)
- [x] **Core Indexing**: Recursive directory scanning with fast insertion via DuckDB Appender.
- [x] **High Performance**: 
  - **Parallel Processing**: Multi-threaded indexing using `rayon`.
  - **Instance Pooling**: Wasm instance caching (`thread_local`) for minimal overhead.
- [x] **Storage**: ZSTD-compressed Parquet file storage (`file_index.parquet`).
- [x] **Query Parsing**: Custom parser supporting `&`, `|`, `-` (NOT), and parentheses.
- [x] **Typed Search**: 
  - `filename:` (fuzzy match)
  - `ext:` / `extension:` (exact match, excludes directories)
  - `parent:` (normalized path match)
  - `kind:Folder` (directory filtering)
- [x] **Plugin System (WebAssembly)**: 
  - **WIT Interface**: Standardized plugin interface for `TagFunction`.
  - **Wasm Runtime**: High-speed plugin execution via `wasmtime`.
  - **MIME Type Plugin**: Proof-of-concept plugin with directory detection (`inode/directory`).
- [x] **Cross-Platform**: Path normalization (`\` -> `/`) for Windows/Linux compatibility.
- [x] **CLI Interface**: Basic `index`, `search`, `list`, `clear` commands.

---

## Upcoming Features & Improvements

### 1. User-Defined Tags (Metadata)
Enable users to attach arbitrary key-value metadata to files.
- [ ] **CLI Command**: Add `tag` command (e.g., `ttfm tag <FILE> key:value`).
- [ ] **Storage**: Utilize the existing `tags` MAP column in Parquet.
- [ ] **Re-indexing Strategy**: Update specific rows or merge data without full re-indexing.
- [ ] **Search**: Enable searching user tags (e.g., `project:alpha`).

### 2. Search Result Control
Improve flexibility in viewing search results.
- [ ] **Pagination/Limit**: Allow users to specify result limits (currently fixed at 100).
- [ ] **Sorting**: Add options to sort by Size, Date Modified, or Name.
- [ ] **Output Formats**: Support JSON output for integration with other tools.

### 3. Documentation & Help
Make the tool user-friendly.
- [ ] **Ease of Input**: Implement Tab completion (CLI) and Search Suggestions (GUI) for tags.
- [ ] **Enhanced Help**: Update `clap` definitions to include query syntax examples in `--help`.
- [ ] **Query Syntax Guide**: detailed explanation of `&`, `|`, `-`, and available typed tags.

### 4. GUI Implementation (Relm4/GTK4)
Create a desktop interface for ease of use.
- [ ] **Project Structure**: Create `src/bin/gui.rs`.
- [ ] **UI Design**: Search bar, Result Table, Indexing Progress Bar.
- [ ] **Async Integration**: Run indexing/searching in background threads to keep UI responsive.

### 5. Advanced Indexing
- [ ] **Incremental Indexing**: Detect changes and update only modified files (watch mode).
- [ ] **Content Indexing**: (Optional/Future) Index text content within files.

### 6. Entity-Centric Management (v0.2.0)
Transition to a normalized schema to track file identities and movements.
- [ ] **Schema Refactoring**: Implement 3-table structure (`entities`, `locations`, `tags`).
- [ ] **Physical Identification**: Track files via `inode` and `device_id`.
- [ ] **Incremental Sync**: Skip unchanged files by comparing `mtime` and `inode`.
- [ ] **Move Detection**: Detect and update `mv` operations without losing tags.
- [ ] **Content Hashing**: Implement SHA-256 (or BLAKE3) for deduplication and verification.

---

## Technical Debt / Refactoring
- [x] **Performance Optimization**: Solved initial plugin overhead via parallelization and pooling.
- [ ] **Error Handling**: Standardize error messages for invalid queries.
- [ ] **Benchmark**: Validate performance on extreme datasets (1M+ files).
- [ ] **Test Coverage**: Add more edge cases for Windows paths and complex boolean logic.