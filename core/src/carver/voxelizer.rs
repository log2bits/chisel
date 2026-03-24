/// Voxelizer: for each quad, iterate only over voxels in its bounding box,
/// run a SAT intersection test, and accumulate texture colors.
///
/// Voxel (x, y, z) occupies AABB [x, x+1] × [y, y+1] × [z, z+1].
/// A quad marks a voxel solid if it strictly passes through its interior
/// (touching the boundary does not count). Color is sampled by projecting
/// the voxel center onto the quad's element plane and looking up the pixel.
/// Multiple quads hitting the same voxel have their colors averaged.

use std::collections::HashMap;

use super::model::{FaceDir, Quad, sample_uv, unrotate_point};
use super::texture::{RgbaImage, Palette, apply_tint, sample_texture};

// SAT intersection test (quad vs AABB)

/// Projects both shapes onto `axis` and checks for strict overlap (open intervals).
#[inline]
fn sat_overlap(pts_a: &[f32], pts_b: &[f32]) -> bool {
  let (min_a, max_a) = pts_a.iter().fold((f32::INFINITY, f32::NEG_INFINITY), |(mn,mx), &v| (mn.min(v), mx.max(v)));
  let (min_b, max_b) = pts_b.iter().fold((f32::INFINITY, f32::NEG_INFINITY), |(mn,mx), &v| (mn.min(v), mx.max(v)));
  min_a < max_b && min_b < max_a
}

#[inline] fn dot(v: [f32; 3], axis: [f32; 3]) -> f32 { v[0]*axis[0] + v[1]*axis[1] + v[2]*axis[2] }

fn project_quad(verts: &[[f32; 3]; 4], axis: [f32; 3]) -> [f32; 4] {
  [dot(verts[0], axis), dot(verts[1], axis), dot(verts[2], axis), dot(verts[3], axis)]
}

fn project_aabb(min: [f32; 3], max: [f32; 3], axis: [f32; 3]) -> [f32; 8] {
  let [ax, ay, az] = axis;
  [
    min[0]*ax + min[1]*ay + min[2]*az, max[0]*ax + min[1]*ay + min[2]*az,
    min[0]*ax + max[1]*ay + min[2]*az, max[0]*ax + max[1]*ay + min[2]*az,
    min[0]*ax + min[1]*ay + max[2]*az, max[0]*ax + min[1]*ay + max[2]*az,
    min[0]*ax + max[1]*ay + max[2]*az, max[0]*ax + max[1]*ay + max[2]*az,
  ]
}

#[inline] fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
  [a[1]*b[2]-a[2]*b[1], a[2]*b[0]-a[0]*b[2], a[0]*b[1]-a[1]*b[0]]
}
#[inline] fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] { [a[0]-b[0], a[1]-b[1], a[2]-b[2]] }

/// Full SAT test: returns true iff the quad strictly intersects the voxel interior.
pub fn quad_aabb_intersects(verts: &[[f32; 3]; 4], vox: [usize; 3]) -> bool {
  let min = [vox[0] as f32, vox[1] as f32, vox[2] as f32];
  let max = [min[0]+1.0, min[1]+1.0, min[2]+1.0];

  // 1. AABB face normals (world axes)
  for axis in [[1.0f32,0.0,0.0],[0.0,1.0,0.0],[0.0,0.0,1.0]] {
    if !sat_overlap(&project_quad(verts, axis), &project_aabb(min, max, axis)) { return false; }
  }

  // 2. Quad normal
  let e0 = sub(verts[1], verts[0]);
  let e1 = sub(verts[3], verts[0]);
  let n = cross(e0, e1);
  if n[0]*n[0] + n[1]*n[1] + n[2]*n[2] > 1e-12 {
    if !sat_overlap(&project_quad(verts, n), &project_aabb(min, max, n)) { return false; }
  }

  // 3. Edge × world-axis cross products (12 axes)
  let edges = [sub(verts[1],verts[0]), sub(verts[2],verts[1]), sub(verts[3],verts[2]), sub(verts[0],verts[3])];
  for edge in &edges {
    for world in [[1.0f32,0.0,0.0],[0.0,1.0,0.0],[0.0,0.0,1.0]] {
      let axis = cross(*edge, world);
      if axis[0]*axis[0] + axis[1]*axis[1] + axis[2]*axis[2] < 1e-12 { continue; }
      if !sat_overlap(&project_quad(verts, axis), &project_aabb(min, max, axis)) { return false; }
    }
  }

  true
}

// Color sampling

