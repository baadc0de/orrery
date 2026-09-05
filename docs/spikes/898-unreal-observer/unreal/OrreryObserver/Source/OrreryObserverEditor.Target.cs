using UnrealBuildTool;

public class OrreryObserverEditorTarget : TargetRules
{
	public OrreryObserverEditorTarget(TargetInfo Target) : base(Target)
	{
		Type = TargetType.Editor;
		DefaultBuildSettings = BuildSettingsVersion.V7;
		IncludeOrderVersion = EngineIncludeOrderVersion.Unreal5_8;
		ExtraModuleNames.AddRange(new string[] { "OrreryObserver" });
	}
}
