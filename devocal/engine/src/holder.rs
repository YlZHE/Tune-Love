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
//!   [`FollowReport::error`]; until a pass succeeds the capture gain falls back to 1.0 (no
//!   compensation), because a session that could not be lowered plays at its full level.

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HolderPhase {
    Idle,
    Attaching,
    Held,
    Releasing,
    /// A session volume could not be set (see the module docs); the message names it.
    Failed(String),
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

/// A session the holder lowered (or is about to lower); its entry is in the restore file.
struct Owned {
    entry: RestoreEntry,
    /// The volume we last set; `None` = never lowered (e.g. `set_volume` failed).
    last_set: Option<f32>,
    release_from: f32,
    release: Release,
}

impl Owned {
    fn new(entry: RestoreEntry) -> Self {
        Self {
            entry,
            last_set: None,
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
    /// Gain reference: the largest original volume among the lowered sessions (1.0 if none).
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
}

/// True for a volume the holder lowers: finite, above `HELD_VOLUME` and not already held.
fn should_lower(v: f32) -> bool {
    v.is_finite() && v > HELD_VOLUME && !is_held(v)
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

/// Rewrites the restore file as the entries already in it whose instance id is not in
/// `owned_ids` (kept from earlier runs), followed by `ours`; deletes it when nothing remains.
/// An unreadable or corrupt file is an error (never overwritten).
fn update_file(path: &Path, ours: &[RestoreEntry], owned_ids: &HashSet<String>) -> io::Result<()> {
    let existing = restore::read(path)?.map(|r| r.entries).unwrap_or_default();
    let mut entries: Vec<RestoreEntry> = existing
        .into_iter()
        .filter(|e| !owned_ids.contains(&e.instance_id))
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
    /// (sessions at or below it are never lowered) and writes the restore file. Nothing is
    /// lowered here; the ramp runs in `tick`. A session whose volume cannot be read now is
    /// picked up by `follow`.
    pub fn begin_attach(&mut self, pid: u32, created_at: u64, now_us: u64) -> Result<(), String> {
        if self.stage != Stage::Idle {
            return Err(format!("cannot attach while {:?}", self.stage));
        }
        restore::restore(&self.restore_path, &self.sessions);
        let tree = self
            .sessions
            .sessions_for_tree(pid)
            .map_err(|e| format!("enumerating the player's sessions failed: {e}"))?;
        let mut owned = Vec::new();
        let mut unread = false;
        for s in &tree {
            match self.sessions.volume(&s.instance_id) {
                Ok(v) if should_lower(v) => {
                    owned.push(Owned::new(self.entry_for(s, v, pid, created_at)));
                }
                Ok(_) => {}
                // Picked up by `follow`; until then it may play at full level.
                Err(_) => unread = true,
            }
        }
        if !owned.is_empty() {
            let ours: Vec<RestoreEntry> = owned.iter().map(|o| o.entry.clone()).collect();
            let ids = ours.iter().map(|e| e.instance_id.clone()).collect();
            update_file(&self.restore_path, &ours, &ids)
                .map_err(|e| format!("writing the restore file failed: {e}"))?;
        }
        self.original = owned
            .iter()
            .map(|o| o.entry.original_volume)
            .reduce(f32::max)
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
        Ok(())
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
        for o in &mut self.owned {
            o.release_from = o.last_set.unwrap_or(o.entry.original_volume);
            o.release = if o.last_set.is_some() {
                Release::Pending
            } else {
                Release::NotLowered
            };
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
                self.settle_gain(now_us, true, false);
                return r;
            }
        };

        let mut degraded = false;
        let mut spiked = false;
        let mut candidates = Vec::new();
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
                Some(i) if self.owned[i].last_set.is_some() => {
                    if is_held(v) {
                        continue;
                    }
                    r.overridden += 1;
                    // Until lowered again it played at `v`: no more gain than original / v
                    // relative to its own original for the next window.
                    let o = &self.owned[i];
                    let id = o.entry.instance_id.clone();
                    self.record_gain(now_us, self.original * v / o.entry.original_volume);
                    spiked = true;
                    match self.sessions.set_volume(&id, HELD_VOLUME) {
                        Ok(()) => self.owned[i].last_set = Some(HELD_VOLUME),
                        Err(e) => {
                            r.fail(format!("lowering session {id} again failed: {e}"));
                            degraded = true;
                        }
                    }
                }
                Some(i) if is_held(v) => {
                    // Has an entry but was never lowered by us, yet is held now: treat it as
                    // held so the release restores it to the recorded original.
                    self.owned[i].last_set = Some(HELD_VOLUME);
                }
                owned => {
                    if should_lower(v) {
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

        if !candidates.is_empty() {
            match self.record_candidates(&candidates) {
                Err(e) => {
                    r.fail(format!("writing the restore file failed: {e}"));
                    degraded = true;
                }
                Ok(targets) => {
                    if !self.lowered_any {
                        // Nothing was held yet: the reference is the loudest session lowered now.
                        if let Some(m) = targets.iter().filter_map(|t| t.1).reduce(f32::max) {
                            self.original = m;
                        }
                    }
                    for (i, lower) in targets {
                        if lower.is_none() {
                            self.owned[i].last_set = Some(HELD_VOLUME); // adopted, already held
                            continue;
                        }
                        // It played at its original level until now: gain 1 for the window.
                        self.record_gain(now_us, self.original);
                        spiked = true;
                        let id = self.owned[i].entry.instance_id.clone();
                        match self.sessions.set_volume(&id, HELD_VOLUME) {
                            Ok(()) => {
                                self.owned[i].last_set = Some(HELD_VOLUME);
                                r.newly_lowered += 1;
                            }
                            Err(e) => {
                                r.fail(format!("lowering session {id} failed: {e}"));
                                degraded = true;
                            }
                        }
                    }
                    if r.newly_lowered > 0 && !self.lowered_any {
                        self.lowered_any = true;
                        self.set_attenuation(HELD_VOLUME / self.original);
                    }
                }
            }
        }
        self.settle_gain(now_us, degraded, spiked);
        r
    }

    /// Gain for the captured audio: `original / max(effective volume in the last 100 ms)`,
    /// never louder than the audio was before the takeover.
    pub fn capture_gain(&self, now_us: u64) -> f32 {
        self.gains.conservative_gain(self.original, now_us)
    }

    /// `HELD_VOLUME / original` while sessions are held; 1.0 otherwise.
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
        for o in &mut self.owned {
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
            if let Err(e) = update_file(&self.restore_path, &keep, &ids) {
                self.release_error
                    .get_or_insert(format!("updating the restore file failed: {e}"));
            }
        }
        self.owned.clear();
        self.stage = Stage::Idle;
        self.output_gain = 0.0;
        self.lowered_any = false;
    }

    /// The player process exited (or its pid was reused): restore through the restore file,
    /// which also handles sessions found again by their session identifier, and go idle.
    fn player_exited(&mut self, r: &mut FollowReport) {
        r.player_exited = true;
        let out = restore::restore(&self.restore_path, &self.sessions);
        if out.failed > 0 {
            r.failed += out.failed;
            r.error.get_or_insert(format!(
                "{} restore entries could not be restored yet (kept in the restore file)",
                out.failed
            ));
        }
        self.owned.clear();
        self.stage = Stage::Idle;
        self.output_gain = 0.0;
        self.lowered_any = false;
        self.attach_failure = None;
        self.gains = GainHistory::new(GAIN_WINDOW_US);
        self.effective = self.original;
        self.set_attenuation(1.0);
    }

    /// Writes the restore file with the candidates added (or their original updated) and,
    /// on success, records them as owned. Returns `(owned index, Some(volume) to lower or
    /// None when adopted)` per candidate.
    fn record_candidates(
        &mut self,
        candidates: &[Candidate],
    ) -> io::Result<Vec<(usize, Option<f32>)>> {
        let mut ours: Vec<RestoreEntry> = self.owned.iter().map(|o| o.entry.clone()).collect();
        let mut targets = Vec::with_capacity(candidates.len());
        for c in candidates {
            match c {
                Candidate::Lower {
                    owned: Some(i),
                    volume,
                    ..
                } => {
                    ours[*i].original_volume = *volume;
                    targets.push((*i, Some(*volume)));
                }
                Candidate::Lower {
                    owned: None,
                    session,
                    volume,
                } => {
                    targets.push((ours.len(), Some(*volume)));
                    ours.push(self.entry_for(session, *volume, self.pid, self.created_at));
                }
                Candidate::Adopt { session, original } => {
                    targets.push((ours.len(), None));
                    ours.push(self.entry_for(session, *original, self.pid, self.created_at));
                }
            }
        }
        let ids = ours.iter().map(|e| e.instance_id.clone()).collect();
        update_file(&self.restore_path, &ours, &ids)?;
        let known = self.owned.len();
        for (o, e) in self.owned.iter_mut().zip(&ours) {
            o.entry = e.clone();
        }
        self.owned
            .extend(ours.into_iter().skip(known).map(Owned::new));
        Ok(targets)
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
    /// session is held, otherwise `original` (gain 1). Recorded after any spike so the spike
    /// only covers its window.
    fn settle_gain(&mut self, now_us: u64, degraded: bool, spiked: bool) {
        let target = if degraded || !self.lowered_any {
            self.original
        } else {
            HELD_VOLUME
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
        // An unlowered session plays at full level: no compensation while it does.
        assert!(h.capture_gain(t + 200 * MS) <= 1.0 + 1e-6);

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

    #[test]
    fn begin_release_when_idle_is_a_noop() {
        let dir = TempDir::new("release-idle");
        let mut h = holder(&dir, &[("a", 0.5)]);
        h.begin_release(T0);
        assert_eq!(h.tick(T0), HolderPhase::Idle);
        assert_eq!(h.sessions().set_volume_calls(), 0);
    }
}
