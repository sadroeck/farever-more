# Add-on API compatibility and versioning

The **add-on API version** is the version of the `farever:addon` WIT package.
The WIT file defines it; the SDK, runtime, and manager use the same version,
and each packed add-on records the version it was built against. Wasmtime
checks these versioned interface names when connecting a component to the
host, so this number is part of the binary contract.

```text
MAJOR  breaking: components from another major are refused, never adapted
MINOR  additive: components built for an older minor keep loading
PATCH  fixes:    no contract change; compatible in both directions
```

## Why the numbering matters

Wasmtime matches imports by their versioned interface names. For API versions
1.0.0 and later, it can match different minor versions within the same major.
For 0.x versions, the minor is part of the compatibility track: a component
built for 0.14 cannot link to 0.15. The public contract starts at 1.0.0 so an
additive minor release can remain compatible with older components.

`farever_more_manifest::api::compatibility` therefore accepts exactly the pairs
the linker can satisfy:

| Required | Provided | Result |
| --- | --- | --- |
| `1.0.0` | `1.0.0` | loads |
| `1.0.0` | `1.4.2` | loads (older minor on a newer runtime) |
| `1.4.2` | `1.0.0` | refused: needs a newer runtime |
| `2.0.0` | `1.4.2` | refused: major versions make no guarantee |
| `0.14.0` | `0.15.0` | refused: a 0.x API is valid only within its minor |

If this check and the linker ever disagreed, an add-on would pass the gate and
then die inside Wasmtime with an unreadable instantiation error, so the rule is
deliberately the linker's own.

## Where it is enforced

* **Runtime load** (`farever-more-runtime/src/plugin.rs`): checks the manifest
  before compiling, then checks the component's own imports. A mismatch blocks
  activation and reports the required and available versions.
* **Manager install** (`farever-more-manager/src/archive.rs`): refuses an
  add-on archive that requires a newer API.
* **Manager list** (`farever-more-manager/src/model.rs`): marks incompatible
  add-ons already on disk and shows why they cannot load.

A unit that declares nothing (`api-version` absent) is a source tree or a
hand-built development unit; the runtime still judges it by the component's own
imports, and only the installer requires the field, exactly like `sha256`.

## The pack step

`build-wasm-addons.ps1` and `farever-addons.ps1` stamp `api-version` by reading
the `farever:addon/…@X.Y.Z` names out of the built component, so the manifest
cannot disagree with the bytes it ships beside. Hand-written manifests omit the
field, which is why it is only checked when present at load time.

## Bumping the API

1. **Additive change** (new interface, new field, new optional argument): bump
   the minor in `wit/farever-addon.wit`, run `scripts/sync-addon-sdk.ps1`, and
   update `ADDON_API_VERSION`. Existing components keep loading.
2. **Breaking change** (removed or changed type, renamed interface): bump the
   major, then regenerate and rebuild everything in the same pass
   (`scripts/build-wasm-addons.ps1` or `farever-addons.ps1 Update -Force`).
   Components from the previous major are refused with a message naming both
   versions.
3. **Fix** (documentation, no contract change): bump the patch in the WIT
   package. Neither direction is affected.

The SDK consistency script checks that the WIT package, SDK package version,
`WIT_PACKAGE`, and `ADDON_API_VERSION` all match. Both component build flows
run it before building.

### Release policy

The public add-on API starts at `1.0.0`. From this release onward, additive
changes use a minor bump, breaking changes use a major bump, and fixes that do
not change the contract use a patch bump.

## Other version numbers

The API version is separate from an add-on's release version in its manifest
and the runtime version in `runtime.json`. Services also use the provider's
add-on version; consumers declare the compatible major in `dependencies`.
