use anyhow::{Result, ensure};
use serde_json::Value;
use std::collections::BTreeMap;
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    thread,
};
use trakkin_mappings_source::{FetchRequest, Source};

pub struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
    pub expected: &'static str,
}

pub fn fetch(provider: &mut impl Source, key: &str) -> Result<Option<Value>> {
    Ok(fetch_many(provider, &[key.to_owned()])?
        .remove(key)
        .unwrap())
}

pub fn fetch_many(
    provider: &mut impl Source,
    keys: &[String],
) -> Result<BTreeMap<String, Option<Value>>> {
    let requests: Vec<_> = keys
        .iter()
        .map(|key| FetchRequest {
            key: key.clone(),
            descriptor: provider
                .descriptor(key)
                .unwrap_or_else(|| serde_json::json!({})),
        })
        .collect();
    let records = provider.fetch(&requests)?;
    ensure!(records.len() == keys.len(), "Incomplete provider response");
    Ok(keys.iter().cloned().zip(records).collect())
}

pub fn reply(expected: &'static str, body: Value) -> Reply {
    Reply {
        status: 200,
        body: serde_json::to_vec(&body).unwrap(),
        expected,
    }
}

pub fn server(replies: Vec<Reply>) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        for reply in replies {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            assert!(request.contains(reply.expected), "{request}");
            write_reply(&mut stream, reply, "0");
        }
    });
    (endpoint, handle)
}

pub fn read_request(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let mut request = Vec::new();
    let header_end = loop {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        request.push(byte[0]);
        if request.ends_with(b"\r\n\r\n") {
            break request.len();
        }
    };
    let headers = String::from_utf8(request.clone()).unwrap();
    let length = headers
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|length| length.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    request.resize(header_end + length, 0);
    stream.read_exact(&mut request[header_end..]).unwrap();
    String::from_utf8(request).unwrap()
}

pub fn write_reply(stream: &mut TcpStream, reply: Reply, retry_after: &str) {
    write!(stream,"HTTP/1.1 {} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nRetry-After: {retry_after}\r\n\r\n",reply.status,reply.body.len()).unwrap();
    stream.write_all(&reply.body).unwrap();
}
