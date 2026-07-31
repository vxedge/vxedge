//! Experiment-workspace scaffolding for `vxedge experiment new`.
//!
//! A workspace is a folder: every file declares its `kind`, and everything is referenced by
//! unique id rather than by path, so renaming or restructuring breaks nothing. See
//! `docs/TASKS.md` Track D for the design and its rationale.
//!
//! The generated files use the **proposed** ARCHITECTURE.md §6 shape, which is a breaking
//! schema change still awaiting owner sign-off. Nothing in this crate parses it yet — the
//! scaffold exists so §6 can be reviewed against a workspace someone can open and edit
//! rather than against prose. The generated README says so plainly; claiming otherwise would
//! ship a scaffold whose output cannot run.
//!
//! Layering follows the house rule: [`WorkspacePlan::plan`] is pure data and unit-testable
//! anywhere, [`WorkspacePlan::write`] is the only part that touches the filesystem.

use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum ScaffoldError {
    #[error(
        "workspace id `{0}` is not usable — use lowercase letters, digits and '-', starting \
         with a letter (e.g. `h1-slice-isolation`). The id becomes both a directory name and \
         an OCI repository path component when the workspace is published, and OCI rejects \
         leading separators and uppercase."
    )]
    InvalidId(String),

    #[error(
        "`{}` already exists — pick another id, or remove that directory first",
        .0.display()
    )]
    AlreadyExists(PathBuf),

    #[error("writing `{}`: {source}", .path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Which starting workspace to generate.
///
/// An enum rather than a string so adding a template forces every match to be revisited
/// (house rule: enums over stringly-typed modes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Template {
    /// One zone, one compute node, one slice. Runs on any laptop, no GPU.
    Minimal,
    /// H1 shape: an adas slice measured while an infotainment slice is overloaded.
    Slicing,
}

impl Template {
    pub fn as_str(self) -> &'static str {
        match self {
            Template::Minimal => "minimal",
            Template::Slicing => "slicing",
        }
    }

    /// What a host must provide to run this template. Surfaced at scaffold time so an
    /// engineer learns the requirement before a long build rather than after one.
    pub fn requires_gpu(self) -> bool {
        match self {
            Template::Minimal | Template::Slicing => false,
        }
    }
}

impl std::str::FromStr for Template {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "minimal" => Ok(Template::Minimal),
            "slicing" => Ok(Template::Slicing),
            other => Err(format!(
                "unknown template `{other}` (expected: minimal, slicing)"
            )),
        }
    }
}

/// One file to be written, with a path relative to the workspace root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceFile {
    pub path: PathBuf,
    pub contents: String,
    /// Written with the owner-execute bit. `scripts/run.sh` is the only such file today; a
    /// campaign script that arrives non-executable is a papercut every recipient hits.
    pub executable: bool,
}

/// A planned workspace. Pure data: building one performs no I/O and touches no filesystem.
#[derive(Debug, Clone)]
pub struct WorkspacePlan {
    pub id: String,
    pub template: Template,
    pub files: Vec<WorkspaceFile>,
}

/// Ids become directory names *and* OCI repository path components on publish. OCI requires
/// lowercase alphanumerics with separators between them, so reject anything else here rather
/// than letting it fail at `experiment publish` — which is exactly how a `_`-prefixed name
/// bit us during the container publish work (see vxparts TASKS.md).
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.starts_with(|c: char| c.is_ascii_lowercase())
        && id.ends_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !id.contains("--")
}

