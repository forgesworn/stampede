# Build the authority from public source

The isolated guest manifest compiles the existing authority source directly.
It needs only public Stampede and Fray checkouts in their conventional layout:

```text
sources/
  Stampede/
  forgesworn/fray/
```

The native application workspace has additional dependencies. These manifests
do not require a Twang or Signet checkout and do not copy or replace game rules.
Keep the isolated guest package version aligned with the main workspace.

From the Stampede root, using Rust 1.99.1 and a configured `CARGO_TARGET_DIR`:

```sh
rustup target add --toolchain 1.99.1 wasm32-unknown-unknown
cargo +1.99.1 build --locked --release --target wasm32-unknown-unknown \
  --manifest-path qualification/guest/Cargo.toml
cargo +1.99.1 run --locked --manifest-path qualification/component-pack/Cargo.toml \
  -- "$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/race_authority.wasm" \
  "$STAMPEDE_COMPONENT_OUT"
```

Set `STAMPEDE_COMPONENT_OUT` to a file outside the checkout. For a distributable artifact, use the same build through the path-remapping
wrapper instead of the two Cargo commands:

```sh
python3 qualification/build-component.py "$STAMPEDE_COMPONENT_OUT"
```

The wrapper pins the compiler, records artifact digests and refuses an artifact
that retains the selected local source/cache paths. It changes no game code or
WASM feature. The packer bounds input/output, requires an import-free core
and validates component encoding.
The runtime still enforces the authority world, capabilities and resource profile.
Building a component does not qualify a tier, platform, recovery or production
deployment. Retain exact source revisions, toolchain, artifact digests and
comparison reports when running acceptance.
