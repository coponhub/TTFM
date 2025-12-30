# TTFM (Typed Tag File Manager) System Design Specification

## 1. プロジェクト概要
TTFMは、従来のディレクトリ階層構造に依存せず、**Typed Tag（型付きタグ）** を用いてファイルを管理・検索するためのファイルマネージャである。
ファイルシステムから抽出されたメタデータおよびユーザー定義のメタデータを統一的なタグ形式で扱い、高速かつ柔軟な検索を提供する。

## 2. コア・コンセプト

### 2.1 Typed Tag
全てのファイル属性は `Type(Key):Tag(Value)` 形式の「タグ」として扱われる。タグには大きく分けて2つのカテゴリが存在する。
- **SystemTag**: システムによってファイルから自動的に抽出されるタグ（拡張子、サイズ、更新日時など）。
- **UserTag**: ユーザーが手動で定義・付与するタグ。

### 2.2 非ディレクトリ指向
ファイルシステム上の「パス」は単なる属性の一つとして扱われ、ユーザーは「どのフォルダにあるか」ではなく「どのような属性（タグ）を持っているか」に基づいてファイルを管理する。

## 3. アーキテクチャ

### 3.1 ストレージ戦略 (Unified Parquet)
データの保存形式は **DuckDB** エンジンを用いた **ZSTD圧縮 Parquet ファイル** に完全に統一する。
- 高い圧縮率と、ファイルをメモリに全ロードせずにクエリ可能なパフォーマンスを両立する。

### 3.2 プラグイン・コンポーネント設計 (TagFunction パターン)
新しいタグ機能を追加していくための拡張基盤として、以下のトレイトの包含関係を維持する。

#### A. `TagFunction` trait (`src/functions.rs`)
特定の TypedTag に関する**定義・検索・抽出の統合単位**。
- **タグ名の管理**: 担当する識別子（例: `"path"`, `"extension"`) を `NAME` 定数として保持する。
- **検索の変換**: ユーザー入力（TypedTag）を解釈し、SQL条件式へ変換する (`to_sql`)。
- **Taggerの提供**: 内部に `Tagger` を必ず持ち、インデックス作成時の抽出ロジックをシステムへ提供する。 

#### B. `Tagger` trait (`src/taggers.rs`)
**「実際のタグ付け」を行う実行部**。`TagFunction` に内包される。
- **DB定義**: そのタグをインデックス登録する際に必要なデータベースカラム（名前、型）を定義する (`get_columns`)。
- **タグ付けロジック**: ファイルパスを受け取り、具体的な抽出・生成ロジックを実行して値を生成する (`tag_file`)。

#### C. `FunctionRegistry` (`src/lib.rs`)
個別の `TagFunction` を一括管理するハブ。
- インデックス作成時は `TagFunction` から `Tagger` を取得して実行し、検索時はクエリに対応する `TagFunction` にSQL変換を委譲する。

## 4. プロセス設計

### 4.1 インデックス作成 (`ttfm index`)
1. 指定ディレクトリを再帰的にスキャン。
2. 各ファイルに対し、`FunctionRegistry` に登録された全ての `TagFunction` 経由で `Tagger` を実行。
3. 生成されたタグ値を DuckDB のインメモリテーブルへ挿入。
4. 最終的に単一（または複数）の Parquet ファイルとして ZSTD 圧縮して書き出し。

### 4.2 検索 (`ttfm search`)
1. 検索クエリをパーサによって AST（抽象構文木）へ変換。
    1. クエリはTypedTagを受け付ける。
    2. この際、クエリは &(AND) |(OR) -(Not) ()(括弧)を組み合わせた論理式を受け付ける。 
2. 各 `TypedTag` ノードについて、対応する `TagFunction` が SQL 条件式を生成。
3. DuckDB を介して Parquet ファイルに対して SQL クエリを実行し、結果を返す。

## 5. プラグインシステム設計 (WebAssembly)

バイナリを再コンパイルすることなく機能を拡張するため、`wasmtime` を用いた WebAssembly (Wasm) プラグインシステムを導入する。

