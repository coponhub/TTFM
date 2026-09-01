# TTFM (Typed Tag File Manager) Roadmap

## Project Overview
TTFM is a high-performance, query-based file manager utilizing DuckDB and Parquet for indexing. It supports complex boolean logic search (AND/OR/NOT) and extensible typed tagging systems.

---

## Milestone 1: Prototype (Status: [x])
Initial foundation, core indexing infrastructure, and schema reorganization.

### Core Features (v0.1.0)
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
  - `type_from_ext:Folder` (directory filtering)
- [x] **Plugin System (WebAssembly)**: 
  - **WIT Interface**: Standardized plugin interface for `IndexingFunction`.
  - **Wasm Runtime**: High-speed plugin execution via `wasmtime`.
  - **MIME Type Plugin**: Proof-of-concept plugin with directory detection (`inode/directory`).
- [x] **User-Defined Tags & Persistence**:
  - **Table Implementation**: `user_tags` table for persistent storage (item_id, type, value).
  - **CLI Command**: `tag` command implemented (`ttfm tag <FILE> project:alpha`).
  - **Search**: Support for searching user tags and filtering.
- [x] **CLI Interface**: Basic `index`, `search`, `list`, `clear`, `tag`, `note`, `rank` commands.
- [x] **Cross-Platform**: Path normalization (`\` -> `/`) for Windows/Linux compatibility.

### Infrastructure & Refactoring 
- [x] **Home Directory Support**: Switch storage/config location to `~/.ttfm`.
- [x] **Database Schema Migration**:
    - Rename columns for clarity: `id` -> `item_id`, `inode` -> `file_id`.
    - Split tag storage into `base_tags`, `system_tags`, and `user_tags`.
    - Implement **Unified View (oneview)** with `name` and `origin` resolution logic.
- [x] **Other Refactoring**:
    - Refactored `ScanEntry` using macros and `TagDefinition` for DRY/type safety.
    - Improved error handling with `Result` propagation and logging.
    - Unified static (`TagDefinition`) and dynamic (`IndexingFunction`) tag systems using `ScanRole`.
    - Avoid redundant metadata calls via hash-based early filtering and UUID-based identity tracking.
- [x] **Location Management**:
    - **Abolish "Move" logic**: Replace explicit move detection with static location set synchronization.
    - **Entity-Based Location Management**: Support multiple locations per item (e.g., hard links).
- [x] **Benchmarks**: Validated performance on large datasets (1M+ files).
- [x] **Search Control**: Pagination/Limit support for result sets.

---

## Milestone 2: Search & Query (CLI) (Status: [x])
Advanced TTQL (Typed Tag Query Language) and performance optimizations.

- [x] **Typed Literals**: Support for Float (e.g., `1.23`) and Date (e.g., `2024-01-01`) literals.
- [x] **Projection**: Value extraction via `type:`.
- [x] **Backend Optimization**:
    - **Separate `label` column into stored typed columns**:
        - `label_str` (VARCHAR), `label_int` (BIGINT), `label_double` (DOUBLE), `label_bool` (BOOLEAN).
    - Remove privileged physical columns (`size`, `mtime`, `rank`) and map them to `label_int`.
    - **Tag Schema Table**: Dedicated table to map Tag Keys to Data Types (e.g., `size` -> `Int`).
    - **Identity Verification**: Multi-layer identity matching.
- [x] **Query Module Decoupling**: Split `src/query.rs` into a dedicated `query/` module (AST, Parser, SQL gen).
- [x] **Aggregation**: Statistical calculations such as `sum(size:)`, `count(ext:jpg)`.
- [x] **Comparison Operators**: Scalar, Label, and Stuck comparisons (e.g., `size > 100MB`).
- [x] **Calculation**: Arithmetic operations within queries, including parentheses support.
- [x] **Nest & Flatten**:
    - Multi-key projections and dynamic mapping using the `&:` operator.
    - Group-based scalar comparisons and filtering (e.g., `parentdir: &: (sum(size:) > 1GB)`).
- [x] **Refactoring of Query module**

---

## Milestone 3: Tag & Plugin Refinement (Status: [x])
Modularizing the core for tag-centricity and extensibility.

- [x] **Tag-Centric Management**: Centralize `IndexingFunction`, `QueryFunction`, display/extraction rules within `TagType` modules.
- [x] **Modular Plugins**: Enable adding/overriding functionality on a per-`TagType` basis via plugins.
- [x] **Component Decoupling**:
    - Decompose `FileManager` in `lib.rs` (extract indexing and plugin management).

---

## Milestone 4: Tag Edit (Status: [ ])
Tag-based operations including file movement and functional integration.

- [ ] **Query-Based Tagging**: Support `ttfm tag <QUERY> <TAG>` to batch-tag items.
- [ ] **Functional Integration**: Merge `rank` assignment into the generalized tag editing system.
- [ ] **Virtual Operations (mv)**: Realize file moving/renaming by updating `path` tags.

---

## Milestone 5: Interactive Mode (CLI) (Status: [ ])
The fdisk-like conversational interface.

- [ ] **feature**: Support `ttfm -i`.
    - **REPL Interface**: Interactive interface for managing files and tags.
    - **Continuous Flow**: Operation results scroll smoothly upward for iterative processing.

---

## Milestone 6: GUI Prototype (Status: [ ])
Desktop experience with asynchronous integration.

- [ ] **Relm4/GTK4 Implementation**:
    - **Project Structure**: `src/bin/gui.rs`.
    - **UI Design**: Search bar, Result Table, Indexing Progress Bar.
    - **Async Integration**: Responsive UI with background indexing/searching threads.

---

## Milestone 7: Windows Support (Status: [ ])
Core compatibility and native integration for Windows environments.

### CLI Support
- [ ] **Terminal Integration**: Optimize console output (UTF-8), colors, and input handling for PowerShell and CMD.
- [ ] **Native Path Handling**: Robust handling and autocompletion of Windows-style paths (`C:\...`) in CLI.

### GUI Support
- [ ] **Windows Native UI**: Optimize GTK4/Relm4 window decorations, font rendering, and High DPI support on Windows.
- [ ] **System Integration**: Integration with Windows desktop notifications, tray icons, and Shell extensions.

### Common & Infrastructure
- [ ] **File System Events**: Implement Windows-specific file system monitoring (e.g., `ReadDirectoryChangesW`).
- [ ] **Packaging**: Establish reliable build and signing pipelines for Windows binaries (`.exe`, `.msi`).

---

## MileStone X: Future Backlog (Status: [ ])
Remaining features, optimizations, and long-term vision.

### Features & Operations
- [ ] **Advanced Indexing**: Incremental indexing (watch mode) and Content indexing.
- [ ] **Open Operations**: Support `ttfm open <QUERY>` with system-default apps.
- [ ] **Item Operations**: `split` (separate multi-location items) and `merge` (combine entities).
- [ ] **Remote Support**: Integrate `RemoteID` (ETag/VersionID) for online files.
- [ ] **Unique Tag Types**: Allow a user-defined type to be marked unique (e.g. a `unique:true` meta tag on the Type item) so its tag is single-valued, rejecting or replacing duplicates on edit.

### Search & UI Enhancements
- [ ] **Sorting**: Sort results by Size, Date Modified, or Name.
- [ ] **Output Formats**: Support JSON output.
- [ ] **Rank Display Scaling**: Decimal representation of BigInt ranks.
- [ ] **UI/UX Improvements**:
    - Tab completion (CLI) and Search Suggestions (GUI).
      - Suggestions search the full `type:label` tag string and use progressive lazy search with deduplication, appending results to the displayed list as each phase completes:
        `label_cache(tag)` → `item_references(content partial match, item_kind=tag)` → full scan of `base_tags`/`user_tags`
      - `label_cache` is optional; if absent, the first phase is skipped. When present, sorted by `tag ASC` to benefit from ZoneMap on prefix search.
      - Search results are registered as tag/type definition items in `item_references`, naturally building a suggestion cache over time.
    - Enhanced Help: Syntax examples in `--help` and detailed Query syntax guide.
- [ ] **Query Strictness**: Investigate implementing tag existence verification in `lens_resolver` to improve query strictness and detect typos.

### System & Maintenance
- [ ] **Query SQL Lensification**: Move SQL construction out of the `fetcher`/`query::sql` builders and behind the Lens. The builders should only compose Lens-provided functions/combinators (as `NameFn` composes `Prefer`), leaving physical SQL construction entirely to the Lens. This makes read uniformly go through the StorageMapping abstraction (STORE.md §5) instead of touching `oneview` directly.
    - [ ] Optimize performance by delaying name deduplication (uniquification) until after item ID selection.
- [ ] **Plugin System Optimization**:
    - WASM instance management optimization (parallel initialization).
    - WASI security (restricting `preopened_dir`).
    - Conditional Plugin Updates: Version-aware overwriting of default plugins.
- [ ] **Identity Matching**: Enhanced heuristics using Hash (MD5/SHA256).
- [ ] **Observability**: Async error visualization for background cache workers.
- [ ] **Indexing Performance Improvements**:
    - Further optimize parallel scanning and metadata extraction for massive directories.
    - Investigate more efficient memory management during high-concurrency indexing.
    - Explore batch insertion strategies to consolidate data before writing to the database to improve I/O throughput.
- [ ] **Test Coverage**: Windows paths and complex boolean logic edge cases.
- [ ] **Optimization Trade-offs**: Directory optimization to reduce metadata calls (scalability target: 100M+ files).

