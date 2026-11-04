//! The zip archive a map embeds (its pakfile): custom textures, models and lighting. Read just far enough to
//! drop files and decode entries, and written back with every entry stored, which is what Source writes too.
//! A zip crate would add little: Valve's LZMA entries (method 14) need handling by hand either way.

use std::{borrow::Cow, io::Read};

use crate::strip::{decode_lzma, read_u16, read_u32};

/// No entry inflates past this, so a crafted file can't make the packer hold gigabytes.
const MAX_ENTRY: u64 = 512 << 20;

pub struct Entry<'a> {
    pub name: Vec<u8>,
    pub content: Content<'a>,
}

pub enum Content<'a> {
    Plain(Cow<'a, [u8]>),
    /// Valve's LZMA entry, kept as stored: a 4-byte header (version, properties size), the properties, the
    /// stream. Passed through unchanged when decoding would make the map too large.
    Lzma {
        stored: &'a [u8],
        flags: u16,
        crc: u32,
        decoded_len: usize,
    },
}

impl Content<'_> {
    pub fn decoded_len(&self) -> usize {
        match self {
            Self::Plain(data) => data.len(),
            Self::Lzma { decoded_len, .. } => *decoded_len,
        }
    }

    /// The entry's bytes, decoding LZMA.
    pub fn decode(&self) -> Result<Vec<u8>, String> {
        match self {
            Self::Plain(data) => Ok(data.to_vec()),
            Self::Lzma {
                stored,
                decoded_len,
                ..
            } => {
                let properties_len = usize::from(read_u16(stored, 2)?);
                let properties = stored
                    .get(4..4 + properties_len)
                    .ok_or("an LZMA entry is cut short")?;
                decode_lzma(properties, &stored[4 + properties_len..], *decoded_len)
            }
        }
    }
}

/// Every entry in central-directory order. Stored and deflated entries come back as their bytes, LZMA ones as
/// stored.
pub fn read(pak: &[u8]) -> Result<Vec<Entry<'_>>, String> {
    // The end record is the last 22 bytes, unless the archive has a comment of up to 64 KB.
    let search_from = pak.len().saturating_sub(22 + 0xffff);
    let end = pak[search_from..]
        .windows(4)
        .rposition(|window| window == b"PK\x05\x06")
        .map(|at| search_from + at)
        .ok_or("the pakfile isn't a zip archive")?;
    let count = usize::from(read_u16(pak, end + 10)?);
    let directory_len = read_u32(pak, end + 12)? as usize;
    let directory_at = read_u32(pak, end + 16)? as usize;
    // Offsets count from the archive's own start; if bytes sit before it, everything shifts by that much.
    let shift = end
        .checked_sub(directory_len)
        .and_then(|start| start.checked_sub(directory_at))
        .ok_or("the pakfile's directory lies outside it")?;

    let mut entries = Vec::with_capacity(count);
    let mut at = directory_at + shift;
    for _ in 0..count {
        if pak.get(at..at + 4) != Some(b"PK\x01\x02") {
            return Err("the pakfile's directory is damaged".to_owned());
        }
        let flags = read_u16(pak, at + 8)?;
        let method = read_u16(pak, at + 10)?;
        let crc = read_u32(pak, at + 16)?;
        let stored_len = read_u32(pak, at + 20)? as usize;
        let decoded_len = read_u32(pak, at + 24)? as usize;
        let name_len = usize::from(read_u16(pak, at + 28)?);
        let extra_len = usize::from(read_u16(pak, at + 30)?);
        let comment_len = usize::from(read_u16(pak, at + 32)?);
        let header = read_u32(pak, at + 42)? as usize + shift;
        let name = pak
            .get(at + 46..at + 46 + name_len)
            .ok_or("the pakfile's directory is cut short")?
            .to_vec();
        at += 46 + name_len + extra_len + comment_len;

        if pak.get(header..header + 4) != Some(b"PK\x03\x04") {
            return Err("a pakfile entry is missing".to_owned());
        }
        let data_at = header
            + 30
            + usize::from(read_u16(pak, header + 26)?)
            + usize::from(read_u16(pak, header + 28)?);
        let stored = pak
            .get(data_at..data_at + stored_len)
            .ok_or("a pakfile entry is cut short")?;
        let content = match method {
            0 => Content::Plain(Cow::Borrowed(stored)),
            8 => {
                let mut inflated = Vec::new();
                flate2::read::DeflateDecoder::new(stored)
                    .take(MAX_ENTRY + 1)
                    .read_to_end(&mut inflated)
                    .map_err(|error| format!("a deflated pakfile entry can't be read: {error}"))?;
                if inflated.len() as u64 > MAX_ENTRY {
                    return Err("a pakfile entry inflates past the size limit".to_owned());
                }
                Content::Plain(Cow::Owned(inflated))
            }
            14 => Content::Lzma {
                stored,
                flags,
                crc,
                decoded_len,
            },
            other => return Err(format!("a pakfile entry uses compression method {other}")),
        };
        entries.push(Entry { name, content });
    }
    Ok(entries)
}

