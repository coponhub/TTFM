# TTFM Tag Edit Design Specification

Tag Edit は、検索クエリにマッチしたアイテム群に対して、タグの付与・削除・リネーム、
ファイルの移動/リネーム (仮想 mv) を**単一の編集モデル**で扱う機構である。
「ファイルの管理操作はすべてタグの編集である」という TTFM の中心思想を体現する

## 1. コマンド体系

- **`ttfm tag <SearchQuery> <EditQuery>`**: マッチしたアイテムに編集を適用する (付与 / mv)。
- **`ttfm tag <SearchQuery>`** 結果を加工せずそのまま登録する (検索結果の Stored 化 / 結果を note として残す。§5.7)。
- **`ttfm untag <SearchQuery> <TagQuery>`**: マッチしたアイテムからタグを削除する。
  `TagQuery` には projection も指定できる (`project:` = その type のタグ全てを除去する **type 指定の Delete**。
  ラベル値の解決は不要)。 `untag <Q> item_id:` は各 item の `item_id` ラベル除去 = **item 削除**を表す (§5.1)。
- **`ttfm untag <SearchQuery> <TagQuery> <TagCondition>`**: `TagQuery` にマッチしたエントリのうち、
  エントリ自身のメタ属性 (`tagged_at`、タグ自身の `rank` 等) が `TagCondition` を満たすものだけを削除する。
  例: `untag '*:*' 'project:X' 'tagged_at:>T'` → `project:X` エントリのうち `tagged_at > T` のものだけ除去。
- **`ttfm replace <OLD> <NEW>`**: `OLD` (SearchQuery) にマッチするアイテムの終端タグを `NEW` へ付け替える。`tag` + 条件付き `untag` の糖衣 (第6章)。
- **`ttfm decal <From> <To>`**: `From` のアイテムが持つタグ群を `To` のアイテムへ転写する (第9章)。

**`SearchQuery` が編集対象アイテムを解決**し、**`EditQuery` が適用するラベル (新しい値) を解決**する。

### 1.1 対象アイテムの解決
- `<SearchQuery>` の解決は検索系の `search()` を再利用する (`ttfm rank` と同様のパイプライン)。
  ttfm tagは、クエリ → 結果アイテムの `item_id` リスト → 編集の一括適用、という流れ。

### 1.2 EditQuery
`EditQuery` では、評価結果のラベル群が各対象アイテムへ適用される。以下の機能を持つ
- **リテラル `key:value`**: 単一ラベルの付与(もっとも単純な指定)。
- **複数ラベル**: Basic タグの複数 add (例: `project:A status:done`)。
- **メタ型は不可**: `type:/label:/tag:` は EditQuery に記載できず、指定するとエラー。
- **キャプチャ参照 `{n}`**: SearchQuery 内のワイルドカードでキャプチャしたアイテム毎の部分文字列を参照する (第8章)。
- **ディスパッチ**: EditQuery に記載されたタグの type に設定された `EditStrategy` で実際の操作が決まる (第2章)。

### 1.3 確認プロンプト(tag / untag / replace / decal 共通)
- マッチ件数に加え、**編集操作の総数 (対象アイテム数 × EditQuery 解決ラベル数)** を表示し、
  ユーザーに確認を求める。`-y` / `--yes` グローバルフラグ及びポリシーフラグでバイパス可能 (スクリプト・パイプ用途)。

## 2. 編集ディスパッチ

`EditQuery` が返す各ラベルについて、**その type の `TagFunction` が宣言する `EditStrategy` が操作を決定する**。
ディスパッチは `TagRegistry::get(type)` への一本道とする

- その type が registry に登録され**編集可能**なら、宣言された戦略
  (Append / Replace / Relocate / SetFileAttr) で実行する。
- 登録されているが**編集不可**なら **Forbidden** (即エラー)。
- 未登録のユーザー定義型なら、デフォルトで **Append** (重複可で追記)。

