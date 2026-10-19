//! Borrowed projections of registered input/media DTOs. No new wire messages.
//! Call endpoint admission first. Decode, typed reads and encode into a caller's
//! slice allocate nothing; unknown fields remain borrowed, including nested ones.
//! Owning a retaining tree is an explicit cold-path operation (`Map::to_owned`).
use super::{ErrorCode, RawMap, Result, cbor, dto::*, frame::*, limits, schema::*};
use minicbor::{Decoder, Encoder};
use std::marker::PhantomData;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Map<'a> {
    bytes: &'a [u8],
}
impl<'a> Map<'a> {
    /// Validate an entire profile map before allowing allocation-free traversal.
    pub fn parse(bytes: &'a [u8], maximum: usize) -> Result<Self> {
        if bytes.len() > maximum {
            return Err(ErrorCode::LimitExceeded.into());
        }
        let mut d = Decoder::new(bytes);
        cbor::preflight_map(&mut d, 0, |_, d, depth| cbor::skip_profile(d, depth))?;
        if d.position() != bytes.len() {
            return Err(ErrorCode::MalformedFrame.into());
        }
        Ok(Self { bytes })
    }
    pub fn bytes(self) -> &'a [u8] {
        self.bytes
    }
    pub fn to_owned(self) -> Result<RawMap> {
        cbor::decode_map(self.bytes, self.bytes.len())
    }
    pub fn field(self, key: u32) -> Result<Option<&'a [u8]>> {
        let mut d = Decoder::new(self.bytes);
        let n = d.map()?.ok_or(ErrorCode::MalformedFrame)?;
        for _ in 0..n {
            let k = d.u32()?;
            if k > key {
                break;
            }
            let start = d.position();
            // The Oink profile already excluded indefinite containers. minicbor
            // skip uses no heap stack for definite containers, at any depth.
            d.skip()?;
            if k == key {
                return Ok(Some(&self.bytes[start..d.position()]));
            }
        }
        Ok(None)
    }
    pub fn required<T: BorrowValue>(self, key: u32) -> Result<T::Output<'a>> {
        T::read_borrowed(self.field(key)?.ok_or(ErrorCode::MalformedFrame)?).map_err(|e| e.at_field(key))
    }
}
// Cache the current hot family's common scalar field spans on the stack.
// Future appended keys still work through Map::field without changing the wire.
fn slot(key: u32) -> Option<usize> {
    match key {
        1..=7 => Some(key as usize - 1),
        100..=102 => Some(key as usize - 93),
        _ => None,
    }
}
#[derive(Debug, PartialEq)]
pub struct View<'a, T> {
    map: Map<'a>,
    spans: [(u32, u32); 10],
    marker: PhantomData<T>,
}
impl<T> Copy for View<'_, T> {}
impl<T> Clone for View<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<'a, T: HotDto> View<'a, T> {
    pub fn parse(bytes: &'a [u8], maximum: usize) -> Result<Self> {
        if bytes.len() > maximum || bytes.len() > u32::MAX as usize {
            return Err(ErrorCode::LimitExceeded.into());
        }
        let mut spans = [(0, 0); 10];
        let mut d = Decoder::new(bytes);
        cbor::preflight_map(&mut d, 0, |key, d, depth| {
            let start = d.position();
            cbor::skip_profile(d, depth)?;
            if let Some(slot) = slot(key) {
                spans[slot] = (start as u32, d.position() as u32);
            }
            Ok(())
        })?;
        if d.position() != bytes.len() {
            return Err(ErrorCode::MalformedFrame.into());
        }
        Self::checked(Map { bytes }, spans)
    }
    pub fn from_map(map: Map<'a>) -> Result<Self> {
        if map.bytes.len() > u32::MAX as usize {
            return Err(ErrorCode::LimitExceeded.into());
        }
        let mut spans = [(0, 0); 10];
        let mut d = Decoder::new(map.bytes);
        let count = d.map()?.ok_or(ErrorCode::MalformedFrame)?;
        for _ in 0..count {
            let key = d.u32()?;
            let start = d.position();
            d.skip()?;
            if let Some(slot) = slot(key) {
                spans[slot] = (start as u32, d.position() as u32);
            }
        }
        Self::checked(map, spans)
    }
    fn checked(map: Map<'a>, spans: [(u32, u32); 10]) -> Result<Self> {
        let view = Self {
            map,
            spans,
            marker: PhantomData,
        };
        T::validate(&view)?;
        Ok(view)
    }
    pub fn raw(&self) -> Map<'a> {
        self.map
    }
    pub fn bytes(&self) -> &'a [u8] {
        self.map.bytes
    }
    pub fn field(&self, key: u32) -> Result<Option<&'a [u8]>> {
        match slot(key) {
            Some(slot) => {
                let (start, end) = self.spans[slot];
                Ok((end != 0).then(|| &self.map.bytes[start as usize..end as usize]))
            }
            None => self.map.field(key),
        }
    }
    pub fn required<U: BorrowValue>(&self, key: u32) -> Result<U::Output<'a>> {
        U::read_borrowed(self.field(key)?.ok_or(ErrorCode::MalformedFrame)?).map_err(|e| e.at_field(key))
    }
}
pub trait HotDto: WireDto {
    fn validate(view: &View<'_, Self>) -> Result<()>;
}
/// Associated outputs borrow variable-sized fields; lists are typed iterators,
/// maps are borrowed trees, fixed identifiers and primitives live on the stack.
pub trait BorrowValue {
    type Output<'a>;
    fn read_borrowed<'a>(bytes: &'a [u8]) -> Result<Self::Output<'a>>;
}
impl<T: HotDto> BorrowValue for T {
    type Output<'a> = View<'a, T>;
    fn read_borrowed<'a>(bytes: &'a [u8]) -> Result<Self::Output<'a>> {
        // bytes is a subtree of an already validated Map.
        View::from_map(Map { bytes })
    }
}
macro_rules! scalar {
    ($($t:ty=>$read:ident),*)=>{$(impl BorrowValue for $t {
        type Output<'a>=$t;
        fn read_borrowed<'a>(bytes:&'a [u8])->Result<Self::Output<'a>> {Ok(Decoder::new(bytes).$read()?)}
    })*};
}
scalar!(u8=>u8,u16=>u16,u32=>u32,u64=>u64,i32=>i32,i64=>i64,bool=>bool);
macro_rules! float {
    ($t:ty,$read:ident,$tag:literal) => {
        impl BorrowValue for $t {
            type Output<'a> = $t;
            fn read_borrowed<'a>(bytes: &'a [u8]) -> Result<Self::Output<'a>> {
                if bytes.first() != Some(&$tag) {
                    return Err(ErrorCode::MalformedFrame.into());
                }
                Ok(Decoder::new(bytes).$read()?)
            }
        }
    };
}
float!(f32, f32, 0xfa);
float!(f64, f64, 0xfb);
impl<T: BorrowValue> BorrowValue for Option<T> {
    type Output<'a> = Option<T::Output<'a>>;
    fn read_borrowed<'a>(bytes: &'a [u8]) -> Result<Self::Output<'a>> {
        if bytes == [0xf6] {
            Ok(None)
        } else {
            T::read_borrowed(bytes).map(Some)
        }
    }
}
impl BorrowValue for RawMap {
    type Output<'a> = Map<'a>;
    fn read_borrowed<'a>(bytes: &'a [u8]) -> Result<Self::Output<'a>> {
        Decoder::new(bytes).map()?.ok_or(ErrorCode::MalformedFrame)?;
        Ok(Map { bytes })
    }
}
impl<const N: usize> BorrowValue for FixedBytes<N> {
    type Output<'a> = &'a [u8; N];
    fn read_borrowed<'a>(bytes: &'a [u8]) -> Result<Self::Output<'a>> {
        Decoder::new(bytes)
            .bytes()?
            .try_into()
            .map_err(|_| ErrorCode::MalformedFrame.into())
    }
}
impl<const MAX: usize> BorrowValue for Bytes<MAX> {
    type Output<'a> = &'a [u8];
    fn read_borrowed<'a>(bytes: &'a [u8]) -> Result<Self::Output<'a>> {
        let value = Decoder::new(bytes).bytes()?;
        if value.len() > MAX {
            return Err(ErrorCode::LimitExceeded.into());
        }
        Ok(value)
    }
}
impl<const MAX: usize> BorrowValue for Text<MAX> {
    type Output<'a> = &'a str;
    fn read_borrowed<'a>(bytes: &'a [u8]) -> Result<Self::Output<'a>> {
        let value = Decoder::new(bytes).str()?;
        if value.len() > MAX {
            return Err(ErrorCode::LimitExceeded.into());
        }
        Ok(value)
    }
}
impl<const MAX: usize> BorrowValue for Ascii<MAX> {
    type Output<'a> = &'a str;
    fn read_borrowed<'a>(bytes: &'a [u8]) -> Result<Self::Output<'a>> {
        let value = Text::<MAX>::read_borrowed(bytes)?;
        if value.is_empty() || !value.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
            return Err(ErrorCode::MalformedFrame.into());
        }
        Ok(value)
    }
}
#[derive(Clone, Debug)]
pub struct Items<'a, T> {
    decoder: Decoder<'a>,
    remaining: usize,
    marker: PhantomData<T>,
}
impl<'a, T: BorrowValue> Iterator for Items<'a, T> {
    type Item = Result<T::Output<'a>>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        let start = self.decoder.position();
        Some(
            self.decoder
                .skip()
                .map_err(Into::into)
                .and_then(|()| T::read_borrowed(&self.decoder.input()[start..self.decoder.position()])),
        )
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}
impl<T: BorrowValue> ExactSizeIterator for Items<'_, T> {}
impl<T: BorrowValue, const MAX: usize> BorrowValue for List<T, MAX> {
    type Output<'a> = Items<'a, T>;
    fn read_borrowed<'a>(bytes: &'a [u8]) -> Result<Self::Output<'a>> {
        let mut decoder = Decoder::new(bytes);
        let n = decoder.array()?.ok_or(ErrorCode::MalformedFrame)?;
        if n > MAX as u64 {
            return Err(ErrorCode::LimitExceeded.into());
        }
        let items = Items {
            decoder,
            remaining: n as usize,
            marker: PhantomData,
        };
        // Validate every element before returning a lazy typed iterator.
        let mut check = Items::<T> {
            decoder: items.decoder.clone(),
            remaining: items.remaining,
            marker: PhantomData,
        };
        for item in &mut check {
            item?;
        }
        Ok(items)
    }
}

