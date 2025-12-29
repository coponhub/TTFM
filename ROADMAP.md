# TTFM (Typed Tag File Manager) Roadmap

## Project Overview
TTFM is a high-performance, query-based file manager utilizing DuckDB and Parquet for indexing. It supports boolean logic search (AND/OR/NOT) and typed tags (e.g., `ext:rs`, `parent:src`).

## Current Status (v0.1.0)
- [x] **Core Indexing**: Recursive directory scanning with fast insertion via DuckDB Appender.
- [x] **Storage**: ZSTD-compressed Parquet file storage (`file_index.parquet`).
- [x] **Query Parsing**: Custom parser supporting `&`, `|`, `-` (NOT), and parentheses.
- [x] **Typed Search**: 
  - `filename:` (fuzzy match)
  - `ext:` / `extension:` (exact match, excludes directories)
  - `parent:` (normalized path match)
  - `kind:Folder` (directory filtering)
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

### 6. Plugin System (WebAssembly)
Enable extending functionality by adding external plugin files (e.g., `.wasm`) without recompiling the binary.
- [ ] **Interface Design**: Define Wasm interface (WIT) compatible with `TagFunction`.
- [ ] **Runtime Integration**: Integrate `wasmtime` to load and execute plugins.
- [ ] **Plugin Discovery**: Load plugins from user configuration directory.
- [ ] **MIME Type Plugin**: Implement MIME detection as a proof-of-concept Wasm plugin.

---

## Technical Debt / Refactoring
- [ ] **Error Handling**: Standardize error messages for invalid queries.
- [ ] **Performance**: Benchmark indexing speed on large storage (100k+ files).
- [ ] **Test Coverage**: Add more edge cases for Windows paths and complex boolean logic.
