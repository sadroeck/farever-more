# Add-on message bus

## Status

Add-ons can send messages to one another through an in-process bus. These
messages are separate from events the host reads from Farever: add-ons define
their own topics and payloads, so receiving code must treat them as untrusted.

The executable contract is [`../wit/farever-addon.wit`](../wit/farever-addon.wit).

## Contract

The world imports a dedicated `bus` interface:

```wit
subscribe: func(topic: string) -> result<_, message-error>;
publish: func(
    topic: string,
    target: message-target,
    correlation-id: option<u64>,
    payload: list<u8>,
) -> result<u32, message-error>;
```

Subscriptions are accepted only while `plugin.activate` is running. They match
exact topics; the current bus has no wildcard or prefix subscriptions. Duplicate
subscriptions from one add-on are idempotent.

Publishing is accepted only while `plugin.on-event`, `plugin.on-message`, or
`plugin.on-tick` is running. The returned number is the count of active
recipients matched when the message is staged. It is not a delivery or handling
acknowledgement.

`message-target.subscribers` broadcasts to every active exact subscriber except
the publisher. `message-target.addon(id)` selects one add-on, but only if that
add-on subscribed to the topic. An explicit direct message to the publisher
itself is allowed.

Recipients export:

```wit
on-message: func(
    dropped-before: u64,
    messages: list<addon-message>,
) -> result<callback-output, string>;
```

The host stamps every delivered message with a process-local monotonic message
ID, monotonic time, and the source add-on ID. A publisher cannot provide or
spoof those fields. `correlation-id` is chosen by the publisher and scoped to
that source ID; a reply copies it and directly targets the original
`source-addon-id`. Host-originated native-chat slash commands reuse this path
with the reserved source ID `farever.host`, no correlation ID, and broadcast
targeting. That pseudo-source cannot receive direct replies.

Native full-map clicks use the same reserved source on `farever.map-click@1`.
The payload is UTF-8 text containing one finite horizontal world-coordinate
pair, `x y`, with no correlation ID. The host captures the game's converted
coordinates from `MapWindow.onClickWorld`; on beta `25531577` and stable
`25628371`, activity-marker clicks are captured from `popupActivityMenu` before its native HUD selector
opens. The 64-record native queue holds copied coordinates only. Delivery is
discarded outside the world, and no click is retained for a future subscriber.
GPS accepts this topic only from `farever.host` and applies its "Arrows from
the map" preference to these observations. Explicit host `/gps` commands on
`gps` bypass that preference.

## Topic and payload rules

Topics contain 1 through 128 ASCII bytes. They start with a lowercase letter,
end with an ASCII letter or digit, and otherwise use lowercase letters, digits,
`.`, `-`, `_`, `/`, or `@`. Empty path segments are rejected. A shared protocol
should include its version in the topic, for example:

```text
farever.wayfinding/set-target@1
```

The host never decodes `payload`. The topic's protocol specification owns the
encoding and validation rules. Shared protocols should use a language-neutral
encoding rather than a Rust-specific memory or serialization format.

## Scheduling and delivery

The host runs one component's callbacks in order. When several kinds of work
are ready, it calls:

1. `on-event`
2. `on-message`
3. `on-tick`

Messages are staged until the publishing callback succeeds and become
available on the next runtime iteration. This prevents synchronous re-entry,
recursive request/reply chains, and behavior that depends on component
discovery order. Recipients see accepted messages in host message-ID order.

Delivery is at most once. If the publishing callback fails, its staged
messages are discarded; if a recipient callback fails, delivery is not retried.
Messages are never saved. An add-on that needs current state after startup or
loss should request it through its topic protocol.

## Bounds and loss

The initial host policy is:

- at most 64 exact subscriptions per add-on;
- at most 64 KiB per payload;
- at most 32 publications and 256 KiB of payload per callback;
- at most 256 pending messages and 1 MiB of payload per recipient.

Invalid publication calls return a typed `message-error`. If a recipient queue
is already full after a publication was staged, the host omits that delivery and
increments the recipient's next `dropped-before` value. A non-zero value means
the recipient must use its topic protocol's resynchronization behavior.

## Add-on IDs

Messages identify add-ons with the `addon-id` in `activation-context`. The
current loader derives it from the component filename and allows only one
active component with that ID. Host messages use the reserved source ID
`farever.host`, which cannot receive direct messages. The loader is planned to
use stable IDs from the manifest in a future release.

The bus does not keep messages private. Any installed add-on can subscribe to
a topic, so never publish credentials or other secrets. Per-add-on message
permissions are planned work.
