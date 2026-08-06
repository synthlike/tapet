#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StreamEvent {
    TextDelta(String),
    ToolCallProposed(ToolCall),
    Completed(Completion),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Completion {
    pub provider_response_id: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
}
