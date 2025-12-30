/// ファイルパスからMIMEタイプを判定する内部ロジック
pub fn detect_mime(path: &str) -> String {
    use std::fs::File;
    use std::io::Read;

    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return "empty".to_string(),
    };

    let mut buffer = [0; 128];
    let n = match file.read(&mut buffer) {
        Ok(n) => n,
        Err(_) => return "empty".to_string(),
    };

    if n == 0 {
        return "application/x-empty".to_string();
    }

    if let Some(kind) = infer::get(&buffer[..n]) {
        kind.mime_type().to_string()
    } else {
        if std::str::from_utf8(&buffer[..n]).is_ok() {
            "text/plain".to_string()
        } else {
            "application/octet-stream".to_string()
        }
    }
}

// --- Unit Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_detect_mime_text() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "Hello, this is a plain text file.").unwrap();
        
        let mime = detect_mime(file.path().to_str().unwrap());
        assert_eq!(mime, "text/plain");
    }

    #[test]
    fn test_detect_mime_png() {
        let mut file = NamedTempFile::new().unwrap();
        // PNG magic number
        file.write_all(b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR").unwrap();
        
        let mime = detect_mime(file.path().to_str().unwrap());
        assert_eq!(mime, "image/png");
    }

    #[test]
    fn test_detect_mime_empty() {
        let file = NamedTempFile::new().unwrap();
        let mime = detect_mime(file.path().to_str().unwrap());
        assert_eq!(mime, "application/x-empty");
    }

    #[test]
    fn test_detect_mime_nonexistent() {
        let mime = detect_mime("this_file_does_not_exist.txt");
        assert_eq!(mime, "empty");
    }
}
