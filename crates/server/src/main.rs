use crate::command::Command;
use anyhow::{bail, Context, Result};
use axum::{
    response::{Html, IntoResponse},
    routing::post,
    Extension, Form, Router,
};
use contacts::{add_contact, process_contact_submission};
use dotenv::dotenv;
use help::handle_help;
use log::*;
use openapi::apis::{
    api20100401_message_api::{create_message, CreateMessageParams},
    configuration::Configuration,
};
use sqlx::{query, query_as, Pool, Sqlite};
use std::env;
use std::str::FromStr;
use util::E164;

mod command;
mod contacts;
mod help;
#[cfg(test)]
mod test;
mod util;

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
    query!("PRAGMA foreign_keys = ON").execute(&pool).await?; // SQLite has this off by default
    let app = Router::new()
        .route("/sms", post(handle_incoming_sms))
        .layer(Extension(pool));
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
#[derive(serde::Deserialize, Default, Debug)]
struct SmsMessage {
    Body: String,
    From: String,
    NumMedia: Option<String>,
    MediaContentType0: Option<String>,
    MediaUrl0: Option<String>,
}

struct User {
    number: String,
    #[allow(dead_code)]
    name: String,
}

#[derive(Clone, sqlx::FromRow)]
struct Contact {
    id: i64,
    contact_name: String,
    contact_user_number: String,
}

#[derive(Debug)]
enum ImportResult {
    Added,
    Updated,
    Unchanged,
    Deferred,
}

