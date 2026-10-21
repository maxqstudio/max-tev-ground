//! Ollama adapter over the loopback client: listing installed models and structured generation
//! with fixed decoding settings. Models are never pulled, created, copied, or deleted.

use std::{collections::BTreeMap, time::Duration};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::{
    http::{self, HttpLimits, LoopbackEndpoint, RuntimeCall, RuntimeError},
    spec::{is_sha256, OutputField, OutputKind, RowReason},
};

pub const PROMPT_TEMPLATE_VERSION: u16 = 1;
pub const MAX_LISTING_BYTES: u64 = 1024 * 1024;
pub const MAX_GENERATION_BYTES: u64 = 256 * 1024;
pub const MAX_LISTED_MODELS: usize = 1024;
const LISTING_TIMEOUT: Duration = Duration::from_secs(10);

/// One installed model as the runtime lists it. Only these fields are kept.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OllamaListing {
    pub name: String,
    pub digest: String,
    pub size_bytes: Option<u64>,
    pub modified_at: Option<String>,
    pub format: Option<String>,
    pub family: Option<String>,
    pub parameter_size: Option<String>,
    pub quantization_level: Option<String>,
}

fn bounded_text(value: &Value, limit: usize) -> Option<String> {
    value
        .as_str()
        .filter(|text| text.len() <= limit && !text.chars().any(char::is_control))
        .map(str::to_owned)
}

fn valid_model_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '/'))
        && !name.contains("..")
        && !name.starts_with(['/', '.', '-'])
}

/// Lists installed models. A listing with any entry that lacks a valid name or SHA-256 digest is
/// rejected as a whole, so a partially readable listing is never mistaken for the truth.
pub fn list_models(endpoint: &LoopbackEndpoint) -> Result<Vec<OllamaListing>, RuntimeError> {
    let call = RuntimeCall::ListModels;
    let body = http::send(
        endpoint,
        call.method(),
        call.path(),
        None,
        &HttpLimits {
            timeout: LISTING_TIMEOUT,
            max_response_bytes: MAX_LISTING_BYTES,
        },
    )?;
    parse_listing(&body)
}

pub fn parse_listing(body: &[u8]) -> Result<Vec<OllamaListing>, RuntimeError> {
    let malformed = |reason: &str| RuntimeError::Malformed(format!("model listing {reason}"));
    let document: Value = serde_json::from_slice(body).map_err(|_| malformed("is not JSON"))?;
    let models = document
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| malformed("has no models array"))?;
    if models.len() > MAX_LISTED_MODELS {
        return Err(malformed("lists too many models"));
    }
    models
        .iter()
        .map(|entry| {
            let name = entry
                .get("name")
                .or_else(|| entry.get("model"))
                .and_then(Value::as_str)
                .filter(|name| valid_model_name(name))
                .ok_or_else(|| malformed("contains an entry without a valid model name"))?;
            let digest = entry
                .get("digest")
                .and_then(Value::as_str)
                .map(|digest| digest.strip_prefix("sha256:").unwrap_or(digest))
                .filter(|digest| is_sha256(digest))
                .ok_or_else(|| {
                    malformed(&format!(
                        "entry {name} has no valid SHA-256 manifest digest"
                    ))
                })?;
            let details = entry.get("details").cloned().unwrap_or(Value::Null);
            Ok(OllamaListing {
                name: name.into(),
                digest: digest.into(),
                size_bytes: entry.get("size").and_then(Value::as_u64),
                modified_at: entry.get("modified_at").and_then(|v| bounded_text(v, 64)),
                format: bounded_text(&details["format"], 32),
                family: bounded_text(&details["family"], 64),
                parameter_size: bounded_text(&details["parameter_size"], 32),
                quantization_level: bounded_text(&details["quantization_level"], 32),
            })
        })
        .collect()
}

/// Fixed-decoding settings sent with every generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationSettings {
    pub seed: u32,
    pub max_output_tokens: u32,
    pub context_tokens: u32,
    pub instruction: String,
}

