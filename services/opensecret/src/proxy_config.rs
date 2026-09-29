#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub provider_name: String,
}

#[derive(Debug, Clone)]
pub struct ProxyRouter {
    default_proxy: ProxyConfig,
    tinfoil_proxy: ProxyConfig,
}

impl ProxyRouter {
    pub fn new(openai_base: String, openai_key: Option<String>, tinfoil_base: String) -> Self {
        assert!(
            !tinfoil_base.trim().is_empty(),
            "Tinfoil API base must be configured"
        );

        let default_proxy = ProxyConfig {
            base_url: openai_base.clone(),
            api_key: if openai_base.contains("api.openai.com") {
                openai_key
            } else {
                None
            },
            provider_name: if openai_base.contains("api.openai.com") {
                "openai".to_string()
            } else {
                "continuum".to_string()
            },
        };

        let tinfoil_proxy = ProxyConfig {
            base_url: tinfoil_base,
            api_key: None,
            provider_name: "tinfoil".to_string(),
        };

        Self {
            default_proxy,
            tinfoil_proxy,
        }
    }

    pub fn get_default_proxy(&self) -> ProxyConfig {
        self.default_proxy.clone()
    }

    pub fn get_completion_proxy(&self) -> ProxyConfig {
        self.tinfoil_proxy.clone()
    }

    pub fn get_tinfoil_proxy(&self) -> ProxyConfig {
        self.tinfoil_proxy.clone()
    }

    #[cfg(test)]
    pub fn get_tinfoil_base_url(&self) -> String {
        self.tinfoil_proxy.base_url.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proxy_router_uses_openai_api_key_only_for_openai() {
        let router = ProxyRouter::new(
            "https://api.openai.com".to_string(),
            Some("test-key".to_string()),
            "http://tinfoil.example.com".to_string(),
        );

        assert_eq!(router.get_default_proxy().provider_name, "openai");
        assert_eq!(
            router.get_default_proxy().api_key,
            Some("test-key".to_string())
        );

        let continuum_router = ProxyRouter::new(
            "http://continuum.example.com".to_string(),
            Some("ignored".to_string()),
            "http://tinfoil.example.com".to_string(),
        );

        assert_eq!(
            continuum_router.get_default_proxy().provider_name,
            "continuum"
        );
        assert_eq!(continuum_router.get_default_proxy().api_key, None);
    }

    #[test]
    fn test_completion_proxy_prefers_tinfoil_when_configured() {
        let router = ProxyRouter::new(
            "http://continuum.example.com".to_string(),
            None,
            "http://tinfoil.example.com".to_string(),
        );

        assert_eq!(router.get_completion_proxy().provider_name, "tinfoil");
        assert_eq!(
            router.get_tinfoil_base_url(),
            "http://tinfoil.example.com".to_string()
        );
    }

    #[test]
    #[should_panic(expected = "Tinfoil API base must be configured")]
    fn test_proxy_router_requires_tinfoil() {
        let _router = ProxyRouter::new(
            "http://continuum.example.com".to_string(),
            None,
            String::new(),
        );
    }

    #[test]
    fn test_completion_proxy_uses_tinfoil_even_with_continuum_default() {
        let router = ProxyRouter::new(
            "http://continuum.example.com".to_string(),
            None,
            "http://tinfoil.example.com".to_string(),
        );

        assert_eq!(router.get_completion_proxy().provider_name, "tinfoil");
        assert_eq!(router.get_tinfoil_proxy().provider_name, "tinfoil");
    }
}
