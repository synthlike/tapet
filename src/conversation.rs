use crate::message::Message;
use std::future::Future;

#[derive(Debug, Default)]
pub struct Conversation {
    messages: Vec<Message>,
}

impl Conversation {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub async fn turn<E, F, Fut>(&mut self, user_message: String, request: F) -> Result<&str, E>
    where
        F: FnOnce(Vec<Message>) -> Fut,
        Fut: Future<Output = Result<String, E>>,
    {
        self.messages.push(Message::user(user_message));
        let assistant_message = request(self.messages.clone()).await?;
        self.messages.push(Message::assistant(assistant_message));

        Ok(self
            .messages
            .last()
            .expect("the assistant message was just appended")
            .content())
    }
}

#[cfg(test)]
mod tests {
    use super::Conversation;
    use crate::message::{Message, MessageRole};
    use std::convert::Infallible;
    use std::future::ready;

    #[tokio::test]
    async fn the_second_request_contains_the_first_completed_turn() {
        let mut conversation = Conversation::new();

        conversation
            .turn("What is ownership?".to_owned(), |messages| {
                assert_eq!(messages, [Message::user("What is ownership?")]);
                ready(Ok::<_, Infallible>("Ownership controls values.".to_owned()))
            })
            .await
            .unwrap();

        conversation
            .turn("How does borrowing relate?".to_owned(), |messages| {
                assert_eq!(
                    messages,
                    [
                        Message::user("What is ownership?"),
                        Message::assistant("Ownership controls values."),
                        Message::user("How does borrowing relate?")
                    ]
                );
                ready(Ok::<_, Infallible>(
                    "Borrowing grants temporary access.".to_owned(),
                ))
            })
            .await
            .unwrap();

        assert_eq!(conversation.messages().len(), 4);
    }

    #[tokio::test]
    async fn a_failed_response_does_not_append_an_assistant_message() {
        let mut conversation = Conversation::new();

        let result = conversation
            .turn("Will this fail?".to_owned(), |_| {
                ready(Err::<String, _>("provider failed"))
            })
            .await;

        assert_eq!(result.unwrap_err(), "provider failed");
        assert_eq!(conversation.messages().len(), 1);
        assert_eq!(conversation.messages()[0].role(), MessageRole::User);
        assert_eq!(conversation.messages()[0].content(), "Will this fail?");
    }
}
