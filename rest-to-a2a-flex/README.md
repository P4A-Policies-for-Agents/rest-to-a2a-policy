# "rest-to-a2a" Policy

This is the policy-implementation half of the **REST to A2A Bridge** policy. It compiles to a `wasm32-wasip1` module loaded by Omni Gateway at runtime.

The policy was created with the Omni Gateway Policy Development Kit (PDK). For the complete PDK documentation, see [PDK Overview](https://docs.mulesoft.com/pdk/latest/policies-pdk-overview).

For the A2A v1.0 protocol coverage map and limitations (including why streaming is unsupported), see [`docs/spec.md`](docs/spec.md).

## Make command reference

This project has a Makefile with the goals used during the policy development lifecycle.

### Setup
`make setup` installs the PDK build dependencies (`cargo-anypoint`).

### Build asset files
`make build-asset-files` fetches the latest definition from Exchange and verifies the config struct is present.

> **Note:** `src/generated/config.rs` is hand-maintained for this policy because the GCL schema declares DataWeave selectors and a nested `a2aConfiguration` object that the current `cargo anypoint config-gen` cannot prettify. The hand-rolled struct is checked in. The `build-asset-files` target only fetches the definition and verifies presence — it does not regenerate the file. Run `cargo anypoint config-gen` manually only when adding top-level scalar properties; do **not** regenerate over the nested object.

### Build
`make build` compiles the WebAssembly binary. Run `make build-asset-files` at least once before compiling so the source stays in sync with the definition.

### Run
`make run` executes the current build in a Docker containerized Omni Gateway. The `playground/config` directory must contain:

- A `registration.yaml` produced by an Omni Gateway registration in Local Mode (gitignored). Generate one via Runtime Manager → Omni Gateway → Add Gateway → Docker, replacing `--connected=true` with `--connected=false`, and run it in `playground/config`.
- An `api.yaml` with the desired policy configuration. The repo ships an example using the `jsonrpc` binding and a header-based conversation cache key.

### Test
`make test` runs unit + integration tests. Integration tests require a `tests/config/registration.yaml` produced via `anypoint-cli-v4 registration create-local` (gitignored).

Convenience targets:
- `make test-unit` — unit tests only (no Docker required).
- `make test-one TEST=<name>` — a single test by name.

### Publish / Release
`make publish` publishes a development (`-DEV`) asset; `make release` publishes a production asset. Both require the matching `definition_asset_id.version` in `Cargo.toml`.

*For more information, see [Uploading Custom Policies to Exchange](https://docs.mulesoft.com/pdk/latest/policies-pdk-publish-policies).*
