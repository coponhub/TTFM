# TTFM (Typed Tag File Manager) System Design Specification

## 1. プロジェクト概要
TTFMは、従来のディレクトリ階層構造に依存せず、**Typed Tag（型付きタグ）** を用いてファイルを管理・検索するためのファイルマネージャである。
ファイルシステムから抽出されたメタデータおよびユーザー定義のメタデータを統一的なタグ形式で扱い、高速かつ柔軟な検索を提供する。
さらに、ファイルの実体（Entity）を識別・追跡することで、場所の移動や属性の変化を管理するシステムとして設計されている。

## 2. コア・コンセプト

### 2.1 Typed Tag
全てのファイル属性は `Type(Key):Tag(Value)` 形式の「タグ」として扱われる。タグには大きく分けて2つのカテゴリが存在する。
- **SystemTag**: システムによってファイルから自動的に抽出されるタグ（拡張子、サイズ、更新日時など）。
- **UserTag**: ユーザーが手動で定義・付与するタグ。

### 2.2 実体中心設計 (Item/Entity-Centric)
システムが管理する全ての対象は **Item（アイテム）** と呼ばれる一意のIDを持つ対象として定義される。Itemにはその実体である **Entity（エンティティ）** があり、以下の分類で管理される。

- **File Entity**: ファイルシステム上の実ファイルに基づく実体。InodeおよびDevice IDによって同一性が追跡される。
- **Item Entity**: ファイル以外の対象に基づく実体。
    - **Kinds**:
        - `type`: タグの型（例: `location`）。
        - `typedtag`: 型と値のペア（例: `location:tokyo`）。
        - `label`: タグの値（例: `tokyo`）。
        - `note`: ユーザーが作成する仮想的なテキストメモ。

これらの実体はすべてデータベース上のItemとして、等しくタグ付けの対象となる。これにより、ファイルだけでなくタグ定義自体にメタデータを付与したり、メモ情報を記録・管理したりすることが可能となる。

### 2.4 優先度システム (RANK)
全てのアイテムは `rank` と呼ばれる整数値の優先度を保持する。
- **アイテムのソート**: 検索結果は `rank` の降順で表示される。
- **列の表示順序**: タグの型（type）自体が持つ `rank` に基づき、値が大きい
  タグほど CLI の表示において左側の列に配置される。

## 3. 非ディレクトリ指向
ファイルシステム上の「パス」は単なる属性の一つとして扱われ、ユーザーは「どのフォルダにあるか」ではなく「どのような属性（タグ）を持っているか」に基づいてファイルを管理する。

## 3. アーキテクチャ

### 3.1 ストレージ戦略 (Unified Parquet)
データの保存形式は **DuckDB** エンジンを用いた **ZSTD圧縮 Parquet ファイル** に完全に統一する。
- 高い圧縮率と、ファイルをメモリに全ロードせずにクエリ可能なパフォーマンスを両立する。

### 3.2 データベース・スキーマ (File & Item Store)
実体と属性を分離し、移動検知や柔軟な拡張を可能にするため、以下の構成を採用する。
書き込みは DuckDB を介して Parquet ファイルに対して行われる。
これらのテーブルは".ttfm/db/"ディレクトリに格納される。

#### 1. File Store (大規模データ用)
ファイルの実体とパス、およびタグを管理する。

**A. `file_entities` テーブル (実体) (.ttfm/db/entities.parquet)**
- `id`: 内部管理用ユニークID (PRIMARY KEY)
- `rank`: 優先度 (BIGINT, DEFAULT 0)
- `inode`: OSレベルの識別子 (Inode number / File Index)
- `device_id`: デバイス識別子
- `size`: ファイルサイズ
- `mtime`: 最終更新日時
- `hash`: コンテンツハッシュ (オプション)

**B. `locations` テーブル (場所) (.ttfm/db/locations.parquet)**
- `entity_id`: `file_entities.id` への外部キー
- `path`: フルパス (UNIQUE)
- `filename`: ファイル名
- `parentdir`: 親ディレクトリパス（検索最適化用）
- `extension`: 拡張子

**C. `file_tags` テーブル (属性) (.ttfm/db/tags.parquet)**
- `entity_id`: `file_entities.id` への外部キー
- `tag_type`: タグの種類（例: `mimetype`, `project`）
- `tag_value`: タグの値

