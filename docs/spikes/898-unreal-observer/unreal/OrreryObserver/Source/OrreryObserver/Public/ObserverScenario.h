#pragma once

#include "CoreMinimal.h"
#include "GameFramework/Actor.h"
#include "ObserverScenario.generated.h"

class UCapsuleComponent;

/**
 * The Unreal observer (#898 step 3).
 *
 * One actor. On BeginPlay it dials the serving sidecars named on the command
 * line; every Tick it polls each link, copies out the presentation set, and
 * moves one capsule per stable id. It writes a CSV of what it observed and
 * exits after a requested number of ticks.
 *
 * WHAT IT MAY DO, AND WHAT IT MAY NOT. It draws. That is all it does. There
 * is no path from this actor back to a sidecar — `orrery_unreal_observer`
 * exposes no send symbol — so it cannot produce a canonical fact, which is
 * ADR-0053 clause (f) items 1 and 2 held by the shape of the surface rather
 * than by this comment. Capsules are moved by assignment from the frame; the
 * actor runs no physics, no CharacterMovementComponent and no Unreal
 * replication, so nothing on this side can invent a position between frames.
 *
 * Command line:
 *   -ObserverAddr=HOST:PORT      may be given twice; one link each
 *   -ObserverTicks=N             quit after N ticks (0 = run until killed)
 *   -ObserverOut=DIR             where the CSV and the summary are written
 *   -ObserverHz=N                pace the tick to N Hz by sleeping to a
 *                                deadline; 0 (the default) free-runs
 *
 * PACING, AND WHY IT CHANGES THE MEANING OF THE NUMBER (#1106). Free-running,
 * this actor ticks thousands of times a second against a sidecar presenting
 * at `PRESENTATION_HZ = 120`, so the large majority of samples are a poll that
 * finds nothing new: the cost of *asking*. With `-ObserverHz=60` the tick
 * sleeps to a real-time deadline outside the measured window, so a sample is
 * one frame's worth of crossing at the frame rate a client would actually run,
 * and `decoded_ticks` in the summary says how many samples had new messages
 * behind them rather than leaving the reader to guess.
 */
UCLASS()
class ORRERYOBSERVER_API AObserverScenario : public AActor
{
	GENERATED_BODY()

public:
	AObserverScenario();

	virtual void BeginPlay() override;
	virtual void EndPlay(const EEndPlayReason::Type Reason) override;
	virtual void Tick(float DeltaSeconds) override;

private:
	/** One dialled sidecar. */
	struct FLink
	{
		FString Addr;
		void* Handle = nullptr;
		int32 LastStatus = 0;
		uint64 Applied = 0;
	};

	/** One capsule, keyed on the stable id it renders. */
	struct FCapsule
	{
		TObjectPtr<AActor> Actor = nullptr;
		TObjectPtr<UCapsuleComponent> Shape = nullptr;
	};

	void Dial();
	void PaceToDeadline();
	void RenderLink(int32 LinkIndex);
	FCapsule& CapsuleFor(int32 LinkIndex, uint64 PersistId, uint8 Timeline);
	void WriteReport();

	TArray<FLink> Links;
	TMap<FString, FCapsule> Capsules;

	/** Rows of the CSV: one per observed entity per sampled tick. */
	TArray<FString> Rows;

	/**
	 * Nanoseconds spent inside poll + snapshot + capsule move, per tick,
	 * across all links.
	 *
	 * ADR-0053's "what this record could not establish" item 4 says every
	 * estimate in it is about the *Rust* side of the boundary. This array is
	 * the other side: what the crossing costs the Unreal game thread, measured
	 * in the Unreal process. Warmup ticks are excluded because the first ticks
	 * pay for spawning the capsules, which happens once.
	 */
	TArray<double> TickNanos;

	/**
	 * The boundary half of the same samples: poll + copy-out only, with the
	 * actor moves excluded.
	 *
	 * `TickNanos` minus this is Unreal's own cost of applying the frame, which
	 * scales with N and has no counterpart in #1100's extractor figure. Two
	 * `FPlatformTime::Seconds()` calls per link per tick land inside the outer
	 * window to produce it; at tens of nanoseconds each against a window of
	 * tens of microseconds that is below the resolution of anything reported.
	 */
	TArray<double> BoundaryNanos;

	/** The pacing deadline for the next tick, in `FPlatformTime::Seconds()`. */
	double NextDeadline = 0.0;

	/** When the first paced tick ran, so the achieved rate can be reported. */
	double PacedStart = 0.0;

	/** When the last measured tick's window closed. */
	double MeasuredEnd = 0.0;

	/** The first tick counted towards the achieved rate. */
	int32 PacedFrom = 0;

	/** Target tick rate from `-ObserverHz=`; 0 free-runs, as before #1106. */
	double ObserverHz = 0.0;

	/**
	 * Measured ticks on which at least one link had newly applied messages.
	 *
	 * The decode itself happens on `orrery_unreal_observer`'s own link thread,
	 * not here — so this is not "ticks that decoded", it is "ticks whose
	 * copy-out saw a set the decoder had just replaced". A crossing figure
	 * whose samples are mostly *not* those is a figure about polling an idle
	 * link, and #1105's was; this counter is what makes the difference
	 * visible rather than assumed.
	 */
	int32 FreshTicks = 0;

	/** Newly applied messages seen across all links this tick. */
	uint32 AppliedThisTick = 0;

	/**
	 * Whether this tick also built CSV rows, which allocate and format inside
	 * the timed window. Such a tick is excluded from the percentiles: it
	 * measures the instrument as much as the crossing.
	 */
	bool bSampledCsvThisTick = false;

	/** Seconds spent inside poll + copy-out this tick, across all links. */
	double BoundaryThisTick = 0.0;

	int32 RequestedTicks = 0;
	int32 TicksRun = 0;
	uint64 EntitiesSeen = 0;
	uint64 PredictedSeen = 0;
	uint64 InterpolatedSeen = 0;
	uint64 BracketedSeen = 0;
	FString OutDir;
	bool bReported = false;
};
