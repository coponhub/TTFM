# TTFM (Typed Tag File Manager) System Design Specification

## 1. プロジェクト概要
TTFMは、従来のディレクトリ階層構造に依存せず、**Typed Tag（型付きタグ）** を用いてファイルを管理・検索するためのファイルマネージャである。
ファイルシステムから抽出されたメタデータおよびユーザー定義のメタデータを統一的なタグ形式で扱い、高速かつ柔軟な検索を提供する。
さらに、ファイルの実体（Reference）を識別・追跡することで、場所の移動や属性の変化を管理するシステムとして設計されている。

## 2. コア・コンセプト

### 2.1 Typed Tag
全てのファイル属性は `Type:Label` 形式の「タグ(TypedTag)」として扱われる。タグには大きく分けて2つのカテゴリが存在する。
- **SystemTag**: システムによってファイルから自動的に抽出されるタグ（拡張子、サイズ、更新日時など）。
- **UserTag**: ユーザーが手動で定義・付与するタグ。

### 2.2 実体中心設計 (Item/Reference-Centric)
システムが管理する全ての対象は **Item（アイテム）** と呼ばれる一意のIDを持つ対象として定義される。Itemにはその実体である **Reference（エンティティ）** があり、以下の分類で管理される。

- **File Reference**: ファイルシステム上の実ファイルに基づく実体。InodeおよびDevice IDによって同一性が追跡される。
- **Item Reference**: ファイル以外の対象に基づく実体。
    - **ItemKinds**:
        - `type`: タグの型（例: `location`）。
        - `typedtag`: 型と値のペア（例: `location:tokyo`）。
        - `label`: タグの値（例: `tokyo`）。
        - `note`: ユーザーが作成する仮想的なテキストメモ。

これらの実体はすべてデータベース上のItemとして、等しくタグ付けの対象となる。これにより、ファイルだけでなくタグ定義自体にメタデータを付与したり、メモ情報を記録・管理したりすることが可能となる。

### 2.3 Item Name Abstraction (アイテム名の抽象化)
ユーザーが認識・操作する「アイテムの名前」を、ファイルシステム上の「ファイル名」から分離して抽象化する。
- **name**: GUIやCLIでユーザーに提示されるアイテムの名称。
- **filename**: ファイルシステム上の物理的な識別子。

デフォルトでは `name` は `filename` と同一だが、ユーザーは任意の `name` をタグとして付与できる。これにより、物理的なファイル名を変更することなく、コンテキストに応じたわかりやすい名前で管理可能となる。

なお、タグの種類としての `type:name` は `origin:system` であるが、ユーザーが付与した個別のタグ値（例: `name:foo`）は `origin:user` として扱われる。一方で、ファイル名から自動解決された `name` は `origin:system` となる。

### 2.4 優先度システム (RANK)
全てのアイテムは `rank` と呼ばれる整数値の優先度を保持する。
- **アイテムのソート**: 検索結果は `rank` の降順で表示される。
- **列の表示順序**: タグの型（type）自体が持つ `rank` に基づき、値が大きい
  タグほど CLI の表示において左側の列に配置される。

## 3. 非ディレクトリ指向
ファイルシステム上の「パス」は単なる属性の一つとして扱われ、ユーザーは「どのフォルダにあるか」ではなく「どのようなタグを持っているか」に基づいてファイルを管理する。

## 3. アーキテクチャ

### 3.1 ストレージ戦略 (Unified Parquet)
データの保存形式は **DuckDB** エンジンを用いた **ZSTD圧縮 Parquet ファイル** に完全に統一する。
- 高い圧縮率と、ファイルをメモリに全ロードせずにクエリ可能なパフォーマンスを両立する。

### 3.2 データベース・スキーマ (File & Item Store)
実体と属性を分離し、移動検知や柔軟な拡張を可能にするため、以下の構成を採用する。
書き込みは DuckDB を介して Parquet ファイルに対して行われる。
これらのテーブルは TTFM Home ディレクトリ内の `db/` に格納される。

#### 1. File Store (System / Large Data)
ファイルの実体とパス、およびスキャンにより自動抽出されたタグを管理する。
これらのテーブルは `ttfm index` 実行時に更新・洗い替えされる。

**A. `file_references` テーブル (実体) (.ttfm/db/file_references.parquet)**
- `item_id`: 内部管理用ユニークID (PRIMARY KEY)
- `rank`: 優先度 (DEFAULT 0)
- `file_id`: OSレベルの識別子 (Inode number / File Index)
- `device_id`: デバイス識別子
- `size`: ファイルサイズ
- `mtime`: 最終更新日時
- `hash`: コンテンツハッシュ (オプション)

