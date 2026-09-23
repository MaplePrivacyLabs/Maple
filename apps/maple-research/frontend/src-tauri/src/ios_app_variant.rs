pub fn custom_url_scheme(
    ios_variant: Option<&str>,
    frontend_variant: Option<&str>,
) -> Result<&'static str, &'static str> {
    match (ios_variant, frontend_variant) {
        (None | Some("production"), None | Some("production")) => Ok("cloud.opensecret.maple"),
        (Some("dev"), Some("dev")) => Ok("cloud.opensecret.maple.dev"),
        _ => Err("iOS and frontend Maple app variants must agree"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variants_must_match_and_production_stays_default() {
        assert_eq!(custom_url_scheme(None, None), Ok("cloud.opensecret.maple"));
        assert_eq!(
            custom_url_scheme(Some("production"), Some("production")),
            Ok("cloud.opensecret.maple")
        );
        assert_eq!(
            custom_url_scheme(Some("dev"), Some("dev")),
            Ok("cloud.opensecret.maple.dev")
        );
        for pair in [
            (Some("dev"), None),
            (None, Some("dev")),
            (Some("dev"), Some("production")),
            (Some("production"), Some("dev")),
            (Some("staging"), Some("staging")),
        ] {
            assert!(custom_url_scheme(pair.0, pair.1).is_err());
        }
    }
}