impl WorkspacePlan {
    /// Build the workspace contents. Pure — no filesystem access.
    ///
    /// `dir` names the folder; `id` is the experiment's identity. They are usually the same, and
    /// [`WorkspacePlan::plan`] keeps them so. They must be separable because a migration wants
    /// the folder distinguished from the frozen manifest beside it (`exp-003-ws/`) while the
    /// experiment id stays `exp-003`, so results and comparisons line up — found while migrating
    /// exp-003, where the `.vxp` had to be hand-edited afterwards.
    pub fn plan_with_id(dir: &str, id: &str, template: Template) -> Result<Self, ScaffoldError> {
        if !valid_id(dir) {
            return Err(ScaffoldError::InvalidId(dir.to_string()));
        }
        if !valid_id(id) {
            return Err(ScaffoldError::InvalidId(id.to_string()));
        }

        let files = vec![
            // Named for the id, not the folder: the descriptor carries the workspace's identity,
            // and a file called after its directory says nothing the directory did not.
            WorkspaceFile {
                path: PathBuf::from(format!("{id}.vxp")),
                contents: vxp(id, template),
                executable: false,
            },
            WorkspaceFile {
                path: PathBuf::from("main.yaml"),
                contents: main_yaml(id, template),
                executable: false,
            },
            // `vehicle.yaml`, not `vehicle.yaml`: the name is purely pedagogical (documents route
            // by `kind`, never by path) and `car1` teaches nothing about what the file is for.
            WorkspaceFile {
                path: PathBuf::from("vehicle.yaml"),
                contents: vehicle_yaml(template),
                executable: false,
            },
            WorkspaceFile {
                path: PathBuf::from("parts.yaml"),
                contents: parts_yaml(template),
                executable: false,
            },
            WorkspaceFile {
                path: PathBuf::from("README.md"),
                contents: readme(id, template),
                executable: false,
            },
            // The campaign itself. An experiment that cannot be run without knowing which
            // shared script to invoke is not shareable; this one resolves its own workspace
            // from its own location, so the sandbox works wherever it is unzipped.
            WorkspaceFile {
                path: PathBuf::from("scripts/run.sh"),
                contents: run_sh(id),
                executable: true,
            },
            // The rest of the sandbox. Directories rather than files, because what goes in them
            // is the experiment's own: a local loadable, a scenario asset, an analysis tool.
            // Each carries a README rather than a bare .gitkeep — an empty directory with no
            // explanation is a directory people put the wrong thing in.
            WorkspaceFile {
                path: PathBuf::from("parts/README.md"),
                contents: parts_dir_readme(),
                executable: false,
            },
            WorkspaceFile {
                path: PathBuf::from("assets/README.md"),
                contents: assets_dir_readme(),
                executable: false,
            },
            WorkspaceFile {
                path: PathBuf::from("tools/README.md"),
                contents: tools_dir_readme(),
                executable: false,
            },
            // Keeps the results directory in place before the first run, and in git.
            WorkspaceFile {
                path: PathBuf::from("results/.gitkeep"),
                contents: String::new(),
                executable: false,
            },
        ];

        Ok(WorkspacePlan {
            id: dir.to_string(),
            template,
            files,
        })
    }

    /// Scaffold a workspace whose folder and experiment id are the same — the common case.
    pub fn plan(id: &str, template: Template) -> Result<Self, ScaffoldError> {
        Self::plan_with_id(id, id, template)
    }

    /// Write the planned workspace under `parent`. The only I/O in this module.
    ///
    /// Refuses to write into an existing directory: silently merging into someone's folder is
    /// a far worse failure than asking them to pick another name.
    pub fn write(&self, parent: &Path) -> Result<PathBuf, ScaffoldError> {
        let root = parent.join(&self.id);
        if root.exists() {
            return Err(ScaffoldError::AlreadyExists(root));
        }

        for file in &self.files {
            let target = root.join(&file.path);
            if let Some(dir) = target.parent() {
                std::fs::create_dir_all(dir).map_err(|source| ScaffoldError::Io {
                    path: dir.to_path_buf(),
                    source,
                })?;
            }
            std::fs::write(&target, &file.contents).map_err(|source| ScaffoldError::Io {
                path: target.clone(),
                source,
            })?;
            #[cfg(unix)]
            if file.executable {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).map_err(
                    |source| ScaffoldError::Io {
                        path: target.clone(),
                        source,
                    },
                )?;
            }
        }

        Ok(root)
    }
}

/// The campaign runner every sandbox ships with.
///
/// A working skeleton, not a placeholder: it resolves its own workspace from its own location
/// (so the sandbox runs wherever it is unzipped), refuses politely without the privileges
/// provisioning needs, and drives `vxedge experiment up|down` rather than re-implementing the
/// daemon's sequence in bash. The knobs at the top are *method* — repetitions, warmup, window —
/// which is the half of a campaign that genuinely belongs to the experiment.
fn run_sh(id: &str) -> String {
    format!(
        r#"#!/usr/bin/env bash
# {id} — campaign runner.
#
# Self-contained: this script resolves the workspace from its own location, so the experiment
# runs wherever it is unpacked. Provisioning is `vxedge experiment up`; what stays here is the
# method — how many repetitions, how long to warm up, how long to measure.
#
#   sudo -E env "PATH=$PATH" ./scripts/run.sh [--quick]
set -euo pipefail

WS="$(cd "$(dirname "${{BASH_SOURCE[0]}}")/.." && pwd)"
VXEDGE="${{VXEDGE:-vxedge}}"

QUICK=0
for arg in "$@"; do case "$arg" in
  --quick) QUICK=1 ;;
  *) echo "error: unknown argument '$arg' (supported: --quick)" >&2; exit 1 ;;
