// Spike #1044 — TraceBody commandlet.
//
//   UnrealEditor-Cmd OneBodyCook.uproject -run=TraceBody -map=<dir>/Body_1.umap -rays=<rays.bin> -out=<hits.bin> [-complex=1]
//
// A separate process from the cook: it loads the saved map from disk, initialises an editor world with a
// physics scene (the same InitializationValues UWorldPartitionBuilder uses), and traces every ray in the
// file with UWorld::LineTraceSingleByChannel on ECC_WorldStatic. Per ray it records which component
// class answered, so a terrain hit (UMeshPartitionCollisionComponent — Unreal's cooked trimesh, not the
// Nanite render mesh) is never confused with a scatter hit (ISM instance) or anything else.
#include "TraceBodyCommandlet.h"

#include "Editor.h"
#include "EditorWorldUtils.h"
#include "Engine/World.h"
#include "Engine/Level.h"
#include "EngineUtils.h"
#include "Components/InstancedStaticMeshComponent.h"
#include "Components/PrimitiveComponent.h"
#include "CollisionQueryParams.h"
#include "Misc/FileHelper.h"
#include "Misc/PackageName.h"
#include "Misc/Paths.h"
#include "HAL/PlatformTime.h"
#include "Serialization/BufferArchive.h"
#include "Serialization/MemoryReader.h"
#include "UObject/Package.h"
#include "Engine/StaticMesh.h"
#include "StaticMeshCompiler.h"
#include "PhysicsEngine/BodyInstance.h"
#include "Commandlets/Commandlet.h"

#include "MeshPartitionCollisionComponent.h"
#include "MeshPartitionCompiledSection.h"

DEFINE_LOG_CATEGORY_STATIC(LogTraceBody, Log, All);

UTraceBodyCommandlet::UTraceBodyCommandlet()
{
	IsClient = false;
	IsEditor = true;
	IsServer = false;
	LogToConsole = true;
}

