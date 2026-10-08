//! On-disk framing: the 64-byte segment header and the
//! `[len][crc32c][lsn][json]` record frame, plus the decoder that tells a
//! clean end from a torn write.

use txp_core::{LogRecord, Lsn};

/// Magic bytes at offset 0 of every segment; the trailing digits are the
/// format generation.
pub const MAGIC: &[u8; 8] = b"TXPWAL01";
/// Size of the segment header. Records start at this offset.
pub const HEADER_LEN: usize = 64;
/// Bytes of framing per record: `len` + `crc32c` + `lsn`.
pub const RECORD_HDR: usize = 4 + 4 + 8;
/// Largest `len` the decoder accepts; anything bigger is treated as garbage.
pub const MAX_RECORD: u32 = 16 * 1024 * 1024;

/// Fixed-size header written once when a segment is created and never
/// rewritten. Protected by its own CRC32C.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentHeader {
    /// Header/record format version (currently 1).
    pub version: u32,
    /// Monotonic segment number, also encoded in the file name.
    pub segment_id: u64,
    /// LSN the first record in this segment will carry.
    pub base_lsn: Lsn,
    /// Creation time in seconds since the Unix epoch (informational).
    pub created_unix: u64,
}

impl SegmentHeader {
    /// Serialize to the on-disk layout, zero-padded to [`HEADER_LEN`].
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0..8].copy_from_slice(MAGIC);
        b[8..12].copy_from_slice(&self.version.to_le_bytes());
        b[12..20].copy_from_slice(&self.segment_id.to_le_bytes());
        b[20..28].copy_from_slice(&self.base_lsn.0.to_le_bytes());
        b[28..36].copy_from_slice(&self.created_unix.to_le_bytes());
        let crc = crc32c::crc32c(&b[0..36]);
        b[36..40].copy_from_slice(&crc.to_le_bytes());
        b
    }

    /// Parse a header. Returns `None` on short input, wrong magic or CRC mismatch.
    pub fn decode(b: &[u8]) -> Option<SegmentHeader> {
        if b.len() < HEADER_LEN || &b[0..8] != MAGIC {
            return None;
        }
        let crc = u32::from_le_bytes(b[36..40].try_into().unwrap());
        if crc32c::crc32c(&b[0..36]) != crc {
            return None;
        }
        Some(SegmentHeader {
            version: u32::from_le_bytes(b[8..12].try_into().unwrap()),
            segment_id: u64::from_le_bytes(b[12..20].try_into().unwrap()),
            base_lsn: Lsn(u64::from_le_bytes(b[20..28].try_into().unwrap())),
            created_unix: u64::from_le_bytes(b[28..36].try_into().unwrap()),
        })
    }
}

/// Encode one record frame. `len` counts bytes after the len field
/// (crc + lsn + payload).
pub fn encode_record(lsn: Lsn, rec: &LogRecord) -> Vec<u8> {
    let payload = serde_json::to_vec(rec).expect("LogRecord serializes");
    let mut body = Vec::with_capacity(8 + payload.len());
    body.extend_from_slice(&lsn.0.to_le_bytes());
    body.extend_from_slice(&payload);
    let crc = crc32c::crc32c(&body);
    let len = (4 + body.len()) as u32;
    let mut out = Vec::with_capacity(4 + len as usize);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// Result of decoding the bytes at a given offset.
#[derive(Debug, PartialEq, Eq)]
pub enum Frame {
    /// A complete, checksummed record.
    Ok {
        /// Position stored in the frame.
        lsn: Lsn,
        /// Decoded payload.
        rec: LogRecord,
        /// Total frame length in bytes, including the `len` field.
        consumed: usize,
    },
    /// Zero-filled tail (preallocated space) or truncated: the clean end.
    End,
    /// Bytes present but do not decode: torn write or corruption.
    Bad(String),
}

/// Decode one frame at the start of `buf`.
pub fn decode_record(buf: &[u8]) -> Frame {
    if buf.len() < 4 {
        return if buf.iter().all(|&b| b == 0) { Frame::End } else { Frame::Bad("short tail".into()) };
    }
    let len = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    if len == 0 {
        return if buf.iter().take(64).all(|&b| b == 0) { Frame::End } else { Frame::Bad("zero length".into()) };
    }
    if !(12..=MAX_RECORD).contains(&len) {
        return Frame::Bad(format!("implausible length {len}"));
    }
    let total = 4 + len as usize;
    if buf.len() < total {
        return Frame::Bad("truncated record".into());
    }
    let crc = u32::from_le_bytes(buf[4..8].try_into().unwrap());
    let body = &buf[8..total];
    if crc32c::crc32c(body) != crc {
        return Frame::Bad("crc mismatch".into());
    }
    let lsn = Lsn(u64::from_le_bytes(body[0..8].try_into().unwrap()));
    match serde_json::from_slice::<LogRecord>(&body[8..]) {
        Ok(rec) => Frame::Ok { lsn, rec, consumed: total },
        Err(e) => Frame::Bad(format!("payload decode: {e}")),
    }
}
