use anyhow::{bail, Context, Result};
use axum::{
    response::{Html, IntoResponse},
    routing::post,
    Extension, Form, Router,
};
use dotenv::dotenv;
use enum_iterator::all;
use log::*;
use openapi::apis::{
    api20100401_message_api::{create_message, CreateMessageParams},
    configuration::Configuration,
};
use sqlx::{query, query_as, Pool, Sqlite};
use std::env;

use crate::{
    command::Command,
    friends::{accept, friend},
};

mod command;
mod friends;

#[tokio::main]
async fn main() -> Result<()> {
    dotenv()?;
    env_logger::init();
    info!("Starting up");
    let twilio_config = Configuration {
        basic_auth: Some((
            env::var("TWILIO_API_KEY_SID")?,
            Some(env::var("TWILIO_API_KEY_SECRET")?),
        )),
        ..Default::default()
    };
    send(
        &twilio_config,
        env::var("CLIENT_NUMBER")?,
        "Server is starting up".to_string(),
    )
    .await?;
    let pool = sqlx::SqlitePool::connect(&env::var("DATABASE_URL")?).await?;
    let app = Router::new()
        .route("/", post(handle_incoming_sms))
        .layer(Extension(pool))
        .layer(Extension(twilio_config));
    let listener = tokio::net::TcpListener::bind(format!(
        "{}:{}",
        env::var("CALLBACK_IP")?,
        env::var("CALLBACK_PORT")?
    ))
    .await?;
    info!("Listening on {}", listener.local_addr()?);
    axum::serve(listener, app).await?;

    Ok(())
}

// field names must be exact (including case) to match API
#[allow(non_snake_case)]
#[derive(serde::Deserialize)]
struct SmsMessage {
    Body: String,
    From: String,
}

struct User {
    #[allow(dead_code)]
    number: String,
    name: String,
}

struct RowId {
    rowid: i64,
}

// Handler for incoming SMS messages
async fn handle_incoming_sms(
    Extension(pool): Extension<Pool<Sqlite>>,
    Extension(twilio_config): Extension<Configuration>,
    Form(message): Form<SmsMessage>,
) -> impl IntoResponse {
    let response = match process_message(&pool, Some(&twilio_config), message).await {
        Ok(response) => response,
        Err(error) => {
            error!("Error: {error:?}");
            "Internal Server Error!".to_string()
        }
    };
    debug!("Sending response: {response}");
    Html(format!(
        r#"
        <?xml version="1.0" encoding="UTF-8"?>
        <Response>
        <Message>{response}</Message>
        </Response>
        "#
    ))
}

