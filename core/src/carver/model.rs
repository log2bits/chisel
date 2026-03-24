/// Minecraft block model parsing, parent-chain resolution, and quad generation.
///
/// The UV conventions and face-direction mappings here are taken directly from
/// Blockbench's `CubeFace.UVToLocal()` (js/outliner/cube.js), which is the
/// reference implementation for how Minecraft renders block models.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use super::jar::Jar;

// Raw JSON types

#[derive(Deserialize, Default)]
struct RawModel {
  #[serde(default)]
  parent: Option<String>,
  #[serde(default)]
  textures: HashMap<String, String>,
  #[serde(default)]
  elements: Option<Vec<RawElement>>,
}

#[derive(Deserialize)]
struct RawElement {
  from: [f32; 3],
  to: [f32; 3],
  #[serde(default)]
  rotation: Option<RawRotation>,
  faces: HashMap<String, RawFace>,
}

#[derive(Deserialize)]
struct RawRotation {
  origin: [f32; 3],
  axis: String,
  angle: f32,
  #[serde(default)]
  rescale: bool,
}

#[derive(Deserialize)]
struct RawFace {
  uv: Option<[f32; 4]>,
  texture: String,
  #[serde(default)]
  rotation: u32,
  tintindex: Option<i32>,
}

// Resolved types

/// A face direction, matching Minecraft / Blockbench conventions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaceDir { North, South, East, West, Up, Down }

impl FaceDir {
  fn from_str(s: &str) -> Option<Self> {
    match s {
      "north" => Some(Self::North),
      "south" => Some(Self::South),
      "east" => Some(Self::East),
      "west" => Some(Self::West),
      "up"  => Some(Self::Up),
      "down" => Some(Self::Down),
      _ => None,
    }
  }
}

/// A single quad ready for voxelization.
pub struct Quad {
  pub vertices: [[f32; 3]; 4],
  pub texture: String,
  pub uv: [f32; 4],
  pub uv_rotation: u32,
  pub face_dir: FaceDir,
  pub elem_from: [f32; 3],
  pub elem_to: [f32; 3],
  pub elem_rot: Option<ElemRotation>,
  pub tint: i32,
  /// Blockstate variant rotation (degrees, multiples of 90).
  /// Applied Y-axis first, then X-axis, around block center (8,8,8).
  pub bs_x_rot: u32,
  pub bs_y_rot: u32,
}

#[derive(Clone)]
pub struct ElemRotation {
  pub origin: [f32; 3],
  pub axis: String,
  pub angle: f32,
  pub rescale: bool,
}

// Model resolution

fn model_path(name: &str) -> String {
  let name = name.trim_start_matches("minecraft:");
  format!("assets/minecraft/models/{name}.json")
}

fn resolve_model(
  start: &str,
  jar: &mut Jar,
) -> Result<(HashMap<String, String>, Vec<RawElement>)> {
  let mut textures: HashMap<String, String> = HashMap::new();
  let mut elements: Option<Vec<RawElement>> = None;
  let mut current = start.to_owned();
  let mut depth = 0usize;

  loop {
    depth += 1;
    if depth > 64 { bail!("parent chain too deep starting from {start}"); }
    if current.contains("builtin") { break; }
    let path = model_path(&current);
    let bytes = match jar.get(&path)? { Some(b) => b, None => break };
    let raw: RawModel = serde_json::from_slice(&bytes)
      .with_context(|| format!("parsing model JSON at {path}"))?;
    for (k, v) in raw.textures { textures.entry(k).or_insert(v); }
    if elements.is_none() {
      if let Some(els) = raw.elements { elements = Some(els); }
    }
    match raw.parent { Some(p) => current = p, None => break }
  }

  for _ in 0..16 {
    let mut changed = false;
    let keys: Vec<String> = textures.keys().cloned().collect();
    for k in &keys {
      let v = textures[k].clone();
      if let Some(inner) = v.strip_prefix('#') {
        if let Some(resolved) = textures.get(inner).cloned() {
          if !resolved.starts_with('#') {
            textures.insert(k.clone(), resolved);
            changed = true;
          }
        }
      }
    }
    if !changed { break; }
  }

  Ok((textures, elements.unwrap_or_default()))
}

// 3D rotation helpers

fn rotate_point(p: [f32; 3], origin: [f32; 3], axis: &str, angle_deg: f32, rescale: bool) -> [f32; 3] {
  let rad = angle_deg.to_radians();
  let (sin, cos) = (rad.sin(), rad.cos());
  let [ox, oy, oz] = origin;
  let [px, py, pz] = [p[0] - ox, p[1] - oy, p[2] - oz];
  let (rx, ry, rz) = match axis {
    "x" => (px, py * cos - pz * sin, py * sin + pz * cos),
    "y" => (px * cos + pz * sin, py, -px * sin + pz * cos),
    "z" => (px * cos - py * sin, px * sin + py * cos, pz),
    _ => (px, py, pz),
  };
  let scale = if rescale { 1.0 / rad.abs().cos().max(1e-4) } else { 1.0 };
  [ox + rx * scale, oy + ry * scale, oz + rz * scale]
}

