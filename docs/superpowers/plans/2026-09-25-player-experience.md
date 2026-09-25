# Player Experience Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use the repository golem pipeline to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver identity-safe playlist playback, resilient SoundCloud playback, mixed-provider local collections, paginated Home recommendations, YouTube Music radio, and corrected Noctalia list/search/help behavior.

**Architecture:** Stable `(provider, id)` identities replace positional playlist playback. Provider-specific parsing stays in provider crates; provider-neutral orchestration lives in core/player/daemon. App-owned playlists use the existing SQLite cache and appear alongside capability-filtered provider playlists. Noctalia keeps fixed row geometry and owns a UTF-8-safe visual search editor.

**Tech Stack:** Rust 2024, Tokio, serde newline-delimited JSON protocol, rusqlite, ratatui, Noctalia Luau plugin.

**Spec:** `docs/superpowers/specs/2026-09-25-player-experience-design.md`

## Global Constraints

- Migrate every caller in one clean cutover; do not retain positional playlist-start aliases or deprecated protocol variants.
- Provider-specific JSON and policy rules remain inside the matching provider crate.
- Do not introduce undocumented Noctalia properties; use only primitives already present in `panel.luau`.
- Do not add tests. Update existing tests only when a changed public contract makes their source fail to compile.
- Workers do not run tests, formatters, linters, builds, or smoke checks.
- Preserve user-owned files outside this worktree.
- Explicit playlist playback remains finite; Search/Home single-track playback may start YTM radio.
- Active radio and continuation tokens remain in memory and are not persisted.

---

### Task 1: Classify and Recover From Unplayable SoundCloud Tracks

**Files:**
- Modify: `crates/tmus-provider/src/lib.rs`
- Modify: `crates/tmus-provider/src/ytdlp.rs`
- Modify: `crates/tmus-player/src/lib.rs`
- Modify: `crates/tmus-soundcloud/src/parse.rs`
- Modify: `crates/tmus-daemon/src/filler.rs`

**Interfaces:**
- Produces: `ProviderError::Unplayable { provider: ProviderId, reason: String }`.
- Consumes: existing `failure_streak`, `failure_action`, `on_track_end`, `MAX_AUTO_SKIPS`, and `PlayerState.last_error` behavior.
- Does not change protocol JSON.

- [ ] **Step 1: Add the provider-neutral error variant**

```rust
#[error("{provider}: трек нельзя воспроизвести: {reason}")]
Unplayable {
    provider: ProviderId,
    reason: String,
},
```

Place it next to `NoSuchTrack`/`Unsupported`; do not overload `Unsupported`, because catalog callers intentionally treat that variant as an empty capability.

- [ ] **Step 2: Classify yt-dlp DRM failures**

In `stderr_error`, preserve the existing bot-gate classification, then match the final stderr reason case-insensitively for `drm protected` and return `Unplayable` with the requested provider. Leave unrelated non-zero exits as `Tool`.

- [ ] **Step 3: Route resolve-time unplayable errors through bounded skip**

In the `resolve_fresh` failure path:

```rust
if let ProviderError::Unplayable { reason, .. } = &error {
    state.last_error = Some(reason.clone());
    // Increment the existing failure streak and use failure_action().
    // Advance through on_track_end() only while the existing cap permits it.
}
```

Keep generic network/tool failures on their current stop/report path. Avoid recursive unbounded advancement; reuse the existing mpv failure policy.

- [ ] **Step 4: Filter known unplayable SoundCloud metadata**

Extend the internal SoundCloud track parse decision to reject an item when:

```rust
policy == Some("SNIPPET") || streamable == Some(false)
```

Apply the same helper to search results, likes, playlist tracks, and stream/Home shelves. Missing fields remain accepted.

- [ ] **Step 5: Make filler treat Unplayable as terminal for that item**

Do not place `Unplayable` on the transient 1m→5m→15m→1h retry ladder. Record/log it once and continue to the next candidate through existing filler control flow.

- [ ] **Step 6: Review the task diff**

