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
extracts its own archive, holds it to the shipped file names, and launches the
extracted binary — including from a folder it cannot write — before uploading
it. None of that touches the campaign service, so a packaging run neither needs
a seat nor waits for one, and it can be re-run while somebody is flying.

## Operator note: validating a release, and the window between

**Publishing a build no longer join-tests it.** Until #1062 each packaging leg
joined the deployed campaign, and because that campaign has three human seats
and the three legs run in parallel, the Linux cohort took every seat and the
other two platforms failed on join — which is how `playtest-2026-09-04` failed
to publish at all, with three red legs that said nothing about the archives
they had built.

The joins moved, whole, to `.github/workflows/validate-client-release.yml`:

* one client from the published Windows archive,
* one from the published macOS archive,
* and the three-client cohort from the published Linux archive,

run one platform at a time, under a concurrency group, against the archives the
release actually carries. It runs nightly at 05:00 UTC, and it **skips with a
stated reason** — rather than failing — when the deployed campaign's
`client_rev` pin does not name the revision inside the published archive, when
the campaign is not open, or when fewer than three human seats are free. A red
leg there means the client is broken; it does not mean somebody else had the
seat.

The cost of that split, stated plainly: between publishing a build and
validating it, **nothing has join-tested it on any platform.** That check
exists because a Windows join defect once reached a volunteer (#769). The
nightly
closes the window in the steady state. For a release you intend to hand to a
tester, close it deliberately:

1. Point the campaign's `client_rev` pin at the release commit.
2. Dispatch `validate-client-release.yml` with that release's tag.
3. Read the result — including a skip, which means nothing was validated —
   before you send anyone the link.
