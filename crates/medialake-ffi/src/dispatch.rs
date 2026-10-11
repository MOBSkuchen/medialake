//! Method table behind `sn_call`.

use std::path::PathBuf;

use medialake_core::Core;
use medialake_core::config::Config;
use medialake_proto::models::{Model, Quality};
use serde_json::{Value, json};

pub async fn call(core: &std::sync::Arc<Core>, method: &str, args: Value) -> Result<Value, String> {
    match method {
        "search" => {
            let q = args["query"].as_str().unwrap_or_default();
            let hits = core.search(q).await.map_err(|e| e.to_string())?;
            Ok(json!({ "results": hits }))
        }
        "status" => Ok(serde_json::to_value(core.status()).unwrap()),
        "get_settings" => Ok(serde_json::to_value(core.config()).unwrap()),
        "set_settings" => {
            let cfg: Config = serde_json::from_value(args).map_err(|e| e.to_string())?;
            let reindex = core.set_config(cfg).await.map_err(|e| e.to_string())?;
            Ok(json!({ "reindex": reindex }))
        }
        "add_dir" => {
            let path = args["path"].as_str().ok_or("missing path")?;
            let recursive = args["recursive"].as_bool().unwrap_or(true);
            core.add_dir(medialake_core::config::normalize_path(&PathBuf::from(path)), recursive).await.map_err(|e| e.to_string())?;
            Ok(json!({}))
        }
        "remove_dir" => {
            let path = args["path"].as_str().ok_or("missing path")?;
            core.remove_dir(&PathBuf::from(path)).await.map_err(|e| e.to_string())?;
            Ok(json!({}))
        }
        "reindex_all" => {
            core.reindex_all().await;
            Ok(json!({}))
        }
        "pair_remote" => {
            let code = args["code"].as_str().ok_or("missing code")?;
            let r = core.pair_remote(code).await?;
            Ok(serde_json::to_value(r).unwrap())
        }
        "use_local" => {
            core.use_local().await?;
            Ok(json!({}))
        }
        "forget_remote" => {
            core.forget_remote()?;
            Ok(json!({}))
        }
        // `qualities`: the tiers of the configured model; `models`: every supported model with its tiers
        "models" => {
            let current = core.config().model;
            let models: Vec<Value> = Model::ALL
                .iter()
                .map(|m| {
                    json!({
                        "id": m.as_str(),
                        "name": m.display_name(),
                        "dims": m.dims(),
                        "supports_audio": m.supports_audio(),
                        "qualities": core.models_status(*m),
                    })
                })
                .collect();
            Ok(json!({ "model": current.as_str(), "qualities": core.models_status(current), "models": models, "dir": core.model_dir() }))
        }
        // `model` defaults to the configured one
        "download_model" => {
            let q = args["quality"].as_str().and_then(Quality::parse).ok_or("bad quality")?;
            let m = match args["model"].as_str() {
                Some(s) => Model::parse(s).ok_or("bad model")?,
                None => core.config().model,
            };
            core.download_quality(m, q).await?;
            Ok(json!({}))
        }
        "cancel_download" => {
            core.cancel_download();
            Ok(json!({}))
        }
        other => Err(format!("unknown method '{other}'")),
    }
}
