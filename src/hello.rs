//! Windows Hello (face, fingerprint or PIN) before showing a saved password, as Chrome does.
//!
//! `verify` blocks until the user answers, so call it off the UI thread.

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Outcome {
    Verified,
    /// The user cancelled or failed the check.
    Refused,
    /// Windows Hello isn't set up on this PC (or is disabled by policy).
    Unavailable,
}

/// Asks the user to confirm it's them, with the prompt attached to `window` (an HWND).
#[cfg(windows)]
pub fn verify(window: isize, message: &str) -> Outcome {
    use windows::{
        Security::Credentials::UI::{UserConsentVerificationResult, UserConsentVerifier, UserConsentVerifierAvailability},
        Win32::{
            Foundation::HWND,
            System::WinRT::{IUserConsentVerifierInterop, RO_INIT_MULTITHREADED, RoInitialize},
        },
        core::{HSTRING, factory},
    };
    use windows_future::IAsyncOperation;

    // SAFETY: initialises WinRT for this (worker) thread; harmless if already done.
    let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
    match UserConsentVerifier::CheckAvailabilityAsync().and_then(|op| op.join()) {
        Ok(UserConsentVerifierAvailability::Available) => {}
        _ => return Outcome::Unavailable,
    }
    let Ok(interop) = factory::<UserConsentVerifier, IUserConsentVerifierInterop>() else { return Outcome::Unavailable };
    // SAFETY: `window` is Bare's top-level window, alive while the prompt shows.
    let request: windows::core::Result<IAsyncOperation<UserConsentVerificationResult>> =
        unsafe { interop.RequestVerificationForWindowAsync(HWND(window as _), &HSTRING::from(message)) };
    match request.and_then(|op| op.join()) {
        Ok(UserConsentVerificationResult::Verified) => Outcome::Verified,
        Ok(UserConsentVerificationResult::DeviceNotPresent | UserConsentVerificationResult::NotConfiguredForUser | UserConsentVerificationResult::DisabledByPolicy) => {
            Outcome::Unavailable
        }
        _ => Outcome::Refused,
    }
}

#[cfg(not(windows))]
pub fn verify(_window: isize, _message: &str) -> Outcome {
    Outcome::Unavailable
}
