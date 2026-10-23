//! Chain facts (§15.1/§15.2): the two domains, the CCTP V2 contract addresses,
//! the USDC addresses, the finality constants, and the boot wiring asserts.
//!
//! Everything here is a **protocol constant measured on the forks**, not
//! configuration: the addresses are identical on both chains and were read
//! live (§15.2's table), the domains and thresholds come from the contracts
//! themselves. Configuration carries only deployment facts (URLs, keys,
//! state dir) — never an address a typo could move value on.
use crate::error::{Error, Result};
use crate::transfer::{eth_call, FeeSurface, IMessageTransmitterV2, ITokenMessengerV2, IERC20};
use alloy_primitives::{address, Address, B256, U256};
use alloy_sol_types::SolCall;
use mirror_anvil::Rpc;
use std::time::Duration;

/// Ethereum's CCTP domain (`MT.localDomain()` = 0 on T, measured).
pub const DOMAIN_ETHEREUM: u32 = 0;
/// HyperEVM's CCTP domain (`MT.localDomain()` = 19 on V, measured).
pub const DOMAIN_HYPER_EVM: u32 = 19;
/// T forks Ethereum mainnet: chain id 1.
pub const CHAIN_ID_ETHEREUM: u64 = 1;
/// V forks HyperEVM: chain id 999.
pub const CHAIN_ID_HYPER_EVM: u64 = 999;

/// `TOKEN_MESSENGER_MIN_FINALITY_THRESHOLD` (FinalityThresholds.sol): the
/// messenger accepts the unfinalized path only at ≥ 500.
pub const FINALITY_THRESHOLD_MIN: u32 = 500;
/// Confirmed / Fast (FinalityThresholds.sol).
pub const FINALITY_THRESHOLD_CONFIRMED: u32 = 1000;
/// Finalized / Standard (FinalityThresholds.sol).
pub const FINALITY_THRESHOLD_FINALIZED: u32 = 2000;
/// `BaseTokenMessenger.MIN_FEE_MULTIPLIER`: `minFee` is in 1/1000 bps.
pub const MIN_FEE_MULTIPLIER: u64 = 10_000_000;
/// The message format version both transmitters report (`MT.version()`,
/// `TM.messageBodyVersion()`, both measured = 1).
pub const MESSAGE_VERSION: u32 = 1;
/// Circle's USDC is 6-decimal on both chains (`TokenMinterV2.getLocalToken`
/// cross-checked, §15.1).
pub const USDC_DECIMALS: u8 = 6;

// The five contracts, identical on both chains (verified live, §15.1/§15.2).
/// TokenMessengerV2 — `depositForBurn` / `handleReceive*`.
pub const TOKEN_MESSENGER_V2: Address = address!("28b5a0e9C621a5BadaA536219b3a228C8168cf5d");
/// MessageTransmitterV2 — `sendMessage` / `receiveMessage` / attester admin.
pub const MESSAGE_TRANSMITTER_V2: Address = address!("81D40F21F12A8F0E3252Bccb954D722d4c464B64");
/// TokenMinterV2 — burns the local token, mints the remote one.
pub const TOKEN_MINTER_V2: Address = address!("fd78EE919681417d192449715b2594ab58f5D002");
/// MessageV2 — the message library contract (no call surface this bridge
/// needs; pinned because §15.1 lists it).
pub const MESSAGE_V2: Address = address!("ec546b6B005471ECf012e5aF77FBeC07e0FD8f78");

/// Ethereum USDC (the burn token on domain 0).
pub const USDC_ETHEREUM: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
/// HyperEVM USDC — Circle's native USDC, the CCTP local token
/// (`getLocalToken(0, eth-USDC)` = this, §15.1). The venue's `spotMeta` link
/// `0x6b9e…` is **not** an ERC-20 and is never used here.
pub const USDC_HYPER_EVM: Address = address!("b88339CB7199b77E23DB6E890353E22632Ba630f");

/// One served chain: its protocol identity plus the RPC the bridge talks to.
#[derive(Clone)]
pub struct Chain {
    /// The human name (`ethereum`, `hyper_evm`) for logs and metrics.
    pub name: &'static str,
    pub domain: u32,
    pub chain_id: u64,
    pub rpc: Rpc,
    /// The burn/mint token on this domain.
    pub usdc: Address,
}

