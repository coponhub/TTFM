# TTFM Item Store Design Specification

TTFMのデータベースは以下の様式で定義される。

## 1. 物理テーブル定義

### 1.1 `file_references` テーブル (実体)
- **ファイルパス**: `.ttfm/db/file_references.parquet`
- `item_id`: Item ID (PRIMARY KEY)
- `rank`: 優先度 (DEFAULT 0)
- `file_id`: OSレベルの識別子 device_id + (Inode number / File Index)
- `size`: ファイルサイズ
- `mtime`: 最終更新日時
- `hash`: コンテンツハッシュ (オプション)
- `is_dir`: ディレクトリかどうかの判定

### 1.2 `locations` テーブル (場所)
- **ファイルパス**: `.ttfm/db/locations.parquet`
- `item_id`: `file_references.item_id` への外部キー
- `path`: フルパス (UNIQUE)
- `filename`: ファイル名
- `parentdir`: 親ディレクトリパス（検索最適化用）
- `extension`: 拡張子
- `scan_hash`: Path・Mtime・Size のhash
- `basename_scan_hash`: filename・Mtime・Size・Inodeのhash

### 1.3 `base_tags` テーブル (自動抽出タグ)
- **ファイルパス**: `.ttfm/db/base_tags.parquet`
- `item_id`: `file_references.item_id` への外部キー
- `type`: タグの種類（例: `size_str`, `type_from_ext`）
- `label_str`, `label_int`, `label_double`, `label_bool`: タグの値（型ごとに物理カラムを持ち、適切な型で格納される）
- ※ 旧 `file_tags`。スキャンごとの洗い替え対象。

### 1.4 `removed_files` テーブル (`user_tags`が付与されているのにファイルが失われたItem)
- **ファイルパス**: `.ttfm/db/removed_files.parquet`
- `item_id`:  Item ID (PRIMARY KEY)
- `rank`: 削除時点のrank
- `file_id`: 削除時点のfile_id
- `scan_hash`, `basename_scan_hash`: 復帰判定に用いる識別子
- `path`, `size`, `mtime`, `is_dir`: 削除時点のメタデータ
- `removed_at`: 削除を検知した日時 (epoch)

## 2. Item Store (Definition Registry)
タグの型(Type)や値(Label)の定義自体を管理するID台帳。
システム定義とユーザー定義の両方が混在するが、IDによって管理される。

### 2.1 `item_references` テーブル
- **ファイルパス**: `.ttfm/db/item_references.parquet`
- `item_id`: ユニークID (PRIMARY KEY)
- `rank`: 優先度 (DEFAULT 0)
- `name`: アイテムの名称
- `item_kind`: アイテムの種類 (`type`, `tag`, `note` のいずれか。`label` は登録対象外＝Volatile)
- `content`: 識別名（Type名等）または Note の本文

## 3. Tag Store (Relations)
Item（FileおよびDefinition）に対するタグ付けを管理する。
データの由来（Origin）によってテーブルを分離することで、ユーザーデータを保護しつつ効率的な更新を実現する。

### 3.1 `system_tags` テーブル (System Tags)
- **ファイルパス**: `.ttfm/db/system_tags.parquet`
- `item_id`: `item_references.item_id` への外部キー
- `type`: タグの種類
- `label_str`, `label_int`, `label_double`, `label_bool`: タグの値
- ※ 定義アイテムの index 時登録の廃止（ITEM.md §8）に伴い、現在このテーブルへの書き手は存在しない
  （oneview の UNION 元としては残存）。

### 3.2 `user_tags` テーブル (User Tags)
- **ファイルパス**: `.ttfm/db/user_tags.parquet`
- `item_id`: 対象のID (`file_references` または `item_references` のいずれか)
- `type`: タグの種類
- `label_str`, `label_int`, `label_double`, `label_bool`: タグの値
- `tagged_at`: その行 (item×tag の関連) が付与された日時 (epoch)。oneview に現れ、行粒度で
  クエリ・削除できる (`untag '*:*' 'project:X & tagged_at:>T'` 等)。`user_tags` 専用
  (`base_tags` / `system_tags` には無く、oneview では NULL)。
