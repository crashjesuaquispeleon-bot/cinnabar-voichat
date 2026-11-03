//! Best-effort classification of local microphone permission outcomes.
//!
//! Desktop OSes do not expose one portable pre-flight API that is safe to call from the
//! Bevy frame schedule. Stage 1 therefore treats the first real open attempt as the probe:
//! success means the process can capture; errors that look like access denial mean Denied.
//! macOS still requires `NSMicrophoneUsageDescription` in the bundle (already added).

use super::voice_stream::VoiceCaptureError;
use super::voice_input::MicrophonePermission;

/// Classifies an open failure for policy updates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureOpenOutcome {
    /// Stream is running; OS allowed capture.
    Granted,
    /// OS or host refused access; do not spin-reopen until the user changes settings.
    Denied,
    /// Device missing, unsupported format, or transient failure — keep prior permission.
    Transient,
}

/// Maps a successful open to [`CaptureOpenOutcome::Granted`].
#[must_use]
pub fn outcome_from_success() -> CaptureOpenOutcome {
    CaptureOpenOutcome::Granted
}

/// Maps a [`VoiceCaptureError`] to a permission/open outcome.
#[must_use]
pub fn outcome_from_error(error: &VoiceCaptureError) -> CaptureOpenOutcome {
    match error {
        VoiceCaptureError::NotPermitted => CaptureOpenOutcome::Denied,
        VoiceCaptureError::NoDevice | VoiceCaptureError::UnsupportedFormat => {
            CaptureOpenOutcome::Transient
        }
        VoiceCaptureError::Other(message) => classify_message(message),
    }
}

fn classify_message(message: &str) -> CaptureOpenOutcome {
    let lower = message.to_ascii_lowercase();
    // Heuristics across WASAPI / CoreAudio / ALSA error text. Prefer Denied only when the
    // message clearly indicates access control; otherwise treat as transient.
    const DENY_MARKERS: &[&str] = &[
        "permission",
        "not authorized",
        "notauthorized",
        "access denied",
        "accessdenied",
        "denied",
        "not allowed",
        "privacy",
        "avfoundation",
        "errsec",
    ];
    if DENY_MARKERS.iter().any(|marker| lower.contains(marker)) {
        CaptureOpenOutcome::Denied
    } else {
        CaptureOpenOutcome::Transient
    }
}

/// Applies an open outcome to the stored OS permission.
#[must_use]
pub fn permission_after_outcome(
    previous: MicrophonePermission,
    outcome: CaptureOpenOutcome,
) -> MicrophonePermission {
    match outcome {
        CaptureOpenOutcome::Granted => MicrophonePermission::Granted,
        CaptureOpenOutcome::Denied => MicrophonePermission::Denied,
        CaptureOpenOutcome::Transient => previous,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_denied_message_marks_denied() {
        let error = VoiceCaptureError::Other("Access Denied by privacy settings".into());
        assert_eq!(outcome_from_error(&error), CaptureOpenOutcome::Denied);
        assert_eq!(
            permission_after_outcome(MicrophonePermission::Unknown, CaptureOpenOutcome::Denied),
            MicrophonePermission::Denied
        );
    }

    #[test]
    fn missing_device_stays_transient() {
        assert_eq!(
            outcome_from_error(&VoiceCaptureError::NoDevice),
            CaptureOpenOutcome::Transient
        );
        assert_eq!(
            permission_after_outcome(
                MicrophonePermission::Unknown,
                CaptureOpenOutcome::Transient
            ),
            MicrophonePermission::Unknown
        );
    }

    #[test]
    fn success_grants() {
        assert_eq!(
            permission_after_outcome(
                MicrophonePermission::Unknown,
                outcome_from_success()
            ),
            MicrophonePermission::Granted
        );
    }
}
