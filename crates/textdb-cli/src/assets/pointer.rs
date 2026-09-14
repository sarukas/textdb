//! Asset pointers: the small text document (`NAME.tdbasset`) that stands next to where an asset
//! belongs and says what it is — an id, the hash and size of its bytes, and where they are kept.
//! Versioned in textdb and git like any document; the bytes live in an asset store.

use std::io::Read;
use std::path::Path;

use serde::Serialize;
use sha2::{Digest, Sha256};

pub use textdb_md::resolve::{asset_path, is_asset_pointer, ASSET_POINTER_SUFFIX as SUFFIX};

/// The pointer format this build writes and reads.
pub const FORMAT: &str = "1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Pointer {
    /// UUIDv7, assigned when the asset is first pushed; kept through renames, moves and edits.
    pub id: String,
    /// Of the raw bytes, lower-case hex.
    pub sha256: String,
    pub size: u64,
    /// Media type, from the file's extension.
    #[serde(rename = "type")]
    pub media_type: String,
    /// The asset store holding the bytes.
    pub store: String,
    /// The provider's id of the stored file; absent where files are addressed by path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    /// Keys this build does not know, kept in order.
    #[serde(skip)]
    pub extra: Vec<(String, String)>,
}

fn split(line: &str) -> Option<(&str, &str)> {
    let (k, v) = line.split_once(':')?;
    Some((k.trim(), v.trim()))
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `8-4-4-4-12` hex digits.
fn is_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5 && parts.iter().zip([8, 4, 4, 4, 12]).all(|(p, n)| p.len() == n && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// An asset store name: letters, digits, `_`, `-` and `.`, starting with a letter or digit.
pub fn valid_store_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

impl Pointer {
    pub fn parse(bytes: &[u8]) -> Result<Pointer, String> {
        let text = std::str::from_utf8(bytes).map_err(|_| "a pointer is text, and this is not UTF-8".to_string())?;
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut lines = text.lines().filter(|l| !l.trim().is_empty());
        match lines.next().and_then(split) {
            Some(("textdb-asset", FORMAT)) => {}
            Some(("textdb-asset", v)) => return Err(format!("pointer format {v}: this build reads format {FORMAT}")),
            _ => return Err("not an asset pointer: the first line is not `textdb-asset: 1`".into()),
        }
        let mut known: [Option<&str>; 6] = [None; 6];
        let mut extra = Vec::new();
        for line in lines {
            let (k, v) = split(line).ok_or_else(|| format!("not a `key: value` line: {line}"))?;
            let slot = ["id", "sha256", "size", "type", "store", "item"].iter().position(|n| *n == k);
            match slot {
                Some(i) if known[i].is_some() => return Err(format!("`{k}` is given twice")),
                Some(i) => known[i] = Some(v),
                None => extra.push((k.to_string(), v.to_string())),
            }
        }
        let need = |i: usize, name: &str| known[i].ok_or_else(|| format!("`{name}` is missing"));
        let id = need(0, "id")?;
        if !is_uuid(id) {
            return Err(format!("id `{id}` is not a UUID"));
        }
        let sha256 = need(1, "sha256")?;
        if !is_hex(sha256, 64) {
            return Err("sha256 is not 64 lower-case hex digits".into());
        }
        let size = need(2, "size")?.parse::<u64>().map_err(|_| "size is not a number of bytes".to_string())?;
        let store = need(4, "store")?;
        if !valid_store_name(store) {
            return Err(format!("store `{store}` is not a valid asset store name"));
        }
        Ok(Pointer {
            id: id.to_string(),
            sha256: sha256.to_string(),
            size,
            media_type: known[3].unwrap_or("application/octet-stream").to_string(),
            store: store.to_string(),
            item: known[5].filter(|s| !s.is_empty()).map(str::to_string),
            extra,
        })
    }

    /// The pointer as written: keys in order, LF line ends.
    pub fn to_text(&self) -> String {
        let mut s = format!(
            "textdb-asset: {FORMAT}\nid: {}\nsha256: {}\nsize: {}\ntype: {}\nstore: {}\n",
            self.id, self.sha256, self.size, self.media_type, self.store
        );
        if let Some(item) = &self.item {
            s.push_str(&format!("item: {item}\n"));
        }
        for (k, v) in &self.extra {
            s.push_str(&format!("{k}: {v}\n"));
        }
        s
    }
}

/// A new asset id: UUIDv7, so ids sort by when the asset was first pushed.
pub fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// The SHA-256 (lower-case hex) and size of a file, read in blocks.
pub fn hash_file(path: &Path) -> std::io::Result<(String, u64)> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut size = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Ok((hex(&hasher.finalize()), size))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The media type of a file name, by extension.
pub fn media_type(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "ico" => "image/vnd.microsoft.icon",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "avif" => "image/avif",
        "psd" => "image/vnd.adobe.photoshop",
        "pdf" => "application/pdf",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "odt" => "application/vnd.oasis.opendocument.text",
        "ods" => "application/vnd.oasis.opendocument.spreadsheet",
        "odp" => "application/vnd.oasis.opendocument.presentation",
        "rtf" => "application/rtf",
        "epub" => "application/epub+zip",
        "zip" => "application/zip",
        "7z" => "application/x-7z-compressed",
        "rar" => "application/vnd.rar",
        "tar" => "application/x-tar",
        "gz" | "tgz" => "application/gzip",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "ogg" | "opus" => "audio/ogg",
        "flac" => "audio/flac",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "drawio" => "application/vnd.jgraph.mxfile",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Pointer {
        Pointer {
            id: "01926d1e-7b4a-7c1e-9d2f-3a5b6c7d8e9f".into(),
            sha256: "9f".repeat(32),
            size: 184233,
            media_type: "image/png".into(),
            store: "team-drive".into(),
            item: Some("1AbCdEf".into()),
            extra: vec![],
        }
    }

    #[test]
    fn pointers_round_trip_and_keep_what_they_do_not_know() {
        let p = sample();
        assert_eq!(Pointer::parse(p.to_text().as_bytes()).unwrap(), p);
        let crlf = p.to_text().replace('\n', "\r\n") + "provider-version: 7\r\n";
        let read = Pointer::parse(crlf.as_bytes()).unwrap();
        assert_eq!(read.extra, [("provider-version".to_string(), "7".to_string())]);
        assert!(read.to_text().ends_with("item: 1AbCdEf\nprovider-version: 7\n"));
        assert!(is_uuid(&new_id()) && new_id() != new_id());
    }

    #[test]
    fn bad_pointers_say_what_is_wrong() {
        let text = sample().to_text();
        let err = |t: &str| Pointer::parse(t.as_bytes()).unwrap_err();
        assert!(err("# Not a pointer\n").contains("first line"));
        assert!(err(&text.replace("textdb-asset: 1", "textdb-asset: 2")).contains("format 2"));
        assert!(err(&text.replace(&"9f".repeat(32), "abc")).contains("sha256"));
        assert!(err(&text.replace("size: 184233", "size: big")).contains("size"));
        assert!(err(&text.replace("store: team-drive", "store: a b")).contains("store"));
        assert!(err(&format!("{text}sha256: {}\n", "00".repeat(32))).contains("twice"));
        assert!(err(&text.replace("id: 01926d1e-7b4a-7c1e-9d2f-3a5b6c7d8e9f\n", "")).contains("`id` is missing"));
    }

    #[test]
    fn files_hash_and_types() {
        let dir = std::env::temp_dir().join(format!("textdb-pointer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x.bin");
        std::fs::write(&f, b"abc").unwrap();
        assert_eq!(hash_file(&f).unwrap(), ("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".to_string(), 3));
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(media_type("Deck.PDF"), "application/pdf");
        assert_eq!(media_type("noext"), "application/octet-stream");
        assert!(valid_store_name("team-drive.1") && !valid_store_name("-x") && !valid_store_name(""));
    }
}
