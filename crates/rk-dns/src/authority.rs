//! Custom hickory-dns authority.
//!
//! Serves DNS records from our in-memory RecordStore,
//! with fallback forwarding for external queries.

use crate::records::RecordStore;
use async_trait::async_trait;
use hickory_proto::op::ResponseCode;
use hickory_proto::rr::rdata::{A, PTR, SRV};
use hickory_proto::rr::{LowerName, Name, RData, Record, RecordType};
use hickory_server::authority::{
    Authority, LookupControlFlow, LookupError, LookupObject, LookupOptions, MessageRequest,
    UpdateResult, ZoneType,
};
use hickory_server::server::RequestInfo;
use std::sync::Arc;
use tracing::debug;

/// A hickory-dns authority backed by our RecordStore.
pub struct ClusterAuthority {
    origin: LowerName,
    store: Arc<RecordStore>,
}

impl ClusterAuthority {
    pub fn new(domain: &str, store: Arc<RecordStore>) -> Self {
        let origin = Name::from_utf8(domain)
            .unwrap_or_else(|_| Name::from_utf8("cluster.local").unwrap())
            .into();

        Self { origin, store }
    }
}

/// A simple lookup result that holds records.
pub struct RecordLookup {
    records: Vec<Record>,
}

impl RecordLookup {
    fn new(records: Vec<Record>) -> Self {
        Self { records }
    }

    fn empty() -> Self {
        Self {
            records: Vec::new(),
        }
    }
}

impl LookupObject for RecordLookup {
    fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    fn iter<'a>(&'a self) -> Box<dyn Iterator<Item = &'a Record> + Send + 'a> {
        Box::new(self.records.iter())
    }

    fn take_additionals(&mut self) -> Option<Box<dyn LookupObject>> {
        None
    }
}

#[async_trait]
impl Authority for ClusterAuthority {
    type Lookup = RecordLookup;

    fn zone_type(&self) -> ZoneType {
        ZoneType::Primary
    }

    fn is_axfr_allowed(&self) -> bool {
        false
    }

    async fn update(&self, _update: &MessageRequest) -> UpdateResult<bool> {
        Err(ResponseCode::Refused)
    }

    fn origin(&self) -> &LowerName {
        &self.origin
    }

    async fn lookup(
        &self,
        name: &LowerName,
        rtype: RecordType,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<Self::Lookup> {
        let query_name = name.to_string();
        // Strip trailing dot if present
        let query_name = query_name.trim_end_matches('.');

        debug!("DNS lookup: {query_name} {rtype}");

        let result = match rtype {
            RecordType::A => {
                let ips = self.store.lookup_a(query_name);
                if ips.is_empty() {
                    Err(LookupError::from(ResponseCode::NXDomain))
                } else {
                    let records: Vec<Record> = ips
                        .into_iter()
                        .map(|ip| {
                            Record::from_rdata(
                                Name::from(name.clone()),
                                self.store.ttl,
                                RData::A(A(ip)),
                            )
                        })
                        .collect();
                    Ok(RecordLookup::new(records))
                }
            }

            RecordType::SRV => {
                let srv_records = self.store.lookup_srv(query_name);
                if srv_records.is_empty() {
                    Err(LookupError::from(ResponseCode::NXDomain))
                } else {
                    let records: Vec<Record> = srv_records
                        .into_iter()
                        .filter_map(|(target, port, priority, weight)| {
                            let target_name = Name::from_utf8(&target).ok()?;
                            Some(Record::from_rdata(
                                Name::from(name.clone()),
                                self.store.ttl,
                                RData::SRV(SRV::new(priority, weight, port, target_name)),
                            ))
                        })
                        .collect();
                    Ok(RecordLookup::new(records))
                }
            }

            RecordType::PTR => {
                if let Some(target) = self.store.lookup_ptr(query_name) {
                    match Name::from_utf8(&target) {
                        Ok(target_name) => Ok(RecordLookup::new(vec![Record::from_rdata(
                            Name::from(name.clone()),
                            self.store.ttl,
                            RData::PTR(PTR(target_name)),
                        )])),
                        Err(_) => Err(LookupError::from(ResponseCode::ServFail)),
                    }
                } else {
                    Err(LookupError::from(ResponseCode::NXDomain))
                }
            }

            _ => Err(LookupError::from(ResponseCode::NXDomain)),
        };

        LookupControlFlow::Continue(result)
    }

    async fn search(
        &self,
        request_info: RequestInfo<'_>,
        lookup_options: LookupOptions,
    ) -> LookupControlFlow<Self::Lookup> {
        let name = request_info.query.name();
        let rtype = request_info.query.query_type();
        self.lookup(name, rtype, lookup_options).await
    }

    async fn get_nsec_records(
        &self,
        _name: &LowerName,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<Self::Lookup> {
        LookupControlFlow::Continue(Ok(RecordLookup::empty()))
    }
}
