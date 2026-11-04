//! Limits a map's own textures (`.vtf`) to `MAX_SIZE` pixels a side by dropping their largest mip levels, which
//! makes large maps' viewer files much smaller (chosen 2026-10-27 by comparing caps by eye in the viewer).
//!
//! A VTF stores its mip levels smallest first, so the largest ones are the end of the image data. Dropping them
//! cuts that tail and halves the header's size once per level. Only plain 2D DXT textures whose image data ends the
//! file are changed, and only when their computed layout matches the file to the byte; anything else is left as
//! it is. Brush faces keep tiling at the original size because the viewer reads it from the BSP's texdata.

/// The largest side kept.
pub const MAX_SIZE: u32 = 1024;

/// Header fields, from Valve's `vtf.h` (`VTFFileHeader_t`).
const SIGNATURE: &[u8; 4] = b"VTF\0";
const VERSION_MINOR: usize = 8;
const HEADER_SIZE: usize = 12;
const WIDTH: usize = 16;
const HEIGHT: usize = 18;
const FLAGS: usize = 20;
const FRAMES: usize = 24;
const FORMAT: usize = 52;
const MIP_COUNT: usize = 56;
const LOW_RES_FORMAT: usize = 57;
const LOW_RES_WIDTH: usize = 61;
const LOW_RES_HEIGHT: usize = 62;
const DEPTH: usize = 63;
const RESOURCE_COUNT: usize = 68;
const RESOURCES: usize = 80;
/// The resource tag of the high-resolution image data (7.3 and later).
const HIGH_RES_TAG: [u8; 3] = [0x30, 0, 0];
/// Cubemaps carry six faces (seven in older files) and volume textures several slices; neither is changed.
const ENVMAP: u32 = 0x4000;
/// `IMAGE_FORMAT_DXT1`, `_DXT3`, `_DXT5`, the formats the viewer uploads compressed.
const DXT1: u32 = 13;
const DXT3: u32 = 14;
const DXT5: u32 = 15;
const NO_FORMAT: u32 = u32::MAX;

/// The texture with its levels above `MAX_SIZE` dropped, or None when it has none or isn't a texture this
/// changes safely.
pub fn cap(file: &[u8]) -> Option<Vec<u8>> {
    let header = Header::read(file)?;
    let mut dropped = 0;
    while dropped + 1 < header.mip_count
        && (header.width >> dropped).max(header.height >> dropped) > MAX_SIZE
    {
        dropped += 1;
    }
    if dropped == 0 {
        return None;
    }
    let image_at = header.image_offset(file)?;
    let level_sizes: Vec<usize> = (0..header.mip_count)
        .map(|level| header.level_size(level))
        .collect();
    // The whole layout must account for the file exactly, or this isn't the texture it seems to be.
    if image_at.checked_add(level_sizes.iter().sum())? != file.len() {
        return None;
    }
    let cut: usize = level_sizes[..dropped as usize].iter().sum();
    let mut out = file[..file.len() - cut].to_vec();
    let width = u16::try_from((header.width >> dropped).max(1)).ok()?;
    let height = u16::try_from((header.height >> dropped).max(1)).ok()?;
    out[WIDTH..WIDTH + 2].copy_from_slice(&width.to_le_bytes());
    out[HEIGHT..HEIGHT + 2].copy_from_slice(&height.to_le_bytes());
    out[MIP_COUNT] = u8::try_from(header.mip_count - dropped).ok()?;
    Some(out)
}

struct Header {
    minor: u32,
    length: usize,
    width: u32,
    height: u32,
    frames: usize,
    block_bytes: usize,
    mip_count: u32,
    low_res_bytes: usize,
}

impl Header {
    fn read(file: &[u8]) -> Option<Self> {
        if file.get(..4)? != SIGNATURE || u32_at(file, 4)? != 7 {
            return None;
        }
        let minor = u32_at(file, VERSION_MINOR)?;
        let flags = u32_at(file, FLAGS)?;
        let depth = if minor >= 2 { u16_at(file, DEPTH)? } else { 1 };
        let block_bytes = match u32_at(file, FORMAT)? {
            DXT1 => 8,
            DXT3 | DXT5 => 16,
            _ => return None,
        };
        if flags & ENVMAP != 0 || depth > 1 {
            return None;
        }
        let low_res_bytes = match u32_at(file, LOW_RES_FORMAT)? {
            NO_FORMAT => 0,
            DXT1 => blocks(*file.get(LOW_RES_WIDTH)?, *file.get(LOW_RES_HEIGHT)?) * 8,
            _ => return None,
        };
        Some(Self {
            minor,
            length: u32_at(file, HEADER_SIZE)? as usize,
            width: u32::from(u16_at(file, WIDTH)?),
            height: u32::from(u16_at(file, HEIGHT)?),
            frames: usize::from(u16_at(file, FRAMES)?.max(1)),
            block_bytes,
            mip_count: u32::from(*file.get(MIP_COUNT)?),
            low_res_bytes,
        })
    }

