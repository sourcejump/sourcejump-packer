//! Strips a Source map (`.bsp`) down to what the web viewer reads. The result is still a valid BSP, so the
//! viewer's loader (noclip's `BSPFile`) reads it unchanged. These are daf's `viewerpack.py` rules, each checked
//! against noclip's loader:
//! - sounds and model files the viewer never loads leave the embedded pakfile;
//! - the HDR lighting set goes when the map has usable LDR lighting, since the viewer falls back to LDR when the
//!   HDR lumps are empty;
//! - PHYSCOLLIDE goes (the viewer has no physics);
//! - the map's own textures keep at most 1024 pixels a side (`vtf.rs`);
//! - LZMA lumps and pakfile entries are decoded, so brotli can compress them and the browser doesn't have to
//!   decode LZMA, unless decoding the pakfile would make a huge map larger still.
//!
//! On daf's 50-map sample this took the maps from 842 MB to 499 MB (both zstd-compressed). The input is a file
//! anyone could make, so every offset and size in it is checked before use.

use std::{borrow::Cow, io::Read};

use crate::{
    pakfile::{self, Content},
    vtf,
};

/// Lump indexes, from Valve's `bspfile.h`.
const LIGHTING: usize = 8;
const PHYSCOLLIDE: usize = 29;
const GAME_LUMP: usize = 35;
const PAKFILE: usize = 40;
/// `LEAF_AMBIENT_INDEX_HDR`, `LIGHTING_HDR`, `WORLDLIGHTS_HDR`, `LEAF_AMBIENT_LIGHTING_HDR`, `FACES_HDR`.
const HDR_LUMPS: [usize; 5] = [51, 53, 54, 55, 58];

const LUMP_COUNT: usize = 64;
/// Ident, version, 64 lump entries of 16 bytes, map revision.
const HEADER_LEN: usize = 8 + LUMP_COUNT * 16 + 4;

/// Decoding the pakfile may grow a map up to this size, or its original size if larger. Past it, LZMA entries
/// stay compressed and the viewer decodes them lazily: one map would grow from 180 to 514 MB to save 3%.
const DECODED_BUDGET: usize = 256 << 20;
/// No lump or pakfile entry decodes past this, so a crafted file can't make the packer hold gigabytes.
const MAX_DECODED: usize = 512 << 20;

/// Strips a map. Errors say why the file can't be read as a Source map.
pub fn strip(bsp: &[u8]) -> Result<Vec<u8>, String> {
    strip_with(bsp, true)
}

/// Whether the map is already stripped: stripping it again changes nothing. Its pakfile's LZMA entries are
/// left as they are, since a huge map keeps them, and their budget was judged against the map before stripping.
pub fn is_stripped(bsp: &[u8]) -> Result<bool, String> {
    Ok(strip_with(bsp, false)? == bsp)
}

