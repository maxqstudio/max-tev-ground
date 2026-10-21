//! Labeled synthetic ONNX fixture models, encoded directly as ONNX protobuf bytes.
//!
//! Their weights are closed-form numbers chosen for hand-checkable tests. They were never trained
//! and say nothing about any market.

pub const FIXTURE_PRODUCER: &str = "rg-synthetic-fixture";

#[derive(Default)]
struct Writer(Vec<u8>);

impl Writer {
    fn varint(&mut self, mut value: u64) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                self.0.push(byte);
                return;
            }
            self.0.push(byte | 0x80);
        }
    }

    fn key(&mut self, field: u32, wire: u8) {
        self.varint((u64::from(field) << 3) | u64::from(wire));
    }

    fn int(&mut self, field: u32, value: i64) {
        self.key(field, 0);
        self.varint(value as u64);
    }

    fn bytes(&mut self, field: u32, value: &[u8]) {
        self.key(field, 2);
        self.varint(value.len() as u64);
        self.0.extend_from_slice(value);
    }

    fn text(&mut self, field: u32, value: &str) {
        self.bytes(field, value.as_bytes());
    }

    fn float(&mut self, field: u32, value: f32) {
        self.key(field, 5);
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    fn message(&mut self, field: u32, build: impl FnOnce(&mut Writer)) {
        let mut inner = Writer::default();
        build(&mut inner);
        self.bytes(field, &inner.0);
    }
}

#[derive(Clone, Debug)]
pub enum FixtureAttribute {
    Int(i64),
    Float(f32),
}

#[derive(Clone, Debug)]
pub struct FixtureNode {
    op_type: String,
    inputs: Vec<String>,
    output: String,
    attributes: Vec<(String, FixtureAttribute)>,
}

impl FixtureNode {
    pub fn new(op_type: &str, inputs: &[&str], output: &str) -> Self {
        Self {
            op_type: op_type.into(),
            inputs: inputs.iter().map(|input| (*input).to_owned()).collect(),
            output: output.into(),
            attributes: Vec::new(),
        }
    }

    pub fn attribute(mut self, name: &str, value: FixtureAttribute) -> Self {
        self.attributes.push((name.into(), value));
        self
    }
}

#[derive(Clone, Debug)]
enum FixturePayload {
    Floats(Vec<f32>),
    Ints(Vec<i64>),
    External,
}

#[derive(Clone, Debug)]
struct FixtureTensor {
    name: String,
    dims: Vec<i64>,
    payload: FixturePayload,
}

/// A small graph with one FLOAT input named `input` of shape `[1, width]` and one FLOAT output
/// named `output`.
#[derive(Clone, Debug)]
pub struct FixtureGraph {
    width: i64,
    opset: i64,
    initializers: Vec<FixtureTensor>,
    nodes: Vec<FixtureNode>,
}

impl FixtureGraph {
    pub fn new(width: usize) -> Self {
        Self {
            width: width as i64,
            opset: 17,
            initializers: Vec::new(),
            nodes: Vec::new(),
        }
    }

    pub fn opset(mut self, opset: i64) -> Self {
        self.opset = opset;
        self
    }

    pub fn floats(mut self, name: &str, dims: &[i64], values: &[f32]) -> Self {
        self.initializers.push(FixtureTensor {
            name: name.into(),
            dims: dims.to_vec(),
            payload: FixturePayload::Floats(values.to_vec()),
        });
        self
    }

    pub fn ints(mut self, name: &str, dims: &[i64], values: &[i64]) -> Self {
        self.initializers.push(FixtureTensor {
            name: name.into(),
            dims: dims.to_vec(),
            payload: FixturePayload::Ints(values.to_vec()),
        });
        self
    }

    fn external(mut self, name: &str, dims: &[i64]) -> Self {
        self.initializers.push(FixtureTensor {
            name: name.into(),
            dims: dims.to_vec(),
            payload: FixturePayload::External,
        });
        self
    }

    pub fn node(mut self, node: FixtureNode) -> Self {
        self.nodes.push(node);
        self
    }