- ※ ユーザーが手動で付与した全てのタグ。`ttfm index` によるスキャン更新の影響を受けず、永続化される。

## 4. Unified View (`oneview`)
全てのタグ情報を一元的に扱うための論理ビュー。検索クエリはこのビューに対して実行される。
- `item_id`: 対象のID
- `item_kind`: アイテムの種類 (`file`, `note`, `type`, `tag` 等。`label` は Volatile のみ)
- `rank`: 対象の優先度（ソート用）
- `origin`: タグの由来(**Origin**)
- `type`: タグの種類
- `label_str`, `label_int`, `label_double`, `label_bool`: タグの値（それぞれの物理カラムから合流）
- `tag`: タグ全体（`type:label`）を表す文字列
- `name`: アイテムの名称
- `tagged_at`: 付与日時 (epoch)。`user_tags` 由来の行のみ値を持ち、`base_tags` / `system_tags` 由来は NULL。


### 4.1 Origin & Name Resolution
- **Origin**:
  - `file_references/locations/base_tags/removed_files` 由来の行は`file`(indexingでのplugin関与タグも含む)、 
  - `item_references` 由来の行はItemIDで判定(現状は`builtin/plugin/user`)
  - `system_tags` 由来の行は `builtin`
  - `user_tags` 由来の行は `user`
- **Name**: ユーザー定義（`user_tags` 内の `type:name`）を優先し、存在しなければ `locations.filename` を採用する。

## 5. Storage Mapping (Lens) — 論理タグ ↔ 物理スキーマ
`oneview`（§4）は read 専用の派生 VIEW（複数 parquet の UNION ALL）であり、**書き込めない**。
そのため write は Lens が論理タグを基底テーブル/カラムへ解決して直接書き、完了後に
`OneView::recreate`（§4）で oneview を作り直す。read（検索）と write（編集）は同じ
StorageMapping を共有し、read 方向は oneview への射影、write 方向は基底テーブルへの解決として使う。

StorageMapping は3種:

| マッピング | read（`oneview` 上の表現） | write（書き込み先の基底テーブル） |
|---|---|---|
| **Fixed(col)** | 専用カラム（`item_kind` / `content` 等） | 対象 item の物理テーブルの当該カラムを更新（`item_references` の `item_kind` / `content` は `item_references`） |
| **Basic{column, tag_type}** | 汎用ラベルカラム（`label_str` / `label_int` / `label_double` / `label_bool`）＋ `type` | `user_tags` に行を追加/削除（`type` = tag_type、値を型に応じた `label_*` カラムへ） |
| **Composite** | 他タグへ展開される論理タグ（`directory:` 等） | 直接の格納先を持たず、展開先の各タグの write へ委ねる |

- **書き込み先テーブルは origin で決まる**: ユーザー編集は常に `user_tags`（または Fixed の専用カラム）。
  `base_tags` / `system_tags` は `ttfm index` 専管で、編集の write 対象外。
- **値の型解決**（どの `label_*` カラムを使うか）は read と同じロジックを共有する。
- **`rank` はカラムと `user_tags` の両方に載る**: ユーザーが指定した rank は `user_tags` の
  `rank` タグとして保存し、`file_references` / `item_references` の `rank` カラムは
  rankタグから導出したソート用の値として保持する。カラムの値は system 既定とユーザー指定分の
  合算であり、編集時は変更前の値との差分をカラムと `user_tags` の双方へ加算する。
  index時 は行を system 既定で作り直したうえで `user_tags` の値を加算するため、
  どちらの経路でも同じ値に落ちる。`rank` タグは1つの item につき1行のみを持つ。

## 6. Sorting strategy
- `base_tags`, `system_tags`, `user_tags`は保存時、以下の順序でソートしておき、DuckDBのZoneMapを活用する
    - type ASC
    - label_int ASC
    - label_str ASC
    - item_id ASC
- `item_references` は保存時に `item_id ASC` でソートする
    - item idは区画毎の連続値になっているため、特定の区画にアクセスする際zone mapによる最適化が期待できる
    - 書き出し経路: `tagging::add_item` / edit の write エンジン / `rank::batch_update_rank`