/// The JSON schema the runtime must fill: exactly the declared fields, all required.
pub fn output_schema(outputs: &[OutputField]) -> Value {
    let mut properties = Map::new();
    for output in outputs {
        let property = match &output.kind {
            OutputKind::Float64 { minimum, maximum } => {
                json!({"type": "number", "minimum": minimum, "maximum": maximum})
            }
            OutputKind::Category { labels } => json!({"type": "string", "enum": labels}),
        };
        properties.insert(output.name.clone(), property);
    }
    json!({
        "type": "object",
        "properties": properties,
        "required": outputs.iter().map(|output| output.name.clone()).collect::<Vec<_>>(),
        "additionalProperties": false
    })
}

/// The prompt for one row: the Owner's instruction and the row's bound values only.
pub fn prompt(instruction: &str, fields: &[String], values: &[f64]) -> String {
    let inputs = fields
        .iter()
        .zip(values)
        .map(|(field, value)| (field.clone(), json!(value)))
        .collect::<Map<_, _>>();
    format!(
        "{instruction}\n\nINPUT_JSON: {}\n\nAnswer with one JSON object that has exactly the required fields and nothing else.",
        Value::Object(inputs)
    )
}

/// Runs one generation and returns the model's answer text. Runtime failures are errors; whether
/// the text satisfies the declared schema is decided by [`parse_output`].
pub fn generate(
    endpoint: &LoopbackEndpoint,
    model_name: &str,
    settings: &GenerationSettings,
    outputs: &[OutputField],
    prompt_text: &str,
    timeout: Duration,
) -> Result<String, RuntimeError> {
    let body = json!({
        "model": model_name,
        "prompt": prompt_text,
        "stream": false,
        "format": output_schema(outputs),
        "options": {
            "temperature": 0,
            "top_k": 1,
            "top_p": 1,
            "seed": settings.seed,
            "num_predict": settings.max_output_tokens,
            "num_ctx": settings.context_tokens
        }
    });
    let call = RuntimeCall::Generate;
    let response = http::send(
        endpoint,
        call.method(),
        call.path(),
        Some(&serde_json::to_vec(&body).expect("generation request serializes")),
        &HttpLimits {
            timeout,
            max_response_bytes: MAX_GENERATION_BYTES,
        },
    )?;
    let document: Value = serde_json::from_slice(&response)
        .map_err(|_| RuntimeError::Malformed("generation response is not JSON".into()))?;
    if document.get("done").and_then(Value::as_bool) != Some(true) {
        return Err(RuntimeError::Malformed(
            "generation response is not complete".into(),
        ));
    }
    if document.get("model").and_then(Value::as_str) != Some(model_name) {
        return Err(RuntimeError::Malformed(
            "generation response names a different model".into(),
        ));
    }
    document
        .get("response")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| RuntimeError::Malformed("generation response has no text".into()))
}

/// A validated output value.
#[derive(Clone, Debug, PartialEq)]
pub enum OutputValue {
    Number(f64),
    Label(String),
}

