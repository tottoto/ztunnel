// Copyright Istio Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use futures::stream::StreamExt;

use http::{Method, Response, StatusCode};

use tokio::sync::watch;

use tracing::{debug, info, instrument, trace_span, Instrument};

use super::{Error, LocalWorkloadInformation};
use crate::baggage::parse_baggage_header;
use crate::identity::Identity;

use crate::drain::DrainWatcher;
use crate::proxy::h2::server::H2Request;
use crate::proxy::metrics::{ConnectionOpen, Reporter};
use crate::proxy::{metrics, ProxyInputs, TraceParent, BAGGAGE_HEADER, TRACEPARENT_HEADER};
use crate::rbac::Connection;
use crate::socket::to_canonical;
use crate::state::service::Service;
use crate::state::workload::application_tunnel::Protocol as AppProtocol;
use crate::{assertions, copy, proxy, socket, strng, tls};

use crate::config::ProxyMode;
use crate::drain::run_with_drain;
use crate::proxy::h2;
use crate::state::workload::address::Address;
use crate::state::workload::{self, NetworkAddress, Workload};
use crate::state::DemandProxyState;
use crate::tls::TlsError;

pub(super) struct Inbound {
    listener: socket::Listener,
    drain: DrainWatcher,
    pi: Arc<ProxyInputs>,
    enable_orig_src: bool,
}

impl Inbound {
    pub(super) async fn new(pi: Arc<ProxyInputs>, drain: DrainWatcher) -> Result<Inbound, Error> {
        let listener = pi
            .socket_factory
            .tcp_bind(pi.cfg.inbound_addr)
            .map_err(|e| Error::Bind(pi.cfg.inbound_addr, e))?;
        let enable_orig_src = super::maybe_set_transparent(&pi, &listener)?;

        info!(
            address=%listener.local_addr(),
            component="inbound",
            transparent=enable_orig_src,
            "listener established",
        );
        Ok(Inbound {
            listener,
            drain,
            pi,
            enable_orig_src,
        })
    }

    pub(super) fn address(&self) -> SocketAddr {
        self.listener.local_addr()
    }

    pub(super) async fn run(self) {
        let pi = self.pi.clone();
        let acceptor = InboundCertProvider {
            local_workload: self.pi.local_workload_information.clone(),
        };

        // Safety: we set nodelay directly in tls_server, so it is safe to convert to a normal listener.
        // Although, that is *after* the TLS handshake; in theory we may get some benefits to setting it earlier.
        let mut stream = crate::hyper_util::tls_server(acceptor, self.listener.inner());

        let accept = |drain: DrainWatcher, force_shutdown: watch::Receiver<()>| {
            async move {
                while let Some(tls) = stream.next().await {
                    let pi = self.pi.clone();
                    let (raw_socket, ssl) = tls.get_ref();
                    let src_identity: Option<Identity> = tls::identity_from_connection(ssl);
                    let dst = crate::socket::orig_dst_addr_or_default(raw_socket);
                    let src = to_canonical(raw_socket.peer_addr().expect("peer_addr available"));
                    let drain = drain.clone();
                    let force_shutdown = force_shutdown.clone();
                    let network = pi.cfg.network.clone();
                    let serve_client = async move {
                        let conn = Connection {
                            src_identity,
                            src,
                            dst_network: strng::new(&network), // inbound request must be on our network
                            dst,
                        };
                        debug!(%conn, "accepted connection");
                        let cfg = pi.cfg.clone();
                        let request_handler = move |req| {
                            Self::serve_connect(pi.clone(), conn.clone(), self.enable_orig_src, req)
                        };
                        let serve = Box::pin(h2::server::serve_connection(
                            cfg,
                            tls,
                            drain,
                            force_shutdown,
                            request_handler,
                        ));
                        serve.await
                    };
                    assertions::size_between_ref(1000, 1500, &serve_client);
                    tokio::task::spawn(serve_client.in_current_span());
                }
            }
            .in_current_span()
        };

        run_with_drain(
            "inbound".to_string(),
            self.drain,
            pi.cfg.self_termination_deadline,
            accept,
        )
        .await
    }

