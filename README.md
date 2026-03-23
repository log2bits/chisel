# Chisel

A Minecraft Java Edition world renderer built in Rust with wgpu/WebGPU and WGSL. Chisel voxelizes every Minecraft block into a 16x16x16 sub-voxel brick, stores the world in a Sparse Voxel DAG (SVDAG) that encodes both geometry and block identity, and renders it via GPU ray casting with hard shadows. Targets native (Vulkan/Metal/DX12) and WebAssembly + WebGPU.

No game logic. No entity simulation. Renderer only.

---

## Goals

- Load any Minecraft Java Edition world from disk (all versions from 1.2.1+)
- Voxelize all block states into 16x16x16 bricks at build time
- Compress the world into a Sparse Voxel DAG encoding geometry and block IDs
- Render via GPU ray casting with primary rays and hard shadows
- Run natively and in the browser via WebAssembly + WebGPU
- Handle large worlds including multi-hundred-GB downloads

---

## Tech Stack

| Concern         | Choice                               |
|-----------------|--------------------------------------|
| Language        | Rust                                 |
| GPU API         | wgpu (WebGPU backend)                |
| Shader language | WGSL                                 |
| Build targets   | Native (Vulkan/Metal/DX12) + WASM    |
| World format    | Minecraft Java Edition (Anvil .mca)  |
| NBT parsing     | fastnbt crate (custom chunk decoder) |

---

## Architecture

The project is split into five modules. Each one does one job, and they chain together into a pipeline.

```
Minecraft world folder (.mca files)
        |
        v
  [ Reader ]
    Finds region files, decompresses chunks,
    pulls out block names + properties.
        |
        v
  [ Carver ]  (offline, run once)
    Loads Minecraft models + textures,
    voxelizes every block state into a
    16x16x16 brick with palette-compressed color.
        |
        v
  [ Packer ]  (offline, run once per world)
    Builds a bottom-up SVO per chunk,
    deduplicates globally into an SVDAG.
        |
        v
  [ Loader ]  (once at session start)
    Uploads SVDAG + material data to VRAM.
        |
        v
  [ Tracer ]  (every frame)
    Raymarches through the SVO on the GPU,
    handles lighting and shadows.
```

---

## Reader

The Reader takes a Minecraft world folder, finds the `.mca` region files, and pulls out block names and properties chunk by chunk.

### Why Not fastanvil

The fastanvil crate only supports 1.13 through ~1.20, has flaky 1.12 support, is pre-1.0 and unstable, and doesn't expose all chunk data. The 2b2t 1170 GB world download (a primary test target) was captured on Minecraft 1.12, which predates the Flattening. fastanvil can't reliably parse it.

Instead, Chisel uses fastnbt (the low-level NBT serde crate) directly and implements its own chunk decoder. This gives full control over every version's block format.

### Chunk Format Versions

Each chunk stores a `DataVersion` integer identifying the exact Minecraft build. The Reader branches on DataVersion to select the right block decoder:

```
DataVersion < 1444 (before 1.13, "pre-flattening"):
  Block data: flat Blocks byte array (8-bit block ID per voxel)
              plus optional Add nibble array (4-bit extension for IDs > 255)
              plus Data nibble array (4-bit metadata per voxel)
  ID mapping: numeric (block_id, metadata) -> modern BlockStateId
              via a static pre-flattening lookup table

DataVersion 1444 to 2724 (1.13 through 1.17):
  Block data: per-section palette (list of block state name strings)
              plus paletted BlockStates long array (variable bits per entry)
  Y range: 0 to 255 (16 sections of 16 blocks each)

DataVersion >= 2825 (1.18+):
  Block data: same palette + long array as 1.13-1.17
  Y range: -64 to 319 (24 sections, section Y index can be negative)
  Section Y stored explicitly in each section compound
```

The pre-flattening lookup table maps (block_id, metadata) pairs to their modern block state string. It's derived from Mojang's own pre-flattening data and community mappings.

### Block State ID Assignment

