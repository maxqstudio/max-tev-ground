//! Native, bounded ONNX interpreter for small numeric research models.
//!
//! The adapter accepts one FLOAT input of shape `[N]` or `[1|batch, N]` and FLOAT outputs, and
//! evaluates a documented operator subset ([`SUPPORTED_OPERATORS`]) in a fixed sequential order,
//! so a replay on the same build reproduces every output bit. Shapes, parameter counts, and the
//! per-row operation count are resolved and checked against [`OnnxLimits`] when the model loads;
//! a model outside the subset or the limits is refused with a diagnostic naming the reason. The
//! interpreter reads nothing but the model bytes: external data, functions, and subgraphs are
//! rejected by the decoder.

pub mod proto;

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use proto::{AttributeValue, Dim, Model, Node, Tensor, DATA_TYPE_FLOAT, DATA_TYPE_INT64};

pub const SUPPORTED_OPERATORS: &[&str] = &[
    "Add",
    "Clip",
    "Concat",
    "Div",
    "Flatten",
    "Gemm",
    "Identity",
    "LeakyRelu",
    "MatMul",
    "Mul",
    "Relu",
    "Reshape",
    "Sigmoid",
    "Softmax",
    "Sub",
    "Tanh",
];
pub const MIN_OPSET: i64 = 7;
pub const MAX_OPSET: i64 = 21;
pub const MAX_RANK: usize = 4;

#[derive(Debug, Error, Clone, PartialEq)]
pub enum OnnxError {
    #[error("ONNX model is malformed: {0}")]
    Malformed(String),
    #[error("ONNX model is outside the supported subset: {0}")]
    Unsupported(String),
    #[error("ONNX model exceeds a resource limit: {0}")]
    Limit(String),
    #[error("ONNX model input is invalid: {0}")]
    Input(String),
}

/// Load-time bounds. Every model is checked against them before it can be registered or run.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct OnnxLimits {
    pub max_nodes: usize,
    pub max_parameters: u64,
    pub max_tensor_elements: u64,
    pub max_operations_per_row: u64,
}

pub const DEFAULT_ONNX_LIMITS: OnnxLimits = OnnxLimits {
    max_nodes: 4096,
    max_parameters: 16 * 1024 * 1024,
    max_tensor_elements: 1024 * 1024,
    max_operations_per_row: 64 * 1024 * 1024,
};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OnnxTensorInfo {
    pub name: String,
    pub shape: Vec<u64>,
    pub width: u64,
}

/// What a loaded model declares, as reported to discovery and recorded in its identity.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OnnxModelInfo {
    pub ir_version: i64,
    pub opset: i64,
    pub producer_name: String,
    pub input: OnnxTensorInfo,
    pub outputs: Vec<OnnxTensorInfo>,
    pub operators: BTreeMap<String, u64>,
    pub parameter_count: u64,
    pub operations_per_row: u64,
}

impl OnnxModelInfo {
    pub fn output_width(&self) -> u64 {
        self.outputs.iter().map(|output| output.width).sum()
    }
}

#[derive(Clone, Debug)]
struct Value {
    shape: Vec<usize>,
    data: Vec<f32>,
}

#[derive(Clone, Debug)]
enum Op {
    Gemm {
        alpha: f32,
        beta: f32,
        trans_a: bool,
        trans_b: bool,
    },
    MatMul,
    Binary(BinaryOp),
    Unary(UnaryOp),
    Softmax {
        axis: usize,
        legacy: bool,
    },
    Reshape,
    Concat {
        axis: usize,
    },
    Clip {
        min: f32,
        max: f32,
    },
}

#[derive(Clone, Copy, Debug)]
enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
}

#[derive(Clone, Copy, Debug)]
enum UnaryOp {
    Identity,
    Relu,
    Sigmoid,
    Tanh,
    LeakyRelu(f32),
}

#[derive(Clone, Debug)]
struct Step {
    op: Op,
    inputs: Vec<usize>,
    output: usize,
    shape: Vec<usize>,
}

/// A validated model with its resolved execution plan.
#[derive(Clone, Debug)]
pub struct NativeOnnxModel {
    info: OnnxModelInfo,
    constants: Vec<Option<Value>>,
    input_slot: usize,
    input_shape: Vec<usize>,
    steps: Vec<Step>,
    outputs: Vec<usize>,
}

fn unsupported(reason: impl Into<String>) -> OnnxError {
    OnnxError::Unsupported(reason.into())
}

fn malformed(reason: impl Into<String>) -> OnnxError {
    OnnxError::Malformed(reason.into())
}

fn elements(shape: &[usize]) -> u64 {
    shape.iter().map(|dim| *dim as u64).product()
}

