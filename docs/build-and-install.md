# Build and install

These instructions are for Windows x86-64 and the Steam version of Farever.

## Requirements

- Stable Rust with the `x86_64-pc-windows-msvc` target.
- Visual Studio Build Tools with the Desktop development with C++ workload.
- The `wasm32-unknown-unknown` Rust target to build the reference add-ons.

## Before you install

The native host runs inside Farever and uses in-process hooks to read game
state. A game update can change the internals those hooks rely on, and a bug
could crash or destabilize the game. Check the compatibility notes in
[architecture.md](architecture.md) and try it on an installation you can
repair. An offline build or test does not show whether the host works in-game.

Add-ons run as Wasmtime WebAssembly components and do not receive native game
pointers. Filesystem and network access through WASI are disabled by default.
The host itself is trusted native code, so install only add-ons you trust.

## Build the reference add-ons

From the repository root, install the WebAssembly target and build the add-ons:

~~~powershell
rustup target add wasm32-unknown-unknown
.\scripts\build-wasm-addons.ps1
~~~

## Install into Farever

Close Farever before installing or updating native files. The installer builds
and installs the proxy, host, and reference add-ons:

~~~powershell
.\scripts\farever-addons.ps1 Install
~~~

The installer uses Farever's `dinput8.dll` proxy slot, which can conflict with
another mod using the same slot. It tracks the files it owns and refuses to
replace files it does not own. See the script's other commands and options with:

~~~powershell
Get-Help .\scripts\farever-addons.ps1 -Full
~~~
