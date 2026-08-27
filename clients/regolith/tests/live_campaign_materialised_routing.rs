//! Opt-in wire proof against a deployed campaign.

#![allow(missing_docs)]

use std::time::{Duration, Instant};

use orrery_games::regolith::order::{Order, Outcome};
use orrery_games::regolith::state::{LockClass, RegolithState, RockTier};
use orrery_protocol::UniverseSeed;
use orrery_regolith_client::campaign::{CampaignConfig, CampaignRuntime, JoinState};
use orrery_regolith_client::intent::Controls;
use orrery_regolith_client::net;
use orrery_regolith_client::session::ConfiguredImpairment;
use orrery_regolith_client::telemetry::JsonlTelemetry;
use serde::Deserialize;

#[derive(Deserialize)]
struct AdmissionJoin {
    join: JoinFields,
    host_direct: String,
    configured: Configured,
}

#[derive(Deserialize)]
struct JoinFields {
    host_node: String,
    slot: usize,
    session_id: String,
    session_token: String,
}

#[derive(Deserialize)]
struct Configured {
    loss_pct: f64,
    jitter_p50_ms: u64,
    jitter_p99_ms: u64,
}

#[test]
#[ignore = "mints one real campaign session; set ORRERY_LIVE_ADMISSION to opt in"]
fn deployed_campaign_routes_a_materialised_rock_and_returns_credit() {
    let origin = std::env::var("ORRERY_LIVE_ADMISSION")
        .expect("set ORRERY_LIVE_ADMISSION to the admission service origin");
    let secret = net::slot_secret(99_538);
    let response = reqwest::blocking::Client::new()
        .post(format!(
            "{}/v1/campaigns/shakedown/join",
            origin.trim_end_matches('/')
        ))
        .json(&serde_json::json!({
            "nickname": "cx-538-wire-proof",
            "node": secret.public().to_string(),
            "client_rev": orrery_regolith_client::BUILD_REV,
            "ruleset_version": orrery_games::regolith::REGOLITH_RULESET.version,
        }))
        .send()
        .expect("reach campaign admission");
    let status = response.status();
    let body = response.text().expect("read admission response");
    assert!(
        status.is_success(),
        "campaign admission refused ({status}); do not retry or fight for the slot: {body}"
    );
    let admitted: AdmissionJoin = serde_json::from_str(&body).expect("decode join response");
    let configured = ConfiguredImpairment {
        loss_pct: admitted.configured.loss_pct,
        jitter_p50_ms: admitted.configured.jitter_p50_ms,
        jitter_p99_ms: admitted.configured.jitter_p99_ms,
    };
    let config = CampaignConfig {
        host_node_hex: admitted.join.host_node,
        host_direct: Some(admitted.host_direct),
        slot: admitted.join.slot,
        session_id: admitted.join.session_id.clone(),
        session_token_hex: Some(admitted.join.session_token),
        wall_start_utc: orrery_regolith_client::campaign::utc_now_iso8601(),
        configured,
        transport_secret: secret,
        roster_url: None,
    };
    let telemetry = std::env::temp_dir().join(format!(
        "orrery-cx-538-live-{}.jsonl",
        admitted.join.session_id
    ));
    let mut sink = JsonlTelemetry::open(&telemetry).expect("open live telemetry");
    let mut runtime = CampaignRuntime::launch(config, UniverseSeed([1; 32]));

    let deadline = Instant::now() + Duration::from_secs(45);
    while !matches!(runtime.state(), JoinState::Joined) && Instant::now() < deadline {
        runtime.poll_join();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        matches!(runtime.state(), JoinState::Joined),
        "live host did not accept the minted session: {}",
        runtime.summary_line()
    );

    let deadline = Instant::now() + Duration::from_secs(30);
    let target = loop {
        let _ = runtime.advance(Controls::default(), &mut sink);
        if let Some(entity) = runtime.executor().entities().copied().find(|entity| {
            matches!(
                runtime.executor().state(*entity),
                Some(RegolithState::Rock(rock)) if rock.tier == RockTier::Small && rock.hull > 0
            )
        }) {
            break entity;
        }
        assert!(
            Instant::now() < deadline,
            "no live Small rock replica arrived before the deadline"
        );
        std::thread::sleep(Duration::from_millis(16));
    };

    let mut saw_lock_reply = false;
    let mut saw_damage_route = false;
    let mut credit = None;
    let deadline = Instant::now() + Duration::from_secs(120);
    while credit.is_none() && Instant::now() < deadline {
        let (inside_arc, lock_mature) = match (
            runtime.executor().state(runtime.entity()),
            runtime.executor().state(target),
        ) {
            (Some(RegolithState::Craft(craft)), Some(RegolithState::Rock(rock))) => (
                orrery_games::regolith::firing_arc_measurement(
                    craft.archetype,
                    craft.yaw_urad,
                    craft.pos,
                    rock.pos,
                )
                .inside,
                craft.lock_target == Some(target)
                    && craft.lock_class == Some(LockClass::Rock)
                    && craft.lock_progress >= orrery_games::regolith::LOCK_ACQUISITION_TICKS,
            ),
            _ => (false, false),
        };
        let report = runtime.advance(
            Controls {
                lock_target: Some(target),
                right: !inside_arc,
                fire: inside_arc && lock_mature,
                ..Controls::default()
            },
            &mut sink,
        );
        saw_lock_reply |= report.delivered.iter().any(|delivery| {
            delivery.from == target
                && matches!(
                    delivery.order,
                    Order::LockConfirmed {
                        target: locked,
                        class: LockClass::Rock,
                    } if locked == target
                )
        });
        saw_damage_route |= report.events.iter().any(
            |event| matches!(event, Outcome::DamageDealt { target: hit, .. } if *hit == target),
        );
        credit = report.delivered.iter().find_map(|delivery| {
            (delivery.from == target)
                .then_some(&delivery.order)
                .and_then(|order| match order {
                    Order::RockCredit { points } => Some(*points),
                    _ => None,
                })
        });
        std::thread::sleep(Duration::from_millis(16));
    }

    println!(
        "LIVE_MATERIALISED_ROUTE session={} target={} lock_reply={} damage_emitted={} rock_credit={:?} telemetry={}",
        admitted.join.session_id,
        target.0,
        saw_lock_reply,
        saw_damage_route,
        credit,
        telemetry.display(),
    );
    assert!(saw_lock_reply, "the rock authority returned LockConfirmed");
    assert!(
        saw_damage_route,
        "the client emitted target-addressed Damage"
    );
    assert_eq!(credit, Some(1), "the rock authority returned RockCredit");
}
