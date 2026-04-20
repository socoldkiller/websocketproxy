use anyhow::{Context, Result};
use clap::ValueEnum;
use ipnet::Ipv4Net;
use tracing::info;

mod bridge;
mod nat;

#[cfg(target_os = "linux")]
use futures_util::TryStreamExt;
#[cfg(target_os = "linux")]
use rtnetlink::{
    Handle, LinkUnspec, new_connection,
    packet_route::{
        AddressFamily,
        link::{LinkAttribute, LinkMessage},
        route::{RouteAttribute, RouteMessage},
    },
};
#[cfg(target_os = "linux")]
use std::fs;

#[cfg(target_os = "linux")]
use self::{bridge::configure_bridge_mode, nat::configure_nat_mode};

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum NetworkMode {
    Nat,
    Bridge,
    None,
}

impl NetworkMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Nat => "nat",
            Self::Bridge => "bridge",
            Self::None => "none",
        }
    }
}

impl crate::Cli {
    pub fn validate(&self) -> Result<()> {
        if self.network_mode == NetworkMode::Nat {
            let _ = self.nat_subnet()?;
        }

        Ok(())
    }

    fn nat_subnet(&self) -> Result<Ipv4Net> {
        let subnet = self
            .nat_network
            .parse::<Ipv4Net>()
            .with_context(|| format!("invalid NAT network `{}`", self.nat_network))?;

        anyhow::ensure!(
            subnet.network() == subnet.addr(),
            "NAT network must be a network address, got `{}`",
            self.nat_network
        );

        anyhow::ensure!(
            subnet.prefix_len() <= 30,
            "NAT network `{}` must leave room for a gateway address",
            self.nat_network
        );

        Ok(subnet)
    }
}

#[cfg(target_os = "linux")]
pub async fn resolve_uplink_if(cli: &crate::Cli) -> Result<String> {
    if let Some(uplink_if) = cli.uplink_if.clone() {
        return Ok(uplink_if);
    }

    if let Some(uplink_if) = default_uplink_if().await? {
        info!(uplink = %uplink_if, "using detected uplink interface");
        return Ok(uplink_if);
    }

    anyhow::bail!(
        "could not determine uplink interface automatically; pass --uplink-if explicitly"
    );
}

#[cfg(not(target_os = "linux"))]
pub async fn resolve_uplink_if(cli: &crate::Cli) -> Result<String> {
    cli.uplink_if
        .clone()
        .context("uplink interface must be set explicitly on non-Linux platforms")
}

pub async fn configure_network_mode(cli: &crate::Cli, tap_name: &str) -> Result<()> {
    #[cfg(not(target_os = "linux"))]
    {
        if cli.network_mode == NetworkMode::None {
            return Ok(());
        }

        anyhow::bail!(
            "network mode `{}` is only supported on Linux",
            cli.network_mode.as_str()
        );
    }

    #[cfg(target_os = "linux")]
    {
        match cli.network_mode {
            NetworkMode::None => {
                info!(tap = %tap_name, "network mode none leaves host networking unchanged");
                Ok(())
            }
            NetworkMode::Bridge => configure_bridge_mode(cli, tap_name).await,
            NetworkMode::Nat => configure_nat_mode(cli, tap_name).await,
        }
    }
}

#[cfg(target_os = "linux")]
pub fn new_netlink_handle() -> Result<Handle> {
    let (connection, handle, _) =
        new_connection().context("failed to open rtnetlink connection")?;
    tokio::spawn(connection);
    Ok(handle)
}

#[cfg(target_os = "linux")]
pub async fn set_link_up(handle: &Handle, index: u32) -> Result<()> {
    handle
        .link()
        .set(LinkUnspec::new_with_index(index).up().build())
        .execute()
        .await?;
    Ok(())
}

#[cfg(target_os = "linux")]
pub async fn link_by_name(handle: &Handle, name: &str) -> Result<LinkMessage> {
    link_by_name_optional(handle, name)
        .await?
        .with_context(|| format!("interface not found: {name}"))
}

#[cfg(target_os = "linux")]
pub async fn link_by_name_optional(handle: &Handle, name: &str) -> Result<Option<LinkMessage>> {
    let mut links = handle.link().get().execute();
    let mut matched = None;

    while let Some(link) = links
        .try_next()
        .await
        .with_context(|| format!("failed to query interface {name}"))?
    {
        if !link_has_name(&link, name) {
            continue;
        }

        anyhow::ensure!(matched.is_none(), "multiple interfaces named {name}");
        matched = Some(link);
    }

    Ok(matched)
}

