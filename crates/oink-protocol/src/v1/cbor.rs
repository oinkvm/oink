//! Restrictive Oink profile on minicbor's scalar codec, without Serde or derive.
//! Accepted values have one unique representation (float widths stay distinct),
//! so the recursive tree reproduces every accepted unknown value byte-for-byte.
use super::{Error, ErrorCode, Result, limits};
use minicbor::{Decoder, Encoder};
use std::collections::BTreeMap;

pub type RawMap = BTreeMap<u32, RawValue>;

#[derive(Clone, Debug, PartialEq)]
pub enum RawValue {
    Uint(u64),
    /// CBOR negative integer: mathematical value -1 - argument, including -2^64.
    Negative(u64),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Self>),
    Map(RawMap),
    Bool(bool),
    Null,
    F32(f32),
    F64(f64),
}
impl From<RawMap> for RawValue {
    fn from(v: RawMap) -> Self {
        Self::Map(v)
    }
}
impl From<u64> for RawValue {
    fn from(v: u64) -> Self {
        Self::Uint(v)
    }
}
impl From<bool> for RawValue {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}
impl From<&str> for RawValue {
    fn from(v: &str) -> Self {
        Self::Text(v.to_owned())
    }
}
impl RawValue {
    pub fn as_map(&self) -> Result<&RawMap> {
        match self {
            Self::Map(m) => Ok(m),
            _ => Err(ErrorCode::MalformedFrame.into()),
        }
    }
    pub fn as_u64(&self) -> Result<u64> {
        match self {
            Self::Uint(n) => Ok(*n),
            _ => Err(ErrorCode::MalformedFrame.into()),
        }
    }
    pub fn encode(&self, maximum: usize) -> Result<Vec<u8>> {
        let len = self.encoded_len()?;
        if len > maximum {
            return Err(ErrorCode::LimitExceeded.into());
        }
        let mut out = Encoder::new(Vec::with_capacity(len));
        encode_value(self, &mut out);
        Ok(out.into_writer())
    }
    pub fn encoded_len(&self) -> Result<usize> {
        value_len(self, 0)
    }
}

/// Validate/size a map without cloning its retained subtrees.
pub fn map_len(map: &RawMap) -> Result<usize> {
    map_len_at(map, 0)
}
fn map_len_at(map: &RawMap, depth: usize) -> Result<usize> {
    if depth >= limits::DEPTH || map.len() > limits::MAP_KEYS {
        return Err(ErrorCode::LimitExceeded.into());
    }
    let mut len = head_len(map.len() as u64);
    for (key, value) in map {
        if *key == 0 || *key == u32::MAX {
            return Err(ErrorCode::MalformedFrame.into());
        }
        len = len
            .checked_add(head_len(u64::from(*key)) + value_len(value, depth + 1)?)
            .ok_or(ErrorCode::LimitExceeded)?;
    }
    Ok(len)
}
pub fn encode_map(map: &RawMap, maximum: usize) -> Result<Vec<u8>> {
    let len = map_len(map)?;
    if len > maximum {
        return Err(ErrorCode::LimitExceeded.into());
    }
    let mut encoder = Encoder::new(Vec::with_capacity(len));
    encode_map_to(map, &mut encoder);
    Ok(encoder.into_writer())
}
fn encode_map_to(map: &RawMap, encoder: &mut Encoder<Vec<u8>>) {
    encoder.map(map.len() as u64).unwrap();
    for (key, value) in map {
        encoder.u32(*key).unwrap();
        encode_value(value, encoder);
    }
}

