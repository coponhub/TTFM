# TTFM Tag Edit Design Specification

Tag Edit は、検索クエリにマッチしたアイテム群に対して、タグの付与・削除・リネーム、
ファイルの移動/リネーム (仮想 mv)、優先度 (rank) の設定を**単一の編集モデル**で扱う機構である。
「ファイルの管理操作はすべてタグの編集である」という TTFM の中心思想を体現する (Milestone 4)。

## 1. コマンド体系

- **`ttfm tag <SearchQuery> <EditQuery>`**: マッチしたアイテムに編集を適用する (付与 / mv / rank)。
- **`ttfm tag <SearchQuery>`** (EditQuery 省略): 結果を加工せずそのまま登録する (定義の Stored 化 / 結果を note として残す。§4.7)。
- **`ttfm untag <SearchQuery> <TagQuery>`**: マッチしたアイテムからタグを削除する。`-` (差集合) との混同を避けるため独立コマンドとする。
  `TagQuery` には projection も指定できる (`project:` = その type のタグ全てを除去する **type 指定の Delete**。
  ラベル値の解決は不要)。これにより `untag <Q> item_id:` は各 item の `item_id` ラベル除去 = **item 削除**を表す (§4.1)。
- **`ttfm replace <OLD> <NEW>`**: `OLD` を持つ全アイテムの TypedTag を `OLD` → `NEW` へ付け替える。`tag` + 条件付き `untag` の糖衣 (第5章)。
- **`ttfm decal <From> <To>`**: `From` のアイテムが持つタグ群を `To` のアイテムへ転写する (第9章)。

`SearchQuery` / `EditQuery` はいずれも TTQL クエリであり、両者は対称をなす。
**`SearchQuery` が編集対象アイテムを解決**し、**`EditQuery` が適用するラベル (新しい値) を解決**する。

### 1.1 対象アイテムの解決
- `<SearchQuery>` の解決は検索系の `search()` を再利用する (`ttfm rank` と同一パイプライン)。
  クエリ → 結果アイテムの `item_id` リスト → 編集の一括適用、という流れ。
- **確認プロンプト必須**: マッチ件数に加え、**編集操作の総数 (対象アイテム数 × EditQuery 解決ラベル数)** を
  表示し、ユーザーに確認を求める。`-y` / `--yes` グローバルフラグでバイパス可能 (スクリプト・パイプ用途)。

### 1.2 EditQuery (編集内容の解決)
`EditQuery` も TTQL クエリであり、評価結果のラベル群が各対象アイテムへ適用される。
- **リテラル `key:value`**: 単一ラベルの付与 (縮退形)。
- **複数ラベル**: Basic タグの複数 add (例: `project:A status:done`)。
- **Projection は不可**: 検索を要する動的なラベル展開 (`tag:"project:A" & *:` 等) は EditQuery では行わない。
  別アイテムからのタグ複写は `ttfm decal` (第9章) が担う。これにより EditQuery の解決は DB アクセス不要となる。
- **`tag:"t:l"` リテラル**: 完結した TypedTag として Append する。`type:project` / `label:foo` のような
  付与すべき TypedTag が確定しない指定はエラー。
- **キャプチャ参照 `{n}`**: SearchQuery でキャプチャした部分文字列を参照する per-item テンプレート (第8章)。
- **ディスパッチ**: EditQuery が返す各ラベルの type が宣言する `EditStrategy` で操作クラスが決まる (第2章)。
  キャプチャ参照が無ければ EditQuery はグローバルに1回評価され、有れば per-item で評価される。

## 2. 編集ディスパッチ

`EditQuery` が返す各ラベルについて、**その type の `TagFunction` が宣言する `EditStrategy` が操作クラスを決定する**。
ディスパッチは `TagRegistry::get(type)` への一本道とする (リテラル `key:value` は単一ラベルを返す縮退形)。

