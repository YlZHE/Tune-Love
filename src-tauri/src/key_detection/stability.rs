use super::{
    scale_match::{AutoTuneTarget, ScaleMatcher, FROZEN},
    song_cache::{song_id, SongCache},
    ChromaEvidence, MusicalKey,
};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub source_id: String,
    pub track_key: String,
    pub target_generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    pub identity: Option<Identity>,
    pub playing: bool,
    pub capture_generation: u64,
    pub sample_end_sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnalysisToken {
    identity: Identity,
    capture_generation: u64,
    sample_end_sequence: u64,
    lifecycle_generation: u64,
    job_id: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StableSnapshot {
    pub identity: Option<Identity>,
    pub status: &'static str,
    pub key: Option<MusicalKey>,
    pub updated_at_ms: u64,
    /// Scale-match recommendation for the current song; None without a song.
    pub autotune_target: Option<AutoTuneTarget>,
}

pub struct Stabilizer {
    identity: Option<Identity>,
    playing: bool,
    capture_generation: u64,
    confirmed: Option<MusicalKey>,
    pending: Option<MusicalKey>,
    pending_votes: u8,
    last_window_sequence: Option<u64>,
    active: Option<AnalysisToken>,
    next_job_id: u64,
    lifecycle_generation: u64,
    running: bool,
    status: &'static str,
    updated_at_ms: u64,
    /// Song-scoped evidence: cleared with identity, kept across pause/seek epochs.
    matcher: ScaleMatcher,
    /// Remembered results of earlier plays; consulted only on a song change.
    cache: Option<Arc<Mutex<SongCache>>>,
}

impl Default for Stabilizer {
    fn default() -> Self {
        Self::new()
    }
}

impl Stabilizer {
    pub fn new() -> Self {
        Self {
            identity: None,
            playing: false,
            capture_generation: 0,
            confirmed: None,
            pending: None,
            pending_votes: 0,
            last_window_sequence: None,
            active: None,
            next_job_id: 0,
            lifecycle_generation: 0,
            running: true,
            status: "idle",
            updated_at_ms: 0,
            matcher: ScaleMatcher::new(FROZEN),
            cache: None,
        }
    }

    pub fn set_cache(&mut self, cache: Arc<Mutex<SongCache>>) {
        self.cache = Some(cache);
    }

    fn clear_pending(&mut self) {
        self.pending = None;
        self.pending_votes = 0;
    }

    fn resting_status(&self) -> &'static str {
        if self.confirmed.is_some() {
            "detected"
        } else {
            "idle"
        }
    }

    pub fn observe(&mut self, observed: &Observation, now: u64) -> bool {
        // Audio capture_generation is a process-local monotonic PCM epoch. It
        // advances for target changes, pause/stop, resets, stalls, and reconnects,
        // including observations with no current identity. An already-read older
        // context may arrive after a newer one, but it must never roll state back.
        if observed.capture_generation < self.capture_generation {
            return false;
        }
        if self.identity != observed.identity {
            self.identity = observed.identity.clone();
            self.playing = observed.playing;
            self.capture_generation = observed.capture_generation;
            self.confirmed = None;
            self.clear_pending();
            self.last_window_sequence = None;
            self.active = None;
            self.lifecycle_generation = self.lifecycle_generation.wrapping_add(1);
            self.status = "idle";
            self.updated_at_ms = now;
            self.matcher.reset();
            // Lock order: stabilizer, then cache (in-memory lookup only).
            if let (Some(identity), Some(cache)) = (self.identity.as_ref(), self.cache.as_ref()) {
                let remembered = song_id(&identity.track_key)
                    .and_then(|id| cache.lock().unwrap_or_else(|e| e.into_inner()).get(&id));
                if let Some(hit) = remembered {
                    self.matcher.seed(hit.key, hit.scale);
                }
            }
            return true;
        }

        if self.capture_generation != observed.capture_generation {
            self.capture_generation = observed.capture_generation;
            self.clear_pending();
            self.last_window_sequence = None;
            self.active = None;
            self.lifecycle_generation = self.lifecycle_generation.wrapping_add(1);
            self.status = self.resting_status();
        }
        if self.playing != observed.playing {
            self.playing = observed.playing;
            self.clear_pending();
            self.active = None;
            self.lifecycle_generation = self.lifecycle_generation.wrapping_add(1);
            self.status = self.resting_status();
        }
        true
    }

    #[cfg(test)]
    pub fn begin(&mut self, observed: &Observation, now: u64) -> Option<AnalysisToken> {
        self.begin_if_current(observed, observed, now)
    }

    pub fn can_begin(&self, observed: &Observation) -> bool {
        self.running
            && self.playing
            && self.active.is_none()
            && self.identity == observed.identity
            && self.capture_generation == observed.capture_generation
            && self
                .last_window_sequence
                .is_none_or(|last| observed.sample_end_sequence > last)
    }

    pub fn begin_if_current(
        &mut self,
        window: &Observation,
        current: &Observation,
        now: u64,
    ) -> Option<AnalysisToken> {
        if !self.observe(current, now) {
            return None;
        }
        if window.identity != current.identity
            || window.capture_generation != current.capture_generation
            || !current.playing
        {
            return None;
        }
        let identity = self.identity.clone()?;
        if !self.running || !self.playing || self.active.is_some() {
            return None;
        }
        if self
            .last_window_sequence
            .is_some_and(|last| window.sample_end_sequence <= last)
        {
            return None;
        }
        self.next_job_id = self.next_job_id.wrapping_add(1);
        let token = AnalysisToken {
            identity,
            capture_generation: window.capture_generation,
            sample_end_sequence: window.sample_end_sequence,
            lifecycle_generation: self.lifecycle_generation,
            job_id: self.next_job_id,
        };
        self.last_window_sequence = Some(window.sample_end_sequence);
        self.active = Some(token.clone());
        if self.confirmed.is_none() {
            self.status = "analyzing";
        }
        Some(token)
    }

    #[cfg(test)]
    pub fn complete(
        &mut self,
        token: &AnalysisToken,
        observed: &Observation,
        candidate: Option<MusicalKey>,
        failed: bool,
        now: u64,
    ) {
        self.complete_with_evidence(token, observed, candidate, None, failed, now)
    }

    /// Like `complete`, also folding this step's pitch-class evidence into the
    /// scale matcher. Evidence passes the same token/identity checks as the key
    /// vote, so a stale window can never feed a newer song.
    pub fn complete_with_evidence(
        &mut self,
        token: &AnalysisToken,
        observed: &Observation,
        candidate: Option<MusicalKey>,
        evidence: Option<ChromaEvidence>,
        failed: bool,
        now: u64,
    ) {
        if !self.observe(observed, now) {
            return;
        }
        if !self.running
            || self.active.as_ref() != Some(token)
            || observed.identity.as_ref() != Some(&token.identity)
            || observed.capture_generation != token.capture_generation
            || !observed.playing
        {
            return;
        }
        self.active = None;

        if let Some(step) = evidence.filter(|_| !failed) {
            self.matcher.add(&step);
        }

        if failed {
            self.confirmed = None;
            self.clear_pending();
            self.status = "unavailable";
            self.updated_at_ms = now;
            return;
        }

        let Some(candidate) = candidate else {
            self.clear_pending();
            self.status = self.resting_status();
            return;
        };

        if candidate.pitch_class >= 12 {
            self.clear_pending();
            self.status = self.resting_status();
            return;
        }

        if self.confirmed == Some(candidate) {
            self.clear_pending();
            self.status = "detected";
            return;
        }

        if self.pending == Some(candidate) {
            self.pending_votes = self.pending_votes.saturating_add(1);
        } else {
            self.pending = Some(candidate);
            self.pending_votes = 1;
        }
        let required = if self.confirmed.is_some() { 5 } else { 3 };
        if self.pending_votes >= required {
            self.confirmed = Some(candidate);
            self.clear_pending();
            self.updated_at_ms = now;
        }
        self.status = self.resting_status();
    }

    pub fn stop(&mut self, now: u64) {
        self.running = false;
        self.clear_pending();
        self.active = None;
        self.lifecycle_generation = self.lifecycle_generation.wrapping_add(1);
        self.status = self.resting_status();
        self.updated_at_ms = now;
    }

    pub fn snapshot(&self) -> StableSnapshot {
        StableSnapshot {
            identity: self.identity.clone(),
            status: self.status,
            key: self.confirmed,
            updated_at_ms: self.updated_at_ms,
            autotune_target: self.identity.as_ref().and_then(|_| self.matcher.target()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key_detection::{Mode, MusicalKey};

    fn key(pitch_class: u8) -> MusicalKey {
        MusicalKey {
            pitch_class,
            mode: Mode::Major,
        }
    }

    fn c_major_step() -> ChromaEvidence {
        let mut chroma = [0.0; 12];
        for pc in [0, 2, 4, 5, 7, 9, 11] {
            chroma[pc] = 1.0;
        }
        ChromaEvidence {
            chroma,
            seconds: 1.0,
            rms: 0.1,
        }
    }

    fn obs(
        identity: Option<(&str, u64)>,
        playing: bool,
        capture: u64,
        sequence: u64,
    ) -> Observation {
        Observation {
            identity: identity.map(|(track, target_generation)| Identity {
                source_id: "player".into(),
                track_key: track.into(),
                target_generation,
            }),
            playing,
            capture_generation: capture,
            sample_end_sequence: sequence,
        }
    }

    #[test]
    fn a_remembered_song_starts_from_its_cached_target_and_others_do_not() {
        use super::super::scale_match::{Scale, TargetSource};
        let cache = Arc::new(Mutex::new(SongCache::default()));
        let track = serde_json::to_string(&["player", "Song", "Artist", "Album"]).unwrap();
        let other = serde_json::to_string(&["player", "Other", "Artist", "Album"]).unwrap();
        cache.lock().unwrap().put(
            &song_id(&track).unwrap(),
            &AutoTuneTarget {
                key: 6,
                scale: Scale::Minor,
                evidence_seconds: 60.0,
                source: TargetSource::Analysis,
            },
            1,
        );
        let mut stabilizer = Stabilizer::new();
        stabilizer.set_cache(cache);
        let seen = |key: &str| Observation {
            identity: Some(Identity {
                source_id: "player".into(),
                track_key: key.into(),
                target_generation: 1,
            }),
            playing: true,
            capture_generation: 1,
            sample_end_sequence: 0,
        };
        stabilizer.observe(&seen(&track), 1);
        let target = stabilizer.snapshot().autotune_target.unwrap();
        assert_eq!(
            (target.key, target.scale, target.source),
            (6, Scale::Minor, TargetSource::Cache)
        );
        stabilizer.observe(&seen(&other), 2);
        assert!(stabilizer.snapshot().autotune_target.is_none());
    }

    #[test]
    fn evidence_narrows_the_target_only_under_a_live_token_and_resets_with_the_song() {
        let mut stabilizer = Stabilizer::new();
        let observed = obs(Some(("track", 1)), true, 1, 0);
        let mut now = 1;
        stabilizer.observe(&observed, now);
        assert!(
            stabilizer.snapshot().autotune_target.is_none(),
            "undecided before evidence"
        );
        for step in 1..=30_u64 {
            let current = obs(Some(("track", 1)), true, 1, step * 48_000);
            let token = stabilizer.begin(&current, now).expect("token");
            now += 1;
            stabilizer.complete_with_evidence(
                &token,
                &current,
                Some(key(0)),
                Some(c_major_step()),
                false,
                now,
            );
        }
        let target = stabilizer.snapshot().autotune_target.unwrap();
        assert_eq!((target.key, target.scale), (0, super::super::Scale::Major));
        assert_eq!(target.evidence_seconds, 30.0);

        // A stale token (new epoch) must not feed evidence.
        let current = obs(Some(("track", 1)), true, 1, 31 * 48_000);
        let token = stabilizer.begin(&current, now).expect("token");
        let newer = obs(Some(("track", 1)), true, 2, 31 * 48_000);
        stabilizer.complete_with_evidence(
            &token,
            &newer,
            Some(key(0)),
            Some(c_major_step()),
            false,
            now + 1,
        );
        assert_eq!(
            stabilizer
                .snapshot()
                .autotune_target
                .unwrap()
                .evidence_seconds,
            30.0
        );

        // A failed analysis carries no evidence either.
        let current = obs(Some(("track", 1)), true, 2, 32 * 48_000);
        let token = stabilizer.begin(&current, now + 2).expect("token");
        stabilizer.complete_with_evidence(
            &token,
            &current,
            None,
            Some(c_major_step()),
            true,
            now + 3,
        );
        assert_eq!(
            stabilizer
                .snapshot()
                .autotune_target
                .unwrap()
                .evidence_seconds,
            30.0
        );

        // Pause keeps the song's evidence; a new track forgets it.
        stabilizer.observe(&obs(Some(("track", 1)), false, 2, 32 * 48_000), now + 4);
        assert_eq!(
            stabilizer
                .snapshot()
                .autotune_target
                .unwrap()
                .evidence_seconds,
            30.0
        );
        stabilizer.observe(&obs(Some(("next", 2)), true, 3, 0), now + 5);
        assert!(
            stabilizer.snapshot().autotune_target.is_none(),
            "new track starts undecided"
        );
        stabilizer.observe(&obs(None, false, 3, 0), now + 6);
        assert!(stabilizer.snapshot().autotune_target.is_none());
    }

    fn observation(
        track: &str,
        target_generation: u64,
        capture_generation: u64,
        sequence: u64,
    ) -> Observation {
        Observation {
            identity: Some(Identity {
                source_id: "player".into(),
                track_key: track.into(),
                target_generation,
            }),
            playing: true,
            capture_generation,
            sample_end_sequence: sequence,
        }
    }

    fn vote(
        state: &mut Stabilizer,
        observed: Observation,
        candidate: Option<MusicalKey>,
        now: u64,
    ) {
        state.observe(&observed, now);
        let token = state.begin(&observed, now).expect("fresh window");
        state.complete(&token, &observed, candidate, false, now);
    }

    #[test]
    fn three_distinct_windows_publish_but_duplicate_window_does_not_vote() {
        let mut state = Stabilizer::new();
        vote(&mut state, observation("a", 1, 1, 10), Some(key(0)), 10);
        state.observe(&observation("a", 1, 1, 10), 11);
        assert!(state.begin(&observation("a", 1, 1, 10), 11).is_none());
        vote(&mut state, observation("a", 1, 1, 20), Some(key(0)), 20);
        assert_eq!(state.snapshot().key, None);
        vote(&mut state, observation("a", 1, 1, 30), Some(key(0)), 30);
        assert_eq!(state.snapshot().key, Some(key(0)));
    }

    #[test]
    fn different_or_invalid_candidate_breaks_consecutive_initial_streak() {
        let mut state = Stabilizer::new();
        vote(&mut state, observation("a", 1, 1, 10), Some(key(0)), 10);
        vote(&mut state, observation("a", 1, 1, 20), Some(key(2)), 20);
        vote(&mut state, observation("a", 1, 1, 30), Some(key(0)), 30);
        vote(&mut state, observation("a", 1, 1, 40), None, 40);
        vote(&mut state, observation("a", 1, 1, 50), Some(key(0)), 50);
        vote(&mut state, observation("a", 1, 1, 60), Some(key(0)), 60);
        assert_eq!(state.snapshot().key, None);
        vote(&mut state, observation("a", 1, 1, 70), Some(key(0)), 70);
        assert_eq!(state.snapshot().key, Some(key(0)));
    }

    #[test]
    fn malformed_candidate_interrupts_consecutive_valid_votes() {
        let mut state = Stabilizer::new();
        vote(&mut state, observation("a", 1, 1, 10), Some(key(0)), 10);
        vote(&mut state, observation("a", 1, 1, 20), Some(key(0)), 20);
        vote(&mut state, observation("a", 1, 1, 30), Some(key(12)), 30);
        vote(&mut state, observation("a", 1, 1, 40), Some(key(0)), 40);
        assert_eq!(state.snapshot().key, None);
    }

    #[test]
    fn five_new_matching_candidates_replace_a_confirmed_result() {
        let mut state = Stabilizer::new();
        for sequence in [10, 20, 30] {
            vote(
                &mut state,
                observation("a", 1, 1, sequence),
                Some(key(0)),
                sequence,
            );
        }
        for sequence in [40, 50, 60, 70] {
            vote(
                &mut state,
                observation("a", 1, 1, sequence),
                Some(key(7)),
                sequence,
            );
        }
        assert_eq!(state.snapshot().key, Some(key(0)));
        vote(&mut state, observation("a", 1, 1, 80), Some(key(7)), 80);
        assert_eq!(state.snapshot().key, Some(key(7)));
    }

    #[test]
    fn pause_preserves_only_confirmed_result_and_track_change_clears_immediately() {
        let mut state = Stabilizer::new();
        vote(&mut state, observation("a", 1, 1, 10), Some(key(0)), 10);
        vote(&mut state, observation("a", 1, 1, 20), Some(key(0)), 20);
        let mut paused = observation("a", 1, 1, 20);
        paused.playing = false;
        state.observe(&paused, 21);
        let mut resumed = paused.clone();
        resumed.playing = true;
        resumed.sample_end_sequence = 30;
        vote(&mut state, resumed, Some(key(0)), 30);
        assert_eq!(
            state.snapshot().key,
            None,
            "unconfirmed streak must be cleared by pause"
        );
        vote(&mut state, observation("a", 1, 1, 40), Some(key(0)), 40);
        vote(&mut state, observation("a", 1, 1, 50), Some(key(0)), 50);
        let mut paused_confirmed = observation("a", 1, 1, 50);
        paused_confirmed.playing = false;
        state.observe(&paused_confirmed, 51);
        assert_eq!(state.snapshot().key, Some(key(0)));
        state.observe(&observation("b", 2, 2, 60), 60);
        assert_eq!(state.snapshot().key, None);
    }

    #[test]
    fn stale_result_cannot_disturb_votes_for_current_target() {
        let mut state = Stabilizer::new();
        let old = observation("a", 1, 1, 10);
        state.observe(&old, 10);
        let stale = state.begin(&old, 10).unwrap();

        let current = observation("b", 2, 2, 20);
        state.observe(&current, 20);
        let current_token = state.begin(&current, 20).unwrap();
        state.complete(&stale, &current, None, true, 21);
        state.complete(&current_token, &current, Some(key(7)), false, 22);
        vote(&mut state, observation("b", 2, 2, 30), Some(key(7)), 30);
        vote(&mut state, observation("b", 2, 2, 40), Some(key(7)), 40);
        assert_eq!(state.snapshot().key, Some(key(7)));
    }

    #[test]
    fn current_native_failure_clears_confirmed_result_and_reports_unavailable() {
        let mut state = Stabilizer::new();
        for sequence in [10, 20, 30] {
            vote(
                &mut state,
                observation("a", 1, 1, sequence),
                Some(key(0)),
                sequence,
            );
        }
        let failed = observation("a", 1, 1, 40);
        state.observe(&failed, 40);
        let token = state.begin(&failed, 40).unwrap();
        state.complete(&token, &failed, None, true, 41);
        let snapshot = state.snapshot();
        assert_eq!(snapshot.status, "unavailable");
        assert_eq!(snapshot.key, None);
    }

    #[test]
    fn current_silence_keeps_confirmed_result_but_clears_pending_replacement() {
        let mut state = Stabilizer::new();
        for sequence in [10, 20, 30] {
            vote(
                &mut state,
                observation("a", 1, 1, sequence),
                Some(key(0)),
                sequence,
            );
        }
        vote(&mut state, observation("a", 1, 1, 40), Some(key(7)), 40);
        vote(&mut state, observation("a", 1, 1, 50), None, 50);
        for sequence in [60, 70, 80, 90] {
            vote(
                &mut state,
                observation("a", 1, 1, sequence),
                Some(key(7)),
                sequence,
            );
        }
        let snapshot = state.snapshot();
        assert_eq!(snapshot.status, "detected");
        assert_eq!(snapshot.key, Some(key(0)));
    }

    #[test]
    fn stale_native_failure_cannot_clear_current_confirmed_result() {
        let mut state = Stabilizer::new();
        let stale_observation = observation("old", 1, 1, 10);
        state.observe(&stale_observation, 10);
        let stale = state.begin(&stale_observation, 10).unwrap();
        for sequence in [20, 30, 40] {
            vote(
                &mut state,
                observation("current", 2, 2, sequence),
                Some(key(7)),
                sequence,
            );
        }
        let current = observation("current", 2, 2, 40);
        state.complete(&stale, &current, None, true, 41);
        let snapshot = state.snapshot();
        assert_eq!(snapshot.status, "detected");
        assert_eq!(snapshot.key, Some(key(7)));
    }

    #[test]
    fn capture_generation_mismatches_and_stop_invalidate_old_tokens() {
        let mut state = Stabilizer::new();
        let first = observation("a", 1, 1, 10);
        state.observe(&first, 10);
        let old_capture = state.begin(&first, 10).unwrap();
        let reset = observation("a", 1, 2, 20);
        state.observe(&reset, 20);
        state.complete(&old_capture, &reset, Some(key(0)), false, 21);
        assert_eq!(state.snapshot().key, None);

        let stop_token = state.begin(&reset, 22).unwrap();
        state.stop(23);
        state.complete(&stop_token, &reset, Some(key(0)), false, 24);
        assert_eq!(state.snapshot().key, None);
    }

    #[test]
    fn stale_window_cannot_begin_after_current_identity_was_observed() {
        let mut state = Stabilizer::new();
        let stale = observation("a", 1, 1, 10);
        let current = observation("a", 3, 2, 20);
        state.observe(&current, 20);
        assert!(state.begin_if_current(&stale, &current, 21).is_none());
        assert_eq!(state.snapshot().identity, current.identity);
    }

    #[test]
    fn delayed_older_context_cannot_restore_identity_or_pause_state() {
        let mut state = Stabilizer::new();
        let old = observation("a", 1, 1, 10);
        let current = observation("b", 2, 3, 30);
        state.observe(&current, 30);
        state.observe(&old, 31);
        assert_eq!(state.snapshot().identity, current.identity);

        let mut paused = current.clone();
        paused.playing = false;
        paused.capture_generation = 4;
        state.observe(&paused, 40);
        state.observe(&current, 41);
        assert!(state.begin(&paused, 42).is_none());
        assert_eq!(state.snapshot().identity, paused.identity);

        let unavailable = Observation {
            identity: None,
            playing: false,
            capture_generation: 5,
            sample_end_sequence: 30,
        };
        state.observe(&unavailable, 50);
        state.observe(&paused, 51);
        assert_eq!(state.snapshot().identity, None);
    }
}