`type` / `tag` の定義アイテムをSearchQueryに記載すると、`type:` / `tag:` の定義アイテムにタグを付与できる
(例: `ttfm tag tag:"project:A" rank:100` -> `tag:"project:A"`を登録し、tagに対し`rank:100`を付与)。
EditQuery には `type:/label:/tag:` は記載できない (§1.2)。

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
    Replace,     // 単一値・置換。物理的な格納先が user_tags 行か rank 等のカラムかは
                 // Lens が吸収するため、両者を区別しない (旧 ReplaceCol を統合)

    // --- ホスト内部専用 (組み込み TagFunction のみ) ---
    Relocate,    // path/filename/parentdir/extension → FS rename + 再 index
    SetFileAttr, // mtime → 実ファイルの OS ネイティブ属性 (タイムスタンプ等) を設定
}
```

### 3.1 設計方針
- **宣言的**: `Edit` は「どの戦略か」を宣言するのみで、物理的な実行はホストが行う
- **デフォルトは編集不可**: `edit()` を実装しない (`None` を返す) タグは編集できない。
  これにより組み込み・プラグインが意図せず編集される事故を防ぐ。
- `Forbidden` は `edit() == None` で表現する (列挙子としては持たない)。
  よって `type` / `tag` のための専用戦略は存在しない。

### 3.2 プラグイン向け簡便化
- プラグインが選択できる戦略は **`Append` / `Replace` / Forbidden (未実装)** の3つ。
- WIT の `edit` インターフェースは `enum edit-strategy { append, replace }` とする
  (Relocate / SetFileAttr などのホスト内部戦略はプラグインに公開しない)。
- ボイラープレート削減のため、ホスト側に既製の `Edit` 実装 (`AppendEdit` / `ReplaceEdit` 等の
  ゼロサイズ構造体) を提供し、`edit()` から `Some(&ReplaceEdit)` のように1行で返せるようにする。
- Wasm プラグインは `target!(edit)` で edit インターフェースを有効化し、
  `strategy()` で `append` / `replace` を返すだけで opt-in できる。

### 3.3 組み込みタグの戦略割当
| タグ | 戦略 |
|---|---|
| `path` / `filename` / `parentdir` / `extension` | `Relocate` |
| `mtime` | `SetFileAttr` (Windows/Unix 両対応) |
| `rank` | `Replace` (カラム書き込みは Lens が吸収) |
| `name` | `Replace` (単一値) |
| `size` / `hash` / `file_id` / `item_kind` / `origin` | Forbidden |
| `directory:` 等の Composite (複合タグ) | Forbidden |
| `type` / `tag` (定義アイテムのメタ編集) | EditQuery の型に従う (リネーム/再分類は §6 の合成) |

## 4. 編集パイプライン (`edit`)

タグ編集は `edit` をエントリポイントとし、検索・キャプチャ束縛・計画・確認・適用を束ねる
一連のパイプラインで処理される。対象アイテムの解決には `search` を再利用し (§1.1)、
最終的な永続化は `write` (第5章)、実ファイル操作は `fs_operate` (第7章) が担う。
パイプラインは物理 parquet を直接操作せず、oneview と Lens の抽象レイヤー上で動作する (第5章)。

```rust
pub enum WriteAction {
    Add    { item: ItemId, tags: Vec<TagOp> },  // この item に tags を追記 (格納先は Lens が解決)
    Delete { item: ItemId, tags: Tags },        // この item から tags を除去
}
// Add 内の各タグは由来の strategy を保持する。confirm はこれを見て
// 同一 item・同一 type に Replace が複数来た競合 (§8.2) を検出・解決する。
pub enum TagOp {
    Append(Label),   // 重複可で追記
    Replace(Label),  // 単値。同一 type が複数あれば confirm が1つに畳む
}

pub fn search(store, registry, cache, query, options) -> Result<SearchResponse>;
  // 読み出し (既存)
pub fn search_and_apply_captures(store, registry, search_query: SearchQuery, edit_query: EditQuery) -> Result<Vec<(Item, EditQuery)>>;
  // 検索 + キャプチャ束縛: search を実行し、{n} を各アイテムのタグ値で展開した具体的 EditQuery を返す (§8)
pub fn plan(item_edits: Vec<(Item, EditQuery)>, registry) -> Result<(Vec<(Item, EditQuery)>, Vec<WriteAction>)>;
  // planning: item_edits を strategy で分割し fs 操作列と WriteAction 列を構築する純関数。
  // 単値タグに複数候補が来た場合 (§8.2) もエラーにせず Replace を複数積み、解決は confirm に委ねる
pub fn edit(store, registry, search_query: SearchQuery, edit_query: EditQuery, options: WriteOptions) -> Result<EditResponse>;
  // 編集エントリポイント: search_and_apply_captures → plan → confirm → fs_operate_all + write を束ねる
