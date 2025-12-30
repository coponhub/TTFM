use infer;
use std::fs::{self, File};
use std::io::Read;

pub fn detect_mime(path: &str) -> String {
    // ディレクトリ判定
    if let Ok(metadata) = fs::metadata(path) {
        if metadata.is_dir() {
            return "inode/directory".to_string();
        }
    }

    // ファイルの中身を読み込んで判定
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return "empty".to_string(),
    };

    let mut buffer = [0; 16];
    let res = file.read_exact(&mut buffer);

    if res.is_err() {
        // 先頭16バイト読めないような小さいファイルなどの場合
        // 拡張子などでの判定にフォールバックせず、ここでは application/octet-stream 等にする
        return "application/octet-stream".to_string();
    }

    let kind = infer::get(&buffer);
    kind.map(|k| k.mime_type().to_string())
        .unwrap_or_else(|| "application/octet-stream".to_string())
}