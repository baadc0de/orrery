// Spike #1044 — CookBody commandlet.
//
//   UnrealEditor-Cmd OneBodyCook.uproject -run=CookBody -seed=<u64> -body=<id> -out=<dir> [-size=256] [-spacing=1] [-density=0.03]
//
// Path mirrored: UWorldPartitionMeshPartitionBuilder::Run (WorldPartitionMeshPartitionBuilder.cpp) —
// modifiers -> FMeshData (ModifiersProcessed) -> PrepareCompiledSections -> MakeTransformerUnit ->
// UMeshPartitionEditorComponent::LaunchTransformers -> PostProcessSection. This commandlet drives the
// same UMeshPartitionEditorComponent entry points directly (no World Partition), with our
// FOrreryExportTransformer appended to the pipeline behind FStaticMeshTransformer and FCollisionTransformer.
#include "CookBodyCommandlet.h"
#include "OrreryExport.h"

#include "Editor.h"
#include "Engine/World.h"
#include "Engine/Level.h"
#include "Engine/StaticMesh.h"
#include "Engine/Engine.h"
#include "EngineUtils.h"
#include "Components/BoxComponent.h"
#include "Components/InstancedStaticMeshComponent.h"
#include "PhysicsEngine/BodySetup.h"
#include "StaticMeshResources.h"
#include "StaticMeshCompiler.h"
#include "Misc/Paths.h"
#include "Misc/FileHelper.h"
#include "Misc/PackageName.h"
#include "HAL/PlatformTime.h"
#include "HAL/FileManager.h"
#include "UObject/SavePackage.h"
#include "UObject/UnrealType.h"
#include "Dom/JsonObject.h"
#include "Serialization/JsonSerializer.h"
#include "Serialization/JsonWriter.h"
#include "StructUtils/InstancedStruct.h"

#include "MeshPartition.h"
#include "MeshPartitionDefinition.h"
#include "MeshPartitionEditorComponent.h"
#include "MeshPartitionTransformerPipeline.h"
#include "MeshPartitionMeshBuilder.h"
#include "MeshPartitionMeshBuilderCommon.h"
#include "MeshPartitionCompiledSection.h"
#include "MeshPartitionCollisionComponent.h"
#include "MeshPartitionModifierComponent.h"
#include "Modifiers/MeshPartitionNoiseModifier.h"
#include "Modifiers/MeshPartitionMeshProvider.h"
#include "Generators/RectangleMeshGenerator.h"
#include "DynamicMesh/DynamicMesh3.h"

#include "PCGGraph.h"
#include "PCGNode.h"
#include "PCGPin.h"
#include "PCGComponent.h"
#include "PCGCommon.h"
#include "PCGInputOutputSettings.h"
#include "Subsystems/PCGSubsystem.h"
#include "Elements/PCGWorldQuery.h"
#include "Elements/PCGSurfaceSampler.h"
#include "Elements/PCGTransformPoints.h"
#include "Elements/PCGStaticMeshSpawner.h"
#include "MeshSelectors/PCGMeshSelectorWeighted.h"

DEFINE_LOG_CATEGORY_STATIC(LogCookBody, Log, All);

using namespace UE::MeshPartition;
using UE::Geometry::FDynamicMesh3;

namespace
{
	template <typename T>
	void SetProp(UObject* Obj, const TCHAR* Name, const T& Value)
	{
		FProperty* P = Obj->GetClass()->FindPropertyByName(Name);
		checkf(P, TEXT("property %s not found on %s"), Name, *Obj->GetClass()->GetName());
		*P->ContainerPtrToValuePtr<T>(Obj) = Value;
	}

	template <typename T>
	T* PropPtr(UObject* Obj, const TCHAR* Name)
	{
		FProperty* P = Obj->GetClass()->FindPropertyByName(Name);
		checkf(P, TEXT("property %s not found on %s"), Name, *Obj->GetClass()->GetName());
		return P->ContainerPtrToValuePtr<T>(Obj);
	}

	// splitmix64 over the seed: every knob the body takes from the seed goes through this, so the body is a
	// pure function of (seed, body id) and nothing else.
	struct FSeedStream
	{
		uint64 State;
		explicit FSeedStream(uint64 Seed) : State(Seed) {}
		uint64 Next()
		{
			State += 0x9E3779B97F4A7C15ull;
			uint64 Z = State;
			Z = (Z ^ (Z >> 30)) * 0xBF58476D1CE4E5B9ull;
			Z = (Z ^ (Z >> 27)) * 0x94D049BB133111EBull;
			return Z ^ (Z >> 31);
		}
		double Unit() { return (double)(Next() >> 11) / 9007199254740992.0; }
		double Range(double Lo, double Hi) { return Lo + (Hi - Lo) * Unit(); }
	};

	UPCGNode* Connect(UPCGGraph* Graph, UPCGNode* From, FName FromPin, UPCGNode* To, FName ToPin)
	{
		UPCGNode* Result = Graph->AddEdge(From, FromPin, To, ToPin);
		if (!Result)
		{
			FString Outs, Ins;
			for (const UPCGPin* P : From->GetOutputPins()) { Outs += P->Properties.Label.ToString() + TEXT(","); }
			for (const UPCGPin* P : To->GetInputPins()) { Ins += P->Properties.Label.ToString() + TEXT(","); }
			UE_LOG(LogCookBody, Error, TEXT("edge %s.%s -> %s.%s failed; outputs=[%s] inputs=[%s]"), *From->GetName(), *FromPin.ToString(), *To->GetName(), *ToPin.ToString(), *Outs, *Ins);
		}
		return Result;
	}

	FString Json(const TSharedRef<FJsonObject>& Obj)
	{
		FString Out;
		TSharedRef<TJsonWriter<>> Writer = TJsonWriterFactory<>::Create(&Out);
		FJsonSerializer::Serialize(Obj, Writer);
		return Out;
	}
}

UCookBodyCommandlet::UCookBodyCommandlet()
{
	IsClient = false;
	IsEditor = true;
	IsServer = false;
	LogToConsole = true;
}

