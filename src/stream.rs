#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StreamEvent {
    TextDelta(String),
    Completed(Completion),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Completion {
    pub provider_response_id: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
}
