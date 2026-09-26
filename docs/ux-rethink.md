# UX rethink: mobile as the primary target

Written before any code changes, to be argued with rather than built from blind.
The current layout, one browse column, a detail/generate column, switched by a
`Pane` tab below 900px, is a desktop three-column layout with a column folded
away, not a design that started from a phone. This is what a phone-first
version looks like instead, and what in the current tree has to change to get
there.

## Status

Built, in build-order:

1. **Navigation stack replacing `Pane`/`explore`-as-tab-set.** Landed
   differently than first sketched: rather than a general route stack, the two
   places that needed one turned out to have their own natural flag already,
   `Selection` for the sheet, a single `explore` toggle for pipeline (now a
   chip, not a two-tab `nav`), so no separate stack type exists. Revisit if a
   third destination shows up that does not already have a signal to hang off.
2. **Track sheet** (`ui/sheet.rs`, `TrackSheet`): done. Merges `DetailPane` +
   `GeneratePanel`; docks beside the list ≥900px, overlays below it.
3. **Path pill** (`ui/sheet.rs`, `PathPill`): done.
4. **Search overlay**: done, but landed as an icon-triggered collapse/expand
   in place rather than a separate full-screen route: `LibraryPanel` already
   rendered search results inline in the one shelf, so there was no second
   surface to push onto a stack. `Search::open` gates it now.
5. **Header trim**: done: title, counts, search icon, pipeline icon, settings
   icon. The selected-track label was dropped rather than trimmed, the sheet
   already shows the selection, so it was saying the same thing twice. (The
   map icon that was here briefly moved on to step 6 below.)
6. **Mini-player → full player, map/pipeline entry points onto it**: done,
   differently than the open question below anticipated needing to be
   answered first. `FullPlayer` (`ui/player.rs`) opens from the mini-bar's art
   or title at every width, a centred, capped overlay rather than a
   dock/overlay split, since "now playing" is a focus mode everywhere, not a
   phone concession. Contents: big art, title/artist, seek, transport,
   volume, quality, then `queue` and `on the map`, both close the full player
   on the way to opening themselves rather than stacking overlays. Map's
   header chip is gone; it's a permanent chip on the mini-bar instead (visible
   whether or not the full player is open), which is what "a small icon
   beside it" in the original design meant.

The rest of this document is the original design reasoning, largely still
accurate; where later judgment changed the shape of something (search,
mainly), the section below says so rather than being silently rewritten.

## The problem with the pane switch

`Pane::Library` / `Pane::Tools` (`app.rs`) means the most common action on the
app (find a track, do something with it) costs two navigations on a narrow
screen: tap the row (selects it), then tap the `tools` tab to reach the panel
that can play it, queue it, or ask the space for its neighbours. On desktop
that panel is just sitting there, a glance to the right. On a phone it is a
full screen replaced by another, every single time, for the single most
frequent thing the app is for.

It compounds: the *result* of acting on a track (a radio walk, a path) is
drawn on the map, a third surface, an overlay you have to remember to open.
Select → switch pane → act → switch surface is four contexts for one thought
("play something like this").

`docs/design.md` already names the fix for the first half, "the detail pane
... rather than a sheet that slides over the browse list", as unshipped. This
document is the rest of it: sheet in, pane switch out, and the generate result
lands in the same sheet that produced it instead of a separate overlay.

## Principle

**Phone first, tablet/desktop is the same tree given more width.** Not two
layouts maintained in parallel, one component structure where a wide viewport
lets the sheet dock as a permanent side panel instead of overlaying, the way
the sheet-vs-panel duality already exists for the map (`MapView`) and settings
(`Settings` modal). No second `Pane`-style fork.

**One surface owns the track, wherever you are.** Browsing, search results,
the queue, the map, tapping a track always opens the same sheet, never a
different screen depending on where the tap came from.

**Navigation is a stack, not a set of tabs.** Home (browse) is the base. Search
and the sheet push onto it and pop off; nothing here is a persistent tab you
"switch to and remember to switch back from" the way `pane`/`explore` are now.

## Screens

### Home: the browse list, full screen

What `.library` is today, minus the column: the merged Qobuz + space list with
its section headings, tabs (search/library/playlists), breadcrumbs, and the
sticky hidden-artists block at the bottom. No sibling column competes for
width, so this is what most of the screen is, always.