esac; done

# --- method -----------------------------------------------------------------
# --quick is pipeline validation only. Its sample counts are below tail-percentile
# support, so its numbers must never be reported as results.
if [[ $QUICK -eq 1 ]]; then
  REPS=1; WARMUP_S=3; WINDOW_S=12
else
  REPS=5; WARMUP_S=10; WINDOW_S=60
fi

if [[ $(id -u) -ne 0 ]]; then
  echo "error: provisioning needs CAP_NET_ADMIN — sudo -E env \"PATH=\$PATH\" $0" >&2
  exit 1
fi
command -v "$VXEDGE" >/dev/null || {{ echo "error: vxedge not on PATH (set VXEDGE=/path/to/vxedge)" >&2; exit 1; }}

RESULTS="$WS/results/$( [[ $QUICK -eq 1 ]] && echo quick || date +%Y-%m-%d )"
mkdir -p "$RESULTS"

cleanup() {{ "$VXEDGE" --chdir "$WS" experiment down >/dev/null 2>&1 || true; }}
trap cleanup EXIT

for rep in $(seq 1 "$REPS"); do
  echo "== rep$rep: experiment up"
  "$VXEDGE" --chdir "$WS" experiment up

  echo "   warmup ${{WARMUP_S}}s"
  sleep "$WARMUP_S"

  echo "   measuring ${{WINDOW_S}}s"
  "$VXEDGE" --chdir "$WS" traffic up
  sleep "$WINDOW_S"
  "$VXEDGE" --chdir "$WS" traffic down

  # Collection: point vxtelemetry at this workspace; the archive carries the workspace itself,
  # so a result is re-runnable from what is inside it.
  if command -v vxtelemetry >/dev/null; then
    vxtelemetry collect --workspace "$WS" --ingest-dir "$WS/results/.ingest" \
      --out "$RESULTS/rep$rep.zip"
  fi

  "$VXEDGE" --chdir "$WS" experiment down
done

echo "== done: $RESULTS"
"#
    )
}

fn parts_dir_readme() -> String {
    "# parts/ — this experiment's own loadables\n\n\
     Local parts live here: a guest bundle, a container context, a config layer this experiment\n\
     needs and nothing else does. `parts.yaml` references them by relative path\n\
     (`./parts/my-bundle`), which is a *local* ref and needs no digest — it is content-hashed\n\
     into the results archive instead.\n\n\
     Keeping them inside the sandbox is what makes the experiment shareable: a workspace whose\n\
     parts live three directories up is not something anyone can unzip and run.\n\n\
     A part that outgrows one experiment belongs in `vxparts`, published and digest-pinned.\n\
     That is a promotion, not a move: swap the relative ref for the published one.\n"
        .to_string()
}

fn assets_dir_readme() -> String {
    "# assets/ — inputs that are not software\n\n\
     Scenario files, recorded traces, sensor captures, a CARLA map — anything the experiment\n\
     reads that is not a loadable and not a manifest.\n\n\
     Large binaries do not belong in git. Keep the small ones here; for the rest, keep a\n\
     fetch script in `tools/` and the checksum here, so what ran is still identifiable.\n"
        .to_string()
}

fn tools_dir_readme() -> String {
    "# tools/ — analysis and helpers for this experiment\n\n\
     The notebook or script that turns this experiment's archives into its table, plus anything\n\
     else specific to it. Shared analysis belongs in the repo's own notebooks; what lives here\n\
     is what only this experiment needs.\n\n\
     `scripts/run.sh` runs the campaign; `tools/` is for everything after it.\n"
        .to_string()
}

fn vxp(id: &str, template: Template) -> String {
    format!(
        "# Package descriptor: identity and requirements, separate from `main.yaml` so a\n\
         # version bump never touches experiment content.\n\
         id: {id}\n\
         version: 0.1.0\n\
         entry: main.yaml\n\
         description: generated by `vxedge experiment new --template {t}`\n\
         requires:\n\
         \x20 gpu: none\n",
        t = template.as_str()
    )
}

