//! Aggregators Venera knows out of the box.
//!
//! Every entry is OpenAI-compatible: the chat path and the key are the only things the
//! request layer needs. Catalogs are fetched from a public endpoint, so browsing models
//! does not require a key; the key is only needed for completions.

/// How a provider publishes its model catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogSource {
    /// Server-rendered `proxyapi.ru/models/browse` cards.
    ProxyapiBrowse,
    /// Public JSON map at `api.aitunnel.ru/public/aitunnel/models/chat`.
    AitunnelPublic,
}

/// How a provider spells the reasoning hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningStyle {
    /// OpenAI-compatible `reasoning_effort: "medium"`.
    EffortField,
    /// AITUNNEL's `reasoning: { "effort": "medium" }`.
    EffortObject,
}

/// One supported aggregator.
#[derive(Debug, Clone, Copy)]
pub struct Provider {
    /// Stable id used in configs, sessions, and the keyring.
    pub id: &'static str,
    /// Human-readable name.
    pub title: &'static str,
    /// Public site.
    pub site: &'static str,
    /// API root without a trailing slash.
    pub base_url: &'static str,
    /// Path appended for chat completions.
    pub chat_path: &'static str,
    /// Public model catalog URL.
    pub models_url: &'static str,
    /// Environment variable checked before the stored key.
    pub key_env: &'static str,
    /// Where models come from.
    pub catalog: CatalogSource,
    /// How the reasoning effort is serialized.
    pub reasoning: ReasoningStyle,
}

/// Aggregators the binary supports.
pub const PROVIDERS: &[Provider] = &[
    Provider {
        id: "proxyapi",
        title: "ProxyAPI",
        site: "https://proxyapi.ru",
        base_url: "https://api.proxyapi.ru/v1",
        chat_path: "/chat/completions",
        models_url: "https://proxyapi.ru/models/browse",
        key_env: "PROXYAPI_KEY",
        catalog: CatalogSource::ProxyapiBrowse,
        reasoning: ReasoningStyle::EffortField,
    },
    Provider {
        id: "aitunnel",
        title: "AITUNNEL",
        site: "https://aitunnel.ru",
        base_url: "https://api.aitunnel.ru/v1",
        chat_path: "/chat/completions",
        models_url: "https://api.aitunnel.ru/public/aitunnel/models/chat",
        key_env: "AITUNNEL_API_KEY",
        catalog: CatalogSource::AitunnelPublic,
        reasoning: ReasoningStyle::EffortObject,
    },
];

/// Looks a provider up by id.
pub fn get(id: &str) -> Option<&'static Provider> {
    PROVIDERS.iter().find(|provider| provider.id == id)
}

/// The provider used when neither the session nor the config says otherwise.
pub fn default_provider() -> &'static Provider {
    &PROVIDERS[0]
}

/// `provider/model` for messages.
pub fn label(id: &str) -> &str {
    get(id).map_or(id, |provider| provider.title)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_provider_has_a_unique_id_and_an_https_root() {
        let mut ids = std::collections::BTreeSet::new();
        for provider in PROVIDERS {
            assert!(ids.insert(provider.id), "duplicate id {}", provider.id);
            assert!(provider.base_url.starts_with("https://"));
            assert!(provider.chat_path.starts_with('/'));
            assert!(provider.models_url.starts_with("https://"));
            assert!(!provider.key_env.is_empty());
        }
    }

    #[test]
    fn lookup_is_exact() {
        assert_eq!(
            get("aitunnel").map(|provider| provider.title),
            Some("AITUNNEL")
        );
        assert!(get("AITUNNEL").is_none());
    }
}
