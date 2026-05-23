use crate::logic;

wit_bindgen::generate!({
    path: "../../wit/plugin.wit",
    inline: "
        package mimetype:plugin;

        world plugin {
            export ttfm:plugin/core;
            export ttfm:plugin/indexing;
        }
    ",
    generate_all,
});

struct MimetypePlugin;

impl exports::ttfm::plugin::core::Guest for MimetypePlugin {
    fn name() -> String {
        "mimetype".to_string()
    }
    fn version() -> String {
        "0.2.3".to_string()
    }
}

impl exports::ttfm::plugin::indexing::Guest for MimetypePlugin {
    fn get_value_type() -> exports::ttfm::plugin::indexing::ValueType {
        exports::ttfm::plugin::indexing::ValueType::Text
    }
    fn tag_file(path: String) -> Vec<exports::ttfm::plugin::indexing::TagValue> {
        let mime = logic::detect_mime(&path);
        if mime == "empty" {
            vec![exports::ttfm::plugin::indexing::TagValue::Empty]
        } else {
            vec![exports::ttfm::plugin::indexing::TagValue::Text(mime)]
        }
    }
}

export!(MimetypePlugin);
