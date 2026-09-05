#include "ObserverScenario.h"

#include "Components/CapsuleComponent.h"
#include "Engine/World.h"
#include "HAL/PlatformTime.h"
#include "Kismet/GameplayStatics.h"
#include "Misc/CommandLine.h"
#include "Misc/FileHelper.h"
#include "Misc/Parse.h"
#include "Misc/Paths.h"
#include "OrreryObserverModule.h"

// The whole Rust boundary: one hand-written C header, no Bevy type, no
// lightyear type, no socket.
#include "orrery_unreal_observer.h"

namespace
{
/** The lattice is millimetres; Unreal units are centimetres. */
constexpr double MillimetresPerUnrealUnit = 10.0;

/** How large a capsule stands in for an entity, in Unreal units. */
constexpr float CapsuleRadius = 34.0f;
constexpr float CapsuleHalfHeight = 88.0f;

/** How many records the snapshot buffer starts at; it grows if asked to. */
constexpr int32 InitialCapacity = 64;

/** Ticks excluded from the timing percentiles: they pay for capsule spawns. */
constexpr int32 WarmupTicks = 60;

/** The `p`th percentile of an already-sorted array, nearest-rank. */
double PercentileOf(const TArray<double>& Sorted, double P)
{
	if (Sorted.Num() == 0)
	{
		return 0.0;
	}
	const int32 Rank = FMath::Clamp(
		static_cast<int32>(FMath::CeilToDouble(P * static_cast<double>(Sorted.Num()))) - 1, 0,
		Sorted.Num() - 1);
	return Sorted[Rank];
}

/**
 * Build a rotation from the frame's quantized forward and up.
 *
 * The quantization carries direction and not magnitude, so both are
 * normalised here; a degenerate or collinear pair (which the schema forbids a
 * producer from sending) falls back to identity rather than to a NaN
 * rotation that would poison the transform.
 */
FRotator RotationFrom(const OrreryObservedEntity& Entity)
{
	const FVector Forward(static_cast<double>(Entity.forward_x), static_cast<double>(Entity.forward_y),
		static_cast<double>(Entity.forward_z));
	const FVector Up(static_cast<double>(Entity.up_x), static_cast<double>(Entity.up_y),
		static_cast<double>(Entity.up_z));
	if (Forward.IsNearlyZero() || Up.IsNearlyZero())
	{
		return FRotator::ZeroRotator;
	}
	const FVector F = Forward.GetSafeNormal();
	const FVector U = Up.GetSafeNormal();
	if (FMath::IsNearlyEqual(FMath::Abs(FVector::DotProduct(F, U)), 1.0, 1e-4))
	{
		return FRotator::ZeroRotator;
	}
	return FRotationMatrix::MakeFromXZ(F, U).Rotator();
}
} // namespace

AObserverScenario::AObserverScenario()
{
	PrimaryActorTick.bCanEverTick = true;
	PrimaryActorTick.bStartWithTickEnabled = true;
	RootComponent = CreateDefaultSubobject<USceneComponent>(TEXT("Root"));
}

void AObserverScenario::BeginPlay()
{
	Super::BeginPlay();

	// The archive and the header have to agree before a single byte is read.
	// A mismatch here is a stale `.a` beside a new header, which would
	// otherwise show up as garbled transforms much later.
	if (orrery_observer_abi_version() != ORRERY_OBSERVER_ABI_VERSION)
	{
		UE_LOG(LogOrreryObserver, Error, TEXT("spike 898: ABI %u, header %u"),
			orrery_observer_abi_version(), ORRERY_OBSERVER_ABI_VERSION);
		return;
	}
	if (orrery_observer_entity_size() != static_cast<uint32>(sizeof(OrreryObservedEntity)))
	{
		UE_LOG(LogOrreryObserver, Error, TEXT("spike 898: record %u bytes, header %u"),
			orrery_observer_entity_size(), static_cast<uint32>(sizeof(OrreryObservedEntity)));
		return;
	}

	FParse::Value(FCommandLine::Get(), TEXT("ObserverTicks="), RequestedTicks);
	if (!FParse::Value(FCommandLine::Get(), TEXT("ObserverOut="), OutDir))
	{
		OutDir = FPaths::ProjectSavedDir();
	}

	Dial();
	UE_LOG(LogOrreryObserver, Display, TEXT("spike 898: observing %d sidecar(s), ticks=%d"),
		Links.Num(), RequestedTicks);
}

