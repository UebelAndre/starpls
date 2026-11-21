use std::collections::HashMap;

use lsp_types::request::Request;
use lsp_types::TextDocumentIdentifier;
use serde::Deserialize;
use serde::Serialize;

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ShowSyntaxTreeParams {
    pub text_document: TextDocumentIdentifier,
}

#[derive(Debug)]
pub enum ShowSyntaxTree {}

impl Request for ShowSyntaxTree {
    type Params = ShowSyntaxTreeParams;
    type Result = String;
    const METHOD: &'static str = "starpls/showSyntaxTree";
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ShowHirParams {
    pub text_document: TextDocumentIdentifier,
}

#[derive(Debug)]
pub enum ShowHir {}

impl Request for ShowHir {
    type Params = ShowHirParams;
    type Result = String;
    const METHOD: &'static str = "starpls/showHir";
}

/// Asks the client for the repository mapping of a canonical repository
/// (`""` for the main one), as `bazel mod dump_repo_mapping` prints it.
/// `null` means the client could not provide it.
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct BazelRepoMappingParams {
    pub repo: String,
}

#[derive(Debug)]
pub enum BazelRepoMapping {}

impl Request for BazelRepoMapping {
    type Params = BazelRepoMappingParams;
    type Result = Option<HashMap<String, String>>;
    const METHOD: &'static str = "starpls/bazel/repoMapping";
}

/// Asks the client for the path of a file holding the output of
/// `bazel info build-language`, in the server's view of the file system.
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct BazelBuildLanguageParams {}

#[derive(Debug)]
pub enum BazelBuildLanguage {}

impl Request for BazelBuildLanguage {
    type Params = BazelBuildLanguageParams;
    type Result = Option<String>;
    const METHOD: &'static str = "starpls/bazel/buildLanguage";
}

/// Asks the client to fetch a canonical repository into the output base.
/// The result says whether it succeeded.
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct BazelFetchRepoParams {
    pub repo: String,
}

#[derive(Debug)]
pub enum BazelFetchRepo {}

impl Request for BazelFetchRepo {
    type Params = BazelFetchRepoParams;
    type Result = bool;
    const METHOD: &'static str = "starpls/bazel/fetchRepo";
}