Header shrinks to three things: title, a search icon, a settings icon. The
header currently also carries the header-search input, the `map` chip, the
`explore`/`pipeline` tabs and the selected-track label (`app.rs`, `SearchBox`),
on a 393px screen that's already two wrapped rows before a single track is
visible. Search becomes an icon that pushes a full-screen search state (below)
rather than permanent header real estate. `explore`/`pipeline` and `map` move
out of the header entirely (below).

### Track sheet: bottom sheet, opened by tapping any row

Replaces `DetailPane` living in the tools pane. Slides up over Home (or
search, or the queue, wherever the tap happened), covering most but not all
of the screen so the list underneath stays visibly there. Swipe down or tap
the scrim to dismiss, straight back to exactly where you were, no pane to
switch back from.

Contents are `DetailPane`'s, largely unchanged: cover, title, artist, album,
badges (`in your space`, bpm), then the verb buttons, Play, Play next, Queue,
On the map, Find neighbours, Start radio, Path A, Path B. What changes is
where a generate result appears:

**The result renders in the sheet**, replacing the verb list with a labelled
result (the `Recipe` label that already exists in `generate.rs`) and its track
list, plus a small "show on map" affordance rather than an automatic jump to
the overlay. `GeneratePanel`'s mode tabs and weight sliders move here too,
see below.

This is one component doing what `DetailPane` + `GeneratePanel` do today,
because on a phone they're one continuous thought ("this track → this walk"),
not two panels you visit separately.

### Path mode: visible, persistent state

Tapping "Path A" today sets `Generator.from` and the sheet closes; nothing on
screen says a path is half-built until you reopen a second track's sheet and
notice "Path B" is enabled. On a phone, where the app is backgrounded
constantly, that state needs to survive being forgotten about.

Proposal: a small persistent pill, docked above the mini-player, that appears
the moment `from` is set, *"Path from **{track}** → pick an end"*, and stays
until `to` is set (generates) or the pill's own ✕ clears it. Tapping the pill
reopens A's sheet. This is a few lines of state, not a new subsystem:
`Generator.from`/`.to` already exist; this is a read of them rendered outside
the sheet instead of only inside it.

### Weight sliders

Currently a `panel` sitting under `GeneratePanel` in the tools column, always
visible. On a phone that is real estate spent on a control most sessions never
touch. Moves into the track sheet as a collapsed "tune the space" disclosure
below the mode tabs, expand to reach the eight sliders, collapsed by default.
Nothing about the sliders' live-query behavior changes; this is placement only.

### Search: full-screen overlay, not header-resident

Tapping the header's search icon pushes a full-screen search state: input at
the top (autofocus, so the keyboard is already up), results below as they
would render in Home's shelf. Backing out returns to wherever search was
opened from. The two-query model underneath, instant local filter, debounced
remote query (`Search { text, submitted }` in `ui/mod.rs`), is unchanged;
this only moves where the box lives and how much it costs when you're not
using it.

### Map: a deliberate destination, not part of the primary loop

Stays an overlay (`MapView`, `map.js`'s DOM-node-caching constraint is
unchanged and still means "hide with a class, never unmount", see
`app.rs`'s comment on the canvas). Reached from the mini-player (long-press,
or a small icon beside it) rather than a header chip competing with search and
settings. This is also the honest choice given coverage: at ~18% of the
catalogue analysed, the map is not where most sessions should start, so it
shouldn't sit in the primary chrome insisting otherwise.

"On the map" from inside a track sheet, and "show on map" from a generate
result, both still open it in route mode (`MapView::show_route`) exactly as
today.

### Player and queue

Mini-player pinned at the screen bottom, always present once something has
played, this is close to what `.player`/`PlayerBar` already is; the change is
just that it's the one persistent chrome element instead of sharing the
bottom of the screen with a pane switcher. Tapping it expands to a full player
screen (large art, transport, quality picker, currently squeezed into a
single bar). The queue drawer (`Player.queue_open`, drag-to-reorder by grip)
opens from the full player rather than from the mini-bar directly, since a
one-line bar has no room for a queue-toggle button that doesn't compete with
transport controls.

### Pipeline