fn strip_with(bsp: &[u8], decode_pakfile: bool) -> Result<Vec<u8>, String> {
    let table = lump_table(bsp)?;
    let lump = |index: usize| {
        let (offset, len) = table[index];
        &bsp[offset..offset + len]
    };
    let has_ldr = decode_lump(lump(LIGHTING))?.iter().any(|&byte| byte != 0);

    let mut kept: Vec<(usize, Vec<u8>)> = Vec::new();
    for (index, &(_, len)) in table.iter().enumerate() {
        let dropped = index == PHYSCOLLIDE || (has_ldr && HDR_LUMPS.contains(&index));
        if len == 0 || dropped {
            continue;
        }
        let data = decode_lump(lump(index))?;
        let data = if index == PAKFILE {
            strip_pakfile(&data, bsp.len(), has_ldr, decode_pakfile)?
        } else {
            data.into_owned()
        };
        kept.push((index, data));
    }

    // Lumps keep their original order; Source aligns each to 4 bytes.
    kept.sort_by_key(|&(index, _)| table[index].0);
    let mut out = bsp[..HEADER_LEN].to_vec();
    let mut new_offsets = [0usize; LUMP_COUNT];
    for (index, data) in &kept {
        out.resize(out.len().next_multiple_of(4), 0);
        new_offsets[*index] = out.len();
        out.extend_from_slice(data);
    }
    let mut new_table = [(0usize, 0usize); LUMP_COUNT];
    for (index, data) in &kept {
        new_table[*index] = (new_offsets[*index], data.len());
    }
    for (index, &(offset, len)) in new_table.iter().enumerate() {
        let entry = 8 + index * 16;
        write_i32(&mut out, entry, offset)?;
        write_i32(&mut out, entry + 4, len)?;
        // The fourCC field holds the uncompressed size of an LZMA lump; no lump is compressed any more.
        out[entry + 12..entry + 16].fill(0);
    }

    // The game lump's directory holds offsets from the start of the file, so they move with the lump.
    if new_table[GAME_LUMP].1 != 0 {
        let start = new_table[GAME_LUMP].0;
        let shift =
            i64::try_from(start).expect("fits") - i64::try_from(table[GAME_LUMP].0).expect("fits");
        let count = read_u32(&out, start)? as usize;
        for game_lump in 0..count {
            let field = start + 4 + game_lump * 16 + 8;
            let offset = i64::from(read_u32(&out, field)?);
            if offset != 0 {
                let moved = usize::try_from(offset + shift)
                    .map_err(|_| "a game lump offset is out of range")?;
                write_i32(&mut out, field, moved)?;
            }
        }
    }
    Ok(out)
}

/// Each lump's (offset, length), checked to lie inside the file.
fn lump_table(bsp: &[u8]) -> Result<[(usize, usize); LUMP_COUNT], String> {
    if bsp.len() < HEADER_LEN || &bsp[..4] != b"VBSP" {
        return Err("not a Source map: no VBSP header".to_owned());
    }
    // 19 and 20 are the versions Counter-Strike: Source loads.
    let version = read_u32(bsp, 4)?;
    if !(19..=20).contains(&version) {
        return Err(format!(
            "BSP version {version} isn't a Counter-Strike: Source map"
        ));
    }
    let mut table = [(0, 0); LUMP_COUNT];
    for (index, lump) in table.iter_mut().enumerate() {
        let offset = read_u32(bsp, 8 + index * 16)? as usize;
        let len = read_u32(bsp, 8 + index * 16 + 4)? as usize;
        if len != 0 && offset.checked_add(len).is_none_or(|end| end > bsp.len()) {
            return Err(format!("lump {index} lies outside the file"));
        }
        *lump = if len == 0 { (0, 0) } else { (offset, len) };
    }
    Ok(table)
}

/// A lump's bytes, decoded if it was stored LZMA-compressed.
fn decode_lump(data: &[u8]) -> Result<Cow<'_, [u8]>, String> {
    if !data.starts_with(b"LZMA") {
        return Ok(Cow::Borrowed(data));
    }
    // "LZMA", decoded size, compressed size, 5 bytes of LZMA properties, then the stream.
    let decoded_len = read_u32(data, 4)? as usize;
    let compressed_len = read_u32(data, 8)? as usize;
    let stream = data
        .get(17..17usize.saturating_add(compressed_len))
        .ok_or("an LZMA lump is cut short")?;
    decode_lzma(&data[12..17], stream, decoded_len).map(Cow::Owned)
}

/// Decodes a raw LZMA stream (Valve's header form: properties apart from the stream, size known).
pub(crate) fn decode_lzma(
    properties: &[u8],
    stream: &[u8],
    decoded_len: usize,
) -> Result<Vec<u8>, String> {
    if properties.len() != 5 {
        return Err("LZMA properties must be 5 bytes".to_owned());
    }
    if decoded_len > MAX_DECODED {
        return Err(format!(
            "an LZMA block claims {decoded_len} bytes, over the {MAX_DECODED}-byte limit"
        ));
    }
    let options = lzma_rs::decompress::Options {
        unpacked_size: lzma_rs::decompress::UnpackedSize::UseProvided(Some(decoded_len as u64)),
        memlimit: Some(MAX_DECODED),
        allow_incomplete: false,
    };
    let mut input = properties.chain(stream);
    let mut out = Vec::with_capacity(decoded_len);
    lzma_rs::lzma_decompress_with_options(
        &mut std::io::BufReader::new(&mut input),
        &mut out,
        &options,
    )
    .map_err(|error| format!("an LZMA block can't be decoded: {error}"))?;
    Ok(out)
}

