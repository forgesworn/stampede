# Stampede authority component

MIT-licensed binding of Stampede's existing race rules to
`fray:authority@0.1.0`, world `game-authority`. The component exports `init`,
`on-event`, `tick`, `checkpoint`, `restore` and `state-hash`. It imports no host
services. The host supplies authenticated membership, ordered input and logical
time, and validates commands before sending them. Game messages remain Stampede's
existing protocol.

The local component checks pass on x86-64 Linux: exact native race outputs,
checkpoints, hashes, seven restore continuations, built-in tracks and cold lobby
track changes. The measured preparation profile is described below. Ingle worker
integration, cross-platform qualification and tier capacity remain later gates.

One component instance owns one session. Initialise it once with `init` or
`restore`. Any error ends its lifetime: discard tentative output and instantiate
a fresh component. Checkpoints use the native adapter's canonical bytes and
config binding. No networking, signing or owner credentials enter the guest.

## Build and compare

Use the repository's shared build lock and disk-floor guard for these commands.
The workspace requires the matching sibling Fray checkout containing the WIT
contract and the existing native dependencies for qualification.

```sh
export CARGO_TARGET_DIR="$HOME/.cache/twangthang-target"
export CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0
RUSTFLAGS='-C target-feature=-simd128 -C link-arg=--max-memory=67108864' \
  cargo +1.99.1 build -p race-authority --release --target wasm32-unknown-unknown

RACE_AUTHORITY_WASM="$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/race_authority.wasm" \
  cargo +1.99.1 test -p race-authority --features qualification --test component \
  -- --test-threads=1 --nocapture
```

Set `RACE_AUTHORITY_COMPONENT` to an output filename to retain the packed,
validated component used by the tests. Set `RACE_AUTHORITY_EVIDENCE` to a
directory to retain both checkpoint images if a byte comparison fails.
Set `RACE_AUTHORITY_REPORT` to save the successful race's resource report as JSON.
Missing guest input fails qualification.
The guest build uses the pure authority dependency graph; the optional native
`qualification` feature supplies the engine and original race fixture.

The local harness pins Wasmtime 50.0.0 and checks zero imports. It uses 64 MiB
guest memory, a 256 KiB execution stack and a 1 MiB lifting budget, with SIMD
disabled and NaNs canonicalised. The `stampede-preparation-v1` profile keeps ten
million fuel for live ticks, player events, checkpoints and hashes. Init, restore
and trusted controller changes have a separate one-billion-fuel preparation
budget. The selected track is prepared before the next live tick. A local epoch
clock enforces 500 ms for preparation and 100 ms for ordinary calls. Reports include
actual maximum fuel, checkpoint size and component size. The full race compares
every output, checkpoint and hash with native execution and continues seven
fresh restores across lobby, racing, departure, results and rematch.

This harness is a local component qualification tool. It does not provide the
Ingle process sandbox, worker deadlines, authenticated admission, module trust,
live room integration or a completed tier-S capacity assessment.

`RACE_AUTHORITY_DIAGNOSTICS=1` enables up to 32 backtrace frames for local
diagnosis. Keep a separate guest binary with function names retained for useful
frames. This changes diagnostic capture only. Diagnostic runs are separate from
acceptance.

`cargo +1.99.1 build -p race-authority --features qualification --example profile_setup`
builds a separate setup profiler. Run `profile_setup REPORT.json` with
`RACE_AUTHORITY_WASM` set to the local first-party core image and a 60-second
outer timeout under the shared lock. Its one-billion-fuel diagnostic ceiling
measures cold work; reports always contain `qualification: false`. Component
tests enforce the operation budgets above. Ordinary-frame timing, cold
race startup, restore and exact-state mismatches are reported separately.
`profile_setup --decode IMAGE.bin OUTPUT.json` inspects retained first-party
checkpoint images for field-level diagnosis.

The setup profiler includes an explicit cold Savanna-to-Oasis controller change
at tick 3, followed by exact native commands, checkpoint/hash and fresh-restore
checks. Its fuel/time measurements remain local diagnostics, with
`qualification: false`; they do not relax worker deadlines.

For a local baseline using retained first-party bytes, the profiler alone accepts
`RACE_AUTHORITY_PACKED_COMPONENT` instead of a raw core. Both paths share the
same size/structure/import/engine checks. Qualification tests continue to use
their original core-loading path and budgets.
