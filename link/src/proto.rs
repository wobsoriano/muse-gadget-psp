//! The few protobuf wire rules the Muse envelopes use.

use alloc::vec::Vec;

const VARINT: u8 = 0;
const FIXED64: u8 = 1;
const DELIMITED: u8 = 2;
const FIXED32: u8 = 5;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Malformed;

pub(crate) fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            return out.push(byte);
        }
        out.push(byte | 0x80);
    }
}

pub(crate) fn varint_len(value: u64) -> usize {
    (64 - (value | 1).leading_zeros() as usize).div_ceil(7)
}

pub(crate) fn put_uint(out: &mut Vec<u8>, field: u32, value: u64) {
    put_varint(out, u64::from(field) << 3 | u64::from(VARINT));
    put_varint(out, value);
}

/// The key and length of a delimited field. Its `len` bytes go next.
pub(crate) fn put_delimited(out: &mut Vec<u8>, field: u32, len: usize) {
    put_varint(out, u64::from(field) << 3 | u64::from(DELIMITED));
    put_varint(out, len as u64);
}

pub(crate) fn put_bytes(out: &mut Vec<u8>, field: u32, data: &[u8]) {
    put_delimited(out, field, data.len());
    out.extend_from_slice(data);
}

/// How long `put_bytes` makes a field of `len` bytes, for fields under 16.
pub(crate) fn bytes_len(len: usize) -> usize {
    1 + varint_len(len as u64) + len
}

#[derive(Clone, Copy)]
pub(crate) enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

impl<'a> Value<'a> {
    /// A field arriving as the wrong kind is a broken message, not a missing field.
    pub(crate) fn varint(self) -> Result<u64, Malformed> {
        match self {
            Value::Varint(value) => Ok(value),
            Value::Bytes(_) => Err(Malformed),
        }
    }

    pub(crate) fn bytes(self) -> Result<&'a [u8], Malformed> {
        match self {
            Value::Bytes(data) => Ok(data),
            Value::Varint(_) => Err(Malformed),
        }
    }
}

fn read_varint(data: &[u8], at: &mut usize) -> Result<u64, Malformed> {
    let mut value = 0u64;
    for index in 0..10 {
        let byte = *data.get(*at).ok_or(Malformed)?;
        *at += 1;
        if index == 9 && byte & 0xFE != 0 {
            return Err(Malformed);
        }
        value |= u64::from(byte & 0x7F) << (7 * index);
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(Malformed)
}

fn take<'a>(data: &'a [u8], at: &mut usize, len: u64) -> Result<&'a [u8], Malformed> {
    let len = usize::try_from(len).map_err(|_| Malformed)?;
    let end = at.checked_add(len).ok_or(Malformed)?;
    let taken = data.get(*at..end).ok_or(Malformed)?;
    *at = end;
    Ok(taken)
}

/// Hands every varint and delimited field of `data` to `each`, in order. The
/// caller keeps the last one it sees of each number, as protobuf says to.
pub(crate) fn fields<'a>(
    data: &'a [u8],
    mut each: impl FnMut(u32, Value<'a>) -> Result<(), Malformed>,
) -> Result<(), Malformed> {
    let mut at = 0;
    while at < data.len() {
        let key = read_varint(data, &mut at)?;
        let number = key >> 3;
        if number == 0 || number > (1 << 29) - 1 || (19000..=19999).contains(&number) {
            return Err(Malformed);
        }
        match (key & 7) as u8 {
            VARINT => each(number as u32, Value::Varint(read_varint(data, &mut at)?))?,
            DELIMITED => {
                let len = read_varint(data, &mut at)?;
                each(number as u32, Value::Bytes(take(data, &mut at, len)?))?
            }
            FIXED64 => drop(take(data, &mut at, 8)?),
            FIXED32 => drop(take(data, &mut at, 4)?),
            _ => return Err(Malformed),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_round_trip_at_every_length() {
        for value in [0, 1, 127, 128, 300, 1 << 32, u64::MAX] {
            let mut out = Vec::new();
            put_varint(&mut out, value);
            assert_eq!(out.len(), varint_len(value), "length of {value}");
            let mut at = 0;
            assert_eq!(read_varint(&out, &mut at), Ok(value));
            assert_eq!(at, out.len());
        }
        assert_eq!(read_varint(&[0x80], &mut 0), Err(Malformed), "cut short");
        assert_eq!(read_varint(&[0xFF; 11], &mut 0), Err(Malformed), "too long");
    }

    #[test]
    fn reads_known_fields_and_skips_the_rest() {
        let mut data = Vec::new();
        put_uint(&mut data, 1, 150);
        data.extend_from_slice(&[0x4D, 1, 2, 3, 4]); // field 9, fixed32
        put_bytes(&mut data, 4, b"abc");
        assert_eq!(data.len(), 3 + 5 + bytes_len(3));
        let mut seen = Vec::new();
        fields(&data, |number, value| {
            seen.push(match value {
                Value::Varint(v) => (number, v, &b""[..]),
                Value::Bytes(b) => (number, 0, b),
            });
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, [(1, 150, &b""[..]), (4, 0, &b"abc"[..])]);
    }

    #[test]
    fn rejects_broken_input() {
        let nothing = |_, _| Ok(());
        assert_eq!(fields(&[0x22, 5, 1], nothing), Err(Malformed), "length past the end");
        assert_eq!(fields(&[0x00, 0], nothing), Err(Malformed), "field zero");
        assert_eq!(fields(&[0x0B], nothing), Err(Malformed), "group wire type");
        assert!(Value::Varint(1).bytes().is_err() && Value::Bytes(b"").varint().is_err());
    }
}