Today `explore`/`pipeline` is a header tab, i.e. a whole second app-mode
swapped in for the entire screen. That's fine as a concept, pipeline
management is not something you do while also browsing, but it shouldn't
share header real estate with search and the map. Moves to be reached from
settings, or a dedicated icon, rather than a tab living beside the two things
you actually touch every session.

## What doesn't change

- The backend split (`backend::backend()`, local vs. paired server), none of
  this is backend-shaped.
- The four generate modes, their semantics, and `Recipe` labelling.
- The merged browse list (space + Qobuz, one list, section headings), that
  decision already solved the provenance problem this doc isn't touching.
- Hiding/blocking, and the hidden-artists panel's behavior.
- `map.js`'s cache-at-eval-time constraint: the map still lives permanently in
  the DOM, hidden by class, never remounted.
- The queue's drag-to-reorder and shortest-path sort.
- Desktop keeps working, a wide viewport docks the sheet as a permanent side
  panel instead of overlaying (same component, `if wide { docked } else {
  overlay }`, the way `MapView` and `Settings` already branch on being a modal
  vs. not), so this isn't "phone gets a new tree and desktop keeps the old
  one" to maintain twice.

## What this costs to build

Roughly, in order of how much they unlock each other, see **Status** above
for what actually landed at each step and where it diverged from this list:

1. **Navigation stack** replacing `Pane`/`explore` booleans, a small enum or
   `Vec<Route>` the shell renders from, with push/pop instead of tab-set.
   *(Landed as: reuse `Selection` and a single `explore` bool, no stack type
   was needed; see Status.)*
2. **Track sheet** merging `DetailPane` + `GeneratePanel`, as an overlay
   (later: dockable) component, taking `Selection` the way `DetailPane` does
   now.
3. **Path pill**: a small new component reading `Generator.from`/`.to`,
   mounted at the shell level.
4. **Search overlay**: mostly moving `SearchBox` and the existing shelf
   rendering into a full-screen route; the query model is untouched.
   *(Landed as: icon-gated collapse of the existing box, not a separate route,
   `LibraryPanel` was already the one surface for both; see Status.)*
5. **Header trim** and **mini-player → full player** expansion. *(Trim done;
   full player not started.)*
6. Map/pipeline entry points relocate; no internal change to either.
   *(Pipeline done, a chip, off the header's two-tab `nav`. Map deferred:
   its planned destination is the mini-player, which does not exist yet.)*

None of this touches `db.rs`, `space.rs`, `engine`, or the backend trait,
it's `app.rs` and `ui/*` only, plus `style.css` for the sheet/overlay
mechanics (a bottom sheet is a translate-Y transition plus a scrim, nothing
`map.js`-level tricky).

## Open questions

- **Docked-vs-overlay breakpoint.** ~~The existing `900px` breakpoint~~ Kept
  at 900px for now, unchanged from the old `.panes` cutoff. It has not been
  looked at on a real tablet-width device since the sheet is narrower than the
  three-column arithmetic that originally produced that number; revisit if a
  wide phone or small tablet docks the sheet somewhere awkward.
- ~~Does Home need its own bottom tab bar~~ Resolved by not needing to decide:
  there was no navigation stack to design an IA around in the first place
  (see Status, item 1). If a genuine third destination shows up later, one
  with no existing signal to key off, this question is back.
- ~~Full player screen contents aren't designed here~~ Resolved by building
  it rather than deciding it on paper: art up to 320px, title/artist, seek,
  transport, volume, quality, then `queue`/`on the map` as exits rather than
  inline content, see Status, item 6, for why the queue stayed its own
  drawer instead of moving inside. Untested on a real screen (see the note
  below); the one thing worth an eye once it can be run is whether 320px art
  plus everything below it fits a 393px phone without scrolling, or whether
  the transport row needs to come above the art instead.

## Standing note: none of this has been looked at

Every screen in this document is verified at the compiler/type level only,
`cargo check` on both the desktop and headless feature sets, `cargo test`,
`cargo clippy`, never by actually looking at the webview. That covers "does
it build and hang together" but not "does the sheet's animation feel right",
"is 320px too much art on a small phone", or "does the docked sheet's
900px breakpoint land somewhere sensible", all genuinely open until someone
runs `cargo run` from `app/` and taps through it.
