use maple_proxy::Pcr0Environment;

pub(crate) fn parse_pcr0_environment(value: Option<&str>) -> Result<Pcr0Environment, String> {
    match value {
        None | Some("production") => Ok(Pcr0Environment::Production),
        Some("development") => Ok(Pcr0Environment::Development),
        Some(_) => Err(
            "VITE_OPEN_SECRET_PCR_ENVIRONMENT must be either production or development".to_string(),
        ),
    }
}

pub fn configured_pcr0_environment() -> Result<Pcr0Environment, String> {
    parse_pcr0_environment(option_env!("VITE_OPEN_SECRET_PCR_ENVIRONMENT"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcr_environment_defaults_to_production_and_requires_an_exact_override() {
        assert_eq!(
            parse_pcr0_environment(None).unwrap(),
            Pcr0Environment::Production
        );
        assert_eq!(
            parse_pcr0_environment(Some("production")).unwrap(),
            Pcr0Environment::Production
        );
        assert_eq!(
            parse_pcr0_environment(Some("development")).unwrap(),
            Pcr0Environment::Development
        );
        for invalid in ["", "dev", "prod", "Development", "staging"] {
            assert!(parse_pcr0_environment(Some(invalid)).is_err());
        }
    }
}
