use ginger_shared_rs::rocket_utils::{APIClaims, Claims};
use ginger_shared_rs::ISCClaims;
use jsonwebtoken::{decode, DecodingKey, Validation};
use warp::Filter;

use crate::error::{InvalidTokenError, JWTError};


// ── Internal JWT decode helper ────────────────────────────────────────────────

fn jwt_secret() -> String {
    std::env::var("JWT_SECRET").unwrap_or_else(|_| "1234".to_string())
}

// ── Bearer (Authorization header) — human users ───────────────────────────────

pub fn with_auth() -> impl Filter<Extract = (Claims,), Error = warp::Rejection> + Clone {
    warp::header::optional::<String>("Authorization").and_then(
        |auth_header: Option<String>| async move {
            let token = extract_bearer(auth_header)
                .ok_or_else(|| warp::reject::custom(JWTError))?;
            authenticate_token(&token)
                .map_err(|_| warp::reject::custom(JWTError))
        },
    )
}

fn authenticate_token(token: &str) -> Result<Claims, ()> {
    let key = DecodingKey::from_secret(jwt_secret().as_ref());
    let validation = Validation::new(jsonwebtoken::Algorithm::HS256);
    decode::<Claims>(token, &key, &validation)
        .map(|d| d.claims)
        .map_err(|_| ())
}

// ── X-API-Authorization header — API servers ──────────────────────────────────

pub fn with_api_auth() -> impl Filter<Extract = (APIClaims,), Error = warp::Rejection> + Clone {
    warp::header::optional::<String>("X-API-Authorization").and_then(
        |auth_header: Option<String>| async move {
            let token = extract_bearer(auth_header)
                .ok_or_else(|| warp::reject::custom(JWTError))?;
            authenticate_api_token(&token)
                .map_err(|_| warp::reject::custom(JWTError))
        },
    )
}

fn authenticate_api_token(token: &str) -> Result<APIClaims, ()> {
    let key = DecodingKey::from_secret(jwt_secret().as_ref());
    let validation = Validation::new(jsonwebtoken::Algorithm::HS256);
    decode::<APIClaims>(token, &key, &validation)
        .map(|d| d.claims)
        .map_err(|_| ())
}

// ── X-ISC-API-Authorization header — inter-service calls ─────────────────────

pub fn with_isc_auth() -> impl Filter<Extract = (ISCClaims,), Error = warp::Rejection> + Clone {
    warp::header::optional::<String>("X-ISC-API-Authorization").and_then(
        |auth_header: Option<String>| async move {
            let token = extract_bearer(auth_header)
                .ok_or_else(|| warp::reject::custom(JWTError))?;
            authenticate_isc_token(&token)
                .map_err(|_| warp::reject::custom(JWTError))
        },
    )
}

fn authenticate_isc_token(token: &str) -> Result<ISCClaims, ()> {
    let key = DecodingKey::from_secret(jwt_secret().as_ref());
    let validation = Validation::new(jsonwebtoken::Algorithm::HS256);
    decode::<ISCClaims>(token, &key, &validation)
        .map(|d| d.claims)
        .map_err(|_| ())
}


/// Strip "Bearer " prefix if present, return None if header is absent or empty.
fn extract_bearer(header: Option<String>) -> Option<String> {
    let h = header?;
    let token = h.trim_start_matches("Bearer ").trim().to_string();
    if token.is_empty() { None } else { Some(token) }
}