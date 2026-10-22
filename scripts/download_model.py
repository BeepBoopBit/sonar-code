#!/usr/bin/env python3
"""Download a pinned embedding model for Sonar's optional local search."""

import argparse
import hashlib
import json
import os
import tempfile
import urllib.request
from pathlib import Path

REPOSITORY = "minishlab/potion-code-16M-v2"
REVISION = "e9d2a44ca6a05ac6685f3b23709ea57eb7352d5b"
SHA256 = {
    "config.json": "148e5691a6fcc553437156859701fba017a1ba5d340b170f17e0f3668fb861a7",
    "tokenizer.json": "107bbdcbad4bff1d299b7a4c3a2fb17c52890688b7dd0e4c9deab79d3c4f3d45",
    "model.safetensors": "75cf7a6c2171b230ad19b1e7d8e0b1aee86da5a02af8e7cacedd9921d227623c",
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--destination", type=Path)
    parser.add_argument("--model", choices=["potion", "minilm"], default="potion")
    args = parser.parse_args()
    repository, revision, checksums = REPOSITORY, REVISION, SHA256
    if args.model == "minilm":
        repository = "sentence-transformers/all-MiniLM-L6-v2"
        revision = "1110a243fdf4706b3f48f1d95db1a4f5529b4d41"
        checksums = {
            "config.json": "953f9c0d463486b10a6871cc2fd59f223b2c70184f49815e7efbcab5d8908b41",
            "tokenizer.json": "be50c3628f2bf5bb5e3a7f17b1f74611b2561a3a27eeab05e5aa30f411572037",
            "model.safetensors": "53aa51172d142c89d9012cce15ae4d6cc0ca6895895114379cacb4fab128d9db",
            "sentence_bert_config.json": "fc1993fde0a95c24ec6c022539d41cf6e2f7c9721e5415d6fb6897472a9cd4b7",
            "1_Pooling/config.json": "4be450dde3b0273bb9787637cfbd28fe04a7ba6ab9d36ac48e92b11e350ffc23",
        }
    if args.destination is None:
        args.destination = Path(".sonar/models") / ("all-MiniLM-L6-v2" if args.model == "minilm" else "potion-code-16M-v2")
    args.destination.mkdir(parents=True, exist_ok=True)
    provenance = {"repository": repository, "revision": revision, "files": {}}
    for name in checksums:
        url = f"https://huggingface.co/{repository}/resolve/{revision}/{name}"
        existing = args.destination / name
        if existing.is_file() and hashlib.sha256(existing.read_bytes()).hexdigest() == checksums[name]:
            data = existing.read_bytes()
        else:
            with urllib.request.urlopen(url, timeout=60) as response:
                data = response.read()
        digest = hashlib.sha256(data).hexdigest()
        if digest != checksums[name]:
            raise SystemExit(f"Checksum mismatch for {name}; existing asset was not replaced.")
        (args.destination / name).parent.mkdir(parents=True, exist_ok=True)
        with tempfile.NamedTemporaryFile(dir=args.destination, delete=False) as temp:
            temporary = Path(temp.name)
            temp.write(data)
        try:
            os.replace(temporary, args.destination / name)
        finally:
            temporary.unlink(missing_ok=True)
        provenance["files"][name] = {"sha256": digest, "bytes": len(data)}
    (args.destination / "provenance.json").write_text(json.dumps(provenance, indent=2) + "\n")
    print(json.dumps(provenance, indent=2))


if __name__ == "__main__":
    main()