fn attribute<'a>(node: &'a Node, name: &str) -> Option<&'a AttributeValue> {
    node.attributes
        .iter()
        .find(|attribute| attribute.name == name)
        .map(|attribute| &attribute.value)
}

fn int_attribute(node: &Node, name: &str, default: i64) -> Result<i64, OnnxError> {
    match attribute(node, name) {
        None => Ok(default),
        Some(AttributeValue::Int(value)) => Ok(*value),
        Some(_) => Err(malformed(format!(
            "{} attribute {name} is not an integer",
            node.op_type
        ))),
    }
}

fn float_attribute(node: &Node, name: &str, default: f32) -> Result<f32, OnnxError> {
    match attribute(node, name) {
        None => Ok(default),
        Some(AttributeValue::Float(value)) if value.is_finite() => Ok(*value),
        Some(_) => Err(malformed(format!(
            "{} attribute {name} is not a finite float",
            node.op_type
        ))),
    }
}

fn normalize_axis(axis: i64, rank: usize, op: &str) -> Result<usize, OnnxError> {
    let rank_i = rank as i64;
    let resolved = if axis < 0 { axis + rank_i } else { axis };
    if !(0..rank_i).contains(&resolved) {
        return Err(malformed(format!(
            "{op} axis {axis} is outside rank {rank}"
        )));
    }
    Ok(resolved as usize)
}

/// Multidirectional (NumPy-style) broadcast shape of two operands.
fn broadcast_shape(left: &[usize], right: &[usize]) -> Option<Vec<usize>> {
    let rank = left.len().max(right.len());
    let mut shape = vec![0; rank];
    for index in 0..rank {
        let l = if index + left.len() >= rank {
            left[index + left.len() - rank]
        } else {
            1
        };
        let r = if index + right.len() >= rank {
            right[index + right.len() - rank]
        } else {
            1
        };
        shape[index] = match (l, r) {
            (l, r) if l == r => l,
            (1, r) => r,
            (l, 1) => l,
            _ => return None,
        };
    }
    Some(shape)
}

/// Index of the source element that broadcasts to `target` position `flat` in `shape`.
fn broadcast_index(flat: usize, shape: &[usize], source: &[usize]) -> usize {
    let mut remaining = flat;
    let mut index = 0;
    let mut stride = 1;
    let offset = shape.len() - source.len();
    for axis in (0..shape.len()).rev() {
        let coordinate = remaining % shape[axis];
        remaining /= shape[axis];
        if axis >= offset {
            let dim = source[axis - offset];
            if dim != 1 {
                index += coordinate * stride;
            }
            stride *= dim;
        }
    }
    index
}

impl NativeOnnxModel {
    pub fn load(bytes: &[u8], limits: &OnnxLimits) -> Result<Self, OnnxError> {
        let model = proto::decode_model(bytes)?;
        Self::from_model(model, limits)
    }

    pub fn info(&self) -> &OnnxModelInfo {
        &self.info
    }

