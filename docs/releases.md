# Releases and CI

GitHub Actions validates the Windows x86-64 build on pushes to `main` and on
pull requests targeting `main`. The check covers both Rust workspaces, the
WIT/SDK synchronization and facade boundary, formatting, tests, strict Clippy,
the native framework release build, reference component packaging, and the
ignored runtime host-boundary smoke tests.

## Release assets

A tag such as `v0.2.0` starts the Windows release workflow. The tag version
must match the framework runtime version in `farever-more-host/Cargo.toml`.
The workflow publishes:

- `farever-more-framework-v<version>-windows-x86_64.zip`, containing the
  manager, native proxy and host binaries, the direct-install runtime layout,
  and the framework notices;
- one `farever-more-addon-<id>-v<addon-version>.zip` per maintained reference
  add-on, containing `addon.wasm`, its stamped `addon.json`, and any add-on
  notices or licenses;
- `release-manifest.json`, which records the framework and add-on versions,
  API versions, archive names, and archive SHA-256 hashes.

The framework release version and add-on release versions are independent of
the WIT add-on API version. The current public API is `1.0.0`; the framework
release is `0.2.0`, while each add-on keeps the version in its source
`addon.json`.

To reproduce the release package locally on Windows:

```powershell
.\scripts\package-release.ps1 -Version 0.2.0 -OutputRoot dist\release-local
```

The output directory must not already exist. The command builds the native
manager, proxy, and host, builds and componentizes all maintained add-ons, and
stamps each packed manifest from the component bytes.
