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

use tokio::{runtime, net::{TcpListener, UdpSocket}};
use trust_dns_server::client::rr::RecordSet;
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

#[derive(Clone)]
struct SynchronizedState {
    authority: Arc<InMemoryAuthority>,
}

impl SynchronizedState {
    pub fn new(authority: Arc<InMemoryAuthority>) -> Self {
        Self {
            authority
        }
    }
}

#[derive(Clone)]
struct AppConfig {
    base_dns_name: Name,
}


fn make_authority(name: &Name, records: BTreeMap<RrKey, RecordSet>) -> InMemoryAuthority {
    InMemoryAuthority::new(name.clone(), records, ZoneType::Primary, false)
        .expect("cannot create InMemoryAuthority")
}


fn main() {
    let opts = DynDnsServerConfig::parse();

    let name = Name::from_str("d.q0b.de.").expect("cannot parse name");
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

    let authority = Arc::new(make_authority(&name, records));
    let mut catalog = Catalog::new();
    catalog.upsert(name.clone().into(), Box::new(Arc::clone(&authority)));

    // NICE: we can keep a reference to the authority even if we gave it to the catalog for serving, i.e. we can update it from the other thread :-)
    // let foo = authority.lookup(&name.clone().into(), RecordType::A, LookupOptions::default());

    let mut dns_server = ServerFuture::new(catalog);

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
        .layer(Extension(SynchronizedState::new(authority)))
        .layer(Extension(AppConfig { base_dns_name: name }));

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

async fn update_own_ipaddr(state: Extension<SynchronizedState>, app_config: Extension<AppConfig>, client_addr: ConnectInfo<SocketAddr>) -> impl IntoResponse {
    let name = Name::from_str("foo")
        .expect("parse foo")
        .append_domain(&app_config.base_dns_name)
        .expect("append domain");

    let record = Record::from_rdata(
        name,
        1800,
        match client_addr_family(&client_addr.0) {
            V46::V4(v4) => RData::A(v4),
            V46::V6(v6) => RData::AAAA(v6),
        },
    );

    state
        .authority
        .upsert(record, 42)
        .await;

    "ok"
}