Confirm that no provider name branching entered `tmus-player` or `tmus-daemon`, and that the existing auto-skip cap remains the only loop bound.

---

### Task 2: Add Local Mixed-Provider Playlist Persistence

**Files:**
- Modify: `crates/tmus-core/src/model.rs`
- Modify: `crates/tmus-core/src/cache.rs`

**Interfaces:**
- Produces: `ProviderId::LOCAL`, local playlist CRUD/cache methods, and ordered local playlist entries represented by existing `Playlist`, `PlaylistId`, `Track`, and `TrackId` types.
- Consumes: the existing `tracks` table as metadata source.
- Later tasks route protocol commands to these methods when `playlist.provider == ProviderId::LOCAL`.

- [ ] **Step 1: Reserve the local collection namespace**

```rust
impl ProviderId {
    pub const LOCAL: Self = Self("local");
    pub const ALL: &'static [Self] = &[Self::LOCAL, Self::YTMUSIC, Self::SOUNDCLOUD];
}
```

Update the type comment: `ProviderId` is a stable source namespace, including the app-owned local source; it is not proof that an account/provider registry entry exists.

- [ ] **Step 2: Add SQLite tables through the existing idempotent schema path**

```sql
CREATE TABLE IF NOT EXISTS local_playlists (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    title       TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS local_playlist_tracks (
    playlist_id     INTEGER NOT NULL,
    position        INTEGER NOT NULL,
    track_provider  TEXT NOT NULL,
    track_id        TEXT NOT NULL,
    PRIMARY KEY (playlist_id, position),
    FOREIGN KEY (playlist_id) REFERENCES local_playlists(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS local_playlist_track_lookup
    ON local_playlist_tracks (playlist_id, track_provider, track_id);
```

Enable/retain foreign keys according to the cache's current connection setup.

- [ ] **Step 3: Implement local playlist CRUD**

Add focused cache methods with these contracts:

```rust
pub fn local_playlist_create(&mut self, title: &str) -> Result<Playlist>;
pub fn local_playlist_rename(&mut self, id: &PlaylistId, title: &str) -> Result<()>;
pub fn local_playlist_delete(&mut self, id: &PlaylistId) -> Result<()>;
pub fn local_playlists(&self) -> Result<Vec<Playlist>>;
```

Reject non-local IDs. Trim titles and reject empty names. Convert the SQLite integer key to `PlaylistId::new(ProviderId::LOCAL, rowid.to_string())`.

- [ ] **Step 4: Implement ordered entry mutation**

```rust
pub fn local_playlist_add(&mut self, playlist: &PlaylistId, track: &TrackId) -> Result<()>;
pub fn local_playlist_remove_at(
    &mut self,
    playlist: &PlaylistId,
    position: usize,
) -> Result<()>;
pub fn local_playlist_tracks(&self, playlist: &PlaylistId) -> Result<Vec<Track>>;
```

Addition appends at `MAX(position) + 1` and permits duplicates. Removal is positional, then compacts later positions inside one transaction. Track reads join `local_playlist_tracks` to the existing `tracks` metadata table and fail clearly if an entry has no cached metadata instead of fabricating a track.

- [ ] **Step 5: Expose recency for aggregate likes**

Add a cache query returning locally liked tracks with their stored rating timestamp:

```rust
pub fn liked_tracks(&self) -> Result<Vec<(Track, i64)>>;
```

Join `ratings(kind = 'liked')` with `tracks`, order by `updated_at DESC`, and skip corrupt/unknown provider rows using the cache's current defensive parsing convention.

- [ ] **Step 6: Review persistence invariants**

Confirm deletion cascades, positions stay dense after removal, duplicates are preserved, and remote provider playlist cache tables are untouched.

---

### Task 3: Replace Positional Playlist Playback With Stable Track Identity

**Files:**
- Modify: `crates/tmus-core/src/protocol.rs`
- Modify: `crates/tmus-daemon/src/app.rs`
- Modify: `crates/tmus-daemon/src/catalog.rs`
- Modify: `crates/tmus-tui/src/main.rs`
- Modify: `crates/tmus-tui/src/ui.rs`
- Modify: `packaging/noctalia/tmus/service.luau`
- Modify: `packaging/noctalia/tmus/panel.luau`

