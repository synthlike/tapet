use crate::config::Agent;
use crate::message::Message;
use crate::stream::{Completion, ResponseRound, StreamEvent, ToolCall};
use eventsource_stream::{Event, Eventsource};
use futures_util::stream::{BoxStream, Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::env;
use std::fmt;
use thiserror::Error;

const MAX_ERROR_BODY_BYTES: usize = 8 * 1024;
const STATELESS_REASONING_INCLUDE: [&str; 1] = ["reasoning.encrypted_content"];

pub struct OpenAiClient {
    http: reqwest::Client,
    endpoint: String,
    api_key: String,
    model: String,
}

impl OpenAiClient {
    pub fn from_agent(agent: &Agent) -> Result<Self, ProviderError> {
        Self::from_settings(agent.base_url(), agent.api_key_env(), agent.model())
    }

    pub fn from_settings(
        base_url: &str,
        api_key_env: &str,
        model: &str,
    ) -> Result<Self, ProviderError> {
        let api_key = read_api_key(api_key_env)?;
        Ok(Self::new(base_url, api_key, model))
    }

    fn new(base_url: &str, api_key: String, model: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            endpoint: format!("{}/responses", base_url.trim_end_matches('/')),
            api_key,
            model: model.to_owned(),
        }
    }

    pub async fn stream(
        &self,
        instructions: &str,
        messages: &[Message],
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let request = self.build_request(instructions, messages)?;
        self.execute_stream(request).await
    }

    pub async fn stream_with_tools(
        &self,
        instructions: &str,
        input: &ToolInput,
        tools_enabled: bool,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let tools = available_tools();
        let request = self.build_json_request(
            instructions,
            input.items(),
            if tools_enabled { tools.as_slice() } else { &[] },
            Some(&STATELESS_REASONING_INCLUDE),
        )?;
        self.execute_stream(request).await
    }

    async fn execute_stream(
        &self,
        request: reqwest::Request,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let response = self.http.execute(request).await?;

        let status = response.status();
        if !status.is_success() {
            let body = read_bounded_body(response).await?;
            return Err(ProviderError::Api {
                status: status.as_u16(),
                body,
            });
        }

        Ok(decode_stream(response.bytes_stream()))
    }

    fn build_request(
        &self,
        instructions: &str,
        messages: &[Message],
    ) -> Result<reqwest::Request, ProviderError> {
        let input = serde_json::to_value(messages)?;
        self.build_json_request(instructions, input, &[], None)
    }

    fn build_json_request(
        &self,
        instructions: &str,
        input: Value,
        tools: &[FunctionTool],
        include: Option<&[&str]>,
    ) -> Result<reqwest::Request, ProviderError> {
        let request = CreateResponseRequest {
            model: &self.model,
            instructions,
            input,
            store: false,
            stream: true,
            tools,
            include,
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
    input: Value,
    store: bool,
    stream: bool,
    #[serde(skip_serializing_if = "<[FunctionTool]>::is_empty")]
    tools: &'a [FunctionTool],
    #[serde(skip_serializing_if = "Option::is_none")]
    include: Option<&'a [&'a str]>,
}

#[derive(Clone, Debug)]
pub struct ToolInput {
    items: Vec<Value>,
}

impl ToolInput {
    pub fn from_messages(messages: &[Message]) -> Result<Self, ProviderError> {
        let Value::Array(items) = serde_json::to_value(messages)? else {
            unreachable!("a message slice serializes as a JSON array");
        };
        Ok(Self { items })
    }

    pub fn append_output_items(&mut self, output_items: Vec<Value>) {
        self.items.extend(output_items);
    }

    pub fn append_function_output(&mut self, call_id: &str, output: String) {
        self.items.push(json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": output,
        }));
    }

    fn items(&self) -> Value {
        Value::Array(self.items.clone())
    }
}

#[derive(Debug, Serialize)]
struct FunctionTool {
    #[serde(rename = "type")]
    kind: &'static str,
    name: &'static str,
    description: &'static str,
    parameters: Value,
    strict: bool,
}

