// Spike #1044 — pipeline hook and collision-package writers. See OrreryExport.h.
#include "OrreryExport.h"

#include "MeshPartitionCompiledSection.h"
#include "MeshPartitionMeshData.h"
#include "Spatial/MeshAABBTree3.h"
#include "Tasks/Task.h"
#include "Misc/FileHelper.h"
#include "Serialization/BufferArchive.h"
#include "HAL/FileManager.h"

DEFINE_LOG_CATEGORY_STATIC(LogOrreryExport, Log, All);

using namespace UE::MeshPartition;

// ---------------------------------------------------------------------------
// Capture registry + transformer
// ---------------------------------------------------------------------------

TArray<FOrreryCapturedSection>& FOrreryCaptureRegistry::Get()
{
	static TArray<FOrreryCapturedSection> Registry;
	return Registry;
}

void FOrreryCaptureRegistry::Reset()
{
	Get().Reset();
}

bool FOrreryExportTransformer::Execute(FTransformerContext& InTransformerContext) const
{
	if (InTransformerContext.bWasCancelled)
	{
		return false;
	}

	// The transformer pipeline runs each transformer as a task with the previous one as prerequisite
	// (MeshPartitionEditorComponent.cpp, LaunchTransformers), so by now FCollisionTransformer has
	// finalized its components on the game thread. Read them there too.
	UE::Tasks::FTask Capture = UE::Tasks::Launch(TEXT("OrreryExport::Capture"), [&InTransformerContext]()
	{
		for (const FTransformerUnit& Unit : InTransformerContext.TransformerUnits)
		{
			FOrreryCapturedSection Captured;
			Captured.Section = Unit.Section;
			Captured.MeshData = Unit.MeshData;

			if (ACompiledSection* Section = Cast<ACompiledSection>(GetSectionChecked(Unit)))
			{
				for (UMeshPartitionCollisionComponent* Collision : Section->GetCollisionComponents())
				{
					if (Collision)
					{
						Captured.UnrealCollision.Add(Collision->GetMeshCollisionData());
					}
				}
			}
			FOrreryCaptureRegistry::Get().Add(MoveTemp(Captured));
		}
	}, UE::Tasks::ETaskPriority::Normal, UE::Tasks::EExtendedTaskPriority::GameThreadNormalPri);
	Capture.Wait();
	return true;
}

// ---------------------------------------------------------------------------
// Writers
// ---------------------------------------------------------------------------

namespace
{
	constexpr uint8 MagicTri[8] = { 'O','R','R','Y','T','R','I','1' };
	constexpr uint8 MagicHf[8]  = { 'O','R','R','Y','H','F','_','1' };
	constexpr uint8 MagicVox[8] = { 'O','R','R','Y','V','O','X','1' };

	void WriteHeader(FBufferArchive& Ar, const uint8* Magic, const FOrreryBodyExport& Body)
	{
		Ar.Serialize(const_cast<uint8*>(Magic), 8);
		uint64 Seed = Body.Seed; Ar << Seed;
		uint32 Id = Body.BodyId; Ar << Id;
		int64 V;
		V = Body.BoundsMin.X; Ar << V; V = Body.BoundsMin.Y; Ar << V; V = Body.BoundsMin.Z; Ar << V;
		V = Body.BoundsMax.X; Ar << V; V = Body.BoundsMax.Y; Ar << V; V = Body.BoundsMax.Z; Ar << V;
	}

	void WriteSoup(FBufferArchive& Ar, const FOrreryTriSoup& Soup)
	{
		uint32 N = Soup.Verts.Num(); Ar << N;
		for (const FInt64Vector& P : Soup.Verts) { int64 X = P.X, Y = P.Y, Z = P.Z; Ar << X << Y << Z; }
		uint32 T = Soup.Tris.Num(); Ar << T;
		for (const FIntVector& Tri : Soup.Tris) { uint32 A = Tri.X, B = Tri.Y, C = Tri.Z; Ar << A << B << C; }
	}

	bool Flush(FBufferArchive& Ar, const FString& Path)
	{
		const bool bOk = FFileHelper::SaveArrayToFile(Ar, *Path);
		UE_LOG(LogOrreryExport, Display, TEXT("wrote %s (%lld bytes) %s"), *Path, (long long)Ar.Num(), bOk ? TEXT("ok") : TEXT("FAILED"));
		return bOk;
	}

