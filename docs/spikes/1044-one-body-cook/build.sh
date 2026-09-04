set -o pipefail
UE="/Users/Shared/Epic Games/UE_5.8"
PROJ="$HOME/Development/orrery-onebody/OneBodyCook"
cd "$PROJ"
"$UE/Engine/Build/BatchFiles/Mac/Build.sh" OneBodyCookEditor Mac Development -Project="$PROJ/OneBodyCook.uproject" -WaitMutex -NoHotReload 2>&1 | tee "$HOME/Development/orrery-onebody/build.log" | grep -E "error|Error|warning: unused|Total time|Result|succeeded|failed|Building|Compiling|Link" | head -120
echo "exit: ${PIPESTATUS[0]}"