int32 UCookBodyCommandlet::Main(const FString& Params)
{
	const double TStart = FPlatformTime::Seconds();

	TArray<FString> Tokens, Switches;
	TMap<FString, FString> ParamVals;
	ParseCommandLine(*Params, Tokens, Switches, ParamVals);

	uint64 Seed = 1;
	if (const FString* S = ParamVals.Find(TEXT("seed"))) { Seed = FCString::Strtoui64(**S, nullptr, 10); }
	uint32 BodyId = 1;
	if (const FString* S = ParamVals.Find(TEXT("body"))) { BodyId = FCString::Atoi(**S); }
	FString OutDir = FPaths::ProjectSavedDir() / TEXT("OneBody");
	if (const FString* S = ParamVals.Find(TEXT("out"))) { OutDir = *S; }
	double SizeM = 256.0;
	if (const FString* S = ParamVals.Find(TEXT("size"))) { SizeM = FCString::Atod(**S); }
	double SpacingM = 1.0;
	if (const FString* S = ParamVals.Find(TEXT("spacing"))) { SpacingM = FCString::Atod(**S); }
	float Density = 0.03f;
	if (const FString* S = ParamVals.Find(TEXT("density"))) { Density = FCString::Atof(**S); }
	uint32 HfCellMm = 500;
	if (const FString* S = ParamVals.Find(TEXT("hfcell"))) { HfCellMm = FCString::Atoi(**S); }
	uint32 VoxEdgeMm = 500;
	if (const FString* S = ParamVals.Find(TEXT("voxedge"))) { VoxEdgeMm = FCString::Atoi(**S); }
	const bool bNoNanite = Switches.Contains(TEXT("nonanite"));
	// -deterministicguids: derive every GUID the package carries (section BuildKey, actor GUIDs) from the
	// seed instead of FGuid::NewGuid(), to find out whether the Unreal half can be byte-deterministic.
	const bool bDeterministicGuids = Switches.Contains(TEXT("deterministicguids"));
	const bool bSimplifyCollision = Switches.Contains(TEXT("simplifycollision"));

	IFileManager::Get().MakeDirectory(*OutDir, true);
	const FString BodyName = FString::Printf(TEXT("Body_%u"), BodyId);
	const FString MountRoot = TEXT("/OneBodyOut/");
	FPackageName::RegisterMountPoint(MountRoot, OutDir / TEXT(""));
	const FString PackageName = MountRoot + BodyName;

	UE_LOG(LogCookBody, Display, TEXT("CookBody seed=%llu body=%u out=%s size=%.0fm spacing=%.2fm density=%.3f/m2 nanite=%d"), (unsigned long long)Seed, BodyId, *OutDir, SizeM, SpacingM, Density, !bNoNanite);

	FOrreryCaptureRegistry::Reset();

	// ---- World ---------------------------------------------------------------------------------
	UPackage* Package = CreatePackage(*PackageName);
	Package->SetFlags(RF_Public | RF_Standalone);
	UWorld::InitializationValues IVS;
	IVS.RequiresHitProxies(false).ShouldSimulatePhysics(false).EnableTraceCollision(true).CreateNavigation(false).CreateAISystem(false).AllowAudioPlayback(false).CreatePhysicsScene(true);
	UWorld* World = UWorld::CreateWorld(EWorldType::Editor, false, *BodyName, Package, true, ERHIFeatureLevel::Num, &IVS);
	check(World);
	FWorldContext& WorldContext = GEditor->GetEditorWorldContext(true);
	WorldContext.SetCurrentWorld(World);
	GWorld = World;

	// ---- Definition + pipeline (StaticMesh -> Collision -> OrreryExport) ------------------------
	UTransformerPipeline* Pipeline = NewObject<UTransformerPipeline>(Package, TEXT("OneBodyPipeline"), RF_Public | RF_Standalone);
	{
		// The engine's two transformers are instantiated through reflection (their headers drag a private
		// plugin header along, and their vtables are not exported from the plugin on Mac); ours is a
		// plain Make<>. Order matters: ours runs last, after Unreal's collision component exists.
		auto* Transformers = PropPtr<TArray<TInstancedStruct<FTransformer>>>(Pipeline, TEXT("Transformers"));
		UScriptStruct* StaticMeshStruct = FindObject<UScriptStruct>(nullptr, TEXT("/Script/MeshPartitionEditor.StaticMeshTransformer"));
		UScriptStruct* CollisionStruct = FindObject<UScriptStruct>(nullptr, TEXT("/Script/MeshPartitionEditor.CollisionTransformer"));
		checkf(StaticMeshStruct && CollisionStruct, TEXT("MeshPartitionEditor transformer structs not found"));
		TInstancedStruct<FTransformer>& SM = Transformers->AddDefaulted_GetRef();
		SM.InitializeAsScriptStruct(StaticMeshStruct);
		if (bNoNanite)
		{
			FBoolProperty* P = CastField<FBoolProperty>(StaticMeshStruct->FindPropertyByName(TEXT("bUseNanite")));
			P->SetPropertyValue_InContainer(SM.GetMutableMemory(), false);
		}
		TInstancedStruct<FTransformer>& Col = Transformers->AddDefaulted_GetRef();
		Col.InitializeAsScriptStruct(CollisionStruct);
		if (bSimplifyCollision)
		{
			FStructProperty* SettingsProp = CastField<FStructProperty>(CollisionStruct->FindPropertyByName(TEXT("CollisionSimplificationSettings")));
			FBoolProperty* P = CastField<FBoolProperty>(SettingsProp->Struct->FindPropertyByName(TEXT("bSimplifyCollision")));
			P->SetPropertyValue_InContainer(SettingsProp->ContainerPtrToValuePtr<void>(Col.GetMutableMemory()), true);
		}
		Transformers->Add(TInstancedStruct<FTransformer>::Make<FOrreryExportTransformer>());
	}
	UMeshPartitionDefinition* Definition = NewObject<UMeshPartitionDefinition>(Package, TEXT("OneBodyDefinition"), RF_Public | RF_Standalone);
	{
		auto* Variants = PropPtr<TArray<FCompiledSectionBuildVariant>>(Definition, TEXT("CompiledSectionBuildVariants"));
		if (Variants->Num() == 0) { Variants->AddDefaulted(); }
		(*Variants)[0].Name = TEXT("Default");
		(*Variants)[0].TransformerPipeline = Pipeline;
		(*Variants)[0].bSplitSectionsToMatchWorldPartitionRuntimeGrid = false;
		auto* Priorities = PropPtr<TArray<FName>>(Definition, TEXT("ModifierTypePriorities"));
		Priorities->Reset();
		Priorities->Add(TEXT("Noise"));
	}
	const FCompiledSectionBuildVariant& Variant = Definition->GetCompiledSectionBuildVariants()[0];

	// ---- Mesh Terrain actor, base plane, seeded noise -------------------------------------------
	AMeshPartition* MeshPartition = World->SpawnActor<AMeshPartition>(AMeshPartition::StaticClass(), FTransform::Identity);
	UMeshPartitionEditorComponent* EditorComponent = NewObject<UMeshPartitionEditorComponent>(MeshPartition, UMeshPartitionEditorComponent::StaticClass(), TEXT("MegaMeshEditorComponent"));
	EditorComponent->SetForceSynchronousPreviewSectionBuild(true);
	MeshPartition->SetMeshPartitionComponent(EditorComponent);
	MeshPartition->SetMeshPartitionDefinition(Definition);

	const double SizeCm = SizeM * 100.0;
	const int32 VertsPerSide = (int32)FMath::RoundToInt(SizeM / SpacingM) + 1;
	UE::Geometry::FRectangleMeshGenerator RectGen;
	RectGen.Width = SizeCm;
	RectGen.Height = SizeCm;
	RectGen.WidthVertexCount = VertsPerSide;
	RectGen.HeightVertexCount = VertsPerSide;
	AActor* BaseActor = EditorComponent->SpawnBaseModifier(FDynamicMesh3(&RectGen.Generate()), {}, FTransform(FVector(SizeCm / 2.0, SizeCm / 2.0, 0.0)));
	check(BaseActor);
	UModifierComponent* BaseModifier = BaseActor->FindComponentByClass<UMeshProviderModifier>();
	check(BaseModifier);

	FSeedStream Rng(Seed ^ ((uint64)BodyId << 32));
	TArray<AActor*> NoiseActors;
	TArray<UModifierComponent*> Modifiers;
	Modifiers.Add(BaseModifier);
	TSharedRef<FJsonObject> NoiseLog = MakeShared<FJsonObject>();
	for (int32 Layer = 0; Layer < 2; ++Layer)
	{
		AActor* NoiseActor = World->SpawnActor<AActor>();
		NoiseActor->SetActorLabel(FString::Printf(TEXT("Noise_%d"), Layer));
		USceneComponent* Root = NewObject<USceneComponent>(NoiseActor, TEXT("Root"));
		NoiseActor->SetRootComponent(Root);
		Root->RegisterComponent();
		UNoiseModifier* Noise = NewObject<UNoiseModifier>(NoiseActor, UNoiseModifier::StaticClass());
		// Every knob below is seed-derived; the noise modifier has no seed of its own, so translation,
		// frequency and rotation carry the seed into the fBM.
		const double Intensity = Layer == 0 ? Rng.Range(1200.0, 2500.0) : Rng.Range(150.0, 400.0);  // cm
		const double Wavelength = Layer == 0 ? Rng.Range(6000.0, 12000.0) : Rng.Range(800.0, 2000.0); // cm
		const FVector2D Translate(Rng.Range(-1.0e6, 1.0e6), Rng.Range(-1.0e6, 1.0e6));
		const double Rotation = Rng.Range(0.0, 360.0);
		const int32 Octaves = Layer == 0 ? 5 : 3;
		SetProp<FVector3d>(Noise, TEXT("UnscaledCoverage"), FVector3d(SizeCm * 1.2, SizeCm * 1.2, 50000.0));
		SetProp<FVector2D>(Noise, TEXT("NoiseTranslate"), Translate);
		SetProp<FVector2D>(Noise, TEXT("NoiseFrequency"), FVector2D(1.0 / Wavelength, 1.0 / Wavelength));
		SetProp<double>(Noise, TEXT("NoiseRotation"), Rotation);
		SetProp<ENoiseModifierType>(Noise, TEXT("DisplacementType"), ENoiseModifierType::FBmNoise);
		SetProp<EFBMMode>(Noise, TEXT("FBMMode"), Layer == 0 ? EFBMMode::Standard : EFBMMode::Ridge);
		SetProp<double>(Noise, TEXT("Intensity"), Intensity);
		SetProp<double>(Noise, TEXT("Falloff"), 0.0);
		SetProp<int>(Noise, TEXT("FBMOctaves"), Octaves);
		SetProp<double>(Noise, TEXT("FBMLacunarity"), 2.0);
		SetProp<double>(Noise, TEXT("FBMGain"), 0.5);
		SetProp<bool>(Noise, TEXT("bDrawPatchRectangle"), false);
		SetProp<bool>(Noise, TEXT("bDrawAffectedBox"), false);
		Noise->SetType(TEXT("Noise"));
		Noise->SetPriority((double)Layer);
		NoiseActor->AddInstanceComponent(Noise);
		Noise->SetAffectedMeshPartition(MeshPartition);
		Noise->AttachToComponent(Root, FAttachmentTransformRules::KeepWorldTransform);
		Noise->RegisterComponent();
		Noise->SetWorldLocation(FVector(SizeCm / 2.0, SizeCm / 2.0, 0.0));
		NoiseActors.Add(NoiseActor);
		Modifiers.Add(Noise);

		TSharedRef<FJsonObject> L = MakeShared<FJsonObject>();
		L->SetNumberField(TEXT("intensity_cm"), Intensity);
		L->SetNumberField(TEXT("wavelength_cm"), Wavelength);
		L->SetNumberField(TEXT("rotation_deg"), Rotation);
		L->SetNumberField(TEXT("octaves"), Octaves);
		L->SetStringField(TEXT("mode"), Layer == 0 ? TEXT("Standard") : TEXT("Ridge"));
		NoiseLog->SetObjectField(FString::Printf(TEXT("layer_%d"), Layer), L);
	}
	EditorComponent->UpdateModifierList();

	// ---- Build the intermediate: modifiers -> FMeshData -------------------------------------------
	const double TBuild0 = FPlatformTime::Seconds();
	FBuilderSettings Settings;
	Settings.BuildType = EBuildType::CompiledSection;
	Settings.Transform = MeshPartition->GetActorTransform();
	Settings.ModifiersToProcess = Modifiers;
	Settings.TypePriorities = Definition->GetModifierTypePriorities();
	Settings.MaxSectionComplexity = Variant.MaxSectionComplexity;
	Settings.bRecomputeNormals = true;
	Settings.bBuildSpatial = false;
	Settings.bCacheResult = false;
	Settings.bAllowDDCRead = false;
	Settings.bAllowDDCWrite = false;
	TArray<FBuildTaskHandle> Handles = Build::LaunchBuilds(Settings);
	Build::Wait(Handles);
	if (Handles.Num() != 1)
	{
		UE_LOG(LogCookBody, Error, TEXT("expected one modifier group for one base, got %d"), Handles.Num());
		return 1;
	}
	TSharedPtr<const FMeshData> BuiltMesh = Handles[0].GetTask()->GetMesh();
	if (!BuiltMesh.IsValid() || BuiltMesh->TriangleCount() == 0)
	{
		UE_LOG(LogCookBody, Error, TEXT("modifier build produced no mesh"));
		return 1;
	}
	const double TBuild1 = FPlatformTime::Seconds();
	UE_LOG(LogCookBody, Display, TEXT("intermediate FMeshData: %d verts, %d tris in %.2fs"), BuiltMesh->VertexCount(), BuiltMesh->TriangleCount(), TBuild1 - TBuild0);

	// ---- Compiled section + transformer pipeline (the cook path) ---------------------------------
	FSeedStream GuidRng(Seed ^ 0x6f6e65626f6479ull ^ ((uint64)BodyId << 40));
	auto SeededGuid = [&GuidRng]() { const uint64 A = GuidRng.Next(), B = GuidRng.Next(); return FGuid((uint32)(A >> 32), (uint32)A, (uint32)(B >> 32), (uint32)B); };
	FCompiledSectionBuildInfo BuildInfo;
	BuildInfo.BuildKey = bDeterministicGuids ? SeededGuid() : FGuid::NewGuid();
	BuildInfo.BuildVariantName = Variant.Name;
	BuildInfo.MegaMeshDefinitionPath = FTopLevelAssetPath(Definition);
	BuildInfo.SetMegaMeshDefinition(Definition);
	BuildInfo.MegaMeshPath = FSoftObjectPath(MeshPartition);
	FMeshData FullMesh;
	FullMesh.Copy(*BuiltMesh);
	FPrepareCompiledSectionsParams Prepare{ .FullMesh = FullMesh };
	TArray<ACompiledSection*> Sections = EditorComponent->PrepareCompiledSections(BuildInfo, Variant, Prepare);
	if (Sections.Num() != 1)
	{
		UE_LOG(LogCookBody, Error, TEXT("PrepareCompiledSections returned %d sections"), Sections.Num());
		return 1;
	}
	ACompiledSection* Section = Sections[0];
	TArray<TWeakObjectPtr<UModifierComponent>> ModifierPtrs;
	for (UModifierComponent* M : Modifiers) { ModifierPtrs.Add(M); }
	EditorComponent->BuildMegaMeshCompiledSectionTextures(Section, Handles[0].GetTask()->GetGroup(), FullMesh);
	EditorComponent->PostBuildSectionMesh(Section, FullMesh, ModifierPtrs);
	TSharedPtr<const FMeshData> SharedMesh = MakeShared<const FMeshData>(MoveTemp(FullMesh));
	TArray<FTransformerUnit> Units;
	Units.Add(MakeTransformerUnit(Section, SharedMesh));
	const double TTrans0 = FPlatformTime::Seconds();
	TUniquePtr<FTransformerContext> Context = EditorComponent->LaunchTransformers(MoveTemp(Units), Definition, Variant);
	if (!Context.IsValid())
	{
		UE_LOG(LogCookBody, Error, TEXT("LaunchTransformers returned no context (pipeline empty?)"));
		return 1;
	}
	WaitOnGameThread(*Context);
	EditorComponent->PostProcessSection(Section, ModifierPtrs);
	const double TTransA = FPlatformTime::Seconds();
	// The static mesh transformer hands the mesh to the async static-mesh compiler (Nanite build included);
	// the cook is not done until that has drained, so wait for it here and count it.
	FStaticMeshCompilingManager::Get().FinishAllCompilation();
	const double TTrans1 = FPlatformTime::Seconds();
	UE_LOG(LogCookBody, Display, TEXT("transformers (StaticMesh%s, Collision, OrreryExport) in %.2fs + %.2fs async static mesh/Nanite build"), bNoNanite ? TEXT("") : TEXT("+Nanite"), TTransA - TTrans0, TTrans1 - TTransA);
	for (UStaticMesh* SM : Section->GetStaticMeshes())
	{
		if (SM) { UE_LOG(LogCookBody, Display, TEXT("section static mesh %s: nanite=%d, LODs=%d, LOD0 tris=%d"), *SM->GetName(), SM->IsNaniteEnabled() ? 1 : 0, SM->GetNumLODs(), SM->GetRenderData() && SM->GetRenderData()->LODResources.Num() ? SM->GetRenderData()->LODResources[0].GetNumTriangles() : -1); }
	}

	// Register the section's components so its collision body exists for PCG's ray queries.
	Section->ReregisterAllComponents();
	World->UpdateWorldComponents(true, false);

	TArray<FOrreryCapturedSection>& Captured = FOrreryCaptureRegistry::Get();
	if (Captured.Num() != 1 || !Captured[0].MeshData.IsValid())
	{
		UE_LOG(LogCookBody, Error, TEXT("export transformer captured %d sections"), Captured.Num());
		return 1;
	}
	const FMeshData& Intermediate = *Captured[0].MeshData;
	const bool bSameIntermediate = Captured[0].MeshData.Get() == SharedMesh.Get();
	const uint64 IntermediateHash = OrreryExport::HashMeshData(Intermediate);
	TSharedRef<FJsonObject> CollisionLog = MakeShared<FJsonObject>();
	{
		int32 Idx = 0;
		for (const TSharedPtr<const FMeshPartitionCollisionData>& Col : Captured[0].UnrealCollision)
		{
			TSharedRef<FJsonObject> C = MakeShared<FJsonObject>();
			if (Col.IsValid() && Col->Mesh.IsSet())
			{
				const FTriMeshCollisionData& Tri = Col->Mesh.GetValue();
				C->SetNumberField(TEXT("verts"), Tri.Vertices.Num());
				C->SetNumberField(TEXT("tris"), Tri.Indices.Num());
				C->SetStringField(TEXT("hash_mm"), FString::Printf(TEXT("%016llx"), (unsigned long long)OrreryExport::HashTriMeshCollision(Tri)));
				// Unreal's collision vertices are the intermediate's doubles cast to float (MeshPartitionCollisionGeneration.cpp,
				// CutAndConvertMeshToCollisionMesh); measure that cast rather than only hashing it.
				double MaxDevMm = 0.0; int32 MmFlips = 0; int32 Compared = 0;
				int32 K = 0;
				for (int VID : Intermediate.VertexIndicesItr())
				{
					if (K >= Tri.Vertices.Num()) { break; }
					const FVector3d P = Intermediate.GetVertex(VID);
					const FVector3d Q = FVector3d(Tri.Vertices[K]);
					MaxDevMm = FMath::Max(MaxDevMm, (P - Q).GetAbsMax() * 10.0);
					if (OrreryExport::ToMm(P.X) != OrreryExport::ToMm(Q.X) || OrreryExport::ToMm(P.Y) != OrreryExport::ToMm(Q.Y) || OrreryExport::ToMm(P.Z) != OrreryExport::ToMm(Q.Z)) { ++MmFlips; }
					++K; ++Compared;
				}
				bool bSameTopology = Tri.Indices.Num() == Intermediate.TriangleCount();
				int32 TK = 0;
				for (int TID : Intermediate.TriangleIndicesItr())
				{
					if (TK >= Tri.Indices.Num()) { bSameTopology = false; break; }
					const UE::Geometry::FIndex3i T = Intermediate.GetTriangle(TID);
					if (T.A != Tri.Indices[TK].v0 || T.B != Tri.Indices[TK].v1 || T.C != Tri.Indices[TK].v2) { bSameTopology = false; break; }
					++TK;
				}
				C->SetNumberField(TEXT("vertices_compared"), Compared);
				C->SetNumberField(TEXT("max_vertex_deviation_mm"), MaxDevMm);
				C->SetNumberField(TEXT("vertices_that_round_to_a_different_mm"), MmFlips);
				C->SetBoolField(TEXT("same_triangle_list_as_intermediate"), bSameTopology);
			}
			CollisionLog->SetObjectField(FString::Printf(TEXT("component_%d"), Idx++), C);
		}
	}

	// ---- PCG scatter (cook time only), sampling the section's collision -------------------------
	const double TPcg0 = FPlatformTime::Seconds();
	UPCGGraph* Graph = NewObject<UPCGGraph>(Package, TEXT("OneBodyScatter"), RF_Public | RF_Standalone);
	UPCGWorldRayHitSettings* RayHit = nullptr;
	UPCGNode* RayHitNode = Graph->AddNodeOfType<UPCGWorldRayHitSettings>(RayHit);
	RayHit->QueryParams.CollisionChannel = ECC_WorldStatic;
	RayHit->QueryParams.bTraceComplex = true;
	RayHit->QueryParams.bIgnorePCGHits = true;
	RayHit->QueryParams.bIgnoreSelfHits = true;
	UPCGSurfaceSamplerSettings* Sampler = nullptr;
	UPCGNode* SamplerNode = Graph->AddNodeOfType<UPCGSurfaceSamplerSettings>(Sampler);
	Sampler->PointsPerSquaredMeter = Density;
	Sampler->PointExtents = FVector(60.0);
	Sampler->Looseness = 1.0f;
	Sampler->bApplyDensityToPoints = false;
	UPCGTransformPointsSettings* Xform = nullptr;
	UPCGNode* XformNode = Graph->AddNodeOfType<UPCGTransformPointsSettings>(Xform);
	Xform->RotationMin = FRotator(0, 0, 0);
	Xform->RotationMax = FRotator(0, 360, 0);
	Xform->ScaleMin = FVector(0.6);
	Xform->ScaleMax = FVector(3.0);
	Xform->bUniformScale = true;
	UPCGStaticMeshSpawnerSettings* Spawner = nullptr;
	UPCGNode* SpawnerNode = Graph->AddNodeOfType<UPCGStaticMeshSpawnerSettings>(Spawner);
	Spawner->SetMeshSelectorType(UPCGMeshSelectorWeighted::StaticClass());
	Spawner->bSynchronousLoad = true;
	Spawner->bWarnOnIdenticalSpawn = false;
	TArray<TPair<FString, int32>> Meshes = {
		{ TEXT("/Engine/BasicShapes/Cube.Cube"), 4 },
		{ TEXT("/Engine/BasicShapes/Cylinder.Cylinder"), 2 },
		{ TEXT("/Engine/BasicShapes/Cone.Cone"), 2 },
		{ TEXT("/Engine/BasicShapes/Sphere.Sphere"), 1 },
	};
	if (UPCGMeshSelectorWeighted* Weighted = Cast<UPCGMeshSelectorWeighted>(Spawner->MeshSelectorParameters))
	{
		for (const TPair<FString, int32>& M : Meshes)
		{
			FPCGMeshSelectorWeightedEntry& Entry = Weighted->MeshEntries.Emplace_GetRef(TSoftObjectPtr<UStaticMesh>(FSoftObjectPath(M.Key)), M.Value);
			Entry.Descriptor.Mobility = EComponentMobility::Static;
			Entry.Descriptor.bUseDefaultCollision = false;
			Entry.Descriptor.BodyInstance.SetCollisionProfileName(UCollisionProfile::BlockAll_ProfileName);
		}
	}
	UPCGNode* InputNode = Graph->GetInputNode();
	UPCGNode* OutputNode = Graph->GetOutputNode();
	auto PinList = [](const TArray<TObjectPtr<UPCGPin>>& Pins) { FString S; for (const UPCGPin* P : Pins) { S += P->Properties.Label.ToString() + TEXT(","); } return S; };
	UE_LOG(LogCookBody, Display, TEXT("PCG pins: input.out=[%s] output.in=[%s] rayhit.in=[%s] rayhit.out=[%s] sampler.in=[%s] spawner.in=[%s]"),
		*PinList(InputNode->GetOutputPins()), *PinList(OutputNode->GetInputPins()), *PinList(RayHitNode->GetInputPins()), *PinList(RayHitNode->GetOutputPins()), *PinList(SamplerNode->GetInputPins()), *PinList(SpawnerNode->GetInputPins()));
	const FName InputOut = InputNode->GetOutputPins().Num() ? InputNode->GetOutputPins()[0]->Properties.Label : NAME_None;
	const FName OutputIn = OutputNode->GetInputPins().Num() ? OutputNode->GetInputPins()[0]->Properties.Label : NAME_None;
	const FName RayHitIn = RayHitNode->GetInputPins().Num() ? RayHitNode->GetInputPins()[0]->Properties.Label : NAME_None;
	bool bEdges = true;
	if (!RayHitIn.IsNone()) { bEdges &= Connect(Graph, InputNode, InputOut, RayHitNode, RayHitIn) != nullptr; }
	bEdges &= Connect(Graph, RayHitNode, PCGPinConstants::DefaultOutputLabel, SamplerNode, PCGSurfaceSamplerConstants::SurfaceLabel) != nullptr;
	bEdges &= Connect(Graph, InputNode, InputOut, SamplerNode, PCGSurfaceSamplerConstants::BoundingShapeLabel) != nullptr;
	bEdges &= Connect(Graph, SamplerNode, PCGPinConstants::DefaultOutputLabel, XformNode, PCGPinConstants::DefaultInputLabel) != nullptr;
	bEdges &= Connect(Graph, XformNode, PCGPinConstants::DefaultOutputLabel, SpawnerNode, PCGPinConstants::DefaultInputLabel) != nullptr;
	bEdges &= Connect(Graph, SpawnerNode, PCGPinConstants::DefaultOutputLabel, OutputNode, OutputIn) != nullptr;
	if (!bEdges)
	{
		UE_LOG(LogCookBody, Error, TEXT("PCG graph wiring failed"));
		return 1;
	}

	AActor* ScatterActor = World->SpawnActor<AActor>();
	ScatterActor->SetActorLabel(TEXT("Scatter"));
	UBoxComponent* Bounds = NewObject<UBoxComponent>(ScatterActor, TEXT("Bounds"));
	ScatterActor->SetRootComponent(Bounds);
	Bounds->SetCollisionEnabled(ECollisionEnabled::NoCollision);
	Bounds->SetBoxExtent(FVector(SizeCm / 2.0, SizeCm / 2.0, 40000.0));
	Bounds->SetMobility(EComponentMobility::Static);
	Bounds->RegisterComponent();
	ScatterActor->SetActorLocation(FVector(SizeCm / 2.0, SizeCm / 2.0, 0.0));
	UPCGComponent* Pcg = NewObject<UPCGComponent>(ScatterActor, TEXT("PCG"));
	Pcg->Seed = (int32)(Seed & 0x7fffffff) ^ (int32)BodyId;
	Pcg->GenerationTrigger = EPCGComponentGenerationTrigger::GenerateOnDemand;
	Pcg->bActivated = true;
	Pcg->bIsComponentPartitioned = false;
	ScatterActor->AddInstanceComponent(Pcg);
	Pcg->RegisterComponent();
	Pcg->SetGraphLocal(Graph);
	if (!UPCGSubsystem::GetInstance(World))
	{
		UE_LOG(LogCookBody, Error, TEXT("no UPCGSubsystem on the cook world"));
		return 1;
	}
	const FPCGTaskId Task = Pcg->GenerateLocalGetTaskId(true);
	if (Task == InvalidPCGTaskId)
	{
		UE_LOG(LogCookBody, Error, TEXT("PCG generate was not scheduled"));
		return 1;
	}
	int32 Ticks = 0;
	while (Pcg->IsGenerating())
	{
		CommandletHelpers::TickEngine(World, 1.0 / 30.0);
		if (++Ticks > 200000) { UE_LOG(LogCookBody, Error, TEXT("PCG generation did not finish")); return 1; }
	}
	CommandletHelpers::TickEngine(World, 1.0 / 30.0);
	const double TPcg1 = FPlatformTime::Seconds();

	// ---- Gather the scatter the way Unreal will render and collide it ----------------------------
	FOrreryBodyExport Body;
	Body.Seed = Seed;
	Body.BodyId = BodyId;
	OrreryExport::SoupFromMeshData(Intermediate, MeshPartition->GetActorTransform(), Body.Terrain);
	TSharedRef<FJsonObject> ScatterLog = MakeShared<FJsonObject>();
	int32 TotalInstances = 0;
	uint64 InstanceTris = 0;
	{
		TArray<UInstancedStaticMeshComponent*> Isms;
		ScatterActor->GetComponents<UInstancedStaticMeshComponent>(Isms);
		int32 CompIdx = 0;
		for (UInstancedStaticMeshComponent* Ism : Isms)
		{
			UStaticMesh* Mesh = Ism->GetStaticMesh();
			if (!Mesh) { continue; }
			const FStaticMeshRenderData* RD = Mesh->GetRenderData();
			if (!RD || RD->LODResources.Num() == 0) { continue; }
			const FStaticMeshLODResources& LOD = RD->LODResources[0];
			TArray<uint32> Indices;
			LOD.IndexBuffer.GetCopy(Indices);
			const uint32 NumVerts = LOD.VertexBuffers.PositionVertexBuffer.GetNumVertices();
			const int32 Count = Ism->GetInstanceCount();
			TSharedRef<FJsonObject> C = MakeShared<FJsonObject>();
			C->SetStringField(TEXT("mesh"), Mesh->GetPathName());
			C->SetStringField(TEXT("component_class"), Ism->GetClass()->GetName());
			C->SetNumberField(TEXT("instances"), Count);
			C->SetNumberField(TEXT("lod0_tris"), Indices.Num() / 3);
			C->SetStringField(TEXT("collision_profile"), Ism->GetCollisionProfileName().ToString());
			C->SetStringField(TEXT("collision_enabled"), StaticEnum<ECollisionEnabled::Type>()->GetNameStringByValue((int64)Ism->GetCollisionEnabled()));
			if (UBodySetup* BS = Mesh->GetBodySetup())
			{
				C->SetStringField(TEXT("mesh_collision_trace_flag"), StaticEnum<ECollisionTraceFlag>()->GetNameStringByValue((int64)BS->CollisionTraceFlag));
				C->SetNumberField(TEXT("simple_boxes"), BS->AggGeom.BoxElems.Num());
				C->SetNumberField(TEXT("simple_spheres"), BS->AggGeom.SphereElems.Num());
				C->SetNumberField(TEXT("simple_convex"), BS->AggGeom.ConvexElems.Num());
			}
			ScatterLog->SetObjectField(FString::Printf(TEXT("ism_%d"), CompIdx++), C);
			for (int32 I = 0; I < Count; ++I)
			{
				FTransform T;
				if (!Ism->GetInstanceTransform(I, T, true)) { continue; }
				FOrreryInstance& Inst = Body.Instances.AddDefaulted_GetRef();
				Inst.MeshPath = Mesh->GetPathName();
				Inst.InstanceIndex = I;
				Inst.Soup.Verts.Reserve(NumVerts);
				for (uint32 V = 0; V < NumVerts; ++V)
				{
					const FVector3d P = T.TransformPosition(FVector3d(LOD.VertexBuffers.PositionVertexBuffer.VertexPosition(V)));
					Inst.Soup.Verts.Add(FInt64Vector(OrreryExport::ToMm(P.X), OrreryExport::ToMm(P.Y), OrreryExport::ToMm(P.Z)));
				}
				Inst.Soup.Tris.Reserve(Indices.Num() / 3);
				for (int32 K = 0; K + 2 < Indices.Num(); K += 3)
				{
					Inst.Soup.Tris.Add(FIntVector(Indices[K], Indices[K + 1], Indices[K + 2]));
				}
				InstanceTris += Inst.Soup.Tris.Num();
				++TotalInstances;
			}
		}
	}
	UE_LOG(LogCookBody, Display, TEXT("PCG placed %d instances (%llu triangles) in %.2fs"), TotalInstances, (unsigned long long)InstanceTris, TPcg1 - TPcg0);

	// Bounds over everything the ruleset must collide with.
	{
		FInt64Vector Mn(MAX_int64), Mx(MIN_int64);
		auto Acc = [&](const FInt64Vector& V) { Mn.X = FMath::Min(Mn.X, V.X); Mn.Y = FMath::Min(Mn.Y, V.Y); Mn.Z = FMath::Min(Mn.Z, V.Z); Mx.X = FMath::Max(Mx.X, V.X); Mx.Y = FMath::Max(Mx.Y, V.Y); Mx.Z = FMath::Max(Mx.Z, V.Z); };
		for (const FInt64Vector& V : Body.Terrain.Verts) { Acc(V); }
		for (const FOrreryInstance& Inst : Body.Instances) for (const FInt64Vector& V : Inst.Soup.Verts) { Acc(V); }
		Body.BoundsMin = Mn; Body.BoundsMax = Mx;
	}

	// ---- Ruleset collision packages, each from the captured intermediate --------------------------
	TSharedRef<FJsonObject> Reps = MakeShared<FJsonObject>();
	auto Timed = [&](const TCHAR* Name, const FString& Path, TFunctionRef<bool()> Fn)
	{
		const double T0 = FPlatformTime::Seconds();
		const bool bOk = Fn();
		const double T1 = FPlatformTime::Seconds();
		TSharedRef<FJsonObject> R = MakeShared<FJsonObject>();
		R->SetStringField(TEXT("path"), Path);
		R->SetNumberField(TEXT("seconds"), T1 - T0);
		R->SetNumberField(TEXT("bytes"), (double)IFileManager::Get().FileSize(*Path));
		R->SetBoolField(TEXT("ok"), bOk);
		Reps->SetObjectField(Name, R);
	};
	const FString Stem = OutDir / FString::Printf(TEXT("body-%u"), BodyId);
	Timed(TEXT("tri"), Stem + TEXT(".tri.collision"), [&] { return OrreryExport::WriteTri(Body, Stem + TEXT(".tri.collision")); });
	Timed(TEXT("heightfield"), Stem + TEXT(".hf.collision"), [&] { return OrreryExport::WriteHeightfield(Body, Intermediate, HfCellMm, Stem + TEXT(".hf.collision")); });
	Timed(TEXT("voxel"), Stem + TEXT(".vox.collision"), [&] { return OrreryExport::WriteVoxels(Body, Intermediate, VoxEdgeMm, Stem + TEXT(".vox.collision")); });

	// ---- Save the Unreal package: the cooked section (Nanite static mesh + collision component) and the scatter ----
	// The authoring modifiers are removed first: the compiled section is the product, and the package
	// size spike 4 multiplies should be the product's, not the recipe's.
	const double TSave0 = FPlatformTime::Seconds();
	for (AActor* A : NoiseActors) { World->DestroyActor(A); }
	World->DestroyActor(BaseActor);
	World->SetFlags(RF_Public | RF_Standalone);
	if (bDeterministicGuids)
	{
		// Actors get FGuid::NewGuid() at spawn (AActor::ActorGuid); rewrite them from the seed in a stable order.
		TArray<AActor*> Actors;
		for (TActorIterator<AActor> It(World); It; ++It) { Actors.Add(*It); }
		Actors.Sort([](const AActor& A, const AActor& B) { return A.GetPathName() < B.GetPathName(); });
		int32 Rewritten = 0;
		for (AActor* A : Actors)
		{
			if (FProperty* P = AActor::StaticClass()->FindPropertyByName(TEXT("ActorGuid")))
			{
				*P->ContainerPtrToValuePtr<FGuid>(A) = SeededGuid();
				++Rewritten;
			}
			if (FProperty* P = AActor::StaticClass()->FindPropertyByName(TEXT("ActorInstanceGuid")))
			{
				*P->ContainerPtrToValuePtr<FGuid>(A) = SeededGuid();
			}
		}
		UE_LOG(LogCookBody, Display, TEXT("deterministic guids: rewrote %d actor guids from the seed"), Rewritten);
	}
	const FString MapFilename = FPaths::ConvertRelativePathToFull(FPackageName::LongPackageNameToFilename(PackageName, FPackageName::GetMapPackageExtension()));
	FSavePackageArgs SaveArgs;
	SaveArgs.TopLevelFlags = RF_Public | RF_Standalone;
	SaveArgs.SaveFlags = SAVE_NoError;
	const bool bSaved = UPackage::SavePackage(Package, World, *MapFilename, SaveArgs);
	const double TSave1 = FPlatformTime::Seconds();
	UE_LOG(LogCookBody, Display, TEXT("saved %s: %s (%lld bytes) in %.2fs"), *MapFilename, bSaved ? TEXT("ok") : TEXT("FAILED"), (long long)IFileManager::Get().FileSize(*MapFilename), TSave1 - TSave0);

	const double TEnd = FPlatformTime::Seconds();

	// ---- cook.json: everything the comparator and the report need ---------------------------------
	TSharedRef<FJsonObject> Root = MakeShared<FJsonObject>();
	Root->SetStringField(TEXT("seed"), FString::Printf(TEXT("%llu"), (unsigned long long)Seed));
	Root->SetNumberField(TEXT("body"), BodyId);
	Root->SetStringField(TEXT("engine"), FEngineVersion::Current().ToString());
	Root->SetStringField(TEXT("platform"), FPlatformProperties::IniPlatformName());
	Root->SetNumberField(TEXT("size_m"), SizeM);
	Root->SetNumberField(TEXT("spacing_m"), SpacingM);
	Root->SetNumberField(TEXT("density_per_m2"), Density);
	Root->SetBoolField(TEXT("nanite"), !bNoNanite);
	Root->SetBoolField(TEXT("unreal_collision_simplified"), bSimplifyCollision);
	Root->SetObjectField(TEXT("noise"), NoiseLog);
	Root->SetNumberField(TEXT("intermediate_verts"), Intermediate.VertexCount());
	Root->SetNumberField(TEXT("intermediate_tris"), Intermediate.TriangleCount());
	Root->SetStringField(TEXT("intermediate_hash_mm"), FString::Printf(TEXT("%016llx"), (unsigned long long)IntermediateHash));
	Root->SetBoolField(TEXT("export_saw_same_meshdata_pointer_as_static_mesh_and_collision"), bSameIntermediate);
	Root->SetObjectField(TEXT("unreal_collision"), CollisionLog);
	Root->SetObjectField(TEXT("scatter"), ScatterLog);
	Root->SetNumberField(TEXT("instances"), TotalInstances);
	Root->SetNumberField(TEXT("instance_tris"), (double)InstanceTris);
	TSharedRef<FJsonObject> BoundsJson = MakeShared<FJsonObject>();
	BoundsJson->SetNumberField(TEXT("min_x_mm"), (double)Body.BoundsMin.X); BoundsJson->SetNumberField(TEXT("min_y_mm"), (double)Body.BoundsMin.Y); BoundsJson->SetNumberField(TEXT("min_z_mm"), (double)Body.BoundsMin.Z);
	BoundsJson->SetNumberField(TEXT("max_x_mm"), (double)Body.BoundsMax.X); BoundsJson->SetNumberField(TEXT("max_y_mm"), (double)Body.BoundsMax.Y); BoundsJson->SetNumberField(TEXT("max_z_mm"), (double)Body.BoundsMax.Z);
	Root->SetObjectField(TEXT("bounds"), BoundsJson);
	TSharedRef<FJsonObject> Timing = MakeShared<FJsonObject>();
	Timing->SetNumberField(TEXT("modifiers_to_meshdata_s"), TBuild1 - TBuild0);
	Timing->SetNumberField(TEXT("transformers_s"), TTrans1 - TTrans0);
	Timing->SetNumberField(TEXT("pcg_s"), TPcg1 - TPcg0);
	Timing->SetNumberField(TEXT("save_s"), TSave1 - TSave0);
	Timing->SetNumberField(TEXT("commandlet_main_s"), TEnd - TStart);
	Root->SetObjectField(TEXT("timing"), Timing);
	Root->SetObjectField(TEXT("representations"), Reps);
	TSharedRef<FJsonObject> Unreal = MakeShared<FJsonObject>();
	Unreal->SetStringField(TEXT("map"), MapFilename);
	Unreal->SetNumberField(TEXT("bytes"), (double)IFileManager::Get().FileSize(*MapFilename));
	Unreal->SetStringField(TEXT("package"), PackageName);
	Root->SetObjectField(TEXT("unreal"), Unreal);
	FFileHelper::SaveStringToFile(Json(Root), *(Stem + TEXT(".cook.json")));

	WorldContext.SetCurrentWorld(nullptr);
	GWorld = nullptr;
	return bSaved ? 0 : 1;
}