    fn from_model(model: Model, limits: &OnnxLimits) -> Result<Self, OnnxError> {
        if model.ir_version < 3 {
            return Err(unsupported(format!(
                "IR version {} predates opset imports",
                model.ir_version
            )));
        }
        let mut opset = None;
        for (domain, version) in &model.opsets {
            match domain.as_str() {
                "" | "ai.onnx" => opset = Some(*version),
                other => {
                    return Err(unsupported(format!(
                        "operator domain {other:?} is not supported"
                    )))
                }
            }
        }
        let opset = opset.ok_or_else(|| malformed("model imports no default operator set"))?;
        if !(MIN_OPSET..=MAX_OPSET).contains(&opset) {
            return Err(unsupported(format!(
                "opset {opset} is outside the supported range {MIN_OPSET}..={MAX_OPSET}"
            )));
        }
        let graph = model.graph;
        if graph.nodes.len() > limits.max_nodes {
            return Err(OnnxError::Limit(format!(
                "{} nodes exceed the limit of {}",
                graph.nodes.len(),
                limits.max_nodes
            )));
        }

        let mut slots: HashMap<String, usize> = HashMap::new();
        let mut shapes: Vec<Vec<usize>> = Vec::new();
        let mut constants: Vec<Option<Value>> = Vec::new();
        let mut int_constants: HashMap<usize, Vec<i64>> = HashMap::new();
        let mut parameter_count = 0u64;
        fn new_slot(
            name: &str,
            shape: Vec<usize>,
            value: Option<Value>,
            slots: &mut HashMap<String, usize>,
            shapes: &mut Vec<Vec<usize>>,
            constants: &mut Vec<Option<Value>>,
        ) -> Result<usize, OnnxError> {
            if name.is_empty() || slots.contains_key(name) {
                return Err(malformed(format!(
                    "value name {name:?} is empty or defined more than once"
                )));
            }
            let slot = shapes.len();
            slots.insert(name.to_owned(), slot);
            shapes.push(shape);
            constants.push(value);
            Ok(slot)
        }

        for tensor in graph.initializers {
            let shape = tensor
                .dims
                .iter()
                .map(|dim| *dim as usize)
                .collect::<Vec<_>>();
            if shape.len() > MAX_RANK {
                return Err(unsupported(format!(
                    "initializer {} has rank {} above {MAX_RANK}",
                    tensor.name,
                    shape.len()
                )));
            }
            parameter_count = parameter_count.saturating_add(elements(&shape));
            if parameter_count > limits.max_parameters {
                return Err(OnnxError::Limit(format!(
                    "{parameter_count} parameters exceed the limit of {}",
                    limits.max_parameters
                )));
            }
            let Tensor {
                name,
                data_type,
                floats,
                int64s,
                ..
            } = tensor;
            if data_type == DATA_TYPE_INT64 {
                let slot = new_slot(&name, shape, None, &mut slots, &mut shapes, &mut constants)?;
                int_constants.insert(slot, int64s);
            } else {
                let value = Value {
                    shape: shape.clone(),
                    data: floats,
                };
                new_slot(
                    &name,
                    shape,
                    Some(value),
                    &mut slots,
                    &mut shapes,
                    &mut constants,
                )?;
            }
        }

        let runtime_inputs = graph
            .inputs
            .iter()
            .filter(|input| !slots.contains_key(&input.name))
            .collect::<Vec<_>>();
        let [input] = runtime_inputs.as_slice() else {
            return Err(unsupported(format!(
                "exactly one runtime input is supported; the model declares {}",
                runtime_inputs.len()
            )));
        };
        if input.elem_type != DATA_TYPE_FLOAT {
            return Err(unsupported(format!(
                "input {} has element type {}; only FLOAT is supported",
                input.name, input.elem_type
            )));
        }
        let declared = input
            .dims
            .as_ref()
            .ok_or_else(|| unsupported(format!("input {} has no declared shape", input.name)))?;
        let input_shape = match declared.as_slice() {
            [Dim::Value(width)] if *width > 0 => vec![*width as usize],
            [Dim::Value(1) | Dim::Symbolic, Dim::Value(width)] if *width > 0 => {
                vec![1, *width as usize]
            }
            _ => {
                return Err(unsupported(format!(
                    "input {} must have shape [N] or [batch, N] with a fixed N",
                    input.name
                )))
            }
        };
        let input_name = input.name.clone();
        let input_slot = new_slot(
            &input_name,
            input_shape.clone(),
            None,
            &mut slots,
            &mut shapes,
            &mut constants,
        )?;

        let mut operators = BTreeMap::new();
        let mut operations = 0u64;
        let mut steps = Vec::with_capacity(graph.nodes.len());
        for node in &graph.nodes {
            if !node.domain.is_empty() && node.domain != "ai.onnx" {
                return Err(unsupported(format!(
                    "operator {}::{} is not supported",
                    node.domain, node.op_type
                )));
            }
            if !SUPPORTED_OPERATORS.contains(&node.op_type.as_str()) {
                return Err(unsupported(format!(
                    "operator {} is not in the supported subset ({})",
                    node.op_type,
                    SUPPORTED_OPERATORS.join(", ")
                )));
            }
            *operators.entry(node.op_type.clone()).or_insert(0u64) += 1;
            let [output_name] = node.outputs.as_slice() else {
                return Err(unsupported(format!(
                    "{} must have exactly one output",
                    node.op_type
                )));
            };
            let resolve = |name: &String| {
                slots.get(name).copied().ok_or_else(|| {
                    malformed(format!(
                        "{} reads {name:?} before it is defined (nodes must be topologically sorted)",
                        node.op_type
                    ))
                })
            };
            let present = node
                .inputs
                .iter()
                .filter(|name| !name.is_empty())
                .map(resolve)
                .collect::<Result<Vec<_>, _>>()?;
            let shape_of = |slot: usize| shapes[slot].clone();
            let float_operand = |slot: usize| -> Result<(), OnnxError> {
                if int_constants.contains_key(&slot) {
                    return Err(unsupported(format!(
                        "{} reads an INT64 tensor as data",
                        node.op_type
                    )));
                }
                Ok(())
            };
            let arity = |min: usize, max: usize| -> Result<(), OnnxError> {
                if node.inputs.len() < min || node.inputs.len() > max || present.len() < min {
                    return Err(malformed(format!(
                        "{} has {} inputs; expected {min}..={max}",
                        node.op_type,
                        node.inputs.len()
                    )));
                }
                Ok(())
            };
            let (op, inputs, shape, cost) = match node.op_type.as_str() {
                "Gemm" => {
                    arity(2, 3)?;
                    present.iter().try_for_each(|slot| float_operand(*slot))?;
                    let (a, b) = (shape_of(present[0]), shape_of(present[1]));
                    let trans_a = int_attribute(node, "transA", 0)? != 0;
                    let trans_b = int_attribute(node, "transB", 0)? != 0;
                    if a.len() != 2 || b.len() != 2 {
                        return Err(malformed("Gemm operands must be rank 2"));
                    }
                    let (m, k) = if trans_a { (a[1], a[0]) } else { (a[0], a[1]) };
                    let (k2, n) = if trans_b { (b[1], b[0]) } else { (b[0], b[1]) };
                    if k != k2 {
                        return Err(malformed(format!(
                            "Gemm inner dimensions differ ({k} and {k2})"
                        )));
                    }
                    if let Some(c) = present.get(2) {
                        let c_shape = shape_of(*c);
                        if broadcast_shape(&[m, n], &c_shape).as_deref() != Some(&[m, n][..]) {
                            return Err(malformed("Gemm bias does not broadcast to the output"));
                        }
                    }
                    let op = Op::Gemm {
                        alpha: float_attribute(node, "alpha", 1.0)?,
                        beta: float_attribute(node, "beta", 1.0)?,
                        trans_a,
                        trans_b,
                    };
                    let cost = (m as u64) * (n as u64) * (k as u64 + 1);
                    (op, present.clone(), vec![m, n], cost)
                }
                "MatMul" => {
                    arity(2, 2)?;
                    present.iter().try_for_each(|slot| float_operand(*slot))?;
                    let (a, b) = (shape_of(present[0]), shape_of(present[1]));
                    let shape = match (a.as_slice(), b.as_slice()) {
                        ([k], [k2, n]) if k == k2 => vec![*n],
                        ([m, k], [k2, n]) if k == k2 => vec![*m, *n],
                        _ => {
                            return Err(unsupported(format!(
                                "MatMul supports [K]x[K,N] and [M,K]x[K,N]; got {a:?}x{b:?}"
                            )))
                        }
                    };
                    let k = a[a.len() - 1] as u64;
                    let cost = elements(&shape) * k;
                    (Op::MatMul, present.clone(), shape, cost)
                }
                "Add" | "Sub" | "Mul" | "Div" => {
                    arity(2, 2)?;
                    present.iter().try_for_each(|slot| float_operand(*slot))?;
                    let (a, b) = (shape_of(present[0]), shape_of(present[1]));
                    let shape = broadcast_shape(&a, &b).ok_or_else(|| {
                        malformed(format!(
                            "{} operands {a:?} and {b:?} do not broadcast",
                            node.op_type
                        ))
                    })?;
                    let op = match node.op_type.as_str() {
                        "Add" => BinaryOp::Add,
                        "Sub" => BinaryOp::Sub,
                        "Mul" => BinaryOp::Mul,
                        _ => BinaryOp::Div,
                    };
                    let cost = elements(&shape);
                    (Op::Binary(op), present.clone(), shape, cost)
                }
                "Identity" | "Relu" | "Sigmoid" | "Tanh" | "LeakyRelu" => {
                    arity(1, 1)?;
                    float_operand(present[0])?;
                    let op = match node.op_type.as_str() {
                        "Identity" => UnaryOp::Identity,
                        "Relu" => UnaryOp::Relu,
                        "Sigmoid" => UnaryOp::Sigmoid,
                        "Tanh" => UnaryOp::Tanh,
                        _ => UnaryOp::LeakyRelu(float_attribute(node, "alpha", 0.01)?),
                    };
                    let shape = shape_of(present[0]);
                    let cost = elements(&shape);
                    (Op::Unary(op), present.clone(), shape, cost)
                }
                "Softmax" => {
                    arity(1, 1)?;
                    float_operand(present[0])?;
                    let shape = shape_of(present[0]);
                    let legacy = opset < 13;
                    let axis = normalize_axis(
                        int_attribute(node, "axis", if legacy { 1 } else { -1 })?,
                        shape.len(),
                        "Softmax",
                    )?;
                    let cost = elements(&shape) * 3;
                    (Op::Softmax { axis, legacy }, present.clone(), shape, cost)
                }
                "Flatten" => {
                    arity(1, 1)?;
                    float_operand(present[0])?;
                    let input_shape = shape_of(present[0]);
                    let axis = int_attribute(node, "axis", 1)?;
                    let rank = input_shape.len() as i64;
                    let axis = if axis < 0 { axis + rank } else { axis };
                    if !(0..=rank).contains(&axis) {
                        return Err(malformed("Flatten axis is outside the input rank"));
                    }
                    let axis = axis as usize;
                    let shape = vec![
                        input_shape[..axis].iter().product(),
                        input_shape[axis..].iter().product(),
                    ];
                    (Op::Reshape, vec![present[0]], shape, 0)
                }
                "Reshape" => {
                    let input_shape = if opset >= 5 {
                        arity(2, 2)?;
                        float_operand(present[0])?;
                        shape_of(present[0])
                    } else {
                        return Err(unsupported("Reshape before opset 5 is not supported"));
                    };
                    let target = int_constants.get(&present[1]).ok_or_else(|| {
                        unsupported("Reshape requires its shape operand to be an INT64 initializer")
                    })?;
                    let allow_zero = int_attribute(node, "allowzero", 0)? != 0;
                    let mut shape = Vec::with_capacity(target.len());
                    let mut inferred = None;
                    for (index, dim) in target.iter().enumerate() {
                        match *dim {
                            -1 if inferred.is_none() => {
                                inferred = Some(index);
                                shape.push(1);
                            }
                            0 if !allow_zero => {
                                shape.push(*input_shape.get(index).ok_or_else(|| {
                                    malformed("Reshape copies a missing dimension")
                                })?)
                            }
                            value if value > 0 => shape.push(value as usize),
                            _ => return Err(malformed("Reshape shape operand is invalid")),
                        }
                    }
                    let total = elements(&input_shape);
                    if let Some(index) = inferred {
                        let known = elements(&shape);
                        if known == 0 || !total.is_multiple_of(known) {
                            return Err(malformed("Reshape cannot infer its -1 dimension"));
                        }
                        shape[index] = (total / known) as usize;
                    }
                    if elements(&shape) != total {
                        return Err(malformed("Reshape changes the element count"));
                    }
                    (Op::Reshape, vec![present[0]], shape, 0)
                }
                "Concat" => {
                    if present.is_empty() || present.len() != node.inputs.len() {
                        return Err(malformed("Concat needs one or more inputs"));
                    }
                    present.iter().try_for_each(|slot| float_operand(*slot))?;
                    let first = shape_of(present[0]);
                    let axis = normalize_axis(
                        match attribute(node, "axis") {
                            Some(AttributeValue::Int(value)) => *value,
                            _ => return Err(malformed("Concat requires an integer axis")),
                        },
                        first.len(),
                        "Concat",
                    )?;
                    let mut shape = first.clone();
                    shape[axis] = 0;
                    for slot in &present {
                        let other = shape_of(*slot);
                        let compatible = other.len() == first.len()
                            && other
                                .iter()
                                .zip(&first)
                                .enumerate()
                                .all(|(index, (a, b))| index == axis || a == b);
                        if !compatible {
                            return Err(malformed("Concat operand shapes are incompatible"));
                        }
                        shape[axis] += other[axis];
                    }
                    let cost = elements(&shape);
                    (Op::Concat { axis }, present.clone(), shape, cost)
                }
                "Clip" => {
                    let bound =
                        |position: usize, attr: &str, default: f32| -> Result<f32, OnnxError> {
                            if opset < 11 {
                                return float_attribute(node, attr, default);
                            }
                            match node.inputs.get(position).filter(|name| !name.is_empty()) {
                                None => Ok(default),
                                Some(name) => {
                                    let slot = resolve(name)?;
                                    match &constants[slot] {
                                        Some(value)
                                            if value.data.len() == 1
                                                && value.data[0].is_finite() =>
                                        {
                                            Ok(value.data[0])
                                        }
                                        _ => Err(unsupported(
                                            "Clip bounds must be finite scalar FLOAT initializers",
                                        )),
                                    }
                                }
                            }
                        };
                    if node.inputs.is_empty() || node.inputs[0].is_empty() || node.inputs.len() > 3
                    {
                        return Err(malformed("Clip has an invalid input list"));
                    }
                    float_operand(present[0])?;
                    let min = bound(1, "min", f32::MIN)?;
                    let max = bound(2, "max", f32::MAX)?;
                    if min > max {
                        return Err(malformed("Clip minimum exceeds its maximum"));
                    }
                    let shape = shape_of(present[0]);
                    let cost = elements(&shape);
                    (Op::Clip { min, max }, vec![present[0]], shape, cost)
                }
                other => unreachable!("operator {other} passed the allowlist"),
            };
            if shape.len() > MAX_RANK || elements(&shape) > limits.max_tensor_elements {
                return Err(OnnxError::Limit(format!(
                    "{} produces shape {shape:?}, beyond rank {MAX_RANK} or {} elements",
                    node.op_type, limits.max_tensor_elements
                )));
            }
            operations = operations.saturating_add(cost);
            if operations > limits.max_operations_per_row {
                return Err(OnnxError::Limit(format!(
                    "the model needs more than {} operations per row",
                    limits.max_operations_per_row
                )));
            }
            let output = new_slot(
                output_name,
                shape.clone(),
                None,
                &mut slots,
                &mut shapes,
                &mut constants,
            )?;
            steps.push(Step {
                op,
                inputs,
                output,
                shape,
            });
        }

        if graph.outputs.is_empty() {
            return Err(malformed("model declares no outputs"));
        }
        let mut outputs = Vec::with_capacity(graph.outputs.len());
        let mut output_info = Vec::with_capacity(graph.outputs.len());
        for output in &graph.outputs {
            if output.elem_type != DATA_TYPE_FLOAT {
                return Err(unsupported(format!(
                    "output {} has element type {}; only FLOAT is supported",
                    output.name, output.elem_type
                )));
            }
            let slot = *slots
                .get(&output.name)
                .ok_or_else(|| malformed(format!("output {} is never produced", output.name)))?;
            if slot == input_slot || int_constants.contains_key(&slot) {
                return Err(unsupported(format!(
                    "output {} must be computed by an operator",
                    output.name
                )));
            }
            let shape = shapes[slot].clone();
            outputs.push(slot);
            output_info.push(OnnxTensorInfo {
                name: output.name.clone(),
                width: elements(&shape),
                shape: shape.iter().map(|dim| *dim as u64).collect(),
            });
        }
        let info = OnnxModelInfo {
            ir_version: model.ir_version,
            opset,
            producer_name: model.producer_name,
            input: OnnxTensorInfo {
                name: input_name,
                width: elements(&input_shape),
                shape: input_shape.iter().map(|dim| *dim as u64).collect(),
            },
            outputs: output_info,
            operators,
            parameter_count,
            operations_per_row: operations,
        };
        Ok(Self {
            info,
            constants,
            input_slot,
            input_shape,
            steps,
            outputs,
        })
    }

