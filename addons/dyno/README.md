# Reference damage meter

## Status and intent

This reference add-on consumes Farever More's combat-event and visualization
APIs. Its current implementation measures only the local player and demonstrates
encounter boundaries, damage aggregation, responsive tables, and meter-style
rows.

The design borrows established behavior rather than source code from:

- [Skada](https://github.com/zarnivoop/skada), especially its descending
  per-skill view, damage-share percentages, and bars scaled to the largest
  visible contributor;
- [farever-minimap](https://github.com/ramisotti13-eng/farever-minimap),
  especially its built-in DPS meter's local-player-only coverage, game-owned
  combat state, combat indicator, and responsive column priorities.

Both projects are behavior references only. Farever More copies no source code
or assets from either one for the damage meter.

## Included features

- Tracks one current-player encounter and groups outgoing damage by skill or
  damage type. The host uses the validated local damage hook when available,
  with the floating-damage decoder as a fallback.
- Shows each skill's name, icon, cumulative damage, and share of the encounter
  in a compact table. Bars compare each skill with the largest contributor.
- Uses combat events from the host and references to skill icons from the
  current game build.
- Places the surface above Farever's action and utility bars and hides it while
  a registered game window is open.
- Warns when event delivery is discontinuous.

The component also registers a `settings` page in the host Add-ons menu from
activation onward. Its persisted `Enable damage meter` checkbox updates component
state and replaces the retained frame immediately. The host supplies the shell
and navigation; the component supplies the declarative page and callback
behavior. There is no Save button.

Native chat commands `/dyno clear` and `/dyno reset` clear the current encounter
segment. `/dyno show` and `/dyno hide` update the persisted visibility setting.
The older `/dps` command name remains an alias. See the [slash-command
reference](../../docs/slash-commands.md) for the reset behavior.

Party ranking, additional damage event types, encounter history, and richer
controls remain planned.

The add-on emits no surface before the first encounter starts. An active
encounter with no observed damage shows only the title strip and an empty skill
table; it does not synthesize a waiting or empty-state row. The completed
encounter remains visible until the next combat session starts. Encounter
history is not persisted, while the visibility preference is persisted across
component and host restarts.

## Visibility policy

The meter submits an empty frame whenever the host's
`windows.current().value.open-windows` registry contains an entry. Normal
HUD-only play has an empty registry. Blocking every registered window also
covers future modal windows without a meter-specific allowlist. UI open and
close events invalidate the frame immediately, even after combat when the
meter's tick is dormant. If UI state is unavailable, the add-on keeps the
meter visible.

Town suppression is intentionally not inferred from
`zone.current().value.area-id`:
live observations use `World/W1_Siagarta` for both the town/rest area and the
wider overworld. The inspected game bytecode instead defines
`ent.Hero.isInRestArea()`, which resolves the zone at the hero's position and
tests its `RestArea` flag. A validated read-only `player.in-rest-area` API is
not part of this add-on.

## Damage calculation contract

`damage-event.amount` is the observed damage-done value. It is accumulated
without adding `damage-event.blocked`:

```text
encounter damage = sum(valid positive amount)
skill damage     = sum(valid positive amount for that add-on aggregation key)
encounter DPS    = encounter damage / max(1 second, encounter duration)
skill share      = skill damage / encounter damage
critical percent = critical displays / hit displays
maximum hit      = max(amount for that skill)
```

The current direct provider publishes one `damage-event` per validated
`ent.Unit.onInflictDamage` callback and sets `hit-count = 1`. Farever's internal
`_hitCount` is a running ordinal, not the number of hits represented by the
callback. Critical and maximum-hit statistics are consequently event-level
statistics. Without `use_polling.flag`, direct installation or sustained record
validation failure automatically selects the earlier one-event-per-
`DamageDisplay` decoder. With the flag present, that polling decoder is selected
even when the installed hook is healthy.

`blocked = none` means the provider could not observe the value; `blocked =
some(0)` means it observed zero. The add-on retains valid block observations
separately but does not display or add them to damage done. Farever's `_amount`
has not been verified as before or after `_block`, so the meter does not report
mitigated or attempted damage.

Overkill is not available in the current event contract and no adjustment is
made. This matches the Skada-style rule of treating the normalized damage
amount as the meter total rather than silently inventing a second damage model.

## Encounter lifecycle

The host, not the add-on, owns fight identity. A `fight-id` is monotonic within
one `process-session`; the corresponding `combat-started` and `combat-ended`
events carry the same ID.

The host starts or ends a fight when it observes the local Hero's
`set_isInCombat` flag change. If that signal is unavailable, the first damage
event can start the encounter. When using sampled state, one false reading
doesn't end a fight: the host waits for combat references to clear and for five
seconds without damage. Any new damage, a true reading, or an occupied
reference cancels that wait. If references are unavailable or stay occupied,
the host ends the fight after 60 seconds without another hit. Leaving the world
ends an active fight immediately.

Encounter duration runs from combat start to combat end, including time spent
moving or doing no damage while Farever still considers the player in combat.

## Skill-list visualization

Rows sort by descending skill damage, with the stable normalized `skill-id` as
the tie-breaker. The host resolves `skill-display-name` from the current
build's packaged `skill.texts.name` catalog when authored metadata exists; the
add-on prefers that public name and derives a readable label only when it is
absent. Exact CastleDB name references such as `[OtherSkill]` are resolved by
the host rather than exposed as UI copy.

The host also resolves the skill record's `gfx` metadata into an opaque image
reference. CastleDB's omitted/non-positive tile size defaults to 96 pixels and
its omitted/non-positive width and height default to one cell, matching the
game's nullable tile convention. The host lazily reads and bounds-checks the
referenced PNG atlas from the user's current Farever installation, crops the
declared cell, and owns the decoded resource. Neither the archive path, atlas
coordinates, PNG/RGBA bytes, nor renderer texture identifiers cross into Wasm.
Missing or invalid metadata produces `skill-icon = none`; the add-on leaves the
aligned icon cell empty. The lookup joins packaged metadata by raw skill ID
rather than following the live skill instance.

Weapon attack stages such as `GS_Base_Attack`, `GS_Base_Attack2`, and
`GS_Base_Attack3` intentionally aggregate into one add-on-owned `BaseAttack`
bucket displayed as `Attack`. Combo skills retain their distinct raw ID
and use their authored name when available—for example, `GS_Nova_Combo` is
displayed as `Mania` in the currently profiled data.

The numerical percentage is the skill's share of total encounter damage. The
bar fraction is instead relative to the largest skill in the encounter:

```text
bar fraction = skill damage / largest skill damage
```

The largest skill therefore always has a full bar, matching Skada's comparison
behavior. The generic row-progress primitive begins the track at the skill
column, leaving the fixed icon gutter outside the bar with no intervening seam.
It paints the
fraction continuously behind the name and combined `damage (share%)` value. The
remaining portion of each row retains an explicit translucent background.
Adjacent body rows form the vertically stacked meter without a table header,
ranking numbers, column separators, or zebra striping.

Columns are declared once through the table API:

| Column | Alignment | Width | Content padding | Visible from |
| --- | --- | ---: | ---: | ---: |
| Skill icon | center | 21 | 0 | always |
| Skill | left | remainder | 6 | always |
| Damage (share%) | right | 150 | 6 | always |

All rows use the same column declaration, while the remainder column absorbs
available width. The surface width is clamped to the client viewport. Content
padding insets the text without changing the bar or its maximum-relative
width.

The surface uses a compact 440-point preferred width and a custom translucent
warm parchment frame. Cocoa text, muted class-color bars, and subtle alternating
row tones align it with Farever's internal UI without changing the meter's
icon/name/value structure. Its native host title bar is hidden in favor of an in-content
encounter strip resembling Skada's `[time] Damage: Current fight` title. The
add-on anchors the panel to `bottom-right` with a 24-point right margin and a
190-point bottom safe offset. That provisional offset is intended to clear
Farever's bottom HUD at the 1080p reference scale and scales with the host's
resolution-aware UI; live review decides whether it becomes the default.
All visible meter copy requests opaque cocoa ink explicitly. The add-on embeds and
registers Noto Sans Regular for body/small text and Noto Sans Bold for
strong/heading text during initialization. The host renders those registered
semantic faces; neither side extracts Farever's packaged bitmap fonts.

## Framework and add-on separation

The host validates game observations, assigns encounter boundaries, and owns
generic table, image, and font rendering. The add-on aggregates events, formats
the results, keeps the current and completed encounters in memory, and chooses
how the meter looks. The framework has no damage-meter widget or built-in
damage aggregation.

## Live QA

The encounter behavior, displayed totals, and layout still need more in-game
QA.

## Support

<a href="https://www.buymeacoffee.com/colorise" target="_blank" rel="noopener noreferrer">
  <img src="https://cdn.buymeacoffee.com/buttons/v2/default-blue.png" alt="Buy Me a Coffee" style="height: 60px !important; width: 217px !important;">
</a>
