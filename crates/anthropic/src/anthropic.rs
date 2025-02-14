use anyhow::{anyhow, Result};
use futures::{io::BufReader, stream::BoxStream, AsyncBufReadExt, AsyncReadExt, StreamExt};
use http::{AsyncBody, HttpClient, Method, Request as HttpRequest};
use isahc::config::Configurable;
use serde::{Deserialize, Serialize};
use std::{convert::TryFrom, time::Duration};
use strum::EnumIter;

pub const ANTHROPIC_API_URL: &'static str = "https://api.anthropic.com";

#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, EnumIter)]
pub enum Model {
    #[default]
    #[serde(alias = "claude-3-5-sonnet", rename = "claude-3-5-sonnet-20240620")]
    Claude3_5Sonnet,
    #[serde(alias = "claude-3-opus", rename = "claude-3-opus-20240229")]
    Claude3Opus,
    #[serde(alias = "claude-3-sonnet", rename = "claude-3-sonnet-20240229")]
    Claude3Sonnet,
    #[serde(alias = "claude-3-haiku", rename = "claude-3-haiku-20240307")]
    Claude3Haiku,
    #[serde(rename = "custom")]
    Custom {
        name: String,
        #[serde(default)]
        max_tokens: Option<usize>,
    },
}

impl Model {
    pub fn from_id(id: &str) -> Result<Self> {
        if id.starts_with("claude-3-5-sonnet") {
            Ok(Self::Claude3_5Sonnet)
        } else if id.starts_with("claude-3-opus") {
            Ok(Self::Claude3Opus)
        } else if id.starts_with("claude-3-sonnet") {
            Ok(Self::Claude3Sonnet)
        } else if id.starts_with("claude-3-haiku") {
            Ok(Self::Claude3Haiku)
        } else {
            Ok(Self::Custom {
                name: id.to_string(),
                max_tokens: None,
            })
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Model::Claude3_5Sonnet => "claude-3-5-sonnet-20240620",
            Model::Claude3Opus => "claude-3-opus-20240229",
            Model::Claude3Sonnet => "claude-3-sonnet-20240229",
            Model::Claude3Haiku => "claude-3-opus-20240307",
            Model::Custom { name, .. } => name,
        }
    }

    pub fn display_name(&self) -> &str {
        match self {
            Self::Claude3_5Sonnet => "Claude 3.5 Sonnet",
            Self::Claude3Opus => "Claude 3 Opus",
            Self::Claude3Sonnet => "Claude 3 Sonnet",
            Self::Claude3Haiku => "Claude 3 Haiku",
            Self::Custom { name, .. } => name,
        }
    }

    pub fn max_token_count(&self) -> usize {
        match self {
            Self::Claude3_5Sonnet
            | Self::Claude3Opus
            | Self::Claude3Sonnet
            | Self::Claude3Haiku => 200_000,
            Self::Custom { max_tokens, .. } => max_tokens.unwrap_or(200_000),
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize, Debug, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

impl TryFrom<String> for Role {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self> {
        match value.as_str() {
            "user" => Ok(Self::User),
            "assistant" => Ok(Self::Assistant),
            _ => Err(anyhow!("invalid role '{value}'")),
        }
    }
}

impl From<Role> for String {
    fn from(val: Role) -> Self {
        match val {
            Role::User => "user".to_owned(),
            Role::Assistant => "assistant".to_owned(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Request {
    #[serde(serialize_with = "serialize_request_model")]
    pub model: Model,
    pub messages: Vec<RequestMessage>,
    pub stream: bool,
    pub system: String,
    pub max_tokens: u32,
}

fn serialize_request_model<S>(model: &Model, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&model.id())
}

#[derive(Serialize, Deserialize, Debug, Eq, PartialEq)]
pub struct RequestMessage {
    pub role: Role,
    pub content: String,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseEvent {
    MessageStart {
        message: ResponseMessage,
    },
    ContentBlockStart {
        index: u32,
        content_block: ContentBlock,
    },
    Ping {},
    ContentBlockDelta {
        index: u32,
        delta: TextDelta,
    },
    ContentBlockStop {
        index: u32,
    },
    MessageDelta {
        delta: ResponseMessage,
        usage: Usage,
    },
    MessageStop {},
}

#[derive(Deserialize, Debug)]
pub struct ResponseMessage {
    #[serde(rename = "type")]
    pub message_type: Option<String>,
    pub id: Option<String>,
    pub role: Option<String>,
    pub content: Option<Vec<String>>,
    pub model: Option<String>,
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
    pub usage: Option<Usage>,
}

#[derive(Deserialize, Debug)]
pub struct Usage {
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TextDelta {
    TextDelta { text: String },
}

pub async fn stream_completion(
    client: &dyn HttpClient,
    api_url: &str,
    api_key: &str,
    request: Request,
    low_speed_timeout: Option<Duration>,
) -> Result<BoxStream<'static, Result<ResponseEvent>>> {
    let uri = format!("{api_url}/v1/messages");
    let mut request_builder = HttpRequest::builder()
        .method(Method::POST)
        .uri(uri)
        .header("Anthropic-Version", "2023-06-01")
        .header("Anthropic-Beta", "tools-2024-04-04")
        .header("X-Api-Key", api_key)
        .header("Content-Type", "application/json");
    if let Some(low_speed_timeout) = low_speed_timeout {
        request_builder = request_builder.low_speed_timeout(100, low_speed_timeout);
    }
    let request = request_builder.body(AsyncBody::from(serde_json::to_string(&request)?))?;
    let mut response = client.send(request).await?;
    if response.status().is_success() {
        let reader = BufReader::new(response.into_body());
        Ok(reader
            .lines()
            .filter_map(|line| async move {
                match line {
                    Ok(line) => {
                        let line = line.strip_prefix("data: ")?;
                        match serde_json::from_str(line) {
                            Ok(response) => Some(Ok(response)),
                            Err(error) => Some(Err(anyhow!(error))),
                        }
                    }
                    Err(error) => Some(Err(anyhow!(error))),
                }
            })
            .boxed())
    } else {
        let mut body = Vec::new();
        response.body_mut().read_to_end(&mut body).await?;

        let body_str = std::str::from_utf8(&body)?;

        match serde_json::from_str::<ResponseEvent>(body_str) {
            Ok(_) => Err(anyhow!(
                "Unexpected success response while expecting an error: {}",
                body_str,
            )),
            Err(_) => Err(anyhow!(
                "Failed to connect to API: {} {}",
                response.status(),
                body_str,
            )),
        }
    }
}

// #[cfg(test)]
// mod tests {
//     use super::*;
//     use http::IsahcHttpClient;

//     #[tokio::test]
//     async fn stream_completion_success() {
//         let http_client = IsahcHttpClient::new().unwrap();

//         let request = Request {
//             model: Model::Claude3Opus,
//             messages: vec![RequestMessage {
//                 role: Role::User,
//                 content: "Ping".to_string(),
//             }],
//             stream: true,
//             system: "Respond to ping with pong".to_string(),
//             max_tokens: 4096,
//         };

//         let stream = stream_completion(
//             &http_client,
//             "https://api.anthropic.com",
//             &std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY not set"),
//             request,
//         )
//         .await
//         .unwrap();

//         stream
//             .for_each(|event| async {
//                 match event {
//                     Ok(event) => println!("{:?}", event),
//                     Err(e) => eprintln!("Error: {:?}", e),
//                 }
//             })
//             .await;
//     }
// }