- `Some(tf)` かつ `tf.edit() = Some(e)` → `e.strategy()` に従って実行
  (Append / Replace / Relocate / SetFileAttr)。
- `Some(tf)` かつ `tf.edit() = None` → **Forbidden** (即エラー)。
- `None` (registry 未登録のユーザー定義型) → **Append** (重複可で追記)。

`type` / `tag` の定義アイテムも item なので、**それ自身のメタは通常のタグ付与で編集**する
(例: `ttfm tag tag:"project:A" rank:100` は EditQuery の `rank` 型 → `Replace` でディスパッチ)。
なお EditQuery 終端が `type:` / `label:` / `tag:` のときの扱い (完結 TypedTag なら Append、不完全ならエラー) は §1.2 を参照。

### 2.1 操作クラスの混在
- **1コマンド = 1操作クラス**を原則とする。
- 例外として、Basic タグ (Append / Replace) の**複数 add は1コマンドで可**
  (例: `ttfm tag project:A status:done`)。
- mv (Relocate) / rename はそれぞれ独立した意味論・確認フローを持つため、
  Basic タグや他クラスとの**混在はエラー**とする。
  (rank は `Replace` = Basic タグ扱いとなり、他の Basic タグと同一コマンドで混在可。)

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
  (PLUGIN.md の「Wasm 側に SQL 生成を持たせない」原則と同じ)。
- **デフォルトは編集不可**: `edit()` を実装しない (`None` を返す) タグは編集できない。
  これにより組み込み・プラグインが意図せず編集される事故を防ぐ。
- `Forbidden` は `edit() == None` で表現する (列挙子としては持たない)。
  よって `type` / `tag` のための専用戦略は存在しない。

### 3.2 プラグイン向け簡便化
- プラグインが選択できる戦略は **`Append` / `Replace` / Forbidden (未実装)** の3つ。
- 各プラグインは単一の type を表すため、構造操作 (リネーム等) は持たない。
- WIT の `edit` インターフェースは `enum edit-strategy { append, replace }` 程度の最小公開とする
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
| `type` / `tag` (定義アイテムのメタ編集) | EditQuery の型に従う (リネーム/再分類は §5 の合成) |

## 4. 永続化エンジン `write`

`write` は、`search` の鏡像となる**永続化エンジン**であり、`search` と同じ階層
(free function) に置く。両者はいずれも物理 parquet を直接操作せず、**oneview と Lens が
成す抽象レイヤー**を介して動作する。oneview は物理テーブル群を統合した論理ビュー、
Lens は論理タグと物理スキーマを相互に対応づける写像であり、両者が物理配置
(`user_tags` / `item_references` / `rank` カラム等の区別) を隠蔽する。

`search` が抽象レイヤーから `Item`(旧 `SearchResult`) を読み出すのに対し、`write` は編集を表す
**`WriteAction` の列**を受け取り、Lens を通じて抽象レイヤーへ適用する。

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

