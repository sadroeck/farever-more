# Third-Party Notices

## Project code and dependencies

Farever More's Rust source is licensed under MIT. See [LICENSE-MIT](LICENSE-MIT).
Third-party dependencies remain under their own licenses. The root and add-on
Cargo lockfiles record the versions used; package metadata identifies each
dependency's license.

## Vendored hlbc crates

The API inspector vendors the MIT-licensed hlbc and hlbc-derive 0.8.0 crates from [Gui-Yom/hlbc](https://github.com/Gui-Yom/hlbc). Guillaume Anthouard's copyright and license are retained in the [vendored LICENSE file](farever-api-inspector/vendor/hlbc/LICENSE).

## Design reference

The damage-meter documentation credits [Skada](https://github.com/zarnivoip/skada) as a behavior reference for its per-skill table and damage-share presentation. Farever More uses no Skada source code or bundled assets.

## Bundled fonts

The damage-meter and GPS add-ons bundle Noto Sans Regular and Bold. Their SIL Open Font License 1.1 text is included at [addons/dyno/assets/fonts/OFL-1.1.txt](addons/dyno/assets/fonts/OFL-1.1.txt).

## Farever-derived material

Some artwork and data in Farever More are derived from Farever and included
solely for use within the Farever client. Farever is developed and published by
Shiro Games. These materials are not covered by the project MIT license; their
attribution appears beside the relevant assets.

## Slint

The manager pins Slint 1.18.0 and uses its Royalty-free Desktop, Mobile, and
Web Applications License 2.0, allowing the project source to remain
MIT-licensed. The license requires either an AboutSlint widget in an About
dialog reachable from the top-level menu or a Slint attribution badge on a
public page. The manager's About menu opens a dialog with the widget.

See the [Slint 1.18.0 license overview](https://github.com/slint-ui/slint/blob/v1.18.0/LICENSE.md), the [royalty-free license text](https://github.com/slint-ui/slint/blob/v1.18.0/LICENSES/LicenseRef-Slint-Royalty-free-2.0.md), and [Slint's license terms](https://slint.dev/terms-and-conditions). Recheck the terms if the pinned version or distribution method changes.
