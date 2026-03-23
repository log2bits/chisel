pub mod region;
pub mod block_states;
pub mod chunk;
pub mod legacy;

use std::{fs::File, io::Read, path::Path};
use zip::ZipArchive;
use anyhow::Result;

pub fn open_world(world_path: &Path) -> Result<()> {
  let table_data = std::fs::read("data/block_states.bin")?;
  let table = block_states::BlockStateTable::load(&table_data);


  let world_file = File::open(world_path)?;
  let mut zip = ZipArchive::new(world_file)?;

  for i in 0..zip.len() {
    let mut file = zip.by_index(i)?;
    let name = file.name().to_string();

    if !name.ends_with(".mca") {
      continue;
    }

    let path = Path::new(&name);
    let parent = path.parent()
      .and_then(|x| x.file_name())
      .and_then(|x| x.to_str());

    let is_region = match parent {
      Some("region") => true,
      None => true,
      _ => false,
    };

    if !is_region {
      continue;
    }

    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    for chunk_location in region::read_locations(&bytes) {
      if let Some(location) = chunk_location {
        let decompressed = chunk::decompress_chunk(&bytes, &location)?;
        let chunk = chunk::decode_chunk(&decompressed, &table)?;
      }
    }
    
    println!("{}", file.name());
  }

  Ok(())
}