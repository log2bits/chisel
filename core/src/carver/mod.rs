//! The Carver: reads all Minecraft block models + textures from the client JAR,
//! voxelizes every block state into a 16×16×16 brick, writes geometry.bin + materials.bin.
//!
//! geometry.bin — hot path (stays in GPU L2 cache):
//!   [4]               "GEOM"
//!   [4]               count (u32)
//!   [4]               num_shapes (u32)
//!   [count × 2]       bitmask_id per block state (u16)
//!   [num_shapes × 520] shape table: coarse(u64) + bitmask([u32;128])
//!
//! materials.bin — cold path (accessed once per ray hit):
//!   [4]                  "MATL"
//!   [4]                  count (u32)
//!   [4]                  num_payloads (u32)
//!   [count × 2]          color_id per block state (u16, 0 = no color)
//!   [num_payloads × 4]   payload byte offsets (u32, from start of payload data)
//!   [variable]           payload data: meta(4) + palette(N×4) + indices(M) per payload

pub mod jar;
pub mod model;
pub mod texture;
pub mod voxelizer;
pub mod brick;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use rayon::prelude::*;
use serde::Deserialize;

use jar::Jar;
use model::build_quads;
use texture::{load_texture, RgbaImage};
use voxelizer::{voxelize, VoxelGrid};
use brick::serialize_color_data;

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
  #[serde(default)] x: u32,
  #[serde(default)] y: u32,
}

#[derive(Deserialize)]
struct BlockstateJson {
  #[serde(default)] variants: HashMap<String, BlockstateApply>,
  #[serde(default)] multipart: Vec<MultipartEntry>,
}

#[derive(Deserialize)]
struct MultipartEntry {
  apply: BlockstateApply,
}

// block_states.bin reader

fn load_block_state_entries(path: &Path) -> Result<Vec<(u16, String)>> {
  let data = fs::read(path).with_context(|| format!("reading {path:?}"))?;
  let magic = u32::from_le_bytes(data[0..4].try_into()?);
  if magic != 0x42534944 { anyhow::bail!("invalid block_states.bin magic"); }
  let count = u32::from_le_bytes(data[4..8].try_into()?) as usize;
  let mut entries = Vec::with_capacity(count);
  let mut cursor = 8usize;
  for _ in 0..count {
    let id  = u16::from_le_bytes(data[cursor..cursor+2].try_into()?);
    let len = u16::from_le_bytes(data[cursor+2..cursor+4].try_into()?) as usize;
    let key = std::str::from_utf8(&data[cursor+4..cursor+4+len])?.to_owned();
    cursor += 4 + len;
    entries.push((id, key));
  }
  Ok(entries)
}

// Thread-local state for rayon workers.
// Each worker thread opens the JAR once and keeps its own texture cache,
// eliminating all locking overhead during parallel voxelization.
thread_local! {
  static THREAD_JAR: RefCell<Option<Jar>> = RefCell::new(None);
  static THREAD_TEX: RefCell<HashMap<String, RgbaImage>> = RefCell::new(HashMap::new());
}

// Entry point

