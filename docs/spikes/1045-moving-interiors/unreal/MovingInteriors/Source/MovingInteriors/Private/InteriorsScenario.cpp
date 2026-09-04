#include "InteriorsScenario.h"

#include "InteriorsCharacter.h"
#include "MovingInteriorsModule.h"

#include "Camera/CameraActor.h"
#include "Camera/CameraComponent.h"
#include "Components/CapsuleComponent.h"
#include "Components/StaticMeshComponent.h"
#include "Engine/LevelStreamingDynamic.h"
#include "Engine/StaticMesh.h"
#include "Engine/World.h"
#include "GameFramework/PlayerController.h"
#include "HAL/FileManager.h"
#include "HAL/PlatformTime.h"
#include "Misc/App.h"
#include "Misc/CommandLine.h"
#include "Misc/FileHelper.h"
#include "Misc/Paths.h"
#include "RHI.h"
#include "UnrealClient.h"
#include "UObject/UObjectGlobals.h"

#include <cmath>

THIRD_PARTY_INCLUDES_START
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wunused-function"
#include "interiors_shared.h"
#pragma clang diagnostic pop
THIRD_PARTY_INCLUDES_END

namespace
{
	uint64_t ClockNs()
	{
		return static_cast<uint64_t>(FPlatformTime::Seconds() * 1e9);
	}

	const uint8 Seed[32] = {0x10, 0x45, 0x10, 0x45, 0x10, 0x45, 0x10, 0x45, 0x10, 0x45, 0x10, 0x45, 0x10, 0x45, 0x10, 0x45,
							0x10, 0x45, 0x10, 0x45, 0x10, 0x45, 0x10, 0x45, 0x10, 0x45, 0x10, 0x45, 0x10, 0x45, 0x10, 0x45};

	constexpr double MmToCm = 0.1;
	constexpr double CapsuleHalfHeightCm = 88.0;
	constexpr double HitchMs = 16.7;

	double Percentile(TArray<double> Samples, double Pct)
	{
		if (Samples.Num() == 0)
		{
			return 0;
		}
		Samples.Sort();
		double Index = (Pct / 100.0) * Samples.Num();
		double Ceiled = std::ceil(Index);
		if (Ceiled < 1.0)
		{
			Ceiled = 1.0;
		}
		int32 I = static_cast<int32>(Ceiled) - 1;
		return Samples[FMath::Min(I, Samples.Num() - 1)];
	}

	/** The ruleset's Rz(yaw)·Rx(roll) plus a position, as an FTransform.
	 * UE matrices are row-vector: row i is the image of basis vector i, i.e.
	 * column i of the ruleset's matrix. */
	FTransform MakeTransform(const int64_t* PosMm, int32 YawUrad, int32 RollUrad)
	{
		const double Yaw = YawUrad * 1e-6, Roll = RollUrad * 1e-6;
		const double Sy = std::sin(Yaw), Cy = std::cos(Yaw), Sr = std::sin(Roll), Cr = std::cos(Roll);
		// R = [[cy, -sy*cr, sy*sr], [sy, cy*cr, -cy*sr], [0, sr, cr]]
		const FVector XAxis(Cy, Sy, 0.0);
		const FVector YAxis(-Sy * Cr, Cy * Cr, Sr);
		const FVector ZAxis(Sy * Sr, -Cy * Sr, Cr);
		const FVector Origin(PosMm[0] * MmToCm, PosMm[1] * MmToCm, PosMm[2] * MmToCm);
		FMatrix M(FPlane(XAxis, 0.0), FPlane(YAxis, 0.0), FPlane(ZAxis, 0.0), FPlane(Origin, 1.0));
		return FTransform(M);
	}
}

/** The host and the predictor, plus the stand-in authority when rollback is armed. */
struct FInteriorsSim
{
	orrery_host* Host = nullptr;
	orrery_host* AuthorityHost = nullptr;
	interiors_predictor Predictor;
	interiors_authority Authority;
	interiors_body Bodies[INTERIORS_SCENE_ENTITIES];
	interiors_scene Scene = INTERIORS_SCENE_ROLL;
	uint64 Total = 0;
	bool bRollback = false;

	struct FArrangement
	{
		int32 Kind = 0;
		uint64 Tc = 0, Ta = 0, Tx = 0, Now = 0;
		bool bApplied = false;
		uint32 MismatchWindow = 0, MismatchAfter = 0;
		interiors_rollback_report Report;
	};
	TArray<FArrangement> Arrangements;
	int32 NextArr = 0, PostArr = -1;
	uint64 PostUntil = 0;

	bool Create(interiors_scene InScene, uint64 InTotal, bool bInRollback)
	{
		Scene = InScene;
		Total = InTotal;
		bRollback = bInRollback;
		Host = interiors_create_host(Seed);
		if (Host == nullptr)
		{
			return false;
		}
		interiors_predictor_init(&Predictor, Host);
		if (bRollback)
		{
			AuthorityHost = interiors_create_host(Seed);
			if (AuthorityHost == nullptr)
			{
				return false;
			}
			interiors_authority_init(&Authority, AuthorityHost);
			interiors_transition Transitions[4096];
			unsigned N = interiors_script_transitions(Scene, Total, Transitions, 4096);
			for (unsigned I = 0; I < N; ++I)
			{
				FArrangement A;
				A.Kind = Transitions[I].kind;
				A.Tc = Transitions[I].tick;
				A.Ta = A.Tc - (I % INTERIORS_WINDOW);
				A.Tx = A.Ta - 1;
				A.Now = A.Ta + INTERIORS_WINDOW;
				Arrangements.Add(A);
			}
			Arrangements.Sort([](const FArrangement& A, const FArrangement& B) { return A.Now < B.Now; });
		}
		ReadBodies();
		return true;
	}

	void Destroy()
	{
		if (Host)
		{
			interiors_predictor_free(&Predictor);
			orrery_host_destroy(Host);
			Host = nullptr;
		}
		if (AuthorityHost)
		{
			orrery_host_destroy(AuthorityHost);
			AuthorityHost = nullptr;
		}
	}