/// The pakfile without the files the viewer never loads and with its textures capped, rewritten with its entries
/// stored.
fn strip_pakfile(
    pak: &[u8],
    map_len: usize,
    has_ldr: bool,
    decode: bool,
) -> Result<Vec<u8>, String> {
    let mut entries = pakfile::read(pak)?;
    entries.retain(|entry| viewer_reads(&entry.name, has_ldr));
    // Before the decoding budget, so a big map's capped textures count at their new size. An LZMA texture that
    // isn't capped stays as stored.
    for entry in &mut entries {
        if entry.name.to_ascii_lowercase().ends_with(b".vtf")
            && let Some(capped) = vtf::cap(&entry.content.decode()?)
        {
            entry.content = Content::Plain(Cow::Owned(capped));
        }
    }

    let decoded: usize = entries
        .iter()
        .map(|entry| entry.content.decoded_len())
        .sum();
    let budget = map_len.max(DECODED_BUDGET) - map_len;
    if decode && decoded.saturating_sub(pak.len()) <= budget {
        for entry in &mut entries {
            if matches!(entry.content, Content::Lzma { .. }) {
                entry.content = Content::Plain(Cow::Owned(entry.content.decode()?));
            }
        }
    }
    Ok(pakfile::write(&entries))
}

/// Whether the viewer loads a pakfile file. It has no audio and no physics, reads only `.dx90.vtx` meshes,
/// and with LDR lighting kept it never reads the HDR textures or HDR prop lighting.
fn viewer_reads(name: &[u8], has_ldr: bool) -> bool {
    let name = name.to_ascii_lowercase();
    let ends = |suffix: &str| name.ends_with(suffix.as_bytes());
    let unused = [
        ".wav",
        ".mp3",
        ".ogg",
        ".dx80.vtx",
        ".sw.vtx",
        ".xbox.vtx",
        ".phy",
    ];
    if unused.iter().any(|suffix| ends(suffix)) {
        return false;
    }
    !(has_ldr && (ends(".hdr.vtf") || is_hdr_prop_lighting(&name)))
}

/// `sp_hdr_<n>.vhv`: per-prop HDR vertex lighting.
fn is_hdr_prop_lighting(name: &[u8]) -> bool {
    let file = name.rsplit(|&byte| byte == b'/').next().unwrap_or(name);
    file.strip_prefix(b"sp_hdr_")
        .and_then(|rest| rest.strip_suffix(b".vhv"))
        .is_some_and(|number| !number.is_empty() && number.iter().all(u8::is_ascii_digit))
}

pub(crate) fn read_u32(bytes: &[u8], at: usize) -> Result<u32, String> {
    bytes
        .get(at..at + 4)
        .map(|field| u32::from_le_bytes(field.try_into().expect("4 bytes")))
        .ok_or_else(|| format!("the file ends early, at byte {at}"))
}

pub(crate) fn read_u16(bytes: &[u8], at: usize) -> Result<u16, String> {
    bytes
        .get(at..at + 2)
        .map(|field| u16::from_le_bytes(field.try_into().expect("2 bytes")))
        .ok_or_else(|| format!("the file ends early, at byte {at}"))
}