int32 UTraceBodyCommandlet::Main(const FString& Params)
{
	TArray<FString> Tokens, Switches;
	TMap<FString, FString> ParamVals;
	ParseCommandLine(*Params, Tokens, Switches, ParamVals);

	const FString* MapPath = ParamVals.Find(TEXT("map"));
	const FString* RaysPath = ParamVals.Find(TEXT("rays"));
	const FString* OutPath = ParamVals.Find(TEXT("out"));
	if (!MapPath || !RaysPath || !OutPath)
	{
		UE_LOG(LogTraceBody, Error, TEXT("usage: -map=<Body.umap> -rays=<rays.bin> -out=<hits.bin> [-complex=0|1]"));
		return 1;
	}
	bool bComplex = true;
	if (const FString* S = ParamVals.Find(TEXT("complex"))) { bComplex = FCString::Atoi(**S) != 0; }

	// Mount the output directory so the map's package name resolves the way the cook wrote it.
	const FString MapDir = FPaths::GetPath(*MapPath);
	const FString MapStem = FPaths::GetBaseFilename(*MapPath);
	FPackageName::RegisterMountPoint(TEXT("/OneBodyOut/"), MapDir / TEXT(""));
	const FString PackageName = TEXT("/OneBodyOut/") + MapStem;

	// ---- rays --------------------------------------------------------------------------------------
	TArray<uint8> RayBytes;
	if (!FFileHelper::LoadFileToArray(RayBytes, **RaysPath))
	{
		UE_LOG(LogTraceBody, Error, TEXT("cannot read %s"), **RaysPath);
		return 1;
	}
	FMemoryReader Reader(RayBytes);
	uint8 Magic[8];
	Reader.Serialize(Magic, 8);
	if (FMemory::Memcmp(Magic, "ORRYRAY1", 8) != 0)
	{
		UE_LOG(LogTraceBody, Error, TEXT("bad ray file magic"));
		return 1;
	}
	uint64 RaySeed; uint32 N;
	Reader << RaySeed << N;
	struct FRay { int64 O[3]; int64 D[3]; int64 MaxMm; };
	TArray<FRay> Rays;
	Rays.SetNumUninitialized(N);
	for (uint32 I = 0; I < N; ++I)
	{
		Reader << Rays[I].O[0] << Rays[I].O[1] << Rays[I].O[2] << Rays[I].D[0] << Rays[I].D[1] << Rays[I].D[2] << Rays[I].MaxMm;
	}
	UE_LOG(LogTraceBody, Display, TEXT("TraceBody map=%s rays=%u (seed %llu) complex=%d"), **MapPath, N, (unsigned long long)RaySeed, bComplex);

	// ---- world from disk ---------------------------------------------------------------------------
	const double TLoad0 = FPlatformTime::Seconds();
	UPackage* Package = LoadPackage(nullptr, *PackageName, LOAD_None);
	if (!Package)
	{
		UE_LOG(LogTraceBody, Error, TEXT("cannot load package %s"), *PackageName);
		return 1;
	}
	UWorld* World = UWorld::FindWorldInPackage(Package);
	if (!World)
	{
		UE_LOG(LogTraceBody, Error, TEXT("no world in %s"), *PackageName);
		return 1;
	}
	UWorld::InitializationValues IVS;
	IVS.RequiresHitProxies(false).ShouldSimulatePhysics(false).EnableTraceCollision(true).CreateNavigation(false).CreateAISystem(false).AllowAudioPlayback(false).CreatePhysicsScene(true);
	FScopedEditorWorld ScopedWorld(World, IVS);
	World->UpdateWorldComponents(true, false);
	// UInstancedStaticMeshComponent::ShouldCreatePhysicsState() refuses while its static mesh is still
	// being async-compiled (InstancedStaticMesh.cpp:4735), and at registration time the engine
	// BasicShapes still are. Finish compilation, then give every ISM its physics state back.
	FStaticMeshCompilingManager::Get().FinishAllCompilation();
	for (TActorIterator<AActor> It(World); It; ++It)
	{
		TArray<UInstancedStaticMeshComponent*> Isms;
		It->GetComponents<UInstancedStaticMeshComponent>(Isms);
		for (UInstancedStaticMeshComponent* Ism : Isms)
		{
			if (!Ism->IsPhysicsStateCreated()) { Ism->RecreatePhysicsState(); }
		}
	}
	// Let the physics scene flush its pending static bodies into the query structure before tracing.
	for (int32 K = 0; K < 3; ++K) { CommandletHelpers::TickEngine(World, 1.0 / 30.0); }
	const double TLoad1 = FPlatformTime::Seconds();

	int32 TerrainComponents = 0, IsmComponents = 0, IsmInstances = 0, OtherColliders = 0;
	for (TActorIterator<AActor> It(World); It; ++It)
	{
		TArray<UPrimitiveComponent*> Prims;
		It->GetComponents<UPrimitiveComponent>(Prims);
		for (UPrimitiveComponent* P : Prims)
		{
			if (P->GetCollisionEnabled() == ECollisionEnabled::NoCollision) { continue; }
			if (P->IsA<UE::MeshPartition::UMeshPartitionCollisionComponent>()) { ++TerrainComponents; }
			else if (UInstancedStaticMeshComponent* Ism = Cast<UInstancedStaticMeshComponent>(P))
			{
				++IsmComponents; IsmInstances += Ism->GetInstanceCount();
				UE_LOG(LogTraceBody, Display, TEXT("ISM %s: mesh=%s instances=%d physics_state=%d profile=%s collision_enabled=%d mobility=%d"),
					*Ism->GetName(), Ism->GetStaticMesh() ? *Ism->GetStaticMesh()->GetName() : TEXT("none"), Ism->GetInstanceCount(),
					Ism->IsPhysicsStateCreated() ? 1 : 0, *Ism->GetCollisionProfileName().ToString(), (int32)Ism->GetCollisionEnabled(), (int32)Ism->Mobility);
			}
			else { ++OtherColliders; UE_LOG(LogTraceBody, Warning, TEXT("other collider: %s on %s"), *P->GetClass()->GetName(), *It->GetActorNameOrLabel()); }
		}
	}
	UE_LOG(LogTraceBody, Display, TEXT("world loaded in %.2fs: %d terrain collision components, %d ISM components (%d instances), %d other colliders"), TLoad1 - TLoad0, TerrainComponents, IsmComponents, IsmInstances, OtherColliders);

	// ---- trace -------------------------------------------------------------------------------------
	FBufferArchive Out;
	Out.Serialize((void*)"ORRYHIT1", 8);
	uint32 NW = N; uint8 ComplexW = bComplex ? 1 : 0;
	Out << NW << ComplexW;
	FCollisionQueryParams QueryParams(SCENE_QUERY_STAT(OneBodyTrace), bComplex);
	QueryParams.bReturnFaceIndex = true;
	int32 Hits = 0, TerrainHits = 0, IsmHits = 0, OtherHits = 0, Penetrating = 0;
	const double TTrace0 = FPlatformTime::Seconds();
	for (uint32 I = 0; I < N; ++I)
	{
		const FRay& R = Rays[I];
		const FVector Start(R.O[0] / 10.0, R.O[1] / 10.0, R.O[2] / 10.0);
		const FVector Dir(R.D[0] / 1.0e9, R.D[1] / 1.0e9, R.D[2] / 1.0e9);
		const FVector End = Start + Dir * (R.MaxMm / 10.0);
		FHitResult Hit;
		const bool bHit = World->LineTraceSingleByChannel(Hit, Start, End, ECC_WorldStatic, QueryParams);
		uint8 HitW = bHit ? 1 : 0;
		int64 DistMm = bHit ? llround((double)Hit.Distance * 10.0) : -1;
		int32 Nx = 0, Ny = 0, Nz = 0;
		uint8 Kind = 0, Pen = 0;
		int32 Face = -1;
		if (bHit)
		{
			++Hits;
			Nx = (int32)llround(Hit.ImpactNormal.X * 1.0e6); Ny = (int32)llround(Hit.ImpactNormal.Y * 1.0e6); Nz = (int32)llround(Hit.ImpactNormal.Z * 1.0e6);
			if (const UPrimitiveComponent* C = Hit.Component.Get())
			{
				if (C->IsA<UE::MeshPartition::UMeshPartitionCollisionComponent>()) { Kind = 1; ++TerrainHits; }
				else if (C->IsA<UInstancedStaticMeshComponent>()) { Kind = 2; ++IsmHits; }
				else { Kind = 3; ++OtherHits; }
			}
			Pen = Hit.bStartPenetrating ? 1 : 0;
			if (Pen) { ++Penetrating; }
			Face = Hit.FaceIndex;
		}
		Out << HitW << DistMm << Nx << Ny << Nz << Kind << Pen << Face;
	}
	const double TTrace1 = FPlatformTime::Seconds();
	UE_LOG(LogTraceBody, Display, TEXT("traced %u rays in %.3fs: %d hits (%d terrain, %d ISM, %d other), %d start-penetrating; trace channel WorldStatic, bTraceComplex=%d"), N, TTrace1 - TTrace0, Hits, TerrainHits, IsmHits, OtherHits, Penetrating, bComplex);

	if (!FFileHelper::SaveArrayToFile(Out, **OutPath))
	{
		UE_LOG(LogTraceBody, Error, TEXT("cannot write %s"), **OutPath);
		return 1;
	}
	return 0;
}
