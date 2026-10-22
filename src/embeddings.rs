use std::{path::Path, time::Instant};

use anyhow::{Context, Result, ensure};
use model2vec_rs::model::StaticModel;
use rusqlite::params;
use serde::Serialize;

use crate::store;

const INPUT_VERSION: &str =
    "source-views-v4|normalize=true|model2vec-rs=0.3.0|max-tokens=512|candle=0.11.0|masked-mean";

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, serde::Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum Representation {
    Combined,
    #[default]
    Separate,
    /// Separate views with bounded source identity prepended to content views.
    Context,
}

impl Representation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Combined => "combined",
            Self::Separate => "separate",
            Self::Context => "context",
        }
    }
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, serde::Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum EncodingPolicy {
    #[default]
    Legacy,
    /// Exclude artificial tokenizer padding while retaining legacy prefix limits.
    Unpadded,
    /// Preserve the complete supplied input in static encoders. Region boundaries are unchanged.
    Full,
}

impl EncodingPolicy {
    pub(crate) fn from_metadata(conn: &rusqlite::Connection) -> Result<Self> {
        match store::metadata(conn, "embedding_encoding")?.as_deref() {
            None | Some("legacy") => Ok(Self::Legacy),
            Some("unpadded") => Ok(Self::Unpadded),
            Some("full") => Ok(Self::Full),
            Some(other) => anyhow::bail!("unsupported embedding encoding policy: {other}"),
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Unpadded => "unpadded",
            Self::Full => "full",
        }
    }
}

enum Encoder {
    Static(StaticModel),
    Contextual(Box<crate::contextual::ContextualEncoder>),
}

/// Local-only model loader. Asset bytes, encoding settings, and input construction identify vectors.
pub struct Embedder {
    model: Encoder,
    encoding: EncodingPolicy,
    workers: rayon::ThreadPool,
    pub(crate) fingerprint: String,
    pub(crate) dimensions: usize,
}

impl Embedder {
    pub fn load(path: &Path) -> Result<Self> {
        Self::load_with_encoding(path, EncodingPolicy::Legacy)
    }

    pub fn load_with_encoding(path: &Path, encoding: EncodingPolicy) -> Result<Self> {
        ensure!(
            path.is_dir(),
            "model must be an existing local directory: {}",
            path.display()
        );
        let mut hash = blake3::Hasher::new();
        hash.update(INPUT_VERSION.as_bytes());
        if encoding != EncodingPolicy::Legacy {
            hash.update(b"|static-input-v1|");
            hash.update(encoding.as_str().as_bytes());
        }
        let mut assets = Vec::new();
        for name in ["tokenizer.json", "model.safetensors", "config.json"] {
            let bytes = std::fs::read(path.join(name))
                .with_context(|| format!("loading model asset {name}"))?;
            hash.update(name.as_bytes());
            hash.update(&(bytes.len() as u64).to_le_bytes());
            hash.update(&bytes);
            assets.push(bytes);
        }
        let config: serde_json::Value = serde_json::from_slice(&assets[2])?;
        let model = if config["model_type"] == "bert" {
            ensure!(
                encoding == EncodingPolicy::Legacy,
                "experimental input encoding supports static models only"
            );
            hash.update(b"length-buckets-v1");
            let mut extra = Vec::new();
            for name in ["sentence_bert_config.json", "1_Pooling/config.json"] {
                let bytes =
                    std::fs::read(path.join(name)).with_context(|| format!("loading {name}"))?;
                hash.update(name.as_bytes());
                hash.update(&(bytes.len() as u64).to_le_bytes());
                hash.update(&bytes);
                extra.push(bytes);
            }
            let weights = std::mem::take(&mut assets[1]);
            Encoder::Contextual(Box::new(crate::contextual::ContextualEncoder::load(
                &assets[0], weights, &assets[2], &extra[0], &extra[1],
            )?))
        } else {
            ensure!(
                config.get("model_type").is_none(),
                "unsupported embedding model architecture"
            );
            // StaticModel pools token IDs without an attention mask. Padding must not
            // become semantic evidence or make vectors depend on their batch partners.
            let mut tokenizer: serde_json::Value = serde_json::from_slice(&assets[0])?;
            if encoding != EncodingPolicy::Legacy {
                tokenizer["padding"] = serde_json::Value::Null;
            }
            if encoding == EncodingPolicy::Full {
                tokenizer["truncation"] = serde_json::Value::Null;
            }
            let tokenizer_bytes = if encoding == EncodingPolicy::Legacy {
                assets[0].clone()
            } else {
                serde_json::to_vec(&tokenizer)?
            };
            Encoder::Static(StaticModel::from_bytes(
                &tokenizer_bytes,
                &assets[1],
                &assets[2],
                Some(true),
            )?)
        };
        let workers = rayon::ThreadPoolBuilder::new()
            .num_threads(
                std::thread::available_parallelism()
                    .map_or(1, |n| n.get())
                    .min(4),
            )
            .build()?;
        let dimensions = match &model {
            Encoder::Static(model) => {
                workers.install(|| model.encode_single("dimension probe").len())
            }
            Encoder::Contextual(model) => model.dimensions,
        };
        ensure!(dimensions > 0, "embedding model has no dimensions");
        Ok(Self {
            model,
            encoding,
            workers,
            fingerprint: hash.finalize().to_hex().to_string(),
            dimensions,
        })
    }