    /// Evaluates one row. Outputs are returned flattened in declared output order. Non-finite
    /// values are returned as computed; the adapter decides how to record them.
    pub fn run(&self, input: &[f32]) -> Result<Vec<f32>, OnnxError> {
        if input.len() as u64 != self.info.input.width {
            return Err(OnnxError::Input(format!(
                "expected {} input values, received {}",
                self.info.input.width,
                input.len()
            )));
        }
        let mut values: Vec<Option<Value>> = self.constants.clone();
        values[self.input_slot] = Some(Value {
            shape: self.input_shape.clone(),
            data: input.to_vec(),
        });
        for step in &self.steps {
            let operand = |position: usize| -> &Value {
                values[step.inputs[position]]
                    .as_ref()
                    .expect("validated plans only read defined values")
            };
            let data = match &step.op {
                Op::Gemm {
                    alpha,
                    beta,
                    trans_a,
                    trans_b,
                } => gemm(
                    operand(0),
                    operand(1),
                    step.inputs.get(2).map(|_| operand(2)),
                    *alpha,
                    *beta,
                    *trans_a,
                    *trans_b,
                    &step.shape,
                ),
                Op::MatMul => matmul(operand(0), operand(1), &step.shape),
                Op::Binary(op) => binary(*op, operand(0), operand(1), &step.shape),
                Op::Unary(op) => operand(0).data.iter().map(|x| unary(*op, *x)).collect(),
                Op::Softmax { axis, legacy } => softmax(operand(0), *axis, *legacy),
                Op::Reshape => operand(0).data.clone(),
                Op::Concat { axis } => {
                    let parts = (0..step.inputs.len()).map(operand).collect::<Vec<_>>();
                    concat(&parts, *axis, &step.shape)
                }
                Op::Clip { min, max } => operand(0)
                    .data
                    .iter()
                    .map(|x| if x.is_nan() { *x } else { x.clamp(*min, *max) })
                    .collect(),
            };
            values[step.output] = Some(Value {
                shape: step.shape.clone(),
                data,
            });
        }
        let mut result = Vec::with_capacity(self.info.output_width() as usize);
        for slot in &self.outputs {
            result.extend_from_slice(
                &values[*slot]
                    .as_ref()
                    .expect("validated outputs are computed")
                    .data,
            );
        }
        Ok(result)
    }
}

