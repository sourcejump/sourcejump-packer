//! A map's file for the web viewer, made from its `.bsp` or `.bsp.bz2`: stripped to what the viewer reads
//! (`strip.rs`) and brotli-compressed, so browsers decode it themselves as it downloads.
//!
//! SourceJump's staff pack maps with it in their browser (built for WebAssembly) or with `sj-pack`, a folder at a
//! time; SourceJump's API checks what they upload with `check` before storing it.

mod pakfile;
mod strip;
mod vtf;

use std::{fmt::Write, io::Read};

use sha2::{Digest, Sha256};

/// Larger maps aren't packed, before and after unpacking from `.bz2`: a bound on memory, and on a `.bz2` made to
/// unpack without end. The largest maps known are around 600 MB; stripping leaves much less of them.
pub const MAX_MAP_BYTES: u64 = 1 << 30;

/// The largest stripped map `check` decodes from an upload, bounding what SourceJump's API holds in memory.
pub const MAX_STRIPPED_BYTES: u64 = 512 << 20;

/// The largest packed file SourceJump's API takes.
pub const MAX_PACKED_BYTES: usize = 256 << 20;

/// A packed map.
pub struct Packed {
    /// The map as given, unpacked from `.bz2`, in bytes.
    pub source_bytes: usize,
    pub stripped_bytes: usize,
    /// The file to store: the stripped map, brotli-compressed.
    pub file: Vec<u8>,
    /// Names the stored file: a hash of the stripped map.
    pub hash: String,
}

/// Where packing is, for a progress bar: compressing takes nearly all the time.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stage {
    Unpacking,
    Stripping,
    /// With the bytes compressed so far, of `stripped_bytes`.
    Compressing {
        done: usize,
        total: usize,
    },
}

/// Packs a map. Errors say why the file can't be read as a Source map, or why SourceJump wouldn't take it.
pub fn pack(file: &[u8], mut progress: impl FnMut(Stage)) -> Result<Packed, String> {
    let bsp = if file.starts_with(b"BZh") {
        progress(Stage::Unpacking);
        let mut bsp = Vec::new();
        bzip2::read::BzDecoder::new(file)
            .take(MAX_MAP_BYTES + 1)
            .read_to_end(&mut bsp)
            .map_err(|error| format!("the .bz2 file can't be read: {error}"))?;
        std::borrow::Cow::Owned(bsp)
    } else {
        std::borrow::Cow::Borrowed(file)
    };
    if bsp.len() as u64 > MAX_MAP_BYTES {
        return Err(format!("the map is over {} MB", MAX_MAP_BYTES >> 20));
    }
    progress(Stage::Stripping);
    let stripped = strip::strip(&bsp)?;
    if stripped.len() as u64 > MAX_STRIPPED_BYTES {
        return Err(format!(
            "stripped, the map is still over {} MB",
            MAX_STRIPPED_BYTES >> 20
        ));
    }
    let total = stripped.len();
    let mut reader = Progress {
        bytes: &stripped[..],
        done: 0,
        report: |done| progress(Stage::Compressing { done, total }),
    };
    let file = compress(&mut reader, total);
    if file.len() > MAX_PACKED_BYTES {
        return Err(format!(
            "packs to over the {} MB SourceJump takes",
            MAX_PACKED_BYTES >> 20
        ));
    }
    Ok(Packed {
        source_bytes: bsp.len(),
        stripped_bytes: total,
        hash: hash(&stripped),
        file,
    })
}

/// A packed file that passed `check`.
#[derive(Debug)]
pub struct Checked {
    pub stripped_bytes: usize,
    pub hash: String,
}

/// Checks an uploaded file: brotli that decodes to a map `pack` could have made, so stripping it again changes
/// nothing. Errors say what's wrong with it.
pub fn check(file: &[u8]) -> Result<Checked, String> {
    let mut stripped = Vec::new();
    brotli::Decompressor::new(file, 1 << 16)
        .take(MAX_STRIPPED_BYTES + 1)
        .read_to_end(&mut stripped)
        .map_err(|_| "the file isn't brotli-compressed".to_owned())?;
    if stripped.len() as u64 > MAX_STRIPPED_BYTES {
        return Err(format!("the map is over {} MB", MAX_STRIPPED_BYTES >> 20));
    }
    if !strip::is_stripped(&stripped)? {
        return Err("the map isn't packed for the viewer: pack it again".to_owned());
    }
    Ok(Checked {
        stripped_bytes: stripped.len(),
        hash: hash(&stripped),
    })
}

