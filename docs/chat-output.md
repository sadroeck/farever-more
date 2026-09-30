# Local chat output

## Add-on API

`farever:addon@1.0.0` imports a `chat` interface with two functions:

```wit
print: func(text: string);
print-error: func(text: string);
```

`print` adds an ordinary local line to Farever's chat window. `print-error`
uses the game's existing chat-error presentation. Neither function sends a
chat packet or makes the line visible to another player.

Add-on output identifies the add-on that called `print` or `print-error`,
including a reply to a host-originated slash command. The host
uses its manifest display name, falling back to the component name when the
manifest has no name. It bounds and sanitizes that name before queueing output;
the guest cannot provide a different sender for an individual line. Framework
diagnostics continue to use `Farever-More`.

Calls made during `on-ui-event`, `on-event`, `on-message`, or `on-tick` are
staged with the rest of that callback's effects. A successful callback commits
them in call order. A trap, returned error, or invalid callback output discards
them. Chat output is currently ignored during activation and deactivation.

The host accepts at most eight lines and 1,000 UTF-16 code units from one
callback. A line is truncated at a Unicode character boundary to 250 UTF-16
code units. The native queue holds 128 lines and the game-thread dispatcher
processes at most four per frame. A full queue drops new lines and records the
drop in the host log.

## Game integration

The runtime supports Steam builds `25257040`, `25628371`, and beta `25531577`. It
checks their game-file hashes and selects the matching chat format. Other
builds are unsupported.

The dispatcher detours the validated
`ui.hud.ChatBox.hasFocus(ChatBox) -> Bool` method. Farever calls it from the
chat box's frame update, which supplies a live `ChatBox` receiver on the UI
thread. The detour first calls the original method, then processes queued
lines. Error lines call
`ui.hud.ChatBox.chatError(ChatBox, String) -> Void`. Normal lines invoke
`ui.hud.ChatBox.receiveMessage(ChatBox, message) -> Void`; they do not call
`st.player.ChatClient.sendMessage`.

The two supported builds use different `receiveMessage` message shapes:

| Profile | `sender` | `st.Channel.Local` |
| --- | --- | --- |
| Steam `25257040` | `ent.Unit` | `Local()` |
| Beta `25531577` | virtual `{ name, uid }` | `Local(position: { x, y, z })` |

Both shapes are checked against live HashLink metadata before enabling chat
output. The built-in String type may be unnamed in bytecode; the host accepts it
only as an HOBJ with exactly `bytes: Bytes` and `length: I32`, then validates the
runtime field offsets before using it. For beta, the host creates the nested
sender virtual with the queued add-on name (or `Farever-More` for framework
output) and an empty UID, and uses the latest validated player position for
`Local`. Supplying non-null strings prevents the game from displaying a `null`
sender name. The stable build's `sender: ent.Unit` cannot represent an add-on
name, so the host prefixes normal text with `Add-on Name: `. Both builds prefix
`print-error` text in the same way because `chatError` accepts text but no
sender. If no position is currently available, it sends zero coordinates. It
reads the `Local` parameter type and offset from the enum metadata before
constructing the payload. Unused message fields still receive registered null
backing slots; they are not omitted from the HashLink virtual mapping.

The host also validates the exact names and signatures of `hasFocus`,
`chatError`, and `receiveMessage`; the `String` fields used to construct text;
all seven message fields; and the required HashLink allocator, conversion, and
numeric-field exports. A mismatch leaves chat output disabled and writes a
diagnostic. This is a shape check, not a string-only lookup.

The chat hook runs on the game thread and calls Farever's chat methods to add
the queued lines. It does not invoke Wasm, inspect metadata, or write logs.
This hook is available on Windows x86-64 only.

## Live QA

The beta profile still needs more in-game QA.
