# Documentation

These documents describe the current code and contracts. Read the owner document before changing the behavior it covers.

| Document | Covers |
| --- | --- |
| [architecture.md](architecture.md) | How the game loads add-ons, reads game state, and draws overlays |
| [sdk.md](sdk.md) | Rust authoring SDK, manifests, and component build workflow |
| [api-versioning.md](api-versioning.md) | How add-on API versions work |
| [lifecycle.md](lifecycle.md) | When add-ons start, run, and stop |
| [hot-reload.md](hot-reload.md) | How code changes are loaded while the game is running |
| [message-bus.md](message-bus.md) | How add-ons send messages to one another |
| [services.md](services.md) | How an add-on uses a service provided by another add-on |
| [config-ui.md](config-ui.md) | How to build settings pages and handle their controls |
| [chat-output.md](chat-output.md) | Local chat output |
| [slash-commands.md](slash-commands.md) | How in-game slash commands reach add-ons |
| [api-inspector.md](api-inspector.md) | How to inspect Farever's internal API and compare game versions |
| [minimap.md](minimap.md) | Minimap behavior and points of interest |
| [gps.md](gps.md) | The GPS waypoint-arrow add-on |
| [dyno.md](dyno.md) | The damage-meter add-on |

The executable contract is [../wit/farever-addon.wit](../wit/farever-addon.wit). Its package version is the add-on API version. See [api-versioning.md](api-versioning.md) for the version rules.

For release readiness, current priorities, and known limitations, see [../ROADMAP.md](../ROADMAP.md). Build and install commands are in the root [README](../README.md).
