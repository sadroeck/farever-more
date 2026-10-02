# Synchronous add-on services

## Status

Add-ons using `farever:addon@1.0.0` can make synchronous, read-only calls to a
declared dependency. `poi-database` (1.3.0) provides the `poi` service, which
the minimap opens during activation through `farever-poi-protocol`.

The provider serves 1,232 bundled W1 records: 1,224 imported placements and eight
reviewed fixed demon summoning sites from `farever_db::Inventory::soulstones()`.
The `soulstone` kind uses the required inventory item ID as its POI ID. The
existing string-ID wire format preserves this additive kind without a WIT or
wire-version change. The provider reads no game files at
runtime. The minimap caches nearby records and fetches more as the player moves.
Category checkboxes filter the cache without another request.

The shared protocol defines POI kinds and activity families. Consumers can
filter by kind and use the family to choose artwork or group activity records.
Unknown IDs are preserved, so newer records are not silently discarded.

GPS (0.2.0) also provides the read-only `waypoint` service. The minimap declares
GPS as optional with `^0.2` and opens a handle during activation. Its typed
`farever-waypoint-protocol` client queries the active destination each tick;
missing/failed queries remove the destination highlight or ordinary pin without
affecting the minimap or permanent POIs. Map Waypoints also requires the POI
provider and declares GPS optional; it fetches all demon sites on activation.
The GPS destination remains owned by GPS, so arrival, queue advancement, and
clearing are reflected without broadcasting a second state stream.
`farever-waypoint-protocol` is a shared Rust library compiled into both GPS and
the minimap, not a separately installed component. It owns their common data
type, encoder/decoder, and service client over the existing WIT byte transport.

Services are experimental and not yet a stable public API. Spatial queries use
a grid, so points near cell edges may fall just outside the requested bounds.
Nearest-first queries use 3D distance when both points have depth, and planar
distance otherwise.

## Manifest

Each add-on folder contains `addon.json` and `addon.wasm`. The provider
declares its ID and the services it offers. A service uses the provider's
version, so the manifest lists only the service ID:

```json
{
  "manifest-version": 1,
  "id": "poi-database",
  "version": "1.3.0",
  "provides": ["poi"]
}
```

The consumer names the provider by its add-on ID and lists the service versions
it accepts:

```json
{
  "manifest-version": 1,
  "id": "minimap",
  "version": "0.1.0",
  "dependencies": [
    {
      "addon": "poi-database",
      "services": [
        { "id": "poi", "version": "^1" }
      ]
    }
  ]
}
```

Dependencies are required by default. If a required provider is unavailable,
the consumer fails activation; the minimap requires `poi-database`. Mark a
dependency `"optional": true` when the consumer can continue without it. The
host reports load failures in local chat and in the manager.

During initial loading, providers activate before the add-ons that depend on
them. Unrelated add-ons load in ID and path order. Version matching currently
checks the major version only; full semantic-version requirements are planned.

The manifest currently sits beside the component. The planned release format
will embed it in the published component so the code and its dependency data
always update together.

## WIT and SDK

The `dependencies` import opens a service on a declared provider and returns a
handle for that consumer instance. Calling the handle runs the provider's
`plugin.call-service` export immediately, before the consumer continues.

WIT passes an operation number and raw bytes. A protocol crate handles the
encoding, so the minimap can query POIs through `PoiClient` and `PoiKind`.

Requests and responses are limited to 256 KiB. An instance can open 32 handles
and make 64 calls per callback. Calls use the provider's normal callback fuel
budget. A busy, missing, trapped, or rejecting provider returns an error.

The POI protocol encodes values in little-endian order and strings as UTF-8. It
does not depend on Rust's in-memory layout and rejects malformed, oversized,
or unsupported data.

The waypoint service uses operation `1` with an empty request. Its response is
`[version=2, present=0]` for no target, or `[2, 1]` followed by a little-endian
`u64` process session, three finite `f32` XYZ values, world ID, target name,
and optional inventory item ID (an empty string means no item portrait).
Each string has a little-endian `u16` UTF-8 byte count bounded to 512. The codec
rejects truncation, unknown versions, nonfinite coordinates, invalid UTF-8,
oversized strings, and trailing data. Other operations or nonempty requests
are rejected. A target without a known area is not exposed. Consumers match
the world and process session before drawing; the minimap also suppresses the
pin inside the GPS horizontal arrival radius, even between GPS ticks.
The decoder also accepts version 1 responses, which have no item ID and use
the generic destination marker. Known soulstone IDs select the gold outline on
one of eight permanent POIs, using the same inventory portrait and diamond
frame on both maps. The selected marker also has an enlarged portrait and a
contrasting gold circular halo. Map-icon clicks select the same site identity
as inventory clicks. Unknown IDs and regular waypoints use the minimap ring.

## What is not supported yet

Currently, a consumer can call a provider during activation, including one
nested service call. If a required provider is still compiling, the consumer
waits and retries activation. The host does not yet recover from every provider
change:

- dependents are not disabled when a provider is removed or fails to reload;
- reloading a provider does not restart its dependents;
- manifest changes are not watched independently from component publication;
- the manager has no separate status for dependency cycles or missing providers;
- the host does not yet block every unsupported service-call phase, even though
  service calls are documented as read-only.
- a consumer whose required provider is definitively absent is rejected at
  activation with a player-visible chat error naming the missing add-on;
  re-querying after a late provider install remains planned work.

## Live QA

POI extraction and rendering still need more in-game QA.
