use iroh_base::SecretKey;
use orrery_protocol::{
    audit_witness_epoch_draw, draw_witness_set, verify_witness_epoch, verify_witness_epoch_reveal,
    witness_epoch_binding, witness_epoch_commitment, witness_epoch_seed, AccountId, CellEpoch,
    CellId, GridId, Intent, IntentOp, IssuerKey, IssuerKeyId, NodeId, WitnessEpochClaimsV1,
    WitnessEpochV1, INTENT_PREIMAGE_TAG, PROTOCOL_VERSION, WITNESS_EPOCH_V1_DOMAIN,
};
use serde_json::{json, Value};

const ATTESTATION_PREIMAGE_TAG: &[u8] = b"orrery/attestation/v1";
const ATTESTATION_PREIMAGE_LEN: usize = 157;

fn hex(bytes: impl AsRef<[u8]>) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let bytes = bytes.as_ref();
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        encoded.push(DIGITS[usize::from(byte >> 4)] as char);
        encoded.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    encoded
}

fn secret(byte: u8) -> SecretKey {
    SecretKey::from_bytes(&[byte; 32])
}

fn attestation_preimage_from_d27(intent: &Intent, witness: NodeId) -> [u8; 157] {
    let mut preimage = [0_u8; ATTESTATION_PREIMAGE_LEN];
    let intent_preimage = intent.signing_preimage();
    let intent_hash = blake3::hash(&intent_preimage);

    preimage[0..21].copy_from_slice(ATTESTATION_PREIMAGE_TAG);
    preimage[21..53].copy_from_slice(intent_hash.as_bytes());
    preimage[53..61].copy_from_slice(&intent.cell_epoch.0.to_le_bytes());
    preimage[61..125].copy_from_slice(&intent.signature.to_bytes());
    preimage[125..157].copy_from_slice(witness.as_bytes());
    preimage
}

fn epoch_vector(
    coordinator: &SecretKey,
    issuer_key_id: IssuerKeyId,
    grid: GridId,
    cell: CellId,
    epoch: u32,
    handle_counter: u64,
    candidates: &[NodeId],
    seed_key: [u8; 32],
    prev_seed_key: Option<[u8; 32]>,
) -> (WitnessEpochV1, Value) {
    let binding = witness_epoch_binding(grid, cell, epoch);
    let commitment = witness_epoch_commitment(grid, cell, epoch, &seed_key);
    let draw_seed = witness_epoch_seed(&seed_key, grid, cell, epoch);
    let selected = draw_witness_set(candidates, &draw_seed);
    let candidate_accounts = (0..candidates.len())
        .map(|index| AccountId::new(10_000_000 + index as u64))
        .collect();
    let claims = WitnessEpochClaimsV1::new(
        grid,
        cell,
        epoch,
        WitnessEpochClaimsV1::compose_handle(0x1234, handle_counter),
        30_000,
        30_000,
        candidates.to_vec(),
        selected.clone(),
        commitment,
        prev_seed_key,
        issuer_key_id,
    )
    .with_candidate_accounts(candidate_accounts);
    let claims_postcard = postcard::to_stdvec(&claims).expect("claims encode");
    let mut signing_preimage = WITNESS_EPOCH_V1_DOMAIN.to_vec();
    signing_preimage.extend_from_slice(&claims_postcard);
    let announcement = WitnessEpochV1::sign(claims.clone(), coordinator).expect("epoch sign");
    assert_eq!(
        announcement.signature,
        coordinator.sign(&signing_preimage),
        "published signing preimage must be the one used by WitnessEpochV1::sign"
    );
    let envelope = announcement.encode().expect("epoch encode");
    let verified = verify_witness_epoch(
        &envelope,
        &[IssuerKey::new(issuer_key_id, coordinator.public())],
    )
    .expect("offline verification");
    assert_eq!(verified, claims);
    audit_witness_epoch_draw(&claims, &seed_key).expect("draw audit");

    let value = json!({
        "epoch": epoch,
        "handle": format!("0x{:016x}", claims.handle),
        "seed_key_hex": hex(seed_key),
        "binding_hex": hex(binding),
        "seed_commitment_hex": hex(commitment),
        "draw_seed_hex": hex(draw_seed),
        "prev_seed_key": claims.prev_seed_key.map(hex),
        "selected_public_keys_hex": selected.iter().map(|node| hex(node.as_bytes())).collect::<Vec<_>>(),
        "claims_postcard_len": claims_postcard.len(),
        "claims_postcard_hex": hex(&claims_postcard),
        "signing_preimage_len": signing_preimage.len(),
        "signing_preimage_hex": hex(&signing_preimage),
        "signature_hex": hex(announcement.signature.to_bytes()),
        "envelope_postcard_len": envelope.len(),
        "envelope_postcard_blake3_hex": hex(blake3::hash(&envelope).as_bytes()),
        "envelope_postcard_hex": hex(&envelope),
        "offline_signature_and_bounds_verified": true,
        "commitment_and_draw_audited": true
    });
    (announcement, value)
}

