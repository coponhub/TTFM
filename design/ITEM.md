# TTFM Item Design Specification

TTFMにおけるItemの分類や性質は以下の通り

## 1. 由来(Origin)
**Origin**には、大分類として、**User** と **System** があるが、
**System**はさらに、以下の通り細かく分類される
- **Builtin**: TTFMが最初から所持している
- **Plugin**: プラグインによって作成・付与された
- **File**: TTFMがディスクをスキャンして得たファイルの情報を加工して登録した

## 2. 参照
TTFMではItemの参照を保持する。
アイテムは以下の2つのどちらかの参照を持つ。

- **File Reference**: ファイルシステム上の実ファイルを示す参照。InodeおよびDevice IDによって同一性が追跡される。
- **Item Reference**: ファイル以外の対象を示す参照。
    - **ItemKinds** (登録可能な種): `tag` / `type` / `note`
        `tag`: `Type:Label` 形式のTypedTagそのもの
        `type` : TypedTagのType
        `note`: (noteはユーザーがDBに格納可能なメモ)
        - なお `label`（型を伴わない裸のラベル値）は独自 identity を持たないため**登録対象としない**（常に tag 行から導出される Volatile）。

## 3. 永続化状態 (Stored / Volatile)
Item は永続化の有無によって区別される。

- **Stored Item**: DB に永続化済みの Item。`item_references`（File Reference 経由なら `file_references`）に行を持ち、**正式な item_id** が採番されている。index 済みファイルや、明示的に登録された type/tag/note 定義がこれにあたる。
- **Volatile Item**: まだ DB に永続化されていない Item。正式な item_id を持たない。検索結果には現れるが永続化されていないもの

TTFM は Itemが Volatile かどうかを、検索結果から判定せず、登録されたItemは結果的に Stored Itemになる。

## 4. ItemのID (識別子)
Itemはその由来と永続化状態によって区別され、Item毎のIDを付与される。IDは、以下の通り分類される。

- **Stored ID**: ItemがDBに保存される際に付与される永続的なID。**SystemTag**はTTFM内で最初からIDが定義されている。
- **Volatile ID**: **Volatile Item**のID。一時的なもの。
- **Settling ID**: **Volatile Item**のIDのうち、**Origin**が明確なもの。

## 5. ID空間 (ID Space)
**Stored ID**は単一の識別空間(Space)を持つ。ただし、Itemのカテゴリによって全ID空間のうちのどの区画(block)を使うかを分けている。

- 現在、**Stored ID**の区画は**Origin**によって分割されており、区画の種類はTTFMが識別する**Origin**の種類と一致している。
  - 各IDは区画のもつ最も低い値から順に割り振られる。
  - ID空間はInt64を使用しており、64分割している。
  - 区画には-32から+31までの番号が振られている。
  - 区画は、以下の通り割り当てられている。
    - **Plugin区画**: 16
    - **File区画**: 8
    - **User区画**: 0
    - **Builtin区画**: -1

- **Volatile ID**と**Settling ID**も特定のスコープで有効なアイテム毎の識別子を持つが、Stored IDとは異なる空間となっており、区画は定義されていない

## 6. Item Name Abstraction (アイテム名の抽象化)
「アイテムの名前」(name)を、ファイルシステム上の「ファイル名」(filename)から分離し、ユーザーが認識・操作する対象をnameとする
- **name**: GUIやCLIでユーザーに提示されるアイテムの名称。
- **filename**: ファイルシステム上の物理的な識別子。

デフォルトでは `name` は `filename` と同一だが、ユーザーは任意の `name` をタグとして付与できる。
これにより、ファイルシステム上のファイル名を変更することなく、コンテキストに応じたわかりやすい名前で管理可能となる。

なお、タグの種類としての `type:name` は `origin:system` であるが、ユーザーが付与した個別のタグ（例: `name:foo`）は `origin:user` として扱われる。一方で、ファイル名から自動解決された `name` は `origin:file` となる。

## 7. 優先度システム (RANK)
全てのアイテムは `rank` と呼ばれる整数値の優先度を保持する。
- **アイテムのソート**: 検索結果は、特に指定が無ければ `rank` の降順で表示される。
- **列の表示順序**: タグの型（type）自体が持つ `rank` に基づき、値が大きい
  タグほど CLI の表示において左側の列に配置される。

## 8. 定義アイテム (Definition)

TTFMでは、TypedTagの`Type`や`TypedTag`自体もItemとして扱う事が出来る。

Itemとしての`Type`や`TypedTag`を**定義アイテム**と呼ぶ。

定義アイテムは、以下の性質を持つ

- 検索によって抽出可能
- **初期化やindexingではDBに登録されない**
- 定義されていないものは、ユーザーが作成出来る。
- DBに登録されていない定義アイテムは、ユーザーがDBに登録できる。
- 定義アイテムにタグを付与する際、DBに登録される

### 8.1 定義アイテムの生成と取得
定義アイテムは、検索時に以下の方法で取得される

- DBから取得する
- TTFMの内的な定義(コードに記載された情報)から所得する
- Itemに付与されたTypedTagから抽出する

### 8.2 型定義設定 (TypeConfig) と構造保護
型定義アイテム（`ItemKind::Type`）には、その型に属するタグの振る舞いをカスタマイズするプロパティを付与できる。

- **設定可能なプロパティ**:
  - `display_unit`: 表示単位・フォーマット（`binary`, `si`, またはプラグイン/組み込みの `DisplayFormats` オプション）。
  - `strategy`: 編集戦略（`append` 重複追記 / `replace` 単一値置換）。
  - `bitical_type`: 物理格納型（`string`, `integer`, `double`, `boolean`, `uuid`）。
- **組み込み型の構造保護**:
  - `origin:builtin` / `origin:system` の組み込み型に対して、構造的な `strategy` や `bitical_type` を付与・変更することは禁止される（`display_unit` などの表示設定のみ変更可能）。
- **型の変更 (Tag Cast)**:
  - `bitical_type` を変更する操作（tag cast）は単一のコマンドにつき1つの型のみ許可される。
  - 変更前に既存のタグ値がすべて新しい型にパース可能であるか事前検証され、不正な値が含まれる場合はアトミックに拒否される。
  - 型定義から `bitical_type` を `untag` することは禁止され、型変更には明示的な cast を使用する。

## 9. 削除
Itemの削除については、**Origin**によって以下の通り分類される

- **User** ユーザーが付与したタグ・作成したアイテムはユーザーによって明示的に削除できる。
- **System** システムが付与したタグ・作成したアイテムはユーザーによって明示的に削除できないか、削除してもシステム都合によって再作成される

`ttfm clear` コマンド（CLI.md §3）で、DBごとの削除が可能。