**Interfaces:**
- Consumes: local playlist cache methods from Task 2.
- Produces: `Cmd::PlayPlaylist { playlist: PlaylistId, track: Option<TrackId> }` and CLI `tmus play-playlist <playlist> [--track <provider:id>]`.
- Removes: `start: Option<usize>` and `--start` from playlist playback only. `PlayContext.start` remains positional because the client supplies the complete context in the same command.

- [ ] **Step 1: Change the protocol contract**

```rust
PlayPlaylist {
    playlist: PlaylistId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    track: Option<TrackId>,
},
```

Update existing serialization assertions to the new shape. Do not add a legacy deserialization alias.

- [ ] **Step 2: Make daemon selection transactional**

Change `play_playlist` to accept `Option<&TrackId>`. Load all necessary pages and locate the requested ID before calling `q.clear()`.

```rust
let selected = match requested {
    None => 0,
    Some(id) => tracks.iter().position(|track| &track.id == id)
        .ok_or_else(|| anyhow!("трек {id} больше не входит в плейлист {playlist}"))?,
};
```

For paginated remote playlists, continue loading until found or continuation is exhausted. For local playlists, use Task 2's ordered cache read. Then replace the queue and play the selected track. A lookup error leaves the old queue intact.

- [ ] **Step 3: Stabilize TUI navigation identity**

Change `Nav.open_playlist` from `Option<usize>` to `Option<PlaylistId>`. Resolve the ID when entering the playlist, not when pressing Enter later. On library refresh, retain the open view only if that ID remains present; otherwise close it and reset its track selection.

Build playback from the selected `Track`:

```rust
Cmd::PlayPlaylist {
    playlist: open_playlist.clone(),
    track: Some(selected_track.id.clone()),
}
```

Remove the filtered-to-absolute index mapping from playlist playback. Keep it only where rendering/filtering still needs it.

- [ ] **Step 4: Replace CLI positional syntax**

Change Clap fields and command conversion to `--track <provider:id>`. Update help text and parsing through the existing `parse_track_id` helper.

- [ ] **Step 5: Replace Noctalia payload identity**

Each playlist row action carries a collision-safe JSON payload containing both string IDs, rather than delimiter-splitting a numeric index:

```lua
json.encode({ playlist = playlistId, track = trackId })
```

`service.luau` decodes it and runs:

```lua
poke({ "play-playlist", payload.playlist, "--track", payload.track })
```

Playlist-header activation omits `--track` and starts from the first item.

- [ ] **Step 6: Remove obsolete positional branches**

Delete comments, tests, payload formats, and helper variables whose only purpose was playlist start indices. Leave queue-goto and play-context indexing unchanged.

---

### Task 4: Route Local Playlists and Aggregate Liked Music Through the Daemon

**Files:**
- Modify: `crates/tmus-core/src/protocol.rs`
- Modify: `crates/tmus-provider/src/lib.rs`
- Modify: `crates/tmus-ytmusic/src/lib.rs`
- Modify: `crates/tmus-soundcloud/src/lib.rs`
- Modify: `crates/tmus-daemon/src/app.rs`
- Modify: `crates/tmus-daemon/src/catalog.rs`
- Modify: `crates/tmus-tui/src/main.rs`
- Modify: `crates/tmus-tui/src/ui.rs`

**Interfaces:**
- Consumes: Task 2 cache CRUD and Task 3 stable playback.
- Produces: explicit provider capabilities in `ProviderView`, `Cmd::PlaylistRename`, positional local removal, local playlist selection via `provider = "local"`, and aggregate `Cmd::Liked { provider: None }` semantics.

- [ ] **Step 1: Publish catalog mutation capabilities**

Define a serializable capability value carried by `ProviderView`:

```rust
pub struct CatalogCapabilities {
    pub rate: bool,
    pub playlist_create: bool,
    pub playlist_add: bool,
    pub playlist_remove: bool,
    pub playlist_delete: bool,
}
```

Expose it synchronously from the provider account/catalog registration. YouTube Music advertises its implemented mutations; SoundCloud advertises rating/create/delete but not add/remove. The synthetic local source advertises all local operations and is assembled by the daemon rather than inserted into the remote registry.

- [ ] **Step 2: Extend commands for local manipulation**

Add:

```rust
PlaylistRename { playlist: PlaylistId, title: String },
PlaylistRemoveAt { playlist: PlaylistId, position: usize },
```

Keep `PlaylistRemove { playlist, track }` for provider-native APIs. Local UI uses `RemoveAt` so duplicate entries remain independently addressable.

- [ ] **Step 3: Merge local and provider libraries**

`library()` includes local playlists plus requested remote sources. `library_tracks()` routes local IDs to cache methods and remote IDs to providers. Remote fallback/cache behavior remains unchanged.

`playlist_create(provider = Some("local"))`, rename, add, remove-at, and delete route to cache methods and emit the existing library/playlist change events. Native operations continue through provider capabilities.

- [ ] **Step 4: Build aggregate Liked Music**

For `Cmd::Liked { provider: None }`:

1. Read provider likes independently; retain successful providers if another fails.
2. Read locally liked tracks and timestamps.
3. Union by `TrackId` (`provider + id`).
4. Prefer fresh provider metadata over cached metadata.
5. Emit local timestamp-descending items first, then remaining provider items in provider response order with deterministic provider/ID tie-breaking.

For an explicit provider, preserve the current provider-only behavior.

- [ ] **Step 5: Keep local likes on remote synchronization failure**

`Cmd::Rate` writes local state and emits `RatingChanged` before optional provider mirroring. On remote failure, return/report the synchronization error without rolling the local state back. Do not silently claim remote success.

- [ ] **Step 6: Update CLI/TUI collection operations**

Add CLI rename/remove-at forms under the existing `tmus pl` group. Default interactive creation to local mixed-provider playlists; keep explicit remote provider creation available. Picker rows include local playlists unconditionally and native playlists only when provider/track compatibility plus advertised add capability permit the operation.

---

### Task 5: Add Home Continuations and YouTube Music Radio

**Files:**
- Modify: `crates/tmus-core/src/model.rs`
- Modify: `crates/tmus-core/src/protocol.rs`
- Modify: `crates/tmus-provider/src/lib.rs`
- Modify: `crates/tmus-ytmusic/src/innertube.rs`
- Modify: `crates/tmus-ytmusic/src/parse.rs`
- Modify: `crates/tmus-ytmusic/src/lib.rs`
- Modify: `crates/tmus-soundcloud/src/lib.rs`
- Modify: `crates/tmus-daemon/src/catalog.rs`
- Modify: `crates/tmus-daemon/src/app.rs`
- Modify: `crates/tmus-daemon/src/watcher.rs`
- Modify: `crates/tmus-tui/src/main.rs`
- Modify: `crates/tmus-tui/src/ui.rs`

**Interfaces:**
- Produces core types:

```rust
pub struct HomePage {
    pub shelves: Vec<CatalogShelf>,
    pub next: Option<String>,
}

pub struct RadioPage {
    pub tracks: Vec<Track>,
    pub next: Option<String>,
}
```

- Produces provider methods:

```rust
async fn home_page(&self, cursor: Option<&str>) -> Result<HomePage>;
async fn radio(&self, seed: &TrackId, cursor: Option<&str>) -> Result<RadioPage>;
```

- Produces protocol commands `HomeMore { provider, cursor }` and `PlayRadio { track }`.

- [ ] **Step 1: Replace the Home trait result with pages**

Default `home_page` returns `Unsupported`. SoundCloud returns its existing stream shelves with `next: None`. YouTube Music parses the first `FEmusic_home` response and continuation responses using the existing generic browse continuation support.

- [ ] **Step 2: Carry Home pages through protocol and cache**

