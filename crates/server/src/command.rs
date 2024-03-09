use std::fmt::Display;

use enum_iterator::Sequence;
use serde::{Deserialize, Serialize};

// variants must be all lowercase for serde_json to deserialize them
#[allow(non_camel_case_types)]
#[derive(Deserialize, Serialize, Sequence, Debug)]
pub(crate) enum Command {
    h,
    name,
    info,
    stop,
}

impl TryFrom<&str> for Command {
    type Error = serde_json::Error;
    fn try_from(value: &str) -> std::prelude::v1::Result<Self, Self::Error> {
        serde_json::from_str(&format!("\"{}\"", value.to_lowercase()))
    }
}

impl Display for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", format!("{:?}", self).split("::").last().unwrap())
    }
}

struct ParameterDoc {
    example: String,
    description: String,
}

impl Command {
    pub fn description(&self) -> String {
        match self {
            Self::h => "Show a list of available commands ",
            Self::info => "See information about a command",
            Self::name => "Set your preferred name",
            Self::stop => "Stop receiving messages and remove yourself from the database",
        }
        .to_string()
    }
    fn parameter_doc(&self) -> Option<ParameterDoc> {
        match self {
            Self::h => None,
            Self::info => Some(ParameterDoc {
                example: Command::name.to_string(),
                description: "the command you want to see help for".to_string(),
            }),
            Self::name => Some(ParameterDoc {
                example: "John S.".to_string(),
                description: "your name".to_string(),
            }),
            Self::stop => None,
        }
    }
    pub fn usage(&self) -> String {
        if let Some(ParameterDoc {
            example,
            description,
        }) = self.parameter_doc()
        {
            format!("Reply \"{self} X\", where X is {description}.\nExample: \"{self} {example}\".")
        } else {
            format!("Reply \"{self}\"")
        }
    }
}

#[test]
fn command() {
    let command_text = "name";
    assert_eq!(
        Command::try_from(command_text).unwrap().to_string(),
        command_text
    );
}
