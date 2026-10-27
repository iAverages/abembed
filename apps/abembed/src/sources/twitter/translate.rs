use super::{
    page::{decode_html_entities, non_empty_str},
    BEARER,
};
use crate::accounts::{with_account, Attempt};
use crate::sources::{warn, EmbedPost, ProviderResponse, Providers, Translation};
use serde::Serialize;
use serde_json::{json, Value};
use worker::{Env, RequestRedirect};

const GROK_URL: &str = "https://api.x.com/2/grok/translation.json";
const AI_BINDING: &str = "AI";
const AI_MODEL: &str = "@cf/meta/m2m100-1.2b";

const NO_TEXT_LANGS: [&str; 8] = ["und", "zxx", "qme", "qam", "qct", "qht", "qst", "art"];

pub(super) async fn translate(
    env: &Env,
    providers: &Providers,
    post: &mut EmbedPost,
    language: &str,
) {
    let source = post
        .lang
        .as_deref()
        .filter(|lang| !NO_TEXT_LANGS.contains(lang));
    let Some(text) = post.description.as_deref() else {
        return;
    };
    if source.is_some_and(|source| primary(source) == primary(language)) {
        return;
    }

    let translated = match grok(env, providers, &post.provider_id, language).await {
        Some(text) => Some((text, "Grok")),
        None => workers_ai(env, text, source, language)
            .await
            .map(|text| (text, "AI")),
    };
    post.translation = translated.map(|(text, provider)| Translation {
        text,
        source_lang: source.unwrap_or("und").to_string(),
        target_lang: language.to_string(),
        provider: provider.to_string(),
    });
}

async fn grok(env: &Env, providers: &Providers, id: &str, language: &str) -> Option<String> {
    let body = json!({
        "content_type": "POST",
        "id": id,
        "dst_lang": language,
        "include_polls": true,
    })
    .to_string();

    with_account(
        env,
        providers,
        "twitter",
        GROK_URL,
        &[
            ("Authorization", BEARER),
            ("Content-Type", "application/json"),
            ("x-twitter-auth-type", "OAuth2Session"),
            ("x-twitter-active-user", "yes"),
            ("x-twitter-client-language", language),
        ],
        RequestRedirect::Manual,
        Some(&body),
        classify,
    )
    .await
}

fn classify(response: &ProviderResponse) -> Attempt<String> {
    match response.status {
        200..=299 => match grok_text(&response.body) {
            Some(text) => Attempt::Success(text),
            None => Attempt::Failed("grok returned no translation".to_string()),
        },
        401 => Attempt::AuthFailed("grok translation returned 401".to_string()),
        status => Attempt::Failed(format!(
            "grok translation returned {status}: {}",
            response.body.chars().take(200).collect::<String>()
        )),
    }
}

// the response streams json objects back to back, the text is spread over result.text
fn grok_text(body: &str) -> Option<String> {
    let text: String = serde_json::Deserializer::from_str(body)
        .into_iter::<Value>()
        .filter_map(Result::ok)
        .filter_map(|chunk| chunk["result"]["text"].as_str().map(str::to_string))
        .collect();
    let text = decode_html_entities(text.trim());
    (!text.is_empty()).then_some(text)
}

#[derive(Serialize)]
struct AiInput<'a> {
    text: &'a str,
    target_lang: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_lang: Option<&'a str>,
}

async fn workers_ai(env: &Env, text: &str, source: Option<&str>, language: &str) -> Option<String> {
    let failed = |error: String| warn(&json!({ "event": "translation", "error": error }));
    let ai = env
        .ai(AI_BINDING)
        .inspect_err(|error| failed(error.to_string()))
        .ok()?;

    let input = AiInput {
        text,
        target_lang: primary(language),
        source_lang: source.map(primary),
    };

    let output: Value = ai
        .run(AI_MODEL, input)
        .await
        .inspect_err(|error| failed(error.to_string()))
        .ok()?;

    non_empty_str(&output["translated_text"]).map(|text| text.trim().to_string())
}

// `zh-tw` is `zh` to the models
fn primary(language: &str) -> &str {
    language.split('-').next().unwrap_or(language)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(status: u16, body: &str) -> ProviderResponse {
        ProviderResponse {
            status,
            location: None,
            set_cookies: Vec::new(),
            body: body.to_string(),
        }
    }

    #[test]
    fn joins_the_streamed_translation_chunks() {
        let body = r#"{"result":{"content_type":"POST","text":"Vier "}}
{"result":{"text":"was? &gt;:3"}}{"result":{"entities":{}}}"#;
        assert_eq!(grok_text(body).as_deref(), Some("Vier was? >:3"));
        assert!(grok_text(r#"{"result":{"text":"  "}}"#).is_none());
        assert!(grok_text("").is_none());
    }

    #[test]
    fn classifies_grok_responses() {
        assert!(matches!(
            classify(&response(200, r#"{"result":{"text":"Hallo"}}"#)),
            Attempt::Success(text) if text == "Hallo"
        ));
        assert!(matches!(classify(&response(200, "{}")), Attempt::Failed(_)));
        assert!(matches!(
            classify(&response(401, "")),
            Attempt::AuthFailed(_)
        ));
        assert!(matches!(
            classify(&response(429, r#"{"errors":[{"code":88}]}"#)),
            Attempt::Failed(reason) if reason.contains("429")
        ));
    }

    #[test]
    fn strips_regions_from_language_codes() {
        assert_eq!(primary("zh-tw"), "zh");
        assert_eq!(primary("de"), "de");
    }
}
