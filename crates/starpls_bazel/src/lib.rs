use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use prost::Message;

pub use crate::builtin::Builtins;
pub use crate::label::Label;
pub use crate::label::ParseError;

pub mod attr;
pub mod build_language;
pub mod client;
pub mod env;
pub mod label;

#[cfg(bazel)]
pub mod builtin {
    pub use builtin_proto::builtin::*;
}

#[cfg(not(bazel))]
pub mod builtin {
    include!(concat!(env!("OUT_DIR"), "/builtin.rs"));
}

#[cfg(bazel)]
pub mod build {
    pub use build_proto::blaze_query::*;
}

#[cfg(not(bazel))]
pub mod build {
    include!(concat!(env!("OUT_DIR"), "/blaze_query.rs"));
}

pub const BUILTINS_TYPES_DENY_LIST: &[&str] = &[
    "Attribute",
    "bool",
    "bytes",
    "builtin_function_or_method",
    "dict",
    "float",
    "function",
    "int",
    "list",
    "range",
    "string",
    "struct",
    "Target",
    "tuple",
    "None",
    "NoneType",
];

pub const BUILTINS_VALUES_DENY_LIST: &[&str] = &[
    "False",
    "True",
    "None",
    "abs",
    "all",
    "any",
    "bool",
    "dict",
    "dir",
    "enumerate",
    "fail",
    "float",
    "getattr",
    "hasattr",
    "hash",
    "int",
    "len",
    "list",
    "max",
    "min",
    "print",
    "range",
    "repr",
    "reversed",
    "sorted",
    "str",
    "tuple",
    "type",
    "zip",
];

pub const KNOWN_PROVIDER_TYPES: &[&str] = &[
    "AnalysisTestResultInfo",
    "AndroidNeverLinkLibrariesProvider",
    "ApkInfo",
    "BaselineProfileProvider",
    "CcInfo",
    "CcToolchainConfigInfo",
    "CcToolchainInfo",
    "ConstraintSettingInfo",
    "ConstraintValueInfo",
    "DebugPackageInfo",
    "DefaultInfo",
    "ExecutionInfo",
    "FeatureFlagInfo",
    "FilesToRunProvider",
    "GeneratedExtensionRegistryProvider",
    "IncompatiblePlatformProvider",
    "InstrumentedFilesInfo",
    "java_compilation_info",
    "java_output_jars",
    "JavaRuntimeInfo",
    "JavaToolchainInfo",
    "ObjcProvider",
    "OutputGroupInfo",
    "PackageSpecificationInfo",
    "PlatformInfo",
    "ProguardSpecProvider",
    "ProtoRegistryProvider",
    "PyInfo",
    "PyRuntimeInfo",
    "RunEnvironmentInfo",
    "TemplateVariableInfo",
    "ToolchainInfo",
    "ToolchainTypeInfo",
];

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum APIContext {
    Bzl,
    Build,
    Module,
    Repo,
    Workspace,
    Prelude,
    Cquery,
    Vendor,
}

pub fn load_builtins(path: impl AsRef<Path>) -> anyhow::Result<Builtins> {
    let data = fs::read(path)?;
    decode_builtins(&data)
}

pub fn decode_builtins(data: &[u8]) -> anyhow::Result<Builtins> {
    let builtins = Builtins::decode(data)?;
    Ok(builtins)
}

pub fn resolve_workspace(from: impl AsRef<Path>) -> io::Result<Option<(PathBuf, PathBuf)>> {
    const WORKSPACE_MARKERS: [&str; 4] =
        ["WORKSPACE", "WORKSPACE.bazel", "MODULE.bazel", "REPO.bazel"];
    const PACKAGE_MARKERS: [&str; 2] = ["BUILD", "BUILD.bazel"];

    // Probing the marker files keeps this off directory handles, which some
    // WebAssembly hosts cannot close once the walk reaches their virtual root.
    // The directories themselves are not stat'ed either: under Node's WASI a
    // symbolic link that leaves a preopened directory (an external repository
    // in Bazel's repo contents cache) fails to stat as the last path component
    // while paths through it resolve, and a marker probe only ever goes
    // through it.
    let mut package: Option<PathBuf> = None;
    for ancestor in from.as_ref().ancestors() {
        if WORKSPACE_MARKERS
            .iter()
            .any(|marker| ancestor.join(marker).is_file())
        {
            return Ok(Some((
                ancestor.to_path_buf(),
                package.unwrap_or_else(|| ancestor.to_path_buf()),
            )));
        }
        if package.is_none()
            && PACKAGE_MARKERS
                .iter()
                .any(|marker| ancestor.join(marker).is_file())
        {
            package = Some(ancestor.to_path_buf());
        }
    }

    Ok(None)
}