pub fn unrotate_point(p: [f32; 3], origin: [f32; 3], axis: &str, angle_deg: f32, rescale: bool) -> [f32; 3] {
  if !rescale {
    return rotate_point(p, origin, axis, -angle_deg, false);
  }
  // Forward: world = origin + R(angle) × (local − origin) / |cos(angle)|
  // Inverse: local = origin + |cos(angle)| × R(−angle) × (world − origin)
  let cos = angle_deg.to_radians().abs().cos().max(1e-4);
  let r = rotate_point(p, origin, axis, -angle_deg, false);
  let [ox, oy, oz] = origin;
  [ox + (r[0] - ox) * cos, oy + (r[1] - oy) * cos, oz + (r[2] - oz) * cos]
}

// Face vertices and inward shift

fn face_vertices(from: [f32; 3], to: [f32; 3], dir: FaceDir) -> [[f32; 3]; 4] {
  let [x0, y0, z0] = from;
  let [x1, y1, z1] = to;
  match dir {
    FaceDir::North => [[x1,y1,z0],[x0,y1,z0],[x0,y0,z0],[x1,y0,z0]],
    FaceDir::South => [[x0,y1,z1],[x1,y1,z1],[x1,y0,z1],[x0,y0,z1]],
    FaceDir::East => [[x1,y1,z0],[x1,y1,z1],[x1,y0,z1],[x1,y0,z0]],
    FaceDir::West => [[x0,y1,z1],[x0,y1,z0],[x0,y0,z0],[x0,y0,z1]],
    FaceDir::Up  => [[x0,y1,z0],[x1,y1,z0],[x1,y1,z1],[x0,y1,z1]],
    FaceDir::Down => [[x0,y0,z1],[x1,y0,z1],[x1,y0,z0],[x0,y0,z0]],
  }
}

fn shift_face_inward(vertices: &mut [[f32; 3]; 4], dir: FaceDir, from: [f32; 3], to: [f32; 3]) {
  let zero_thickness = match dir {
    FaceDir::North | FaceDir::South => (to[2] - from[2]).abs() < 0.0001,
    FaceDir::East | FaceDir::West => (to[0] - from[0]).abs() < 0.0001,
    FaceDir::Up  | FaceDir::Down => (to[1] - from[1]).abs() < 0.0001,
  };
  if zero_thickness { return; }

  // Step 1: shift the whole face 0.5 units inward along the face normal.
  let (normal_axis, normal_delta) = match dir {
    FaceDir::North => (2, 0.5f32),
    FaceDir::South => (2, -0.5),
    FaceDir::East => (0, -0.5),
    FaceDir::West => (0, 0.5),
    FaceDir::Up  => (1, -0.5),
    FaceDir::Down => (1, 0.5),
  };
  for v in vertices.iter_mut() {
    v[normal_axis] += normal_delta;
  }

  // Step 2: shrink the quad 0.5 units on each edge in the face plane.
  // Prevents adjacent shifted faces from overlapping at corners/edges.
  // UV sampling is unaffected because it uses elem_from/elem_to, not vertices.
  let (ax1, ax2) = match dir {
    FaceDir::North | FaceDir::South => (0, 1), // X, Y
    FaceDir::East | FaceDir::West => (2, 1), // Z, Y
    FaceDir::Up  | FaceDir::Down => (0, 2), // X, Z
  };
  for ax in [ax1, ax2] {
    let lo = vertices.iter().map(|v| v[ax]).fold(f32::INFINITY, f32::min);
    let hi = vertices.iter().map(|v| v[ax]).fold(f32::NEG_INFINITY, f32::max);
    if (hi - lo) < 0.0001 { continue; }
    let new_lo = lo + 0.5;
    let new_hi = hi - 0.5;
    for v in vertices.iter_mut() {
      v[ax] = if (v[ax] - lo).abs() < 0.001 { new_lo } else { new_hi };
    }
  }
}

// Public entry point