void AObserverScenario::Dial()
{
	// `-ObserverAddr=` may appear twice: #898 step 3 renders *two* sidecars,
	// and one link per sidecar is the shape `orrery_unreal_observer` offers.
	// Nothing multiplexes them; two handles is the whole design.
	const TCHAR* Stream = FCommandLine::Get();
	FString Addr;
	while (FParse::Value(Stream, TEXT("ObserverAddr="), Addr))
	{
		FLink Link;
		Link.Addr = Addr;
		Link.Handle = orrery_observer_connect(TCHAR_TO_UTF8(*Addr));
		if (Link.Handle == nullptr)
		{
			UE_LOG(LogOrreryObserver, Error, TEXT("spike 898: cannot dial %s"), *Addr);
		}
		else
		{
			UE_LOG(LogOrreryObserver, Display, TEXT("spike 898: dialled %s"), *Addr);
			Links.Add(MoveTemp(Link));
		}

		// Advance past the token just consumed so a second `-ObserverAddr=`
		// is found rather than the first one being returned forever.
		const TCHAR* Found = FCString::Strifind(Stream, TEXT("ObserverAddr="));
		if (Found == nullptr)
		{
			break;
		}
		Stream = Found + FCString::Strlen(TEXT("ObserverAddr="));
	}
}

AObserverScenario::FCapsule& AObserverScenario::CapsuleFor(int32 LinkIndex, uint64 PersistId, uint8 Timeline)
{
	// Keyed on (link, stable id) — never on an engine-native handle, and
	// never on an index into the snapshot, which is only stable within one
	// copy-out. Two sidecars may legitimately present the same id.
	const FString Key = FString::Printf(TEXT("%d/%llu"), LinkIndex, static_cast<unsigned long long>(PersistId));
	if (FCapsule* Existing = Capsules.Find(Key))
	{
		return *Existing;
	}

	FCapsule Capsule;
	Capsule.Actor = GetWorld()->SpawnActor<AActor>(AActor::StaticClass(), FTransform::Identity);
	Capsule.Shape = NewObject<UCapsuleComponent>(Capsule.Actor);
	Capsule.Shape->SetCapsuleSize(CapsuleRadius, CapsuleHalfHeight);
	// Wireframe, which is what #898 step 3 asks for, and no collision: this
	// capsule is a picture of a fact, not a participant in one.
	Capsule.Shape->SetCollisionEnabled(ECollisionEnabled::NoCollision);
	Capsule.Shape->ShapeColor =
		Timeline == ORRERY_OBSERVER_PREDICTED ? FColor(80, 200, 120) : FColor(120, 160, 240);
	Capsule.Shape->SetHiddenInGame(false);
	Capsule.Shape->RegisterComponent();
	Capsule.Actor->SetRootComponent(Capsule.Shape);

	UE_LOG(LogOrreryObserver, Display, TEXT("spike 898: capsule for %s (%s)"), *Key,
		Timeline == ORRERY_OBSERVER_PREDICTED ? TEXT("predicted") : TEXT("interpolated"));
	return Capsules.Add(Key, Capsule);
}

