// Spike #1046 — MeasureLevel: load a hand-authored level from disk, count its static-mesh
// collision geometry, and write the ruleset `tri` collision package for it (spike 2's format,
// every static-mesh component flattened to world space the way spike 2 flattened PCG instances).
// Added to spike 2's OneBodyCook editor module on the Mac; source mirrored on the spike branch.
#pragma once

#include "CoreMinimal.h"
#include "Commandlets/Commandlet.h"
#include "MeasureLevelCommandlet.generated.h"

UCLASS()
class UMeasureLevelCommandlet : public UCommandlet
{
	GENERATED_BODY()
public:
	UMeasureLevelCommandlet();
	virtual int32 Main(const FString& Params) override;
};
