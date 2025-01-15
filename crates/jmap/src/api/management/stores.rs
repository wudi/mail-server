/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use common::{
    auth::AccessToken,
    ipc::{HousekeeperEvent, PurgeType},
    manager::webadmin::Resource,
    *,
};
use directory::{
    backend::internal::manage::{self, ManageDirectory},
    Permission,
};
use hyper::Method;
use jmap_proto::{
    object::{index::ObjectIndexBuilder, Object},
    types::{collection::Collection, property::Property, value::Value},
};
use serde_json::json;
use store::write::{assert::HashedValue, BatchBuilder, ValueClass, F_VALUE};
use trc::AddContext;
use utils::url_params::UrlParams;

use crate::{
    api::{
        http::{HttpSessionData, ToHttpResponse},
        HttpRequest, HttpResponse, JsonResponse,
    },
    email::ingest::EmailIngest,
    mailbox::{set::SCHEMA, UidMailbox},
    services::index::Indexer,
    JmapMethods,
};

use super::decode_path_element;
#[cfg(feature = "enterprise")]
use super::enterprise::undelete::UndeleteApi;
use std::future::Future;

pub trait ManageStore: Sync + Send {
    fn handle_manage_store(
        &self,
        req: &HttpRequest,
        path: Vec<&str>,
        body: Option<Vec<u8>>,
        session: &HttpSessionData,
        access_token: &AccessToken,
    ) -> impl Future<Output = trc::Result<HttpResponse>> + Send;

    fn housekeeper_request(
        &self,
        event: HousekeeperEvent,
    ) -> impl Future<Output = trc::Result<HttpResponse>> + Send;
}

