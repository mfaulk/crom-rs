use wire::varint::{read, write};

#[test]
fn roundtrips_across_widths() {
    let mut values = vec![
        0u64,
        127,
        128,
        255,
        300,
        16_383,
        16_384,
        u32::MAX as u64,
        u64::MAX,
    ];
    for shift in 0..64 {
        values.push(1u64 << shift);
        values.push((1u64 << shift) - 1);
    }
    for v in values {
        let mut buf = Vec::new();
        write(&mut buf, v);
        assert_eq!(read(&buf).unwrap(), (v, buf.len()), "value {v}");
    }
}

#[test]
fn decodes_known_encoding() {
    assert_eq!(read(&[0xAC, 0x02]).unwrap(), (300, 2));
    assert_eq!(read(&[0xE5, 0x8E, 0x26, 0xFF]).unwrap(), (624_485, 3));
}