/// A zip of the entries: plain ones stored, LZMA ones as they were. The fields match `viewerpack.py`'s output
/// byte for byte, which is how the port was checked against it.
pub fn write(entries: &[Entry]) -> Vec<u8> {
    let mut local = Vec::new();
    let mut directory = Vec::new();
    for entry in entries {
        let (version, flags, method, crc, data, decoded_len): (u16, u16, u16, u32, &[u8], usize) =
            match &entry.content {
                Content::Plain(data) => (10, 0, 0, crc32(data), data, data.len()),
                Content::Lzma {
                    stored,
                    flags,
                    crc,
                    decoded_len,
                } => (63, *flags, 14, *crc, stored, *decoded_len),
            };
        let mut fields = Vec::with_capacity(22);
        for half in [version, flags, method, 0, 0x21] {
            fields.extend_from_slice(&half.to_le_bytes());
        }
        for word in [crc, to_u32(data.len()), to_u32(decoded_len)] {
            fields.extend_from_slice(&word.to_le_bytes());
        }
        let name_len = u16::try_from(entry.name.len()).expect("names come from a zip, so they fit");

        directory.extend_from_slice(b"PK\x01\x02");
        directory.extend_from_slice(&63u16.to_le_bytes());
        directory.extend_from_slice(&fields);
        for half in [name_len, 0, 0, 0, 0] {
            directory.extend_from_slice(&half.to_le_bytes());
        }
        directory.extend_from_slice(&0u32.to_le_bytes());
        directory.extend_from_slice(&to_u32(local.len()).to_le_bytes());
        directory.extend_from_slice(&entry.name);

        local.extend_from_slice(b"PK\x03\x04");
        local.extend_from_slice(&fields);
        local.extend_from_slice(&name_len.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(&entry.name);
        local.extend_from_slice(data);
    }
    let count = u16::try_from(entries.len()).expect("entries come from a zip, so they fit");
    let (directory_len, directory_at) = (to_u32(directory.len()), to_u32(local.len()));
    let mut pak = local;
    pak.append(&mut directory);
    pak.extend_from_slice(b"PK\x05\x06");
    for half in [0, 0, count, count] {
        pak.extend_from_slice(&half.to_le_bytes());
    }
    pak.extend_from_slice(&directory_len.to_le_bytes());
    pak.extend_from_slice(&directory_at.to_le_bytes());
    pak.extend_from_slice(&0u16.to_le_bytes());
    pak
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = flate2::Crc::new();
    crc.update(data);
    crc.sum()
}

/// Zip fields are 32-bit; maps are capped well below 4 GB before they get here.
fn to_u32(len: usize) -> u32 {
    u32::try_from(len).expect("a pakfile under 4 GB")
}