    fn extract_traceparent(req: &H2Request) -> TraceParent {
        req.headers()
            .get(TRACEPARENT_HEADER)
            .and_then(|b| b.to_str().ok())
            .and_then(|b| TraceParent::try_from(b).ok())
            .unwrap_or_else(TraceParent::new)
    }

    #[allow(clippy::too_many_arguments)]
    #[instrument(name="inbound", skip_all, fields(
        id=%Self::extract_traceparent(&req),
        peer=%conn.src,
    ))]
    async fn serve_connect(
        pi: Arc<ProxyInputs>,
        conn: Connection,
        enable_original_source: bool,
        req: H2Request,
    ) -> Result<(), Error> {
        if req.method() != Method::CONNECT {
            metrics::log_early_deny(
                conn.src,
                conn.dst,
                Reporter::destination,
                Error::NonConnectMethod(req.method().to_string()),
            );
            return req.send_error(build_response(StatusCode::NOT_FOUND));
        }
        let start = Instant::now();
        let Ok(hbone_addr) = req.uri().to_string().as_str().parse::<SocketAddr>() else {
            metrics::log_early_deny(
                conn.src,
                conn.dst,
                Reporter::destination,
                Error::ConnectAddress(req.uri().to_string()),
            );
            return req.send_error(build_response(StatusCode::BAD_REQUEST));
        };

        let destination_workload = pi.local_workload_information.get_workload().await?;

        if let Err(e) =
            Self::validate_destination(&pi.state, &conn, &destination_workload, hbone_addr).await
        {
            metrics::log_early_deny(conn.src, conn.dst, Reporter::destination, e);
            return req.send_error(build_response(StatusCode::BAD_REQUEST));
        }

        // Determine the next hop.
        let (upstream_addr, inbound_protocol, upstream_service) =
            Self::find_inbound_upstream(&pi.state, &conn, &destination_workload, hbone_addr);
        let illegal_call = if pi.cfg.proxy_mode == ProxyMode::Shared {
            // User sent a request to pod:15006. This would forward to pod:15006 infinitely
            // Use hbone_addr instead of upstream_addr to allow for sandwich mode, which intentionally
            // sends to 15008.
            pi.cfg.illegal_ports.contains(&hbone_addr.port())
        } else {
            false // TODO: do we need any check here?
        };
        if illegal_call {
            metrics::log_early_deny(
                conn.src,
                upstream_addr,
                Reporter::destination,
                Error::SelfCall,
            );
            return req.send_error(build_response(StatusCode::BAD_REQUEST));
        }
        let original_dst = conn.dst;
        // Connection has 15008, swap with the real port
        let conn = Connection {
            dst: upstream_addr,
            ..conn
        };
        let from_gateway = proxy::check_from_network_gateway(
            &pi.state,
            &destination_workload,
            conn.src_identity.as_ref(),
        )
        .await;

        if from_gateway {
            debug!("request from gateway");
        }

        let rbac_ctx = crate::state::ProxyRbacContext {
            conn,
            dest_workload: destination_workload.clone(),
        };

        let source_ip = rbac_ctx.conn.src.ip();

        let for_host = parse_forwarded_host(&req);
        let baggage =
            parse_baggage_header(req.headers().get_all(BAGGAGE_HEADER)).unwrap_or_default();

        let source = match from_gateway {
            true => None, // we cannot lookup source workload since we don't know the network, see https://github.com/istio/ztunnel/issues/515
            false => {
                let src_network_addr = NetworkAddress {
                    // we can assume source network is our network because we did not traverse a gateway
                    network: rbac_ctx.conn.dst_network.clone(),
                    address: source_ip,
                };
                // Find source info. We can lookup by XDS or from connection attributes
                pi.state.fetch_workload_by_address(&src_network_addr).await
            }
        };

        let derived_source = metrics::DerivedWorkload {
            identity: rbac_ctx.conn.src_identity.clone(),
            cluster_id: baggage.cluster_id,
            namespace: baggage.namespace,
            workload_name: baggage.workload_name,
            revision: baggage.revision,
            ..Default::default()
        };
        let ds = proxy::guess_inbound_service(
            &rbac_ctx.conn,
            &for_host,
            upstream_service,
            &destination_workload,
        );
        let result_tracker = Box::new(metrics::ConnectionResult::new(
            rbac_ctx.conn.src,
            // For consistency with outbound logs, report the original destination (with 15008 port)
            // as dst.addr, and the target address as dst.hbone_addr
            original_dst,
            Some(hbone_addr),
            start,
            ConnectionOpen {
                reporter: Reporter::destination,
                source,
                derived_source: Some(derived_source),
                destination: Some(destination_workload),
                connection_security_policy: metrics::SecurityPolicy::mutual_tls,
                destination_service: ds,
            },
            pi.metrics.clone(),
        ));

        let conn_guard = match pi
            .connection_manager
            .assert_rbac(&pi.state, &rbac_ctx, for_host)
            .await
        {
            Ok(cg) => cg,
            Err(e) => {
                result_tracker
                    .record_with_flag(Err(e), metrics::ResponseFlags::AuthorizationPolicyDenied);
                return req.send_error(build_response(StatusCode::UNAUTHORIZED));
            }
        };

        let orig_src = enable_original_source.then_some(source_ip);
        let stream =
            super::freebind_connect(orig_src, upstream_addr, pi.socket_factory.as_ref()).await;
        let mut stream = match stream {
            Err(err) => {
                result_tracker.record(Err(err));
                return req.send_error(build_response(StatusCode::SERVICE_UNAVAILABLE));
            }
            Ok(stream) => stream,
        };

        debug!("connected to: {upstream_addr}");

        let h2_stream = req.send_response(build_response(StatusCode::OK)).await?;

        let send = async {
            if inbound_protocol == AppProtocol::PROXY {
                let Connection {
                    src, src_identity, ..
                } = rbac_ctx.conn;
                super::write_proxy_protocol(&mut stream, (src, hbone_addr), src_identity)
                    .instrument(trace_span!("proxy protocol"))
                    .await?;
            }
            copy::copy_bidirectional(h2_stream, copy::TcpStreamSplitter(stream), &result_tracker)
                .instrument(trace_span!("hbone server"))
                .await
        };
        let res = conn_guard.handle_connection(send).await;
        result_tracker.record(res);
        Ok(())
    }

    /// validate_destination ensures the destination is an allowed request.
    async fn validate_destination(
        state: &DemandProxyState,
        conn: &Connection,
        local_workload: &Workload,
        hbone_addr: SocketAddr,
    ) -> Result<(), Error> {
        if conn.dst.ip() == hbone_addr.ip() {
            // Normal case: both are aligned. This is allowed (we really only need the HBONE address for the port.)
            return Ok(());
        }
        if local_workload.application_tunnel.is_some() {
            // In the case they have their own tunnel, they will get the HBONE target address in the PROXY
            // header, and their application can decide what to do with it; we don't validate this.
            // This is the case, for instance, with a waypoint using PROXY.
            return Ok(());
        }
        // There still may be the case where we are doing a "waypoint sandwich" but not using any tunnel.
        // Presumably, the waypoint is only matching on L7 attributes.
        // We want to make sure in this case we don't deny the requests just because the HBONE destination
        // mismatches (though we will, essentially, ignore the address).
        // To do this, we do a lookup to see if the HBONE target has us as its waypoint.
        let hbone_dst = &NetworkAddress {
            network: conn.dst_network.clone(),
            address: hbone_addr.ip(),
        };

        // None means we need to do on-demand lookup
        let lookup_is_destination_this_waypoint = || -> Option<bool> {
            let state = state.read();

            // TODO Allow HBONE address to be a hostname. We have to respect rules about
            // hostname scoping. Can we use the client's namespace here to do that?
            let hbone_target = state.find_address(hbone_dst)?;

            // HBONE target can point to some service or workload. In either case, get the waypoint
            let Some(target_waypoint) = (match hbone_target {
                Address::Service(ref svc) => &svc.waypoint,
                Address::Workload(ref wl) => &wl.waypoint,
            }) else {
                // Target has no waypoint
                return Some(false);
            };

            // Resolve the reference from our HBONE target
            let Some(target_waypoint) = state.find_destination(&target_waypoint.destination) else {
                return Some(false);
            };

            // Validate that the HBONE target references the Waypoint we're connecting to
            Some(match target_waypoint {
                Address::Service(svc) => svc.contains_endpoint(local_workload),
                Address::Workload(wl) => wl.workload_ips.contains(&conn.dst.ip()),
            })
        };

        let res = match lookup_is_destination_this_waypoint() {
            Some(r) => Some(r),
            None => {
                if !state.supports_on_demand() {
                    None
                } else {
                    state
                        .fetch_on_demand(strng::new(hbone_dst.to_string()))
                        .await;
                    lookup_is_destination_this_waypoint()
                }
            }
        };

        if res.is_none() || res == Some(false) {
            return Err(Error::IPMismatch(conn.dst.ip(), hbone_addr.ip()));
        }
        Ok(())
    }

    fn find_inbound_upstream(
        state: &DemandProxyState,
        conn: &Connection,
        local_workload: &Workload,
        hbone_addr: SocketAddr,
    ) -> (SocketAddr, AppProtocol, Vec<Arc<Service>>) {
        let upstream_addr = SocketAddr::new(conn.dst.ip(), hbone_addr.port());

        // Application tunnel may override the port.
        let (upstream_addr, inbound_protocol) = match local_workload.application_tunnel.clone() {
            Some(workload::ApplicationTunnel { port, protocol }) => {
                // We may need to override the target port. For instance, we may send all PROXY
                // traffic over a dedicated port like 15088.
                let new_target =
                    SocketAddr::new(upstream_addr.ip(), port.unwrap_or(upstream_addr.port()));
                // Note: the logic to decide which destination address to set inside the PROXY headers
                // is handled outside of this call. This just determines that location we actually send the
                // connection to
                (new_target, protocol)
            }
            None => (upstream_addr, AppProtocol::NONE),
        };
        let services = state.get_services_by_workload(local_workload);

        (upstream_addr, inbound_protocol, services)
    }
}

