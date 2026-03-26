#!/usr/bin/env python3
"""Experiment with color compression strategies on materials.bin + geometry.bin."""
import struct, sys, collections

mat_path  = "data/materials.bin"
geom_path = "data/geometry.bin"
bs_path   = "data/block_states.bin"

with open(mat_path,  "rb") as f: mat  = f.read()
with open(geom_path, "rb") as f: geom = f.read()
with open(bs_path,   "rb") as f: bs   = f.read()

assert mat[:4]  == b"MATL", "bad materials.bin magic"
assert geom[:4] == b"GEOM", "bad geometry.bin magic"
assert struct.unpack_from("<I", bs, 0)[0] == 0x42534944, f"bad block_states.bin magic: {bs[:4]}"

count        = struct.unpack_from("<I", mat,  4)[0]
num_payloads = struct.unpack_from("<I", mat,  8)[0]
geom_count   = struct.unpack_from("<I", geom, 4)[0]
color_ids_base      = 12
payload_offsets_base = color_ids_base + count * 2
payload_data_base    = payload_offsets_base + num_payloads * 4
color_ids    = struct.unpack_from(f"<{count}H", mat, color_ids_base)
payload_offs = struct.unpack_from(f"<{num_payloads}I", mat, payload_offsets_base)

bs_count = struct.unpack_from("<I", bs, 4)[0]
id_to_key = {}
cursor = 8
for _ in range(bs_count):
    bid = struct.unpack_from("<H", bs, cursor)[0]
    klen = struct.unpack_from("<H", bs, cursor + 2)[0]
    key = bs[cursor + 4 : cursor + 4 + klen].decode()
    id_to_key[bid] = key
    cursor += 4 + klen

print(f"block states: {count}")
print()

bricks = []  # list of (palette: list[tuple[int,int,int,int]], indices: bytes)

for cid in color_ids:
    if cid == 0:
        bricks.append(([], b""))
        continue
    off  = payload_data_base + payload_offs[cid - 1]
    meta        = struct.unpack_from("<I", mat, off)[0]
    pal_count   = (meta >> 24) & 0xFF
    solid_count = (meta >> 8)  & 0xFFFF
    if solid_count > 0 and pal_count == 0:
        pal_count = 256
    pal_base = off + 4
    palette = [tuple(mat[pal_base + i*4 : pal_base + i*4 + 4]) for i in range(pal_count)]
    idx_base = pal_base + pal_count * 4
    indices  = mat[idx_base : idx_base + solid_count]
    bricks.append((palette, indices))

non_empty       = sum(1 for p, _ in bricks if p)
total_pal_bytes = sum(len(p) * 4 for p, _ in bricks)
total_idx_bytes = sum(len(i)     for _, i in bricks)
current_total   = total_pal_bytes + total_idx_bytes

unique_colors = set(c for p, _ in bricks for c in p)

print(f"non-empty bricks:          {non_empty}")
print(f"unique RGBA colors:        {len(unique_colors)}")
print(f"palette data:              {total_pal_bytes/1024/1024:.2f} MB")
print(f"index data:                {total_idx_bytes/1024/1024:.2f} MB")
print(f"total color data:          {current_total/1024/1024:.2f} MB  (baseline)")
print()

payload_set  = {}
dedup_bytes  = 0
for pal, idx in bricks:
    if not pal:
        continue
    key = bytes(c for rgba in pal for c in rgba) + bytes(idx)
    if key not in payload_set:
        payload_set[key] = True
        dedup_bytes += len(key)

ref_bytes   = non_empty * 2  # u16 per block state
dedup_total = dedup_bytes + ref_bytes
print(f"--- 1. full color-payload deduplication ---")
print(f"  unique payloads:         {len(payload_set)}")
print(f"  payload bytes:           {dedup_bytes/1024/1024:.2f} MB")
print(f"  + u16 refs:              {ref_bytes/1024:.1f} KB")
print(f"  total:                   {dedup_total/1024/1024:.2f} MB")
print(f"  saves:                   {(current_total - dedup_total)/1024/1024:.2f} MB")
print()

