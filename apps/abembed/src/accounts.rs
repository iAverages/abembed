use crate::sources::{facebook, instagram, internal, warn, HttpError, ProviderResponse, Providers};
use aes_gcm::{
    aead::{Aead, AeadCore, KeyInit, OsRng, Payload},
    Aes256Gcm, Nonce,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
use worker::{
    wasm_bindgen::JsValue, D1Database, Env, Fetch, Headers, Method, Request, RequestInit,
    RequestRedirect,
};

const DATABASE_BINDING: &str = "ACCOUNTS";
const KEY_SECRET: &str = "ACCOUNT_KEY";
const ALERT_WEBHOOK_SECRET: &str = "ACCOUNT_ALERT_WEBHOOK";
const LIMITER_BINDING: &str = "ACCOUNT_LIMITER";

pub struct AccountFields {
    pub account_id_cookie: &'static str,
    pub required_cookies: &'static [&'static str],
    pub cookies: &'static [&'static str],
    pub refreshed_cookies: &'static [&'static str],
    pub headers: &'static [&'static str],
}

pub enum Attempt<T> {
    Success(T),
    AuthFailed(String),
    Failed(String),
}

pub fn account_fields(platform: &str) -> Option<&'static AccountFields> {
    match platform {
        "instagram" => Some(&instagram::ACCOUNT_FIELDS),
        "facebook" => Some(&facebook::ACCOUNT_FIELDS),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Credential {
    pub cookies: BTreeMap<String, String>,
    pub headers: BTreeMap<String, String>,
}

pub struct Account {
    pub id: i64,
    pub account_id: String,
    pub label: Option<String>,
    pub region: String,
    pub version: i64,
    pub credential: Credential,
}

#[derive(Deserialize)]
struct AccountRow {
    id: i64,
    account_id: String,
    label: Option<String>,
    region: String,
    credential: String,
    nonce: String,
    version: i64,
}

struct Cipher(Aes256Gcm);

impl Cipher {
    fn new(key_base64: &str) -> Result<Self, HttpError> {
        let key = STANDARD
            .decode(key_base64.trim())
            .map_err(|_| internal(format!("{KEY_SECRET} is not valid base64")))?;
        Aes256Gcm::new_from_slice(&key)
            .map(Self)
            .map_err(|_| internal(format!("{KEY_SECRET} must be 32 bytes")))
    }

    fn encrypt(&self, aad: &str, credential: &Credential) -> Result<(String, String), HttpError> {
        let plaintext =
            serde_json::to_vec(credential).map_err(|error| internal(error.to_string()))?;
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = self
            .0
            .encrypt(
                &nonce,
                Payload {
                    msg: &plaintext,
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| internal("failed to encrypt account credential"))?;

        Ok((STANDARD.encode(nonce), STANDARD.encode(ciphertext)))
    }

    fn decrypt(&self, aad: &str, nonce: &str, ciphertext: &str) -> Result<Credential, HttpError> {
        let failed = || internal("Failed to decrypt account credential");
        let nonce = STANDARD.decode(nonce).map_err(|_| failed())?;
        let ciphertext = STANDARD.decode(ciphertext).map_err(|_| failed())?;
        if nonce.len() != 12 {
            return Err(failed());
        }
        let plaintext = self
            .0
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &ciphertext,
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| failed())?;

        serde_json::from_slice(&plaintext).map_err(|_| failed())
    }
}

fn aad(platform: &str, account_id: &str) -> String {
    format!("{platform}:{account_id}")
}

pub struct AccountStore {
    db: D1Database,
    cipher: Cipher,
    alert_webhook: Option<String>,
}

impl AccountStore {
    pub fn from_env(env: &Env) -> Result<Self, HttpError> {
        let db = env
            .d1(DATABASE_BINDING)
            .map_err(|error| internal(error.to_string()))?;
        let key = env
            .secret(KEY_SECRET)
            .map_err(|error| internal(error.to_string()))?;

        Ok(Self {
            db,
            cipher: Cipher::new(&key.to_string())?,
            alert_webhook: env
                .secret(ALERT_WEBHOOK_SECRET)
                .ok()
                .map(|webhook| webhook.to_string()),
        })
    }

    pub async fn active(&self, platform: &str) -> Result<Vec<Account>, HttpError> {
        let rows: Vec<AccountRow> = self
            .db
            .prepare(
                "SELECT id, account_id, label, region, credential, nonce, version
                 FROM accounts WHERE platform = ?1 AND status = 'active'
                 ORDER BY last_used_at",
            )
            .bind(&[platform.into()])
            .map_err(|error| internal(error.to_string()))?
            .all()
            .await
            .and_then(|result| result.results())
            .map_err(|error| internal(error.to_string()))?;

        Ok(rows
            .into_iter()
            .filter_map(|row| {
                match self.cipher.decrypt(
                    &aad(platform, &row.account_id),
                    &row.nonce,
                    &row.credential,
                ) {
                    Ok(credential) => Some(Account {
                        id: row.id,
                        account_id: row.account_id,
                        label: row.label,
                        region: row.region,
                        version: row.version,
                        credential,
                    }),
                    Err(HttpError(_, message)) => {
                        warn(&json!({ "event": "account", "account": row.id, "error": message }));
                        None
                    }
                }
            })
            .collect())
    }

    pub async fn upsert(
        &self,
        platform: &str,
        account_id: &str,
        label: Option<&str>,
        region: &str,
        credential: &Credential,
    ) -> Result<(), HttpError> {
        let (nonce, ciphertext) = self
            .cipher
            .encrypt(&aad(platform, account_id), credential)?;

        self.execute(
            "INSERT INTO accounts (platform, account_id, label, region, credential, nonce, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active')
                 ON CONFLICT (platform, account_id) DO UPDATE SET
                     label = excluded.label,
                     region = excluded.region,
                     credential = excluded.credential,
                     nonce = excluded.nonce,
                     status = 'active',
                     status_reason = NULL,
                     version = accounts.version + 1,
                     updated_at = unixepoch()",
            &[
                platform.into(),
                account_id.into(),
                label.map_or(JsValue::NULL, JsValue::from),
                region.into(),
                ciphertext.into(),
                nonce.into(),
            ],
        )
        .await?;

        Ok(())
    }

    pub async fn first_success<T>(
        &self,
        providers: &Providers,
        platform: &str,
        url: &str,
        headers: &[(&str, &str)],
        redirect: RequestRedirect,
        classify: impl Fn(&ProviderResponse) -> Attempt<T>,
    ) -> Result<Option<T>, HttpError> {
        let fields = account_fields(platform)
            .ok_or_else(|| internal(format!("No account fields for {platform}")))?;

        for account in self.active(platform).await? {
            let request_headers = request_headers(headers, &account.credential);
            let request_headers: Vec<(&str, &str)> = request_headers
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str()))
                .collect();
            let response = match providers
                .fetch_raw(&account.region, url, &request_headers, redirect)
                .await
            {
                Ok(response) => response,
                Err(HttpError(_, message)) => {
                    warn(
                        &json!({ "event": "account", "account": account.id, "region": account.region, "error": message }),
                    );
                    continue;
                }
            };

            match classify(&response) {
                Attempt::Success(value) => {
                    let refreshed =
                        refreshed_credential(&account.credential, &response.set_cookies, fields);
                    if let Err(HttpError(_, message)) =
                        self.record_use(platform, &account, refreshed).await
                    {
                        warn(
                            &json!({ "event": "account", "account": account.id, "error": message }),
                        );
                    }
                    return Ok(Some(value));
                }
                Attempt::AuthFailed(reason) => {
                    warn(
                        &json!({ "event": "account", "account": account.id, "reauth": true, "error": reason }),
                    );
                    match self.mark_needs_reauth(account.id, &reason).await {
                        Ok(true) => self.alert_reauth(platform, &account, &reason).await,
                        Ok(false) => {}
                        Err(HttpError(_, message)) => warn(
                            &json!({ "event": "account", "account": account.id, "error": message }),
                        ),
                    }
                }
                Attempt::Failed(reason) => warn(
                    &json!({ "event": "account", "account": account.id, "region": account.region, "error": reason }),
                ),
            }
        }

        Ok(None)
    }

    async fn record_use(
        &self,
        platform: &str,
        account: &Account,
        refreshed: Option<Credential>,
    ) -> Result<(), HttpError> {
        if let Some(credential) = refreshed {
            let (nonce, ciphertext) = self
                .cipher
                .encrypt(&aad(platform, &account.account_id), &credential)?;
            let changes = self
                .execute(
                    "UPDATE accounts SET credential = ?3, nonce = ?4,
                         version = version + 1, last_used_at = unixepoch(), updated_at = unixepoch()
                     WHERE id = ?1 AND version = ?2",
                    &[
                        JsValue::from_f64(account.id as f64),
                        JsValue::from_f64(account.version as f64),
                        ciphertext.into(),
                        nonce.into(),
                    ],
                )
                .await?;
            if changes > 0 {
                return Ok(());
            }
            warn(&json!({
                "event": "account",
                "account": account.id,
                "error": "changed concurrently, keeping the newer credentials",
            }));
        }

        self.execute(
            "UPDATE accounts SET last_used_at = unixepoch() WHERE id = ?1",
            &[JsValue::from_f64(account.id as f64)],
        )
        .await?;
        Ok(())
    }

    // returns whether this call set the account to disabled.
    // used to ensure only one webhook is sent
    async fn mark_needs_reauth(&self, id: i64, reason: &str) -> Result<bool, HttpError> {
        let changes = self
            .execute(
                "UPDATE accounts SET status = 'needs_reauth', status_reason = ?2,
                     version = version + 1, updated_at = unixepoch()
                 WHERE id = ?1 AND status = 'active'",
                &[JsValue::from_f64(id as f64), reason.into()],
            )
            .await?;
        Ok(changes > 0)
    }

    async fn alert_reauth(&self, platform: &str, account: &Account, reason: &str) {
        let Some(webhook) = &self.alert_webhook else {
            return;
        };
        let body = json!({
            "content": reauth_message(platform, account, reason),
            "allowed_mentions": { "parse": [] },
        });
        let headers = Headers::new();
        let sent = async {
            headers.set("content-type", "application/json")?;
            let mut init = RequestInit::new();
            init.with_method(Method::Post)
                .with_headers(headers)
                .with_body(Some(body.to_string().into()));
            Fetch::Request(Request::new_with_init(webhook, &init)?)
                .send()
                .await
        };

        match sent.await {
            Ok(response) if (200..300).contains(&response.status_code()) => {}
            Ok(response) => {
                warn(&json!({ "event": "reauth_alert", "status": response.status_code() }))
            }
            Err(error) => warn(&json!({ "event": "reauth_alert", "error": error.to_string() })),
        }
    }

    async fn execute(&self, sql: &str, params: &[JsValue]) -> Result<usize, HttpError> {
        let result = self
            .db
            .prepare(sql)
            .bind(params)
            .map_err(|error| internal(error.to_string()))?
            .run()
            .await
            .map_err(|error| internal(error.to_string()))?;

        Ok(result
            .meta()
            .map_err(|error| internal(error.to_string()))?
            .and_then(|meta| meta.changes)
            .unwrap_or(0))
    }
}