impl std::fmt::Debug for Chain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Chain")
            .field("name", &self.name)
            .field("domain", &self.domain)
            .field("chain_id", &self.chain_id)
            .field("usdc", &self.usdc)
            .finish_non_exhaustive()
    }
}

impl Chain {
    /// T — Ethereum, domain 0.
    pub fn ethereum(url: &str, timeout: Duration) -> Result<Self> {
        Ok(Self {
            name: "ethereum",
            domain: DOMAIN_ETHEREUM,
            chain_id: CHAIN_ID_ETHEREUM,
            rpc: Rpc::new(url, timeout)?,
            usdc: USDC_ETHEREUM,
        })
    }

    /// V — HyperEVM, domain 19.
    pub fn hyper_evm(url: &str, timeout: Duration) -> Result<Self> {
        Ok(Self {
            name: "hyper_evm",
            domain: DOMAIN_HYPER_EVM,
            chain_id: CHAIN_ID_HYPER_EVM,
            rpc: Rpc::new(url, timeout)?,
            usdc: USDC_HYPER_EVM,
        })
    }

    /// The boot wiring asserts (§15.4): the chain must be the one the domain
    /// pins, the message versions must match, the local transmitter must be
    /// the pinned one, and the peer's TokenMessenger must be registered
    /// before the bridge will move value. The min-fee surface is probed, not
    /// assumed (it is revision-dependent, §15.2).
    pub async fn verify_wiring(&self, peer: &Chain) -> Result<Wiring> {
        let chain_id = self.rpc.chain_id().await?;
        if chain_id != self.chain_id {
            return Err(self.wiring("chain id", chain_id, self.chain_id));
        }
        let local_domain = self
            .assert_getter(
                MESSAGE_TRANSMITTER_V2,
                &IMessageTransmitterV2::localDomainCall {},
                self.domain,
                "localDomain",
            )
            .await?;
        let version = self
            .assert_getter(
                MESSAGE_TRANSMITTER_V2,
                &IMessageTransmitterV2::versionCall {},
                MESSAGE_VERSION,
                "message version",
            )
            .await?;
        let body_version = self
            .assert_getter(
                TOKEN_MESSENGER_V2,
                &ITokenMessengerV2::messageBodyVersionCall {},
                MESSAGE_VERSION,
                "messageBodyVersion",
            )
            .await?;
        let local_transmitter = self
            .assert_getter(
                TOKEN_MESSENGER_V2,
                &ITokenMessengerV2::localMessageTransmitterCall {},
                MESSAGE_TRANSMITTER_V2,
                "TM.localMessageTransmitter()",
            )
            .await?;
        let peer_messenger = self
            .assert_getter(
                TOKEN_MESSENGER_V2,
                &ITokenMessengerV2::remoteTokenMessengersCall {
                    domain: peer.domain,
                },
                TOKEN_MESSENGER_V2.into_word(),
                "TM.remoteTokenMessengers(peer)",
            )
            .await?;
        let fees = FeeSurface::probe(&self.rpc, TOKEN_MESSENGER_V2).await?;
        Ok(Wiring {
            chain_id,
            local_domain,
            version,
            body_version,
            local_transmitter,
            peer_messenger,
            min_fee: fees.min_fee,
        })
    }

    /// One wiring assert: `eth_call` the getter and compare it, naming the
    /// field on mismatch.
    async fn assert_getter<C: SolCall>(
        &self,
        to: Address,
        call: &C,
        want: C::Return,
        what: &'static str,
    ) -> Result<C::Return>
    where
        C::Return: PartialEq + std::fmt::Display,
    {
        let got = eth_call(&self.rpc, to, call).await?;
        if got != want {
            return Err(self.wiring(what, &got, &want));
        }
        Ok(got)
    }

    fn wiring(&self, what: &'static str, got: impl std::fmt::Display, want: impl std::fmt::Display) -> Error {
        Error::Wiring {
            chain: self.name,
            what,
            got: got.to_string(),
            want: want.to_string(),
        }
    }

    /// The owner's USDC balance on this chain.
    pub async fn usdc_balance(&self, owner: Address) -> Result<U256> {
        eth_call(
            &self.rpc,
            self.usdc,
            &IERC20::balanceOfCall { account: owner },
        )
        .await
    }