	void ReadBodies()
	{
		for (unsigned E = 0; E < INTERIORS_SCENE_ENTITIES; ++E)
		{
			interiors_read_body(Host, E + 1, &Bodies[E]);
		}
	}

	const interiors_body& Body(uint64 Entity) const { return Bodies[Entity - 1]; }

	/** A correction arriving at this boundary, if one is arranged. Returns true if one fired. */
	bool MaybeCorrect(uint64 Tick, FArrangement*& Out)
	{
		Out = nullptr;
		if (!bRollback || NextArr >= Arrangements.Num() || Arrangements[NextArr].Now != Tick)
		{
			return false;
		}
		FArrangement& A = Arrangements[NextArr];
		unsigned Slot = static_cast<unsigned>(A.Ta % INTERIORS_AUTHORITY_RING);
		if (Authority.tick[Slot] != A.Ta)
		{
			UE_LOG(LogInteriors, Error, TEXT("authority ring miss at %llu"), (unsigned long long)A.Ta);
			NextArr += 1;
			return false;
		}
		interiors_correction_entity Ent[INTERIORS_SCENE_ENTITIES];
		for (unsigned E = 0; E < INTERIORS_SCENE_ENTITIES; ++E)
		{
			Ent[E].entity = E + 1;
			FMemory::Memcpy(Ent[E].bytes, Authority.state[Slot][E], ORRERY_INTERIORS_BODY_BYTES);
		}
		A.Report = interiors_apply_correction(&Predictor, Ent, INTERIORS_SCENE_ENTITIES, A.Ta, A.Now, ClockNs);
		A.bApplied = true;
		for (uint64 T = A.Ta; T < A.Now; ++T)
		{
			A.MismatchWindow += interiors_compare_tick(&Predictor, &Authority, T);
		}
		PostArr = NextArr;
		PostUntil = A.Now + 30;
		for (int32 K = NextArr + 1; K < Arrangements.Num(); ++K)
		{
			if (Arrangements[K].Tx < PostUntil)
			{
				PostUntil = Arrangements[K].Tx;
			}
		}
		NextArr += 1;
		ReadBodies();
		Out = &A;
		return true;
	}

	/** The divergence the authority applies at this tick, if arranged (shape: ship). */
	void ArmDivergence(uint64 Tick)
	{
		Authority.has_extra = 0;
		if (!bRollback)
		{
			return;
		}
		for (int32 K = NextArr; K < Arrangements.Num(); ++K)
		{
			if (Arrangements[K].Tx == Tick)
			{
				interiors_body Ship;
				interiors_read_body(AuthorityHost, ORRERY_INTERIORS_SHIP, &Ship);
				Authority.has_extra = 1;
				Authority.extra_tick = Tick;
				interiors_cmd_cruise(&Authority.extra, ORRERY_INTERIORS_SHIP, Ship.vel[0], Ship.vel[1] + 3000, Ship.vel[2],
									 Ship.yaw_rate_urad_tick + 100, Ship.roll_rate_urad_tick + 200);
				break;
			}
		}
	}

	bool Step(uint64 Tick)
	{
		if (interiors_predict_tick(&Predictor, Scene, Total) != Tick)
		{
			return false;
		}
		if (bRollback)
		{
			if (interiors_authority_tick(&Authority, Scene, Total) != Tick)
			{
				return false;
			}
			if (PostArr >= 0 && Tick < PostUntil && Tick >= Arrangements[PostArr].Now)
			{
				Arrangements[PostArr].MismatchAfter += interiors_compare_tick(&Predictor, &Authority, Tick);
			}
		}
		ReadBodies();
		return true;
	}

	/** Drain events; return the FrameChanged records as (entity, from, to). */
	void DrainFrameChanges(TArray<TTuple<uint64, uint64, uint64>>& Out, uint64& OutCount)
	{
		orrery_host_result result;
		interiors_bytes* B = &Predictor.scratch;
		INTERIORS_CALL_INTO(B, orrery_host_drain_events(Host, B->data, B->cap, &required_));
		OutCount = 0;
		if (result != ORRERY_HOST_OK)
		{
			return;
		}
		size_t At = 0;
		while (At + 12 <= B->len)
		{
			uint32 L = interiors_read_u32(B->data + At + 8);
			const uint8* Ev = B->data + At + 12;
			if (L == 25 && Ev[0] == 0)
			{
				Out.Add(MakeTuple(interiors_read_u64(Ev + 1), interiors_read_u64(Ev + 9), interiors_read_u64(Ev + 17)));
			}
			OutCount += 1;
			At += 12 + L;
		}
	}
};

// ---------------------------------------------------------------------------

AInteriorsScenario::AInteriorsScenario()
{
	PrimaryActorTick.bCanEverTick = true;
	PrimaryActorTick.TickGroup = TG_PrePhysics;
	USceneComponent* Root = CreateDefaultSubobject<USceneComponent>(TEXT("Root"));
	SetRootComponent(Root);
}

AInteriorsScenario::~AInteriorsScenario()
{
	delete Sim;
	Sim = nullptr;
}

void AInteriorsScenario::ParseCommandLine()
{
	const TCHAR* Cmd = FCommandLine::Get();
	FString S;
	if (FParse::Value(Cmd, TEXT("InteriorsScene="), S))
	{
		Scene = S;
	}
	if (FParse::Value(Cmd, TEXT("InteriorsVariant="), S))
	{
		if (S == TEXT("mirror")) Variant = EInteriorsVariant::Mirror;
		else if (S == TEXT("cmc")) Variant = EInteriorsVariant::Cmc;
		else if (S == TEXT("cmc_nobase")) Variant = EInteriorsVariant::CmcNoBase;
		else if (S == TEXT("cmc_drive")) Variant = EInteriorsVariant::CmcDrive;
	}
	if (FParse::Value(Cmd, TEXT("InteriorsInterior="), S))
	{
		if (S == TEXT("resident")) Interior = EInteriorsInterior::Resident;
		else if (S == TEXT("spawn")) Interior = EInteriorsInterior::Spawn;
		else if (S == TEXT("stream")) Interior = EInteriorsInterior::Stream;
	}
	int32 N = 0;
	if (FParse::Value(Cmd, TEXT("InteriorsTicks="), N))
	{
		Ticks = N;
	}
	if (FParse::Value(Cmd, TEXT("InteriorsOut="), S))
	{
		OutDir = S;
	}
	if (FParse::Value(Cmd, TEXT("InteriorsRollback="), N))
	{
		bRollback = N != 0;
	}
	if (FParse::Value(Cmd, TEXT("InteriorsShots="), N))
	{
		bScreenshots = N != 0;
	}
	if (FParse::Value(Cmd, TEXT("InteriorsPieces="), N))
	{
		InteriorPieces = N;
	}
}