def rle_size(indices):
    if not indices: return 0
    size, i = 0, 0
    while i < len(indices):
        run = 1
        while i + run < len(indices) and indices[i+run] == indices[i] and run < 255:
            run += 1
        size += 2
        i += run
    return size

rle_idx_bytes = sum(rle_size(i) for _, i in bricks)
single_color  = sum(1 for _, i in bricks if i and len(set(i)) == 1)

print(f"--- 2. RLE on color indices ---")
print(f"  single-color bricks:     {single_color} / {non_empty}  ({100*single_color//non_empty}%)")
print(f"  RLE index bytes:         {rle_idx_bytes/1024/1024:.2f} MB  (was {total_idx_bytes/1024/1024:.2f} MB)")
print(f"  index savings:           {(total_idx_bytes - rle_idx_bytes)/1024/1024:.2f} MB")
print(f"  total with RLE:          {(total_pal_bytes + rle_idx_bytes)/1024/1024:.2f} MB")
print()

fits_16 = sum(1 for p, _ in bricks if 0 < len(p) <= 16)
fits_32 = sum(1 for p, _ in bricks if 0 < len(p) <= 32)
fits_64 = sum(1 for p, _ in bricks if 0 < len(p) <= 64)

idx_4bit_bytes = sum(
    (len(i) + 1) // 2 if 0 < len(p) <= 16 else len(i)
    for p, i in bricks
)

print(f"--- 3. 4-bit indices for ≤16-color bricks ---")
print(f"  bricks with ≤16 colors:  {fits_16} / {non_empty}  ({100*fits_16//non_empty}%)")
print(f"  bricks with ≤32 colors:  {fits_32} / {non_empty}  ({100*fits_32//non_empty}%)")
print(f"  bricks with ≤64 colors:  {fits_64} / {non_empty}  ({100*fits_64//non_empty}%)")
print(f"  4-bit index bytes:       {idx_4bit_bytes/1024/1024:.2f} MB  (was {total_idx_bytes/1024/1024:.2f} MB)")
print(f"  index savings:           {(total_idx_bytes - idx_4bit_bytes)/1024/1024:.2f} MB")
print(f"  total with 4-bit:        {(total_pal_bytes + idx_4bit_bytes)/1024/1024:.2f} MB")
print()

combined_set   = {}
combined_bytes = 0
for pal, idx in bricks:
    if not pal:
        continue
    rle = bytearray()
    i = 0
    while i < len(idx):
        run = 1
        while i + run < len(idx) and idx[i+run] == idx[i] and run < 255:
            run += 1
        rle += bytes([run, idx[i]])
        i += run
    key = bytes(c for rgba in pal for c in rgba) + bytes(rle)
    if key not in combined_set:
        combined_set[key] = True
        combined_bytes += len(key)

combined_total = combined_bytes + non_empty * 2
print(f"--- 4. RLE + dedup combined ---")
print(f"  unique RLE payloads:     {len(combined_set)}")
print(f"  payload bytes:           {combined_bytes/1024/1024:.2f} MB")
print(f"  total:                   {combined_total/1024/1024:.2f} MB")
print(f"  saves vs current:        {(current_total - combined_total)/1024/1024:.2f} MB")
print()

