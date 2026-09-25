# Player Experience and Cross-Provider Library Design

**Status:** Approved in chat on 2026-09-25

## Goal

Make playlist selection identity-safe, keep Noctalia list geometry synchronized with scrolling, recover from unplayable SoundCloud tracks, support mixed SoundCloud/YouTube Music collections, paginate the Home feed, add YouTube Music radio, and replace the current search/help interactions with predictable terminal-style UI.

## Scope

This design changes five connected areas:

1. Playlist playback identity across the TUI, CLI protocol, daemon, and Noctalia plugin.
2. SoundCloud playback failure handling and listing filters.
3. App-owned mixed-provider playlists and an aggregate Liked Music collection.
4. YouTube Music Home continuation and radio queue refill.
5. Noctalia list geometry, search editing, and keybinding help.

The implementation must migrate all callers in one cutover. No compatibility aliases or deprecated positional playback paths remain.

## Current Failures

### Delayed list scrolling

`packaging/noctalia/tmus/panel.luau` uses fixed logical windows in `clampScroll()`. Long labels wrap, so rendered rows become taller than the logical row count. The selection can enter clipped rows before the logical top advances. Rust TUI list scrolling is unaffected because ratatui owns the viewport offset.

### Playlist selection mismatch

`crates/tmus-tui/src/ui.rs` stores an open playlist as a numeric index into a mutable playlist vector. A library refresh can retarget that index. Both clients also send a numeric track offset, while the daemon reads a fresh playlist snapshot. Reordering between snapshots makes the same offset identify another track. The daemon itself correctly clears and rebuilds the queue.

### SoundCloud playback stalls

SoundCloud authentication is healthy. Some tracks fail during yt-dlp resolution with `This video is DRM protected`. Resolve-time failures do not enter the mpv playback-failure auto-skip path and do not populate `PlayerState.last_error`, leaving playback stopped after the queue cursor moved.

### Collection limitations

SoundCloud remote likes work. Native SoundCloud playlist mutation is unsupported because the web-client composition contract is unconfirmed. The current destination picker can still expose operations that cannot succeed. Local ratings and provider likes are separate views.

### Home and radio limitations

The YouTube Music provider fetches one `FEmusic_home` response. Continuation tokens do not cross the catalog/protocol boundary. `InnerTube::next(video_id, playlist_id)` exists but is unused, so finite materialized queue contexts simply end.

### Search and help limitations

The Noctalia search buffer is append-only and byte-oriented. It has no cursor; Left/Right are captured but unused; Backspace can split UTF-8 text. `oo` is an explicit insert-mode escape. Help uses two dense fixed-width columns that wrap with Russian copy.

## Architecture

### 1. Identity-safe playlist playback

Use stable IDs at every boundary.

- `Nav.open_playlist` stores `PlaylistId`, never a vector index.
- The playlist playback command carries `PlaylistId` and selected `TrackId`; numeric start offsets are removed.
- Noctalia playlist rows carry both IDs in their action payload.
- The daemon loads playlist pages until it finds the selected `TrackId`, rebuilds the queue, and selects that track.
- If a selected track disappeared before playback, return a precise not-found error and leave the current queue unchanged.
- Duplicate occurrences of one `TrackId` resolve to the first occurrence, matching existing queue lookup semantics; they represent the same audio identity.

Queue mutation is transactional from the caller's perspective: gather and locate the requested track first, then clear and replace the queue.

### 2. Provider-neutral unplayable tracks

Add `ProviderError::Unplayable { provider, reason }`.

- yt-dlp classifies known DRM/unavailable-stream stderr as `Unplayable` rather than generic `Tool`.
- The player writes the reason to `last_error` and routes resolve-time `Unplayable` through the existing bounded failure counter and `MAX_AUTO_SKIPS` policy.
- A queue containing only unplayable tracks stops after the existing limit; it cannot loop forever.
- SoundCloud parsing rejects tracks with `policy == "SNIPPET"` or `streamable == false` before they enter search, likes, playlists, or Home shelves.
- Filler treats `Unplayable` as a terminal item result rather than scheduling the normal transient-error backoff ladder.

