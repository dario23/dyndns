#![feature(ip)]

use std::net::{SocketAddr, Ipv4Addr, Ipv6Addr};
use std::{collections::BTreeMap};
use std::str::FromStr;
use std::time::Duration;
use std::sync::Arc;

use axum::response::IntoResponse;
use axum::{
    extract::ConnectInfo,
    Extension,
    Router,
    routing::{get, post},
};
use clap::Parser;

use tokio::{runtime, net::{TcpListener, UdpSocket}, sync::Mutex};
use trust_dns_server::client::rr::RecordSet;
use trust_dns_server::server::{RequestHandler, ResponseHandler, Request, ResponseInfo};
use trust_dns_server::{
    authority::{Catalog, ZoneType},
    store::in_memory::InMemoryAuthority,
    ServerFuture,
    proto::rr::Name,
    client::rr::{RecordType, RData, rdata::SOA, Record, RrKey},
};

#[derive(Debug, Parser)]
#[clap(name = "dyndns", version = "0.1.0")]
struct DynDnsServerConfig {
    #[clap(long, short = 'd')]
    pub dns_listen_addr: String,

    #[clap(long, short = 'p')]
    pub http_listen_addr: String,
}

struct SynchronizedState {
    catalog: Arc<Mutex<Catalog>>,
}

impl SynchronizedState {
    pub fn new(catalog: Catalog) -> Self {
        Self {
            catalog: Arc::new(Mutex::new(catalog)),
        }
    }
}

impl Clone for SynchronizedState {
    fn clone(&self) -> Self {
        Self {
            catalog: self.catalog.clone()
        }
    }
}

#[async_trait::async_trait]
impl RequestHandler for SynchronizedState {
    async fn handle_request<R: ResponseHandler>(&self, request: &Request, response_handle: R) -> ResponseInfo {
        self.catalog
            .lock()
            .await
            .handle_request(request, response_handle)
            .await
    }
}

fn make_authority(name: &Name, records: BTreeMap<RrKey, RecordSet>) -> InMemoryAuthority {
    InMemoryAuthority::new(name.clone(), records, ZoneType::Primary, false)
        .expect("cannot create InMemoryAuthority")
}

fn main() {
    let opts = DynDnsServerConfig::parse();

    let name = Name::from_str("d.q0b.de").expect("cannot parse name");
    let mut records = BTreeMap::new();
    records.insert(
        RrKey::new(name.clone().into(), RecordType::SOA),
        Record::from_rdata(
            name.clone(),
            0,
            RData::SOA(SOA::new(name.clone(), name.clone(), 0, 0, 0, 0, 0)),
        )
        .into(),
    );

    let authority = make_authority(&name, records);
    let mut catalog = Catalog::new();
    catalog.upsert(name.clone().into(), Box::new(Arc::new(authority)));

    let dns_state_handle = SynchronizedState::new(catalog);
    let http_state_handle = dns_state_handle.clone();

    let mut dns_server = ServerFuture::new(dns_state_handle);

    // NEXT TODO: this line doesn't work, because ServerFuture wants ownership of the handler
    // catalog.lookup(todo!(), todo!(), ResponseHandle::new(todo!(), todo!()));
    //
    // IDEA: create a wrapper type that has e.g. a Arc<tokio::Mutex<Catalog>>
    // member and implement the handler stuff for that by locking+passing
    // through, so we can keep a reference out here for the http handler, which
    // can then update zone info as it gets new data.

    // next-next TODO: figure out how to properly use the sqlite backend instead
    // of the in-memory one, so we don't loose data on restart (esp. DNS serials..)

    let tokio_runtime = runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    let _guard = tokio_runtime.enter();

    let tcp_listener = tokio_runtime.block_on(TcpListener::bind(&opts.dns_listen_addr)).expect("listen TCP");
    dns_server.register_listener(tcp_listener, Duration::from_secs(10));
    let udp_listener = tokio_runtime.block_on(UdpSocket::bind(&opts.dns_listen_addr)).expect("bind UDP");
    dns_server.register_socket(udp_listener);

    let http_app = Router::new()
        .route("/", get(get_own_ipaddr))
        .route("/", post(update_own_ipaddr))
        .layer(Extension(http_state_handle));

    let http_listen_addr = opts.http_listen_addr.parse().expect("HTTP listen address malformed");
    let http_server = axum::Server::bind(&http_listen_addr)
        .serve(http_app.into_make_service_with_connect_info::<SocketAddr>());

    eprintln!("Serving via TCP+UDP on {}, HTTP on {}", opts.dns_listen_addr, opts.http_listen_addr);

    tokio_runtime.block_on(async {
        // TODO: actually use the return values here?
        let (_, _) = tokio::join!(dns_server.block_until_done(), http_server);
    });
}


