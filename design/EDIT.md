# TTFM Tag Edit Design Specification

Tag Edit は、検索クエリにマッチしたアイテム群に対して、タグの付与・削除・リネーム、
ファイルの移動/リネーム (仮想 mv)、優先度 (rank) の設定を**単一の編集モデル**で扱う機構である。
「ファイルの管理操作はすべてタグの編集である」という TTFM の中心思想を体現する (Milestone 4)。

## 1. コマンド体系

- **`ttfm tag <QUERY> <EDIT...>`**: マッチしたアイテムに編集を適用する (付与 / mv / rank / rename)。
- **`ttfm untag <QUERY> <TAG>`**: マッチしたアイテムからタグを削除する。`-` (差集合) との混同を避けるため独立コマンドとする。

### 1.1 対象アイテムの解決
- `<QUERY>` の解決は検索系の `search()` を再利用する (`ttfm rank` と同一パイプライン)。
  クエリ → 結果アイテムの `item_id` リスト → 編集の一括適用、という流れ。
- **確認プロンプト必須**: マッチ件数を表示し、ユーザーに確認を求める。
  `-y` / `--yes` グローバルフラグでバイパス可能 (スクリプト・パイプ用途)。

## 2. 編集ディスパッチ

`<EDIT>` は `key:value` の形を取り、**`key` の `TagFunction` が宣言する `EditStrategy` が操作クラスを決定する**。
ディスパッチは `TagRegistry::get(key)` への一本道とする。

- `Some(tf)` かつ `tf.edit() = Some(e)` → `e.strategy()` に従って実行
  (Append / Replace / Relocate / SetMeta / ReplaceCol / Rename)。
- `Some(tf)` かつ `tf.edit() = None` → **Forbidden** (即エラー)。
- `None` (registry 未登録のユーザー定義型) → **Append** (重複可で追記)。

`type` / `label` / `tag` も `TypeFn` / `LabelFn` / `TypedTagFn` という TagFunction であり、
それぞれ `edit()` で `Rename` を宣言する (第4章)。よって定義リネームも特例分岐ではなく
上記の一般ディスパッチに乗る。Rename 固有の「key 一致必須」等は戦略側の検証ルールとして扱う。

### 2.1 操作クラスの混在
- **1コマンド = 1操作クラス**を原則とする。
- 例外として、Basic タグ (Append / Replace) の**複数 add は1コマンドで可**
  (例: `ttfm tag project:A status:done`)。
- mv (Relocate) / rank (ReplaceCol) / rename はそれぞれ独立した意味論・確認フローを持つため、
  Basic タグや他クラスとの**混在はエラー**とする。

## 3. Edit コンポーネント

`TagFunction` は従来の Index / Query / Display に加え、**第4のコンポーネント `Edit`** を束ねる。

```rust
pub trait TagFunction {
    // ... index() / query() / display() ...
    fn edit(&self) -> Option<&dyn Edit> { None } // デフォルト: 編集不可 (Forbidden)
}

pub trait Edit: Send + Sync {
    /// 編集戦略の宣言。実際の実行 (FS rename / カラム更新 / 再 index 等) はホストが担う。
    fn strategy(&self) -> EditStrategy;
    /// 新しい値の検証・正規化 (省略可)。
    fn validate(&self, new: &Label) -> Result<Label> { Ok(new.clone()) }
}

pub enum EditStrategy {
    // --- プラグイン作者が選択可能 (WIT に公開) ---
    Append,      // 重複可で追記 (一般ユーザータグ相当)
    Replace,     // 単一値・置換

    // --- ホスト内部専用 (組み込み TagFunction のみ) ---
    Relocate,    // path/filename/parentdir/extension → FS rename + 再 index
    SetMeta,     // mtime → 実ファイルのメタデータ設定
    ReplaceCol,  // rank → 物理カラム上書き
    Rename,      // type/label/tag → 定義リネーム (TypeFn/LabelFn/TypedTagFn が宣言)
}
```

