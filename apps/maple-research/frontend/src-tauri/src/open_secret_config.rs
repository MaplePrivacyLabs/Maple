use opensecret::Pcr0Environment;

pub(crate) fn parse_pcr0_environment(value: Option<&str>) -> Result<Pcr0Environment, String> {
    match value {
        None | Some("production") => Ok(Pcr0Environment::Production),
        Some("development") => Ok(Pcr0Environment::Development),
        Some(_) => Err(
            "VITE_OPEN_SECRET_PCR_ENVIRONMENT must be either production or development".to_string(),
        ),
    }
}

pub(crate) fn configured_pcr0_environment() -> Result<Pcr0Environment, String> {
    parse_pcr0_environment(option_env!("VITE_OPEN_SECRET_PCR_ENVIRONMENT"))
}

pub(crate) fn normalize_api_url(api_url: &str) -> Result<String, String> {
    let mut url = reqwest::Url::parse(api_url.trim())
        .map_err(|_| "OpenSecret API URL is invalid".to_string())?;
    let host = url
        .host_str()
        .ok_or_else(|| "OpenSecret API URL must include a host".to_string())?;
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Err(
            "OpenSecret API URL must use HTTPS or a loopback development address".to_string(),
        );
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("OpenSecret API URL must not contain credentials".to_string());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("OpenSecret API URL must not contain a query or fragment".to_string());
    }
    if url.path() != "/" && !url.path().is_empty() {
        return Err("OpenSecret API URL must not contain a path".to_string());
    }
    url.set_path("");
    Ok(url.as_str().trim_end_matches('/').to_string())
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

    #[test]
    fn api_url_requires_https_or_exact_loopback_http() {
        assert_eq!(
            normalize_api_url("https://api.example.test/").unwrap(),
            "https://api.example.test"
        );
        assert_eq!(
            normalize_api_url("http://127.0.0.1:3000").unwrap(),
            "http://127.0.0.1:3000"
        );
        for invalid in [
            "http://api.example.test",
            "https://user@example.test",
            "https://api.example.test/path",
            "https://api.example.test?query=1",
        ] {
            assert!(normalize_api_url(invalid).is_err(), "{invalid}");
        }
    }
}
