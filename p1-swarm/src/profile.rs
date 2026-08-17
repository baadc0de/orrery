//! How a bot behaves, beyond cruising in a circle.
//!
//! P4's criterion asks for ≥ 500 honest player-hours producing **zero**
//! false-positive discrepancy reports. A swarm of identical smooth cruisers
//! would accumulate the hours and prove very little: it is the friendliest
//! possible input distribution, and the false positives worth finding come from
//! the awkward cases — a peer that stops sending for a moment, one that sits
//! perfectly still, one that slams the accelerator.
//!
//! Every profile here is **honest**. Each drives the ruleset with legal inputs
//! and logs exactly what it applied; none of them edits state behind the log.
//! That is the point: a signal raised against any of them is a false positive by
//! construction, which is what makes the count meaningful without needing a
//! separate oracle for who was cheating.

/// A bot's behavioural profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// Constant thrust to cruise speed, constant turn. The smooth case.
    Cruise,
    /// No thrust at all: the bot coasts, then sits.
    ///
    /// docs/07-witnessing.md §3 calls continuous log re-execution *the* signal
    /// for entities nobody is interacting with, precisely because prediction
    /// error gives nothing there. This is that entity.
    Idle,
    /// Alternating full thrust and coast, in one-second blocks.
    ///
    /// Produces sharp accelerations near the stage-1 invariant thresholds —
    /// the shape most likely to trip a speed or acceleration check that is
    /// tuned a little too tight.
    Burst,
    /// Cruises, but stops *sending* for a second at a time.
    ///
    /// Execution stays honest and the log stays intact; only the wire goes
    /// quiet, which is exactly a client hitch. Every watcher sees a chain gap.
    /// This is the profile that matters most: an unfillable hole is the one
    /// witness input that is reportable, so a harness without it never
    /// exercises the difference between "lost" and "refused".
    Stall,
}

impl Profile {
    /// The profiles, in the order they are dealt round-robin across a swarm.
    pub const ALL: [Self; 4] = [Self::Cruise, Self::Idle, Self::Burst, Self::Stall];

    /// The profile for bot `index`, when profiles are in play.
    ///
    /// Uniform cruise when they are not. The awkward profiles exist to stress
    /// the *witness*, and they change how far a bot travels — an idling bot
    /// visits one cell — so dealing them during a P1 criterion run would move
    /// the goalposts on a clause about roaming rather than test anything.
    #[must_use]
    pub fn for_index(index: usize, varied: bool) -> Self {
        if varied {
            Self::ALL[index % Self::ALL.len()]
        } else {
            Self::Cruise
        }
    }

    /// A short name for the report.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Cruise => "cruise",
            Self::Idle => "idle",
            Self::Burst => "burst",
            Self::Stall => "stall",
        }
    }

    /// The thrust this profile asks for at `tick`, given the current speed.
    ///
    /// `full` is the profile-independent cruise thrust; returning zero is a
    /// legal input, not an absence of one — the tick is still logged and still
    /// draws from the RNG, which is what keeps a witness on the same trajectory.
    #[must_use]
    pub fn accel_mmss(self, tick: u64, speed_mps: f64, full: i32, cruise_mps: f64) -> i32 {
        match self {
            Self::Idle => 0,
            Self::Burst => {
                // One second on, one second off.
                if (tick / 60).is_multiple_of(2) {
                    full
                } else {
                    0
                }
            }
            Self::Cruise | Self::Stall => {
                if speed_mps >= cruise_mps {
                    0
                } else {
                    full
                }
            }
        }
    }

    /// Whether this profile is transmitting at `tick`.
    ///
    /// Only [`Profile::Stall`] ever answers `false`, and only for one second in
    /// every eight — long enough that a watcher's rolling head goes stale and it
    /// must repair rather than accuse.
    #[must_use]
    pub fn is_sending(self, tick: u64) -> bool {
        match self {
            Self::Stall => (tick / 60) % 8 != 3,
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_profile_is_dealt_across_a_swarm_of_32() {
        let dealt: Vec<Profile> = (0..32).map(|i| Profile::for_index(i, true)).collect();
        for profile in Profile::ALL {
            assert!(
                dealt.contains(&profile),
                "{} never appears in a 32-peer swarm",
                profile.name()
            );
        }
    }

    #[test]
    fn without_variety_every_bot_cruises() {
        // A P1 criterion run must not be handed a swarm where a quarter of the
        // peers sit still: "roaming across >= 64 interest cells" would then be
        // measuring the profile dealer, not the spatial stack.
        for index in 0..32 {
            assert_eq!(Profile::for_index(index, false), Profile::Cruise);
        }
    }

    #[test]
    fn idle_never_thrusts_and_burst_sometimes_does() {
        assert_eq!(Profile::Idle.accel_mmss(0, 0.0, 60_000, 32.0), 0);
        assert_eq!(Profile::Idle.accel_mmss(500, 0.0, 60_000, 32.0), 0);

        let on = Profile::Burst.accel_mmss(0, 0.0, 60_000, 32.0);
        let off = Profile::Burst.accel_mmss(60, 0.0, 60_000, 32.0);
        assert_eq!(on, 60_000);
        assert_eq!(off, 0);
    }

    #[test]
    fn cruise_cuts_thrust_at_speed() {
        // Skirmish does have drag, and a per-archetype speed clamp above it —
        // but both sit far above the 32 m/s these bots roam at, so neither is
        // what holds a bot at cruise. This cutoff is. Without it the clamp
        // would be, and every bot would sit pinned at its archetype ceiling:
        // an interceptor at 120 m/s crosses a 128 m cell every other tick,
        // which is not roaming, and every hysteresis and churn figure measured
        // over it would be meaningless.
        assert_eq!(Profile::Cruise.accel_mmss(0, 0.0, 60_000, 32.0), 60_000);
        assert_eq!(Profile::Cruise.accel_mmss(0, 32.0, 60_000, 32.0), 0);
    }

    #[test]
    fn only_stall_stops_sending_and_it_comes_back() {
        for tick in [0u64, 60, 120, 6_000] {
            for profile in [Profile::Cruise, Profile::Idle, Profile::Burst] {
                assert!(profile.is_sending(tick), "{} went quiet", profile.name());
            }
        }
        // One second in every eight, and only that one.
        let quiet: Vec<u64> = (0..480)
            .filter(|t| !Profile::Stall.is_sending(*t))
            .collect();
        assert_eq!(quiet.len(), 60, "exactly one second of silence per eight");
        assert!(quiet.iter().all(|t| (180..240).contains(t)));
    }
}
