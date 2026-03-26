use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use serde::Deserialize;
use anyhow::Result;
use chisel_core::reader::block_states::{build_block_key, BlockStateTable};
use chisel_core::carver;
use chisel_core::carver::texture::Palette;
use chisel_core::carver::voxelizer::VoxelGrid;

#[derive(Deserialize)]
struct BlockEntry {
  states: Vec<BlockState>,
}

#[derive(Deserialize)]
struct BlockState {
  id: u32,
  #[serde(default)]
  properties: BTreeMap<String, String>,
}

fn main() -> Result<()> {
  let args: Vec<String> = std::env::args().collect();

  // --export-vox "block_state_key" [output.vox]
  // e.g. cargo run --bin chisel-bake -- --export-vox "dark_oak_log[axis=x]"
  if let Some(pos) = args.iter().position(|a| a == "--export-vox") {
    let key = args.get(pos + 1)
      .ok_or_else(|| anyhow::anyhow!("--export-vox requires a block state key, e.g. \"dark_oak_log[axis=x]\""))?;
    let default_out = format!("{}.vox", key.replace(['[', ']', '=', ','], "_"));
    let out_path = args.get(pos + 2).map(String::as_str).unwrap_or(&default_out);
    return export_vox(key, out_path);
  }

  // Normal full bake.
  let server_jar = Path::new("jars/server.jar").canonicalize()?;
  let client_jar = Path::new("jars/client.jar").canonicalize()?;
  let temp_dir = Path::new("data/temp");
  let out_dir = Path::new("data");

  fs::create_dir_all(out_dir)?;
  fs::create_dir_all(temp_dir)?;

  print!("  [1/3] running minecraft data generator... ");
  let status = Command::new("java")
    .args([
      "-DbundlerMainClass=net.minecraft.data.Main",
      "-jar", server_jar.to_str().unwrap(),
      "--reports",
      "--output", "generated",
    ])
    .current_dir(temp_dir)
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()?;
  if !status.success() {
    println!("failed");
    anyhow::bail!("java data generator failed");
  }
  println!("done");

  print!("  [2/3] generating block_states.bin... ");
  let json_file = fs::File::open(temp_dir.join("generated/reports/blocks.json"))?;
  let blocks: BTreeMap<String, BlockEntry> = serde_json::from_reader(json_file)?;
  let mut entries: Vec<(u16, String)> = Vec::new();
  for (name, entry) in &blocks {
    for state in &entry.states {
      let key = build_block_key(name, &state.properties);
      entries.push((state.id as u16, key));
    }
  }
  BlockStateTable::save(&entries)?;
  fs::remove_dir_all(temp_dir)?;
  println!("done ({} block states)", entries.len());

  print!("  [3/3] running carver... ");
  carver::generate_materials(&client_jar, &out_dir)?;
  println!("done");

  println!("\nall data files generated successfully.");
  Ok(())
}

// ---------------------------------------------------------------------------
// --export-vox
// Reads from data/block_states.bin, data/geometry.bin, data/materials.bin
// to test the full end-to-end pipeline without re-voxelizing.
// ---------------------------------------------------------------------------

/// Normalize a block state key so properties are sorted alphabetically,
/// matching the format stored in block_states.bin.
fn normalize_key(key: &str) -> String {
  if let Some(bracket) = key.find('[') {
    let name = &key[..bracket];
    let props_str = &key[bracket+1..key.len()-1];
    let props: BTreeMap<String, String> = props_str.split(',')
      .filter_map(|kv| { let mut it = kv.splitn(2, '='); Some((it.next()?.to_string(), it.next()?.to_string())) })
      .collect();
    build_block_key(name, &props)
  } else {
    key.to_string()
  }
}

