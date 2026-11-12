//! Dump the input/output tensor spec of an ONNX model.
//!
//! Usage: cargo run --example dump_model -- [path-to-model.onnx]

use tract_onnx::prelude::*;

fn main() -> TractResult<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "assets/selfie_segmentation_landscape.onnx".to_string());
    let model = tract_onnx::onnx().model_for_path(&path)?;
    println!("model: {path}");
    for (i, outlet) in model.input_outlets()?.iter().enumerate() {
        let node = model.node(outlet.node);
        println!(
            "input  {i}: node={:?} op={:?} fact={:?}",
            node.name, node.op, model.inputs[i]
        );
    }
    for (i, outlet) in model.output_outlets()?.iter().enumerate() {
        let node = model.node(outlet.node);
        println!(
            "output {i}: node={:?} op={:?} fact={:?}",
            node.name, node.op, model.outputs[i]
        );
    }
    let typed = model.into_typed()?;
    for i in 0..typed.inputs.len() {
        println!("typed input  {i}: {:?}", typed.input_fact(i));
    }
    for i in 0..typed.outputs.len() {
        println!("typed output {i}: {:?}", typed.output_fact(i));
    }
    Ok(())
}
