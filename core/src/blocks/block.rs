use crate::blocks::{
    browser::Browser, code::Code, curl::Curl, data::Data, input::Input, llm::LLM, map::Map,
    reduce::Reduce, search::Search,
};
use crate::project::Project;
use crate::run::{Credentials, RunConfig};
use crate::stores::store::Store;
use crate::utils::ParseError;
use crate::Rule;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use lazy_static::lazy_static;
use pest::iterators::Pair;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::any::Any;
use std::collections::HashMap;
use std::str::FromStr;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Serialize, PartialEq, Clone, Debug)]
pub struct MapState {
    pub name: String,
    pub iteration: usize,
}

#[derive(Serialize, PartialEq, Clone, Debug)]
pub struct InputState {
    pub value: Option<Value>,
    pub index: usize,
}

// Env is serialized when passed to code blocks. RunConfig.credentials are not serialized.
#[derive(Serialize, Clone)]
pub struct Env {
    pub config: RunConfig,
    pub state: HashMap<String, Value>,
    pub input: InputState,
    pub map: Option<MapState>,
    #[serde(skip_serializing)]
    pub store: Box<dyn Store + Sync + Send>,
    #[serde(skip_serializing)]
    pub project: Project,
    #[serde(skip_serializing)]
    pub credentials: Credentials,
}

// pub enum Expectations {
//   Keys(Vec<String>),
//   Array(Box<Expectations>),
// }

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum BlockType {
    Input,
    Data,
    Code,
    LLM,
    Map,
    Reduce,
    Search,
    Curl,
    Browser,
}

impl ToString for BlockType {
    fn to_string(&self) -> String {
        match self {
            BlockType::Input => String::from("input"),
            BlockType::Data => String::from("data"),
            BlockType::Code => String::from("code"),
            BlockType::LLM => String::from("llm"),
            BlockType::Map => String::from("map"),
            BlockType::Reduce => String::from("reduce"),
            BlockType::Search => String::from("search"),
            BlockType::Curl => String::from("curl"),
            BlockType::Browser => String::from("browser"),
        }
    }
}

impl FromStr for BlockType {
    type Err = ParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "input" => Ok(BlockType::Input),
            "data" => Ok(BlockType::Data),
            "code" => Ok(BlockType::Code),
            "llm" => Ok(BlockType::LLM),
            "map" => Ok(BlockType::Map),
            "reduce" => Ok(BlockType::Reduce),
            "search" => Ok(BlockType::Search),
            "curl" => Ok(BlockType::Curl),
            "browser" => Ok(BlockType::Browser),
            _ => Err(ParseError::with_message("Unknown BlockType"))?,
        }
    }
}

#[async_trait]
pub trait Block {
    fn block_type(&self) -> BlockType;

    fn inner_hash(&self) -> String;

    async fn execute(
        &self,
        name: &str,
        env: &Env,
        event_sender: Option<UnboundedSender<Value>>,
    ) -> Result<Value>;

    fn clone_box(&self) -> Box<dyn Block + Sync + Send>;
    fn as_any(&self) -> &dyn Any;
}

impl Clone for Box<dyn Block + Sync + Send> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

/// Parses a block pair from a pest parser Pair.
pub fn parse_pair(pair_pair: Pair<Rule>) -> Result<(String, String)> {
    assert!(pair_pair.as_rule() == Rule::pair);

    let mut key: Option<String> = None;
    let mut value: Option<String> = None;
    for pair in pair_pair.into_inner() {
        match pair.as_rule() {
            Rule::key => {
                key = Some(pair.as_str().to_string());
            }
            Rule::string => {
                value = Some(pair.as_str().to_string());
            }
            Rule::multiline => {
                let chars = pair.as_str().chars().collect::<Vec<char>>();
                if chars[chars.len() - 4] != '\n' {
                    Err(anyhow!("Multine values are expected to end with '\\n```'"))?;
                }
                value = Some(chars.iter().skip(4).take(chars.len() - 8).collect());
            }
            _ => unreachable!(),
        }
    }
    assert!(key.is_some());
    assert!(value.is_some());

    Ok((key.unwrap(), value.unwrap()))
}

