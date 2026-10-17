//! Stateless native function-tool content. No run endpoint or capability is
//! enabled by this module. Applications own history and authorize every effect.
use crate::application_run::{
    UniqueJson, ValidationError, MAX_INSTRUCTIONS_BYTES, MAX_REQUEST_BYTES, MAX_RESULT_BYTES,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

pub const NATIVE_FUNCTION_TURNS_V1: &str = "native_function_turns_v1";
pub const MAX_HISTORY_ITEMS: usize = 512;
pub const MAX_FUNCTION_TOOLS: usize = 512;
pub const MAX_TURN_CALLS: usize = 16;
pub const MAX_HISTORY_BYTES: usize = 512 * 1024;
pub const MAX_TOOLS_BYTES: usize = 256 * 1024;
const MAX_TEXT_BYTES: usize = 128 * 1024;

fn invalid() -> ValidationError {
    ValidationError::InvalidRequest
}
fn bounded_json(value: &Value) -> Result<(), ValidationError> {
    let mut pending = vec![(value, 0)];
    let mut count = 0;
    while let Some((value, depth)) = pending.pop() {
        count += 1;
        if count > 16384 || depth > 64 {
            return Err(ValidationError::TooLarge);
        }
        match value {
            Value::Object(map) => {
                if count + pending.len() + map.len() > 16384 {
                    return Err(ValidationError::TooLarge);
                }
                pending.extend(map.values().map(|value| (value, depth + 1)));
            }
            Value::Array(array) => {
                if count + pending.len() + array.len() > 16384 {
                    return Err(ValidationError::TooLarge);
                }
                pending.extend(array.iter().map(|value| (value, depth + 1)));
            }
            _ => {}
        }
    }
    Ok(())
}
fn size(value: &impl Serialize, max: usize) -> Result<(), ValidationError> {
    struct Limit {
        left: usize,
        exceeded: bool,
    }
    impl std::io::Write for Limit {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.left {
                self.exceeded = true;
                return Err(std::io::Error::other("native content exceeds byte limit"));
            }
            self.left -= bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut limit = Limit {
        left: max,
        exceeded: false,
    };
    match serde_json::to_writer(&mut limit, value) {
        Ok(()) => Ok(()),
        Err(_) if limit.exceeded => Err(ValidationError::TooLarge),
        Err(_) => Err(invalid()),
    }
}
fn fields(value: &Value, allowed: &[&str]) -> bool {
    value
        .as_object()
        .is_some_and(|map| map.keys().all(|key| allowed.contains(&key.as_str())))
}
fn text(value: &Value, min: usize, max: usize) -> bool {
    value
        .as_str()
        .is_some_and(|text| text.len() >= min && text.len() <= max)
}
fn identifier(value: &Value) -> bool {
    text(value, 1, 512)
        && value
            .as_str()
            .unwrap()
            .bytes()
            .all(|byte| (33..=126).contains(&byte))
}
fn name(value: &Value) -> bool {
    text(value, 1, 64)
        && value
            .as_str()
            .unwrap()
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}
fn optional(value: &Value, key: &str, predicate: impl Fn(&Value) -> bool) -> bool {
    value
        .get(key)
        .is_none_or(|value| value.is_null() || predicate(value))
}
fn completed_status(value: &Value) -> bool {
    optional(value, "status", |status| status == "completed")
}
fn direct_call(value: &Value) -> bool {
    optional(value, "async", |value| value == false)
        && optional(value, "caller", |value| {
            fields(value, &["type"]) && value["type"] == "direct"
        })
        && value.get("namespace").is_none_or(Value::is_null)
}
fn unique_value(bytes: &[u8]) -> Result<Value, ValidationError> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = UniqueJson::deserialize(&mut deserializer).map_err(|_| invalid())?;
    deserializer.end().map_err(|_| invalid())?;
    Ok(value.0)
}

/// Original wire object, validated without adding, removing or rewriting schema
/// fields. `strict` is explicit, so native schemas cannot be silently normalized.
#[derive(Clone, PartialEq)]
pub struct FunctionTool(Value);
impl FunctionTool {
    pub fn new(value: Value) -> Result<Self, ValidationError> {
        bounded_json(&value)?;
        if !fields(
            &value,
            &["type", "name", "description", "parameters", "strict"],
        ) || value["type"] != "function"
            || !name(&value["name"])
            || !value["parameters"].is_object()
            || !value["strict"].is_boolean()
            || !optional(&value, "description", |value| text(value, 0, 16384))
        {
            return Err(invalid());
        }
        size(&value, MAX_TOOLS_BYTES)?;
        Ok(Self(value))
    }
    pub fn as_value(&self) -> &Value {
        &self.0
    }
    pub fn name(&self) -> &str {
        self.0["name"].as_str().unwrap()
    }
    pub fn parameters(&self) -> &Value {
        &self.0["parameters"]
    }
}

/// A replayable text, reasoning, call or call-output item. Private values remain
/// untouched, including opaque encrypted reasoning and function argument text.
#[derive(Clone, PartialEq)]
pub struct NativeItem(Value);
impl NativeItem {
    pub fn new(value: Value) -> Result<Self, ValidationError> {
        bounded_json(&value)?;
        if !optional(&value, "id", identifier) || !completed_status(&value) {
            return Err(invalid());
        }
        let valid = match value["type"].as_str() {
            Some("message") => {
                fields(
                    &value,
                    &["type", "id", "role", "content", "status", "phase"],
                ) && matches!(value["role"].as_str(), Some("user" | "assistant"))
                    && optional(&value, "phase", |value| {
                        matches!(value.as_str(), Some("commentary" | "final_answer"))
                    })
                    && value["content"].as_array().is_some_and(|content| {
                        !content.is_empty()
                            && content.len() <= 128
                            && content.iter().all(|part| {
                                let output = value["role"] == "assistant";
                                fields(
                                    part,
                                    if output {
                                        &["type", "text", "annotations", "logprobs"]
                                    } else {
                                        &["type", "text"]
                                    },
                                ) && part["type"]
                                    == if output { "output_text" } else { "input_text" }
                                    && text(&part["text"], 0, MAX_TEXT_BYTES)
                                    && optional(part, "annotations", Value::is_array)
                                    && optional(part, "logprobs", Value::is_array)
                            })
                    })
            }
            Some("reasoning") => {
                fields(
                    &value,
                    &[
                        "type",
                        "id",
                        "summary",
                        "encrypted_content",
                        "status",
                        "content",
                    ],
                ) && identifier(&value["id"])
                    && text(&value["encrypted_content"], 1, MAX_RESULT_BYTES)
                    && optional(&value, "content", |content| {
                        content.as_array().is_some_and(|parts| {
                            parts.len() <= 128
                                && parts.iter().all(|part| {
                                    fields(part, &["type", "text"])
                                        && part["type"] == "reasoning_text"
                                        && text(&part["text"], 0, MAX_TEXT_BYTES)
                                })
                        })
                    })
                    && value["summary"].as_array().is_some_and(|summary| {
                        summary.len() <= 128
                            && summary.iter().all(|part| {
                                fields(part, &["type", "text"])
                                    && part["type"] == "summary_text"
                                    && text(&part["text"], 0, MAX_TEXT_BYTES)
                            })
                    })
            }
            Some("function_call") => {
                fields(
                    &value,
                    &[
                        "type",
                        "id",
                        "call_id",
                        "name",
                        "arguments",
                        "status",
                        "async",
                        "caller",
                        "namespace",
                    ],
                ) && identifier(&value["call_id"])
                    && name(&value["name"])
                    && direct_call(&value)
                    && text(&value["arguments"], 2, MAX_TEXT_BYTES)
                    && unique_value(value["arguments"].as_str().unwrap().as_bytes()).is_ok_and(
                        |arguments| arguments.is_object() && bounded_json(&arguments).is_ok(),
                    )
            }
            Some("function_call_output") => {
                fields(
                    &value,
                    &[
                        "type",
                        "id",
                        "call_id",
                        "output",
                        "status",
                        "caller",
                        "name",
                        "namespace",
                    ],
                ) && identifier(&value["call_id"])
                    && direct_call(&value)
                    && optional(&value, "name", name)
                    && text(&value["output"], 0, MAX_TEXT_BYTES)
            }
            _ => false,
        };
        if !valid {
            return Err(invalid());
        }
        size(&value, MAX_RESULT_BYTES)?;
        Ok(Self(value))
    }
    pub fn as_value(&self) -> &Value {
        &self.0
    }
    pub fn kind(&self) -> &str {
        self.0["type"].as_str().unwrap()
    }
}

macro_rules! private_wire {
    ($type:ty, $label:literal) => {
        impl fmt::Debug for $type {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(concat!($label, "([private content omitted])"))
            }
        }
        impl Serialize for $type {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                self.0.serialize(serializer)
            }
        }
        impl<'de> Deserialize<'de> for $type {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = UniqueJson::deserialize(deserializer)?;
                Self::new(value.0).map_err(serde::de::Error::custom)
            }
        }
    };
}
private_wire!(FunctionTool, "FunctionTool");
private_wire!(NativeItem, "NativeItem");

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeTurnInput {
    pub instructions: String,
    pub history: Vec<NativeItem>,
    pub tools: Vec<FunctionTool>,
    pub max_output_tokens: u32,
}
impl fmt::Debug for NativeTurnInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NativeTurnInput([private content omitted])")
    }
}
impl NativeTurnInput {
    pub fn parse(bytes: &[u8]) -> Result<Self, ValidationError> {
        if bytes.len() > MAX_REQUEST_BYTES {
            return Err(ValidationError::TooLarge);
        }
        let value: Self = serde_json::from_value(unique_value(bytes)?).map_err(|_| invalid())?;
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.instructions.len() > MAX_INSTRUCTIONS_BYTES
            || self.history.is_empty()
            || self.history.len() > MAX_HISTORY_ITEMS
            || self.tools.len() > MAX_FUNCTION_TOOLS
            || self.max_output_tokens == 0
        {
            return Err(invalid());
        }
        let names: BTreeSet<_> = self.tools.iter().map(FunctionTool::name).collect();
        if names.len() != self.tools.len() {
            return Err(invalid());
        }
        validate_history(&self.history)?;
        size(&self.history, MAX_HISTORY_BYTES)?;
        size(&self.tools, MAX_TOOLS_BYTES)?;
        size(self, MAX_REQUEST_BYTES)
    }
}