fn head_len(n: u64) -> usize {
    match n {
        0..=23 => 1,
        24..=255 => 2,
        256..=65535 => 3,
        65536..=4294967295 => 5,
        _ => 9,
    }
}
fn value_len(v: &RawValue, depth: usize) -> Result<usize> {
    if depth >= limits::DEPTH {
        return Err(ErrorCode::LimitExceeded.into());
    }
    let size = match v {
        RawValue::Uint(n) | RawValue::Negative(n) => head_len(*n),
        RawValue::Bytes(b) => head_len(b.len() as u64) + b.len(),
        RawValue::Text(s) => head_len(s.len() as u64) + s.len(),
        RawValue::Bool(_) | RawValue::Null => 1,
        RawValue::F32(n) => {
            if !n.is_finite() {
                return Err(ErrorCode::UnsupportedValue.into());
            }
            5
        }
        RawValue::F64(n) => {
            if !n.is_finite() {
                return Err(ErrorCode::UnsupportedValue.into());
            }
            9
        }
        RawValue::Array(a) => {
            let mut len = head_len(a.len() as u64);
            for x in a {
                len = len
                    .checked_add(value_len(x, depth + 1)?)
                    .ok_or(ErrorCode::LimitExceeded)?;
            }
            len
        }
        RawValue::Map(m) => map_len_at(m, depth)?,
    };
    Ok(size)
}
fn encode_value(v: &RawValue, e: &mut Encoder<Vec<u8>>) {
    // Vec's minicbor writer is infallible. All profile/budget validation precedes allocation.
    match v {
        RawValue::Uint(n) => {
            e.u64(*n).unwrap();
        }
        RawValue::Negative(n) => {
            e.int(minicbor::data::Int::try_from(-1_i128 - i128::from(*n)).unwrap())
                .unwrap();
        }
        RawValue::Bytes(b) => {
            e.bytes(b).unwrap();
        }
        RawValue::Text(s) => {
            e.str(s).unwrap();
        }
        RawValue::Bool(b) => {
            e.bool(*b).unwrap();
        }
        RawValue::Null => {
            e.null().unwrap();
        }
        RawValue::F32(n) => {
            e.f32(if *n == 0.0 { 0.0 } else { *n }).unwrap();
        }
        RawValue::F64(n) => {
            e.f64(if *n == 0.0 { 0.0 } else { *n }).unwrap();
        }
        RawValue::Array(a) => {
            e.array(a.len() as u64).unwrap();
            for x in a {
                encode_value(x, e);
            }
        }
        RawValue::Map(m) => encode_map_to(m, e),
    }
}