static const TCHAR* VariantName(EInteriorsVariant V)
{
	switch (V)
	{
	case EInteriorsVariant::Mirror: return TEXT("mirror");
	case EInteriorsVariant::Cmc: return TEXT("cmc");
	case EInteriorsVariant::CmcNoBase: return TEXT("cmc_nobase");
	case EInteriorsVariant::CmcDrive: return TEXT("cmc_drive");
	}
	return TEXT("?");
}

static const TCHAR* InteriorName(EInteriorsInterior I)
{
	switch (I)
	{
	case EInteriorsInterior::Resident: return TEXT("resident");
	case EInteriorsInterior::Spawn: return TEXT("spawn");
	case EInteriorsInterior::Stream: return TEXT("stream");
	}
	return TEXT("?");
}

void AInteriorsScenario::BeginPlay()
{
	Super::BeginPlay();
	ParseCommandLine();

	interiors_scene Parsed;
	if (!interiors_scene_parse(TCHAR_TO_ANSI(*Scene), &Parsed))
	{
		UE_LOG(LogInteriors, Error, TEXT("unknown scene %s"), *Scene);
		Finish();
		return;
	}
	SceneIndex = Parsed;
	TotalTicks = Ticks > 0 ? Ticks : (Parsed == INTERIORS_SCENE_TRANSITIONS ? 24 * INTERIORS_CYCLE_TICKS : 36000);
	if (OutDir.IsEmpty())
	{
		OutDir = FPaths::ProjectSavedDir() / TEXT("Interiors");
	}
	IFileManager::Get().MakeDirectory(*OutDir, true);

	Sim = new FInteriorsSim();
	if (!Sim->Create(Parsed, TotalTicks, bRollback))
	{
		UE_LOG(LogInteriors, Error, TEXT("host creation failed"));
		Finish();
		return;
	}
	{
		interiors_transition Transitions[4096];
		unsigned N = interiors_script_transitions(Parsed, TotalTicks, Transitions, 4096);
		for (unsigned I = 0; I < N; ++I)
		{
			TransitionTicks.Add(Transitions[I].tick);
			TransitionKinds.Add(Transitions[I].kind);
		}
	}

	UE_LOG(LogInteriors, Display, TEXT("spike 1045: scene=%s variant=%s interior=%s ticks=%llu rollback=%d fixed_dt=%d dt=%.6f nullrhi=%d out=%s"),
		   *Scene, VariantName(Variant), InteriorName(Interior), (unsigned long long)TotalTicks, bRollback ? 1 : 0,
		   FApp::UseFixedTimeStep() ? 1 : 0, FApp::GetFixedDeltaTime(), GUsingNullRHI ? 1 : 0, *OutDir);

	BuildWorld();

	CsvPath = OutDir / FString::Printf(TEXT("ticks-%s-%s-%s.csv"), *Scene, VariantName(Variant), InteriorName(Interior));
	Csv = IFileManager::Get().CreateFileWriter(*CsvPath);
	if (Csv)
	{
		FString Header = TEXT("tick,frame_ms,host_us,gc,spawned,stream,av_frame,ship_wx,ship_wy,ship_wz,ship_yaw,ship_roll,tgt_x,tgt_y,tgt_z,"
							  "mir_dx,mir_dy,mir_dz,rep_dx,rep_dy,rep_dz,cmc_dx,cmc_dy,cmc_dz,cmc_based_dx,cmc_based_dy,cmc_based_dz,cmc_mode,cmc_base_ok,event\n");
		auto Utf8 = StringCast<UTF8CHAR>(*Header);
		Csv->Serialize(const_cast<UTF8CHAR*>(Utf8.Get()), Utf8.Length());
	}

	FCoreUObjectDelegates::GetPostGarbageCollect().AddUObject(this, &AInteriorsScenario::OnGarbageCollect);
	LastFrameStart = FPlatformTime::Seconds();

	AInteriorsProbe* Probe = GetWorld()->SpawnActor<AInteriorsProbe>();
	Probe->Scenario = this;
	Probe->AddTickPrerequisiteActor(this);
	if (Character)
	{
		Probe->AddTickPrerequisiteActor(Character);
	}
}

void AInteriorsScenario::EndPlay(const EEndPlayReason::Type Reason)
{
	if (Csv)
	{
		Csv->Close();
		delete Csv;
		Csv = nullptr;
	}
	if (Sim)
	{
		Sim->Destroy();
		delete Sim;
		Sim = nullptr;
	}
	FCoreUObjectDelegates::GetPostGarbageCollect().RemoveAll(this);
	Super::EndPlay(Reason);
}

void AInteriorsScenario::OnGarbageCollect()
{
	GcThisFrame += 1;
}

static UStaticMeshComponent* AddBox(AActor* Owner, USceneComponent* Parent, UStaticMesh* Cube, const FVector& RelLoc, const FVector& ScaleCm, const FName& Name, bool bMovable)
{
	UStaticMeshComponent* C = NewObject<UStaticMeshComponent>(Owner, Name);
	C->SetStaticMesh(Cube);
	C->SetMobility(bMovable ? EComponentMobility::Movable : EComponentMobility::Static);
	C->SetCollisionProfileName(TEXT("BlockAll"));
	C->CanCharacterStepUpOn = ECB_Yes;
	C->SetCanEverAffectNavigation(false);
	C->SetRelativeScale3D(ScaleCm / 100.0);
	C->SetRelativeLocation(RelLoc);
	C->SetupAttachment(Parent);
	C->RegisterComponent();
	return C;
}

