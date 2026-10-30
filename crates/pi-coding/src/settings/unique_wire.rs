use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use pi_sandbox::{
    DnsName, EnvironmentVariableName, HostHttpDestination, HostHttpDestinationKey, HostHttpHost,
};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use super::{
    CredentialAudienceWire, CredentialGrantDefinitionSettings, CredentialGrantOutputConfigWire,
    HostHttpDestinationWire, HostHttpHostWire,
};

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct McpEnvironmentWire(BTreeMap<String, String>);

impl McpEnvironmentWire {
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&String> {
        self.0.get(key)
    }

    pub fn insert(&mut self, key: String, value: String) -> Option<String> {
        self.0.insert(key, value)
    }

    #[must_use]
    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.0.iter()
    }
}

impl FromIterator<(String, String)> for McpEnvironmentWire {
    fn from_iter<T: IntoIterator<Item = (String, String)>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl<'a> IntoIterator for &'a McpEnvironmentWire {
    type Item = (&'a String, &'a String);
    type IntoIter = std::collections::btree_map::Iter<'a, String, String>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}
impl<'de> Deserialize<'de> for McpEnvironmentWire {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(McpEnvironmentVisitor)
    }
}

pub struct McpEnvironmentVisitor;

impl<'de> Visitor<'de> for McpEnvironmentVisitor {
    type Value = McpEnvironmentWire;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an MCP environment object with unique normalized keys")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut environment = BTreeMap::new();
        let mut normalized = BTreeSet::new();
        while let Some((key, value)) = map.next_entry::<String, String>()? {
            let parsed = EnvironmentVariableName::parse(key.clone()).map_err(de::Error::custom)?;
            if !normalized.insert(parsed.into_string()) {
                return Err(de::Error::custom(format!(
                    "duplicate normalized MCP environment key {key:?}"
                )));
            }
            environment.insert(key, value);
        }
        Ok(McpEnvironmentWire(environment))
    }
}

pub(crate) fn deserialize_credential_grants<'de, D>(
    deserializer: D,
) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_seq(McpCredentialGrantVisitor::<String>::references())
}

pub(crate) fn deserialize_credential_grant_definitions<'de, D>(
    deserializer: D,
) -> Result<Vec<CredentialGrantDefinitionSettings>, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_seq(
        McpCredentialGrantVisitor::<CredentialGrantDefinitionSettings>::definitions(),
    )
}

pub struct McpCredentialGrantVisitor<T>(std::marker::PhantomData<T>);

impl McpCredentialGrantVisitor<String> {
    fn references() -> Self {
        Self(std::marker::PhantomData)
    }
}

impl McpCredentialGrantVisitor<CredentialGrantDefinitionSettings> {
    fn definitions() -> Self {
        Self(std::marker::PhantomData)
    }
}

impl<'de> Visitor<'de> for McpCredentialGrantVisitor<String> {
    type Value = Vec<String>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a list of unique credential grant references")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut references = Vec::new();
        let mut seen = BTreeSet::new();
        while let Some(reference) = sequence.next_element::<String>()? {
            let id = pi_sandbox::CredentialGrantId::parse(reference.clone())
                .map_err(de::Error::custom)?;
            if !seen.insert(id.into_string()) {
                return Err(de::Error::custom(format!(
                    "duplicate credential grant reference {reference:?}"
                )));
            }
            references.push(reference);
        }
        Ok(references)
    }
}

impl<'de> Visitor<'de> for McpCredentialGrantVisitor<CredentialGrantDefinitionSettings> {
    type Value = Vec<CredentialGrantDefinitionSettings>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a list of unique credential grant definitions")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut definitions = Vec::new();
        let mut ids = BTreeSet::new();
        let mut outputs = BTreeSet::new();
        while let Some(definition) = sequence.next_element::<CredentialGrantDefinitionSettings>()? {
            let id = pi_sandbox::CredentialGrantId::parse(definition.id.clone())
                .map_err(de::Error::custom)?;
            if !ids.insert(id.into_string()) {
                return Err(de::Error::custom(format!(
                    "duplicate credential grant definition id {:?}",
                    definition.id
                )));
            }
            let output_environment = match &definition.output {
                CredentialGrantOutputConfigWire::ApiKey { environment }
                | CredentialGrantOutputConfigWire::OAuthAccessToken { environment, .. } => {
                    EnvironmentVariableName::parse(environment.clone())
                        .map_err(de::Error::custom)?
                        .into_string()
                }
            };
            let audience = match &definition.audience {
                CredentialAudienceWire::McpServer { name } => format!("mcp-server:{name}"),
                CredentialAudienceWire::SpawnFamily { family } => {
                    format!("spawn-family:{}", family.as_str())
                }
            };
            if !outputs.insert((audience, output_environment)) {
                return Err(de::Error::custom(
                    "duplicate credential grant audience and output environment",
                ));
            }
            definitions.push(definition);
        }
        Ok(definitions)
    }
}

pub(crate) fn deserialize_host_http_destinations<'de, D>(
    deserializer: D,
) -> Result<Vec<HostHttpDestinationWire>, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_seq(HostHttpDestinationVisitor)
}

pub struct HostHttpDestinationVisitor;

impl<'de> Visitor<'de> for HostHttpDestinationVisitor {
    type Value = Vec<HostHttpDestinationWire>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a list of unique canonical host HTTP destinations")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut destinations = Vec::new();
        let mut keys = BTreeSet::<HostHttpDestinationKey>::new();
        while let Some(destination) = sequence.next_element::<HostHttpDestinationWire>()? {
            let host = match &destination.host {
                HostHttpHostWire::Dns { name } => HostHttpHost::Dns {
                    name: DnsName::parse(name.clone()).map_err(de::Error::custom)?,
                },
                HostHttpHostWire::Ip { address } => HostHttpHost::Ip { address: *address },
            };
            let validated = HostHttpDestination::new(
                destination.class,
                destination.scheme,
                host,
                destination.port,
                destination.allowed_ip_scopes.clone(),
            )
            .map_err(de::Error::custom)?;
            if !keys.insert(validated.canonical_key()) {
                return Err(de::Error::custom(
                    "duplicate canonical host HTTP destination",
                ));
            }
            destinations.push(destination);
        }
        Ok(destinations)
    }
}