Regardless of input version, all blocks are normalized to a `BlockStateId` (u16) before anything else touches them. The assignment is stable across runs and derived from Mojang's `blocks.json` data generator output, which enumerates all 29,671 valid block states with canonical numeric IDs.

Measured from Minecraft 1.21.11: there are 29,671 total block states. A u16 (0-65535) is sufficient with headroom for future versions and mods.

`BlockStateId = 0` is reserved for air.

---

## Carver

The Carver takes a block state, loads the right Minecraft model and textures, and turns it into a 16x16x16 voxel grid with palette-compressed color. Every visually distinct block state, including every rotation, every connection variant, and every color variant, gets its own entry. There's no runtime rotation or tinting. The entry is the complete, final, renderable brick.

### Voxelization Technique

The Carver uses a quad-intersection approach. Here's how it works step by step.

**1. Build the color palette.** Read all the textures that a given block needs and collect every unique RGBA color across all of them. This becomes the block's palette. The max number of unique colors across all textures on a single block in Minecraft 1.21.11 is 219, well under 256, so an 8-bit palette index always works.

**2. Decompose the model into quads.** Parse the Minecraft model JSON and break it into individual quads. A simple full cube gives you 6 quads (one per face). More complicated models like stairs or fences give you more.

Minecraft models represent geometry as rectangular prisms ("elements" in the JSON). For each rectangular prism, the Carver generates 6 quads (one per face), then shifts each quad inward toward the center of the prism by 0.5 voxels. This gives quads a slight inward offset so they properly intersect the shell voxels of the prism rather than sitting exactly on the boundary.

Some models, like grass cross-planes, already have zero-thickness quads. These are used as-is with no inward shift.

**3. Test every voxel for intersections.** Loop through all 4096 voxels in the 16x16x16 grid. For each voxel, check if any quads pass through it. A quad must actually pass through the voxel's volume to count. A quad that merely touches a face or edge without entering the interior does not count.

**4. Sample and average colors.** For each intersecting quad, sample the texture color at the voxel's position by projecting the voxel center onto the quad and using the model's UV coordinates for the texture lookup. If multiple quads intersect the same voxel (which happens a lot, especially at corners and edges), average all the sampled colors together.

**5. Snap to palette.** Take the averaged color and find the nearest match in the block's original palette. That palette index becomes the voxel's material value.

This technique naturally produces mostly-hollow bricks. Interior voxels that no quad passes through remain empty. A full solid cube ends up with only its shell voxels filled (roughly 1,352 out of 4,096), which is the same result you'd get from an explicit hollowing pass, but it falls out of the algorithm for free.

### Brick Format

Each entry in the MaterialLookup table is one complete voxelized block state.

**Geometry bitmask:**
```
512 bytes flat (4096 bits, one bit per voxel)
Coarse bitmask: 8 bytes (64 bits, one bit per 4x4x4 sub-region)
Index: flat_idx = x + y*16 + z*256
Word:  bitmask[flat_idx / 32]
Bit:   (word >> (flat_idx % 32)) & 1
```

**Material storage:**

Material data is stored separately from geometry. Only solid (set) voxels have material entries. The material array for an entry is indexed via popcount on the geometry bitmask up to the target voxel.

Per-entry layout:
```
[1 u32]       meta: palette_count (8 bits) | solid_voxel_count (16 bits) | reserved (8 bits)
[N * 4 bytes] palette: RGBA entries, N = unique colors (1-256, always <= 219 in practice)
[M bytes]     indices: 8-bit palette index per solid voxel, packed
```

The 4th byte in each palette entry is alpha, so transparency info is stored per-color. Variable palette size means simple blocks (stone with ~8 colors) only pay for what they use.

### MaterialLookup Memory Estimate

```
29,671 entries worst case (one per block state)
Per entry average:
  Geometry bitmask:                512 bytes
  Coarse bitmask:                    8 bytes
  Palette (~20 colors * 4 RGBA):    80 bytes
  Material indices (~800 voxels):  800 bytes
  Meta + overhead:                   8 bytes
  Total per entry: ~1.4 KB

29,671 * 1.4 KB = ~41 MB average

Worst case (all entries fully solid, 256-color palette):
  (512 + 8 + 1024 + 1352 + 4) * 29,671 = ~86 MB

Realistic: ~40-55 MB
```

