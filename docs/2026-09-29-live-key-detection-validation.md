# Live key detection validation

Date: 2026-09-29.

## Current status

Implemented, reviewed, debug-built, and verified against genuine current-player audio. The debug helper is running; the release executable was not changed. This validates the live PCM-to-label pipeline, not independently labeled real-song musical accuracy. No Auto-Tune parameters were written.

## Frontend and browser evidence

| Command | Result |
| --- | --- |
| `npm test` | 5 files, 143 tests passed |
| `npm run build` | TypeScript and Vite build passed; the existing >500 kB chunk warning remains |
| `npm run test:ui` | Final full rerun: 132 Playwright tests passed in 1.9 minutes |
| `node --check scripts/verify-key-detection.mjs` | Native verifier syntax passed |
| Focused key UI/evidence/transport tests | 30 passed in 34.3 seconds |
| `cargo test --manifest-path src-tauri/Cargo.toml --lib` | 73 passed, zero failures |
| `cargo fmt --manifest-path src-tauri/Cargo.toml -- --check` | Passed |
| Verification debug Tauri build | Passed; no bundle/release build |

The focused key-label unit suite covers all 24 literal labels plus malformed status, pitch class, mode, ownership, generation, current identity, and timestamp inputs. Browser tests render the real App and Motion components while replacing only the native IPC boundary. They cover:

- initial `AutoTune Helper` fallback and validated `A 小调` display;
- old source/song/generation rejection after a media switch;
- same-song pause retention without aging out the confirmed result;
- identical polling results retaining the same DOM node without restarting motion;
- reduced-motion fade fallback;
- synchronous removal of the old label in the same render that commits a new media identity;
- one native read in flight across rapid hook identity lifecycles;
- invalidation of a read started before a hidden/visible transition;
- preserved 41 px titlebar, drag region, window actions, 14 px / 650 label, and no native-size clipping;
- `get_key_detection` forwarding as read-only IPC while unknown commands remain blocked.

The initial implementation regression exposed three player-controls fixture assertions that did not yet list the new read-only command. The fixture now returns an unavailable key snapshot and permits only `get_key_detection` in addition to its previous reads; that stage passed 111/111. The review subsequently added evidence-harness and animation-interval tests, bringing the suite to 132.

The controller's first 132-test run passed 131 and timed out waiting for an isolated ElasticSlider fixture. Its error snapshot showed the normal App rather than the route-injected EndpointConsumer. The immediate isolated rerun passed, and the second complete run passed 132/132. No slider implementation/test change was made, and the intermittent fixture cause is not claimed proved or fixed. An evidence excerpt is retained at `artifacts/key-detection-acceptance/regression-first-run.txt`. Runner color warnings were eliminated through process-scoped NO_COLOR removal, not global environment edits.

## Native verification completed

The controller used `npm run tauri -- build --debug --no-bundle --config src-tauri/tauri.verify.conf.json`. The old debug process was checked against its exact path and creation timestamp immediately before normal main-window close, and was confirmed exited. No process was forcibly terminated. The new debug executable was launched normally via Start-Process and the verifier began immediately when CDP became available. The player queue, playback and DAWs were not operated.

Actual native result:

- Process PID at acceptance: 22444, created `2026-09-29T14:50:18.9498130Z`; path `src-tauri/target/debug/autotune-helper-now-playing.exe`. These are historical evidence, not authority for later process operations.
- Verifier: PASS, exit 0, 30 observations. It observed an initial idle result and later a new matching detected result while media was ready/playing and matching process audio was capturing at nonzero level.
- Accepted label: `F♯ 小调`, pitch class 6, mode minor, source/song/media-generation ownership all matching. Audio RMS was 0.0909253, peak/level 0.265695, capture age 0 ms at the accepted sample.
- First confirmed publication occurred 15,659 ms after process creation; the accepted visible label was sampled at 16,411 ms. These are this run's measurements, not a fixed latency promise.
- The first eight native analysis logs used 6,040 ms, 7,090 ms, then 8,000 ms windows. Two initial calls returned None in 35/38 ms; the six non-None calls took 246–291 ms. The final three distinct sample cursors all produced F-sharp minor before confirmation.
- Present title: one current frame, 14 px / weight 650, 41 px titlebar, no horizontal overlap with actions. Screenshot was taken after exit frames disappeared and incoming position/opacity settled; controller visually inspected it.
- No page/console errors. Guard recorded only get_audio_level/get_now_playing/get_key_detection, zero blocked commands, intact ownership and successful restoration.

Evidence: `artifacts/key-detection-native-1790693420399/verification.json` and `key-detection.png`; native logs/process record under `artifacts/key-detection-acceptance/run-1790693416395/`. The verifier's result observations contain numeric/status/match metadata rather than audio or artwork bytes. Screenshots naturally contain visible song information. No PCM was saved or uploaded.

Memory: the ring is statically bounded to 768,000 f32 samples (3,072,000 bytes); one analysis takes a bounded copy, without a job queue. The main process working set was 48.25 MiB before verification and 77.23 MiB afterward, with a 78.63 MiB observed peak at that point; private bytes were 12.23 and 39.88 MiB. A later read was 78,082,048 working-set bytes and 39,530,496 private bytes. These measurements exclude WebView child processes and are not a long-duration leak proof.

Settings preservation: `ui-before.json` records the pre-close preferences, preview values, pin state and viewport. The new process loaded different persisted preference values; they were backed up to `ui-newly-loaded-backup.json` before restoring exactly the saved three helper preference keys. The original non-pinned state, 468×242 viewport and preview values (strength 50, flex 0, vibrato 0, humanize 0, vocal removal false) were read back matching in `ui-after.json`. The final restored visual was also inspected. The origin of the pre-/post-launch persisted-setting discrepancy was not investigated as a production defect in this task.

Build warnings retained: existing Vite chunk-size warning, plus a dead-code warning for Stabilizer::begin, which is used by tests rather than the production worker. Build exit status was successful; no warning was hidden by changing production settings.

Release SHA-256 before/after: `78FB7563BBEA3EC48E1478CFFC9AAFC3415D7CD65DC817140FC8CD5C550396CB` (unchanged). Debug SHA-256: `B980CEDA5C781873217D2B40E0FAE68408D066196FCD058F4E48374367A3562D`.

The last read-back confirmed the native main window visible, native size 468×242, idle footer restored and no verifier guard left active. The debug helper remains open for user inspection.

## Retained implementation decisions

1. No Git initialization: retained pre-edit filesystem snapshots and no-index review diffs instead; tradeoff is more local evidence disk usage.
2. Serial implementation with independent task/cross-task review: tradeoff is additional review time.
3. Required the pinned, retained CMake build tool rather than implicit environment/PATH fallback: another development machine must restore that documented toolchain. This does not add a commercial runtime dependency.

## Evidence boundary and engine limits

The controller freshly reran all 73 Rust tests. Engine/backend and frontend production integration received independent reviews; Task 3's five evidence-harness issues and its follow-up timeout-options issue were fixed and separately re-reviewed. Native acceptance is recorded above, independently of browser fixtures.

Synthetic C-major and A-minor cadence results verify the pinned native engine path and enum mapping only. They do not establish real-song accuracy or calibrated confidence. As documented in [`keyfinder-dependencies.md`](keyfinder-dependencies.md), the current native engine classified a single A tone as A major and classified seeded broadband noise and a click train as A-flat minor. Single-tone, noise, percussion-heavy material, relationships between relative keys, vocals, mixed player sounds, and modulating songs therefore require independently labeled comparison before any correctness claim.
