wit_bindgen::generate!({
    path: "../../wit/plugin.wit",
    world: "plugin",
});

struct MimetypeOverridePlugin;

impl exports::ttfm::plugin::core::Guest for MimetypeOverridePlugin {
    fn get_info() -> exports::ttfm::plugin::core::PluginInfo {
        exports::ttfm::plugin::core::PluginInfo {
            name: "mimetype".to_string(),
            version: "0.1.0".to_string(),
            kind: exports::ttfm::plugin::core::PluginKind::IndexingFunction,
            value_type: exports::ttfm::plugin::core::ValueType::Text,
        }
    }
}

impl exports::ttfm::plugin::indexing_function::Guest for MimetypeOverridePlugin {
    fn tag_file(_path: String) -> Vec<exports::ttfm::plugin::indexing_function::TagValue> {
        vec![exports::ttfm::plugin::indexing_function::TagValue::Text(
            "application/x-test-override".to_string(),
        )]
    }
}

export!(MimetypeOverridePlugin);