void AInteriorsScenario::BuildWorld()
{
	UWorld* World = GetWorld();
	Cube = LoadObject<UStaticMesh>(nullptr, TEXT("/Engine/BasicShapes/Cube.Cube"));
	check(Cube);
	FActorSpawnParameters P;
	P.SpawnCollisionHandlingOverride = ESpawnActorCollisionHandlingMethod::AlwaysSpawn;

	// The station: a 200 m x 200 m floor whose top is the station's z = 0, at
	// the ruleset's station pose (100 km along +x). Static: it never moves.
	Station = World->SpawnActor<AActor>(AActor::StaticClass(), FrameTransform(ORRERY_INTERIORS_STATION), P);
	{
		USceneComponent* Root = NewObject<USceneComponent>(Station, TEXT("Root"));
		Root->SetMobility(EComponentMobility::Movable);
		Station->SetRootComponent(Root);
		Root->RegisterComponent();
		Station->SetActorTransform(FrameTransform(ORRERY_INTERIORS_STATION));
		// Two slabs with the docking bay between them (station y 25..75 m):
		// the docked ship's deck fills the gap exactly, so the avatar walks
		// from station floor onto ship deck with no coplanar overlap for a
		// floor trace to pick at random.
		AddBox(Station, Root, Cube, FVector(0, -3750.0, -10.0), FVector(20000, 12500, 20), TEXT("FloorAft"), true);
		AddBox(Station, Root, Cube, FVector(0, 8750.0, -10.0), FVector(20000, 2500, 20), TEXT("FloorFore"), true);
	}

	// The ship: a movable root whose transform is written from the mirror
	// every tick; a deck 40 m x 50 m whose top is ship z = 0 (the CMC base),
	// wide enough for the 20 m x 3 m corridor loop the script walks;
	// the mech's platform 2.4 m square, its transform written from the mirror.
	Ship = World->SpawnActor<AActor>(AActor::StaticClass(), FTransform::Identity, P);
	{
		USceneComponent* Root = NewObject<USceneComponent>(Ship, TEXT("Root"));
		Root->SetMobility(EComponentMobility::Movable);
		Ship->SetRootComponent(Root);
		Root->RegisterComponent();
		Ship->SetActorTransform(FrameTransform(ORRERY_INTERIORS_SHIP));
		Deck = AddBox(Ship, Root, Cube, FVector(1000.0, 0, -10.0), FVector(4000, 5000, 20), TEXT("Deck"), true);
		// The mech: an unscaled frame root (what the avatar attaches to and
		// what reprojection pulls back through) with its platform box below.
		MechRoot = NewObject<USceneComponent>(Ship, TEXT("MechRoot"));
		MechRoot->SetMobility(EComponentMobility::Movable);
		MechRoot->SetupAttachment(Root);
		MechRoot->SetRelativeTransform(BodyTransform(ORRERY_INTERIORS_MECH));
		MechRoot->RegisterComponent();
		MechPlatform = AddBox(Ship, MechRoot, Cube, FVector(0, 0, -10.0), FVector(240, 240, 20), TEXT("Mech"), true);
		if (Interior == EInteriorsInterior::Resident)
		{
			SetInteriorPresent(true, Ship->GetActorTransform());
		}
	}

	if (Variant == EInteriorsVariant::Mirror)
	{
		MirrorAvatar = World->SpawnActor<AActor>(AActor::StaticClass(), FTransform::Identity, P);
		USceneComponent* Root = NewObject<USceneComponent>(MirrorAvatar, TEXT("Root"));
		Root->SetMobility(EComponentMobility::Movable);
		MirrorAvatar->SetRootComponent(Root);
		Root->RegisterComponent();
		UStaticMeshComponent* Body = AddBox(MirrorAvatar, Root, Cube, FVector(0, 0, 90.0), FVector(60, 60, 180), TEXT("Body"), true);
		Body->SetCollisionEnabled(ECollisionEnabled::NoCollision);
	}
	else
	{
		const FTransform Start = FTransform(FVector(0, 0, CapsuleHalfHeightCm)) * BodyTransform(ORRERY_INTERIORS_AVATAR) * FrameTransform(Sim->Body(ORRERY_INTERIORS_AVATAR).frame);
		Character = World->SpawnActor<AInteriorsCharacter>(AInteriorsCharacter::StaticClass(), Start, P);
		Character->AddTickPrerequisiteActor(this);
		UInteriorsMovement* M = Character->Movement();
		M->AddTickPrerequisiteActor(this);
		M->bDisableBasedMovement = (Variant == EInteriorsVariant::CmcNoBase);
		M->MaxWalkSpeed = 240.0f;
		M->MaxFlySpeed = 100000.0f;
		M->MaxAcceleration = 1.0e6f;
		M->BrakingDecelerationWalking = 1.0e6f;
		M->BrakingDecelerationFlying = 1.0e6f;
		M->GroundFriction = 8.0f;
		M->SetMovementMode(MOVE_Walking);
	}

	Camera = World->SpawnActor<ACameraActor>(ACameraActor::StaticClass(), FTransform::Identity, P);
	Camera->AttachToComponent(Ship->GetRootComponent(), FAttachmentTransformRules::KeepRelativeTransform);
	Camera->SetActorRelativeLocation(FVector(-1200.0, 0, 700.0));
	Camera->SetActorRelativeRotation(FRotator(-20.0, 0, 0));
	if (APlayerController* PC = World->GetFirstPlayerController())
	{
		PC->SetViewTarget(Camera);
	}
}

FTransform AInteriorsScenario::FrameTransform(uint64 Frame) const
{
	if (Frame == ORRERY_INTERIORS_UNIVERSE)
	{
		return FTransform::Identity;
	}
	const interiors_body& B = Sim->Body(Frame);
	// Compose: this frame's pose is in its parent frame.
	return MakeTransform(B.pos, B.yaw_urad, B.roll_urad) * FrameTransform(B.frame);
}

