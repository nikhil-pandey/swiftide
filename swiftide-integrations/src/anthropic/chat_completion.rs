use anyhow::{Context as _, Result};
use async_anthropic::types::{
    CreateMessagesRequestBuilder, Message, MessageBuilder, MessageContent, MessageContentList,
    MessageRole, ToolChoice, ToolResultBuilder, ToolUseBuilder,
};
use async_trait::async_trait;
use serde_json::json;
use swiftide_core::{
    chat_completion::{
        errors::ChatCompletionError, ChatCompletionRequest, ChatCompletionResponse, ChatMessage,
        ToolCall, ToolSpec,
    },
    ChatCompletion,
};

use super::Anthropic;

#[async_trait]
impl ChatCompletion for Anthropic {
    #[tracing::instrument(skip_all, err)]
    async fn complete(
        &self,
        request: &ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, ChatCompletionError> {
        let model = &self.default_options.prompt_model;

        let messages = request
            .messages()
            .iter()
            .map(message_to_antropic)
            .collect::<Result<Vec<_>>>()?;

        let mut anthropic_request = CreateMessagesRequestBuilder::default()
            .model(model)
            .messages(messages)
            .to_owned();

        if !request.tools_spec.is_empty() {
            anthropic_request
                .tools(
                    request
                        .tools_spec()
                        .iter()
                        .map(tools_to_anthropic)
                        .collect::<Result<Vec<_>>>()?,
                )
                .tool_choice(ToolChoice::Auto);
        }

        let request = anthropic_request
            .build()
            .map_err(|e| ChatCompletionError::LLM(Box::new(e)))?;

        tracing::debug!(
            model = &model,
            messages = serde_json::to_string_pretty(&request).expect("Infallible"),
            "[ChatCompletion] Request to anthropic"
        );

        let response = self
            .client
            .messages()
            .create(request)
            .await
            .map_err(|e| ChatCompletionError::LLM(Box::new(e)))?;

        tracing::debug!(
            response = serde_json::to_string_pretty(&response).expect("Infallible"),
            "[ChatCompletion] Response from anthropic"
        );

        let maybe_tool_calls = response
            .messages()
            .iter()
            .flat_map(Message::tool_uses)
            .map(|atool| {
                ToolCall::builder()
                    .id(atool.id)
                    .name(atool.name)
                    .args(atool.input.to_string())
                    .build()
                    .expect("infallible")
            })
            .collect::<Vec<_>>();
        let maybe_tool_calls = if maybe_tool_calls.is_empty() {
            None
        } else {
            Some(maybe_tool_calls)
        };

        ChatCompletionResponse::builder()
            .maybe_message(response.messages().iter().find_map(Message::text))
            .maybe_tool_calls(maybe_tool_calls)
            .build()
            .map_err(ChatCompletionError::from)
    }
}

#[allow(clippy::items_after_statements)]
fn message_to_antropic(message: &ChatMessage) -> Result<Message> {
    let mut builder = MessageBuilder::default().role(MessageRole::User).to_owned();

    use ChatMessage::{Assistant, Summary, System, ToolOutput, User};

    match message {
        ToolOutput(tool_call, tool_output) => builder.content(
            ToolResultBuilder::default()
                .tool_use_id(tool_call.id())
                .content(tool_output.content().unwrap_or("Success"))
                .build()?,
        ),
        Summary(msg) | System(msg) | User(msg) => builder.content(msg),
        Assistant(msg, tool_calls) => {
            builder.role(MessageRole::Assistant);

            let mut content_list: Vec<MessageContent> = Vec::new();

            if let Some(msg) = msg {
                content_list.push(msg.into());
            }

            if let Some(tool_calls) = tool_calls {
                for tool_call in tool_calls {
                    let tool_call = ToolUseBuilder::default()
                        .id(tool_call.id())
                        .name(tool_call.name())
                        .input(tool_call.args())
                        .build()?;

                    content_list.push(tool_call.into());
                }
            }

            let content_list = MessageContentList(content_list);

            builder.content(content_list)
        }
    };

    builder.build().context("Failed to build message")
}

fn tools_to_anthropic(
    spec: &ToolSpec,
) -> Result<serde_json::value::Map<String, serde_json::Value>> {
    let properties = spec
        .parameters
        .iter()
        .map(|param| {
            let map = json!({
                param.name: {
                    "type": "string",
                    "description": param.description,
                }
            })
            .as_object()
            .context("Failed to build tool")?
            .to_owned();

            Ok(map)
        })
        .collect::<Result<Vec<_>>>()?;
    let map = json!({
        "name": spec.name,
        "description": spec.description,
        "input_schema": {
            "type": "object",
            "properties": properties,
        },
        "required": spec.parameters.iter().filter(|param| param.required).map(|param| param.name).collect::<Vec<_>>(),
    })
    .as_object_mut()
    .context("Failed to build tool")?
    .to_owned();

    Ok(map)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use swiftide_core::{
        chat_completion::{ChatCompletionRequest, ChatMessage, ParamSpec},
        AgentContext, Tool,
    };
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    #[derive(Clone)]
    struct FakeTool();

    #[async_trait]
    impl Tool for FakeTool {
        async fn invoke(
            &self,
            _agent_context: &dyn AgentContext,
            _raw_args: Option<&str>,
        ) -> std::result::Result<
            swiftide_core::chat_completion::ToolOutput,
            swiftide_core::chat_completion::errors::ToolError,
        > {
            todo!()
        }

        fn name(&self) -> &'static str {
            "get_weather"
        }

        fn tool_spec(&self) -> ToolSpec {
            ToolSpec::builder()
                .description("Gets the weather")
                .name("get_weather")
                .parameters(vec![ParamSpec::builder()
                    .description("Location")
                    .name("location")
                    .required(true)
                    .build()
                    .unwrap()])
                .build()
                .unwrap()
        }
    }

