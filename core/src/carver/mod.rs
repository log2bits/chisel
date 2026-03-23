//! The Carver: reads all Minecraft block models + textures from the client JAR,
//! voxelizes every block state into a 16×16×16 brick, writes materials.bin.

pub mod jar;
pub mod model;
pub mod texture;
pub mod voxelizer;
pub mod brick;

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use jar::Jar;
use model::build_quads;
use texture::load_texture;
use texture::RgbaImage;
use voxelizer::voxelize;
use brick::serialize_brick;

// Blockstate JSON types

#[derive(Deserialize)]
#[serde(untagged)]
enum BlockstateApply {
  Single(BlockstateVariant),
  List(Vec<BlockstateVariant>),
}

#[derive(Deserialize)]
struct BlockstateVariant {
  model: String,
  #[serde(default)]
  x: i32,
  #[serde(default)]
  y: i32,
}

#[derive(Deserialize)]
struct BlockstateJson {
  #[serde(default)]
  variants: HashMap<String, BlockstateApply>,
  #[serde(default)]
  multipart: Vec<MultipartEntry>,
}

#[derive(Deserialize)]
struct MultipartEntry {
  apply: BlockstateApply,
}

// Read block_states.bin

/// Read all (id, key) pairs from block_states.bin.
/// Keys are stored WITHOUT the "minecraft:" prefix.
fn load_block_state_entries(path: &Path) -> Result<Vec<(u16, String)>> {
  let data = fs::read(path)
    .with_context(|| format!("reading {path:?}"))?;

  let magic = u32::from_le_bytes(data[0..4].try_into()?);
  if magic != 0x42534944 {
    anyhow::bail!("invalid block_states.bin magic");
  }
  let count = u32::from_le_bytes(data[4..8].try_into()?) as usize;
  let mut entries = Vec::with_capacity(count);
  let mut cursor = 8usize;
  for _ in 0..count {
    let id = u16::from_le_bytes(data[cursor..cursor+2].try_into()?);
    let len = u16::from_le_bytes(data[cursor+2..cursor+4].try_into()?) as usize;
    let key = std::str::from_utf8(&data[cursor+4..cursor+4+len])?.to_owned();
    cursor += 4 + len;
    entries.push((id, key));
  }
  Ok(entries)
}

// Entry point

pub fn generate_materials(client_jar: &Path, output_path: &Path) -> Result<()> {
  let mut jar = Jar::open(client_jar)?;

  let block_states_path = Path::new("data/block_states.bin");
  let mut entries = load_block_state_entries(block_states_path)?;
  entries.sort_by_key(|(id, _)| *id);
  let max_id = entries.iter().map(|(id, _)| *id).max().unwrap_or(0);

  eprintln!(" carver: {} block states, max id={}", entries.len(), max_id);

  let empty_brick = serialize_brick(&voxelize(&[], &HashMap::new()));
  let mut texture_cache: HashMap<String, RgbaImage> = HashMap::new();

  let out_file = File::create(output_path)
    .with_context(|| format!("creating {output_path:?}"))?;
  let mut writer = BufWriter::new(out_file);

  writer.write_all(b"MATL")?;
  writer.write_all(&(max_id as u32 + 1).to_le_bytes())?;

  let mut written_id = 0u16;
  let total = entries.len();

  for (i, (id, key)) in entries.iter().enumerate() {
    if i % 200 == 0 {
      eprint!("\r carver: {i}/{total}");
      let _ = std::io::stderr().flush();
    }

    while written_id < *id {
      writer.write_all(&empty_brick)?;
      written_id += 1;
    }

    let brick_bytes = voxelize_block_state(key, &mut jar, &mut texture_cache)
      .unwrap_or_else(|_| serialize_brick(&voxelize(&[], &HashMap::new())));

    writer.write_all(&brick_bytes)?;
    written_id += 1;
  }

  eprintln!("\r carver: {total}/{total} done     ");
  writer.flush()?;
  Ok(())
}

