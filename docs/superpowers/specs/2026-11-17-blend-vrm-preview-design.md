# .blend / .vrm preview (short design)

## Why

The user keeps Blender scenes and VRM avatars next to images and wants to see
what each file is without opening Blender or a VRM tool.

## Goal

- `.blend` and `.vrm` files show up in the file list, get a thumbnail, show in
  the preview panel and open in the viewer, like images.
- The picture is the thumbnail the file already carries. Nothing is rendered.

## Non-goals

- `.glb` / `.gltf` (no embedded thumbnail; rendering a model was declined for now).
- Editing: rotating the file, "개인 정보 지우기", or any other write. View-only
  rotation is fine.
- Showing VRM license / author metadata.

## Anti-goals

- Freezing on a large `.blend` (hundreds of MB): only read until the thumbnail.
- Crashing or hanging on a broken or truncated file.
- Writing to a `.blend` / `.vrm` file.

## Approaches

| # | Approach | Trade-off |
|---|----------|-----------|
| 1 | Read the embedded thumbnail (chosen) | Fast, no Blender needed. Picture is small (Blender stores 128x128 by default) and as of the last save. Files saved without a preview have no picture. |
| 2 | Windows shell thumbnail (BlendThumb.dll) | Works only where Blender is installed and registered; no VRM support. |
| 3 | Render the model | Big work, needs a 3D renderer; declined. |

## How (decided)

- `.blend`: plain, gzip (`1F 8B`) or zstd (`28 B5 2F FD`) file. Walk file blocks
  to the `TEST` block: `int32 width, int32 height`, then RGBA rows stored
  bottom-up. Both the old 12-byte header (`BLENDER_v` / `BLENDER-v` + version)
  and the Blender 5.x header must work. Stop after the `TEST` block, at `ENDB`,
  or after a fixed read limit.
- `.vrm` (GLB): VRM 1.0 `extensions.VRMC_vrm.meta.thumbnailImage` (image index),
  VRM 0.x `extensions.VRM.meta.texture` (texture index -> `source` image). Image
  bytes come from its bufferView in the BIN chunk and are decoded as PNG/JPEG.
- No thumbnail inside -> the normal "can't read" thumbnail; no error dialog.

## Left to the builder

Module layout, read limits, crate choice for zstd (pure Rust preferred).
