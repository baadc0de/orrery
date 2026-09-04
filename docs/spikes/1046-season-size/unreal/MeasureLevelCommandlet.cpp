// Spike #1046 — MeasureLevel commandlet.
//
//   UnrealEditor-Cmd OneBodyCook.uproject -run=MeasureLevel -map=/Game/Variant_Horror/Lvl_Horror -out=<dir> [-id=1]
//
// Loads a level package by name (so OFPA external actors resolve), initialises an editor world with a
// physics scene exactly as spike 2's TraceBody does, then walks every UStaticMeshComponent (including
// instanced ones) that has collision enabled and:
//   * counts LOD0 render triangles, Chaos simple shapes, and whether a complex trimesh exists;
//   * flattens LOD0 to world-space integer millimetres and writes spike 2's `tri` collision package
//     (OrreryExport::WriteTri) so the interior's ruleset half is measured in the same representation
//     and the same bytes-per-triangle as the PCG body.
// It writes <out>/level-<id>.tri.collision and <out>/level-<id>.measure.json.
#include "MeasureLevelCommandlet.h"
#include "OrreryExport.h"

#include "Editor.h"
#include "EditorWorldUtils.h"
#include "Engine/World.h"
#include "Engine/Level.h"
#include "EngineUtils.h"
#include "Components/InstancedStaticMeshComponent.h"
#include "Components/StaticMeshComponent.h"
#include "Components/PrimitiveComponent.h"
#include "Engine/StaticMesh.h"
#include "StaticMeshResources.h"
#include "StaticMeshCompiler.h"
#include "PhysicsEngine/BodySetup.h"
#include "Misc/FileHelper.h"
#include "Misc/PackageName.h"
#include "Misc/Paths.h"
#include "HAL/PlatformTime.h"
#include "UObject/Package.h"
#include "Dom/JsonObject.h"
#include "Serialization/JsonSerializer.h"
#include "Serialization/JsonWriter.h"
#include "Commandlets/Commandlet.h"

DEFINE_LOG_CATEGORY_STATIC(LogMeasureLevel, Log, All);

UMeasureLevelCommandlet::UMeasureLevelCommandlet()
{
	IsClient = false;
	IsEditor = true;
	IsServer = false;
	LogToConsole = true;
}

