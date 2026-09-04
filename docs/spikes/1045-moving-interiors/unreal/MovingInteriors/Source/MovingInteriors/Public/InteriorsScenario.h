#pragma once

#include "CoreMinimal.h"
#include "GameFramework/Actor.h"
#include "InteriorsScenario.generated.h"

class AInteriorsCharacter;
class ACameraActor;
class UStaticMesh;
class UStaticMeshComponent;
class ULevelStreamingDynamic;
struct FInteriorsSim;

/** Which presentation carries the avatar (#1045 "What to build" item 2). */
UENUM()
enum class EInteriorsVariant : uint8
{
	/** (b) the control: a scene component in the frame's local space, transform written from the mirror only */
	Mirror,
	/** (a) CharacterMovementComponent, walking on the moving deck, the mirror's pose written into the capsule before it ticks */
	Cmc,
	/** (a) as Cmc, with based movement disabled in the component */
	CmcNoBase,
	/** (a) CMC drives: input and speed from the ruleset, pose never written; the drift is what accumulates */
	CmcDrive,
};

/** How the ship's interior geometry comes to exist at boarding (the loading-screen question). */
UENUM()
enum class EInteriorsInterior : uint8
{
	/** attached to the ship from BeginPlay */
	Resident,
	/** spawned as attached components at the boarding tick, destroyed on leaving */
	Spawn,
	/** a sub-level streamed in at the boarding tick at the ship's world transform (world-fixed: it does not follow the ship) */
	Stream,
};

/**
 * Spike #1045's runnable map, as an actor: place one in a level (Scripts/
 * make_maps.py does) and play. It creates the host over the nested-frame
 * rules, steps it once per engine frame under -UseFixedTimeStep -FPS=60,
 * mirrors every body into a per-grid local frame, walks the scripted scene,
 * and writes the drift, hitch, CMC-assertion and rollback numbers to CSV and
 * JSON in the output directory.
 *
 * Command line: -InteriorsScene=rest|straight|roll|mech|transitions
 *               -InteriorsVariant=mirror|cmc|cmc_nobase|cmc_drive
 *               -InteriorsInterior=resident|spawn|stream
 *               -InteriorsTicks=N -InteriorsOut=DIR -InteriorsRollback=0|1
 *               -InteriorsShots=0|1
 */
UCLASS()
class MOVINGINTERIORS_API AInteriorsScenario : public AActor
{
	GENERATED_BODY()

public:
	AInteriorsScenario();
	virtual ~AInteriorsScenario();

	UPROPERTY(EditAnywhere, Category = "Spike")
	FString Scene = TEXT("roll");
	UPROPERTY(EditAnywhere, Category = "Spike")
	EInteriorsVariant Variant = EInteriorsVariant::Mirror;
	UPROPERTY(EditAnywhere, Category = "Spike")
	EInteriorsInterior Interior = EInteriorsInterior::Resident;
	UPROPERTY(EditAnywhere, Category = "Spike")
	int32 Ticks = 0;
	UPROPERTY(EditAnywhere, Category = "Spike")
	FString OutDir;
	UPROPERTY(EditAnywhere, Category = "Spike")
	bool bRollback = false;
	UPROPERTY(EditAnywhere, Category = "Spike")
	bool bScreenshots = false;
	UPROPERTY(EditAnywhere, Category = "Spike")
	int32 InteriorPieces = 200;

	virtual void BeginPlay() override;
	virtual void EndPlay(const EEndPlayReason::Type Reason) override;
	virtual void Tick(float DeltaSeconds) override;

	/** Called by the probe after physics and after the character moved. */
	void AfterPhysics();

private:
	FInteriorsSim* Sim = nullptr;
	int32 SceneIndex = 2;
	uint64 TotalTicks = 0;
	uint64 CurrentTick = 0;
	bool bFinished = false;

	// actors and components
	UPROPERTY() AActor* Station = nullptr;
	UPROPERTY() AActor* Ship = nullptr;
	UPROPERTY() UStaticMeshComponent* Deck = nullptr;
	UPROPERTY() USceneComponent* MechRoot = nullptr;
	UPROPERTY() UStaticMeshComponent* MechPlatform = nullptr;
	UPROPERTY() AActor* MirrorAvatar = nullptr;
	UPROPERTY() AInteriorsCharacter* Character = nullptr;
	UPROPERTY() ACameraActor* Camera = nullptr;
	UPROPERTY() UStaticMesh* Cube = nullptr;
	UPROPERTY() TArray<UStaticMeshComponent*> InteriorPieceComponents;
	UPROPERTY() ULevelStreamingDynamic* InteriorLevel = nullptr;
	uint64 MirrorFrame = ~0ull;

	// per-tick observations, filled in Tick and completed in AfterPhysics
	struct FRow
	{
		uint64 Tick = 0;
		double FrameMs = 0, HostUs = 0;
		int32 Gc = 0, Spawned = 0, StreamState = 0;
		uint64 AvatarFrame = 0;
		FVector Target = FVector::ZeroVector;         // ruleset local, cm
		FVector ShipWorld = FVector::ZeroVector;      // cm
		int32 ShipYaw = 0, ShipRoll = 0;
		FVector MirrorDirect = FVector::ZeroVector;   // mm: relative location - target
		FVector MirrorReproj = FVector::ZeroVector;   // mm: frame^-1(world) - target
		FVector CmcDelta = FVector::ZeroVector;       // mm: after CMC's own update
		FVector CmcBasedDelta = FVector::ZeroVector;  // cm: what UpdateBasedMovement moved
		int32 CmcMode = 0, CmcBaseOk = 0;
		FString Event;
	};
	FRow Row;
	TArray<double> DriftDirect, DriftReproj, DriftCmc, FrameMs, HostUs;
	TArray<int32> Assertions;   // per tick: 0/1 (|cmc - target| > 1 mm)
	TArray<int32> GcFlags, SpawnFlags;
	TArray<uint64> TransitionTicks;
	TArray<int32> TransitionKinds;
	int32 GcThisFrame = 0;
	double LastFrameStart = 0;
	double MaxAbsWorld = 0;
	FString CsvPath;
	FArchive* Csv = nullptr;
	uint64 EventsReemitted = 0;
	int64 PresentationResidualMaxMm = 0;
	int32 CorrectionsApplied = 0, CorrectionsMismatchWindow = 0, CorrectionsMismatchAfter = 0;
	TArray<double> CorrectionNs;
	int32 FrameChangesAfterCorrection = 0;
	int32 AssertVerticalCount = 0, AssertHorizontalCount = 0, AssertWithBasedMovement = 0;
	int32 TicksWalking = 0, TicksFalling = 0, TicksFlying = 0, TicksOtherMode = 0, TicksBaseOk = 0;

	void ParseCommandLine();
	void BuildWorld();
	FTransform FrameTransform(uint64 Frame) const;
	FTransform BodyTransform(uint64 Entity) const;
	void WriteMirror(const FString& Event);
	void SetInteriorPresent(bool bPresent, const FTransform& ShipWorld);
	void OnGarbageCollect();
	void Finish();
	void WriteSummary();
};

UCLASS()
class AInteriorsProbe : public AActor
{
	GENERATED_BODY()

public:
	AInteriorsProbe();
	UPROPERTY() AInteriorsScenario* Scenario = nullptr;
	virtual void Tick(float DeltaSeconds) override;
};
