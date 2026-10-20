//! Explicit field allocations, shared by DTO validation and registry export.
use super::{Error, ErrorCode, RawMap, RawValue, Result};
use serde::Serialize;

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Field {
    pub key: u32,
    pub name: &'static str,
    pub shape: &'static str,
    /// "required", "optional" (unknown/no report), or the literal absence default.
    pub absence: &'static str,
}
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Schema {
    pub name: &'static str,
    pub fields: &'static [Field],
}

/// A retaining wire view: the raw tree, including null/omitted/default fields,
/// owns serialization. Typed getters never become the persistence path.
pub trait WireDto: Clone + std::fmt::Debug + PartialEq {
    const SCHEMA: Schema;
    fn preflight(d: &mut minicbor::Decoder<'_>, depth: usize) -> Result<()> {
        super::cbor::skip_profile(d, depth)
    }
    fn from_raw(raw: RawMap) -> Result<Self>;
    /// Check a retained subtree without constructing another owned DTO.
    fn validate_raw(raw: &RawMap) -> Result<()> {
        Self::from_raw(raw.clone()).map(|_| ())
    }
    fn raw(&self) -> &RawMap;
    fn into_raw(self) -> RawMap;
    fn encode(&self, maximum: usize) -> Result<Vec<u8>> {
        super::cbor::encode_map(self.raw(), maximum)
    }
    /// Decode one standalone map after checking its byte budget and nested schema
    /// limits without allocating the retained tree. Stream admission remains the
    /// caller's responsibility for messages carried over a live connection.
    fn decode(bytes: &[u8], maximum: usize) -> Result<Self> {
        if bytes.len() > maximum {
            return Err(ErrorCode::LimitExceeded.into());
        }
        let mut decoder = minicbor::Decoder::new(bytes);
        Self::preflight(&mut decoder, 0)?;
        if decoder.position() != bytes.len() {
            return Err(ErrorCode::MalformedFrame.into());
        }
        Self::from_raw(super::cbor::decode_map(bytes, maximum)?)
    }
}
pub trait WireValue: Sized {
    fn preflight(d: &mut minicbor::Decoder<'_>, depth: usize) -> Result<()> {
        super::cbor::skip_profile(d, depth)
    }
    fn read(raw: &RawValue) -> Result<Self>;
    fn validate(raw: &RawValue) -> Result<()> {
        Self::read(raw).map(|_| ())
    }
    fn write(&self) -> RawValue;
}
impl<T: WireDto> WireValue for T {
    fn preflight(d: &mut minicbor::Decoder<'_>, depth: usize) -> Result<()> {
        <Self as WireDto>::preflight(d, depth)
    }
    fn validate(raw: &RawValue) -> Result<()> {
        Self::validate_raw(raw.as_map()?)
    }
    fn read(raw: &RawValue) -> Result<Self> {
        Self::from_raw(raw.as_map()?.clone())
    }
    fn write(&self) -> RawValue {
        RawValue::Map(self.raw().clone())
    }
}
impl WireValue for RawMap {
    fn validate(raw: &RawValue) -> Result<()> {
        raw.as_map().map(|_| ())
    }
    fn read(raw: &RawValue) -> Result<Self> {
        Ok(raw.as_map()?.clone())
    }
    fn write(&self) -> RawValue {
        RawValue::Map(self.clone())
    }
}
impl WireValue for RawValue {
    fn validate(_: &RawValue) -> Result<()> {
        Ok(())
    }
    fn read(raw: &RawValue) -> Result<Self> {
        Ok(raw.clone())
    }
    fn write(&self) -> RawValue {
        self.clone()
    }
}
macro_rules! unsigned {
    ($($t:ty),*) => {$(impl WireValue for $t {
        fn read(raw: &RawValue) -> Result<Self> { Self::try_from(raw.as_u64()?).map_err(|_| ErrorCode::MalformedFrame.into()) }
        fn write(&self) -> RawValue { RawValue::Uint(u64::from(*self)) }
    })*};
}
unsigned!(u8, u16, u32, u64);
macro_rules! signed {
    ($($t:ty),*) => {$(impl WireValue for $t {
        fn read(raw: &RawValue) -> Result<Self> {
            let n = match raw { RawValue::Uint(n) => i128::from(*n), RawValue::Negative(n) => -1 - i128::from(*n), _ => return Err(ErrorCode::MalformedFrame.into()) };
            Self::try_from(n).map_err(|_| ErrorCode::MalformedFrame.into())
        }
        fn write(&self) -> RawValue { if *self >= 0 { RawValue::Uint(*self as u64) } else { RawValue::Negative((-1_i128 - *self as i128) as u64) } }
    })*};
}
signed!(i32, i64);
impl WireValue for bool {
    fn read(raw: &RawValue) -> Result<Self> {
        match raw {
            RawValue::Bool(b) => Ok(*b),
            _ => Err(ErrorCode::MalformedFrame.into()),
        }
    }
    fn write(&self) -> RawValue {
        RawValue::Bool(*self)
    }
}
macro_rules! float {
    ($t:ty,$v:ident) => {
        impl WireValue for $t {
            fn read(raw: &RawValue) -> Result<Self> {
                match raw {
                    RawValue::$v(n) if n.is_finite() => Ok(*n),
                    _ => Err(ErrorCode::MalformedFrame.into()),
                }
            }
            fn write(&self) -> RawValue {
                RawValue::$v(*self)
            }
        }
    };
}
float!(f32, F32);
float!(f64, F64);
impl<T: WireValue> WireValue for Option<T> {
    fn validate(raw: &RawValue) -> Result<()> {
        if matches!(raw, RawValue::Null) {
            Ok(())
        } else {
            T::validate(raw)
        }
    }
    fn preflight(d: &mut minicbor::Decoder<'_>, depth: usize) -> Result<()> {
        if d.input().get(d.position()) == Some(&0xf6) {
            super::cbor::skip_profile(d, depth)
        } else {
            T::preflight(d, depth)
        }
    }
    fn read(raw: &RawValue) -> Result<Self> {
        if matches!(raw, RawValue::Null) {
            Ok(None)
        } else {
            T::read(raw).map(Some)
        }
    }
    fn write(&self) -> RawValue {
        self.as_ref().map_or(RawValue::Null, WireValue::write)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Bytes<const MAX: usize>(pub Vec<u8>);
impl<const MAX: usize> WireValue for Bytes<MAX> {
    fn validate(raw: &RawValue) -> Result<()> {
        match raw {
            RawValue::Bytes(b) if b.len() <= MAX => Ok(()),
            RawValue::Bytes(_) => Err(ErrorCode::LimitExceeded.into()),
            _ => Err(ErrorCode::MalformedFrame.into()),
        }
    }
    fn preflight(d: &mut minicbor::Decoder<'_>, depth: usize) -> Result<()> {
        let mut probe = d.clone();
        if probe.bytes()?.len() > MAX {
            return Err(ErrorCode::LimitExceeded.into());
        }
        super::cbor::skip_profile(d, depth)
    }
    fn read(raw: &RawValue) -> Result<Self> {
        match raw {
            RawValue::Bytes(b) if b.len() <= MAX => Ok(Self(b.clone())),
            RawValue::Bytes(_) => Err(ErrorCode::LimitExceeded.into()),
            _ => Err(ErrorCode::MalformedFrame.into()),
        }
    }
    fn write(&self) -> RawValue {
        RawValue::Bytes(self.0.clone())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FixedBytes<const N: usize>(pub [u8; N]);
impl<const N: usize> WireValue for FixedBytes<N> {
    fn preflight(d: &mut minicbor::Decoder<'_>, depth: usize) -> Result<()> {
        let mut probe = d.clone();
        if probe.bytes()?.len() != N {
            return Err(ErrorCode::MalformedFrame.into());
        }
        super::cbor::skip_profile(d, depth)
    }
    fn read(raw: &RawValue) -> Result<Self> {
        match raw {
            RawValue::Bytes(b) => Ok(Self(b.as_slice().try_into().map_err(|_| ErrorCode::MalformedFrame)?)),
            _ => Err(ErrorCode::MalformedFrame.into()),
        }
    }
    fn write(&self) -> RawValue {
        RawValue::Bytes(self.0.to_vec())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Text<const MAX: usize>(pub String);
impl<const MAX: usize> WireValue for Text<MAX> {
    fn validate(raw: &RawValue) -> Result<()> {
        match raw {
            RawValue::Text(s) if s.len() <= MAX => Ok(()),
            RawValue::Text(_) => Err(ErrorCode::LimitExceeded.into()),
            _ => Err(ErrorCode::MalformedFrame.into()),
        }
    }
    fn preflight(d: &mut minicbor::Decoder<'_>, depth: usize) -> Result<()> {
        let mut probe = d.clone();
        if probe.str()?.len() > MAX {
            return Err(ErrorCode::LimitExceeded.into());
        }
        super::cbor::skip_profile(d, depth)
    }
    fn read(raw: &RawValue) -> Result<Self> {
        match raw {
            RawValue::Text(s) if s.len() <= MAX => Ok(Self(s.clone())),
            RawValue::Text(_) => Err(ErrorCode::LimitExceeded.into()),
            _ => Err(ErrorCode::MalformedFrame.into()),
        }
    }
    fn write(&self) -> RawValue {
        RawValue::Text(self.0.clone())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ascii<const MAX: usize>(pub String);
impl<const MAX: usize> WireValue for Ascii<MAX> {
    fn validate(raw: &RawValue) -> Result<()> {
        Text::<MAX>::validate(raw)?;
        let RawValue::Text(s) = raw else { unreachable!() };
        if s.is_empty() || !s.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
            return Err(ErrorCode::MalformedFrame.into());
        }
        Ok(())
    }
    fn preflight(d: &mut minicbor::Decoder<'_>, depth: usize) -> Result<()> {
        Text::<MAX>::preflight(d, depth)
    }
    fn read(raw: &RawValue) -> Result<Self> {
        let s = Text::<MAX>::read(raw)?.0;
        if s.is_empty() || !s.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
            return Err(ErrorCode::MalformedFrame.into());
        }
        Ok(Self(s))
    }
    fn write(&self) -> RawValue {
        RawValue::Text(self.0.clone())
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct List<T, const MAX: usize>(pub Vec<T>);
impl<T: WireValue, const MAX: usize> WireValue for List<T, MAX> {
    fn validate(raw: &RawValue) -> Result<()> {
        let RawValue::Array(items) = raw else {
            return Err(ErrorCode::MalformedFrame.into());
        };
        if items.len() > MAX {
            return Err(ErrorCode::LimitExceeded.into());
        }
        for item in items {
            T::validate(item)?;
        }
        Ok(())
    }
    fn preflight(d: &mut minicbor::Decoder<'_>, depth: usize) -> Result<()> {
        if depth >= super::limits::DEPTH {
            return Err(ErrorCode::LimitExceeded.into());
        }
        let n = d.array()?.ok_or(ErrorCode::MalformedFrame)?;
        if n > MAX as u64 {
            return Err(ErrorCode::LimitExceeded.into());
        }
        if n > (d.input().len() - d.position()) as u64 {
            return Err(ErrorCode::MalformedFrame.into());
        }
        for _ in 0..n {
            T::preflight(d, depth + 1)?;
        }
        Ok(())
    }
    fn read(raw: &RawValue) -> Result<Self> {
        let RawValue::Array(items) = raw else {
            return Err(ErrorCode::MalformedFrame.into());
        };
        if items.len() > MAX {
            return Err(ErrorCode::LimitExceeded.into());
        }
        items.iter().map(T::read).collect::<Result<Vec<_>>>().map(Self)
    }
    fn write(&self) -> RawValue {
        RawValue::Array(self.0.iter().map(WireValue::write).collect())
    }
}
pub fn required<T: WireValue>(raw: &RawMap, key: u32) -> Result<T> {
    T::read(
        raw.get(&key)
            .ok_or_else(|| Error::from(ErrorCode::MalformedFrame).at_field(key))?,
    )
    .map_err(|e| e.at_field(key))
}
pub fn defaulted<T: WireValue>(raw: &RawMap, key: u32, default: impl FnOnce() -> T) -> Result<T> {
    raw.get(&key)
        .map_or_else(|| Ok(default()), |v| T::read(v).map_err(|e| e.at_field(key)))
}
pub(crate) fn no_validation(_: &RawMap) -> Result<()> {
    Ok(())
}

macro_rules! field_read {
    ($m:expr, $k:expr, $t:ty, required) => {
        $crate::v1::schema::required::<$t>($m, $k)
    };
    ($m:expr, $k:expr, $t:ty, optional) => {
        $crate::v1::schema::defaulted::<$t>($m, $k, || None)
    };
    ($m:expr, $k:expr, $t:ty, $d:expr) => {
        $crate::v1::schema::defaulted::<$t>($m, $k, || $d)
    };
}
pub(crate) use field_read;
// Validation traverses borrowed fields; only a getter needs an owned value.
macro_rules! field_validate {
    ($m:expr, $k:expr, $t:ty, required) => {
        $m.get(&$k)
            .ok_or_else(|| $crate::v1::Error::from($crate::v1::ErrorCode::MalformedFrame))
            .and_then(<$t as $crate::v1::schema::WireValue>::validate)
            .map_err(|e| e.at_field($k))
    };
    ($m:expr, $k:expr, $t:ty, $d:tt) => {
        $m.get(&$k)
            .map_or(Ok(()), <$t as $crate::v1::schema::WireValue>::validate)
            .map_err(|e| e.at_field($k))
    };
}
pub(crate) use field_validate;

// Known diagnostic kinds select their recursive body contract before any raw
// tree is materialized. Unrecognized kinds retain their bounded opaque maps.
pub(crate) fn preflight_diagnostic_event(d: &mut minicbor::Decoder<'_>, depth: usize) -> Result<()> {
    let mut kind = None;
    let mut fields = false;
    super::cbor::preflight_map(d, depth, |key, d, depth| match key {
        1 => {
            let start = d.position();
            Ascii::<96>::preflight(d, depth)?;
            let text = minicbor::Decoder::new(&d.input()[start..d.position()]).str()?;
            if text.is_empty() || !text.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) {
                return Err(ErrorCode::MalformedFrame.into());
            }
            kind = Some(super::dto::diagnostic_event_schema(text));
            Ok(())
        }
        2 => {
            fields = true;
            match kind.ok_or(ErrorCode::MalformedFrame)? {
                Some(schema) => super::registry::preflight_schema(schema.name, d, depth),
                None => super::cbor::preflight_map(d, depth, |_, d, depth| super::cbor::skip_profile(d, depth)),
            }
        }
        _ => super::cbor::skip_profile(d, depth),
    })?;
    if kind.is_none() || !fields {
        return Err(ErrorCode::MalformedFrame.into());
    }
    Ok(())
}

macro_rules! wire_map {
    ($name:ident, $validate:path, { $($key:literal => $field:ident : $ty:ty = $default:tt;)* }) => {
        #[derive(Clone, Debug, PartialEq)]
        pub struct $name($crate::v1::RawMap);
        impl $crate::v1::schema::WireDto for $name {
            const SCHEMA: $crate::v1::schema::Schema = $crate::v1::schema::Schema {
                name: stringify!($name),
                fields: &[$($crate::v1::schema::Field {key: $key, name: stringify!($field), shape: stringify!($ty), absence: stringify!($default)},)*],
            };
            fn preflight(d:&mut minicbor::Decoder<'_>,depth:usize)->$crate::v1::Result<()> {
                if stringify!($name) == "DiagnosticEvent" {
                    return $crate::v1::schema::preflight_diagnostic_event(d,depth);
                }
                let start=d.position();
                $crate::v1::cbor::preflight_map(d,depth,|key,d,depth|match key {
                    $($key=><$ty as $crate::v1::schema::WireValue>::preflight(d,depth),)*
                    _=>$crate::v1::cbor::skip_profile(d,depth),
                })?;
                $crate::v1::schema::check_dto_size(stringify!($name),d.position()-start)
            }
            fn from_raw(raw: $crate::v1::RawMap) -> $crate::v1::Result<Self> {
                <Self as $crate::v1::schema::WireDto>::validate_raw(&raw)?;
                Ok(Self(raw))
            }
            fn validate_raw(raw: &$crate::v1::RawMap) -> $crate::v1::Result<()> {
                $crate::v1::schema::check_dto_size(stringify!($name),$crate::v1::cbor::map_len(raw)?)?;
                $($crate::v1::schema::field_validate!(raw, $key, $ty, $default)?;)*
                $validate(raw)
            }
            fn raw(&self) -> &$crate::v1::RawMap { &self.0 }
            fn into_raw(self) -> $crate::v1::RawMap { self.0 }
        }
        impl $name {
            $(pub fn $field(&self) -> $crate::v1::Result<$ty> { $crate::v1::schema::field_read!(&self.0, $key, $ty, $default) })*
            /// Validate a narrow edit atomically; unchanged subtrees survive.
            pub fn edit(&mut self, patch: impl FnOnce(&mut $crate::v1::RawMap)) -> $crate::v1::Result<()> {
                let mut candidate = self.0.clone(); patch(&mut candidate);
                *self = <Self as $crate::v1::schema::WireDto>::from_raw(candidate)?; Ok(())
            }
        }
    };
}
pub(crate) use wire_map;

macro_rules! open_enum {
    ($name:ident { $($variant:ident = $number:literal),* $(,)? }) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub enum $name { $($variant,)* Unknown(u32) }
        impl $name {
            pub const fn from_number(n: u32) -> Self { match n { $($number => Self::$variant,)* n => Self::Unknown(n) } }
            pub const fn number(self) -> u32 { match self { $(Self::$variant => $number,)* Self::Unknown(n) => n } }
            pub const VALUES: &'static [(u32, &'static str)] = &[$(($number,stringify!($variant)),)*];
        }
        impl $crate::v1::hot::BorrowValue for $name {
            type Output<'a>=$name;
            fn read_borrowed(bytes:&[u8])->$crate::v1::Result<Self> {
                let n=<u32 as $crate::v1::hot::BorrowValue>::read_borrowed(bytes)?;
                if n==u32::MAX {return Err($crate::v1::ErrorCode::MalformedFrame.into());}
                Ok(Self::from_number(n))
            }
        }
        impl $crate::v1::schema::WireValue for $name {
            fn read(raw: &$crate::v1::RawValue) -> $crate::v1::Result<Self> {
                let n = <u32 as $crate::v1::schema::WireValue>::read(raw)?;
                if n == u32::MAX { return Err($crate::v1::ErrorCode::MalformedFrame.into()); }
                Ok(Self::from_number(n))
            }
            fn write(&self) -> $crate::v1::RawValue { $crate::v1::RawValue::Uint(u64::from(self.number())) }
        }
    };
}
pub(crate) use open_enum;

/// Known semantic variants and future unknown bodies are different outcomes.
#[derive(Clone, Debug, PartialEq)]
pub enum Open<T> {
    Known(T),
    Unknown { id: u32, body: RawMap },
}
pub trait EnumSchema: WireValue + Clone + std::fmt::Debug + PartialEq {
    fn known(&self) -> bool;
    fn number(&self) -> u32;
    fn validate_body(&self, body: &RawMap) -> Result<()>;
    fn preflight_body(&self, d: &mut minicbor::Decoder<'_>, depth: usize) -> Result<()>;
}
#[derive(Clone, Debug, PartialEq)]
pub struct OpenEnvelope<K: EnumSchema> {
    raw: RawMap,
    marker: std::marker::PhantomData<K>,
}
impl<K: EnumSchema> OpenEnvelope<K> {
    pub fn variant(&self) -> Result<Open<K>> {
        let kind: K = required(&self.raw, 1)?;
        if kind.known() {
            Ok(Open::Known(kind))
        } else {
            Ok(Open::Unknown {
                id: kind.number(),
                body: self.body()?,
            })
        }
    }
    pub fn body(&self) -> Result<RawMap> {
        defaulted(&self.raw, 2, RawMap::new)
    }
}
impl<K: EnumSchema> WireDto for OpenEnvelope<K> {
    const SCHEMA: Schema = Schema {
        name: "OpenEnvelope",
        fields: &[
            Field {
                key: 1,
                name: "variant",
                shape: "u32",
                absence: "required",
            },
            Field {
                key: 2,
                name: "body",
                shape: "RawMap",
                absence: "empty map for unit variants",
            },
        ],
    };
    fn preflight(d: &mut minicbor::Decoder<'_>, depth: usize) -> Result<()> {
        let mut kind = None;
        super::cbor::preflight_map(d, depth, |key, d, depth| match key {
            1 => {
                let start = d.position();
                super::cbor::skip_profile(d, depth)?;
                let number = minicbor::Decoder::new(&d.input()[start..d.position()]).u32()?;
                kind = Some(K::read(&RawValue::Uint(u64::from(number)))?);
                Ok(())
            }
            2 => kind.as_ref().ok_or(ErrorCode::MalformedFrame)?.preflight_body(d, depth),
            _ => super::cbor::skip_profile(d, depth),
        })?;
        kind.ok_or(ErrorCode::MalformedFrame)?;
        Ok(())
    }
    fn from_raw(raw: RawMap) -> Result<Self> {
        Self::validate_raw(&raw)?;
        Ok(Self {
            raw,
            marker: std::marker::PhantomData,
        })
    }
    fn validate_raw(raw: &RawMap) -> Result<()> {
        super::cbor::map_len(raw)?;
        let kind: K = required(raw, 1)?;
        let empty = RawMap::new();
        let body = match raw.get(&2) {
            Some(body) => body.as_map().map_err(|e| e.at_field(2))?,
            None => &empty,
        };
        kind.validate_body(body).map_err(|e| e.at_field(2))
    }
    fn raw(&self) -> &RawMap {
        &self.raw
    }
    fn into_raw(self) -> RawMap {
        self.raw
    }
}
macro_rules! enum_body {
    ($name:ident {$($variant:ident => $body:ty),* $(,)?}) => {
        impl $crate::v1::schema::EnumSchema for $name {
            fn known(&self) -> bool { !matches!(self, Self::Unknown(_)) }
            fn number(&self) -> u32 { (*self).number() }
            fn validate_body(&self, body: &$crate::v1::RawMap) -> $crate::v1::Result<()> {
                match self { $(Self::$variant => { <$body as $crate::v1::schema::WireDto>::validate_raw(body)?; },)* _ => {} }
                Ok(())
            }
            fn preflight_body(&self, d: &mut minicbor::Decoder<'_>, depth: usize) -> $crate::v1::Result<()> {
                match self {
                    $(Self::$variant => <$body as $crate::v1::schema::WireDto>::preflight(d, depth),)*
                    _ => $crate::v1::cbor::preflight_map(d, depth, |_, d, depth| $crate::v1::cbor::skip_profile(d, depth)),
                }
            }
        }
    };
}
pub(crate) use enum_body;

pub(crate) fn check_dto_size(name: &str, size: usize) -> Result<()> {
    let limit = match name {
        "Inventory" => 8 * 1024 * 1024,
        "DirectoryPage" => 1024 * 1024,
        "DeviceFacts" => super::limits::CONFIG_FRAME - 6,
        "ResourceFacts" => super::limits::RESOURCE_FACTS,
        "ExitVolumeState" => super::limits::EXIT_VOLUME_STATE,
        "LocalPauseState" => super::limits::LOCAL_PAUSE_STATE,
        "SetLocalOption" | "LocalEditResult" | "SetExitVolume" | "ExitVolumeResult" | "ExitVolumeChanged"
        | "EmergencyPause" | "EmergencyPauseAck" | "PauseDevice" | "PauseDeviceResult" => {
            super::limits::CONTROL_FRAME - 6
        }
        _ => usize::MAX,
    };
    if size > limit {
        return Err(ErrorCode::LimitExceeded.into());
    }
    Ok(())
}

#[cfg(test)]
#[allow(dead_code)] // The fixture DTOs also generate getters and edit methods.
mod validation_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static READS: AtomicUsize = AtomicUsize::new(0);
    static CHECKS: AtomicUsize = AtomicUsize::new(0);
    pub(super) struct Counted;
    impl WireValue for Counted {
        fn read(raw: &RawValue) -> Result<Self> {
            READS.fetch_add(1, Ordering::Relaxed);
            Self::validate(raw)?;
            Ok(Self)
        }
        fn validate(raw: &RawValue) -> Result<()> {
            CHECKS.fetch_add(1, Ordering::Relaxed);
            u32::read(raw).map(|_| ())
        }
        fn write(&self) -> RawValue {
            RawValue::Uint(1)
        }
    }
    wire_map!(Leaf, no_validation, { 1 => value: Counted = required; });
    wire_map!(Middle, no_validation, { 2 => leaves: List<Leaf, 600> = required; });
    wire_map!(Outer, no_validation, {
        3 => middle: Middle = required;
        4 => optional: Option<Middle> = optional;
        5 => default: bool = false;
    });

    #[test]
    fn nested_validation_checks_each_leaf_once_without_constructing_owned_values() {
        READS.store(0, Ordering::Relaxed);
        CHECKS.store(0, Ordering::Relaxed);
        let leaves = (0..600)
            .map(|_| RawValue::Map([(1, RawValue::Uint(1))].into()))
            .collect();
        let middle = RawValue::Map([(2, RawValue::Array(leaves))].into());
        let raw: RawMap = [(3, middle), (900, RawValue::Text("retained".into()))].into();
        let value = Outer::from_raw(raw.clone()).unwrap();
        assert_eq!(value.raw(), &raw);
        assert_eq!(READS.load(Ordering::Relaxed), 0);
        assert_eq!(CHECKS.load(Ordering::Relaxed), 600);

        let mut bad = raw.clone();
        let RawValue::Map(middle) = bad.get_mut(&3).unwrap() else {
            panic!()
        };
        let RawValue::Array(leaves) = middle.get_mut(&2).unwrap() else {
            panic!()
        };
        leaves[0] = RawValue::Map([(1, RawValue::Bool(false))].into());
        let error = Outer::from_raw(bad).unwrap_err();
        assert_eq!(error.code, ErrorCode::MalformedFrame);
        assert_eq!(error.field_path, vec![3, 2, 1]);
        assert_eq!(Outer::from_raw(RawMap::new()).unwrap_err().field_path, vec![3]);

        // Explicit null is accepted only by Option, never by a defaulted bool.
        let mut explicit = raw;
        explicit.insert(4, RawValue::Null);
        Outer::from_raw(explicit.clone()).unwrap();
        explicit.insert(5, RawValue::Null);
        assert_eq!(Outer::from_raw(explicit).unwrap_err().field_path, vec![5]);
    }
}
