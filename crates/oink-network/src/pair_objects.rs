//! Bounded, authenticated C0 object staging. No partial object reaches a caller.
use oink_protocol::v1::{
    self, Message, RawMap, RawValue, dto,
    schema::{WireDto, WireValue},
};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
const MAX_LINK: usize = 2;
const MAX_STAGING: usize = 4 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(10);
#[derive(Debug)]
struct Staging {
    context: dto::ObjectContext,
    reference: dto::ObjectRef,
    bytes: Vec<u8>,
    deadline: Instant,
}
#[derive(Debug, Default)]
pub struct Objects {
    staging: HashMap<(u64, [u8; 32]), Staging>,
    reserved: usize,
}
#[derive(Debug)]
pub struct Complete {
    pub context: dto::ObjectContext,
    pub reference: dto::ObjectRef,
    pub raw: RawMap,
}
impl Objects {
    pub fn staging_bytes(&self) -> usize {
        self.reserved
    }
    pub fn staging_count(&self, link: u64) -> usize {
        self.staging.keys().filter(|(source, _)| *source == link).count()
    }
    pub fn contains(&self, link: u64, digest: [u8; 32]) -> bool {
        self.staging.contains_key(&(link, digest))
    }

    pub fn begin(
        &mut self,
        link: u64,
        begin: dto::PairObjectBegin,
        authorized: impl FnOnce(&dto::ObjectContext, dto::ObjectKind) -> bool,
        now: Instant,
    ) -> v1::Result<()> {
        let reference = begin.object()?;
        let context = begin.context()?;
        let kind = reference.kind()?;
        if !authorized(&context, kind) {
            return Err(v1::ErrorCode::AuthenticationFailed.into());
        }
        let key = (link, reference.digest()?.0);
        let bytes = reference.bytes()? as usize;
        if bytes == 0 || bytes > v1::limits::PAIR_OBJECT {
            return Err(v1::ErrorCode::LimitExceeded.into());
        }
        if let Some(existing) = self.staging.get(&key) {
            return if existing.context == context && existing.reference == reference {
                Ok(())
            } else {
                Err(v1::ErrorCode::StateConflict.into())
            };
        }
        if self.staging.keys().filter(|(source, _)| *source == link).count() >= MAX_LINK
            || bytes > MAX_STAGING - self.reserved
        {
            return Err(v1::ErrorCode::LimitExceeded.into());
        }
        self.reserved += bytes;
        self.staging.insert(
            key,
            Staging {
                context,
                reference,
                bytes: Vec::with_capacity(bytes),
                deadline: now + DEADLINE,
            },
        );
        Ok(())
    }
    pub fn chunk(&mut self, link: u64, chunk: dto::PairObjectChunk, now: Instant) -> v1::Result<()> {
        let key = (link, chunk.digest()?.0);
        let s = self.staging.get_mut(&key).ok_or(v1::ErrorCode::StateConflict)?;
        let bytes = chunk.bytes()?.0;
        if bytes.is_empty()
            || chunk.offset()? as usize != s.bytes.len()
            || bytes.len() > s.reference.bytes()? as usize - s.bytes.len()
        {
            return Err(v1::ErrorCode::MalformedFrame.into());
        }
        s.bytes.extend_from_slice(&bytes);
        s.deadline = now + DEADLINE;
        Ok(())
    }
    pub fn end(&mut self, link: u64, end: dto::PairObjectEnd) -> v1::Result<Complete> {
        let digest = end.digest()?.0;
        let s = self
            .staging
            .remove(&(link, digest))
            .ok_or(v1::ErrorCode::StateConflict)?;
        self.reserved -= s.reference.bytes()? as usize;
        if s.bytes.len() != s.reference.bytes()? as usize || v1::retained::sha256(&s.bytes).0 != digest {
            return Err(v1::ErrorCode::AuthenticationFailed.into());
        }
        let raw = v1::cbor::decode_map(&s.bytes, v1::limits::PAIR_OBJECT)?;
        match s.reference.kind()? {
            dto::ObjectKind::SourceState => {
                dto::GroupState::from_raw(raw.clone())?.encode(v1::limits::CONFIG_STATE)?;
            }
            dto::ObjectKind::Plan => {
                dto::MergePlan::from_raw(raw.clone())?;
            }
            dto::ObjectKind::Transfer => {
                dto::MergeTransferBundle::from_raw(raw.clone())?;
            }
            _ => return Err(v1::ErrorCode::UnsupportedValue.into()),
        }
        Ok(Complete {
            context: s.context,
            reference: s.reference,
            raw,
        })
    }
    pub fn context(&self, link: u64, digest: [u8; 32]) -> Option<&dto::ObjectContext> {
        self.staging.get(&(link, digest)).map(|s| &s.context)
    }
    pub fn discard(&mut self, link: u64, digest: [u8; 32]) {
        if let Some(s) = self.staging.remove(&(link, digest)) {
            self.reserved -= s.reference.bytes().expect("validated object") as usize;
        }
    }
    pub fn drop_link(&mut self, link: u64) {
        self.staging.retain(|(source, _), s| {
            if *source == link {
                self.reserved -= s.reference.bytes().expect("validated reference") as usize;
                false
            } else {
                true
            }
        });
    }
    pub fn expire(&mut self, now: Instant) -> Vec<(u64, dto::ObjectContext)> {
        let mut expired = Vec::new();
        self.staging.retain(|(source, _), s| {
            if now >= s.deadline {
                self.reserved -= s.reference.bytes().expect("validated reference") as usize;
                expired.push((*source, s.context.clone()));
                false
            } else {
                true
            }
        });
        expired
    }
    pub fn next_deadline(&self) -> Option<Instant> {
        self.staging.values().map(|s| s.deadline).min()
    }
}
pub fn transfer(
    context: dto::ObjectContext,
    kind: dto::ObjectKind,
    raw: &RawMap,
) -> v1::Result<(dto::ObjectRef, Vec<Message>)> {
    let bytes = v1::cbor::encode_map(raw, v1::limits::PAIR_OBJECT)?;
    let digest = v1::retained::sha256(&bytes);
    let reference = dto::ObjectRef::from_raw(
        [
            (1, digest.write()),
            (2, (bytes.len() as u32).write()),
            (3, kind.write()),
        ]
        .into(),
    )?;
    let mut messages = vec![Message::ConfigPairObjectBegin(dto::PairObjectBegin::from_raw(
        [(1, context.write()), (2, reference.write())].into(),
    )?)];
    let mut offset = 0u32;
    for chunk in bytes.chunks(16384) {
        messages.push(Message::ConfigPairObjectChunk(dto::PairObjectChunk::from_raw(
            [
                (1, digest.write()),
                (2, offset.write()),
                (3, RawValue::Bytes(chunk.to_vec())),
            ]
            .into(),
        )?));
        offset += chunk.len() as u32;
    }
    messages.push(Message::ConfigPairObjectEnd(dto::PairObjectEnd::from_raw(
        [(1, digest.write())].into(),
    )?));
    Ok((reference, messages))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn context() -> dto::ObjectContext {
        dto::ObjectContext::from_raw(
            [
                (1, 1u8.write()),
                (
                    2,
                    RawValue::Map(
                        [(
                            1,
                            RawValue::Map(
                                [(1, RawValue::Bytes(vec![1; 16])), (2, RawValue::Bytes(vec![2; 32]))].into(),
                            ),
                        )]
                        .into(),
                    ),
                ),
            ]
            .into(),
        )
        .unwrap()
    }
    fn begin(size: u32, digest: u8) -> dto::PairObjectBegin {
        let object = dto::ObjectRef::from_raw(
            [
                (1, RawValue::Bytes(vec![digest; 32])),
                (2, size.write()),
                (3, dto::ObjectKind::Transfer.write()),
            ]
            .into(),
        )
        .unwrap();
        dto::PairObjectBegin::from_raw([(1, context().write()), (2, object.write())].into()).unwrap()
    }
    #[test]
    fn authorization_and_allocation_limits_precede_staging_and_duplicates_do_not_refresh() {
        let now = Instant::now();
        let mut objects = Objects::default();
        assert!(objects.begin(1, begin(1, 1), |_, _| false, now).is_err());
        assert_eq!(objects.reserved, 0);
        objects.begin(1, begin(1024 * 1024, 1), |_, _| true, now).unwrap();
        objects.begin(1, begin(1024 * 1024, 2), |_, _| true, now).unwrap();
        assert!(objects.begin(1, begin(1, 3), |_, _| true, now).is_err());
        objects.begin(2, begin(1024 * 1024, 3), |_, _| true, now).unwrap();
        objects.begin(2, begin(1024 * 1024, 4), |_, _| true, now).unwrap();
        assert!(objects.begin(3, begin(1, 5), |_, _| true, now).is_err());
        objects
            .begin(1, begin(1024 * 1024, 1), |_, _| true, now + Duration::from_secs(9))
            .unwrap();
        assert_eq!(objects.expire(now + Duration::from_secs(10)).len(), 4);
        assert_eq!(objects.reserved, 0);
    }
    #[test]
    fn offsets_and_digest_are_checked_and_truncated_objects_never_complete() {
        let now = Instant::now();
        let mut objects = Objects::default();
        objects.begin(1, begin(3, 1), |_, _| true, now).unwrap();
        let chunk = |offset: u32, bytes: Vec<u8>| {
            dto::PairObjectChunk::from_raw(
                [
                    (1, RawValue::Bytes(vec![1; 32])),
                    (2, offset.write()),
                    (3, RawValue::Bytes(bytes)),
                ]
                .into(),
            )
            .unwrap()
        };
        assert!(objects.chunk(1, chunk(1, vec![1]), now).is_err());
        assert!(objects.chunk(1, chunk(0, vec![]), now).is_err());
        objects.chunk(1, chunk(0, vec![0xa0]), now).unwrap();
        assert!(
            objects
                .end(
                    1,
                    dto::PairObjectEnd::from_raw([(1, RawValue::Bytes(vec![1; 32]))].into()).unwrap()
                )
                .is_err()
        );
        assert_eq!(objects.reserved, 0);
    }
}
