//! The wasmtime store provisioning shared by the driver's child modes:
//! the store data (WASI + runner context + WebRTC host), the native HTTP
//! mailbox host, and the two linker profiles (hosted and composed).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[allow(unused_imports)]
use anyhow::Result;
use component_test_runner::{CtCtx, RunnerView};
use wasmtime::component::{Accessor, Linker, Resource, ResourceTable};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::p3::{WasiHttpCtxView, WasiHttpView};
use wasmtime_wasi_http::WasiHttpCtx;
use wasmtime_webrtc_datachannels::{self as webrtc_host, WebrtcCtx, WebrtcCtxView, WebrtcView};

mod bindings {
    wasmtime::component::bindgen!({
        path: "../wit",
        world: "sut-imports",
        imports: {
            default: async | store | trappable,
        },
        with: {
            "conformance:signaling/mailbox.session": crate::host::MailboxSession,
        },
    });
}

use bindings::conformance::signaling::mailbox::{self, Role as MailboxRole};
use bindings::polymorph::webrtc_datachannels::types::Error;

/// The inbound-buffer bound every leg applies (and exports to the suite
/// through `WEBRTC_MAX_INBOUND_BUFFER_BYTES`): small enough that the
/// overflow probe's 1 MiB flood overflows it.
pub const MAX_INBOUND_BUFFER_BYTES: usize = 512 * 1024;

/// Per-store host state: WASI (the suite reads its config from the
/// environment), the runner's diagnostic sink, and the WebRTC host
/// (unused by composed runs, where the provider is in the artifact).
pub struct Data {
    wasi: WasiCtx,
    table: ResourceTable,
    ct: CtCtx,
    webrtc: WebrtcCtx,
    http: WasiHttpCtx,
}

