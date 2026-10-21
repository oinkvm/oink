//! Fixed MT ABI and pure frame decoding, shared with non-device regression tests.
use oink_foundation::contacts::{Contact, Sample};

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Point {
    pub(crate) x: f32,
    pub(crate) y: f32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Vector {
    pub(crate) position: Point,
    pub(crate) velocity: Point,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct NativeContact {
    pub(crate) frame: i32,
    pub(crate) timestamp: f64,
    pub(crate) path_index: i32,
    pub(crate) state: i32,
    pub(crate) finger_identity: i32,
    pub(crate) hand_id: i32,
    pub(crate) normalized: Vector,
    pub(crate) size: f32,
    pub(crate) unknown3: i32,
    pub(crate) angle: f32,
    pub(crate) major: f32,
    pub(crate) minor: f32,
    pub(crate) absolute: Vector,
    pub(crate) unknown4: i32,
    pub(crate) unknown5: i32,
    pub(crate) density: f32,
}
const _: () = assert!(std::mem::size_of::<NativeContact>() == 96);
const _: () = assert!(std::mem::offset_of!(NativeContact, path_index) == 16);
const _: () = assert!(std::mem::offset_of!(NativeContact, finger_identity) == 24);
const _: () = assert!(std::mem::offset_of!(NativeContact, normalized) == 32);
pub(crate) fn decode(native: &[NativeContact], sample: &mut Sample) -> Result<(), u8> {
    // Never accumulate contacts from a preceding frame. States 0/1/2/5/6/7 are
    // not down, even if their IDs remain in the framework's hover/lift array.
    sample.len = 0;
    sample.contacts = Default::default();
    for p in native {
        if !(0..=7).contains(&p.state) {
            return Err(2);
        }
        // Match MultitouchSupport's _mthid_pathWasRejected predicate: Digitizer
        // Identity outside 1..=5 is rejected, even in touching states. The raw
        // callback still includes those paths (e.g. identity 6/10 in the 3018
        // capture). Never let a rejected path hold a gesture or admission open.
        // This is a classification, not the stable path_index used as wire ID.
        if !matches!(p.state, 3 | 4) || !(1..=5).contains(&p.finger_identity) {
            continue;
        }
        let Point { x, y } = p.normalized.position;
        if sample.len == 5
            || p.path_index <= 0
            || !x.is_finite()
            || !y.is_finite()
            || !(0.0..=1.0).contains(&x)
            || !(0.0..=1.0).contains(&y)
        {
            return Err(3);
        }
        let c = Contact {
            id: p.path_index as u32,
            x: (f64::from(x) * f64::from(sample.surface.width)).round() as i32,
            y: ((1. - f64::from(y)) * f64::from(sample.surface.height)).round() as i32,
        };
        if sample.points().iter().any(|p| p.id == c.id) {
            return Err(4);
        }
        sample.contacts[usize::from(sample.len)] = c;
        sample.len += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_foundation::contacts::Surface;
    #[test]
    fn only_states_three_and_four_are_down_and_no_id_survives_the_next_report() {
        let mut sample = Sample {
            device: 1,
            surface: Surface {
                width: 15000,
                height: 9000,
            },
            ..Default::default()
        };
        for state in 0..=7 {
            decode(
                &[NativeContact {
                    path_index: 7,
                    finger_identity: 2,
                    state,
                    ..Default::default()
                }],
                &mut sample,
            )
            .unwrap();
            assert_eq!(sample.len, u8::from(matches!(state, 3 | 4)));
        }
        decode(
            &[NativeContact {
                path_index: 7,
                finger_identity: 2,
                state: 4,
                ..Default::default()
            }],
            &mut sample,
        )
        .unwrap();
        decode(&[], &mut sample).unwrap();
        assert_eq!(sample.len, 0);
        assert_eq!(sample.contacts, [Contact::default(); 5]);
    }

    fn contact(path_index: i32, finger_identity: i32, state: i32) -> NativeContact {
        NativeContact {
            path_index,
            finger_identity,
            state,
            ..Default::default()
        }
    }
    fn ids(native: &[NativeContact]) -> Vec<u32> {
        let mut sample = Sample {
            surface: Surface {
                width: 15000,
                height: 9000,
            },
            ..Default::default()
        };
        decode(native, &mut sample).unwrap();
        sample.points().iter().map(|c| c.id).collect()
    }
    #[test]
    fn rejected_identity_never_becomes_a_gesture_contact() {
        for identity in [-1, 0, 6, 7, 8, 9, 10, 11, 12, i32::MAX] {
            for state in [3, 4] {
                assert_eq!(
                    ids(&[contact(8, identity, state), contact(11, 2, 4)]),
                    [11],
                    "identity={identity}, state={state}"
                );
                assert!(ids(&[contact(8, identity, state)]).is_empty());
            }
        }
        for identity in 1..=5 {
            assert_eq!(ids(&[contact(8, identity, 4)]), [8]);
        }
    }
    #[test]
    fn recorded_1641_rejected_path_does_not_add_a_ghost_finger() {
        // Raw metadata from C, 2026-10-21 16:41:42.260--43.392Z.
        // Path 8 remains touching but switches rejected identities 6/10.
        let frames = [
            vec![contact(8, 6, 3)],
            vec![contact(5, 2, 4), contact(8, 10, 4)],
            vec![contact(8, 6, 4)],
            vec![contact(8, 10, 4), contact(11, 2, 3)],
            vec![contact(8, 10, 4), contact(9, 3, 3), contact(11, 2, 4)],
        ];
        let expected = [vec![], vec![5], vec![], vec![11], vec![9, 11]];
        for (frame, expected) in frames.iter().zip(expected) {
            assert_eq!(ids(frame), expected);
        }
    }
    #[test]
    fn rejection_during_a_touch_releases_the_path_without_waiting_for_raw_lift() {
        use oink_foundation::contacts::{Frame, Phase};
        let mut before = Sample {
            device: 1,
            surface: Surface {
                width: 15000,
                height: 9000,
            },
            ..Default::default()
        };
        decode(&[contact(8, 2, 4), contact(9, 3, 4)], &mut before).unwrap();
        let mut after = before;
        decode(&[contact(8, 10, 4), contact(9, 3, 4)], &mut after).unwrap();
        let frame = Frame::between(Some(&before), &after);
        assert_eq!(frame.phase, Phase::Move);
        assert_eq!(
            frame
                .changes()
                .iter()
                .map(|c| (c.contact.id, c.phase))
                .collect::<Vec<_>>(),
            [(8, Phase::End), (9, Phase::Move)]
        );
        let mut lifted = after;
        decode(&[contact(8, 6, 4)], &mut lifted).unwrap();
        let frame = Frame::between(Some(&after), &lifted);
        assert_eq!(frame.phase, Phase::End);
        assert_eq!(
            frame
                .changes()
                .iter()
                .map(|c| (c.contact.id, c.phase))
                .collect::<Vec<_>>(),
            [(9, Phase::End)]
        );
        // A later accepted classification of the same tracking ID starts anew.
        decode(&[contact(8, 2, 3)], &mut after).unwrap();
        let frame = Frame::between(None, &after);
        assert_eq!(frame.phase, Phase::Begin);
        assert_eq!(frame.changes()[0].phase, Phase::Begin);
        assert_eq!(frame.changes()[0].contact.id, 8);
    }
    #[test]
    fn rejected_sixth_path_does_not_exhaust_capacity_or_validate_its_geometry() {
        let mut frames: Vec<_> = (1..=5).map(|id| contact(id, id, 4)).collect();
        let mut rejected = contact(8, 10, 4);
        rejected.normalized.position.x = f32::NAN;
        frames.push(rejected);
        assert_eq!(ids(&frames), [1, 2, 3, 4, 5]);
    }
}