#[derive(Clone)]
struct InboundCertProvider {
    local_workload: Arc<LocalWorkloadInformation>,
}

#[async_trait::async_trait]
impl crate::tls::ServerCertProvider for InboundCertProvider {
    async fn fetch_cert(&mut self) -> Result<Arc<rustls::ServerConfig>, TlsError> {
        debug!(
            identity=%self.local_workload.workload_info(),
            "fetching cert"
        );
        let cert = self.local_workload.fetch_certificate().await?;
        Ok(Arc::new(cert.server_config()?))
    }
}

pub fn parse_forwarded_host(req: &H2Request) -> Option<String> {
    req.headers()
        .get(http::header::FORWARDED)
        .and_then(|rh| rh.to_str().ok())
        .and_then(|rh| http_types::proxies::Forwarded::parse(rh).ok())
        .and_then(|ph| ph.host().map(|s| s.to_string()))
}

fn build_response(status: StatusCode) -> Response<()> {
    Response::builder()
        .status(status)
        .body(())
        .expect("builder with known status code should not fail")
}

#[cfg(test)]
mod tests {
    use super::Inbound;
    use crate::strng;

    use crate::{
        rbac::Connection,
        state::{
            self,
            service::{Endpoint, EndpointSet, Service},
            workload::{
                application_tunnel::Protocol as AppProtocol, gatewayaddress::Destination,
                ApplicationTunnel, GatewayAddress, NetworkAddress, Protocol, Workload,
            },
            DemandProxyState,
        },
        test_helpers,
    };
    use std::{
        net::SocketAddr,
        sync::{Arc, RwLock},
    };