fn available_tools() -> [FunctionTool; 4] {
    [
        FunctionTool {
            kind: "function",
            name: "read_file",
            description: "Read a UTF-8 text file relative to the current workspace after the user approves access.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Workspace-relative path of the file to read"
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            strict: true,
        },
        FunctionTool {
            kind: "function",
            name: "list_files",
            description: "List the immediate entries of a workspace-relative directory after the user approves access.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Workspace-relative directory to list"
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            strict: true,
        },
        FunctionTool {
            kind: "function",
            name: "write_file",
            description: "Write UTF-8 text to a workspace-relative file after the user reviews a diff and approves. Creates the file if it doesn't exist; the parent directory must already exist.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Workspace-relative path of the file to write"
                    },
                    "content": {
                        "type": "string",
                        "description": "Full replacement contents of the file"
                    }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
            strict: true,
        },
        FunctionTool {
            kind: "function",
            name: "search_files",
            description: "Search text files under a workspace-relative directory for a literal, case-sensitive substring, after the user approves access. Returns matching file paths, line numbers, and line content.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Literal substring to search for (case-sensitive)"
                    },
                    "path": {
                        "type": ["string", "null"],
                        "description": "Workspace-relative directory to search; omit or null to search the whole workspace"
                    }
                },
                "required": ["query", "path"],
                "additionalProperties": false
            }),
            strict: true,
        },
    ]
}

#[derive(Debug, Deserialize)]
struct EventEnvelope {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Debug, Deserialize)]
struct TextDeltaEvent {
    delta: String,
}

#[derive(Debug, Deserialize)]
struct OutputItemDoneEvent {
    item: OutputItem,
}

#[derive(Debug, Deserialize)]
struct OutputItem {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Debug, Deserialize)]
struct FunctionCallDoneEvent {
    item: FunctionCallItem,
}

