//! Tiny ONNX file builder for tests: writes real `.onnx` bytes with `prost`, so the tests
//! exercise the actual ONNX Runtime without shipping binary fixtures.

use prost::Message;

#[derive(Clone, PartialEq, Message)]
pub struct ModelProto {
    #[prost(int64, tag = "1")]
    pub ir_version: i64,
    #[prost(string, tag = "2")]
    pub producer_name: String,
    #[prost(message, optional, tag = "7")]
    pub graph: Option<GraphProto>,
    #[prost(message, repeated, tag = "8")]
    pub opset_import: Vec<OperatorSetIdProto>,
}

#[derive(Clone, PartialEq, Message)]
pub struct OperatorSetIdProto {
    #[prost(string, tag = "1")]
    pub domain: String,
    #[prost(int64, tag = "2")]
    pub version: i64,
}

#[derive(Clone, PartialEq, Message)]
pub struct GraphProto {
    #[prost(message, repeated, tag = "1")]
    pub node: Vec<NodeProto>,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(message, repeated, tag = "5")]
    pub initializer: Vec<TensorProto>,
    #[prost(message, repeated, tag = "11")]
    pub input: Vec<ValueInfoProto>,
    #[prost(message, repeated, tag = "12")]
    pub output: Vec<ValueInfoProto>,
}

#[derive(Clone, PartialEq, Message)]
pub struct NodeProto {
    #[prost(string, repeated, tag = "1")]
    pub input: Vec<String>,
    #[prost(string, repeated, tag = "2")]
    pub output: Vec<String>,
    #[prost(string, tag = "3")]
    pub name: String,
    #[prost(string, tag = "4")]
    pub op_type: String,
    #[prost(message, repeated, tag = "5")]
    pub attribute: Vec<AttributeProto>,
}

#[derive(Clone, PartialEq, Message)]
pub struct AttributeProto {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(int64, tag = "3")]
    pub i: i64,
    #[prost(int32, tag = "20")]
    pub r#type: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct TensorProto {
    #[prost(int64, repeated, tag = "1")]
    pub dims: Vec<i64>,
    #[prost(int32, tag = "2")]
    pub data_type: i32,
    #[prost(float, repeated, tag = "4")]
    pub float_data: Vec<f32>,
    #[prost(int64, repeated, tag = "7")]
    pub int64_data: Vec<i64>,
    #[prost(string, tag = "8")]
    pub name: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct ValueInfoProto {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(message, optional, tag = "2")]
    pub r#type: Option<TypeProto>,
}

#[derive(Clone, PartialEq, Message)]
pub struct TypeProto {
    #[prost(message, optional, tag = "1")]
    pub tensor_type: Option<TensorTypeProto>,
}

#[derive(Clone, PartialEq, Message)]
pub struct TensorTypeProto {
    #[prost(int32, tag = "1")]
    pub elem_type: i32,
    #[prost(message, optional, tag = "2")]
    pub shape: Option<TensorShapeProto>,
}

#[derive(Clone, PartialEq, Message)]
pub struct TensorShapeProto {
    #[prost(message, repeated, tag = "1")]
    pub dim: Vec<Dimension>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Dimension {
    #[prost(int64, optional, tag = "1")]
    pub dim_value: Option<i64>,
    #[prost(string, optional, tag = "2")]
    pub dim_param: Option<String>,
}

const FLOAT: i32 = 1;
const INT64: i32 = 7;

fn fixed(n: i64) -> Dimension {
    Dimension { dim_value: Some(n), dim_param: None }
}

fn symbolic(name: &str) -> Dimension {
    Dimension { dim_value: None, dim_param: Some(name.into()) }
}

fn value_info(name: &str, elem: i32, dims: Vec<Dimension>) -> ValueInfoProto {
    ValueInfoProto {
        name: name.into(),
        r#type: Some(TypeProto {
            tensor_type: Some(TensorTypeProto { elem_type: elem, shape: Some(TensorShapeProto { dim: dims }) }),
        }),
    }
}

fn model(graph: GraphProto) -> Vec<u8> {
    ModelProto {
        ir_version: 8,
        producer_name: "cardinal-tests".into(),
        graph: Some(graph),
        opset_import: vec![OperatorSetIdProto { domain: String::new(), version: 17 }],
    }
    .encode_to_vec()
}

/// `output = input · W + b` with `input: [batch, n]`, `W: [n, c]`, `b: [c]`.
pub fn linear_model(weights: &[f32], bias: &[f32], n: usize, c: usize) -> Vec<u8> {
    assert_eq!(weights.len(), n * c);
    assert_eq!(bias.len(), c);
    model(GraphProto {
        name: "linear".into(),
        node: vec![
            NodeProto {
                input: vec!["input".into(), "W".into()],
                output: vec!["mm".into()],
                name: "mm".into(),
                op_type: "MatMul".into(),
                attribute: vec![],
            },
            NodeProto {
                input: vec!["mm".into(), "B".into()],
                output: vec!["output".into()],
                name: "add".into(),
                op_type: "Add".into(),
                attribute: vec![],
            },
        ],
        initializer: vec![
            TensorProto {
                dims: vec![n as i64, c as i64],
                data_type: FLOAT,
                float_data: weights.to_vec(),
                int64_data: vec![],
                name: "W".into(),
            },
            TensorProto {
                dims: vec![c as i64],
                data_type: FLOAT,
                float_data: bias.to_vec(),
                int64_data: vec![],
                name: "B".into(),
            },
        ],
        input: vec![value_info("input", FLOAT, vec![symbolic("batch"), fixed(n as i64)])],
        output: vec![value_info("output", FLOAT, vec![symbolic("batch"), fixed(c as i64)])],
    })
}

/// A bag-of-words causal LM: `logits[b, t, :] = Σ_{s<=t} table[input_ids[b, s], :]`.
/// Every token seen so far shifts the next-token distribution, so the answer depends on
/// the whole prompt — enough to test prompt rules end to end.
pub fn bow_lm(table: &[f32], vocab: usize) -> Vec<u8> {
    assert_eq!(table.len(), vocab * vocab);
    model(GraphProto {
        name: "bow".into(),
        node: vec![
            NodeProto {
                input: vec!["W".into(), "input_ids".into()],
                output: vec!["emb".into()],
                name: "gather".into(),
                op_type: "Gather".into(),
                attribute: vec![AttributeProto { name: "axis".into(), i: 0, r#type: 2 }],
            },
            NodeProto {
                input: vec!["emb".into(), "axis".into()],
                output: vec!["logits".into()],
                name: "cumsum".into(),
                op_type: "CumSum".into(),
                attribute: vec![],
            },
        ],
        initializer: vec![
            TensorProto {
                dims: vec![vocab as i64, vocab as i64],
                data_type: FLOAT,
                float_data: table.to_vec(),
                int64_data: vec![],
                name: "W".into(),
            },
            TensorProto {
                dims: vec![],
                data_type: INT64,
                float_data: vec![],
                int64_data: vec![1],
                name: "axis".into(),
            },
        ],
        input: vec![value_info("input_ids", INT64, vec![symbolic("batch"), symbolic("seq")])],
        output: vec![value_info("logits", FLOAT, vec![symbolic("batch"), symbolic("seq"), fixed(vocab as i64)])],
    })
}
