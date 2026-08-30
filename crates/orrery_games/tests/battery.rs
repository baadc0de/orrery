//! The battery every game in the catalogue passes.
//!
//! Written once against [`Game`] and run over
//! [`CATALOGUE`](orrery_games::CATALOGUE), so the second game inherits every
//! property the first one is held to instead of arriving with its own ad-hoc
//! suite. What is asserted here is the part that has to be true of *any*
//! reference game; what is specific to a game's rules lives beside it — see
//! `skirmish.rs`.
//!
//! The properties, and what each one would catch:
//!
//! | Property | The failure it names |
//! |---|---|
//! | reproducible | VC-4/VC-8: hash iteration order, address hashing, an ambient input |
//! | self-verifying | the rules are not a pure function of the log — nothing downstream can adjudicate |
//! | no stage-1 flag on honest play | a false positive, on the checks that run on every peer (D17 risk 3) |
//! | canonical codec | two honest builds disagreeing about a state they both hold |
//! | combat actually happens | a green suite that measured a coasting scenario |
//! | every tamper is adjudicable | a cheat the pipeline cannot reach |
//! | chains match the golden | an unintended rules change, and cross-platform drift |

use orrery_core::{OrderedInputs, QPos};
use orrery_games::game::{for_each_game, Game, GameVisitor, Tamper, CATALOGUE};
use orrery_games::regolith::{
    archetype::Archetype as RegolithArchetype, order::Outcome as RegolithOutcome,
    weapon::WeaponKind, Regolith,
};
use orrery_games::scenario::{
    adjudicate, adjudicate_isolated, check_codec, play, replay, SCENARIOS,
};
use orrery_protocol::PersistId;

/// Declare a test that runs over every game in the catalogue.
macro_rules! game_test {
    ($test:ident, $visitor:ident, $body:block) => {
        struct $visitor;
        impl GameVisitor for $visitor {
            fn visit<G: Game>(&mut self) $body
        }
        #[test]
        fn $test() {
            for_each_game(&mut $visitor);
        }
    };
}