fn main_yaml(id: &str, template: Template) -> String {
    let slices = match template {
        Template::Minimal => {
            "  slices:\n\
             \x20   - { id: adas, mode: vlan, vlan: 101, qos: { min_rate_mbps: 100, priority: 1 } }\n"
        }
        Template::Slicing => {
            "  slices:\n\
             \x20   - id: adas\n\
             \x20     mode: vlan            # switch to `s-nssai` for the H2 comparison —\n\
             \x20     vlan: 101             # the vehicle does not change at all\n\
             \x20     qos: { min_rate_mbps: 100, max_rate_mbps: 200, priority: 1 }\n\
             \x20   - id: infotainment\n\
             \x20     mode: vlan\n\
             \x20     vlan: 102\n\
             \x20     qos: { min_rate_mbps: 50, max_rate_mbps: 1000, priority: 3 }\n"
        }
    };

    let traffic = match template {
        Template::Minimal => {
            "  probe:\n\
             \x20   source: zonefl          # a component id; `internet` is the host-side reflector\n\
             \x20   target: internet\n\
             \x20   pps: 1000\n\
             \x20   payload_bytes: 200\n"
        }
        Template::Slicing => {
            "  adas_probe:\n\
             \x20   source: zonefl          # a component id; `internet` is the host-side reflector\n\
             \x20   target: internet\n\
             \x20   pps: 1000\n\
             \x20   payload_bytes: 200\n\
             \x20 infotainment_load:\n\
             \x20   source: zonerl\n\
             \x20   target: internet\n\
             \x20   bandwidth_mbps: 600     # the deliberate overload the adas slice must survive\n"
        }
    };

    format!(
        "kind: experiment\n\
         id: {id}\n\n\
         # What is under test, and which software is on it.\n\
         vehicles: [car1]\n\
         baseline: bl-local\n\n\
         # The vehicle names a slice; this file defines what that name means. Changing only\n\
         # this block gives a controlled comparison over a provably unchanged vehicle.\n\
         network:\n\
         {slices}\n\
         traffic_profiles:\n\
         {traffic}\n\
         # Repetitions and a discarded warmup are what separate a measurement from an anecdote.\n\
         method:\n\
         \x20 duration_s: 60\n\
         \x20 repetitions: 5\n\
         \x20 warmup_s: 10\n\n\
         telemetry:\n\
         \x20 prometheus:\n\
         \x20   scrape_targets: [\"127.0.0.1:9801\"]   # vxprobe's live exporter (host:port)\n\n\
         results:\n\
         \x20 artifact_dir: results/\n"
    )
}

fn vehicle_yaml(template: Template) -> String {
    let (extra_zone, extra_dev, extra_fn, extra_link) = match template {
        Template::Minimal => ("", "", "", ""),
        Template::Slicing => (
            "\x20 rear-left: {}\n",
            "\x20 zonerl: { role: zone-controller, zone: rear-left, backend: container }\n",
            "\x20 infotainment: { deployed_on: zonerl }\n",
            "\x20 - { id: trunk-rl, endpoints: [gw1:eth1, zonerl:eth0], slice: infotainment }\n",
        ),
    };

    format!(
        "kind: vehicle\n\
         id: car1\n\
         class: road          # road | air | marine | rail — the axis that keeps other mobility\n\
         subtype: car         # entities expressible; nothing is built on it yet\n\n\
         # Zones are REGIONS of the entity, not devices. Named rather than chosen from an enum,\n\
         # because front-left/rear-right are road conventions and an eVTOL has nacelles.\n\
         zones:\n\
         \x20 front-left: {{}}\n\
         {extra_zone}\
         \x20 central: {{}}\n\n\
         # Devices are units LOCATED IN zones. `role` says what one is; `backend` says how it is\n\
         # realised — H3's experimental variable, and required for anything on a slice, because\n\
         # the planner must know whether to build a veth or a tap.\n\
         devices:\n\
         \x20 zonefl: {{ role: zone-controller, zone: front-left, backend: container }}\n\
         {extra_dev}\
         \x20 hpc1:   {{ role: hpc, zone: central, backend: container, resources: {{ cpu: 4, memory: 8 }} }}\n\
         \x20 gw1:    {{ role: gateway, backend: container }}\n\n\
         # Functions are units of BEHAVIOUR, deployed ONTO devices. Software attaches here, so one\n\
         # device can host several and a study can redeploy without rewriting `channels`.\n\
         # `parts.yaml` targets these ids.\n\
         functions:\n\
         \x20 adas:    {{ deployed_on: zonefl }}\n\
         {extra_fn}\
         \x20 percept: {{ deployed_on: hpc1 }}\n\
         \x20 tcu:     {{ deployed_on: gw1 }}\n\n\
         # Transport: how bytes move. This is the wiring harness (ARCHITECTURE.md §7).\n\
         links:\n\
         \x20 - {{ id: trunk-fl,  endpoints: [gw1:eth0, zonefl:eth0], slice: adas }}\n\
         {extra_link}\
         \x20 - {{ id: trunk-hpc, endpoints: [gw1:eth2, hpc1:eth0] }}\n\n\
         # Interaction: what is exchanged, between FUNCTIONS. `carried_by` binds a channel to the\n\
         # links carrying it — what makes latency attributable per topic per slice, not just per\n\
         # link.\n\
         channels:\n\
         \x20 - id: adas.probe\n\
         \x20   from: adas\n\
         \x20   to: [percept]\n\
         \x20   protocol: udp\n\
         \x20   carried_by: [trunk-fl, trunk-hpc]\n"
    )
}

