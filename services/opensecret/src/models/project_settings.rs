use crate::models::schema::project_settings;
use chrono::{DateTime, Utc};
use diesel::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProjectSettingError {
    #[error("Database error: {0}")]
    DatabaseError(#[from] diesel::result::Error),
    #[error("Invalid settings format: {0}")]
    InvalidSettings(String),
    #[error("Settings serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingCategory {
    Email,
    OAuth,
}

impl SettingCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            SettingCategory::Email => "email",
            SettingCategory::OAuth => "oauth",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailSettings {
    pub provider: String,
    pub send_from: String,
    pub email_verification_url: String,
}

impl Default for EmailSettings {
    fn default() -> Self {
        Self {
            provider: "resend".to_string(),
            send_from: String::new(),
            email_verification_url: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OAuthProviderSettings {
    pub client_id: String,
    pub redirect_url: String,
    /// Missing or null on an update preserves the stored list; an empty list clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_redirect_urls: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppleOAuthSettings {
    pub client_id: String,
    pub redirect_url: String,
    /// Missing or null on an update preserves the stored list; an empty list clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_redirect_urls: Option<Vec<String>>,
    pub team_id: Option<String>, // Apple Developer Team ID (10 chars)
    pub key_id: Option<String>,  // Apple Private Key ID (10 chars)
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OAuthSettings {
    pub google_oauth_enabled: bool,
    pub github_oauth_enabled: bool,
    #[serde(default)]
    pub apple_oauth_enabled: bool,
    pub google_oauth_settings: Option<OAuthProviderSettings>,
    pub github_oauth_settings: Option<OAuthProviderSettings>,
    #[serde(default)]
    pub apple_oauth_settings: Option<AppleOAuthSettings>,
}

impl OAuthSettings {
    /// Preserve only the additive fields older settings clients cannot send.
    /// Other fields retain the existing whole-object replacement semantics.
    pub(crate) fn preserve_omitted_redirect_urls(&mut self, existing: &Self) {
        for (incoming, stored) in [
            (
                &mut self.google_oauth_settings,
                &existing.google_oauth_settings,
            ),
            (
                &mut self.github_oauth_settings,
                &existing.github_oauth_settings,
            ),
        ] {
            if let (Some(incoming), Some(stored)) = (incoming, stored) {
                if incoming.additional_redirect_urls.is_none() {
                    incoming
                        .additional_redirect_urls
                        .clone_from(&stored.additional_redirect_urls);
                }
            }
        }
        if let (Some(incoming), Some(stored)) = (
            &mut self.apple_oauth_settings,
            &existing.apple_oauth_settings,
        ) {
            if incoming.additional_redirect_urls.is_none() {
                incoming
                    .additional_redirect_urls
                    .clone_from(&stored.additional_redirect_urls);
            }
        }
    }
}

#[derive(Queryable, Identifiable)]
#[diesel(table_name = project_settings)]
pub struct ProjectSetting {
    pub id: i32,
    pub project_id: i32,
    pub category: String,
    #[diesel(sql_type = Jsonb)]
    pub settings: Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl ProjectSetting {
    pub fn get_email_settings(&self) -> Result<EmailSettings, ProjectSettingError> {
        serde_json::from_value(self.settings.clone())
            .map_err(ProjectSettingError::SerializationError)
    }

    pub fn get_oauth_settings(&self) -> Result<OAuthSettings, ProjectSettingError> {
        serde_json::from_value(self.settings.clone())
            .map_err(ProjectSettingError::SerializationError)
    }

    pub fn get_by_project_and_category(
        conn: &mut PgConnection,
        lookup_project_id: i32,
        lookup_category: SettingCategory,
    ) -> Result<Option<ProjectSetting>, ProjectSettingError> {
        project_settings::table
            .filter(project_settings::project_id.eq(lookup_project_id))
            .filter(project_settings::category.eq(lookup_category.as_str()))
            .first(conn)
            .optional()
            .map_err(ProjectSettingError::DatabaseError)
    }

    pub fn update(&self, conn: &mut PgConnection) -> Result<(), ProjectSettingError> {
        use crate::models::schema::project_settings::dsl::*;

        diesel::update(project_settings.find(self.id))
            .set((
                category.eq(&self.category),
                settings.eq(&self.settings),
                updated_at.eq(diesel::dsl::now),
            ))
            .execute(conn)
            .map(|_| ())
            .map_err(ProjectSettingError::DatabaseError)
    }
}

#[derive(Insertable)]
#[diesel(table_name = project_settings)]
pub struct NewProjectSetting {
    pub project_id: i32,
    pub category: String,
    #[diesel(sql_type = Jsonb)]
    pub settings: Value,
}

impl NewProjectSetting {
    pub fn new_email_settings(
        project_id: i32,
        email_settings: EmailSettings,
    ) -> Result<Self, ProjectSettingError> {
        Ok(Self {
            project_id,
            category: SettingCategory::Email.as_str().to_string(),
            settings: serde_json::to_value(email_settings)
                .map_err(ProjectSettingError::SerializationError)?,
        })
    }

    pub fn new_oauth_settings(
        project_id: i32,
        oauth_settings: OAuthSettings,
    ) -> Result<Self, ProjectSettingError> {
        Ok(Self {
            project_id,
            category: SettingCategory::OAuth.as_str().to_string(),
            settings: serde_json::to_value(oauth_settings)
                .map_err(ProjectSettingError::SerializationError)?,
        })
    }

    pub fn insert(&self, conn: &mut PgConnection) -> Result<ProjectSetting, ProjectSettingError> {
        diesel::insert_into(project_settings::table)
            .values(self)
            .get_result(conn)
            .map_err(ProjectSettingError::DatabaseError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn legacy_settings() -> Value {
        json!({
            "google_oauth_enabled": true,
            "github_oauth_enabled": true,
            "apple_oauth_enabled": true,
            "google_oauth_settings": {
                "client_id": "google-client",
                "redirect_url": "https://customer.example/google"
            },
            "github_oauth_settings": {
                "client_id": "github-client",
                "redirect_url": "http://127.0.0.1:5173/github"
            },
            "apple_oauth_settings": {
                "client_id": "apple-client",
                "redirect_url": "https://customer.example/apple",
                "team_id": "ABCDEFGHIJ",
                "key_id": "1234567890"
            }
        })
    }

    #[test]
    fn legacy_oauth_settings_round_trip_without_additional_fields() {
        let legacy = legacy_settings();
        let settings: OAuthSettings = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(serde_json::to_value(settings).unwrap(), legacy);
    }

    #[test]
    fn omitted_and_null_lists_preserve_only_existing_provider_additions() {
        let mut current_json = legacy_settings();
        for provider in ["google", "github", "apple"] {
            current_json[format!("{provider}_oauth_settings")]["additional_redirect_urls"] =
                json!([format!("https://auth.customer.example/{provider}")]);
        }
        let current: OAuthSettings = serde_json::from_value(current_json).unwrap();
        for null in [false, true] {
            let mut update_json = legacy_settings();
            if null {
                for provider in ["google", "github", "apple"] {
                    update_json[format!("{provider}_oauth_settings")]["additional_redirect_urls"] =
                        Value::Null;
                }
            }
            update_json["google_oauth_settings"]["redirect_url"] =
                json!("https://new.customer.example/google");
            update_json["github_oauth_enabled"] = json!(false);
            update_json["github_oauth_settings"] = Value::Null;
            let mut update: OAuthSettings = serde_json::from_value(update_json).unwrap();
            update.preserve_omitted_redirect_urls(&current);

            let google = update.google_oauth_settings.unwrap();
            assert_eq!(google.redirect_url, "https://new.customer.example/google");
            assert_eq!(
                google.additional_redirect_urls,
                current
                    .google_oauth_settings
                    .as_ref()
                    .unwrap()
                    .additional_redirect_urls
            );
            assert_eq!(
                update
                    .apple_oauth_settings
                    .unwrap()
                    .additional_redirect_urls,
                current
                    .apple_oauth_settings
                    .as_ref()
                    .unwrap()
                    .additional_redirect_urls
            );
            assert!(!update.github_oauth_enabled);
            assert!(update.github_oauth_settings.is_none());
        }
    }

    #[test]
    fn explicit_lists_replace_and_empty_lists_clear() {
        let mut current_json = legacy_settings();
        for provider in ["google", "github", "apple"] {
            current_json[format!("{provider}_oauth_settings")]["additional_redirect_urls"] =
                json!(["https://old.customer.example/callback"]);
        }
        let current: OAuthSettings = serde_json::from_value(current_json).unwrap();
        for replacement in [json!([]), json!(["https://new.customer.example/callback"])] {
            let mut update_json = legacy_settings();
            for provider in ["google", "github", "apple"] {
                update_json[format!("{provider}_oauth_settings")]["additional_redirect_urls"] =
                    replacement.clone();
            }
            let mut update: OAuthSettings = serde_json::from_value(update_json.clone()).unwrap();
            update.preserve_omitted_redirect_urls(&current);
            assert_eq!(serde_json::to_value(update).unwrap(), update_json);
        }
    }
}
