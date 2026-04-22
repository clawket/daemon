use anyhow::{Context, Result};
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};
use hf_hub::{api::tokio::Api, Repo, RepoType};
use once_cell::sync::OnceCell;
use std::path::PathBuf;
use std::sync::Arc;
use tokenizers::{PaddingParams, Tokenizer, TruncationParams};
use tokio::sync::Mutex;

pub const EMBEDDING_DIM: usize = 384;
const MODEL_REPO: &str = "sentence-transformers/all-MiniLM-L6-v2";
const MAX_CHARS: usize = 2000;
const MAX_TOKENS: usize = 256;

struct Embedder {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
}

static EMBEDDER: OnceCell<Arc<Mutex<Option<Embedder>>>> = OnceCell::new();

fn slot() -> Arc<Mutex<Option<Embedder>>> {
    EMBEDDER
        .get_or_init(|| Arc::new(Mutex::new(None)))
        .clone()
}

async fn fetch_files() -> Result<(PathBuf, PathBuf, PathBuf)> {
    let api = Api::new().context("hf-hub api init")?;
    let repo = api.repo(Repo::new(MODEL_REPO.to_string(), RepoType::Model));
    let config = repo.get("config.json").await.context("download config.json")?;
    let tokenizer = repo
        .get("tokenizer.json")
        .await
        .context("download tokenizer.json")?;
    let weights = repo
        .get("model.safetensors")
        .await
        .context("download model.safetensors")?;
    Ok((config, tokenizer, weights))
}

async fn load() -> Result<Embedder> {
    let (config_path, tokenizer_path, weights_path) = fetch_files().await?;
    let config_json = std::fs::read_to_string(&config_path).context("read config.json")?;
    let config: Config = serde_json::from_str(&config_json).context("parse bert config")?;

    let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
        .map_err(|e| anyhow::anyhow!("tokenizer load: {}", e))?;
    let pad_id = tokenizer.token_to_id("[PAD]").unwrap_or(0);
    tokenizer
        .with_padding(Some(PaddingParams {
            strategy: tokenizers::PaddingStrategy::BatchLongest,
            pad_id,
            pad_token: "[PAD]".to_string(),
            ..Default::default()
        }))
        .with_truncation(Some(TruncationParams {
            max_length: MAX_TOKENS,
            ..Default::default()
        }))
        .map_err(|e| anyhow::anyhow!("tokenizer config: {}", e))?;

    let device = Device::Cpu;
    let vb = unsafe {
        VarBuilder::from_mmaped_safetensors(&[weights_path], DType::F32, &device)
            .context("load safetensors")?
    };
    let model = BertModel::load(vb, &config).context("load bert model")?;
    Ok(Embedder {
        model,
        tokenizer,
        device,
    })
}

pub async fn embed(text: &str) -> Result<Option<Vec<f32>>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let truncated = if trimmed.len() > MAX_CHARS {
        &trimmed[..MAX_CHARS]
    } else {
        trimmed
    };

    let lock = slot();
    let mut guard = lock.lock().await;
    if guard.is_none() {
        *guard = Some(load().await?);
    }
    let emb = guard.as_ref().expect("embedder loaded");

    let encoding = emb
        .tokenizer
        .encode(truncated, true)
        .map_err(|e| anyhow::anyhow!("tokenize: {}", e))?;
    let ids: Vec<i64> = encoding.get_ids().iter().map(|&v| v as i64).collect();
    let mask: Vec<i64> = encoding
        .get_attention_mask()
        .iter()
        .map(|&v| v as i64)
        .collect();

    let input_ids = Tensor::new(ids.as_slice(), &emb.device)?.unsqueeze(0)?;
    let token_type_ids = input_ids.zeros_like()?;
    let attention_mask = Tensor::new(mask.as_slice(), &emb.device)?.unsqueeze(0)?;

    let hidden = emb
        .model
        .forward(&input_ids, &token_type_ids, Some(&attention_mask))?;

    let mask_f = attention_mask.to_dtype(DType::F32)?.unsqueeze(2)?;
    let masked = hidden.broadcast_mul(&mask_f)?;
    let summed = masked.sum(1)?;
    let counts = mask_f.sum(1)?.clamp(1e-9f32, f32::INFINITY)?;
    let mean = summed.broadcast_div(&counts)?;
    let norm = mean
        .broadcast_div(&mean.sqr()?.sum_keepdim(1)?.sqrt()?)?
        .squeeze(0)?;
    let v: Vec<f32> = norm.to_vec1()?;
    Ok(Some(v))
}