fn validate_history(history: &[NativeItem]) -> Result<BTreeSet<&str>, ValidationError> {
    item_ids(history)?;
    let mut seen = BTreeSet::new();
    let mut pending = BTreeMap::new();
    for item in history {
        let value = item.as_value();
        match item.kind() {
            "function_call" => {
                let call_id = value["call_id"].as_str().unwrap();
                if !seen.insert(call_id) {
                    return Err(invalid());
                }
                pending.insert(call_id, value["name"].as_str().unwrap());
            }
            "function_call_output" => {
                let Some(expected_name) = pending.remove(value["call_id"].as_str().unwrap()) else {
                    return Err(invalid());
                };
                if value["name"]
                    .as_str()
                    .is_some_and(|name| name != expected_name)
                {
                    return Err(invalid());
                }
            }
            "message" if value["role"] == "user" && !pending.is_empty() => return Err(invalid()),
            _ => {}
        }
    }
    if !pending.is_empty() {
        return Err(invalid());
    }
    Ok(seen)
}

fn item_ids(items: &[NativeItem]) -> Result<BTreeSet<&str>, ValidationError> {
    let mut ids = BTreeSet::new();
    for item in items {
        if let Some(id) = item.as_value()["id"].as_str() {
            if !ids.insert(id) {
                return Err(invalid());
            }
        }
    }
    Ok(ids)
}

