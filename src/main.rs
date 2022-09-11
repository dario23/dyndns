use std::collections::BTreeMap;
use std::str::FromStr;
use std::time::Duration;
use std::sync::Arc;

use clap::Parser;

use tokio::{runtime, net::{TcpListener, UdpSocket}};
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

    let authority = InMemoryAuthority::new(name.clone(), records, ZoneType::Primary, false)
        .expect("cannot create InMemoryAuthority");
    let mut catalog = Catalog::new();
    catalog.upsert(name.clone().into(), Box::new(Arc::new(authority)));

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

    eprintln!("Serving via TCP+UDP on {}", opts.dns_listen_addr);
    tokio_runtime.block_on(dns_server.block_until_done()).expect("runtime error");
}