/// Project the voxel center onto the quad's element plane and sample the texture.
pub fn sample_quad_at_voxel(quad: &Quad, vox: [usize; 3], textures: &HashMap<String, RgbaImage>) -> Option<[u8; 4]> {
  let center = [vox[0] as f32 + 0.5, vox[1] as f32 + 0.5, vox[2] as f32 + 0.5];
  // Undo blockstate rotation (inverse of Y-then-X forward = undo-X-then-undo-Y).
  let bs_origin = [8.0f32, 8.0, 8.0];
  let center = if quad.bs_x_rot != 0 {
    unrotate_point(center, bs_origin, "x", quad.bs_x_rot as f32, false)
  } else { center };
  let center = if quad.bs_y_rot != 0 {
    unrotate_point(center, bs_origin, "y", quad.bs_y_rot as f32, false)
  } else { center };
  let local = if let Some(rot) = &quad.elem_rot {
    unrotate_point(center, rot.origin, &rot.axis, rot.angle, rot.rescale)
  } else {
    center
  };

  // For zero-thickness elements (flat decorative quads like the cross model),
  // the element's spatial extent (e.g. 0.8..15.2) doesn't align with the 16-voxel
  // grid, causing some pixels to be skipped or doubled.  Using the full block
  // extent [0, 16] maps each of the 16 voxels to exactly one texture pixel.
  let thin = match quad.face_dir {
    FaceDir::North | FaceDir::South => (quad.elem_to[2] - quad.elem_from[2]).abs() < 0.0001,
    FaceDir::East  | FaceDir::West  => (quad.elem_to[0] - quad.elem_from[0]).abs() < 0.0001,
    FaceDir::Up    | FaceDir::Down  => (quad.elem_to[1] - quad.elem_from[1]).abs() < 0.0001,
  };
  let (uv_from, uv_to) = if thin {
    ([0.0f32; 3], [16.0f32; 3])
  } else {
    (quad.elem_from, quad.elem_to)
  };

  let [u, v] = sample_uv(local, quad.face_dir, uv_from, uv_to, quad.uv, quad.uv_rotation);
  let img = textures.get(&quad.texture)?;
  let mut rgba = sample_texture(img, u, v);
  if quad.tint >= 0 { rgba = apply_tint(rgba, quad.tint); }
  if rgba[3] == 0 { return None; }
  Some(rgba)
}

// Bounding box

/// Compute the inclusive voxel bounding box of a quad from its (shifted+clipped) vertices.
fn quad_voxel_bbox(verts: &[[f32; 3]; 4]) -> ([usize; 3], [usize; 3]) {
  let mut mn = [f32::INFINITY; 3];
  let mut mx = [f32::NEG_INFINITY; 3];
  for v in verts {
    for i in 0..3 { mn[i] = mn[i].min(v[i]); mx[i] = mx[i].max(v[i]); }
  }
  let lo = mn.map(|f| (f.floor() as i32).clamp(0, 15) as usize);
  let hi = mx.map(|f| ((f.ceil() as i32).saturating_sub(1)).clamp(0, 15) as usize);
  (lo, hi)
}

// Main voxelization

pub struct VoxelGrid {
  /// 4096-bit geometry bitmask. Bit index = x + y*16 + z*256.
  pub bitmask: [u32; 128],
  /// 64-bit coarse bitmask. One bit per 4×4×4 region.
  pub coarse: u64,
  /// Palette-indexed color per solid voxel, in x+y*16+z*256 popcount order.
  pub color_indices: Vec<u8>,
  pub palette: Palette,
}

impl Default for VoxelGrid {
  fn default() -> Self {
    VoxelGrid { bitmask: [0u32; 128], coarse: 0, color_indices: Vec::new(), palette: Palette::default() }
  }
}

pub fn voxelize(quads: &[Quad], textures: &HashMap<String, RgbaImage>) -> VoxelGrid {
  // Per-voxel color accumulator: [r_sum, g_sum, b_sum, a_sum, count].
  let mut accum = [[0u32; 5]; 4096];

  // Quad-major: iterate only over voxels in each quad's bounding box.
  for quad in quads {
    let ([xlo, ylo, zlo], [xhi, yhi, zhi]) = quad_voxel_bbox(&quad.vertices);
    for z in zlo..=zhi {
      for y in ylo..=yhi {
        for x in xlo..=xhi {
          if !quad_aabb_intersects(&quad.vertices, [x, y, z]) { continue; }
          if let Some([r, g, b, a]) = sample_quad_at_voxel(quad, [x, y, z], textures) {
            let flat = x + y*16 + z*256;
            accum[flat][0] += r as u32;
            accum[flat][1] += g as u32;
            accum[flat][2] += b as u32;
            accum[flat][3] += a as u32;
            accum[flat][4] += 1;
          }
        }
      }
    }
  }

  // Build bitmask, palette, and color index list from accumulators.
  let mut bitmask = [0u32; 128];
  let mut palette = Palette::default();
  let mut color_indices = Vec::new();

  for z in 0..16usize {
    for y in 0..16usize {
      for x in 0..16usize {
        let flat = x + y*16 + z*256;
        let count = accum[flat][4];
        if count == 0 { continue; }
        let avg = [
          (accum[flat][0] / count) as u8,
          (accum[flat][1] / count) as u8,
          (accum[flat][2] / count) as u8,
          (accum[flat][3] / count) as u8,
        ];
        bitmask[flat/32] |= 1 << (flat%32);
        color_indices.push(palette.get_or_insert(avg));
      }
    }
  }

  // Build coarse bitmask: one bit per 4×4×4 region.
  let mut coarse = 0u64;
  for cz in 0..4usize {
    for cy in 0..4usize {
      for cx in 0..4usize {
        let coarse_bit = cx + cy*4 + cz*16;
        'outer: for dz in 0..4 {
          for dy in 0..4 {
            for dx in 0..4 {
              let flat = (cx*4+dx) + (cy*4+dy)*16 + (cz*4+dz)*256;
              if (bitmask[flat/32] >> (flat%32)) & 1 != 0 {
                coarse |= 1u64 << coarse_bit;
                break 'outer;
              }
            }
          }
        }
      }
    }
  }

  VoxelGrid { bitmask, coarse, color_indices, palette }
}
