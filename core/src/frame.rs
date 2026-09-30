//! Frame encoding: `COBS([CRC32][size-prefixed Header][pad to 8][payload]) 0x00`.

use alloc::vec::Vec;

use crate::wire::{Header as FbHeader, HeaderArgs, Kind, Status};

const CRC_LEN: usize = 4;
const PAYLOAD_ALIGN: usize = 8;

/// Owned copy of the fields of a `tether.wire.Header`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Header {
    pub kind: Kind,
    pub seq: u16,
    pub call_id: u32,
    pub service: u8,
    pub method: u8,
    pub credit: u16,
    pub status: Status,
}

impl Header {
    pub fn new(kind: Kind) -> Self {
        Self { kind, ..Default::default() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    Cobs,
    TooShort,
    Crc,
    /// The CRC passed but the header doesn't fit or doesn't verify.
    BadHeader,
    /// Non-zero padding or a payload that starts past the end of the frame.
    BadLayout,
    Overflow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub header: Header,
    pub payload: Vec<u8>,
}

/// Encodes a frame, including the trailing 0x00 delimiter.
pub fn encode(header: &Header, payload: &[u8]) -> Vec<u8> {
    let mut fbb = flatbuffers::FlatBufferBuilder::with_capacity(64);
    let h = FbHeader::create(
        &mut fbb,
        &HeaderArgs {
            kind: header.kind,
            seq: header.seq,
            call_id: header.call_id,
            service: header.service,
            method: header.method,
            credit: header.credit,
            status: header.status,
        },
    );
    fbb.finish_size_prefixed(h, None);
    let hdr = fbb.finished_data();
    debug_assert_eq!(hdr.len() % 4, 0);

    let mut body = Vec::with_capacity(CRC_LEN + hdr.len() + PAYLOAD_ALIGN + payload.len());
    body.extend_from_slice(&[0; CRC_LEN]);
    body.extend_from_slice(hdr);
    if !payload.is_empty() {
        body.resize(body.len().next_multiple_of(PAYLOAD_ALIGN), 0);
        body.extend_from_slice(payload);
    }
    let crc = crc32fast::hash(&body[CRC_LEN..]);
    body[..CRC_LEN].copy_from_slice(&crc.to_le_bytes());

    let mut wire = cobs::encode_vec(&body);
    wire.push(0);
    wire
}

/// Decodes one COBS frame (without its 0x00 delimiter).
pub fn decode(cobs_frame: &[u8]) -> Result<Frame, FrameError> {
    // A C++ receiver decodes into an 8-aligned buffer so the flatbuffers are
    // aligned in memory; the Rust flatbuffers reader doesn't need it.
    let body = cobs::decode_vec(cobs_frame).map_err(|_| FrameError::Cobs)?;
    if body.len() < CRC_LEN + 4 {
        return Err(FrameError::TooShort);
    }
    let expected = u32::from_le_bytes(body[..CRC_LEN].try_into().unwrap());
    if crc32fast::hash(&body[CRC_LEN..]) != expected {
        return Err(FrameError::Crc);
    }

    let hdr_size = u32::from_le_bytes(body[CRC_LEN..CRC_LEN + 4].try_into().unwrap()) as usize;
    let hdr_end = CRC_LEN
        .checked_add(4)
        .and_then(|n| n.checked_add(hdr_size))
        .filter(|&end| end <= body.len())
        .ok_or(FrameError::BadHeader)?;
    let h = flatbuffers::size_prefixed_root::<FbHeader>(&body[CRC_LEN..hdr_end])
        .map_err(|_| FrameError::BadHeader)?;
    let header = Header {
        kind: h.kind(),
        seq: h.seq(),
        call_id: h.call_id(),
        service: h.service(),
        method: h.method(),
        credit: h.credit(),
        status: h.status(),
    };

    let payload = if hdr_end == body.len() {
        Vec::new()
    } else {
        let start = hdr_end.next_multiple_of(PAYLOAD_ALIGN);
        if start >= body.len() || body[hdr_end..start].iter().any(|&b| b != 0) {
            return Err(FrameError::BadLayout);
        }
        body[start..].to_vec()
    };
    Ok(Frame { header, payload })
}

/// Splits a byte stream on 0x00 delimiters.
pub struct Deframer {
    buf: Vec<u8>,
    max: usize,
    overflowed: bool,
}

impl Deframer {
    pub fn new(max: usize) -> Self {
        Self { buf: Vec::new(), max, overflowed: false }
    }

    /// Feeds one byte; returns a result when a delimiter completes a frame.
    pub fn push(&mut self, byte: u8) -> Option<Result<Frame, FrameError>> {
        if byte != 0 {
            if self.buf.len() < self.max {
                self.buf.push(byte);
            } else {
                self.overflowed = true;
            }
            return None;
        }
        if core::mem::take(&mut self.overflowed) {
            self.buf.clear();
            return Some(Err(FrameError::Overflow));
        }
        if self.buf.is_empty() {
            return None;
        }
        let result = decode(&self.buf);
        self.buf.clear();
        Some(result)
    }
}