    /// Encodes the graph as an ONNX model whose output `output` has the given shape.
    pub fn encode(&self, output_dims: &[i64]) -> Vec<u8> {
        let value_info = |writer: &mut Writer, name: &str, dims: &[i64]| {
            writer.text(1, name);
            writer.message(2, |type_proto| {
                type_proto.message(1, |tensor| {
                    tensor.int(1, 1);
                    tensor.message(2, |shape| {
                        for dim in dims {
                            shape.message(1, |dimension| dimension.int(1, *dim));
                        }
                    });
                });
            });
        };
        let mut model = Writer::default();
        model.int(1, 8);
        model.text(2, FIXTURE_PRODUCER);
        model.message(7, |graph| {
            for node in &self.nodes {
                graph.message(1, |writer| {
                    for input in &node.inputs {
                        writer.text(1, input);
                    }
                    writer.text(2, &node.output);
                    writer.text(4, &node.op_type);
                    for (name, value) in &node.attributes {
                        writer.message(5, |attribute| {
                            attribute.text(1, name);
                            match value {
                                FixtureAttribute::Int(value) => {
                                    attribute.int(3, *value);
                                    attribute.int(20, 2);
                                }
                                FixtureAttribute::Float(value) => {
                                    attribute.float(2, *value);
                                    attribute.int(20, 1);
                                }
                            }
                        });
                    }
                });
            }
            graph.text(2, "synthetic_fixture");
            for tensor in &self.initializers {
                graph.message(5, |writer| {
                    for dim in &tensor.dims {
                        writer.int(1, *dim);
                    }
                    match &tensor.payload {
                        FixturePayload::Floats(values) => {
                            writer.int(2, 1);
                            writer.text(8, &tensor.name);
                            let raw = values
                                .iter()
                                .flat_map(|value| value.to_le_bytes())
                                .collect::<Vec<_>>();
                            writer.bytes(9, &raw);
                        }
                        FixturePayload::Ints(values) => {
                            writer.int(2, 7);
                            writer.text(8, &tensor.name);
                            for value in values {
                                writer.int(7, *value);
                            }
                        }
                        FixturePayload::External => {
                            writer.int(2, 1);
                            writer.text(8, &tensor.name);
                            writer.message(13, |entry| {
                                entry.text(1, "location");
                                entry.text(2, "../../outside/weights.bin");
                            });
                            writer.int(14, 1);
                        }
                    }
                });
            }
            graph.message(11, |writer| value_info(writer, "input", &[1, self.width]));
            graph.message(12, |writer| value_info(writer, "output", output_dims));
        });
        model.message(8, |opset| {
            opset.text(1, "");
            opset.int(2, self.opset);
        });
        model.0
    }
}

/// `output = input · W + b` with `W` of shape `[width, outputs]` in row-major order.
pub fn linear_model(width: usize, weights: &[f32], bias: &[f32]) -> Vec<u8> {
    let outputs = bias.len();
    assert_eq!(
        weights.len(),
        width * outputs,
        "weights must be [width, outputs]"
    );
    FixtureGraph::new(width)
        .floats("W", &[width as i64, outputs as i64], weights)
        .floats("b", &[outputs as i64], bias)
        .node(FixtureNode::new("Gemm", &["input", "W", "b"], "output"))
        .encode(&[1, outputs as i64])
}

/// A two-layer perceptron with a three-class softmax head and closed-form weights.
pub fn classifier_model(width: usize) -> Vec<u8> {
    let hidden = 4usize;
    let w1 = (0..width * hidden)
        .map(|index| ((index * 7 % 11) as f32 - 5.0) / 8.0)
        .collect::<Vec<_>>();
    let b1 = (0..hidden)
        .map(|index| index as f32 / 10.0 - 0.15)
        .collect::<Vec<_>>();
    let w2 = (0..hidden * 3)
        .map(|index| ((index * 5 % 7) as f32 - 3.0) / 4.0)
        .collect::<Vec<_>>();
    let b2 = [0.05f32, -0.05, 0.0];
    FixtureGraph::new(width)
        .floats("W1", &[width as i64, hidden as i64], &w1)
        .floats("b1", &[hidden as i64], &b1)
        .floats("W2", &[hidden as i64, 3], &w2)
        .floats("b2", &[3], &b2)
        .node(FixtureNode::new("Gemm", &["input", "W1", "b1"], "hidden"))
        .node(FixtureNode::new("Relu", &["hidden"], "activated"))
        .node(FixtureNode::new(
            "Gemm",
            &["activated", "W2", "b2"],
            "logits",
        ))
        .node(
            FixtureNode::new("Softmax", &["logits"], "output")
                .attribute("axis", FixtureAttribute::Int(1)),
        )
        .encode(&[1, 3])
}

/// A model whose weights live in an external file referenced by a relative path. The native
/// adapter must refuse it without touching the file system.
pub fn external_data_model() -> Vec<u8> {
    FixtureGraph::new(2)
        .external("W", &[2, 1])
        .node(FixtureNode::new("MatMul", &["input", "W"], "output"))
        .encode(&[1, 1])
}