fn export_vox(block_state_key: &str, out_path: &str) -> Result<()> {
  let key = normalize_key(block_state_key);

  // block_states.bin → block state ID
  let bs_data = fs::read("data/block_states.bin")?;
  let bs_table = BlockStateTable::load(&bs_data);
  let block_id = bs_table.lookup(&key)
    .ok_or_else(|| anyhow::anyhow!("block state not found: {key}"))?;
  println!("block: {key}  id={block_id}");

  // geometry.bin → bitmask + coarse
  let geom = fs::read("data/geometry.bin")?;
  anyhow::ensure!(&geom[0..4] == b"GEOM", "bad geometry.bin magic");
  let count      = u32::from_le_bytes(geom[4..8].try_into()?)   as usize;
  let bitmask_id = u16::from_le_bytes(geom[12 + block_id as usize * 2..][..2].try_into()?) as usize;
  let shape_base = 12 + count * 2 + bitmask_id * 520;
  let coarse     = u64::from_le_bytes(geom[shape_base..][..8].try_into()?);
  let mut bitmask = [0u32; 128];
  for (i, w) in bitmask.iter_mut().enumerate() {
    *w = u32::from_le_bytes(geom[shape_base + 8 + i*4..][..4].try_into()?);
  }
  let solid: u32 = bitmask.iter().map(|w| w.count_ones()).sum();
  println!("shape id={bitmask_id}  solid={solid}");

  // materials.bin → palette + color indices
  let mat = fs::read("data/materials.bin")?;
  anyhow::ensure!(&mat[0..4] == b"MATL", "bad materials.bin magic");
  let mat_count    = u32::from_le_bytes(mat[4..8].try_into()?)   as usize;
  let num_payloads = u32::from_le_bytes(mat[8..12].try_into()?)  as usize;
  let color_ids_base      = 12;
  let payload_offsets_base = color_ids_base + mat_count * 2;
  let payload_data_base    = payload_offsets_base + num_payloads * 4;

  let color_id = u16::from_le_bytes(
    mat[color_ids_base + block_id as usize * 2..][..2].try_into()?
  ) as usize;

  let (palette, color_indices, is_emissive) = if color_id == 0 {
    (Palette::default(), Vec::new(), false)
  } else {
    let payload_off = u32::from_le_bytes(
      mat[payload_offsets_base + (color_id - 1) * 4..][..4].try_into()?
    ) as usize;
    let p = payload_data_base + payload_off;

    let meta          = u32::from_le_bytes(mat[p..][..4].try_into()?);
    let mut pal_count = ((meta >> 24) & 0xFF) as usize;
    let solid_count   = ((meta >> 8) & 0xFFFF) as usize;
    let is_emissive   = (meta & 1) != 0;
    if solid_count > 0 && pal_count == 0 { pal_count = 256; }

    let mut palette = Palette::default();
    let pal_base = p + 4;
    for i in 0..pal_count {
      let o = pal_base + i * 4;
      palette.get_or_insert([mat[o], mat[o+1], mat[o+2], mat[o+3]]);
    }
    let idx_base = pal_base + pal_count * 4;
    (palette, mat[idx_base..idx_base + solid_count].to_vec(), is_emissive)
  };
  println!("color_id={color_id}  palette={} colors  indices={}  emissive={}", palette.colors.len(), color_indices.len(), is_emissive);

  let grid = VoxelGrid { bitmask, coarse, color_indices, palette, is_emissive };
  let bytes = write_vox(&grid);
  fs::write(out_path, &bytes)?;
  println!("wrote {out_path}");
  Ok(())
}

// ---------------------------------------------------------------------------
// MagicaVoxel .vox writer
// Coordinate remap: MC(x,y,z) -> Vox(x, z, y)  (Y-up -> Z-up)
// ---------------------------------------------------------------------------

fn write_vox(grid: &VoxelGrid) -> Vec<u8> {
  let mut voxels: Vec<(u8,u8,u8,u8)> = Vec::new();
  let mut color_iter = grid.color_indices.iter();
  for z in 0u8..16 {
    for y in 0u8..16 {
      for x in 0u8..16 {
        let flat = x as usize + y as usize * 16 + z as usize * 256;
        if (grid.bitmask[flat/32] >> (flat%32)) & 1 != 0 {
          let ci = color_iter.next().copied().unwrap_or(0);
          voxels.push((x, z, y, ci + 1)); // vox palette is 1-indexed
        }
      }
    }
  }

  let mut palette = [[0u8;4]; 256];
  for (i, rgba) in grid.palette.colors.iter().enumerate() {
    if i < 256 { palette[i] = [rgba[0], rgba[1], rgba[2], 255]; }
  }

  let mut main_children: Vec<u8> = Vec::new();

  // SIZE
  let mut s = Vec::new();
  s.extend_from_slice(&16u32.to_le_bytes());
  s.extend_from_slice(&16u32.to_le_bytes());
  s.extend_from_slice(&16u32.to_le_bytes());
  main_children.extend_from_slice(&chunk(b"SIZE", &s, &[]));

  // XYZI
  let mut xi = Vec::new();
  xi.extend_from_slice(&(voxels.len() as u32).to_le_bytes());
  for (x,y,z,ci) in &voxels { xi.extend_from_slice(&[*x,*y,*z,*ci]); }
  main_children.extend_from_slice(&chunk(b"XYZI", &xi, &[]));

  // RGBA
  let mut rgba = Vec::with_capacity(1024);
  for c in &palette { rgba.extend_from_slice(c); }
  main_children.extend_from_slice(&chunk(b"RGBA", &rgba, &[]));

  let mut out = Vec::new();
  out.extend_from_slice(b"VOX ");
  out.extend_from_slice(&150u32.to_le_bytes());
  out.extend_from_slice(&chunk(b"MAIN", &[], &main_children));
  out
}

fn chunk(id: &[u8;4], content: &[u8], children: &[u8]) -> Vec<u8> {
  let mut v = Vec::new();
  v.extend_from_slice(id);
  v.extend_from_slice(&(content.len() as u32).to_le_bytes());
  v.extend_from_slice(&(children.len() as u32).to_le_bytes());
  v.extend_from_slice(content);
  v.extend_from_slice(children);
  v
}