**B. `locations` テーブル (場所) (.ttfm/db/locations.parquet)**
- `item_id`: `file_references.item_id` への外部キー
- `path`: フルパス (UNIQUE)
- `filename`: ファイル名
- `parentdir`: 親ディレクトリパス（検索最適化用）
- `extension`: 拡張子

**C. `base_tags` テーブル (自動抽出タグ) (.ttfm/db/base_tags.parquet)**
- `item_id`: `file_references.item_id` への外部キー
- `type`: タグの種類（例: `size_str`, `type_from_ext`）
- `label_str`, `label_int`, `label_double`, `label_bool`: タグの値（型ごとに物理カラムを持ち、適切な型で格納される）
- ※ 旧 `file_tags`。スキャンごとの洗い替え対象。

#### 2. Item Store (Definition Registry)
タグの型(Type)や値(Label)の定義自体を管理するID台帳。
システム定義とユーザー定義の両方が混在するが、IDによって管理される。

**D. `item_references` テーブル (.ttfm/db/item_references.parquet)**
- `item_id`: ユニークID (PRIMARY KEY)
- `rank`: 優先度 (DEFAULT 0)
- `item_kind`: アイテムの種類 (`type`, `typedtag`, `label`, `note` のいずれか)
- `content`: 識別名（Type名等）または Note の本文

#### 3. Tag Store (Relations)
Item（FileおよびDefinition）に対するタグ付けを管理する。
データの由来（Origin）によってテーブルを分離することで、ユーザーデータを保護しつつ効率的な更新を実現する。

**E. `system_tags` テーブル (System Tags) (.ttfm/db/system_tags.parquet)**
- `item_id`: `item_references.item_id` への外部キー
- `type`: タグの種類
- `label_str`, `label_int`, `label_double`, `label_bool`: タグの値
- ※ システム定義Item（`filename` Type等）に対してシステムが付与するタグ（例: `origin:system`）。

**F. `user_tags` テーブル (User Tags) (.ttfm/db/user_tags.parquet)**
- `item_id`: 対象のID (`file_references` または `item_references` のいずれか)
- `type`: タグの種類
- `label_str`, `label_int`, `label_double`, `label_bool`: タグの値
- ※ ユーザーが手動で付与した全てのタグ。`ttfm index` によるスキャン更新の影響を受けず、永続化される。

#### 4. Unified View (`oneview`)
全てのタグ情報を一元的に扱うための論理ビュー。検索クエリはこのビューに対して実行される。
- `item_id`: 対象のID
- `item_kind`: アイテムの種類 (`file`, `note`, `type`, `label`, `typedtag` 等)
- `rank`: 対象の優先度（ソート用）
- `origin`: タグの出典 (`system` または `user`)
- `type`: タグの種類
- `label_str`, `label_int`, `label_double`, `label_bool`: タグの値（それぞれの物理カラムから合流）
- `typedtag`: タグ全体（`type:label`）を表す文字列
- `name`: アイテムの名称

**Origin & Name Resolution**:
- **Origin**: `base_tags` と `system_tags` 由来の行は `system`、`user_tags` 由来の行は `user` とする。
- **Name**: ユーザー定義（`user_tags` 内の `type:name`）を優先し、存在しなければ `locations.filename` を採用する。

### 3.3 プラグイン・コンポーネント設計 (IndexingFunction パターン)
新しいタグ機能を追加していくための拡張基盤として、以下のトレイトの包含関係を維持する。

#### A. `IndexingFunction` trait (`src/functions.rs`)
特定の TypedTag に関する**定義・抽出の統合単位**。
- **タグ名の管理**: 担当する識別子（例: `"path"`, `"extension"`) を `NAME` 定数として保持する。
- **Taggerの提供**: 内部に `Tagger` を必ず持ち、インデックス作成時の抽出ロジックをシステムへ提供する。 

#### B. `Tagger` trait (`src/taggers.rs`)
**「実際のタグ付け」を行う実行部**。`IndexingFunction` に内包される。
- **DB定義**: そのタグをインデックス登録する際に必要なデータベースカラム（名前、型）を定義する (`get_columns`)。
- **タグ付けロジック**: ファイルパスを受け取り、具体的な抽出・生成ロジックを実行して値を生成する (`tag_file`)。

#### C. `FunctionRegistry` (`src/lib.rs`)
個別の `IndexingFunction` を一括管理するハブ。
- インデックス作成時は `IndexingFunction` から `Tagger` を取得して実行し、検索時はクエリに対応する `IndexingFunction` にSQL変換を委譲する。