fn reauth_message(platform: &str, account: &Account, reason: &str) -> String {
    let name = account
        .label
        .as_deref()
        .map_or(String::new(), |label| format!("**{label}** "));
    format!(
        "{platform} account {name}(`{}`, region `{}`) needs reauth: {reason}.",
        account.account_id, account.region
    )
}

pub async fn with_account<T>(
    env: &Env,
    providers: &Providers,
    platform: &str,
    url: &str,
    headers: &[(&str, &str)],
    redirect: RequestRedirect,
    classify: impl Fn(&ProviderResponse) -> Attempt<T>,
) -> Option<T> {
    let allowed = match env.rate_limiter(LIMITER_BINDING) {
        Ok(limiter) => limiter
            .limit(platform.to_string())
            .await
            .is_ok_and(|outcome| outcome.success),
        Err(error) => {
            warn(
                &json!({ "event": "account_fallback", "binding": LIMITER_BINDING, "error": error.to_string() }),
            );
            false
        }
    };
    if !allowed {
        warn(
            &json!({ "event": "account_fallback", "platform": platform, "url": url, "error": "rate limited" }),
        );
        return None;
    }

    AccountStore::from_env(env)
        .inspect_err(|HttpError(_, message)| {
            warn(&json!({ "event": "account_fallback", "platform": platform, "url": url, "error": message }));
        })
        .ok()?
        .first_success(providers, platform, url, headers, redirect, classify)
        .await
        .unwrap_or_else(|HttpError(_, message)| {
            warn(&json!({ "event": "account_fallback", "platform": platform, "url": url, "error": message }));
            None
        })
}

