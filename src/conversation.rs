use crate::message::Message;
use crate::session::Session;
use crate::store::{Store, StoreError};
use crate::stream::Completion;

pub struct Conversation {
    store: Store,
    session: Session,
}

impl Conversation {
    pub fn new(store: Store, session: Session) -> Self {
        Self { store, session }
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub async fn begin_turn(&self, user_message: String) -> Result<RunningTurn, StoreError> {
        let call = self
            .store
            .begin_call(self.session.id(), user_message)
            .await?;
        Ok(RunningTurn {
            store: self.store.clone(),
            call_id: call.call_id,
            messages: call.messages,
        })
    }
}

pub struct RunningTurn {
    store: Store,
    call_id: i64,
    messages: Vec<Message>,
}

impl RunningTurn {
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub async fn complete(
        self,
        assistant_message: String,
        completion: Completion,
    ) -> Result<(), StoreError> {
        self.store
            .complete_call(self.call_id, assistant_message, completion)
            .await
    }

    pub async fn fail(self, error: String) -> Result<(), StoreError> {
        self.store.fail_call(self.call_id, error).await
    }
}

#[cfg(test)]
mod tests {
    use super::Conversation;
    use crate::message::Message;
    use crate::session::AgentSnapshot;
    use crate::store::Store;
    use crate::stream::Completion;
    use tempfile::TempDir;

    #[tokio::test]
    async fn a_completed_turn_becomes_part_of_the_next_request() {
        let temporary = TempDir::new().unwrap();
        let store = Store::open(temporary.path().join("tapet.db"))
            .await
            .unwrap();
        let session = store
            .create_session(AgentSnapshot::fixture("Prompt"))
            .await
            .unwrap();
        let conversation = Conversation::new(store, session);

        let first = conversation.begin_turn("First".to_owned()).await.unwrap();
        assert_eq!(first.messages(), [Message::user("First")]);
        first
            .complete("Answer".to_owned(), completion())
            .await
            .unwrap();

        let second = conversation.begin_turn("Second".to_owned()).await.unwrap();
        assert_eq!(
            second.messages(),
            [
                Message::user("First"),
                Message::assistant("Answer"),
                Message::user("Second")
            ]
        );
        second.fail("test cleanup".to_owned()).await.unwrap();
    }

    fn completion() -> Completion {
        Completion {
            provider_response_id: Some("resp_test".to_owned()),
            input_tokens: 1,
            output_tokens: 1,
        }
    }
}