The player and daemon remain provider-neutral; SoundCloud-specific metadata rules stay inside `tmus-soundcloud`.

### 3. App-owned mixed-provider playlists

Add local playlists to the existing SQLite persistence layer.

A local playlist has:

- Stable app-owned playlist ID.
- User-visible name.
- Ordered entries containing `(provider, track_id)` plus the existing cached track metadata needed to render offline/cache fallback views.
- Creation and update timestamps.

Operations:

- Create, rename, and delete a local playlist.
- Add a track from any provider.
- Remove an entry.
- Preserve insertion order and duplicate entries unless the user explicitly removes one.
- Play a local playlist through the same stable `TrackId` selection contract.

Provider-native playlists remain separate. The destination picker exposes an operation only when the selected provider advertises the required capability. It always exposes eligible local playlists.

### 4. Aggregate Liked Music

Liked Music is a virtual collection, not a remote cross-provider playlist.

- Source rows are the union of provider-reported likes and locally liked/rated tracks.
- Identity key is `(provider, track_id)`.
- Provider metadata wins when fresh; cached local metadata is the fallback.
- Stable ordering is most-recent known like/rating first, with deterministic provider/ID tie-breaking.
- A provider fetch failure does not erase locally known liked tracks or successful results from another provider.
- `f` updates local liked state first and mirrors the remote like/unlike operation when the provider supports rating.
- If remote synchronization fails, the local state remains visible and the UI reports that remote synchronization failed.

This supports discovered SoundCloud tracks immediately while retaining provider-native likes when available.

### 5. Home continuation

Replace the single-result Home contract with a page contract:

```text
HomePage {
  shelves: Vec<CatalogShelf>,
  next: Option<String>,
}
```

- The initial Home command returns the first page.
- A separate continuation command accepts the opaque token and returns the next page.
- The daemon caches the assembled Home feed per provider and auth/cache generation, not only the first response.
- The Noctalia panel requests another page near the final visible shelf.
- Merge logic deduplicates repeated shelf/card identities while preserving server order.
- Opaque continuation tokens are memory/cache-lifetime data and are not persisted across application restarts.
- The panel continues to render only its visible shelf/card slices; loaded backing data may grow independently.

### 6. YouTube Music radio

Add an explicit radio session to daemon playback state.

- A radio session stores provider, seed track, optional playlist context, and the next continuation token.
- Starting a single YouTube Music track from Search or Home starts radio mode.
- Explicit playlist playback remains finite and deterministic.
- Near the queue tail, the daemon calls the provider radio continuation method and appends deduplicated recommendations.
- Replacing or clearing the queue cancels the current radio session.
- Exhausted or invalid continuation ends radio without discarding playable queued tracks.
- Provider errors are surfaced in state; the existing queue keeps playing when possible.

The provider-neutral catalog trait exposes radio start/continuation pages. YouTube Music implements them using the existing InnerTube `next` primitive; unsupported providers return `Unsupported`.

### 7. Noctalia row geometry and scrolling

Rendered geometry and scroll math share one source of truth.

- Define fixed row heights and gaps per surface.
- Derive each logical visible count from the corresponding viewport budget and row pitch.
- Give title and subtitle independent character budgets.
- Fix result, library, queue, and Home card text regions to their declared heights so labels cannot expand a row.
- Reduce queue spacing to fit the declared visible count.
- `clampScroll()` receives the derived count; it never assumes more rows than can render.

Only demonstrated Noctalia primitives and properties are used. No undocumented no-wrap/elide property is introduced.

### 8. Terminal-style search editor

Keep a visual-only editor owned by the existing `onKey` handler.

State:

- UTF-8 query text.
- Cursor index measured in Unicode code points.
- Horizontal display offset measured in code points.

Editing keys:

- Character input inserts at the cursor.
- Left/Right move one code point.
- Home/End move to boundaries.
- Backspace removes the previous code point.
- Delete removes the next code point.
- Ctrl+Backspace removes the preceding word using the existing word-boundary convention, converted to code-point-safe operations.
- Escape, `jj`, and `jk` leave insert mode.
- `oo` is removed as an escape sequence.

