// Voxelizer: for each quad, iterate only over voxels in its bounding box,
// run a SAT intersection test, and accumulate texture colors.
//
// Voxel (x, y, z) occupies AABB [x, x+1] × [y, y+1] × [z, z+1].
// Two accumulators are kept per voxel (tinted and untinted) so that overlay
// textures like grass_block's side grass are alpha-composited over the base
// rather than averaged with it.

use std::collections::HashMap;

use super::model::{FaceDir, Quad, sample_uv, unrotate_point};
use super::texture::{RgbaImage, Palette, apply_tint, sample_texture};

const WATER_TINT: [u8; 3] = [63, 118, 228]; // plains biome water color

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

// Returns true iff the quad strictly intersects the voxel interior (SAT test).
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
  let n  = cross(e0, e1);
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

// Project the voxel center onto the quad's element plane and sample the texture.
fn sample_quad_at_voxel(quad: &Quad, vox: [usize; 3], textures: &HashMap<String, RgbaImage>) -> Option<[u8; 4]> {
  let center = [vox[0] as f32 + 0.5, vox[1] as f32 + 0.5, vox[2] as f32 + 0.5];
  // Undo blockstate rotation: inverse Y-then-X = undo-X-then-undo-Y.
  let bs_origin = [8.0f32, 8.0, 8.0];
  let center = if quad.bs_x_rot != 0 {
    unrotate_point(center, bs_origin, "x", quad.bs_x_rot as f32, false)
  } else { center };
  let center = if quad.bs_y_rot != 0 {
    unrotate_point(center, bs_origin, "y", quad.bs_y_rot as f32, false)
  } else { center };
  let local = if let Some(rot) = &quad.elem_rot {
    unrotate_point(center, rot.origin, &rot.axis, rot.angle, rot.rescale)
  } else { center };

  // Reject voxel centers that project outside the element bounds in the face plane.
  // This discards edge voxels that technically intersect the quad's bounding box but
  // whose centers lie outside the element (e.g. chain or cross model edge voxels).
  let (ax1, ax2) = match quad.face_dir {
    FaceDir::North | FaceDir::South => (0, 1),
    FaceDir::East  | FaceDir::West  => (2, 1),
    FaceDir::Up    | FaceDir::Down  => (0, 2),
  };
  let (lo1, hi1) = (quad.elem_from[ax1].min(quad.elem_to[ax1]), quad.elem_from[ax1].max(quad.elem_to[ax1]));
  let (lo2, hi2) = (quad.elem_from[ax2].min(quad.elem_to[ax2]), quad.elem_from[ax2].max(quad.elem_to[ax2]));
  if local[ax1] < lo1 - 1e-4 || local[ax1] > hi1 + 1e-4
  || local[ax2] < lo2 - 1e-4 || local[ax2] > hi2 + 1e-4 {
    return None;
  }

  let [u, v] = sample_uv(local, quad.face_dir, quad.elem_from, quad.elem_to, quad.uv, quad.uv_rotation);
  let img = textures.get(&quad.texture)?;
  let mut rgba = sample_texture(img, u, v);
  if let Some(tint) = quad.tint_color { rgba = apply_tint(rgba, tint); }
  if rgba[3] == 0 { return None; }
  Some(rgba)
}

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

pub struct VoxelGrid {
  // 4096-bit geometry bitmask. Bit index = x + y*16 + z*256.
  pub bitmask: [u32; 128],
  // 64-bit coarse bitmask. One bit per 4×4×4 region.
  pub coarse: u64,
  // Palette-indexed color per solid voxel, in x+y*16+z*256 popcount order.
  pub color_indices: Vec<u8>,
  pub palette: Palette,
  // True if this block emits light (stored as bit 0 of the flags byte in materials.bin).
  pub is_emissive: bool,
}

impl Default for VoxelGrid {
  fn default() -> Self {
    VoxelGrid { bitmask: [0u32; 128], coarse: 0, color_indices: Vec::new(), palette: Palette::default(), is_emissive: false }
  }
}

// Voxelize a list of quads into a VoxelGrid.
//
// Maintains separate accumulators for tinted and untinted quads. When both
// contribute to a voxel (e.g. grass_block side: dirt base + tinted overlay),
// the tinted color is alpha-composited over the base rather than averaged.
pub fn voxelize(quads: &[Quad], textures: &HashMap<String, RgbaImage>) -> VoxelGrid {
  let mut base_acc = [[0u32; 5]; 4096]; // untinted: [r, g, b, a, count]
  let mut tint_acc = [[0u32; 5]; 4096]; // tinted:   [r, g, b, a, count]

  for quad in quads {
    let ([xlo, ylo, zlo], [xhi, yhi, zhi]) = quad_voxel_bbox(&quad.vertices);
    for z in zlo..=zhi {
      for y in ylo..=yhi {
        for x in xlo..=xhi {
          if !quad_aabb_intersects(&quad.vertices, [x, y, z]) { continue; }
          if let Some([r, g, b, a]) = sample_quad_at_voxel(quad, [x, y, z], textures) {
            let flat = x + y*16 + z*256;
            let acc = if quad.tint_color.is_some() { &mut tint_acc[flat] } else { &mut base_acc[flat] };
            acc[0] += r as u32; acc[1] += g as u32; acc[2] += b as u32; acc[3] += a as u32; acc[4] += 1;
          }
        }
      }
    }
  }

  let mut bitmask = [0u32; 128];
  let mut palette = Palette::default();
  let mut color_indices = Vec::new();

  for z in 0..16usize {
    for y in 0..16usize {
      for x in 0..16usize {
        let flat = x + y*16 + z*256;
        let tc = tint_acc[flat][4];
        let bc = base_acc[flat][4];
        if tc == 0 && bc == 0 { continue; }

        let rgba = if tc > 0 {
          // Tinted samples: alpha-composite over base (overlay semantics).
          let t = [tint_acc[flat][0]/tc, tint_acc[flat][1]/tc, tint_acc[flat][2]/tc, tint_acc[flat][3]/tc];
          if bc > 0 {
            let b = [base_acc[flat][0]/bc, base_acc[flat][1]/bc, base_acc[flat][2]/bc];
            let a = t[3];
            [
              ((t[0]*a + b[0]*(255-a)) / 255) as u8,
              ((t[1]*a + b[1]*(255-a)) / 255) as u8,
              ((t[2]*a + b[2]*(255-a)) / 255) as u8,
              255u8,
            ]
          } else {
            [t[0] as u8, t[1] as u8, t[2] as u8, t[3] as u8]
          }
        } else {
          let b = &base_acc[flat];
          [(b[0]/bc) as u8, (b[1]/bc) as u8, (b[2]/bc) as u8, (b[3]/bc) as u8]
        };

        bitmask[flat/32] |= 1 << (flat%32);
        color_indices.push(palette.get_or_insert(rgba));
      }
    }
  }

  let coarse = compute_coarse(&bitmask);
  VoxelGrid { bitmask, coarse, color_indices, palette, is_emissive: false }
}

