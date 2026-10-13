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
        m if m.starts_with("sync_") => sync_call(core, m, args).await,
        other => Err(format!("unknown method '{other}'")),
    }
}

fn arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args[key].as_str().ok_or_else(|| format!("missing {key}"))
}

fn val<T: serde::Serialize>(v: T) -> Result<Value, String> {
    serde_json::to_value(v).map_err(|e| e.to_string())
}

/// The `sync_*` methods (§11.1). They fail with "sync is turned off" unless `sync.enabled`;
/// `sync_status` and `sync_enable` always work.
async fn sync_call(core: &std::sync::Arc<Core>, method: &str, args: Value) -> Result<Value, String> {
    match method {
        "sync_enable" => {
            let mut c = core.config();
            c.sync.enabled = args["enabled"].as_bool().ok_or("missing enabled")?;
            core.set_config(c).await.map_err(|e| e.to_string())?;
            Ok(json!({ "enabled": core.config().sync.enabled }))
        }
        // uninstall on Windows: the sync roots go back to plain folders; needs no running sync
        "sync_unregister_roots" => {
            let (n, errors) = core.sync_unregister_roots().await;
            Ok(json!({ "unregistered": n, "errors": errors.into_iter().map(|(p, e)| json!({ "path": p, "error": e })).collect::<Vec<_>>() }))
        }
        "sync_status" => match core.sync() {
            Ok(m) => Ok(json!({ "enabled": true, "syncs": m.status(), "conflicts": m.conflicts().len(), "hosting": m.hosting_status().await })),
            Err(_) => Ok(json!({ "enabled": false, "syncs": [], "conflicts": 0 })),
        },
        _ => {
            let m = core.sync()?;
            match method {
                "sync_activity" => Ok(json!({ "items": m.status().into_iter().map(|s| json!({
                    "sync_id": s.sync_id, "name": s.name, "last_sync": s.last_sync,
                    "downloaded": s.downloaded, "uploaded": s.uploaded, "pushed": s.pushed,
                })).collect::<Vec<_>>() })),
                // hosts
                "sync_pair" => val(core.sync_pair(arg(&args, "code")?).await?),
                "sync_hosts" => Ok(json!({ "hosts": m.hosts() })),
                "sync_forget_host" => {
                    m.forget_host(arg(&args, "id")?).await?;
                    Ok(json!({}))
                }
                "sync_available" => Ok(json!({ "syncs": m.available(arg(&args, "host")?).await? })),
                // membership
                "sync_join" => {
                    let path = PathBuf::from(arg(&args, "path")?);
                    let f = core
                        .sync_join(
                            arg(&args, "host")?,
                            arg(&args, "sync_id")?,
                            &path,
                            args["frontend"].as_str().unwrap_or("auto"),
                            args["conflict"].as_str().unwrap_or("server_wins"),
                        )
                        .await?;
                    val(f)
                }
                "sync_leave" => {
                    core.sync_leave(arg(&args, "sync_id")?, args["keep_files"].as_bool().unwrap_or(true)).await?;
                    Ok(json!({}))
                }
                "sync_set" => val(m.set_folder(arg(&args, "sync_id")?, &args).await?),
                "sync_pause" | "sync_resume" => {
                    let paused = method == "sync_pause";
                    match args["sync_id"].as_str() {
                        Some(id) => {
                            m.set_folder(id, &json!({ "paused": paused })).await?;
                        }
                        None => m.set_all_paused(paused).await?,
                    }
                    Ok(json!({}))
                }
                "sync_verify" => {
                    m.verify(arg(&args, "sync_id")?)?;
                    Ok(json!({}))
                }
                // files
                "sync_browse" => Ok(json!({ "items": m.browse(arg(&args, "sync_id")?, args["path"].as_str().unwrap_or(""))? })),
                "sync_pin" => {
                    m.pin(arg(&args, "sync_id")?, args["path"].as_str().unwrap_or(""))?;
                    Ok(json!({}))
                }
                "sync_unpin" => {
                    m.unpin(arg(&args, "sync_id")?, args["path"].as_str().unwrap_or(""), args["evict"].as_bool().unwrap_or(true))?;
                    Ok(json!({}))
                }
                // conflicts
                "sync_conflicts" => Ok(json!({ "conflicts": m.conflicts() })),
                "sync_resolve" => {
                    let resolved = m.resolve(arg(&args, "sync_id")?, arg(&args, "path")?, arg(&args, "choice")?).await?;
                    Ok(json!({ "resolved": resolved }))
                }
                "sync_confirm_deletes" => {
                    m.confirm_deletes(arg(&args, "sync_id")?)?;
                    Ok(json!({}))
                }
                // history
                "sync_versions" => Ok(json!({ "versions": m.versions(arg(&args, "sync_id")?, arg(&args, "path")?).await?.into_iter().map(|v| json!({
                    "rev": v.rev, "hash": v.hash, "size": v.size, "mtime": v.mtime, "author": v.author, "created": v.created,
                })).collect::<Vec<_>>() })),
                "sync_snapshots" => Ok(json!({ "snapshots": m.snapshots(arg(&args, "sync_id")?).await?.into_iter().map(|s| json!({
                    "id": s.id, "seq": s.seq, "created": s.created, "entries": s.entries,
                })).collect::<Vec<_>>() })),
                "sync_restore" => {
                    let n = m.restore(arg(&args, "sync_id")?, &args["what"], args["as_copy"].as_bool().unwrap_or(false)).await?;
                    Ok(json!({ "restored": n }))
                }
                // hosting
                "sync_hosting" => val(m.hosting_status().await),
                "sync_host_add" => {
                    let store = args["store"].as_str().map(PathBuf::from);
                    let c = m
                        .host_add(arg(&args, "name")?, &PathBuf::from(arg(&args, "root")?), args["backup"].as_str().unwrap_or(""), store)
                        .await?;
                    Ok(json!({ "id": c.id, "name": c.name, "root": c.root, "backup": c.backup.describe() }))
                }
                "sync_host_set" => {
                    let ignore = args["ignore"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect());
                    let c = m.host_set(arg(&args, "sync")?, args["backup"].as_str(), ignore).await?;
                    Ok(json!({ "id": c.id, "name": c.name, "backup": c.backup.describe(), "ignore": c.ignore }))
                }
                "sync_host_remove" => {
                    m.host_remove(arg(&args, "sync")?).await?;
                    Ok(json!({}))
                }
                "sync_host_snapshot" => Ok(json!({ "id": m.host_snapshot(arg(&args, "sync")?).await? })),
                "sync_invite" => {
                    let grants: Vec<(String, String)> = args["grants"]
                        .as_array()
                        .ok_or("missing grants")?
                        .iter()
                        .map(|g| {
                            Ok((
                                g["sync"].as_str().ok_or("grant without sync")?.to_string(),
                                g["access"].as_str().unwrap_or("rw").to_string(),
                            ))
                        })
                        .collect::<Result<_, &str>>()?;
                    Ok(json!({ "code": m.invite(&grants).await? }))
                }
                "sync_clients" => Ok(json!({ "clients": m.clients().await })),
                "sync_grant" => {
                    m.grant(arg(&args, "client")?, arg(&args, "sync")?, args["access"].as_str().unwrap_or("rw")).await?;
                    Ok(json!({}))
                }
                "sync_revoke" => {
                    match args["sync"].as_str() {
                        Some(s) => m.grant(arg(&args, "client")?, s, "none").await?,
                        None => m.revoke_client(arg(&args, "client")?).await?,
                    }
                    Ok(json!({}))
                }
                other => Err(format!("unknown method '{other}'")),
            }
        }
    }
}