/// The first 8 bytes of the stripped map's SHA-256, in hex.
fn hash(stripped: &[u8]) -> String {
    Sha256::digest(stripped)[..8]
        .iter()
        .fold(String::with_capacity(16), |mut hex, byte| {
            write!(hex, "{byte:02x}").expect("writing to a String can't fail");
            hex
        })
}

/// Compresses a file that browsers download whole, to be served with `Content-Encoding: br`, as maps are.
#[must_use]
pub fn compress_for_browsers(bytes: &[u8]) -> Vec<u8> {
    compress(&mut &bytes[..], bytes.len())
}

/// Brotli at quality 11 with a 16 MB window, the largest browsers accept. 9-11% smaller than zstd -19 on
/// daf's sample, and decoded by the browser itself.
fn compress(input: &mut impl Read, len: usize) -> Vec<u8> {
    let params = brotli::enc::BrotliEncoderParams {
        quality: 11,
        lgwin: 24,
        size_hint: len,
        ..Default::default()
    };
    let mut out = Vec::new();
    brotli::BrotliCompress(input, &mut out, &params)
        .expect("reading and writing memory can't fail");
    out
}

/// Reads bytes, reporting how many have been read.
struct Progress<'a, F: FnMut(usize)> {
    bytes: &'a [u8],
    done: usize,
    report: F,
}

impl<F: FnMut(usize)> Read for Progress<'_, F> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.bytes.read(buf)?;
        self.done += read;
        (self.report)(self.done);
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use super::{Stage, check, pack};

    /// The smallest file the stripper accepts: a header with no lumps.
    fn empty_map() -> Vec<u8> {
        let mut bsp = vec![0u8; 8 + 64 * 16 + 4];
        bsp[..4].copy_from_slice(b"VBSP");
        bsp[4..8].copy_from_slice(&20u32.to_le_bytes());
        bsp
    }

    #[test]
    fn a_packed_map_passes_the_check_with_the_same_hash() {
        let mut stages = Vec::new();
        let packed = pack(&empty_map(), |stage| stages.push(stage)).unwrap();
        let checked = check(&packed.file).unwrap();
        assert_eq!(checked.hash, packed.hash);
        assert_eq!(checked.stripped_bytes, packed.stripped_bytes);
        assert_eq!(stages.first(), Some(&Stage::Stripping));
        assert!(matches!(stages.last(), Some(Stage::Compressing { done, total }) if done == total));
    }

    #[test]
    fn a_bz2_map_is_unpacked_first() {
        let mut bz2 = Vec::new();
        std::io::copy(
            &mut bzip2::read::BzEncoder::new(&empty_map()[..], bzip2::Compression::fast()),
            &mut bz2,
        )
        .unwrap();
        let packed = pack(&bz2, |_| {}).unwrap();
        assert_eq!(packed.source_bytes, empty_map().len());
    }

    #[test]
    fn the_check_refuses_what_pack_wouldnt_make() {
        let unstripped = {
            // A map with physics, which packing drops.
            let mut bsp = empty_map();
            let offset = u32::try_from(bsp.len()).unwrap();
            bsp[8 + 29 * 16..12 + 29 * 16].copy_from_slice(&offset.to_le_bytes());
            bsp[12 + 29 * 16..16 + 29 * 16].copy_from_slice(&4u32.to_le_bytes());
            bsp.extend_from_slice(&[1, 2, 3, 4]);
            bsp
        };
        let mut compressed = Vec::new();
        brotli::BrotliCompress(
            &mut &unstripped[..],
            &mut compressed,
            &brotli::enc::BrotliEncoderParams::default(),
        )
        .unwrap();
        assert!(check(&compressed).unwrap_err().contains("pack it again"));
        assert!(check(b"nope").is_err());
    }
}

/// Checks that packed maps pass the check on real maps, which can't live in the repo. Run with
/// `SJ_PACKED_DIR=<dir> cargo test -p sourcejump-viewer-pack -- --ignored packed_maps_pass`, where `<dir>` holds
/// viewer files as stored (brotli).
#[cfg(test)]
#[test]
#[ignore = "needs real maps"]
fn packed_maps_pass_the_check() {
    let dir = std::env::var("SJ_PACKED_DIR").expect("SJ_PACKED_DIR");
    for file in std::fs::read_dir(dir).unwrap() {
        let path = file.unwrap().path();
        let checked = check(&std::fs::read(&path).unwrap());
        assert!(checked.is_ok(), "{path:?}: {:?}", checked.err());
    }
}
