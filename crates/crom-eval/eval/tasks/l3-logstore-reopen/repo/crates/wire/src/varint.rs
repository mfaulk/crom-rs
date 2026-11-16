//! Unsigned LEB128 varints: 7 bits per byte, least-significant group first,
//! high bit set on every byte except the last.

use crate::WireError;

pub fn write(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Decode a varint from the front of `buf`; returns `(value, bytes_used)`.
pub fn read(buf: &[u8]) -> Result<(u64, usize), WireError> {
    let mut value: u64 = 0;
    for (i, &byte) in buf.iter().enumerate() {
        if i >= 10 {
            return Err(WireError::VarintOverflow);
        }
        value |= ((byte & 0x7f) as u64) << (8 * i);
        if byte & 0x80 == 0 {
            return Ok((value, i + 1));
        }
    }
    Err(WireError::Truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_byte_values() {
        for v in [0u64, 1, 42, 127] {
            let mut buf = Vec::new();
            write(&mut buf, v);
            assert_eq!(buf.len(), 1);
            assert_eq!(read(&buf).unwrap(), (v, 1));
        }
    }

    #[test]
    fn encodes_multi_byte() {
        let mut buf = Vec::new();
        write(&mut buf, 300);
        assert_eq!(buf, [0xAC, 0x02]);
    }

    #[test]
    fn truncated_input() {
        assert_eq!(read(&[0x80]), Err(WireError::Truncated));
        assert_eq!(read(&[]), Err(WireError::Truncated));
    }
}
