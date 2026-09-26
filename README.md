# Farever More

An add-on framework for the Steam version of
[Farever](https://store.steampowered.com/app/3672400/Farever/). Add-ons are
written in Rust and built as WebAssembly components. The native Windows host
loads them, reads selected game state, and draws their overlays.

Farever More is still in early development, so the add-on API and installer may
change. It currently supports Windows x86-64. Linux and Proton are not
supported yet; Linux support is planned for a future release.

## What you can build

Farever More includes a Rust SDK and build tools for add-on authors, a desktop
manager, and an installer. It also comes with reference add-ons for damage
tracking, GPS waypoints, the minimap, and points of interest. The repository
includes an item-search tool, a Farever API inspector, and game data used by
the add-ons.

## Before you install

The native host runs inside Farever and uses in-process hooks to read game
state. An update can change the internals those hooks rely on, and a bug could
crash or destabilize the game. Check the compatibility notes and try it on an
installation you can repair. An offline build or test does not show whether the
host works in-game.

Add-ons run as Wasmtime WebAssembly components and do not receive native game
pointers. Filesystem and network access through WASI are disabled by default.
The host itself is trusted native code, so install only add-ons you trust.

## Build and install

### What you need

- Windows x86-64 and the Steam version of Farever.
- Stable Rust with the `x86_64-pc-windows-msvc` target.
- Visual Studio Build Tools with the Desktop development with C++ workload.
- The `wasm32-unknown-unknown` Rust target for the reference add-ons.

### Build the reference add-ons

To build the reference add-ons from the repository root:

~~~powershell
rustup target add wasm32-unknown-unknown
.\scripts\build-wasm-addons.ps1
~~~

### Install into Farever

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

Contributor checks are listed in [CONTRIBUTING.md](CONTRIBUTING.md). The
[architecture guide](docs/architecture.md) lists supported game builds and
where more in-game QA is needed. The
[framework documentation index](docs/README.md) covers the framework. Guides
for the [damage meter](addons/dyno/README.md), [GPS](addons/gps/README.md), and
[minimap](addons/minimap/README.md) live alongside their source.

## Release history

See [CHANGELOG.md](CHANGELOG.md) for release history.

## Credits

Farever More's minimap was inspired by ramisotti13-eng's [Farever Minimap &
DPS project](https://github.com/ramisotti13-eng/farever-minimap). Some map and
marker assets come from its [v1.2.7
release](https://github.com/ramisotti13-eng/farever-minimap/releases/tag/v1.2.7).
The [minimap asset note](addons/minimap/assets/README.md) attributes the
Farever-derived artwork.

Farever More is an unofficial community project, not affiliated with or
endorsed by the Farever developers. Farever names, artwork, maps, and other
game-derived content remain subject to their respective rights holders.

## License

The project source is licensed under MIT; see [LICENSE-MIT](LICENSE-MIT). That
license does not cover Farever-derived artwork or data. See [the third-party
notices](THIRD_PARTY_NOTICES.md) for other licenses and attributions.

## Support

<a href="https://www.buymeacoffee.com/colorise" target="_blank" rel="noopener noreferrer">
  <img src="https://cdn.buymeacoffee.com/buttons/v2/default-blue.png" alt="Buy Me a Coffee" style="height: 60px !important; width: 217px !important;">
</a>