// 読み出し (既存)
pub fn search(store, registry, cache, query, options) -> Result<SearchResponse>;
// 検索 + キャプチャ束縛: search を実行し、{n} を各アイテムのタグ値で展開した具体的 EditQuery を返す (§8)
pub fn search_and_apply_captures(store, registry, search_query: SearchQuery, edit_query: EditQuery) -> Result<Vec<(Item, EditQuery)>>;
// planning: item_edits を strategy で分割し fs 操作列と WriteAction 列を構築する純関数。
// 単値タグに複数候補が来た場合 (§8.2) もエラーにせず Replace を複数積み、解決は confirm に委ねる
pub fn plan(item_edits: Vec<(Item, EditQuery)>, registry) -> Result<(Vec<(Item, EditQuery)>, Vec<WriteAction>)>;
// 編集エントリポイント: search_and_apply_captures → plan → confirm → fs_operate_all + write を束ねる
pub fn edit(store, registry, search_query: SearchQuery, edit_query: EditQuery, options: WriteOptions) -> Result<EditResponse>;
// fs 操作担当: Relocate + SetFileAttr を実行しインデクサーへ通知する
pub fn fs_operate_all(fs_ops: Vec<(Item, EditQuery)>, registry) -> Result<()>;
// WriteAction の構築 (EditStrategy のコンパイラ, §4.3)。{n} 解決済みの具体的タグのみ受け取る純関数
pub fn modify(item: &Item, query: EditQuery, registry) -> Result<Vec<WriteAction>>;
// DB への書き込み。WriteAction を Lens 経由で永続化する実行器
pub fn write(store, registry, actions: Vec<WriteAction>, options: WriteOptions) -> Result<WriteResponse>;
```

編集は **`edit` → `search_and_apply_captures` → `plan` → confirm → (`fs_operate_all` + `write`)** の流れで処理される。
`search_and_apply_captures` が検索とキャプチャ束縛を一括で行い、`{n}` を解決済みの per-item な `(Item, EditQuery)` を返す。
`plan` がそれを strategy で fs/db に分割し、fs 操作列と全 WriteAction を構築する。confirm はここで全件を把握してから表示できる。
`modify` は `{n}` を含まない具体的なタグのみ受け取り、strategy をディスパッチして Add/Delete を組み立てる純関数。
`write` は全 WriteAction を一括で受け取り DB に書き込む。
`modify` は loaded Item を主に対象 item の特定に使い、**現値の参照には依存しない** (Replace も旧値不要、後述)。
但し**条件付き Delete** (例: `untag '*:*' 'project:X & tagged_at:>T'`) では、loaded Item のタグ値
(`tagged_at` 等) を述語で in-memory フィルタして消す行を選ぶため、現値を参照する。この場合も Item は
検索で全タグをロード済みなので **DB 再検索は不要**で、純関数性は保たれる (WriteAction は具体タグのまま)。

### 4.1 WriteAction を入力とする
- `WriteAction` は **`{item: ItemId, tags: Tags}`** という最小の指示単位である。`search` の戻り値
  `Item` が持つ表示用情報 (`representative` / `intrinsic` / `rank` 等) は `write` に渡らない——
  `Item` から WriteAction への翻訳 (どのタグを足し/消すか) は `modify` の責務であり、
  `write` が受け取るのは確定済みの (item, tags) だけである。
- Add と Delete は対称で、Add は `tags` を追記し、Delete は `tags` を除去する。
- **item の作成も `Add` で表す**: 対象 `item` が Volatile (DB に無い) の Add は item 作成を兼ねる。
  note の作成も `Add { item: Volatile, tags: [item_kind:note, content:"...", ...] }` という
  純粋なタグ列で表現する (`item_kind` / `content` は oneview 上もタグとして見えており、
  Lens が item_references のカラムへ振り分ける)。専用の作成アクションは無い。
- **item の削除も `Delete` で表す**: `item_id` は item の identity タグなので、それを除去する
  `Delete { tags: [item_id:] }` は、タグ1本の除去ではなく **item 行ごと削除** (全タグ cascade) を意味する。
  `untag <Q> item_id:` がこれを生む (untag の projection も tag 同様ラベルへ展開して Delete するため、
  マッチ各 item の `item_id` ラベルが対象になる)。専用の `DeleteItem` アクションや削除コマンドは設けず、
  **すべてタグ編集で貫く**。ファイルは対象外 (作成/削除とも index 専管)。
  `write` は `Delete` のタグ型で分岐する (`item_id` なら行削除、それ以外はタグ削除)。
- `write` は渡された action 列を**そのまま**適用する。item の「あるべき状態」を
  暗黙に再構築するような差分は取らない (明示された `Add` / `Delete` だけを実行するので、
  渡し忘れたタグが消える危険がない)。

### 4.2 Lens による抽象化 (双方向)
- Lens は read 時、TagFunction の Query が宣言する論理スキーマ (StorageMapping) を用いて
  「論理タグ → 物理スキーマ」を解決し、oneview として統一的に読み出せるようにする。
  `write` は**同じ StorageMapping を逆向き**に使い、各 `WriteAction` のラベルを
  書き込み先へ解決する。スキーマ定義は1つで、read と write が共有する。
- この抽象化により、action は格納先の物理差を意識しない。`rank` は物理カラム、
  一般ユーザータグは `user_tags` の行だが、`Add` から見ればどちらも「ラベルの永続化」であり、
  Lens が書き込み先の違いを吸収する。**これが `Replace` と旧 `ReplaceCol` を区別しない理由**である。

### 4.3 EditStrategy は WriteAction へのコンパイラ
`EditStrategy` は実行ロジックではなく、ユーザーの編集意図を `Vec<WriteAction>` へ
**展開する規則**である。`write` 自身は戦略を知らず、action 列を実行するだけ。
削除も独立したエンジンではなく単に `WriteAction::Delete` である。

| 戦略 | WriteAction への展開 |
|---|---|
| `Append` | `[Add{item, tags:[Append(tag)]}]` |
| `Replace` | `[Delete{item, tags:[type:]}, Add{item, tags:[Replace(type:new)]}]` (単一値化。Delete は **type 指定**で対象を指し、旧 label を知る必要はない。`Add` の `Replace` マーカーで confirm が多値競合を検出する) |

- 物理書き込みは Lens 経由で各 action の格納先 (`user_tags` 行 / `rank` カラム等) を解決し、
  最後に `OneView::recreate` で oneview を再構築して完了する。
- 既存の `add_item` / `tag_item` / `untag` 等の個別関数は、対応する `WriteAction` を
  組み立てて `write` を呼ぶ薄いラッパに再構成される (`untag` = `Delete` のみ、tag = `Add`)。

- **`write` の範囲外**: `Relocate` / `SetFileAttr` は fs 操作・再 index が必要なため `fs_operate` が担う (第6章)。
  `edit` が `split_by_strategy` で振り分けるため `modify` / `write` はこれらを知らない。

### 4.4 守備範囲 (境界)
- `write` は**渡された `WriteAction` の対象 item だけ**を操作する純粋な永続化単位であり、
  内部で `search` / `index` やファイルシステム操作 (`fs::rename` 等) を呼び出さない。
- 対象の解決 (どの item をどう編集するか = action 列の構築) は呼び出し側の責務である。
- 実ファイル変更を伴う `Relocate` / `SetFileAttr` の fs 操作・再 index・対話プロンプトは、
  `edit` が `split_by_strategy` で分割し `fs_operate` が担う (第6章)。
- この境界により、`write` は副作用が DB に限定されたテスト容易な単位となる。

### 4.5 抽象と SQL 最適化の分離
- 編集は **TTFM が扱いやすい抽象 (`WriteAction` の `Add` / `Delete`)** で表現することを優先し、
  SQL の都合に合わせて抽象を歪めない。
- その抽象を物理 SQL へ落とす段階で最適化する。これは `search` が論理クエリを最適化された
  SQL へ変換するのと同じ方針。たとえば同一 item の同一行を指す `Delete(旧) + Add(新)` の
  action 対は、SQL 構成側で in-place `UPDATE` へ融合してよい (parquet の書き直しを 1 回に抑える)。
- したがって `Delete` + `Add` という論理表現を採っても効率は損なわれない。効率上の懸念は
  抽象ではなく **SQL 構成側の最適化**で吸収する。

### 4.6 ItemId 採番
Stored / Volatile の定義は TTFM.md §2.2 を参照。

- `write` が新しいアイテム行を追加するか否かは Stored / Volatile で分岐する (Stored は既存 `item_id` を再利用、
  Volatile は新規追加)。アイテムを追加する局面では item_id の採番が行われ、採番は `write` が所有する。
- **定義アイテムは lazy に登録する**: 通常のタグ付与 (`status:done` を付ける等) では type / tag の
  定義アイテムを eager に登録しない。これらは tag 行から導出可能な Volatile として projection に現れる。
  **Volatile な定義が `Add` の対象になったとき** (例: `ttfm tag tag:"project:ttfm" rank:100` で
  定義へ rank を付与) に限り、write が採番して登録する。付与されるタグ自身が登録の契機となる。
  (`label` 単独は登録対象としない。TTFM.md §2.2 参照。)

### 4.7 SearchQuery 結果の登録
`ttfm tag <SearchQuery>` を **EditQuery 無し**で実行すると、結果を加工せずそのまま登録する
(`search(Q)` の各結果を `Add{item, tags}` として `write` へ流す, §4.1)。
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

## 5. タグの付け替え (`ttfm replace`)

特定の TypedTag を別の TypedTag へ付け替える操作は、専用の cascade 機構を持たず、**キャプチャ付き `tag` +
条件付き `untag` の糖衣**として実現する。`ttfm replace <OLD> <NEW>` は、`OLD` を持つ全アイテムの `OLD` を
`NEW` へ付け替える 1 論理操作である。コマンド名 `replace` は「本来 Append なタグにも Replace 的な付け替えを
行う」意を表す。スコープ限定 (一部のアイテムだけ) は `replace` の役目ではなく、`tag` / `untag` を直接使う。

### 5.1 展開 (NEW の戦略で分岐)
`tag` ステップを **NEW の `EditStrategy`** でディスパッチし、`untag` は **NEW が `Append` のときだけ**走らせる。

| NEW の戦略 | 展開 | 理由 |
|---|---|---|
| `Append` (多値ユーザータグ) | `tag <OLD> <NEW>` → `untag <NEW> <OLD>` | NEW は併存追記なので OLD を別途除去 |
| `Replace` / `Relocate` / `SetFileAttr` (単一値) | `tag <OLD> <NEW>` のみ (untag なし) | `tag` が単一値を上書きし OLD は既に消える |
| Forbidden | エラー | §2 ディスパッチで弾く |

- 各操作は通常のディスパッチに乗り、`WriteAction` の `Add` / `Delete` に帰着する (§4.3)。
- **単一値で untag を走らせない理由**は効率だけでなく**必須**でもある: 例えば `extension:txt` を untag しようとすると
  system タグを user_tags から消そうとしてエラーになる。skip して初めて system typed tag の付け替えが成立する。
- **1 論理操作**: 内部で 2 ステップでも確認プロンプトは 1 回 (`OLD → NEW` と対象数を表示)、可能なら 1 トランザクションで
  原子的に適用する。`untag` ステップの対象は step1 で `NEW` が付いたアイテム集合 (item_id) をそのまま使うため、
  `<NEW>` を再クエリせず精密に「step1 の対象から `OLD` を除去」できる (キャプチャ参照の曖昧さも回避)。

### 5.2 例
- `ttfm replace project:A status:A` (project:A → status:A。Append なので tag + untag)
- `ttfm replace 'project:*' 'proj:{1}'` (type を project → proj に一括。各 project:X → proj:X、§8.2 で複数ラベルも展開)
- `ttfm replace extension:txt extension:md` (.txt → .md。Relocate でファイルリネーム + 再 index、untag なし)
- `ttfm replace rank:5 rank:10` / `ttfm replace name:foo name:bar` (Replace、tag のみ)

### 5.3 system / プラグイン由来タグ
- **typed tag (label) の付け替えは可**: `extension:txt → extension:md` のように、型の `EditStrategy` (Relocate 等)
  を通じて実行される (単一値なので untag なし)。
- **type 名自体のリネームは不可**: `extension` → `ext` のような型名変更は、`ext:` という型が存在せず EditQuery
  として表現不能。コードで固定された型名は変えられない。

### 5.4 定義アイテムは据え置き
旧タグ `project:A` の定義アイテムが登録済み (Stored) でも削除せず、付与済みのメタ (rank / note 等) も
新タグへ自動移動しない。新タグの定義は付与に伴い、必要なら lazy に登録される (§4.6)。

## 6. 仮想 mv (Relocate)

`path` / `filename` / `parentdir` / `extension` は1つのフルパスの射影である。
いずれか1成分を編集すると新しいフルパスを導出し、実ファイルを `fs::rename` で移動/リネームする。

### 6.1 実行フロー (2フェーズ)
1. **計画フェーズ**: マッチした全アイテムの新ターゲットパスを算出し、以下を**厳格に事前検証**する。
   - 書き込み権限 / 同一ファイルシステム (cross-device rename 不可)
   - ターゲット同士の重複・既存ファイルとの衝突
   - ターゲットの親ディレクトリの存在 (不在時は「ディレクトリを作成しますか？」と確認し、
     yes で `mkdir -p` してから rename。単純な yes/no のため -y では自動作成する。§6.4)
   - 複数 location (ハードリンク) の有無
2. **実行フェーズ**: 検証済みのターゲットに対して `fs::rename` を実行する。
   事前検証を通過している前提のため、実行中の失敗は例外的事象として扱う。
   **失敗時はその時点で中断しエラーを表示する。既に完了したリネームはロールバックしない**
   (原理的に困難なため、計画フェーズの厳格な事前検証で失敗確率を抑えることで担保する)。

### 6.2 衝突解決 (対話)
複数アイテムが同一ターゲットパスに衝突する場合 (例: 多数のファイルを同名へ)、
エクスプローラ風に**衝突ファイルごと**に以下を選択させる。
- スキップ / 連番サフィックス付与 / キャンセル / スキップ (以降全て) / 連番サフィックス付与 (以降全て)

### 6.3 複数 location (ハードリンク) の扱い
1アイテムが複数の実パスを持つ場合、**パスごと**に以下を選択させる。
- スキップ / 移動する / キャンセル (および「以降全て」)

### 6.4 非対話モード (`-y` / パイプ)
- プロンプトを出せないため、既定では**衝突または複数 location (ハードリンク) を検出した時点で
  error 中断**し、何も適用しない (どちらも人の選択を要するため)。ハードリンクで中断する場合、
  エラーメッセージに**当該アイテムの全実パスを列挙**する。
- 対話で決めるはずの解決は、**関心ごとのポリシーフラグ**で事前宣言すれば -y でも一括適用できる
  (対話の「以降全て」を CLI で先出しするのと同義):
  - `--on-conflict <abort|skip|serial>` — ターゲット衝突時 (§6.2)。`abort`=中断 (既定) /
    `skip`=衝突アイテムを飛ばし残りを処理 / `serial`=連番サフィックス付与。
  - `--on-hardlink <abort|skip|all>` — 複数 location 時 (§6.3)。`abort`=中断 (既定) /
    `skip`=ハードリンクのアイテムを飛ばす / `all`=全リンクを移動。
- フラグを与えない -y は従来どおり安全側 (abort) のまま。
- 例外: **親ディレクトリ不在は -y でも中断せず自動作成する** (`skip/serial` や
  `which link?` のような多択でなく単純な yes/no で、作成が明らかな既定のため。§6.1)。

### 6.5 DB への反映
DB への反映方法は、編集が**実ファイルを変更するか / DB のみを書き換えるか**で分かれる。

- **実ファイルを変更する戦略 (`Relocate` / `SetFileAttr`) → 完了後に即時再インデックス**。
  - `Relocate`: `file_id` (inode) は不変のため、再 index が Moved として検出し locations を再生成する。
  - `SetFileAttr` (mtime): 変更した mtime は `file_references` のスキャン値であり、
    DB 側が古くなる。再 index が Mtime 変化を検出して `file_references` を更新する。
  - いずれも parquet を手書き更新せず、既存のインデックス機構で整合を取る。
    (大量編集時のコストはパフォーマンス計測に応じて直接更新方式への切り替えを検討する。)
- **DB のみを書き換える戦略 (`Append` / `Replace`) → 再インデックス不要**。
  各戦略を `WriteAction` (`Add` / `Delete`) 列へ展開し、`write` (第4章) が Lens 経由で適用する
  (`rank` カラム含む)。タグの付け替え (`ttfm replace`, 第5章) も `tag` / 条件付き `untag` の合成なので、
  個々のコマンドが上記に帰着する。最後に `OneView::recreate` で整合する。

## 7. 値セマンティクス

- **name**: 単一値。書き込みは既存 name の置換となる (1アイテム1名前)。
- **一般ユーザー定義タグ**: `Append` (重複可)。
  値を変えたい場合は第5章の `ttfm replace` で付け替える。
- **TagFunction が提供するタグ**: デフォルトで編集不可。宣言した `EditStrategy` に従う。
- **将来拡張**: ユーザー定義型を Unique にしたい場合、Type アイテムに `unique:true` のような
  メタタグを付与することで重複不可 (置換またはエラー) にできるようにする (M4 では未実装)。

## 8. キャプチャとバックリファレンス (per-item テンプレート)

SearchQuery 側のパターンでキャプチャした部分文字列を、EditQuery 側で参照し、
**アイテムごとに動的な値**を構築する機能。一括リネーム等で強力に機能する。

### 8.1 構文
- **キャプチャ**: 未クォートの Glob `*` / `?` / `[]` を**暗黙の位置キャプチャ**として扱う。
  捕捉されるのは **Glob メタ文字が実際にマッチした部分文字列のみ** (ラベル全体ではない)。
  例: `project:tt*` が `ttfm` にマッチした場合、`*` が捕まえるのは `fm` (接頭辞 `tt` は含まない)。
- **参照**: EditQuery 側で `{1}` / `{2}` … と記述する
  (`\1` 形式はシェルでの扱いが煩雑なため波括弧を採用)。
- **番号付け**: SearchQuery 全体を左から走査した通し番号とする。
- **クォート時は無効**: SearchQuery と同様、クォート内では Glob が無効化され完全一致となるため
  キャプチャされない。リテラルな `{1}` を書きたい場合もクォートする (`"{1}"`)。
- **束縛されない参照は空文字**: 参照 `{n}` がそのアイテムで束縛されない場合
  (キャプチャ総数を超える `{3}`、OR の非マッチ枝にある glob、差集合の除外側にある glob 等) は
  **既定で空文字に展開**する (per-item で実行中断しない)。`ttfm.toml` の設定で「束縛なしをエラーにする」
  厳格モードへ切り替え可能としてもよい。

### 8.2 実行モデル
- キャプチャの展開は `search_and_apply_captures` が担う (§4)。各アイテムのタグ値を SearchQuery の
  glob パターンに当て、`{n}` を具体値で置換した EditQuery を per-item で返す。`modify` は
  `{n}` を含まない具体的な EditQuery のみ受け取り、キャプチャを知らない。
- **1アイテムが同 type の該当ラベルを複数持つ場合** (SearchQuery の glob が item ごとに複数マッチ): マッチ
  ごとにテンプレートを展開し**複数の `(Item, EditQuery)` ペアを生成**する。展開先 strategy で扱いが分かれる:
  - `Append`: 異なる label は複数行として共存 (完全一致は §4.1 の冪等性で吸収)。
  - `Replace`: 同一 item・同一 type に複数の `Replace(tag)` が積まれ、`confirm` が1候補に解決する。
- 静的な多重リテラル (`name:a name:b`) は曖昧なので **`edit` 入口で事前エラー**とする。

### 8.3 例
- `ttfm tag filename:*_draft.txt filename:{1}.txt` (各ファイルの `_draft` を剥がしてリネーム)
- `ttfm tag filename:proj_* project:{1}` (ファイル名の一部を project タグとして付与)

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