#[derive(Debug, Deserialize)]
struct FunctionCallItem {
    call_id: String,
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct CompletedEvent {
    response: CompletedResponse,
}

#[derive(Debug, Deserialize)]
struct CompletedResponse {
    #[serde(default)]
    id: Option<String>,
    status: String,
    usage: Usage,
    #[serde(default)]
    output: Vec<Value>,
}

#[derive(Debug, Deserialize)]
struct Usage {
    input_tokens: u64,
    output_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct ErrorEvent {
    message: String,
}

#[derive(Debug, Deserialize)]
struct FailedEvent {
    response: FailedResponse,
}

#[derive(Debug, Deserialize)]
struct FailedResponse {
    status: String,
    #[serde(default)]
    error: Option<ErrorEvent>,
}

pub(crate) fn decode_stream<S, B, E>(
    source: S,
) -> BoxStream<'static, Result<StreamEvent, ProviderError>>
where
    S: Stream<Item = Result<B, E>> + Send + 'static,
    B: AsRef<[u8]> + Send + 'static,
    E: fmt::Display + Send + 'static,
{
    source
        .eventsource()
        .filter_map(|result| async move {
            match result {
                Ok(event) => decode_event(&event).transpose(),
                Err(error) => Some(Err(ProviderError::StreamProtocol(error.to_string()))),
            }
        })
        .boxed()
}

fn decode_event(event: &Event) -> Result<Option<StreamEvent>, ProviderError> {
    let kind = if event.event == "message" {
        serde_json::from_str::<EventEnvelope>(&event.data)
            .map_err(|source| malformed_event("message", source))?
            .kind
    } else {
        event.event.clone()
    };

    match kind.as_str() {
        "response.output_text.delta" => {
            let event: TextDeltaEvent = parse_known_event(&kind, &event.data)?;
            Ok(Some(StreamEvent::TextDelta(event.delta)))
        }
        "response.output_item.done" => {
            let output: OutputItemDoneEvent = parse_known_event(&kind, &event.data)?;
            if output.item.kind != "function_call" {
                return Ok(None);
            }
            let event: FunctionCallDoneEvent = parse_known_event(&kind, &event.data)?;
            Ok(Some(StreamEvent::ToolCallProposed(ToolCall {
                call_id: event.item.call_id,
                name: event.item.name,
                arguments: event.item.arguments,
            })))
        }
        "response.completed" => {
            let event: CompletedEvent = parse_known_event(&kind, &event.data)?;
            if event.response.status != "completed" {
                return Err(ProviderError::UnexpectedCompletionStatus(
                    event.response.status,
                ));
            }
            Ok(Some(StreamEvent::Completed(ResponseRound {
                completion: Completion {
                    provider_response_id: event.response.id,
                    input_tokens: event.response.usage.input_tokens,
                    output_tokens: event.response.usage.output_tokens,
                },
                output_items: event.response.output,
            })))
        }
        "error" => {
            let event: ErrorEvent = parse_known_event(&kind, &event.data)?;
            Err(ProviderError::Response(event.message))
        }
        "response.failed" | "response.incomplete" => {
            let event: FailedEvent = parse_known_event(&kind, &event.data)?;
            let message = event
                .response
                .error
                .map(|error| error.message)
                .unwrap_or(event.response.status);
            Err(ProviderError::Response(message))
        }
        _ => Ok(None),
    }
}

fn parse_known_event<T>(kind: &str, data: &str) -> Result<T, ProviderError>
where
    T: for<'de> Deserialize<'de>,
{
    let envelope: EventEnvelope =
        serde_json::from_str(data).map_err(|source| malformed_event(kind, source))?;
    if envelope.kind != kind {
        return Err(ProviderError::MismatchedStreamEvent {
            event_kind: kind.to_owned(),
            data_kind: envelope.kind,
        });
    }
    serde_json::from_str(data).map_err(|source| malformed_event(kind, source))
}

fn malformed_event(kind: &str, source: serde_json::Error) -> ProviderError {
    ProviderError::MalformedStreamEvent {
        kind: kind.to_owned(),
        source,
    }
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
    #[error("could not serialize an OpenAI request: {0}")]
    Json(#[from] serde_json::Error),
    #[error("OpenAI API returned HTTP {status}: {body}")]
    Api { status: u16, body: BoundedBody },
    #[error("OpenAI stream protocol failed: {0}")]
    StreamProtocol(String),
    #[error("OpenAI `{kind}` event was malformed: {source}")]
    MalformedStreamEvent {
        kind: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("OpenAI SSE event `{event_kind}` contained `{data_kind}` data")]
    MismatchedStreamEvent {
        event_kind: String,
        data_kind: String,
    },
    #[error("OpenAI stream reported an error: {0}")]
    Response(String),
    #[error("OpenAI completion event contained status `{0}`")]
    UnexpectedCompletionStatus(String),
    #[error("OpenAI stream ended before a completion event")]
    IncompleteStream,
}

#[cfg(test)]
mod tests {
    use super::{
        BoundedBodyBuffer, CreateResponseRequest, MAX_ERROR_BODY_BYTES, OpenAiClient,
        ProviderError, STATELESS_REASONING_INCLUDE, ToolInput, available_tools, decode_stream,
        read_api_key,
    };
    use crate::message::Message;
    use crate::stream::{Completion, ResponseRound, StreamEvent, ToolCall};
    use futures_util::{Stream, StreamExt, stream};
    use reqwest::header::AUTHORIZATION;
    use serde_json::json;
    use std::io;
    use std::pin::Pin;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::task::{Context, Poll};

    #[test]
    fn constructs_the_expected_request_json() {
        let messages = [Message::user("Explain ownership")];
        let request = CreateResponseRequest {
            model: "test-model",
            instructions: "Be concise",
            input: serde_json::to_value(&messages).unwrap(),
            store: false,
            stream: true,
            tools: &[],
            include: None,
        };

        assert_eq!(
            serde_json::to_value(request).unwrap(),
            json!({
                "model": "test-model",
                "instructions": "Be concise",
                "input": [{"role": "user", "content": "Explain ownership"}],
                "store": false,
                "stream": true
            })
        );
    }

    #[tokio::test]
    async fn parses_events_split_across_transport_chunks() {
        let fixture = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Owner\"}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ship\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\n\n"
        );
        let bytes = fixture.as_bytes();
        let chunks = [
            bytes[..17].to_vec(),
            bytes[17..93].to_vec(),
            bytes[93..].to_vec(),
        ];

        let events = fixture_events(chunks).await.unwrap();

        assert_eq!(
            events,
            [
                StreamEvent::TextDelta("Owner".to_owned()),
                StreamEvent::TextDelta("ship".to_owned()),
                StreamEvent::Completed(ResponseRound {
                    completion: Completion {
                        provider_response_id: Some("resp_1".to_owned()),
                        input_tokens: 3,
                        output_tokens: 2,
                    },
                    output_items: Vec::new(),
                })
            ]
        );
    }

    #[tokio::test]
    async fn ignores_unknown_events() {
        let fixture = concat!(
            "event: response.future.event\n",
            "data: this need not match a known schema\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"kept\"}\n\n"
        );

        assert_eq!(
            fixture_events([fixture.as_bytes().to_vec()]).await.unwrap(),
            [StreamEvent::TextDelta("kept".to_owned())]
        );
    }

    #[tokio::test]
    async fn decodes_completed_function_call_items_as_proposals() {
        let fixture = concat!(
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"Cargo.toml\\\"}\"}}\n\n"
        );

        assert_eq!(
            fixture_events([fixture.as_bytes().to_vec()]).await.unwrap(),
            [StreamEvent::ToolCallProposed(ToolCall {
                call_id: "call_1".to_owned(),
                name: "read_file".to_owned(),
                arguments: "{\"path\":\"Cargo.toml\"}".to_owned(),
            })]
        );
    }

    #[tokio::test]
    async fn turns_provider_error_events_into_failures() {
        let fixture = concat!(
            "event: error\n",
            "data: {\"type\":\"error\",\"message\":\"rate limit exceeded\"}\n\n"
        );

        let error = fixture_events([fixture.as_bytes().to_vec()])
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            ProviderError::Response(message) if message == "rate limit exceeded"
        ));
    }