impl WasiView for Data {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl RunnerView for Data {
    fn ct(&mut self) -> &mut CtCtx {
        &mut self.ct
    }
}

impl WebrtcView for Data {
    fn webrtc(&mut self) -> WebrtcCtxView<'_> {
        WebrtcCtxView {
            ctx: &mut self.webrtc,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for Data {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            hooks: wasmtime_wasi_http::p3::default_hooks(),
            ctx: &mut self.http,
            table: &mut self.table,
        }
    }
}

/// The environment a child instance hands the suite (role/signaling for
/// pair runs; the buffer bound always), plus the lab-provided ICE
/// profile when the peer runs on a routed, non-loopback path.
pub struct SuiteEnv {
    pub role: Option<String>,
    pub signaling_url: Option<String>,
    pub run_id: Option<String>,
    pub composed: bool,
    pub ice: Option<IceProfile>,
}

/// A lab scenario's network shape: an explicit bind address (the
/// simulated or namespaced interface), optional STUN/TURN, relay-only,
/// and whether mDNS gathering must be off (Shadow's simulated stack
/// lacks the multicast socket options it binds with).
#[derive(Clone, Default)]
pub struct IceProfile {
    pub bind_addr: Option<String>,
    pub stun_url: Option<String>,
    pub turn: Option<(String, String, String)>,
    pub relay_only: bool,
    pub disable_mdns: bool,
}

/// Build the per-instance store data. Hosted runs restrict ICE to
/// loopback so two same-host peers pair deterministically (peers bind
/// on IPv4 loopback, so only loopback candidates are gathered, and the
/// setting-engine hook keeps them rather than discarding them).
/// Composed runs get real network access instead: the in-artifact
/// provider serves `connections` over `wasi:sockets` UDP loopback.
pub fn make_data(env: &SuiteEnv) -> Data {
    let mut webrtc = WebrtcCtx::new();
    match &env.ice {
        None => {
            webrtc.set_setting_engine_hook(|engine| engine.with_include_loopback_candidate(true));
        }
        // Lab peers connect over real interface addresses: loopback
        // candidates are not forced (a never-connectable pair), and
        // mDNS is disabled where the environment cannot serve it.
        Some(ice) => {
            let mut servers = Vec::new();
            if let Some(url) = &ice.stun_url {
                servers.push(wasmtime_webrtc_datachannels::WebrtcIceServer {
                    urls: vec![url.clone()],
                    username: String::new(),
                    credential: String::new(),
                });
            }
            if let Some((url, user, pass)) = &ice.turn {
                servers.push(wasmtime_webrtc_datachannels::WebrtcIceServer {
                    urls: vec![url.clone()],
                    username: user.clone(),
                    credential: pass.clone(),
                });
            }
            webrtc.set_ice_config(wasmtime_webrtc_datachannels::WebrtcIceConfig {
                udp_addrs: ice.bind_addr.iter().map(|a| format!("{a}:0")).collect(),
                ice_servers: servers,
                relay_only: ice.relay_only,
            });
            if ice.disable_mdns {
                webrtc.set_setting_engine_hook(|engine| {
                    engine.with_multicast_dns_mode(rtc::ice::mdns::MulticastDnsMode::Disabled)
                });
            }
        }
    }
    webrtc.set_max_inbound_buffer_bytes(MAX_INBOUND_BUFFER_BYTES);

    let mut wasi = WasiCtxBuilder::new();
    wasi.inherit_stderr().env(
        "WEBRTC_MAX_INBOUND_BUFFER_BYTES",
        MAX_INBOUND_BUFFER_BYTES.to_string(),
    );
    if let Some(role) = &env.role {
        wasi.env("RTC_CT_ROLE", role);
    }
    if let Some(url) = &env.signaling_url {
        wasi.env("RTC_CT_SIGNALING_URL", url);
    }
    if let Some(id) = &env.run_id {
        wasi.env("RTC_CT_RUN_ID", id);
    }
    if env.composed {
        wasi.inherit_network().allow_ip_name_lookup(true);
        // The in-guest provider binds (and derives its host candidate
        // from) this address; loopback default when unset.
        if let Some(ice) = &env.ice {
            if let Some(addr) = &ice.bind_addr {
                wasi.env("WEBRTC_UDP_BIND_ADDR", addr);
            }
        }
    }

    Data {
        wasi: wasi.build(),
        table: ResourceTable::new(),
        ct: CtCtx::default(),
        webrtc,
        http: WasiHttpCtx::new(),
    }
}

/// Wire the suite's imports: the WebRTC host + the native mailbox for
/// hosted runs; WASI p3 (sockets for the in-artifact provider, http for
/// the in-artifact mailbox client) for composed runs.
pub fn configure_linker(linker: &mut Linker<Data>, composed: bool) -> wasmtime::Result<()> {
    if composed {
        wasmtime_wasi::p3::add_to_linker(linker)?;
        wasmtime_wasi_http::p3::add_to_linker(linker)?;
    } else {
        webrtc_host::add_to_linker(linker)?;
        mailbox::add_to_linker::<Data, Data>(linker, |c| c)?;
    }
    Ok(())
}

// ----- mailbox host -----------------------------------------------------------

/// A joined mailbox session: an HTTP client bound to one `{room}` and `{role}`
/// on the signaling server. `Arc`-backed so a handle can be cloned out of the
/// resource table and its async methods driven without holding the store borrow
/// across `.await`.
#[derive(Clone)]
pub struct MailboxSession {
    client: reqwest::Client,
    base: String,
    room: String,
    role: MailboxRole,
    /// The next sequence number to fetch from the peer's mailbox.
    recv_seq: Arc<AtomicUsize>,
}

impl MailboxSession {
    fn own_role(&self) -> &'static str {
        role_str(self.role)
    }

    /// The peer's role path segment (the mailbox this session consumes).
    fn peer_role(&self) -> &'static str {
        match self.role {
            MailboxRole::Offerer => "answerer",
            MailboxRole::Answerer => "offerer",
        }
    }
}

fn role_str(role: MailboxRole) -> &'static str {
    match role {
        MailboxRole::Offerer => "offerer",
        MailboxRole::Answerer => "answerer",
    }
}

/// Map any host-side mailbox failure to the guest-visible `error.other`.
fn mailbox_error(detail: impl std::fmt::Display) -> Error {
    Error::Other(format!("mailbox: {detail}"))
}

