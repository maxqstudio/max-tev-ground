//! Strict, bounded decoder for the subset of the ONNX protobuf schema the native adapter reads.
//!
//! Only fields the interpreter needs are decoded. Fields that would make a model depend on
//! anything outside its own bytes (external tensor data, functions, subgraphs, sparse or training
//! data) are refused rather than skipped. Every length is checked against the remaining input, so
//! a malformed file fails with a diagnostic and never allocates more than the file it came from.

use super::OnnxError;

const MAX_DEPTH: usize = 16;
const MAX_REPEATED: usize = 1 << 22;

#[derive(Clone, Copy, Debug)]
enum Wire<'a> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'a [u8]),
    Fixed32(u32),
}

struct Reader<'a> {
    data: &'a [u8],
    position: usize,
    depth: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8], depth: usize) -> Result<Self, OnnxError> {
        if depth > MAX_DEPTH {
            return Err(malformed("message nesting is too deep"));
        }
        Ok(Self {
            data,
            position: 0,
            depth,
        })
    }

    fn varint(&mut self) -> Result<u64, OnnxError> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *self
                .data
                .get(self.position)
                .ok_or_else(|| malformed("truncated varint"))?;
            self.position += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(malformed("varint is longer than ten bytes"))
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], OnnxError> {
        let end = self
            .position
            .checked_add(length)
            .filter(|end| *end <= self.data.len())
            .ok_or_else(|| malformed("length exceeds the remaining message"))?;
        let bytes = &self.data[self.position..end];
        self.position = end;
        Ok(bytes)
    }

    fn next(&mut self) -> Result<Option<(u32, Wire<'a>)>, OnnxError> {
        if self.position == self.data.len() {
            return Ok(None);
        }
        let key = self.varint()?;
        let field = u32::try_from(key >> 3).map_err(|_| malformed("field number overflows"))?;
        if field == 0 {
            return Err(malformed("field number zero"));
        }
        let wire = match key & 7 {
            0 => Wire::Varint(self.varint()?),
            1 => {
                let bytes = self.take(8)?;
                Wire::Fixed64(u64::from_le_bytes(bytes.try_into().expect("eight bytes")))
            }
            2 => {
                let length =
                    usize::try_from(self.varint()?).map_err(|_| malformed("length overflows"))?;
                Wire::Bytes(self.take(length)?)
            }
            5 => {
                let bytes = self.take(4)?;
                Wire::Fixed32(u32::from_le_bytes(bytes.try_into().expect("four bytes")))
            }
            _ => return Err(malformed("unsupported protobuf wire type")),
        };
        Ok(Some((field, wire)))
    }

    fn child(&self, bytes: &'a [u8]) -> Result<Reader<'a>, OnnxError> {
        Reader::new(bytes, self.depth + 1)
    }
}

fn malformed(reason: &str) -> OnnxError {
    OnnxError::Malformed(reason.into())
}

fn refused(reason: &str) -> OnnxError {
    OnnxError::Unsupported(reason.into())
}

fn bytes<'a>(wire: Wire<'a>, what: &str) -> Result<&'a [u8], OnnxError> {
    match wire {
        Wire::Bytes(bytes) => Ok(bytes),
        _ => Err(malformed(&format!("{what} has the wrong wire type"))),
    }
}

fn string(wire: Wire<'_>, what: &str) -> Result<String, OnnxError> {
    String::from_utf8(bytes(wire, what)?.to_vec())
        .map_err(|_| malformed(&format!("{what} is not UTF-8")))
}

fn int(wire: Wire<'_>, what: &str) -> Result<i64, OnnxError> {
    match wire {
        Wire::Varint(value) => Ok(value as i64),
        _ => Err(malformed(&format!("{what} has the wrong wire type"))),
    }
}

fn push_bounded<T>(values: &mut Vec<T>, value: T) -> Result<(), OnnxError> {
    if values.len() >= MAX_REPEATED {
        return Err(refused("a repeated field exceeds the supported length"));
    }
    values.push(value);
    Ok(())
}

/// Appends packed or unpacked varint values.
fn ints(wire: Wire<'_>, values: &mut Vec<i64>, what: &str) -> Result<(), OnnxError> {
    match wire {
        Wire::Varint(value) => push_bounded(values, value as i64),
        Wire::Bytes(packed) => {
            let mut reader = Reader::new(packed, 0)?;
            while reader.position < packed.len() {
                push_bounded(values, reader.varint()? as i64)?;
            }
            Ok(())
        }
        _ => Err(malformed(&format!("{what} has the wrong wire type"))),
    }
}

