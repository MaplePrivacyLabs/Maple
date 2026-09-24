//! Log-safe summaries of errors whose `Display`/`Debug` output can quote the
//! input they failed on. Logs leave the enclave, so parse failures on
//! decrypted or credential-bearing data must never echo that data.

use std::fmt;

/// Location and category of a `serde_json` failure, without the message.
///
/// A `serde_json::Error` message quotes the offending value for type and
/// value mismatches (for example `invalid type: string "..."`), so it must not
/// be logged when the input was decrypted user data or a token response.
pub(crate) struct JsonErrorSummary<'a>(pub(crate) &'a serde_json::Error);

impl fmt::Display for JsonErrorSummary<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?} error at line {} column {}",
            self.0.classify(),
            self.0.line(),
            self.0.column()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_error_summary_omits_the_offending_value() {
        #[derive(Debug, serde::Deserialize)]
        #[allow(dead_code)]
        struct Typed {
            count: u32,
        }

        let error = serde_json::from_str::<Typed>(r#"{"count":"user secret text"}"#)
            .expect_err("type mismatch");
        assert!(error.to_string().contains("user secret text"));

        let summary = JsonErrorSummary(&error).to_string();
        assert_eq!(summary, "Data error at line 1 column 27");
        assert!(!summary.contains("user secret text"));
    }
}