// Handler for incoming SMS messages
async fn handle_incoming_sms(
    Extension(pool): Extension<Pool<Sqlite>>,
    Form(message): Form<SmsMessage>,
) -> impl IntoResponse {
    let response = match process_message(&pool, message).await {
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

async fn process_message(pool: &Pool<Sqlite>, message: SmsMessage) -> anyhow::Result<String> {
    trace!("Received {message:?}");
    let SmsMessage {
        Body: body,
        From: from,
        NumMedia: media_count,
        MediaContentType0: media_type_0,
        MediaUrl0: media_url_0,
    } = message;
    debug!("Received from {from}: {body}");
    if media_count == Some("1".to_string())
        && media_type_0
            .map(|t| ["text/vcard", "text/x-vcard"].contains(&t.as_str()))
            .unwrap_or(false)
    {
        return process_contact_submission(pool, &from, &media_url_0).await;
    }

    let mut words = body.trim().split_ascii_whitespace();
    let command_word = words.next();
    let command = command_word.map(Command::try_from);

    let Some(User {
        number, name: _, ..
    }) = query_as!(User, "select * from users where number = ?", from)
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
            "We didn't recognize that command word: \"{}\".\n{}",
            command_word.unwrap(),
            Command::h.hint()
        ));
    };

    let response = match command {
        // I would use HELP for the help command, but Twilio intercepts and does not relay that
        Command::h => handle_help(pool, &from).await?,
        Command::name => match process_name(words) {
            Ok(name) => {
                query!("update users set name = ? where number = ?", name, from)
                    .execute(pool)
                    .await?;
                format!("Your name has been updated to \"{name}\"")
            }
            Err(hint) => hint.to_string(),
        },
        Command::stop => {
            query!("delete from users where number = ?", number)
                .execute(pool)
                .await?;
            // They won't actually see this when using Twilio
            "You've been unsubscribed. Goodbye!".to_string()
        }
        Command::info => {
            let command_text = words.next();
            if let Some(command) = command_text.map(Command::try_from) {
                if let Ok(command) = command {
                    format!(
                        "{}, to {}.{}",
                        command.usage(),
                        command.description(),
                        command.example()
                    )
                } else {
                    format!("Command \"{}\" not recognized", command_text.unwrap())
                }
            } else {
                Command::info.hint()
            }
        }
        Command::contacts => {
            // First get the groups
            let groups = query!(
                "SELECT g.name, COUNT(gm.member_number) as member_count 
                 FROM groups g 
                 LEFT JOIN group_members gm ON g.id = gm.group_id
                 WHERE g.creator_number = ?
                 GROUP BY g.id, g.name
                 ORDER BY g.name",
                from
            )
            .fetch_all(pool)
            .await?;

            // Then get the contacts
            let contacts = query_as!(
                Contact,
                "SELECT id as \"id!\", contact_name, contact_user_number 
                 FROM contacts 
                 WHERE submitter_number = ? 
                 ORDER BY contact_name",
                from
            )
            .fetch_all(pool)
            .await?;

            if groups.is_empty() && contacts.is_empty() {
                "You don't have any groups or contacts.".to_string()
            } else {
                let mut response = String::new();

                // Add groups section if there are any
                if !groups.is_empty() {
                    response.push_str("Your groups:\n");
                    for (i, group) in groups.iter().enumerate() {
                        response.push_str(&format!(
                            "{}. {} ({} members)\n",
                            i + 1,
                            group.name,
                            group.member_count
                        ));
                    }
                }

                // Add contacts section if there are any
                if !contacts.is_empty() {
                    if !groups.is_empty() {
                        response.push_str("\n"); // Add spacing between sections
                    }
                    response.push_str("Your contacts:\n");
                    let offset = groups.len(); // Start contact numbering after groups
                    response.push_str(
                        &contacts
                            .iter()
                            .enumerate()
                            .map(|(i, c)| {
                                format!(
                                    "{}. {} ({})",
                                    i + offset + 1,
                                    c.contact_name,
                                    &E164::from_str(&c.contact_user_number)
                                        .expect("Should have been formatted upon db insertion")
                                        .area_code()
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    );
                }
                response
            }
        }
        Command::delete => {
            let name = words.collect::<Vec<_>>().join(" ");
            if name.is_empty() {
                Command::delete.hint()
            } else {
                handle_delete(pool, &from, &name).await?
            }
        }
        Command::confirm => {
            let nums = words.collect::<Vec<_>>().join(" ");
            if nums.is_empty() {
                Command::confirm.hint()
            } else {
                handle_confirm(pool, &from, &nums).await?
            }
        }
        Command::group => {
            let names = words.collect::<Vec<_>>().join(" ");
            if names.is_empty() {
                Command::group.hint()
            } else {
                handle_group(pool, &from, &names).await?
            }
        }
    };
    Ok(response)
}

async fn handle_group(pool: &Pool<Sqlite>, from: &str, names: &str) -> anyhow::Result<String> {
    let name_fragments: Vec<_> = names.split(',').map(str::trim).collect();

    if name_fragments.is_empty() {
        return Ok("Please provide at least one name to search for.".to_string());
    }

    let mut contacts = Vec::new();
    for fragment in &name_fragments {
        let like = format!("%{}%", fragment.to_lowercase());
        let mut matches = query_as!(
            Contact,
            "SELECT id as \"id!\", contact_name, contact_user_number 
             FROM contacts 
             WHERE submitter_number = ? 
             AND LOWER(contact_name) LIKE ?
             ORDER BY contact_name",
            from,
            like
        )
        .fetch_all(pool)
        .await?;
        contacts.append(&mut matches);
    }

    contacts.sort_by(|a, b| a.id.cmp(&b.id));
    contacts.dedup_by(|a, b| a.id == b.id);
    contacts.sort_by(|a, b| a.contact_name.cmp(&b.contact_name));

    if contacts.is_empty() {
        return Ok(format!(
            "No contacts found matching: {}",
            name_fragments.join(", ")
        ));
    }

    let mut tx = pool.begin().await?;

    // Set pending action type to group
    set_pending_action(pool, from, "group", &mut tx).await?;

    // Store contacts for group creation
    for contact in &contacts {
        query!(
            "INSERT INTO pending_group_members (pending_action_submitter, contact_id) 
             VALUES (?, ?)",
            from,
            contact.id
        )
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;

    let list = contacts
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let area_code = E164::from_str(&c.contact_user_number)
                .map(|e| e.area_code().to_string())
                .unwrap_or_else(|_| "???".to_string());

            format!("{}. {} ({})", i + 1, c.contact_name, area_code)
        })
        .collect::<Vec<_>>()
        .join("\n");

    Ok(format!(
        "Found these contacts:\n{}\n\nTo create a group with these contacts, reply \"confirm NUM1, NUM2, ...\"",
        list
    ))
}

async fn handle_delete(pool: &Pool<Sqlite>, from: &str, name: &str) -> anyhow::Result<String> {
    let like = format!("%{}%", name.to_lowercase());

    // Find matching groups
    let groups = query_as!(
        GroupRecord,
        r#"WITH member_counts AS (
            SELECT 
                group_id,
                COUNT(*) as count
            FROM group_members
            GROUP BY group_id
        )
        SELECT 
            groups.id as "id!", 
            groups.name,
            COALESCE(mc.count, 0) as "member_count!"
        FROM groups 
        LEFT JOIN member_counts mc ON mc.group_id = groups.id
        WHERE creator_number = ? 
        AND LOWER(name) LIKE ?
        ORDER BY name"#,
        from,
        like
    )
    .fetch_all(pool)
    .await?;
    // Find matching contacts (rest of the code unchanged)
    let contacts = query_as!(
        Contact,
        "SELECT id as \"id!\", contact_name, contact_user_number 
         FROM contacts 
         WHERE submitter_number = ? 
         AND LOWER(contact_name) LIKE ?
         ORDER BY contact_name",
        from,
        like
    )
    .fetch_all(pool)
    .await?;

    if groups.is_empty() && contacts.is_empty() {
        return Ok(format!("No groups or contacts found matching \"{}\"", name));
    }

    let mut tx = pool.begin().await?;

    // Set pending action type to deletion
    set_pending_action(pool, from, "deletion", &mut tx).await?;

    // Store groups for deletion
    for group in &groups {
        query!(
            "INSERT INTO pending_deletions (pending_action_submitter, group_id, contact_id) 
             VALUES (?, ?, NULL)",
            from,
            group.id
        )
        .execute(&mut *tx)
        .await?;
    }

    // Store contacts for deletion
    for contact in &contacts {
        query!(
            "INSERT INTO pending_deletions (pending_action_submitter, group_id, contact_id) 
             VALUES (?, NULL, ?)",
            from,
            contact.id
        )
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;

    let mut response = String::new();

    // List groups if any were found
    if !groups.is_empty() {
        response.push_str("Found these groups:\n");
        for (i, group) in groups.iter().enumerate() {
            response.push_str(&format!(
                "{}. {} ({} members)\n",
                i + 1,
                group.name,
                group.member_count
            ));
        }
    }

    // List contacts if any were found, continuing the numbering
    if !contacts.is_empty() {
        if !groups.is_empty() {
            response.push_str("\n");
        }
        response.push_str("Found these contacts:\n");
        let offset = groups.len();
        for (i, c) in contacts.iter().enumerate() {
            let area_code = E164::from_str(&c.contact_user_number)
                .map(|e| e.area_code().to_string())
                .unwrap_or_else(|_| "???".to_string());

            response.push_str(&format!(
                "{}. {} ({})\n",
                i + offset + 1,
                c.contact_name,
                area_code
            ));
        }
    }

    response.push_str(
        "\nTo delete items, reply \"confirm NUM1, NUM2, ...\", \
        where NUM1, NUM2, etc. are numbers from the lists above.",
    );

    Ok(response)
}

#[derive(Clone, sqlx::FromRow)]
struct GroupRecord {
    id: i64,
    name: String,
    member_count: i64,
}

async fn handle_confirm(
    pool: &Pool<Sqlite>,
    from: &str,
    selections: &str,
) -> anyhow::Result<String> {
    let pending_action = query!(
        "SELECT action_type FROM pending_actions WHERE submitter_number = ?",
        from
    )
    .fetch_optional(pool)
    .await?;

    let Some(action) = pending_action else {
        return Ok("No pending actions to confirm.".to_string());
    };

    let action_type = action.action_type;

    match action_type.as_str() {
        "deferred_contacts" => {
            let mut successful = Vec::new();
            let mut failed = Vec::new();

            // Get all deferred contacts
            let deferred_contacts = query!(
                "SELECT DISTINCT contact_name FROM deferred_contacts WHERE submitter_number = ?",
                from
            )
            .fetch_all(pool)
            .await?;

            // Process selections like "1a, 2b, 3a"
            for selection in selections.split(',').map(str::trim) {
                // First validate basic format: must be digits followed by a single letter
                if !selection
                    .chars()
                    .rev()
                    .next()
                    .map(|c| c.is_ascii_lowercase())
                    .unwrap_or(false)
                    || !selection[..selection.len() - 1]
                        .chars()
                        .all(|c| c.is_ascii_digit())
                {
                    failed.push(format!("Invalid selection format: {}", selection));
                    continue;
                }

                // Split into numeric and letter parts
                let (num_str, letter) = selection.split_at(selection.len() - 1);
                let contact_idx: usize = match num_str.parse::<usize>() {
                    Ok(n) if n > 0 => n - 1,
                    _ => {
                        failed.push(format!("Invalid contact number: {}", num_str));
                        continue;
                    }
                };

                // Get the contact name
                let Some(contact_name) =
                    deferred_contacts.get(contact_idx).map(|c| &c.contact_name)
                else {
                    failed.push(format!("Contact number {} not found", contact_idx + 1));
                    continue;
                };

                // Get all numbers for this contact to validate letter selection
                let numbers = query!(
                    "SELECT phone_number, phone_description FROM deferred_contacts 
             WHERE submitter_number = ? AND contact_name = ?
             ORDER BY id",
                    from,
                    contact_name
                )
                .fetch_all(pool)
                .await?;

                let letter = letter.chars().next().unwrap();
                let letter_idx = match letter {
                    'a'..='z' => {
                        let idx = (letter as u8 - b'a') as usize;
                        if idx >= numbers.len() {
                            failed.push(format!("Invalid letter selection: {}", letter));
                            continue;
                        }
                        idx
                    }
                    _ => {
                        failed.push(format!("Invalid letter selection: {}", letter));
                        continue;
                    }
                };

                // Get the selected number
                let number = &numbers[letter_idx];

                // Insert the contact
                if let Err(e) = add_contact(pool, from, contact_name, &number.phone_number).await {
                    failed.push(format!(
                        "Failed to add {} ({}): {}",
                        contact_name, number.phone_number, e
                    ));
                } else {
                    successful.push(format!("{} ({})", contact_name, number.phone_number));
                }
            }

            // Clean up processed contacts
            let mut tx = pool.begin().await?;
            for contact in &successful {
                if let Some(name) = contact.split(" (").next() {
                    query!(
                        "DELETE FROM deferred_contacts WHERE submitter_number = ? AND contact_name = ?",
                        from,
                        name
                    )
                    .execute(&mut *tx)
                    .await?;
                }
            }

            // Clean up pending action if all contacts are processed
            let remaining = query!(
                "SELECT COUNT(*) as count FROM deferred_contacts WHERE submitter_number = ?",
                from
            )
            .fetch_one(&mut *tx)
            .await?;

            if remaining.count == 0 {
                query!(
                    "DELETE FROM pending_actions WHERE submitter_number = ?",
                    from
                )
                .execute(&mut *tx)
                .await?;
            }

            tx.commit().await?;

            // Format response
            let mut response = String::new();
            if !successful.is_empty() {
                response.push_str(&format!(
                    "Successfully added {} contact{}:\n",
                    successful.len(),
                    if successful.len() == 1 { "" } else { "s" }
                ));
                for contact in successful {
                    response.push_str(&format!("• {}\n", contact));
                }
            }

            if !failed.is_empty() {
                if !response.is_empty() {
                    response.push_str("\n");
                }
                response.push_str("Failed to process:\n");
                for error in failed {
                    response.push_str(&format!("• {}\n", error));
                }
            }

            Ok(response)
        }
        "deletion" => {
            let mut invalid = Vec::new();
            let mut selected_groups = Vec::new();
            let mut selected_contacts = Vec::new();

            // Get total number of pending items to validate selection range
            let groups = query_as!(
                GroupRecord,
                r#"WITH member_counts AS (
                    SELECT 
                        group_id,
                        COUNT(*) as count
                    FROM group_members
                    GROUP BY group_id
                )
                SELECT 
                    g.id as "id!", 
                    g.name,
                    COALESCE(mc.count, 0) as "member_count!"
                FROM groups g
                JOIN pending_deletions pd ON pd.group_id = g.id
                LEFT JOIN member_counts mc ON mc.group_id = g.id
                WHERE pd.pending_action_submitter = ?
                ORDER BY g.name"#,
                from
            )
            .fetch_all(pool)
            .await?;
            let contacts = query_as!(
                Contact,
                "SELECT c.id as \"id!\", c.contact_name, c.contact_user_number 
                 FROM contacts c
                 JOIN pending_deletions pd ON pd.contact_id = c.id
                 WHERE pd.pending_action_submitter = ?
                 ORDER BY c.contact_name",
                from
            )
            .fetch_all(pool)
            .await?;

            // Process selections
            for num_str in selections.split(',').map(str::trim) {
                match num_str.parse::<usize>() {
                    Ok(num) if num > 0 => {
                        let num = num - 1; // Convert to 0-based index
                        if num < groups.len() {
                            selected_groups.push(GroupRecord {
                                id: groups[num].id,
                                name: groups[num].name.clone(),
                                member_count: groups[num].member_count,
                            });
                        } else if num < groups.len() + contacts.len() {
                            selected_contacts.push(contacts[num - groups.len()].clone());
                        } else {
                            invalid.push(format!("Invalid selection: {}", num + 1));
                        }
                    }
                    _ => invalid.push(format!("Invalid selection: {}", num_str)),
                }
            }

            if selected_groups.is_empty() && selected_contacts.is_empty() && invalid.is_empty() {
                return Ok("No valid selections provided.".to_string());
            }

            let mut tx = pool.begin().await?;

            // Delete selected groups
            for group in &selected_groups {
                query!("DELETE FROM groups WHERE id = ?", group.id)
                    .execute(&mut *tx)
                    .await?;
            }

            // Delete selected contacts
            for contact in &selected_contacts {
                query!("DELETE FROM contacts WHERE id = ?", contact.id)
                    .execute(&mut *tx)
                    .await?;
            }

            // Clean up pending actions
            query!(
                "DELETE FROM pending_actions WHERE submitter_number = ?",
                from
            )
            .execute(&mut *tx)
            .await?;

            tx.commit().await?;

            // Format response
            let mut response = String::new();

            if !selected_groups.is_empty() {
                response.push_str(&format!(
                    "Deleted {} group{}:\n",
                    selected_groups.len(),
                    if selected_groups.len() == 1 { "" } else { "s" }
                ));
                for group in selected_groups {
                    response.push_str(&format!(
                        "• {} ({} members)\n",
                        group.name, group.member_count
                    ));
                }
            }
            if !selected_contacts.is_empty() {
                if !response.is_empty() {
                    response.push_str("\n");
                }
                response.push_str(&format!(
                    "Deleted {} contact{}:\n",
                    selected_contacts.len(),
                    if selected_contacts.len() == 1 {
                        ""
                    } else {
                        "s"
                    }
                ));
                for contact in selected_contacts {
                    let area_code = E164::from_str(&contact.contact_user_number)
                        .map(|e| e.area_code().to_string())
                        .unwrap_or_else(|_| "???".to_string());
                    response.push_str(&format!("• {} ({})\n", contact.contact_name, area_code));
                }
            }

            if !invalid.is_empty() {
                if !response.is_empty() {
                    response.push_str("\n");
                }
                response.push_str("Errors:\n");
                response.push_str(&invalid.join("\n"));
            }

            Ok(response)
        }
        "group" => {
            // Existing group creation logic remains the same
            let mut invalid = Vec::new();
            let mut selected_contacts = Vec::new();

            for num_str in selections.split(',').map(str::trim) {
                match num_str.parse::<usize>() {
                    Ok(num) if num > 0 => {
                        let offset = (num - 1) as i64;
                        let query = query!(
                            "SELECT c.id as \"id!\", c.contact_name, c.contact_user_number 
                             FROM contacts c
                             JOIN pending_group_members pgm ON pgm.contact_id = c.id
                             WHERE pgm.pending_action_submitter = ?
                             ORDER BY c.contact_name
                             LIMIT 1 OFFSET ?",
                            from,
                            offset
                        );
                        if let Some(row) = query.fetch_optional(pool).await? {
                            selected_contacts.push(Contact {
                                id: row.id,
                                contact_name: row.contact_name,
                                contact_user_number: row.contact_user_number,
                            });
                        } else {
                            invalid.push(format!("Invalid selection: {}", num));
                        }
                    }
                    _ => invalid.push(format!("Invalid number: {}", num_str)),
                }
            }

            create_group(pool, from, selected_contacts, invalid).await
        }
        _ => Ok("Invalid action type".to_string()),
    }
}

