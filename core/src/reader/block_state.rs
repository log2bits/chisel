use std::collections::{BTreeMap, HashMap};
use std::io::{BufWriter, Write};
use std::fs::File;
use anyhow::Result;

pub fn build_block_key(name: &str, properties: &BTreeMap<String, String>) -> String {
  if properties.is_empty() {
    name.to_string()
  } else {
    let props = properties
      .iter()
      .map(|(k, v)| format!("{}={}", k, v))
      .collect::<Vec<_>>()
      .join(",");
    format!("{}[{}]", name, props)
  }
}

pub struct BlockStateTable {
  map: HashMap<String, u16>,
}

impl BlockStateTable {
  pub fn load(data: &[u8]) -> Self {
    let magic = u32::from_le_bytes(data[0..4].try_into().unwrap());
    assert_eq!(magic, 0x42534944, "invalid block_states.bin magic number");
    let count = u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize;
    let mut map = HashMap::with_capacity(count);
    let mut cursor = 8;
    for _ in 0..count {
      let id = u16::from_le_bytes(data[cursor..cursor+2].try_into().unwrap());
      let len = u16::from_le_bytes(data[cursor+2..cursor+4].try_into().unwrap()) as usize;
      let key = std::str::from_utf8(&data[cursor+4..cursor+4+len]).unwrap().to_string();
      cursor += 4 + len;
      map.insert(key, id);
    }
    Self { map }
  }

  pub fn save(entries: &[(u16, String)]) -> Result<()> {
    let mut out = BufWriter::new(File::create("data/block_states.bin")?);
    out.write_all(&0x42534944u32.to_le_bytes())?;
    out.write_all(&(entries.len() as u32).to_le_bytes())?;
    for (id, key) in entries {
      out.write_all(&id.to_le_bytes())?;
      out.write_all(&(key.len() as u16).to_le_bytes())?;
      out.write_all(key.as_bytes())?;
    }
    println!("wrote {} block state entries to data/block_states.bin", entries.len());
    Ok(())
  }

  pub fn get(&self, name: &str, properties: &BTreeMap<String, String>) -> Option<u16> {
    let key = build_block_key(name, properties);
    let result = self.map.get(&key).copied();
    if result.is_none() {
      println!("Cant find block in lookup table: {}", key);
    }
    result
  }
}