#[allow(clippy::too_many_arguments)]
fn gemm(
    a: &Value,
    b: &Value,
    c: Option<&Value>,
    alpha: f32,
    beta: f32,
    trans_a: bool,
    trans_b: bool,
    shape: &[usize],
) -> Vec<f32> {
    let (m, n) = (shape[0], shape[1]);
    let k = if trans_a { a.shape[0] } else { a.shape[1] };
    let a_at = |i: usize, p: usize| {
        if trans_a {
            a.data[p * a.shape[1] + i]
        } else {
            a.data[i * a.shape[1] + p]
        }
    };
    let b_at = |p: usize, j: usize| {
        if trans_b {
            b.data[j * b.shape[1] + p]
        } else {
            b.data[p * b.shape[1] + j]
        }
    };
    let mut output = Vec::with_capacity(m * n);
    for i in 0..m {
        for j in 0..n {
            let mut sum = 0.0f32;
            for p in 0..k {
                sum += a_at(i, p) * b_at(p, j);
            }
            let mut value = alpha * sum;
            if let Some(c) = c {
                value += beta * c.data[broadcast_index(i * n + j, shape, &c.shape)];
            }
            output.push(value);
        }
    }
    output
}

fn matmul(a: &Value, b: &Value, shape: &[usize]) -> Vec<f32> {
    let k = a.shape[a.shape.len() - 1];
    let n = b.shape[1];
    let m = if a.shape.len() == 1 { 1 } else { a.shape[0] };
    let mut output = Vec::with_capacity(elements(shape) as usize);
    for i in 0..m {
        for j in 0..n {
            let mut sum = 0.0f32;
            for p in 0..k {
                sum += a.data[i * k + p] * b.data[p * n + j];
            }
            output.push(sum);
        }
    }
    output
}