	// The 13 axis pairs of a 26-DOP, integer directions so the reader can evaluate them exactly.
	constexpr int32 KDop[13][3] = {
		{1,0,0},{0,1,0},{0,0,1},
		{1,1,0},{1,-1,0},{1,0,1},{1,0,-1},{0,1,1},{0,1,-1},
		{1,1,1},{1,1,-1},{1,-1,1},{-1,1,1}
	};

	// Akenine-Möller triangle/AABB overlap, in doubles (cook side only).
	bool TriBoxOverlap(const FVector3d& Center, const FVector3d& Half, const FVector3d& A, const FVector3d& B, const FVector3d& C)
	{
		const FVector3d V0 = A - Center, V1 = B - Center, V2 = C - Center;
		const FVector3d E0 = V1 - V0, E1 = V2 - V1, E2 = V0 - V2;
		auto AxisTest = [&](const FVector3d& Axis) -> bool
		{
			const double P0 = V0.Dot(Axis), P1 = V1.Dot(Axis), P2 = V2.Dot(Axis);
			const double R = Half.X * FMath::Abs(Axis.X) + Half.Y * FMath::Abs(Axis.Y) + Half.Z * FMath::Abs(Axis.Z);
			const double Mn = FMath::Min3(P0, P1, P2), Mx = FMath::Max3(P0, P1, P2);
			return !(Mn > R || Mx < -R);
		};
		const FVector3d Edges[3] = { E0, E1, E2 };
		const FVector3d Axes[3] = { FVector3d(1,0,0), FVector3d(0,1,0), FVector3d(0,0,1) };
		for (int32 I = 0; I < 3; ++I)
			for (int32 J = 0; J < 3; ++J)
				if (!AxisTest(Axes[J].Cross(Edges[I]))) return false;
		for (int32 J = 0; J < 3; ++J) if (!AxisTest(Axes[J])) return false;
		return AxisTest(E0.Cross(E1));
	}
}

void OrreryExport::SoupFromMeshData(const FMeshData& Mesh, const FTransform& LocalToWorld, FOrreryTriSoup& Out)
{
	TArray<int32> Remap;
	Remap.Init(INDEX_NONE, Mesh.MaxVertexID());
	Out.Verts.Reserve(Mesh.VertexCount());
	for (int VID : Mesh.VertexIndicesItr())
	{
		const FVector3d P = LocalToWorld.TransformPosition(Mesh.GetVertex(VID));
		Remap[VID] = Out.Verts.Add(FInt64Vector(ToMm(P.X), ToMm(P.Y), ToMm(P.Z)));
	}
	Out.Tris.Reserve(Mesh.TriangleCount());
	for (int TID : Mesh.TriangleIndicesItr())
	{
		const UE::Geometry::FIndex3i T = Mesh.GetTriangle(TID);
		Out.Tris.Add(FIntVector(Remap[T.A], Remap[T.B], Remap[T.C]));
	}
}

bool OrreryExport::WriteTri(const FOrreryBodyExport& Body, const FString& Path)
{
	FBufferArchive Ar;
	WriteHeader(Ar, MagicTri, Body);
	WriteSoup(Ar, Body.Terrain);
	uint32 N = Body.Instances.Num(); Ar << N;
	for (const FOrreryInstance& Inst : Body.Instances)
	{
		WriteSoup(Ar, Inst.Soup);
	}
	return Flush(Ar, Path);
}