### 4. プロセス設計

### 4.1 インデックス作成 (`ttfm index`)
既存のインデックスデータと現在のファイルシステムの状態を比較し、差分のみを効率的に更新する。
大規模なファイルシステム（100万件以上）に対応するためメタデータを利用したスキャン最適化を行う。

#### A. 処理フロー

1.  **Scan Phase (並列メタデータ収集)**:
    - **高速並列トラバース**: `ignore` クレートを用いて、マルチスレッドでディレクトリ階層を走査する。
    - **メタデータ取得**: 各ファイルについて、ファイルシステムから最新のメタデータ（Inode, Size, Mtime等）を取得する。
    - **一時保存**: 取得した全エントリのメタデータを `current_scan.parquet` に書き出す。

2.  **Diff Phase (Auditing)**:
    - **DiffAuditor** を用いて、`current_scan.parquet` (最新) と既存の Parquet ファイルを比較し、以下のカテゴリに分類する。
        - **To Process**: 新規、または Mtime/Size が変化したファイル。
        - **Moved**: `FileId` は一致するが、Path が異なるファイル。
          - アクション: **Location (path, parentdir, filename, extension) の情報を再生成する。**
        - **Unchanged**: 全てのメタデータが一致、または上位ディレクトリの `mtime` 判定でスキップされたファイル。
        - **Deleted**: 既存インデックスにあるが、今回の走査で見つからず、かつ親ディレクトリが「不一致 (Modified)」判定されていたファイル。

3.  **Triage Phase (Selection & Assembly)**:
    - **ItemTriager** を実行し、抽出されたメタデータを各テーブルへ振り分ける。
    - **Extraction**: **To Process** のリストに対して並列で `Tagger` を実行し、メタデータを抽出する。
    - **Triage**: 抽出された「生の値」を、ItemID の付与と共に、性質に応じて適切なバケツ（Entities/Locations/Tags）へ選別（トリアージ）する。
    - **Reconstruction**: **Moved** のリストに対し、ファイルを開かずにパス情報から場所情報を再構築する。

4.  **Merge Phase (Integration)**:
    - 既存データ、新規抽出データ、および更新された場所情報をDuckDB上で統合し、最終的な `file_references`, `locations`, `base_tags` 等のParquetファイルを更新・保存する。

#### B. ディレクトリ最適化オプション有効時のトレードオフ(オプション)
- **メリット**: ファイル数に対してディレクトリ数が少ない場合、システムコールの回数を劇的に削減でき、1億ファイル規模でも数秒〜十数秒での同期が可能になる。
- **制約**: 「ファイル名を変えずに中身だけを更新」した場合、親ディレクトリの `mtime` が更新されないため、この変更を自動検知できない。

### 4.2 検索処理 (`ttfm search`)