async fn process_message(
    pool: &Pool<Sqlite>,
    twilio_config: Option<&Configuration>,
    message: SmsMessage,
) -> anyhow::Result<String> {
    let SmsMessage {
        Body: body,
        From: from,
    } = message;
    debug!("Received from {from}: {body}");

    let mut words = body.trim().split_ascii_whitespace();
    let command_word = words.next();
    let command = command_word.map(|word| Command::try_from(word));

    let Some(User { name, .. }) = query_as!(User, "select * from users where number = ?", from)
        .fetch_optional(pool)
        .await?
    else {
        return onboard_new_user(command, words, &from, pool).await;
    };

    let Some(command) = command else {
        return Ok(Command::h.hint());
    };

    let Ok(command) = command else {
        return Ok(format!(
            "We didn't recognize that command word: '{}'.\n{}",
            command_word.unwrap(),
            Command::h.hint()
        ));
    };

    let response = match command {
        // I would use HELP for the help command, but Twilio intercepts and does not relay that
        Command::h => {
            let available_commands = format!(
                "Available commands:\n{}\n",
                all::<Command>()
                    .map(|c| format!("- {c}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            format!("{available_commands}\n{}", Command::info.hint())
        }
        Command::name => match process_name(words) {
            Ok(name) => {
                query!("update users set name = ? where number = ?", name, from)
                    .execute(pool)
                    .await?;
                format!("Your name has been updated to '{name}'")
            }
            Err(hint) => hint.to_string(),
        },
        Command::stop => {
            query!("delete from users where number = ?", from)
                .execute(pool)
                .await?;
            // They won't actually see this when using Twilio
            "You've been unsubscribed. Goodbye!".to_string()
        }
        Command::info => {
            if let Some(command_word) = words.next() {
                if let Ok(command) = Command::try_from(command_word) {
                    format!(
                        "{} to {}.{}",
                        command.usage(),
                        command.description(),
                        command.example()
                    )
                } else {
                    format!("Command '{command_word}' not recognized")
                }
            } else {
                command.hint()
            }
        }
        Command::friend => {
            if let Some(friend_number) = words.next() {
                query!("begin transaction").execute(pool).await?;
                let response = friend(pool, twilio_config, &name, &from, friend_number).await?;
                query!("commit").execute(pool).await?;
                response
            } else {
                command.hint()
            }
        }
        Command::unfriend => {
            if let Some(friend_index) = words.next() {
                if let Err(error) = query_as!(
                    RowId,
                    "delete from friend_requests where rowid = ?",
                    friend_index
                )
                .execute(pool)
                .await
                {
                    error!("{error}");
                    "Failed to remove friend!"
                } else {
                    "Successfully removed friend"
                }
                .to_string()
            } else {
                command.hint()
            }
        }
        Command::accept => {
            if let Some(request_index) = words.next().and_then(|x| x.parse().ok()) {
                query!("begin transaction").execute(pool).await?;
                let response = accept(pool, twilio_config, &name, &from, request_index).await?;
                query!("commit").execute(pool).await?;
                response
            } else {
                command.hint()
            }
        }
        // Command::reject => {}
        // Command::block => {}
        // Command::requests => {}
        _ => "".to_string(),
    };
    Ok(response.replace("'", "\""))
}

async fn onboard_new_user(
    command: Option<Result<Command, serde_json::Error>>,
    words: impl Iterator<Item = &str>,
    from: &str,
    pool: &Pool<Sqlite>,
) -> anyhow::Result<String> {
    let Some(Ok(Command::name)) = command else {
        return Ok(format!(
            "Welcome to Sam Carey's experimental social server!\nTo participate:\n{}",
            Command::name.hint()
        ));
    };
    Ok(match process_name(words) {
        Ok(name) => {
            query!("insert into users (number, name) values (?, ?)", from, name)
                .execute(pool)
                .await?;
            format!("Hello, {name}! {}", Command::h.hint())
        }
        Err(hint) => hint.to_string(),
    })
}

fn process_name<'a>(words: impl Iterator<Item = &'a str>) -> Result<String> {
    let name = words.collect::<Vec<_>>().join(" ");
    if name.is_empty() {
        bail!("{}", Command::name.usage());
    }
    const MAX_NAME_LEN: usize = 20;
    if name.len() > MAX_NAME_LEN {
        bail!(
            "That name is {} characters long.\n\
            Please shorten it to {MAX_NAME_LEN} characters or less.",
            name.len()
        );
    }
    Ok(name)
}

async fn send(twilio_config: &Configuration, to: String, message: String) -> Result<()> {
    let message_params = CreateMessageParams {
        account_sid: env::var("TWILIO_ACCOUNT_SID")?,
        to,
        from: Some(env::var("SERVER_NUMBER")?),
        body: Some(message),
        ..Default::default()
    };
    let message = create_message(twilio_config, message_params)
        .await
        .context("While sending message")?;
    trace!("Message sent with SID {}", message.sid.unwrap().unwrap());
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use futures::executor::block_on;

    fn create_fixture(pool: &Pool<Sqlite>, number: &str) -> impl Fn(&str) {
        let number = number.to_string();
        let pool = pool.clone();
        move |message: &str| {
            println!("{number} >>> '{message}'");
            let response = block_on(process_message(
                &pool,
                None,
                SmsMessage {
                    From: number.to_string(),
                    Body: message.to_string(),
                },
            ))
            .unwrap();
            println!("{response}\n\n");
        }
    }

    fn one_sided(pool: Pool<Sqlite>, input: &[&str]) {
        let fixture = create_fixture(&pool, "TEST_NUMBER");
        for i in input {
            fixture(i);
        }
    }

    #[sqlx::test]
    async fn basic(pool: Pool<Sqlite>) {
        one_sided(
            pool,
            &[
                "hi",
                "name Sam C.",
                "h",
                "info name",
                "info stop",
                "info  ",
                "info x",
                "info info",
                "info name x",
                "yo",
                "stop",
                "yo",
            ],
        );
    }

    #[sqlx::test]
    async fn manage_friends(pool: Pool<Sqlite>) {
        let a = create_fixture(&pool, "A");
        let b = create_fixture(&pool, "B");
        a("name Sam C.");
        a("friend B");
        b("accept");
        b("accept 1");
    }
}
