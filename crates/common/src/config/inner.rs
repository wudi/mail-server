/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::Arc,
};

use ahash::{AHashMap, AHashSet, RandomState};
use arc_swap::ArcSwap;
use dashmap::DashMap;
use mail_auth::{Parameters, Txt, MX};
use mail_send::smtp::tls::build_tls_connector;
use nlp::bayes::{TokenHash, Weights};
use parking_lot::RwLock;
use utils::{
    cache::{Cache, CacheWithTtl},
    config::Config,
    snowflake::SnowflakeIdGenerator,
};

use crate::{
    auth::{roles::RolePermissions, AccessToken},
    config::smtp::resolver::{Policy, Tlsa},
    listener::blocked::BlockedIps,
    manager::webadmin::WebAdminManager,
    Account, AccountId, Caches, Data, Mailbox, MailboxId, MailboxState, NextMailboxState, Threads,
    ThrottleKeyHasherBuilder, TlsConnectors,
};

use super::server::tls::{build_self_signed_cert, parse_certificates};

impl Data {
    pub fn parse(config: &mut Config) -> Self {
        // Parse certificates
        let mut certificates = AHashMap::new();
        let mut subject_names = AHashSet::new();
        parse_certificates(config, &mut certificates, &mut subject_names);
        if subject_names.is_empty() {
            subject_names.insert("localhost".to_string());
        }

        // Parse capacities
        let shard_amount = config
            .property::<u64>("limiter.shard")
            .unwrap_or_else(|| (num_cpus::get() * 2) as u64)
            .next_power_of_two() as usize;
        let capacity = config.property("limiter.capacity").unwrap_or(100);

        // Parse id generator
        let id_generator = config
            .property::<u64>("cluster.node-id")
            .map(SnowflakeIdGenerator::with_node_id)
            .unwrap_or_default();

        Data {
            tls_certificates: ArcSwap::from_pointee(certificates),
            tls_self_signed_cert: build_self_signed_cert(
                subject_names.into_iter().collect::<Vec<_>>(),
            )
            .or_else(|err| {
                config.new_build_error("certificate.self-signed", err);
                build_self_signed_cert(vec!["localhost".to_string()])
            })
            .ok()
            .map(Arc::new),
            blocked_ips: RwLock::new(BlockedIps::parse(config).blocked_ip_addresses),
            blocked_ips_version: 0.into(),
            jmap_id_gen: id_generator.clone(),
            queue_id_gen: id_generator.clone(),
            span_id_gen: id_generator,
            queue_status: true.into(),
            webadmin: config
                .value("webadmin.path")
                .map(|path| WebAdminManager::new(path.into()))
                .unwrap_or_default(),
            config_version: 0.into(),
            jmap_limiter: DashMap::with_capacity_and_hasher_and_shard_amount(
                capacity,
                RandomState::default(),
                shard_amount,
            ),
            imap_limiter: DashMap::with_capacity_and_hasher_and_shard_amount(
                capacity,
                RandomState::default(),
                shard_amount,
            ),
            logos: Default::default(),
            smtp_session_throttle: DashMap::with_capacity_and_hasher_and_shard_amount(
                capacity,
                ThrottleKeyHasherBuilder::default(),
                shard_amount,
            ),
            smtp_queue_throttle: DashMap::with_capacity_and_hasher_and_shard_amount(
                capacity,
                ThrottleKeyHasherBuilder::default(),
                shard_amount,
            ),
            smtp_connectors: TlsConnectors::default(),
            asn_geo_data: Default::default(),
        }
    }
}

