#include "InteriorsCharacter.h"

#include "Components/CapsuleComponent.h"

UInteriorsMovement::UInteriorsMovement()
{
	// No controller ever possesses the spike's avatar; without this the
	// component performs no movement at all (CharacterMovementComponent.cpp,
	// TickComponent: bShouldPerformControlledCharMove).
	bRunPhysicsWithNoController = true;
	// The ruleset does not simulate jumping, and the capsule must not be
	// pushed by anything.
	NavAgentProps.bCanJump = false;
	bEnablePhysicsInteraction = false;
	bAlwaysCheckFloor = true;
	// Do not drift the capsule's rotation with the base; the mirror owns it.
	bIgnoreBaseRotation = false;
}

void UInteriorsMovement::UpdateBasedMovement(float DeltaSeconds)
{
	if (bDisableBasedMovement)
	{
		return;
	}
	const FVector Before = UpdatedComponent ? UpdatedComponent->GetComponentLocation() : FVector::ZeroVector;
	Super::UpdateBasedMovement(DeltaSeconds);
	const FVector After = UpdatedComponent ? UpdatedComponent->GetComponentLocation() : FVector::ZeroVector;
	LastBasedDelta = After - Before;
	if (!LastBasedDelta.IsNearlyZero(1e-6))
	{
		BasedMovementApplied += 1;
	}
}

AInteriorsCharacter::AInteriorsCharacter(const FObjectInitializer& ObjectInitializer)
	: Super(ObjectInitializer.SetDefaultSubobjectClass<UInteriorsMovement>(ACharacter::CharacterMovementComponentName))
{
	PrimaryActorTick.bCanEverTick = true;
	PrimaryActorTick.TickGroup = TG_PrePhysics;
	GetCapsuleComponent()->InitCapsuleSize(34.0f, 88.0f);
	bUseControllerRotationYaw = false;
	// The spike's own mesh-less capsule: nothing to render but the collision
	// shape, and nothing to animate.
	if (GetMesh())
	{
		GetMesh()->SetSkeletalMesh(nullptr);
	}
}
