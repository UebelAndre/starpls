//! Bazel information supplied by the editor through `initializationOptions`.
//!
//! When the server runs where it cannot spawn Bazel (WebAssembly), the editor
//! runs the commands itself and passes the results here:
//!
//! ```json
//! {
//!   "bazel": {
//!     "info": {
//!       "workspace": "/workspace",
//!       "outputBase": "/output_base",
//!       "release": "release 8.0.0",
//!       "starlarkSemantics": "...",
//!       "workspaceName": null
//!     },
//!     "buildLanguagePath": "/data/build-language.pb",
//!     "repoMappings": { "": { "bazel_skylib": "bazel_skylib+" } },
//!     "workspaceTargets": ["//foo:bar"],
//!     "requests": true
//!   }
//! }
//! ```
//!
//! With `requests` set, anything missing above is asked for when first needed
//! through the `starpls/bazel/repoMapping`, `starpls/bazel/buildLanguage` and
//! `starpls/bazel/fetchRepo` requests (see `crate::extensions`); a request
//! that depends on the answer waits for it, up to a bound.
//!
//! Paths are as the server sees them, so with a WebAssembly file system they
//! are the mount points, not the host paths. `repoMappings` is keyed by the
//! canonical repository the mapping belongs to; the main repository is `""`.
//! The same options work for the native binary, which is useful for tests.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use anyhow::Context;
use serde::Deserialize;
use starpls_bazel::client::BazelInfo;
use starpls_bazel::client::StaticBazelClient;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HostBazelData {
    info: HostBazelInfo,
    build_language_path: Option<PathBuf>,
    #[serde(default)]
    repo_mappings: HashMap<String, HashMap<String, String>>,
    #[serde(default)]
    workspace_targets: Vec<String>,
    /// Whether the client answers `starpls/bazel/*` requests for data it did
    /// not supply here.
    #[serde(default)]
    requests: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HostBazelInfo {
    workspace: PathBuf,
    output_base: PathBuf,
    #[serde(default)]
    release: String,
    #[serde(default)]
    starlark_semantics: String,
    workspace_name: Option<String>,
}

/// Builds a client from the `bazel` member of the initialization options.
/// Returns `None` when the options carry no Bazel data, in which case the
/// server runs Bazel itself as before.
pub(crate) fn bazel_client_from_options(
    options: &serde_json::Value,
) -> Option<anyhow::Result<StaticBazelClient>> {
    let data = options.get("bazel")?;
    Some(build(data.clone()))
}

fn build(data: serde_json::Value) -> anyhow::Result<StaticBazelClient> {
    let data: HostBazelData =
        serde_json::from_value(data).context("invalid `bazel` initialization options")?;
    let build_language = match &data.build_language_path {
        Some(path) => Some(
            fs::read(path)
                .with_context(|| format!("reading build language from {}", path.display()))?,
        ),
        None => None,
    };
    Ok(StaticBazelClient::new(
        BazelInfo {
            output_base: data.info.output_base,
            release: data.info.release,
            starlark_semantics: data.info.starlark_semantics,
            workspace: data.info.workspace,
            workspace_name: data.info.workspace_name,
        },
        build_language,
        data.repo_mappings,
        data.workspace_targets,
    )
    .with_client_requests(data.requests))
}
