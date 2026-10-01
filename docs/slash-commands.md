# Native chat slash commands

## Status

Slash-command interception is implemented for the Steam builds listed in the
architecture guide. Its behavior in chat still needs live in-game validation.

The Wayfinder arrow subscribes to the `gps` topic. It accepts:

```text
/gps hide
/gps show
/gps next
/gps <x> <y>
/gps <x> <y> <name>
/gps <x> <y> "<name>", <x> <y> "<second name>"
```

`hide` and `show` only change visibility; they preserve the waypoint sequence.
Setting a new waypoint or sequence replaces it and automatically shows the arrow.
Comma-separated entries are visited in order. A name may contain spaces without
quotes; enclose it in double quotes when it contains a comma. Inside a quoted
name, `""` represents a literal double quote. When a name is omitted, the
displayed name is `Waypoint`.

Coordinates must be finite numbers. Because the command supplies only
horizontal coordinates, every waypoint in the sequence captures the player's
current Z value when the command is processed. `/gps next` discards the current
waypoint and advances to the following entry. Reaching a horizontal ground
distance strictly below 3 meters does the same automatically, including while
the surface is hidden. The surface disappears when the sequence is exhausted.
The sequence remains scoped to the current in-world session and is cleared
after leaving the world or starting a new process session.

The Dyno damage meter subscribes to the `dyno` topic. It accepts:

```text
/dyno clear
/dyno reset
/dyno show
/dyno hide
```

`clear` and `reset` are aliases. They clear the current encounter segment while
preserving the visibility choice. In an instance Overall view, they also remove
that current or most recent encounter's damage and duration from Overall.
`show` and `hide` update the same persisted visibility setting exposed in the
add-on configuration page; `show` does not create an empty meter before
encounter data exists. The former configuration-page reset button is
intentionally not exposed.

The earlier `/dps` topic remains available and accepts the same actions.

Slash-command interception defaults to enabled. The `Farever More` settings
page in the central configuration menu exposes an `Enable slash commands`
toggle. Changes take effect and persist immediately. When disabled, the detour
forwards slash-prefixed text to Farever's original chat handler instead of
suppressing or publishing it.

## Input contract

Farever More detours the build-verified
`ui.hud.ChatBox.processMessage(ChatBox, String) -> Void` method. Static
`hlboot.dat` inspection shows that the native chat submit callback clears the
input and then calls this method with the original string. The game's method
otherwise trims and parses the message before calling
`st.player.ChatClient.sendMessage`.

A message is a Farever More slash command only when its first UTF-16 code unit
is `/`. Leading whitespace is significant: ` /gps 1 2` remains ordinary chat.
Once a message is recognized as a slash command, it is never forwarded to the
game's original method, even if the command is malformed or the host queue is
full.

The command name starts at `/` and ends at the first literal ASCII space. Any
extra spaces before the payload are removed; the remaining text is
preserved and encoded as UTF-8 bytes on the existing message bus:

```text
/gps  1 2  -> topic "gps", payload bytes for "1 2"
/gps       -> topic "gps", empty payload
```

No quoting, escaping, case folding, or typed argument parsing is performed.

## Validation and delivery

Command topics use the existing message-bus rules: 1 through 128 ASCII bytes,
lowercase start, lowercase letters/digits plus `.`, `-`, `_`, `/`, or `@`, no
empty path segments, and an ASCII alphanumeric end. Invalid command topics are
silently suppressed from native chat and reported in the Farever More host log.
They cannot be delivered to an add-on because no valid bus topic exists for
them.

Valid commands go to every active add-on subscribed to that topic. The host
sends each command at most once and does not confirm receipt.
Host-originated commands carry the reserved
`source-addon-id` value `farever.host` and no correlation ID. Add-ons cannot use
that reserved ID.

A subscribing add-on owns payload parsing. It can use the local
[`chat` output capability](chat-output.md) to confirm an action or show a parse
error without sending a message to the server.

## Hook safety

The game-thread hook checks the chat value's type, copies at most 250 UTF-16
code units, and writes once to a fixed-size queue. It does not parse, log,
invoke Wasm, or render. The runtime thread decodes the text, checks the topic,
and sends the command to subscribers.
