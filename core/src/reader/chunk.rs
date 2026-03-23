use std::{collections::BTreeMap, io::Read};

use flate2::read::{GzDecoder, ZlibDecoder};
use serde::Deserialize;
use anyhow::Result;

use crate::reader::block_states::BlockStateTable;

pub struct Chunk {
  pub sections: Vec<Section>,
}

pub struct ChunkLocation {
  pub sector_offset: u32,
  pub sector_count: u8,
}
pub struct Section {
  pub y: i8,
  pub blocks: [u16; 4096],
}

#[derive(Deserialize)]
struct RawChunk {
  #[serde(rename = "DataVersion")]
  data_version: i32,
  sections: Vec<RawSection>,
}

#[derive(Deserialize)]
struct RawSection {
  #[serde(rename = "Y")]
  y: i8,
  #[serde(rename = "block_states")]
  block_states: Option<RawBlockStates>,
}

#[derive(Deserialize)]
struct RawBlockStates {
  palette: Vec<PaletteEntry>,
  data: Option<fastnbt::LongArray>,
}

#[derive(Deserialize)]
struct PaletteEntry {
  #[serde(rename = "Name")]
  name: String,
  #[serde(rename = "Properties")]
  #[serde(default)]
  properties: BTreeMap<String, String>,
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

pub fn decode_chunk(data: &[u8], table: &BlockStateTable) -> Result<Chunk> {
  let raw_chunk: RawChunk = fastnbt::from_bytes(data)?;
  
  match raw_chunk.data_version {
    ..1444 => decode_pre_flattening(&raw_chunk, table),
    1444..2564 => decode_legacy_palette(&raw_chunk, table),
    2564.. => decode_modern(&raw_chunk, table),
  }
}

fn decode_pre_flattening(raw_chunk: &RawChunk, table: &BlockStateTable) -> Result<Chunk> {
  todo!();
}

fn decode_legacy_palette(raw_chunk: &RawChunk, table: &BlockStateTable) -> Result<Chunk> {
  let mut chunk = Chunk { sections: Vec::new() };

  for raw_section in &raw_chunk.sections {
    match &raw_section.block_states {
      None => {
        chunk.sections.push(Section { y: raw_section.y, blocks: [0u16; 4096] });
      }
      Some(states) if states.data.is_none() => {
        let id = table.get(&states.palette[0].name, &states.palette[0].properties).unwrap_or(0);
        chunk.sections.push(Section { y: raw_section.y, blocks: [id; 4096] });
      }
      Some(states) => {
        let palette: Vec<u16> = states.palette.iter().map(|entry| {
          table.get(&entry.name, &entry.properties).unwrap_or(0)
        }).collect();

        let bits_per_entry = (usize::BITS - (states.palette.len() - 1).leading_zeros()).max(4) as usize;
        let mask = (1u64 << bits_per_entry) - 1;
        let mut blocks = [0u16; 4096];
        let data = states.data.as_ref().unwrap();

        let mut bit_buf: u64 = 0;
        let mut bits_in_buf: usize = 0;
        let mut long_iter = data.iter();
        let mut block_index = 0;

        while block_index < 4096 {
          // fill the buffer if we don't have enough bits
          while bits_in_buf < bits_per_entry {
            match long_iter.next() {
              Some(&long) => {
                bit_buf |= (long as u64) << bits_in_buf;
                bits_in_buf += 64;
              }
              None => break,
            }
          }
          let palette_index = (bit_buf & mask) as usize;
          bit_buf >>= bits_per_entry;
          bits_in_buf -= bits_per_entry;
          blocks[block_index] = palette[palette_index];
          block_index += 1;
        }

        chunk.sections.push(Section { y: raw_section.y, blocks });
      }
    }
  }

  Ok(chunk)
}

fn decode_modern(raw_chunk: &RawChunk, table: &BlockStateTable) -> Result<Chunk> {
    let mut chunk = Chunk {
      sections: Vec::new(),
    };
    for raw_section in &raw_chunk.sections {
      match &raw_section.block_states {
        None => {
          chunk.sections.push(Section {
            y: raw_section.y,
            blocks: [0u16; 4096],
          });
        }
        Some(states) if states.data.is_none() => {
          let id = table.get(&states.palette[0].name, &states.palette[0].properties).unwrap_or(0);
          chunk.sections.push(Section {
            y: raw_section.y,
            blocks: [id; 4096],
          });
        }
        Some(states) => {
          let palette: Vec<u16> = states.palette.iter().map(|entry| {
            table.get(&entry.name, &entry.properties).unwrap_or(0)
          }).collect();

          let bits_per_entry = (usize::BITS - (states.palette.len() - 1).leading_zeros()).max(4) as usize;
          let entries_per_long = 64 / bits_per_entry;
          let mask = (1u64 << bits_per_entry) - 1;
          let mut blocks = [0u16; 4096];
          let data = states.data.as_ref().unwrap();

          for (long_index, &long) in data.iter().enumerate() {
            for i in 0..entries_per_long {
              let palette_index = (long as u64 >> (i * bits_per_entry) as u64) & mask;
              let block_index = long_index * entries_per_long + i;
              if block_index < 4096 {
                blocks[block_index] = palette[palette_index as usize];
              }
            }
          }

          chunk.sections.push(Section {
            y: raw_section.y,
            blocks,
          });
        }
      }
    }
    Ok(chunk)
}