impl ManageStore for Server {
    async fn handle_manage_store(
        &self,
        req: &HttpRequest,
        path: Vec<&str>,
        body: Option<Vec<u8>>,
        session: &HttpSessionData,
        access_token: &AccessToken,
    ) -> trc::Result<HttpResponse> {
        match (
            path.get(1).copied(),
            path.get(2).copied(),
            path.get(3).copied(),
            req.method(),
        ) {
            (Some("blobs"), Some(blob_hash), _, &Method::GET) => {
                // Validate the access token
                access_token.assert_has_permission(Permission::BlobFetch)?;

                let blob_hash = URL_SAFE_NO_PAD
                    .decode(decode_path_element(blob_hash).as_bytes())
                    .map_err(|err| {
                        trc::EventType::Resource(trc::ResourceEvent::BadParameters)
                            .from_base64_error(err)
                    })?;
                let contents = self
                    .core
                    .storage
                    .blob
                    .get_blob(&blob_hash, 0..usize::MAX)
                    .await?
                    .ok_or_else(|| trc::ManageEvent::NotFound.into_err())?;
                let params = UrlParams::new(req.uri().query());
                let offset = params.parse("offset").unwrap_or(0);
                let limit = params.parse("limit").unwrap_or(usize::MAX);
                let contents = if offset == 0 && limit == usize::MAX {
                    contents
                } else {
                    contents
                        .get(offset..std::cmp::min(offset + limit, contents.len()))
                        .unwrap_or_default()
                        .to_vec()
                };

                Ok(Resource::new("application/octet-stream", contents).into_http_response())
            }
            (Some("purge"), Some("blob"), _, &Method::GET) => {
                // Validate the access token
                access_token.assert_has_permission(Permission::PurgeBlobStore)?;

                self.housekeeper_request(HousekeeperEvent::Purge(PurgeType::Blobs {
                    store: self.core.storage.data.clone(),
                    blob_store: self.core.storage.blob.clone(),
                }))
                .await
            }
            (Some("purge"), Some("data"), id, &Method::GET) => {
                // Validate the access token
                access_token.assert_has_permission(Permission::PurgeDataStore)?;

                let store = if let Some(id) = id.filter(|id| *id != "default") {
                    if let Some(store) = self.core.storage.stores.get(id) {
                        store.clone()
                    } else {
                        return Err(trc::ResourceEvent::NotFound.into_err());
                    }
                } else {
                    self.core.storage.data.clone()
                };

                self.housekeeper_request(HousekeeperEvent::Purge(PurgeType::Data(store)))
                    .await
            }
            (Some("purge"), Some("in-memory"), id, &Method::GET) => {
                // Validate the access token
                access_token.assert_has_permission(Permission::PurgeInMemoryStore)?;

                let store = if let Some(id) = id.filter(|id| *id != "default") {
                    if let Some(store) = self.core.storage.lookups.get(id) {
                        store.clone()
                    } else {
                        return Err(trc::ResourceEvent::NotFound.into_err());
                    }
                } else {
                    self.core.storage.lookup.clone()
                };

                let prefix = match path.get(4).copied() {
                    Some("acme") => vec![KV_ACME].into(),
                    Some("oauth") => vec![KV_OAUTH].into(),
                    Some("rate-rcpt") => vec![KV_RATE_LIMIT_RCPT].into(),
                    Some("rate-scan") => vec![KV_RATE_LIMIT_SCAN].into(),
                    Some("rate-loiter") => vec![KV_RATE_LIMIT_LOITER].into(),
                    Some("rate-auth") => vec![KV_RATE_LIMIT_AUTH].into(),
                    Some("rate-hash") => vec![KV_RATE_LIMIT_HASH].into(),
                    Some("rate-contact") => vec![KV_RATE_LIMIT_CONTACT].into(),
                    Some("rate-http-authenticated") => {
                        vec![KV_RATE_LIMIT_HTTP_AUTHENTICATED].into()
                    }
                    Some("rate-http-anonymous") => vec![KV_RATE_LIMIT_HTTP_ANONYMOUS].into(),
                    Some("rate-imap") => vec![KV_RATE_LIMIT_IMAP].into(),
                    Some("reputation-ip") => vec![KV_REPUTATION_IP].into(),
                    Some("reputation-from") => vec![KV_REPUTATION_FROM].into(),
                    Some("reputation-domain") => vec![KV_REPUTATION_DOMAIN].into(),
                    Some("reputation-asn") => vec![KV_REPUTATION_ASN].into(),
                    Some("greylist") => vec![KV_GREYLIST].into(),
                    Some("bayes-account") => {
                        if let Some(account) = path.get(5).copied() {
                            let account_id = self
                                .core
                                .storage
                                .data
                                .get_principal_id(decode_path_element(account).as_ref())
                                .await?
                                .ok_or_else(|| trc::ManageEvent::NotFound.into_err())?;

                            let mut key = Vec::with_capacity(std::mem::size_of::<u32>() + 1);
                            key.push(KV_BAYES_MODEL_USER);
                            key.extend_from_slice(&account_id.to_be_bytes());
                            key.into()
                        } else {
                            vec![KV_BAYES_MODEL_USER].into()
                        }
                    }
                    Some("bayes-global") => vec![KV_BAYES_MODEL_GLOBAL].into(),
                    Some("trusted-reply") => vec![KV_TRUSTED_REPLY].into(),
                    Some("lock-purge-account") => vec![KV_LOCK_PURGE_ACCOUNT].into(),
                    Some("lock-queue-message") => vec![KV_LOCK_QUEUE_MESSAGE].into(),
                    Some("lock-queue-report") => vec![KV_LOCK_QUEUE_REPORT].into(),
                    Some("lock-email-task") => vec![KV_LOCK_EMAIL_TASK].into(),
                    Some("lock-housekeeper") => vec![KV_LOCK_HOUSEKEEPER].into(),
                    _ => None,
                };

                self.housekeeper_request(HousekeeperEvent::Purge(PurgeType::Lookup {
                    store,
                    prefix,
                }))
                .await
            }
            (Some("purge"), Some("account"), id, &Method::GET) => {
                // Validate the access token
                access_token.assert_has_permission(Permission::PurgeAccount)?;

                let account_id = if let Some(id) = id {
                    self.core
                        .storage
                        .data
                        .get_principal_id(decode_path_element(id).as_ref())
                        .await?
                        .ok_or_else(|| trc::ManageEvent::NotFound.into_err())?
                        .into()
                } else {
                    None
                };

                self.housekeeper_request(HousekeeperEvent::Purge(PurgeType::Account(account_id)))
                    .await
            }
            (Some("reindex"), id, None, &Method::GET) => {
                // Validate the access token
                access_token.assert_has_permission(Permission::FtsReindex)?;

                let account_id = if let Some(id) = id {
                    self.core
                        .storage
                        .data
                        .get_principal_id(decode_path_element(id).as_ref())
                        .await?
                        .ok_or_else(|| trc::ManageEvent::NotFound.into_err())?
                        .into()
                } else {
                    None
                };
                let tenant_id = access_token.tenant.map(|t| t.id);

                let jmap = self.clone();
                tokio::spawn(async move {
                    if let Err(err) = jmap.reindex(account_id, tenant_id).await {
                        trc::error!(err.details("Failed to reindex FTS"));
                    }
                });

                Ok(JsonResponse::new(json!({
                    "data": (),
                }))
                .into_http_response())
            }
            // SPDX-SnippetBegin
            // SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
            // SPDX-License-Identifier: LicenseRef-SEL
            #[cfg(feature = "enterprise")]
            (Some("undelete"), _, _, _) => {
                // WARNING: TAMPERING WITH THIS FUNCTION IS STRICTLY PROHIBITED
                // Any attempt to modify, bypass, or disable this license validation mechanism
                // constitutes a severe violation of the Stalwart Enterprise License Agreement.
                // Such actions may result in immediate termination of your license, legal action,
                // and substantial financial penalties. Stalwart Labs Ltd. actively monitors for
                // unauthorized modifications and will pursue all available legal remedies against
                // violators to the fullest extent of the law, including but not limited to claims
                // for copyright infringement, breach of contract, and fraud.

                // Validate the access token
                access_token.assert_has_permission(Permission::Undelete)?;

                if self.core.is_enterprise_edition() {
                    self.handle_undelete_api_request(req, path, body, session)
                        .await
                } else {
                    Err(manage::enterprise())
                }
            }
            // SPDX-SnippetEnd
            (Some("uids"), Some(account_id), None, &Method::DELETE) => {
                let account_id = self
                    .core
                    .storage
                    .data
                    .get_principal_id(decode_path_element(account_id).as_ref())
                    .await?
                    .ok_or_else(|| trc::ManageEvent::NotFound.into_err())?;

                let result = reset_imap_uids(self, account_id).await?;

                Ok(JsonResponse::new(json!({
                    "data": result,
                }))
                .into_http_response())
            }
            _ => Err(trc::ResourceEvent::NotFound.into_err()),
        }
    }