bool OrreryExport::WriteHeightfield(const FOrreryBodyExport& Body, const FMeshData& Mesh, uint32 CellMm, const FString& Path)
{
	// Heights come from the intermediate itself: a vertical ray per grid node against the FMeshData AABB tree.
	FMeshABBTree3 Tree(&Mesh, true);
	const int64 X0 = Body.BoundsMin.X, Y0 = Body.BoundsMin.Y;
	const uint32 Nx = (uint32)((Body.BoundsMax.X - X0) / CellMm) + 1;
	const uint32 Ny = (uint32)((Body.BoundsMax.Y - Y0) / CellMm) + 1;
	const double TopCm = (Body.BoundsMax.Z + 1000) / 10.0;

	TArray<int32> Heights;
	Heights.Init(MIN_int32, Nx * Ny);
	for (uint32 J = 0; J < Ny; ++J)
	{
		for (uint32 I = 0; I < Nx; ++I)
		{
			const double Xcm = (X0 + (int64)I * CellMm) / 10.0;
			const double Ycm = (Y0 + (int64)J * CellMm) / 10.0;
			FRay3d Ray(FVector3d(Xcm, Ycm, TopCm), FVector3d(0, 0, -1), true);
			double NearestT = TNumericLimits<double>::Max();
			int32 TID = INDEX_NONE;
			FVector3d Bary;
			if (Tree.FindNearestHitTriangle(Ray, NearestT, TID, Bary) && TID != INDEX_NONE)
			{
				Heights[J * Nx + I] = (int32)ToMm(TopCm - NearestT);
			}
		}
	}

	FBufferArchive Ar;
	WriteHeader(Ar, MagicHf, Body);
	uint32 NxW = Nx, NyW = Ny, Cell = CellMm; int64 X0W = X0, Y0W = Y0;
	Ar << NxW << NyW << X0W << Y0W << Cell;
	for (int32 H : Heights) { Ar << H; }
	uint32 N = Body.Instances.Num(); Ar << N;
	for (const FOrreryInstance& Inst : Body.Instances)
	{
		for (int32 K = 0; K < 13; ++K)
		{
			int64 Mn = MAX_int64, Mx = MIN_int64;
			for (const FInt64Vector& V : Inst.Soup.Verts)
			{
				const int64 D = (int64)KDop[K][0] * V.X + (int64)KDop[K][1] * V.Y + (int64)KDop[K][2] * V.Z;
				Mn = FMath::Min(Mn, D); Mx = FMath::Max(Mx, D);
			}
			Ar << Mn << Mx;
		}
	}
	return Flush(Ar, Path);
}

