use std::io::Read;

use anyhow::Result;
use flate2::read::{GzDecoder, ZlibDecoder};


pub struct ChunkLocation {
  pub sector_offset: u32,
  pub sector_count: u8,
}

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

pub fn decompress_chunk(data: &[u8], chunk_location: &ChunkLocation) -> Result<Vec<u8>> {
  let base = (chunk_location.sector_offset * 4096) as usize;
  let length = u32::from_be_bytes([data[base], data[base+1], data[base+2], data[base+3]]);
  let compression_type = data[base + 4];
  let compressed = &data[base + 5..base + 4 + length as usize];
  match compression_type {
    1 => {
      let mut decoder = GzDecoder::new(compressed);
      let mut out = Vec::new();
      decoder.read_to_end(&mut out)?;
      Ok(out)
    }
    2 => {
      let mut decoder = ZlibDecoder::new(compressed);
      let mut out = Vec::new();
      decoder.read_to_end(&mut out)?;
      Ok(out)
    }
    3 => {
      Ok(compressed.to_vec())
    }
    _ => Err(anyhow::anyhow!("unsupported compression type: {}", compression_type)),
  }
}