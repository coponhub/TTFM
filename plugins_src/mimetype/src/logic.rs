use anyhow::Result;
use infer;
use std::fs::File;
use std::io::Read;

pub fn get_mimetype(path: &str) -> Result<String> {
    let mut file = File::open(path)?;
    let mut buffer = [0; 16];
    file.read_exact(&mut buffer).ok();

    let kind = infer::get(&buffer);
    let mime = kind.map(|k| k.mime_type()).unwrap_or("application/octet-stream");
    
    Ok(mime.to_string())
}