// Voxelize a fluid (water or lava) block from its `level` property.
//
// Level 0 (source) and 8+ (falling) fill the full 16-voxel height.
// Levels 1–7 fill `(8 - level) * 2` voxels from the bottom.
// Top surface uses the still texture (XZ plane); sides use the flow texture.
// Water colors are tinted to plains biome water color.
pub fn voxelize_fluid(is_lava: bool, level: u32, still: &RgbaImage, flow: &RgbaImage) -> VoxelGrid {
  let height = if level == 0 || level >= 8 { 16usize } else { (8 - level) as usize * 2 };

  let mut bitmask = [0u32; 128];
  let mut palette = Palette::default();
  let mut color_indices = Vec::new();

  for z in 0..16usize {
    for y in 0..height {
      for x in 0..16usize {
        let flat = x + y*16 + z*256;
        bitmask[flat/32] |= 1 << (flat%32);

        let on_top  = y == height - 1;
        let on_side = x == 0 || x == 15 || z == 0 || z == 15;

        let mut color = if on_top || !on_side {
          sample_texture(still, x as f32 + 0.5, z as f32 + 0.5)
        } else {
          let (u, v) = if x == 0 || x == 15 { (z as f32 + 0.5, y as f32 + 0.5) }
                       else                  { (x as f32 + 0.5, y as f32 + 0.5) };
          sample_texture(flow, u, v)
        };

        if !is_lava { color = apply_tint(color, WATER_TINT); }
        color_indices.push(palette.get_or_insert(color));
      }
    }
  }

  let coarse = compute_coarse(&bitmask);
  VoxelGrid { bitmask, coarse, color_indices, palette, is_emissive: is_lava }
}

// Fill every empty voxel in `grid` with water. Used for waterlogged blocks.
// Side voxels (outer ring) use the flow texture; all others use the still texture.
// Colors are tinted to plains biome water color.
pub fn apply_waterlogging(grid: &mut VoxelGrid, still: &RgbaImage, flow: &RgbaImage) {
  let mut new_indices = Vec::with_capacity(4096);
  let mut old_iter = grid.color_indices.iter();

  for z in 0..16usize {
    for y in 0..16usize {
      for x in 0..16usize {
        let flat = x + y*16 + z*256;
        if (grid.bitmask[flat/32] >> (flat%32)) & 1 != 0 {
          new_indices.push(*old_iter.next().unwrap_or(&0));
        } else {
          let on_side = x == 0 || x == 15 || z == 0 || z == 15;
          let raw = if on_side {
            let (u, v) = if x == 0 || x == 15 { (z as f32 + 0.5, y as f32 + 0.5) }
                         else                  { (x as f32 + 0.5, y as f32 + 0.5) };
            sample_texture(flow, u, v)
          } else {
            sample_texture(still, x as f32 + 0.5, z as f32 + 0.5)
          };
          let color = apply_tint(raw, WATER_TINT);
          grid.bitmask[flat/32] |= 1 << (flat%32);
          new_indices.push(grid.palette.get_or_insert(color));
        }
      }
    }
  }

  grid.color_indices = new_indices;
  grid.coarse = compute_coarse(&grid.bitmask);
}

// Build the 64-bit coarse bitmask from a fine-grained 4096-bit bitmask.
// One bit per 4×4×4 sub-region; set if any voxel in that region is solid.
pub fn compute_coarse(bitmask: &[u32; 128]) -> u64 {
  let mut coarse = 0u64;
  for cz in 0..4usize {
    for cy in 0..4usize {
      for cx in 0..4usize {
        let bit = cx + cy*4 + cz*16;
        'outer: for dz in 0..4 {
          for dy in 0..4 {
            for dx in 0..4 {
              let flat = (cx*4+dx) + (cy*4+dy)*16 + (cz*4+dz)*256;
              if (bitmask[flat/32] >> (flat%32)) & 1 != 0 {
                coarse |= 1u64 << bit;
                break 'outer;
              }
            }
          }
        }
      }
    }
  }
  coarse
}
