//! Store-only zip archives, built in memory and written atomically. Opus is
//! already compressed, so entries are never deflated.

use std::io::{Cursor, Write};
use std::path::Path;

use anyhow::{Context, Result};
use zip::write::SimpleFileOptions;
use zip::CompressionMethod;

pub struct ZipEntry {
    pub name: String,
    pub bytes: Vec<u8>,
}

impl ZipEntry {
    pub fn stored(name: impl Into<String>, bytes: Vec<u8>) -> Self {
        Self { name: name.into(), bytes }
    }
}

/// Build a store-only zip from `entries`.
pub fn build_stored_zip(entries: &[ZipEntry]) -> Result<Vec<u8>> {
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for e in entries {
        let opts = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .large_file(false);
        w.start_file(&e.name, opts)
            .with_context(|| format!("zip entry {}", e.name))?;
        w.write_all(&e.bytes)?;
    }
    Ok(w.finish()?.into_inner())
}

#[cfg(test)]
/// Read every entry of a zip back, in archive order.
pub fn read_zip(bytes: &[u8]) -> Result<Vec<ZipEntry>> {
    use std::io::Read;
    let mut z = zip::ZipArchive::new(Cursor::new(bytes)).context("open zip")?;
    let mut entries = Vec::with_capacity(z.len());
    for i in 0..z.len() {
        let mut f = z.by_index(i)?;
        let mut v = Vec::with_capacity(f.size() as usize);
        f.read_to_end(&mut v)?;
        entries.push(ZipEntry { name: f.name().to_string(), bytes: v });
    }
    Ok(entries)
}

/// Build and write the zip via `atomic_write`.
pub fn write_stored_zip(path: &Path, entries: &[ZipEntry]) -> Result<()> {
    let bytes = build_stored_zip(entries)?;
    super::atomic_write(path, &bytes).with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_stored() {
        let entries = vec![
            ZipEntry::stored("manifest.json", b"{}".to_vec()),
            ZipEntry::stored("1-2.opus", vec![1, 2, 3]),
        ];
        let bytes = build_stored_zip(&entries).unwrap();
        let z = zip::ZipArchive::new(Cursor::new(bytes.clone())).unwrap();
        assert_eq!(z.len(), 2);
        let back = read_zip(&bytes).unwrap();
        assert_eq!(back[1].name, "1-2.opus");
        assert_eq!(back[1].bytes, vec![1, 2, 3]);
    }
}