1.  **クエリ解析**: 検索クエリをパーサによって AST（抽象構文木）へ変換。クエリは以下の要素で構成される。

    - **TypedTag（タグ）**:
        - `type:label` 形式の基本単位。
        - **ルール**: `type` と `label` の間にスペースを含めることはできない。
    - **集合演算**:
        - 演算子
            - `&`: 積集合 (Intersection)
            - `|`: 和集合 (Union)
            - `-`: 差集合 (Difference) ※二項演算子
            - `^()`: 補集合 (Complement) ※単項演算子。
                - **対象を必ず括弧 `()` で囲む必要があり、かつ `^(` と密着させる（スペース不可）。**
            - 例: `type:file & project:ttfm`
            - 例: `^(type:file)`
        - **演算対象 (Operand)**:
	        - グループ
	        - TypedTag
	        - ラベル比較
        - **演算子の優先順位**: 
            - `^()` > `&` > `|` = `-`
    - **集約 (Aggregation)**:
        - 形式: `[aggregator]([query])`
        - aggregator: `count`, `sum`, `avg`, `max`, `min`
        - query: 任意の検索クエリ
        - 戻り値: クエリ結果を集約した数値（スカラー）。
        - 例: `sum(size:)`
    - **グループ比較 (Grouping Comparison)**:
        - 形式: `[key]:( [Comparison] )`
        - key: グルーピングのキーとなるtype。
        - Comparison: **集約** を含む比較式。
        - 演算子: `==` (一致), `^` または `^=` (不一致), `>`, `>=`, `<`, `<=` (大小比較)。
        - 挙動: 指定されたキーでグルーピングし、条件を満たすグループ（に対応するItem）を返す。キーと括弧を省略した場合は全体の合計が返る
        - **例**
            - `sum(project:A & size:) > 1GB` (キー省略時は全体の合計)
            - `parentdir:( sum(size:) > 1GB )` (フォルダ毎の合計サイズが1GB超)
            - `parentdir:( count(extension:jpg) > 10 )` (JPGファイルを10個以上含むフォルダを検索)
    - **ラベル取得 (Label Retrieval / Projection)**:
        - `Type:`形式。「Typeに含まれるラベル」と、「そのType:Labelが付与されたItemのItemID」のペアを取得する。
        - **仮想的なラベル**:
            - `type:`: Typeを抽出
            - `origin:`: データの出典 (`system` または `user`)を抽出。
            - `typedtag:`: TypedTagを抽出
        - **例**:
            - `project:A & price:` (プロジェクトAに属するアイテムの価格一覧を取得)
            - `type:` (全アイテムの型一覧を取得。値からの逆引き検索 `label:foo & type:` も可能)
            - `path:` (各アイテムのパスを取得)
    - **ラベル比較 (Label Comparison)**:
        - **ラベル比較式** `[Operand] [ComparisonOp] [Operand]` 形式。一つの項として扱われる。取得した各ラベルを比較する。
        - **演算対象 (Operand)**:
            - **Labelリテラル**: 文字列または数値。
            - **Label取得**: 上記「ラベル取得」
            - **ラベル計算式**: 下記「ラベル計算」
        - **演算子**:
            - **ラベル比較演算子**: `=` (一致), `^` または `^=` (不一致), `>`, `>=`, `<`, `<=` (大小比較)。
        - **ルール**:
            - 比較演算子は必ず:の後に記載する必要がある。
            - 比較演算子の前にスペースを挿入する場合、演算子の前に:を追記する（例: `size: :> 100`）。
            - スペースを挿入しない場合はそのまま記載可能（例: `size:>100`）。
            - 演算子の後にスペースを挿入可能（例: `size:> 100`）。
            - 数式のような柔軟な記述が可能（例: `100 :< size:`, `width: :> height:`）。
            - 連鎖比較が可能（例: `50 :< height: :< 100`）。
    - **ラベル計算 (Label Calculation)**:
        - ラベル比較式の一部として使用可能 `(Operand [ArithmeticOp] Operand)` の形式。
        - **演算子**:
            - **ラベル算術演算子**: `+`, `-`, `*`, `/`, `%`。
        - **演算対象 (Operand)**:
            - ラベル比較と同じ
        - **ルール**:
            - 算術演算はラベル比較の演算対象（Operand）括弧内でのみ使用可能。
        - **例**: `(size: + 1024)`, `(width: * height:)`
    - **エスケープと引用符 (Escaping & Quoting)**:
        - **基本**: スペース、演算子記号、あるいはクオート自体を含める場合は、`""` (ダブルクオート) または `''` (シングルクオート) で囲む。
        - **適用範囲**: Type（左側）と Label（右側）の両方で使用可能。ただし、単語の途中を引用符で囲むことはできず、全体を囲む必要がある。
        - **リテラル化**: 引用符で囲まれた文字列内では、Globパターン（`*`, `?` 等）は無効化され、完全一致検索が行われる。
        - **バックスラッシュ**: クォート内での `\"`, `\'`, `\\` 等のエスケープ、および未クォート時の一文字エスケープに使用する。
        - 例: `"extension":rs`, `filename:"project_*"` (Glob無効、完全一致), `filename:\[WIP\]_*` (ブラケットを文字として扱い、末尾はワイルドカード)
    - **グルーピング**:
        - `()`: ラベル計算、集合演算の評価の優先順位を制御するために使用する。
    - **Globパターンのサポート**: 未クォートの文字列では `*`, `?`, `[]`, `[!...]` を Label および Type の両方で使用できる。

2.  **評価の優先順位**:
    - 以下の順序で評価される。
    - `(ラベル計算)` > `ラベル比較` > `TypedTag / ラベル取得` > `集約` > `グループ比較` > `集合演算 `
    - **注**: `集約` や `グループ比較`、`ラベル計算` 等で使用される括弧 `()` 内の式は、再帰的に評価され、常に外側の演算よりも優先される。

3.  **論理演算の解決**: 各比較式およびタグに対し、以下の 2 つの側面を持つ **Universal Selector** を生成。`oneview` またはキャッシュに対してクエリを発行する。
    - **Item Selector**: アイテムを絞り込むための SQL 条件（`WHERE` 句）。
    - **Tag Selector**: アイテムからどの属性（Tag）を取り出すかを定義する SQL 条件。クエリ内容から自動導出される。