FTransform AInteriorsScenario::BodyTransform(uint64 Entity) const
{
	const interiors_body& B = Sim->Body(Entity);
	return MakeTransform(B.pos, B.yaw_urad, B.roll_urad);
}

void AInteriorsScenario::SetInteriorPresent(bool bPresent, const FTransform& ShipWorld)
{
	if (Interior == EInteriorsInterior::Stream)
	{
		if (bPresent && InteriorLevel == nullptr)
		{
			bool bOk = false;
			InteriorLevel = ULevelStreamingDynamic::LoadLevelInstance(this, TEXT("/Game/Maps/ShipInterior"), ShipWorld.GetLocation(), ShipWorld.Rotator(), bOk);
			if (InteriorLevel)
			{
				InteriorLevel->bShouldBlockOnLoad = true;
			}
			UE_LOG(LogInteriors, Display, TEXT("stream: load requested ok=%d"), bOk ? 1 : 0);
		}
		else if (!bPresent && InteriorLevel != nullptr)
		{
			InteriorLevel->SetIsRequestingUnloadAndRemoval(true);
			InteriorLevel = nullptr;
		}
		return;
	}
	if (Interior == EInteriorsInterior::Resident && !bPresent)
	{
		return;
	}
	if (bPresent && InteriorPieceComponents.Num() == 0)
	{
		// A corridor of boxes along the deck: bulkheads and fittings, attached
		// to the ship so they move with it. Clear of the walked loop (x 0..3 m,
		// y 0..20 m): the ruleset has no collision, so any contact would be a
		// CMC assertion by construction and would muddy the floor/base count.
		for (int32 I = 0; I < InteriorPieces; ++I)
		{
			const double X = 100.0 + (I % 40) * 100.0;
			const double Y = ((I / 40) % 2 == 0 ? -1.0 : 1.0) * (450.0 + (I / 80) * 20.0);
			const double Z = 50.0 + (I % 3) * 40.0;
			UStaticMeshComponent* C = AddBox(Ship, Ship->GetRootComponent(), Cube, FVector(X, Y, Z), FVector(40, 40, 40), NAME_None, true);
			InteriorPieceComponents.Add(C);
		}
		Row.Spawned += InteriorPieces;
	}
	else if (!bPresent && InteriorPieceComponents.Num() > 0)
	{
		for (UStaticMeshComponent* C : InteriorPieceComponents)
		{
			C->DestroyComponent();
		}
		InteriorPieceComponents.Empty();
		Row.Spawned -= InteriorPieces;
	}
}

void AInteriorsScenario::WriteMirror(const FString& Event)
{
	const interiors_body& Av = Sim->Body(ORRERY_INTERIORS_AVATAR);
	const interiors_body& ShipBody = Sim->Body(ORRERY_INTERIORS_SHIP);
	const FTransform ShipWorld = FrameTransform(ORRERY_INTERIORS_SHIP);
	const FTransform AvatarFrame = FrameTransform(Av.frame);
	const FVector TargetLocal(Av.pos[0] * MmToCm, Av.pos[1] * MmToCm, Av.pos[2] * MmToCm);
	const FTransform AvatarLocal = MakeTransform(Av.pos, Av.yaw_urad, 0);

	Ship->SetActorTransform(ShipWorld, false, nullptr, ETeleportType::TeleportPhysics);
	MechRoot->SetRelativeTransform(BodyTransform(ORRERY_INTERIORS_MECH), false, nullptr, ETeleportType::TeleportPhysics);

	Row.AvatarFrame = Av.frame;
	Row.Target = TargetLocal;
	Row.ShipWorld = ShipWorld.GetLocation();
	Row.ShipYaw = ShipBody.yaw_urad;
	Row.ShipRoll = ShipBody.roll_urad;
	Row.Event = Event;
	MaxAbsWorld = FMath::Max(MaxAbsWorld, Row.ShipWorld.GetAbsMax());

	USceneComponent* FrameComponent = nullptr;
	if (Av.frame == ORRERY_INTERIORS_STATION) FrameComponent = Station->GetRootComponent();
	else if (Av.frame == ORRERY_INTERIORS_SHIP) FrameComponent = Ship->GetRootComponent();
	else if (Av.frame == ORRERY_INTERIORS_MECH) FrameComponent = MechRoot;

	if (Variant == EInteriorsVariant::Mirror)
	{
		USceneComponent* Root = MirrorAvatar->GetRootComponent();
		if (MirrorFrame != Av.frame)
		{
			// The frame change: re-parent, then write the local pose. No
			// world-space step in between: the relative transform is the
			// ruleset's number, never a reprojection.
			if (FrameComponent)
			{
				Root->AttachToComponent(FrameComponent, FAttachmentTransformRules::KeepRelativeTransform);
			}
			else
			{
				Root->DetachFromComponent(FDetachmentTransformRules::KeepRelativeTransform);
			}
			MirrorFrame = Av.frame;
		}
		Root->SetRelativeTransform(AvatarLocal, false, nullptr, ETeleportType::TeleportPhysics);
		// Direct: what the component holds, minus the target. Reprojection:
		// the world transform pulled back through the frame's own world
		// transform (UE's, LWC) minus the target.
		Row.MirrorDirect = (Root->GetRelativeLocation() - TargetLocal) * 10.0;
		const FTransform FrameWorldUE = FrameComponent ? FrameComponent->GetComponentTransform() : FTransform::Identity;
		const FVector Reproj = FrameWorldUE.InverseTransformPosition(Root->GetComponentLocation());
		Row.MirrorReproj = (Reproj - TargetLocal) * 10.0;
	}
	else
	{
		UInteriorsMovement* M = Character->Movement();
		const FTransform TargetWorld = FTransform(FVector(0, 0, CapsuleHalfHeightCm)) * AvatarLocal * AvatarFrame;
		const FVector Down = -AvatarFrame.GetUnitAxis(EAxis::Z);
		M->SetGravityDirection(Down);
		if (Av.frame == ORRERY_INTERIORS_UNIVERSE)
		{
			if (M->MovementMode != MOVE_Flying) M->SetMovementMode(MOVE_Flying);
		}
		else if (M->MovementMode != MOVE_Walking)
		{
			M->SetMovementMode(MOVE_Walking);
		}
		M->BasedMovementApplied = 0;
		M->LastBasedDelta = FVector::ZeroVector;
		if (Variant == EInteriorsVariant::CmcDrive)
		{
			const FVector LocalVel(Av.vel[0] * MmToCm, Av.vel[1] * MmToCm, Av.vel[2] * MmToCm);
			const FVector WorldVel = AvatarFrame.TransformVectorNoScale(LocalVel);
			const double Speed = WorldVel.Size();
			M->MaxWalkSpeed = FMath::Max(1.0, Speed);
			M->MaxFlySpeed = FMath::Max(1.0, Speed);
			if (Speed > 0)
			{
				Character->AddMovementInput(WorldVel / Speed, 1.0f);
			}
		}
		else
		{
			Character->SetActorLocationAndRotation(TargetWorld.GetLocation(), TargetWorld.GetRotation(), false, nullptr, ETeleportType::TeleportPhysics);
		}
		Row.MirrorDirect = FVector::ZeroVector;
		const FTransform FrameWorldUE = FrameComponent ? FrameComponent->GetComponentTransform() : FTransform::Identity;
		const FVector Reproj = FrameWorldUE.InverseTransformPosition(Character->GetActorLocation()) - FVector(0, 0, CapsuleHalfHeightCm);
		Row.MirrorReproj = (Reproj - TargetLocal) * 10.0;
	}
}

