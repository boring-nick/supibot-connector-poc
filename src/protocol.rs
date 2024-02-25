use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
pub struct IncomingMessage {
    pub platform: String,
    pub instance: Option<String>,
    #[serde(flatten)]
    pub value: IncomingMessageValue,
}

// Because of the tag attribute, this gets serialized into an object with `type` and `data` fields,
// where `type` is the variant name and `data` is the contents.
#[derive(Debug, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum IncomingMessageValue {
    Message(IncomingChatMessage),
}

#[derive(Debug, Serialize)]
pub struct IncomingChatMessage {
    pub private: bool,
    pub channel: Option<Channel>,
    pub user: User,
    pub message: String,
    pub timestamp: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Channel {
    pub name: String,
    pub id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct User {
    pub name: String,
    pub id: String,
}

// Platform and instance are not specified here as they're part of the redis stream key
#[derive(Debug, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum OutgoingMessage {
    Message(OutgoingChatMessage),
    ChannelJoin(ChannelJoinMessage),
}

#[derive(Debug, Deserialize)]
pub struct OutgoingChatMessage {
    pub channel: Option<Channel>,
    pub user: Option<User>,
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub struct ChannelJoinMessage {
    pub channel: Channel,
}
