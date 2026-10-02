# Releases and CI

GitHub Actions validates the Windows x86-64 build on pushes to `main` and on
pull requests targeting `main`. The check covers both Rust workspaces, the
WIT/SDK synchronization and facade boundary, formatting, tests, strict Clippy,
the native framework release build, reference component packaging, and the
ignored runtime host-boundary smoke tests.
The standalone manager is under development and excluded from CI tests,
Clippy, native builds, and release packages.

## Release assets

A tag such as `v0.2.3` starts the Windows release workflow. The tag version
must match the framework runtime version in `farever-more-host/Cargo.toml`.
The workflow publishes:

- `farever-more-framework-v<version>-windows-x86_64.zip`, containing the
  native proxy and host binaries, the direct-install runtime layout,
  and the framework notices;
- `farever-more-framework-v<version>-windows-x86_64-full.zip`, containing that
  framework plus Dyno, GPS, Minimap, Map Waypoints, and POI Database placed under
  `farever-addons/addons/<id>/`, including their manifests and notices;
- one `farever-more-addon-<id>-v<addon-version>.zip` per maintained reference
  add-on, containing `addon.wasm`, its stamped `addon.json`, and any add-on
  notices or licenses;
- `release-manifest.json`, which records the framework and add-on versions,
  API versions, archive names, and archive SHA-256 hashes. Its `full` entry
  records the bundled archive's framework version, filename, and checksum;
  the bundled add-on versions are listed in `addons`.

The full bundle is the default installation: extract its contents beside
`Farever.exe` with the game closed. The separate framework and add-on ZIPs
support selecting individual add-ons.

The framework release version and add-on release versions are independent of
the WIT add-on API version. This checkout implements API `1.1.0` (unreleased); the framework
release is `0.2.3`, while each add-on keeps the version in its source
`addon.json`.

To reproduce the release package locally on Windows:

```powershell
.\scripts\package-release.ps1 -Version 0.2.3 -OutputRoot dist\release-local
```

The output directory must not already exist. The command builds the native
proxy and host, builds and componentizes all maintained add-ons, and stamps
each packed manifest from the component bytes.

The pack step also writes `release-notes.md` from the tagged version's
`CHANGELOG.md` entry and the root `README.md` installation section. The release
workflow uses it as the GitHub release body, so every release shows the manual
installation steps, exact add-on folder layout, and Minimap's POI Database
dependency. The framework archive includes that same README.
