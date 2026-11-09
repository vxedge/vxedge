//! Typed refusals. Every refusal carries `google.rpc.ErrorInfo`, whose `reason` is an
//! `ErrorReason` value name and whose `domain` is `vxedge.v1`, so a client branches on the reason
//! and never on the message, which stays for people.

use std::collections::HashMap;
use tonic::{Code, Status};
use tonic_types::{ErrorDetails, StatusExt};

pub use vxedge_api::v1::ErrorReason as R;

pub const DOMAIN: &str = "vxedge.v1";

/// A refusal with its reason attached.
pub fn refuse(code: Code, reason: R, message: impl Into<String>) -> Status {
    let details = ErrorDetails::with_error_info(
        reason.as_str_name(),
        DOMAIN,
        HashMap::<String, String>::new(),
    );
    Status::with_error_details(code, message, details)
}

/// A refusal with its reason and typed `ErrorInfo.metadata` (`active_campaign`,
/// `retry_after_ms`, ...), so a client reads the detail without parsing the message.
pub fn refuse_with(
    code: Code,
    reason: R,
    message: impl Into<String>,
    metadata: &[(&str, String)],
) -> Status {
    let metadata: HashMap<String, String> = metadata
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    let details = ErrorDetails::with_error_info(reason.as_str_name(), DOMAIN, metadata);
    Status::with_error_details(code, message, details)
}

pub fn precondition(reason: R, message: impl Into<String>) -> Status {
    refuse(Code::FailedPrecondition, reason, message)
}

pub fn invalid(reason: R, message: impl Into<String>) -> Status {
    refuse(Code::InvalidArgument, reason, message)
}

pub fn not_found(reason: R, message: impl Into<String>) -> Status {
    refuse(Code::NotFound, reason, message)
}

pub fn unavailable(reason: R, message: impl Into<String>) -> Status {
    refuse(Code::Unavailable, reason, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_carries_its_reason_and_domain() {
        let status = precondition(R::WrongWorkspace, "not this one");
        let info = status.get_error_details().error_info().cloned().unwrap();
        assert_eq!(info.reason, "ERROR_REASON_WRONG_WORKSPACE");
        assert_eq!(info.domain, DOMAIN);
        assert_eq!(status.code(), Code::FailedPrecondition);
        assert_eq!(status.message(), "not this one");
    }

    /// Every refusal goes through this module, so none can reach a client without a reason. A
    /// bare `Status::failed_precondition` or similar outside it, and outside tests, fails here.
    #[test]
    fn no_refusal_bypasses_the_reasons() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut bare = Vec::new();
        for entry in std::fs::read_dir(&src).unwrap() {
            let path = entry.unwrap().path();
            if path.file_name().unwrap() == "errors.rs" {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let code = text.split("#[cfg(test)]").next().unwrap();
            for (n, line) in code.lines().enumerate() {
                for bad in [
                    "Status::failed_precondition",
                    "Status::invalid_argument",
                    "Status::not_found",
                    "Status::unavailable",
                    "Status::aborted",
                ] {
                    if line.contains(bad) {
                        bare.push(format!("{}:{}: {}", path.display(), n + 1, line.trim()));
                    }
                }
            }
        }
        assert!(
            bare.is_empty(),
            "refusals with no reason:\n{}",
            bare.join("\n")
        );
    }
}