dist = collections.Counter(len(p) for p, _ in bricks if p)
print(f"--- 5. palette size distribution ---")
buckets = [(1,1),(2,4),(5,8),(9,16),(17,32),(33,64),(65,128),(129,256)]
for lo, hi in buckets:
    n = sum(dist[k] for k in range(lo, hi+1))
    bar = "#" * (n * 40 // non_empty)
    print(f"  {lo:>3}–{hi:<3} colors: {n:>6} bricks  {bar}")
print()

vox_dist = collections.Counter(len(i) for _, i in bricks if i)
print(f"--- 6. solid voxel count distribution ---")
vox_buckets = [(1,100),(101,300),(301,600),(601,1000),(1001,2000),(2001,3000),(3001,4096)]
for lo, hi in vox_buckets:
    n = sum(vox_dist[k] for k in range(lo, hi+1))
    bar = "#" * (n * 40 // non_empty)
    print(f"  {lo:>4}–{hi:<4} voxels: {n:>6} bricks  {bar}")
print()

# Build payload_key → list of block state keys
payload_to_blocks = collections.defaultdict(list)
for block_id, (pal, idx) in enumerate(bricks):
    if not pal:
        continue
    key = bytes(c for rgba in pal for c in rgba) + bytes(idx)
    bs_key = id_to_key.get(block_id, f"<unknown id={block_id}>")
    payload_to_blocks[key].append(bs_key)

share_counts = {k: len(v) for k, v in payload_to_blocks.items()}
freq = collections.Counter(share_counts.values())
print(f"--- 7. how many block states share each unique payload ---")
share_buckets = [(1,1),(2,2),(3,4),(5,8),(9,16),(17,32),(33,64),(65,999)]
for lo, hi in share_buckets:
    n = sum(freq[k] for k in range(lo, hi+1))
    total_blocks = sum(freq[k]*k for k in range(lo, hi+1))
    print(f"  shared by {lo:>2}–{hi:<3}: {n:>5} payloads  ({total_blocks:>6} block states)")
print()

all_alpha = collections.Counter()
for pal, _ in bricks:
    for rgba in pal:
        all_alpha[rgba[3]] += 1

total_palette_entries = sum(all_alpha.values())
non_255 = sum(v for k, v in all_alpha.items() if k != 255)
print(f"--- 8. alpha channel analysis ---")
print(f"  total palette entries:   {total_palette_entries:,}")
print(f"  alpha == 255:            {all_alpha[255]:,}  ({100*all_alpha[255]//total_palette_entries}%)")
print(f"  alpha != 255:            {non_255:,}  ({100*non_255//total_palette_entries if total_palette_entries else 0}%)")
if non_255 > 0:
    print(f"  non-255 alpha values:    {sorted(k for k in all_alpha if k != 255)[:20]}")
print()

seen_keys_4bit = {}
dedup_4bit_payload_bytes = 0
for pal, idx in bricks:
    if not pal: continue
    if len(pal) <= 16:
        nibble_idx = bytearray((len(idx) + 1) // 2)
        for i in range(0, len(idx), 2):
            lo = idx[i]
            hi = idx[i+1] if i+1 < len(idx) else 0
            nibble_idx[i//2] = lo | (hi << 4)
        key = bytes(c for rgba in pal for c in rgba) + bytes(nibble_idx)
    else:
        key = bytes(c for rgba in pal for c in rgba) + bytes(idx)
    if key not in seen_keys_4bit:
        seen_keys_4bit[key] = True
        dedup_4bit_payload_bytes += len(key)

dedup_4bit_total = dedup_4bit_payload_bytes + non_empty * 2
print(f"--- 9. dedup + 4-bit nibble indices for ≤16-color payloads ---")
print(f"  unique payloads:         {len(seen_keys_4bit)}")
print(f"  payload bytes:           {dedup_4bit_payload_bytes/1024/1024:.2f} MB")
print(f"  total:                   {dedup_4bit_total/1024/1024:.2f} MB")
print(f"  saves vs current:        {(current_total - dedup_4bit_total)/1024/1024:.2f} MB")
print(f"  vs dedup alone:          {(dedup_total - dedup_4bit_total)/1024/1024:.2f} MB extra savings")
print()

print(f"--- 10. shared payload groups (most-shared first) ---")
groups = sorted(payload_to_blocks.values(), key=len, reverse=True)

# Print top groups (shared by 3+), truncating very long lists
printed = 0
for group in groups:
    if len(group) < 3:
        break
    pal_bytes = len(next(k for k, v in payload_to_blocks.items() if v is group))
    print(f"\n  [{len(group)} block states share this payload]")
    for bs_key in sorted(group)[:30]:
        print(f"    {bs_key}")
    if len(group) > 30:
        print(f"    ... and {len(group) - 30} more")
    printed += 1

# Also print a sample of unique payloads (shared by exactly 1)
unique_payloads = [g for g in groups if len(g) == 1]
print(f"\n  [{len(unique_payloads)} payloads are unique to a single block state]")
print(f"  sample of 10 unique blocks:")
for g in unique_payloads[:10]:
    print(f"    {g[0]}")
