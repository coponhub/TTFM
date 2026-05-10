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
            kind: exports::ttfm::plugin::core::PluginKind::IndexingFunction,
            value_type: exports::ttfm::plugin::core::ValueType::Text,
        }
    }
}

impl exports::ttfm::plugin::indexing_function::Guest for MimetypePlugin {
    fn tag_file(path: String) -> Vec<exports::ttfm::plugin::indexing_function::TagValue> {
        let mime = logic::detect_mime(&path);
        if mime == "empty" {
            vec![exports::ttfm::plugin::indexing_function::TagValue::Empty]
        } else {
            vec![exports::ttfm::plugin::indexing_function::TagValue::Text(mime)]
        }
    }
}

export!(MimetypePlugin);