enum V46 {
    V4(Ipv4Addr),
    V6(Ipv6Addr),
}

fn client_addr_family(addr: &SocketAddr) -> V46 {
    match addr {
        SocketAddr::V4(v4addr) => V46::V4(*v4addr.ip()),
        SocketAddr::V6(v6) => {
            if let Some(v4addr) = v6.ip().to_ipv4_mapped() {
                V46::V4(v4addr)
            } else {
                V46::V6(*v6.ip())
            }
        }
    }
}

async fn get_own_ipaddr(client_addr: ConnectInfo<SocketAddr>) -> String {
    match client_addr_family(&client_addr.0) {
        V46::V4(v4) => format!(r#"{{ "ipv4": "{}" }}"#, v4),
        V46::V6(v6) => format!(r#"{{ "ipv6": "{}" }}"#, v6),
    }
}

async fn update_own_ipaddr(state: Extension<SynchronizedState>, client_addr: ConnectInfo<SocketAddr>) -> impl IntoResponse {
    let name = Name::from_str("d.q0b.de").expect("cannot parse name");

    let mut new_records = BTreeMap::new();

    // always need a SOA entry!
    // TODO: de-duplicate with above/somehow not recreate the whole thing
    new_records.insert(
        RrKey::new(name.clone().into(), RecordType::SOA),
        Record::from_rdata(
            name.clone(),
            0,
            RData::SOA(SOA::new(name.clone(), name.clone(), 0, 0, 0, 0, 0)),
        )
        .into(),
    );

    match client_addr_family(&client_addr.0) {
        V46::V4(v4) => {
            new_records.insert(
                RrKey::new(name.clone().into(), RecordType::A),
                Record::from_rdata(
                    name.clone(),
                    0,
                    RData::A(v4)
                ).into(),
            );
            eprintln!("updated ipv4 addr to {}", v4);
        }
        V46::V6(v6) => {
            new_records.insert(
                RrKey::new(name.clone().into(), RecordType::AAAA),
                Record::from_rdata(
                    name.clone(),
                    0,
                    RData::AAAA(v6)
                ).into()
            );
            eprintln!("updated ipv6 addr to {}", v6);
        }
    }

    let new_authority = make_authority(&name.clone(), new_records);
    state.catalog
        .lock()
        .await
        .upsert(name.into(), Box::new(Arc::new(new_authority)));
}

    /*
    let previous_data = catalog.find(&name.clone().into());

    let new_records = if let Some(previous_data) = previous_data {
        let mut previous_entries = BTreeMap::new();
        for record in previous_data.lookup(&name.clone().into(), RecordType::ANY, LookupOptions::default()).await.unwrap().iter() {
            record.data()
        }
    } else {
        BTreeMap::new()
    };
    let new_authority = make_authority(&name, new_records);
    catalog.upsert(name.into(), Box::new(Arc::new(new_authority)))
 */

 /*
    let previous_data = catalog.find(&name.clone().into()).unwrap();
    match (&previous_data as &dyn Any).downcast_ref::<InMemoryAuthority>() {
        Some(in_memory_authority) => {
            let record = Record::from_rdata(
                name.clone(),
                0,
                RData::SOA(SOA::new(name.clone(), name.clone(), 0, 0, 0, 0, 0))
            );
            in_memory_authority.upsert(record, 42).await;
            Ok("success")
        },
        None => return Err("internal error"),
    }
 */