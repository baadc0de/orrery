use orrery_protocol::channels::{
    apply_delta_patch, decode_replication_delta, encode_delta_patch, encode_replication_compressed,
    encode_replication_delta, tag, untag, Channel, ReplicationDelta, TAG_REPLICATION_DELTA,
};
use orrery_protocol::{CellId, PersistId};
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::test_runner::TestRunner;

fn assert_reconstructs(keyframe: &[u8], current: &[u8]) {
    let patch = encode_delta_patch(keyframe, current);
    let reconstructed = apply_delta_patch(keyframe, &patch);
    assert!(
        reconstructed.as_deref() == Some(current),
        "failed to reconstruct {} -> {} canonical bytes",
        keyframe.len(),
        current.len()
    );
}

#[test]
fn a_delta_patch_reconstructs_the_authors_canonical_bytes_exactly() {
    let keyframe = vec![0x41; 134];
    let mut v18_trail = keyframe.clone();
    v18_trail.extend(0x80..=0x97);
    assert_reconstructs(&keyframe, &v18_trail);

    let strategy = (vec(any::<u8>(), 0..512), vec(any::<u8>(), 0..512));
    TestRunner::default()
        .run(&strategy, |(keyframe, current)| {
            let patch = encode_delta_patch(&keyframe, &current);
            prop_assert_eq!(apply_delta_patch(&keyframe, &patch), Some(current));
            Ok(())
        })
        .expect("arbitrary canonical pairs reconstruct exactly");
}

#[test]
fn fixed_length_delta_legs_reconstruct_exactly() {
    assert_reconstructs(b"abcdefgh", b"abCDefGh");
    assert_reconstructs(&[0; 158], &[1; 158]);
    assert_reconstructs(&[7; 158], &[7; 158]);
}

#[test]
fn shrinking_and_growing_delta_legs_reconstruct_exactly() {
    assert_reconstructs(b"canonical state with a tail", b"canonical state");
    assert_reconstructs(b"short", b"short plus a literal trail tail");
    assert_reconstructs(&[], b"new state");
    assert_reconstructs(b"removed state", &[]);
}

#[test]
fn a_delta_envelope_roundtrips_without_entering_the_snapshot_decoder() {
    let delta = ReplicationDelta {
        entity: PersistId::new(9),
        tick: 16_384,
        keyframe_age: 60,
        cell: Some(CellId::ROOT),
        patch: encode_delta_patch(b"keyframe", b"key frame"),
    };
    let absolute = (0u8..=u8::MAX).collect::<Vec<_>>();
    let encoded = encode_replication_delta(&absolute, &delta);
    let (_, state_body) = untag(&encoded).expect("state channel tag");
    assert_eq!(state_body.first(), Some(&TAG_REPLICATION_DELTA));
    assert_eq!(decode_replication_delta(&encoded), Some(delta));
    assert!(orrery_protocol::channels::decode_replication::<Vec<u8>>(&encoded).is_none());
}

#[test]
fn an_unknown_state_sub_tag_is_dropped_not_misparsed() {
    let unknown = tag(Channel::State, &[0xec, 0xff, 0xff, 0xff, 0xff, 0x0f]);
    assert!(decode_replication_delta(&unknown).is_none());
    assert!(orrery_protocol::channels::decode_replication::<Vec<u8>>(&unknown).is_none());
}

#[test]
fn a_delta_that_would_not_be_smaller_ships_as_a_keyframe() {
    let keyframe = (0..158)
        .map(|index| u8::try_from(index).expect("fixture index fits u8"))
        .collect::<Vec<_>>();
    let mut current = keyframe.clone();
    for index in (0..current.len()).step_by(3) {
        current[index] ^= 0x80;
    }
    let patch = encode_delta_patch(&keyframe, &current);
    assert!(
        patch.len() > current.len(),
        "scattered one-byte writes make the run headers exceed the canonical body"
    );

    let absolute = (current, CellId::ROOT, PersistId::new(7), 200_000u64);
    let delta = ReplicationDelta {
        entity: PersistId::new(7),
        tick: 200_000,
        keyframe_age: 60,
        cell: Some(CellId::ROOT),
        patch,
    };
    let encoded = encode_replication_delta(&absolute, &delta);
    let keyframe = encode_replication_compressed(&absolute);
    eprintln!(
        "degenerate canonical={} patch={} selected_keyframe={}",
        absolute.0.len(),
        delta.patch.len(),
        keyframe.len()
    );
    assert_eq!(encoded, keyframe);
    let (_, state_body) = untag(&encoded).expect("state channel tag");
    assert_ne!(state_body.first(), Some(&TAG_REPLICATION_DELTA));
}

