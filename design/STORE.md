# TTFM Item Store Design Specification

## TTFMのDataBaseは以下の様式で定義される

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