void AObserverScenario::RenderLink(int32 LinkIndex)
{
	FLink& Link = Links[LinkIndex];
	if (Link.Handle == nullptr)
	{
		return;
	}

	uint32 Applied = 0;
	Link.LastStatus = orrery_observer_poll(Link.Handle, &Applied);
	Link.Applied += Applied;
	if (Link.LastStatus != ORRERY_OBSERVER_OK && Link.LastStatus != ORRERY_OBSERVER_LINK_CLOSED
		&& Link.LastStatus != ORRERY_OBSERVER_LINK_FAILED)
	{
		UE_LOG(LogOrreryObserver, Error, TEXT("spike 898: poll on %s returned %d"), *Link.Addr,
			Link.LastStatus);
		return;
	}

	// Size, then copy out. A buffer that is too small is answered with the
	// size rather than with a truncated set, so the pool grows once and never
	// tears a frame.
	static TArray<OrreryObservedEntity> Buffer;
	if (Buffer.Num() < InitialCapacity)
	{
		Buffer.SetNumUninitialized(InitialCapacity);
	}
	uint32 Required = 0;
	int32 Status = orrery_observer_snapshot(Link.Handle, Buffer.GetData(),
		static_cast<uint32>(Buffer.Num()), &Required);
	if (Status == ORRERY_OBSERVER_TOO_SMALL)
	{
		Buffer.SetNumUninitialized(static_cast<int32>(Required));
		Status = orrery_observer_snapshot(Link.Handle, Buffer.GetData(),
			static_cast<uint32>(Buffer.Num()), &Required);
	}
	if (Status != ORRERY_OBSERVER_OK)
	{
		UE_LOG(LogOrreryObserver, Error, TEXT("spike 898: snapshot on %s returned %d"), *Link.Addr,
			Status);
		return;
	}

	for (uint32 Index = 0; Index < Required; ++Index)
	{
		const OrreryObservedEntity& Entity = Buffer[static_cast<int32>(Index)];
		FCapsule& Capsule = CapsuleFor(LinkIndex, Entity.persist_id, Entity.timeline);

		// Assignment, not interpolation and not physics. The frame already
		// *is* the presented value: the sidecar's interpolated class was
		// blended by `orrery_predict` on the basis this record carries, and
		// smoothing it again here would put a position on screen that no
		// ruleset and no basis describes.
		const FVector Location(static_cast<double>(Entity.x_mm) / MillimetresPerUnrealUnit,
			static_cast<double>(Entity.y_mm) / MillimetresPerUnrealUnit,
			static_cast<double>(Entity.z_mm) / MillimetresPerUnrealUnit);
		Capsule.Actor->SetActorLocationAndRotation(Location, RotationFrom(Entity));

		++EntitiesSeen;
		if (Entity.timeline == ORRERY_OBSERVER_PREDICTED)
		{
			++PredictedSeen;
		}
		else
		{
			++InterpolatedSeen;
		}
		if (Entity.basis_from != Entity.basis_to)
		{
			++BracketedSeen;
		}

		if (TicksRun % 30 == 0)
		{
			Rows.Add(FString::Printf(TEXT("%d,%d,%llu,%s,%lld,%llu,%llu,%llu,%u,%u"), TicksRun,
				LinkIndex, static_cast<unsigned long long>(Entity.persist_id),
				Entity.timeline == ORRERY_OBSERVER_PREDICTED ? TEXT("predicted") : TEXT("interpolated"),
				static_cast<long long>(Entity.x_mm),
				static_cast<unsigned long long>(Entity.presented_at),
				static_cast<unsigned long long>(Entity.basis_from),
				static_cast<unsigned long long>(Entity.basis_to),
				static_cast<unsigned>(Entity.basis_alpha), static_cast<unsigned>(Entity.corrected)));
		}
	}
}

