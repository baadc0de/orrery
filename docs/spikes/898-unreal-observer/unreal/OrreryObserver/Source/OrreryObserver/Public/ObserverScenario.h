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

	int32 RequestedTicks = 0;
	int32 TicksRun = 0;
	uint64 EntitiesSeen = 0;
	uint64 PredictedSeen = 0;
	uint64 InterpolatedSeen = 0;
	uint64 BracketedSeen = 0;
	FString OutDir;
	bool bReported = false;
};
