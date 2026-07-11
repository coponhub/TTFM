# TTFM Plugin Design Specification

バイナリを再コンパイルすることなく機能を拡張するため、`wasmtime` を用いた WebAssembly (Wasm) プラグインシステムを導入する。

## 1. アーキテクチャ概要

Wasmモジュールはホスト（Rust）から見て1つの `TagFunction` として振る舞う。
ホスト側で `WasmPluginAdapter` を作成し、これが `TagFunction + Index` トレイトを実装することで、`TagRegistry` にそのまま登録可能とする。

## 2. インターフェース定義

Wasmコンポーネントモデルを採用する。インターフェース定義は `plugins_src/ttfm_plugin_macro/src/lib.rs` の定数が正典。

### core（必須）

```wit
interface core {
    name: func() -> string;
    version: func() -> string;
}
```

### indexing（省略可能）

```wit
interface indexing {
    enum value-type { text, big-int, boolean, double }
    variant tag-value {
        text(string),
        big-int(s64),
        boolean(bool),
        double(f64),
        empty,
    }
    get-value-type: func() -> value-type;
    tag-file: func(path: string) -> list<tag-value>;
}
```

### query（省略可能）

```wit
interface query {
    normalize-label: func(label: string) -> option<string>;
    expand: func(tag-type: string, label: string) -> option<string>;
    expand-projection: func(tag-type: string) -> option<string>;
}
```

### display（省略可能）

```wit
interface display {
    record display-format {
        id: string,
        label: string,
    }
    default-format: func() -> option<display-format>;
    formats: func() -> list<display-format>;
    show: func(value: string, format-id: string) -> string;
}
```

## 3. ホスト側の責務 (Rust)

1. **プラグインロード順序**: ユーザープラグイン → 組み込みプラグインの順でロード。同名のプラグインが既に登録されている場合はスキップ（ユーザープラグインが常に優先）。
2. **組み込み(Embedded)プラグイン**: バイナリに `include_bytes!` で埋め込み、ディスクへのコピーなしにメモリから直接ロード。常にバイナリと同じバージョンが使われる。
3. **ユーザープラグイン**: `~/.ttfm/plugins/` をスキャンし、`.wasm` ファイルをロード。Embeddedと同名のファイルを置けばEmbeddedより優先される（古いバージョンの固定も可能）。
4. **WASI構成**: プラグインが対象ファイルを読み込めるよう、実行時にWASIのファイルシステムアクセス権限（Read-Only）を付与。
5. **SQL生成**: Wasm側にはSQL生成ロジックを持たせず、ホスト側が標準的なSQLを自動生成。
6. **実行最適化**: `thread_local` による Wasm インスタンスキャッシュで高速化。

## 4. ゲスト側の責務 (Wasm/Rust, C, etc.)

`core` は必須。それ以外は必要なものだけ実装する。

- **core**: `name()` でプラグイン識別名、`version()` でバージョンを返す。
- **indexing**（省略可能）: 渡されたファイルパスを解析し `tag-value` のリストを返す。空の場合は `empty` を返す。
- **query**（省略可能）: ラベル正規化・クエリ展開ロジックを実装する。省略時はホスト側のデフォルト動作が使われる。
- **display**（省略可能）: 値の表示フォーマットを実装する。省略時は生値がそのまま表示される。

## 5. プラグインの実装例 (Rust)

`Cargo.toml` に依存を追加：

```toml
[dependencies]
ttfm_plugin = { path = "../ttfm_plugin_macro" }  # または crates.io バージョン
wit-bindgen = "0.36.0"
```

`core` + `indexing` のみ実装する最小構成：

```rust
ttfm_plugin::target!(indexing);

struct MyPlugin;

impl exports::ttfm::plugin::core::Guest for MyPlugin {
    fn name() -> String { "my_tag".to_string() }
    fn version() -> String { "0.1.0".to_string() }
}

impl exports::ttfm::plugin::indexing::Guest for MyPlugin {
    fn get_value_type() -> exports::ttfm::plugin::indexing::ValueType {
        exports::ttfm::plugin::indexing::ValueType::Text
    }
    fn tag_file(path: String) -> Vec<exports::ttfm::plugin::indexing::TagValue> {
        vec![exports::ttfm::plugin::indexing::TagValue::Text("value".to_string())]
    }
}

export!(MyPlugin);
```

`query` や `display` も実装する場合は `target!` の引数に追加する：

```rust
ttfm_plugin::target!(indexing, query, display);
```

## 6. プラグインのビルド手順

### 前提

```bash
rustup target add wasm32-wasip1
cargo install wasm-tools
```

### ビルド

```bash
# 1. WASMバイナリのコンパイル
cd plugins_src/my_plugin
cargo build --target wasm32-wasip1 --release

# 2. WASIアダプターを組み込んでコンポーネント化
wasm-tools component new target/wasm32-wasip1/release/my_plugin.wasm \
  --adapt wasi_snapshot_preview1=../../adapters/wasi_snapshot_preview1.reactor.wasm \
  -o ../../plugins/my_plugin.component.wasm
```

`wit/plugin.wit` は不要。インターフェース定義は `ttfm_plugin::target!()` マクロに内包されている。

### インストール

ビルドしたコンポーネントを `~/.ttfm/plugins/` に配置する。

```bash
cp ../../plugins/my_plugin.component.wasm ~/.ttfm/plugins/
```

### 有効化

`ttfm.toml` の `[plugins.status]` にプラグイン名（`name()` の戻り値）を追加する。

```toml
[plugins]
enabled = true

[plugins.status]
my_tag = true
```

デフォルトは有効（エントリがなければ `true` 扱い）。

## 7. 組み込みプラグインの更新

組み込みプラグイン（`mimetype` など）はバイナリに埋め込まれているため、ttfm本体を再ビルドするだけで自動的に更新される。ユーザープラグイン（`~/.ttfm/plugins/` 内のファイル）は上書きされない。

ビルトインを更新する場合は `plugins_src/` 以下のソースを修正後、「6. プラグインのビルド手順」に従って `plugins/` 以下の `.component.wasm` を更新し、ttfm本体を再ビルドする。