fn binary(op: BinaryOp, a: &Value, b: &Value, shape: &[usize]) -> Vec<f32> {
    (0..elements(shape) as usize)
        .map(|flat| {
            let x = a.data[broadcast_index(flat, shape, &a.shape)];
            let y = b.data[broadcast_index(flat, shape, &b.shape)];
            match op {
                BinaryOp::Add => x + y,
                BinaryOp::Sub => x - y,
                BinaryOp::Mul => x * y,
                BinaryOp::Div => x / y,
            }
        })
        .collect()
}

fn unary(op: UnaryOp, x: f32) -> f32 {
    match op {
        UnaryOp::Identity => x,
        UnaryOp::Relu => {
            if x > 0.0 || x.is_nan() {
                x
            } else {
                0.0
            }
        }
        UnaryOp::Sigmoid => 1.0 / (1.0 + (-x).exp()),
        UnaryOp::Tanh => x.tanh(),
        UnaryOp::LeakyRelu(alpha) => {
            if x >= 0.0 {
                x
            } else {
                alpha * x
            }
        }
    }
}

fn softmax(value: &Value, axis: usize, legacy: bool) -> Vec<f32> {
    // Opset < 13 flattens to [outer, inner] at `axis`; opset >= 13 normalizes along `axis` only.
    let (outer, length, inner) = if legacy {
        (
            value.shape[..axis].iter().product::<usize>(),
            value.shape[axis..].iter().product::<usize>(),
            1,
        )
    } else {
        (
            value.shape[..axis].iter().product::<usize>(),
            value.shape[axis],
            value.shape[axis + 1..].iter().product::<usize>(),
        )
    };
    let mut output = value.data.clone();
    for o in 0..outer {
        for i in 0..inner {
            let at = |position: usize| o * length * inner + position * inner + i;
            let mut maximum = f32::NEG_INFINITY;
            for position in 0..length {
                maximum = maximum.max(value.data[at(position)]);
            }
            let mut sum = 0.0f32;
            for position in 0..length {
                let exponent = (value.data[at(position)] - maximum).exp();
                output[at(position)] = exponent;
                sum += exponent;
            }
            for position in 0..length {
                output[at(position)] /= sum;
            }
        }
    }
    output
}