async fn create_group(
    pool: &Pool<Sqlite>,
    from: &str,
    contacts: Vec<Contact>,
    invalid: Vec<String>,
) -> anyhow::Result<String> {
    let mut group_num = 0;
    loop {
        let group_name = format!("group{}", group_num);
        let existing = query!(
            "SELECT id FROM groups WHERE creator_number = ? AND name = ?",
            from,
            group_name
        )
        .fetch_optional(pool)
        .await?;

        if existing.is_none() {
            break;
        }
        group_num += 1;
    }

    let group_name = format!("group{}", group_num);

    let mut tx = pool.begin().await?;

    query!(
        "INSERT INTO groups (name, creator_number) VALUES (?, ?)",
        group_name,
        from
    )
    .execute(&mut *tx)
    .await?;

    let group_id = query!(
        "SELECT id FROM groups WHERE creator_number = ? AND name = ?",
        from,
        group_name
    )
    .fetch_one(&mut *tx)
    .await?
    .id;

    for contact in &contacts {
        query!(
            "INSERT INTO group_members (group_id, member_number) VALUES (?, ?)",
            group_id,
            contact.contact_user_number
        )
        .execute(&mut *tx)
        .await?;
    }

    // Clean up pending actions
    query!(
        "DELETE FROM pending_actions WHERE submitter_number = ?",
        from
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    let mut response = format!(
        "Created group \"{}\" with {} members:\n",
        group_name,
        contacts.len()
    );

    for contact in contacts {
        let area_code = E164::from_str(&contact.contact_user_number)
            .map(|e| e.area_code().to_string())
            .unwrap_or_else(|_| "???".to_string());
        response.push_str(&format!("• {} ({})\n", contact.contact_name, area_code));
    }

    if !invalid.is_empty() {
        response.push_str("\nErrors:\n");
        response.push_str(&invalid.join("\n"));
    }

    Ok(response)
}

async fn onboard_new_user(
    command: Option<Result<Command, serde_json::Error>>,
    words: impl Iterator<Item = &str>,
    from: &str,
    pool: &Pool<Sqlite>,
) -> anyhow::Result<String> {
    let Some(Ok(Command::name)) = command else {
        return Ok(format!(
            "Greetings! This is Decision Bot (https://github.com/samcarey/decisionbot).\n\
            To participate:\n{}",
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

async fn cleanup_expired_pending_actions(pool: &Pool<Sqlite>) -> Result<()> {
    query!("DELETE FROM pending_actions WHERE created_at < unixepoch() - 300")
        .execute(pool)
        .await?;
    Ok(())
}

async fn set_pending_action(
    _pool: &Pool<Sqlite>, // Changed to _pool since it's unused
    from: &str,
    action_type: &str,
    tx: &mut sqlx::Transaction<'_, Sqlite>,
) -> Result<()> {
    // Clear any existing pending action
    query!(
        "DELETE FROM pending_actions WHERE submitter_number = ?",
        from
    )
    .execute(&mut **tx)
    .await?;

    // Create new pending action
    query!(
        "INSERT INTO pending_actions (submitter_number, action_type) VALUES (?, ?)",
        from,
        action_type
    )
    .execute(&mut **tx)
    .await?;

    Ok(())
}
