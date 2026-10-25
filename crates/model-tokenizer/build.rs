use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{collections::HashSet, env, fs, path::PathBuf};

#[derive(Deserialize)]
struct Manifest {
    schema_version: u32,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    id: String,
    filename: String,
    format: String,
    sha256: String,
    size_bytes: u64,
    sources: Vec<Source>,
}

#[derive(Deserialize)]
struct Source {
    model: String,
    repository: String,
    revision: String,
    url: String,
    license: String,
    license_url: String,
}

fn main() {
    println!("cargo:rerun-if-env-changed=MAPLE_TOKENIZER_ASSETS_DIR");
    println!("cargo:rerun-if-changed=assets/manifest.json");
    println!("cargo:rerun-if-changed=assets/kimi-k3-tokenizer-config.json");
    assert_eq!(
        format!(
            "{:x}",
            Sha256::digest(
                fs::read("assets/kimi-k3-tokenizer-config.json")
                    .expect("read pinned Kimi tokenizer configuration")
            )
        ),
        "5d0803c94db9cd78763499e0956c95fd5a225c14a727e5a6cf5db3f96f010a6e",
        "Kimi reserved-token configuration differs from reviewed source"
    );
    let manifest: Manifest = serde_json::from_slice(
        &fs::read("assets/manifest.json").expect("read pinned tokenizer manifest"),
    )
    .expect("parse pinned tokenizer manifest");
    assert_eq!(manifest.schema_version, 1, "unsupported tokenizer manifest");
    let directory = PathBuf::from(env::var_os("MAPLE_TOKENIZER_ASSETS_DIR").expect(
        "MAPLE_TOKENIZER_ASSETS_DIR must point to the offline, hash-pinned assets; use the repository Nix shell or asset fetcher",
    ))
    .canonicalize()
    .expect("resolve offline tokenizer asset directory");
    let mut ids = HashSet::new();
    let mut models = HashSet::new();
    let mut generated =
        String::from("// Generated from assets/manifest.json; never downloads assets.\n");
    generated.push_str("static EMBEDDED_ASSETS: &[EmbeddedAsset] = &[\n");
    for asset in &manifest.assets {
        assert!(ids.insert(asset.id.clone()), "duplicate tokenizer asset id");
        assert!(
            asset.filename == format!("{}.json", asset.sha256)
                || asset.filename == format!("{}.tiktoken", asset.sha256),
            "tokenizer filenames must be content addressed"
        );
        let path = directory.join(&asset.filename);
        println!("cargo:rerun-if-changed={}", path.display());
        let bytes = fs::read(&path)
            .unwrap_or_else(|_| panic!("missing pinned tokenizer asset {}", asset.id));
        assert_eq!(
            bytes.len() as u64,
            asset.size_bytes,
            "tokenizer asset length mismatch: {}",
            asset.id
        );
        assert_eq!(
            format!("{:x}", Sha256::digest(&bytes)),
            asset.sha256,
            "tokenizer asset hash mismatch: {}",
            asset.id
        );
        let format = match asset.format.as_str() {
            "huggingface_json" => "TokenizerEngine::HuggingFace",
            "tiktoken_ranks" => "TokenizerEngine::TiktokenRanks",
            _ => panic!("unsupported tokenizer format"),
        };
        generated.push_str(&format!(
            "EmbeddedAsset {{ id: {:?}, sha256: {:?}, engine: {format}, bytes: include_bytes!({:?}), sources: &[\n",
            asset.id, asset.sha256, path.to_str().expect("UTF-8 asset path")
        ));
        for source in &asset.sources {
            assert!(
                models.insert(source.model.clone()),
                "duplicate catalog model in manifest"
            );
            assert_eq!(
                source.revision.len(),
                40,
                "model source must pin full revision"
            );
            generated.push_str(&format!(
                "TokenizerSource {{ model: {:?}, repository: {:?}, revision: {:?}, url: {:?}, license: {:?}, license_url: {:?} }},\n",
                source.model, source.repository, source.revision, source.url, source.license, source.license_url
            ));
        }
        generated.push_str("] },\n");
    }
    generated.push_str("];\n");
    generated.push_str("static MODEL_PROVENANCE: &[TokenizerProvenance] = &[\n");
    for asset in &manifest.assets {
        let engine = match asset.format.as_str() {
            "huggingface_json" => "TokenizerEngine::HuggingFace",
            "tiktoken_ranks" => "TokenizerEngine::TiktokenRanks",
            _ => unreachable!(),
        };
        for source in &asset.sources {
            generated.push_str(&format!(
                "TokenizerProvenance {{ model: {:?}, asset_id: {:?}, asset_sha256: {:?}, engine: {engine}, accuracy: Accuracy::ExactTextPipeline, source: TokenizerSource {{ model: {:?}, repository: {:?}, revision: {:?}, url: {:?}, license: {:?}, license_url: {:?} }} }},\n",
                source.model, asset.id, asset.sha256, source.model, source.repository, source.revision, source.url, source.license, source.license_url
            ));
        }
    }
    generated.push_str("];\n");
    assert_eq!(
        models.len(),
        8,
        "manifest must contain the eight reviewed models"
    );
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo OUT_DIR"));
    fs::write(output.join("embedded_assets.rs"), generated)
        .expect("write embedded tokenizer assets");
}