fn concat(parts: &[&Value], axis: usize, shape: &[usize]) -> Vec<f32> {
    let outer = shape[..axis].iter().product::<usize>();
    let inner = shape[axis + 1..].iter().product::<usize>();
    let mut output = Vec::with_capacity(elements(shape) as usize);
    for o in 0..outer {
        for part in parts {
            let block = part.shape[axis] * inner;
            output.extend_from_slice(&part.data[o * block..(o + 1) * block]);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::onnx::{FixtureGraph, FixtureNode};

    fn load(bytes: &[u8]) -> Result<NativeOnnxModel, OnnxError> {
        NativeOnnxModel::load(bytes, &DEFAULT_ONNX_LIMITS)
    }

    #[test]
    fn gate_typed_runtime_adapters_onnx_linear_model_matches_hand_calculation() {
        // y = [x0 + 2 x1 - 0.5, 3 x0 - x1 + 1]
        let model = load(&crate::fixtures::onnx::linear_model(
            2,
            &[1.0, 3.0, 2.0, -1.0],
            &[-0.5, 1.0],
        ))
        .unwrap();
        assert_eq!(model.info().input.width, 2);
        assert_eq!(model.info().output_width(), 2);
        assert_eq!(model.run(&[1.5, -2.0]).unwrap(), vec![-3.0, 7.5]);
    }

    #[test]
    fn gate_typed_runtime_adapters_onnx_mlp_softmax_is_bit_identical_across_runs() {
        let bytes = crate::fixtures::onnx::classifier_model(3);
        let model = load(&bytes).unwrap();
        let first = model.run(&[0.25, -1.0, 2.0]).unwrap();
        let sum: f32 = first.iter().sum();
        assert!((sum - 1.0).abs() < 1e-6, "softmax output sums to one");
        for _ in 0..5 {
            let again = load(&bytes).unwrap().run(&[0.25, -1.0, 2.0]).unwrap();
            assert_eq!(
                again
                    .iter()
                    .map(|value| value.to_bits())
                    .collect::<Vec<_>>(),
                first
                    .iter()
                    .map(|value| value.to_bits())
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn gate_typed_runtime_adapters_onnx_refuses_unsupported_operators_and_wrong_inputs() {
        let graph = FixtureGraph::new(2).node(FixtureNode::new("Erf", &["input"], "output"));
        let error = load(&graph.encode(&[2])).unwrap_err();
        assert!(
            matches!(&error, OnnxError::Unsupported(reason) if reason.contains("Erf")),
            "{error}"
        );
        let model = load(&crate::fixtures::onnx::linear_model(
            2, &[1.0; 4], &[0.0; 2],
        ))
        .unwrap();
        assert!(matches!(model.run(&[1.0]), Err(OnnxError::Input(_))));
    }

    #[test]
    fn gate_adversarial_tamper_onnx_decoder_fails_closed_on_truncated_and_external_data() {
        let bytes = crate::fixtures::onnx::linear_model(2, &[1.0; 4], &[0.0; 2]);
        for length in [0, 1, bytes.len() / 2, bytes.len() - 1] {
            assert!(load(&bytes[..length]).is_err(), "truncated at {length}");
        }
        let external = crate::fixtures::onnx::external_data_model();
        let error = load(&external).unwrap_err();
        assert!(
            matches!(&error, OnnxError::Unsupported(reason) if reason.contains("external")),
            "{error}"
        );
        assert!(load(&[0xff; 32]).is_err());
    }

    #[test]
    fn gate_resource_cancel_recovery_onnx_limits_are_enforced_at_load() {
        let bytes = crate::fixtures::onnx::linear_model(64, &[0.5; 64 * 64], &[0.0; 64]);
        let tight = OnnxLimits {
            max_parameters: 100,
            ..DEFAULT_ONNX_LIMITS
        };
        assert!(matches!(
            NativeOnnxModel::load(&bytes, &tight),
            Err(OnnxError::Limit(_))
        ));
        let tight = OnnxLimits {
            max_operations_per_row: 1000,
            ..DEFAULT_ONNX_LIMITS
        };
        assert!(matches!(
            NativeOnnxModel::load(&bytes, &tight),
            Err(OnnxError::Limit(_))
        ));
        assert!(NativeOnnxModel::load(&bytes, &DEFAULT_ONNX_LIMITS).is_ok());
    }

    #[test]
    fn broadcasting_and_concat_follow_onnx_semantics() {
        let a = Value {
            shape: vec![2, 3],
            data: vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0],
        };
        let b = Value {
            shape: vec![3],
            data: vec![10.0, 20.0, 30.0],
        };
        assert_eq!(
            binary(BinaryOp::Add, &a, &b, &[2, 3]),
            vec![11.0, 22.0, 33.0, 14.0, 25.0, 36.0]
        );
        let c = Value {
            shape: vec![2, 1],
            data: vec![7.0, 8.0],
        };
        assert_eq!(
            concat(&[&a, &c], 1, &[2, 4]),
            vec![1.0, 2.0, 3.0, 7.0, 4.0, 5.0, 6.0, 8.0]
        );
    }
}
