use crate::logic;

wit_bindgen::generate!({
    path: "../../wit/plugin.wit",
    world: "plugin",
});

struct MimetypePlugin;

impl exports::ttfm::plugin::core::Guest for MimetypePlugin {
    fn get_info() -> exports::ttfm::plugin::core::PluginInfo {
        exports::ttfm::plugin::core::PluginInfo {
            name: "mimetype".to_string(),
            version: "0.2.3".to_string(),
            kind: exports::ttfm::plugin::core::PluginKind::TagFunction,
        }
    }
}

impl exports::ttfm::plugin::tag_function::Guest for MimetypePlugin {
    fn get_columns() -> Vec<exports::ttfm::plugin::tag_function::ColumnDef> {
        vec![exports::ttfm::plugin::tag_function::ColumnDef {
            name: "mimetype".to_string(),
            sql_type: "TEXT".to_string(),
        }]
    }

    fn tag_file(path: String) -> Vec<exports::ttfm::plugin::tag_function::TagValue> {
        let mime = logic::detect_mime(&path);
        if mime == "empty" {
            vec![exports::ttfm::plugin::tag_function::TagValue::Empty]
        } else {
            vec![exports::ttfm::plugin::tag_function::TagValue::Text(mime)]
        }
    }
}

export!(MimetypePlugin);