    /// The owner's USDC allowance for `spender` on this chain.
    pub async fn usdc_allowance(&self, owner: Address, spender: Address) -> Result<U256> {
        eth_call(
            &self.rpc,
            self.usdc,
            &IERC20::allowanceCall { owner, spender },
        )
        .await
    }

    /// The native balance of `who` (gas alerts; the relay key pays its own).
    pub async fn native_balance(&self, who: Address) -> Result<U256> {
        Ok(self.rpc.call("eth_getBalance", (who, "latest")).await?)
    }

    /// A devnet stipend: write the native balance directly
    /// (`anvil_setBalance` — the same substitution daloc's harness makes; T's
    /// engine already runs on cheatcodes). The deployment's block producer
    /// mines nothing here; the balance is chain state, not a transaction.
    pub async fn fund_native(&self, who: Address, amount: U256) -> Result<()> {
        self.rpc.anvil_set_balance(who, amount).await?;
        Ok(())
    }
}

/// What a successful [`Chain::verify_wiring`] observed (logged at boot).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wiring {
    pub chain_id: u64,
    pub local_domain: u32,
    pub version: u32,
    pub body_version: u32,
    pub local_transmitter: Address,
    pub peer_messenger: B256,
    /// `None` = the revision has no min-fee surface (Ethereum's instance,
    /// measured); `Some(0)` = present and zero (HyperEVM's, measured).
    pub min_fee: Option<U256>,
}

/// The chain serving `domain`, or a named refusal.
pub fn by_domain(chains: &[Chain], domain: u32) -> Result<&Chain> {
    chains
        .iter()
        .find(|chain| chain.domain == domain)
        .ok_or(Error::UnknownDomain { domain })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domains_and_addresses_are_the_measured_ones() {
        assert_eq!(DOMAIN_ETHEREUM, 0);
        assert_eq!(DOMAIN_HYPER_EVM, 19);
        assert_eq!(CHAIN_ID_ETHEREUM, 1);
        assert_eq!(CHAIN_ID_HYPER_EVM, 999);
        assert_eq!(FINALITY_THRESHOLD_MIN, 500);
        assert_eq!(FINALITY_THRESHOLD_CONFIRMED, 1000);
        assert_eq!(FINALITY_THRESHOLD_FINALIZED, 2000);
        assert_eq!(MIN_FEE_MULTIPLIER, 10_000_000);
        // The addresses §15.1 verified identical on both chains.
        assert_eq!(
            TOKEN_MESSENGER_V2,
            address!("28b5a0e9C621a5BadaA536219b3a228C8168cf5d")
        );
        assert_eq!(
            MESSAGE_TRANSMITTER_V2,
            address!("81D40F21F12A8F0E3252Bccb954D722d4c464B64")
        );
        assert_eq!(
            TOKEN_MINTER_V2,
            address!("fd78EE919681417d192449715b2594ab58f5D002")
        );
        assert_eq!(
            MESSAGE_V2,
            address!("ec546b6B005471ECf012e5aF77FBeC07e0FD8f78")
        );
        assert_eq!(
            USDC_ETHEREUM,
            address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")
        );
        assert_eq!(
            USDC_HYPER_EVM,
            address!("b88339CB7199b77E23DB6E890353E22632Ba630f")
        );
    }

    #[test]
    fn by_domain_names_the_domain_it_cannot_serve() {
        let chains = vec![
            Chain {
                name: "ethereum",
                domain: DOMAIN_ETHEREUM,
                chain_id: CHAIN_ID_ETHEREUM,
                rpc: Rpc::new("http://127.0.0.1:1", Duration::from_secs(1)).unwrap(),
                usdc: USDC_ETHEREUM,
            },
            Chain {
                name: "hyper_evm",
                domain: DOMAIN_HYPER_EVM,
                chain_id: CHAIN_ID_HYPER_EVM,
                rpc: Rpc::new("http://127.0.0.1:1", Duration::from_secs(1)).unwrap(),
                usdc: USDC_HYPER_EVM,
            },
        ];
        assert_eq!(by_domain(&chains, 0).unwrap().name, "ethereum");
        assert_eq!(by_domain(&chains, 19).unwrap().name, "hyper_evm");
        let err = by_domain(&chains, 7).unwrap_err();
        assert!(matches!(err, Error::UnknownDomain { domain: 7 }), "{err}");
    }
}
