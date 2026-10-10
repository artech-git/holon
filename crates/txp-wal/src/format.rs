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

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> SegmentHeader {
        SegmentHeader { version: 1, segment_id: 7, base_lsn: Lsn(42), created_unix: 1_700_000_000 }
    }

    #[test]
    fn header_roundtrip_and_rejections() {
        let b = header().encode();
        assert_eq!(SegmentHeader::decode(&b), Some(header()));
        assert_eq!(SegmentHeader::decode(&b[..HEADER_LEN - 1]), None, "short");
        let mut bad_magic = b;
        bad_magic[0] ^= 1;
        assert_eq!(SegmentHeader::decode(&bad_magic), None, "magic");
        let mut bad_crc = b;
        bad_crc[20] ^= 1;
        assert_eq!(SegmentHeader::decode(&bad_crc), None, "crc");
    }

    #[test]
    fn every_kind_of_bad_frame_is_explained() {
        let rec = LogRecord::Done { txid: txp_core::TxId(1) };
        let good = encode_record(Lsn(3), &rec);
        assert_eq!(decode_record(&good), Frame::Ok { lsn: Lsn(3), rec, consumed: good.len() });
        assert_eq!(decode_record(&[]), Frame::End);
        assert_eq!(decode_record(&[0; 100]), Frame::End);
        assert_eq!(decode_record(&[1, 0]), Frame::Bad("short tail".into()));
        let mut zero_len_garbage = vec![0u8; 8];
        zero_len_garbage[6] = 9;
        assert_eq!(decode_record(&zero_len_garbage), Frame::Bad("zero length".into()));
        assert_eq!(decode_record(&5u32.to_le_bytes()), Frame::Bad("implausible length 5".into()));
        assert_eq!(decode_record(&good[..good.len() - 1]), Frame::Bad("truncated record".into()));
        let mut flipped = good.clone();
        *flipped.last_mut().unwrap() ^= 1;
        assert_eq!(decode_record(&flipped), Frame::Bad("crc mismatch".into()));

        // A frame whose checksum is right but whose payload is not a record.
        let mut body = 3u64.to_le_bytes().to_vec();
        body.extend_from_slice(b"not json");
        let mut frame = ((4 + body.len()) as u32).to_le_bytes().to_vec();
        frame.extend_from_slice(&crc32c::crc32c(&body).to_le_bytes());
        frame.extend_from_slice(&body);
        assert!(matches!(decode_record(&frame), Frame::Bad(r) if r.starts_with("payload decode:")));
    }
}
