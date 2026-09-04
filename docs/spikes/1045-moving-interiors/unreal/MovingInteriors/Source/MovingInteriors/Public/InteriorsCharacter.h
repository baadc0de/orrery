#pragma once

#include "CoreMinimal.h"
#include "GameFramework/Character.h"
#include "GameFramework/CharacterMovementComponent.h"
#include "InteriorsCharacter.generated.h"

/**
 * Spike #1045 variant (a): CharacterMovementComponent as presentation.
 *
 * The scenario writes the ruleset's pose into the capsule before this
 * component ticks; the component then runs its own update (based movement,
 * floor finding, penetration resolution, physics interaction), and the probe
 * measures how far the capsule ended from the pose it was given. Everything
 * the component moved the capsule by is, by construction, a pose the ruleset
 * did not produce (#1045, "assertion count").
 *
 * Two switches make the variants:
 *  - bDisableBasedMovement: UpdateBasedMovement becomes a no-op, so the
 *    component stops composing the base's per-frame delta onto a capsule the
 *    mirror already placed in the base's new frame.
 *  - the scenario sets bEnablePhysicsInteraction, bRunPhysicsWithNoController
 *    and the gravity direction (ship-down) on it directly.
 */
UCLASS()
class UInteriorsMovement : public UCharacterMovementComponent
{
	GENERATED_BODY()

public:
	UInteriorsMovement();

	bool bDisableBasedMovement = false;
	/** Counters read by the probe, reset every tick by the scenario. */
	int32 BasedMovementApplied = 0;
	FVector LastBasedDelta = FVector::ZeroVector;

	virtual void UpdateBasedMovement(float DeltaSeconds) override;
};

UCLASS()
class AInteriorsCharacter : public ACharacter
{
	GENERATED_BODY()

public:
	AInteriorsCharacter(const FObjectInitializer& ObjectInitializer);

	UInteriorsMovement* Movement() const { return CastChecked<UInteriorsMovement>(GetCharacterMovement()); }
};