Change `Payload::Home` to carry the object `HomePage`. `Cmd::Home` returns the first page; `Cmd::HomeMore` validates the provider and opaque cursor, fetches the next page, and merges it into the in-memory per-provider Home cache. Deduplicate shelf/card identities while preserving first-seen server order.

- [ ] **Step 3: Parse YouTube Music radio pages**

Use `InnerTube::next(seed.id, playlist_id)` for the initial radio response and its continuation endpoint for later pages. Parse only playable tracks and the next continuation token. Keep all raw InnerTube JSON inside `tmus-ytmusic`.

- [ ] **Step 4: Add daemon radio session state**

```rust
struct RadioSession {
    provider: ProviderId,
    seed: TrackId,
    next: Option<String>,
    exhausted: bool,
    refill_in_flight: bool,
}
```

`PlayRadio` clears/replaces the queue with the seed plus initial recommendations, selects the seed, and stores the session. Explicit `PlayPlaylist`, `PlayContext`, `QueueClear`, and queue replacement cancel it.

- [ ] **Step 5: Refill near the queue tail**

Use `watcher::run_state_watcher`, the existing single observer of player state, as the daemon transition hook: after reading `PlayerState`, call an `App::maybe_refill_radio(queue_len, queue_index)` method. Request another radio page when the remaining queue depth reaches a small fixed threshold and `refill_in_flight` is false. Append tracks not already present in the current radio session. Set `exhausted` when no token/tracks remain. Always clear the in-flight guard. A continuation error updates visible error state/logging but leaves current queued tracks intact and marks the attempted token exhausted so the one-second watcher cannot create a tight retry loop.

- [ ] **Step 6: Switch Search/Home single-track actions to radio**

TUI and Noctalia use `PlayRadio` for a selected YTM track from Search or Home. Non-YTM providers fall back to the existing finite `PlayContext`/`PlayTrack` path when radio is unsupported. Explicit playlist actions remain finite.

---

### Task 6: Rebuild Noctalia Geometry, Search Editing, Help, and Feature Wiring

**Files:**
- Modify: `packaging/noctalia/tmus/panel.luau`
- Modify: `packaging/noctalia/tmus/service.luau`
- Modify: `packaging/noctalia/tmus/plugin.toml`

**Interfaces:**
- Consumes: final CLI contracts from Tasks 3–5.
- Produces: fixed row geometry, UTF-8-safe search cursor, grouped help, capability-aware local playlist picker, Home continuation triggers, and radio actions.
- Owns all final edits to `panel.luau`/`service.luau`; earlier task changes to those files must be integrated rather than overwritten.

- [ ] **Step 1: Define row geometry once**

Replace unrelated magic window counts with named height/gap/viewport constants and derived counts:

```lua
local TRACK_ROW_HEIGHT = 36
local TRACK_ROW_GAP = 6
local TRACK_VIEWPORT_HEIGHT = 420
local LIST_WINDOW = math.floor((TRACK_VIEWPORT_HEIGHT + TRACK_ROW_GAP)
  / (TRACK_ROW_HEIGHT + TRACK_ROW_GAP))
```

Use separate constants for queue and Home cards. Apply explicit row/text-region heights and per-surface `shorten` budgets so wrapped text cannot increase the row pitch. Pass only derived counts to `clampScroll`.

- [ ] **Step 2: Add a code-point-safe editor state**

Add `queryCursor` and `queryLeft` as code-point indices. Implement focused helpers using Luau's UTF-8 facilities:

```lua
local function chars(s) ... end
local function insertAt(s, cursor, text) ... end
local function deleteBefore(s, cursor) ... end
local function deleteAt(s, cursor) ... end
```

All helpers return the new string and cursor. Enforce the existing maximum by character count rather than byte length.

- [ ] **Step 3: Implement editing chords and caret rendering**

Handle Left, Right, Home, End, Backspace, Delete, and Ctrl+Backspace in insert mode. Render `before`, a visible accent caret, and `after` as separate fixed-height labels, adjusting `queryLeft` so the caret remains visible. Remove the `isO`/`oo` escape branch; keep Escape, `jj`, and `jk`.