---

## Packer

The Packer takes the Reader's block positions and the Carver's bitmasks and compresses the whole world into an SVDAG. It processes one region file at a time, so peak memory stays bounded to the final DAG size plus one region's worth of working data.

### Construction

```
For each .mca region file:
  1. Decompress each chunk with fastnbt
  2. Decode block data according to DataVersion
  3. Map each block to BlockStateId (u16)
  4. For each chunk in the region:
     a. Build a flat 3D array of u16 leaf values
     b. Construct octree bottom-up:
        Leaf level: group 2x2x2 blocks -> create SvoNode
        Each parent level: group 2x2x2 children -> create SvoNode
        Before inserting any node, check global hash table for duplicates
        Reuse existing node ID if found, allocate new if not
     c. Bottom-up pass: compute face colors for each new inner node
        from children's cached face colors
     d. Store chunk root NodeId in chunk table
  5. Discard region working data, continue to next file
```

Deduplication key includes block state IDs. Two nodes are identical iff every field matches including all descendant block state IDs.

Reference: Kampe, Sintorn, Assarsson, "High Resolution Sparse Voxel DAGs" (ACM TOG 2013).

### Chunk Structure

The world is divided into chunks, each one the root of one independent octree. The chunk size is configurable at compile time for benchmarking.

```rust
// src/config.rs

// Octree depth
pub const OCTREE_DEPTH: usize = 6;
```

The idea is to benchmark at CHUNK_BLOCKS = 16, 32, 64, and 128 to find the best trade-off between traversal depth, deduplication ratio, and cache behavior.

Minecraft 1.18+ worlds span Y = -64 to 320 (384 blocks). The chunk grid covers this full range in vertical layers of CHUNK_BLOCKS height each.

### Why Geometry and Block IDs Live in the Same DAG

Most SVDAG papers (Dolonius 2017, Dado 2016) separate geometry from color because their scenes use continuous 24-bit RGB with ~16.7 million possible values. The color entropy is so high that including it in the DAG hash makes nearly every leaf unique, which destroys deduplication. Separation is the only way to recover compression.

Minecraft is different. Block state IDs have low entropy. The dominant underground block (stone) alone makes up 60-70% of non-air blocks in a typical generated world. Including the block state ID in the hash key doesn't harm deduplication. It actually helps it, because the most common subtrees are homogeneous regions of the same block type, and those deduplicate perfectly.

The combined DAG also gives you hierarchical dictionary compression for free:
```
All-stone 2x2x2 region -> one canonical node, shared everywhere
All-stone 4x4x4 region -> one canonical node pointing to above
All-stone 8x8x8 region -> one canonical node pointing to above
...and so on up the tree
```

This is strictly better than RLE because deduplication is global across the entire world, not local to a single scan line.

### SVDAG Inner Node Format

```rust
#[repr(C)]
pub struct SvoNode {
  pub first_child: u32,     // index into child_ids array
  pub child_mask:  u8,      // which of 8 octants are non-empty
  pub leaf_mask:   u8,      // which non-empty children are brick leaves
  pub flags:       u8,      // reserved
  pub _pad:        u8,
  pub face_colors: [u8; 18], // 6 * RGB888, one per axis face (+X -X +Y -Y +Z -Z)
  pub _pad2:       [u8; 2],
}
// Total: 28 bytes
```

`leaf_mask` is always a subset of `child_mask`.

Child slot interpretation:
```
child_mask[i] = 0                     -> octant i is empty air
child_mask[i] = 1, leaf_mask[i] = 1  -> octant i is a brick leaf
child_mask[i] = 1, leaf_mask[i] = 0  -> octant i is a branch node
```

Branch children are stored contiguously in a `child_ids` array. Leaf children are stored contiguously in a `leaf_data` array. Both are indexed via `first_child` + popcount offset.

