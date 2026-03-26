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
//!                        meta flags (bits 7-0): bit 0 = is_emissive

pub mod blockstate;
pub mod jar;
pub mod model;
pub mod output;
pub mod texture;
pub mod voxelizer;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use rayon::prelude::*;

use jar::Jar;
use model::build_quads;
use texture::{load_texture, RgbaImage};
use voxelizer::{voxelize, VoxelGrid, voxelize_fluid, apply_waterlogging};

// ── Thread-local state ────────────────────────────────────────────────────────
// Each rayon worker opens the JAR once and keeps its own texture cache,
// eliminating all locking overhead during parallel voxelization.

thread_local! {
  static THREAD_JAR: RefCell<Option<Jar>> = RefCell::new(None);
  static THREAD_TEX: RefCell<HashMap<String, RgbaImage>> = RefCell::new(HashMap::new());
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn generate_materials(client_jar: &Path, output_dir: &Path) -> Result<()> {
  let mut entries = blockstate::load_entries(Path::new("data/block_states.bin"))?;
  entries.sort_by_key(|(id, _)| *id);
  let max_id = entries.iter().map(|(id, _)| *id).max().unwrap_or(0);
  let count  = max_id as usize + 1;

  eprintln!(" carver: {} block states, max id={}", entries.len(), max_id);

  let jar_path = client_jar.to_path_buf();
  let total    = entries.len();
  let progress = AtomicUsize::new(0);

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

  output::write_geometry(&results, count, &output_dir.join("geometry.bin"))?;
  output::write_materials(&results, count, &output_dir.join("materials.bin"))?;

  Ok(())
}

// ── Biome tint colors (plains) ────────────────────────────────────────────────

/// Returns the plains-biome tint color for a block name.
/// Used for any face with `tintindex` in the model JSON.
fn plains_tint(name: &str) -> [u8; 3] {
  match name {
    "oak_leaves" | "jungle_leaves" | "acacia_leaves" | "dark_oak_leaves"
    | "mangrove_leaves" | "cherry_leaves" | "vine"
      => [0x77, 0xAB, 0x2F], // foliage colormap at plains
    _ => [0x91, 0xBD, 0x59], // grass colormap at plains
  }
}

// ── Emissive blocks ───────────────────────────────────────────────────────────

fn is_emissive(name: &str, props: &HashMap<&str, &str>) -> bool {
  match name {
    // Always emissive
    "glowstone" | "sea_lantern" | "beacon" | "shroomlight" | "jack_o_lantern"
    | "fire" | "soul_fire" | "magma_block" | "lava"
    | "ochre_froglight" | "verdant_froglight" | "pearlescent_froglight"
    | "torch" | "wall_torch" | "soul_torch" | "soul_wall_torch"
    | "lantern" | "soul_lantern" | "end_rod" | "conduit"
    | "crying_obsidian" | "glow_lichen" | "sculk_catalyst"
      => true,

    // Conditionally emissive (when lit=true)
    "redstone_lamp" | "furnace" | "blast_furnace" | "smoker"
    | "campfire" | "soul_campfire"
      => props.get("lit") == Some(&"true"),

    // Redstone torches are lit by default (lit=false when burnt out)
    "redstone_torch" | "redstone_wall_torch"
      => props.get("lit").copied().unwrap_or("true") == "true",

    // Cave vines emit when they have berries
    "cave_vines" | "cave_vines_plant"
      => props.get("berries") == Some(&"true"),

    _ => false,
  }
}

// ── Per-block-state voxelization ──────────────────────────────────────────────

/// Load a texture into the cache if not already present.
fn ensure_texture<'a>(name: &str, jar: &mut Jar, cache: &'a mut HashMap<String, RgbaImage>) -> Option<&'a RgbaImage> {
  if !cache.contains_key(name) {
    if let Ok(img) = load_texture(name, jar) { cache.insert(name.to_owned(), img); }
  }
  cache.get(name)
}

fn voxelize_block_state(key: &str, jar: &mut Jar, tex_cache: &mut HashMap<String, RgbaImage>) -> Result<VoxelGrid> {
  if matches!(key, "air" | "void_air" | "cave_air") {
    return Ok(VoxelGrid::default());
  }

  let (name, props) = blockstate::parse_key(key);

  // Fluids are procedural — no usable model in the JAR.
  if matches!(name, "water" | "lava") {
    let level: u32 = props.get("level").and_then(|v| v.parse().ok()).unwrap_or(0);
    let is_lava   = name == "lava";
    let still_key = if is_lava { "block/lava_still" } else { "block/water_still" };
    let flow_key  = if is_lava { "block/lava_flow"  } else { "block/water_flow" };
    ensure_texture(still_key, jar, tex_cache);
    ensure_texture(flow_key,  jar, tex_cache);
    let grid = match (tex_cache.get(still_key), tex_cache.get(flow_key)) {
      (Some(still), Some(flow)) => voxelize_fluid(is_lava, level, still, flow),
      _ => return Ok(VoxelGrid::default()),
    };
    return Ok(grid); // is_emissive already set by voxelize_fluid (lava = true)
  }

  let models = blockstate::find_model(name, &props, jar)?;
  if models.is_empty() { return Ok(VoxelGrid::default()); }

  let mut all_quads = Vec::new();
  for (model_name, bs_x_rot, bs_y_rot) in &models {
    if model_name.is_empty() { continue; }
    all_quads.extend(build_quads(model_name, jar, *bs_x_rot, *bs_y_rot, plains_tint(name))?);
  }
  if all_quads.is_empty() { return Ok(VoxelGrid::default()); }

  let needed: HashSet<&str> = all_quads.iter().map(|q| q.texture.as_str()).collect();
  for tex in needed {
    if !tex_cache.contains_key(tex) {
      if let Ok(img) = load_texture(tex, jar) { tex_cache.insert(tex.to_owned(), img); }
    }
  }

  let mut grid = voxelize(&all_quads, tex_cache);
  grid.is_emissive = is_emissive(name, &props);

  if props.get("waterlogged").copied() == Some("true") {
    ensure_texture("block/water_still", jar, tex_cache);
    ensure_texture("block/water_flow",  jar, tex_cache);
    if let (Some(still), Some(flow)) = (tex_cache.get("block/water_still"), tex_cache.get("block/water_flow")) {
      apply_waterlogging(&mut grid, still, flow);
    }
  }

  Ok(grid)
}
