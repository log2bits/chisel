use chisel_core::reader;
use std::path::Path;

fn main() {
  if let Err(e) = reader::open_world(Path::new("worlds/New World.zip")) {
    eprintln!("Error: {}", e);
    std::process::exit(1);
  }
}