use crate::config::OpenAiConfig;
use crate::message::Message;
use serde::{Deserialize, Serialize};
use std::env;
use std::fmt;
use thiserror::Error;

const MAX_ERROR_BODY_BYTES: usize = 8 * 1024;

pub struct OpenAiClient {
    http: reqwest::Client,
    endpoint: String,
    api_key: String,
    model: String,
}

impl OpenAiClient {
    pub fn from_config(config: &OpenAiConfig) -> Result<Self, ProviderError> {
        let api_key = read_api_key(config.api_key_env())?;
        Ok(Self::new(config.base_url(), api_key, config.model()))
    }

    fn new(base_url: &str, api_key: String, model: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            endpoint: format!("{}/responses", base_url.trim_end_matches('/')),
            api_key,
            model: model.to_owned(),
        }
    }

    pub async fn complete(
        &self,
        instructions: &str,
        messages: &[Message],
    ) -> Result<String, ProviderError> {
        let request = self.build_request(instructions, messages)?;
        let response = self.http.execute(request).await?;

        let status = response.status();
        if !status.is_success() {
            let body = read_bounded_body(response).await?;
            return Err(ProviderError::Api {
                status: status.as_u16(),
                body,
            });
        }

        let bytes = response.bytes().await?;
        decode_response(&bytes)
    }

    fn build_request(
        &self,
        instructions: &str,
        messages: &[Message],
    ) -> Result<reqwest::Request, ProviderError> {
        let request = CreateResponseRequest {
            model: &self.model,
            instructions,
            input: messages,
            store: false,
        };

        Ok(self
            .http
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .json(&request)
            .build()?)
    }
}

fn read_api_key(name: &str) -> Result<String, ProviderError> {
    match env::var(name) {
        Ok(value) if !value.trim().is_empty() => Ok(value),
        Ok(_) | Err(_) => Err(ProviderError::MissingApiKey {
            name: name.to_owned(),
        }),
    }
}

#[derive(Debug, Serialize)]
struct CreateResponseRequest<'a> {
    model: &'a str,
    instructions: &'a str,
    input: &'a [Message],
    store: bool,
}

#[derive(Debug, Deserialize)]
struct CreateResponse {
    status: String,
    #[serde(default)]
    error: Option<ResponseError>,
    output: Vec<OutputItem>,
}

#[derive(Debug, Deserialize)]
struct ResponseError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct OutputItem {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    content: Vec<ContentPart>,
}

#[derive(Debug, Deserialize)]
struct ContentPart {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

fn decode_response(bytes: &[u8]) -> Result<String, ProviderError> {
    let response: CreateResponse = serde_json::from_slice(bytes)?;

    if let Some(error) = response.error {
        return Err(ProviderError::Response(error.message));
    }
    if response.status != "completed" {
        return Err(ProviderError::IncompleteResponse(response.status));
    }

    let mut output_text = String::new();
    for item in response.output {
        if item.kind != "message" {
            continue;
        }
        for part in item.content {
            if part.kind != "output_text" {
                continue;
            }
            let text = part.text.ok_or(ProviderError::MalformedOutputText)?;
            output_text.push_str(&text);
        }
    }

    if output_text.is_empty() {
        return Err(ProviderError::MissingOutputText);
    }

    Ok(output_text)
}

async fn read_bounded_body(mut response: reqwest::Response) -> Result<BoundedBody, reqwest::Error> {
    let mut buffer = BoundedBodyBuffer::new();

    while let Some(chunk) = response.chunk().await? {
        if !buffer.push(&chunk) {
            break;
        }
    }

    Ok(buffer.finish())
}

struct BoundedBodyBuffer {
    bytes: Vec<u8>,
    truncated: bool,
}

impl BoundedBodyBuffer {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            truncated: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) -> bool {
        let remaining = MAX_ERROR_BODY_BYTES.saturating_sub(self.bytes.len());
        if chunk.len() > remaining {
            self.bytes.extend_from_slice(&chunk[..remaining]);
            self.truncated = true;
            return false;
        }

        self.bytes.extend_from_slice(chunk);
        true
    }

    fn finish(self) -> BoundedBody {
        BoundedBody {
            text: String::from_utf8_lossy(&self.bytes).into_owned(),
            truncated: self.truncated,
        }
    }
}

#[derive(Debug)]
pub(crate) struct BoundedBody {
    text: String,
    truncated: bool,
}

