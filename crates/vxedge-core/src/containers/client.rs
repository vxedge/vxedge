//! [`ContainerClient`]: the `bollard` edge. Each method observes the group, asks
//! [`super::spec`] what to do, and does exactly that. Every daemon error is wrapped into
//! [`ContainerError`] with the operation and container named, so no caller matches on bollard.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use bollard::Docker;
use bollard::models::{ContainerCreateBody, ContainerSummary, DeviceMapping, HostConfig};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, ListContainersOptionsBuilder,
    LogsOptionsBuilder, RemoveContainerOptionsBuilder,
};
use futures_util::TryStreamExt;

use super::ContainerError;
use super::spec::{
    ContainerSpec, GroupSpec, GroupStatus, INFRA_MEMBER, InfraAction, LABEL_GROUP, LABEL_MEMBER,
    LABEL_SPEC, MemberAction, ObservedContainer, infra_action, member_action, member_name,
    network_mode,
};

/// Per-request timeout. Generous because an image pull is one request, and `mongo:7` on a
/// cold store takes longer than bollard's 120 s default.
const API_TIMEOUT_SECS: u64 = 300;

/// What `up` did to one container — returned so a job message can say "already up
/// (idempotent)" from what was observed, not from what was assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Kept,
    Started,
    Created,
    Recreated,
}

/// A running group's network-namespace anchor: the infra container's id (what members join)
/// and PID (what `nsenter -t` and `ip link set … netns <pid>` target, since these namespaces
/// are not in `ip netns`'s named registry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupHandle {
    pub group: String,
    pub infra_id: String,
    pub infra_pid: u32,
}

#[derive(Debug, Clone)]
pub struct ContainerClient {
    docker: Docker,
    socket: PathBuf,
}

impl ContainerClient {
    /// Connect to the API socket at `socket` and confirm a daemon answers on it — a client that
    /// constructs fine against a dead socket would defer the failure to the middle of an `up`.
    pub async fn connect(socket: &Path) -> Result<Self, ContainerError> {
        let unreachable = |detail: String| ContainerError::SocketUnreachable {
            path: socket.display().to_string(),
            detail,
        };
        let path = socket
            .to_str()
            .ok_or_else(|| unreachable("path is not valid UTF-8".into()))?;
        let docker =
            Docker::connect_with_unix(path, API_TIMEOUT_SECS, bollard::API_DEFAULT_VERSION)
                .map_err(|e| unreachable(e.to_string()))?;
        // Doubles as the liveness check: settles on the lower of bollard's and the daemon's API
        // versions, so a Podman whose compat API trails Docker's still gets requests it accepts.
        let docker = docker
            .negotiate_version()
            .await
            .map_err(|e| unreachable(e.to_string()))?;
        Ok(Self {
            docker,
            socket: socket.to_path_buf(),
        })
    }