#### 2. Item Store (定義・仮想データ用)
タグの定義やメモなどを管理する。

**D. `item_entities` テーブル (.ttfm/db/items.parquet)**
- `id`: ユニークID (PRIMARY KEY)
- `rank`: 優先度 (BIGINT, DEFAULT 0)
- `kind`: `type`, `typedtag`, `label`, `note` のいずれか
- `content`: 識別名（Type名等）または Note の本文

**E. `item_tags` テーブル (.ttfm/db/item_tags.parquet)**
- `item_id`: `item_entities.id` への外部キー
- `type`: タグの種類
- `value`: タグの値

#### 3. Unified View (`all_tags`)
全てのタグ情報を一元的に扱うための論理ビュー。検索クエリはこのビューに対して実行される。
- `target_id`: 対象のID
- `target_kind`: `file` または `item`
- `rank`: 対象の優先度（ソート用）
- `type`: タグの種類
- `value`: タグの値

### 3.3 プラグイン・コンポーネント設計 (TagFunction パターン)
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

### 4. プロセス設計

### 4.1 インデックス作成 (`ttfm index`)
既存のインデックスデータと現在のファイルシステムの状態を比較し、差分のみを効率的に更新する。
大規模なファイルシステム（100万件以上）に対応するため、ディレクトリのメタデータを利用したスキャン最適化を行う。

#### A. 処理フロー

1.  **Scan Phase (走査と最適化)**:
    - **ディレクトリ・キャッシュのロード**: インデックス開始時に、既存DBから「ディレクトリ」のパスと `mtime` のリストをメモリ上の `HashMap` にロードする。
    - **高速並列トラバース**: `ignore` クレートを用いてマルチスレッドでディレクトリ階層を走査する。
    - **ディレクトリ単位の判定**: 各ディレクトリにおいて以下の処理を行う。
        - **一致 (Unchanged)**: そのディレクトリの現在の `mtime` がキャッシュと一致する場合。
            - その直下にある **「ファイル」の走査と `stat` 呼び出しを全てスキップ** する。
            - 既存のレコードが有効であると判断し、DBには「このディレクトリは変更なし」というフラグを記録する。
            - ※ただし、サブディレクトリ内での変更を検知するため、子ディレクトリの探索は継続する。
        - **不一致 (Modified/New)**: `mtime` が異なるか、新規ディレクトリの場合。
            - その直下の全てのファイルおよびディレクトリに対して `stat` を行い、最新のメタデータを取得する。
    - **一時保存**: 変更が検知されたエントリのみを `current_scan.parquet` に書き出す。

2.  **Diff Phase (差分分析)**:
    - DuckDB 上で、`current_scan.parquet` (最新) と既存の Parquet ファイルを Inode をキーにして比較し、以下のカテゴリに分類する。
        - **To Process**: 新規、または Mtime/Size が変化したファイル。
        - **Moved**: Inode は一致するが、Path が異なるファイル。
          - アクション: **Location (path, parentdir, filename, extension) の情報を更新する。**
        - **Unchanged**: 全てのメタデータが一致、または上位ディレクトリの `mtime` 判定でスキップされたファイル。
        - **Deleted**: 既存インデックスにあるが、今回の走査で見つからず、かつ親ディレクトリが「不一致 (Modified)」判定されていたファイル。

3.  **Tagging Phase (実行)**:
    - **To Process** のリストに対してのみ `Tagger` を実行し、メタデータを抽出する。

4.  **Merge Phase (統合)**:
    - 既存データ、新規抽出分、および更新された Location 情報を DuckDB 上で統合し、最終的な `file_entities`, `locations`, `file_tags` Parquet ファイルを更新・保存する。
    - 読み込み中のファイル破壊を防ぐため、一旦 `.tmp` ファイルに書き出し、完了後にリネームを行う。

#### B. 最適化のトレードオフ
- **メリット**: ファイル数に対してディレクトリ数が少ない場合、システムコールの回数を劇的に削減でき、1億ファイル規模でも数秒〜十数秒での同期が可能になる。
- **制約**: 「ファイル名を変えずに中身だけを更新」した場合、親ディレクトリの `mtime` が更新されないため、この変更を自動検知できない。この場合は `ttfm index --force` 等による再スキャンが必要となる。

