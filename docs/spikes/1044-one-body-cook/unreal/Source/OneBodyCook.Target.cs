using UnrealBuildTool;
using System.Collections.Generic;

public class OneBodyCookTarget : TargetRules
{
	public OneBodyCookTarget(TargetInfo Target) : base(Target)
	{
		Type = TargetType.Game;
		DefaultBuildSettings = BuildSettingsVersion.V7;
		IncludeOrderVersion = EngineIncludeOrderVersion.Unreal5_8;
		// The spike module is editor-only (commandlets); the game target carries no module.
		ExtraModuleNames.AddRange(new string[] { });
	}
}
