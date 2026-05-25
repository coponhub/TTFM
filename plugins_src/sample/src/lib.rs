ttfm_plugin::target!(indexing, query, display);

struct SamplePlugin;

impl exports::ttfm::plugin::core::Guest for SamplePlugin {
    fn name() -> String {
        "sample".to_string()
    }
    fn version() -> String {
        "0.1.0".to_string()
    }
}

impl exports::ttfm::plugin::indexing::Guest for SamplePlugin {
    fn get_value_type() -> exports::ttfm::plugin::indexing::ValueType {
        exports::ttfm::plugin::indexing::ValueType::Text
    }
    fn tag_file(_path: String) -> Vec<exports::ttfm::plugin::indexing::TagValue> {
        vec![exports::ttfm::plugin::indexing::TagValue::Text("text/plain".to_string())]
    }
}

impl exports::ttfm::plugin::query::Guest for SamplePlugin {
    fn normalize_label(_label: String) -> Option<String> {
        None
    }
    fn expand(_tag_type: String, _label: String) -> Option<String> {
        None
    }
    fn expand_projection(_tag_type: String) -> Option<String> {
        None
    }
}

impl exports::ttfm::plugin::display::Guest for SamplePlugin {
    fn default_format() -> Option<exports::ttfm::plugin::display::DisplayFormat> {
        None
    }
    fn formats() -> Vec<exports::ttfm::plugin::display::DisplayFormat> {
        vec![]
    }
    fn show(value: String, _format_id: String) -> String {
        value
    }
}

export!(SamplePlugin);