    #[test_log::test(tokio::test)]
    async fn test_complete_without_tools() {
        // Start a wiremock server
        let mock_server = MockServer::start().await;

        // Create a mock response
        let mock_response = ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "content": [{"type": "text", "text": "mocked response"}]
        }));

        // Mock the expected endpoint
        Mock::given(method("POST"))
            .and(path("/v1/messages")) // Adjust path to match expected endpoint
            .respond_with(mock_response)
            .mount(&mock_server)
            .await;

        let client = async_anthropic::Client::builder()
            .base_url(mock_server.uri())
            .build()
            .unwrap();

        // Build an Anthropic client with the mock server's URL
        let mut client_builder = Anthropic::builder();
        client_builder.client(client);
        let client = client_builder.build().unwrap();

        // Prepare a sample request
        let request = ChatCompletionRequest::builder()
            .messages(vec![ChatMessage::User("hello".into())])
            .build()
            .unwrap();

        // Call the complete method
        let result = client.complete(&request).await.unwrap();

        // Assert the result
        assert_eq!(result.message, Some("mocked response".into()));
        assert!(result.tool_calls.is_none());
    }

    #[test_log::test(tokio::test)]
    async fn test_complete_with_tools() {
        // Start a wiremock server
        let mock_server = MockServer::start().await;

        // Create a mock response
        let mock_response = ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "msg_016zKNb88WhhgBQXhSaQf1rs",
            "content": [
            {
                "type": "text",
                "text": "I'll check the current weather in San Francisco, CA for you."
            },
            {
                "type": "tool_use",
                "id": "toolu_01E1yxpxXU4hBgCMLzPL1FuR",
                "input": {
                "location": "San Francisco, CA"
                },
                "name": "get_weather"
            }
            ],
            "model": "claude-3-5-sonnet-20241022",
            "stop_reason": "tool_use",
            "stop_sequence": null,
            "usage": {
            "input_tokens": 403,
            "output_tokens": 71
            }
        }));

        // Mock the expected endpoint
        Mock::given(method("POST"))
            .and(path("/v1/messages")) // Adjust path to match expected endpoint
            .respond_with(mock_response)
            .mount(&mock_server)
            .await;

        let client = async_anthropic::Client::builder()
            .base_url(mock_server.uri())
            .build()
            .unwrap();

        // Build an Anthropic client with the mock server's URL
        let mut client_builder = Anthropic::builder();
        client_builder.client(client);
        let client = client_builder.build().unwrap();

        // Prepare a sample request
        let request = ChatCompletionRequest::builder()
            .messages(vec![ChatMessage::User("hello".into())])
            .tools_spec(HashSet::from([FakeTool().tool_spec()]))
            .build()
            .unwrap();

        // Call the complete method
        let result = client.complete(&request).await.unwrap();

        // Assert the result
        assert_eq!(
            result.message,
            Some("I'll check the current weather in San Francisco, CA for you.".into())
        );
        assert!(result.tool_calls.is_some());

        let Some(tool_call) = result.tool_calls.and_then(|f| f.first().cloned()) else {
            panic!("No tool call found")
        };
        assert_eq!(tool_call.name(), "get_weather");
        assert_eq!(
            tool_call.args(),
            Some(
                json!({"location": "San Francisco, CA"})
                    .to_string()
                    .as_str()
            )
        );
    }
}
