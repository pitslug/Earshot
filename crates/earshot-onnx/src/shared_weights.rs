//! One copy of a model's weights across a process's CPU sessions (T9.1ma).
//!
//! Parakeet 0.6B keeps 2.27 GiB of weights beside its graph in `encoder.weights`, and a call runs
//! three sessions of it: one per track and one for provisional words. Left alone each session holds
//! its own copy. ONNX Runtime's `AddInitializer` uses a tensor given to it instead of reading one
//! from the model, so sessions given the same tensors share them. Measured on the bundled meeting
//! with provisional words on, peak private memory went from 9.39 to 5.81 GiB, with identical stored
//! words (docs/live-previews.md).
//!
//! Three things were tried and left out, with their numbers in that document:
//! - `AddExternalInitializersFromMemory` copies the buffer, so each session still costs 2.4 GiB.
//! - Prepacking stays on only at the price of a second copy of the weights (7.68 GiB), so a session
//!   with shared weights turns it off. Sharing the prepacked copies did not beat that.
//! - On CUDA a shared copy adds to what the graphics card holds and saves nothing (16.41 against
//!   15.40 GiB), so only CPU sessions share.
//!
//! Anything here that cannot be read as expected means an ordinary load, never a failed one: this
//! saves memory and changes nothing else. [`OnnxSession::shares_weights`] says which one ran.
//!
//! [`OnnxSession::shares_weights`]: crate::runtime::OnnxSession::shares_weights

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use ort::value::{DynValue, Tensor};

/// ONNX `TensorProto.DataType.FLOAT`.
const ONNX_FLOAT: u64 = 1;
/// ONNX `TensorProto.DataLocation.EXTERNAL`.
const ONNX_EXTERNAL: u64 = 1;

/// Whether sessions share weights: on, except to compare against the ordinary load.
static ENABLED: AtomicBool = AtomicBool::new(true);
/// The weights in use, freed with the last session that holds them.
static CURRENT: Mutex<Option<(PathBuf, Weak<Weights>)>> = Mutex::new(None);

/// Turns sharing off or on for sessions loaded after this, for this whole process. Only for
/// comparing the two loads: production never calls it.
pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::SeqCst);
}

/// A model's external weights, held by every session that uses them.
pub struct Weights {
    initializers: Vec<(String, Arc<DynValue>)>,
}

impl Weights {
    /// Each tensor with the name the graph knows it by.
    #[must_use]
    pub fn initializers(&self) -> &[(String, Arc<DynValue>)] {
        &self.initializers
    }
}

/// The shared weights of `model`, reading them if no session holds them now. `None` means an
/// ordinary load: the model has no `<stem>.weights` beside it, or they cannot be read as expected.
#[must_use]
pub fn for_model(model: &Path) -> Option<Arc<Weights>> {
    if !ENABLED.load(Ordering::SeqCst) {
        return None;
    }
    let file = model.with_extension("weights");
    if !file.is_file() {
        return None;
    }
    // Held across the read, so sessions loading together read the file once.
    let mut current = CURRENT.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((path, weights)) = current.as_ref()
        && path == model
        && let Some(weights) = weights.upgrade()
    {
        return Some(weights);
    }
    let weights = Arc::new(read(model, &file).ok()?);
    *current = Some((model.to_path_buf(), Arc::downgrade(&weights)));
    Some(weights)
}

fn read(model: &Path, file: &Path) -> Result<Weights, String> {
    let graph = std::fs::read(model).map_err(|e| e.to_string())?;
    let name = file
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("the weights file has no usable name")?;
    let tensors = external_tensors(&graph, name)?;
    let mut data = File::open(file).map_err(|e| e.to_string())?;
    let size = data.metadata().map_err(|e| e.to_string())?.len();
    let mut initializers = Vec::with_capacity(tensors.len());
    for t in tensors {
        let end = t
            .offset
            .checked_add(t.length)
            .ok_or("a tensor's range overflows")?;
        if end > size {
            return Err(format!("tensor {} ends past the weights file", t.name));
        }
        data.seek(SeekFrom::Start(t.offset))
            .map_err(|e| e.to_string())?;
        let mut bytes = vec![0u8; usize::try_from(t.length).map_err(|e| e.to_string())?];
        data.read_exact(&mut bytes).map_err(|e| e.to_string())?;
        let values: Vec<f32> = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        drop(bytes);
        let tensor = Tensor::from_array((t.dims, values)).map_err(|e| e.to_string())?;
        initializers.push((t.name, Arc::new(tensor.into_dyn())));
    }
    Ok(Weights { initializers })
}

/// One float tensor stored outside the graph.
#[derive(Debug, Clone, PartialEq, Eq)]
struct External {
    name: String,
    dims: Vec<i64>,
    offset: u64,
    length: u64,
}

/// One protobuf field's value.
#[derive(Debug, Clone, Copy)]
enum Field<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