```
branch_mask  = child_mask & !leaf_mask
branch range = child_ids[first_child .. first_child + popcount(branch_mask)]

leaf_mask_before_slot = leaf_mask & ((1 << slot) - 1)
leaf index            = leaf_data[first_leaf + popcount(leaf_mask_before_slot)]
```

### SVDAG Leaf Node Format

Each leaf is a single u16, a direct index into the MaterialLookup table.

```
block_state_id: u16 (0 to 65535)
```

No rotation bits. No tint index. No flags. Everything about how the block looks is already baked into the MaterialLookup entry at that index.

In the GPU buffer, leaves are packed two per u32:
```
leaf_data[i] = (block_state_id_1) | (block_state_id_2 << 16)
```

### Serialization

The Packer serializes the SVDAG and MaterialLookup to a `.chisel` file on disk. This is the offline build product that the Loader reads at runtime.

---

## Loader

The Loader takes the `.chisel` file and uploads everything into VRAM as GPU storage buffers. This happens once at session start.

### GPU Buffers

```
svo_nodes       : array<SvoNode>   // 28 bytes each (geometry + face colors)
child_ids       : array<u32>       // branch child node indices
leaf_data       : array<u32>       // u16 block_state_ids packed 2 per u32
chunk_roots     : array<ChunkRoot> // one per loaded chunk
block_geometry  : array<u32>       // 128+2 u32s per entry (512 byte bitmask + coarse)
block_material  : array<u32>       // variable layout per entry
block_offsets   : array<u32>       // per-entry byte offset into block_material
block_meta      : array<u32>       // per-entry flags
```

WGSL bindings:
```wgsl
@group(0) @binding(0) var<storage, read> svo_nodes:      array<SvoNode>;
@group(0) @binding(1) var<storage, read> child_ids:      array<u32>;
@group(0) @binding(2) var<storage, read> leaf_data:      array<u32>;
@group(0) @binding(3) var<storage, read> chunk_roots:    array<ChunkRoot>;
@group(0) @binding(4) var<storage, read> block_geometry: array<u32>;
@group(0) @binding(5) var<storage, read> block_material: array<u32>;
@group(0) @binding(6) var<storage, read> block_offsets:  array<u32>;
@group(0) @binding(7) var<storage, read> block_meta:     array<u32>;
```

### Memory Budget (approximate, 512x384x512 block region)

| Structure                        | Size estimate     |
|----------------------------------|-------------------|
| SVDAG nodes (post-DAG)           | 8-60 MB           |
| child_ids + leaf_data arrays     | 4-20 MB           |
| Block geometry bitmasks          | ~8 MB             |
| Block material data              | ~45 MB            |
| Block meta + offsets             | < 1 MB            |
| **Total**                        | **~65-135 MB**    |

The range is wide because compression ratio depends on world content. A generated vanilla world (vast stone, repeated biome patterns) compresses much more than a dense player-built structure.

For very large worlds, the top levels of the SVDAG stay permanently in VRAM while leaf-level chunks are streamed in and out of a GPU memory pool as the camera moves.

---

## Tracer

The Tracer raymarches through the SVDAG on the GPU and handles lighting and shadows. It runs a WGSL compute shader with one thread per pixel.

### Two-Level Traversal

Ray traversal operates at two levels with different algorithms.

**Level 1: Block-Level SVDAG.** Standard stackless SVO traversal (Laine & Karras 2010 style). Each step skips entire octree subtrees of empty or homogeneous space in O(log n). The DAG deduplication means repeated regions (underground stone, ocean water, identical builds) collapse to shared nodes.

A ray descends until it reaches a leaf node (a single block position), then enters Level 2.

**Level 2: Brick DDA.** The leaf stores a BlockStateId (u16) that indexes into the MaterialLookup. The lookup entry contains the pre-baked 16x16x16 geometry bitmask and material data, already oriented correctly. No runtime rotation needed.

A 3D DDA walks through the 16x16x16 bitmask (512 bytes, 8 cache lines). Once loaded, all geometry checks are free bitwise operations with no further memory accesses. A hierarchical 4x4x4 coarse bitmask sits alongside the full bitmask, letting the DDA skip empty 4x4x4 regions in a single step.

