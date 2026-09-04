// Spike #1044 — the pipeline hook and the collision-package writers.
//
// FOrreryExportTransformer is a MeshPartition::FTransformer appended to the
// same UTransformerPipeline that carries FStaticMeshTransformer (the Nanite
// render mesh) and FCollisionTransformer (Unreal's own Chaos trimesh). All
// three receive the identical FTransformerContext, whose TransformerUnits[i]
// .MeshData is the built MeshPartition::FMeshData — that shared pointer is the
// "one intermediate" G10 talks about. This transformer only *captures* it (and
// the FTriMeshCollisionData Unreal derived from it); the commandlet writes the
// ruleset packages after PCG has placed the scatter.
#pragma once

#include "CoreMinimal.h"
#include "MeshPartitionTransformer.h"
#include "MeshPartitionCollisionComponent.h"
#include "OrreryExport.generated.h"

/** One captured section: the built intermediate and what Unreal's collision transformer made of it. */
struct FOrreryCapturedSection
{
	TWeakObjectPtr<AActor> Section;
	TSharedPtr<const UE::MeshPartition::FMeshData> MeshData;
	TArray<TSharedPtr<const UE::MeshPartition::FMeshPartitionCollisionData>> UnrealCollision;
};

/** Process-wide capture registry (the transformer runs on a worker thread; the commandlet drains it on the game thread). */
struct FOrreryCaptureRegistry
{
	static TArray<FOrreryCapturedSection>& Get();
	static void Reset();
};

USTRUCT()
struct FOrreryExportTransformer : public UE::MeshPartition::FTransformer
{
	GENERATED_BODY()

	virtual bool Execute(UE::MeshPartition::FTransformerContext& InTransformerContext) const override;
};

/** A world-space triangle soup in integer millimetres — the lattice-snapped form of the intermediate. */
struct FOrreryTriSoup
{
	TArray<FInt64Vector> Verts; // mm
	TArray<FIntVector> Tris;    // indices into Verts
};

/** One scattered instance, flattened to world space. */
struct FOrreryInstance
{
	FString MeshPath;
	int32 InstanceIndex = 0;
	FOrreryTriSoup Soup; // mm, world space
};

/** Everything the writers need, gathered by the commandlet. */
struct FOrreryBodyExport
{
	uint64 Seed = 0;
	uint32 BodyId = 0;
	FInt64Vector BoundsMin = FInt64Vector(0); // mm, terrain + instances
	FInt64Vector BoundsMax = FInt64Vector(0);
	FOrreryTriSoup Terrain;
	TArray<FOrreryInstance> Instances;
};

namespace OrreryExport
{
	/** Snap a cm double to integer mm. */
	inline int64 ToMm(double Cm) { return llround(Cm * 10.0); }

	/** Build the mm triangle soup from the captured FMeshData (cm doubles, MeshPartition-local == world here). */
	void SoupFromMeshData(const UE::MeshPartition::FMeshData& Mesh, const FTransform& LocalToWorld, FOrreryTriSoup& Out);

	/** Representation A: lattice triangles (terrain + flattened instances), BVH built by the reader. */
	bool WriteTri(const FOrreryBodyExport& Body, const FString& Path);

	/** Representation B: heightfield at CellMm sampled from the intermediate, plus a 26-DOP prism per instance. */
	bool WriteHeightfield(const FOrreryBodyExport& Body, const UE::MeshPartition::FMeshData& Mesh, uint32 CellMm, const FString& Path);

	/** Representation C: column-RLE voxel occupancy at EdgeMm (terrain columns solid below the surface; instance shells). */
	bool WriteVoxels(const FOrreryBodyExport& Body, const UE::MeshPartition::FMeshData& Mesh, uint32 EdgeMm, const FString& Path);

	/** blake3-free content hash (FNV-1a 64) over a captured mesh, to compare FMeshData against Unreal's FTriMeshCollisionData. */
	uint64 HashMeshData(const UE::MeshPartition::FMeshData& Mesh);
	uint64 HashTriMeshCollision(const FTriMeshCollisionData& Data);
}