fn varint(d: &[u8], at: &mut usize) -> Result<u64, String> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *d.get(*at).ok_or("the graph is cut off")?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err("a number in the graph is too long".to_owned())
}

/// The fields of one protobuf message (protobuf wire format: varint, 64-bit, length, 32-bit).
fn fields(d: &[u8]) -> Result<Vec<(u64, Field<'_>)>, String> {
    let (mut at, mut out) = (0, Vec::new());
    while at < d.len() {
        let key = varint(d, &mut at)?;
        let number = key >> 3;
        let skip = |at: usize, n: usize| {
            at.checked_add(n)
                .filter(|end| *end <= d.len())
                .ok_or_else(|| "the graph is cut off".to_owned())
        };
        match key & 7 {
            0 => out.push((number, Field::Varint(varint(d, &mut at)?))),
            1 => at = skip(at, 8)?,
            5 => at = skip(at, 4)?,
            2 => {
                let n = usize::try_from(varint(d, &mut at)?).map_err(|e| e.to_string())?;
                let end = skip(at, n)?;
                out.push((number, Field::Bytes(&d[at..end])));
                at = end;
            }
            other => return Err(format!("field type {other} in the graph")),
        }
    }
    Ok(out)
}

/// Every external initializer of the ONNX model `graph`, each checked to be a float tensor in
/// `weights_file` of the size its shape says. Field numbers are from `onnx.proto`: `ModelProto.graph`
/// 7, `GraphProto.initializer` 5, and in `TensorProto` dims 1, data_type 2, name 8, external_data 13
/// and data_location 14, with `StringStringEntryProto` key 1 and value 2.
fn external_tensors(graph: &[u8], weights_file: &str) -> Result<Vec<External>, String> {
    let model = fields(graph)?;
    let Some(body) = model.iter().find_map(|(n, f)| match (n, f) {
        (7, Field::Bytes(b)) => Some(*b),
        _ => None,
    }) else {
        return Err("the model has no graph".to_owned());
    };
    let mut out = Vec::new();
    let mut names = BTreeSet::new();
    for (number, field) in fields(body)? {
        let (5, Field::Bytes(tensor)) = (number, field) else {
            continue;
        };
        let Some(t) = external_tensor(tensor, weights_file)? else {
            continue;
        };
        if !names.insert(t.name.clone()) {
            return Err(format!("tensor {} appears twice", t.name));
        }
        out.push(t);
    }
    if out.is_empty() {
        return Err("the model names no external tensors".to_owned());
    }
    Ok(out)
}