4.  **ソート**: 結果は決定論的な順序（`rank DESC, item_id DESC`）でソートされる。

#### **検索インターフェイスとキャッシュ機構**
検索エンジンは、初回レスポンスの高速性と大規模結果のページングを両立させるため、**ResultCache** と **Strategic Fetch** を採用する。

1. **検索関数 `search()`**
    `search()` は `SearchOptions` 構造体を受け取り、状況に応じて自律的にリソース（Connection）を管理する。

    - **引数 (`SearchOptions`)**:
        - `query`: 検索クエリ。
        - `n`: 今回取得するアイテム数（デフォルト 100）。`None`（または0）は全取得。
        - `offset`: 開始位置。`None` の場合は `ResultCache` のメタデータに基づき「現在の Page + 1」を自動計算する。
        - `cid`: 利用する `ResultCache` の ID。
    - **戻り値 (`SearchResponse`)**:
        - `results`: `n` 件分の結果リスト。
        - `cid`: キャッシュ ID（続きがある場合のみ発行）。
        - `has_more`: 続きのデータが存在するかどうかのフラグ。
        - `total_count`: 検索結果の総件数（判明次第格納）。
        - `progress`: キャッシュ生成の進捗状況。

2. **実行プロセスとキャッシュ生成**
    - **n+1 判定**: `search` は内部的に `n + 1` 件を要求し、実際に $n+1$ 件目が取得できた場合に `has_more = true` と判定する。
    - **非同期キャッシュ**: `has_more` が true の場合、一意の `cid` を発行し、別スレッドで全結果を `~/ttfm/cache/{cid}.parquet` に書き出す。
        - 書き出し時、`KV_METADATA` を用いてクエリ文字列やインデックスバージョン等のメタデータを Parquet に直接埋め込む。
    - **オンデマンド取得**: `cid` が指定された呼び出しでは、キャッシュが生成中であれば進捗のみを返し、完了していれば Parquet から高速にデータを読み取る。

3. **ResultCache のライフサイクル**
    - **整合性**: `ttfm index` 等により本体のインデックスが更新された場合、全てのキャッシュは削除される。
    - **クリーンアップ**: キャッシュディレクトリの合計サイズが閾値（例: 3GB）を超えた場合、最終アクセス日時が古いものから自動削除（LRU）される。この閾値はコンフィグで設定可能。


#### 検索における `name` の扱い
- クエリ `name:foo` は、ユーザーが明示的に `foo` と名付けたアイテムと、ファイル名に `foo` を含む（かつ名前未定義の）アイテムの両方にマッチする。
- 物理的なファイル名のみを対象としたい場合は、明示的に `filename:foo` を使用する。

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
Wasmモジュールはホスト（Rust）から見て1つの `IndexingFunction` として振る舞う。
ホスト側で `WasmPluginAdapter`（仮称）を作成し、これが `IndexingFunction` トレイトを実装することで、既存の `FunctionRegistry` にそのまま登録可能とする。

### 5.2 インターフェース定義 (WIT: Wasm Interface Type)
Wasmコンポーネントモデルを採用し、`wit` ファイルでインターフェースを定義する。将来的な拡張性を考慮し、プラグイン種別を特定する `core` インターフェースを設ける。

```wit
package ttfm:plugin

// プラグインの基本情報を定義する共通インターフェース
interface core {
    enum plugin-kind {
        indexing-function,
    }

    record plugin-info {
        name: string,
        version: string,
        kind: plugin-kind,
    }

    // プラグインの種別やバージョンを返す
    get-info: func() -> plugin-info
}

// IndexingFunction (メタデータ抽出) 固有のインターフェース
interface indexing-function {
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
    export indexing-function
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
- **配置場所**: TTFM Home ディレクトリ直下 (`~/.ttfm/ttfm.toml`)

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

## 7. Directory Structure (ディレクトリ構成)
TTFMは、全てのデータと設定をユーザーごとの単一のホームディレクトリで管理する。
カレントディレクトリごとの設定やデータベースはサポートしない。

### 7.1 TTFM Home
- **Linux**: `~/.ttfm/`
- **Windows**: `%USERPROFILE%\.ttfm\`

この場所は環境変数 `TTFM_HOME` によってオーバーライド可能である。

### 7.2 内部構造
- `ttfm.toml`: 設定ファイル
- `db/`: データベースファイル (`.parquet`) の格納先
- `plugins/`: Wasmプラグインの格納先

