# Roadmap

This is a working plan, not a release schedule. Priorities and dates are not
guaranteed.

## Current focus

- Publish a compatibility record for specific Farever builds.
- Keep source attribution beside game-derived maps, icons, and embedded data
  as those files change. The project source license does not cover them.
- Explain which parts of the add-on interface are stable and which may change.
- Review install, update, recovery, and uninstall flows before recommending the
  host for general use.

## Next

- Handle missing providers, dependency cycles, and provider reloads without
  leaving dependent add-ons stuck.
- Add repeatable checks for each supported game build, including a check that
  the host can load add-ons.
- Make it easier to write add-ons and explain how the included add-ons are
  built and installed.
- Add more game data only when its source is clear and its format can be
  checked.
- Check the data format against every supported Farever build before
  regenerating the tables.

## Later

- Consider publishing packages with bundled assets on crates.io after their
  license details are settled.
- Consider ways to find and distribute add-ons after deciding how they will be
  trusted and updated.
- Expose more game state and events after confirming what they mean and hearing
  what add-on authors need.
- Consider persistent storage and richer interactions after the add-on
  lifecycle is more reliable.

## Current limitations

- The native host is Windows x86-64 only and tied to Farever internals.
- The project does not promise compatibility with every game build.
- Dependency services and some reference add-ons remain under development.
- The bundled point-of-interest provider currently covers the W1 dataset.
- Public binary distribution, signing, and support policy need a separate
  release decision.
