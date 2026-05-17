use utoipa::openapi::security::{ApiKey, ApiKeyValue, Http, HttpAuthScheme, SecurityScheme};
use utoipa::Modify;

pub struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.as_mut().unwrap();

        // Standard Bearer JWT — for human users (Claims)
        components.add_security_scheme(
            "bearerAuth",
            SecurityScheme::Http(Http::new(HttpAuthScheme::Bearer)),
        );

        // API key header — for API servers (APIClaims)
        components.add_security_scheme(
            "apiBearerAuth",
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::new("X-API-Authorization"))),
        );

        // ISC key header — for inter-service calls (ISCClaims)
        components.add_security_scheme(
            "apiISCBearerAuth",
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::new("X-ISC-API-Authorization"))),
        );
    }
}