impl wasmtime::component::HasData for Data {
    type Data<'a> = &'a mut Self;
}

impl mailbox::Host for Data {}

impl mailbox::HostSession for Data {}

impl mailbox::HostSessionWithStore<Data> for Data {
    async fn open(
        accessor: &Accessor<Data, Data>,
        server: String,
        room: String,
        as_role: MailboxRole,
    ) -> wasmtime::Result<std::result::Result<Resource<MailboxSession>, Error>> {
        let session = MailboxSession {
            client: reqwest::Client::new(),
            base: server.trim_end_matches('/').to_string(),
            room,
            role: as_role,
            recv_seq: Arc::new(AtomicUsize::new(0)),
        };
        accessor.with(|mut access| {
            let resource = access.get().table.push(session)?;
            Ok(Ok(resource))
        })
    }

    async fn send(
        accessor: &Accessor<Data, Data>,
        self_: Resource<MailboxSession>,
        blob: Vec<u8>,
    ) -> wasmtime::Result<std::result::Result<(), Error>> {
        let session = accessor
            .with(|mut access| Ok::<_, wasmtime::Error>(access.get().table.get(&self_)?.clone()))?;
        let url = format!(
            "{}/rooms/{}/{}",
            session.base,
            session.room,
            session.own_role()
        );
        Ok(match session.client.post(&url).body(blob).send().await {
            Ok(resp) if resp.status().is_success() => Ok(()),
            Ok(resp) => Err(mailbox_error(format!("publish status {}", resp.status()))),
            Err(err) => Err(mailbox_error(err)),
        })
    }

    async fn recv(
        accessor: &Accessor<Data, Data>,
        self_: Resource<MailboxSession>,
    ) -> wasmtime::Result<std::result::Result<Option<Vec<u8>>, Error>> {
        let session = accessor
            .with(|mut access| Ok::<_, wasmtime::Error>(access.get().table.get(&self_)?.clone()))?;
        Ok(fetch_next(&session).await)
    }

    async fn done(
        accessor: &Accessor<Data, Data>,
        self_: Resource<MailboxSession>,
    ) -> wasmtime::Result<std::result::Result<(), Error>> {
        let session = accessor
            .with(|mut access| Ok::<_, wasmtime::Error>(access.get().table.get(&self_)?.clone()))?;
        let url = format!(
            "{}/rooms/{}/{}/done",
            session.base,
            session.room,
            session.own_role()
        );
        Ok(match session.client.post(&url).send().await {
            Ok(resp) if resp.status().is_success() => Ok(()),
            Ok(resp) => Err(mailbox_error(format!("done status {}", resp.status()))),
            Err(err) => Err(mailbox_error(err)),
        })
    }

    async fn drop(
        accessor: &Accessor<Data, Data>,
        rep: Resource<MailboxSession>,
    ) -> wasmtime::Result<()> {
        accessor.with(|mut access| {
            access.get().table.delete(rep)?;
            Ok(())
        })
    }
}

/// Fetch the next blob from the peer's mailbox, long-polling and retrying `304`
/// until a blob arrives (`some`) or the peer marks its mailbox done (`none`).
async fn fetch_next(session: &MailboxSession) -> std::result::Result<Option<Vec<u8>>, Error> {
    loop {
        let seq = session.recv_seq.load(Ordering::SeqCst);
        let url = format!(
            "{}/rooms/{}/{}?seq={}&wait=10000",
            session.base,
            session.room,
            session.peer_role(),
            seq
        );
        let resp = session
            .client
            .get(&url)
            .send()
            .await
            .map_err(mailbox_error)?;
        match resp.status().as_u16() {
            // A blob is available: advance our read cursor and return it.
            200 => {
                let bytes = resp.bytes().await.map_err(mailbox_error)?.to_vec();
                session.recv_seq.store(seq + 1, Ordering::SeqCst);
                return Ok(Some(bytes));
            }
            // The peer marked its mailbox done at or before this seq.
            204 => return Ok(None),
            // Not yet available; retry the same seq.
            304 => continue,
            other => return Err(mailbox_error(format!("fetch status {other}"))),
        }
    }
}
