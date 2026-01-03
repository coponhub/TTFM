# コーディングガイドライン (TTFM)

## 1. SQLクエリの構築方針

### Sea-query の優先利用
プロジェクト内、特に `TagFunction` の `to_sql` メソッドなどにおいてSQLクエリを構築する際は、**可能な限り素のSQL文字列（`format!` 等）を直接使用せず、Sea-query クレートを使用してください。**

これにより、以下のメリットを享受します：
- **SQLインジェクションの防止**: パラメータの適切なエスケープが自動的に行われます。
- **可読性の向上**: Rustのメソッドチェーンによる構造的なクエリ構築が可能です。
- **保守性の向上**: テーブル名やカラム名の変更に強くなります。

### DuckDB 固有の記法への対応
DuckDB独自の構文や、Sea-queryの標準メソッドでは表現が難しい複雑な式が必要な場合は、以下の構成で対応してください。

1.  **Iden トレイトの実装**: テーブル名やカラム名だけでなく、**独自の関数名**も列挙型などで定義し、`Iden` トレイトを実装（または `#[derive(Iden)]`）します。
2.  **Func 構造体の利用**: `sea_query::Func` を使用して、定義した `Iden` を関数として呼び出します。これにより、文字列リテラルを減らし、型安全性を高めることができます。

#### 実装例：関数の宣言（ラップパターン）と呼び出し
単に `Iden` を定義するだけでなく、以下のように専用の構造体や関数として宣言しておくことで、呼び出し側で `Func::cust` を意識せずに再利用できるようになります。

```rust
use sea_query::{Iden, Expr, Func, SimpleExpr};

// 1. SQL上の名前（識別子）を宣言
#[derive(Iden)]
enum DuckDbFunc {
    #[iden = "regexp_matches"]
    RegexpMatches,
}

// 2. Rust の関数として「宣言」し、内部で Iden と Func を組み合わせて構築
pub struct CustomFunc;

impl CustomFunc {
    /// DuckDB の regexp_matches(string, pattern) を呼び出すための宣言
    pub fn regexp_matches<E, P>(expr: E, pattern: P) -> SimpleExpr
    where
        E: Into<SimpleExpr>,
        P: Into<SimpleExpr>,
    {
        Func::cust(DuckDbFunc::RegexpMatches)
            .args(vec![expr.into(), pattern.into()])
    }
}

// 3. 呼び出し側のコード
// 生成されるSQLイメージ: regexp_matches("path", '.*\.rs$')
let condition = CustomFunc::regexp_matches(
    Expr::col(Locations::Path),
    ".*\\.rs$"
);
```

#### 実装例：Cast を伴う関数の宣言
引数に対して特定の型へのキャストを強制するような関数の宣言例です。

```rust
use sea_query::{Iden, Expr, Func, SimpleExpr, Alias};

#[derive(Iden)]
enum MyFuncIden {
    #[iden = "custom_size_logic"]
    CustomSizeLogic,
}

impl CustomFunc {
    /// 引数を BIGINT にキャストしてから関数に渡す宣言
    pub fn size_logic<E>(expr: E) -> SimpleExpr
    where
        E: Into<SimpleExpr>,
    {
        // 引数に対して .cast_as() を適用
        let casted_expr = expr.into().cast_as(Alias::new("BIGINT"));

        Func::cust(MyFuncIden::CustomSizeLogic)
            .arg(casted_expr)
    }
}
```

#### 実装例：明示的なキャスト (Cast)
DuckDB は型に厳格な場合があるため、必要に応じて `Expr` の `cast_as` メソッドを使用します。

```rust
use sea_query::{Expr, Alias};

// 生成されるSQLイメージ: CAST("size" AS BIGINT)
let expr = Expr::col(Alias::new("size")).cast_as(Alias::new("BIGINT"));
```

## 2. 実装上の注意点
- 自前での文字列エスケープ（`replace("'", "''")` 等）は極力避け、Sea-queryの提供する `Expr::val` 等のパラメータバインド機能を利用してください。
- DuckDBはPostgreSQLに近い方言を持つため、クエリの生成には基本的に `PostgresQueryBuilder` を使用しますが、差異がある場合は必要に応じて調整してください。

## 3. コードのフォーマットとスタイル

### 1行の最大文字数
- **1行の長さは最大80文字**としてください。
- これにより、複数のファイルを並べて表示した際の可読性や、ターミナル上での閲覧性を確保します。
- Rust標準の `rustfmt` を使用する場合も、可能であればこの設定を尊重するように構成してください。
