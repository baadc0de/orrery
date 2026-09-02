# Play Regolith

This folder is a complete, code-only Regolith client release. Download the
archive for your computer from the Orrery [Releases page](https://github.com/baadc0de/orrery/releases), then extract it before opening the game.

**Extract the archive before running it, and extract it somewhere you own** —
your Desktop or a folder under your user account. The game writes its files
into the folder the game itself is in: the session log, the join file, and
the retry state all appear next to the executable, so you can find them and
send them back without hunting through hidden system directories.

That is also why the folder has to be writable. If you run the game from
*inside* the ZIP (Windows opens it in a read-only temporary folder), or from
`Program Files`, or from a folder Windows' Controlled Folder Access is
guarding, the game will tell you **"Could not save the join file: Access is
denied"** and stop before you join. Extracting to your Desktop avoids all
three. If you would rather keep the game where it is, start it with
`--telemetry-jsonl <a writable path>` and every file follows that location.

- **Windows PC:** download `orrery-regolith-x86_64-windows.zip`, right-click it
  and choose **Extract All**, then open `orrery-regolith-x86_64-windows.exe`.
  Windows may show *Windows protected your PC*. Choose **More info**, then
  **Run anyway**. The playtest build is unsigned, so this warning is expected.
- **Intel Linux PC:** download `orrery-regolith-x86_64-linux.tar.gz`, extract
  it, open a terminal in the extracted folder and run
  `chmod +x orrery-regolith-x86_64-linux` once, then
  `./orrery-regolith-x86_64-linux`.
- **Apple-silicon Mac (M-series):** download
  `orrery-regolith-aarch64-macos.tar.gz` and extract it. In Finder,
  Control-click `orrery-regolith-aarch64-macos`, choose **Open**, then choose
  **Open** again. macOS may initially say the developer cannot be verified:
  this is expected because the playtest build is not notarized. The first
  Control-click → Open approval lets that copy run afterwards. This release
  does not support Intel Macs.

Keep the extracted folder together. `build-info.json` records the exact commit
and ruleset version embedded in the binary; `<client>.sha256` lets a technical
helper verify the binary download. The archive contains no game assets: the
current client draws its ships and rocks with built-in Bevy primitives. In
particular, it contains no loose `.glb` files or licensed kitbash content.

## Join the shakedown campaign

Open the client normally. It contacts the Orrery campaign service automatically.
Choose **Shakedown**, enter the nickname agreed with the organiser, read the
recording notice and confirm it, then join. No Steam account, invite code, or
command-line options are needed.

If Shakedown is absent, it is not open yet; check with the organiser. If the
game says it cannot reach the campaign service, check your connection and try
again. If it says this build is refused or needs a different revision, download
the newest archive from the Releases page and replace the whole extracted
folder. A version refusal is intentional: the campaign only accepts the build
and ruleset it has pinned, so every recorded session remains auditable.

`--help` prints the available command-line options and exits without opening a
window.

## Operator note: a rebuild cannot rescue a live session

The three-platform packaging workflow takes roughly fifteen minutes and only
publishes the GitHub release after its slowest platform finishes. Do not spend
a waiting tester session on a rebuild. When one platform is urgently needed,
its `regolith-<platform>` workflow artifact is downloadable as soon as that
matrix leg finishes, before the final release-publishing job runs. Every leg
extracts its own archive and joins the deployed campaign with the extracted
binary before the artifact is uploaded, and the Linux leg additionally runs the
three-client cohort preflight, so update the campaign's revision pin to the
candidate commit before starting the release workflow. The workflow-dispatch
input `join_all_platforms` turns those joins off when a waiting tester makes
their wall-clock cost worse than the coverage they buy.
