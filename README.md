# Chisel

A Minecraft Java Edition world renderer built in Rust with wgpu/WebGPU and WGSL. Chisel voxelizes every Minecraft block into a 16x16x16 sub-voxel brick, stores the world in a Sparse Voxel DAG (SVDAG) that encodes both geometry and block identity, and renders it via GPU ray casting with hard shadows. Targets native (Vulkan/Metal/DX12) and WebAssembly + WebGPU.

No game logic. No entity simulation. Renderer only. Minecraft-specific by design.

---

## Goals

- Load any Minecraft Java Edition world from a zip file (all versions 1.13+, pre-1.13 planned)
- Voxelize all block states into 16x16x16 bricks offline
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
| World input     | Zipped world folders (.zip)          |

---

## Two Executables

Chisel is split into two separate programs with a `.chisel` file as the handoff between them.

**chisel-pack** runs offline on your machine. It takes a zipped Minecraft world and produces a `.chisel` file. This is where all the heavy CPU work happens: reading NBT, voxelizing blocks, building and deduplicating the SVDAG. It can be multithreaded as aggressively as you want since it's offline.

**chisel-render** takes a `.chisel` file and renders it. This is what runs in the browser via WASM, or natively. It doesn't know anything about Minecraft worlds. It just uploads the DAG to the GPU and traces rays.

So the workflow is:

```
[chisel-bake]   Minecraft jars -> block_states.bin + geometry.bin + materials.bin  (run once per MC version)
[chisel-pack]   world.zip + geometry.bin + materials.bin -> world.chisel            (run once per world)
[chisel-render] world.chisel -> frames                                              (runs in browser or native)
```

---

## Architecture

The pipeline inside chisel-pack:

```
world.zip
    |
    v
  [ Reader ]
    Finds region files inside the zip,
    decompresses chunks, decodes block
    names + properties, maps to u16 IDs.
    |
    v
  [ Packer ]
    Builds a bottom-up SVO per chunk,
    deduplicates globally into an SVDAG,
    serializes to .chisel.
```

The pipeline inside chisel-render:

```
world.chisel + geometry.bin + materials.bin
    |
    v
  [ Loader ]
    Uploads SVDAG + material data to VRAM.
    |
    v
  [ Tracer ]  (every frame)
    Raymarches through the SVO on the GPU,
    handles lighting and shadows.
```

The Carver runs inside chisel-bake, not as part of world packing:

```
Minecraft client jar
    |
    v
  [ Carver ]  (inside chisel-bake, run once per MC version)
    Loads models + textures from the client jar,
    voxelizes every block state into a 16x16x16 brick,
    writes geometry.bin + materials.bin.
```

---

## Static Data Files

Three binary files live in `data/` and are produced by `chisel-bake`. They're committed to the repo and only need to be regenerated when the Minecraft version changes.

### block_states.bin

Maps every block state string to a stable u16 ID. Used by the Reader at world-load time to convert chunk NBT strings like `"minecraft:grass_block[snowy=false]"` into numeric IDs. Once world loading is done this file can be freed from memory.

Format:
```
[4 bytes]  magic: 0x42534944 ("BSID")
[4 bytes]  u32 entry count
for each entry:
  [2 bytes]  u16 block state id
  [2 bytes]  u16 string length
  [n bytes]  utf8 key string
```

The key string format is `"block_name"` for blocks with no properties, or `"block_name[prop1=val1,prop2=val2]"` with properties sorted alphabetically. The `minecraft:` namespace prefix is stripped.

Measured from Minecraft 1.21.11: 29,671 total block states. A u16 is sufficient with headroom. `BlockStateId = 0` is reserved for air.

File size: ~2.4 MB.

### geometry.bin

Maps every u16 BlockStateId to a deduplicated voxel geometry shape. The geometry section is the hot path during ray traversal and is sized to fit in GPU L2 cache.

Bitmask shapes are deduplicated across all block states: 29,671 block states collapse to ~407 unique shapes. Many blocks share the same 16x16x16 geometry (e.g. all fully-solid blocks share one all-ones bitmask).