Sub-voxel hit -> fetch material from lookup -> return position, normal, color.

### Cone Ray LOD

Distant voxels that project to less than one pixel cause shimmer and aliasing. Chisel eliminates this by casting a cone instead of an infinitely thin ray. The cone's half-angle corresponds to exactly one pixel at the current FOV, so it widens linearly with distance.

At each inner node during SVDAG traversal, the renderer checks whether the node's bounding volume fits entirely within the cone. If it does, the node is sub-pixel and further descent would add nothing. The renderer reads the node's pre-cached face color for the entry face and returns immediately.

This is exact anti-aliasing: geometry is resolved at precisely the resolution the screen can represent.

**Per-node face color cache:**

Every inner node stores 6 precomputed average colors, one per axis-aligned face (+X, -X, +Y, -Y, +Z, -Z). These are computed offline during SVDAG construction, bottom-up.

The face color for a given direction is the average material color of all solid voxels in the subtree whose outward face on that side is exposed to air or to the boundary of the node's region. Interior voxels don't contribute.

Construction is purely bottom-up:
```
Leaf nodes (brick level):
  For each of the 6 faces:
    Average the material colors of solid voxels in the brick
    whose face on that side touches air or the brick boundary.
    Weight by solid voxel count contributing to that face.

Inner nodes at level L:
  For each of the 6 faces:
    Weighted average of the corresponding cached face color
    from each child node, weighted by the child's contributing
    solid voxel count on that face.
    (Uses only the children's cached face colors, no further descent.)
```

Face colors are 6 * RGB888 = 18 bytes per node. Because the DAG deduplicates identical subtrees, face colors for shared nodes are computed once and stored once.

**LOD threshold:**

```wgsl
fn node_subtends_less_than_one_pixel(
  node_half_size: f32,
  distance: f32,
  pixel_solid_angle: f32,
) -> bool {
  let projected = (2.0 * node_half_size / distance) / pixel_solid_angle;
  return projected < 1.0;
}
```

When this returns true, the renderer samples `face_colors[entry_face]` and terminates traversal. No Level 2 brick DDA is performed.

### Ray Casting Pseudocode

```wgsl
fn trace_ray(origin: vec3f, dir: vec3f, pixel_solid_angle: f32) -> HitResult {

  // Level 1: Block-level SVO-DAG traversal with cone LOD
  var node = root;
  loop {
    if node_subtends_less_than_one_pixel(node.half_size, distance(origin, node.center), pixel_solid_angle) {
      return lod_hit(node.face_colors[entry_face]);
    }

    if node.is_leaf {
      break;
    }

    node = next_child(node, ray);
    if miss { return miss(); }
  }

  // Level 2: Brick DDA
  let state_id   = leaf_data[leaf_index];
  let mat_offset = block_offsets[state_id];
  let meta       = block_meta[state_id];

  let sub_hit = brick_dda(block_local_ray, state_id, mat_offset);
  if !sub_hit.found { return miss(); }

  let color = fetch_material_color(state_id, sub_hit.voxel_index, mat_offset);

  return HitResult {
    pos:    block_pos + sub_hit.local_pos / 16.0,
    normal: sub_hit.normal,
    color:  color,
  };
}

fn brick_dda(ray: Ray, state_id: u32, mat_offset: u32) -> BrickHit {
  // Coarse bitmask pass: skip empty 4x4x4 regions in one step
  // Fine bitmask pass: standard 3D DDA, max 48 steps (16+16+16)
  // On hit: popcount(bitmask, 0..flat_idx) -> index into solid-only material array
  ...
}
```

### Lighting

Primary visibility plus hard shadow rays. Sun directional light with basic Lambertian shading.

---

## Future Work

These features are explicitly deferred. The data formats reserve space for them where possible, but nothing is implemented yet.

**Transparent block rendering.** Alpha values are stored in the palette (RGBA), but the ray caster currently treats all hits as opaque. Transparent rendering will require the ray to continue through transparent blocks and composite colors.

