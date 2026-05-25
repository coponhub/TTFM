use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, punctuated::Punctuated, Ident, Token};

// --- WIT interface definitions ---
// (これらの定数がインターフェース仕様の正典。wit/plugin.wit は不要。)

// interface core {
//     name: func() -> string;
//     version: func() -> string;
// }
const CORE_WIT: &str = "interface core {
    name: func() -> string;
    version: func() -> string;
}";

// interface indexing {
//     enum value-type { text, big-int, boolean, double }
//     variant tag-value {
//         text(string),
//         big-int(s64),
//         boolean(bool),
//         double(f64),
//         empty,
//     }
//     get-value-type: func() -> value-type;
//     tag-file: func(path: string) -> list<tag-value>;
// }
const INDEXING_WIT: &str = "interface indexing {
    enum value-type { text, big-int, boolean, double }
    variant tag-value {
        text(string),
        big-int(s64),
        boolean(bool),
        double(f64),
        empty,
    }
    get-value-type: func() -> value-type;
    tag-file: func(path: string) -> list<tag-value>;
}";

// interface query {
//     normalize-label: func(label: string) -> option<string>;
//     expand: func(tag-type: string, label: string) -> option<string>;
//     expand-projection: func(tag-type: string) -> option<string>;
// }
const QUERY_WIT: &str = "interface query {
    normalize-label: func(label: string) -> option<string>;
    expand: func(tag-type: string, label: string) -> option<string>;
    expand-projection: func(tag-type: string) -> option<string>;
}";

// interface display {
//     record display-format {
//         id: string,
//         label: string,
//     }
//     default-format: func() -> option<display-format>;
//     formats: func() -> list<display-format>;
//     show: func(value: string, format-id: string) -> string;
// }
const DISPLAY_WIT: &str = "interface display {
    record display-format {
        id: string,
        label: string,
    }
    default-format: func() -> option<display-format>;
    formats: func() -> list<display-format>;
    show: func(value: string, format-id: string) -> string;
}";

/// 実装するインターフェースを宣言するマクロ。
///
/// `core` は常に必須のため引数不要。
/// 引数には実装するオプショナルインターフェースをカンマ区切りで指定する。
///
/// # 例
/// ```ignore
/// ttfm_plugin::target!(indexing);
/// ttfm_plugin::target!(indexing, query);
/// ttfm_plugin::target!(indexing, query, display);
/// ```
#[proc_macro]
pub fn target(input: TokenStream) -> TokenStream {
    let interfaces = parse_macro_input!(
        input with Punctuated::<Ident, Token![,]>::parse_terminated
    );

    let mut wit_parts: Vec<&str> = vec!["package ttfm:plugin;", CORE_WIT];
    let mut exports: Vec<&str> = vec!["export core;"];

    for iface in &interfaces {
        match iface.to_string().as_str() {
            "indexing" => {
                wit_parts.push(INDEXING_WIT);
                exports.push("export indexing;");
            }
            "query" => {
                wit_parts.push(QUERY_WIT);
                exports.push("export query;");
            }
            "display" => {
                wit_parts.push(DISPLAY_WIT);
                exports.push("export display;");
            }
            other => {
                return syn::Error::new(
                    iface.span(),
                    format!(
                        "unknown interface: `{other}`. expected one of: indexing, query, display"
                    ),
                )
                .to_compile_error()
                .into();
            }
        }
    }

    let exports_str = exports.join("\n    ");
    let world = format!("world plugin {{\n    {exports_str}\n}}");
    wit_parts.push(Box::leak(world.into_boxed_str()));
    let wit = wit_parts.join("\n\n");

    quote! {
        ::wit_bindgen::generate!({
            inline: #wit,
        });
    }
    .into()
}