fn parts_yaml(template: Template) -> String {
    let targets = match template {
        Template::Minimal => "[adas]",
        Template::Slicing => "[adas, infotainment]",
    };
    format!(
        "kind: baseline\n\
         id: bl-local\n\n\
         # A published ref is pinned by digest; a local ref needs none — it is content-hashed\n\
         # into the results archive instead. So a local run is reproducible for you, and\n\
         # publishing is what makes it reproducible for anyone else.\n\
         #\n\
         # `kind:` says what a loadable *is*, and it is checked against the device's `backend:`\n\
         # — a container backend boots a container image, libvirt and qemu-microvm boot a guest\n\
         # bundle. State it wherever the ref does not: a published vxparts ref names the kind of\n\
         # node a part loads onto, not how it is packaged.\n\
         parts:\n\
         \x20 - ref: ghcr.io/vxedge/parts/agent/linux/app/probe:0.1.0\n\
         \x20   digest: sha256:33c9c8d8625bd5c6d1f4d3d0c561fdaa930e057fcd1fb24fb3badb402f25429d\n\
         \x20   kind: container            # these devices run `backend: container`\n\
         \x20   target_ids: {targets}     # one declaration, however many nodes share the part\n\n\
         \x20 - ref: ./parts/hpc-reflector\n\
         \x20   kind: container\n\
         \x20   target_ids: percept\n\n\
         \x20 - ref: ./parts/gw-tcu\n\
         \x20   kind: container\n\
         \x20   target_ids: tcu\n"
    )
}