**LOD face color alpha.** Inner node face colors are currently RGB888. If subtrees contain transparent blocks, the LOD path would render distant glass as opaque. This needs alpha in the face color cache.

**Biome tinting.** Grass, leaves, and water change color per biome in Minecraft. The Carver currently bakes a single default color. Runtime tinting will need a mechanism to multiply the baked color by a biome-dependent tint, but the approach isn't decided yet.

**Emissive blocks.** Glowstone, lava, sea lanterns, etc. The node format has a flags byte with room for an emissive bit, but emissive lighting isn't implemented.

**Chunk streaming.** For worlds larger than VRAM, the top SVDAG levels stay resident while leaf-level chunks stream in and out as the camera moves.

---

## Data Flow: World Load to First Frame

```
1. Load config (chunk size, octree depth)

2. [OFFLINE, run once, cache result as .chisel file]

   a. Carver::bake(resource_pack_path)
      -> enumerate all 29,671 block states
      -> voxelize each into 16x16x16 bitmask via quad intersection
      -> compress material (variable RGBA palette, max 256, surface-only indices)
      -> assign each entry a stable BlockStateId (u16)
      -> serialize MaterialLookup to disk

   b. Packer::build(world_path)
      -> for each .mca region file, one at a time:
         -> Reader parses NBT with fastnbt
         -> Reader decodes blocks according to DataVersion
         -> map to BlockStateId
         -> build octree per chunk, deduplicate globally
         -> compute face colors bottom-up
      -> serialize SVDAG to disk

3. Load .chisel file

4. Loader::upload(dag, material_lookup, chunk_roots)
   -> write all buffers to GPU (once)

5. Tracer::render_frame(camera)
   -> compute pixel_solid_angle from fov and resolution
   -> dispatch primary ray compute shader (1 thread per pixel)
      -> cone LOD check at each node
      -> brick DDA for resolved geometry
   -> dispatch shadow ray pass
   -> present frame
```

---

## File Structure

```
chisel/
  Cargo.toml
  build.rs
  README.md

  assets/
    minecraft/            # extracted resource pack for the Carver

  shaders/
    common.wgsl           # shared structs, math utilities
    traverse.wgsl         # block-level SVO-DAG traversal with cone LOD
    brick_dda.wgsl        # sub-voxel brick DDA with coarse bitmask
    primary.wgsl          # primary ray dispatch + shading
    shadow.wgsl           # hard shadow ray pass
    debug.wgsl            # debug overlays (normals, DAG depth, LOD level, bitmask)

  src/
    main.rs               # entry point, wires modules together
    lib.rs
    config.rs             # chunk size, octree depth, brick size constants

    reader/
      mod.rs
      region.rs           # .mca file parsing, chunk extraction
      chunk.rs            # chunk NBT decoding (all DataVersion branches)
      block_state.rs      # block name + properties -> BlockStateId (u16)
      legacy.rs           # pre-1.13 numeric ID + metadata mapping

    carver/
      mod.rs
      model.rs            # Minecraft model JSON parsing, element decomposition
      texture.rs          # texture loading, palette extraction
      voxelizer.rs        # quad generation, intersection testing, color sampling
      brick.rs            # final 16x16x16 bitmask + material entry output

    packer/
      mod.rs
      octree.rs           # bottom-up SVO construction per chunk
      interner.rs         # hash-table DAG deduplication
      face_color.rs       # bottom-up face color computation for LOD
      serialize.rs        # .chisel file I/O

    loader/
      mod.rs
      buffers.rs          # buffer layout definitions
      upload.rs           # CPU -> GPU buffer upload

    tracer/
      mod.rs
      renderer.rs         # render loop, pass orchestration
      camera.rs           # camera, view/proj matrices, ray gen, pixel solid angle
      primary.rs          # primary ray compute pass
      shadow.rs           # shadow ray compute pass
      debug.rs            # debug overlay passes

    app.rs                # wgpu device setup, window, event loop
```

---

# Resources

## YouTube Channels