// TODO(spolu): pass in block_name for better error messages.
pub fn parse_block(t: BlockType, block_pair: Pair<Rule>) -> Result<Box<dyn Block + Sync + Send>> {
    match t {
        BlockType::Input => Ok(Box::new(Input::parse(block_pair)?)),
        BlockType::Data => Ok(Box::new(Data::parse(block_pair)?)),
        BlockType::Code => Ok(Box::new(Code::parse(block_pair)?)),
        BlockType::LLM => Ok(Box::new(LLM::parse(block_pair)?)),
        BlockType::Map => Ok(Box::new(Map::parse(block_pair)?)),
        BlockType::Reduce => Ok(Box::new(Reduce::parse(block_pair)?)),
        BlockType::Search => Ok(Box::new(Search::parse(block_pair)?)),
        BlockType::Curl => Ok(Box::new(Curl::parse(block_pair)?)),
        BlockType::Browser => Ok(Box::new(Browser::parse(block_pair)?)),
    }
}

pub fn find_variables(text: &str) -> Vec<(String, String)> {
    lazy_static! {
        static ref RE: Regex =
            Regex::new(r"\$\{(?P<name>[A-Z0-9_]+)\.(?P<key>[a-zA-Z0-9_\.]+)\}").unwrap();
    }

    RE.captures_iter(text)
        .map(|c| {
            let name = c.name("name").unwrap().as_str();
            let key = c.name("key").unwrap().as_str();
            // println!("{} {}", name, key);
            (String::from(name), String::from(key))
        })
        .collect::<Vec<_>>()
}

pub fn replace_variables_in_string(text: &str, field: &str, env: &Env) -> Result<String> {
    let variables = find_variables(text);

    let mut result = text.to_string();

    variables
        .iter()
        .map(|(name, key)| {
            // Check that the block output exists and is an object.
            let output = env
                .state
                .get(name)
                .ok_or_else(|| anyhow!("Block `{}` output not found", name))?;
            if !output.is_object() {
                Err(anyhow!(
                    "Block `{}` output is not an object, the output of `{}` referred to \
                     as a variable in `{}` must be an object",
                    name,
                    name,
                    field
                ))?;
            }
            let output = output.as_object().unwrap();

            if !output.contains_key(key) {
                Err(anyhow!(
                    "Key `{}` is not present in block `{}` output",
                    key,
                    name
                ))?;
            }
            // Check that output[key] is a string.
            if !output.get(key).unwrap().is_string() {
                Err(anyhow!("`{}.{}` is not a string", name, key,))?;
            }
            result = result.replace(
                &format!("${{{}.{}}}", name, key),
                &output[key].as_str().unwrap(),
            );

            Ok(())
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::block::InputState;
    use crate::project::Project;
    use crate::run::{Credentials, RunConfig};
    use crate::stores::sqlite::SQLiteStore;
    use std::collections::HashMap;

    #[test]
    fn find_variables_test() -> Result<()> {
        assert_eq!(
            find_variables("QUESTION: ${RETRIEVE.question}\nANSWER: ${DATA.answer}"),
            vec![
                ("RETRIEVE".to_string(), "question".to_string()),
                ("DATA".to_string(), "answer".to_string()),
            ]
        );

        Ok(())
    }

    #[test]
    fn replace_variables_in_string_test() -> Result<()> {
        let env = Env {
            config: RunConfig {
                blocks: HashMap::new(),
            },
            state: serde_json::from_str(
                r#"{"RETRIEVE":{"question":"What is your name?"},"DATA":{"answer":"John"}}"#,
            )
            .unwrap(),
            input: InputState {
                value: Some(serde_json::from_str(r#"{"question":"Who is it?"}"#).unwrap()),
                index: 0,
            },
            map: None,
            project: Project::new_from_id(1),
            store: Box::new(SQLiteStore::new_in_memory()?),
            credentials: Credentials::new(),
        };
        assert_eq!(
            replace_variables_in_string(
                r#"QUESTION: ${RETRIEVE.question} ANSWER: ${DATA.answer}"#,
                "foo",
                &env
            )?,
            r#"QUESTION: What is your name? ANSWER: John"#.to_string()
        );

        Ok(())
    }
}
