use orrery_regolith_client::{CAMPAIGN_LOBBY_HOLD, JOIN_DEADLINE, JOIN_HANDSHAKE_READ_TIMEOUT};

/// The host sends the join reply only when its lobby closes, so a client whose
/// patience is shorter than the lobby abandons a seat admission already gave
/// it. That is how a 120-second handshake read and a 125-second join deadline,
/// both written for a 90-second freeze, broke every early joiner once the
/// standing campaign moved to a 180-second lobby.
#[test]
fn a_client_outwaits_the_campaign_lobby_before_it_gives_up() {
    assert!(
        JOIN_HANDSHAKE_READ_TIMEOUT > CAMPAIGN_LOBBY_HOLD,
        "the handshake read must outlast the whole lobby hold: read {JOIN_HANDSHAKE_READ_TIMEOUT:?}, lobby {CAMPAIGN_LOBBY_HOLD:?}"
    );
    assert!(
        JOIN_DEADLINE > JOIN_HANDSHAKE_READ_TIMEOUT,
        "the outer join deadline must outlast the handshake read, or a lobby that never closes is reported as an unattributed dial failure: deadline {JOIN_DEADLINE:?}, read {JOIN_HANDSHAKE_READ_TIMEOUT:?}"
    );
}
