use serde_json::Value;
use tokio::io::AsyncReadExt;

pub(crate) async fn request_body(socket: &mut tokio::net::TcpStream) -> Value {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let read = socket.read(&mut chunk).await.unwrap();
        assert!(read > 0, "request ended before its headers");
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(end) = bytes.windows(4).position(|slice| slice == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let headers = std::str::from_utf8(&bytes[..header_end]).unwrap();
    let content_length: usize = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .expect("the JSON request has a bounded content length");
    while bytes.len() - header_end < content_length {
        let read = socket.read(&mut chunk).await.unwrap();
        assert!(read > 0, "request ended before its JSON body");
        bytes.extend_from_slice(&chunk[..read]);
    }
    serde_json::from_slice(&bytes[header_end..header_end + content_length]).unwrap()
}
