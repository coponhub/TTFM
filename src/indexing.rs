use crate::taggers::TagValue;

#[derive(Debug, PartialEq)]
pub struct EntityRow {
    pub id: i64,
    pub size: i64,
    pub mtime: i64,
}

#[derive(Debug, PartialEq)]
pub struct LocationRow {
    pub entity_id: i64,
    pub path: String,
    pub filename: String,
    pub parentdir: String,
}

#[derive(Debug, PartialEq)]
pub struct TagRow {
    pub entity_id: i64,
    pub tag_type: String,
    pub tag_value: String,
}

/// カラム名と値のペアから、3テーブル用の行データを生成する
pub fn convert_to_rows(
    entity_id: i64,
    data: &[(String, TagValue)],
) -> (EntityRow, LocationRow, Vec<TagRow>) {
    let mut size = 0;
    let mut mtime = 0;
    let mut path = String::new();
    let mut filename = String::new();
    let mut parentdir = String::new();
    let mut tags = Vec::new();

    for (col_name, value) in data {
        match col_name.as_str() {
            // Entities
            "size_bytes" => {
                if let TagValue::BigInt(v) = value {
                    size = *v;
                }
            }
            "modified_ts" => {
                if let TagValue::BigInt(v) = value {
                    mtime = *v;
                }
            }
            
            // Locations
            "path" => {
                if let TagValue::Text(v) = value {
                    path = v.clone();
                }
            }
            "filename" => {
                if let TagValue::Text(v) = value {
                    filename = v.clone();
                }
            }
            "parent_dir" => { // Note: Tagger returns "parent_dir", DB column is "parentdir"
                if let TagValue::Text(v) = value {
                    parentdir = v.clone();
                }
            }

            // Tags (Others)
            _ => {
                // 表示用のカラムは除外
                if col_name != "size_str" && col_name != "modified_str" {
                    let val_str = match value {
                        TagValue::Text(s) => s.clone(),
                        TagValue::BigInt(i) => i.to_string(),
                        TagValue::Boolean(b) => b.to_string(),
                        TagValue::Null => String::new(),
                        _ => String::new(),
                    };
                    
                    // 値が存在する場合のみタグとして登録
                    if !val_str.is_empty() {
                         tags.push(TagRow {
                            entity_id,
                            tag_type: col_name.clone(),
                            tag_value: val_str,
                        });
                    }
                }
            }
        }
    }

    (
        EntityRow { id: entity_id, size, mtime },
        LocationRow { entity_id, path, filename, parentdir },
        tags
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_convert_to_rows() {
        let entity_id = 100;
        let data = vec![
            ("path".to_string(), TagValue::Text("/home/user/doc.txt".to_string())),
            ("filename".to_string(), TagValue::Text("doc.txt".to_string())),
            ("parent_dir".to_string(), TagValue::Text("/home/user".to_string())),
            ("size_bytes".to_string(), TagValue::BigInt(1024)),
            ("modified_ts".to_string(), TagValue::BigInt(123456789)),
            ("extension".to_string(), TagValue::Text("txt".to_string())),
            ("kind".to_string(), TagValue::Text("File".to_string())),
            ("size_str".to_string(), TagValue::Text("1KB".to_string())), // Should be ignored
        ];

        let (entity, location, tags) = convert_to_rows(entity_id, &data);

        assert_eq!(entity, EntityRow {
            id: 100,
            size: 1024,
            mtime: 123456789
        });

        assert_eq!(location, LocationRow {
            entity_id: 100,
            path: "/home/user/doc.txt".to_string(),
            filename: "doc.txt".to_string(),
            parentdir: "/home/user".to_string(),
        });

        // Tags should check contents, order might vary if hashmap but vec preserves order
        assert_eq!(tags.len(), 2);
        assert!(tags.contains(&TagRow {
            entity_id: 100,
            tag_type: "extension".to_string(),
            tag_value: "txt".to_string()
        }));
        assert!(tags.contains(&TagRow {
            entity_id: 100,
            tag_type: "kind".to_string(),
            tag_value: "File".to_string()
        }));
    }
}
