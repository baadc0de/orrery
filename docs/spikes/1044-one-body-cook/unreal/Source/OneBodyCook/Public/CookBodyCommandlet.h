// Spike #1044 — CookBody: one seed in, one Unreal map package plus ruleset collision packages out.
#pragma once

#include "CoreMinimal.h"
#include "Commandlets/Commandlet.h"
#include "CookBodyCommandlet.generated.h"

UCLASS()
class UCookBodyCommandlet : public UCommandlet
{
	GENERATED_BODY()
public:
	UCookBodyCommandlet();
	virtual int32 Main(const FString& Params) override;
};
