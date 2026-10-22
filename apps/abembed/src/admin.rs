use crate::{
    accounts::{account_fields, AccountStore, Credential},
    sources::{bad_request, internal, json_response, HttpError, REGIONS},
};
use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::Response,
    routing::post,
    Router,
};
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use subtle::ConstantTimeEq;
use worker::Env;

const ADMIN_TOKEN_SECRET: &str = "ADMIN_TOKEN";
const ADMIN_LIMITER: &str = "ADMIN_LIMITER";

pub fn get_router() -> Router<Env> {
    Router::new().route("/admin/accounts", post(upsert_account))
}

#[derive(Deserialize)]
struct AccountRequest {
    platform: String,
    region: String,
    label: Option<String>,
    cookie: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

#[worker::send]
async fn upsert_account(
    State(env): State<Env>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, HttpError> {
    authorize(&env, &headers).await?;
    let request: AccountRequest =
        serde_json::from_str(&body).map_err(|_| bad_request("Invalid account request"))?;
    let (account_id, credential) = credential_from_request(&request)?;

    AccountStore::from_env(&env)?
        .upsert(
            &request.platform,
            &account_id,
            request.label.as_deref(),
            &request.region,
            &credential,
        )
        .await?;

    Ok(json_response(
        StatusCode::OK,
        &json!({
            "platform": request.platform,
            "account_id": account_id,
            "region": request.region,
            "status": "active",
        }),
    ))
}

async fn authorize(env: &Env, headers: &HeaderMap) -> Result<(), HttpError> {
    let token = env
        .secret(ADMIN_TOKEN_SECRET)
        .map_err(|_| internal(format!("{ADMIN_TOKEN_SECRET} is not configured")))?;
    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default();

    if token_matches(provided, &token.to_string()) {
        return Ok(());
    }

    let ip = headers
        .get("cf-connecting-ip")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let outcome = env
        .rate_limiter(ADMIN_LIMITER)
        .map_err(|_| internal(format!("{ADMIN_LIMITER} is not configured")))?
        .limit(ip.to_string())
        .await
        .map_err(|error| internal(error.to_string()))?;
    Err(if outcome.success {
        HttpError(StatusCode::UNAUTHORIZED, "Unauthorized".to_string())
    } else {
        HttpError(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many requests".to_string(),
        )
    })
}

fn token_matches(provided: &str, token: &str) -> bool {
    !token.is_empty() && bool::from(provided.as_bytes().ct_eq(token.as_bytes()))
}

fn credential_from_request(request: &AccountRequest) -> Result<(String, Credential), HttpError> {
    let fields = account_fields(&request.platform)
        .ok_or_else(|| bad_request(format!("Unsupported platform {}", request.platform)))?;
    if !REGIONS.contains(&request.region.as_str()) {
        return Err(bad_request(format!(
            "Region must be one of {}",
            REGIONS.join(", ")
        )));
    }

    let cookies: BTreeMap<String, String> = request
        .cookie
        .split(';')
        .filter_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            fields
                .cookies
                .contains(&name)
                .then(|| (name.to_string(), value.to_string()))
        })
        .collect();
    let account_id = cookies
        .get(fields.account_id_cookie)
        .cloned()
        .ok_or_else(|| bad_request(format!("Missing {} cookie", fields.account_id_cookie)))?;
    if let Some(missing) = fields
        .required_cookies
        .iter()
        .find(|name| !cookies.contains_key(**name))
    {
        return Err(bad_request(format!("Missing {missing} cookie")));
    }

    let headers: BTreeMap<String, String> = request
        .headers
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value.clone()))
        .filter(|(name, _)| fields.headers.contains(&name.as_str()))
        .collect();
    if !headers.contains_key("user-agent") {
        return Err(bad_request("Missing user-agent header"));
    }

    Ok((account_id, Credential { cookies, headers }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(cookie: &str) -> AccountRequest {
        AccountRequest {
            platform: "instagram".to_string(),
            region: "b".to_string(),
            label: None,
            cookie: cookie.to_string(),
            headers: BTreeMap::from([
                (
                    "User-Agent".to_string(),
                    "Mozilla/5.0 Chrome/152".to_string(),
                ),
                ("accept-language".to_string(), "en".to_string()),
                ("viewport-width".to_string(), "933".to_string()),
            ]),
        }
    }

    #[test]
    fn keeps_only_allowlisted_cookies_and_headers() {
        let (account_id, credential) = credential_from_request(&request(
            "csrftoken=csrf; wd=933x921; ds_user_id=123; sessionid=123%3Aabc%3D; rur=NCG%2C1",
        ))
        .unwrap();

        assert_eq!(account_id, "123");
        assert_eq!(
            credential.cookies.keys().collect::<Vec<_>>(),
            ["csrftoken", "ds_user_id", "rur", "sessionid"]
        );
        assert_eq!(credential.cookies["sessionid"], "123%3Aabc%3D");
        assert_eq!(
            credential.headers.keys().collect::<Vec<_>>(),
            ["accept-language", "user-agent"]
        );
    }

    #[test]
    fn rejects_sessions_without_required_cookies() {
        let HttpError(status, message) =
            credential_from_request(&request("ds_user_id=123")).unwrap_err();

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(message, "Missing sessionid cookie");
        assert!(credential_from_request(&request("sessionid=abc")).is_err());
    }

    #[test]
    fn accepts_facebook_sessions() {
        let mut request = request("datr=d; sb=s; wd=933x921; c_user=615; xs=30%3Aabc; presence=p");
        request.platform = "facebook".to_string();
        let (account_id, credential) = credential_from_request(&request).unwrap();

        assert_eq!(account_id, "615");
        assert_eq!(
            credential.cookies.keys().collect::<Vec<_>>(),
            ["c_user", "datr", "sb", "xs"]
        );
    }

    #[test]
    fn rejects_unknown_regions_and_platforms() {
        let mut bad_region = request("ds_user_id=1; sessionid=a");
        bad_region.region = "e".to_string();
        let mut bad_platform = request("ds_user_id=1; sessionid=a");
        bad_platform.platform = "myspace".to_string();

        assert!(credential_from_request(&bad_region).is_err());
        assert!(credential_from_request(&bad_platform).is_err());
    }

    #[test]
    fn requires_a_user_agent() {
        let mut request = request("ds_user_id=1; sessionid=a");
        request.headers.remove("User-Agent");

        assert!(credential_from_request(&request).is_err());
    }

    #[test]
    fn matches_tokens_exactly() {
        assert!(token_matches("secret", "secret"));
        assert!(!token_matches("secreT", "secret"));
        assert!(!token_matches("secret-longer", "secret"));
        assert!(!token_matches("", ""));
    }
}
