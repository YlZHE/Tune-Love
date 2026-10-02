//! Gain for the app's own capture of the player while the engine holds its sessions.
//!
//! While the engine holds a player, the player's sessions play at `HELD_VOLUME`, so the
//! app's process-loopback capture (key detection) records a signal attenuated by
//! `attenuation`. The [`Supervisor`](super::supervisor::Supervisor) publishes here what the
//! capture has to do with the data it records:
//! - `None`: the attenuation is unknown (attaching, releasing, engine gone): drop the data;
//! - `Some(g)`: multiply by `g` (`1.0` when nothing is held).
//!
//! Every `set` bumps the epoch, so a reader can invalidate history recorded under another gain.

use std::sync::Mutex;

pub struct AttenuationGate {
    state: Mutex<(u64, Option<f32>)>,
}

impl Default for AttenuationGate {
    fn default() -> Self {
        Self::new()
    }
}

impl AttenuationGate {
    /// Epoch 0, gain `Some(1.0)` (nothing held).
    pub fn new() -> Self {
        Self {
            state: Mutex::new((0, Some(1.0))),
        }
    }

    /// Publishes a new gain; the epoch always advances, even if the value is the same.
    pub fn set(&self, gain: Option<f32>) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.0 = s.0.wrapping_add(1);
        s.1 = gain;
    }

    /// `(epoch, gain)`.
    pub fn read(&self) -> (u64, Option<f32>) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_at_unity_and_every_set_bumps_the_epoch() {
        let gate = AttenuationGate::new();
        assert_eq!(gate.read(), (0, Some(1.0)));
        gate.set(None);
        assert_eq!(gate.read(), (1, None));
        gate.set(None);
        assert_eq!(gate.read(), (2, None));
        gate.set(Some(3000.0));
        assert_eq!(gate.read(), (3, Some(3000.0)));
    }
}
