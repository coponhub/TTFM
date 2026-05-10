wit_bindgen::generate!({
    path: "../../wit/plugin.wit",
    world: "plugin",
});

struct SamplePlugin;

impl exports::ttfm::plugin::core::Guest for SamplePlugin {
    fn get_info() -> exports::ttfm::plugin::core::PluginInfo {
        exports::ttfm::plugin::core::PluginInfo {
            name: "sample".to_string(),
            version: "0.1.0".to_string(),
            kind: exports::ttfm::plugin::core::PluginKind::IndexingFunction,
            value_type: exports::ttfm::plugin::core::ValueType::Text,
        }
    }
}

impl exports::ttfm::plugin::indexing_function::Guest for SamplePlugin {
    fn tag_file(_path: String) -> Vec<exports::ttfm::plugin::indexing_function::TagValue> {
        vec![exports::ttfm::plugin::indexing_function::TagValue::Text("text/plain".to_string())]
    }
}

export!(SamplePlugin);
