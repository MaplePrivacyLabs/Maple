//! Anonymous feedback for Maple Work only. No account or conversation data.
use std::time::Duration;

use serde::Serialize;

pub const ENDPOINT: &str = "https://maple-feedback.mutiny-waitlist.workers.dev/v1/feedback";
pub const MAX_MESSAGE: usize = 5000;

#[derive(Clone, Serialize, PartialEq, Debug)]
pub struct Submission {
    pub id: String,
    pub category: String,
    pub message: String,
    pub app_version: &'static str,
    pub platform: &'static str,
    pub build_profile: &'static str,
}

impl Submission {
    pub fn new(category: &str, message: &str) -> Result<Self, String> {
        if !["bug", "idea", "other"].contains(&category) {
            return Err("Choose a feedback category.".into());
        }
        let message = message.trim();
        if message.is_empty() || message.encode_utf16().count() > MAX_MESSAGE {
            return Err("Please enter between 1 and 5,000 characters.".into());
        }
        Ok(Self {
            id: uuid::Uuid::new_v4().to_string(),
            category: category.into(),
            message: message.into(),
            app_version: env!("CARGO_PKG_VERSION"),
            platform: std::env::consts::OS,
            build_profile: crate::profile::PROFILE,
        })
    }
}

pub async fn submit(submission: &Submission) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "Could not prepare feedback. Please try again.".to_string())?;
    let response = client
        .post(ENDPOINT)
        .json(submission)
        .send()
        .await
        .map_err(|_| {
            "Could not send feedback. Your draft is here; please try again.".to_string()
        })?;
    match response.status().as_u16() {
        201 => Ok(()),
        429 => Err("Too many submissions. Please wait a minute and try again.".into()),
        _ => Err("Could not send feedback. Your draft is here; please try again.".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submission_is_bounded_and_contains_no_identity() {
        assert!(Submission::new("other", "  ").is_err());
        assert!(Submission::new("other", &"x".repeat(5001)).is_err());
        assert!(Submission::new("other", &"😀".repeat(2501)).is_err());
        assert!(Submission::new("unknown", "hello").is_err());
        let value = serde_json::to_value(Submission::new("idea", " hello ").unwrap()).unwrap();
        let object = value.as_object().unwrap();
        assert_eq!(object.len(), 6);
        assert_eq!(value["message"], "hello");
        assert!(uuid::Uuid::parse_str(value["id"].as_str().unwrap()).is_ok());
    }
}
