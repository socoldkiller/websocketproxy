use anyhow::{Context, Result};
use clap::ValueEnum;
use ipnet::Ipv4Net;
use tracing::info;

mod bridge;
mod nat;

#[cfg(target_os = "linux")]
use futures_util::TryStreamExt;
#[cfg(target_os = "linux")]
use rtnetlink::{Handle, LinkUnspec, new_connection, packet_route::link::LinkMessage};

#[cfg(target_os = "linux")]
use self::{bridge::configure_bridge_mode, nat::configure_nat_mode};

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum NetworkMode {
    Nat,
    Bridge,
    None,
}

impl NetworkMode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Nat => "nat",
            Self::Bridge => "bridge",
            Self::None => "none",
        }
    }

    const fn requires_uplink(self) -> bool {
        matches!(self, Self::Nat | Self::Bridge)
    }
}

impl crate::Cli {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.network_mode.requires_uplink() && self.uplink_if.is_none() {
            anyhow::bail!(
                "--uplink-if is required when --network-mode={}",
                self.network_mode.as_str()
            );
        }

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

pub(crate) async fn configure_network_mode(cli: &crate::Cli, tap_name: &str) -> Result<()> {
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
pub(super) fn new_netlink_handle() -> Result<Handle> {
    let (connection, handle, _) =
        new_connection().context("failed to open rtnetlink connection")?;
    tokio::spawn(connection);
    Ok(handle)
}

#[cfg(target_os = "linux")]
pub(super) async fn set_link_up(handle: &Handle, index: u32) -> Result<()> {
    handle
        .link()
        .set(LinkUnspec::new_with_index(index).up().build())
        .execute()
        .await?;
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) async fn link_by_name(handle: &Handle, name: &str) -> Result<LinkMessage> {
    link_by_name_optional(handle, name)
        .await?
        .with_context(|| format!("interface not found: {name}"))
}

#[cfg(target_os = "linux")]
async fn link_by_name_optional(handle: &Handle, name: &str) -> Result<Option<LinkMessage>> {
    let mut links = handle.link().get().match_name(name.to_owned()).execute();
    let link = links
        .try_next()
        .await
        .with_context(|| format!("failed to query interface {name}"))?;

    if link.is_some() {
        anyhow::ensure!(
            links.try_next().await?.is_none(),
            "multiple interfaces named {name}"
        );
    }

    Ok(link)
}

#[cfg(test)]
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
    fn cli_rejects_nat_without_uplink() {
        let cli = Cli::try_parse_from(["websockproxy-relay", "--network-mode", "nat"])
            .expect("cli parse");

        let error = cli.validate().expect_err("nat without uplink must fail");
        assert!(
            error
                .to_string()
                .contains("--uplink-if is required when --network-mode=nat")
        );
    }

    #[test]
    fn nat_gateway_uses_first_host_address() {
        let subnet = "10.200.0.0/24".parse::<Ipv4Net>().expect("nat subnet");

        assert_eq!(
            nat_gateway(subnet).expect("nat gateway"),
            "10.200.0.1".parse::<Ipv4Addr>().expect("gateway ip")
        );
    }
}