Add `Delete`, `Home`, and `End` to `capture_keys`.

- [ ] **Step 4: Replace help layout**

Build three sections named `Навигация`, `Поиск`, and `Плеер и библиотека`. Each row uses a compact key chip and concise one-line Russian description. Use current theme roles, Agave monospace, fixed heights, and widths that fit the existing central panel without wrapping.

- [ ] **Step 5: Integrate local collections and capabilities**

Show local playlists for any selected track. Show native destinations only when `ProviderView.capabilities.playlist_add` is true and the track/playlist provider identities match. Use positional local removal for duplicate-safe entry deletion. Surface remote-like synchronization errors without removing the local liked mark.

- [ ] **Step 6: Integrate Home continuation and radio**

Store Home's opaque `next` token in service state. Request `home-more` when navigation approaches the final loaded shelf, guard one in-flight request, merge/deduplicate returned shelves, and keep rendering bounded slices. Search/Home YTM activation sends the radio command; explicit playlist activation stays finite.

- [ ] **Step 7: Inspect every list surface**

Confirm result rows, library rows, playlist tracks, queue rows, Home cards, picker rows, and help rows all use fixed geometry and cannot add an undeclared line through text wrapping.

---

### Task 7: Update Repository Documentation

**Files:**
- Modify: `README.md`

**Interfaces:**
- Consumes: final command names and behavior from Tasks 1–6.
- Produces: user-facing documentation matching the implemented contracts.

- [ ] **Step 1: Update command reference**

Replace `play-playlist --start N` with `--track <provider:id>`. Add local playlist create/rename/add/remove-at/delete commands and Home continuation/radio commands exposed by the CLI.

- [ ] **Step 2: Update behavior sections**

Document mixed local playlists, aggregate Liked Music, capability-filtered native destinations, bounded SoundCloud DRM skipping, paginated Home, and YTM radio semantics.

- [ ] **Step 3: Update keybindings**

Document cursor navigation, Home/End/Delete, UTF-8-safe editing, Ctrl+Backspace, Escape/`jj`/`jk`, and removal of `oo` as an insert-mode escape.

- [ ] **Step 4: Preserve screenshots unless separately authorized**

Do not replace images or open the live panel. If screenshots no longer exactly match, describe behavior in text and leave image regeneration for a separately authorized visual-verification task.

---

### Task 8: Integration Review and Allowed Verification

**Files:**
- Review: all files changed by Tasks 1–7.

**Interfaces:**
- Consumes: complete integrated worktree.
- Produces: reviewed diffs and compile/lint evidence only; no behavioral claims without runtime observation.

- [ ] **Step 1: Review actual diffs**

Check all changed call sites for removed `PlayPlaylist.start`, old `--start`, old `playlist-at` numeric payloads, `Payload::Home(Vec<...>)`, append-only search mutations, and `oo` escape handling. Remove dead comments/helpers and update existing contract assertions that otherwise fail to compile.

- [ ] **Step 2: Check the heavy-job slot**

Run:

```bash
pgrep -af 'vitest|vue-tsc|nuxt|esbuild|tsc'
free -h
```

If a listed heavy job exists or `MemAvailable` is below approximately 8 GiB, stop and report the conflict. Do not start another heavy job.

- [ ] **Step 3: Compile under a memory cap**

If the slot is free and user systemd scopes are available, run:

```bash
systemd-run --user --scope -p MemoryMax=12G -p MemorySwapMax=2G -- \
  cargo check --workspace
```

Do not silently fall back to an uncapped command.

- [ ] **Step 4: Run static plugin lint only**

Use the repository's existing Noctalia plugin lint command if discoverable and if it does not start or reload the live shell. Do not create a substitute script.

- [ ] **Step 5: Report verification boundaries**

Report compile/lint output exactly. State that playback, Home continuation, radio refill, and rendered layout remain runtime-unobserved unless the user separately authorizes an applicable verification path.
