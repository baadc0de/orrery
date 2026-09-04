using UnrealBuildTool;
using System.IO;

public class OneBodyCook : ModuleRules
{
	public OneBodyCook(ReadOnlyTargetRules Target) : base(Target)
	{
		PCHUsage = ModuleRules.PCHUsageMode.UseExplicitOrSharedPCHs;
		// FTransformer (the pipeline hook we subclass) lives in namespace UE::MeshPartition.
		bAllowUETypesInNamespaces = true;

		// MeshPartitionCompiledSection.h (a Public header of an Experimental plugin) includes an
		// Engine *Internal* header (MaterialCache/MaterialCacheVirtualTexture.h); a project module
		// cannot see Engine/Internal unless told where it is. Spike-grade workaround, noted in the report.
		PrivateIncludePaths.Add(Path.Combine(EngineDirectory, "Source/Runtime/Engine/Internal"));

		PublicDependencyModuleNames.AddRange(new string[]
		{
			"Core", "CoreUObject", "Engine",
		});

		PrivateDependencyModuleNames.AddRange(new string[]
		{
			"UnrealEd",
			"EditorSubsystem",
			"MeshPartition",
			"MeshPartitionEditor",
			"PCG",
			"GeometryCore",
			"GeometryFramework",
			"DynamicMesh",
			"MeshDescription",
			"StaticMeshDescription",
			"PhysicsCore",
			"Chaos",
			"Json",
			"JsonUtilities",
			"Projects",
			"ModelingComponents",
		});
	}
}
