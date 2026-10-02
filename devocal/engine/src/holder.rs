//! Session holder: lowers every audio session of the player's process tree to
//! [`HELD_VOLUME`] while the engine plays the captured audio itself, follows sessions that
//! appear or are turned up later, releases them again, keeps the crash-safe restore file, and
//! computes the gains the audio threads need. It owns no threads: every time is passed in
//! (microseconds) and all work happens inside the calls.
//!
//! Lifecycle: `begin_attach` (restore leftovers, read originals, write the restore file) →
//! `tick` every millisecond (4-step volume ramp at 0/10/20/30 ms, then `Held`) → `follow`
//! every 0.5 s while held → `begin_release` → `tick` (4-step ramp back) → `Idle`.
//!
//! Restore file: every write first reads the file and keeps the entries the holder does not
//! own (entries kept by `restore::restore` from an earlier run: awaiting the player or failed),
//! then appends the holder's own entries. A session is lowered only after a write that lists
//! it succeeded. After a release only the holder's entries that were restored (or that the
//! user changed, or that were never lowered) are removed; the file is deleted only when no
//! entry remains.
//!
//! Errors never panic and are never dropped silently:
//! - `begin_attach` returns `Err` when the sessions cannot be enumerated or the restore file
//!   cannot be written; nothing has been lowered then and the holder stays `Idle`. A session
//!   whose volume cannot be read at attach is left for `follow`, and the capture gain stays
//!   1.0 until a `follow` pass succeeds.
//! - A `set_volume` failure during the attach ramp stops the ramp and makes every later
//!   `tick` return [`HolderPhase::Failed`] until `begin_release` (the caller must release).
//! - A failure during the release ramp is returned once as [`HolderPhase::Failed`] by the
//!   `tick` on which it happened; the release continues (`begin_release` again is a no-op),
//!   ends `Idle`, and that session's entry stays in the restore file for a later restore.
//! - Failures in `follow` are counted in [`FollowReport::failed`] with the first message in
//!   [`FollowReport::error`]; until a pass succeeds the capture gain falls back to at most 1.0
//!   (no compensation), because a session that could not be lowered plays at its full level:
//!   `original / v` when it plays at a volume `v` above the reference.
//! - The outcome of every `restore::restore` the holder runs is surfaced: at attach in
//!   [`AttachReport`] (or the `Err` message), after the player exited in
//!   [`FollowReport::restore_failed`] / [`FollowReport::restore_corrupt`] (not counted in
//!   `failed`, which is about this pass's own operations).
//!
//! Gain reference (ruling 16): `original` is the smallest original volume among the lowered
//! sessions, so the compensating gain `original / HELD_VOLUME` never makes any session
//! louder than before the takeover; quieter-than-max sessions are not boosted.
//!
//! Never amplify an unlowered session: a session `follow` finds not at the held volume played
//! at its observed volume `v` until it is lowered, so `v` is recorded in the gain history
//! before the lowering and the capture gain stays at most `original / v` for the window. A
//! session the player creates on a new endpoint starts at whatever volume Windows remembers
//! for the app there (1.0 on a new device), not at the user's original.
//!
//! Default render device change ([`Holder::default_device_changed`]): the player recreates its
//! stream on the new endpoint, unlowered at first. The capture gain is 0 (silent, never
//! amplified) from the change until a `follow` pass completes without errors (so every tree
//! session is held or the user's own near-zero) and with a tree session on the new default
//! endpoint, or for at most [`DEVICE_MUTE_US`]; then the normal rules apply again.
//!
//! Parking (only the sessions that can play are held): while held, `follow` reads the default
//! render endpoint(s) ([`SessionVolumes::default_render_endpoints`]). A held tree session that
//! is inactive and on an endpoint that is known not to be a default one plays nothing; it is
//! "parked": set back to its recorded original (inaudible) and then dropped from the restore
//! file, so a crash, a player exit or Windows' per-app-per-device volume memory never leaves it
//! at the held volume. A new tree session in that state is owned parked without being lowered.
//! Never parked: an active session, a session on a default endpoint, a session on an endpoint
//! the player is playing on (the endpoints of the tree's active sessions in the latest pass
//! that saw any: a player pinned to a non-default device that pauses stops its stream there,
//! and would otherwise be parked and resume unlowered for a few milliseconds), anything while
//! the default or the playing endpoint is unknown (an `Err` or no default; no session has
//! played since the attach). A parked session is held again (written to the restore
//! file, its observed volume recorded in the gain history, then lowered) as soon as a pass
//! finds it active, on a default endpoint or on a playing endpoint: after a default device change that is the
//! event-driven pass right after the change, before the player starts its stream there; for a
//! player pinned to a non-default device the engine subscribes to the parked sessions' state
//! changes ([`Holder::parked_sessions`]) and follows within one loop pass. Parking never
//! raises `original` (it only ever decreases while held), so the capture stays conservative.

use crate::dsp::GainHistory;
use devocal_core::restore::{self, RestoreEntry, RestoreRecord, RESTORE_VERSION};
use devocal_core::sessions::{is_held, SessionInfo, SessionVolumes, HELD_VOLUME};
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Steps of the attach/release volume ramp.
pub const RAMP_STEPS: u32 = 4;
/// Time between ramp steps: steps at 0, 10, 20 and 30 ms.
pub const RAMP_STEP_US: u64 = 10_000;
/// Window of the conservative capture gain (see `GainHistory`).
pub const GAIN_WINDOW_US: u64 = 100_000;
/// Longest the capture stays silent after a default render device change.
pub const DEVICE_MUTE_US: u64 = 1_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HolderPhase {
    Idle,
    Attaching,
    Held,
    Releasing,
    /// A session volume could not be set (see the module docs); the message names it.
    Failed(String),
}

/// Result of a successful `begin_attach`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AttachReport {
    /// Entries the attach-time `restore::restore` could not restore (`RestoreOutcome::failed`);
    /// they stay in the restore file.
    pub restore_failed: usize,
    /// The restore file did not parse and was quarantined (`RestoreOutcome::corrupt`).
    pub restore_corrupt: bool,
    /// Sessions still at the held volume that were taken over from a kept restore entry with
    /// the same session identifier (their original is that entry's).
    pub adopted: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FollowReport {
    pub newly_lowered: usize,
    pub overridden: usize,
    pub player_exited: bool,
    /// Errors in this pass (session/volume lookups, `set_volume`, restore file).
    pub failed: usize,
    /// The first error message of this pass.
    pub error: Option<String>,
    /// After `player_exited`: entries `restore::restore` could not restore (kept in the file).
    pub restore_failed: usize,
    /// After `player_exited`: the restore file did not parse and was quarantined.
    pub restore_corrupt: bool,
}

impl FollowReport {
    fn fail(&mut self, message: String) {
        self.failed += 1;
        self.error.get_or_insert(message);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Idle,
    Attaching,
    Held,
    Releasing,
}

/// What the release did with one owned session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Release {
    Pending,
    Restored,
    /// Not at the volume we set: the user's setting wins.
    UserChanged,
    NotLowered,
    /// The session no longer exists (`restore::restore` can find a recreated one later).
    Gone,
    Failed,
}

/// A session the holder lowered (or is about to lower); its entry is in the restore file
/// unless it is parked.
struct Owned {
    entry: RestoreEntry,
    /// The volume we last set; `None` = never lowered (e.g. `set_volume` failed) or parked.
    last_set: Option<f32>,
    /// Parked (see the module docs): at its original, not in the restore file, not held.
    parked: bool,
    /// Lowered by the attach ramp (false for a session adopted at attach, already held).
    ramp: bool,
    release_from: f32,
    release: Release,
}

impl Owned {
    fn new(entry: RestoreEntry) -> Self {
        Self {
            entry,
            last_set: None,
            parked: false,
            ramp: true,
            release_from: 0.0,
            release: Release::Pending,
        }
    }

    /// The entry stays in the restore file after the release.
    fn keep_after_release(&self) -> bool {
        matches!(
            self.release,
            Release::Pending | Release::Gone | Release::Failed
        )
    }
}

/// A session `follow` lowers (or adopts) after writing the restore file.
enum Candidate {
    /// Not held yet: lower it. `owned` is its index if it already has an entry.
    Lower {
        owned: Option<usize>,
        session: SessionInfo,
        volume: f32,
    },
    /// Already at the held volume and shares its session identifier with a session we hold
    /// (the player recreated it and Windows applied the persisted volume): record it with
    /// that session's original so the release restores it.
    Adopt { session: SessionInfo, original: f32 },
    /// A parked session that must be held again but is already at the held volume (someone
    /// set it there): list it again with its recorded original, without lowering.
    Unpark { owned: usize },
}

/// Capture silenced after a default render device change (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
struct DeviceMute {
    /// The capture gain is 0 before this time at the latest.
    until_us: u64,
    /// The new default render endpoint (`None`: none, or its id was unreadable).
    endpoint: Option<String>,
}

pub struct Holder<S: SessionVolumes> {
    sessions: S,
    restore_path: PathBuf,
    pid: u32,
    created_at: u64,
    stage: Stage,
    ramp_start_us: u64,
    ramp_step: u32,
    owned: Vec<Owned>,
    /// Gain reference: the smallest original volume among the lowered sessions (1.0 if none),
    /// so no session comes out louder than its own original (ruling 16). Only ever decreases
    /// while held.
    original: f32,
    /// Some session has been lowered since the attach (`original` comes from sessions).
    lowered_any: bool,
    gains: GainHistory,
    /// The effective volume last recorded in `gains` (on the scale of `original`).
    effective: f32,
    gain_from: f32,
    output_gain: f32,
    output_from: f32,
    attenuation: f32,
    epoch: u64,
    /// Sticky: a `set_volume` failed during the attach ramp.
    attach_failure: Option<String>,
    /// One-shot: an error during the release, reported by the next `tick`.
    release_error: Option<String>,
    /// A session's volume could not be read at attach (it was not lowered).
    attach_unread: bool,
    /// Session identifiers of kept restore entries replaced by our own entries at attach.
    absorbed: HashSet<String>,
    /// Capture silenced after a default render device change.
    device_mute: Option<DeviceMute>,
    /// Incremented whenever the set of parked sessions changes.
    parked_epoch: u64,
    /// Endpoints the player is playing on: those of the tree's active sessions in the latest
    /// pass (or the attach) that saw any. Empty = unknown.
    playing: Vec<String>,
    /// Log lines already written (`park:<id>`, `unpark:<id>`), so a repeating failure is
    /// logged once until it clears.
    logged: HashSet<String>,
}

/// True for a volume the holder lowers: finite, above `HELD_VOLUME` and not already held.
fn should_lower(v: f32) -> bool {
    v.is_finite() && v > HELD_VOLUME && !is_held(v)
}

/// Endpoints of the active sessions in `tree`, without duplicates.
fn active_endpoints(tree: &[SessionInfo]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in tree.iter().filter(|s| s.active) {
        if !out.contains(&s.endpoint_id) {
            out.push(s.endpoint_id.clone());
        }
    }
    out
}

/// Geometric interpolation from `from` to `to`: step 0 is `from`, step `RAMP_STEPS` exactly `to`.
fn ramp_value(from: f32, to: f32, step: u32) -> f32 {
    if step >= RAMP_STEPS || !from.is_finite() || from <= 0.0 || !to.is_finite() || to <= 0.0 {
        return to;
    }
    if step == 0 {
        return from;
    }
    let ratio = f64::from(to) / f64::from(from);
    (f64::from(from) * ratio.powf(f64::from(step) / f64::from(RAMP_STEPS))) as f32
}

