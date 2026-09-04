// Spike #1044 — TraceBody: load the cooked map from disk in a fresh process and trace the ray file with UWorld::LineTraceSingleByChannel.
#pragma once

#include "CoreMinimal.h"
#include "Commandlets/Commandlet.h"
#include "TraceBodyCommandlet.generated.h"

UCLASS()
class UTraceBodyCommandlet : public UCommandlet
{
	GENERATED_BODY()
public:
	UTraceBodyCommandlet();
	virtual int32 Main(const FString& Params) override;
};