int32 UMeasureLevelCommandlet::Main(const FString& Params)
{
	TArray<FString> Tokens, Switches;
	TMap<FString, FString> ParamVals;
	ParseCommandLine(*Params, Tokens, Switches, ParamVals);

	const FString* MapName = ParamVals.Find(TEXT("map"));
	const FString* OutDir = ParamVals.Find(TEXT("out"));
	if (!MapName || !OutDir)
	{
		UE_LOG(LogMeasureLevel, Error, TEXT("usage: -map=/Game/Path/Level -out=<dir> [-id=1]"));
		return 1;
	}
	uint32 Id = 1;
	if (const FString* S = ParamVals.Find(TEXT("id"))) { Id = (uint32)FCString::Atoi(**S); }
	IFileManager::Get().MakeDirectory(**OutDir, true);

	const double TLoad0 = FPlatformTime::Seconds();
	UPackage* Package = LoadPackage(nullptr, **MapName, LOAD_None);
	if (!Package)
	{
		UE_LOG(LogMeasureLevel, Error, TEXT("cannot load package %s"), **MapName);
		return 1;
	}
	UWorld* World = UWorld::FindWorldInPackage(Package);
	if (!World)
	{
		UE_LOG(LogMeasureLevel, Error, TEXT("no world in %s"), **MapName);
		return 1;
	}
	UWorld::InitializationValues IVS;
	IVS.RequiresHitProxies(false).ShouldSimulatePhysics(false).EnableTraceCollision(true).CreateNavigation(false).CreateAISystem(false).AllowAudioPlayback(false).CreatePhysicsScene(true);
	FScopedEditorWorld ScopedWorld(World, IVS);
	World->UpdateWorldComponents(true, false);
	FStaticMeshCompilingManager::Get().FinishAllCompilation();
	const double TLoad1 = FPlatformTime::Seconds();

	FOrreryBodyExport Body;
	Body.Seed = 0;
	Body.BodyId = Id;
	int32 Actors = 0, Components = 0, NoCollision = 0, Instances = 0, SimpleShapes = 0, ComplexTrimeshes = 0, OtherPrims = 0;
	uint64 Tris = 0, Verts = 0;
	TMap<FString, int32> MeshInstanceCount;
	TMap<FString, int32> MeshLod0Tris;
	TSet<FString> OtherPrimClasses;

	for (TActorIterator<AActor> It(World); It; ++It)
	{
		++Actors;
		TArray<UPrimitiveComponent*> Prims;
		It->GetComponents<UPrimitiveComponent>(Prims);
		for (UPrimitiveComponent* P : Prims)
		{
			if (P->GetCollisionEnabled() == ECollisionEnabled::NoCollision) { ++NoCollision; continue; }
			UStaticMeshComponent* Smc = Cast<UStaticMeshComponent>(P);
			if (!Smc || !Smc->GetStaticMesh())
			{
				++OtherPrims;
				OtherPrimClasses.Add(P->GetClass()->GetName());
				continue;
			}
			++Components;
			UStaticMesh* Mesh = Smc->GetStaticMesh();
			const FStaticMeshRenderData* RD = Mesh->GetRenderData();
			if (!RD || RD->LODResources.Num() == 0) { continue; }
			const FStaticMeshLODResources& LOD = RD->LODResources[0];
			TArray<uint32> Indices;
			LOD.IndexBuffer.GetCopy(Indices);
			const uint32 NumVerts = LOD.VertexBuffers.PositionVertexBuffer.GetNumVertices();
			const FString MeshPath = Mesh->GetPathName();
			MeshLod0Tris.Add(MeshPath, Indices.Num() / 3);
			if (UBodySetup* BS = Mesh->GetBodySetup())
			{
				SimpleShapes += BS->AggGeom.GetElementCount();
				ComplexTrimeshes += BS->TriMeshGeometries.Num();
			}

			TArray<FTransform> Xforms;
			if (UInstancedStaticMeshComponent* Ism = Cast<UInstancedStaticMeshComponent>(Smc))
			{
				for (int32 I = 0; I < Ism->GetInstanceCount(); ++I)
				{
					FTransform T;
					if (Ism->GetInstanceTransform(I, T, true)) { Xforms.Add(T); }
				}
			}
			else
			{
				Xforms.Add(Smc->GetComponentTransform());
			}
			for (const FTransform& T : Xforms)
			{
				FOrreryInstance& Inst = Body.Instances.AddDefaulted_GetRef();
				Inst.MeshPath = MeshPath;
				Inst.InstanceIndex = Instances;
				Inst.Soup.Verts.Reserve(NumVerts);
				for (uint32 V = 0; V < NumVerts; ++V)
				{
					const FVector3d Pw = T.TransformPosition(FVector3d(LOD.VertexBuffers.PositionVertexBuffer.VertexPosition(V)));
					Inst.Soup.Verts.Add(FInt64Vector(OrreryExport::ToMm(Pw.X), OrreryExport::ToMm(Pw.Y), OrreryExport::ToMm(Pw.Z)));
				}
				Inst.Soup.Tris.Reserve(Indices.Num() / 3);
				for (int32 K = 0; K + 2 < Indices.Num(); K += 3)
				{
					Inst.Soup.Tris.Add(FIntVector((int32)Indices[K], (int32)Indices[K + 1], (int32)Indices[K + 2]));
				}
				Tris += Indices.Num() / 3;
				Verts += NumVerts;
				++Instances;
				MeshInstanceCount.FindOrAdd(MeshPath)++;
			}
		}
	}

	FInt64Vector Mn(MAX_int64), Mx(MIN_int64);
	for (const FOrreryInstance& Inst : Body.Instances)
	{
		for (const FInt64Vector& V : Inst.Soup.Verts)
		{
			Mn.X = FMath::Min(Mn.X, V.X); Mn.Y = FMath::Min(Mn.Y, V.Y); Mn.Z = FMath::Min(Mn.Z, V.Z);
			Mx.X = FMath::Max(Mx.X, V.X); Mx.Y = FMath::Max(Mx.Y, V.Y); Mx.Z = FMath::Max(Mx.Z, V.Z);
		}
	}
	if (Instances == 0) { Mn = FInt64Vector(0); Mx = FInt64Vector(0); }
	Body.BoundsMin = Mn;
	Body.BoundsMax = Mx;

	const FString TriPath = *OutDir / FString::Printf(TEXT("level-%u.tri.collision"), Id);
	const double TW0 = FPlatformTime::Seconds();
	const bool bOk = OrreryExport::WriteTri(Body, TriPath);
	const double TW1 = FPlatformTime::Seconds();
	const int64 TriBytes = IFileManager::Get().FileSize(*TriPath);

	UE_LOG(LogMeasureLevel, Display, TEXT("level %s: %d actors, %d static-mesh components with collision (%d flattened instances), %llu LOD0 tris, %llu verts, %d simple shapes, %d complex trimeshes, %d no-collision prims, %d other colliding prims; tri package %lld bytes in %.3fs; load %.2fs"),
		**MapName, Actors, Components, Instances, (unsigned long long)Tris, (unsigned long long)Verts, SimpleShapes, ComplexTrimeshes, NoCollision, OtherPrims, (long long)TriBytes, TW1 - TW0, TLoad1 - TLoad0);

	TSharedPtr<FJsonObject> Root = MakeShared<FJsonObject>();
	Root->SetStringField(TEXT("map"), *MapName);
	Root->SetNumberField(TEXT("id"), Id);
	Root->SetStringField(TEXT("engine"), FEngineVersion::Current().ToString());
	Root->SetNumberField(TEXT("actors"), Actors);
	Root->SetNumberField(TEXT("static_mesh_components_with_collision"), Components);
	Root->SetNumberField(TEXT("flattened_instances"), Instances);
	Root->SetNumberField(TEXT("lod0_tris"), (double)Tris);
	Root->SetNumberField(TEXT("lod0_verts"), (double)Verts);
	Root->SetNumberField(TEXT("simple_shapes"), SimpleShapes);
	Root->SetNumberField(TEXT("complex_trimeshes"), ComplexTrimeshes);
	Root->SetNumberField(TEXT("no_collision_prims"), NoCollision);
	Root->SetNumberField(TEXT("other_colliding_prims"), OtherPrims);
	TArray<TSharedPtr<FJsonValue>> OtherClasses;
	for (const FString& C : OtherPrimClasses) { OtherClasses.Add(MakeShared<FJsonValueString>(C)); }
	Root->SetArrayField(TEXT("other_colliding_prim_classes"), OtherClasses);
	TSharedPtr<FJsonObject> Meshes = MakeShared<FJsonObject>();
	for (const TPair<FString, int32>& Kv : MeshInstanceCount)
	{
		TSharedPtr<FJsonObject> M = MakeShared<FJsonObject>();
		M->SetNumberField(TEXT("instances"), Kv.Value);
		M->SetNumberField(TEXT("lod0_tris"), MeshLod0Tris.FindRef(Kv.Key));
		Meshes->SetObjectField(Kv.Key, M);
	}
	Root->SetObjectField(TEXT("meshes"), Meshes);
	TSharedPtr<FJsonObject> Bounds = MakeShared<FJsonObject>();
	Bounds->SetNumberField(TEXT("min_x_mm"), (double)Mn.X); Bounds->SetNumberField(TEXT("min_y_mm"), (double)Mn.Y); Bounds->SetNumberField(TEXT("min_z_mm"), (double)Mn.Z);
	Bounds->SetNumberField(TEXT("max_x_mm"), (double)Mx.X); Bounds->SetNumberField(TEXT("max_y_mm"), (double)Mx.Y); Bounds->SetNumberField(TEXT("max_z_mm"), (double)Mx.Z);
	Root->SetObjectField(TEXT("bounds"), Bounds);
	TSharedPtr<FJsonObject> Tri = MakeShared<FJsonObject>();
	Tri->SetStringField(TEXT("path"), TriPath);
	Tri->SetNumberField(TEXT("bytes"), (double)TriBytes);
	Tri->SetNumberField(TEXT("seconds"), TW1 - TW0);
	Tri->SetBoolField(TEXT("ok"), bOk);
	Root->SetObjectField(TEXT("tri"), Tri);
	Root->SetNumberField(TEXT("load_s"), TLoad1 - TLoad0);

	FString Json;
	TSharedRef<TJsonWriter<TCHAR, TPrettyJsonPrintPolicy<TCHAR>>> Writer = TJsonWriterFactory<TCHAR, TPrettyJsonPrintPolicy<TCHAR>>::Create(&Json);
	FJsonSerializer::Serialize(Root.ToSharedRef(), Writer);
	const FString JsonPath = *OutDir / FString::Printf(TEXT("level-%u.measure.json"), Id);
	FFileHelper::SaveStringToFile(Json, *JsonPath);
	return bOk ? 0 : 1;
}
