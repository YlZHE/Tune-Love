//! Engine phase state machine (pure). The engine core feeds it inputs and rejects any
//! transition not listed in [`next`].

use devocal_core::protocol::Phase;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    Attach,
    AttachDone,
    Release,
    ReleaseDone,
    Failure,
    PlayerExited,
}

/// Next phase for `input`, or `Err(phase)` (unchanged) when the transition is not allowed.
///
/// Allowed: `Idle + Attach -> Attaching`, `Attaching + AttachDone -> Active`,
/// `Attaching | Active + Release -> Releasing`, `Releasing + ReleaseDone -> Idle`,
/// any phase `+ Failure -> Releasing`, `Active + PlayerExited -> Idle`.
pub fn next(phase: Phase, input: Input) -> Result<Phase, Phase> {
    use Input::*;
    use Phase::*;
    match (phase, input) {
        (_, Failure) => Ok(Releasing),
        (Idle, Attach) => Ok(Attaching),
        (Attaching, AttachDone) => Ok(Active),
        (Attaching | Active, Release) => Ok(Releasing),
        (Releasing, ReleaseDone) => Ok(Idle),
        (Active, PlayerExited) => Ok(Idle),
        (p, _) => Err(p),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Input::*;
    use Phase::*;

    #[test]
    fn state_transitions_table() {
        let allowed = [
            (Idle, Attach, Attaching),
            (Attaching, AttachDone, Active),
            (Attaching, Release, Releasing),
            (Active, Release, Releasing),
            (Releasing, ReleaseDone, Idle),
            (Idle, Failure, Releasing),
            (Attaching, Failure, Releasing),
            (Active, Failure, Releasing),
            (Releasing, Failure, Releasing),
            (Active, PlayerExited, Idle),
        ];
        for (from, input, to) in allowed {
            assert_eq!(next(from, input), Ok(to), "{from:?} + {input:?}");
        }

        let phases = [Idle, Attaching, Active, Releasing];
        let inputs = [
            Attach,
            AttachDone,
            Release,
            ReleaseDone,
            Failure,
            PlayerExited,
        ];
        let mut rejected = 0;
        for from in phases {
            for input in inputs {
                if allowed.iter().any(|&(f, i, _)| f == from && i == input) {
                    continue;
                }
                assert_eq!(next(from, input), Err(from), "{from:?} + {input:?}");
                rejected += 1;
            }
        }
        assert_eq!(rejected, 24 - allowed.len());

        // Explicit illegal combinations.
        assert_eq!(next(Idle, AttachDone), Err(Idle));
        assert_eq!(next(Idle, Release), Err(Idle));
        assert_eq!(next(Attaching, Attach), Err(Attaching));
        assert_eq!(next(Attaching, PlayerExited), Err(Attaching));
        assert_eq!(next(Active, Attach), Err(Active));
        assert_eq!(next(Active, ReleaseDone), Err(Active));
        assert_eq!(next(Releasing, Attach), Err(Releasing));
        assert_eq!(next(Releasing, PlayerExited), Err(Releasing));
        assert_eq!(next(Idle, PlayerExited), Err(Idle));
    }
}