/// Validates answer text against the declared outputs: exactly the declared fields, finite numbers
/// within bounds, and declared labels only.
pub fn parse_output(text: &str, outputs: &[OutputField]) -> Result<Vec<OutputValue>, RowReason> {
    let object: BTreeMap<String, Value> =
        serde_json::from_str(text.trim()).map_err(|_| RowReason::SchemaMismatch)?;
    if object.len() != outputs.len() {
        return Err(RowReason::SchemaMismatch);
    }
    outputs
        .iter()
        .map(|output| {
            let value = object.get(&output.name).ok_or(RowReason::SchemaMismatch)?;
            match &output.kind {
                OutputKind::Float64 { minimum, maximum } => {
                    let number = value.as_f64().ok_or(RowReason::SchemaMismatch)?;
                    if !number.is_finite() {
                        return Err(RowReason::NonFiniteOutput);
                    }
                    if number < *minimum || number > *maximum {
                        return Err(RowReason::OutOfRange);
                    }
                    Ok(OutputValue::Number(number))
                }
                OutputKind::Category { labels } => {
                    let label = value.as_str().ok_or(RowReason::SchemaMismatch)?;
                    if !labels.iter().any(|known| known == label) {
                        return Err(RowReason::SchemaMismatch);
                    }
                    Ok(OutputValue::Label(label.into()))
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::ollama::{MockBehavior, MockOllama, FIXTURE_DIGEST, FIXTURE_MODEL};

    fn outputs() -> Vec<OutputField> {
        vec![
            OutputField {
                name: "direction".into(),
                kind: OutputKind::Category {
                    labels: vec!["UP".into(), "DOWN".into()],
                },
            },
            OutputField {
                name: "confidence".into(),
                kind: OutputKind::Float64 {
                    minimum: -1.0,
                    maximum: 1.0,
                },
            },
        ]
    }

    #[test]
    fn gate_typed_runtime_adapters_ollama_output_must_match_the_declared_schema() {
        let outputs = outputs();
        assert_eq!(
            parse_output(r#"{"direction":"UP","confidence":0.25}"#, &outputs),
            Ok(vec![
                OutputValue::Label("UP".into()),
                OutputValue::Number(0.25)
            ])
        );
        for (text, reason) in [
            (r#"{"direction":"UP"}"#, RowReason::SchemaMismatch),
            (
                r#"{"direction":"UP","confidence":0.2,"position_size":1}"#,
                RowReason::SchemaMismatch,
            ),
            (
                r#"{"direction":"SIDEWAYS","confidence":0.2}"#,
                RowReason::SchemaMismatch,
            ),
            (
                r#"{"direction":"UP","confidence":"high"}"#,
                RowReason::SchemaMismatch,
            ),
            (
                r#"{"direction":"UP","confidence":3.5}"#,
                RowReason::OutOfRange,
            ),
            (
                r#"{"direction":"UP","confidence":1e400}"#,
                RowReason::SchemaMismatch,
            ),
            ("Buy now!", RowReason::SchemaMismatch),
            (r#"[1,2]"#, RowReason::SchemaMismatch),
        ] {
            assert_eq!(parse_output(text, &outputs), Err(reason), "{text}");
        }
    }

    #[test]
    fn gate_typed_runtime_adapters_ollama_requests_fixed_decoding_and_exact_schema() {
        let schema = output_schema(&outputs());
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(schema["required"], json!(["direction", "confidence"]));
        let text = prompt("Classify.", &["fast".into(), "slow".into()], &[1.5, -0.25]);
        assert!(text.contains(r#"INPUT_JSON: {"fast":1.5,"slow":-0.25}"#));
        assert!(
            !text.contains("20"),
            "no dates or row positions reach the prompt"
        );

        let mock = MockOllama::start(MockBehavior::Normal);
        let settings = GenerationSettings {
            seed: 42,
            max_output_tokens: 64,
            context_tokens: 2048,
            instruction: "Classify.".into(),
        };
        let answer = generate(
            &mock.endpoint(),
            FIXTURE_MODEL,
            &settings,
            &outputs(),
            &text,
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(parse_output(&answer, &outputs()).is_ok(), "{answer}");
        assert_eq!(mock.requests(), vec!["POST /api/generate".to_owned()]);
    }

    #[test]
    fn gate_model_discovery_listing_parses_digests_and_rejects_malformed_listings() {
        let mock = MockOllama::start(MockBehavior::Normal);
        let listing = list_models(&mock.endpoint()).unwrap();
        assert_eq!(listing.len(), 1);
        assert_eq!(listing[0].name, FIXTURE_MODEL);
        assert_eq!(listing[0].digest, FIXTURE_DIGEST);
        assert_eq!(listing[0].family.as_deref(), Some("synthetic"));

        mock.set_behavior(MockBehavior::MalformedListing);
        assert!(matches!(
            list_models(&mock.endpoint()),
            Err(RuntimeError::Malformed(_))
        ));
        assert!(parse_listing(br#"{"models":[{"name":"../evil","digest":"00"}]}"#).is_err());
        assert!(parse_listing(br#"{"models":"none"}"#).is_err());
        assert_eq!(parse_listing(br#"{"models":[]}"#).unwrap(), vec![]);
    }
}
