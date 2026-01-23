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
  - `type_from_ext:Folder` (directory filtering)
- [x] **Plugin System (WebAssembly)**: 
  - **WIT Interface**: Standardized plugin interface for `IndexingFunction`.
  - **Wasm Runtime**: High-speed plugin execution via `wasmtime`.
  - **MIME Type Plugin**: Proof-of-concept plugin with directory detection (`inode/directory`).
- [x] **User-Defined Tags & Persistence**:
  - **Table Implementation**: `user_tags` table for persistent storage (item_id, type, value).
  - **CLI Command**: `tag` command implemented (`ttfm tag <FILE> project:alpha`).
  - **Search**: Support for searching user tags and filtering.
- [x] **Cross-Platform**: Path normalization (`\` -> `/`) for Windows/Linux compatibility.
- [x] **CLI Interface**: Basic `index`, `search`, `list`, `clear`, `tag`, `note`, `rank` commands.

---

## Upcoming Features & Improvements

### 1. Query-Based Tagging
Enable tagging multiple files at once using search queries.
- [ ] **Feature**: Support `ttfm tag <QUERY> <TAG>`.
    - Allow the first argument of the `tag` command to be a search query (e.g., `ext:jpg`) instead of just a single file path/ID.
    - All items matching the query will be tagged with the specified tag.

### 2. Search Result Control
Improve flexibility in viewing search results.
- [x] **Pagination/Limit**: Allow users to specify result limits (currently fixed at 100).
- [ ] **Sorting**: Add options to sort by Size, Date Modified, or Name.
- [x] **Typed Literals**: Support for Float (e.g., `1.23`) and Date (e.g., `2024-01-01`) literals in query parser.
- [ ] **Rank Display Scaling**: Represent internal BigInt ranks as user-friendly decimals (e.g., 2 decimal places).
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

### 6. Schema Redesign & Origin Management (v0.2.0)
Transition to a robust schema that separates system-generated metadata from user-defined tags.

- [x] **Home Directory Support**:
    - [x] Switch storage/config location to `~/.ttfm` (Linux) / `%USERPROFILE%\.ttfm` (Windows).
    - [x] Remove current directory dependency.

- [x] **Database Schema Migration**:
    - [x] Rename columns for clarity: `id` -> `item_id`, `inode` -> `file_id`.
    - [x] Split tag storage into `base_tags` (scan results), `system_tags` (definitions), and `user_tags` (persistent).
    - [x] Implement `Unified View (oneview)` with `name` and `origin` resolution logic.

- **Upcoming Features**:
    - [ ] **Advanced Query Features**:
        - [ ] **集約 (Aggregation)**: `sum(size:)`, `count(ext:jpg)` 等の統計値計算。
        - [ ] **グループ比較 (Grouping Comparison)**: `parentdir:(sum(size:) > 1GB)` 等のグルーピング検索。
        - [x] **射影 (Projection)**: `type:` による値の抽出。
    - [x] Implement `name` tag support in query parser and search results.
    - [x] Update UI/CLI to display resolved names instead of raw filenames by default.

- [x] **Entity-Based Location Management**: Support multiple locations per item (e.g., hard links) and replace "move detection" with robust location synchronization.

### 7. File Operations
Directly interact with files from the CLI.
- [ ] **Feature**: Support `ttfm open <QUERY>`.
    - Opens the file(s) matching the search query using the system's default application.
    - If the argument does not contain a colon (`:`), it is treated as a relative path to a local file rather than a query.
    - Implementation should handle cross-platform openers (`xdg-open`, `open`, `start`).

### 9. Interactive Mode
- [ ] **Feature**: Support `ttfm search -i`.
    - Interactive results browser (TUI) allowing navigation and opening files directly.

---

## Technical Debt / Refactoring

- [x] **Phase 1: Error Handling & Architecture**:

  - [x] Refactored `ScanEntry` using macros and `TagDefinition` for better DRY and type safety.

  - [x] Improved error handling by replacing silent failures with `Result` propagation and logging.

  - [x] Unified static (`TagDefinition`) and dynamic (`IndexingFunction`) tag systems using `ScanRole`.

  - [x] Generalized file move detection logic to be implementation-agnostic.

- [x] **Phase 1.5: Indexing Optimization & Robustness**:
  - [x] **Avoid redundant metadata calls**: (Implemented via hash-based early filtering and UUID-based identity tracking).

- [ ] **Phase 2: Plugin System Optimization**:

  - [ ] Optimize WASM instance management to prevent initialization bottlenecks during parallel indexing.

  - [ ] Enhance WASI security by restricting `preopened_dir` to specific scan targets.

- [ ] **Phase 3: Database & Search Refinement**:

  - [ ] Support comparison operators (e.g., `size > 100`) in `QueryParser` and `IndexingFunction`.
  - [ ] **Schema Optimization (Phase E)**: 
    - Separate `label` column into stored typed columns:
      - `label_str` (VARCHAR): Text data, extensions, paths.
      - `label_int` (BIGINT): Size, mtime, rank, counts.
      - `label_double` (DOUBLE): Scores, durations, ratios.
      - `label_bool` (BOOLEAN): Flags like `is_dir`, `readonly`.
    - This allows strict typed querying (e.g., `is_dir IS TRUE`) and efficient storage (DuckDB handles NULLs efficiently).
    - Remove privileged physical columns (`size`, `mtime`, `rank`) and map them to `label_int`.
    - **Tag Schema Table**: Create a dedicated table (e.g., `tag_schema` or `datatype_definitions`) to map Tag Keys to Data Types.
      - Example: `size` -> `Int`, `score` -> `Double`.
      - This provides the most rigorous validation and fastest lookup for query planning.

- [ ] **Identity & Location Management**:
  - [x] **Abolish "Move" logic**: Replace explicit move detection with static location set synchronization to naturally handle hard links.
  - [ ] **Multi-Layer Identity Verification**: Support identity matching using FileID, Hash (MD5/SHA256), or Name+Size+Mtime heuristics.
  - [ ] **Online File Support**: Integrate `RemoteID` (ETag/VersionID) into the location identity model.
  - [ ] **Split & Merge Commands**: Implement `ttfm split` to separate multi-location items and `ttfm merge` to combine identical entities.

- [x] **Benchmark**: Validate performance on extreme datasets (1M+ files).

- [ ] **Test Coverage**: Add more edge cases for Windows paths and complex boolean logic.

- [ ] **Observability & Error Handling**:
  - [ ] **Async Error Visualization**: Implement a mechanism to track and display errors from background cache workers (e.g., via a status command or log file).

- [ ] **Code Refinement**:
  - [ ] **DRY Refactoring**: Consolidate redundant logic for handling empty node sets in `build_and_sql` and `build_or_sql`.