/// Appends packed or unpacked 32-bit floats.
fn floats(wire: Wire<'_>, values: &mut Vec<f32>, what: &str) -> Result<(), OnnxError> {
    match wire {
        Wire::Fixed32(bits) => push_bounded(values, f32::from_bits(bits)),
        Wire::Bytes(packed) => {
            if packed.len() % 4 != 0 {
                return Err(malformed(&format!(
                    "packed {what} is not a whole number of floats"
                )));
            }
            for chunk in packed.as_chunks::<4>().0 {
                push_bounded(values, f32::from_le_bytes(*chunk))?;
            }
            Ok(())
        }
        _ => Err(malformed(&format!("{what} has the wrong wire type"))),
    }
}

/// Appends packed or unpacked 64-bit floats.
fn doubles(wire: Wire<'_>, values: &mut Vec<f64>, what: &str) -> Result<(), OnnxError> {
    match wire {
        Wire::Fixed64(bits) => push_bounded(values, f64::from_bits(bits)),
        Wire::Bytes(packed) => {
            if packed.len() % 8 != 0 {
                return Err(malformed(&format!(
                    "packed {what} is not a whole number of doubles"
                )));
            }
            for chunk in packed.as_chunks::<8>().0 {
                push_bounded(values, f64::from_le_bytes(*chunk))?;
            }
            Ok(())
        }
        _ => Err(malformed(&format!("{what} has the wrong wire type"))),
    }
}

pub const DATA_TYPE_FLOAT: i64 = 1;
pub const DATA_TYPE_INT64: i64 = 7;
pub const DATA_TYPE_DOUBLE: i64 = 11;

