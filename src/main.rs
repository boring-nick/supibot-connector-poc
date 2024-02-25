mod config;
mod protocol;

use crate::{
    config::Config,
    protocol::{Channel, User},
};
use anyhow::{anyhow, Context};
use chrono::Utc;
use envconfig::Envconfig;
use futures::StreamExt;
use irc::{
    client::{data::Config as IrcConfig, Client as IrcClient},
    proto::{Command, Message as IrcMessage, Prefix},
};
use protocol::{IncomingChatMessage, IncomingMessage, IncomingMessageValue, OutgoingMessage};
use redis::{
    aio::ConnectionManager,
    streams::{StreamReadOptions, StreamReadReply},
    AsyncCommands, Value,
};
use tokio::{select, sync::mpsc};
use tracing::{debug, error, info};
use tracing_subscriber::EnvFilter;

const INCOMING_MESSAGES_KEY: &str = "messages:incoming";
const OUTGOING_MESSAGES_KEY_PREFIX: &str = "messages:outgoing";
const PLATFORM: &str = "irc";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Set up logging, configurable via the `RUST_LOG` env variable
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::init_from_env().context("Could not load config")?;

    // These should probably be in a similar style
    let channels_key = format!("irc:{}:channels", config.instance_name);
    let outgoing_keys = vec![format!(
        "{OUTGOING_MESSAGES_KEY_PREFIX}:irc:{}",
        config.instance_name
    )];

    info!("Using {outgoing_keys:?} as keys for outgoing messages");

    let redis_client = redis::Client::open(config.redis_url)?;

    let mut redis_conn = ConnectionManager::new(redis_client.clone())
        .await
        .context("Could not connect to redis")?;

    // Because the listener uses the `xread` command with the `block` argument,
    // listening to new messages blocks the entire redis connection.
    // To be able to use redis for other things while listening for messages,
    // there needs to be a dedicated connection for the listener.
    let listener_redis_conn = ConnectionManager::new(redis_client)
        .await
        .context("Could not connect to redis")?;

    // Fetch initial channels list from redis
    let channels: Vec<String> = redis_conn
        .smembers(&channels_key)
        .await
        .context("Could not get the channels list")?;
    info!("Joining {} channels", channels.len());

    let irc_config = IrcConfig {
        server: Some(config.irc_server),
        nickname: Some(config.irc_nickname),
        // Add '#' to the start of every channel for IRC
        channels: channels
            .into_iter()
            .map(|channel| format!("#{channel}"))
            .collect(),
        ..IrcConfig::default()
    };

    let mut irc_client = IrcClient::from_config(irc_config)
        .await
        .context("Could not create IRC client")?;
    irc_client.identify().context("Failed to identify")?;

    let mut irc_messages_stream = irc_client.stream()?;

    // Listening to redis commands is done in a separate task using the dedicated connection,
    // and then sent over a channel to the main one
    let (outgoing_messages_tx, mut outgoing_messages_rx) = mpsc::channel(100);
    tokio::spawn(listen_redis_messages(
        listener_redis_conn,
        outgoing_keys,
        outgoing_messages_tx,
    ));

    info!("Listening to messages");

    // In a loop, listen to either a new irc message, or a command sent over redis, whichever one comes first
    loop {
        select! {
            irc_message = irc_messages_stream.next() => {
                match irc_message {
                    Some(Ok(message)) => {
                        debug!("Received message {message:?}");
                        handle_irc_message(message, &mut redis_conn, &config.instance_name).await;
                    }
                    Some(Err(err)) => {
                        error!("IRC error: {err}");
                    }
                    // Quit if the IRC stream ended for some reason
                    None => break,
                }
            }
            Some(outgoing_message) = outgoing_messages_rx.recv() => {
                if let Err(err) = handle_outgoing_message(outgoing_message, &mut irc_client).await {
                    error!("Could not handle outgoing message: {err:#}");
                }
            }
        };
    }

    Ok(())
}

