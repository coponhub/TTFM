use super::{default_scope, get_nvalue_f64};

define_cases! {
    discrepancy_count_projection_distinct: {
        setup: |dir| {
            // dirA: 10ファイル、拡張子は .txt のみ (1種類)
            let dira = dir.join("dirA");
            std::fs::create_dir(&dira)?;
            for i in 0..10 {
                std::fs::write(dira.join(format!("file{}.txt", i)), "content")?;
            }
            // dirB: 3ファイル、拡張子は3種類 (.txt, .html, .rs)
            let dirb = dir.join("dirB");
            std::fs::create_dir(&dirb)?;
            std::fs::write(dirb.join("a.txt"), "t")?;
            std::fs::write(dirb.join("b.html"), "h")?;
            std::fs::write(dirb.join("c.rs"), "r")?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "parentdir: &: count(extension:)",
        assert: |res, _dir| {
            let dira = res.results.iter().find(|r| r.raw_repr().contains("dirA")).expect("dirA");
            let dirb = res.results.iter().find(|r| r.raw_repr().contains("dirB")).expect("dirB");
            assert_eq!(get_nvalue_f64(dira), Some(1.0), "dirA: 1 extension type (txt only)");
            assert_eq!(get_nvalue_f64(dirb), Some(3.0), "dirB: 3 extension types (txt, html, rs)");
            Ok(())
        },
    },
    discrepancy_count_typedtag_items: {
        setup: |dir| {
            // dirA: txt が 5 個
            let dira = dir.join("dirA");
            std::fs::create_dir(&dira)?;
            for i in 0..5 {
                std::fs::write(dira.join(format!("file{}.txt", i)), "content")?;
            }
            // dirB: txt が 2 個、html が 1 個
            let dirb = dir.join("dirB");
            std::fs::create_dir(&dirb)?;
            std::fs::write(dirb.join("a.txt"), "t")?;
            std::fs::write(dirb.join("b.txt"), "t")?;
            std::fs::write(dirb.join("c.html"), "h")?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "parentdir: &: count(extension:txt)",
        assert: |res, _dir| {
            let dira = res.results.iter().find(|r| r.raw_repr().contains("dirA")).expect("dirA");
            let dirb = res.results.iter().find(|r| r.raw_repr().contains("dirB")).expect("dirB");
            assert_eq!(get_nvalue_f64(dira), Some(5.0), "dirA: 5 txt files");
            assert_eq!(get_nvalue_f64(dirb), Some(2.0), "dirB: 2 txt files");
            Ok(())
        },
    },
}
