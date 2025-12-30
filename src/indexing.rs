use crate::taggers::{TagValue, ColumnDef, TargetTable};

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

/// カラム定義と値のペアから、3テーブル用の行データを生成する
pub fn convert_to_rows(
    entity_id: i64,
    data: &[(ColumnDef, TagValue)],
) -> (EntityRow, LocationRow, Vec<TagRow>) {
    let mut size = 0;
    let mut mtime = 0;
    let mut path = String::new();
    let mut filename = String::new();
    let mut parentdir = String::new();
    let mut tags = Vec::new();

    for (col_def, value) in data {
        match col_def.target_table {
            TargetTable::Entities => {
                match col_def.name.as_str() {
                    "size_bytes" => if let TagValue::BigInt(v) = value { size = *v; },
                    "modified_ts" => if let TagValue::BigInt(v) = value { mtime = *v; },
                    _ => {}
                }
            },
            TargetTable::Locations => {
                match col_def.name.as_str() {
                    "path" => if let TagValue::Text(v) = value { path = v.clone(); },
                    "filename" => if let TagValue::Text(v) = value { filename = v.clone(); },
                    "parentdir" => if let TagValue::Text(v) = value { parentdir = v.clone(); },
                    _ => {}
                }
            },
            TargetTable::Tags => {
                let val_str = match value {
                    TagValue::Text(s) => s.clone(),
                    TagValue::BigInt(i) => i.to_string(),
                    TagValue::Boolean(b) => b.to_string(),
                    TagValue::Null => String::new(),
                    _ => String::new(),
                };
                
                if !val_str.is_empty() {
                     tags.push(TagRow {
                        entity_id,
                        tag_type: col_def.name.clone(),
                        tag_value: val_str,
                    });
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

    fn col(name: &str, table: TargetTable) -> ColumnDef {
        ColumnDef { name: name.to_string(), sql_type: "TEXT", target_table: table }
    }

    #[test]
    fn test_convert_to_rows() {
        let entity_id = 100;
        let data = vec![
            (col("path", TargetTable::Locations), TagValue::Text("/home/user/doc.txt".to_string())),
            (col("filename", TargetTable::Locations), TagValue::Text("doc.txt".to_string())),
            (col("parentdir", TargetTable::Locations), TagValue::Text("/home/user".to_string())),
            (col("size_bytes", TargetTable::Entities), TagValue::BigInt(1024)),
            (col("modified_ts", TargetTable::Entities), TagValue::BigInt(123456789)),
            (col("extension", TargetTable::Tags), TagValue::Text("txt".to_string())),
            (col("kind", TargetTable::Tags), TagValue::Text("File".to_string())),
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

        assert_eq!(tags.len(), 2);
    }
}