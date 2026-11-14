//! Record framing.
//!
//! A frame is: `varint(len(payload)) || payload || crc32(payload) (LE u32)`.
//! A record payload is: `tag (u8) || varint(len(key)) || key || [varint(len(value)) || value]`.

pub mod crc;
pub mod varint;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    Truncated,
    VarintOverflow,
    BadChecksum { expected: u32, actual: u32 },
    UnknownTag(u8),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::Truncated => write!(f, "truncated input"),
            WireError::VarintOverflow => write!(f, "varint does not fit in u64"),
            WireError::BadChecksum { expected, actual } => {
                write!(
                    f,
                    "checksum mismatch: expected {expected:08x}, got {actual:08x}"
                )
            }
            WireError::UnknownTag(t) => write!(f, "unknown record tag {t}"),
        }
    }
}

impl std::error::Error for WireError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    Put { key: Vec<u8>, value: Vec<u8> },
    Delete { key: Vec<u8> },
}

const TAG_PUT: u8 = 1;
const TAG_DELETE: u8 = 2;

impl Record {
    pub fn key(&self) -> &[u8] {
        match self {
            Record::Put { key, .. } | Record::Delete { key } => key,
        }
    }

    fn encode_payload(&self, out: &mut Vec<u8>) {
        match self {
            Record::Put { key, value } => {
                out.push(TAG_PUT);
                varint::write(out, key.len() as u64);
                out.extend_from_slice(key);
                varint::write(out, value.len() as u64);
                out.extend_from_slice(value);
            }
            Record::Delete { key } => {
                out.push(TAG_DELETE);
                varint::write(out, key.len() as u64);
                out.extend_from_slice(key);
            }
        }
    }

    fn decode_payload(buf: &[u8]) -> Result<Record, WireError> {
        let (&tag, mut rest) = buf.split_first().ok_or(WireError::Truncated)?;
        let key = take_bytes(&mut rest)?;
        match tag {
            TAG_PUT => {
                let value = take_bytes(&mut rest)?;
                Ok(Record::Put { key, value })
            }
            TAG_DELETE => Ok(Record::Delete { key }),
            other => Err(WireError::UnknownTag(other)),
        }
    }
}

fn take_bytes(rest: &mut &[u8]) -> Result<Vec<u8>, WireError> {
    let (len, used) = varint::read(rest)?;
    *rest = &rest[used..];
    let len = len as usize;
    if rest.len() < len {
        return Err(WireError::Truncated);
    }
    let (bytes, tail) = rest.split_at(len);
    *rest = tail;
    Ok(bytes.to_vec())
}

/// Append one framed record to `out`.
pub fn encode_frame(record: &Record, out: &mut Vec<u8>) {
    let mut payload = Vec::new();
    record.encode_payload(&mut payload);
    varint::write(out, payload.len() as u64);
    out.extend_from_slice(&payload);
    out.extend_from_slice(&crc::crc32(&payload).to_le_bytes());
}

/// Decode one frame from the front of `buf`; returns the record and the
/// number of bytes consumed.
pub fn decode_frame(buf: &[u8]) -> Result<(Record, usize), WireError> {
    let (len, used) = varint::read(buf)?;
    let len = len as usize;
    let end = used + len;
    if buf.len() < end + 4 {
        return Err(WireError::Truncated);
    }
    let payload = &buf[used..end];
    let stored = u32::from_le_bytes(buf[end..end + 4].try_into().expect("4 bytes"));
    let actual = crc::crc32(payload);
    if stored != actual {
        return Err(WireError::BadChecksum {
            expected: stored,
            actual,
        });
    }
    Ok((Record::decode_payload(payload)?, end + 4))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip_small() {
        let recs = vec![
            Record::Put {
                key: b"a".to_vec(),
                value: b"hello".to_vec(),
            },
            Record::Delete { key: b"a".to_vec() },
        ];
        let mut buf = Vec::new();
        for r in &recs {
            encode_frame(r, &mut buf);
        }
        let (r1, n1) = decode_frame(&buf).unwrap();
        let (r2, n2) = decode_frame(&buf[n1..]).unwrap();
        assert_eq!(vec![r1, r2], recs);
        assert_eq!(n1 + n2, buf.len());
    }

    #[test]
    fn detects_corruption() {
        let mut buf = Vec::new();
        encode_frame(
            &Record::Put {
                key: b"k".to_vec(),
                value: b"v".to_vec(),
            },
            &mut buf,
        );
        let last = buf.len() - 5;
        buf[last] ^= 0xff;
        assert!(matches!(
            decode_frame(&buf),
            Err(WireError::BadChecksum { .. })
        ));
    }

    #[test]
    fn detects_truncation() {
        let mut buf = Vec::new();
        encode_frame(
            &Record::Delete {
                key: b"gone".to_vec(),
            },
            &mut buf,
        );
        buf.pop();
        assert_eq!(decode_frame(&buf), Err(WireError::Truncated));
    }
}
