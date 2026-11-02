use std::net::IpAddr;
use std::path::PathBuf;

use serde::Deserialize;

use super::{
    AbsolutePath, CredentialGrantId, CredentialPolicy, DnsName, FileSystemPolicy, HostHttpClass,
    HostHttpDestination, HostHttpHost, HostHttpPolicy, HostHttpScheme, HostPlatform, IpScope,
    NetworkPolicy, RelativePath, ResourceLimits, SandboxPolicy, WritableRoot,
};
use crate::error::PolicyValidationError;

pub(crate) type AbsolutePathWire = PathBuf;
pub(crate) type RelativePathWire = PathBuf;
pub(crate) type CredentialGrantIdWire = String;
pub(crate) type DnsNameWire = String;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum HostPlatformWire {
    Linux,
    Macos,
    Windows,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) enum NetworkPolicyWire {
    None,
    ProxyRouted,
    FullAccess,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum HostHttpClassWire {
    ProviderApi,
    ImageGeneration,
    WebSearch,
    GithubApi,
    MemoryApi,
    PluginMarketplace,
    PluginArchive,
    NpmRegistry,
    NpmTarball,
    AuthOauth,
    LiveStt,
    LlamaRouter,
    LlamaHuggingFace,
    LlamaGguf,
    RealtimeApi,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum HostHttpSchemeWire {
    Https,
    Http,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum IpScopeWire {
    Global,
    Private,
    Loopback,
    LinkLocal,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PolicySnapshotWire {
    pub(crate) revision: u64,
    pub(crate) created_on: HostPlatformWire,
    pub(crate) policy: SandboxPolicyWire,
    pub(crate) digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WritableRootPolicyWire {
    pub(crate) root: AbsolutePathWire,
    pub(crate) protected_descendants: Vec<RelativePathWire>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileSystemPolicyWire {
    pub(crate) read_only_roots: Vec<AbsolutePathWire>,
    pub(crate) writable_roots: Vec<WritableRootPolicyWire>,
    pub(crate) sealed_scratch_root: Option<AbsolutePathWire>,
    pub(crate) denied_paths: Vec<AbsolutePathWire>,
    #[serde(default, rename = "privateRoots")]
    pub(crate) private_roots: Vec<AbsolutePathWire>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) enum HostHttpHostPolicyWire {
    Dns { name: DnsNameWire },
    Ip { address: IpAddr },
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HostHttpDestinationPolicyWire {
    pub(crate) class: HostHttpClassWire,
    pub(crate) scheme: HostHttpSchemeWire,
    pub(crate) host: HostHttpHostPolicyWire,
    pub(crate) port: u16,
    pub(crate) allowed_ip_scopes: Vec<IpScopeWire>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HostHttpPolicyWire {
    pub(crate) destinations: Vec<HostHttpDestinationPolicyWire>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CredentialPolicyWire {
    pub(crate) allowed_grants: Vec<CredentialGrantIdWire>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResourceLimitsWire {
    pub(crate) address_space_bytes: u64,
    pub(crate) cpu_seconds: u64,
    pub(crate) file_size_bytes: u64,
    pub(crate) open_files: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) enum SandboxPolicyWire {
    ReadOnly {
        filesystem: FileSystemPolicyWire,
        network: NetworkPolicyWire,
        #[serde(rename = "hostHttp")]
        host_http: HostHttpPolicyWire,
        credentials: CredentialPolicyWire,
        limits: ResourceLimitsWire,
    },
    WorkspaceWrite {
        filesystem: FileSystemPolicyWire,
        network: NetworkPolicyWire,
        #[serde(rename = "hostHttp")]
        host_http: HostHttpPolicyWire,
        credentials: CredentialPolicyWire,
        limits: ResourceLimitsWire,
    },
    DangerFullAccess {
        credentials: CredentialPolicyWire,
    },
}

impl From<HostPlatformWire> for HostPlatform {
    fn from(wire: HostPlatformWire) -> Self {
        match wire {
            HostPlatformWire::Linux => Self::Linux,
            HostPlatformWire::Macos => Self::Macos,
            HostPlatformWire::Windows => Self::Windows,
        }
    }
}

impl From<NetworkPolicyWire> for NetworkPolicy {
    fn from(wire: NetworkPolicyWire) -> Self {
        match wire {
            NetworkPolicyWire::None => Self::None,
            NetworkPolicyWire::ProxyRouted => Self::ProxyRouted,
            NetworkPolicyWire::FullAccess => Self::FullAccess,
        }
    }
}

impl From<HostHttpClassWire> for HostHttpClass {
    fn from(wire: HostHttpClassWire) -> Self {
        match wire {
            HostHttpClassWire::ProviderApi => Self::ProviderApi,
            HostHttpClassWire::ImageGeneration => Self::ImageGeneration,
            HostHttpClassWire::WebSearch => Self::WebSearch,
            HostHttpClassWire::GithubApi => Self::GithubApi,
            HostHttpClassWire::MemoryApi => Self::MemoryApi,
            HostHttpClassWire::PluginMarketplace => Self::PluginMarketplace,
            HostHttpClassWire::PluginArchive => Self::PluginArchive,
            HostHttpClassWire::NpmRegistry => Self::NpmRegistry,
            HostHttpClassWire::NpmTarball => Self::NpmTarball,
            HostHttpClassWire::AuthOauth => Self::AuthOauth,
            HostHttpClassWire::LiveStt => Self::LiveStt,
            HostHttpClassWire::LlamaRouter => Self::LlamaRouter,
            HostHttpClassWire::LlamaHuggingFace => Self::LlamaHuggingFace,
            HostHttpClassWire::LlamaGguf => Self::LlamaGguf,
            HostHttpClassWire::RealtimeApi => Self::RealtimeApi,
        }
    }
}

impl From<HostHttpSchemeWire> for HostHttpScheme {
    fn from(wire: HostHttpSchemeWire) -> Self {
        match wire {
            HostHttpSchemeWire::Https => Self::Https,
            HostHttpSchemeWire::Http => Self::Http,
        }
    }
}

impl From<IpScopeWire> for IpScope {
    fn from(wire: IpScopeWire) -> Self {
        match wire {
            IpScopeWire::Global => Self::Global,
            IpScopeWire::Private => Self::Private,
            IpScopeWire::Loopback => Self::Loopback,
            IpScopeWire::LinkLocal => Self::LinkLocal,
        }
    }
}

impl TryFrom<WritableRootPolicyWire> for WritableRoot {
    type Error = PolicyValidationError;

    fn try_from(wire: WritableRootPolicyWire) -> Result<Self, Self::Error> {
        let protected = wire
            .protected_descendants
            .into_iter()
            .map(RelativePath::parse)
            .collect::<Result<Vec<_>, _>>()?;
        Self::new(AbsolutePath::parse(wire.root)?, protected)
    }
}

impl TryFrom<FileSystemPolicyWire> for FileSystemPolicy {
    type Error = PolicyValidationError;

    fn try_from(wire: FileSystemPolicyWire) -> Result<Self, Self::Error> {
        let read_only = wire
            .read_only_roots
            .into_iter()
            .map(AbsolutePath::parse)
            .collect::<Result<Vec<_>, _>>()?;
        let writable = wire
            .writable_roots
            .into_iter()
            .map(WritableRoot::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let sealed_scratch = wire
            .sealed_scratch_root
            .map(AbsolutePath::parse)
            .transpose()?;
        let denied = wire
            .denied_paths
            .into_iter()
            .map(AbsolutePath::parse)
            .collect::<Result<Vec<_>, _>>()?;
        let private_roots = wire
            .private_roots
            .into_iter()
            .map(AbsolutePath::parse)
            .collect::<Result<Vec<_>, _>>()?;
        Self::with_private_roots(read_only, writable, denied, sealed_scratch, private_roots)
    }
}

impl TryFrom<HostHttpHostPolicyWire> for HostHttpHost {
    type Error = PolicyValidationError;

    fn try_from(wire: HostHttpHostPolicyWire) -> Result<Self, Self::Error> {
        match wire {
            HostHttpHostPolicyWire::Dns { name } => Ok(Self::Dns {
                name: DnsName::parse(name)?,
            }),
            HostHttpHostPolicyWire::Ip { address } => Ok(Self::Ip { address }),
        }
    }
}

impl TryFrom<HostHttpDestinationPolicyWire> for HostHttpDestination {
    type Error = PolicyValidationError;

    fn try_from(wire: HostHttpDestinationPolicyWire) -> Result<Self, Self::Error> {
        let scopes = wire
            .allowed_ip_scopes
            .into_iter()
            .map(IpScope::from)
            .collect();
        Self::new(
            wire.class.into(),
            wire.scheme.into(),
            HostHttpHost::try_from(wire.host)?,
            wire.port,
            scopes,
        )
    }
}

impl TryFrom<HostHttpPolicyWire> for HostHttpPolicy {
    type Error = PolicyValidationError;

    fn try_from(wire: HostHttpPolicyWire) -> Result<Self, Self::Error> {
        Self::new(
            wire.destinations
                .into_iter()
                .map(HostHttpDestination::try_from)
                .collect::<Result<Vec<_>, _>>()?,
        )
    }
}

impl TryFrom<CredentialPolicyWire> for CredentialPolicy {
    type Error = PolicyValidationError;

    fn try_from(wire: CredentialPolicyWire) -> Result<Self, Self::Error> {
        Self::new(
            wire.allowed_grants
                .into_iter()
                .map(CredentialGrantId::parse)
                .collect::<Result<Vec<_>, _>>()?,
        )
    }
}

impl TryFrom<ResourceLimitsWire> for ResourceLimits {
    type Error = PolicyValidationError;

    fn try_from(wire: ResourceLimitsWire) -> Result<Self, Self::Error> {
        Self::new(
            wire.address_space_bytes,
            wire.cpu_seconds,
            wire.file_size_bytes,
            wire.open_files,
        )
    }
}

impl TryFrom<SandboxPolicyWire> for SandboxPolicy {
    type Error = PolicyValidationError;

    fn try_from(wire: SandboxPolicyWire) -> Result<Self, Self::Error> {
        match wire {
            SandboxPolicyWire::ReadOnly {
                filesystem,
                network,
                host_http,
                credentials,
                limits,
            } => {
                let filesystem = FileSystemPolicy::try_from(filesystem)?;
                if filesystem.sealed_scratch_root().is_some() {
                    Self::resolved_read_only(
                        filesystem,
                        network.into(),
                        HostHttpPolicy::try_from(host_http)?,
                        CredentialPolicy::try_from(credentials)?,
                        ResourceLimits::try_from(limits)?,
                    )
                } else {
                    Self::read_only(
                        filesystem,
                        network.into(),
                        HostHttpPolicy::try_from(host_http)?,
                        CredentialPolicy::try_from(credentials)?,
                        ResourceLimits::try_from(limits)?,
                    )
                }
            }
            SandboxPolicyWire::WorkspaceWrite {
                filesystem,
                network,
                host_http,
                credentials,
                limits,
            } => Self::workspace_write(
                FileSystemPolicy::try_from(filesystem)?,
                network.into(),
                HostHttpPolicy::try_from(host_http)?,
                CredentialPolicy::try_from(credentials)?,
                ResourceLimits::try_from(limits)?,
            ),
            SandboxPolicyWire::DangerFullAccess { credentials } => {
                Self::danger_full_access(CredentialPolicy::try_from(credentials)?)
            }
        }
    }
}
