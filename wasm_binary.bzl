"""Builds a binary for a WebAssembly platform.

The transition also forces an optimized build: a fastbuild wasm carries debug
info and is several times larger, and a consumer that builds the artifact as
a dependency has no reason to know about that.
"""

def _wasm_transition_impl(settings, attr):
    return {
        "//command_line_option:compilation_mode": "opt",
        "//command_line_option:platforms": str(attr.platform),
    }

_wasm_transition = transition(
    implementation = _wasm_transition_impl,
    inputs = [],
    outputs = [
        "//command_line_option:compilation_mode",
        "//command_line_option:platforms",
    ],
)

def _wasm_binary_impl(ctx):
    return [DefaultInfo(files = depset(ctx.files.binary))]

wasm_binary = rule(
    implementation = _wasm_binary_impl,
    attrs = {
        "binary": attr.label(
            cfg = _wasm_transition,
            allow_files = True,
            doc = "The binary target to build for the platform.",
            mandatory = True,
        ),
        "platform": attr.label(
            doc = "The WebAssembly platform to build for.",
            mandatory = True,
        ),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
    doc = "The given binary built for a WebAssembly platform, optimized.",
)