impl fmt::Display for BoundedBody {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.text)?;
        if self.truncated {
            formatter.write_str(" [truncated]")?;
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("environment variable `{name}` is missing or empty")]
    MissingApiKey { name: String },
    #[error("OpenAI request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("OpenAI API returned HTTP {status}: {body}")]
    Api { status: u16, body: BoundedBody },
    #[error("OpenAI response was not valid JSON: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("OpenAI response reported an error: {0}")]
    Response(String),
    #[error("OpenAI response ended with status `{0}` instead of `completed`")]
    IncompleteResponse(String),
    #[error("OpenAI output text was missing its `text` field")]
    MalformedOutputText,
    #[error("OpenAI response contained no output text")]
    MissingOutputText,
}

#[cfg(test)]
mod tests {
    use super::{
        BoundedBodyBuffer, CreateResponseRequest, MAX_ERROR_BODY_BYTES, OpenAiClient,
        ProviderError, decode_response, read_api_key,
    };
    use crate::message::Message;
    use reqwest::header::AUTHORIZATION;
    use serde_json::json;

    #[test]
    fn constructs_the_expected_request_json() {
        let messages = [Message::user("Explain ownership")];
        let request = CreateResponseRequest {
            model: "test-model",
            instructions: "Be concise",
            input: &messages,
            store: false,
        };

        assert_eq!(
            serde_json::to_value(request).unwrap(),
            json!({
                "model": "test-model",
                "instructions": "Be concise",
                "input": [{"role": "user", "content": "Explain ownership"}],
                "store": false
            })
        );
    }

    #[test]
    fn decodes_text_without_assuming_the_first_output_item_is_a_message() {
        let response = br#"{
            "status": "completed",
            "output": [
                {"type": "reasoning"},
                {
                    "type": "message",
                    "content": [
                        {"type": "output_text", "text": "Ownership "},
                        {"type": "output_text", "text": "is explicit."}
                    ]
                }
            ]
        }"#;

        assert_eq!(decode_response(response).unwrap(), "Ownership is explicit.");
    }

    #[test]
    fn rejects_malformed_and_incomplete_responses() {
        assert!(matches!(
            decode_response(b"not json"),
            Err(ProviderError::Decode(_))
        ));
        assert!(matches!(
            decode_response(br#"{"status":"incomplete","output":[]}"#),
            Err(ProviderError::IncompleteResponse(status)) if status == "incomplete"
        ));
        assert!(matches!(
            decode_response(br#"{"status":"completed","output":[]}"#),
            Err(ProviderError::MissingOutputText)
        ));
    }

    #[test]
    fn missing_api_keys_are_reported_by_variable_name() {
        let name = format!("TAPET_TEST_MISSING_API_KEY_{}", std::process::id());

        assert!(matches!(
            read_api_key(&name),
            Err(ProviderError::MissingApiKey { name: missing }) if missing == name
        ));
    }

    #[test]
    fn builds_the_endpoint_authentication_header_and_body() {
        let client = OpenAiClient::new(
            "https://example.test/v1/",
            "test-secret".to_owned(),
            "test-model",
        );

        let messages = [
            Message::user("Hello?"),
            Message::assistant("Hello!"),
            Message::user("Remember me?"),
        ];
        let request = client.build_request("Be helpful", &messages).unwrap();

        assert_eq!(request.method(), reqwest::Method::POST);
        assert_eq!(request.url().as_str(), "https://example.test/v1/responses");
        assert_eq!(
            request.headers().get(AUTHORIZATION).unwrap(),
            "Bearer test-secret"
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(
                request.body().and_then(reqwest::Body::as_bytes).unwrap()
            )
            .unwrap(),
            json!({
                "model": "test-model",
                "instructions": "Be helpful",
                "input": [
                    {"role": "user", "content": "Hello?"},
                    {"role": "assistant", "content": "Hello!"},
                    {"role": "user", "content": "Remember me?"}
                ],
                "store": false
            })
        );
    }

    #[test]
    fn bounds_non_successful_response_bodies() {
        let mut buffer = BoundedBodyBuffer::new();

        assert!(!buffer.push(&vec![b'x'; MAX_ERROR_BODY_BYTES + 100]));
        let body = buffer.finish();
        let error = ProviderError::Api { status: 429, body };

        match &error {
            ProviderError::Api { status, body } => {
                assert_eq!(*status, 429);
                assert_eq!(body.text.len(), MAX_ERROR_BODY_BYTES);
                assert!(body.truncated);
            }
            other => panic!("expected an API error, got {other}"),
        }
        assert!(error.to_string().ends_with(" [truncated]"));
    }
}
