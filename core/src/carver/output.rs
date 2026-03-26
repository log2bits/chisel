// Write geometry.bin and materials.bin from voxelized block state results.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::{Context, Result};

use super::voxelizer::VoxelGrid;

// Write geometry.bin: deduplicated voxel bitmask shapes for all block states.
//
// Layout: "GEOM" + count(u32) + num_shapes(u32) + bitmask_ids(count*u16)
//         + shape_table(num_shapes*520 bytes: coarse(u64) + bitmask([u32;128]))
pub fn write_geometry(results: &[(u16, VoxelGrid)], count: usize, path: &Path) -> Result<()> {
  let mut shape_map: HashMap<[u32; 128], u16> = HashMap::new();
  let mut shapes: Vec<(u64, [u32; 128])> = Vec::new();

  // Shape 0 is always the empty (all-zero) bitmask.
  shape_map.insert([0u32; 128], 0);
  shapes.push((0u64, [0u32; 128]));

  let mut bitmask_ids = vec![0u16; count];
  for (id, grid) in results {
    let next = shapes.len() as u16;
    let sid  = *shape_map.entry(grid.bitmask).or_insert_with(|| {
      shapes.push((grid.coarse, grid.bitmask));
      next
    });
    bitmask_ids[*id as usize] = sid;
  }

  let mut w = BufWriter::new(
    File::create(path).with_context(|| format!("creating {path:?}"))?
  );
  w.write_all(b"GEOM")?;
  w.write_all(&(count as u32).to_le_bytes())?;
  w.write_all(&(shapes.len() as u32).to_le_bytes())?;
  for &sid in &bitmask_ids { w.write_all(&sid.to_le_bytes())?; }
  for (coarse, bitmask) in &shapes {
    w.write_all(&coarse.to_le_bytes())?;
    for word in bitmask { w.write_all(&word.to_le_bytes())?; }
  }
  w.flush()?;
  eprintln!(" geometry: {} unique shapes", shapes.len());
  Ok(())
}

// Write materials.bin: deduplicated color payloads for all block states.
//
// Layout: "MATL" + count(u32) + num_payloads(u32)
//         + color_ids(count*u16) + payload_offsets(num_payloads*u32)
//         + payload data (meta(4) + palette(N*4) + indices(M) per payload)
pub fn write_materials(results: &[(u16, VoxelGrid)], count: usize, path: &Path) -> Result<()> {
  let mut payload_map: HashMap<Vec<u8>, u16> = HashMap::new();
  let mut payloads: Vec<Vec<u8>> = Vec::new();
  let mut color_ids = vec![0u16; count];

  for (id, grid) in results {
    if grid.color_indices.is_empty() { continue; }
    let data = serialize_payload(grid);
    let next_id = payloads.len() as u16 + 1;
    use std::collections::hash_map::Entry;
    let color_id = match payload_map.entry(data) {
      Entry::Occupied(e) => *e.get(),
      Entry::Vacant(e) => { payloads.push(e.key().clone()); *e.insert(next_id) }
    };
    color_ids[*id as usize] = color_id;
  }

  let num_payloads = payloads.len();
  let mut payload_offsets = Vec::with_capacity(num_payloads);
  let mut off = 0u32;
  for p in &payloads { payload_offsets.push(off); off += p.len() as u32; }

  let mut w = BufWriter::new(
    File::create(path).with_context(|| format!("creating {path:?}"))?
  );
  w.write_all(b"MATL")?;
  w.write_all(&(count as u32).to_le_bytes())?;
  w.write_all(&(num_payloads as u32).to_le_bytes())?;
  for &cid in &color_ids       { w.write_all(&cid.to_le_bytes())?; }
  for &off  in &payload_offsets { w.write_all(&off.to_le_bytes())?; }
  for p     in &payloads        { w.write_all(p)?; }
  w.flush()?;
  eprintln!(" materials: {} unique payloads", num_payloads);
  Ok(())
}

// Serialize a VoxelGrid's color data into a payload byte vector.
//
// Format: meta(4) + palette(N*4) + indices(M)
// Meta word: palette_count(8b) | solid_count(16b) | flags(8b)
// flags bit 0: is_emissive
fn serialize_payload(grid: &VoxelGrid) -> Vec<u8> {
  let palette_count = grid.palette.colors.len() as u32;
  let solid_count   = grid.color_indices.len() as u32;
  let flags: u32    = if grid.is_emissive { 1 } else { 0 };
  let meta = ((palette_count & 0xFF) << 24) | ((solid_count & 0xFFFF) << 8) | flags;

  let mut out = Vec::with_capacity(4 + palette_count as usize * 4 + solid_count as usize);
  out.extend_from_slice(&meta.to_le_bytes());
  for rgba in &grid.palette.colors { out.extend_from_slice(rgba); }
  out.extend_from_slice(&grid.color_indices);
  out
}