/// `v` is (still) the volume `set` we wrote.
fn same_volume(v: f32, set: f32) -> bool {
    (v - set).abs() <= 1.0e-6_f32.max(set.abs() * 1.0e-4)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Rewrites the restore file as the entries already in it (kept from earlier runs) whose
/// instance id is not in `owned_ids` and whose session identifier is not in `absorbed`,
/// followed by `ours`; deletes it when nothing remains. An unreadable or corrupt file is an
/// error (never overwritten).
fn update_file(
    path: &Path,
    ours: &[RestoreEntry],
    owned_ids: &HashSet<String>,
    absorbed: &HashSet<String>,
) -> io::Result<()> {
    let existing = restore::read(path)?.map(|r| r.entries).unwrap_or_default();
    let mut entries: Vec<RestoreEntry> = existing
        .into_iter()
        .filter(|e| {
            !owned_ids.contains(&e.instance_id)
                && (e.session_identifier.is_empty() || !absorbed.contains(&e.session_identifier))
        })
        .collect();
    entries.extend_from_slice(ours);
    if entries.is_empty() {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    } else {
        restore::write_atomic(
            path,
            &RestoreRecord {
                version: RESTORE_VERSION,
                entries,
            },
        )
    }
}

impl<S: SessionVolumes> Holder<S> {
    pub fn new(sessions: S, restore_path: PathBuf) -> Self {
        Self {
            sessions,
            restore_path,
            pid: 0,
            created_at: 0,
            stage: Stage::Idle,
            ramp_start_us: 0,
            ramp_step: 0,
            owned: Vec::new(),
            original: 1.0,
            lowered_any: false,
            gains: GainHistory::new(GAIN_WINDOW_US),
            effective: 1.0,
            gain_from: 1.0,
            output_gain: 0.0,
            output_from: 0.0,
            attenuation: 1.0,
            epoch: 0,
            attach_failure: None,
            release_error: None,
            attach_unread: false,
            absorbed: HashSet::new(),
            device_mute: None,
            parked_epoch: 0,
            playing: Vec::new(),
            logged: HashSet::new(),
        }
    }

    pub fn sessions(&self) -> &S {
        &self.sessions
    }

    /// Current phase without advancing anything (a pending one-shot release error is not shown).
    pub fn phase(&self) -> HolderPhase {
        if let Some(e) = &self.attach_failure {
            return HolderPhase::Failed(e.clone());
        }
        match self.stage {
            Stage::Idle => HolderPhase::Idle,
            Stage::Attaching => HolderPhase::Attaching,
            Stage::Held => HolderPhase::Held,
            Stage::Releasing => HolderPhase::Releasing,
        }
    }

    /// Starts holding the process tree of `pid` (created at `created_at`, FILETIME).
    ///
    /// First runs `restore::restore` on the restore file, so a relaunched player whose
    /// session came back at the persisted held volume gets its real original back before
    /// originals are read. Then records every session whose volume is above `HELD_VOLUME`
    /// and writes the restore file. Nothing is lowered here; the ramp runs in `tick`.
    ///
    /// A session still at the held volume whose session identifier matches an entry the
    /// restore kept (it could not be restored, or is awaiting the player) is adopted: owned
    /// with that entry's original, not lowered again (it already is), and our entry replaces
    /// the kept one in the file (ruling 15). Other sessions at or below `HELD_VOLUME` are the
    /// user's own near-zero and are left alone. A session whose volume cannot be read now is
    /// picked up by `follow`.
    ///
    /// The restore outcome and the adoptions are returned in [`AttachReport`]; on `Err` the
    /// restore outcome is appended to the message when it was not clean.
    pub fn begin_attach(
        &mut self,
        pid: u32,
        created_at: u64,
        now_us: u64,
    ) -> Result<AttachReport, String> {
        if self.stage != Stage::Idle {
            return Err(format!("cannot attach while {:?}", self.stage));
        }
        let outcome = restore::restore(&self.restore_path, &self.sessions);
        let mut report = AttachReport {
            restore_failed: outcome.failed,
            restore_corrupt: outcome.corrupt,
            adopted: 0,
        };
        let with_restore = |msg: String| -> String {
            if outcome.failed > 0 || outcome.corrupt {
                format!(
                    "{msg} (restore before attach: {} entries failed, corrupt file: {})",
                    outcome.failed, outcome.corrupt
                )
            } else {
                msg
            }
        };
        // An unreadable file is already counted in `restore_failed`; adoption then finds
        // nothing, and the write below fails if anything is to be lowered.
        let kept = restore::read(&self.restore_path)
            .ok()
            .flatten()
            .map(|r| r.entries)
            .unwrap_or_default();
        let tree = self
            .sessions
            .sessions_for_tree(pid)
            .map_err(|e| with_restore(format!("enumerating the player's sessions failed: {e}")))?;
        let mut owned = Vec::new();
        let mut absorbed = HashSet::new();
        let mut unread = false;
        for s in &tree {
            match self.sessions.volume(&s.instance_id) {
                Ok(v) if should_lower(v) => {
                    owned.push(Owned::new(self.entry_for(s, v, pid, created_at)));
                }
                Ok(v) if is_held(v) && !s.session_identifier.is_empty() => {
                    let Some(k) = kept
                        .iter()
                        .find(|k| k.session_identifier == s.session_identifier)
                    else {
                        continue; // the user's own near-zero: left alone
                    };
                    let mut entry = self.entry_for(s, k.original_volume, pid, created_at);
                    entry.original_mute = k.original_mute;
                    let mut o = Owned::new(entry);
                    o.last_set = Some(HELD_VOLUME);
                    o.ramp = false;
                    owned.push(o);
                    absorbed.insert(s.session_identifier.clone());
                    report.adopted += 1;
                }
                Ok(_) => {}
                // Picked up by `follow`; until then it may play at full level.
                Err(_) => unread = true,
            }
        }
        if !owned.is_empty() {
            let ours: Vec<RestoreEntry> = owned.iter().map(|o| o.entry.clone()).collect();
            let ids = ours.iter().map(|e| e.instance_id.clone()).collect();
            update_file(&self.restore_path, &ours, &ids, &absorbed)
                .map_err(|e| with_restore(format!("writing the restore file failed: {e}")))?;
        }
        self.absorbed = absorbed;
        self.original = owned
            .iter()
            .map(|o| o.entry.original_volume)
            .reduce(f32::min)
            .unwrap_or(1.0);
        self.owned = owned;
        self.pid = pid;
        self.created_at = created_at;
        self.stage = Stage::Attaching;
        self.ramp_start_us = now_us;
        self.ramp_step = 0;
        self.lowered_any = false;
        self.gains = GainHistory::new(GAIN_WINDOW_US);
        self.effective = self.original;
        self.output_gain = 0.0;
        self.attach_failure = None;
        self.release_error = None;
        self.attach_unread = unread;
        self.device_mute = None;
        self.playing = active_endpoints(&tree);
        self.logged.clear();
        Ok(report)
    }

    /// Starts the ramp back to the original volumes. No-op when idle or already releasing.
    /// The attenuation reported to the app returns to 1.0 at once (never over-amplified).
    pub fn begin_release(&mut self, now_us: u64) {
        if matches!(self.stage, Stage::Idle | Stage::Releasing) {
            return;
        }
        self.stage = Stage::Releasing;
        self.ramp_start_us = now_us;
        self.ramp_step = 0;
        self.attach_failure = None;
        self.gain_from = self.effective;
        self.output_from = self.output_gain;
        let mut unparked = false;
        for o in &mut self.owned {
            o.release_from = o.last_set.unwrap_or(o.entry.original_volume);
            o.release = if o.last_set.is_some() {
                Release::Pending
            } else {
                Release::NotLowered
            };
            // Already at its original: nothing to restore, and no longer watched.
            unparked |= std::mem::take(&mut o.parked);
        }
        if unparked {
            self.parked_epoch += 1;
        }
        self.set_attenuation(1.0);
    }

    /// Advances the attach or release ramp; call it every millisecond.
    pub fn tick(&mut self, now_us: u64) -> HolderPhase {
        match self.stage {
            Stage::Attaching if self.attach_failure.is_none() => self.attach_step(now_us),
            Stage::Releasing => self.release_step(now_us),
            _ => {}
        }
        if let Some(e) = self.release_error.take() {
            return HolderPhase::Failed(e);
        }
        self.phase()
    }

    /// Every 0.5 s while held: lowers sessions that appeared in the player's process tree
    /// (restore file first), lowers again sessions that are no longer at the held volume, and
    /// detects that the player exited (then restores through `restore::restore` and goes idle).
    /// Does nothing in other phases.
    pub fn follow(&mut self, now_us: u64) -> FollowReport {
        let mut r = FollowReport::default();
        if self.stage != Stage::Held {
            return r;
        }
        match self.sessions.process_created(self.pid) {
            Ok(Some(c)) if c == self.created_at => {}
            Ok(_) => {
                self.player_exited(&mut r);
                return r;
            }
            Err(e) => r.fail(format!("querying the player process failed: {e}")),
        }
        let tree = match self.sessions.sessions_for_tree(self.pid) {
            Ok(t) => t,
            Err(e) => {
                r.fail(format!("enumerating the player's sessions failed: {e}"));
                self.settle_gain(now_us, Some(self.original), false);
                return r;
            }
        };

        let mut degraded = false;
        // The loudest effective volume a session left unlowered by this pass plays at (an
        // unreadable one is assumed at `original`).
        let mut unlowered = self.original;
        let mut spiked = false;
        let mut candidates = Vec::new();
        // Default render endpoint(s); `None` = unknown (nothing is parked then).
        let defaults = self
            .sessions
            .default_render_endpoints()
            .ok()
            .filter(|d| !d.is_empty());
        let on_default = |s: &SessionInfo| {
            defaults
                .as_ref()
                .is_some_and(|d| d.contains(&s.endpoint_id))
        };
        let now_playing = active_endpoints(&tree);
        if !now_playing.is_empty() {
            self.playing = now_playing;
        }
        let playing = self.playing.clone();
        let on_playing = |s: &SessionInfo| playing.contains(&s.endpoint_id);
        // Plays nothing and cannot start playing unnoticed: inactive, default and playing
        // endpoints known, on neither.
        let parkable = |s: &SessionInfo| {
            !s.active
                && defaults.is_some()
                && !playing.is_empty()
                && !on_default(s)
                && !on_playing(s)
        };
        let mut parked_any = false;
        let mut new_parked: Vec<(SessionInfo, f32)> = Vec::new();
        // Sessions held by this pass (lowered or listed again), for the device-mute check.
        let mut held_now: Vec<String> = Vec::new();
        for s in &tree {
            let v = match self.sessions.volume(&s.instance_id) {
                Ok(v) => v,
                Err(e) => {
                    r.fail(format!("reading session {} failed: {e}", s.instance_id));
                    degraded = true;
                    continue;
                }
            };
            let idx = self
                .owned
                .iter()
                .position(|o| o.entry.instance_id == s.instance_id);
            match idx {
                Some(i) if self.owned[i].parked => {
                    if !(s.active || on_default(s) || on_playing(s)) {
                        continue; // still cannot play
                    }
                    if should_lower(v) {
                        candidates.push(Candidate::Lower {
                            owned: Some(i),
                            session: s.clone(),
                            volume: v,
                        });
                    } else if is_held(v) {
                        candidates.push(Candidate::Unpark { owned: i });
                    }
                    // Otherwise the user's own near-zero: it stays parked.
                }
                Some(i) if self.owned[i].last_set.is_some() => {
                    if is_held(v) {
                        if parkable(s) {
                            // Back to its original first, then out of the restore file (below):
                            // a crash in between leaves an entry for a session that is no
                            // longer held, which a restore leaves alone.
                            let id = self.owned[i].entry.instance_id.clone();
                            let original = self.owned[i].entry.original_volume;
                            // A failure is not an error of the pass: the session simply stays
                            // held, which is safe. Logged once until it succeeds.
                            match self.sessions.set_volume(&id, original) {
                                Ok(()) => {
                                    let o = &mut self.owned[i];
                                    o.parked = true;
                                    o.last_set = None;
                                    parked_any = true;
                                    self.logged.remove(&format!("park:{id}"));
                                }
                                Err(e) => self.log_once(
                                    format!("park:{id}"),
                                    format!("parking session {id} failed (it stays held): {e}"),
                                ),
                            }
                        }
                        continue;
                    }
                    r.overridden += 1;
                    // Until lowered again it played at `v`: no more gain than original / v
                    // relative to its own original for the next window.
                    let o = &self.owned[i];
                    let id = o.entry.instance_id.clone();
                    let level = self.original * v / o.entry.original_volume;
                    self.record_gain(now_us, level);
                    spiked = true;
                    match self.sessions.set_volume(&id, HELD_VOLUME) {
                        Ok(()) => self.owned[i].last_set = Some(HELD_VOLUME),
                        Err(e) => {
                            r.fail(format!("lowering session {id} again failed: {e}"));
                            degraded = true;
                            unlowered = unlowered.max(level);
                        }
                    }
                }
                Some(i) if is_held(v) => {
                    // Has an entry but was never lowered by us, yet is held now: treat it as
                    // held so the release restores it to the recorded original.
                    self.owned[i].last_set = Some(HELD_VOLUME);
                }
                owned => {
                    if should_lower(v) && owned.is_none() && parkable(s) {
                        // A new session that cannot play: owned parked, never lowered.
                        new_parked.push((s.clone(), v));
                    } else if should_lower(v) {
                        candidates.push(Candidate::Lower {
                            owned,
                            session: s.clone(),
                            volume: v,
                        });
                    } else if owned.is_none() && is_held(v) {
                        if let Some(original) = self.held_original(&s.session_identifier) {
                            candidates.push(Candidate::Adopt {
                                session: s.clone(),
                                original,
                            });
                        }
                    }
                }
            }
        }

        for (session, v) in new_parked {
            let entry = self.entry_for(&session, v, self.pid, self.created_at);
            let mut o = Owned::new(entry);
            o.parked = true;
            o.ramp = false;
            self.owned.push(o);
            parked_any = true;
        }
        if parked_any {
            self.parked_epoch += 1;
        }
        let mut file_written = false;
        if !candidates.is_empty() {
            match self.record_candidates(&candidates) {
                Err(e) => {
                    r.fail(format!("writing the restore file failed: {e}"));
                    degraded = true;
                    for c in &candidates {
                        match c {
                            Candidate::Lower { volume, .. } => unlowered = unlowered.max(*volume),
                            Candidate::Unpark { owned } => {
                                let id = self.owned[*owned].entry.instance_id.clone();
                                self.log_once(
                                    format!("unpark:{id}"),
                                    format!(
                                        "parked session {id} is at the held volume but could not \
                                         be listed in the restore file ({e}); retrying"
                                    ),
                                );
                            }
                            Candidate::Adopt { .. } => {}
                        }
                    }
                }
                Ok(targets) => {
                    file_written = true;
                    let mut lowered_min: Option<f32> = None;
                    for (i, lower) in targets {
                        let Some(v) = lower else {
                            // Adopted or listed again, already held.
                            let key = format!("unpark:{}", self.owned[i].entry.instance_id);
                            self.logged.remove(&key);
                            self.owned[i].last_set = Some(HELD_VOLUME);
                            held_now.push(self.owned[i].entry.instance_id.clone());
                            continue;
                        };
                        // It played at `v` until now (a new session starts at whatever volume
                        // Windows remembers for the app on its endpoint, 1.0 on a new device):
                        // no more gain than original / v for the next window. Recorded before
                        // the lowering, like the re-lower path above.
                        self.record_gain(now_us, v);
                        spiked = true;
                        let id = self.owned[i].entry.instance_id.clone();
                        match self.sessions.set_volume(&id, HELD_VOLUME) {
                            Ok(()) => {
                                self.owned[i].last_set = Some(HELD_VOLUME);
                                held_now.push(id.clone());
                                r.newly_lowered += 1;
                                let o = self.owned[i].entry.original_volume;
                                lowered_min = Some(lowered_min.map_or(o, |m| m.min(o)));
                            }
                            Err(e) => {
                                r.fail(format!("lowering session {id} failed: {e}"));
                                degraded = true;
                                unlowered = unlowered.max(v);
                            }
                        }
                    }
                    if let Some(m) = lowered_min {
                        // Ruling 16: the reference is the quietest lowered original and only
                        // goes down. Gain entries recorded against a larger value now yield
                        // less gain, so the window stays conservative.
                        self.original = if self.lowered_any {
                            self.original.min(m)
                        } else {
                            m
                        };
                        self.lowered_any = true;
                        self.set_attenuation(HELD_VOLUME / self.original);
                    }
                }
            }
        }
        if parked_any && !file_written {
            // Drop the parked sessions' entries (they are back at their originals).
            if let Err(e) = self.write_owned() {
                r.fail(format!(
                    "updating the restore file after parking failed: {e}"
                ));
            }
        }
        self.settle_gain(now_us, degraded.then_some(unlowered), spiked);
        if let Some(m) = &self.device_mute {
            // The player is held on the new endpoint: a held session there that plays, or that
            // this pass lowered before it could start (a stale inactive session does not count).
            let on_new_endpoint = m.endpoint.as_ref().is_none_or(|ep| {
                tree.iter().any(|s| {
                    &s.endpoint_id == ep
                        && self.holds(&s.instance_id)
                        && (s.active || held_now.contains(&s.instance_id))
                })
            });
            if now_us >= m.until_us || (r.failed == 0 && !degraded && on_new_endpoint) {
                self.device_mute = None;
            }
        }
        r
    }

    /// The default render device changed (to `endpoint`, `None` if there is none or its id is
    /// unknown): silence the capture until the player is held on it (see the module docs).
    /// Only while attaching or held; a later change restarts the timeout.
    pub fn default_device_changed(&mut self, now_us: u64, endpoint: Option<String>) {
        if matches!(self.stage, Stage::Attaching | Stage::Held) {
            self.device_mute = Some(DeviceMute {
                until_us: now_us.saturating_add(DEVICE_MUTE_US),
                endpoint,
            });
        }
    }

    /// Instance ids of the parked sessions (the engine watches their state changes).
    pub fn parked_sessions(&self) -> Vec<String> {
        self.owned
            .iter()
            .filter(|o| o.parked)
            .map(|o| o.entry.instance_id.clone())
            .collect()
    }

    /// Incremented whenever the set returned by [`Holder::parked_sessions`] changes.
    pub fn parked_epoch(&self) -> u64 {
        self.parked_epoch
    }

    /// The session is ours and held (not parked, last set to the held volume).
    fn holds(&self, instance_id: &str) -> bool {
        self.owned.iter().any(|o| {
            o.entry.instance_id == instance_id && !o.parked && o.last_set.is_some_and(is_held)
        })
    }

    /// Rewrites the restore file with the owned entries that are not parked.
    fn write_owned(&self) -> io::Result<()> {
        let ours: Vec<RestoreEntry> = self
            .owned
            .iter()
            .filter(|o| !o.parked)
            .map(|o| o.entry.clone())
            .collect();
        let ids = self
            .owned
            .iter()
            .map(|o| o.entry.instance_id.clone())
            .collect();
        update_file(&self.restore_path, &ours, &ids, &self.absorbed)
    }

    /// Gain for the captured audio: `original / max(effective volume in the last 100 ms)`,
    /// with `original` the quietest lowered session's original, so no session is louder than
    /// it was before the takeover. 0 while silenced after a default device change.
    pub fn capture_gain(&self, now_us: u64) -> f32 {
        if self
            .device_mute
            .as_ref()
            .is_some_and(|m| now_us < m.until_us)
        {
            return 0.0;
        }
        self.gains.conservative_gain(self.original, now_us)
    }

    /// `HELD_VOLUME / original` (quietest lowered original) while sessions are held; 1.0
    /// otherwise.
    pub fn attenuation(&self) -> f32 {
        self.attenuation
    }

    /// Incremented every time the value of `attenuation()` changes.
    pub fn attenuation_epoch(&self) -> u64 {
        self.epoch
    }

    /// Fade gain (0..1) for the engine's own output, stepped with the ramp: `k / 4` after
    /// attach step `k`, back down to 0 over the release steps.
    pub fn output_gain(&self) -> f32 {
        self.output_gain
    }

    fn due_step(&self, now_us: u64) -> u32 {
        if now_us < self.ramp_start_us {
            return 0;
        }
        let k = (now_us - self.ramp_start_us) / RAMP_STEP_US + 1;
        k.min(u64::from(RAMP_STEPS)) as u32
    }

    fn attach_step(&mut self, now_us: u64) {
        let step = self.due_step(now_us);
        if step <= self.ramp_step {
            return;
        }
        self.ramp_step = step;
        self.output_gain = step as f32 / RAMP_STEPS as f32;
        if !self.owned.is_empty() {
            self.record_gain(now_us, ramp_value(self.original, HELD_VOLUME, step));
        }
        for o in self.owned.iter_mut().filter(|o| o.ramp) {
            let v = ramp_value(o.entry.original_volume, HELD_VOLUME, step);
            match self.sessions.set_volume(&o.entry.instance_id, v) {
                Ok(()) => o.last_set = Some(v),
                Err(e) => {
                    self.attach_failure = Some(format!(
                        "lowering session {} failed: {e}",
                        o.entry.instance_id
                    ));
                    break;
                }
            }
        }
        if self.attach_failure.is_some() {
            // Some sessions are not lowered: no compensation until released.
            self.record_gain(now_us, self.original);
            return;
        }
        if step == RAMP_STEPS {
            self.stage = Stage::Held;
            if !self.owned.is_empty() {
                self.lowered_any = true;
                self.set_attenuation(HELD_VOLUME / self.original);
            }
            if self.attach_unread {
                // A session's volume could not be read, so it was not lowered: no
                // compensation until a `follow` pass succeeds.
                self.record_gain(now_us, self.original);
            }
        }
    }

    fn release_step(&mut self, now_us: u64) {
        let step = self.due_step(now_us);
        if step <= self.ramp_step {
            return;
        }
        self.ramp_step = step;
        self.output_gain = self.output_from * (1.0 - step as f32 / RAMP_STEPS as f32);
        // Volumes only rise here; the gain follows at once (max over the window).
        self.record_gain(now_us, ramp_value(self.gain_from, self.original, step));
        let mut error = None;
        for o in self
            .owned
            .iter_mut()
            .filter(|o| o.release == Release::Pending)
        {
            let Some(last) = o.last_set else {
                o.release = Release::NotLowered;
                continue;
            };
            let id = &o.entry.instance_id;
            match self.sessions.volume(id) {
                Ok(v) if same_volume(v, last) || is_held(v) => {
                    let target = ramp_value(o.release_from, o.entry.original_volume, step);
                    match self.sessions.set_volume(id, target) {
                        Ok(()) => {
                            o.last_set = Some(target);
                            if step == RAMP_STEPS {
                                o.release = Release::Restored;
                            }
                        }
                        Err(e) => {
                            o.release = Release::Failed;
                            error.get_or_insert(format!("restoring session {id} failed: {e}"));
                        }
                    }
                }
                Ok(_) => o.release = Release::UserChanged,
                Err(e) => match self.sessions.session(id) {
                    Ok(None) => o.release = Release::Gone,
                    _ => {
                        o.release = Release::Failed;
                        error.get_or_insert(format!("reading session {id} failed: {e}"));
                    }
                },
            }
        }
        if let Some(e) = error {
            self.release_error.get_or_insert(e);
        }
        if step == RAMP_STEPS {
            self.finish_release();
        }
    }

    /// Removes the holder's settled entries from the restore file (keeping failed/gone ones
    /// and every foreign entry) and goes idle.
    fn finish_release(&mut self) {
        if !self.owned.is_empty() {
            let keep: Vec<RestoreEntry> = self
                .owned
                .iter()
                .filter(|o| o.keep_after_release())
                .map(|o| o.entry.clone())
                .collect();
            let ids = self
                .owned
                .iter()
                .map(|o| o.entry.instance_id.clone())
                .collect();
            if let Err(e) = update_file(&self.restore_path, &keep, &ids, &self.absorbed) {
                self.release_error
                    .get_or_insert(format!("updating the restore file failed: {e}"));
            }
        }
        self.clear_owned();
        self.stage = Stage::Idle;
        self.output_gain = 0.0;
        self.lowered_any = false;
    }

    /// The player process exited (or its pid was reused): restore through the restore file,
    /// which also handles sessions found again by their session identifier, and go idle.
    fn player_exited(&mut self, r: &mut FollowReport) {
        r.player_exited = true;
        let out = restore::restore(&self.restore_path, &self.sessions);
        r.restore_failed = out.failed;
        r.restore_corrupt = out.corrupt;
        if out.failed > 0 || out.corrupt {
            r.error.get_or_insert(format!(
                "restore after the player exited: {} entries not restored yet (kept in the \
                 restore file), corrupt file: {}",
                out.failed, out.corrupt
            ));
        }
        self.clear_owned();
        self.stage = Stage::Idle;
        self.output_gain = 0.0;
        self.lowered_any = false;
        self.attach_failure = None;
        self.gains = GainHistory::new(GAIN_WINDOW_US);
        self.effective = self.original;
        self.device_mute = None;
        self.set_attenuation(1.0);
    }

    /// Writes the restore file with the candidates added (or their original updated, or
    /// listed again when parked) and, on success, records them as owned (no longer parked).
    /// Returns `(owned index, Some(volume) to lower, or None when adopted / listed again
    /// already held)` per candidate.
    fn record_candidates(
        &mut self,
        candidates: &[Candidate],
    ) -> io::Result<Vec<(usize, Option<f32>)>> {
        let mut entries: Vec<RestoreEntry> = self.owned.iter().map(|o| o.entry.clone()).collect();
        // Which entries the file lists: every owned one that is not parked, plus candidates.
        let mut listed: Vec<bool> = self.owned.iter().map(|o| !o.parked).collect();
        let mut targets = Vec::with_capacity(candidates.len());
        for c in candidates {
            match c {
                Candidate::Lower {
                    owned: Some(i),
                    volume,
                    ..
                } => {
                    entries[*i].original_volume = *volume;
                    listed[*i] = true;
                    targets.push((*i, Some(*volume)));
                }
                Candidate::Lower {
                    owned: None,
                    session,
                    volume,
                } => {
                    targets.push((entries.len(), Some(*volume)));
                    entries.push(self.entry_for(session, *volume, self.pid, self.created_at));
                    listed.push(true);
                }
                Candidate::Adopt { session, original } => {
                    targets.push((entries.len(), None));
                    entries.push(self.entry_for(session, *original, self.pid, self.created_at));
                    listed.push(true);
                }
                Candidate::Unpark { owned } => {
                    listed[*owned] = true;
                    targets.push((*owned, None));
                }
            }
        }
        let ours: Vec<RestoreEntry> = entries
            .iter()
            .zip(&listed)
            .filter(|(_, l)| **l)
            .map(|(e, _)| e.clone())
            .collect();
        let ids = entries.iter().map(|e| e.instance_id.clone()).collect();
        update_file(&self.restore_path, &ours, &ids, &self.absorbed)?;
        let known = self.owned.len();
        let mut unparked = false;
        for ((o, e), l) in self.owned.iter_mut().zip(&entries).zip(&listed) {
            o.entry = e.clone();
            if *l && o.parked {
                o.parked = false;
                unparked = true;
            }
        }
        if unparked {
            self.parked_epoch += 1;
        }
        self.owned
            .extend(entries.into_iter().skip(known).map(Owned::new));
        Ok(targets)
    }

    /// Writes `message` to the engine log unless the line for `key` was already written (and
    /// not cleared since).
    fn log_once(&mut self, key: String, message: String) {
        if self.logged.insert(key) {
            eprintln!("devocal engine: {message}");
        }
    }

    /// Forgets every owned session (bumping the parked epoch if any was parked).
    fn clear_owned(&mut self) {
        if self.owned.iter().any(|o| o.parked) {
            self.parked_epoch += 1;
        }
        self.owned.clear();
    }

    /// Original volume of a held session with this (non-empty) session identifier.
    fn held_original(&self, session_identifier: &str) -> Option<f32> {
        if session_identifier.is_empty() {
            return None;
        }
        self.owned
            .iter()
            .find(|o| o.last_set.is_some() && o.entry.session_identifier == session_identifier)
            .map(|o| o.entry.original_volume)
    }

    /// Restore entry for `s`. Sessions of child processes get their own creation time; if
    /// it cannot be read, 0 makes `restore::restore` treat the process as exited and find
    /// the session by its session identifier instead (it is still restored).
    fn entry_for(
        &self,
        s: &SessionInfo,
        original: f32,
        root_pid: u32,
        root_created: u64,
    ) -> RestoreEntry {
        let created_at = if s.pid == root_pid {
            root_created
        } else {
            match self.sessions.process_created(s.pid) {
                Ok(Some(t)) => t,
                _ => 0,
            }
        };
        RestoreEntry {
            pid: s.pid,
            created_at,
            instance_id: s.instance_id.clone(),
            original_volume: original,
            // Mute is never changed by the holder; an unreadable state is recorded as unmuted.
            original_mute: self.sessions.muted(&s.instance_id).unwrap_or(false),
            saved_at_ms: now_ms(),
            session_identifier: s.session_identifier.clone(),
        }
    }

    /// After a follow pass: the steady effective volume is `HELD_VOLUME` when every lowered
    /// session is held; with `degraded` (some session could not be read or lowered) it is that
    /// level, at least `original` (gain 1, no compensation), so a session still playing above
    /// the reference is not amplified either; with nothing lowered yet it is `original`.
    /// Recorded after any spike so the spike only covers its window.
    fn settle_gain(&mut self, now_us: u64, degraded: Option<f32>, spiked: bool) {
        let target = match degraded {
            Some(level) => level.max(self.original),
            None if !self.lowered_any => self.original,
            None => HELD_VOLUME,
        };
        if spiked || target != self.effective {
            self.record_gain(now_us, target);
        }
    }

    fn record_gain(&mut self, now_us: u64, effective: f32) {
        self.gains.set(now_us, effective);
        self.effective = effective;
    }

    fn set_attenuation(&mut self, value: f32) {
        if value != self.attenuation {
            self.attenuation = value;
            self.epoch += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devocal_core::restore::{self, RestoreEntry, RestoreRecord};
    use devocal_core::sessions::{FakeSessions, SessionInfo, HELD_VOLUME};
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const PID: u32 = 100;
    const CREATED: u64 = 555;
    const MS: u64 = 1_000;
    const T0: u64 = 1_000_000;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!(
                "devocal-holder-test-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
        fn file(&self) -> PathBuf {
            self.0.join("devocal-restore.json")
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn info(id: &str, pid: u32) -> SessionInfo {
        SessionInfo {
            instance_id: id.into(),
            session_identifier: format!("ident:{id}"),
            pid,
            endpoint_id: "ep".into(),
            active: true,
        }
    }

    fn fake(sessions: &[(&str, f32)]) -> FakeSessions {
        let f = FakeSessions::new();
        f.set_process_created(PID, Some(CREATED));
        for &(id, v) in sessions {
            f.add_session(info(id, PID), v, false);
        }
        f
    }

    fn holder(dir: &TempDir, sessions: &[(&str, f32)]) -> Holder<FakeSessions> {
        Holder::new(fake(sessions), dir.file())
    }

    /// Attaches at `t0` and ticks the four ramp steps; returns the last phase.
    fn attach(h: &mut Holder<FakeSessions>, t0: u64) -> HolderPhase {
        h.begin_attach(PID, CREATED, t0).unwrap();
        (0..4).map(|k| h.tick(t0 + k * 10 * MS)).last().unwrap()
    }

    /// Releases at `t0` and ticks the four ramp steps; returns the last phase.
    fn release(h: &mut Holder<FakeSessions>, t0: u64) -> HolderPhase {
        h.begin_release(t0);
        (0..4).map(|k| h.tick(t0 + k * 10 * MS)).last().unwrap()
    }

    fn vol(h: &Holder<FakeSessions>, id: &str) -> f32 {
        h.sessions().volume(id).unwrap()
    }

    fn entries(dir: &TempDir) -> Vec<RestoreEntry> {
        restore::read(&dir.file())
            .unwrap()
            .map(|r| r.entries)
            .unwrap_or_default()
    }

    fn entry_ids(dir: &TempDir) -> Vec<String> {
        entries(dir).into_iter().map(|e| e.instance_id).collect()
    }

    fn geo(from: f32, to: f32, k: u32) -> f32 {
        (f64::from(from) * (f64::from(to) / f64::from(from)).powf(f64::from(k) / 4.0)) as f32
    }

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() <= b.abs() * 1e-5
    }

    /// Records, for every `set_volume`, whether the restore file already listed that session.
    fn track_file_before_write(h: &Holder<FakeSessions>, path: PathBuf) -> Rc<RefCell<Vec<bool>>> {
        let seen = Rc::new(RefCell::new(Vec::new()));
        let s2 = seen.clone();
        h.sessions().on_set_volume(Box::new(move |id, _| {
            let listed = restore::read(&path)
                .ok()
                .flatten()
                .is_some_and(|r| r.entries.iter().any(|e| e.instance_id == id));
            s2.borrow_mut().push(listed);
        }));
        seen
    }

    #[test]
    fn attach_writes_file_before_lowering() {
        let dir = TempDir::new("order");
        let mut h = holder(&dir, &[("a", 0.5), ("b", 0.8)]);
        let seen = track_file_before_write(&h, dir.file());
        h.begin_attach(PID, CREATED, T0).unwrap();
        assert_eq!(
            h.sessions().set_volume_calls(),
            0,
            "begin_attach lowers nothing"
        );
        assert!(dir.file().exists());
        for k in 0..4 {
            h.tick(T0 + k * 10 * MS);
        }
        let seen = seen.borrow();
        assert_eq!(seen.len(), 8);
        assert!(seen.iter().all(|&listed| listed), "{seen:?}");

        let e = entries(&dir);
        assert_eq!(e.len(), 2);
        assert_eq!(
            (
                e[0].instance_id.as_str(),
                e[0].original_volume,
                e[0].pid,
                e[0].created_at
            ),
            ("a", 0.5, PID, CREATED)
        );
        assert_eq!(e[0].session_identifier, "ident:a");
        assert_eq!(
            (e[1].instance_id.as_str(), e[1].original_volume),
            ("b", 0.8)
        );
    }

    #[test]
    fn ramp_reaches_held_in_four_steps_over_30ms() {
        let dir = TempDir::new("ramp");
        let mut h = holder(&dir, &[("a", 0.5)]);
        h.begin_attach(PID, CREATED, T0).unwrap();
        assert_eq!(vol(&h, "a"), 0.5);
        assert_eq!(h.output_gain(), 0.0);
        for k in 1..=3u32 {
            let t = T0 + u64::from(k - 1) * 10 * MS;
            assert_eq!(h.tick(t), HolderPhase::Attaching);
            let expected = geo(0.5, HELD_VOLUME, k);
            assert!(
                approx(vol(&h, "a"), expected),
                "step {k}: {} vs {expected}",
                vol(&h, "a")
            );
            assert_eq!(h.output_gain(), k as f32 / 4.0);
        }
        // A tick between steps changes nothing.
        assert_eq!(h.tick(T0 + 25 * MS), HolderPhase::Attaching);
        assert_eq!(h.sessions().set_volume_calls(), 3);
        assert_eq!(h.tick(T0 + 30 * MS), HolderPhase::Held);
        assert_eq!(vol(&h, "a"), HELD_VOLUME);
        assert_eq!(h.output_gain(), 1.0);
        assert_eq!(h.tick(T0 + 40 * MS), HolderPhase::Held);
        assert_eq!(h.sessions().set_volume_calls(), 4);
    }

    #[test]
    fn release_ramps_back_in_four_steps_and_fades_out() {
        let dir = TempDir::new("release-ramp");
        let mut h = holder(&dir, &[("a", 0.5)]);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        let tr = T0 + 500 * MS;
        h.begin_release(tr);
        assert_eq!(h.attenuation(), 1.0);
        for k in 1..=3u32 {
            assert_eq!(
                h.tick(tr + u64::from(k - 1) * 10 * MS),
                HolderPhase::Releasing
            );
            let expected = geo(HELD_VOLUME, 0.5, k);
            assert!(approx(vol(&h, "a"), expected), "step {k}");
            assert_eq!(h.output_gain(), 1.0 - k as f32 / 4.0);
        }
        assert_eq!(h.tick(tr + 30 * MS), HolderPhase::Idle);
        assert_eq!(vol(&h, "a"), 0.5);
        assert_eq!(h.output_gain(), 0.0);
        assert!(!dir.file().exists());
    }

    #[test]
    fn gain_matches_original_volume() {
        let dir = TempDir::new("gain");
        let mut h = holder(&dir, &[("a", 0.3)]);
        assert_eq!(h.attenuation(), 1.0);
        assert_eq!(h.capture_gain(0), 1.0);
        let e0 = h.attenuation_epoch();
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        assert!(
            (h.attenuation() - 1e-4 / 0.3).abs() < 1e-9,
            "{}",
            h.attenuation()
        );
        assert_eq!(h.attenuation_epoch(), e0 + 1);
        let g = h.capture_gain(T0 + 131 * MS);
        assert!((g - 0.3 / 1e-4).abs() < 0.05, "{g}");
    }

    /// Ticks every 1 ms from `start` to `start + 130 ms` and checks that `capture_gain(t)` is at
    /// most `original / v` for every volume `v` the session had at any time in `[t - 100 ms, t]`.
    /// `history` holds `(since_us, volume)` for the volumes in effect before `start`.
    fn check_gain_bound(
        h: &mut Holder<FakeSessions>,
        id: &str,
        original: f32,
        start: u64,
        mut history: Vec<(u64, f32)>,
    ) {
        for ms in 0..=130 {
            let t = start + ms * MS;
            h.tick(t);
            let v = vol(h, id);
            if history.last().unwrap().1 != v {
                history.push((t, v));
            }
            let g = h.capture_gain(t);
            let window_start = t.saturating_sub(100 * MS);
            for (i, &(since, v)) in history.iter().enumerate() {
                let until = history.get(i + 1).map_or(u64::MAX, |n| n.0);
                if until < window_start || since > t {
                    continue;
                }
                assert!(
                    g <= original / v * (1.0 + 1e-5),
                    "t = +{ms} ms: gain {g} > {original} / {v}"
                );
            }
        }
    }

    #[test]
    fn gain_never_exceeds_true_during_attach() {
        let dir = TempDir::new("gain-attach");
        let mut h = holder(&dir, &[("a", 0.3)]);
        h.begin_attach(PID, CREATED, T0).unwrap();
        check_gain_bound(&mut h, "a", 0.3, T0, vec![(0, 0.3)]);
        assert_eq!(vol(&h, "a"), HELD_VOLUME);
        let g = h.capture_gain(T0 + 131 * MS);
        assert!((g - 3000.0).abs() < 0.05, "{g}");
    }

    #[test]
    fn gain_never_exceeds_true_during_release() {
        let dir = TempDir::new("gain-release");
        let mut h = holder(&dir, &[("a", 0.3)]);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        let tr = T0 + 500 * MS;
        assert!((h.capture_gain(tr) - 3000.0).abs() < 0.05);
        h.begin_release(tr);
        check_gain_bound(&mut h, "a", 0.3, tr, vec![(T0 + 30 * MS, HELD_VOLUME)]);
        assert_eq!(vol(&h, "a"), 0.3);
        assert!(h.capture_gain(tr + 131 * MS) <= 1.0 + 1e-6);
    }

    #[test]
    fn near_zero_session_is_left_alone() {
        let dir = TempDir::new("near-zero");
        let mut h = holder(&dir, &[("a", 0.6), ("z", 0.0), ("tiny", 5e-5)]);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        assert_eq!(vol(&h, "z"), 0.0);
        assert_eq!(vol(&h, "tiny"), 5e-5);
        assert!(h.sessions().writes().iter().all(|(id, _)| id == "a"));
        assert_eq!(entry_ids(&dir), vec!["a"]);
        assert!((h.attenuation() - 1e-4 / 0.6).abs() < 1e-9);
        // Following does not lower them either.
        assert_eq!(h.follow(T0 + 500 * MS), FollowReport::default());
        assert_eq!(vol(&h, "tiny"), 5e-5);
    }

    #[test]
    fn session_appearing_later_is_lowered() {
        let dir = TempDir::new("late");
        let mut h = holder(&dir, &[]);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        assert!(!dir.file().exists());
        assert_eq!(h.attenuation(), 1.0);
        assert_eq!(
            h.capture_gain(T0 + 200 * MS),
            1.0,
            "nothing lowered, nothing to undo"
        );
        let e0 = h.attenuation_epoch();

        h.sessions().add_session(info("late", PID), 0.8, false);
        let seen = track_file_before_write(&h, dir.file());
        let t = T0 + 500 * MS;
        let r = h.follow(t);
        assert_eq!(
            r,
            FollowReport {
                newly_lowered: 1,
                ..Default::default()
            }
        );
        assert_eq!(vol(&h, "late"), HELD_VOLUME);
        assert_eq!(*seen.borrow(), vec![true]);
        let e = entries(&dir);
        assert_eq!(e.len(), 1);
        assert_eq!(
            (e[0].instance_id.as_str(), e[0].original_volume),
            ("late", 0.8)
        );
        assert!((h.attenuation() - 1e-4 / 0.8).abs() < 1e-9);
        assert_eq!(h.attenuation_epoch(), e0 + 1);
        assert!(h.capture_gain(t) <= 1.0 + 1e-6);
        assert!(h.capture_gain(t + 100 * MS) <= 1.0 + 1e-6);
        assert!((h.capture_gain(t + 101 * MS) - 8000.0).abs() < 0.1);
    }

    #[test]
    fn overridden_session_is_lowered_again() {
        let dir = TempDir::new("override");
        let mut h = holder(&dir, &[("a", 0.5)]);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        let (e0, att) = (h.attenuation_epoch(), h.attenuation());
        h.sessions().set_volume("a", 0.6).unwrap();
        let t = T0 + 500 * MS;
        let r = h.follow(t);
        assert_eq!(
            r,
            FollowReport {
                overridden: 1,
                ..Default::default()
            }
        );
        assert_eq!(vol(&h, "a"), HELD_VOLUME);
        assert_eq!(h.attenuation_epoch(), e0);
        assert_eq!(h.attenuation(), att);
        // The louder override was audible in the capture: no boost beyond 0.5/0.6 for 100 ms.
        assert!(h.capture_gain(t) <= 0.5 / 0.6 + 1e-6);
        assert!(h.capture_gain(t + 100 * MS) <= 0.5 / 0.6 + 1e-6);
        assert!((h.capture_gain(t + 101 * MS) - 5000.0).abs() < 0.1);
        assert_eq!(h.follow(t + 500 * MS), FollowReport::default());
    }

    #[test]
    fn release_respects_user_change_and_deletes_file() {
        let dir = TempDir::new("release");
        let mut h = holder(&dir, &[("a", 0.5), ("b", 0.8)]);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        assert!(dir.file().exists());
        h.sessions().set_volume("b", 0.3).unwrap(); // the user's own setting
        let n = h.sessions().writes().len();
        assert_eq!(release(&mut h, T0 + 500 * MS), HolderPhase::Idle);
        assert_eq!(vol(&h, "a"), 0.5);
        assert_eq!(vol(&h, "b"), 0.3);
        assert!(h.sessions().writes()[n..].iter().all(|(id, _)| id == "a"));
        assert!(!dir.file().exists());
        assert_eq!(h.attenuation(), 1.0);
        assert_eq!(h.output_gain(), 0.0);
        assert_eq!(h.tick(T0 + 600 * MS), HolderPhase::Idle);
    }

    #[test]
    fn player_exit_reports_and_clears_file() {
        let dir = TempDir::new("exit");
        let mut h = holder(&dir, &[("a", 0.5)]);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        let e0 = h.attenuation_epoch();
        // The process is gone; its expired session is still enumerable at the held volume.
        h.sessions().set_process_created(PID, None);
        let r = h.follow(T0 + 500 * MS);
        assert!(r.player_exited);
        assert_eq!((r.newly_lowered, r.overridden, r.failed), (0, 0, 0));
        assert!(!dir.file().exists());
        assert_eq!(vol(&h, "a"), 0.5);
        assert_eq!(h.tick(T0 + 501 * MS), HolderPhase::Idle);
        assert_eq!(h.attenuation(), 1.0);
        assert_eq!(h.attenuation_epoch(), e0 + 1);
        assert_eq!(h.output_gain(), 0.0);
        assert_eq!(h.capture_gain(T0 + 501 * MS), 1.0);
        assert_eq!(h.follow(T0 + 1000 * MS), FollowReport::default());
    }

    #[test]
    fn player_exit_keeps_entry_until_the_player_returns() {
        let dir = TempDir::new("exit-await");
        let mut h = holder(&dir, &[("a", 0.5)]);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        h.sessions().remove_session("a");
        h.sessions().set_process_created(PID, Some(CREATED + 1)); // pid reused
        let r = h.follow(T0 + 500 * MS);
        assert!(r.player_exited);
        assert_eq!(r.failed, 0);
        // Windows persists the held volume per app: keep the entry for the relaunch.
        assert_eq!(entry_ids(&dir), vec!["a"]);
        assert_eq!(h.tick(T0 + 501 * MS), HolderPhase::Idle);
    }

    #[test]
    fn attach_restores_a_relaunched_player_first() {
        let dir = TempDir::new("relaunch");
        let old = RestoreEntry {
            pid: 10,
            created_at: 111,
            instance_id: "old".into(),
            original_volume: 0.7,
            original_mute: false,
            saved_at_ms: 1,
            session_identifier: "ident:player".into(),
        };
        restore::write_atomic(
            &dir.file(),
            &RestoreRecord {
                version: 1,
                entries: vec![old],
            },
        )
        .unwrap();
        let f = fake(&[]);
        let mut s = info("new", PID);
        s.session_identifier = "ident:player".into();
        f.add_session(s, HELD_VOLUME, false); // came back at the persisted held volume
        let mut h = Holder::new(f, dir.file());
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        assert!(
            (h.attenuation() - 1e-4 / 0.7).abs() < 1e-9,
            "{}",
            h.attenuation()
        );
        let e = entries(&dir);
        assert_eq!(e.len(), 1);
        assert_eq!(
            (e[0].instance_id.as_str(), e[0].original_volume),
            ("new", 0.7)
        );
        assert_eq!(vol(&h, "new"), HELD_VOLUME);
        assert_eq!(release(&mut h, T0 + 500 * MS), HolderPhase::Idle);
        assert_eq!(vol(&h, "new"), 0.7);
        assert!(!dir.file().exists());
    }

    #[test]
    fn foreign_entries_are_kept_through_attach_follow_and_release() {
        let dir = TempDir::new("foreign");
        let foreign = RestoreEntry {
            pid: 77,
            created_at: 7,
            instance_id: "other".into(),
            original_volume: 0.9,
            original_mute: false,
            saved_at_ms: 1,
            session_identifier: "ident:other-player".into(),
        };
        restore::write_atomic(
            &dir.file(),
            &RestoreRecord {
                version: 1,
                entries: vec![foreign.clone()],
            },
        )
        .unwrap();
        let mut h = holder(&dir, &[("a", 0.5)]);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        assert_eq!(entry_ids(&dir), vec!["other", "a"]);
        h.sessions().add_session(info("late", PID), 0.4, false);
        assert_eq!(h.follow(T0 + 500 * MS).newly_lowered, 1);
        assert_eq!(entry_ids(&dir), vec!["other", "a", "late"]);
        assert_eq!(release(&mut h, T0 + 1000 * MS), HolderPhase::Idle);
        assert_eq!(entries(&dir), vec![foreign]);
        assert_eq!((vol(&h, "a"), vol(&h, "late")), (0.5, 0.4));
    }

    #[test]
    fn attach_fails_without_lowering_when_restore_file_cannot_be_written() {
        let dir = TempDir::new("attach-write-fail");
        let path = dir.0.join("missing-dir").join("devocal-restore.json");
        let mut h = Holder::new(fake(&[("a", 0.5)]), path);
        assert!(h.begin_attach(PID, CREATED, T0).is_err());
        assert_eq!(h.tick(T0), HolderPhase::Idle);
        assert_eq!(h.tick(T0 + 30 * MS), HolderPhase::Idle);
        assert_eq!(h.sessions().set_volume_calls(), 0);
        assert_eq!(h.attenuation(), 1.0);
        // Still Idle: a retry is accepted (and fails the same way, still lowering nothing).
        assert!(h.begin_attach(PID, CREATED, T0 + 100 * MS).is_err());
        assert_eq!(h.sessions().set_volume_calls(), 0);
    }

    #[test]
    fn attach_while_not_idle_is_rejected() {
        let dir = TempDir::new("attach-twice");
        let mut h = holder(&dir, &[("a", 0.5)]);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        let n = h.sessions().set_volume_calls();
        assert!(h.begin_attach(PID, CREATED, T0 + 100 * MS).is_err());
        assert_eq!(h.tick(T0 + 100 * MS), HolderPhase::Held);
        assert_eq!(h.sessions().set_volume_calls(), n);
        assert_eq!(entry_ids(&dir), vec!["a"]);
    }

    #[test]
    fn set_volume_failure_during_attach_is_reported() {
        let dir = TempDir::new("attach-set-fail");
        let mut h = holder(&dir, &[("a", 0.5), ("b", 0.8)]);
        h.sessions().fail_set_volume("b");
        h.begin_attach(PID, CREATED, T0).unwrap();
        assert!(matches!(h.tick(T0), HolderPhase::Failed(m) if m.contains('b')));
        let n = h.sessions().set_volume_calls();
        for ms in [10, 20, 30, 40] {
            assert!(matches!(h.tick(T0 + ms * MS), HolderPhase::Failed(_)));
        }
        assert_eq!(h.sessions().set_volume_calls(), n, "the ramp stops");
        assert_eq!(h.attenuation(), 1.0);
        assert!(h.capture_gain(T0 + 200 * MS) <= 1.0 + 1e-6);
        assert_eq!(h.follow(T0 + 200 * MS), FollowReport::default());

        h.sessions().clear_failures();
        assert_eq!(release(&mut h, T0 + 300 * MS), HolderPhase::Idle);
        assert_eq!((vol(&h, "a"), vol(&h, "b")), (0.5, 0.8));
        assert!(!dir.file().exists());
    }

    #[test]
    fn set_volume_failure_during_follow_is_reported() {
        let dir = TempDir::new("follow-set-fail");
        let mut h = holder(&dir, &[("a", 0.5)]);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        h.sessions().add_session(info("late", PID), 0.8, false);
        h.sessions().fail_set_volume("late");
        let t = T0 + 500 * MS;
        let r = h.follow(t);
        assert_eq!((r.newly_lowered, r.failed), (0, 1));
        assert!(r.error.as_deref().is_some_and(|e| e.contains("late")));
        // An unlowered session plays at full level: no compensation while it does, and no
        // more than original / 0.8 since it plays above the reference.
        assert!(h.capture_gain(t + 200 * MS) <= 0.5 / 0.8 + 1e-6);

        h.sessions().clear_failures();
        let r = h.follow(t + 500 * MS);
        assert_eq!((r.newly_lowered, r.failed), (1, 0));
        assert_eq!(vol(&h, "late"), HELD_VOLUME);
        assert!((h.capture_gain(t + 601 * MS) - 5000.0).abs() < 0.1);
        assert_eq!(entry_ids(&dir), vec!["a", "late"]);
    }

    #[test]
    fn restore_file_write_failure_during_follow_lowers_nothing() {
        let dir = TempDir::new("follow-write-fail");
        let mut h = holder(&dir, &[]);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        std::fs::create_dir(dir.file()).unwrap(); // unreadable restore path
        h.sessions().add_session(info("late", PID), 0.8, false);
        let r = h.follow(T0 + 500 * MS);
        assert_eq!((r.newly_lowered, r.failed), (0, 1));
        assert!(r.error.is_some());
        assert_eq!(vol(&h, "late"), 0.8);
        assert_eq!(h.sessions().set_volume_calls(), 0);
        assert_eq!(h.attenuation(), 1.0);
    }

    #[test]
    fn set_volume_failure_during_release_keeps_entry_and_is_reported() {
        let dir = TempDir::new("release-set-fail");
        let mut h = holder(&dir, &[("a", 0.5), ("b", 0.8)]);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        h.sessions().fail_set_volume("b");
        let tr = T0 + 500 * MS;
        h.begin_release(tr);
        assert!(matches!(h.tick(tr), HolderPhase::Failed(m) if m.contains('b')));
        assert_eq!(h.tick(tr + 10 * MS), HolderPhase::Releasing);
        assert_eq!(h.tick(tr + 20 * MS), HolderPhase::Releasing);
        assert_eq!(h.tick(tr + 30 * MS), HolderPhase::Idle);
        assert_eq!((vol(&h, "a"), vol(&h, "b")), (0.5, HELD_VOLUME));
        assert_eq!(entry_ids(&dir), vec!["b"]);
        // The kept entry is enough for a later restore.
        h.sessions().clear_failures();
        assert_eq!(restore::restore(&dir.file(), h.sessions()).restored, 1);
        assert_eq!(vol(&h, "b"), 0.8);
    }

    #[test]
    fn recreated_session_at_held_volume_is_adopted_and_restored() {
        let dir = TempDir::new("adopt");
        let mut h = holder(&dir, &[("a", 0.5)]);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        // The player recreates its session; Windows applies the persisted held volume.
        h.sessions().remove_session("a");
        let mut s = info("a2", PID);
        s.session_identifier = "ident:a".into();
        h.sessions().add_session(s, HELD_VOLUME, false);
        let n = h.sessions().set_volume_calls();
        assert_eq!(h.follow(T0 + 500 * MS), FollowReport::default());
        assert_eq!(h.sessions().set_volume_calls(), n);
        let e = entries(&dir);
        assert_eq!(e.len(), 2);
        assert_eq!(
            (e[1].instance_id.as_str(), e[1].original_volume),
            ("a2", 0.5)
        );

        // Release: a2 is ramped back; the vanished a is not an error and its entry is kept.
        h.begin_release(T0 + 1000 * MS);
        for k in 0..4 {
            let p = h.tick(T0 + 1000 * MS + k * 10 * MS);
            assert!(!matches!(p, HolderPhase::Failed(_)), "{p:?}");
        }
        assert_eq!(h.phase(), HolderPhase::Idle);
        assert_eq!(vol(&h, "a2"), 0.5);
        assert_eq!(entry_ids(&dir), vec!["a"]);
        // A later restore matches it by identifier, sees a2 is no longer held and drops it.
        let out = restore::restore(&dir.file(), h.sessions());
        assert_eq!(
            (out.left_changed, out.failed, out.awaiting_player),
            (1, 0, 0)
        );
        assert!(!dir.file().exists());
    }

    #[test]
    fn unreadable_session_in_follow_is_reported_and_disables_compensation() {
        let dir = TempDir::new("follow-read-fail");
        let mut h = holder(&dir, &[("a", 0.5)]);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        h.sessions().fail_volume("a");
        let t = T0 + 500 * MS;
        let r = h.follow(t);
        assert_eq!(r.failed, 1);
        assert!(r.error.is_some());
        assert!(h.capture_gain(t + 101 * MS) <= 1.0 + 1e-6);
        h.sessions().clear_failures();
        assert_eq!(h.follow(t + 500 * MS), FollowReport::default());
        assert!((h.capture_gain(t + 601 * MS) - 5000.0).abs() < 0.1);
    }

    #[test]
    fn unreadable_session_at_attach_disables_compensation_until_followed() {
        let dir = TempDir::new("attach-read-fail");
        let mut h = holder(&dir, &[("a", 0.5), ("u", 0.7)]);
        h.sessions().fail_volume("u");
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        assert_eq!(entry_ids(&dir), vec!["a"]);
        assert!(h.capture_gain(T0 + 200 * MS) <= 1.0 + 1e-6);

        h.sessions().clear_failures();
        let t = T0 + 500 * MS;
        assert_eq!(h.follow(t).newly_lowered, 1);
        assert_eq!(vol(&h, "u"), HELD_VOLUME);
        assert!((h.capture_gain(t + 101 * MS) - 5000.0).abs() < 0.1);
    }

    #[test]
    fn entry_never_lowered_but_found_held_is_restored_on_release() {
        let dir = TempDir::new("pending-held");
        let mut h = holder(&dir, &[("a", 0.5)]);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        h.sessions().add_session(info("late", PID), 0.8, false);
        h.sessions().fail_set_volume("late");
        assert_eq!(h.follow(T0 + 500 * MS).failed, 1);
        assert_eq!(entry_ids(&dir), vec!["a", "late"]);
        h.sessions().clear_failures();
        h.sessions().set_volume("late", HELD_VOLUME).unwrap();
        assert_eq!(h.follow(T0 + 1000 * MS), FollowReport::default());
        assert_eq!(release(&mut h, T0 + 1500 * MS), HolderPhase::Idle);
        assert_eq!(vol(&h, "late"), 0.8);
        assert!(!dir.file().exists());
    }

    fn kept_entry(pid: u32, id: &str, ident: &str, original: f32) -> RestoreEntry {
        RestoreEntry {
            pid,
            created_at: 7,
            instance_id: id.into(),
            original_volume: original,
            original_mute: false,
            saved_at_ms: 1,
            session_identifier: ident.into(),
        }
    }

    fn write_file(dir: &TempDir, entries: Vec<RestoreEntry>) {
        restore::write_atomic(
            &dir.file(),
            &RestoreRecord {
                version: 1,
                entries,
            },
        )
        .unwrap();
    }

    #[test]
    fn attach_adopts_a_still_held_session_from_a_kept_entry() {
        let dir = TempDir::new("adopt-kept");
        // Earlier crash: player pid 10 (gone) was held; its original was 0.7.
        write_file(&dir, vec![kept_entry(10, "old", "ident:player", 0.7)]);
        let f = fake(&[]);
        let mut s = info("new", PID);
        s.session_identifier = "ident:player".into();
        f.add_session(s, HELD_VOLUME, false); // relaunched at the persisted held volume
        f.add_session(info("mine", PID), HELD_VOLUME, false); // user's own, no entry
        f.fail_set_volume("new"); // the restore cannot bring it back
        let mut h = Holder::new(f, dir.file());

        let report = h.begin_attach(PID, CREATED, T0).unwrap();
        assert_eq!(
            report,
            AttachReport {
                restore_failed: 1,
                restore_corrupt: false,
                adopted: 1,
            }
        );
        let n = h.sessions().set_volume_calls(); // the restore's failed attempt
        for k in 0..4 {
            assert_eq!(
                h.tick(T0 + k * 10 * MS),
                if k == 3 {
                    HolderPhase::Held
                } else {
                    HolderPhase::Attaching
                }
            );
        }
        assert_eq!(
            h.sessions().set_volume_calls(),
            n,
            "already held: not lowered again"
        );
        assert!((h.attenuation() - 1e-4 / 0.7).abs() < 1e-9);
        assert!((h.capture_gain(T0 + 131 * MS) - 7000.0).abs() < 0.1);
        // Our entry replaces the kept one; the user's own near-zero session is untouched.
        let e = entries(&dir);
        assert_eq!(e.len(), 1);
        assert_eq!(
            (e[0].instance_id.as_str(), e[0].original_volume),
            ("new", 0.7)
        );
        assert_eq!(vol(&h, "mine"), HELD_VOLUME);

        h.sessions().clear_failures();
        assert_eq!(release(&mut h, T0 + 500 * MS), HolderPhase::Idle);
        assert_eq!(vol(&h, "new"), 0.7);
        assert_eq!(vol(&h, "mine"), HELD_VOLUME);
        assert!(!dir.file().exists());
    }

    #[test]
    fn attach_reports_restore_failures() {
        // A kept entry whose process cannot be queried: failed, kept, merged.
        let dir = TempDir::new("attach-restore-failed");
        write_file(&dir, vec![kept_entry(77, "x", "ident:x", 0.9)]);
        let mut h = holder(&dir, &[("a", 0.5)]);
        h.sessions().fail_process_created(77);
        let report = h.begin_attach(PID, CREATED, T0).unwrap();
        assert_eq!(
            report,
            AttachReport {
                restore_failed: 1,
                restore_corrupt: false,
                adopted: 0,
            }
        );
        assert_eq!(entry_ids(&dir), vec!["x", "a"]);

        // A corrupt file is quarantined and reported.
        let dir = TempDir::new("attach-restore-corrupt");
        std::fs::write(dir.file(), "{oops").unwrap();
        let mut h = holder(&dir, &[("a", 0.5)]);
        let report = h.begin_attach(PID, CREATED, T0).unwrap();
        assert!(report.restore_corrupt);
        assert!(dir.0.join("devocal-restore.json.corrupt").exists());
        assert_eq!(entry_ids(&dir), vec!["a"]);

        // An unreadable file: the attach fails and the message carries the restore outcome.
        let dir = TempDir::new("attach-restore-unreadable");
        std::fs::create_dir(dir.file()).unwrap();
        let mut h = holder(&dir, &[("a", 0.5)]);
        let err = h.begin_attach(PID, CREATED, T0).unwrap_err();
        assert!(
            err.contains("restore before attach: 1 entries failed"),
            "{err}"
        );
        assert_eq!(h.sessions().set_volume_calls(), 0);

        // A clean attach reports nothing.
        let dir = TempDir::new("attach-restore-clean");
        let mut h = holder(&dir, &[("a", 0.5)]);
        assert_eq!(
            h.begin_attach(PID, CREATED, T0).unwrap(),
            AttachReport::default()
        );
    }

    #[test]
    fn player_exit_reports_restore_failures() {
        let dir = TempDir::new("exit-restore-failed");
        let mut h = holder(&dir, &[("a", 0.5)]);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        h.sessions().set_process_created(PID, None);
        h.sessions().fail_set_volume("a");
        let r = h.follow(T0 + 500 * MS);
        assert!(r.player_exited);
        assert_eq!((r.restore_failed, r.restore_corrupt), (1, false));
        assert!(r.error.is_some());
        assert_eq!(entry_ids(&dir), vec!["a"]);

        let dir = TempDir::new("exit-restore-corrupt");
        let mut h = holder(&dir, &[("a", 0.5)]);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        std::fs::write(dir.file(), "{oops").unwrap();
        h.sessions().set_process_created(PID, None);
        let r = h.follow(T0 + 500 * MS);
        assert!(r.player_exited && r.restore_corrupt);
        assert!(r.error.is_some());
    }

    #[test]
    fn two_sessions_use_the_quieter_original() {
        let dir = TempDir::new("quieter");
        let mut h = holder(&dir, &[("loud", 1.0), ("quiet", 0.2)]);
        h.begin_attach(PID, CREATED, T0).unwrap();
        // Every ms of the ramp and the window after it: no session is boosted above its own
        // original, i.e. gain <= o_s / v for every volume v either session had in the window.
        type History = Vec<(u64, f32)>;
        let mut hist: Vec<(&str, f32, History)> = vec![
            ("loud", 1.0, vec![(0, 1.0)]),
            ("quiet", 0.2, vec![(0, 0.2)]),
        ];
        for ms in 0..=130 {
            let t = T0 + ms * MS;
            h.tick(t);
            let g = h.capture_gain(t);
            let window_start = t.saturating_sub(100 * MS);
            for (id, o, history) in &mut hist {
                let v = vol(&h, id);
                if history.last().unwrap().1 != v {
                    history.push((t, v));
                }
                for (i, &(since, v)) in history.iter().enumerate() {
                    let until = history.get(i + 1).map_or(u64::MAX, |n| n.0);
                    if until < window_start || since > t {
                        continue;
                    }
                    assert!(g <= *o / v * (1.0 + 1e-5), "{id} +{ms} ms: {g} > {o}/{v}");
                }
            }
        }
        assert_eq!(
            (vol(&h, "loud"), vol(&h, "quiet")),
            (HELD_VOLUME, HELD_VOLUME)
        );
        assert!((h.attenuation() - 1e-4 / 0.2).abs() < 1e-9);
        assert!((h.capture_gain(T0 + 131 * MS) - 2000.0).abs() < 0.05);
        // Each session still goes back to its own original.
        assert_eq!(release(&mut h, T0 + 500 * MS), HolderPhase::Idle);
        assert_eq!((vol(&h, "loud"), vol(&h, "quiet")), (1.0, 0.2));
    }

    #[test]
    fn a_quieter_late_session_lowers_the_original_only_downwards() {
        let dir = TempDir::new("quieter-late");
        let mut h = holder(&dir, &[("a", 0.5)]);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        let e0 = h.attenuation_epoch();
        h.sessions().add_session(info("loud", PID), 0.9, false);
        let t = T0 + 500 * MS;
        assert_eq!(h.follow(t).newly_lowered, 1);
        assert!((h.attenuation() - 1e-4 / 0.5).abs() < 1e-9, "never goes up");
        assert_eq!(h.attenuation_epoch(), e0);

        h.sessions().add_session(info("quiet", PID), 0.25, false);
        let t = t + 500 * MS;
        assert_eq!(h.follow(t).newly_lowered, 1);
        assert!((h.attenuation() - 1e-4 / 0.25).abs() < 1e-9);
        assert_eq!(h.attenuation_epoch(), e0 + 1);
        assert!(h.capture_gain(t + 100 * MS) <= 1.0 + 1e-6);
        assert!((h.capture_gain(t + 101 * MS) - 2500.0).abs() < 0.1);
    }

    /// Session `id` of the player on endpoint `ep` (its own session identifier there).
    fn info_on(id: &str, ep: &str) -> SessionInfo {
        SessionInfo {
            endpoint_id: ep.into(),
            session_identifier: format!("{ep}|ident:player"),
            ..info(id, PID)
        }
    }

    #[test]
    fn a_new_session_at_full_volume_is_never_amplified() {
        let dir = TempDir::new("new-loud");
        let mut h = holder(&dir, &[("a", 0.3)]);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        let t = T0 + 500 * MS;
        assert!((h.capture_gain(t) - 3000.0).abs() < 0.05);
        // The player recreated its stream on a new device: Windows starts it at 1.0.
        h.sessions()
            .add_session(info_on("ep2|player", "ep2"), 1.0, false);
        let r = h.follow(t);
        assert_eq!(r.newly_lowered, 1);
        assert_eq!(vol(&h, "ep2|player"), HELD_VOLUME);
        assert_eq!(entries(&dir)[1].original_volume, 1.0);
        // It played at 1.0 until now: no more than original / 1.0 for the whole window.
        for ms in 0..=100 {
            let g = h.capture_gain(t + ms * MS);
            assert!(g <= 0.3 / 1.0 + 1e-6, "+{ms} ms: gain {g}");
        }
        assert!((h.capture_gain(t + 101 * MS) - 3000.0).abs() < 0.05);
        assert!(
            (h.attenuation() - 1e-4 / 0.3).abs() < 1e-9,
            "reference unchanged"
        );
    }

    #[test]
    fn default_device_change_mutes_until_the_player_is_held_on_the_new_endpoint() {
        let dir = TempDir::new("device-mute");
        let mut h = holder(&dir, &[("a", 0.3)]);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        let t = T0 + 500 * MS;
        h.default_device_changed(t, Some("ep2".into()));
        assert_eq!(h.capture_gain(t), 0.0);
        // A clean pass before the player moved does not end the mute.
        assert_eq!(h.follow(t + MS), FollowReport::default());
        assert_eq!(h.capture_gain(t + MS), 0.0);
        // The player's new session appears on the new endpoint at 1.0 and is lowered.
        h.sessions()
            .add_session(info_on("ep2|player", "ep2"), 1.0, false);
        let tf = t + 40 * MS;
        assert_eq!(h.capture_gain(tf), 0.0);
        assert_eq!(h.follow(tf).newly_lowered, 1);
        // Unmuted with the conservative gain: original / 1.0 for the window, then 3000.
        let g = h.capture_gain(tf);
        assert!(g > 0.0 && g <= 0.3 + 1e-6, "{g}");
        assert!(h.capture_gain(tf + 100 * MS) <= 0.3 + 1e-6);
        assert!((h.capture_gain(tf + 101 * MS) - 3000.0).abs() < 0.05);
    }

    #[test]
    fn default_device_mute_ends_after_one_second_without_a_follow() {
        let dir = TempDir::new("device-mute-timeout");
        let mut h = holder(&dir, &[("a", 0.3)]);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        let t = T0 + 500 * MS;
        h.default_device_changed(t, Some("ep2".into()));
        assert_eq!(h.capture_gain(t + 999 * MS), 0.0);
        assert!((h.capture_gain(t + DEVICE_MUTE_US) - 3000.0).abs() < 0.05);
        // A pass after the timeout clears it for good.
        assert_eq!(h.follow(t + 1_200 * MS), FollowReport::default());
        assert!((h.capture_gain(t + 1_200 * MS) - 3000.0).abs() < 0.05);
    }

    #[test]
    fn default_device_mute_outlasts_a_failed_follow_then_normal_rules_apply() {
        let dir = TempDir::new("device-mute-fail");
        let mut h = holder(&dir, &[("a", 0.3)]);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        let t = T0 + 500 * MS;
        h.default_device_changed(t, Some("ep2".into()));
        h.sessions()
            .add_session(info_on("ep2|player", "ep2"), 1.0, false);
        h.sessions().fail_set_volume("ep2|player");
        let r = h.follow(t + 10 * MS);
        assert_eq!((r.newly_lowered, r.failed), (0, 1));
        assert_eq!(
            h.capture_gain(t + 10 * MS),
            0.0,
            "not lowered: still silent"
        );
        assert_eq!(h.capture_gain(t + 999 * MS), 0.0);
        // After the timeout: the unlowered session plays at 1.0, so no compensation (gain
        // original / 1.0 at most).
        let g = h.capture_gain(t + DEVICE_MUTE_US);
        assert!(g > 0.0 && g <= 0.3 + 1e-6, "{g}");
    }

    #[test]
    fn default_device_change_without_a_new_endpoint_ends_on_the_next_clean_pass() {
        let dir = TempDir::new("device-mute-none");
        let mut h = holder(&dir, &[("a", 0.3)]);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        let t = T0 + 500 * MS;
        h.default_device_changed(t, None);
        assert_eq!(h.capture_gain(t), 0.0);
        assert_eq!(h.follow(t + MS), FollowReport::default());
        assert!((h.capture_gain(t + MS) - 3000.0).abs() < 0.05);
    }

    #[test]
    fn default_device_change_is_ignored_unless_holding() {
        let dir = TempDir::new("device-mute-idle");
        let mut h = holder(&dir, &[("a", 0.3)]);
        h.default_device_changed(T0, Some("ep2".into()));
        assert_eq!(h.capture_gain(T0), 1.0);
        // An attach starts unmuted, and a change while held ends with the player's exit.
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        assert!((h.capture_gain(T0 + 131 * MS) - 3000.0).abs() < 0.05);
        h.default_device_changed(T0 + 200 * MS, Some("ep2".into()));
        h.sessions().set_process_created(PID, None);
        assert!(h.follow(T0 + 201 * MS).player_exited);
        assert_eq!(h.capture_gain(T0 + 202 * MS), 1.0);
    }

    /// Player session `id` on endpoint `ep`, active or not, with its own identifier.
    fn sess(id: &str, ep: &str, active: bool) -> SessionInfo {
        SessionInfo {
            instance_id: id.into(),
            session_identifier: format!("{ep}|ident:{id}"),
            pid: PID,
            endpoint_id: ep.into(),
            active,
        }
    }

    /// A holder whose player has `sessions` as (id, endpoint, active, volume); the default
    /// render endpoint is `ep1`.
    fn parking_holder(dir: &TempDir, sessions: &[(&str, &str, bool, f32)]) -> Holder<FakeSessions> {
        let f = FakeSessions::new();
        f.set_process_created(PID, Some(CREATED));
        f.set_default_endpoints(&["ep1"]);
        for &(id, ep, active, v) in sessions {
            f.add_session(sess(id, ep, active), v, false);
        }
        Holder::new(f, dir.file())
    }

    #[test]
    fn parks_only_inactive_sessions_off_the_default_endpoint() {
        let dir = TempDir::new("park");
        let mut h = parking_holder(
            &dir,
            &[
                ("play", "ep1", true, 0.5),
                ("paused", "ep1", false, 0.6), // default endpoint: never parked
                ("idle", "ep2", false, 0.4),   // parked
                ("pinned", "ep3", true, 0.7),  // active: never parked
            ],
        );
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        let g = h.capture_gain(T0 + 200 * MS);
        let e0 = (h.parked_epoch(), h.attenuation_epoch(), h.attenuation());
        let n = h.sessions().writes().len();

        let t = T0 + 500 * MS;
        let r = h.follow(t);
        assert_eq!(r, FollowReport::default());
        assert_eq!(vol(&h, "idle"), 0.4, "back at its original");
        for id in ["play", "paused", "pinned"] {
            assert_eq!(vol(&h, id), HELD_VOLUME, "{id}");
        }
        assert_eq!(h.sessions().writes()[n..], [("idle".to_string(), 0.4)]);
        assert_eq!(entry_ids(&dir), vec!["play", "paused", "pinned"]);
        assert_eq!(h.parked_sessions(), vec!["idle"]);
        assert_eq!(h.parked_epoch(), e0.0 + 1);
        // The capture reference is not raised by parking (still the 0.4 of `idle`).
        assert_eq!((h.attenuation_epoch(), h.attenuation()), (e0.1, e0.2));
        assert_eq!(h.capture_gain(t + 200 * MS), g);
        // A second pass changes nothing.
        let n = h.sessions().writes().len();
        assert_eq!(h.follow(t + 500 * MS), FollowReport::default());
        assert_eq!(h.sessions().writes().len(), n);
        assert_eq!(h.parked_epoch(), e0.0 + 1);
    }

    #[test]
    fn nothing_is_parked_while_the_default_endpoint_is_unknown() {
        for unknown in ["none", "error"] {
            let dir = TempDir::new("park-unknown");
            let mut h = parking_holder(
                &dir,
                &[("play", "ep1", true, 0.5), ("idle", "ep2", false, 0.4)],
            );
            match unknown {
                "none" => h.sessions().set_default_endpoints(&[]),
                _ => h.sessions().fail_default_endpoints(),
            }
            assert_eq!(attach(&mut h, T0), HolderPhase::Held);
            assert_eq!(
                h.follow(T0 + 500 * MS),
                FollowReport::default(),
                "{unknown}"
            );
            assert_eq!(vol(&h, "idle"), HELD_VOLUME, "{unknown}");
            assert!(h.parked_sessions().is_empty());
            assert_eq!(entry_ids(&dir), vec!["play", "idle"]);
        }
    }

    #[test]
    fn a_parked_session_stays_parked_while_the_default_is_unknown() {
        let dir = TempDir::new("park-then-unknown");
        let mut h = parking_holder(
            &dir,
            &[("play", "ep1", true, 0.5), ("idle", "ep2", false, 0.4)],
        );
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        h.follow(T0 + 500 * MS);
        assert_eq!(h.parked_sessions(), vec!["idle"]);
        h.sessions().fail_default_endpoints();
        assert_eq!(h.follow(T0 + 1000 * MS), FollowReport::default());
        assert_eq!(vol(&h, "idle"), 0.4);
        // ...but turning active holds it again, default known or not.
        h.sessions().set_active("idle", true);
        assert_eq!(h.follow(T0 + 1500 * MS).newly_lowered, 1);
        assert_eq!(vol(&h, "idle"), HELD_VOLUME);
    }

    #[test]
    fn default_change_lowers_the_parked_session_on_the_new_default_before_it_plays() {
        let dir = TempDir::new("park-default-change");
        let mut h = parking_holder(&dir, &[("a", "ep1", true, 0.3), ("b", "ep2", false, 0.3)]);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        let t = T0 + 500 * MS;
        h.follow(t);
        assert_eq!(
            (vol(&h, "b"), h.parked_sessions()),
            (0.3, vec!["b".to_string()])
        );
        let e = h.parked_epoch();

        // The default moves to ep2; the event-driven pass runs before the player starts there.
        let tc = t + 500 * MS;
        h.sessions().set_default_endpoints(&["ep2"]);
        h.default_device_changed(tc, Some("ep2".into()));
        let seen = track_file_before_write(&h, dir.file());
        let r = h.follow(tc);
        assert_eq!(r.newly_lowered, 1);
        assert_eq!(vol(&h, "b"), HELD_VOLUME, "lowered while still inactive");
        assert_eq!(
            *seen.borrow(),
            vec![true],
            "listed in the restore file first"
        );
        assert!(h.parked_sessions().is_empty());
        assert_eq!(h.parked_epoch(), e + 1);
        let entry = entries(&dir)
            .into_iter()
            .find(|e| e.instance_id == "b")
            .unwrap();
        assert_eq!(entry.original_volume, 0.3);
        // Held on the new default before it could start: the mute ends, and the gain is the
        // conservative one (it was at 0.3 until now): never above original / 0.3.
        for ms in 0..=100 {
            let g = h.capture_gain(tc + ms * MS);
            assert!(g > 0.0 && g <= 0.3 / 0.3 + 1e-6, "+{ms} ms: {g}");
        }
        assert!((h.capture_gain(tc + 101 * MS) - 3000.0).abs() < 0.05);

        // The player's stream moves: a stops (and is parked), b plays.
        h.sessions().set_active("a", false);
        h.sessions().set_active("b", true);
        let r = h.follow(tc + 500 * MS);
        assert_eq!((r.newly_lowered, r.failed), (0, 0));
        assert_eq!((vol(&h, "a"), vol(&h, "b")), (0.3, HELD_VOLUME));
        assert_eq!(h.parked_sessions(), vec!["a"]);
        assert_eq!(entry_ids(&dir), vec!["b"]);
    }

    #[test]
    fn a_parked_session_turning_active_is_lowered_again() {
        let dir = TempDir::new("park-state");
        let mut h = parking_holder(&dir, &[("a", "ep1", true, 0.3), ("b", "ep2", false, 0.6)]);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        h.follow(T0 + 500 * MS);
        assert_eq!(vol(&h, "b"), 0.6);
        // The player (pinned to ep2) starts playing there.
        h.sessions().set_active("b", true);
        let t = T0 + 700 * MS;
        let r = h.follow(t);
        assert_eq!(r.newly_lowered, 1);
        assert_eq!(vol(&h, "b"), HELD_VOLUME);
        assert_eq!(entry_ids(&dir), vec!["a", "b"]);
        assert!(h.parked_sessions().is_empty());
        // It played at 0.6 until now: no more than 0.3 / 0.6 for the window.
        assert!(h.capture_gain(t) <= 0.3 / 0.6 + 1e-6);
        assert!(h.capture_gain(t + 100 * MS) <= 0.3 / 0.6 + 1e-6);
        assert!((h.capture_gain(t + 101 * MS) - 3000.0).abs() < 0.05);
    }

    #[test]
    fn a_parked_session_the_user_turned_up_keeps_that_as_its_original() {
        let dir = TempDir::new("park-user");
        let mut h = parking_holder(&dir, &[("a", "ep1", true, 0.3), ("b", "ep2", false, 0.6)]);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        h.follow(T0 + 500 * MS);
        h.sessions().set_volume("b", 0.9).unwrap();
        h.sessions().set_active("b", true);
        assert_eq!(h.follow(T0 + 1000 * MS).newly_lowered, 1);
        let entry = entries(&dir)
            .into_iter()
            .find(|e| e.instance_id == "b")
            .unwrap();
        assert_eq!(entry.original_volume, 0.9);
        assert_eq!(release(&mut h, T0 + 1500 * MS), HolderPhase::Idle);
        assert_eq!(vol(&h, "b"), 0.9);
    }

    #[test]
    fn a_new_idle_session_off_the_default_is_owned_parked_without_lowering() {
        let dir = TempDir::new("park-new");
        let mut h = parking_holder(&dir, &[("a", "ep1", true, 0.3)]);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        h.sessions()
            .add_session(sess("n", "ep2", false), 1.0, false);
        let n = h.sessions().writes().len();
        let t = T0 + 500 * MS;
        assert_eq!(h.follow(t), FollowReport::default());
        assert_eq!(h.sessions().writes().len(), n, "never touched");
        assert_eq!(h.parked_sessions(), vec!["n"]);
        assert_eq!(entry_ids(&dir), vec!["a"]);
        assert!((h.capture_gain(t) - 3000.0).abs() < 0.05, "no gain dip");
        // It starts playing: lowered at once, never amplified.
        h.sessions().set_active("n", true);
        let t = t + 200 * MS;
        assert_eq!(h.follow(t).newly_lowered, 1);
        assert_eq!(vol(&h, "n"), HELD_VOLUME);
        assert!(h.capture_gain(t) <= 0.3 / 1.0 + 1e-6);
        assert_eq!(entry_ids(&dir), vec!["a", "n"]);
    }

    #[test]
    fn release_and_player_exit_leave_every_session_at_its_original() {
        for exit in [false, true] {
            let dir = TempDir::new("park-release");
            let mut h = parking_holder(
                &dir,
                &[
                    ("a", "ep1", true, 0.3),
                    ("b", "ep2", false, 0.6),
                    ("c", "ep3", false, 0.8),
                ],
            );
            assert_eq!(attach(&mut h, T0), HolderPhase::Held);
            h.follow(T0 + 500 * MS);
            assert_eq!(h.parked_sessions(), vec!["b", "c"]);
            let e = h.parked_epoch();
            // c is held again before the end.
            h.sessions().set_active("c", true);
            h.follow(T0 + 1000 * MS);
            assert_eq!(h.parked_sessions(), vec!["b"]);
            if exit {
                h.sessions().set_process_created(PID, None);
                assert!(h.follow(T0 + 1500 * MS).player_exited);
            } else {
                assert_eq!(release(&mut h, T0 + 1500 * MS), HolderPhase::Idle);
            }
            assert_eq!(
                (vol(&h, "a"), vol(&h, "b"), vol(&h, "c")),
                (0.3, 0.6, 0.8),
                "exit: {exit}"
            );
            assert!(!dir.file().exists());
            assert!(h.parked_sessions().is_empty());
            assert!(h.parked_epoch() > e + 1, "the watches are dropped");
        }
    }

    #[test]
    fn a_crash_after_parking_leaves_nothing_to_restore_for_the_parked_session() {
        let dir = TempDir::new("park-crash");
        let mut h = parking_holder(&dir, &[("a", "ep1", true, 0.3), ("b", "ep2", false, 0.6)]);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        h.follow(T0 + 500 * MS);
        // The engine dies here: the app's restore only sees `a`.
        let out = restore::restore(&dir.file(), h.sessions());
        assert_eq!((out.restored, out.awaiting_player, out.failed), (1, 0, 0));
        assert_eq!((vol(&h, "a"), vol(&h, "b")), (0.3, 0.6));
    }

    #[test]
    fn a_stale_inactive_session_on_the_new_endpoint_does_not_end_the_mute() {
        let dir = TempDir::new("mute-stale");
        // Default unknown, so nothing is parked: `b` stays held but idle on ep2.
        let mut h = holder(&dir, &[]);
        h.sessions().add_session(sess("a", "ep1", true), 0.3, false);
        h.sessions()
            .add_session(sess("b", "ep2", false), 0.3, false);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        let t = T0 + 500 * MS;
        h.default_device_changed(t, Some("ep2".into()));
        assert_eq!(h.follow(t + MS), FollowReport::default());
        assert_eq!(
            h.capture_gain(t + MS),
            0.0,
            "held but idle there: still silent"
        );
        // The player starts on ep2 (joining the held session): the mute ends.
        h.sessions().set_active("b", true);
        assert_eq!(h.follow(t + 20 * MS), FollowReport::default());
        assert!((h.capture_gain(t + 20 * MS) - 3000.0).abs() < 0.05);
    }

    #[test]
    fn a_pinned_player_pausing_on_a_non_default_endpoint_is_not_parked() {
        let dir = TempDir::new("park-pinned");
        let mut h = parking_holder(&dir, &[("p", "ep2", true, 0.4)]);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        assert_eq!(h.follow(T0 + 500 * MS), FollowReport::default());
        // Pause: the client stops, the session goes inactive on the (non-default) ep2.
        h.sessions().set_active("p", false);
        let n = h.sessions().writes().len();
        assert_eq!(h.follow(T0 + 1000 * MS), FollowReport::default());
        assert_eq!(vol(&h, "p"), HELD_VOLUME, "the player is still on ep2");
        assert!(h.parked_sessions().is_empty());
        assert_eq!(h.sessions().writes().len(), n);
        // The player moves to ep3 and plays there: ep2 is left behind and parked.
        h.sessions().add_session(sess("q", "ep3", true), 1.0, false);
        let r = h.follow(T0 + 1500 * MS);
        assert_eq!(r.newly_lowered, 1);
        assert_eq!((vol(&h, "p"), vol(&h, "q")), (0.4, HELD_VOLUME));
        assert_eq!(h.parked_sessions(), vec!["p"]);
        assert_eq!(entry_ids(&dir), vec!["q"]);
        // ...and paused there, ep3 is not parked either.
        h.sessions().set_active("q", false);
        h.follow(T0 + 2000 * MS);
        assert_eq!(vol(&h, "q"), HELD_VOLUME);
        // Back to ep2: held again as soon as it plays there.
        h.sessions().set_active("p", true);
        assert_eq!(h.follow(T0 + 2500 * MS).newly_lowered, 1);
        assert_eq!(vol(&h, "p"), HELD_VOLUME);
    }

    #[test]
    fn nothing_is_parked_before_the_player_has_played() {
        let dir = TempDir::new("park-unknown-playing");
        // Attached while paused: no session active, so the playing endpoint is unknown.
        let mut h = parking_holder(&dir, &[("p", "ep2", false, 0.4), ("o", "ep3", false, 0.5)]);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        assert_eq!(h.follow(T0 + 500 * MS), FollowReport::default());
        assert!(h.parked_sessions().is_empty());
        assert_eq!((vol(&h, "p"), vol(&h, "o")), (HELD_VOLUME, HELD_VOLUME));
        // It plays on ep2: the idle ep3 session is parked, ep2 never.
        h.sessions().set_active("p", true);
        h.follow(T0 + 1000 * MS);
        assert_eq!(h.parked_sessions(), vec!["o"]);
        assert_eq!(vol(&h, "p"), HELD_VOLUME);
    }

    #[test]
    fn a_failed_park_is_not_a_follow_error_and_the_session_stays_held() {
        let dir = TempDir::new("park-fail");
        let mut h = parking_holder(&dir, &[("a", "ep1", true, 0.3), ("b", "ep2", false, 0.6)]);
        assert_eq!(attach(&mut h, T0), HolderPhase::Held);
        h.sessions().fail_set_volume("b");
        let t = T0 + 500 * MS;
        for k in 0..3 {
            assert_eq!(h.follow(t + k * 500 * MS), FollowReport::default());
        }
        assert_eq!(vol(&h, "b"), HELD_VOLUME);
        assert!(h.parked_sessions().is_empty());
        assert_eq!(entry_ids(&dir), vec!["a", "b"]);
        assert!(h.logged.contains("park:b"), "logged once");
        assert!(
            (h.capture_gain(t + 2_000 * MS) - 3000.0).abs() < 0.05,
            "not degraded"
        );
        h.sessions().clear_failures();
        h.follow(t + 2_000 * MS);
        assert_eq!(h.parked_sessions(), vec!["b"]);
        assert!(!h.logged.contains("park:b"));
    }

    #[test]
    fn begin_release_when_idle_is_a_noop() {
        let dir = TempDir::new("release-idle");
        let mut h = holder(&dir, &[("a", 0.5)]);
        h.begin_release(T0);
        assert_eq!(h.tick(T0), HolderPhase::Idle);
        assert_eq!(h.sessions().set_volume_calls(), 0);
    }
}
