using UnrealBuildTool;

public class OrreryObserverTarget : TargetRules
{
	public OrreryObserverTarget(TargetInfo Target) : base(Target)
	{
		Type = TargetType.Game;
		DefaultBuildSettings = BuildSettingsVersion.V7;
		IncludeOrderVersion = EngineIncludeOrderVersion.Unreal5_8;
		ExtraModuleNames.AddRange(new string[] { "OrreryObserver" });
	}
}