Format:
```
[4 bytes]             magic "GEOM"
[4 bytes]             u32 count (number of block state slots = max_id + 1)
[4 bytes]             u32 num_shapes (number of unique bitmask shapes)
[count * 2 bytes]     u16 bitmask_id per block state (index into shape table)
[num_shapes * 520 bytes]  shape table, each entry:
  [8 bytes]   coarse bitmask (u64, one bit per 4x4x4 sub-region)
  [512 bytes] geometry bitmask (4096 bits, one per voxel)
```

GPU lookup:
```
bitmask_id = bitmask_ids[block_state_id]        // ~58 KB, fits in L1
shape      = shape_table[bitmask_id]            // ~203 KB, fits in L2
coarse     = shape.coarse
bitmask    = shape.bitmask
```

Bitmask bit indexing:
```
flat_idx = x + y*16 + z*256
word:  bitmask[flat_idx / 32]
bit:   (word >> (flat_idx % 32)) & 1
```

Total size: ~261 KB.

### materials.bin

Maps every u16 BlockStateId to palette-compressed color data for its solid voxels. This is the cold path — accessed only when a ray hits a block. Geometry (bitmask/coarse) is in geometry.bin.

The IDs in `block_states.bin`, `geometry.bin`, and `materials.bin` are derived from the same `blocks.json` and always match as long as all three are generated together by `chisel-bake`.

Color payloads are deduplicated across all block states: 26,756 non-empty block states collapse to ~4,332 unique color payloads. Many block states share identical visual data despite having different behavioral properties (e.g. all redstone power levels, all note block instrument/note combinations, all waterlogged variants of the same block type). This mirrors the bitmask deduplication in geometry.bin.

Color_id 0 is a sentinel meaning "no color data" (empty/air block). Valid color_ids are 1..=num_payloads.

Format:
```
[4 bytes]              magic "MATL"
[4 bytes]              u32 count (number of block state slots = max_id + 1)
[4 bytes]              u32 num_payloads (number of unique color payloads)
[count * 2 bytes]      color_id per block state (u16, 0 = no color data)
[num_payloads * 4 bytes] payload byte offsets (u32, from start of payload data section)
[variable]             payload data, one entry per unique payload:
  [4 bytes]              meta: palette_count (8 bits) | solid_voxel_count (16 bits) | reserved (8 bits)
  [N * 4 bytes]          palette: RGBA8 entries, N = unique colors (1–256)
  [M bytes]              indices: 8-bit palette index per solid voxel, in popcount order
```

GPU lookup:
```
color_id      = color_ids[block_state_id]         // ~58 KB, fits in L1
payload_off   = payload_offsets[color_id - 1]     // ~17 KB for ~4K payloads
payload       = payload_data[payload_off..]        // palette + indices
```

Color indices are stored only for solid voxels. The index into the color array for a given voxel is computed via popcount on the geometry bitmask up to that voxel's position.

Measured from Minecraft 1.21.11:
```
26,756 non-empty block states
 4,332 unique color payloads (after deduplication)
17,705 unique RGBA colors across all voxelized blocks
Total payload data:    ~3.97 MB
Total materials.bin:   ~4.02 MB (was ~20.2 MB without deduplication)
Savings:               ~16.2 MB (80% reduction)
```

---

## chisel-bake

`chisel-bake` is the data preparation tool. Run it once when the Minecraft version changes. It requires the Minecraft server jar and client jar in `jars/`.

```
cargo run --bin chisel-bake
```

It does three things in order:

1. Runs the Minecraft data generator via subprocess (`java -DbundlerMainClass=net.minecraft.data.Main`) to produce `blocks.json`, then deletes the temp files.
2. Parses `blocks.json` and writes `data/block_states.bin`.
3. Runs the Carver against the client jar to produce `data/geometry.bin` and `data/materials.bin`.

All output goes to `data/`. All temp files are cleaned up automatically.

---

## Reader

The Reader lives in `chisel-core` and is called by `chisel-pack`. It takes a path to a zipped Minecraft world and produces decoded chunks: for each chunk, a list of sections each containing a flat `[u16; 4096]` of BlockStateIds.

### World Input

The Reader accepts zip files containing Minecraft worlds in any of these layouts:

- `WorldName/region/r.X.Z.mca` (zipped folder)
- `region/r.X.Z.mca` (zipped contents)
- `r.X.Z.mca` (just the region files)

It only reads overworld region files. It identifies them by finding `.mca` files whose immediate parent directory is named `"region"`, or `.mca` files at the root of the zip with no parent. `entities/` and `poi/` directories are skipped. Nether and End dimensions are skipped for now.

### Why Not fastanvil

The fastanvil crate only supports 1.13 through ~1.20, has flaky 1.12 support, is pre-1.0 and unstable, and doesn't expose all chunk data. The 2b2t 1170 GB world download is on Minecraft 1.12, which predates the Flattening. fastanvil can't reliably parse it.

Chisel uses fastnbt (the low-level NBT serde crate) directly and implements its own chunk decoder. This gives full control over every version's block format.

### Chunk Format Versions

Each chunk stores a `DataVersion` integer. The Reader branches on it to pick the right decoder:

```
DataVersion < 1444 (before 1.13, "pre-flattening"):
  Block data: flat Blocks byte array (8-bit block ID per voxel)
              plus optional Add nibble array (4-bit extension for IDs > 255)
              plus Data nibble array (4-bit metadata per voxel)
  ID mapping: numeric (block_id, metadata) -> modern block state string
              via a static pre-flattening lookup table in legacy.rs
  Status: TODO

DataVersion 1444 to 2563 (1.13 through 1.15):
  Block data: per-section palette (list of block state name strings + properties)
              plus paletted BlockStates long array
  Packing: indices CAN span across two longs (cross-long packing)
  Y range: 0 to 255 (16 sections)

DataVersion 2564+ (1.16+):
  Block data: same palette format as 1.13-1.15
  Packing: indices never span longs, padding bits are wasted
  Y range: 0 to 255 for 1.16-1.17, -64 to 319 for 1.18+ (24 sections)
  Section Y stored explicitly in each section compound
```

### Block State ID Assignment

Regardless of input version, all blocks are normalized to a `BlockStateId` (u16) by looking them up in a `BlockStateTable` loaded from `data/block_states.bin`. The table maps canonical key strings to IDs.

Key string format: `"stone"` for blocks with no properties, `"acacia_button[face=floor,facing=north,powered=true]"` with properties sorted alphabetically. The `minecraft:` namespace prefix is stripped.

The `build_block_key(name, properties)` function in `block_state.rs` is public and used by both the Reader and `chisel-bake` to build these strings consistently.

---

## Carver

The Carver runs inside `chisel-bake`. It takes the Minecraft client jar, reads model JSON and texture files directly out of it, and voxelizes every block state into a 16x16x16 brick.

### Voxelization Technique

The Carver uses a quad-intersection approach.

**1. Build the color palette.** Read all the textures the block needs and collect every unique RGBA color across all of them. This becomes the block's palette. The max number of unique colors across all textures on a single block in Minecraft 1.21.11 is 219, so an 8-bit palette index always works.

**2. Resolve the model.** Parse the blockstate JSON to find which model file applies. Follow the parent chain (most models inherit from a parent like `block/cube_all`) until you have a complete set of elements with all texture variables resolved. Texture variables like `#side` get substituted through the model's `textures` map.

**3. Decompose into quads.** Each element in the model JSON is a rectangular prism with a `from` and `to` in 0–16 unit space. Generate up to 6 quads per prism (one per face). For volumetric faces, shift each quad 0.5 units inward toward the prism center and trim 0.5 units from all four edges in the face plane, so the quad strictly intersects the shell voxels' interiors rather than touching boundaries. Zero-thickness quads (like the grass cross-plane) skip the shift and trim. Flat ground quads sitting exactly on an integer voxel boundary (like rails at y=1) are nudged 0.5 units into the nearest voxel so the intersection test can find them.

**4. Intersect quad-major.** For each quad, compute its voxel bounding box and test only the voxels within it using a full SAT (Separating Axis Theorem) intersection test. Strict intersection only — a quad touching a voxel face or edge without entering its interior doesn't count. This visits far fewer voxels than looping all 4096 for every quad.

