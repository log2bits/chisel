use crate::reader::chunk::ChunkLocation;

pub fn read_locations(data: &[u8]) -> Vec<Option<ChunkLocation>> {
  let mut locations: Vec<Option<ChunkLocation>> = Vec::new();
  for i in 0..1024 {
    let base = i * 4;
    let sector_offset = u32::from_be_bytes([data[base], data[base + 1], data[base + 2], data[base + 3]]) >> 8;
    let sector_count = data[base + 3];
    locations.push(
      match sector_offset | sector_count as u32 {
        0 => None,
        _ => Some(ChunkLocation {
          sector_offset,
          sector_count,
        })
      }
    );
  }
  locations
}