impl Caches {
    pub fn parse(config: &mut Config) -> Self {
        const MB_10: u64 = 10 * 1024 * 1024;
        const MB_5: u64 = 5 * 1024 * 1024;
        const MB_1: u64 = 1024 * 1024;

        Caches {
            access_tokens: Cache::from_config(
                config,
                "access-token",
                MB_10,
                (std::mem::size_of::<AccessToken>() + 255) as u64,
            ),
            http_auth: Cache::from_config(
                config,
                "http-auth",
                MB_1,
                (50 + std::mem::size_of::<u32>()) as u64,
            ),
            permissions: Cache::from_config(
                config,
                "permission",
                MB_5,
                std::mem::size_of::<RolePermissions>() as u64,
            ),
            account: Cache::from_config(
                config,
                "account",
                MB_10,
                (std::mem::size_of::<AccountId>()
                    + std::mem::size_of::<Account>()
                    + (15 * (std::mem::size_of::<Mailbox>() + 60))) as u64,
            ),
            mailbox: Cache::from_config(
                config,
                "mailbox",
                MB_10,
                (std::mem::size_of::<MailboxId>()
                    + std::mem::size_of::<MailboxState>()
                    + std::mem::size_of::<NextMailboxState>()
                    + (1024 * std::mem::size_of::<u64>())) as u64,
            ),
            threads: Cache::from_config(
                config,
                "thread",
                MB_10,
                (std::mem::size_of::<Threads>() + (500 * std::mem::size_of::<u64>())) as u64,
            ),
            bayes: CacheWithTtl::from_config(
                config,
                "bayes",
                MB_10,
                (std::mem::size_of::<TokenHash>() + std::mem::size_of::<Weights>()) as u64,
            ),
            dns_txt: CacheWithTtl::from_config(
                config,
                "dns.txt",
                MB_5,
                (std::mem::size_of::<Txt>() + 255) as u64,
            ),
            dns_mx: CacheWithTtl::from_config(
                config,
                "dns.mx",
                MB_5,
                ((std::mem::size_of::<MX>() + 255) * 2) as u64,
            ),
            dns_ptr: CacheWithTtl::from_config(
                config,
                "dns.ptr",
                MB_1,
                (std::mem::size_of::<IpAddr>() + 255) as u64,
            ),
            dns_ipv4: CacheWithTtl::from_config(
                config,
                "dns.ipv4",
                MB_5,
                ((std::mem::size_of::<Ipv4Addr>() + 255) * 2) as u64,
            ),
            dns_ipv6: CacheWithTtl::from_config(
                config,
                "dns.ipv6",
                MB_5,
                ((std::mem::size_of::<Ipv6Addr>() + 255) * 2) as u64,
            ),
            dns_tlsa: CacheWithTtl::from_config(
                config,
                "dns.tlsa",
                MB_1,
                (std::mem::size_of::<Tlsa>() + 255) as u64,
            ),
            dbs_mta_sts: CacheWithTtl::from_config(
                config,
                "dns.mta-sts",
                MB_1,
                (std::mem::size_of::<Policy>() + 255) as u64,
            ),
            dns_rbl: CacheWithTtl::from_config(
                config,
                "dns.rbl",
                MB_5,
                ((std::mem::size_of::<Ipv4Addr>() + 255) * 2) as u64,
            ),
        }
    }

    #[allow(clippy::type_complexity)]
    #[inline(always)]
    pub fn build_auth_parameters<T>(
        &self,
        params: T,
    ) -> Parameters<
        '_,
        T,
        CacheWithTtl<String, Txt>,
        CacheWithTtl<String, Arc<Vec<MX>>>,
        CacheWithTtl<String, Arc<Vec<Ipv4Addr>>>,
        CacheWithTtl<String, Arc<Vec<Ipv6Addr>>>,
        CacheWithTtl<IpAddr, Arc<Vec<String>>>,
    > {
        Parameters {
            params,
            cache_txt: Some(&self.dns_txt),
            cache_mx: Some(&self.dns_mx),
            cache_ptr: Some(&self.dns_ptr),
            cache_ipv4: Some(&self.dns_ipv4),
            cache_ipv6: Some(&self.dns_ipv6),
        }
    }
}

impl Default for Data {
    fn default() -> Self {
        Self {
            tls_certificates: Default::default(),
            tls_self_signed_cert: Default::default(),
            blocked_ips: Default::default(),
            blocked_ips_version: 0.into(),
            jmap_id_gen: Default::default(),
            queue_id_gen: Default::default(),
            span_id_gen: Default::default(),
            queue_status: true.into(),
            webadmin: Default::default(),
            config_version: Default::default(),
            jmap_limiter: Default::default(),
            imap_limiter: Default::default(),
            logos: Default::default(),
            smtp_session_throttle: Default::default(),
            smtp_queue_throttle: Default::default(),
            smtp_connectors: Default::default(),
            asn_geo_data: Default::default(),
        }
    }
}

impl Default for TlsConnectors {
    fn default() -> Self {
        TlsConnectors {
            pki_verify: build_tls_connector(false),
            dummy_verify: build_tls_connector(true),
        }
    }
}
