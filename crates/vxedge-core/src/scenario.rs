//! **Scenarios and roads** (`ARCHITECTURE.md` §6.0.3, decision X-8): what happens on the road
//! during a trial. A package carries ASAM OpenDRIVE and OpenSCENARIO files unchanged in `assets/`;
//! `kind: road` and `kind: scenario` only point at them and add what a reproducible trial needs:
//! the simulator, its clock and seed, which storyboard entity is the vxedge vehicle under test,
//! and the storyboard parameters a campaign may turn, with units and bounds.
//!
//! vxedge reads only what it checks: the `.xosc`'s entity names and `ParameterDeclaration`s, and
//! that the `.xodr` is an OpenDRIVE file. Checking the storyboard itself is the simulator's job.
//! Every problem is typed [`ProblemKind::Scenario`] (vxAPI `ERROR_REASON_SCENARIO_INVALID`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::intent::{Problem, ProblemKind};

/// The simulators a scenario may name.
pub const SIMULATORS: &[&str] = &["esmini", "carla"];

/// The executables that make a simulator available: `esmini`, or CARLA's server launcher.
pub fn binaries(simulator: &str) -> &'static [&'static str] {
    match simulator {
        "esmini" => &["esmini"],
        "carla" => &["CarlaUE4.sh", "CarlaUnreal.sh"],
        _ => &[],
    }
}

fn on_path(bin: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            std::fs::metadata(dir.join(bin))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    })
}

/// The simulators available here: those with one of their [`binaries`] on `PATH`.
pub fn available_simulators() -> Vec<String> {
    SIMULATORS
        .iter()
        .filter(|s| binaries(s).iter().any(|b| on_path(b)))
        .map(|s| s.to_string())
        .collect()
}

/// A road network: an ASAM OpenDRIVE file, unchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoadDoc {
    pub kind: String,
    pub id: String,
    /// The `.xodr` file, relative to the package.
    pub network: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Clock {
    pub tick_ms: u32,
    /// `synchronous`: the simulator is the master clock and steps on each tick.
    #[serde(default = "synchronous")]
    pub mode: String,
}

fn synchronous() -> String {
    "synchronous".into()
}

/// An OpenSCENARIO entity that is a vxedge vehicle under test.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Actor {
    pub vehicle: String,
}

/// A storyboard parameter a campaign may turn: its unit and bounds.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioParameter {
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
}

/// A scenario: an ASAM OpenSCENARIO storyboard on a road, with what a reproducible trial needs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioDoc {
    pub kind: String,
    pub id: String,
    /// A `kind: road` document, by id.
    pub road: String,
    /// The `.xosc` file, relative to the package.
    pub storyboard: String,
    /// `esmini` or `carla`; the package's `requires.simulator` names it too.
    pub simulator: String,
    #[serde(default)]
    pub clock: Option<Clock>,
    #[serde(default)]
    pub seed: Option<u64>,
    /// OpenSCENARIO entity name to the vxedge vehicle it is. Every other entity is traffic.
    #[serde(default)]
    pub actors: BTreeMap<String, Actor>,
    /// OpenSCENARIO `ParameterDeclaration` names, with units and bounds.
    #[serde(default)]
    pub parameters: BTreeMap<String, ScenarioParameter>,
}

/// A `ParameterDeclaration` of a storyboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declared {
    pub parameter_type: String,
    pub value: String,
}

/// What vxedge reads from a `.xosc`: its entity names and its parameter declarations.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Storyboard {
    pub entities: BTreeSet<String>,
    pub parameters: BTreeMap<String, Declared>,
}

fn element<'a, 'i>(node: roxmltree::Node<'a, 'i>, name: &str) -> Option<roxmltree::Node<'a, 'i>> {
    node.children()
        .find(|c| c.is_element() && c.tag_name().name() == name)
}

