//! The ways a step can fail, in the Python build's exception classes. The class matters: `run_many`
//! attaches no screenshot to a failure that happened before any input was sent, and that is decided
//! by whether the exception was a `ValueError` or `PermissionError` (see [`StepError::is_validation`]).

use std::fmt;

use crate::ack::AckRefused;
use crate::backend::BackendError;
use crate::destructive::DestructiveAction;
use crate::human::HumanActive;
use crate::input::TypeError;
use crate::keys::KeyComboError;
use crate::scale::ScaleError;

/// Why a step stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepError {
    /// A bad argument: Python `ValueError` (and `KeyComboError`, the scale errors).
    Value(String),
    /// A target that is missing, refused or ambiguous: Python `TargetError`, a `ValueError`.
    Target(String),
    /// A refusal: Python `PermissionError` (protected window, destructive action, not acknowledged,
    /// paused).
    Permission(String),
    /// Anything else (`RuntimeError`, `OSError`): the OS failed or the outcome is uncertain.
    Runtime(String),
}

impl StepError {
    /// A `ValueError`.
    pub fn value(msg: impl Into<String>) -> Self {
        StepError::Value(msg.into())
    }

    /// A `TargetError`.
    pub fn target(msg: impl Into<String>) -> Self {
        StepError::Target(msg.into())
    }

    /// A `PermissionError`.
    pub fn permission(msg: impl Into<String>) -> Self {
        StepError::Permission(msg.into())
    }

    /// A `RuntimeError`.
    pub fn runtime(msg: impl Into<String>) -> Self {
        StepError::Runtime(msg.into())
    }

    /// The message, as `str(exc)`.
    pub fn message(&self) -> &str {
        match self {
            StepError::Value(m)
            | StepError::Target(m)
            | StepError::Permission(m)
            | StepError::Runtime(m) => m,
        }
    }

    /// Is this a `ValueError` (incl. `TargetError`) or a `PermissionError`? Those raised before any
    /// input went out leave the screen as the caller last saw it.
    pub fn is_validation(&self) -> bool {
        !matches!(self, StepError::Runtime(_))
    }
}

impl fmt::Display for StepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for StepError {}

impl From<BackendError> for StepError {
    fn from(e: BackendError) -> Self {
        let msg = e.to_string();
        match e {
            BackendError::WindowGone | BackendError::ElementGone => StepError::Target(msg),
            BackendError::Unsupported(_) => StepError::Value(msg),
            BackendError::Denied(_) => StepError::Permission(msg),
            BackendError::Timeout | BackendError::Os(_) => StepError::Runtime(msg),
        }
    }
}

impl From<ScaleError> for StepError {
    fn from(e: ScaleError) -> Self {
        StepError::Value(e.to_string())
    }
}

impl From<KeyComboError> for StepError {
    fn from(e: KeyComboError) -> Self {
        StepError::Value(e.0)
    }
}

impl From<DestructiveAction> for StepError {
    fn from(e: DestructiveAction) -> Self {
        StepError::Permission(e.message)
    }
}

impl From<AckRefused> for StepError {
    fn from(e: AckRefused) -> Self {
        StepError::Permission(e.0)
    }
}

impl From<HumanActive> for StepError {
    fn from(e: HumanActive) -> Self {
        StepError::Target(e.to_string())
    }
}

impl From<TypeError> for StepError {
    fn from(e: TypeError) -> Self {
        match e {
            TypeError::Refused(d) => d.into(),
            TypeError::Backend(b) => b.into(),
        }
    }
}

impl From<crate::imaging::ImageError> for StepError {
    fn from(e: crate::imaging::ImageError) -> Self {
        StepError::Runtime(e.0)
    }
}

/// The result of a step.
pub type StepResult<T> = Result<T, StepError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_runtime_failure_keeps_the_picture() {
        assert!(StepError::value("x").is_validation());
        assert!(StepError::target("x").is_validation());
        assert!(StepError::permission("x").is_validation());
        assert!(!StepError::runtime("x").is_validation());
    }

    #[test]
    fn backend_errors_map_to_the_python_classes() {
        assert!(matches!(
            StepError::from(BackendError::WindowGone),
            StepError::Target(_)
        ));
        assert!(matches!(
            StepError::from(BackendError::Denied("x".into())),
            StepError::Permission(_)
        ));
        assert!(matches!(
            StepError::from(BackendError::Os("x".into())),
            StepError::Runtime(_)
        ));
        assert!(matches!(
            StepError::from(BackendError::Timeout),
            StepError::Runtime(_)
        ));
        assert!(matches!(
            StepError::from(BackendError::Unsupported("ocr".into())),
            StepError::Value(_)
        ));
    }
}
