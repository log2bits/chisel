// Read-only accessor for the Minecraft client JAR (a ZIP file).
//
// Caches decompressed file bytes in a HashMap so repeated lookups of the same
// asset (e.g. the same texture referenced by several block models) are free.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use zip::ZipArchive;

pub struct Jar {
  archive: ZipArchive<File>,
  cache: HashMap<String, Vec<u8>>,
}

impl Jar {
  pub fn open(path: &Path) -> Result<Self> {
    let file = File::open(path)
      .with_context(|| format!("opening client jar {:?}", path))?;
    let archive = ZipArchive::new(file)
      .with_context(|| format!("parsing client jar {:?} as ZIP", path))?;
    Ok(Self { archive, cache: HashMap::new() })
  }

  // Returns the decompressed bytes for a JAR-internal path, or None if not found.
  pub fn get(&mut self, path: &str) -> Result<Option<Vec<u8>>> {
    if let Some(bytes) = self.cache.get(path) {
      return Ok(Some(bytes.clone()));
    }
    let result = self.archive.by_name(path);
    match result {
      Err(zip::result::ZipError::FileNotFound) => return Ok(None),
      Err(e) => return Err(e).with_context(|| format!("reading JAR entry {path}")),
      Ok(mut entry) => {
        let mut bytes = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut bytes)
          .with_context(|| format!("decompressing JAR entry {path}"))?;
        self.cache.insert(path.to_owned(), bytes.clone());
        Ok(Some(bytes))
      }
    }
  }

  // Like get, but errors instead of returning None if the entry's missing.
  pub fn get_required(&mut self, path: &str) -> Result<Vec<u8>> {
    self.get(path)?.with_context(|| format!("JAR entry not found: {path}"))
  }
}