### 3.1 設計方針
- **宣言的**: `Edit` は「どの戦略か」を宣言するのみで、物理的な実行はホストが行う
  (PLUGIN.md の「Wasm 側に SQL 生成を持たせない」原則と同じ)。
- **デフォルトは編集不可**: `edit()` を実装しない (`None` を返す) タグは編集できない。
  これにより組み込み・プラグインが意図せず編集される事故を防ぐ。
- `Forbidden` は `edit() == None` で表現する (列挙子としては持たない)。
- `Rename` は `TypeFn` / `LabelFn` / `TypedTagFn` という組み込み TagFunction が `edit()` で宣言する
  **ホスト内部専用の戦略**。プラグインは選択できない。
  EDIT 側キーが `type` / `label` / `tag` のとき、ディスパッチで自然に Rename へ振り分けられる。

### 3.2 プラグイン向け簡便化
- プラグインが選択できる戦略は **`Append` / `Replace` / Forbidden (未実装)** の3つ。
- 各プラグインは単一の type を表すため、`Rename` のような構造操作は持たない
  (プラグイン由来の label/tag インスタンスのリネームはホスト側 Rename が担う)。
- WIT の `edit` インターフェースは `enum edit-strategy { append, replace }` 程度の最小公開とする
  (Relocate / SetMeta / ReplaceCol / Rename などのホスト内部戦略はプラグインに公開しない)。
- ボイラープレート削減のため、ホスト側に既製の `Edit` 実装 (`AppendEdit` / `ReplaceEdit` 等の
  ゼロサイズ構造体) を提供し、`edit()` から `Some(&ReplaceEdit)` のように1行で返せるようにする。
- Wasm プラグインは `target!(edit)` で edit インターフェースを有効化し、
  `strategy()` で `append` / `replace` を返すだけで opt-in できる。

### 3.3 組み込みタグの戦略割当
| タグ | 戦略 |
|---|---|
| `path` / `filename` / `parentdir` / `extension` | `Relocate` |
| `mtime` | `SetMeta` (Windows/Unix 両対応) |
| `rank` | `ReplaceCol` |
| `name` | `Replace` (単一値) |
| `size` / `hash` / `file_id` / `item_kind` / `origin` | Forbidden |
| `directory:` 等の Composite (volatile) | Forbidden |
| `type` / `label` / `tag` | Rename (ユーザー定義のみ。第4章) |

## 4. Rename (定義リネーム)

タグ定義そのものの名称変更。`TypeFn` / `LabelFn` / `TypedTagFn` が `edit()` で `EditStrategy::Rename`
を宣言しており、EDIT 側キーが `type` / `label` / `tag` のときにディスパッチから発火する。

- **対象はユーザー定義タグのみ**。`extension` 等の system 由来 / プラグイン由来の型名は
  コードで固定されているためリネーム不可 (DB だけ変えると `name()` と乖離する)。
- **key 一致必須**: QUERY 側と EDIT 側のキーが同一の場合のみ許可する (例: `type:... type:...`)。
- **cascade 範囲**: `item_references` の `content` と `user_tags` の該当 `type`
  (label の場合は `label_str`) を一括更新する。system_tags / base_tags は system 由来の型しか
  持たないため触る必要がない。
- 例:
  - `ttfm tag tag:"project:A" tag:"project:Alpha"` (タグ `project:A` を `project:Alpha` に)
  - `ttfm tag type:projet type:project` (型名のタイプミス修正)

## 5. 仮想 mv (Relocate)

`path` / `filename` / `parentdir` / `extension` は1つのフルパスの射影である。
いずれか1成分を編集すると新しいフルパスを導出し、実ファイルを `fs::rename` で移動/リネームする。

### 5.1 実行フロー (2フェーズ)
1. **計画フェーズ**: マッチした全アイテムの新ターゲットパスを算出し、以下を**厳格に事前検証**する。
   - 書き込み権限 / 同一ファイルシステム (cross-device rename 不可)
   - ターゲット同士の重複・既存ファイルとの衝突
   - ターゲットの親ディレクトリの存在
   - 複数 location (ハードリンク) の有無
