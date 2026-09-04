using UnrealBuildTool;
using System.Collections.Generic;

public class MovingInteriorsEditorTarget : TargetRules
{
	public MovingInteriorsEditorTarget(TargetInfo Target) : base(Target)
	{
		Type = TargetType.Editor;
		DefaultBuildSettings = BuildSettingsVersion.V7;
		IncludeOrderVersion = EngineIncludeOrderVersion.Unreal5_8;
		ExtraModuleNames.AddRange(new string[] { "MovingInteriors" });
	}
}