    /// [`Self::connect`] to this process's own rootless socket ([`super::user_socket_path`]).
    pub async fn connect_user() -> Result<Self, ContainerError> {
        let euid = nix::unistd::geteuid().as_raw();
        let socket = super::user_socket_path(euid, std::env::var_os("XDG_RUNTIME_DIR").as_deref())?;
        Self::connect(&socket).await
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Every container labelled with `group`, in any state. Never mutates.
    pub async fn group_status(&self, group: &str) -> Result<GroupStatus, ContainerError> {
        let filters = HashMap::from([("label", vec![format!("{LABEL_GROUP}={group}")])]);
        let opts = ListContainersOptionsBuilder::new()
            .all(true)
            .filters(&filters)
            .build();
        let list = self
            .docker
            .list_containers(Some(opts))
            .await
            .map_err(api(format!("list containers of group `{group}`")))?;
        Ok(GroupStatus::from_observed(
            group,
            list.into_iter().map(observed_from_summary).collect(),
        ))
    }

    /// Converge the group's infra container: keep a running one created from this spec,
    /// otherwise remove whatever of the group exists and create it afresh. Idempotent.
    pub async fn ensure_group(
        &self,
        group: &GroupSpec,
    ) -> Result<(GroupHandle, Outcome), ContainerError> {
        let status = self.group_status(group.name()).await?;
        let outcome = match infra_action(group, status.infra.as_ref()) {
            InfraAction::Keep => Outcome::Kept,
            InfraAction::Rebuild => {
                let existed = status.exists();
                self.remove_observed(&status).await?;
                self.create_and_start(group, group.infra(), &group.infra_container_name(), None)
                    .await?;
                if existed {
                    Outcome::Recreated
                } else {
                    Outcome::Created
                }
            }
        };
        Ok((self.handle(group.name()).await?, outcome))
    }

    /// The handle of a group that is already up. Read-only.
    pub async fn handle(&self, group: &str) -> Result<GroupHandle, ContainerError> {
        let name = member_name(group, INFRA_MEMBER);
        let inspected = match self.docker.inspect_container(&name, None).await {
            Ok(i) => i,
            Err(e) if status_code(&e) == Some(404) => {
                return Err(ContainerError::GroupNotUp {
                    group: group.to_string(),
                });
            }
            Err(e) => return Err(api(format!("inspect `{name}`"))(e)),
        };
        let pid = inspected
            .state
            .and_then(|s| s.pid)
            .and_then(|p| u32::try_from(p).ok())
            .filter(|p| *p != 0)
            .ok_or_else(|| ContainerError::NoInfraPid {
                group: group.to_string(),
            })?;
        Ok(GroupHandle {
            group: group.to_string(),
            infra_id: inspected.id.unwrap_or_default(),
            infra_pid: pid,
        })
    }

    /// Converge one long-running member into the group's namespace. Idempotent: a running
    /// member from the same spec and infra container is left alone.
    pub async fn ensure_member(
        &self,
        group: &GroupSpec,
        handle: &GroupHandle,
        spec: &ContainerSpec,
    ) -> Result<Outcome, ContainerError> {
        let name = group.member_container_name(spec)?;
        let status = self.group_status(group.name()).await?;
        let observed = status.member(&name);
        match member_action(spec, &handle.infra_id, observed) {
            MemberAction::Keep => Ok(Outcome::Kept),
            MemberAction::Start => {
                self.docker
                    .start_container(&name, None)
                    .await
                    .map_err(api(format!("start `{name}`")))?;
                Ok(Outcome::Started)
            }
            MemberAction::Create => {
                self.create_and_start(group, spec, &name, Some(&handle.infra_id))
                    .await?;
                Ok(Outcome::Created)
            }
            MemberAction::Recreate => {
                self.remove(&name).await?;
                self.create_and_start(group, spec, &name, Some(&handle.infra_id))
                    .await?;
                Ok(Outcome::Recreated)
            }
        }
    }

    /// Run `spec` to completion inside the group's namespace and return its combined output —
    /// `podman run --rm --pod <pod>`'s job (subscriber provisioning). A non-zero exit is an
    /// error carrying that output. The container is removed either way, including a leftover
    /// from a run that crashed before its own cleanup.
    pub async fn run_to_completion(
        &self,
        group: &GroupSpec,
        handle: &GroupHandle,
        spec: &ContainerSpec,
    ) -> Result<String, ContainerError> {
        let name = group.member_container_name(spec)?;
        self.remove(&name).await?;
        let id = self
            .create_and_start(group, spec, &name, Some(&handle.infra_id))
            .await?;
        let code = match self
            .docker
            .wait_container(&id, None)
            .try_collect::<Vec<_>>()
            .await
        {
            Ok(responses) => responses.last().map_or(0, |r| r.status_code),
            Err(bollard::errors::Error::DockerContainerWaitError { code, .. }) => code,
            Err(e) => {
                let _ = self.remove(&id).await;
                return Err(api(format!("wait for `{name}`"))(e));
            }
        };
        let output = self.output(&id).await;
        self.remove(&id).await?;
        let output = output?;
        if code != 0 {
            return Err(ContainerError::ExitedNonZero {
                container: name,
                code,
                output,
            });
        }
        Ok(output)
    }

    /// Remove every container of the group, members before infra. Idempotent: an absent group
    /// is `Ok(0)`. Returns how many containers were removed.
    pub async fn group_down(&self, group: &str) -> Result<usize, ContainerError> {
        let status = self.group_status(group).await?;
        self.remove_observed(&status).await
    }

    /// Which of `names` are taken by a container carrying no group label: one this module would
    /// create under that name but did not create itself, most likely left by an older vxedged
    /// that drove the `podman` CLI. Label-filtered calls cannot see such a container, so without
    /// this check `group_down` reports success over it and `up` stops halfway on
    /// [`ContainerError::NameTaken`]. Never mutates.
    pub async fn unlabelled(&self, names: &[String]) -> Result<Vec<String>, ContainerError> {
        let opts = ListContainersOptionsBuilder::new().all(true).build();
        let list = self
            .docker
            .list_containers(Some(opts))
            .await
            .map_err(api("list containers".to_string()))?;
        Ok(unlabelled_names(names, &list))
    }

    async fn remove_observed(&self, status: &GroupStatus) -> Result<usize, ContainerError> {
        let mut removed = 0;
        for c in status.members.iter().chain(status.infra.iter()) {
            self.remove(&c.id).await?;
            removed += 1;
        }
        Ok(removed)
    }

    /// Force-remove one container by name or id; already gone is success.
    async fn remove(&self, container: &str) -> Result<(), ContainerError> {
        let opts = RemoveContainerOptionsBuilder::new().force(true).build();
        match self.docker.remove_container(container, Some(opts)).await {
            Ok(()) => Ok(()),
            Err(e) if status_code(&e) == Some(404) => Ok(()),
            Err(e) => Err(api(format!("remove `{container}`"))(e)),
        }
    }

    async fn create_and_start(
        &self,
        group: &GroupSpec,
        spec: &ContainerSpec,
        name: &str,
        netns_owner: Option<&str>,
    ) -> Result<String, ContainerError> {
        let body = create_body(
            spec,
            group.labels(spec, netns_owner),
            Some(network_mode(netns_owner)),
        );
        let id = match self.create(name, body.clone()).await {
            Err(e) if status_code(&e) == Some(404) => {
                // `podman run` pulls a missing image; keep that behaviour, once.
                self.pull(&spec.image).await?;
                self.create(name, body).await
            }
            other => other,
        }
        .map_err(|e| match status_code(&e) {
            Some(409) => ContainerError::NameTaken {
                name: name.to_string(),
            },
            Some(404) => ContainerError::ImageUnavailable {
                image: spec.image.clone(),
                detail: e.to_string(),
            },
            _ => api(format!("create `{name}`"))(e),
        })?;
        self.docker
            .start_container(&id, None)
            .await
            .map_err(api(format!("start `{name}`")))?;
        Ok(id)
    }

    async fn create(
        &self,
        name: &str,
        body: ContainerCreateBody,
    ) -> Result<String, bollard::errors::Error> {
        let opts = CreateContainerOptionsBuilder::new().name(name).build();
        Ok(self.docker.create_container(Some(opts), body).await?.id)
    }

    async fn pull(&self, image: &str) -> Result<(), ContainerError> {
        let opts = CreateImageOptionsBuilder::new().from_image(image).build();
        self.docker
            .create_image(Some(opts), None, None)
            .try_collect::<Vec<_>>()
            .await
            .map(|_| ())
            .map_err(|e| ContainerError::ImageUnavailable {
                image: image.to_string(),
                detail: e.to_string(),
            })
    }

    async fn output(&self, container: &str) -> Result<String, ContainerError> {
        let opts = LogsOptionsBuilder::new().stdout(true).stderr(true).build();
        let chunks: Vec<_> = self
            .docker
            .logs(container, Some(opts))
            .try_collect()
            .await
            .map_err(api(format!("read output of `{container}`")))?;
        Ok(chunks.iter().map(ToString::to_string).collect())
    }
}

/// One [`ContainerClient`] per process, connected on first use. A daemon that holds this starts
/// fine on a host without the socket (most verbs never touch containers), and a failed connect
/// is not cached, so enabling the socket later works without a restart. Clones share the client.
#[derive(Debug, Clone, Default)]
pub struct LazyClient {
    /// `None`: this process's own rootless socket ([`ContainerClient::connect_user`]).
    socket: Option<PathBuf>,
    cell: std::sync::Arc<tokio::sync::OnceCell<ContainerClient>>,
}

impl LazyClient {
    /// Connects to this process's own rootless socket on first use.
    pub fn user() -> Self {
        Self::default()
    }