macro_rules! read_field {
    ($map:expr,$key:expr,$ty:ty,required) => {
        $map.required::<$ty>($key)
    };
    ($map:expr,$key:expr,$ty:ty,optional) => {
        match $map.field($key)? {
            Some(bytes) => <$ty as $crate::v1::hot::BorrowValue>::read_borrowed(bytes),
            None => Ok(None),
        }
    };
    ($map:expr,$key:expr,$ty:ty,$default:expr) => {
        match $map.field($key)? {
            Some(bytes) => <$ty as $crate::v1::hot::BorrowValue>::read_borrowed(bytes),
            None => Ok($default),
        }
    };
}
pub(crate) use read_field;
macro_rules! validate_extra {
    ($map:expr,validate_audio_notifications) => {
        $crate::v1::dto::validate_audio_notifications_hot($map)
    };
    ($map:expr,validate_input_contacts) => {
        $crate::v1::dto::validate_input_contacts_hot($map)
    };
    ($map:expr,validate_claim_stamp) => {{
        if $map.required::<u64>(1)? == 0 {
            Err($crate::v1::ErrorCode::MalformedFrame.into())
        } else {
            Ok::<(), $crate::v1::Error>(())
        }
    }};
    ($map:expr,validate_generation) => {{
        if $map.required::<u64>(2)? == 0 {
            Err($crate::v1::ErrorCode::MalformedFrame.into())
        } else {
            Ok::<(), $crate::v1::Error>(())
        }
    }};
    ($map:expr,validate_volume) => {{
        if $map.required::<u64>(7)? == 0 {
            Err($crate::v1::ErrorCode::MalformedFrame.into())
        } else {
            Ok::<(), $crate::v1::Error>(())
        }
    }};

    ($map:expr,no_validation) => {
        Ok::<(), $crate::v1::Error>(())
    };
    ($map:expr,validate_identity) => {{
        if $map.required::<$crate::v1::dto::AlgorithmId>(1)? == $crate::v1::dto::ED25519
            && $map.required::<$crate::v1::schema::Bytes<4096>>(2)?.len() != 32
        {
            Err($crate::v1::ErrorCode::MalformedFrame.into())
        } else {
            Ok::<(), $crate::v1::Error>(())
        }
    }};
    ($map:expr,validate_packet) => {{
        if $map.bytes().len() + 10 > $crate::v1::limits::DATAGRAM {
            Err($crate::v1::ErrorCode::LimitExceeded.into())
        } else {
            Ok::<(), $crate::v1::Error>(())
        }
    }};
}
pub(crate) use validate_extra;
macro_rules! hot_map {
    ($name:ident,$validate:ident,{$($key:literal=>$field:ident:$ty:ty=$default:tt;)*})=>{
        $crate::v1::schema::wire_map!($name,$validate,{$($key=>$field:$ty=$default;)*});
        impl $crate::v1::hot::HotDto for $name {
            fn validate(map:&$crate::v1::hot::View<'_,Self>)->$crate::v1::Result<()> {
                $(let _=$crate::v1::hot::read_field!(map,$key,$ty,$default)?;)*
                $crate::v1::hot::validate_extra!(map,$validate)
            }
        }
        impl<'a> $crate::v1::hot::View<'a,$name> {
            $(pub fn $field(&self)->$crate::v1::Result<<$ty as $crate::v1::hot::BorrowValue>::Output<'a>> {$crate::v1::hot::read_field!(self,$key,$ty,$default)})*
        }
    };
}
pub(crate) use hot_map;

/// A stack-built, borrowed tree for capture/encoder callers. Unknown trees may
/// be carried as `Encoded` Map fields without reconstructing or dropping them.
#[derive(Clone, Copy, Debug)]
pub enum Value<'a> {
    Uint(u64),
    Bool(bool),
    F64(f64),
    F32(f32),
    Bytes(&'a [u8]),
    Text(&'a str),
    Null,
    Map(&'a [(u32, Self)]),
    Array(&'a [Self]),
    Encoded(Map<'a>),
}
fn write_value(value: Value<'_>, e: &mut Encoder<&mut [u8]>, depth: usize) -> Result<()> {
    if depth >= limits::DEPTH {
        return Err(ErrorCode::LimitExceeded.into());
    }
    let err = |_| super::Error::from(ErrorCode::LimitExceeded);
    match value {
        Value::Uint(n) => {
            e.u64(n).map_err(err)?;
        }
        Value::Bool(n) => {
            e.bool(n).map_err(err)?;
        }
        Value::F32(n) => {
            if !n.is_finite() {
                return Err(ErrorCode::UnsupportedValue.into());
            }
            e.f32(if n == 0.0 { 0.0 } else { n }).map_err(err)?;
        }
        Value::F64(n) => {
            if !n.is_finite() {
                return Err(ErrorCode::UnsupportedValue.into());
            }
            e.f64(if n == 0.0 { 0.0 } else { n }).map_err(err)?;
        }
        Value::Bytes(n) => {
            e.bytes(n).map_err(err)?;
        }
        Value::Text(n) => {
            e.str(n).map_err(err)?;
        }
        Value::Null => {
            e.null().map_err(err)?;
        }
        Value::Array(values) => {
            e.array(values.len() as u64).map_err(err)?;
            for value in values {
                write_value(*value, e, depth + 1)?;
            }
        }
        Value::Map(fields) => {
            if fields.len() > limits::MAP_KEYS {
                return Err(ErrorCode::LimitExceeded.into());
            }
            e.map(fields.len() as u64).map_err(err)?;
            let mut previous = 0;
            for (key, value) in fields {
                if *key <= previous || *key == u32::MAX {
                    return Err(ErrorCode::MalformedFrame.into());
                }
                previous = *key;
                e.u32(*key).map_err(err)?;
                write_value(*value, e, depth + 1)?;
            }
        }
        Value::Encoded(map) => {
            // Check cumulative depth when splicing a previously validated map.
            cbor::skip_profile(&mut Decoder::new(map.bytes()), depth)?;
            minicbor::encode::Write::write_all(e.writer_mut(), map.bytes()).map_err(|_| ErrorCode::LimitExceeded)?;
        }
    }
    Ok(())
}
macro_rules! hot_messages {
    ($($name:ident),*)=>{
        /// Allocation-free projections of the central v1 Message variants.
        #[derive(Clone,Copy,Debug)]
        pub enum Message<'a> {$($name(View<'a,$name>),)*}
        impl<'a> Message<'a> {
            pub fn decode(frame:OpaqueFrame<'a>)->Result<Self> {
                // Use the central registry's ids, not an independent allocation.
                let entry=super::registry::message(frame.header.type_id).ok_or(ErrorCode::UnknownMessage)?;
                match entry.name {$(stringify!($name)=>View::parse(frame.payload(),frame.payload().len()).map(Self::$name),)*_=>Err(ErrorCode::UnsupportedValue.into())}
            }
            pub fn raw(self)->Map<'a> {match self {$(Self::$name(v)=>v.raw(),)*}}
            pub fn type_id(self)->u16 {
                let name=match self {$(Self::$name(_)=>stringify!($name),)*};
                super::registry::MESSAGES.iter().find(|e|e.name==name).expect("registered projection").id
            }
            pub fn encode_into(self,out:&mut [u8],maximum:usize)->Result<usize> {
                let payload=self.raw().bytes();let len=payload.len()+6;
                if len>maximum || len>out.len() {return Err(ErrorCode::LimitExceeded.into());}
                out[..6].copy_from_slice(&FrameHeader {type_id:self.type_id(),payload_length:payload.len() as u32}.bytes());
                out[6..len].copy_from_slice(payload);Ok(len)
            }
        }
    };
}
hot_messages!(
    InputMotion,
    InputKey,
    InputButton,
    InputScroll,
    InputHeldSync,
    InputKeyRepeat,
    InputGesture,
    InputRelativeMotion,
    InputVolume,
    InputTouchpadContacts,
    InputVolumeKeyOrigin,
    AudioPacket,
    AudioSleep
);

/// Encode captured fields without allocating a RawMap. The caller must discard
/// the buffer on failure; only a successful returned prefix is a complete frame.
pub fn encode_into(type_id: u16, fields: &[(u32, Value<'_>)], out: &mut [u8], maximum: usize) -> Result<usize> {
    let capacity = out.len().min(maximum);
    if capacity < 7 {
        return Err(ErrorCode::LimitExceeded.into());
    }
    let mut e = Encoder::new(&mut out[6..capacity]);
    write_value(Value::Map(fields), &mut e, 0)?;
    let len = capacity - e.into_writer().len();
    out[..6].copy_from_slice(
        &FrameHeader {
            type_id,
            payload_length: (len - 6) as u32,
        }
        .bytes(),
    );
    Message::decode(OpaqueFrame::parse_exact(&out[..len], maximum)?)?;
    Ok(len)
}