pub fn build_quads(model_name: &str, jar: &mut Jar, bs_x_rot: u32, bs_y_rot: u32) -> Result<Vec<Quad>> {
  let (textures, elements) = resolve_model(model_name, jar)?;
  let mut quads = Vec::new();

  for elem in &elements {
    let from = elem.from;
    let to = elem.to;
    let elem_rot = elem.rotation.as_ref().map(|r| ElemRotation {
      origin: r.origin,
      axis: r.axis.clone(),
      angle: r.angle,
      rescale: r.rescale,
    });

    for (face_name, raw_face) in &elem.faces {
      let dir = match FaceDir::from_str(face_name) {
        Some(d) => d,
        None => continue,
      };

      let tex_key = raw_face.texture.trim_start_matches('#');
      let texture = match textures.get(tex_key) {
        Some(t) if !t.starts_with('#') => t.trim_start_matches("minecraft:").to_owned(),
        _ => continue,
      };

      let uv = raw_face.uv.unwrap_or_else(|| default_uv(dir, from, to));

      let mut verts = face_vertices(from, to, dir);

      shift_face_inward(&mut verts, dir, from, to);

      if let Some(rot) = &elem.rotation {
        for v in verts.iter_mut() {
          *v = rotate_point(*v, rot.origin, &rot.axis, rot.angle, rot.rescale);
        }
      }

      // Zero-thickness quads at integer voxel boundaries fail the strict SAT test:
      // a plane at y=0 projects to [0,0] on Y; voxel 0 projects to [0,1]; 0<0 is false.
      // After rotation, if all vertices still share the same face-axis coordinate at an
      // integer position, nudge 0.5 into the nearest voxel so the SAT test can find it.
      // Cross model quads (rotated 45°) have varying face-axis coords → no nudge.
      let face_axis = match dir {
        FaceDir::North | FaceDir::South => 2,
        FaceDir::East  | FaceDir::West  => 0,
        FaceDir::Up    | FaceDir::Down  => 1,
      };
      let zero_thickness = match dir {
        FaceDir::North | FaceDir::South => (to[2] - from[2]).abs() < 0.0001,
        FaceDir::East  | FaceDir::West  => (to[0] - from[0]).abs() < 0.0001,
        FaceDir::Up    | FaceDir::Down  => (to[1] - from[1]).abs() < 0.0001,
      };
      if zero_thickness {
        let pos = verts[0][face_axis];
        if verts.iter().all(|v| (v[face_axis] - pos).abs() < 0.001)
           && (pos - pos.round()).abs() < 0.001
        {
          let voxel = ((pos - 0.5).floor() as i32).clamp(0, 15);
          let nudge = (voxel as f32 + 0.5) - pos;
          for v in verts.iter_mut() { v[face_axis] += nudge; }
        }
      }

      // Apply blockstate variant rotation around block center (8,8,8): Y axis first, then X.
      let bs_origin = [8.0f32, 8.0, 8.0];
      if bs_y_rot != 0 {
        for v in verts.iter_mut() { *v = rotate_point(*v, bs_origin, "y", bs_y_rot as f32, false); }
      }
      if bs_x_rot != 0 {
        for v in verts.iter_mut() { *v = rotate_point(*v, bs_origin, "x", bs_x_rot as f32, false); }
      }

      quads.push(Quad {
        vertices: verts,
        texture,
        uv,
        uv_rotation: raw_face.rotation,
        face_dir: dir,
        elem_from: from,
        elem_to: to,
        elem_rot: elem_rot.clone(),
        tint: raw_face.tintindex.unwrap_or(-1),
        bs_x_rot,
        bs_y_rot,
      });
    }
  }

  Ok(quads)
}

fn default_uv(dir: FaceDir, from: [f32; 3], to: [f32; 3]) -> [f32; 4] {
  let [x0, y0, z0] = from;
  let [x1, y1, z1] = to;
  match dir {
    FaceDir::North => [16.0-x1, 16.0-y1, 16.0-x0, 16.0-y0],
    FaceDir::South => [x0, 16.0-y1, x1, 16.0-y0],
    FaceDir::West => [z0, 16.0-y1, z1, 16.0-y0],
    FaceDir::East => [16.0-z1, 16.0-y1, 16.0-z0, 16.0-y0],
    FaceDir::Up  => [x0, z0, x1, z1],
    FaceDir::Down => [x0, 16.0-z1, x1, 16.0-z0],
  }
}

// UV sampling: 3D point on quad → UV coordinates

pub fn sample_uv(
  point: [f32; 3],
  dir: FaceDir,
  from: [f32; 3],
  to: [f32; 3],
  uv: [f32; 4],
  uv_rotation: u32,
) -> [f32; 2] {
  let [x, y, z] = point;
  let [x0, y0, z0] = from;
  let [x1, y1, z1] = to;

  let (lerp_x, lerp_y) = match dir {
    FaceDir::East => (inv_lerp(z1, z0, z), inv_lerp(y1, y0, y)),
    FaceDir::West => (inv_lerp(z0, z1, z), inv_lerp(y1, y0, y)),
    FaceDir::Up   => (inv_lerp(x0, x1, x), inv_lerp(z0, z1, z)),
    FaceDir::Down => (inv_lerp(x0, x1, x), inv_lerp(z1, z0, z)),
    FaceDir::South => (inv_lerp(x0, x1, x), inv_lerp(y1, y0, y)),
    FaceDir::North => (inv_lerp(x1, x0, x), inv_lerp(y1, y0, y)),
  };

  let inv_steps = (4 - (uv_rotation / 90) as usize) % 4;
  let (mut lx, mut ly) = (lerp_x, lerp_y);
  for _ in 0..inv_steps {
    let tmp = lx;
    lx = 1.0 - ly;
    ly = tmp;
  }

  let u = lerp(uv[0], uv[2], lx);
  let v = lerp(uv[1], uv[3], ly);
  [u.clamp(0.0, 16.0), v.clamp(0.0, 16.0)]
}

#[inline] fn lerp(a: f32, b: f32, t: f32) -> f32 { a + (b - a) * t }
#[inline] fn inv_lerp(a: f32, b: f32, v: f32) -> f32 {
  if (b - a).abs() < 1e-6 { 0.5 } else { (v - a) / (b - a) }
}