fn request_headers(base: &[(&str, &str)], credential: &Credential) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = base
        .iter()
        .filter(|(name, _)| !credential.headers.contains_key(&name.to_ascii_lowercase()))
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    headers.extend(credential.headers.clone());
    let cookie = credential
        .cookies
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ");
    headers.push(("cookie".to_string(), cookie));
    headers
}

fn is_deletion(set_cookie: &str) -> bool {
    let mut parts = set_cookie.split(';').map(|s| s.trim());
    let value = parts
        .next()
        .and_then(|pair| pair.split_once('='))
        .map_or("", |(_, value)| value);

    value.is_empty()
        || value == "deleted"
        || parts.any(|attribute| {
            attribute
                .to_ascii_lowercase()
                .strip_prefix("max-age=")
                .and_then(|age| age.parse::<i64>().ok())
                .is_some_and(|age| age <= 0)
        })
}

pub fn clears_cookie(set_cookies: &[String], name: &str) -> bool {
    set_cookies.iter().any(|set_cookie| {
        set_cookie
            .split_once('=')
            .is_some_and(|(cookie, _)| cookie.trim() == name)
            && is_deletion(set_cookie)
    })
}

fn refreshed_credential(
    credential: &Credential,
    set_cookies: &[String],
    fields: &AccountFields,
) -> Option<Credential> {
    let mut updated = credential.clone();
    for set_cookie in set_cookies {
        let Some((name, value)) = set_cookie
            .split(';')
            .next()
            .and_then(|pair| pair.trim().split_once('='))
        else {
            continue;
        };
        if !is_deletion(set_cookie) && fields.cookies.contains(&name) {
            updated.cookies.insert(name.to_string(), value.to_string());
        }
    }

    fields
        .refreshed_cookies
        .iter()
        .any(|name| updated.cookies.get(*name) != credential.cookies.get(*name))
        .then_some(updated)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=";

    fn credential() -> Credential {
        Credential {
            cookies: BTreeMap::from([
                ("sessionid".to_string(), "session".to_string()),
                ("ds_user_id".to_string(), "123".to_string()),
            ]),
            headers: BTreeMap::from([("user-agent".to_string(), "Mozilla/5.0".to_string())]),
        }
    }

    #[test]
    fn round_trips_credentials() {
        let cipher = Cipher::new(KEY).unwrap();
        let (nonce, ciphertext) = cipher.encrypt("instagram:123", &credential()).unwrap();

        assert_eq!(
            cipher
                .decrypt("instagram:123", &nonce, &ciphertext)
                .unwrap(),
            credential()
        );
    }

    #[test]
    fn rejects_credentials_encrypted_with_another_key() {
        let (nonce, ciphertext) = Cipher::new(KEY)
            .unwrap()
            .encrypt("instagram:123", &credential())
            .unwrap();
        let other = Cipher::new("ZmVkY2JhOTg3NjU0MzIxMGZlZGNiYTk4NzY1NDMyMTA=").unwrap();

        assert!(other.decrypt("instagram:123", &nonce, &ciphertext).is_err());
    }

    #[test]
    fn account_headers_override_caller_headers_and_add_cookies() {
        let headers = request_headers(
            &[("User-Agent", "Mozilla/5.0"), ("Accept", "text/html")],
            &credential(),
        );

        assert_eq!(
            headers,
            [
                ("Accept".to_string(), "text/html".to_string()),
                ("user-agent".to_string(), "Mozilla/5.0".to_string()),
                (
                    "cookie".to_string(),
                    "ds_user_id=123; sessionid=session".to_string()
                ),
            ]
        );
    }

    #[test]
    fn ignores_cookie_that_do_not_need_a_write() {
        let set_cookies = ["rur=\"NCG\\054123\"; Domain=.instagram.com; Path=/".to_string()];

        assert_eq!(
            refreshed_credential(&credential(), &set_cookies, &instagram::ACCOUNT_FIELDS),
            None
        );
    }

    #[test]
    fn writes_back_rotated_session_cookies() {
        let set_cookies = [
            "csrftoken=rotated; Max-Age=31449600; Path=/; Secure".to_string(),
            "unrelated=1; Path=/".to_string(),
            "sessionid=deleted; expires=Thu, 01-Jan-1970 00:00:01 GMT; Max-Age=-1791466010; path=/"
                .to_string(),
        ];
        let refreshed =
            refreshed_credential(&credential(), &set_cookies, &instagram::ACCOUNT_FIELDS).unwrap();

        assert_eq!(refreshed.cookies["csrftoken"], "rotated");
        assert_eq!(refreshed.cookies["sessionid"], "session");
        assert!(!refreshed.cookies.contains_key("unrelated"));
    }

    #[test]
    fn detects_deleted_cookies() {
        let set_cookies = [
            "sessionid=deleted; expires=Thu, 01-Jan-1970 00:00:01 GMT; Max-Age=-1791466010; path=/"
                .to_string(),
            "csrftoken=fresh; Max-Age=31449600; Path=/".to_string(),
        ];

        assert!(clears_cookie(&set_cookies, "sessionid"));
        assert!(!clears_cookie(&set_cookies, "csrftoken"));
        assert!(clears_cookie(&["mid=; Path=/".to_string()], "mid"));
    }

    #[test]
    fn rejects_keys_that_are_not_32_bytes() {
        assert!(Cipher::new(&STANDARD.encode([0u8; 16])).is_err());
    }
}