### 4.2 検索処理 (`ttfm search`)

1.  **クエリ解析**: 検索クエリをパーサによって AST（抽象構文木）へ変換。
    - クエリはTypedTag (`key:value`) および論理式 (`&`, `|`, `-`, `()`) を受け付ける。
    - **Globパターンのサポート**: タグの値（Value）部分には、`*`（任意の文字列）や `?`（任意の1文字）などの Glob パターンを使用できる。
        - 例: `filename:*report*` で「report」を含むファイルにマッチ。
        - 例: `extension:p?g` で `png` や `jpg`（判定ロジックによる）などにマッチ。
2.  **論理演算の解決**: 各 `TypedTag` ノードについて、対応する `TagFunction` または `all_tags` ビューを用いて SQL 条件式を生成。
    - **NOT演算 (`-`) の特殊仕様**: ノイズを抑制するため、否定演算の母集団（Universe）は**「そのタグの種類（Type）を保持しているエントリ」**に制限される。
        - 例: `-extension:txt` は「拡張子というタグを持っているが、その値が txt ではないもの」のみにマッチする。拡張子という概念を持たない Item や Note は結果に含まれない。
3.  **実行**: DuckDB を介して Parquet ファイルに対して SQL クエリを実行し、
    一致する ID を集約して結果を返す。結果は `rank` の降順でソートされる。

### 4.3 優先度の操作 (`ttfm rank`)
ユーザーは検索クエリを用いて、マッチしたアイテムの優先度を一括で変更できる。
1.  指定されたクエリで検索を実行。
2.  マッチ件数を表示し、ユーザーに確認を求める。
3.  対象の `rank` カラムを更新。

#### システムデフォルト優先度 (SystemRank)
インデックス作成時、標準的なタグ型には以下の優先度が割り当てられる。
- 7: `filename` (最優先)
- 6: `type_from_ext`
- 5: `size_str`
- 4: `modified_str`
- 3: `parentdir`
- 2: `content`
- 1: その他システムタグ
- 0: 初期値（ユーザータグ等）

## 5. プラグインシステム設計 (WebAssembly)

バイナリを再コンパイルすることなく機能を拡張するため、`wasmtime` を用いた WebAssembly (Wasm) プラグシステムを導入する。

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
5.  **実行最適化**: `rayon` による並列実行、および `thread_local` による Wasm インスタンス・プールを用いて高速化を図る。

5.4 ゲスト側の責務 (Wasm/Rust, C, etc.)
1.  **ファイル解析**: 渡されたファイルパス（WASIパス）を開き、内容を解析する（例: 先頭バイトを読んでMIME判定）。
2.  **値の返却**: 解析結果を `tag-value` のリストとして返す。

### 3.4 永続化とアトミック性 (IndexStore)
インデックス（Parquetファイル）の更新において、データの整合性と堅牢性を確保するため、`IndexStore` は以下の設計指針に基づくアトミックな書き込みを提供する。

#### A. アトミック書き込みロジック (`save_parquet`)
DuckDB の `COPY` コマンドを使用して Parquet を書き出す際、対象のパスへ直接書き込むことは避ける。
1.  まず、対象ファイル名に `.tmp` を付与した一時ファイルへ書き出しを行う。
2.  書き出しが正常に完了したことを確認した後、`std::fs::rename` を用いて一時ファイルを本来のパスへ移動（置換）する。
これにより、書き込み中のプロセス異常終了やディスク容量不足が発生しても、既存のインデックスが破損することを防ぐ。

#### B. 構造化された永続化インターフェース (`write_parquet`)
特定のテーブル全体を Parquet として保存する場合、生SQLの `COPY` 文を直接呼び出すのではなく、`write_parquet` メソッドを使用する。
- `write_parquet` は Sea-query を用いて `SELECT * FROM [table]` というクエリを内部で構築し、`save_parquet` へ渡す。
- これにより、上位のビジネスロジックは DuckDB 固有の `COPY` 構文や一時ファイルの管理を意識することなく、安全にデータを永続化できる。

### 6. 設定管理 (Configuration Management)

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