    /// Connects to `socket` on first use.
    pub fn at(socket: PathBuf) -> Self {
        Self {
            socket: Some(socket),
            ..Self::default()
        }
    }

    pub async fn get(&self) -> Result<&ContainerClient, ContainerError> {
        self.cell
            .get_or_try_init(|| async {
                match &self.socket {
                    Some(p) => ContainerClient::connect(p).await,
                    None => ContainerClient::connect_user().await,
                }
            })
            .await
    }
}

fn api(op: String) -> impl FnOnce(bollard::errors::Error) -> ContainerError {
    move |source| ContainerError::Api { op, source }
}

fn status_code(e: &bollard::errors::Error) -> Option<u16> {
    match e {
        bollard::errors::Error::DockerResponseServerError { status_code, .. } => Some(*status_code),
        _ => None,
    }
}

/// The create request for `spec`. Pure, so the translation from typed spec to API body is
/// tested directly — it is the part that replaces `podman run`'s flag text.
pub(crate) fn create_body(
    spec: &ContainerSpec,
    labels: BTreeMap<String, String>,
    network_mode: Option<String>,
) -> ContainerCreateBody {
    let non_empty = |v: Vec<String>| (!v.is_empty()).then_some(v);
    ContainerCreateBody {
        image: Some(spec.image.clone()),
        cmd: non_empty(spec.cmd.clone()),
        env: non_empty(spec.env.iter().map(|(k, v)| format!("{k}={v}")).collect()),
        labels: Some(labels.into_iter().collect()),
        host_config: Some(HostConfig {
            network_mode,
            cap_add: non_empty(spec.cap_add.clone()),
            binds: non_empty(spec.binds.iter().map(|b| b.to_bind_string()).collect()),
            devices: (!spec.devices.is_empty()).then(|| {
                spec.devices
                    .iter()
                    .map(|d| DeviceMapping {
                        path_on_host: Some(d.clone()),
                        path_in_container: Some(d.clone()),
                        cgroup_permissions: Some("rwm".to_string()),
                    })
                    .collect()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The pure half of [`ContainerClient::unlabelled`], in the order `names` lists them.
fn unlabelled_names(names: &[String], list: &[ContainerSummary]) -> Vec<String> {
    names
        .iter()
        .filter(|wanted| {
            list.iter().any(|s| {
                let named = s
                    .names
                    .iter()
                    .flatten()
                    .any(|n| n.trim_start_matches('/') == wanted.as_str());
                let labelled = s
                    .labels
                    .as_ref()
                    .is_some_and(|l| l.contains_key(LABEL_GROUP));
                named && !labelled
            })
        })
        .cloned()
        .collect()
}

fn observed_from_summary(s: ContainerSummary) -> ObservedContainer {
    let labels = s.labels.unwrap_or_default();
    ObservedContainer {
        id: s.id.unwrap_or_default(),
        // The Docker API reports names with a leading `/`; Podman's compat layer follows it.
        name: s
            .names
            .and_then(|n| n.into_iter().next())
            .map(|n| n.trim_start_matches('/').to_string())
            .unwrap_or_default(),
        member: labels.get(LABEL_MEMBER).cloned(),
        spec_hash: labels.get(LABEL_SPEC).cloned(),
        state: s.state.map(|st| st.to_string()).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::containers::spec::Bind;

    fn upf() -> ContainerSpec {
        ContainerSpec {
            name: "upf-adas".into(),
            image: "vx5g-core".into(),
            cmd: vec![
                "open5gs-upfd".into(),
                "-c".into(),
                "/etc/vx5g/open5gs/upf-adas.yaml".into(),
            ],
            binds: vec![Bind {
                host: "/run/vx/fiveg-config".into(),
                container: "/etc/vx5g".into(),
                read_only: true,
                relabel: true,
            }],
            cap_add: vec!["NET_ADMIN".into()],
            devices: vec!["/dev/net/tun".into()],
            ..Default::default()
        }
    }

    /// The same container `fiveg::lifecycle` asks for today with
    /// `podman run -d --pod vx5g -v …:ro,Z --cap-add=NET_ADMIN --device /dev/net/tun`.
    #[test]
    fn create_body_carries_every_typed_field_the_cli_flags_did() {
        let body = create_body(
            &upf(),
            BTreeMap::from([("k".into(), "v".into())]),
            Some("container:abc".into()),
        );
        assert_eq!(body.image.as_deref(), Some("vx5g-core"));
        assert_eq!(body.cmd.as_ref().unwrap()[0], "open5gs-upfd");
        assert_eq!(body.env, None, "no env declared, none sent");
        assert_eq!(body.labels.unwrap()["k"], "v");
        let hc = body.host_config.unwrap();
        assert_eq!(hc.network_mode.as_deref(), Some("container:abc"));
        assert_eq!(hc.cap_add.unwrap(), ["NET_ADMIN"]);
        assert_eq!(hc.binds.unwrap(), ["/run/vx/fiveg-config:/etc/vx5g:ro,Z"]);
        let dev = &hc.devices.unwrap()[0];
        assert_eq!(dev.path_on_host.as_deref(), Some("/dev/net/tun"));
        assert_eq!(dev.path_in_container.as_deref(), Some("/dev/net/tun"));
    }

    #[test]
    fn create_body_for_infra_leaves_networking_to_the_daemon_default() {
        let body = create_body(
            &ContainerSpec {
                image: "pause".into(),
                env: BTreeMap::from([("B".into(), "2".into()), ("A".into(), "1".into())]),
                ..Default::default()
            },
            BTreeMap::new(),
            None,
        );
        assert_eq!(body.cmd, None, "empty cmd means the image default");
        assert_eq!(
            body.env.unwrap(),
            ["A=1", "B=2"],
            "sorted, so the hash is stable"
        );
        let hc = body.host_config.unwrap();
        assert_eq!(hc.network_mode, None);
        assert_eq!(hc.cap_add, None);
        assert_eq!(hc.devices, None);
    }

    #[test]
    fn summaries_map_names_labels_and_state() {
        let s = ContainerSummary {
            id: Some("abc".into()),
            names: Some(vec!["/vx5g-nrf".into()]),
            labels: Some(HashMap::from([
                (LABEL_GROUP.to_string(), "vx5g".to_string()),
                (LABEL_MEMBER.to_string(), "nrf".to_string()),
                (LABEL_SPEC.to_string(), "h".to_string()),
            ])),
            state: Some(bollard::models::ContainerSummaryStateEnum::RUNNING),
            ..Default::default()
        };
        let o = observed_from_summary(s);
        assert_eq!(o.name, "vx5g-nrf");
        assert_eq!(o.member.as_deref(), Some("nrf"));
        assert_eq!(o.spec_hash.as_deref(), Some("h"));
        assert!(o.is_running());
    }

    #[test]
    fn only_unlabelled_holders_of_the_exact_names_are_reported() {
        let summary = |name: &str, group: Option<&str>| ContainerSummary {
            names: Some(vec![format!("/{name}")]),
            labels: group.map(|g| HashMap::from([(LABEL_GROUP.to_string(), g.to_string())])),
            ..Default::default()
        };
        let list = [
            summary("vx5g-nrf", None), // left by the podman CLI
            summary("vx5g-ran-gnb", Some("vx5g-ran")),
            summary("vx5g-nrf-old", None), // a longer name is not a match
        ];
        let names = ["vx5g-ran-gnb", "vx5g-nrf", "vx5g-mongo"].map(String::from);
        assert_eq!(unlabelled_names(&names, &list), ["vx5g-nrf"]);
    }

    #[tokio::test]
    async fn a_missing_socket_is_reported_with_the_fix() {
        let err = ContainerClient::connect(Path::new("/nonexistent/vxedge-test/podman.sock"))
            .await
            .unwrap_err();
        assert!(matches!(err, ContainerError::SocketUnreachable { .. }));
        assert!(
            err.to_string()
                .contains("systemctl --user enable --now podman.socket")
        );
    }

    #[tokio::test]
    async fn a_lazy_client_does_not_cache_a_failed_connect() {
        let lazy = LazyClient::at("/nonexistent/vxedge-test/podman.sock".into());
        assert!(matches!(
            lazy.get().await,
            Err(ContainerError::SocketUnreachable { .. })
        ));
        // Tried again, not answered from a cached failure: a socket enabled after the daemon
        // started must be picked up without a restart.
        assert!(lazy.get().await.is_err());
        assert!(lazy.cell.get().is_none());
    }

    /// A socket that exists but has no Podman behind it must fail at connect, not mid-`up`.
    #[tokio::test]
    async fn a_socket_with_no_daemon_behind_it_fails_at_connect() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("podman.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        // Accept and immediately drop every connection.
        let server = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        let err = ContainerClient::connect(&path).await.unwrap_err();
        server.abort();
        assert!(
            matches!(err, ContainerError::SocketUnreachable { .. }),
            "{err}"
        );
    }
}