| Channel | Focus |
|---|---|
| [Douglas Dwyer](https://www.youtube.com/@DouglasDwyer) | Octo voxel engine in Rust + WebGPU, path-traced GI |
| [John Lin (Voxely)](https://www.youtube.com/@johnlin) | Path-traced voxel sandbox engine, RTX |
| [Gabe Rundlett](https://www.youtube.com/@GabeRundlett) | Open-source C++ voxel engine with Daxa/Vulkan |
| [Ethan Gore](https://www.youtube.com/@EthanGore) | Voxel engine dev, binary greedy meshing contributor |
| [VoxelRifts](https://www.youtube.com/@VoxelRifts) | Programming explainer videos, voxel focus |
| [SimonDev](https://www.youtube.com/@simondev758) | Accessible intro video on Radiance Cascades |

## Projects and Repos

| Project | Description |
|---|---|
| [VoxelRT](https://github.com/dubiousconst282/VoxelRT) | Voxel rendering experiments: brickmap, Tree64, XBrickMap, MultiDDA benchmarks |
| [HashDAG](https://github.com/Phyronnaz/HashDAG) | Official open-source implementation of the HashDAG paper (Careil et al. 2020) |
| [Voxelis](https://github.com/WildPixelGames/voxelis) | Pure Rust SVO-DAG crate with batching, reference counting, Bevy/Godot bindings |
| [Octo Engine](https://github.com/DouglasDwyer/octo-release) | Rust + WebGPU voxel engine with ray marching and path-traced GI |
| [BrickMap](https://github.com/stijnherfst/BrickMap) | High performance realtime CUDA voxel path tracer using brickmaps |
| [gvox_engine](https://github.com/GabeRundlett/gvox_engine) | Moddable cross-platform voxel engine in C++ with Daxa/Vulkan |
| [gvox](https://github.com/GabeRundlett/gvox) | General voxel format translation library |
| [VoxelHex](https://github.com/Ministry-of-Voxel-Affairs/VoxelHex) | Sparse VoxelBrick Tree with ray tracing support |
| [tree64](https://github.com/expenses/tree64) | Rust sparse 64-tree with hashing, based on dubiousconst282's guide |
| [binary-greedy-meshing](https://github.com/cgerikj/binary-greedy-meshing) | Fast bitwise voxel meshing, contributed to by Ethan Gore |

## Blog Posts

| Resource | Description |
|---|---|
| [A guide to fast voxel ray tracing using sparse 64-trees](https://dubiousconst282.github.io/2024/10/03/voxel-ray-tracing/) | Comprehensive guide: 64-tree traversal, brickmap comparison, benchmarks |
| [A Rundown on Brickmaps](https://uygarb.dev/posts/0003_brickmap_rundown/) | Clear explanation of the van Wingerden brickmap/brickgrid structure |
| [The Perfect Voxel Engine](https://voxely.net/blog/the-perfect-voxel-engine/) | John Lin's vision post on voxel engine architecture |
| [A Voxel Renderer for Learning C/C++](https://jacco.ompf2.com/2021/02/01/a-voxel-renderer-for-learning-c-c/) | Two-level grid renderer, solid color bricks, OpenCL, 1B rays/sec |
| [Voxel raytracing](https://tenebryo.github.io/posts/2021-01-13-voxel-raymarching.html) | SVDAG path tracer writeup inspired by John Lin |
| [Voxelisation Algorithms review](https://pmc.ncbi.nlm.nih.gov/articles/PMC8707769/) | Comprehensive survey of voxel data structures |
| [Voxel.Wiki: Raytracing](https://voxel.wiki/wiki/raytracing/) | Community wiki curating voxel raycasting resources and papers |
| [Amanatides & Woo DDA explainer](https://m4xc.dev/articles/amanatides-and-woo/) | Deep dive into the DDA algorithm with visuals |

## ShaderToy

| Shader | Description |
|---|---|
| [Radiance Cascades 3D (surface-based)](https://www.shadertoy.com/view/X3XfRM) | Surface-based 3D RC, 5 cascades, cubemap storage |
| [Radiance Cascades (volumetric voxel)](https://www.shadertoy.com/view/M3ycWt) | True volumetric 3D RC with voxel raycaster |
| [Amanatides & Woo DDA (branchless)](https://www.shadertoy.com/view/XdtcRM) | Clean branchless 3D DDA implementation |

## Papers

### Foundational Ray Traversal
| Paper | Link |
|---|---|
| A Fast Voxel Traversal Algorithm for Ray Tracing, Amanatides & Woo 1987 | [PDF](http://www.cse.yorku.ca/~amana/research/grid.pdf) |
| Efficient Sparse Voxel Octrees, Laine & Karras 2010 | [ResearchGate](https://www.researchgate.net/publication/47645140_Efficient_Sparse_Voxel_Octrees) |
| GigaVoxels: Ray-Guided Streaming for Efficient and Detailed Voxel Rendering, Crassin et al. 2009 | [INRIA](http://maverick.inria.fr/Publications/2009/CNLE09/) |
| Real-time Ray Tracing and Editing of Large Voxel Scenes (Brickmap), van Wingerden 2015 | [Utrecht](https://studenttheses.uu.nl/handle/20.500.12932/20460) |

### SVDAG Family
| Paper | Link |
|---|---|
| High Resolution Sparse Voxel DAGs, Kampe, Sintorn, Assarsson 2013 | [PDF](https://icg.gwu.edu/sites/g/files/zaxdzs6126/files/downloads/highResolutionSparseVoxelDAGs.pdf) |
| SSVDAGs: Symmetry-aware Sparse Voxel DAGs, Villanueva, Marton, Gobbetti 2016 | [ACM](https://dl.acm.org/doi/10.1145/2856400.2856406) |
| Interactively Modifying Compressed Sparse Voxel Representations (HashDAG), Careil, Billeter, Eisemann 2020 | [Wiley](https://onlinelibrary.wiley.com/doi/abs/10.1111/cgf.13916) |
| Lossy Geometry Compression for High Resolution Voxel Scenes, van der Laan et al. 2020 | [ACM](https://dl.acm.org/doi/10.1145/3384543) |
| Transform-Aware Sparse Voxel Directed Acyclic Graphs (TSVDAG), Molenaar & Eisemann 2025 | [ACM](https://dl.acm.org/doi/10.1145/3728301) |
| Editing Compact Voxel Representations on the GPU, Molenaar & Eisemann 2024 | [TU Delft](https://publications.graphics.tudelft.nl/papers/13) |
| Editing Compressed High-Resolution Voxel Scenes with Attributes, Molenaar & Eisemann 2023 | [Wiley](https://onlinelibrary.wiley.com/doi/full/10.1111/cgf.14757) |

### Color and Attribute Compression
| Paper | Link |
|---|---|
| Geometry and Attribute Compression for Voxel Scenes (Dado), Dado et al. 2016 | [CGF](https://diglib.eg.org/handle/10.1111/cgf.12841) |
| Compressing Color Data for Voxelized Surface Geometry (Dolonius), Dolonius et al. 2017 | [ACM I3D](https://dl.acm.org/doi/10.1145/3023368.3023381) |

### Surveys
| Paper | Link |
|---|---|
| Hybrid Voxel Formats for Efficient Ray Tracing, 2024 | [arxiv](https://arxiv.org/abs/2410.14128) |
| Aokana: A GPU-Driven Voxel Rendering Framework for Open World Games, 2025 | [arxiv](https://arxiv.org/abs/2505.02017) |
| Voxelisation Algorithms and Data Structures: A Review, PMC 2021 | [PMC](https://pmc.ncbi.nlm.nih.gov/articles/PMC8707769/) |

## Misc

| Resource | Description |
|---|---|
| [Voxel.Wiki](https://voxel.wiki) | Community wiki, good starting hub for voxel rendering resources |
| [Voxely.net blog](https://voxely.net/blog/) | John Lin's blog on voxel engine design |
| [Jacco's voxel blog series](https://jacco.ompf2.com) | Practical renderer tutorials with OpenCL |