2. **実行フェーズ**: 検証済みのターゲットに対して `fs::rename` を実行する。
   事前検証を通過している前提のため、実行中の失敗は例外的事象として扱う。

### 5.2 衝突解決 (対話)
複数アイテムが同一ターゲットパスに衝突する場合 (例: 多数のファイルを同名へ)、
エクスプローラ風に**衝突ファイルごと**に以下を選択させる。
- スキップ / 連番サフィックス付与 / キャンセル / スキップ (以降全て) / 連番サフィックス付与 (以降全て)

### 5.3 複数 location (ハードリンク) の扱い
1アイテムが複数の実パスを持つ場合、**パスごと**に以下を選択させる。
- スキップ / 移動する / キャンセル (および「以降全て」)

### 5.4 非対話モード (`-y` / パイプ)
- プロンプトを出せないため、**衝突を検出した時点で error 中断**し、何も適用しない。

### 5.5 DB への反映
DB への反映方法は、編集が**実ファイルを変更するか / DB のみを書き換えるか**で分かれる。

- **実ファイルを変更する戦略 (`Relocate` / `SetMeta`) → 完了後に即時再インデックス**。
  - `Relocate`: `file_id` (inode) は不変のため、再 index が Moved として検出し locations を再生成する。
  - `SetMeta` (mtime): 変更した mtime は `file_references` のスキャン値であり、
    DB 側が古くなる。再 index が Mtime 変化を検出して `file_references` を更新する。
  - いずれも parquet を手書き更新せず、既存のインデックス機構で整合を取る。
    (大量編集時のコストはパフォーマンス計測に応じて直接更新方式への切り替えを検討する。)
- **DB のみを書き換える戦略 (`Append` / `Replace` / `ReplaceCol` / `Rename`) → oneview の再構築のみ**。
  再インデックスは不要。`user_tags` への追記/置換・rank カラム更新・定義リネーム後に
  `OneView::recreate` を呼ぶ。

## 6. 値セマンティクス

- **name**: 単一値。書き込みは既存 name の置換となる (1アイテム1名前)。
- **一般ユーザー定義タグ**: `Append` (重複可)。同一 `type:label` の再付与も許容する。
  値を変えたい場合は第4章の `tag:` 形式によるリネームを使う。
- **TagFunction が提供するタグ**: デフォルトで編集不可。宣言した `EditStrategy` に従う。
- **将来拡張**: ユーザー定義型を Unique にしたい場合、Type アイテムに `unique:true` のような
  メタタグを付与することで重複不可 (置換またはエラー) にできるようにする (M4 では未実装)。

## 7. キャプチャとバックリファレンス (per-item テンプレート)

QUERY 側のパターンでキャプチャした部分文字列を、EDIT 側で参照し、
**アイテムごとに動的な値**を構築する機能。一括リネーム等で強力に機能する。

### 7.1 構文
- **キャプチャ**: 未クォートの Glob `*` / `?` / `[]` を**暗黙の位置キャプチャ**として扱う。
- **参照**: EDIT 側で `{1}` / `{2}` … と記述する
  (`\1` 形式はシェルでの扱いが煩雑なため波括弧を採用)。
- **番号付け**: クエリ全体を左から走査した通し番号とする。
- **クォート時は無効**: QUERY と同様、クォート内では Glob が無効化され完全一致となるため
  キャプチャされない。リテラルな `{1}` を書きたい場合もクォートする (`"{1}"`)。
- **存在しない参照** (例: キャプチャが2個しかないのに `{3}`) は検証段でエラーとする
  (per-item で黙って空文字にしない)。

### 7.2 実行モデル
- EDIT 値が定数ではなくアイテムごとの関数になるため、
  各アイテムのタグ値からキャプチャを抽出し、テンプレートを展開してから書き込む。

### 7.3 例
- `ttfm tag filename:*_draft.txt filename:{1}.txt` (各ファイルの `_draft` を剥がしてリネーム)
- `ttfm tag filename:proj_* project:{1}` (ファイル名の一部を project タグとして付与)