/// Read a storyboard's entities and parameter declarations, or why it cannot be read.
pub fn read_storyboard(text: &str) -> Result<Storyboard, String> {
    let xml = roxmltree::Document::parse(text).map_err(|e| format!("is not XML: {e}"))?;
    let root = xml.root_element();
    if root.tag_name().name() != "OpenSCENARIO" {
        return Err(format!(
            "is not an OpenSCENARIO file: its root element is `{}`",
            root.tag_name().name()
        ));
    }
    let mut out = Storyboard::default();
    if let Some(entities) = element(root, "Entities") {
        for object in entities
            .children()
            .filter(|c| c.is_element() && c.tag_name().name() == "ScenarioObject")
        {
            if let Some(name) = object.attribute("name") {
                out.entities.insert(name.to_string());
            }
        }
    }
    if let Some(decls) = element(root, "ParameterDeclarations") {
        for d in decls
            .children()
            .filter(|c| c.is_element() && c.tag_name().name() == "ParameterDeclaration")
        {
            if let Some(name) = d.attribute("name") {
                out.parameters.insert(
                    name.to_string(),
                    Declared {
                        parameter_type: d.attribute("parameterType").unwrap_or("").to_string(),
                        value: d.attribute("value").unwrap_or("").to_string(),
                    },
                );
            }
        }
    }
    Ok(out)
}

/// Why `text` is not an OpenDRIVE file, if it is not.
pub fn road_problem(text: &str) -> Option<String> {
    match roxmltree::Document::parse(text) {
        Err(e) => Some(format!("is not XML: {e}")),
        Ok(xml) if xml.root_element().tag_name().name() != "OpenDRIVE" => Some(format!(
            "is not an OpenDRIVE file: its root element is `{}`",
            xml.root_element().tag_name().name()
        )),
        Ok(_) => None,
    }
}

const NUMERIC: &[&str] = &["double", "integer", "unsignedInt", "unsignedShort", "int"];

/// Whether a declared parameter is a number, so a unit and bounds apply to it.
pub fn is_numeric(d: &Declared) -> bool {
    NUMERIC.contains(&d.parameter_type.as_str())
}

/// The roads and scenarios of a workspace, and what its experiment refers to.
pub struct Inputs<'a> {
    pub root: &'a Path,
    pub experiment: &'a str,
    /// The experiment's `scenario` (or legacy `world`), when it names one.
    pub scenario_ref: Option<&'a str>,
    pub roads: &'a [RoadDoc],
    pub scenarios: &'a [ScenarioDoc],
    /// The vehicle ids the package declares.
    pub vehicles: &'a [String],
    /// The descriptor's `requires.simulator`, when the package has a `vxp/1` descriptor.
    pub required_simulators: Option<&'a [String]>,
    /// The experiment's typed parameters, whose `binds` may name a storyboard parameter.
    pub parameters: &'a BTreeMap<String, crate::intent::Parameter>,
}