fn write_i32(bytes: &mut [u8], at: usize, value: usize) -> Result<(), String> {
    let value = i32::try_from(value).map_err(|_| "the map is too large for a BSP")?;
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        GAME_LUMP, HEADER_LEN, LIGHTING, PAKFILE, PHYSCOLLIDE, read_u32, strip, viewer_reads,
    };
    use crate::pakfile::{self, Content, Entry};

    /// A small map: the given lumps in index order after the header, each 4-byte aligned.
    fn map(lumps: &[(usize, Vec<u8>)]) -> Vec<u8> {
        let mut bsp = vec![0u8; HEADER_LEN];
        bsp[..4].copy_from_slice(b"VBSP");
        bsp[4..8].copy_from_slice(&20u32.to_le_bytes());
        for (index, data) in lumps {
            bsp.resize(bsp.len().next_multiple_of(4), 0);
            let offset = u32::try_from(bsp.len()).unwrap();
            bsp[8 + index * 16..12 + index * 16].copy_from_slice(&offset.to_le_bytes());
            let len = u32::try_from(data.len()).unwrap();
            bsp[12 + index * 16..16 + index * 16].copy_from_slice(&len.to_le_bytes());
            bsp.extend_from_slice(data);
        }
        bsp
    }

    fn lump(bsp: &[u8], index: usize) -> &[u8] {
        let offset = read_u32(bsp, 8 + index * 16).unwrap() as usize;
        let len = read_u32(bsp, 12 + index * 16).unwrap() as usize;
        &bsp[offset..offset + len]
    }

    fn pak(names: &[&str]) -> Vec<u8> {
        let entries: Vec<Entry> = names
            .iter()
            .map(|name| Entry {
                name: name.as_bytes().to_vec(),
                content: Content::Plain(name.as_bytes().to_vec().into()),
            })
            .collect();
        pakfile::write(&entries)
    }

    #[test]
    fn drops_what_the_viewer_never_reads_and_keeps_the_rest() {
        let files = [
            "materials/a.vtf",
            "materials/a.hdr.vtf",
            "sound/jump.wav",
            "models/p.dx90.vtx",
            "models/p.dx80.vtx",
            "models/p.phy",
            "materials/maps/m/sp_hdr_3.vhv",
            "materials/maps/m/sp_3.vhv",
        ];
        // Game lump: one entry whose data sits right after the directory, addressed from the file's start.
        let bsp_without_game = map(&[
            (1, vec![1; 8]),
            (LIGHTING, vec![7; 8]),
            (PHYSCOLLIDE, vec![9; 8]),
        ]);
        let game_start = bsp_without_game.len().next_multiple_of(4);
        let mut game = 1u32.to_le_bytes().to_vec();
        game.extend_from_slice(b"prps\0\0\0\0");
        game.extend_from_slice(&u32::try_from(game_start + 20).unwrap().to_le_bytes());
        game.extend_from_slice(&4u32.to_le_bytes());
        game.extend_from_slice(b"DATA");
        let bsp = map(&[
            (1, vec![1; 8]),
            (LIGHTING, vec![7; 8]),
            (PHYSCOLLIDE, vec![9; 8]),
            (GAME_LUMP, game),
            (PAKFILE, pak(&files)),
            (53, vec![5; 8]),
        ]);

        let stripped = strip(&bsp).unwrap();

        assert_eq!(lump(&stripped, 1), &[1; 8]);
        assert_eq!(lump(&stripped, PHYSCOLLIDE), &[] as &[u8]);
        assert!(
            lump(&stripped, 53).is_empty(),
            "HDR lighting goes when LDR is there"
        );
        let names: Vec<String> = pakfile::read(lump(&stripped, PAKFILE))
            .unwrap()
            .into_iter()
            .map(|entry| String::from_utf8(entry.name).unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "materials/a.vtf",
                "models/p.dx90.vtx",
                "materials/maps/m/sp_3.vhv"
            ]
        );
        let game = lump(&stripped, GAME_LUMP);
        let data_at = read_u32(game, 12).unwrap() as usize;
        assert_eq!(
            &stripped[data_at..data_at + 4],
            b"DATA",
            "game lump offsets follow the lump"
        );
    }

    #[test]
    fn keeps_hdr_when_the_map_has_no_ldr_lighting() {
        let bsp = map(&[
            (LIGHTING, vec![0; 8]),
            (53, vec![5; 8]),
            (PAKFILE, pak(&["a.hdr.vtf"])),
        ]);
        let stripped = strip(&bsp).unwrap();
        assert_eq!(lump(&stripped, 53), &[5; 8]);
        assert_eq!(pakfile::read(lump(&stripped, PAKFILE)).unwrap().len(), 1);
    }

    #[test]
    fn hdr_prop_lighting_is_matched_by_file_name() {
        assert!(!viewer_reads(b"Materials/Maps/X/SP_HDR_12.VHV", true));
        assert!(viewer_reads(b"materials/maps/x/sp_hdr_12.vhv", false));
        assert!(viewer_reads(b"materials/maps/x/sp_hdr_.vhv", true));
        assert!(viewer_reads(b"materials/maps/x/notsp_hdr_1.vhv", true));
    }

    /// `data` LZMA-compressed: the properties and the stream, without the .lzma size field.
    fn lzma(data: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut alone = Vec::new();
        lzma_rs::lzma_compress(&mut &data[..], &mut alone).unwrap();
        (alone[..5].to_vec(), alone[13..].to_vec())
    }

    #[test]
    fn decodes_lzma_lumps_and_pakfile_entries() {
        let lighting = vec![3u8; 1000];
        let (properties, stream) = lzma(&lighting);
        let mut lighting_lump = b"LZMA".to_vec();
        lighting_lump.extend_from_slice(&1000u32.to_le_bytes());
        lighting_lump.extend_from_slice(&u32::try_from(stream.len()).unwrap().to_le_bytes());
        lighting_lump.extend_from_slice(&properties);
        lighting_lump.extend_from_slice(&stream);

        let texture = b"VTF texture bytes".repeat(20);
        let (properties, stream) = lzma(&texture);
        let mut stored = vec![9, 0, 5, 0];
        stored.extend_from_slice(&properties);
        stored.extend_from_slice(&stream);
        let pak = pakfile::write(&[Entry {
            name: b"materials/a.vtf".to_vec(),
            content: Content::Lzma {
                stored: &stored,
                flags: 2,
                crc: 0,
                decoded_len: texture.len(),
            },
        }]);

        let stripped = strip(&map(&[(LIGHTING, lighting_lump), (PAKFILE, pak)])).unwrap();

        assert_eq!(lump(&stripped, LIGHTING), lighting.as_slice());
        assert_eq!(
            &stripped[8 + LIGHTING * 16 + 12..8 + LIGHTING * 16 + 16],
            &[0; 4]
        );
        let entries = pakfile::read(lump(&stripped, PAKFILE)).unwrap();
        assert!(matches!(&entries[0].content, Content::Plain(data) if **data == *texture));
    }

    #[test]
    fn refuses_files_that_arent_source_maps() {
        assert!(strip(b"not a map").unwrap_err().contains("VBSP"));
        let mut bsp = map(&[(1, vec![1; 8])]);
        bsp[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(strip(&bsp).unwrap_err().contains("outside"));
    }
}

/// Checks the port against daf's `viewerpack.py` on real maps, which can't live in the repo. Run with
/// `SJ_STRIP_CHECK_DIR=<dir> cargo test -- --ignored matches_viewerpack`, where `<dir>/bsp/` holds maps and
/// `<dir>/ref/` the same maps stripped by `python3 -I viewerpack.py <dir>/bsp <dir>/ref`.
#[cfg(test)]
#[test]
#[ignore = "needs real maps"]
fn matches_viewerpack() {
    let dir =
        std::path::PathBuf::from(std::env::var("SJ_STRIP_CHECK_DIR").expect("SJ_STRIP_CHECK_DIR"));
    for file in std::fs::read_dir(dir.join("bsp")).unwrap() {
        let path = file.unwrap().path();
        let name = path.file_name().unwrap();
        let ours = strip(&std::fs::read(&path).unwrap()).unwrap();
        let theirs = std::fs::read(dir.join("ref").join(name)).unwrap();
        assert!(
            ours == theirs,
            "{name:?}: {} bytes here, {} from viewerpack.py",
            ours.len(),
            theirs.len()
        );
    }
}
