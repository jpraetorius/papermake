//! Internationalization: Fluent catalogs (embedded at compile time) with the
//! request language negotiated from `Accept-Language`, falling back to English.
//!
//! `I18n` is resolved per request (an axum extractor) and threaded into the
//! pure page functions — never a global, so concurrent requests can render
//! different languages safely.

use std::borrow::Cow;
use std::collections::HashMap;

use axum::extract::FromRequestParts;
use axum::http::header::ACCEPT_LANGUAGE;
use axum::http::request::Parts;
use fluent_templates::fluent_bundle::FluentValue;
use fluent_templates::{Loader, static_loader};
use unic_langid::LanguageIdentifier;

static_loader! {
    static LOCALES = {
        locales: "./locales",
        fallback_language: "en",
        // Don't wrap interpolated args in Unicode bidi-isolation marks — keeps
        // rendered HTML (and test assertions) clean for our LTR UI.
        customise: |bundle| bundle.set_use_isolating(false),
    };
}

/// Per-request localization context.
#[derive(Clone)]
pub struct I18n {
    lang: LanguageIdentifier,
}

impl I18n {
    /// Resolve the language from an `Accept-Language` header value.
    /// Unknown/absent → English.
    pub fn from_accept_language(header: Option<&str>) -> Self {
        let lang = header
            .and_then(negotiate)
            .unwrap_or_else(|| "en".parse().expect("valid langid"));
        Self { lang }
    }

    /// BCP-47 language code for `<html lang>` (e.g. "en", "de").
    pub fn code(&self) -> String {
        self.lang.to_string()
    }

    /// Look up a message by id.
    pub fn t(&self, id: &str) -> String {
        LOCALES.lookup(&self.lang, id)
    }

    /// Look up a message that has `{ $name }` arguments. All args are passed as
    /// strings (no plural rules needed here).
    pub fn ta(&self, id: &str, args: &[(&'static str, String)]) -> String {
        let map: HashMap<Cow<'static, str>, FluentValue> = args
            .iter()
            .map(|(k, v)| (Cow::Borrowed(*k), FluentValue::from(v.clone())))
            .collect();
        LOCALES.lookup_with_args(&self.lang, id, &map)
    }
}

/// Pick the first supported language mentioned in an `Accept-Language` header.
fn negotiate(header: &str) -> Option<LanguageIdentifier> {
    for part in header.split(',') {
        let tag = part
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if tag.starts_with("de") {
            return "de".parse().ok();
        }
        if tag.starts_with("en") {
            return "en".parse().ok();
        }
    }
    None
}

impl<S: Send + Sync> FromRequestParts<S> for I18n {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get(ACCEPT_LANGUAGE)
            .and_then(|v| v.to_str().ok());
        Ok(I18n::from_accept_language(header))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_english_fallback() {
        let en = I18n::from_accept_language(None);
        assert_eq!(en.t("nav-dashboard"), "Dashboard");
        assert_eq!(en.t("nav-templates"), "Templates");
    }

    #[test]
    fn test_german_from_header() {
        let de = I18n::from_accept_language(Some("de-DE,de;q=0.9,en;q=0.8"));
        assert_eq!(de.t("nav-dashboard"), "Übersicht");
        assert_eq!(de.t("nav-templates"), "Vorlagen");
    }

    #[test]
    fn test_unsupported_falls_back_to_english() {
        let fr = I18n::from_accept_language(Some("fr-FR,fr;q=0.9"));
        assert_eq!(fr.t("nav-dashboard"), "Dashboard");
    }

    #[test]
    fn test_args() {
        let en = I18n::from_accept_language(None);
        // Isolation is disabled, so interpolation is clean (no bidi marks).
        assert_eq!(
            en.ta("by-author", &[("author", "ada@x.com".to_string())]),
            "by ada@x.com"
        );
        let de = I18n::from_accept_language(Some("de"));
        assert_eq!(
            de.ta("by-author", &[("author", "ada@x.com".to_string())]),
            "von ada@x.com"
        );
    }
}