    /// Where the high-resolution levels start: after the header and thumbnail, or where the resource says.
    fn image_offset(&self, file: &[u8]) -> Option<usize> {
        if self.minor < 3 {
            return self.length.checked_add(self.low_res_bytes);
        }
        let count = u32_at(file, RESOURCE_COUNT)? as usize;
        (0..count.min(32)).find_map(|index| {
            let at = RESOURCES + index * 8;
            (file.get(at..at + 3)? == HIGH_RES_TAG)
                .then(|| u32_at(file, at + 4).map(|offset| offset as usize))?
        })
    }

    /// One mip level's bytes across every frame.
    fn level_size(&self, level: u32) -> usize {
        let width = (self.width >> level).max(1);
        let height = (self.height >> level).max(1);
        blocks(width, height) * self.block_bytes * self.frames
    }
}

/// DXT blocks (4×4 pixels) covering an image.
fn blocks(width: impl Into<u32>, height: impl Into<u32>) -> usize {
    (width.into().div_ceil(4) * height.into().div_ceil(4)) as usize
}

fn u32_at(file: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(file.get(at..at + 4)?.try_into().ok()?))
}

fn u16_at(file: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(file.get(at..at + 2)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A version 7.2 DXT texture whose level `n` is filled with byte `n`, with a 16×16 DXT1 thumbnail.
    fn texture(width: u16, height: u16, mips: u8, format: u32, frames: u16) -> Vec<u8> {
        let mut file = vec![0u8; 80];
        file[..4].copy_from_slice(SIGNATURE);
        file[4..8].copy_from_slice(&7u32.to_le_bytes());
        file[VERSION_MINOR..VERSION_MINOR + 4].copy_from_slice(&2u32.to_le_bytes());
        file[HEADER_SIZE..HEADER_SIZE + 4].copy_from_slice(&80u32.to_le_bytes());
        file[WIDTH..WIDTH + 2].copy_from_slice(&width.to_le_bytes());
        file[HEIGHT..HEIGHT + 2].copy_from_slice(&height.to_le_bytes());
        file[FRAMES..FRAMES + 2].copy_from_slice(&frames.to_le_bytes());
        file[FORMAT..FORMAT + 4].copy_from_slice(&format.to_le_bytes());
        file[MIP_COUNT] = mips;
        file[LOW_RES_FORMAT..LOW_RES_FORMAT + 4].copy_from_slice(&DXT1.to_le_bytes());
        file[LOW_RES_WIDTH] = 16;
        file[LOW_RES_HEIGHT] = 16;
        file[DEPTH..DEPTH + 2].copy_from_slice(&1u16.to_le_bytes());
        file.extend(std::iter::repeat_n(0xAA, 16 * 8));
        // Sized as DXT1 (8 bytes a block) or 16 for any other format; unsupported ones are refused before sizes count.
        let block_bytes = if format == DXT1 { 8 } else { 16 };
        for level in (0..mips).rev() {
            let size = blocks((width >> level).max(1), (height >> level).max(1))
                * block_bytes
                * usize::from(frames);
            file.extend(std::iter::repeat_n(level, size));
        }
        file
    }

    #[test]
    fn drops_the_levels_above_the_cap_and_keeps_the_rest_byte_for_byte() {
        let file = texture(4096, 2048, 13, DXT5, 1);
        let capped = cap(&file).unwrap();
        let header = Header::read(&capped).unwrap();
        assert_eq!(
            (header.width, header.height, header.mip_count),
            (1024, 512, 11)
        );
        // Levels 0 and 1 of the original (4096 and 2048 wide) are gone; the rest is the original's start.
        assert_eq!(capped[80..], file[80..capped.len()]);
        assert_eq!(*capped.last().unwrap(), 2);
        assert_eq!(
            header.image_offset(&capped).unwrap()
                + (0..11).map(|l| header.level_size(l)).sum::<usize>(),
            capped.len()
        );
    }

    #[test]
    fn counts_every_frame_of_an_animated_texture() {
        let file = texture(2048, 2048, 12, DXT1, 3);
        let capped = cap(&file).unwrap();
        assert_eq!(file.len() - capped.len(), 512 * 512 * 8 * 3);
    }

    #[test]
    fn leaves_small_and_unsupported_textures_alone() {
        assert!(cap(&texture(1024, 1024, 11, DXT1, 1)).is_none());
        // Uncompressed BGRA8888 (12).
        assert!(cap(&texture(2048, 2048, 12, 12, 1)).is_none());
        // A single level has nothing smaller to fall back to.
        assert!(cap(&texture(2048, 2048, 1, DXT1, 1)).is_none());
    }

    #[test]
    fn leaves_a_texture_whose_layout_does_not_add_up() {
        let mut file = texture(2048, 2048, 12, DXT1, 1);
        file.push(0);
        assert!(cap(&file).is_none());
        let mut cubemap = texture(2048, 2048, 12, DXT1, 1);
        cubemap[FLAGS..FLAGS + 4].copy_from_slice(&ENVMAP.to_le_bytes());
        assert!(cap(&cubemap).is_none());
    }
}
