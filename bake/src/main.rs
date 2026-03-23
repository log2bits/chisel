use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use serde::Deserialize;
use anyhow::Result;
use chisel_core::reader::block_state::{build_block_key, BlockStateTable};
use chisel_core::carver;

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