void AObserverScenario::Tick(float DeltaSeconds)
{
	Super::Tick(DeltaSeconds);
	if (Links.Num() == 0)
	{
		return;
	}

	// The measured window is the whole crossing as the game thread pays for
	// it: poll, copy-out, and the actor moves the copy-out produces. Not the
	// engine's frame, and not the sidecar's tick — the part that exists
	// because the simulation is in another process.
	const double Started = FPlatformTime::Seconds();
	for (int32 Index = 0; Index < Links.Num(); ++Index)
	{
		RenderLink(Index);
	}
	const double Elapsed = FPlatformTime::Seconds() - Started;
	++TicksRun;
	// The first ticks pay once for spawning the capsules; a percentile that
	// included them would be describing startup, not the steady state.
	if (TicksRun > WarmupTicks)
	{
		TickNanos.Add(Elapsed * 1e9);
	}

	if (RequestedTicks > 0 && TicksRun >= RequestedTicks)
	{
		WriteReport();
		UE_LOG(LogOrreryObserver, Display, TEXT("spike 898: requested ticks reached, quitting"));
		FPlatformMisc::RequestExit(false);
	}
}

void AObserverScenario::WriteReport()
{
	if (bReported)
	{
		return;
	}
	bReported = true;

	FString Header = TEXT("tick,link,persist_id,class,x_mm,presented_at,basis_from,basis_to,basis_alpha,corrected\n");
	FFileHelper::SaveStringToFile(Header + FString::Join(Rows, TEXT("\n")) + TEXT("\n"),
		*FPaths::Combine(OutDir, TEXT("observer-frames.csv")));

	TArray<FString> Summary;
	Summary.Add(FString::Printf(TEXT("ticks=%d"), TicksRun));
	Summary.Add(FString::Printf(TEXT("entities_seen=%llu"), static_cast<unsigned long long>(EntitiesSeen)));
	Summary.Add(FString::Printf(TEXT("predicted_seen=%llu"), static_cast<unsigned long long>(PredictedSeen)));
	Summary.Add(FString::Printf(TEXT("interpolated_seen=%llu"), static_cast<unsigned long long>(InterpolatedSeen)));
	Summary.Add(FString::Printf(TEXT("bracketed_seen=%llu"), static_cast<unsigned long long>(BracketedSeen)));
	Summary.Add(FString::Printf(TEXT("capsules=%d"), Capsules.Num()));

	TArray<double> Sorted = TickNanos;
	Sorted.Sort();
	Summary.Add(FString::Printf(TEXT("crossing_samples=%d (warmup %d excluded)"), Sorted.Num(),
		WarmupTicks));
	Summary.Add(FString::Printf(TEXT("crossing_ns_p50=%.0f"), PercentileOf(Sorted, 0.50)));
	Summary.Add(FString::Printf(TEXT("crossing_ns_p99=%.0f"), PercentileOf(Sorted, 0.99)));
	Summary.Add(FString::Printf(TEXT("crossing_ns_p999=%.0f"), PercentileOf(Sorted, 0.999)));
	Summary.Add(FString::Printf(TEXT("crossing_ns_max=%.0f"),
		Sorted.Num() > 0 ? Sorted.Last() : 0.0));
	for (int32 Index = 0; Index < Links.Num(); ++Index)
	{
		Summary.Add(FString::Printf(TEXT("link%d=%s applied=%llu status=%d"), Index, *Links[Index].Addr,
			static_cast<unsigned long long>(Links[Index].Applied), Links[Index].LastStatus));
	}
	const FString Text = FString::Join(Summary, TEXT("\n")) + TEXT("\n");
	FFileHelper::SaveStringToFile(Text, *FPaths::Combine(OutDir, TEXT("observer-summary.txt")));
	UE_LOG(LogOrreryObserver, Display, TEXT("spike 898: summary\n%s"), *Text);
}

void AObserverScenario::EndPlay(const EEndPlayReason::Type Reason)
{
	WriteReport();
	for (FLink& Link : Links)
	{
		orrery_observer_destroy(Link.Handle);
		Link.Handle = nullptr;
	}
	Links.Reset();
	Super::EndPlay(Reason);
}
