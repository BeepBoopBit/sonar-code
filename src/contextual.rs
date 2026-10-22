//! Local BERT sentence encoders with attention-masked mean pooling. No model hub access.
use anyhow::{Context, Result, ensure};
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};
use tokenizers::{PaddingDirection, PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

pub(crate) struct ContextualEncoder {
    tokenizer: Tokenizer,
    model: BertModel,
    pub dimensions: usize,
}

impl ContextualEncoder {
    pub fn load(
        tokenizer: &[u8],
        weights: Vec<u8>,
        config: &[u8],
        sentence: &[u8],
        pooling: &[u8],
    ) -> Result<Self> {
        let config: Config = serde_json::from_slice(config)?;
        let sentence: serde_json::Value = serde_json::from_slice(sentence)?;
        let pooling: serde_json::Value = serde_json::from_slice(pooling)?;
        ensure!(
            pooling["pooling_mode_mean_tokens"] == true
                && pooling
                    .as_object()
                    .context("invalid pooling config")?
                    .iter()
                    .all(|(k, v)| !k.starts_with("pooling_mode_")
                        || k == "pooling_mode_mean_tokens"
                        || v == false),
            "only attention-masked mean pooling is supported"
        );
        let length = sentence["max_seq_length"]
            .as_u64()
            .context("missing max_seq_length")? as usize;
        ensure!(
            length > 1 && length <= config.max_position_embeddings && length <= 512,
            "invalid encoder token budget"
        );
        let mut tokenizer = Tokenizer::from_bytes(tokenizer).map_err(anyhow::Error::msg)?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: length,
                ..Default::default()
            }))
            .map_err(anyhow::Error::msg)?;
        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            pad_id: config.pad_token_id as u32,
            ..Default::default()
        }));
        let vb = VarBuilder::from_buffered_safetensors(weights, DType::F32, &Device::Cpu)?;
        let model = BertModel::load(vb, &config)?;
        Ok(Self {
            tokenizer,
            model,
            dimensions: config.hidden_size,
        })
    }

    pub fn encode(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut result = vec![Vec::new(); texts.len()];
        let mut tokenizer = self.tokenizer.clone();
        tokenizer.with_padding(None);
        let mut ordered: Vec<_> = tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(anyhow::Error::msg)?
            .into_iter()
            .enumerate()
            .collect();
        ordered.sort_by_key(|(_, encoding)| encoding.len());
        let padding = self
            .tokenizer
            .get_padding()
            .context("encoder padding configuration missing")?;
        // A small batch bounds intermediate attention memory on CPU.
        for batch in ordered.chunks_mut(8) {
            let length = batch.iter().map(|(_, e)| e.len()).max().unwrap_or(0);
            ensure!(length > 0, "encoder produced an empty token sequence");
            for (_, encoding) in batch.iter_mut() {
                encoding.pad(
                    length,
                    padding.pad_id,
                    padding.pad_type_id,
                    &padding.pad_token,
                    PaddingDirection::Right,
                );
            }
            let tensor = |get: fn(&tokenizers::Encoding) -> &[u32]| -> Result<Tensor> {
                let values: Vec<_> = batch.iter().flat_map(|(_, e)| get(e)).copied().collect();
                Ok(Tensor::from_vec(
                    values,
                    (batch.len(), length),
                    &Device::Cpu,
                )?)
            };
            let ids = tensor(tokenizers::Encoding::get_ids)?;
            let types = tensor(tokenizers::Encoding::get_type_ids)?;
            let mask = tensor(tokenizers::Encoding::get_attention_mask)?;
            let output = self.model.forward(&ids, &types, Some(&mask))?;
            let mask = mask.to_dtype(DType::F32)?.unsqueeze(2)?;
            let pooled = output
                .broadcast_mul(&mask)?
                .sum(1)?
                .broadcast_div(&mask.sum(1)?)?;
            let norms = pooled
                .sqr()?
                .sum_keepdim(1)?
                .sqrt()?
                .clamp(1e-12f32, f32::MAX)?;
            for ((index, _), vector) in batch
                .iter()
                .zip(pooled.broadcast_div(&norms)?.to_vec2::<f32>()?)
            {
                result[*index] = vector;
            }
        }
        Ok(result)
    }
}
