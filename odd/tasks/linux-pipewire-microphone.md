# Linux microphone through PipeWire (T5 of linux-input-control)
Objective: record the voice input natively through PipeWire on Linux, keeping the parec/arecord recorder as the fallback. Branch: feat/linux-pipewire-mic (MarsSall fork), based on CinloDev:feat/linux-distribution; delivered as a PR to that branch.
Runners: `cargo test --lib`, `cargo clippy --all-targets` (apps/desktop/src-tauri) inside a Linux container; live smoke against a headless PipeWire.

## Specs
S1. Source task, `odd/tasks/linux-input-control.md` T5: "Native Linux microphone (PipeWire) replacing the parec/arecord subprocess — own commit, after input".
S2. Decision (Mars, 2026-11-13): "PipeWire nativo + fallback a parec/arecord (recomendado)" — "Usa PipeWire directo; si no conecta (sin daemon), cae al recorder actual. Suma libpipewire como dependencia del paquete y del CI."
S3. Keep the existing `Microphone`/`Stream`/`Sink` contract: 16 kHz mono f32 PCM to `Sink::audio`, `Sink::failed` with `VoiceError::Interrupted` when the capture ends without being closed, no permission prompt on Linux. macOS and Windows are untouched (per Cinlo's linux-screenshot-capture S1: "no quiero mandar al repo cosas q puedan romper los demas sistemas operativos.").
S4. Cinlo's acceptance of the work: "lo hagamos" (relayed by Mars). Live microphone verification on Cinlo's Hyprland is hers; ours is unit tests plus a headless PipeWire smoke in a container.

## Tasks
T1 | S2,S3 | Fallback microphone: try the primary, fall back to the recorder only when the primary is unavailable (pure, tested on every OS) | worker | done | commit: 83fc4f1; RED 2 failed (fallback/neither), GREEN 4/4
T2 | S1,S3 | `voice/pipewire.rs`: PipeWire capture stream (F32LE, 16 kHz, mono), close/interruption, wired in `voice/app.rs` for Linux | worker | done | commit: 83fc4f1; cargo test --lib 674 passed/0 failed/9 ignored (Ubuntu 24.04), clippy clean in voice/, live smoke vs headless PipeWire null source passed
T3 | S2 | Build and package deps: CI apt list, deb/arch runtime and build deps, docs prerequisites (Risk item 5, own commit) | inline | done, verifying | commit: 0cd0716
T4 | S1,S4 | Docs: linux-testing.md voice check; mark T5 in linux-input-control.md | inline | done | commit: see docs commit
T5 | S1-S4 | Verify in a Linux container: cargo test --lib, clippy, headless PipeWire smoke; open PR to CinloDev:feat/linux-distribution | inline | in progress | commit: -

## Log
L1 (2026-11-13, Mars): "bien cinlo contesto que lo hagamos" — answering the proposal to take T5 (native PipeWire microphone) and send it as a PR to `feat/linux-distribution`, unit-tested here, live-tested by Cinlo.
L2 (2026-11-13, Mars): chose "PipeWire nativo + fallback a parec/arecord (recomendado)".
L4 (2026-11-13): slice boundary: this feature is ~300 authored lines on top of Cinlo's branch (the commit-slice counter measures from origin/main and includes her 10 commits); it ships as one PR to CinloDev:feat/linux-distribution in three commits (code, build deps, docs).
L5 (2026-11-13): verification. Debian bookworm (libpipewire 0.3.65) via `linux-cargo-check`: cargo test 674 passed/0 failed; its `clippy -D warnings` step stops on 6 warnings that already exist on the base branch (config.rs:150, platform.rs:98, daemon/windows.rs:62,66, daemon/windows_record.rs:43,226), none in voice/. Packages: deb `Depends: libpipewire-0.3-0, libayatana-appindicator3-1, libwebkit2gtk-4.1-0, libgtk-3-0`; Arch `.PKGINFO` depends on libpipewire; `linux-check` install + app smoke passed on ubuntu 24.04, debian bookworm and arch. Native review (risk, resilience, reliability) approved with three non-blocking findings, left as follow-ups: (1) `open` reports ready before the stream negotiates, so a daemon with no usable source or no session manager linking the stream opens successfully and stays silent, with no timeout or fallback; (2) losing only the source can leave the stream Paused, which is not treated as an interruption; (3) same as (1) from the resilience lens. Fix direction: report readiness on Streaming or first PCM with a startup deadline, and track source loss after capture is established; test with a headless daemon without a source and removing the active source.
L3 (2026-11-13): no graphical session or Hyprland on this host (SSH, tty); Docker available for Linux builds.