fn readme(id: &str, template: Template) -> String {
    format!(
        "# {id}\n\n\
         Generated by `vxedge experiment new --template {t}`. This folder is a **sandbox**: it\n\
         owns everything the experiment needs, so it can be zipped, sent, unzipped and run.\n\n\
         ```bash\n\
         sudo -E env \"PATH=$PATH\" ./scripts/run.sh --quick   # pipeline validation\n\
         sudo -E env \"PATH=$PATH\" ./scripts/run.sh           # the campaign\n\
         ```\n\n\
         ## Layout\n\n\
         | Path | `kind` | What it owns |\n\
         |---|---|---|\n\
         | `{id}.vxp` | — | identity, version, entry point, host requirements |\n\
         | `main.yaml` | `experiment` | the test: slices, traffic, method, telemetry |\n\
         | `vehicle.yaml` | `vehicle` | the system under test: zones, devices, functions, links |\n\
         | `parts.yaml` | `baseline` | which software runs on which function |\n\
         | `scripts/run.sh` | — | the campaign: repetitions, warmup, measurement window |\n\
         | `parts/` | — | this experiment's own loadables |\n\
         | `assets/` | — | scenario files, traces, maps |\n\
         | `tools/` | — | analysis specific to this experiment |\n\
         | `results/` | — | archives, one per run |\n\n\
         Every YAML file declares its `kind` and `id`, and nothing references anything by path,\n\
         so you can rename them freely — or collapse them into one `---`-separated file. Add a\n\
         file and it joins the experiment; delete one you do not need.\n\n\
         ## What to change first\n\n\
         1. **The load** — `traffic_profiles` in `main.yaml`.\n\
         2. **The isolation mechanism** — `network.slices[].mode`. Switching `vlan` to\n\
         `s-nssai` is the whole difference between H1 and H2, over an unchanged vehicle.\n\
         3. **The vehicle** — `devices`, `functions` and `links` in `vehicle.yaml`.\n\
         Device ids name hardware; function ids name software, and are what `parts.yaml`\n\
         targets.\n\
         4. **The software** — `parts.yaml`. `target_ids` takes one id or a list.\n\
         5. **The method** — the knobs at the top of `scripts/run.sh`.\n\n\
         ## Sharing it\n\n\
         Zip the folder. Everything the experiment needs is inside it, which is the point of\n\
         the layout — a workspace whose parts live three directories up is not something anyone\n\
         can unpack and run.\n\n\
         `vxedge experiment publish` refuses while any local ref (`./parts/...`) is still\n\
         unpublished, for the same reason in reverse: a workspace handed to someone else with a\n\
         path from your disk in it is broken for them by construction. Publish those parts\n\
         first, and the references are rewritten to digests in place.\n",
        t = template.as_str()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_is_pure_and_complete() {
        let ws = WorkspacePlan::plan("h1-slice-isolation", Template::Slicing).expect("valid id");
        let names: Vec<_> = ws.files.iter().map(|f| f.path.to_string_lossy()).collect();
        assert!(names.contains(&"h1-slice-isolation.vxp".into()));
        assert!(names.contains(&"main.yaml".into()));
        assert!(names.contains(&"vehicle.yaml".into()));
        assert!(names.contains(&"parts.yaml".into()));
    }

    #[test]
    fn every_entity_file_declares_kind_and_id() {
        // The loader indexes by (kind, id); a generated file missing either would be
        // unaddressable, which is the one defect that would make the whole model unusable.
        let ws = WorkspacePlan::plan("demo", Template::Minimal).unwrap();
        for f in &ws.files {
            if f.path.extension().and_then(|e| e.to_str()) == Some("yaml") {
                assert!(f.contents.contains("kind: "), "{:?} has no kind", f.path);
                assert!(f.contents.contains("id: "), "{:?} has no id", f.path);
            }
        }
    }

    #[test]
    fn generated_yaml_parses() {
        for template in [Template::Minimal, Template::Slicing] {
            let ws = WorkspacePlan::plan("demo", template).unwrap();
            for f in &ws.files {
                let is_yaml = f.path.extension().and_then(|e| e.to_str()) == Some("yaml")
                    || f.path.to_string_lossy().ends_with(".vxp");
                if is_yaml {
                    serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&f.contents)
                        .unwrap_or_else(|e| panic!("{:?} is not valid YAML: {e}", f.path));
                }
            }
        }
    }

    #[test]
    fn slicing_template_defines_both_slices_and_the_overload() {
        let ws = WorkspacePlan::plan("demo", Template::Slicing).unwrap();
        let main = ws
            .files
            .iter()
            .find(|f| f.path.ends_with("main.yaml"))
            .unwrap();
        assert!(main.contents.contains("id: adas"));
        assert!(main.contents.contains("id: infotainment"));
        assert!(main.contents.contains("bandwidth_mbps: 600"));
        // Without repetitions the run yields a number with no interval, which is the
        // difference between a measurement and an anecdote.
        assert!(main.contents.contains("repetitions:"));
    }

    #[test]
    fn ids_that_oci_would_reject_are_refused_here() {
        for bad in [
            "_leading",
            "Upper",
            "trailing-",
            "has space",
            "",
            "a--b",
            "9start",
        ] {
            assert!(
                WorkspacePlan::plan(bad, Template::Minimal).is_err(),
                "expected `{bad}` to be rejected"
            );
        }
        for good in ["h1", "h1-slice-isolation", "exp-011", "a1"] {
            assert!(
                WorkspacePlan::plan(good, Template::Minimal).is_ok(),
                "expected `{good}` to be accepted"
            );
        }
    }

    #[test]
    fn write_refuses_to_clobber_an_existing_directory() {
        let tmp = std::env::temp_dir().join(format!("vxscaffold-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("taken")).unwrap();

        let ws = WorkspacePlan::plan("taken", Template::Minimal).unwrap();
        assert!(matches!(
            ws.write(&tmp),
            Err(ScaffoldError::AlreadyExists(_))
        ));

        let ws = WorkspacePlan::plan("fresh", Template::Minimal).unwrap();
        let root = ws.write(&tmp).expect("writes into a clean directory");
        assert!(root.join("main.yaml").is_file());
        assert!(root.join("results").is_dir());

        std::fs::remove_dir_all(&tmp).ok();
    }
}
