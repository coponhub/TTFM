ttfm_plugin::target!(indexing);

struct MimetypeOverridePlugin;

impl exports::ttfm::plugin::core::Guest for MimetypeOverridePlugin {
    fn name() -> String {
        "mimetype".to_string()
    }
    fn version() -> String {
        "0.1.0".to_string()
    }
}

impl exports::ttfm::plugin::indexing::Guest for MimetypeOverridePlugin {
    fn get_value_type() -> exports::ttfm::plugin::indexing::ValueType {
        exports::ttfm::plugin::indexing::ValueType::Text
    }
    fn tag_file(_path: String) -> Vec<exports::ttfm::plugin::indexing::TagValue> {
        vec![exports::ttfm::plugin::indexing::TagValue::Text(
            "application/x-test-override".to_string(),
        )]
    }
}

export!(MimetypeOverridePlugin);