#[test]
fn malformed_or_oversized_patch_programs_are_refused() {
    assert!(apply_delta_patch(b"baseline", &[0x80]).is_none());
    assert!(apply_delta_patch(b"baseline", &[0x80, 0]).is_none());
    assert!(apply_delta_patch(b"baseline", &[1, 0, 0]).is_none());
    assert!(apply_delta_patch(b"baseline", &[1, 2, 0]).is_none());
    assert!(apply_delta_patch(&[], &[0xff, 0xff, 0xff, 0xff, 0x0f]).is_none());

    let mut oversized = Vec::new();
    let declared = u32::try_from(orrery_protocol::channels::MAX_RELIABLE_MESSAGE_BYTES + 1)
        .expect("cap fits u32");
    let mut value = declared;
    loop {
        let low = u8::try_from(value & 0x7f).expect("seven bits");
        value >>= 7;
        oversized.push(if value == 0 { low } else { low | 0x80 });
        if value == 0 {
            break;
        }
    }
    assert!(apply_delta_patch(&[], &oversized).is_none());
}

fn canonical_pair(changed: usize, scattered: bool) -> (Vec<u8>, Vec<u8>) {
    let keyframe = (0..158)
        .map(|index| u8::try_from(index).expect("fixture index fits u8"))
        .collect::<Vec<_>>();
    let mut current = keyframe.clone();
    for change in 0..changed {
        let index = if scattered {
            change * current.len() / changed
        } else {
            40 + change
        };
        current[index] ^= 0x80;
    }
    (keyframe, current)
}

#[test]
fn measured_change_count_patch_sizes_are_reported_against_the_tail() {
    for (label, counts) in [
        ("previous", [11usize, 19, 30]),
        ("keyframe", [20usize, 26, 50]),
    ] {
        for changed in counts {
            let (keyframe, contiguous) = canonical_pair(changed, false);
            let (_, scattered) = canonical_pair(changed, true);
            let contiguous_patch = encode_delta_patch(&keyframe, &contiguous);
            let scattered_patch = encode_delta_patch(&keyframe, &scattered);
            let contiguous_delta = ReplicationDelta {
                entity: PersistId::new(7),
                tick: 200_000,
                keyframe_age: 60,
                cell: None,
                patch: contiguous_patch.clone(),
            };
            let scattered_delta = ReplicationDelta {
                patch: scattered_patch.clone(),
                ..contiguous_delta.clone()
            };
            let contiguous_keyframe = (
                contiguous.clone(),
                CellId::ROOT,
                PersistId::new(7),
                200_000u64,
            );
            let scattered_keyframe = (
                scattered.clone(),
                CellId::ROOT,
                PersistId::new(7),
                200_000u64,
            );
            let contiguous_keyframe_len = encode_replication_compressed(&contiguous_keyframe).len();
            let scattered_keyframe_len = encode_replication_compressed(&scattered_keyframe).len();
            let contiguous_wire = encode_replication_delta(&contiguous_keyframe, &contiguous_delta);
            let scattered_wire = encode_replication_delta(&scattered_keyframe, &scattered_delta);
            let contiguous_kind = untag(&contiguous_wire)
                .and_then(|(_, body)| body.first())
                .copied()
                .expect("sub-tag");
            let scattered_kind = untag(&scattered_wire)
                .and_then(|(_, body)| body.first())
                .copied()
                .expect("sub-tag");
            eprintln!(
                "{label} changed={changed} contiguous_keyframe={contiguous_keyframe_len} \
                 contiguous_patch={} contiguous_wire={} contiguous_tag={contiguous_kind:#x} \
                 scattered_keyframe={scattered_keyframe_len} scattered_patch={} \
                 scattered_wire={} scattered_tag={scattered_kind:#x}",
                contiguous_patch.len(),
                contiguous_wire.len(),
                scattered_patch.len(),
                scattered_wire.len(),
            );
            assert_reconstructs(&keyframe, &contiguous);
            assert_reconstructs(&keyframe, &scattered);
        }
    }
}
