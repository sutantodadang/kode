use std::collections::VecDeque;
use std::sync::Mutex;

use crate::error::{ModelError, Result};
use crate::types::{ModelCapabilities, ModelRequest, StreamEvent};
use crate::{Model, ModelStream};

#[derive(Default)]
pub struct MockModel {
    scripts: Mutex<VecDeque<Vec<StreamEvent>>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl MockModel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_script(&self, events: Vec<StreamEvent>) -> &Self {
        self.scripts.lock().unwrap().push_back(events);
        self
    }

    pub fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Model for MockModel {
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream> {
        self.requests.lock().unwrap().push(request);
        let events = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ModelError::Api {
                status: 0,
                message: "mock: no script".to_string(),
            })?;
        Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            id: "mock".to_string(),
            supports_tools: true,
            supports_streaming: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::collect_response;
    use crate::types::{FinishReason, Message};

    #[tokio::test]
    async fn scripted_stream_collects_and_records_request() {
        let mock = MockModel::new();
        mock.push_script(vec![
            StreamEvent::TextDelta("hi".to_string()),
            StreamEvent::Finished {
                reason: FinishReason::Stop,
                usage: None,
            },
        ]);

        let request = ModelRequest {
            messages: vec![Message::User("hello".to_string())],
            ..Default::default()
        };

        let model: &dyn Model = &mock;
        let stream = model.stream(request.clone()).await.unwrap();
        let resp = collect_response(stream).await.unwrap();

        assert_eq!(resp.content, "hi");
        assert_eq!(mock.requests(), vec![request]);
    }

    #[tokio::test]
    async fn no_script_returns_api_error() {
        let mock = MockModel::new();
        let err = match mock.stream(ModelRequest::default()).await {
            Ok(_) => panic!("expected error"),
            Err(e) => e,
        };
        assert!(matches!(err, ModelError::Api { status: 0, .. }));
    }
}