bool OrreryExport::WriteVoxels(const FOrreryBodyExport& Body, const FMeshData& Mesh, uint32 EdgeMm, const FString& Path)
{
	FMeshABBTree3 Tree(&Mesh, true);
	const int64 X0 = Body.BoundsMin.X, Y0 = Body.BoundsMin.Y, Z0 = Body.BoundsMin.Z - (int64)EdgeMm;
	const uint32 Nx = (uint32)((Body.BoundsMax.X - X0) / EdgeMm) + 1;
	const uint32 Ny = (uint32)((Body.BoundsMax.Y - Y0) / EdgeMm) + 1;
	const uint32 Nz = (uint32)((Body.BoundsMax.Z - Z0) / EdgeMm) + 2;
	const double TopCm = (Body.BoundsMax.Z + 1000) / 10.0;

	// Per column: a set of occupied z indices. Terrain fills [0, kTop]; instance shells add sparse voxels.
	TArray<TArray<int32>> Columns;
	Columns.SetNum(Nx * Ny);
	TArray<int32> TerrainTop;
	TerrainTop.Init(-1, Nx * Ny);

	for (uint32 J = 0; J < Ny; ++J)
	{
		for (uint32 I = 0; I < Nx; ++I)
		{
			const double Xcm = (X0 + ((int64)I * EdgeMm) + EdgeMm / 2) / 10.0;
			const double Ycm = (Y0 + ((int64)J * EdgeMm) + EdgeMm / 2) / 10.0;
			FRay3d Ray(FVector3d(Xcm, Ycm, TopCm), FVector3d(0, 0, -1), true);
			double NearestT = TNumericLimits<double>::Max();
			int32 TID = INDEX_NONE;
			FVector3d Bary;
			if (Tree.FindNearestHitTriangle(Ray, NearestT, TID, Bary) && TID != INDEX_NONE)
			{
				const int64 Hmm = ToMm(TopCm - NearestT);
				// voxel k occupied iff its centre is at or below the surface
				const int64 K = (Hmm - Z0 - (int64)EdgeMm / 2) / (int64)EdgeMm;
				TerrainTop[J * Nx + I] = (int32)FMath::Clamp<int64>(K, -1, (int64)Nz - 1);
			}
		}
	}

	uint64 ShellVoxels = 0;
	for (const FOrreryInstance& Inst : Body.Instances)
	{
		for (const FIntVector& T : Inst.Soup.Tris)
		{
			const FInt64Vector& A = Inst.Soup.Verts[T.X];
			const FInt64Vector& B = Inst.Soup.Verts[T.Y];
			const FInt64Vector& C = Inst.Soup.Verts[T.Z];
			const int64 MinX = FMath::Min3(A.X, B.X, C.X), MaxX = FMath::Max3(A.X, B.X, C.X);
			const int64 MinY = FMath::Min3(A.Y, B.Y, C.Y), MaxY = FMath::Max3(A.Y, B.Y, C.Y);
			const int64 MinZ = FMath::Min3(A.Z, B.Z, C.Z), MaxZ = FMath::Max3(A.Z, B.Z, C.Z);
			const int64 I0 = FMath::Max<int64>(0, (MinX - X0) / EdgeMm), I1 = FMath::Min<int64>(Nx - 1, (MaxX - X0) / EdgeMm);
			const int64 J0 = FMath::Max<int64>(0, (MinY - Y0) / EdgeMm), J1 = FMath::Min<int64>(Ny - 1, (MaxY - Y0) / EdgeMm);
			const int64 K0 = FMath::Max<int64>(0, (MinZ - Z0) / EdgeMm), K1 = FMath::Min<int64>(Nz - 1, (MaxZ - Z0) / EdgeMm);
			const FVector3d Av((double)A.X, (double)A.Y, (double)A.Z), Bv((double)B.X, (double)B.Y, (double)B.Z), Cv((double)C.X, (double)C.Y, (double)C.Z);
			const FVector3d Half(EdgeMm / 2.0, EdgeMm / 2.0, EdgeMm / 2.0);
			for (int64 K = K0; K <= K1; ++K)
				for (int64 J = J0; J <= J1; ++J)
					for (int64 I = I0; I <= I1; ++I)
					{
						const FVector3d Center((double)(X0 + I * EdgeMm) + Half.X, (double)(Y0 + J * EdgeMm) + Half.Y, (double)(Z0 + K * EdgeMm) + Half.Z);
						if (TriBoxOverlap(Center, Half, Av, Bv, Cv))
						{
							Columns[J * Nx + I].AddUnique((int32)K);
							++ShellVoxels;
						}
					}
		}
	}

	FBufferArchive Ar;
	WriteHeader(Ar, MagicVox, Body);
	uint32 NxW = Nx, NyW = Ny, NzW = Nz, Edge = EdgeMm; int64 X0W = X0, Y0W = Y0, Z0W = Z0;
	Ar << NxW << NyW << NzW << X0W << Y0W << Z0W << Edge;
	uint64 Intervals = 0;
	for (uint32 Col = 0; Col < Nx * Ny; ++Col)
	{
		// Build inclusive intervals: terrain [0, top] then merged shell voxels.
		TArray<int32>& Shell = Columns[Col];
		Shell.Sort();
		TArray<TPair<int32, int32>> Runs;
		if (TerrainTop[Col] >= 0) { Runs.Emplace(0, TerrainTop[Col]); }
		for (int32 K : Shell)
		{
			if (Runs.Num() && K <= Runs.Last().Value + 1) { Runs.Last().Value = FMath::Max(Runs.Last().Value, K); }
			else { Runs.Emplace(K, K); }
		}
		uint16 NRuns = (uint16)Runs.Num(); Ar << NRuns;
		for (const TPair<int32, int32>& R : Runs) { int32 Lo = R.Key, Hi = R.Value; Ar << Lo << Hi; }
		Intervals += Runs.Num();
	}
	UE_LOG(LogOrreryExport, Display, TEXT("voxels: grid %u x %u x %u at %u mm, %llu shell voxels, %llu column intervals"), Nx, Ny, Nz, EdgeMm, (unsigned long long)ShellVoxels, (unsigned long long)Intervals);
	return Flush(Ar, Path);
}

uint64 OrreryExport::HashMeshData(const FMeshData& Mesh)
{
	uint64 H = 1469598103934665603ull;
	auto Mix = [&H](int64 V) { H ^= (uint64)V; H *= 1099511628211ull; };
	for (int VID : Mesh.VertexIndicesItr())
	{
		const FVector3d P = Mesh.GetVertex(VID);
		Mix(ToMm(P.X)); Mix(ToMm(P.Y)); Mix(ToMm(P.Z));
	}
	for (int TID : Mesh.TriangleIndicesItr())
	{
		const UE::Geometry::FIndex3i T = Mesh.GetTriangle(TID);
		Mix(T.A); Mix(T.B); Mix(T.C);
	}
	return H;
}

uint64 OrreryExport::HashTriMeshCollision(const FTriMeshCollisionData& Data)
{
	uint64 H = 1469598103934665603ull;
	auto Mix = [&H](int64 V) { H ^= (uint64)V; H *= 1099511628211ull; };
	for (const FVector3f& P : Data.Vertices)
	{
		Mix(ToMm(P.X)); Mix(ToMm(P.Y)); Mix(ToMm(P.Z));
	}
	for (const FTriIndices& T : Data.Indices)
	{
		Mix(T.v0); Mix(T.v1); Mix(T.v2);
	}
	return H;
}
