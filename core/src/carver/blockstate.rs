// Blockstate JSON parsing and model resolution.
//
// Reads blockstate JSON files from the client JAR and resolves which model
// (with blockstate x/y rotation) applies for a given set of block properties.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use super::jar::Jar;

#[derive(Deserialize)]
#[serde(untagged)]
enum Apply {
  Single(Variant),
  List(Vec<Variant>),
}

#[derive(Deserialize)]
struct Variant {
  model: String,
  #[serde(default)] x: u32,
  #[serde(default)] y: u32,
}

#[derive(Deserialize)]
struct BlockstateJson {
  #[serde(default)] variants: HashMap<String, Apply>,
  #[serde(default)] multipart: Vec<MultipartEntry>,
}

#[derive(Deserialize)]
struct MultipartEntry {
  // Absent means "always apply". Present is either a flat condition map or
  // `{ "OR": [...] }` with a list of alternative condition maps.
  #[serde(default)]
  when: Option<Value>,
  apply: Apply,
}

// Read `block_states.bin` and return `(id, key)` pairs for every block state.
pub fn load_entries(path: &Path) -> Result<Vec<(u16, String)>> {
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

// Split `"block_name[prop=val,...]"` into `(name, {prop: val})`.
pub fn parse_key(key: &str) -> (&str, HashMap<&str, &str>) {
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

// Resolve all models `(name, x_rot, y_rot)` that apply for the given block name and properties.
//
// For variant blocks: returns the single best-matching variant (or empty).
// For multipart blocks (fences, walls, etc.): returns every entry whose `when`
// condition is absent or matches the given properties. This correctly assembles
// multi-part models (e.g. fence post + each connected arm).
pub fn find_model(short_name: &str, props: &HashMap<&str, &str>, jar: &mut Jar) -> Result<Vec<(String, u32, u32)>> {
  let bs_path = format!("assets/minecraft/blockstates/{short_name}.json");
  let bytes = match jar.get(&bs_path)? { Some(b) => b, None => return Ok(vec![]) };
  let bs: BlockstateJson = serde_json::from_slice(&bytes)
    .with_context(|| format!("parsing blockstate {bs_path}"))?;

  if !bs.variants.is_empty() {
    return Ok(find_variant(&bs.variants, props).into_iter().collect());
  }

  let models = bs.multipart.iter()
    .filter(|e| e.when.as_ref().map_or(true, |w| when_matches(w, props)))
    .map(|e| model_from_apply(&e.apply))
    .collect();
  Ok(models)
}

fn model_from_apply(apply: &Apply) -> (String, u32, u32) {
  match apply {
    Apply::Single(v) => (v.model.trim_start_matches("minecraft:").to_owned(), v.x, v.y),
    Apply::List(list) => list.first()
      .map(|v| (v.model.trim_start_matches("minecraft:").to_owned(), v.x, v.y))
      .unwrap_or_default(),
  }
}

// Returns true if the multipart `when` value matches the given block properties.
// Supports `{ "OR": [...] }` (any condition set matches) and plain condition maps
// (all conditions must match). Property values may be pipe-separated alternatives,
// e.g. `"north": "low|tall"` means north=low OR north=tall.
fn when_matches(when: &Value, props: &HashMap<&str, &str>) -> bool {
  if let Some(or) = when.get("OR") {
    or.as_array().map_or(false, |arr| arr.iter().any(|c| conditions_match(c, props)))
  } else {
    conditions_match(when, props)
  }
}

fn conditions_match(cond: &Value, props: &HashMap<&str, &str>) -> bool {
  cond.as_object().map_or(true, |obj| {
    obj.iter().all(|(k, v)| {
      let expected = v.as_str().unwrap_or("");
      expected.split('|').any(|opt| props.get(k.as_str()) == Some(&opt))
    })
  })
}

fn find_variant(variants: &HashMap<String, Apply>, props: &HashMap<&str, &str>) -> Option<(String, u32, u32)> {
  if variants.len() == 1 {
    return variants.values().next().map(model_from_apply);
  }

  let mut best: Option<&Apply> = None;
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

  best.map(model_from_apply)
}