/// Every problem with the workspace's roads and scenarios.
pub fn check(inputs: &Inputs<'_>) -> Vec<Problem> {
    let mut problems = Vec::new();
    let mut push = |field: String, detail: String| {
        problems.push(Problem {
            kind: ProblemKind::Scenario,
            experiment: inputs.experiment.to_string(),
            field,
            detail,
        })
    };
    let read = |file: &str| {
        let path = inputs.root.join(file);
        std::fs::read_to_string(&path).map_err(|e| format!("`{file}` cannot be read: {e}"))
    };

    if inputs.scenarios.len() > 1 {
        push(
            "scenario".into(),
            format!(
                "a package has at most one scenario; this one has {}",
                inputs.scenarios.len()
            ),
        );
    }
    if let Some(id) = inputs.scenario_ref
        && !inputs.scenarios.iter().any(|s| s.id == id)
    {
        push(
            "scenario".into(),
            format!("`{id}` is not a `kind: scenario` document of this package"),
        );
    }
    for r in inputs.roads {
        let field = format!("roads[{}].network", r.id);
        match read(&r.network) {
            Err(why) => push(field, why),
            Ok(text) => {
                if let Some(why) = road_problem(&text) {
                    push(field, format!("`{}` {why}", r.network));
                }
            }
        }
    }
    let named = inputs
        .scenario_ref
        .and_then(|id| inputs.scenarios.iter().find(|s| s.id == id));
    for (name, p) in inputs.parameters {
        let Some(target) = p
            .binds
            .as_deref()
            .and_then(|b| b.strip_prefix(crate::intent::SCENARIO_BIND))
        else {
            continue;
        };
        let field = format!("parameters.{name}.binds");
        let Some(s) = named else {
            push(
                field,
                "binds a storyboard parameter, and the experiment names no scenario of this \
                 package"
                    .into(),
            );
            continue;
        };
        let Some(declared) = s.parameters.get(target) else {
            push(
                field,
                format!(
                    "`{target}` is not one of scenario `{}`'s `parameters`; it lists {}",
                    s.id,
                    list(&s.parameters.keys().cloned().collect())
                ),
            );
            continue;
        };
        use crate::intent::ParamType as T;
        let numeric = matches!(p.ty, T::Integer | T::Number);
        if numeric {
            let below = declared.min.zip(p.min).is_some_and(|(lo, x)| x < lo)
                || (declared.min.is_some() && p.min.is_none());
            let above = declared.max.zip(p.max).is_some_and(|(hi, x)| x > hi)
                || (declared.max.is_some() && p.max.is_none());
            if below || above {
                push(
                    field.clone(),
                    format!(
                        "the parameter's bounds [{}, {}] are not inside `{target}`'s [{}, {}], so \
                         a campaign could set a value the scenario does not allow",
                        p.min.map_or("-inf".into(), |v| v.to_string()),
                        p.max.map_or("inf".into(), |v| v.to_string()),
                        declared.min.map_or("-inf".into(), |v| v.to_string()),
                        declared.max.map_or("inf".into(), |v| v.to_string()),
                    ),
                );
            }
            if let (Some(pu), Some(du)) = (p.unit.as_deref(), declared.unit.as_deref())
                && pu != du
            {
                push(
                    field,
                    format!("the parameter is in `{pu}`, and `{target}` is in `{du}`"),
                );
            }
        }
    }
    for s in inputs.scenarios {
        let at = |f: &str| format!("scenarios[{}].{f}", s.id);
        if !inputs.roads.iter().any(|r| r.id == s.road) {
            push(
                at("road"),
                format!(
                    "`{}` is not a `kind: road` document of this package",
                    s.road
                ),
            );
        }
        if !SIMULATORS.contains(&s.simulator.as_str()) {
            push(
                at("simulator"),
                format!(
                    "`{}` is not a simulator vxedge knows: {}",
                    s.simulator,
                    SIMULATORS.join(", ")
                ),
            );
        } else if let Some(required) = inputs.required_simulators
            && !required.contains(&s.simulator)
        {
            push(
                at("simulator"),
                format!(
                    "`{}` is not in the descriptor's `requires.simulator`, so a host could \
                     accept the package without it",
                    s.simulator
                ),
            );
        }
        if let Some(clock) = &s.clock {
            if clock.tick_ms == 0 {
                push(at("clock.tick_ms"), "needs at least 1".into());
            }
            if !["synchronous", "asynchronous"].contains(&clock.mode.as_str()) {
                push(
                    at("clock.mode"),
                    format!("`{}` is not `synchronous` or `asynchronous`", clock.mode),
                );
            }
        }
        if s.actors.len() > 1 {
            push(
                at("actors"),
                format!(
                    "names {} vehicles; campaigns run serially, so a scenario has one ego",
                    s.actors.len()
                ),
            );
        }
        for (entity, actor) in &s.actors {
            if !inputs.vehicles.iter().any(|v| v == &actor.vehicle) {
                push(
                    at(&format!("actors.{entity}.vehicle")),
                    format!("`{}` is not a vehicle of this package", actor.vehicle),
                );
            }
        }
        for (name, p) in &s.parameters {
            if let (Some(lo), Some(hi)) = (p.min, p.max)
                && lo > hi
            {
                push(
                    at(&format!("parameters.{name}.min")),
                    format!("`min` {lo} is above `max` {hi}"),
                );
            }
        }
        let board = match read(&s.storyboard)
            .and_then(|t| read_storyboard(&t).map_err(|why| format!("`{}` {why}", s.storyboard)))
        {
            Err(why) => {
                push(at("storyboard"), why);
                continue;
            }
            Ok(board) => board,
        };
        for entity in s.actors.keys() {
            if !board.entities.contains(entity) {
                push(
                    at(&format!("actors.{entity}")),
                    format!(
                        "`{entity}` is not an entity of `{}`; it has {}",
                        s.storyboard,
                        list(&board.entities)
                    ),
                );
            }
        }
        for (name, p) in &s.parameters {
            match board.parameters.get(name) {
                None => push(
                    at(&format!("parameters.{name}")),
                    format!(
                        "`{name}` is not a ParameterDeclaration of `{}`; it declares {}",
                        s.storyboard,
                        list(&board.parameters.keys().cloned().collect())
                    ),
                ),
                Some(d) if !is_numeric(d) && (p.min.is_some() || p.max.is_some()) => push(
                    at(&format!("parameters.{name}")),
                    format!(
                        "`{name}` is declared `{}`, so it has no bounds",
                        d.parameter_type
                    ),
                ),
                Some(_) => {}
            }
        }
    }
    problems
}

