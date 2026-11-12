//! Person segmentation via tract-onnx: MediaPipe Selfie Segmentation
//! (landscape). Pure tract/ndarray — no opencv — so it works on raw
//! tightly-packed RGB buffers.
//!
//! Confirmed model I/O (examples/dump_model.rs on
//! assets/selfie_segmentation_landscape.onnx):
//!   input  "pixel_values": F32 NCHW [1, 3, 144, 256]  (H=144, W=256)
//!   output "/Sigmoid_2":   F32 [1, 1, 144, 256], sigmoid mask, person ≈ 1
//! Preprocessing: RGB scaled to [0,1] then normalized to [-1,1].

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, ensure, Context, Result};
use tract_onnx::prelude::*;

/// Model input width/height.
pub const MODEL_W: usize = 256;
pub const MODEL_H: usize = 144;
/// Sigmoid threshold above which a pixel counts as person.
pub const MASK_THRESHOLD: f32 = 0.5;

pub struct Segmenter {
    model: Arc<TypedRunnableModel>,
}

impl Segmenter {
    pub fn load(path: &Path) -> Result<Self> {
        let model = tract_onnx::onnx()
            .model_for_path(path)
            .with_context(|| format!("load ONNX model {}", path.display()))?;
        // Keep the symbolic batch dim symbolic: an internal Resize depends on
        // it, so pinning it to a concrete value breaks shape analysis. Skip
        // into_optimized for the same reason; the runtime evaluates the Resize
        // per concrete input.
        let typed = model.into_typed()?;
        // Bind all symbols (batch_size) to 1 so the graph's symbolic Resize
        // can be planned and evaluated.
        let subs: HashMap<Symbol, TDim> = typed
            .symbols
            .all_symbols()
            .into_iter()
            .map(|s| (s, TDim::from(1usize)))
            .collect();
        let typed = typed.set_symbols(&subs)?;
        // Symbols are concrete now, so optimization (op fusion, codegen)
        // works — and is where most of the speed comes from.
        let typed = typed.into_optimized()?;
        let fact = typed.input_fact(0)?;
        // Batch dim is symbolic (batch_size); check the rest concretely.
        let dims = fact.shape.dims();
        ensure!(
            fact.datum_type == DatumType::F32
                && dims.len() == 4
                && dims[1] == TDim::from(3usize)
                && dims[2] == TDim::from(MODEL_H)
                && dims[3] == TDim::from(MODEL_W),
            "unexpected model input spec: {:?} {:?}",
            fact.datum_type,
            dims
        );
        let model = typed.into_runnable()?;
        Ok(Self { model })
    }

    /// `rgb`: tightly packed RGB8 at exactly MODEL_W x MODEL_H.
    /// Returns an 8-bit mask (MODEL_H rows x MODEL_W cols, row-major),
    /// 255 = person, 0 = background.
    pub fn mask(&self, rgb: &[u8], w: usize, h: usize) -> Result<Vec<u8>> {
        if w != MODEL_W || h != MODEL_H {
            bail!("expected {}x{} input, got {}x{}", MODEL_W, MODEL_H, w, h);
        }
        if rgb.len() != w * h * 3 {
            bail!("bad rgb buffer len {} (want {})", rgb.len(), w * h * 3);
        }
        // HWC RGB u8 -> CHW f32 normalized to [-1, 1], batch dim 1.
        let mut chw = vec![0f32; 3 * w * h];
        for p in 0..w * h {
            for c in 0..3 {
                chw[c * w * h + p] = (rgb[p * 3 + c] as f32 / 255.0 - 0.5) * 2.0;
            }
        }
        let input = tract_ndarray::Array4::from_shape_vec((1, 3, h, w), chw)
            .context("build input array")?;
        let outputs = self.model.run(tvec!(Tensor::from(input).into_tvalue()))?;
        ensure!(outputs.len() == 1, "expected 1 output, got {}", outputs.len());
        let view = outputs[0].to_plain_array_view::<f32>()?;
        Ok(view
            .iter()
            .map(|&v| if v >= MASK_THRESHOLD { 255u8 } else { 0u8 })
            .collect())
    }
}
