/// Voxelizer: tests each of 4096 voxels against quads, samples colors.
///
/// For each voxel (x, y, z) in 0..16 × 0..16 × 0..16:
/// - Voxel AABB is [x, x+1] × [y, y+1] × [z, z+1]
/// - Voxel center is (x+0.5, y+0.5, z+0.5)
/// - A quad marks the voxel solid if it strictly passes through the voxel interior
/// - Color is sampled by projecting the voxel center onto the quad plane
///  and looking up the UV coordinate

use std::collections::HashMap;

use super::model::{FaceDir, Quad, sample_uv, unrotate_point};
use super::texture::{RgbaImage, Palette, apply_tint, sample_texture};

// Quad-AABB intersection test

/// SAT separating-axis test: project both shapes onto axis, return true if they
/// strictly overlap (open intervals, not just touching).
#[inline]
fn sat_overlap(pts_a: &[f32], pts_b: &[f32]) -> bool {
  let (min_a, max_a) = pts_a.iter().fold((f32::INFINITY, f32::NEG_INFINITY), |(mn,mx), &v| (mn.min(v), mx.max(v)));
  let (min_b, max_b) = pts_b.iter().fold((f32::INFINITY, f32::NEG_INFINITY), |(mn,mx), &v| (mn.min(v), mx.max(v)));
  min_a < max_b && min_b < max_a // strict: touching edges don't count
}

/// Project a 3D point onto an axis (dot product).
#[inline]
fn dot(v: [f32; 3], axis: [f32; 3]) -> f32 {
  v[0]*axis[0] + v[1]*axis[1] + v[2]*axis[2]
}

/// Project all 4 quad vertices onto `axis`.
fn project_quad(verts: &[[f32; 3]; 4], axis: [f32; 3]) -> [f32; 4] {
  [dot(verts[0], axis), dot(verts[1], axis), dot(verts[2], axis), dot(verts[3], axis)]
}

/// Project all 8 AABB corners onto `axis`. AABB = [min, max] per component.
fn project_aabb(min: [f32; 3], max: [f32; 3], axis: [f32; 3]) -> [f32; 8] {
  let [ax, ay, az] = axis;
  let corners = [
    [min[0], min[1], min[2]],
    [max[0], min[1], min[2]],
    [min[0], max[1], min[2]],
    [max[0], max[1], min[2]],
    [min[0], min[1], max[2]],
    [max[0], min[1], max[2]],
    [min[0], max[1], max[2]],
    [max[0], max[1], max[2]],
  ];
  corners.map(|[x,y,z]| x*ax + y*ay + z*az)
}

/// Cross product.
#[inline]
fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
  [
    a[1]*b[2] - a[2]*b[1],
    a[2]*b[0] - a[0]*b[2],
    a[0]*b[1] - a[1]*b[0],
  ]
}

/// Subtract two 3D vectors.
#[inline]
fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
  [a[0]-b[0], a[1]-b[1], a[2]-b[2]]
}

/// Full SAT intersection test between a quad (convex polygon) and an AABB.
///
/// Returns true iff the quad STRICTLY intersects the interior of the AABB.
/// "Touching" along a face or edge (projection touching at a single point) returns false.
pub fn quad_aabb_intersects(verts: &[[f32; 3]; 4], vox: [usize; 3]) -> bool {
  let [vx, vy, vz] = vox;
  let min = [vx as f32, vy as f32, vz as f32];
  let max = [(vx+1) as f32, (vy+1) as f32, (vz+1) as f32];

  // === SAT axes ===

  // 1. AABB face normals (world axes)
  for axis in [[1.0f32,0.0,0.0],[0.0,1.0,0.0],[0.0,0.0,1.0]] {
    let qa = project_quad(verts, axis);
    let ba = project_aabb(min, max, axis);
    if !sat_overlap(&qa, &ba) { return false; }
  }

  // 2. Quad normal (cross of two edges)
  let e0 = sub(verts[1], verts[0]);
  let e1 = sub(verts[3], verts[0]);
  let quad_normal = cross(e0, e1);
  let len2 = quad_normal[0]*quad_normal[0] + quad_normal[1]*quad_normal[1] + quad_normal[2]*quad_normal[2];
  if len2 > 1e-12 {
    let qa = project_quad(verts, quad_normal);
    let ba = project_aabb(min, max, quad_normal);
    if !sat_overlap(&qa, &ba) { return false; }
  }

  // 3. Edge × AABB-axis cross products (12 axes)
  let quad_edges = [
    sub(verts[1], verts[0]),
    sub(verts[2], verts[1]),
    sub(verts[3], verts[2]),
    sub(verts[0], verts[3]),
  ];
  for edge in &quad_edges {
    for world_axis in [[1.0f32,0.0,0.0],[0.0,1.0,0.0],[0.0,0.0,1.0]] {
      let axis = cross(*edge, world_axis);
      let len2 = axis[0]*axis[0] + axis[1]*axis[1] + axis[2]*axis[2];
      if len2 < 1e-12 { continue; } // parallel → skip
      let qa = project_quad(verts, axis);
      let ba = project_aabb(min, max, axis);
      if !sat_overlap(&qa, &ba) { return false; }
    }
  }

  true
}