fn list(items: &BTreeSet<String>) -> String {
    if items.is_empty() {
        "none".into()
    } else {
        items
            .iter()
            .map(|i| format!("`{i}`"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const XOSC: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<OpenSCENARIO>
  <FileHeader revMajor="1" revMinor="2" date="2026-11-10T00:00:00" description="stop line" author="vxedge"/>
  <ParameterDeclarations>
    <ParameterDeclaration name="EgoTargetSpeed" parameterType="double" value="13.9"/>
    <ParameterDeclaration name="LeadName" parameterType="string" value="lead"/>
  </ParameterDeclarations>
  <Entities>
    <ScenarioObject name="Ego"/>
    <ScenarioObject name="Lead"/>
  </Entities>
</OpenSCENARIO>"#;

    const XODR: &str =
        r#"<?xml version="1.0"?><OpenDRIVE><header revMajor="1" revMinor="6"/></OpenDRIVE>"#;

    #[test]
    fn a_storyboard_gives_its_entities_and_parameters() {
        let b = read_storyboard(XOSC).unwrap();
        assert_eq!(b.entities.iter().collect::<Vec<_>>(), ["Ego", "Lead"]);
        assert!(is_numeric(&b.parameters["EgoTargetSpeed"]));
        assert_eq!(b.parameters["EgoTargetSpeed"].value, "13.9");
        assert!(!is_numeric(&b.parameters["LeadName"]));
        assert!(
            read_storyboard("<OpenDRIVE/>")
                .unwrap_err()
                .contains("root element")
        );
        assert!(read_storyboard("not xml").unwrap_err().contains("not XML"));
        assert_eq!(road_problem(XODR), None);
        assert!(road_problem(XOSC).unwrap().contains("OpenSCENARIO"));
    }

    fn scenario(yaml: &str) -> ScenarioDoc {
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    #[test]
    fn every_reference_of_a_scenario_is_checked() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("assets")).unwrap();
        std::fs::write(dir.path().join("assets/stop.xosc"), XOSC).unwrap();
        std::fs::write(dir.path().join("assets/grid.xodr"), XODR).unwrap();
        let roads = vec![RoadDoc {
            kind: "road".into(),
            id: "grid".into(),
            network: "assets/grid.xodr".into(),
        }];
        let good = scenario(
            "kind: scenario\nid: stop\nroad: grid\nstoryboard: assets/stop.xosc\nsimulator: esmini\n\
             clock: { tick_ms: 10 }\nseed: 42\nactors: { Ego: { vehicle: car } }\n\
             parameters: { EgoTargetSpeed: { unit: m/s, min: 0, max: 22.2 } }\n",
        );
        let vehicles = vec!["car".to_string()];
        let inputs = |s: &[ScenarioDoc], r: Option<&'static str>| -> Vec<Problem> {
            check(&Inputs {
                root: dir.path(),
                experiment: "e",
                scenario_ref: r,
                roads: &roads,
                scenarios: s,
                vehicles: &vehicles,
                required_simulators: None,
                parameters: &BTreeMap::new(),
            })
        };
        assert_eq!(inputs(std::slice::from_ref(&good), Some("stop")), vec![]);

        let fields = |p: Vec<Problem>| p.into_iter().map(|p| p.field).collect::<Vec<_>>();
        assert_eq!(
            fields(inputs(std::slice::from_ref(&good), Some("nope"))),
            ["scenario"]
        );
        let mut bad = good.clone();
        bad.road = "highway".into();
        bad.simulator = "unity".into();
        bad.actors = BTreeMap::from([(
            "Truck".to_string(),
            Actor {
                vehicle: "bus".into(),
            },
        )]);
        bad.parameters
            .insert("Rain".into(), ScenarioParameter::default());
        bad.parameters.insert(
            "LeadName".into(),
            ScenarioParameter {
                min: Some(1.0),
                ..Default::default()
            },
        );
        assert_eq!(
            fields(inputs(&[bad], Some("stop"))),
            [
                "scenarios[stop].road",
                "scenarios[stop].simulator",
                "scenarios[stop].actors.Truck.vehicle",
                "scenarios[stop].actors.Truck",
                "scenarios[stop].parameters.LeadName",
                "scenarios[stop].parameters.Rain",
            ]
        );
        let mut missing = good.clone();
        missing.storyboard = "assets/none.xosc".into();
        let p = inputs(&[missing], None);
        assert_eq!(fields(p.clone()), ["scenarios[stop].storyboard"]);
        assert_eq!(p[0].kind, ProblemKind::Scenario);
        assert_eq!(
            fields(inputs(&[good.clone(), good.clone()], None)),
            ["scenario"]
        );
        // A vxp/1 package names the scenario's simulator in `requires.simulator`.
        let carla_only = vec!["carla".to_string()];
        let p = check(&Inputs {
            root: dir.path(),
            experiment: "e",
            scenario_ref: Some("stop"),
            roads: &roads,
            scenarios: std::slice::from_ref(&good),
            vehicles: &vehicles,
            required_simulators: Some(&carla_only),
            parameters: &BTreeMap::new(),
        });
        assert_eq!(fields(p.clone()), ["scenarios[stop].simulator"]);
        assert!(p[0].detail.contains("requires.simulator"), "{p:?}");
    }

    /// An experiment parameter may bind a storyboard parameter the scenario declares, within its
    /// bounds and in its unit.
    #[test]
    fn a_bind_into_the_scenario_is_checked_against_its_declaration() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("assets")).unwrap();
        std::fs::write(dir.path().join("assets/stop.xosc"), XOSC).unwrap();
        std::fs::write(dir.path().join("assets/grid.xodr"), XODR).unwrap();
        let roads = vec![RoadDoc {
            kind: "road".into(),
            id: "grid".into(),
            network: "assets/grid.xodr".into(),
        }];
        let sc = scenario(
            "kind: scenario\nid: stop\nroad: grid\nstoryboard: assets/stop.xosc\nsimulator: esmini\n\
             parameters: { EgoTargetSpeed: { unit: m/s, min: 0, max: 22.2 } }\n",
        );
        let param = |yaml: &str| -> BTreeMap<String, crate::intent::Parameter> {
            BTreeMap::from([("speed".to_string(), serde_yaml_ng::from_str(yaml).unwrap())])
        };
        let run = |ps: &BTreeMap<String, crate::intent::Parameter>, r: Option<&str>| {
            check(&Inputs {
                root: dir.path(),
                experiment: "e",
                scenario_ref: r,
                roads: &roads,
                scenarios: std::slice::from_ref(&sc),
                vehicles: &[],
                required_simulators: None,
                parameters: ps,
            })
            .into_iter()
            .map(|p| p.detail)
            .collect::<Vec<_>>()
        };
        let ok = param(
            "{ type: number, unit: m/s, default: 10, min: 5, max: 20, binds: scenario.parameters.EgoTargetSpeed }",
        );
        assert_eq!(run(&ok, Some("stop")), Vec::<String>::new());
        assert!(run(&ok, None)[0].contains("names no scenario"));
        let wide = param(
            "{ type: number, unit: m/s, default: 10, min: 5, max: 40, binds: scenario.parameters.EgoTargetSpeed }",
        );
        assert!(run(&wide, Some("stop"))[0].contains("not inside"));
        let kmh = param(
            "{ type: number, unit: km/h, default: 10, min: 5, max: 20, binds: scenario.parameters.EgoTargetSpeed }",
        );
        assert!(run(&kmh, Some("stop"))[0].contains("is in `km/h`"));
        let unknown =
            param("{ type: number, unit: m/s, default: 10, binds: scenario.parameters.Rain }");
        assert!(run(&unknown, Some("stop"))[0].contains("`Rain` is not one of"));
    }
}