    use crate::state::workload::HealthStatus;
    use hickory_resolver::config::{ResolverConfig, ResolverOpts};
    use prometheus_client::registry::Registry;
    use test_case::test_case;

    const CLIENT_POD_IP: &str = "10.0.0.1";

    const SERVER_POD_IP: &str = "10.0.0.2";
    const SERVER_SVC_IP: &str = "10.10.0.1";

    const WAYPOINT_POD_IP: &str = "10.0.0.3";
    const WAYPOINT_SVC_IP: &str = "10.10.0.2";

    const TARGET_PORT: u16 = 8080;
    const PROXY_PORT: u16 = 15088;

    const APP_TUNNEL_PROXY: Option<ApplicationTunnel> = Some(ApplicationTunnel {
        port: Some(PROXY_PORT),
        protocol: AppProtocol::PROXY,
    });

    // Regular zTunnel workload traffic inbound
    #[test_case(Waypoint::None, SERVER_POD_IP, SERVER_POD_IP, Some((SERVER_POD_IP, TARGET_PORT)); "to workload no waypoint")]
    // to workload traffic
    #[test_case(Waypoint::Workload(WAYPOINT_POD_IP, None), WAYPOINT_POD_IP, SERVER_POD_IP , Some((WAYPOINT_POD_IP, TARGET_PORT)); "to workload with waypoint referenced by pod")]
    #[test_case(Waypoint::Workload(WAYPOINT_SVC_IP, None), WAYPOINT_POD_IP, SERVER_POD_IP , Some((WAYPOINT_POD_IP, TARGET_PORT)); "to workload with waypoint referenced by vip")]
    #[test_case(Waypoint::Workload(WAYPOINT_SVC_IP, APP_TUNNEL_PROXY), WAYPOINT_POD_IP, SERVER_POD_IP , Some((WAYPOINT_POD_IP, PROXY_PORT)); "to workload with app tunnel")]
    // to service traffic
    #[test_case(Waypoint::Service(WAYPOINT_POD_IP, None), WAYPOINT_POD_IP, SERVER_SVC_IP , Some((WAYPOINT_POD_IP, TARGET_PORT)); "to service with waypoint referenced by pod")]
    #[test_case(Waypoint::Service(WAYPOINT_SVC_IP, None), WAYPOINT_POD_IP, SERVER_SVC_IP , Some((WAYPOINT_POD_IP, TARGET_PORT)); "to service with waypint referenced by vip")]
    #[test_case(Waypoint::Service(WAYPOINT_SVC_IP, APP_TUNNEL_PROXY), WAYPOINT_POD_IP, SERVER_SVC_IP , Some((WAYPOINT_POD_IP, PROXY_PORT)); "to service with app tunnel")]
    // Override port via app_protocol
    // Error cases
    #[test_case(Waypoint::None, SERVER_POD_IP, CLIENT_POD_IP, None; "to server ip mismatch" )]
    #[test_case(Waypoint::None, WAYPOINT_POD_IP, CLIENT_POD_IP, None; "to waypoint without attachment" )]
    #[test_case(Waypoint::Service(WAYPOINT_POD_IP, None), WAYPOINT_POD_IP, SERVER_POD_IP , None; "to workload via waypoint with wrong attachment")]
    #[test_case(Waypoint::Workload(WAYPOINT_POD_IP, None), WAYPOINT_POD_IP, SERVER_SVC_IP , None; "to service via waypoint with wrong attachment")]
    #[tokio::test]
    async fn test_find_inbound_upstream<'a>(
        target_waypoint: Waypoint<'a>,
        connection_dst: &str,
        hbone_dst: &str,
        want: Option<(&str, u16)>,
    ) {
        let state = test_state(target_waypoint).expect("state setup");

        let conn = Connection {
            src_identity: None,
            src: format!("{CLIENT_POD_IP}:1234").parse().unwrap(),
            dst_network: "".into(),
            dst: format!("{connection_dst}:15008").parse().unwrap(),
        };
        let local_wl = state
            .fetch_workload_by_address(&NetworkAddress {
                network: "".into(),
                address: conn.dst.ip(),
            })
            .await
            .unwrap();
        let hbone_addr = format!("{hbone_dst}:{TARGET_PORT}").parse().unwrap();
        let res = Inbound::validate_destination(&state, &conn, &local_wl, hbone_addr)
            .await
            .map(|_| Inbound::find_inbound_upstream(&state, &conn, &local_wl, hbone_addr));

        match want {
            Some((ip, port)) => {
                let got_addr = res.expect("found upstream").0;
                assert_eq!(got_addr, SocketAddr::new(ip.parse().unwrap(), port))
            }
            None => {
                res.expect_err("did not find upstream");
            }
        }
    }

    fn test_state(server_waypoint: Waypoint) -> anyhow::Result<state::DemandProxyState> {
        let mut state = state::ProxyState::new(None);

        let services = vec![
            ("waypoint", WAYPOINT_SVC_IP, Waypoint::None),
            ("server", SERVER_SVC_IP, server_waypoint.clone()),
        ]
        .into_iter()
        .map(|(name, vip, waypoint)| Service {
            name: name.into(),
            namespace: "default".into(),
            hostname: strng::format!("{name}.default.svc.cluster.local"),
            vips: vec![NetworkAddress {
                address: vip.parse().unwrap(),
                network: "".into(),
            }],
            ports: std::collections::HashMap::new(),
            endpoints: EndpointSet::from_list([Endpoint {
                workload_uid: strng::format!("cluster1//v1/Pod/default/{name}"),
                port: std::collections::HashMap::new(),
                status: HealthStatus::Healthy,
            }]),
            subject_alt_names: vec![strng::format!("{name}.default.svc.cluster.local")],
            waypoint: waypoint.service_attached(),
            load_balancer: None,
            ip_families: None,
        });

        let workloads = vec![
            (
                "waypoint",
                WAYPOINT_POD_IP,
                Waypoint::None,
                // the waypoint's _workload_ gets the app tunnel field
                server_waypoint.app_tunnel(),
            ),
            ("client", CLIENT_POD_IP, Waypoint::None, None),
            ("server", SERVER_POD_IP, server_waypoint, None),
        ]
        .into_iter()
        .map(|(name, ip, waypoint, app_tunnel)| Workload {
            workload_ips: vec![ip.parse().unwrap()],
            waypoint: waypoint.workload_attached(),
            protocol: Protocol::HBONE,
            uid: strng::format!("cluster1//v1/Pod/default/{name}"),
            name: strng::format!("workload-{name}"),
            namespace: "default".into(),
            service_account: strng::format!("service-account-{name}"),
            application_tunnel: app_tunnel,
            ..test_helpers::test_default_workload()
        });

        for svc in services {
            state.services.insert(svc);
        }
        for wl in workloads {
            state.workloads.insert(Arc::new(wl));
        }

        let mut registry = Registry::default();
        let metrics = Arc::new(crate::proxy::Metrics::new(&mut registry));
        Ok(DemandProxyState::new(
            Arc::new(RwLock::new(state)),
            None,
            ResolverConfig::default(),
            ResolverOpts::default(),
            metrics,
        ))
    }

    // tells the test if we're using workload-attached or svc-attached waypoints
    #[derive(Clone)]
    enum Waypoint<'a> {
        None,
        Service(&'a str, Option<ApplicationTunnel>),
        Workload(&'a str, Option<ApplicationTunnel>),
    }

    impl<'a> Waypoint<'a> {
        fn app_tunnel(&self) -> Option<ApplicationTunnel> {
            match self.clone() {
                Waypoint::Service(_, v) => v,
                Waypoint::Workload(_, v) => v,
                _ => None,
            }
        }
        fn service_attached(&self) -> Option<GatewayAddress> {
            let Waypoint::Service(s, _) = self else {
                return None;
            };
            Some(GatewayAddress {
                destination: Destination::Address(NetworkAddress {
                    network: strng::EMPTY,
                    address: s.parse().expect("a valid waypoint IP"),
                }),
                hbone_mtls_port: 15008,
            })
        }

        fn workload_attached(&self) -> Option<GatewayAddress> {
            let Waypoint::Workload(w, _) = self else {
                return None;
            };
            Some(GatewayAddress {
                destination: Destination::Address(NetworkAddress {
                    network: strng::EMPTY,
                    address: w.parse().expect("a valid waypoint IP"),
                }),
                hbone_mtls_port: 15008,
            })
        }
    }
}
