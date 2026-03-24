/// Serialize the color portion of a voxelized brick for materials.bin.
///
/// Format per entry:
///  [4 bytes]  meta: palette_count(8b) | solid_voxel_count(16b) | reserved(8b)
///  [N*4 bytes] palette (RGBA, up to 256 entries)
///  [M bytes]  indices (8-bit palette index per solid voxel, popcount-indexed)
///
/// Geometry (bitmask + coarse) is written separately to geometry.bin.

use super::voxelizer::VoxelGrid;

pub fn serialize_color_data(grid: &VoxelGrid) -> Vec<u8> {
  let solid_count   = grid.color_indices.len() as u32;
  let palette_count = grid.palette.colors.len() as u32;

  let mut out = Vec::with_capacity(4 + palette_count as usize * 4 + solid_count as usize);

  // Meta word: palette_count (bits 31-24) | solid_count (bits 23-8) | reserved (bits 7-0).
  let meta = ((palette_count & 0xFF) << 24) | ((solid_count & 0xFFFF) << 8);
  out.extend_from_slice(&meta.to_le_bytes());

  // Palette entries (RGBA, 4 bytes each).
  for rgba in &grid.palette.colors {
    out.extend_from_slice(rgba);
  }

  // Color indices (1 byte per solid voxel, in popcount order = x+y*16+z*256 order).
  out.extend_from_slice(&grid.color_indices);

  out
}