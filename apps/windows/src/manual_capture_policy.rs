use std::time::{Duration, Instant};

pub(crate) const MANUAL_SHORTCUT_RELEASE_GRACE: Duration = Duration::from_millis(1_000);

// Independent of the Windows hook APIs so cancellation ordering can be
// exercised on any host without sending a real key or touching the clipboard.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ShortcutReleaseGuard {
    input_sequence: u64,
    shortcut_key: Option<u32>,
    modifier_releases: [bool; 4],
    expires_at: Instant,
}

impl ShortcutReleaseGuard {
    pub(crate) fn new(
        input_sequence: u64,
        shortcut_key: Option<u32>,
        modifier_releases: [bool; 4],
        now: Instant,
    ) -> Self {
        Self {
            input_sequence,
            shortcut_key,
            modifier_releases,
            expires_at: now + MANUAL_SHORTCUT_RELEASE_GRACE,
        }
    }

    pub(crate) fn consume(
        &mut self,
        sequence: u64,
        key_up: bool,
        virtual_key: u32,
        modifier_group: Option<usize>,
        now: Instant,
    ) -> bool {
        if sequence <= self.input_sequence {
            return true;
        }
        if now >= self.expires_at || !key_up {
            return false;
        }
        if self.shortcut_key == Some(virtual_key) {
            self.shortcut_key = None;
            return true;
        }
        modifier_group
            .and_then(|group| self.modifier_releases.get_mut(group))
            .is_some_and(std::mem::take)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_key_up_is_exact_and_consumed_once() {
        let now = Instant::now();
        let mut guard = ShortcutReleaseGuard::new(10, Some(67), [false; 4], now);
        assert!(guard.consume(10, false, 67, None, now));
        assert!(!guard.consume(11, false, 67, None, now));
        assert!(!guard.consume(11, true, 86, None, now));
        assert!(guard.consume(11, true, 67, None, now));
        assert!(!guard.consume(12, true, 67, None, now));
    }

    #[test]
    fn only_held_modifier_releases_are_consumed() {
        let now = Instant::now();
        let mut guard = ShortcutReleaseGuard::new(10, Some(67), [false, true, false, false], now);
        assert!(!guard.consume(11, false, 17, Some(1), now));
        assert!(guard.consume(11, true, 17, Some(1), now));
        assert!(!guard.consume(12, true, 17, Some(1), now));
        assert!(!guard.consume(12, true, 16, Some(0), now));
        // A fresh Ctrl+C is user input even if the trigger used the same key.
        assert!(!guard.consume(12, false, 67, None, now));
    }

    #[test]
    fn release_grace_expires_and_plain_commands_have_no_trigger_key() {
        let now = Instant::now();
        let mut guard = ShortcutReleaseGuard::new(10, Some(67), [false; 4], now);
        assert!(!guard.consume(11, true, 67, None, now + MANUAL_SHORTCUT_RELEASE_GRACE));
        let mut command_guard = ShortcutReleaseGuard::new(10, None, [false; 4], now);
        assert!(!command_guard.consume(11, true, 67, None, now));
    }
}
