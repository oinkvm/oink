//! Shared bounded visitors. Reject declared collection sizes before reserving;
//! never trust a peer-provided size hint as an allocation request.
use serde::{
    Deserialize, Deserializer,
    de::{Error, SeqAccess, Visitor},
};
use std::{fmt, marker::PhantomData};

pub fn vec<'de, D, T, const N: usize>(d: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct V<T, const N: usize>(PhantomData<T>);
    impl<'de, T: Deserialize<'de>, const N: usize> Visitor<'de> for V<T, N> {
        type Value = Vec<T>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "at most {N} entries")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            if seq.size_hint().is_some_and(|n| n > N) {
                return Err(A::Error::custom("collection exceeds limit"));
            }
            let mut out = Vec::new();
            while let Some(value) = seq.next_element()? {
                if out.len() == N {
                    return Err(A::Error::custom("collection exceeds limit"));
                }
                out.push(value);
            }
            Ok(out)
        }
    }
    d.deserialize_seq(V::<T, N>(PhantomData))
}

pub fn string<'de, D: Deserializer<'de>, const N: usize>(d: D) -> Result<String, D::Error> {
    struct V<const N: usize>;
    impl<const N: usize> Visitor<'_> for V<N> {
        type Value = String;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "at most {N} UTF-8 bytes")
        }
        fn visit_str<E: Error>(self, value: &str) -> Result<String, E> {
            if value.len() > N {
                return Err(E::custom("string exceeds limit"));
            }
            Ok(value.to_owned())
        }
    }
    d.deserialize_str(V::<N>)
}
