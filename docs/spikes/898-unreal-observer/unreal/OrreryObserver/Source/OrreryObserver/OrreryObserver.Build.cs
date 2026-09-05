using System;
using System.IO;
using UnrealBuildTool;

// The Rust boundary, wired exactly as spike #1045's `MovingInteriors.Build.cs`
// wires its own: `PublicAdditionalLibraries` for the archive,
// `PublicIncludePaths` for the hand-written C header, and
// `PublicSystemLibraries` for what `rustc --print native-static-libs` names.
// ADR-0053 clause (c) item 5 predicted this would be "a `.Build.cs` and a C++
// file, not a redesign"; this file is the second instance of that prediction
// holding.
public class OrreryObserver : ModuleRules
{
	public OrreryObserver(ReadOnlyTargetRules Target) : base(Target)
	{
		PCHUsage = PCHUsageMode.UseExplicitOrSharedPCHs;

		PublicDependencyModuleNames.AddRange(new string[] { "Core", "CoreUObject", "Engine" });
		PrivateDependencyModuleNames.AddRange(new string[] { "Projects" });

		// Seven levels up from Source/OrreryObserver:
		// unreal/OrreryObserver/Source/OrreryObserver -> … -> repo root.
		string RepoRoot = Environment.GetEnvironmentVariable("ORRERY_REPO_ROOT");
		if (string.IsNullOrEmpty(RepoRoot))
		{
			RepoRoot = Path.GetFullPath(Path.Combine(ModuleDirectory,
				"..", "..", "..", "..", "..", "..", ".."));
		}

		string Lib = Environment.GetEnvironmentVariable("ORRERY_OBSERVER_LIB");
		if (string.IsNullOrEmpty(Lib))
		{
			Lib = Path.Combine(RepoRoot, "target", "release", "liborrery_unreal_observer.a");
		}
		if (!File.Exists(Lib))
		{
			throw new BuildException("Spike #898: observer staticlib not found at " + Lib
				+ "; run `cargo build --release -p orrery_unreal_observer` first"
				+ " (or set ORRERY_OBSERVER_LIB).");
		}
		PublicAdditionalLibraries.Add(Lib);
		PublicIncludePaths.Add(Path.Combine(RepoRoot, "crates", "orrery_unreal_observer", "include"));

		// What rustc's --print native-static-libs names for this archive. It
		// is a shorter list than #1045's host archives need, because this one
		// carries a socket and a codec and nothing else.
		PublicSystemLibraries.AddRange(new string[] { "gcc_s", "util", "rt", "pthread", "m", "dl" });
	}
}