pub fn generate_materials(client_jar: &Path, output_dir: &Path) -> Result<()> {
  let mut entries = load_block_state_entries(Path::new("data/block_states.bin"))?;
  entries.sort_by_key(|(id, _)| *id);
  let max_id = entries.iter().map(|(id, _)| *id).max().unwrap_or(0);
  let count  = max_id as usize + 1;

  eprintln!(" carver: {} block states, max id={}", entries.len(), max_id);

  let jar_path = client_jar.to_path_buf();
  let total    = entries.len();
  let progress = AtomicUsize::new(0);

  // Voxelize every block state in parallel, collecting raw VoxelGrids.
  let mut results: Vec<(u16, VoxelGrid)> = entries.par_iter().map(|(id, key)| {
    let grid = THREAD_JAR.with(|cell| {
      let mut opt = cell.borrow_mut();
      if opt.is_none() {
        *opt = Some(Jar::open(&jar_path).expect("failed to open JAR in worker thread"));
      }
      let jar = opt.as_mut().unwrap();
      THREAD_TEX.with(|cache_cell| {
        voxelize_block_state(key, jar, &mut cache_cell.borrow_mut())
          .unwrap_or_default()
      })
    });

    let done = progress.fetch_add(1, Ordering::Relaxed) + 1;
    if done % 200 == 0 || done == total {
      eprint!("\r carver: {done}/{total}");
      let _ = std::io::stderr().flush();
    }

    (*id, grid)
  }).collect();

  eprintln!("\r carver: {total}/{total} done     ");

  results.sort_unstable_by_key(|(id, _)| *id);

  // --- geometry.bin ---
  //
  // Deduplicate bitmask shapes across all block states.
  // ~29K states collapse to ~400 unique shapes; the shape table fits in GPU L2.
  //
  // Layout:
  //   [4]               "GEOM"
  //   [4]               count (u32)
  //   [4]               num_shapes (u32)
  //   [count × 2]       bitmask_id per block state (u16)
  //   [num_shapes × 520] shape table: coarse(u64) + bitmask([u32;128])

  let mut shape_map: HashMap<[u32; 128], u16> = HashMap::new();
  let mut shapes: Vec<(u64, [u32; 128])> = Vec::new();

  // Shape 0 is always the empty (all-zero) bitmask.
  shape_map.insert([0u32; 128], 0);
  shapes.push((0u64, [0u32; 128]));

  let mut bitmask_ids = vec![0u16; count];
  for (id, grid) in &results {
    let next = shapes.len() as u16;
    let sid  = *shape_map.entry(grid.bitmask).or_insert_with(|| {
      shapes.push((grid.coarse, grid.bitmask));
      next
    });
    bitmask_ids[*id as usize] = sid;
  }

  let geom_path = output_dir.join("geometry.bin");
  let mut gw = BufWriter::new(
    File::create(&geom_path).with_context(|| format!("creating {geom_path:?}"))?
  );
  gw.write_all(b"GEOM")?;
  gw.write_all(&(count as u32).to_le_bytes())?;
  gw.write_all(&(shapes.len() as u32).to_le_bytes())?;
  for &sid in &bitmask_ids { gw.write_all(&sid.to_le_bytes())?; }
  for (coarse, bitmask) in &shapes {
    gw.write_all(&coarse.to_le_bytes())?;
    for word in bitmask { gw.write_all(&word.to_le_bytes())?; }
  }
  gw.flush()?;
  eprintln!(" geometry: {} unique shapes", shapes.len());

  // --- materials.bin ---
  //
  // Color data deduplicated: 26K block states -> ~4K unique payloads.
  // color_id 0 = no color (empty/air). Valid color_ids are 1..=num_payloads.
  //
  // Layout:
  //   [4]                  "MATL"
  //   [4]                  count (u32)
  //   [4]                  num_payloads (u32)
  //   [count × 2]          color_id per block state (u16, 0 = no color)
  //   [num_payloads × 4]   payload byte offsets (u32, from start of payload data)
  //   [variable]           payload data

  let mut payload_map: HashMap<Vec<u8>, u16> = HashMap::new();
  let mut payloads: Vec<Vec<u8>> = Vec::new();
  let mut color_ids = vec![0u16; count];

  for (id, grid) in &results {
    if grid.color_indices.is_empty() { continue; }
    let data = serialize_color_data(grid);
    let next_id = payloads.len() as u16 + 1;
    use std::collections::hash_map::Entry;
    let color_id = match payload_map.entry(data) {
      Entry::Occupied(e) => *e.get(),
      Entry::Vacant(e) => {
        payloads.push(e.key().clone());
        *e.insert(next_id)
      }
    };
    color_ids[*id as usize] = color_id;
  }

  let num_payloads = payloads.len();

  // Compute per-payload byte offsets (from start of payload data section).
  let mut payload_offsets = Vec::with_capacity(num_payloads);
  let mut off = 0u32;
  for p in &payloads {
    payload_offsets.push(off);
    off += p.len() as u32;
  }

  let mat_path = output_dir.join("materials.bin");
  let mut mw = BufWriter::new(
    File::create(&mat_path).with_context(|| format!("creating {mat_path:?}"))?
  );

  mw.write_all(b"MATL")?;
  mw.write_all(&(count as u32).to_le_bytes())?;
  mw.write_all(&(num_payloads as u32).to_le_bytes())?;
  for &cid in &color_ids       { mw.write_all(&cid.to_le_bytes())?; }
  for &off  in &payload_offsets { mw.write_all(&off.to_le_bytes())?; }
  for p in &payloads            { mw.write_all(p)?; }
  mw.flush()?;

  eprintln!(" materials: {} unique payloads", num_payloads);

  Ok(())
}