/// One native reply, including every replayable reasoning/message/call item.
/// This content is inline-only at Fence; keeping it is the application's job.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeTurnOutput {
    pub output: Vec<NativeItem>,
}
impl fmt::Debug for NativeTurnOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NativeTurnOutput([private content omitted])")
    }
}
impl NativeTurnOutput {
    pub fn parse(bytes: &[u8], input: &NativeTurnInput) -> Result<Self, ValidationError> {
        if bytes.len() > MAX_RESULT_BYTES {
            return Err(ValidationError::TooLarge);
        }
        let value: Self = serde_json::from_value(unique_value(bytes)?).map_err(|_| invalid())?;
        value.validate_for(input)?;
        Ok(value)
    }
    pub fn validate_for(&self, input: &NativeTurnInput) -> Result<(), ValidationError> {
        input.validate()?;
        if self.output.is_empty() || self.output.len() > MAX_HISTORY_ITEMS {
            return Err(invalid());
        }
        let tools: BTreeSet<_> = input.tools.iter().map(FunctionTool::name).collect();
        let mut calls = validate_history(&input.history)?;
        let mut ids = item_ids(&input.history)?;
        let previous_calls = calls.len();
        let mut visible_output = false;
        for item in &self.output {
            let value = item.as_value();
            if let Some(id) = value["id"].as_str() {
                if !ids.insert(id) {
                    return Err(invalid());
                }
            }
            match item.kind() {
                "function_call" => {
                    if !tools.contains(value["name"].as_str().unwrap())
                        || !calls.insert(value["call_id"].as_str().unwrap())
                    {
                        return Err(invalid());
                    }
                    visible_output = true;
                }
                "message" if value["role"] == "assistant" => {
                    visible_output |= value["content"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|part| !part["text"].as_str().unwrap().trim().is_empty());
                }
                "reasoning" => {}
                _ => return Err(invalid()),
            }
        }
        if !visible_output || calls.len() - previous_calls > MAX_TURN_CALLS {
            return Err(invalid());
        }
        size(self, MAX_RESULT_BYTES)
    }
    /// Append the original output and actual application tool outcomes. Refuse
    /// missing, duplicate or orphan outcomes; do not synthesize successful effects.
    pub fn continue_with(
        &self,
        input: &NativeTurnInput,
        outcomes: Vec<NativeItem>,
    ) -> Result<NativeTurnInput, ValidationError> {
        self.validate_for(input)?;
        if outcomes
            .iter()
            .any(|item| item.kind() != "function_call_output")
        {
            return Err(invalid());
        }
        let mut next = input.clone();
        next.history.extend(self.output.iter().cloned());
        next.history.extend(outcomes);
        next.validate()?;
        Ok(next)
    }
}

#[cfg(test)]
mod tests;
