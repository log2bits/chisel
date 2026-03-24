use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use super::jar::Jar;

#[derive(Clone)]
pub struct RgbaImage {
  pub width: u32,
  pub height: u32,
  pub pixels: Vec<u8>, // RGBA8, row-major
}

impl RgbaImage {
  pub fn get_pixel(&self, x: u32, y: u32) -> [u8; 4] {
    let off = (y * self.width + x) as usize * 4;
    [self.pixels[off], self.pixels[off+1], self.pixels[off+2], self.pixels[off+3]]
  }
}

fn decode_png(bytes: &[u8]) -> Result<RgbaImage> {
  let decoder = png::Decoder::new(bytes);
  // Expand palette, strip 16-bit to 8-bit, expand grayscale to RGB.
  let mut decoder = decoder;
  decoder.set_transformations(
    png::Transformations::EXPAND
    | png::Transformations::STRIP_16
  );
  let mut reader = decoder.read_info()
    .context("png read_info failed")?;
  let mut buf = vec![0u8; reader.output_buffer_size()];
  let info = reader.next_frame(&mut buf)
    .context("png next_frame failed")?;

  let width = info.width;
  let height = info.height;
  let raw  = &buf[..info.buffer_size()];

  // After EXPAND + STRIP_16 + ALPHA the output is always RGBA8.
  // But double-check in case the crate version behaves differently.
  let pixels = match info.color_type {
    png::ColorType::Rgba => raw.to_vec(),
    png::ColorType::Rgb => raw.chunks_exact(3)
      .flat_map(|c| [c[0], c[1], c[2], 255u8])
      .collect(),
    png::ColorType::Grayscale => raw.iter()
      .flat_map(|&v| [v, v, v, 255u8])
      .collect(),
    png::ColorType::GrayscaleAlpha => raw.chunks_exact(2)
      .flat_map(|c| [c[0], c[0], c[0], c[1]])
      .collect(),
    png::ColorType::Indexed => bail!(
      "indexed PNG still present after EXPAND transform ({}x{})", width, height
    ),
  };

  Ok(RgbaImage { width, height, pixels })
}

pub fn texture_jar_path(name: &str) -> String {
  let name = name.trim_start_matches("minecraft:");
  format!("assets/minecraft/textures/{name}.png")
}

pub fn load_texture(texture_name: &str, jar: &mut Jar) -> Result<RgbaImage> {
  let path = texture_jar_path(texture_name);
  let bytes = jar.get_required(&path)
    .with_context(|| format!("JAR entry missing: {path}"))?;

  let mut img = decode_png(&bytes)
    .with_context(|| format!("failed to decode PNG: {path}"))?;

  // Animated textures: height > width → stacked frames, use first frame only.
  if img.height > img.width {
    let frame_h = img.width;
    img.pixels.truncate(frame_h as usize * img.width as usize * 4);
    img.height = frame_h;
  }

  Ok(img)
}

pub fn sample_texture(img: &RgbaImage, u: f32, v: f32) -> [u8; 4] {
  let px = ((u / 16.0 * img.width as f32).floor() as i32)
        .clamp(0, img.width as i32 - 1) as u32;
  let py = ((v / 16.0 * img.height as f32).floor() as i32)
        .clamp(0, img.height as i32 - 1) as u32;
  img.get_pixel(px, py)
}

pub fn apply_tint(rgba: [u8; 4], _tint: i32) -> [u8; 4] {
  let t = [0x91u8, 0xBDu8, 0x59u8];
  [
    ((rgba[0] as u32 * t[0] as u32) / 255) as u8,
    ((rgba[1] as u32 * t[1] as u32) / 255) as u8,
    ((rgba[2] as u32 * t[2] as u32) / 255) as u8,
    rgba[3],
  ]
}

#[derive(Default)]
pub struct Palette {
  pub colors: Vec<[u8; 4]>,
  index: HashMap<[u8; 4], u8>,
}

impl Palette {
  pub fn get_or_insert(&mut self, rgba: [u8; 4]) -> u8 {
    if let Some(&idx) = self.index.get(&rgba) {
      return idx;
    }
    if self.colors.len() < 256 {
      let idx = self.colors.len() as u8;
      self.colors.push(rgba);
      self.index.insert(rgba, idx);
      idx
    } else {
      self.nearest(rgba)
    }
  }

  pub fn nearest(&self, rgba: [u8; 4]) -> u8 {
    let mut best = 0u8;
    let mut best_dist = u64::MAX;
    for (i, c) in self.colors.iter().enumerate() {
      let d = (rgba[0] as i32 - c[0] as i32).pow(2) as u64
         + (rgba[1] as i32 - c[1] as i32).pow(2) as u64
         + (rgba[2] as i32 - c[2] as i32).pow(2) as u64;
      if d < best_dist { best_dist = d; best = i as u8; }
    }
    best
  }
}