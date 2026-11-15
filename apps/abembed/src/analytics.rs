use crate::sources::warn;
use common::EventName;
use serde_json::{json, Value};
use worker::{Env, Fetch, Headers, Method, Request, RequestInit};

const API_KEY_VAR: &str = "POSTHOG_API_KEY";
const HOST_VAR: &str = "POSTHOG_HOST";
const DEFAULT_HOST: &str = "https://eu.i.posthog.com";

#[derive(Clone)]
pub struct Event(pub EventName, pub Value);

fn capture_body(api_key: &str, distinct_id: &str, Event(name, mut properties): Event) -> Value {
    properties["$process_person_profile"] = false.into();
    properties["$geoip_disable"] = true.into();
    json!({
        "api_key": api_key,
        "event": name.as_str(),
        "distinct_id": distinct_id,
        "properties": properties,
    })
}

pub async fn capture(env: Env, event: Event) {
    let Ok(api_key) = env.var(API_KEY_VAR) else {
        return;
    };

    let host = env
        .var(HOST_VAR)
        .map_or(DEFAULT_HOST.to_string(), |host| host.to_string());

    let headers = Headers::new();
    let sent = async {
        let mut id = [0u8; 16];
        getrandom::getrandom(&mut id)
            .map_err(|error| worker::Error::RustError(error.to_string()))?;
        let distinct_id = format!("{:032x}", u128::from_ne_bytes(id));
        let body = capture_body(&api_key.to_string(), &distinct_id, event);

        headers.set("content-type", "application/json")?;
        let mut init = RequestInit::new();
        init.with_method(Method::Post)
            .with_headers(headers)
            .with_body(Some(body.to_string().into()));
        Fetch::Request(Request::new_with_init(&format!("{host}/i/v0/e/"), &init)?)
            .send()
            .await
    };

    match sent.await {
        Ok(response) if (200..300).contains(&response.status_code()) => {}
        Ok(response) => warn(&json!({ "event": "analytics", "status": response.status_code() })),
        Err(error) => warn(&json!({ "event": "analytics", "error": error.to_string() })),
    }
}