pub fn fs_operate(fs_ops: Vec<(Item, EditQuery)>, registry) -> Result<()>;
  // fs 操作担当: Relocate + SetFileAttr を実行しインデクサーへ通知する
pub enum QueryType { Tag, Untag }
pub fn modify(item: &Item, query: &str, query_type: QueryType, registry) -> Result<Vec<WriteAction>>;
  // WriteAction の構築 (EditStrategy のコンパイラ, §5.3)。{n} 解決済みの具体文字列を受け取る純関数。
  // plan が per-item で呼び出す内部ヘルパー。内部で parse を呼んでから dispatch する。
  // TagCondition の評価は plan の責務。modify は無条件の TagQuery のみ受け取る。
pub fn write(store, registry, actions: Vec<WriteAction>, options: WriteOptions) -> Result<WriteResponse>;
  // DB への書き込み。WriteAction を Lens 経由で永続化する実行器
```

編集は **`edit` → `search_and_apply_captures` → `plan` → confirm → (`fs_operate` + `write`)** の流れで処理される。
`search_and_apply_captures` が検索とキャプチャ束縛を一括で行い、`{n}` を解決済みの per-item な `(Item, EditQuery)` を返す。
`plan` がそれを strategy で fs/db に分割し、fs 操作列と全 WriteAction を構築する。**`modify` は `plan` の内部から per-item で呼ばれる**。confirm はここで全件を把握してから表示できる。
`modify` は `{n}` を含まない具体文字列を受け取り、内部でパースして strategy をディスパッチし Add/Delete を組み立てる純関数。
`write` は全 WriteAction を一括で受け取り DB に書き込む。
`modify` は loaded Item を主に対象 item の特定に使い、**現値の参照には依存しない** (Replace も旧値不要、後述)。
`modify` は `TagCondition` を持たず常に純関数。`<TagCondition>` が指定された場合は
`plan` が `Store` を使って対象エントリの `tagged_at` 等を取得・評価し、`modify` の呼び方を制御する。
- `TypedTag` 指定 (`project:X`) の場合: 条件を満たすエントリがなければ `modify` を呼ばない（スキップ）。
- `Projection` 指定 (`project:`) の場合: 条件を満たす具体ラベルを取得し、per-entry で `modify` を呼ぶ。
`WriteAction` の生成はあくまで `modify` の責務。

### 4.1 modify — EditStrategy を WriteAction へ展開
`EditStrategy` は実行ロジックではなく、ユーザーの編集意図を `Vec<WriteAction>` へ**展開する規則**である。
`modify` がこの展開を担う純関数で、`write` は戦略を知らず出来上がった action 列を実行するだけ。
削除も独立したエンジンではなく単に `WriteAction::Delete` である。

`modify` は `Relocate` / `SetFileAttr` を受け取った場合はエラーにする (`plan` がこれらを `fs_operate` 側に振り分けた後に呼ぶ前提のため、受け取ること自体が `plan` の実装バグを示す)。

#### EditQuery / TagQuery の構文とパース
`query: &str` の構文は EditQuery (`QueryType::Tag`) と TagQuery (`QueryType::Untag`) で共通の制限付き TTQL サブセットを使う。

- `|` とスペース区切りは**同義**（「列挙された全要素を処理する」和集合）。`project:A status:done` と `project:A | status:done` は同じ意味。
- EditQuery (Tag 方向) 許可: `TypedTag`、`|`・スペース
- TagQuery (Untag 方向) 許可: `TypedTag`、`Projection`、`|`・スペース
- TagCondition 許可: ラベル比較式 (`tagged_at:>T`、`rank:>5` 等。エントリのメタ属性のみ)
- 禁止 (エラー): `&`、Nest、集約、算術演算、`type:/label:/tag:` (EditQuery §1.2)。ラベル比較は TagQuery/EditQuery に書けず TagCondition として分離する

パースは `modify` から呼ばれる内部関数 `parse(query, query_type) -> Result<Vec<EditDirective>>` が担う。

#### 条件付き Delete の in-memory 評価
`tag_condition` が指定された場合、`modify` は `TagQuery` にマッチしたエントリを `tag_condition` で
in-memory フィルタして Delete 対象を絞る。`TagCondition` はエントリ自身のメタ属性
(`tagged_at`、タグ自身の `rank` 等) に対するラベル比較式のみ許可する。
この評価器は `edit/` モジュール内に新規実装する（SQL 生成前提の `query::` 層のロジックは再利用しない）。

| QueryType | 戦略 | WriteAction への展開 |
|---|---|---|
| `Tag` | `Append` | `[Add{item, tags:[Append(tag)]}]` |
| `Tag` | `Replace` | `[Delete{item, tags:[type:]}, Add{item, tags:[Replace(type:new)]}]` (単一値化。Delete は **type 指定**で対象を指し、旧 label を知る必要はない。`Add` の `Replace` マーカーで confirm が多値競合を検出する) |
| `*` | Forbidden (`edit() == None`) | エラー |
| `*` | `Relocate` / `SetFileAttr` | エラー (`plan` の契約違反) |
| `Untag` | `*` | `[Delete{item, tags:[Tag(label)]}]` (具体指定) または `[Delete{item, tags:[Type(tag_type)]}]` (Projection 指定)。条件付きは in-memory フィルタ後に具体 Delete を生成 |

## 5. 永続化エンジン (`write`)

`write` は DBに対する永続化エンジンで、`search` と同じ階層 (free function) に置く。
FileSystemに対する操作は`write`ではなく、`fs_operate`が行う。
両者は物理 parquet を直接操作せず、**oneview と Lens が成す抽象レイヤー**を介して動作する。
oneview は物理テーブル群を統合した論理ビュー、Lens は論理タグと物理スキーマを相互に対応づける
写像であり、両者が物理配置 (`user_tags` / `item_references` / `rank` カラム等の区別) を隠蔽する。
`search` が `Item` を読み出すのに対し、`write` は編集を表す **`WriteAction` の列**を受け取り、
Lens を通じて適用する。

### 5.1 WriteAction を入力とする
- `WriteAction` は **`{item: ItemId, tags: Tags}`** という最小の指示単位である。`search` の戻り値
  `Item` が持つ表示用情報 (`representative` / `intrinsic` / `rank` 等) は `write` に渡らない——
  `Item` から WriteAction への翻訳 (どのタグを足し/消すか) は `modify` の責務であり、
  `write` が受け取るのは確定済みの (item, tags) だけである。
- **`Add { item, tags }`** — item に `tags` を追記する。
  - 対象 `item` が Volatile (DB に無い) の場合は**item 作成**を兼ねる。
  - note の作成も `Add { item: Volatile, tags: [item_kind:note, content:"...", ...] }`
    という純粋なタグ列で表す (`item_kind` / `content` は oneview 上もタグとして見え、Lens が
    item_references のカラムへ振り分ける)。
- **`Delete { item, tags }`** — item から `tags` を除去する。
  - `item_id` は item の identity タグなので`Delete { tags: [item_id:] }` は
    タグ1本の除去ではなく **item 行ごと削除** (全タグ cascade) を意味する。
  - コマンドとしては`untag <Q> item_id:` でDeleteが発生する。 (untag の projection も tag 同様ラベルへ展開して Delete するため、
    マッチ各 item の `item_id` ラベルが対象)。
  - `write` は `Delete` のタグ型で分岐する (`item_id` なら行削除、それ以外はタグ削除)。
- `write` は渡された action 列を受け取り最適化を行うが、順番通りに適用した状態になるよう最適化する。
- `write`の内部でDBの検索等は行わない。

### 5.2 Lens による抽象化 (双方向)
- read / write は同一の StorageMapping を共有し、**read 方向は oneview への射影、write 方向は
  基底テーブルへの解決**として使う (定義は STORE.md §5)。`oneview` は読み取り専用の派生 VIEW で
  書き込めないため、`write` は Lens で各 `WriteAction` のラベルを基底テーブル/カラムへ解決して
  直接書き、完了後に `OneView::recreate` で作り直す。
- この抽象化により、action は格納先の物理差を意識しない。`rank` は物理カラム、一般ユーザータグは
  `user_tags` の行だが、`Add` から見ればどちらも「ラベルの永続化」であり、Lens が書き込み先の違いを吸収する。

### 5.3 守備範囲 (境界)
`write` の作用は **DB に限定**される。

- 対象 item の解決 (action 列の構築) は呼び出し側の責務。
- fs 操作 (`Relocate` / `SetFileAttr`) は `fs_operate` (第7章) の担当。
- `write` 自身は `search` / `index` / `fs::rename` を呼ばない。

これにより `write` はテスト容易な単位に保たれる。

### 5.4 抽象と SQL 最適化の分離
- 編集は **TTFM が扱いやすい抽象 (`WriteAction` の `Add` / `Delete`)** で表現することを優先し、
  SQL の都合に合わせて抽象を歪めない。
- その抽象を物理 SQL へ落とす段階で最適化する。これは `search` が論理クエリを最適化された
  SQL へ変換するのと同じ方針。たとえば同一 item の同一行を指す `Delete(旧) + Add(新)` の
  action 対は、SQL 構成側で in-place `UPDATE` へ融合してよい (parquet の書き直しを 1 回に抑える)。
- したがって `Delete` + `Add` という論理表現を採っても効率は損なわれない。効率上の懸念は
  抽象ではなく **SQL 構成側の最適化**で吸収する。

### 5.6 ItemId 採番
Stored / Volatile の定義は TTFM.md §2.2 を参照。

- `write` が新しいアイテム行を追加するか否かは Stored / Volatile で分岐する (Stored は既存 `item_id` を再利用、
  Volatile は新規追加)。アイテムを追加する局面では item_id の採番が行われ、採番は `write` が所有する。
- **定義アイテムは lazy に登録する**: 通常のタグ付与 (`status:done` を付ける等) では type / tag の
  定義アイテムを eager に登録しない。これらは tag 行から導出可能な Volatile として projection に現れる。
  **Volatile な定義が `Add` の対象になったとき** (例: `ttfm tag tag:"project:ttfm" rank:100` で
  定義へ rank を付与) に限り、write が採番して登録する。付与されるタグ自身が登録の契機となる。
  (`label` 単独は登録対象としない。TTFM.md §2.2 参照。)

### 5.7 SearchQuery 結果の登録
`ttfm tag <SearchQuery>` を **EditQuery 無し**で実行すると、結果を加工せずそのまま登録する
(`search(Q)` の各結果を `Add{item, tags}` として `write` へ流す, §5.1)。
確認プロンプトには「N 件を登録」と明示する。既に Stored の結果は no-op。

結果の種類で登録のされ方が分かれる:
- **定義 (tag / type) の登録 (A)**: 対応する定義アイテムを登録する (kind + content で冪等。既存なら重複しない)。
  `q()` (Eval) で参照したい定義の Stored 化などに使う。
- **Projection / Nest / 計算値を note として残す (B)** (`label:` も含む):
  その時点の**結果全体を1つの `note` アイテム**として保存する。
  - note の `content` には結果の文字列表現を入れる。
  - **由来の保持**: この note には元クエリを `query:"<SearchQuery>"` タグとして注入する
    (例: `query:"project: &: sum(size:)"`)。これにより「何の計算だったか」の文脈が失われない。
    `query` 型は `value` 型と同様、システムが注入する仮想型 (文字列を保持・ファイル由来でない・ユーザー編集不可) として実装する。
  - これらの note は固有 identity を持たないため、再実行のたび**毎回新規**に作られる (重複可)。

## 6. タグの付け替え (`ttfm replace`)

特定の TypedTag を別の TypedTag へ付け替える操作は、専用の cascade 機構を持たず、**キャプチャ付き `tag` +
条件付き `untag` の糖衣**として実現する。コマンド名 `replace` は「本来 Append なタグにも Replace 的な付け替えを
行う」意を表す (Forbidden は付け替え対象にならない)。

`ttfm replace <OLD> <NEW>` の **`OLD`** は編集対象を絞る検索式
  - トップレベルで使えるのは`&` / `-` / `TypedTag` / `()`  のみ
  - `&`で繋がれている最後のTypedTagか、あるいは単独のTypedTagをReplace対象とする
  - `()`内は通常のTTQLに従うが、StoredItemを返さない場合はエラー

### 6.1 展開 (NEW の戦略で分岐)
`tag` ステップを **NEW の `EditStrategy`** でディスパッチし、`untag` は **NEW が `Append` のときだけ**走らせる。

| NEW の戦略 | 展開 | 理由 |
|---|---|---|
| `Append` (多値ユーザータグ) | `tag <OLD> <NEW>` → `untag <NEW> <Replace対象>` | NEW は併存追記なので Replace対象を別途除去 |
| `Replace` / `Relocate` / `SetFileAttr` (単一値) | `tag <OLD> <NEW>` のみ (untag なし) | `tag` が単一値を上書き |
| Forbidden | エラー | §2 ディスパッチで弾く |

- 各操作は通常のディスパッチに乗り、`WriteAction` の `Add` / `Delete` に帰着する (§5.3)。
- **単一値で untag を走らせない理由**は効率だけでなく**必須**でもある: 例えば `extension:txt` を untag しようとすると
  system タグを user_tags から消そうとしてエラーになる。skip して初めて system typed tag の付け替えが成立する。
- **1 論理操作**: 内部で 2 ステップでも確認プロンプトは 1 回 (`OLD → NEW` と対象数を表示)、可能なら 1 トランザクションで
  原子的に適用する。`untag` ステップの対象は step1 で `NEW` が付いたアイテム集合 (item_id) をそのまま使うため、
  `<NEW>` を再クエリせず精密に「step1 の対象から `OLD`のReplace対象を除去」できる (キャプチャ参照の曖昧さも回避)。

### 6.2 例
- `ttfm replace project:A status:A` (project:A → status:A。Append なので tag + untag)
- `ttfm replace 'item_id:123 & project:A' project:B` (item#123 の project:A だけを project:B に。スコープ付き)
- `ttfm replace 'project:*' 'proj:{1}'` (type を project → proj に一括。各 project:X → proj:X、§8.2 で複数ラベルも展開)
- `ttfm replace extension:txt extension:md` (.txt → .md。Relocate でファイルリネーム + 再 index、untag なし)
- `ttfm replace rank:5 rank:10` / `ttfm replace name:foo name:bar` (Replace、tag のみ)

### 6.3 system / プラグイン由来タグ
- **typed tag (label) の付け替えは可**: `extension:txt → extension:md` のように、型の `EditStrategy` (Relocate 等)
  を通じて実行される (単一値なので untag なし)。
- **type 名自体のリネームは不可**: `extension` → `ext` のような型名変更は、`ext:` という型が存在せず EditQuery
  として表現不能。コードで固定された型名は変えられない。

### 6.4 定義アイテムは据え置き
旧タグ `project:A` の定義アイテムが登録済み (Stored) でも削除せず、付与済みのメタ (rank / note 等) も
新タグへ自動移動しない。新タグの定義は付与に伴い、必要なら lazy に登録される (§5.6)。

## 7. ファイルシステム操作 (`fs_operate`)

`fs_operate` は実ファイルを変更する戦略 (`Relocate` / `SetFileAttr`) を実行し、結果を DB へ反映する
(§4 で `edit` が `split_by_strategy` により fs 系をここへ振り分ける)。`SetFileAttr` (mtime 設定) は単純なため
DB 反映 (§7.5) のみで、衝突・ハードリンク・cross-device を伴う `Relocate` (仮想 mv) が本章の主題となる。

`Relocate` では `path` / `filename` / `parentdir` / `extension` が1つのフルパスの射影であり、
いずれか1成分を編集すると新しいフルパスを導出し、実ファイルを移動/リネームする。

### 7.1 Relocate の実行フロー (2フェーズ)
1. **計画フェーズ**: マッチした全アイテムの新ターゲットパスを算出し、以下を**厳格に事前検証**する。
   - 移動先への書き込み権限。
   - 移動先のFSの判定: 同一ファイルシステムなら `fs::rename`に分岐する。
      - 別ファイルシステム (cross-device) ならcopy+delete 経路に分岐する (DB 反映は §7.5)。
          - ターゲットデバイスの空き容量も検証する。
          - cross-device 判定・空き容量・書き込み権限の検査は OS 依存のため、プラットフォーム抽象した
            「移動可能性チェック」に集約する (Unix: `st_dev` / `statvfs`、Windows: volume serial / `GetDiskFreeSpaceExW`)。
          - cross-device 判定は移動元と移動先 (存在する最も近い祖先ディレクトリ、シンボリックリンク解決後) のデバイス ID 比較で行い、
            パス文字列では判定しない。空き容量は厳密保証でなく fail-fast の見積りで、残留失敗は実行フェーズが拾う。
   - ターゲット同士の重複・既存ファイルとの衝突
   - ターゲットの親ディレクトリの存在 (不在時は「ディレクトリを作成しますか？」と確認し、
     yes で `mkdir -p` してから実行。単純な yes/no のため -y では自動作成する。§7.4)
   - 複数 location (ハードリンク) の有無
2. **実行フェーズ**: 検証済みのターゲットに対して移動を実行する (同一 fs は `fs::rename`、
   cross-device は copy → 検証 → 旧削除)。事前検証を通過している前提のため、実行中の失敗は例外的事象として扱う。
   **失敗時はその時点で中断しエラーを表示する。既に完了した移動はロールバックしない**
   (原理的に困難なため、計画フェーズの厳格な事前検証で失敗確率を抑えることで担保する)。

### 7.2 衝突解決 (対話)
複数アイテムが同一ターゲットパスに衝突する場合 (例: 多数のファイルを同名へ)、
エクスプローラ風に**衝突ファイルごと**に以下を選択させる。
- スキップ / 連番サフィックス付与 / キャンセル / スキップ (以降全て) / 連番サフィックス付与 (以降全て)

### 7.3 複数 location (ハードリンク) の扱い
1アイテムが複数の実パスを持つ場合、**パスごと**に以下を選択させる。
- スキップ / 移動する / キャンセル (および「以降全て」)

### 7.4 非対話モード (`-y` / パイプ)
- プロンプトを出せないため、既定では**衝突または複数 location (ハードリンク) を検出した時点で
  error 中断**し、何も適用しない (どちらも人の選択を要するため)。ハードリンクで中断する場合、
  エラーメッセージに**当該アイテムの全実パスを列挙**する。
- 対話で決めるはずの解決は、**関心ごとのポリシーフラグ**で事前宣言すれば -y でも一括適用できる
  (対話の「以降全て」を CLI で先出しするのと同義):
  - `--on-conflict <abort|skip|serial>` — ターゲット衝突時 (§7.2)。`abort`=中断 (既定) /
    `skip`=衝突アイテムを飛ばし残りを処理 / `serial`=連番サフィックス付与。
  - `--on-hardlink <abort|skip|all>` — 複数 location 時 (§7.3)。`abort`=中断 (既定) /
    `skip`=ハードリンクのアイテムを飛ばす / `all`=全リンクを移動。
- フラグを与えない -y は従来どおり安全側 (abort) のまま。
- 例外: **親ディレクトリ不在は -y でも中断せず自動作成する** (`skip/serial` や
  `which link?` のような多択でなく単純な yes/no で、作成が明らかな既定のため。§7.1)。

### 7.5 DB への反映
DB への反映方法は、編集が**実ファイルを変更するか / DB のみを書き換えるか**で分かれる。

- **実ファイルを変更する戦略 (`Relocate` / `SetFileAttr`)**。
  - `Relocate` (同一 fs): `fs::rename` で `file_id` (inode) は不変。**完了後に即時再インデックス**し、
    再 index が Moved として検出して locations を再生成する。
  - `Relocate` (cross-device): copy+delete で `file_id` が変わり Moved 検出が効かないため、**Relocate が
    DB を直接 rebind する** — item_id を据え置き、`file_references` の `file_id` / `device_id` 
    (引き継がないなら `mtime` も) と `locations` の `path` / `parentdir` を新実体の値へ更新する。
    `base_tags` は内容同一で不変、`user_tags` / `system_tags` は item_id キーで保持される。
    indexing には頼らない (後続 `ttfm index`は `file_id` 一致で Unchanged と見える)。
  - `SetFileAttr` (mtime): 変更した mtime は `file_references` のスキャン値であり DB 側が古くなる。
    完了後に再 index が Mtime 変化を検出して `file_references` を更新する。
  - 同一 fs の `Relocate` と `SetFileAttr` は parquet を手書きせず既存のインデックス機構で整合を取る
    (大量編集時のコストはパフォーマンス計測に応じて直接更新方式への切り替えを検討)。cross-device の
    rebind のみ当該 item の行を直接更新する。
- **DB のみを書き換える戦略 (`Append` / `Replace`) → 再インデックス不要**。
  各`Append/Replace`を `WriteAction` (`Add` / `Delete`) 列へ展開し、
  `write` 関数(第5章) が Lens 経由でDBに適用する(`rank` カラム含む)。
  タグの付け替え (`ttfm replace`, 第6章) も`Add/Delete`の合成なので、
  個々のコマンドが上記に帰着する。書き換え後`OneView::recreate` を行う。

## 8. キャプチャと参照

SearchQuery 側のパターンでキャプチャした部分文字列を、EditQuery 側で参照し、
**アイテムごとに動的な値**を構築する機能。一括リネーム等で強力に機能する。

### 8.1 構文
- **キャプチャ**: 未クォートの Glob `*` / `?` / `[]` を**暗黙の位置キャプチャ**として扱う。
  捕捉されるのは **Glob メタ文字が実際にマッチした部分文字列のみ** (ラベル全体ではない)。
  例: `project:tt*` が `ttfm` にマッチした場合、`*` が捕まえるのは `fm` (接頭辞 `tt` は含まない)。
- **参照**: EditQuery 側で `{1}` / `{2}` … と記述する
  (`\1` 形式はシェルでの扱いが煩雑なため波括弧を採用)。
- **参照位置**: `{n}` は type 位置にも書ける (`{1}:{2}` で型ごと動的生成可)。
- **番号付け**: SearchQuery 全体を左から走査した通し番号とする。
- **クォート時は無効**: SearchQuery と同様、クォート内では Glob が無効化され完全一致となるため
  キャプチャされない。リテラルな `{1}` を書きたい場合もクォートする (`"{1}"`)。
- **束縛されない参照は空文字**: 参照 `{n}` がそのアイテムで束縛されない場合
  (キャプチャ総数を超える `{n}`、OR の非マッチ枝にある glob、差集合の除外側にある glob 等) は
  **既定で空文字に展開**する (per-item で実行中断しない)。`ttfm.toml` の設定で「束縛なしをエラーにする」
  厳格モードへ切り替え可能としてもよい。

### 8.2 実行モデル
- キャプチャの展開は `search_and_apply_captures` が担う (§4)。各アイテムのタグ値を SearchQuery の
  glob パターンに当て、`{n}` を具体値で置換した EditQuery を per-item で返す。`modify` は
  `{n}` を含まない具体的な EditQuery のみ受け取り、キャプチャを知らない。
- **1アイテムが同 type の該当ラベルを複数持つ場合** (SearchQuery の glob が item ごとに複数マッチ): マッチ
  ごとにテンプレートを展開し**複数の `(Item, EditQuery)` ペアを生成**する。展開先 strategy で扱いが分かれる:
  - `Append`: 異なる label は複数行として共存 (完全一致は §5.1 の冪等性で吸収)。
  - `Replace`: 同一 item・同一 type に複数の `Replace(tag)` が積まれ、`confirm` が1候補に解決する。
- 静的な多重リテラル (`name:a name:b`) は曖昧なので **`edit` 入口で事前エラー**とする。

### 8.3 例
- `ttfm tag filename:*_draft.txt filename:{1}.txt` (各ファイルの `_draft` を剥がしてリネーム)
- `ttfm tag filename:proj_* project:{1}` (ファイル名の一部を project タグとして付与)
- `ttfm tag filename:*=* {1}:{2}` (`key=value` 名を型付きタグへパース。`author=tanaka` → `author:tanaka`)

## 9. タグの転写 (`ttfm decal`)

`ttfm decal <From> <To>`: `From` のアイテムが持つタグ群を `To` のアイテムへ転写する。
EditQuery は Projection (動的ラベル展開) を含められないため (§1.2)、アイテム間のタグコピーは
本コマンドが担う。内部的にはコマンド層が search(From) でタグ群を取得し、`modify` → `write` (第4章) の
通常経路で `To` へ Add する (検索はコマンド層で行い、`modify` は純関数のまま)。

### 9.1 対象の解決
- `To` にキャプチャ参照 `{n}` が**無い**場合: `From` は単一アイテムに解決されなければならない (複数は error)。
  `To` は複数可 (同じタグ群を一括転写)。
- `To` に `{n}` が**有る**場合: `From` は複数可。各 From アイテムのマッチから `{n}` を束縛し、
  **アイテムごとに To テンプレートを展開**して転写先を解決する (per-item ペアリング。第8章と同一モデル)。

### 9.2 転写対象
- 各タグは type の `EditStrategy` でディスパッチされる (第2章)。Append / Replace 型のみ転写し、
  `Relocate` / `SetFileAttr` / Forbidden 型 (path / extension 等の構造系) は**スキップ**する
  (スキップした型は確認プロンプトに表示する)。decal は DB 編集に限定され、ファイル操作を伴わない。
- 確認プロンプトは tag と同様、対象アイテム数 × 転写タグ数を表示する (§1.1)。

### 9.3 例
- `ttfm decal item_id:123 fileB` (file#123 の全タグを fileB へ複写)
- `ttfm decal tag:"project:A" tag:"status:A"` (`project:A` 定義の全メタを `status:A` 定義へ複写)
- `ttfm decal 'tag:"project:*"' 'tag:"status:{1}"'` (project:A 定義のメタは status:A 定義へ、
  project:B のは status:B へ、それぞれ対応相手に転写)