**5. Sample and average colors.** For each intersecting quad, project the voxel center onto the quad's element plane (applying any element rotation in reverse), use the model's UV coordinates to look up the nearest texture pixel, and average all sampled RGBA values if multiple quads hit the same voxel. Transparent pixels (alpha = 0) are skipped entirely.

**6. Snap to palette.** Find the nearest palette color to the averaged RGBA result. That index becomes the voxel's material value.

Interior voxels that no quad passes through stay empty. A full solid cube ends up with roughly 1,352 solid voxels out of 4,096, which falls out of the algorithm for free.

---

## Packer

The Packer takes the Reader's BlockStateId arrays and the material lookup from `materials.bin` and compresses the whole world into an SVDAG. It processes one region file at a time so peak memory stays bounded.

### Construction

```
For each .mca region file:
  1. Decompress each chunk with fastnbt
  2. Decode block data according to DataVersion
  3. Map each block to BlockStateId (u16) via BlockStateTable
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

The world is divided into chunks, each one the root of one independent octree. Chunk size is set by the octree depth, currently depth 6 = 64x64x64 blocks per chunk.

Minecraft 1.18+ worlds span Y = -64 to 320 (384 blocks). The chunk grid covers this full vertical range.

### Why Geometry and Block IDs Live in the Same DAG

Most SVDAG papers separate geometry from color because continuous 24-bit RGB has so much entropy that including color in the hash key makes every leaf unique and destroys deduplication.

Minecraft is different. Block state IDs have low entropy. Stone alone makes up 60-70% of non-air blocks underground. Including the block state ID in the hash key doesn't hurt deduplication. It actually helps, because the most common subtrees are homogeneous regions of the same block and those deduplicate perfectly.

The combined DAG also gives you hierarchical dictionary compression for free:
```
All-stone 2x2x2 region -> one canonical node, shared everywhere
All-stone 4x4x4 region -> one canonical node pointing to above
All-stone 8x8x8 region -> one canonical node pointing to above
...and so on up the tree
```

This is better than RLE because deduplication is global across the entire world, not local to a single scan line.

### SVDAG Inner Node Format

```rust
#[repr(C)]
pub struct SvoNode {
  pub first_child: u32,      // index into child_ids array
  pub child_mask:  u8,       // which of 8 octants are non-empty
  pub leaf_mask:   u8,       // which non-empty children are brick leaves
  pub flags:       u8,       // reserved
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

Each leaf is a single u16, a direct index into the material lookup.

```
block_state_id: u16 (0 to 65535)
```

No rotation bits. No tint index. No flags. Everything about how the block looks is already baked into the material entry at that index.

In the GPU buffer, leaves are packed two per u32:
```
leaf_data[i] = (block_state_id_1) | (block_state_id_2 << 16)
```

---

## .chisel File Format

The `.chisel` file is the handoff between `chisel-pack` and `chisel-render`. It contains the full SVDAG and chunk root table. It does not contain material data; that's loaded separately from `materials.bin`.

Format TBD as the Packer is implemented.

---

## Loader

The Loader takes the `.chisel` file and `materials.bin` and uploads everything into VRAM as GPU storage buffers. This happens once at session start.

### GPU Buffers

```
svo_nodes        : array<SvoNode>   // 28 bytes each (geometry + face colors)
child_ids        : array<u32>       // branch child node indices
leaf_data        : array<u32>       // u16 block_state_ids packed 2 per u32
chunk_roots      : array<ChunkRoot> // one per loaded chunk
bitmask_ids      : array<u16>       // shape index per block state (~58 KB, fits in L1)
shape_table      : array<u32>       // unique bitmask shapes: coarse(2u32) + bitmask(128u32) each (~203 KB, fits in L2)
color_ids        : array<u16>       // payload index per block state (~58 KB, fits in L1)
payload_offsets  : array<u32>       // byte offset into payload_data per unique payload (~17 KB)
payload_data     : array<u32>       // deduplicated palette + indices (~4 MB, cold path)
```

WGSL bindings:
```wgsl
@group(0) @binding(0) var<storage, read> svo_nodes:       array<SvoNode>;
@group(0) @binding(1) var<storage, read> child_ids:       array<u32>;
@group(0) @binding(2) var<storage, read> leaf_data:       array<u32>;
@group(0) @binding(3) var<storage, read> chunk_roots:     array<ChunkRoot>;
@group(0) @binding(4) var<storage, read> bitmask_ids:     array<u32>; // u16 packed
@group(0) @binding(5) var<storage, read> shape_table:     array<u32>;
@group(0) @binding(6) var<storage, read> color_ids:       array<u32>; // u16 packed
@group(0) @binding(7) var<storage, read> payload_offsets: array<u32>;
@group(0) @binding(8) var<storage, read> payload_data:    array<u32>;
```

### Memory Budget (approximate, 512x384x512 block region)

| Structure                               | Size estimate  |
|-----------------------------------------|----------------|
| SVDAG nodes (post-DAG)                  | 8–60 MB        |
| child_ids + leaf_data                   | 4–20 MB        |
| bitmask_ids + shape_table               | ~261 KB        |
| color_ids + payload_offsets + payloads  | ~4 MB          |
| **Total**                               | **~16–84 MB**  |

The range is wide because compression ratio depends heavily on world content. A generated vanilla world compresses much more than a dense player-built structure.

---

## Tracer

The Tracer raymarches through the SVDAG on the GPU and handles lighting and shadows. It runs a WGSL compute shader with one thread per pixel.

### Two-Level Traversal

**Level 1: Block-level SVDAG.** Standard stackless SVO traversal. Each step skips entire octree subtrees of empty or homogeneous space in O(log n). The DAG deduplication means repeated regions collapse to shared nodes, so traversal is fast even in huge worlds.

A ray descends until it reaches a leaf node (a single block position), then enters Level 2.

**Level 2: Brick DDA.** The leaf stores a BlockStateId (u16) that indexes into the material lookup. The lookup entry contains the pre-baked 16x16x16 geometry bitmask and material data, already oriented correctly. No runtime rotation needed.

A 3D DDA walks through the 16x16x16 bitmask (512 bytes). Once loaded, all geometry checks are free bitwise operations with no further memory accesses. A hierarchical 4x4x4 coarse bitmask lets the DDA skip empty 4x4x4 regions in a single step.

### Cone Ray LOD

Distant voxels that project to less than one pixel cause shimmer and aliasing. Chisel eliminates this by casting a cone instead of an infinitely thin ray. The cone's half-angle corresponds to exactly one pixel at the current FOV.

At each inner node during SVDAG traversal, the renderer checks whether the node's bounding volume fits entirely within the cone. If it does, the node is sub-pixel and the renderer reads the node's pre-cached face color for the entry face and returns immediately. No brick DDA needed.

Every inner node stores 6 precomputed average colors, one per axis-aligned face (+X, -X, +Y, -Y, +Z, -Z). These are computed offline during SVDAG construction, bottom-up from the leaves.

LOD threshold check:
```wgsl
fn node_subtends_less_than_one_pixel(node_half_size: f32, distance: f32, pixel_solid_angle: f32) -> bool {
  let projected = (2.0 * node_half_size / distance) / pixel_solid_angle;
  return projected < 1.0;
}
```

### Lighting

Primary visibility plus hard shadow rays. Sun directional light with basic Lambertian shading.

---

## Future Work

**Pre-1.13 world support.** The Reader currently handles 1.13+. Pre-flattening worlds (1.12 and earlier) need a static (block_id, metadata) -> modern block state string lookup table in `legacy.rs`.

**Transparent block rendering.** Alpha is sampled from textures and averaged per voxel just like RGB. The ray caster currently treats all hits as opaque and needs to be updated to use the alpha channel.

**Biome tinting.** Grass, leaves, and water change color per biome. The Carver currently bakes a single default color.

**Emissive blocks.** Glowstone, lava, sea lanterns, etc. The node format has a flags byte reserved for this.

**Chunk streaming.** For worlds larger than VRAM, top SVDAG levels stay resident while leaf chunks stream in and out as the camera moves.

**Multithreading in chisel-pack.** The Reader and Packer both process region files independently. Parallelism with rayon on the native build path is planned but not yet implemented. (chisel-bake's Carver already uses rayon with per-thread JAR handles and texture caches.)

---

## File Structure

```
chisel/
  Cargo.toml              # workspace root

  data/
    block_states.bin      # generated by chisel-bake, committed to repo
    geometry.bin          # generated by chisel-bake, committed to repo
    materials.bin         # generated by chisel-bake, committed to repo

  jars/                   # gitignored
    server.jar            # used by chisel-bake to generate blocks.json
    client.jar            # used by chisel-bake (Carver reads models + textures)

  worlds/                 # gitignored
    *.zip                 # zipped Minecraft world folders

  core/
    Cargo.toml
    src/
      lib.rs
      config.rs           # octree depth, brick size constants

      reader/
        mod.rs            # open_world entry point, zip traversal
        region.rs         # .mca file parsing
        chunk.rs          # chunk location table, decompression, chunk NBT decoding (all DataVersion branches)
        block_states.rs   # BlockStateTable, build_block_key, load from block_states.bin
        legacy.rs         # pre-1.13 numeric ID + metadata mapping (TODO)

      carver/
        mod.rs            # generate_materials entry point; writes geometry.bin + materials.bin
        jar.rs            # ZIP/JAR reader with entry caching
        model.rs          # model JSON parsing, parent chain resolution, quad generation
        texture.rs        # texture loading, per-brick palette, color sampling
        voxelizer.rs      # quad-major SAT intersection, color accumulation, VoxelGrid output
        brick.rs          # serialize color data (palette + indices) for materials.bin

      packer/
        mod.rs
        octree.rs         # bottom-up SVO construction per chunk
        interner.rs       # hash-table DAG deduplication
        face_color.rs     # bottom-up face color computation for LOD
        serialize.rs      # .chisel file I/O

      loader/
        mod.rs
        buffers.rs        # buffer layout definitions
        upload.rs         # CPU -> GPU buffer upload

      tracer/
        mod.rs
        renderer.rs       # render loop, pass orchestration
        camera.rs         # camera, view/proj matrices, ray gen, pixel solid angle
        primary.rs        # primary ray compute pass
        shadow.rs         # shadow ray compute pass
        debug.rs          # debug overlay passes

  pack/
    Cargo.toml
    src/
      main.rs             # chisel-pack entry point (world.zip -> world.chisel)

  render/
    Cargo.toml
    src/
      main.rs             # chisel-render native entry point
      lib.rs              # chisel-render WASM entry point

  bake/
    Cargo.toml
    src/
      main.rs             # chisel-bake entry point (jars -> block_states.bin + geometry.bin + materials.bin)
                          # also: --export-vox "block_state_key" reads from data/*.bin to test end-to-end

  shaders/
    common.wgsl           # shared structs, math utilities
    traverse.wgsl         # block-level SVO-DAG traversal with cone LOD
    brick_dda.wgsl        # sub-voxel brick DDA with coarse bitmask
    primary.wgsl          # primary ray dispatch + shading
    shadow.wgsl           # hard shadow ray pass
    debug.wgsl            # debug overlays (normals, DAG depth, LOD level, bitmask)
```

---

## Resources

### YouTube Channels

| Channel | Focus |
|---|---|
| [Douglas Dwyer](https://www.youtube.com/@DouglasDwyer) | Octo voxel engine in Rust + WebGPU, path-traced GI |
| [John Lin (Voxely)](https://www.youtube.com/@johnlin) | Path-traced voxel sandbox engine, RTX |
| [Gabe Rundlett](https://www.youtube.com/@GabeRundlett) | Open-source C++ voxel engine with Daxa/Vulkan |
| [Ethan Gore](https://www.youtube.com/@EthanGore) | Voxel engine dev, binary greedy meshing contributor |
| [VoxelRifts](https://www.youtube.com/@VoxelRifts) | Programming explainer videos, voxel focus |
| [SimonDev](https://www.youtube.com/@simondev758) | Accessible intro video on Radiance Cascades |

### Projects and Repos

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

### Blog Posts

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

### ShaderToy

| Shader | Description |
|---|---|
| [Radiance Cascades 3D (surface-based)](https://www.shadertoy.com/view/X3XfRM) | Surface-based 3D RC, 5 cascades, cubemap storage |
| [Radiance Cascades (volumetric voxel)](https://www.shadertoy.com/view/M3ycWt) | True volumetric 3D RC with voxel raycaster |
| [Amanatides & Woo DDA (branchless)](https://www.shadertoy.com/view/XdtcRM) | Clean branchless 3D DDA implementation |

### Papers

#### Foundational Ray Traversal
| Paper | Link |
|---|---|
| A Fast Voxel Traversal Algorithm for Ray Tracing, Amanatides & Woo 1987 | [PDF](http://www.cse.yorku.ca/~amana/research/grid.pdf) |
| Efficient Sparse Voxel Octrees, Laine & Karras 2010 | [ResearchGate](https://www.researchgate.net/publication/47645140_Efficient_Sparse_Voxel_Octrees) |
| GigaVoxels: Ray-Guided Streaming for Efficient and Detailed Voxel Rendering, Crassin et al. 2009 | [INRIA](http://maverick.inria.fr/Publications/2009/CNLE09/) |
| Real-time Ray Tracing and Editing of Large Voxel Scenes (Brickmap), van Wingerden 2015 | [Utrecht](https://studenttheses.uu.nl/handle/20.500.12932/20460) |

#### SVDAG Family
| Paper | Link |
|---|---|
| High Resolution Sparse Voxel DAGs, Kampe, Sintorn, Assarsson 2013 | [PDF](https://icg.gwu.edu/sites/g/files/zaxdzs6126/files/downloads/highResolutionSparseVoxelDAGs.pdf) |
| SSVDAGs: Symmetry-aware Sparse Voxel DAGs, Villanueva, Marton, Gobbetti 2016 | [ACM](https://dl.acm.org/doi/10.1145/2856400.2856406) |
| Interactively Modifying Compressed Sparse Voxel Representations (HashDAG), Careil, Billeter, Eisemann 2020 | [Wiley](https://onlinelibrary.wiley.com/doi/abs/10.1111/cgf.13916) |
| Lossy Geometry Compression for High Resolution Voxel Scenes, van der Laan et al. 2020 | [ACM](https://dl.acm.org/doi/10.1145/3384543) |
| Transform-Aware Sparse Voxel Directed Acyclic Graphs (TSVDAG), Molenaar & Eisemann 2025 | [ACM](https://dl.acm.org/doi/10.1145/3728301) |
| Editing Compact Voxel Representations on the GPU, Molenaar & Eisemann 2024 | [TU Delft](https://publications.graphics.tudelft.nl/papers/13) |
| Editing Compressed High-Resolution Voxel Scenes with Attributes, Molenaar & Eisemann 2023 | [Wiley](https://onlinelibrary.wiley.com/doi/full/10.1111/cgf.14757) |

#### Color and Attribute Compression
| Paper | Link |
|---|---|
| Geometry and Attribute Compression for Voxel Scenes (Dado), Dado et al. 2016 | [CGF](https://diglib.eg.org/handle/10.1111/cgf.12841) |
| Compressing Color Data for Voxelized Surface Geometry (Dolonius), Dolonius et al. 2017 | [ACM I3D](https://dl.acm.org/doi/10.1145/3023368.3023381) |

#### Surveys
| Paper | Link |
|---|---|
| Hybrid Voxel Formats for Efficient Ray Tracing, 2024 | [arxiv](https://arxiv.org/abs/2410.14128) |
| Aokana: A GPU-Driven Voxel Rendering Framework for Open World Games, 2025 | [arxiv](https://arxiv.org/abs/2505.02017) |
| Voxelisation Algorithms and Data Structures: A Review, PMC 2021 | [PMC](https://pmc.ncbi.nlm.nih.gov/articles/PMC8707769/) |

### Misc

| Resource | Description |
|---|---|
| [Voxel.Wiki](https://voxel.wiki) | Community wiki, good starting hub for voxel rendering resources |
| [Voxely.net blog](https://voxely.net/blog/) | John Lin's blog on voxel engine design |
| [Jacco's voxel blog series](https://jacco.ompf2.com) | Practical renderer tutorials with OpenCL |