async fn handle_irc_message(
    message: IrcMessage,
    redis_conn: &mut ConnectionManager,
    instance: &str,
) {
    // We only care about messages which have a nickname in the prefix
    if let Some(Prefix::Nickname(nickname, _, _)) = message.prefix {
        if let Command::PRIVMSG(channel, text) = message.command {
            let channel_name = channel[1..].to_owned(); // Remove the leading #

            let chat_message = IncomingChatMessage {
                private: false,
                channel: Some(Channel {
                    name: channel_name.clone(),
                    id: channel_name,
                }),
                user: User {
                    name: nickname.clone(),
                    id: nickname,
                },
                message: text,
                timestamp: Utc::now().timestamp_millis(), // IRC does not provide a timestamp, so it is generated client-side
            };
            let incoming_message = IncomingMessage {
                platform: PLATFORM.to_owned(),
                instance: Some(instance.to_owned()),
                value: IncomingMessageValue::Message(chat_message),
            };

            // Maybe we could use the fact that the stream items are already maps, instead of serializing everything into a json?
            let data = serde_json::to_string(&incoming_message).unwrap();
            let items = [("data", data)];

            match redis_conn.xadd(INCOMING_MESSAGES_KEY, "*", &items).await {
                Ok(()) => (),
                Err(err) => {
                    error!("Could not publish message to redis: {err}");
                }
            }
        }
    }
}

async fn listen_redis_messages(
    mut redis_conn: ConnectionManager,
    outgoing_keys: Vec<String>,
    outgoing_tx: mpsc::Sender<OutgoingMessage>,
) {
    loop {
        let mut last_command_ids = ["$".to_owned()];
        let xread_opts = StreamReadOptions::default().block(0);

        match redis_conn
            .xread_options(&outgoing_keys, &last_command_ids, &xread_opts)
            .await
        {
            Ok(reply) => {
                if let Err(err) =
                    handle_redis_message(reply, &mut last_command_ids, &outgoing_tx).await
                {
                    error!("Could not handle redis message: {err:#}");
                }
            }

            Err(_) => todo!(),
        }
    }
}

async fn handle_redis_message(
    message: StreamReadReply,
    last_command_ids: &mut [String],
    outgoing_tx: &mpsc::Sender<OutgoingMessage>,
) -> anyhow::Result<()> {
    for (i, key) in message.keys.into_iter().enumerate() {
        for message in key.ids {
            // The id gets saved before all of the processing,
            // if there is an error later on, the id is still saved, so the failed messages are skipped on the next read command.
            // This can be moved to the end of the loop body to change this behaviour
            // (Note: would also need to handle the case with the first message when the id is $)
            last_command_ids[i] = message.id;

            // Same note here as in `handle_irc_messsage` - maybe the fact that this is a map could be used instead of nesting json in it
            let value = message
                .map
                .get("data")
                .context("Missing 'data' field in redis command")?;

            match value {
                Value::Data(data) => {
                    let message: OutgoingMessage =
                        serde_json::from_slice(data).context("Could not deserialize command")?;
                    outgoing_tx.send(message).await?;
                }
                other => {
                    return Err(anyhow!(
                        "Expected the command data to be binary data, got {other:?} instead"
                    ))
                }
            }
        }
    }

    Ok(())
}

async fn handle_outgoing_message(
    message: OutgoingMessage,
    irc_client: &mut IrcClient,
) -> anyhow::Result<()> {
    match message {
        OutgoingMessage::Message(chat_message) => {
            if let Some(channel) = chat_message.channel {
                let target = format!("#{}", channel.name);
                debug!("Sending message to {target}");
                irc_client.send_privmsg(target, chat_message.message)?;
            } else if let Some(_user) = chat_message.user {
                // Handle direct messages
            }
        }
        OutgoingMessage::ChannelJoin(join) => {
            info!("Joining channel {}", join.channel.name);
            irc_client.send_join(format!("#{}", join.channel.name))?;
        }
    }
    Ok(())
}
