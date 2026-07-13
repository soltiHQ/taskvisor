//! Default task execution settings.

use std::num::NonZeroU32;
use std::time::Duration;

use crate::core::ConfigError;
use crate::policies::{BackoffPolicy, RestartPolicy};

/// Converts `Some(Duration::ZERO)` to `None`.
#[inline]
fn normalize_timeout(timeout: Option<Duration>) -> Option<Duration> {
    timeout.filter(|duration| !duration.is_zero())
}

/// Default execution settings for task specifications.
///
/// A [`TaskSpec`](crate::TaskSpec) may inherit any of these values. An explicit
/// setting on the task specification always wins. Use
/// [`TaskSpec::from_defaults`](crate::TaskSpec::from_defaults) to inherit every
/// field, or a named constructor such as [`TaskSpec::restartable`](crate::TaskSpec::restartable)
/// to set the restart mode while inheriting the remaining fields.
#[derive(Clone, Debug)]
#[must_use]
pub struct TaskDefaults {
    restart: RestartPolicy,
    backoff: BackoffPolicy,
    timeout: Option<Duration>,
    max_retries: Option<NonZeroU32>,
}

impl TaskDefaults {
    /// Returns the default restart policy.
    #[must_use]
    pub fn restart(&self) -> RestartPolicy {
        self.restart
    }

    /// Returns the default backoff policy.
    #[must_use]
    pub fn backoff(&self) -> BackoffPolicy {
        self.backoff
    }

    /// Returns the default timeout for one task attempt.
    ///
    /// `None` means that attempts have no default timeout.
    #[must_use]
    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    /// Returns the default failure-retry limit.
    ///
    /// `None` means that failure retries are unlimited by default.
    #[must_use]
    pub fn max_retries(&self) -> Option<NonZeroU32> {
        self.max_retries
    }

    /// Sets the default restart policy.
    pub fn with_restart(mut self, restart: RestartPolicy) -> Self {
        self.restart = restart;
        self
    }

    /// Sets the default backoff policy.
    pub fn with_backoff(mut self, backoff: BackoffPolicy) -> Self {
        self.backoff = backoff;
        self
    }

    /// Sets the default timeout for one task attempt.
    ///
    /// Accepts either a [`Duration`] or an `Option<Duration>`; pass either form
    /// directly without calling `.into()`.
    /// `None`, `Duration::ZERO`, and `Some(Duration::ZERO)` mean no default timeout.
    pub fn with_timeout(mut self, timeout: impl Into<Option<Duration>>) -> Self {
        self.timeout = normalize_timeout(timeout.into());
        self
    }

    /// Sets the default failure-retry limit.
    ///
    /// Accepts either a [`NonZeroU32`] or an `Option<NonZeroU32>`; pass either
    /// form directly without calling `.into()`. Pass `None` for unlimited
    /// failure retries.
    pub fn with_max_retries(mut self, max_retries: impl Into<Option<NonZeroU32>>) -> Self {
        self.max_retries = max_retries.into();
        self
    }

    /// Convenience setter that validates a raw failure-retry limit.
    ///
    /// # Errors
    /// Returns [`ConfigError::Zero`] when `max_retries` is zero. Use
    /// [`with_max_retries`](Self::with_max_retries) with `None` for unlimited retries.
    pub fn try_with_max_retries(self, max_retries: u32) -> Result<Self, ConfigError> {
        let max_retries = NonZeroU32::new(max_retries).ok_or(ConfigError::Zero {
            field: "max_retries",
        })?;
        Ok(self.with_max_retries(max_retries))
    }
}

impl Default for TaskDefaults {
    /// Returns the default task execution settings.
    ///
    /// Defaults:
    /// - restart after failure,
    /// - exponential backoff from 200 ms to 30 seconds with equal jitter,
    /// - no attempt timeout,
    /// - unlimited failure retries.
    fn default() -> Self {
        Self {
            restart: RestartPolicy::default(),
            backoff: BackoffPolicy::default(),
            timeout: None,
            max_retries: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::JitterPolicy;

    #[test]
    fn default_contract_is_explicit_and_safe() {
        let defaults = TaskDefaults::default();

        assert!(matches!(defaults.restart(), RestartPolicy::OnFailure));
        assert_eq!(defaults.backoff().first(), Duration::from_millis(200));
        assert_eq!(defaults.backoff().factor(), 2.0);
        assert_eq!(defaults.backoff().max(), Duration::from_secs(30));
        assert_eq!(defaults.backoff().jitter(), JitterPolicy::Equal);
        assert_eq!(defaults.timeout(), None);
        assert_eq!(defaults.max_retries(), None);
    }

    #[test]
    fn builders_replace_each_default() {
        let backoff = BackoffPolicy::constant(Duration::from_secs(2));
        let retries = NonZeroU32::new(4).unwrap();
        let defaults = TaskDefaults::default()
            .with_restart(RestartPolicy::Never)
            .with_backoff(backoff)
            .with_timeout(Duration::from_secs(10))
            .with_max_retries(retries);

        assert!(matches!(defaults.restart(), RestartPolicy::Never));
        assert_eq!(defaults.backoff().first(), Duration::from_secs(2));
        assert_eq!(defaults.timeout(), Some(Duration::from_secs(10)));
        assert_eq!(defaults.max_retries(), Some(retries));
    }

    #[test]
    fn zero_timeout_means_no_default_timeout() {
        let direct = TaskDefaults::default().with_timeout(Duration::ZERO);
        let optional = TaskDefaults::default().with_timeout(Some(Duration::ZERO));

        assert_eq!(direct.timeout(), None);
        assert_eq!(optional.timeout(), None);
    }

    #[test]
    fn none_disables_the_default_timeout_without_type_annotation() {
        let defaults = TaskDefaults::default()
            .with_timeout(Duration::from_secs(1))
            .with_timeout(None);

        assert_eq!(defaults.timeout(), None);
    }

    #[test]
    fn raw_zero_retry_limit_returns_a_clear_error() {
        assert_eq!(
            TaskDefaults::default().try_with_max_retries(0).unwrap_err(),
            ConfigError::Zero {
                field: "max_retries"
            }
        );
    }
}
