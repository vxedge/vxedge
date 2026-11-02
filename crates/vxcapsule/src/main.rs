//! Runs one WebAssembly capsule for a vxedge zone node, as its own process.
//!
//! A capsule is a WASI 0.2 command component, for a node simulable as a sandboxed control loop
//! rather than a full guest kernel. The `wasm-capsule` zone backend starts this binary inside the
//! node's network namespace; it never loads `wasmtime` into `vxedged`.
//!
//! It is a process rather than a thread of the daemon because an idle guest sits in a blocking
//! host call, where wasmtime's own interruption cannot reach it. A signal to a process stops it
//! there, and a process outlives a daemon restart.
//!
//! The capsule contract: `VX_NODE` names the node, stdout and stderr are the node's log, the
//! network is whatever the namespace holds (`eth0` is addressed before the capsule starts), and
//! nothing else of the host is reachable: no filesystem and no name lookup. Memory is capped at
//! [`MEMORY_LIMIT_BYTES`]. The runner's exit status is the guest's.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::p2::bindings::sync::Command;
use wasmtime_wasi::{I32Exit, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

/// The most linear memory one capsule may hold.
const MEMORY_LIMIT_BYTES: usize = 64 * 1024 * 1024;

/// Written to stderr once the guest is instantiated. The backend waits for this line.
const READY_LINE: &str = "vxcapsule: running";

const USAGE: &str = "usage: vxcapsule --node <node-id> <module.wasm>";

#[derive(Debug, thiserror::Error)]
enum CapsuleError {
    #[error("{USAGE}")]
    Usage,
    #[error(
        "cannot load `{path}`: {detail}. It must be a WASI 0.2 command component, for example \
         one built for `wasm32-wasip2`."
    )]
    Load { path: PathBuf, detail: String },
    #[error("cannot set up the WebAssembly engine: {0}. This is a defect in vxcapsule; report it.")]
    Engine(String),
    #[error(
        "cannot start `{path}`: {detail}. A capsule may import WASI 0.2 interfaces only, so \
         rebuild it without any others."
    )]
    Instantiate { path: PathBuf, detail: String },
    #[error("capsule `{node}` trapped: {detail}. See its source for the cause.")]
    Trap { node: String, detail: String },
}

#[derive(Debug, PartialEq)]
struct Args {
    node: String,
    module: PathBuf,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, CapsuleError> {
    let mut args = args.into_iter();
    let (flag, node, module) = (args.next(), args.next(), args.next());
    match (flag.as_deref(), node, module, args.next()) {
        (Some("--node"), Some(node), Some(module), None) if !node.is_empty() => Ok(Args {
            node,
            module: PathBuf::from(module),
        }),
        _ => Err(CapsuleError::Usage),
    }
}

struct Capsule {
    ctx: WasiCtx,
    table: ResourceTable,
    limits: StoreLimits,
}

impl WasiView for Capsule {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

/// What a capsule may reach. The network namespace is the boundary, so every address in it is
/// allowed. Nothing here opens a directory, so the guest has no filesystem.
fn wasi_policy(node: &str) -> WasiCtxBuilder {
    let mut builder = WasiCtxBuilder::new();
    builder
        .inherit_stdout()
        .inherit_stderr()
        .env("VX_NODE", node)
        .inherit_network()
        .allow_udp(true)
        .allow_tcp(true)
        .allow_ip_name_lookup(false);
    builder
}

fn run(args: &Args) -> Result<ExitCode, CapsuleError> {
    let engine = Engine::new(&Config::new()).map_err(|e| CapsuleError::Engine(e.to_string()))?;
    let component = load(&engine, &args.module)?;
    let mut linker = Linker::<Capsule>::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
        .map_err(|e| CapsuleError::Engine(e.to_string()))?;

    let mut store = Store::new(
        &engine,
        Capsule {
            ctx: wasi_policy(&args.node).build(),
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new()
                .memory_size(MEMORY_LIMIT_BYTES)
                .build(),
        },
    );
    store.limiter(|capsule| &mut capsule.limits);

    let command = Command::instantiate(&mut store, &component, &linker).map_err(|e| {
        CapsuleError::Instantiate {
            path: args.module.clone(),
            detail: format!("{e:#}"),
        }
    })?;
    eprintln!("{READY_LINE}");
    match command.wasi_cli_run().call_run(&mut store) {
        Ok(Ok(())) => Ok(ExitCode::SUCCESS),
        Ok(Err(())) => Ok(ExitCode::FAILURE),
        Err(e) => match e.downcast_ref::<I32Exit>() {
            Some(I32Exit(code)) => Ok(ExitCode::from(u8::try_from(*code).unwrap_or(1))),
            None => Err(CapsuleError::Trap {
                node: args.node.clone(),
                detail: format!("{e:#}"),
            }),
        },
    }
}

fn load(engine: &Engine, path: &Path) -> Result<Component, CapsuleError> {
    Component::from_file(engine, path).map_err(|e| CapsuleError::Load {
        path: path.to_path_buf(),
        detail: format!("{e:#}"),
    })
}

fn main() -> ExitCode {
    let result = parse_args(std::env::args().skip(1)).and_then(|args| run(&args));
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("vxcapsule: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Result<Args, CapsuleError> {
        parse_args(list.iter().map(|s| s.to_string()))
    }

    #[test]
    fn a_node_and_a_module_are_the_whole_command_line() {
        assert_eq!(
            args(&["--node", "sensor-01", "/parts/s.wasm"]).unwrap(),
            Args {
                node: "sensor-01".into(),
                module: "/parts/s.wasm".into()
            }
        );
    }

    #[test]
    fn anything_else_is_refused_with_the_usage() {
        for bad in [
            &[][..],
            &["--node", "n"],
            &["--node", "", "m.wasm"],
            &["--module", "n", "m.wasm"],
            &["--node", "n", "m.wasm", "extra"],
        ] {
            assert!(matches!(args(bad), Err(CapsuleError::Usage)), "{bad:?}");
        }
    }

    #[test]
    fn a_file_that_is_no_component_names_what_a_capsule_must_be() {
        let path = std::env::temp_dir().join(format!("vxcapsule-bad-{}", std::process::id()));
        std::fs::write(&path, b"not wasm").unwrap();
        let engine = Engine::new(&Config::new()).unwrap();
        let e = load(&engine, &path).err().unwrap().to_string();
        std::fs::remove_file(&path).unwrap();
        assert!(e.contains("WASI 0.2 command component"), "{e}");
    }

    #[test]
    fn a_missing_file_is_reported_with_its_path() {
        let engine = Engine::new(&Config::new()).unwrap();
        let e = load(&engine, Path::new("/nonexistent/x.wasm"))
            .err()
            .unwrap();
        assert!(e.to_string().contains("/nonexistent/x.wasm"), "{e}");
    }
}
