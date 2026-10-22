#!/usr/bin/env python3
"""Compare Rust embeddings with model2vec==0.9.0 on the exact same local assets."""

import argparse
import json
import subprocess
from pathlib import Path

import numpy as np
from model2vec import StaticModel


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--model", type=Path, default=Path(".sonar/models/potion-code-16M-v2"))
    parser.add_argument("--binary", type=Path, default=Path("target/release/examples/encode"))
    args = parser.parse_args()
    texts = [
        "", "Where are process spawn workers configured and started?",
        "fn verify_local_peer(stream: &UnixStream) -> io::Result<()> { Ok(()) }",
        "naïve café 東京 🦀 authentication", "authentication " * 2000,
        "東京 🦀 " * 2000, "spawn_workers HTTPServer Default::default",
    ]
    contextual = json.loads((args.model / "config.json").read_text()).get("model_type") == "bert"
    if contextual:
        import torch
        from transformers import AutoModel, PreTrainedTokenizerFast
        from transformers.utils import logging
        logging.set_verbosity_error()
        logging.disable_progress_bar()
        torch.set_num_threads(4)
        limit = json.loads((args.model / "sentence_bert_config.json").read_text())["max_seq_length"]
        # Load the actual tokenizer JSON unchanged. AutoTokenizer without tokenizer_config.json
        # can replace its lowercase normalizer with a different default.
        tokenizer = PreTrainedTokenizerFast(tokenizer_file=str(args.model / "tokenizer.json"), pad_token="[PAD]")
        reference = AutoModel.from_pretrained(str(args.model), local_files_only=True).eval()
        batch = tokenizer(texts, padding=True, truncation=True, max_length=limit, return_tensors="pt")
        with torch.no_grad():
            states = reference(**batch).last_hidden_state
            mask = batch["attention_mask"].unsqueeze(-1)
            pooled = (states * mask).sum(1) / mask.sum(1)
            expected = torch.nn.functional.normalize(pooled, dim=1).numpy()
        reference_name = "transformers==5.0.0; torch==2.10.0+cpu"
    else:
        reference = StaticModel.from_pretrained(str(args.model), normalize=True)
        # Rust loads distributed F16 weights into F32 and accumulates in F32.
        reference.embedding = reference.embedding.astype(np.float32)
        limit = 512
        expected = reference.encode(texts, max_length=limit, use_multiprocessing=False)
        reference_name = "model2vec==0.9.0"
    completed = subprocess.run([str(args.binary.resolve()), str(args.model)],
        input="".join(json.dumps(text) + "\n" for text in texts), text=True, capture_output=True, check=True)
    actual = np.asarray(json.loads(completed.stdout), dtype=np.float32)
    np.testing.assert_allclose(actual, expected, atol=2e-5 if contextual else 2e-6, rtol=2e-5)
    norms = np.linalg.norm(actual, axis=1)
    nonzero = np.linalg.norm(expected, axis=1) > 0
    np.testing.assert_allclose(norms[nonzero], np.ones(np.count_nonzero(nonzero)), atol=2e-6)
    print(json.dumps({"cases": len(texts), "dimensions": actual.shape[1],
        "max_absolute_error": float(np.max(np.abs(actual - expected))),
        "python_reference": reference_name, "accumulation": "float32", "max_tokens": limit}, indent=2))


if __name__ == "__main__":
    main()