    #[tokio::test]
    async fn rejects_malformed_known_events() {
        let fixture = "event: response.output_text.delta\ndata: {}\n\n";

        let error = fixture_events([fixture.as_bytes().to_vec()])
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            ProviderError::MalformedStreamEvent { kind, .. }
                if kind == "response.output_text.delta"
        ));
    }

    #[test]
    fn dropping_the_event_stream_drops_its_transport() {
        let dropped = Arc::new(AtomicBool::new(false));
        let events = decode_stream(PendingTransport {
            dropped: Arc::clone(&dropped),
        });

        assert!(!dropped.load(Ordering::SeqCst));
        drop(events);
        assert!(dropped.load(Ordering::SeqCst));
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
                "store": false,
                "stream": true
            })
        );
    }

    #[test]
    fn builds_a_strict_read_file_tool_for_stateless_continuation() {
        let client = OpenAiClient::new(
            "https://example.test/v1",
            "test-secret".to_owned(),
            "test-model",
        );
        let tools = available_tools();
        let input = ToolInput::from_messages(&[Message::user("Inspect Cargo.toml")]).unwrap();
        let request = client
            .build_json_request(
                "Be helpful",
                input.items(),
                &tools,
                Some(&STATELESS_REASONING_INCLUDE),
            )
            .unwrap();
        let body = serde_json::from_slice::<serde_json::Value>(
            request.body().and_then(reqwest::Body::as_bytes).unwrap(),
        )
        .unwrap();

        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert_eq!(body["tools"][0]["strict"], true);
        assert_eq!(body["tools"][0]["parameters"]["required"], json!(["path"]));
        assert_eq!(
            body["tools"][0]["parameters"]["additionalProperties"],
            false
        );
        assert_eq!(body["tools"][1]["type"], "function");
        assert_eq!(body["tools"][1]["name"], "list_files");
        assert_eq!(body["tools"][1]["strict"], true);
        assert_eq!(body["tools"][1]["parameters"]["required"], json!(["path"]));
        assert_eq!(body["tools"][3]["type"], "function");
        assert_eq!(body["tools"][3]["name"], "search_files");
        assert_eq!(body["tools"][3]["strict"], true);
        assert_eq!(
            body["tools"][3]["parameters"]["required"],
            json!(["query", "path"])
        );
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    }

    #[test]
    fn replays_raw_output_items_before_function_outputs() {
        let mut input = ToolInput::from_messages(&[Message::user("Inspect it")]).unwrap();
        let reasoning = json!({
            "type": "reasoning",
            "id": "rs_1",
            "encrypted_content": "opaque"
        });
        let call = json!({
            "type": "function_call",
            "call_id": "call_1",
            "name": "read_file",
            "arguments": "{\"path\":\"Cargo.toml\"}"
        });
        input.append_output_items(vec![reasoning.clone(), call.clone()]);
        input.append_function_output("call_1", "{\"ok\":true}".to_owned());

        assert_eq!(
            input.items(),
            json!([
                {"role": "user", "content": "Inspect it"},
                reasoning,
                call,
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "{\"ok\":true}"
                }
            ])
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

    async fn fixture_events(
        chunks: impl IntoIterator<Item = Vec<u8>>,
    ) -> Result<Vec<StreamEvent>, ProviderError> {
        let chunks: Vec<_> = chunks.into_iter().collect();
        decode_stream(stream::iter(chunks.into_iter().map(Ok::<_, io::Error>)))
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect()
    }

    struct PendingTransport {
        dropped: Arc<AtomicBool>,
    }

    impl Stream for PendingTransport {
        type Item = Result<Vec<u8>, io::Error>;

        fn poll_next(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Pending
        }
    }

    impl Drop for PendingTransport {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }
}
