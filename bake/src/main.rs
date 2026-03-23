use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use serde::Deserialize;
use anyhow::Result;
use chisel_core::reader::block_states::{build_block_key, BlockStateTable};
use chisel_core::carver;
use chisel_core::carver::jar::Jar;
use chisel_core::carver::model::build_quads;
use chisel_core::carver::texture::load_texture;
use chisel_core::carver::voxelizer::{voxelize, VoxelGrid};

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
    let client_jar = Path::new("jars/client.jar").canonicalize()?;
    return export_vox(&client_jar, key, out_path);
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
  carver::generate_materials(&client_jar, &out_dir.join("materials.bin"))?;
  println!("done");

  println!("\nall data files generated successfully.");
  Ok(())
}

// ---------------------------------------------------------------------------
// --export-vox
// ---------------------------------------------------------------------------

fn export_vox(client_jar: &Path, block_state_key: &str, out_path: &str) -> Result<()> {
  println!("JAR:   {:?}", client_jar);
  println!("block: {}", block_state_key);

  let mut jar = Jar::open(client_jar)?;

  // Parse "dark_oak_log[axis=x]" -> name="dark_oak_log", props={axis:x}
  let (short_name, props) = parse_key(block_state_key);

  // Look up which model to use from the blockstate JSON.
  let model_name = find_model_name(short_name, &props, &mut jar)?
    .ok_or_else(|| anyhow::anyhow!("no blockstate JSON found for '{short_name}'"))?;
  println!("model: {}", model_name);

  let quads = build_quads(&model_name, &mut jar)?;
  println!("quads: {}", quads.len());

  let mut textures = HashMap::new();
  for q in &quads {
    if !textures.contains_key(&q.texture) {
      match load_texture(&q.texture, &mut jar) {
        Ok(img) => { textures.insert(q.texture.clone(), img); }
        Err(e)  => eprintln!("  warn: texture '{}' failed: {:?}", q.texture, e),
      }
    }
  }
  println!("textures: {}", textures.len());

  let grid = voxelize(&quads, &textures);
  let solid: u32 = grid.bitmask.iter().map(|w| w.count_ones()).sum();
  println!("solid voxels: {solid}  palette colors: {}", grid.palette.colors.len());

  let bytes = write_vox(&grid);
  fs::write(out_path, &bytes)?;
  println!("wrote {out_path}");
  Ok(())
}

fn parse_key(key: &str) -> (&str, HashMap<&str, &str>) {
  if let Some(bracket) = key.find('[') {
    let name = &key[..bracket];
    let props_str = &key[bracket+1..key.len()-1];
    let props = props_str.split(',')
      .filter_map(|kv| { let mut it = kv.splitn(2, '='); Some((it.next()?, it.next()?)) })
      .collect();
    (name, props)
  } else {
    (key, HashMap::new())
  }
}

fn find_model_name(
  short_name: &str,
  props: &HashMap<&str, &str>,
  jar: &mut Jar,
) -> Result<Option<String>> {
  use serde::Deserialize;

  #[derive(Deserialize)]
  #[serde(untagged)]
  enum Apply { Single(Variant), List(Vec<Variant>) }

  #[derive(Deserialize)]
  struct Variant { model: String }

  #[derive(Deserialize)]
  struct Bs {
    #[serde(default)] variants: HashMap<String, Apply>,
    #[serde(default)] multipart: Vec<MpEntry>,
  }

  #[derive(Deserialize)]
  struct MpEntry { apply: Apply }

  let path = format!("assets/minecraft/blockstates/{short_name}.json");
  let bytes = match jar.get(&path)? { Some(b) => b, None => return Ok(None) };
  let bs: Bs = serde_json::from_slice(&bytes)?;

  let model_str = |a: &Apply| -> String {
    match a {
      Apply::Single(v) => v.model.trim_start_matches("minecraft:").to_owned(),
      Apply::List(l)   => l.first().map(|v| v.model.trim_start_matches("minecraft:").to_owned()).unwrap_or_default(),
    }
  };

  if !bs.variants.is_empty() {
    // Single-variant blockstates (e.g. stone) have key "".
    if bs.variants.len() == 1 {
      return Ok(bs.variants.values().next().map(model_str));
    }
    let mut best: Option<&Apply> = None;
    let mut best_score = 0usize;
    for (key, apply) in &bs.variants {
      if key.is_empty() { if best_score == 0 { best = Some(apply); } continue; }
      let mut score = 0usize;
      let mut all = true;
      for part in key.split(',') {
        let mut kv = part.splitn(2, '=');
        match (kv.next(), kv.next()) {
          (Some(k), Some(v)) => { if props.get(k) == Some(&v) { score += 1; } else { all = false; break; } }
          _ => { all = false; break; }
        }
      }
      if all && score > best_score { best_score = score; best = Some(apply); }
    }
    return Ok(best.map(model_str));
  }

  if let Some(first) = bs.multipart.first() {
    return Ok(Some(model_str(&first.apply)));
  }

  Ok(None)
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