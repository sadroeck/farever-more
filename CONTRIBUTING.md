# Contributing

Thanks for considering a contribution. Farever More is still early in
development, and its API and install process may change. For a larger feature
or API change, open an issue first so we can agree on the approach.

## Set up

Development currently requires Windows x86-64 and the Steam version of
Farever. Linux and Proton are not supported. Install stable Rust with the MSVC
target and Visual Studio Build Tools with the Desktop development with C++
workload.

The framework and reference add-ons are separate Rust workspaces, each with
its own lockfile. Run framework commands from the repository root and add-on
commands with the add-ons manifest.

## Build and check changes

From the repository root, run the usual checks. Root test and Clippy commands
exclude the vendored `hlbc` crates because their upstream fixtures are not
included here.

~~~powershell
cargo fmt --all --check
cargo test --workspace --exclude hlbc --exclude hlbc-derive
cargo clippy --workspace --all-targets --exclude hlbc --exclude hlbc-derive
.\scripts\sync-addon-sdk.ps1 -Check
.\scripts\check-addon-sdk-boundary.ps1
.\scripts\build-wasm-addons.ps1
cargo fmt --manifest-path .\addons\Cargo.toml --all --check
cargo test --manifest-path .\addons\Cargo.toml --workspace --all-targets
~~~

Optional:

~~~powershell
$env:FAREVER_ADDON_SMOKE_DIR = (Resolve-Path "target\addon-dev\addons").Path
cargo test -p farever-more-runtime -- --ignored
~~~

These checks run offline. If a change depends on Farever internals, include
the game build you checked and say whether you tried it in a live game. A
successful build or test does not establish live compatibility. Close Farever
before installing or updating native files.

## Keep docs in sync

Update the owning document in `docs/` whenever a change alters a documented
API, behavior, or workflow. The [documentation index](docs/README.md) points
to each reference. The [roadmap](ROADMAP.md) lists planned work, not current
behavior.

When changing the WIT contract, bump its version and regenerate the SDK
bindings with `.\scripts\sync-addon-sdk.ps1`. Then run
`.\scripts\check-addon-sdk-boundary.ps1`.

## Source and assets

Farever More source code is licensed under MIT. Farever-derived data and
artwork, fonts, and other third-party material have separate attribution and
license notices. Keep those notices, and record the source and license for
any new asset.

Packages that bundle third-party assets or Farever-derived data are marked as
unavailable for crates.io until their contents and license metadata are
reviewed. Check the adjacent asset notes and third-party notices before
removing that restriction.