// Per-block-state voxelization (called from rayon workers)

fn parse_key(key: &str) -> (&str, HashMap<&str, &str>) {
  if let Some(bracket) = key.find('[') {
    let name      = &key[..bracket];
    let props_str = &key[bracket+1..key.len()-1];
    let props = props_str.split(',')
      .filter_map(|kv| { let mut it = kv.splitn(2, '='); Some((it.next()?, it.next()?)) })
      .collect();
    (name, props)
  } else {
    (key, HashMap::new())
  }
}

// Returns (model_name, x_rot, y_rot)
fn apply_name(apply: &BlockstateApply) -> (String, u32, u32) {
  match apply {
    BlockstateApply::Single(v) => (v.model.trim_start_matches("minecraft:").to_owned(), v.x, v.y),
    BlockstateApply::List(list) => list.first()
      .map(|v| (v.model.trim_start_matches("minecraft:").to_owned(), v.x, v.y))
      .unwrap_or_default(),
  }
}

fn find_model_name(short_name: &str, props: &HashMap<&str, &str>, jar: &mut Jar) -> Result<Option<(String, u32, u32)>> {
  let bs_path = format!("assets/minecraft/blockstates/{short_name}.json");
  let bytes = match jar.get(&bs_path)? { Some(b) => b, None => return Ok(None) };
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

fn find_variant_model(variants: &HashMap<String, BlockstateApply>, props: &HashMap<&str, &str>) -> Option<(String, u32, u32)> {
  if variants.len() == 1 {
    return variants.values().next().map(apply_name);
  }

  let mut best: Option<&BlockstateApply> = None;
  let mut best_score = 0usize;

  for (key, apply) in variants {
    if key.is_empty() {
      if best_score == 0 { best = Some(apply); }
      continue;
    }
    let mut score = 0usize;
    let mut all_match = true;
    for part in key.split(',') {
      let mut kv = part.splitn(2, '=');
      match (kv.next(), kv.next()) {
        (Some(k), Some(v)) => {
          if props.get(k) == Some(&v) { score += 1; } else { all_match = false; break; }
        }
        _ => { all_match = false; break; }
      }
    }
    if all_match && score > best_score { best_score = score; best = Some(apply); }
  }

  best.map(apply_name)
}

fn voxelize_block_state(key: &str, jar: &mut Jar, tex_cache: &mut HashMap<String, RgbaImage>) -> Result<VoxelGrid> {
  if matches!(key, "air" | "void_air" | "cave_air") {
    return Ok(VoxelGrid::default());
  }

  let (short_name, props) = parse_key(key);
  let (model_name, bs_x_rot, bs_y_rot) = match find_model_name(short_name, &props, jar)? {
    Some(m) if !m.0.is_empty() => m,
    _ => return Ok(VoxelGrid::default()),
  };

  let quads = build_quads(&model_name, jar, bs_x_rot, bs_y_rot)?;

  let needed: HashSet<&str> = quads.iter().map(|q| q.texture.as_str()).collect();
  for tex in needed {
    if !tex_cache.contains_key(tex) {
      if let Ok(img) = load_texture(tex, jar) {
        tex_cache.insert(tex.to_owned(), img);
      }
    }
  }

  Ok(voxelize(&quads, tex_cache))
}