    async fn housekeeper_request(&self, event: HousekeeperEvent) -> trc::Result<HttpResponse> {
        self.inner
            .ipc
            .housekeeper_tx
            .send(event)
            .await
            .map_err(|err| {
                trc::EventType::Server(trc::ServerEvent::ThreadError)
                    .reason(err)
                    .details("Failed to send housekeeper event")
            })?;

        Ok(JsonResponse::new(json!({
            "data": (),
        }))
        .into_http_response())
    }
}

pub async fn reset_imap_uids(server: &Server, account_id: u32) -> trc::Result<(u32, u32)> {
    let mut mailbox_count = 0;
    let mut email_count = 0;

    for mailbox_id in server
        .get_document_ids(account_id, Collection::Mailbox)
        .await?
        .unwrap_or_default()
    {
        let mailbox = server
            .get_property::<HashedValue<Object<Value>>>(
                account_id,
                Collection::Mailbox,
                mailbox_id,
                Property::Value,
            )
            .await
            .caused_by(trc::location!())?
            .ok_or_else(|| trc::ImapEvent::Error.into_err().caused_by(trc::location!()))?;

        let mut batch = BatchBuilder::new();
        batch
            .with_account_id(account_id)
            .with_collection(Collection::Mailbox)
            .update_document(mailbox_id)
            .custom(
                ObjectIndexBuilder::new(SCHEMA)
                    .with_current(mailbox)
                    .with_changes(Object::with_capacity(1).with_property(
                        Property::Cid,
                        Value::UnsignedInt(rand::random::<u32>() as u64),
                    )),
            )
            .clear(Property::EmailIds);
        server
            .write_batch(batch)
            .await
            .caused_by(trc::location!())?;
        mailbox_count += 1;
    }

    // Reset all UIDs
    for message_id in server
        .get_document_ids(account_id, Collection::Email)
        .await
        .caused_by(trc::location!())?
        .unwrap_or_default()
    {
        let uids = server
            .get_property::<HashedValue<Vec<UidMailbox>>>(
                account_id,
                Collection::Email,
                message_id,
                Property::MailboxIds,
            )
            .await
            .caused_by(trc::location!())?;
        let mut uids = if let Some(uids) = uids.filter(|uids| !uids.inner.is_empty()) {
            uids
        } else {
            continue;
        };

        for uid_mailbox in &mut uids.inner {
            uid_mailbox.uid = server
                .assign_imap_uid(account_id, uid_mailbox.mailbox_id)
                .await
                .caused_by(trc::location!())?;
        }

        // Prepare write batch
        let mut batch = BatchBuilder::new();
        batch
            .with_account_id(account_id)
            .with_collection(Collection::Email)
            .update_document(message_id)
            .assert_value(ValueClass::Property(Property::MailboxIds.into()), &uids)
            .value(Property::MailboxIds, uids.inner, F_VALUE);
        server
            .write_batch(batch)
            .await
            .caused_by(trc::location!())?;
        email_count += 1;
    }

    Ok((mailbox_count, email_count))
}