void AInteriorsScenario::Tick(float DeltaSeconds)
{
	Super::Tick(DeltaSeconds);
	if (bFinished || !Sim)
	{
		return;
	}
	const double FrameStart = FPlatformTime::Seconds();
	Row = FRow();
	Row.Tick = CurrentTick;
	Row.FrameMs = (FrameStart - LastFrameStart) * 1000.0;
	LastFrameStart = FrameStart;
	Row.Gc = GcThisFrame;
	GcThisFrame = 0;
	Row.Spawned = InteriorPieceComponents.Num();

	if (CurrentTick >= TotalTicks)
	{
		Finish();
		return;
	}

	FString Event;
	// A correction arriving at this boundary (the same arrangement as the C
	// consumer's rollback mode; shape ship). Everything the presentation
	// has to absorb is measured here: re-emitted frame changes, the avatar's
	// frame before and after, the residual.
	FInteriorsSim::FArrangement* Corr = nullptr;
	if (Sim->MaybeCorrect(CurrentTick, Corr) && Corr)
	{
		CorrectionsApplied += 1;
		CorrectionsMismatchWindow += Corr->MismatchWindow;
		CorrectionNs.Add(static_cast<double>(Corr->Report.total_ns));
		if (Corr->Report.residual_mm > PresentationResidualMaxMm) PresentationResidualMaxMm = Corr->Report.residual_mm;
		if (Corr->Report.frame_before != Corr->Report.frame_after) FrameChangesAfterCorrection += 1;
		EventsReemitted = Sim->Predictor.events_reemitted_by_replay;
		Event = FString::Printf(TEXT("correction:%s:depth%llu:mis%u:resid%lld:frame%llu>%llu"),
								ANSI_TO_TCHAR(interiors_transition_name((interiors_transition_kind)Corr->Kind)),
								(unsigned long long)Corr->Report.depth, Corr->MismatchWindow, (long long)Corr->Report.residual_mm,
								(unsigned long long)Corr->Report.frame_before, (unsigned long long)Corr->Report.frame_after);
		// The corrected timeline may have the avatar elsewhere: re-write the
		// mirror before this tick's step, as a presentation layer would on
		// receiving the correction.
		WriteMirror(Event);
	}
	Sim->ArmDivergence(CurrentTick);

	const double T0 = FPlatformTime::Seconds();
	if (!Sim->Step(CurrentTick))
	{
		UE_LOG(LogInteriors, Error, TEXT("step failed at %llu"), (unsigned long long)CurrentTick);
		Finish();
		return;
	}
	Row.HostUs = (FPlatformTime::Seconds() - T0) * 1e6;

	TArray<TTuple<uint64, uint64, uint64>> Changes;
	uint64 EventCount = 0;
	Sim->DrainFrameChanges(Changes, EventCount);
	for (const auto& C : Changes)
	{
		const uint64 Entity = C.Get<0>(), From = C.Get<1>(), To = C.Get<2>();
		Event += FString::Printf(TEXT("%sframe:%llu:%llu>%llu"), Event.IsEmpty() ? TEXT("") : TEXT(";"), (unsigned long long)Entity, (unsigned long long)From, (unsigned long long)To);
		if (Entity == ORRERY_INTERIORS_AVATAR)
		{
			if (To == ORRERY_INTERIORS_SHIP && From != ORRERY_INTERIORS_MECH)
			{
				SetInteriorPresent(true, FrameTransform(ORRERY_INTERIORS_SHIP));
			}
			else if (From == ORRERY_INTERIORS_SHIP && To != ORRERY_INTERIORS_MECH)
			{
				SetInteriorPresent(false, FrameTransform(ORRERY_INTERIORS_SHIP));
			}
		}
	}
	if (InteriorLevel)
	{
		Row.StreamState = InteriorLevel->IsLevelVisible() ? 2 : (InteriorLevel->IsLevelLoaded() ? 1 : 0);
	}

	WriteMirror(Event);

	if (bScreenshots && !GUsingNullRHI && !Event.IsEmpty() && Event.Contains(TEXT("frame:")))
	{
		FScreenshotRequest::RequestScreenshot(OutDir / FString::Printf(TEXT("shot-%s-%s-%llu.png"), *Scene, VariantName(Variant), (unsigned long long)CurrentTick), false, false);
	}
	CurrentTick += 1;
}