/// A decoded tensor. Float and double payloads are both held as `f32` (the interpreter's element
/// type); INT64 payloads are kept separately for shape operands.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tensor {
    pub name: String,
    pub dims: Vec<i64>,
    pub data_type: i64,
    pub floats: Vec<f32>,
    pub int64s: Vec<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Dim {
    Value(i64),
    Symbolic,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ValueInfo {
    pub name: String,
    pub elem_type: i64,
    pub dims: Option<Vec<Dim>>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AttributeValue {
    Float(f32),
    Int(i64),
    Floats(Vec<f32>),
    Ints(Vec<i64>),
    Text(String),
    Tensor(Tensor),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Attribute {
    pub name: String,
    pub value: AttributeValue,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub op_type: String,
    pub domain: String,
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub attributes: Vec<Attribute>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub initializers: Vec<Tensor>,
    pub inputs: Vec<ValueInfo>,
    pub outputs: Vec<ValueInfo>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Model {
    pub ir_version: i64,
    pub producer_name: String,
    pub opsets: Vec<(String, i64)>,
    pub graph: Graph,
}

pub fn decode_model(data: &[u8]) -> Result<Model, OnnxError> {
    let mut reader = Reader::new(data, 0)?;
    let mut ir_version = None;
    let mut producer_name = String::new();
    let mut opsets = Vec::new();
    let mut graph = None;
    while let Some((field, wire)) = reader.next()? {
        match field {
            1 => ir_version = Some(int(wire, "ir_version")?),
            2 => producer_name = string(wire, "producer_name")?,
            7 => {
                if graph.is_some() {
                    return Err(malformed("model declares more than one graph"));
                }
                graph = Some(decode_graph(&mut reader.child(bytes(wire, "graph")?)?)?);
            }
            8 => {
                let mut opset = reader.child(bytes(wire, "opset_import")?)?;
                let (mut domain, mut version) = (String::new(), None);
                while let Some((field, wire)) = opset.next()? {
                    match field {
                        1 => domain = string(wire, "opset domain")?,
                        2 => version = Some(int(wire, "opset version")?),
                        _ => {}
                    }
                }
                let version = version.ok_or_else(|| malformed("opset import has no version"))?;
                push_bounded(&mut opsets, (domain, version))?;
            }
            20 => return Err(refused("training information is not supported")),
            25 => return Err(refused("model-local functions are not supported")),
            // producer_version, domain, model_version, doc_string, metadata_props: descriptive only.
            _ => {}
        }
    }
    Ok(Model {
        ir_version: ir_version.ok_or_else(|| malformed("model has no ir_version"))?,
        producer_name,
        opsets,
        graph: graph.ok_or_else(|| malformed("model has no graph"))?,
    })
}

fn decode_graph(reader: &mut Reader<'_>) -> Result<Graph, OnnxError> {
    let mut graph = Graph::default();
    while let Some((field, wire)) = reader.next()? {
        match field {
            1 => {
                let node = decode_node(&mut reader.child(bytes(wire, "node")?)?)?;
                push_bounded(&mut graph.nodes, node)?;
            }
            5 => {
                let tensor = decode_tensor(&mut reader.child(bytes(wire, "initializer")?)?)?;
                push_bounded(&mut graph.initializers, tensor)?;
            }
            11 => {
                let input = decode_value_info(&mut reader.child(bytes(wire, "graph input")?)?)?;
                push_bounded(&mut graph.inputs, input)?;
            }
            12 => {
                let output = decode_value_info(&mut reader.child(bytes(wire, "graph output")?)?)?;
                push_bounded(&mut graph.outputs, output)?;
            }
            15 => return Err(refused("sparse initializers are not supported")),
            // name, doc_string, value_info, quantization_annotation, metadata_props.
            _ => {}
        }
    }
    Ok(graph)
}

fn decode_node(reader: &mut Reader<'_>) -> Result<Node, OnnxError> {
    let mut node = Node {
        op_type: String::new(),
        domain: String::new(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        attributes: Vec::new(),
    };
    while let Some((field, wire)) = reader.next()? {
        match field {
            1 => push_bounded(&mut node.inputs, string(wire, "node input")?)?,
            2 => push_bounded(&mut node.outputs, string(wire, "node output")?)?,
            4 => node.op_type = string(wire, "op_type")?,
            5 => {
                let attribute = decode_attribute(&mut reader.child(bytes(wire, "attribute")?)?)?;
                push_bounded(&mut node.attributes, attribute)?;
            }
            7 => node.domain = string(wire, "node domain")?,
            // name, doc_string, overload, metadata_props.
            _ => {}
        }
    }
    if node.op_type.is_empty() {
        return Err(malformed("node has no op_type"));
    }
    Ok(node)
}

fn decode_attribute(reader: &mut Reader<'_>) -> Result<Attribute, OnnxError> {
    let mut name = String::new();
    let mut value = None;
    let (mut float_values, mut int_values) = (Vec::new(), Vec::new());
    let (mut has_floats, mut has_ints) = (false, false);
    while let Some((field, wire)) = reader.next()? {
        match field {
            1 => name = string(wire, "attribute name")?,
            2 => match wire {
                Wire::Fixed32(bits) => value = Some(AttributeValue::Float(f32::from_bits(bits))),
                _ => return Err(malformed("attribute f has the wrong wire type")),
            },
            3 => value = Some(AttributeValue::Int(int(wire, "attribute i")?)),
            4 => value = Some(AttributeValue::Text(string(wire, "attribute s")?)),
            5 => {
                let tensor = decode_tensor(&mut reader.child(bytes(wire, "attribute t")?)?)?;
                value = Some(AttributeValue::Tensor(tensor));
            }
            7 => {
                has_floats = true;
                floats(wire, &mut float_values, "attribute floats")?;
            }
            8 => {
                has_ints = true;
                ints(wire, &mut int_values, "attribute ints")?;
            }
            6 | 11 => return Err(refused("subgraph attributes are not supported")),
            21 => return Err(refused("function attribute references are not supported")),
            9 | 10 | 14 | 15 | 22 | 23 => {
                return Err(refused("this attribute kind is not supported"))
            }
            // doc_string, type.
            _ => {}
        }
    }
    if has_floats {
        value = Some(AttributeValue::Floats(float_values));
    } else if has_ints {
        value = Some(AttributeValue::Ints(int_values));
    }
    Ok(Attribute {
        value: value.ok_or_else(|| malformed(&format!("attribute {name:?} has no value")))?,
        name,
    })
}

fn decode_tensor(reader: &mut Reader<'_>) -> Result<Tensor, OnnxError> {
    let mut tensor = Tensor::default();
    let mut raw: Option<&[u8]> = None;
    let mut double_values = Vec::new();
    while let Some((field, wire)) = reader.next()? {
        match field {
            1 => ints(wire, &mut tensor.dims, "tensor dims")?,
            2 => tensor.data_type = int(wire, "tensor data_type")?,
            4 => floats(wire, &mut tensor.floats, "tensor float_data")?,
            7 => ints(wire, &mut tensor.int64s, "tensor int64_data")?,
            8 => tensor.name = string(wire, "tensor name")?,
            9 => raw = Some(bytes(wire, "tensor raw_data")?),
            10 => doubles(wire, &mut double_values, "tensor double_data")?,
            13 => return Err(refused("external tensor data is not supported")),
            14 => {
                if int(wire, "tensor data_location")? != 0 {
                    return Err(refused("external tensor data is not supported"));
                }
            }
            3 => return Err(refused("segmented tensors are not supported")),
            5 | 6 | 11 => return Err(refused("this tensor payload type is not supported")),
            // doc_string, metadata_props.
            _ => {}
        }
    }
    if tensor.dims.iter().any(|dim| *dim < 0) {
        return Err(malformed("tensor has a negative dimension"));
    }
    let count = tensor
        .dims
        .iter()
        .try_fold(1usize, |count, dim| count.checked_mul(*dim as usize))
        .filter(|count| *count <= MAX_REPEATED)
        .ok_or_else(|| refused("tensor exceeds the supported element count"))?;
    match tensor.data_type {
        DATA_TYPE_FLOAT => {
            if let Some(raw) = raw {
                if !tensor.floats.is_empty() || raw.len() != count * 4 {
                    return Err(malformed("float tensor payload does not match its shape"));
                }
                tensor.floats = raw
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|chunk| f32::from_le_bytes(*chunk))
                    .collect();
            }
            if tensor.floats.len() != count || !tensor.int64s.is_empty() {
                return Err(malformed("float tensor payload does not match its shape"));
            }
        }
        DATA_TYPE_DOUBLE => {
            if let Some(raw) = raw {
                if !double_values.is_empty() || raw.len() != count * 8 {
                    return Err(malformed("double tensor payload does not match its shape"));
                }
                double_values = raw
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|chunk| f64::from_le_bytes(*chunk))
                    .collect();
            }
            if double_values.len() != count || !tensor.floats.is_empty() {
                return Err(malformed("double tensor payload does not match its shape"));
            }
            // The interpreter computes in f32, as a FLOAT model would; DOUBLE constants are
            // narrowed once at load so every run uses the same values.
            tensor.floats = double_values.iter().map(|value| *value as f32).collect();
            tensor.data_type = DATA_TYPE_FLOAT;
        }
        DATA_TYPE_INT64 => {
            if let Some(raw) = raw {
                if !tensor.int64s.is_empty() || raw.len() != count * 8 {
                    return Err(malformed("int64 tensor payload does not match its shape"));
                }
                tensor.int64s = raw
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|chunk| i64::from_le_bytes(*chunk))
                    .collect();
            }
            if tensor.int64s.len() != count || !tensor.floats.is_empty() {
                return Err(malformed("int64 tensor payload does not match its shape"));
            }
        }
        other => {
            return Err(refused(&format!(
                "tensor element type {other} is not supported (FLOAT, DOUBLE, or INT64 only)"
            )))
        }
    }
    Ok(tensor)
}

fn decode_value_info(reader: &mut Reader<'_>) -> Result<ValueInfo, OnnxError> {
    let mut name = String::new();
    let mut elem_type = None;
    let mut dims = None;
    while let Some((field, wire)) = reader.next()? {
        match field {
            1 => name = string(wire, "value name")?,
            2 => {
                let mut type_reader = reader.child(bytes(wire, "value type")?)?;
                while let Some((field, wire)) = type_reader.next()? {
                    match field {
                        1 => {
                            let mut tensor = type_reader.child(bytes(wire, "tensor type")?)?;
                            while let Some((field, wire)) = tensor.next()? {
                                match field {
                                    1 => elem_type = Some(int(wire, "elem_type")?),
                                    2 => {
                                        dims = Some(decode_shape(
                                            &mut tensor.child(bytes(wire, "shape")?)?,
                                        )?)
                                    }
                                    _ => {}
                                }
                            }
                        }
                        4 | 5 | 8 | 9 => {
                            return Err(refused(
                                "only tensor-typed graph inputs and outputs are supported",
                            ))
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Ok(ValueInfo {
        elem_type: elem_type
            .ok_or_else(|| malformed(&format!("value {name:?} declares no tensor type")))?,
        name,
        dims,
    })
}

fn decode_shape(reader: &mut Reader<'_>) -> Result<Vec<Dim>, OnnxError> {
    let mut dims = Vec::new();
    while let Some((field, wire)) = reader.next()? {
        if field != 1 {
            continue;
        }
        let mut dim_reader = reader.child(bytes(wire, "dimension")?)?;
        let mut dim = Dim::Symbolic;
        while let Some((field, wire)) = dim_reader.next()? {
            if field == 1 {
                dim = Dim::Value(int(wire, "dim_value")?);
            }
        }
        push_bounded(&mut dims, dim)?;
    }
    Ok(dims)
}