#[test]
fn catalogue_and_visitor_agree() {
    // Two lists of games is one list too many; this is what keeps a game added
    // to one and not the other from being silently unmeasured.
    struct Names(Vec<&'static str>);
    impl GameVisitor for Names {
        fn visit<G: Game>(&mut self) {
            self.0.push(G::META.name);
        }
    }
    let mut names = Names(Vec::new());
    for_each_game(&mut names);
    let catalogued: Vec<&str> = CATALOGUE.iter().map(|meta| meta.name).collect();
    assert_eq!(names.0, catalogued);
}

game_test!(a_run_is_reproducible, Reproducible, {
    // Twice in one process: the cheapest test there is for the two rules that
    // fail silently — unordered iteration (VC-4) and ambient inputs (VC-8).
    for scenario in SCENARIOS {
        let first = play(G::honest(), scenario);
        let second = play(G::honest(), scenario);
        assert_eq!(
            first.chain,
            second.chain,
            "{}/{}: two runs of the same scenario disagreed",
            G::META.name,
            scenario.name
        );
    }
});

game_test!(sealed_inputs_replay_the_same_run, SealedReplay, {
    // A refactor's candidate must receive the exact run the legacy path saw,
    // rather than a newly generated pilot stream.  This asserts the sealed
    // seed, absolute window and log-ordered inputs are sufficient to replay.
    for scenario in SCENARIOS {
        let original = play(G::honest(), scenario);
        let replayed = replay(G::honest(), &original.sealed);
        assert_eq!(
            replayed.chain,
            original.chain,
            "{}/{}: sealed inputs did not reproduce the state chain",
            G::META.name,
            scenario.name
        );
        assert_eq!(
            replayed.outcome_chain,
            original.outcome_chain,
            "{}/{}: sealed inputs did not reproduce the outcome chain",
            G::META.name,
            scenario.name
        );
    }
});

game_test!(honest_play_is_self_verifying, SelfVerifying, {
    // The property everything downstream rests on: re-executing the recorded
    // log under the same rules reproduces every state hash. If this fails, no
    // witness can distinguish a cheat from the rules being unreplayable.
    for scenario in SCENARIOS {
        let honest = play(G::honest(), scenario);
        assert_eq!(
            adjudicate(G::honest(), scenario, &honest),
            None,
            "{}/{}: honest play did not re-execute to itself",
            G::META.name,
            scenario.name
        );
    }
});

game_test!(honest_play_raises_no_stage_one_flag, NoFalsePositives, {
    // The measurement P4 exists for, in miniature. Every scenario samples at
    // the replication rate, two of them with loss, so a check that assumed
    // adjacent samples fails here rather than against a player whose only
    // offence was a lossy link.
    for scenario in SCENARIOS {
        let honest = play(G::honest(), scenario);
        assert!(
            honest.flags.is_empty(),
            "{}/{}: honest play raised {:?}",
            G::META.name,
            scenario.name,
            honest.flagged_validators()
        );
    }
});

game_test!(observation_does_not_perturb_simulation, Observation, {
    // The harness watches the run it is measuring, so it owes a proof that
    // watching changes nothing. Sample loss is the only knob that separates
    // these two runs, and if it moved the chain the false-positive numbers
    // would be measuring the observer.
    for scenario in SCENARIOS {
        let mut blind = *scenario;
        blind.sample_loss_pct = 100;
        assert_eq!(
            play(G::honest(), scenario).chain,
            play(G::honest(), &blind).chain,
            "{}/{}: dropping every sample changed the simulation",
            G::META.name,
            scenario.name
        );
    }
});

game_test!(states_round_trip_through_the_canonical_codec, Codec, {
    // Over real play rather than hand-built values: the field combinations
    // nobody thought to write down are the ones a scenario produces.
    for scenario in SCENARIOS {
        let honest = play(G::honest(), scenario);
        let checked = check_codec(&honest).unwrap_or_else(|(entity, why)| {
            panic!("{}/{}: {why} for {entity:?}", G::META.name, scenario.name)
        });
        assert!(checked > 0, "{}: nothing was checked", G::META.name);
    }
});

game_test!(populated_scenarios_actually_interact, Interaction, {
    // A suite can be entirely green over a scenario where nothing ever
    // happened. Cross-entity events are the cheapest proof that the discrete
    // half of the rules ran at all.
    for scenario in SCENARIOS.iter().filter(|s| s.entities > 1) {
        let honest = play(G::honest(), scenario);
        assert!(
            honest.events > 0,
            "{}/{}: {} entities produced no cross-entity events",
            G::META.name,
            scenario.name,
            scenario.entities
        );
    }
});

game_test!(every_tamper_is_adjudicable, Adjudicable, {
    // P4's demo criterion generalized: a modified client must be catchable by
    // re-execution, whatever the cheap checks did or did not notice. A tamper
    // that produced no divergence would be one the pipeline cannot reach.
    let scenario = SCENARIOS
        .iter()
        .find(|s| s.name == "island")
        .expect("the island scenario is in the table");
    for tamper in Tamper::ALL {
        let Some(cheat) = G::tampered(*tamper) else {
            continue;
        };
        let cheated = play(cheat, scenario);
        let divergence = adjudicate(G::honest(), scenario, &cheated);
        assert!(
            divergence.is_some(),
            "{}: {} re-executed as honest — replay cannot see it",
            G::META.name,
            tamper.name()
        );
    }
});

game_test!(
    honest_play_adjudicates_entity_by_entity,
    IsolatedAdjudication,
    {
        // The world the *shipped* adjudicator actually builds. A bundle carries
        // one claim, so `ReplayHarness::load_claimed_snapshot` installs one state
        // and the step that follows sees an empty neighbour map. Re-executing an
        // honest window that way has to clear, or every honest peer whose rules
        // read a neighbour is convicted the first time anybody asks.
        //
        // `honest_play_is_self_verifying` cannot catch that: it installs the whole
        // population into one executor, which makes every neighbour read succeed
        // and the property pass against a world the pipeline cannot construct.
        for scenario in SCENARIOS {
            let honest = play(G::honest(), scenario);
            assert_eq!(
                adjudicate_isolated(G::honest, scenario, &honest),
                None,
                "{}/{}: honest play did not re-execute against a single-entity \
             executor — a rule read state no adjudicator has",
                G::META.name,
                scenario.name
            );
        }
    }
);

game_test!(every_tamper_is_adjudicable_in_isolation, IsolatedTamper, {
    // And the isolated path keeps the cheats visible. A fix that made honest
    // play replay cleanly by removing what a cheat perturbs would pass the
    // clause above and be worthless.
    let scenario = SCENARIOS
        .iter()
        .find(|s| s.name == "island")
        .expect("the island scenario is in the table");
    for tamper in Tamper::ALL {
        let Some(cheat) = G::tampered(*tamper) else {
            continue;
        };
        let cheated = play(cheat, scenario);
        assert!(
            adjudicate_isolated(G::honest, scenario, &cheated).is_some(),
            "{}: {} re-executed as honest under single-entity replay",
            G::META.name,
            tamper.name()
        );
    }
});

game_test!(chains_match_the_committed_golden, Golden, {
    // Cross-platform, because this crate is in the determinism matrix's
    // headless spine: each target checks its own chain against these bytes, so
    // a libm divergence that survived quantization fails that target by name.
    for scenario in SCENARIOS {
        let expected = G::GOLDEN_CHAINS
            .iter()
            .find(|(name, _)| *name == scenario.name)
            .unwrap_or_else(|| {
                panic!(
                    "{}: no golden for scenario {} — regenerate the table",
                    G::META.name,
                    scenario.name
                )
            })
            .1;
        let actual = play(G::honest(), scenario).chain;
        assert_eq!(
            hex(&actual),
            hex(&expected),
            "{}/{}: chain changed. If the rules changed on purpose, bump the \
             ruleset version and regenerate; if they did not, this is drift.",
            G::META.name,
            scenario.name
        );
    }
});

game_test!(outcome_chains_match_the_committed_golden, OutcomeGolden, {
    let mut mismatches = Vec::new();
    for scenario in SCENARIOS {
        let expected = outcome_goldens(G::META.name)
            .iter()
            .find(|(name, _)| *name == scenario.name)
            .unwrap_or_else(|| {
                panic!(
                    "{}: no outcome golden for scenario {} — regenerate the table",
                    G::META.name,
                    scenario.name
                )
            })
            .1;
        let actual = play(G::honest(), scenario).outcome_chain;
        if actual != expected {
            mismatches.push(format!(
                "{}/{}: actual {}, expected {}",
                G::META.name,
                scenario.name,
                hex(&actual),
                hex(&expected)
            ));
        }
    }
    assert!(
        mismatches.is_empty(),
        "outcome chain changed. If the rules changed on purpose, bump the ruleset version and \
         regenerate; if they did not, this is drift.\n{}",
        mismatches.join("\n")
    );
});

#[test]
fn damage_delivery_targets_the_event_target() {
    let attacker = PersistId::new(41);
    let target = PersistId::new(99);
    let event = RegolithOutcome::DamageDealt {
        attacker,
        target,
        amount: 7,
        attacker_pos: QPos::default(),
        attacker_vel: Default::default(),
        attacker_yaw_urad: 0,
        attacker_archetype: RegolithArchetype::Interceptor,
        attacker_weapon: WeaponKind::Stock,
        flight_ticks: None,
    };
    let (delivered_to, _) = Regolith::honest()
        .deliver(&event)
        .expect("damage is delivered to its target");
    assert_eq!(delivered_to, target);
}

#[test]
fn ordered_inputs_iterates_in_log_order() {
    let logged = [17u8, 3, 42, 9];
    let observed: Vec<u8> = OrderedInputs::new(&logged).iter().copied().collect();
    assert_eq!(observed, logged);
}

/// Print the golden table as Rust source. Run with:
///
/// ```sh
/// cargo test -p orrery_games --test battery -- --ignored --nocapture emit_goldens
/// cargo fmt -p orrery_games
/// ```
///
/// The emitted table is not rustfmt-stable — paste it, then format.
#[test]
#[ignore = "regenerates src/golden.rs by hand; not a check"]
fn emit_goldens() {
    struct Emit;
    impl GameVisitor for Emit {
        fn visit<G: Game>(&mut self) {
            println!("// {}", G::META.name);
            println!(
                "pub const {}: [(&str, [u8; 32]); {}] = [",
                G::META.name.to_uppercase().replace('-', "_"),
                SCENARIOS.len()
            );
            for scenario in SCENARIOS {
                let chain = play(G::honest(), scenario).chain;
                let bytes: Vec<String> = chain.iter().map(|b| format!("0x{b:02x}")).collect();
                println!("    (\"{}\", [", scenario.name);
                for row in bytes.chunks(8) {
                    println!("        {},", row.join(", "));
                }
                println!("    ]),");
            }
            println!("];");
            println!("// {} outcomes", G::META.name);
            println!(
                "pub const {}_OUTCOMES: [(&str, [u8; 32]); {}] = [",
                G::META.name.to_uppercase().replace('-', "_"),
                SCENARIOS.len()
            );
            for scenario in SCENARIOS {
                let chain = play(G::honest(), scenario).outcome_chain;
                let bytes: Vec<String> = chain.iter().map(|b| format!("0x{b:02x}")).collect();
                println!("    (\"{}\", [", scenario.name);
                for row in bytes.chunks(8) {
                    println!("        {},", row.join(", "));
                }
                println!("    ]),");
            }
            println!("];\n");
        }
    }
    for_each_game(&mut Emit);
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn outcome_goldens(game: &str) -> &'static [(&'static str, [u8; 32])] {
    match game {
        "regolith" => &orrery_games::golden::REGOLITH_OUTCOMES,
        "skirmish" => &orrery_games::golden::SKIRMISH_OUTCOMES,
        _ => panic!("{game}: no outcome golden table"),
    }
}
