//! Test-only fixtures: labeled synthetic ONNX models and a loopback mock Ollama runtime. Compiled
//! only for tests or with the `test-fixtures` feature; nothing here is market data or a trained
//! model.

pub mod ollama;
pub mod onnx;