/// Decode one complete value. Before allocating any tree nodes, check its body
/// budget; container declarations are also checked against remaining input bytes.
pub fn decode(bytes: &[u8], maximum: usize) -> Result<RawValue> {
    if bytes.len() > maximum {
        return Err(ErrorCode::LimitExceeded.into());
    }
    let mut decoder = Decoder::new(bytes);
    let value = decode_value(&mut decoder, 0, true)?;
    if decoder.position() != bytes.len() {
        return Err(ErrorCode::MalformedFrame.into());
    }
    Ok(value)
}
pub fn decode_map(bytes: &[u8], maximum: usize) -> Result<RawMap> {
    match decode(bytes, maximum)? {
        RawValue::Map(m) => Ok(m),
        _ => Err(ErrorCode::MalformedFrame.into()),
    }
}
fn shortest(d: &Decoder<'_>, start: usize, n: u64) -> Result<()> {
    if d.position() - start != head_len(n) {
        return Err(Error::from(ErrorCode::MalformedFrame).at_offset(start));
    }
    Ok(())
}
fn count(d: &Decoder<'_>, n: u64, unit: usize, maximum: usize) -> Result<usize> {
    let n = usize::try_from(n).map_err(|_| ErrorCode::LimitExceeded)?;
    if n > maximum {
        return Err(ErrorCode::LimitExceeded.into());
    }
    if n > (d.input().len() - d.position()) / unit {
        return Err(ErrorCode::MalformedFrame.into());
    }
    Ok(n)
}
fn decode_value(d: &mut Decoder<'_>, depth: usize, materialize: bool) -> Result<RawValue> {
    if depth >= limits::DEPTH {
        return Err(ErrorCode::LimitExceeded.into());
    }
    let start = d.position();
    let byte = *d.input().get(start).ok_or(ErrorCode::MalformedFrame)?;
    let major = byte >> 5;
    let value = match major {
        0 => {
            let n = d.u64()?;
            shortest(d, start, n)?;
            RawValue::Uint(n)
        }
        1 => {
            let n = u64::try_from(-1_i128 - i128::from(d.int()?)).map_err(|_| ErrorCode::MalformedFrame)?;
            shortest(d, start, n)?;
            RawValue::Negative(n)
        }
        2 => {
            let b = d.bytes()?;
            if d.position() - start - b.len() != head_len(b.len() as u64) {
                return Err(ErrorCode::MalformedFrame.into());
            }
            if materialize {
                RawValue::Bytes(b.to_vec())
            } else {
                RawValue::Null
            }
        }
        3 => {
            let s = d.str()?;
            if d.position() - start - s.len() != head_len(s.len() as u64) {
                return Err(ErrorCode::MalformedFrame.into());
            }
            if materialize {
                RawValue::Text(s.to_owned())
            } else {
                RawValue::Null
            }
        }
        4 => {
            let n = d.array()?.ok_or(ErrorCode::MalformedFrame)?;
            shortest(d, start, n)?;
            let n = count(d, n, 1, d.input().len())?;
            let mut a = Vec::with_capacity(if materialize { n } else { 0 });
            for _ in 0..n {
                let value = decode_value(d, depth + 1, materialize)?;
                if materialize {
                    a.push(value);
                }
            }
            RawValue::Array(a)
        }
        5 => {
            let n = d.map()?.ok_or(ErrorCode::MalformedFrame)?;
            shortest(d, start, n)?;
            let n = count(d, n, 2, limits::MAP_KEYS)?;
            let mut m = BTreeMap::new();
            let mut previous = 0;
            for _ in 0..n {
                let at = d.position();
                let key = d.u32()?;
                shortest(d, at, u64::from(key))?;
                if key <= previous || key == u32::MAX {
                    return Err(ErrorCode::MalformedFrame.into());
                }
                previous = key;
                let value = decode_value(d, depth + 1, materialize).map_err(|e| e.at_field(key))?;
                if materialize {
                    m.insert(key, value);
                }
            }
            RawValue::Map(m)
        }
        7 => match byte {
            0xf4 | 0xf5 => RawValue::Bool(d.bool()?),
            0xf6 => {
                d.null()?;
                RawValue::Null
            }
            0xfa => {
                let n = d.f32()?;
                if !n.is_finite() || (n == 0.0 && n.is_sign_negative()) {
                    return Err(ErrorCode::MalformedFrame.into());
                }
                RawValue::F32(n)
            }
            0xfb => {
                let n = d.f64()?;
                if !n.is_finite() || (n == 0.0 && n.is_sign_negative()) {
                    return Err(ErrorCode::MalformedFrame.into());
                }
                RawValue::F64(n)
            }
            _ => return Err(ErrorCode::MalformedFrame.into()),
        },
        _ => return Err(ErrorCode::MalformedFrame.into()),
    };
    Ok(value)
}

/// Allocation-free profile scan for unknown fields during typed preflight.
pub fn skip_profile(d: &mut Decoder<'_>, depth: usize) -> Result<()> {
    decode_value(d, depth, false).map(|_| ())
}
/// Known DTOs walk field declarations before materializing the retaining tree,
/// so declared list/string/byte limits are checked before any payload allocation.
pub fn preflight_map(
    d: &mut Decoder<'_>,
    depth: usize,
    mut field: impl FnMut(u32, &mut Decoder<'_>, usize) -> Result<()>,
) -> Result<()> {
    if depth >= limits::DEPTH {
        return Err(ErrorCode::LimitExceeded.into());
    }
    let start = d.position();
    let n = d.map()?.ok_or(ErrorCode::MalformedFrame)?;
    shortest(d, start, n)?;
    let n = count(d, n, 2, limits::MAP_KEYS)?;
    let mut previous = 0;
    for _ in 0..n {
        let start = d.position();
        let key = d.u32()?;
        shortest(d, start, u64::from(key))?;
        if key <= previous || key == u32::MAX {
            return Err(ErrorCode::MalformedFrame.into());
        }
        previous = key;
        field(key, d, depth + 1).map_err(|e| e.at_field(key))?;
    }
    Ok(())
}
