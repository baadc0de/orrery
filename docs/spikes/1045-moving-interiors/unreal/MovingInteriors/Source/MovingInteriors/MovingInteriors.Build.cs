using UnrealBuildTool;
using System;
using System.IO;

// Spike #1045: the Unreal module that links the nested-frame rules staticlib
// (crates/orrery_unreal_interiors, built in the shape of spike #1052's
// non-App prong) and mirrors it in a per-grid local frame. Research code;
// never shipped.
public class MovingInteriors : ModuleRules
{
	public MovingInteriors(ReadOnlyTargetRules Target) : base(Target)
	{
		PCHUsage = PCHUsageMode.UseExplicitOrSharedPCHs;

		PublicDependencyModuleNames.AddRange(new string[] { "Core", "CoreUObject", "Engine" });
		PrivateDependencyModuleNames.AddRange(new string[] { "Projects", "RenderCore", "RHI" });

		// The repository root is seven levels above this directory:
		// docs/spikes/1045-moving-interiors/unreal/MovingInteriors/Source/MovingInteriors
		string RepoRoot = Environment.GetEnvironmentVariable("ORRERY_REPO_ROOT");
		if (string.IsNullOrEmpty(RepoRoot))
		{
			RepoRoot = Path.GetFullPath(Path.Combine(ModuleDirectory, "..", "..", "..", "..", "..", "..", ".."));
		}
		string Lib = Environment.GetEnvironmentVariable("ORRERY_INTERIORS_LIB");
		if (string.IsNullOrEmpty(Lib))
		{
			Lib = Path.Combine(RepoRoot, "target", "release", "liborrery_unreal_interiors.a");
		}
		if (!File.Exists(Lib))
		{
			throw new BuildException("Spike #1045: staticlib not found at " + Lib + "; run `cargo build --release -p orrery_unreal_interiors` first (or set ORRERY_INTERIORS_LIB)");
		}
		PublicAdditionalLibraries.Add(Lib);
		PublicIncludePaths.Add(Path.Combine(RepoRoot, "crates", "orrery_sim_host", "include"));
		PublicIncludePaths.Add(Path.Combine(RepoRoot, "crates", "orrery_unreal_interiors", "include"));
		PublicIncludePaths.Add(Path.Combine(RepoRoot, "crates", "orrery_unreal_interiors", "examples", "c"));
		// What rustc's --print native-static-libs names for the archive.
		PublicSystemLibraries.AddRange(new string[] { "gcc_s", "util", "rt", "pthread", "m", "dl" });
	}
}