void AInteriorsScenario::AfterPhysics()
{
	if (bFinished || !Sim || Row.Tick + 1 != CurrentTick)
	{
		return;
	}
	const interiors_body& Av = Sim->Body(ORRERY_INTERIORS_AVATAR);
	if (Character)
	{
		UInteriorsMovement* M = Character->Movement();
		USceneComponent* FrameComponent = nullptr;
		if (Av.frame == ORRERY_INTERIORS_STATION) FrameComponent = Station->GetRootComponent();
		else if (Av.frame == ORRERY_INTERIORS_SHIP) FrameComponent = Ship->GetRootComponent();
		else if (Av.frame == ORRERY_INTERIORS_MECH) FrameComponent = MechRoot;
		const FTransform FrameWorldUE = FrameComponent ? FrameComponent->GetComponentTransform() : FTransform::Identity;
		const FVector Local = FrameWorldUE.InverseTransformPosition(Character->GetActorLocation()) - FVector(0, 0, CapsuleHalfHeightCm);
		Row.CmcDelta = (Local - Row.Target) * 10.0;
		Row.CmcBasedDelta = M->LastBasedDelta;
		Row.CmcMode = static_cast<int32>(M->MovementMode);
		UPrimitiveComponent* Base = Character->GetMovementBase();
		const UPrimitiveComponent* Expected = Av.frame == ORRERY_INTERIORS_SHIP ? Deck : (Av.frame == ORRERY_INTERIORS_MECH ? MechPlatform : nullptr);
		Row.CmcBaseOk = (Av.frame == ORRERY_INTERIORS_STATION) ? (Base != nullptr && Base->GetOwner() == Station ? 1 : 0)
					   : (Av.frame == ORRERY_INTERIORS_UNIVERSE ? (Base == nullptr ? 1 : 0) : (Base == Expected ? 1 : 0));
	}

	const double DDirect = Row.MirrorDirect.Size(), DReproj = Row.MirrorReproj.Size(), DCmc = Row.CmcDelta.Size();
	DriftDirect.Add(DDirect);
	DriftReproj.Add(DReproj);
	DriftCmc.Add(DCmc);
	Assertions.Add(Character && DCmc > 1.0 ? 1 : 0);
	if (Character && DCmc > 1.0)
	{
		const bool bHorizontal = FMath::Abs(Row.CmcDelta.X) > 1.0 || FMath::Abs(Row.CmcDelta.Y) > 1.0;
		if (bHorizontal) AssertHorizontalCount += 1; else AssertVerticalCount += 1;
		if (!Row.CmcBasedDelta.IsNearlyZero(1e-4)) AssertWithBasedMovement += 1;
	}
	if (Character)
	{
		if (Row.CmcMode == 1) TicksWalking += 1; else if (Row.CmcMode == 3) TicksFalling += 1; else if (Row.CmcMode == 5) TicksFlying += 1; else TicksOtherMode += 1;
		if (Row.CmcBaseOk) TicksBaseOk += 1;
	}
	FrameMs.Add(Row.FrameMs);
	HostUs.Add(Row.HostUs);
	GcFlags.Add(Row.Gc);
	SpawnFlags.Add(Row.Spawned);

	if (Csv)
	{
		FString Line = FString::Printf(TEXT("%llu,%.3f,%.1f,%d,%d,%d,%llu,%.4f,%.4f,%.4f,%d,%d,%.3f,%.3f,%.3f,%.6f,%.6f,%.6f,%.6f,%.6f,%.6f,%.4f,%.4f,%.4f,%.4f,%.4f,%.4f,%d,%d,%s\n"),
									   (unsigned long long)Row.Tick, Row.FrameMs, Row.HostUs, Row.Gc, Row.Spawned, Row.StreamState, (unsigned long long)Row.AvatarFrame,
									   Row.ShipWorld.X, Row.ShipWorld.Y, Row.ShipWorld.Z, Row.ShipYaw, Row.ShipRoll,
									   Row.Target.X, Row.Target.Y, Row.Target.Z,
									   Row.MirrorDirect.X, Row.MirrorDirect.Y, Row.MirrorDirect.Z,
									   Row.MirrorReproj.X, Row.MirrorReproj.Y, Row.MirrorReproj.Z,
									   Row.CmcDelta.X, Row.CmcDelta.Y, Row.CmcDelta.Z,
									   Row.CmcBasedDelta.X, Row.CmcBasedDelta.Y, Row.CmcBasedDelta.Z,
									   Row.CmcMode, Row.CmcBaseOk, *Row.Event);
		auto Utf8 = StringCast<UTF8CHAR>(*Line);
		Csv->Serialize(const_cast<UTF8CHAR*>(Utf8.Get()), Utf8.Length());
	}
}