    pub fn encode(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let vectors = self.workers.install(|| match &self.model {
            Encoder::Static(model) => Ok(model.encode_with_args(
                texts,
                (self.encoding != EncodingPolicy::Full).then_some(512),
                128,
            )),
            Encoder::Contextual(model) => model.encode(texts),
        })?;
        ensure!(
            vectors.len() == texts.len(),
            "model returned an unexpected vector count"
        );
        for vector in &vectors {
            ensure!(
                vector.len() == self.dimensions && vector.iter().all(|v| v.is_finite()),
                "model returned an invalid embedding"
            );
        }
        Ok(vectors)
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

    pub(crate) fn supports_contextual_focus(&self) -> bool {
        !self.contextual() && self.encoding == EncodingPolicy::Legacy
    }

    fn contextual(&self) -> bool {
        matches!(self.model, Encoder::Contextual(_))
    }
}

pub(crate) fn document(symbol: Option<&str>, path: &str, docs: &str, source: &str) -> String {
    // Preserve code spelling and natural comments; FTS stems must never become model input.
    format!(
        "{}\n{}\n{}\n{}",
        symbol.unwrap_or("").chars().take(512).collect::<String>(),
        path.chars().take(512).collect::<String>(),
        docs.chars().take(768).collect::<String>(),
        source.chars().take(6144).collect::<String>()
    )
}

#[derive(Debug, Serialize)]
pub struct EmbeddingReport {
    pub snapshot: String,
    pub model_fingerprint: String,
    pub dimensions: usize,
    pub regions: usize,
    pub representation: Representation,
    pub encoded: usize,
    pub reused: usize,
    pub elapsed_ms: u128,
}

#[derive(Debug, Clone, Copy)]
pub struct EmbeddingProgress {
    pub encoded: usize,
    pub reused: usize,
    pub total: usize,
}

/// Fill missing region vectors in one transaction. Model changes rebuild vectors; source changes
/// cascade away invalid rows. A failed update cannot publish a partially embedded generation.
pub fn embed_index(db: &Path, model_path: &Path) -> Result<EmbeddingReport> {
    embed_index_with(db, model_path, Representation::Separate)
}

pub fn embed_index_with(
    db: &Path,
    model_path: &Path,
    representation: Representation,
) -> Result<EmbeddingReport> {
    embed_index_with_progress(db, model_path, representation, |_| {})
}

pub fn embed_index_with_progress(
    db: &Path,
    model_path: &Path,
    representation: Representation,
    progress: impl FnMut(EmbeddingProgress),
) -> Result<EmbeddingReport> {
    embed_index_with_options(
        db,
        model_path,
        representation,
        EncodingPolicy::Legacy,
        progress,
    )
}

pub fn embed_index_with_options(
    db: &Path,
    model_path: &Path,
    representation: Representation,
    encoding: EncodingPolicy,
    mut progress: impl FnMut(EmbeddingProgress),
) -> Result<EmbeddingReport> {
    let started = Instant::now();
    let model = Embedder::load_with_encoding(model_path, encoding)?;
    let mut conn = store::open(db, true)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let snapshot =
        store::metadata(&tx, "snapshot")?.context("index sources before embedding them")?;
    ensure!(
        matches!(
            store::metadata(&tx, "extraction_version")?.as_deref(),
            Some(
                "4" | "5-ast"
                    | "6-adapters"
                    | "6-adapters-ast"
                    | "7-typescript"
                    | "7-typescript-ast"
            )
        ),
        "run `sonar index` to refresh embedding inputs"
    );
    if store::metadata(&tx, "embedding_model")?.as_deref() != Some(&model.fingerprint)
        || store::metadata(&tx, "embedding_representation")?.as_deref()
            != Some(representation.as_str())
    {
        tx.execute("DELETE FROM embeddings", [])?;
    }
    // Repair damaged rows as well as missing rows when explicitly updating vectors.
    let mut invalid = Vec::new();
    {
        let mut stmt = tx.prepare("SELECT region_id, vector FROM embeddings")?;
        let mut rows = stmt.query([])?;
        let zero = vec![0.0; model.dimensions];
        while let Some(row) = rows.next()? {
            if dot_bytes(&zero, row.get_ref(1)?.as_blob()?).is_err() {
                invalid.push(row.get::<_, i64>(0)?);
            }
        }
    }
    for id in invalid {
        tx.execute("DELETE FROM embeddings WHERE region_id = ?1", [id])?;
    }
    let reused = tx.query_row("SELECT count(*) FROM embeddings", [], |row| {
        row.get::<_, u32>(0)
    })? as usize;
    let mut encoded = 0;
    let total = tx.query_row("SELECT count(*) FROM regions WHERE (?1='combined' AND view='combined') OR (?1 IN ('separate','context') AND view!='combined')", [representation.as_str()], |r| r.get::<_,u32>(0))? as usize;
    progress(EmbeddingProgress {
        encoded,
        reused,
        total,
    });
    if model.contextual() {
        tx.execute("CREATE INDEX IF NOT EXISTS regions_embedding_size ON regions(length(embedding_text), id)", [])?;
    }
    loop {
        let order = if model.contextual() {
            "length(r.embedding_text), r.id"
        } else {
            "r.id"
        };
        let inputs: Vec<(i64, String)> = tx.prepare(&format!(
            "SELECT r.id, CASE WHEN ?1='context' AND r.view!='identity'
             THEN substr(f.path,1,256) || char(10) || coalesce((SELECT substr(i.embedding_text,1,512) FROM regions i WHERE i.location_id=r.location_id AND i.view='identity' ORDER BY i.id LIMIT 1),'') || char(10) || r.embedding_text
             ELSE r.embedding_text END
             FROM regions r LEFT JOIN embeddings e ON e.region_id = r.id
             JOIN locations l ON l.id=r.location_id JOIN files f ON f.id=l.file_id
             WHERE e.region_id IS NULL AND ((?1 = 'combined' AND r.view='combined') OR (?1 IN ('separate','context') AND r.view != 'combined')) ORDER BY {order} LIMIT 128"))?
            .query_map([representation.as_str()], |row| Ok((row.get(0)?, row.get(1)?)))?.collect::<Result<_, _>>()?;
        if inputs.is_empty() {
            break;
        }
        let texts: Vec<_> = inputs.iter().map(|(_, text)| text.clone()).collect();
        let vectors = model.encode(&texts)?;
        let mut insert = tx.prepare("INSERT INTO embeddings(region_id, vector) VALUES (?1, ?2)")?;
        for ((id, _), vector) in inputs.iter().zip(vectors) {
            let bytes: Vec<u8> = vector
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect();
            insert.execute(params![id, bytes])?;
            encoded += 1;
        }
        progress(EmbeddingProgress {
            encoded,
            reused,
            total,
        });
    }
    for (key, value) in [
        ("embedding_model", model.fingerprint.as_str()),
        ("embedding_snapshot", snapshot.as_str()),
        ("embedding_dimensions", &model.dimensions.to_string()),
        ("embedding_representation", representation.as_str()),
        ("embedding_encoding", encoding.as_str()),
    ] {
        tx.execute("INSERT INTO metadata(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value", params![key, value])?;
    }
    tx.commit()?;
    Ok(EmbeddingReport {
        snapshot,
        model_fingerprint: model.fingerprint,
        dimensions: model.dimensions,
        regions: reused + encoded,
        representation,
        encoded,
        reused,
        elapsed_ms: started.elapsed().as_millis(),
    })
}

pub(crate) fn dot_bytes(query: &[f32], bytes: &[u8]) -> Result<f64> {
    ensure!(
        bytes.len() == query.len() * 4,
        "stored vector dimensions do not match the model; rerun `sonar embed`"
    );
    let mut sum = 0.0f64;
    for (&q, &chunk) in query.iter().zip(bytes.as_chunks::<4>().0) {
        let value = f32::from_le_bytes(chunk);
        ensure!(
            value.is_finite(),
            "stored embedding contains non-finite values"
        );
        sum += f64::from(q) * f64::from(value);
    }
    Ok(sum)
}