### 5.1 アーキテクチャ概要
Wasmモジュールはホスト（Rust）から見て1つの `TagFunction` として振る舞う。
ホスト側で `WasmPluginAdapter`（仮称）を作成し、これが `TagFunction` トレイトを実装することで、既存の `FunctionRegistry` にそのまま登録可能とする。

### 5.2 インターフェース定義 (WIT: Wasm Interface Type)
Wasmコンポーネントモデルを採用し、`wit` ファイルでインターフェースを定義する。将来的な拡張性を考慮し、プラグイン種別を特定する `core` インターフェースを設ける。

```wit
package ttfm:plugin

// プラグインの基本情報を定義する共通インターフェース
interface core {
    enum plugin-kind {
        tag-function,
    }

    record plugin-info {
        name: string,
        version: string,
        kind: plugin-kind,
    }

    // プラグインの種別やバージョンを返す
    get-info: func() -> plugin-info
}

// TagFunction (メタデータ抽出) 固有のインターフェース
interface tag-function {
    // カラム定義の型
    record column-def {
        name: string,
        sql-type: string,
    }
    
    // 提供するカラムのリスト
    get-columns: func() -> list<column-def>
    
    // 値のバリアント
    variant tag-value {
        text(string),
        big-int(s64),
        boolean(bool),
        empty,
    }
    
    // 指定されたファイルのパスを受け取り、タグ値を返す
    tag-file: func(path: string) -> list<tag-value>
}

world plugin {
    export core
    export tag-function
}
```

### 5.3 ホスト側の責務 (Rust)
1.  **プラグイン探索**: 所定のディレクトリ（例: `~/.config/ttfm/plugins/`）から `.wasm` ファイルをロードする。
2.  **アダプタ生成**: ロードしたWasmモジュールごとに `WasmPluginAdapter` を生成し、`FunctionRegistry` に登録する。
3.  **WASI構成**: プラグインが対象ファイルを読み込めるよう、実行時にWASIのファイルシステムアクセス権限（Read-Only）を動的に付与する。
4.  **SQL生成**: Wasm側にはSQL生成ロジックを持たせず（複雑化回避）、ホスト側が標準的なSQL（単純なカラム一致検索など）を自動生成するフォールバックロジックを使用する。

### 5.4 ゲスト側の責務 (Wasm/Rust, C, etc.)
1.  **ファイル解析**: 渡されたファイルパス（WASIパス）を開き、内容を解析する（例: 先頭バイトを読んでMIME判定）。
2.  **値の返却**: 解析結果を `tag-value` のリストとして返す。

## 6. 設定管理 (Configuration Management)

TTFMの動作をユーザーが柔軟に変更できるようにするため、TOML形式の設定ファイルを導入する。

### 6.1 設定ファイルの仕様
- **フォーマット**: TOML
- **ファイル名**: `ttfm.toml`
- **探索パス**:
    1. カレントディレクトリ (プロジェクト固有設定)
    2. ユーザー設定ディレクトリ (例: Linuxでは `~/.config/ttfm/ttfm.toml`)

### 6.2 プラグイン制御設定
`[plugins]` セクションにて、Wasmプラグインシステムの有効・無効を切り替えることができる。

- `enabled` (boolean): `false` に設定した場合、`plugins/` ディレクトリからのロードをスキップする。大量のファイルを高速にインデックスしたい場合や、一時的にプラグイン機能を停止したい場合に有効である。

**設定例 (`ttfm.toml`)**:
```toml
[plugins]
enabled = false  # 全てのWasmプラグインを無効化
```

### 6.3 個別プラグインの制御
`[plugins.status]` セクションを使用することで、プラグインごとの有効・無効を個別に設定できる。

- キーにはプラグイン名（Wasmモジュールが返す名前）、値には `true` (有効) または `false` (無効) を指定する。
- `[plugins] enabled` が `false` の場合は、個別設定に関わらず全てのプラグインが停止する。
- 個別設定が存在しないプラグインは、デフォルトで `true` (有効) とみなされる。

**設定例 (`ttfm.toml`)**:
```toml
[plugins]
enabled = true

[plugins.status]
sample = false    # sampleプラグインのみ無効化
mimetype = true   # mimetypeプラグインは明示的に有効
```