/// One initializer as an [`External`]; `None` for one held inside the graph.
fn external_tensor(tensor: &[u8], weights_file: &str) -> Result<Option<External>, String> {
    let (mut dims, mut data_type, mut name, mut external) = (Vec::new(), 0, String::new(), false);
    let (mut offset, mut length, mut location) = (0u64, None, None);
    for (number, field) in fields(tensor)? {
        match (number, field) {
            (1, Field::Varint(v)) => dims.push(as_dim(v)?),
            (1, Field::Bytes(packed)) => {
                let mut at = 0;
                while at < packed.len() {
                    dims.push(as_dim(varint(packed, &mut at)?)?);
                }
            }
            (2, Field::Varint(v)) => data_type = v,
            (8, Field::Bytes(b)) => {
                name = std::str::from_utf8(b)
                    .map_err(|e| e.to_string())?
                    .to_owned();
            }
            (14, Field::Varint(v)) => external = v == ONNX_EXTERNAL,
            (13, Field::Bytes(entry)) => {
                let (mut key, mut value) = ("", "");
                for (n, f) in fields(entry)? {
                    match (n, f) {
                        (1, Field::Bytes(b)) => {
                            key = std::str::from_utf8(b).map_err(|e| e.to_string())?;
                        }
                        (2, Field::Bytes(b)) => {
                            value = std::str::from_utf8(b).map_err(|e| e.to_string())?;
                        }
                        _ => {}
                    }
                }
                match key {
                    "location" => location = Some(value),
                    "offset" => offset = value.parse().map_err(|_| "a bad offset")?,
                    "length" => length = Some(value.parse().map_err(|_| "a bad length")?),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    if !external {
        return Ok(None);
    }
    if data_type != ONNX_FLOAT {
        return Err(format!("tensor {name} is not 32-bit float"));
    }
    if location != Some(weights_file) {
        return Err(format!("tensor {name} is stored in another file"));
    }
    let length = length.ok_or_else(|| format!("tensor {name} has no length"))?;
    let expected = dims
        .iter()
        .try_fold(4u64, |n, d| n.checked_mul(u64::try_from(*d).ok()?))
        .ok_or_else(|| format!("tensor {name}'s shape overflows"))?;
    if name.is_empty() || expected != length {
        return Err(format!("tensor {name} does not match its shape"));
    }
    Ok(Some(External {
        name,
        dims,
        offset,
        length,
    }))
}

fn as_dim(v: u64) -> Result<i64, String> {
    i64::try_from(v).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn number(mut v: u64, out: &mut Vec<u8>) {
        while v >= 0x80 {
            out.push((v as u8 & 0x7f) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }

    fn varint_field(n: u64, v: u64, out: &mut Vec<u8>) {
        number(n << 3, out);
        number(v, out);
    }

    fn bytes_field(n: u64, b: &[u8], out: &mut Vec<u8>) {
        number((n << 3) | 2, out);
        number(b.len() as u64, out);
        out.extend_from_slice(b);
    }

    fn entry(key: &str, value: &str) -> Vec<u8> {
        let mut e = Vec::new();
        bytes_field(1, key.as_bytes(), &mut e);
        bytes_field(2, value.as_bytes(), &mut e);
        e
    }

    /// A `TensorProto` stored in `w.weights`, its fields in a shuffled order as a writer may.
    fn tensor(name: &str, ty: u64, dims: &[u64], offset: u64, length: u64) -> Vec<u8> {
        let mut t = Vec::new();
        bytes_field(8, name.as_bytes(), &mut t);
        bytes_field(13, &entry("length", &length.to_string()), &mut t);
        bytes_field(13, &entry("location", "w.weights"), &mut t);
        bytes_field(13, &entry("offset", &offset.to_string()), &mut t);
        for d in dims {
            varint_field(1, *d, &mut t);
        }
        varint_field(2, ty, &mut t);
        varint_field(14, ONNX_EXTERNAL, &mut t);
        t
    }

    fn model(tensors: &[Vec<u8>]) -> Vec<u8> {
        let mut graph = Vec::new();
        for t in tensors {
            bytes_field(5, t, &mut graph);
        }
        let mut m = Vec::new();
        varint_field(1, 8, &mut m);
        bytes_field(7, &graph, &mut m);
        m
    }

    fn read_one(t: Vec<u8>) -> Result<Vec<External>, String> {
        external_tensors(&model(&[t]), "w.weights")
    }

    #[test]
    fn shared_weights_finds_external_float_tensors_and_leaves_the_rest_to_the_graph() {
        let mut inside = Vec::new();
        bytes_field(8, b"kept_in_the_graph", &mut inside);
        varint_field(2, ONNX_FLOAT, &mut inside);
        let found = external_tensors(
            &model(&[
                tensor("a", 1, &[2, 3], 0, 24),
                inside,
                tensor("b", 1, &[4], 24, 16),
            ]),
            "w.weights",
        )
        .unwrap();
        assert_eq!(
            found,
            [
                External {
                    name: "a".into(),
                    dims: vec![2, 3],
                    offset: 0,
                    length: 24
                },
                External {
                    name: "b".into(),
                    dims: vec![4],
                    offset: 24,
                    length: 16
                },
            ]
        );
    }

    #[test]
    fn shared_weights_reads_packed_dimensions() {
        let mut packed = Vec::new();
        number(300, &mut packed);
        number(2, &mut packed);
        let mut t = tensor("a", 1, &[], 0, 2_400);
        bytes_field(1, &packed, &mut t);
        assert_eq!(read_one(t).unwrap()[0].dims, [300, 2]);
    }

    #[test]
    fn shared_weights_refuses_what_it_cannot_share_exactly() {
        // Another type, a size that does not match the shape, a shape that overflows.
        assert!(read_one(tensor("a", 7, &[2], 0, 16)).is_err());
        assert!(read_one(tensor("a", 1, &[2], 0, 12)).is_err());
        assert!(read_one(tensor("a", 1, &[u64::MAX / 2, 4], 0, 16)).is_err());
        // Another file, and no external tensor at all.
        assert!(external_tensors(&model(&[tensor("a", 1, &[2], 0, 8)]), "other.weights").is_err());
        assert!(external_tensors(&model(&[]), "w.weights").is_err());
        // The same name twice would give one tensor two meanings.
        let twice = [tensor("a", 1, &[2], 0, 8), tensor("a", 1, &[2], 8, 8)];
        assert!(external_tensors(&model(&twice), "w.weights").is_err());
    }

    #[test]
    fn shared_weights_a_damaged_graph_is_an_error_and_never_a_panic() {
        let good = model(&[tensor("a", 1, &[2, 3], 0, 24)]);
        for end in 0..good.len() {
            let _ = external_tensors(&good[..end], "w.weights");
        }
        assert!(external_tensors(&[0xff; 12], "w.weights").is_err());
        assert!(external_tensors(&[0x0b], "w.weights").is_err());
        // A length past the end of the message.
        assert!(external_tensors(&[0x3a, 0xff, 0x01], "w.weights").is_err());
    }
}