#[cfg(target_os = "linux")]
pub async fn link_by_index_optional(handle: &Handle, index: u32) -> Result<Option<LinkMessage>> {
    let mut links = handle.link().get().match_index(index).execute();
    let link = links
        .try_next()
        .await
        .with_context(|| format!("failed to query interface index {index}"))?;

    if link.is_some() {
        anyhow::ensure!(
            links.try_next().await?.is_none(),
            "multiple interfaces with index {index}"
        );
    }

    Ok(link)
}

#[cfg(target_os = "linux")]
pub fn link_name(link: &LinkMessage) -> Option<String> {
    link.attributes
        .iter()
        .find_map(|attribute| match attribute {
            LinkAttribute::IfName(name) => Some(name.clone()),
            _ => None,
        })
}

#[cfg(target_os = "linux")]
fn link_has_name(link: &LinkMessage, name: &str) -> bool {
    link.attributes.iter().any(|attribute| match attribute {
        LinkAttribute::IfName(link_name) => link_name == name,
        _ => false,
    })
}

#[cfg(target_os = "linux")]
async fn default_uplink_if() -> Result<Option<String>> {
    if let Some(uplink_if) = default_route_uplink_if().await? {
        return Ok(Some(uplink_if));
    }

    single_physical_uplink_if()
}

#[cfg(target_os = "linux")]
async fn default_route_uplink_if() -> Result<Option<String>> {
    let handle = new_netlink_handle()?;
    let mut routes = handle.route().get(RouteMessage::default()).execute();
    let mut best_candidate: Option<(u32, u32)> = None;

    while let Some(route) = routes.try_next().await? {
        if route.header.address_family != AddressFamily::Inet
            || route.header.destination_prefix_length != 0
        {
            continue;
        }

        let Some(output_interface) =
            route
                .attributes
                .iter()
                .find_map(|attribute| match attribute {
                    RouteAttribute::Oif(index) => Some(*index),
                    _ => None,
                })
        else {
            continue;
        };

        let metric = route
            .attributes
            .iter()
            .find_map(|attribute| match attribute {
                RouteAttribute::Priority(priority) => Some(*priority),
                _ => None,
            })
            .unwrap_or(0);

        if best_candidate.is_none_or(|(_, best_metric)| metric < best_metric) {
            best_candidate = Some((output_interface, metric));
        }
    }

    let Some((index, _metric)) = best_candidate else {
        return Ok(None);
    };

    let Some(link) = link_by_index_optional(&handle, index).await? else {
        return Ok(None);
    };

    Ok(link_name(&link))
}

#[cfg(target_os = "linux")]
fn single_physical_uplink_if() -> Result<Option<String>> {
    let mut candidates = Vec::new();

    for entry in fs::read_dir("/sys/class/net").context("failed to read /sys/class/net")? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.into_string().map_err(|_| {
            anyhow::anyhow!("encountered non-UTF-8 interface name in /sys/class/net")
        })?;

        if name == "lo" {
            continue;
        }

        if entry.path().join("device").exists() {
            candidates.push(name);
        }
    }

    match candidates.as_slice() {
        [name] => Ok(Some(name.clone())),
        [] => Ok(None),
        _ => anyhow::bail!(
            "multiple physical interfaces detected ({}) and no default route was found; pass --uplink-if explicitly",
            candidates.join(", ")
        ),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{NetworkMode, nat::nat_gateway};
    use crate::Cli;
    use clap::Parser;
    use ipnet::Ipv4Net;
    use std::net::Ipv4Addr;

    #[test]
    fn cli_parses_network_mode_none_by_default() {
        let cli = Cli::try_parse_from(["websockproxy-relay"]).expect("default cli");

        assert_eq!(cli.network_mode, NetworkMode::None);
        assert_eq!(cli.bridge_name, "br0");
        assert_eq!(cli.nat_network, "10.200.0.0/24");
    }

    #[test]
    fn cli_allows_nat_without_uplink() {
        let cli = Cli::try_parse_from(["websockproxy-relay", "--network-mode", "nat"])
            .expect("cli parse");

        cli.validate().expect("nat validation");
    }

    #[test]
    fn nat_gateway_uses_first_host_address() {
        let subnet = "10.200.0.0/24".parse::<Ipv4Net>().expect("nat subnet");

        assert_eq!(
            nat_gateway(subnet),
            "10.200.0.1".parse::<Ipv4Addr>().expect("gateway ip")
        );
    }
}
