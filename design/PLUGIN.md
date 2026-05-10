# TTFM Plugin Design Specification

バイナリを再コンパイルすることなく機能を拡張するため、`wasmtime` を用いた WebAssembly (Wasm) プラグインシステムを導入する。

## 1. アーキテクチャ概要

Wasmモジュールはホスト（Rust）から見て1つの `TagFunction` として振る舞う。
ホスト側で `WasmPluginAdapter` を作成し、これが `TagFunction + Index` トレイトを実装することで、`TagRegistry` にそのまま登録可能とする。

## 2. インターフェース定義 (WIT: Wasm Interface Type)

Wasmコンポーネントモデルを採用し、`wit/plugin.wit` でインターフェースを定義する。

```wit
package ttfm:plugin;

interface core {
    enum plugin-kind {
        indexing-function,
    }

    // プラグインが返す値の型（ホストがDBスキーマを決定するために使用）
    enum value-type {
        text,
        big-int,
        boolean,
        double,
    }

    record plugin-info {
        name: string,
        version: string,
        kind: plugin-kind,
        value-type: value-type,
    }

    get-info: func() -> plugin-info;
}

interface indexing-function {
    variant tag-value {
        text(string),
        big-int(s64),
        boolean(bool),
        double(f64),
        empty,
    }

    tag-file: func(path: string) -> list<tag-value>;
}

world plugin {
    export core;
    export indexing-function;
}
```

**注意:** `get-columns` は廃止。返す値の型は `plugin-info` の `value-type` で宣言する。

## 3. ホスト側の責務 (Rust)

1. **プラグインロード順序**: ユーザープラグイン → ビルトインプラグインの順でロード。同名のプラグインが既に登録されている場合はスキップ（ユーザープラグインが常に優先）。
2. **ビルトインプラグイン**: バイナリに `include_bytes!` で埋め込み、ディスクへのコピーなしにメモリから直接ロード。常にバイナリと同じバージョンが使われる。
3. **ユーザープラグイン**: `~/.ttfm/plugins/` をスキャンし、`.wasm` ファイルをロード。ビルトインと同名のファイルを置けばビルトインより優先される（古いバージョンの固定も可能）。
4. **WASI構成**: プラグインが対象ファイルを読み込めるよう、実行時にWASIのファイルシステムアクセス権限（Read-Only）を付与。
5. **SQL生成**: Wasm側にはSQL生成ロジックを持たせず、ホスト側が標準的なSQLを自動生成。
6. **実行最適化**: `thread_local` による Wasm インスタンスキャッシュで高速化。

## 4. ゲスト側の責務 (Wasm/Rust, C, etc.)

1. **ファイル解析**: 渡されたファイルパス（WASIパス）を開き、内容を解析する。
2. **値の返却**: 解析結果を `tag-value` のリストとして返す。空の場合は `empty` を返す。

## 5. プラグインの実装例 (Rust)

```rust
wit_bindgen::generate!({
    path: "../../wit/plugin.wit",
    world: "plugin",
});

struct MyPlugin;

impl exports::ttfm::plugin::core::Guest for MyPlugin {
    fn get_info() -> exports::ttfm::plugin::core::PluginInfo {
        exports::ttfm::plugin::core::PluginInfo {
            name: "my_tag".to_string(),
            version: "0.1.0".to_string(),
            kind: exports::ttfm::plugin::core::PluginKind::IndexingFunction,
            value_type: exports::ttfm::plugin::core::ValueType::Text,
        }
    }
}

impl exports::ttfm::plugin::indexing_function::Guest for MyPlugin {
    fn tag_file(path: String) -> Vec<exports::ttfm::plugin::indexing_function::TagValue> {
        // ファイルを解析してタグ値を返す
        vec![exports::ttfm::plugin::indexing_function::TagValue::Text("value".to_string())]
    }
}

export!(MyPlugin);
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

### インストール

ビルドしたコンポーネントを `~/.ttfm/plugins/` に配置する。

```bash
cp ../../plugins/my_plugin.component.wasm ~/.ttfm/plugins/
```

### 有効化

`ttfm.toml` の `[plugins.status]` にプラグイン名（`get_info()` の `name`）を追加する。

```toml
[plugins]
enabled = true

[plugins.status]
my_tag = true
```

デフォルトは有効（エントリがなければ `true` 扱い）。

## 7. ビルトインプラグインの更新

ビルトインプラグイン（`mimetype` など）はバイナリに埋め込まれているため、ttfm本体を再ビルドするだけで自動的に更新される。ユーザープラグイン（`~/.ttfm/plugins/` 内のファイル）は上書きされない。

ビルトインを更新する場合は `plugins_src/` 以下のソースを修正後、「6. プラグインのビルド手順」に従って `plugins/` 以下の `.component.wasm` を更新し、ttfm本体を再ビルドする。