void AInteriorsScenario::WriteSummary()
{
	// Drift, in mm, per variant x scene; hitches around every transition,
	// attributed; the CMC assertion count; the ulp at the farthest world
	// position; the chain to compare with the C run.
	// The first frame carries world creation (BeginPlay spawns everything);
	// it is reported on its own, never as a hitch.
	int32 HitchesTotal = 0;
	const double FirstFrameMs = FrameMs.Num() ? FrameMs[0] : 0;
	TArray<double> SteadyFrameMs = FrameMs;
	if (SteadyFrameMs.Num()) SteadyFrameMs.RemoveAt(0);
	for (double Ms : SteadyFrameMs) if (Ms > HitchMs) HitchesTotal += 1;
	FString Transitions;
	for (int32 I = 0; I < TransitionTicks.Num(); ++I)
	{
		const int64 Tc = static_cast<int64>(TransitionTicks[I]);
		int32 Count = 0, GcCount = 0, SpawnCount = 0;
		double MaxMs = 0, AtMs = 0;
		for (int64 T = FMath::Max<int64>(1, Tc - 120); T <= FMath::Min<int64>(FrameMs.Num() - 1, Tc + 120); ++T)
		{
			if (FrameMs[T] > HitchMs)
			{
				Count += 1;
				if (GcFlags[T]) GcCount += 1;
				if (T > 0 && SpawnFlags[T] != SpawnFlags[T - 1]) SpawnCount += 1;
			}
			MaxMs = FMath::Max(MaxMs, FrameMs[T]);
		}
		// The frame that stepped the transition tick sees its wall time on the next row.
		if (Tc + 1 < FrameMs.Num()) AtMs = FrameMs[Tc + 1];
		Transitions += FString::Printf(TEXT("%s    {\"kind\": \"%s\", \"tick\": %lld, \"hitches\": %d, \"hitches_with_gc\": %d, \"hitches_with_spawn\": %d, \"max_frame_ms\": %.3f, \"transition_frame_ms\": %.3f}"),
									   I ? TEXT(",\n") : TEXT(""), ANSI_TO_TCHAR(interiors_transition_name((interiors_transition_kind)TransitionKinds[I])), (long long)Tc, Count, GcCount, SpawnCount, MaxMs, AtMs);
	}
	int32 AssertionCount = 0;
	for (int32 I = 0; I < Assertions.Num(); ++I)
	{
		if (Assertions[I]) AssertionCount += 1;
	}
	const double UlpDouble = std::nextafter(MaxAbsWorld, INFINITY) - MaxAbsWorld;
	const float MaxF = static_cast<float>(MaxAbsWorld);
	const double UlpFloat = static_cast<double>(std::nextafter(MaxF, INFINITY)) - MaxF;
	FString Corrections;
	if (bRollback)
	{
		Corrections = FString::Printf(TEXT(",\n  \"rollback\": {\"corrections\": %d, \"mismatch_window\": %d, \"events_reemitted_by_replay\": %llu, \"presentation_residual_mm_max\": %lld, \"avatar_frame_differs_after_correction\": %d, \"total_ns_p50\": %.0f, \"total_ns_p99\": %.0f, \"total_ns_max\": %.0f}"),
									  CorrectionsApplied, CorrectionsMismatchWindow, (unsigned long long)EventsReemitted, (long long)PresentationResidualMaxMm, FrameChangesAfterCorrection,
									  Percentile(CorrectionNs, 50), Percentile(CorrectionNs, 99), Percentile(CorrectionNs, 100));
	}
	FString Json = FString::Printf(TEXT("{\n  \"schema\": \"orrery-interiors-unreal/1\",\n  \"scene\": \"%s\", \"variant\": \"%s\", \"interior\": \"%s\", \"ticks\": %d,\n")
								   TEXT("  \"fixed_timestep\": %d, \"dt\": %.6f, \"null_rhi\": %d, \"chain\": \"%016llx\",\n")
								   TEXT("  \"drift_mm\": {\"direct\": {\"p50\": %.6f, \"p99\": %.6f, \"max\": %.6f}, \"reproj\": {\"p50\": %.6f, \"p99\": %.6f, \"max\": %.6f}, \"cmc\": {\"p50\": %.4f, \"p99\": %.4f, \"max\": %.4f}},\n")
								   TEXT("  \"cmc_assertions\": %d, \"cmc_assertion_ticks\": %d, \"cmc_assertions_vertical_only\": %d, \"cmc_assertions_horizontal\": %d, \"cmc_assertions_with_based_movement\": %d,\n")
								   TEXT("  \"cmc_ticks\": {\"walking\": %d, \"falling\": %d, \"flying\": %d, \"other\": %d, \"base_ok\": %d},\n")
								   TEXT("  \"frame_ms\": {\"first\": %.3f, \"p50\": %.3f, \"p99\": %.3f, \"max\": %.3f, \"hitches\": %d},\n")
								   TEXT("  \"host_us\": {\"p50\": %.1f, \"p99\": %.1f, \"max\": %.1f},\n")
								   TEXT("  \"world_max_abs_cm\": %.3f, \"ulp_double_mm\": %.3e, \"ulp_float_mm\": %.3e,\n")
								   TEXT("  \"transitions\": [\n%s\n  ]%s\n}\n"),
								   *Scene, VariantName(Variant), InteriorName(Interior), FrameMs.Num(),
								   FApp::UseFixedTimeStep() ? 1 : 0, FApp::GetFixedDeltaTime(), GUsingNullRHI ? 1 : 0, (unsigned long long)Sim->Predictor.chain,
								   Percentile(DriftDirect, 50), Percentile(DriftDirect, 99), Percentile(DriftDirect, 100),
								   Percentile(DriftReproj, 50), Percentile(DriftReproj, 99), Percentile(DriftReproj, 100),
								   Percentile(DriftCmc, 50), Percentile(DriftCmc, 99), Percentile(DriftCmc, 100),
								   AssertionCount, Assertions.Num(), AssertVerticalCount, AssertHorizontalCount, AssertWithBasedMovement,
								   TicksWalking, TicksFalling, TicksFlying, TicksOtherMode, TicksBaseOk,
								   FirstFrameMs, Percentile(SteadyFrameMs, 50), Percentile(SteadyFrameMs, 99), Percentile(SteadyFrameMs, 100), HitchesTotal,
								   Percentile(HostUs, 50), Percentile(HostUs, 99), Percentile(HostUs, 100),
								   MaxAbsWorld, UlpDouble * 10.0, UlpFloat * 10.0,
								   *Transitions, *Corrections);
	const FString Path = OutDir / FString::Printf(TEXT("summary-%s-%s-%s.json"), *Scene, VariantName(Variant), InteriorName(Interior));
	FFileHelper::SaveStringToFile(Json, *Path);
	UE_LOG(LogInteriors, Display, TEXT("spike 1045 summary written: %s\n%s"), *Path, *Json);
}

void AInteriorsScenario::Finish()
{
	if (bFinished)
	{
		return;
	}
	bFinished = true;
	if (Sim && Sim->Host)
	{
		WriteSummary();
	}
	if (Csv)
	{
		Csv->Close();
		delete Csv;
		Csv = nullptr;
	}
	UE_LOG(LogInteriors, Display, TEXT("spike 1045: done, exiting"));
	FPlatformMisc::RequestExit(false);
}

// ---------------------------------------------------------------------------

AInteriorsProbe::AInteriorsProbe()
{
	PrimaryActorTick.bCanEverTick = true;
	PrimaryActorTick.TickGroup = TG_PostPhysics;
}

void AInteriorsProbe::Tick(float DeltaSeconds)
{
	Super::Tick(DeltaSeconds);
	if (Scenario)
	{
		Scenario->AfterPhysics();
	}
}
