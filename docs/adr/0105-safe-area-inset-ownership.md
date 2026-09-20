# ADR 0105 — Safe-area inset ownership, and measuring it

**Status:** Accepted.
Implemented in the web silo alongside this record.

## Context

ADR 0102's packaged shell puts the web client on hardware the browser deploy never ran on.
`clients/web/index.html` sets `viewport-fit=cover`, so on a notched or home-indicator device iOS lays the page out over the status bar, the Dynamic Island and the home indicator instead of inside them, and `index.css` hands each of those strips back deliberately.
Without the tag iOS letterboxes the app and fills the remainder with the webview's own backdrop; with it, every rule that reaches an edge has to say what it does about that edge.

Ten rules ended up calling `env(safe-area-inset-*)`, each written on its own, and two of them sat on the same path.
`main` pads its bottom so the last control of Settings can be scrolled clear of the home indicator.
`.composer` pads its bottom so the message entry box stays above the same indicator.
In a room the composer is inside `main`, so both fired, and the inset was paid twice — with `main`'s copy _below_ the composer, so the composer never reached the bottom edge its own rule was written for.

Measured in the packaged shell on the iOS 26.5 simulator:

| device            | orientation | inset | `main` padding-bottom | `.composer` padding-bottom | blank below the entry box |
| ----------------- | ----------- | ----- | --------------------- | -------------------------- | ------------------------- |
| iPad Pro 11" (M4) | portrait    | 20px  | 44px                  | 29.6px                     | **73.6px**                |
| iPhone 17         | landscape   | 20px  | 44px                  | 29.6px                     | **73.6px**                |

The second row is not a typo: a phone in landscape is 874pt wide, clears the 48rem breakpoint, and so lands on the same branch — 73.6px of dead space on a display 402pt tall.
Below the breakpoint the opposite defect was already there: the narrow `.composer` rule re-declared `padding` as a shorthand and dropped the inset entirely, so on an iPhone 17 in portrait the entry box ended 8px above the bottom of a display whose bottom inset is 34px — 26px of the text box inside the strip the home indicator is drawn on.

Neither could be caught by a test.
`env(safe-area-inset-*)` resolves to `0px` in every headless browser, which is why the change that introduced this said, accurately, "the test suite passing says nothing here — checked on device instead".
Nothing was wrong with that as a description; it is not durable as a process.

## Decision

### One owner per edge

On any path from `.shell` to a screen edge, exactly one element applies that edge's inset: the one whose box actually reaches the edge.

In a room that is `.composer`, so `main` applies no bottom padding at all under `.mode-room`, at every width.
On a utility page (Settings, Accounts, Invites, Licenses) there is no composer, so `main` keeps it.
The room list keeps it on `.room-list-pane`.

The thread and room-info panels are the same question asked twice, and they answer it differently, at every width and in both of their layouts — a fixed overlay drawer below 64rem, a static third column above it.
A thread ends in its own `.composer`, so `.thread-panel` reserves nothing at all and the composer owns the edge exactly as it does in the room.
Measured in the packaged shell on an iPhone 17, portrait, bottom inset 34px: the `1rem` that rule used to have left the thread composer 16px above the bottom of the display and its entry box 18.4px above it, inside the home-indicator strip; with `0` they are 0px and 36.4px.
Room info ends in a member row and has no composer, so `.side-panel` reserves the inset itself.
Its list does scroll — `.room-info-panel` overrides the panel's `overflow: hidden` to `auto` — but only within the panel's content box, which is what that padding sizes, so the reservation is what makes the last row reachable rather than merely tidy.

The top edge is unchanged and already followed this rule: `.topbar` owns it so its background runs up behind the status bar, and `.shell` owns left and right.

### Insets are tokens, not scattered `env()` calls

`:root` defines `--safe-top`, `--safe-right`, `--safe-bottom` and `--safe-left` from `env(safe-area-inset-*, 0px)`, and every other rule reads the token.
This is what makes the owners greppable — the double application above was invisible precisely because neither rule could see the other — and it is what makes the insets injectable.

### An edge inset is a longhand declared after every breakpoint

A rule that owns an inset declares it as its own `padding-bottom` (or `padding-top`) _after_ the last breakpoint that sets that element's `padding`, never as a term inside a shorthand.
A shorthand in a later breakpoint silently drops a longhand from an earlier rule, which is how the phone lost the composer inset, and how `.topbar` lost the status-bar inset before it.
A longhand declared after every breakpoint cannot be undone by a new one.

### The insets are measured on devices and replayed in CI

Two halves, and neither is sufficient alone.

The **device half** reads the real numbers out of the packaged shell on the iOS Simulator, one form factor at a time, and is a manual procedure (`clients/web/AGENTS.md` § Measuring the packaged shell).
That is the only thing that can say what iOS reports.

The **CI half** is `clients/web/e2e/safe-area.spec.ts`, which sets the four tokens as inline properties on `:root` to the numbers the device half measured and asserts the layout invariants at each form factor's viewport.
That is what runs on every PR, on hardware that has no notch at all.

The spec's table therefore carries the measurement date and the runtime it came from, and it is data, not a guess.
It proves our arithmetic is right for what a device reports; it does not prove the device still reports it.

## Consequences

- In a room and in a thread, the composer sits against the bottom edge on every platform and in every layout, and the space below the entry box is the home-indicator inset plus the breakpoint's own padding — nothing else.
- Desktop and any display with no insets lose `main`'s 1.5rem gutter below the composer in a room: the composer now meets the window's bottom edge there too, which is what it already did below the mobile breakpoint.
- A phone gains real clearance under the entry box where it previously had 8px; the change is invisible on hardware without a home indicator, since the token collapses to `0px`.
- A new rule that reaches a screen edge now has a question it must answer, and a place to answer it. A second owner on one path is a bug, and the spec fails on it at nine form factors — in the room, in the thread panel and in room info — rather than on whichever device someone happens to hold.
- The device half stays manual. Automating it needs a simulator on a runner and an installed shell; #445 already tracks the packaging lane that would be the place for it.
- Landscape on the simulator is driven by an app that declares only landscape orientations, not by rotating the device — `Simulator.app` needs a GUI session and `simctl` cannot rotate.
  That is not free: restricting an iPad app's orientations makes it ineligible for multitasking, and **that alone** moves its bottom inset from 20px to 25px, which the same build with all four orientations and `UIRequiresFullScreen` set reproduces exactly.
  So the iPad landscape rows in the spec carry the portrait value, which is the one the shipping configuration reports and the stricter of the two to assert against; iPhones have no multitasking and are unaffected.
  A physically rotated iPad has not been compared against either.
