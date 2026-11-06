#![forbid(unsafe_code)]
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    thread,
    time::Duration,
};
use ores_otel_sidecar::{
    http::handle_connection,
    probe::{NoopProbe, ProductProbe},
    SidecarConfig, SidecarIdentity,
};

struct Unready;
impl ProductProbe for Unready {
    fn ready(&self) -> bool { false }
}

fn start(
    requests: usize,
    probe: impl ProductProbe + Send + 'static,
) -> (SocketAddr, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let config = SidecarConfig::from_bind(SidecarIdentity::ORES_OTEL, &address.to_string(), false)
        .unwrap();
    let worker = thread::spawn(move || {
        for socket in listener.incoming().take(requests) {
            handle_connection(socket.unwrap(), &config, &probe);
        }
    });
    (address, worker)
}

fn exchange(address: SocketAddr, raw_request: &str) -> String {
    let mut socket = TcpStream::connect(address).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    socket.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
    socket.write_all(raw_request.as_bytes()).unwrap();
    let mut payload = String::new();
    socket.read_to_string(&mut payload).unwrap();
    payload
}

fn request(address: SocketAddr, path: &str) -> String {
    exchange(address, &format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"))
}

#[test]
fn independent_services_keep_unready_probe_isolated_under_concurrent_clients() {
    let (healthy, healthy_task) = start(16, NoopProbe);
    let (unhealthy, unhealthy_task) = start(16, Unready);
    let mut tasks = Vec::new();
    for index in 0..16 {
        tasks.push(thread::spawn(move || {
            let route = if index % 2 == 0 { "/readyz" } else { "/healthz" };
            let healthy_result = request(healthy, route);
            let unhealthy_result = request(unhealthy, route);
            assert!(healthy_result.starts_with("HTTP/1.1 200"), "{healthy_result}");
            if route == "/readyz" {
                assert!(unhealthy_result.starts_with("HTTP/1.1 503"), "{unhealthy_result}");
                assert!(unhealthy_result.contains("\"ok\":false"));
            } else {
                assert!(unhealthy_result.starts_with("HTTP/1.1 200"), "{unhealthy_result}");
            }
            assert!(healthy_result.to_ascii_lowercase().contains("cache-control: no-store"));
            assert!(unhealthy_result.to_ascii_lowercase().contains("cache-control: no-store"));
        }));
    }
    for task in tasks { task.join().unwrap(); }
    healthy_task.join().unwrap();
    unhealthy_task.join().unwrap();
}

#[test]
fn rejected_hostile_wire_request_does_not_poison_next_healthy_request() {
    let (address, task) = start(5, NoopProbe);
    let invalid = exchange(address, "GET /healthz HTTP/1.1\r\n\r\n");
    assert!(invalid.starts_with("HTTP/1.1 400"), "{invalid}");
    let encoded = request(address, "/healthz%2fmetrics");
    assert!(encoded.starts_with("HTTP/1.1 404"), "{encoded}");
    let oversized = exchange(address, &format!(
        "GET /healthz HTTP/1.1\r\nHost: localhost\r\nX-Evil: {}\r\n\r\n",
        "a".repeat(1500)
    ));
    assert!(oversized.starts_with("HTTP/1.1 431"), "{oversized}");
    let post = exchange(address, "POST /healthz HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}");
    assert!(post.starts_with("HTTP/1.1 400") || post.starts_with("HTTP/1.1 405"), "{post}");
    let recovery = request(address, "/readyz");
    assert!(recovery.starts_with("HTTP/1.1 200"), "{recovery}");
    task.join().unwrap();
}