// Per-block-state voxelization

/// Keys in block_states.bin have NO "minecraft:" prefix.
/// e.g. "grass_block[snowy=false]" or "stone".
fn parse_key(key: &str) -> (&str, HashMap<&str, &str>) {
  if let Some(bracket) = key.find('[') {
    let name = &key[..bracket];
    let props_str = &key[bracket+1..key.len()-1];
    let props = props_str.split(',')
      .filter_map(|kv| {
        let mut it = kv.splitn(2, '=');
        Some((it.next()?, it.next()?))
      })
      .collect();
    (name, props)
  } else {
    (key, HashMap::new())
  }
}

fn apply_name(apply: &BlockstateApply) -> String {
  match apply {
    BlockstateApply::Single(v) => v.model.trim_start_matches("minecraft:").to_owned(),
    BlockstateApply::List(list) => list.first()
      .map(|v| v.model.trim_start_matches("minecraft:").to_owned())
      .unwrap_or_default(),
  }
}

fn find_model_name(short_name: &str, props: &HashMap<&str, &str>, jar: &mut Jar) -> Result<Option<String>> {
  let bs_path = format!("assets/minecraft/blockstates/{short_name}.json");
  let bytes = match jar.get(&bs_path)? {
    Some(b) => b,
    None => return Ok(None),
  };
  let bs: BlockstateJson = serde_json::from_slice(&bytes)
    .with_context(|| format!("parsing blockstate {bs_path}"))?;

  if !bs.variants.is_empty() {
    return Ok(find_variant_model(&bs.variants, props));
  }
  if let Some(first) = bs.multipart.first() {
    return Ok(Some(apply_name(&first.apply)));
  }
  Ok(None)
}

fn find_variant_model(variants: &HashMap<String, BlockstateApply>, props: &HashMap<&str, &str>) -> Option<String> {
  // Empty key "" = catch-all.
  if variants.len() == 1 {
    if let Some((_, model)) = variants.iter().next() {
      return Some(apply_name(model));
    }
  }

  let mut best: Option<&BlockstateApply> = None;
  let mut best_score = 0usize;

  for (key, model) in variants {
    if key.is_empty() {
      if best_score == 0 { best = Some(model); }
      continue;
    }
    let mut score = 0usize;
    let mut all_match = true;
    for part in key.split(',') {
      let mut kv = part.splitn(2, '=');
      match (kv.next(), kv.next()) {
        (Some(k), Some(v)) => {
          if props.get(k) == Some(&v) { score += 1; }
          else { all_match = false; break; }
        }
        _ => { all_match = false; break; }
      }
    }
    if all_match && score > best_score {
      best_score = score;
      best = Some(model);
    }
  }

  best.map(apply_name)
}

fn voxelize_block_state(key: &str, jar: &mut Jar, texture_cache: &mut HashMap<String, RgbaImage>) -> Result<Vec<u8>> {
  let empty = || serialize_brick(&voxelize(&[], &HashMap::new()));

  // Air variants.
  if matches!(key, "air" | "void_air" | "cave_air") {
    return Ok(empty());
  }

  let (short_name, props) = parse_key(key);
  let model_name = match find_model_name(short_name, &props, jar)? {
    Some(m) if !m.is_empty() => m,
    _ => return Ok(empty()),
  };

  let quads = build_quads(&model_name, jar)?;

  // Load textures.
  let needed: HashSet<String> = quads.iter().map(|q| q.texture.clone()).collect();
  for tex in &needed {
    if !texture_cache.contains_key(tex) {
      if let Ok(img) = load_texture(tex, jar) {
        texture_cache.insert(tex.clone(), img);
      }
    }
  }

  let mut textures: HashMap<String, RgbaImage> = HashMap::new();
  for k in &needed {
    if let Some(v) = texture_cache.get(k) {
      textures.insert(k.clone(), v.clone());
    }
  }

  let grid = voxelize(&quads, &textures);
  Ok(serialize_brick(&grid))
}