// Voxel color sampling

/// Project the voxel center onto the quad plane and sample the texture color.
///
/// Returns None if the quad is degenerate.
pub fn sample_quad_at_voxel(
  quad: &Quad,
  vox: [usize; 3],
  textures: &HashMap<String, RgbaImage>,
) -> Option<[u8; 4]> {
  // Voxel center in world space.
  let center = [vox[0] as f32 + 0.5, vox[1] as f32 + 0.5, vox[2] as f32 + 0.5];

  // If the element has a rotation, un-rotate the voxel center back into the
  // element's local space so the UV formula works correctly.
  let local_center = if let Some(rot) = &quad.elem_rot {
    unrotate_point(center, rot.origin, &rot.axis, rot.angle, rot.rescale)
  } else {
    center
  };

  let [u, v] = sample_uv(
    local_center,
    quad.face_dir,
    quad.elem_from,
    quad.elem_to,
    quad.uv,
    quad.uv_rotation,
  );

  let img = textures.get(&quad.texture)?;
  let mut rgba = sample_texture(img, u, v);

  if quad.tint >= 0 {
    rgba = apply_tint(rgba, quad.tint);
  }

  if rgba[3] == 0 { return None; } // fully transparent → don't count

  Some(rgba)
}

// Main voxelization loop

pub struct VoxelGrid {
  /// 4096-bit geometry bitmask. Bit flat_idx = x + y*16 + z*256.
  pub bitmask: [u32; 128],
  /// 64-bit coarse bitmask. One bit per 4×4×4 region.
  pub coarse: u64,
  /// Palette-indexed color per solid voxel (popcount order).
  pub color_indices: Vec<u8>,
  pub palette: Palette,
}

pub fn voxelize(quads: &[Quad], textures: &HashMap<String, RgbaImage>) -> VoxelGrid {
  let mut bitmask = [0u32; 128];
  let mut color_indices = Vec::new();
  let mut palette = Palette::default();

  for z in 0usize..16 {
    for y in 0usize..16 {
      for x in 0usize..16 {
        let vox = [x, y, z];

        // Collect colors from all quads that intersect this voxel.
        let mut r_acc = 0u32;
        let mut g_acc = 0u32;
        let mut b_acc = 0u32;
        let mut a_acc = 0u32;
        let mut count = 0u32;

        for quad in quads {
          if !quad_aabb_intersects(&quad.vertices, vox) {
            continue;
          }
          if let Some([r, g, b, a]) = sample_quad_at_voxel(quad, vox, textures) {
            r_acc += r as u32;
            g_acc += g as u32;
            b_acc += b as u32;
            a_acc += a as u32;
            count += 1;
          }
        }

        if count == 0 { continue; }

        let avg_rgba = [
          (r_acc / count) as u8,
          (g_acc / count) as u8,
          (b_acc / count) as u8,
          255u8, // always fully opaque for solid voxels
        ];

        // Mark voxel solid.
        let flat = x + y * 16 + z * 256;
        bitmask[flat / 32] |= 1 << (flat % 32);

        // Snap to palette.
        let idx = palette.get_or_insert(avg_rgba);
        color_indices.push(idx);
      }
    }
  }

  // Build coarse bitmask: one bit per 4×4×4 region.
  let mut coarse = 0u64;
  for cz in 0usize..4 {
    for cy in 0usize..4 {
      for cx in 0usize..4 {
        let coarse_bit = cx + cy * 4 + cz * 16;
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