The renderer splits visible text into before-cursor, caret, and after-cursor labels and keeps the caret inside the search viewport. `plugin.toml` captures Home, End, and Delete. README documents the complete behavior.

### 9. Keybinding help

Replace the dense two-column list with three semantic groups:

1. Navigation.
2. Search and editing.
3. Playback and library.

Each group uses a short heading, fixed-height rows, compact accent key chips, and concise Russian descriptions. Existing Noctalia theme roles and the monospace font remain the styling source. Text must fit without wrapping at the current panel width.

## Data Flow

### Playlist selection

```text
Client row (playlist_id, track_id)
  -> protocol command
  -> daemon loads pages and locates track_id
  -> queue replacement
  -> player resolves selected track
```

### Liked Music

```text
provider likes ----\
                    -> union by (provider, track_id) -> ordered virtual collection
local liked state -/
```

### Radio

```text
Search/Home selection
  -> start radio(seed)
  -> initial queue
  -> queue-near-tail event
  -> provider continuation
  -> deduplicated append
```

## Error Handling

- Missing selected playlist/track: explicit not-found response; no queue replacement.
- Unplayable media: visible `last_error`, bounded auto-skip, then stopped state.
- Provider likes unavailable: retain local aggregate and successful provider results.
- Native playlist mutation unsupported: destination is not shown; direct protocol use returns `Unsupported`.
- Home continuation failure: retain already loaded shelves and expose the error without clearing Home.
- Radio continuation failure: retain queue and end or retry only according to existing provider error semantics; no unbounded loop.
- Invalid UTF-8 input cannot corrupt the search buffer because all mutations occur at code-point boundaries.

## Persistence and Migration

- Add schema migration(s) for local playlists and ordered entries using the repository's existing migration mechanism.
- Existing provider playlists and ratings remain intact.
- Liked Music is derived and does not duplicate full provider collections in a new table.
- Continuation tokens and active radio state are not persisted.

## Documentation

Update README sections for:

- Stable playlist playback CLI/protocol syntax.
- Local mixed-provider playlists.
- Aggregate Liked Music and `f` behavior.
- SoundCloud unplayable-track handling.
- Home continuation and YTM radio semantics.
- Search editing keys and removed `oo` chord.
- Revised help screenshot only if a user-authorized visual verification produces one; otherwise leave screenshots unchanged.

## Verification Strategy

The user did not request tests. Repository policy permits tests only through existing Vitest/Playwright integration, and this Rust/Luau repository has no applicable requested test run. Implementation verification therefore consists of:

- Review every worker diff against this design.
- Run a memory-capped `cargo check --workspace` after confirming the host-wide heavy-job slot and memory headroom.
- Run the repository's static Noctalia plugin lint command if it is available and does not open the live shell.
- Do not open the live Noctalia panel, take desktop screenshots, create throwaway runtime scripts, or run browser automation without a separate explicit request.

Behavior not exercised at runtime must be reported as unobserved rather than claimed verified.

## Acceptance Criteria

- Selecting a playlist row plays the same `TrackId` shown by either client, even after a library reorder or playlist content refresh.
- Long queue/list text cannot create hidden logical rows or delayed scrolling.
- Known SoundCloud DRM-only tracks are filtered when metadata permits; any remaining resolve-time unplayable track is reported and skipped within the existing cap.
- Users can create app-owned playlists containing both SoundCloud and YouTube Music tracks and manipulate their entries.
- Liked Music shows deduplicated local and provider likes across both providers.
- Home can append continuation pages while rendering a bounded visible slice.
- Search/Home YouTube Music playback grows through radio recommendations near queue exhaustion.
- Search editing has a visible cursor and UTF-8-safe insertion/deletion/navigation; Latin `oo` inserts text and does not exit insert mode.
- Help is grouped, fixed-height, and non-wrapping at the current panel width.
- README matches the final contracts and keybindings.