fn main() {
    let issuer = secret(0x11);
    let witness = secret(0x22);
    let coordinator = secret(0x33);

    let mut intent = Intent {
        intent_id: 0x0011_2233_4455_6677_8899_aabb_ccdd_eeff,
        issuer: issuer.public(),
        cell_epoch: CellEpoch::new(0x1234_0000_0000_0abc),
        ops: vec![
            IntentOp {
                op: 0x1234,
                args: vec![0x00, 0x11, 0x22, 0x80, 0xff].into(),
            },
            IntentOp {
                op: 0xabcd,
                args: b"orrery".to_vec().into(),
            },
        ],
        attestations: Vec::new(),
        evidence: None,
        signature: issuer.sign(b"replaced by Intent::sign"),
    };
    let signing_preimage = intent.signing_preimage();
    intent.sign(&issuer);
    assert!(intent.verify_issuer());
    assert_eq!(signing_preimage, intent.signing_preimage());

    let attestation_preimage = attestation_preimage_from_d27(&intent, witness.public());
    let attestation_signature = witness.sign(&attestation_preimage);
    witness
        .public()
        .verify(&attestation_preimage, &attestation_signature)
        .expect("D27 reference attestation verifies");
    assert!(
        witness
            .public()
            .verify(&signing_preimage, &attestation_signature)
            .is_err(),
        "D27 witness signature must not verify over the issuer preimage"
    );
    let legacy_attestation_signature = witness.sign(&signing_preimage);
    witness
        .public()
        .verify(&signing_preimage, &legacy_attestation_signature)
        .expect("legacy current-tree attestation verifies over issuer preimage");
    assert!(
        witness
            .public()
            .verify(&attestation_preimage, &legacy_attestation_signature)
            .is_err(),
        "legacy signature must not verify over the D27 preimage"
    );

    let mut candidate_keys = (0x60_u8..=0x67)
        .map(|byte| {
            let key = secret(byte);
            (key.public(), [byte; 32])
        })
        .collect::<Vec<_>>();
    candidate_keys.sort_by_key(|(node, _)| *node.as_bytes());
    let candidates = candidate_keys
        .iter()
        .map(|(node, _)| *node)
        .collect::<Vec<_>>();
    let candidate_material = candidate_keys
        .iter()
        .map(|(node, key)| {
            json!({
                "secret_key_hex": hex(key),
                "public_key_hex": hex(node.as_bytes())
            })
        })
        .collect::<Vec<_>>();

    let grid = GridId::new(300);
    let cell = CellId::from_bits(0xa924_9249_2492_4d65).expect("valid D5 sample cell");
    let issuer_key_id = IssuerKeyId::new(42);
    let seed_key_0 = [0x44; 32];
    let seed_key_1 = [0x55; 32];
    let (epoch_0, epoch_0_json) = epoch_vector(
        &coordinator,
        issuer_key_id,
        grid,
        cell,
        0,
        0x0abc,
        &candidates,
        seed_key_0,
        None,
    );
    let (epoch_1, epoch_1_json) = epoch_vector(
        &coordinator,
        issuer_key_id,
        grid,
        cell,
        1,
        0x0abd,
        &candidates,
        seed_key_1,
        Some(seed_key_0),
    );
    verify_witness_epoch_reveal(
        &epoch_0.claims,
        epoch_1
            .claims
            .prev_seed_key
            .as_ref()
            .expect("epoch 1 reveals epoch 0"),
    )
    .expect("successor opens predecessor commitment");

    let result = json!({
        "schema": "orrery.intent-attestation-wire-v1",
        "protocol_version": PROTOCOL_VERSION,
        "generated_by": "docs/wire-vectors/generate.sh",
        "implementation_status": {
            "intent": "generated and verified through shipped orrery_protocol APIs",
            "witness_epoch": "generated, verified, and audited through shipped orrery_protocol APIs",
            "attestation": "normative D27 reference only: this tree has no ATTESTATION_PREIMAGE_TAG, ATTESTATION_PREIMAGE_LEN, or attestation_preimage API; the current gateway instead verifies attestations over Intent::signing_preimage"
        },
        "intent": {
            "issuer_secret_key_hex": hex(issuer.to_bytes()),
            "issuer_public_key_hex": hex(issuer.public().as_bytes()),
            "intent_id": "0x00112233445566778899aabbccddeeff",
            "cell_epoch": "0x1234000000000abc",
            "ops": [
                {"op": 0x1234, "args_hex": "00112280ff"},
                {"op": 0xabcd, "args_hex": "6f7272657279"}
            ],
            "signing_preimage_tag_ascii": String::from_utf8_lossy(INTENT_PREIMAGE_TAG),
            "signing_preimage_len": signing_preimage.len(),
            "signing_preimage_hex": hex(&signing_preimage),
            "signature_hex": hex(intent.signature.to_bytes()),
            "shipped_verify_issuer": true
        },
        "attestation": {
            "status": "D27 normative reference; not implemented by this tree",
            "witness_secret_key_hex": hex(witness.to_bytes()),
            "witness_public_key_hex": hex(witness.public().as_bytes()),
            "intent_hash_blake3_hex": hex(blake3::hash(&signing_preimage).as_bytes()),
            "cell_epoch": "0x1234000000000abc",
            "preimage_tag_ascii": String::from_utf8_lossy(ATTESTATION_PREIMAGE_TAG),
            "preimage_len": ATTESTATION_PREIMAGE_LEN,
            "preimage_hex": hex(attestation_preimage),
            "signature_hex": hex(attestation_signature.to_bytes()),
            "signature_rejected_over_issuer_preimage": true,
            "current_tree_legacy_preimage_hex": hex(&signing_preimage),
            "current_tree_legacy_signature_hex": hex(legacy_attestation_signature.to_bytes()),
            "current_tree_legacy_signature_verifies_over_issuer_preimage": true,
            "current_tree_legacy_signature_rejected_over_d27_preimage": true
        },
        "witness_epoch": {
            "coordinator_secret_key_hex": hex(coordinator.to_bytes()),
            "coordinator_public_key_hex": hex(coordinator.public().as_bytes()),
            "issuer_key_id": issuer_key_id.0,
            "grid": grid.0,
            "cell_bits": format!("0x{:016x}", cell.to_bits()),
            "epoch_ms": 30_000,
            "accept_grace_ms": 30_000,
            "candidate_keys_in_ascending_public_key_order": candidate_material,
            "epoch_0": epoch_0_json,
            "epoch_1": epoch_1_json,
            "epoch_1_reveals_epoch_0": true
        }
    });

    println!(
        "{}",
        serde_json::to_string_pretty(&result).expect("JSON encode")
    );
}
