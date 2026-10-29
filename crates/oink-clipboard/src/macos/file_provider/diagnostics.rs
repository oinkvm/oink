//! Error descriptions are not safe diagnostics: Cocoa/IO/JSON messages can
//! contain file names, paths or bridge capabilities. Retain codes, not userInfo.
use objc2::Message;
use std::io;

#[derive(Clone, Debug)]
pub(super) struct Failure(pub(super) String);

impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Self(format!(
            "io kind={:?} errno={:?} content_reason={:?}",
            error.kind(),
            error.raw_os_error(),
            crate::content::reason(&error)
        ))
    }
}
impl From<serde_json::Error> for Failure {
    fn from(error: serde_json::Error) -> Self {
        Self(format!(
            "json category={:?} line={} column={}",
            error.classify(),
            error.line(),
            error.column()
        ))
    }
}
impl From<std::string::FromUtf8Error> for Failure {
    fn from(error: std::string::FromUtf8Error) -> Self {
        Self(format!("invalid_utf8 valid_up_to={}", error.utf8_error().valid_up_to()))
    }
}
impl From<&'static str> for Failure {
    fn from(cause: &'static str) -> Self {
        Self(cause.to_owned())
    }
}

pub(super) fn native_domain(domain: &str) -> &'static str {
    match domain {
        "NSCocoaErrorDomain" => "NSCocoaErrorDomain",
        "NSPOSIXErrorDomain" => "NSPOSIXErrorDomain",
        "NSOSStatusErrorDomain" => "NSOSStatusErrorDomain",
        "NSMachErrorDomain" => "NSMachErrorDomain",
        "NSFileProviderErrorDomain" => "NSFileProviderErrorDomain",
        "FPErrorDomain" => "FPErrorDomain",
        "OinkFileProvider" => "OinkFileProvider",
        _ => "other",
    }
}

/// Read NSError's documented underlying-error chain, with a depth bound for
/// malformed/cyclic userInfo. No localized description or arbitrary domain text.
pub(super) fn native_error(error: &objc2_foundation::NSError) -> String {
    use objc2_foundation::{NSError, NSUnderlyingErrorKey};
    let mut chain = Vec::new();
    let mut current = Some(error.retain());
    while let Some(error) = current {
        chain.push(format!(
            "{} code={}",
            native_domain(&error.domain().to_string()),
            error.code()
        ));
        if chain.len() == 8 {
            break;
        }
        // SAFETY: the framework constant is immutable. The downcast checks the
        // userInfo value's actual Objective-C class before reading it as NSError.
        current = error
            .userInfo()
            .objectForKey(unsafe { NSUnderlyingErrorKey })
            .and_then(|value| value.downcast::<NSError>().ok());
    }
    chain.join(" <- ")
}

/// The manager writes this one code-only record before exiting. Ignore all
/// other stderr (including platform crash output); validate domains again here.
pub(super) fn helper_error(stderr: &[u8]) -> String {
    #[derive(serde::Deserialize)]
    struct NativeError {
        domain: String,
        code: i64,
    }
    #[derive(serde::Deserialize)]
    struct Diagnostic {
        errors: Vec<NativeError>,
    }
    for line in stderr.split(|byte| *byte == b'\n') {
        let Some(json) = line.strip_prefix(b"OINK_FP_ERROR ") else {
            continue;
        };
        if let Ok(error) = serde_json::from_slice::<Diagnostic>(json) {
            let chain: Vec<_> = error
                .errors
                .iter()
                .take(8)
                .map(|e| format!("{} code={}", native_domain(&e.domain), e.code))
                .collect();
            if !chain.is_empty() {
                return chain.join(" <- ");
            }
        }
    }
    "no_structured_error".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_foundation::{NSDictionary, NSError, NSLocalizedDescriptionKey, NSString, NSUnderlyingErrorKey};
    #[test]
    fn native_errors_keep_underlying_codes_and_drop_private_text() {
        // SAFETY: standard NSError initialization with a valid constant domain and no userInfo.
        let inner =
            unsafe { NSError::errorWithDomain_code_userInfo(&NSString::from_str("NSPOSIXErrorDomain"), 13, None) };
        // SAFETY: both framework-provided keys are immutable NSString constants.
        let keys = unsafe { [NSLocalizedDescriptionKey, NSUnderlyingErrorKey] };
        let info = NSDictionary::from_slices(
            &keys,
            &[
                &*NSString::from_str("/private/secret-name token=secret") as &objc2::runtime::AnyObject,
                &*inner,
            ],
        );
        // SAFETY: valid NSError initialization; dictionary values have their documented types.
        let error = unsafe {
            NSError::errorWithDomain_code_userInfo(&NSString::from_str("NSCocoaErrorDomain"), 4097, Some(&info))
        };
        assert_eq!(
            native_error(&error),
            "NSCocoaErrorDomain code=4097 <- NSPOSIXErrorDomain code=13"
        );
        // SAFETY: NSError permits arbitrary string domains; no userInfo is supplied.
        let unknown =
            unsafe { NSError::errorWithDomain_code_userInfo(&NSString::from_str("/private/secret-domain"), 7, None) };
        assert_eq!(native_error(&unknown), "other code=7");
    }
    #[test]
    fn helper_output_only_accepts_structured_codes() {
        assert_eq!(helper_error(br#"OINK_FP_ERROR {"errors":[{"domain":"NSCocoaErrorDomain","code":4099},{"domain":"/private/name","code":13}],"description":"token=secret"}"#), "NSCocoaErrorDomain code=4099 <- other code=13");
        assert_eq!(
            helper_error(b"File Provider /private/name token=secret"),
            "no_structured_error"
        );
        let error = io::Error::new(io::ErrorKind::PermissionDenied, "/private/name token=secret");
        let safe = Failure::from(error).0;
        assert!(safe.contains("PermissionDenied"));
        assert!(!safe.contains("private") && !safe.contains("secret"));
    }
}
