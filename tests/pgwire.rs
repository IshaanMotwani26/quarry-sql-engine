//! The Postgres wire protocol server, driven by a minimal client that speaks
//! the protocol directly.

use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

use quarry::catalog::Catalog;
use quarry::csv::{parse_csv, CsvOptions};
use quarry::pgwire::protocol::{read_message, Body, Message, PROTOCOL_3_0, SSL_REQUEST};
use quarry::pgwire::serve;

fn start_server() -> SocketAddr {
    let mut catalog = Catalog::new();
    let csv = "id,name,score\n1,ada,9.5\n2,grace,\n3,linus,7\n";
    catalog
        .register(parse_csv("people", csv, &CsvOptions::default(), None).unwrap())
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || serve(listener, Arc::new(catalog)));
    addr
}

struct Client {
    stream: TcpStream,
}

/// What the server sent in reply to one request, up to ReadyForQuery.
#[derive(Debug, Default)]
struct Reply {
    columns: Vec<(String, i32)>,
    rows: Vec<Vec<Option<String>>>,
    tags: Vec<String>,
    /// (SQLSTATE, message, position)
    errors: Vec<(String, String, Option<String>)>,
    /// Every message tag, in order.
    kinds: String,
    status: char,
}

impl Client {
    fn connect(addr: SocketAddr) -> Client {
        let mut stream = TcpStream::connect(addr).unwrap();
        // Ask for SSL first, as psql does; the server must decline.
        let mut ssl = Vec::new();
        ssl.extend(8i32.to_be_bytes());
        ssl.extend(SSL_REQUEST.to_be_bytes());
        stream.write_all(&ssl).unwrap();
        let mut answer = [0u8];
        std::io::Read::read_exact(&mut stream, &mut answer).unwrap();
        assert_eq!(answer[0], b'N');

        let params = b"user\0test\0database\0quarry\0\0";
        let mut startup = Vec::new();
        startup.extend((8 + params.len() as i32).to_be_bytes());
        startup.extend(PROTOCOL_3_0.to_be_bytes());
        startup.extend(params);
        stream.write_all(&startup).unwrap();
        let mut client = Client { stream };
        let reply = client.read_reply();
        assert_eq!(reply.kinds.chars().next(), Some('R'), "{reply:?}");
        assert!(reply.kinds.contains('S') && reply.kinds.contains('K'));
        assert_eq!(reply.status, 'I');
        client
    }

    fn send(&mut self, tag: u8, body: &[u8]) {
        let mut msg = vec![tag];
        msg.extend((body.len() as i32 + 4).to_be_bytes());
        msg.extend(body);
        self.stream.write_all(&msg).unwrap();
    }

    fn query(&mut self, sql: &str) -> Reply {
        let mut body = sql.as_bytes().to_vec();
        body.push(0);
        self.send(b'Q', &body);
        self.read_reply()
    }

    fn read_reply(&mut self) -> Reply {
        let mut reply = Reply::default();
        loop {
            let Message { tag, body } = read_message(&mut self.stream).unwrap().unwrap();
            reply.kinds.push(tag as char);
            let mut b = Body::new(&body);
            match tag {
                b'T' => {
                    for _ in 0..b.i16().unwrap() {
                        let name = b.cstr().unwrap();
                        b.bytes(6).unwrap();
                        let oid = b.i32().unwrap();
                        b.bytes(8).unwrap();
                        reply.columns.push((name, oid));
                    }
                }
                b'D' => {
                    let row = (0..b.i16().unwrap())
                        .map(|_| match b.i32().unwrap() {
                            -1 => None,
                            n => Some(
                                String::from_utf8(b.bytes(n as usize).unwrap().to_vec()).unwrap(),
                            ),
                        })
                        .collect();
                    reply.rows.push(row);
                }
                b'C' => reply.tags.push(b.cstr().unwrap()),
                b'E' => {
                    let (mut code, mut msg, mut pos) = (String::new(), String::new(), None);
                    loop {
                        match b.u8().unwrap() {
                            0 => break,
                            b'C' => code = b.cstr().unwrap(),
                            b'M' => msg = b.cstr().unwrap(),
                            b'P' => pos = Some(b.cstr().unwrap()),
                            _ => drop(b.cstr().unwrap()),
                        }
                    }
                    reply.errors.push((code, msg, pos));
                }
                b'Z' => {
                    reply.status = b.u8().unwrap() as char;
                    return reply;
                }
                _ => {}
            }
        }
    }
}

fn cells(rows: &[Vec<Option<String>>]) -> Vec<Vec<&str>> {
    rows.iter()
        .map(|r| r.iter().map(|c| c.as_deref().unwrap_or("NULL")).collect())
        .collect()
}

#[test]
fn simple_query_returns_typed_rows() {
    let mut c = Client::connect(start_server());
    let r = c.query("select id, name, score, score > 8 as good from people order by id");
    assert_eq!(
        r.columns,
        [
            ("id".to_string(), 20),
            ("name".to_string(), 25),
            ("score".to_string(), 701),
            ("good".to_string(), 16)
        ]
    );
    assert_eq!(
        cells(&r.rows),
        [
            ["1", "ada", "9.5", "t"],
            ["2", "grace", "NULL", "NULL"],
            ["3", "linus", "7", "f"]
        ]
    );
    assert_eq!(r.tags, ["SELECT 3"]);
    assert_eq!(r.kinds, "TDDDCZ");
}

#[test]
fn several_statements_stop_at_the_first_error() {
    let mut c = Client::connect(start_server());
    let r = c.query("select 1; select count(*) from people; select 1/0; select 2");
    assert_eq!(r.tags, ["SELECT 1", "SELECT 1"]);
    assert_eq!(cells(&r.rows), [["1"], ["3"]]);
    assert_eq!(r.errors[0].0, "22012");
    assert_eq!(r.status, 'I');

    let r = c.query(" ;; -- nothing\n");
    assert_eq!(r.kinds, "IZ", "an empty query gets EmptyQueryResponse");
}

#[test]
fn errors_carry_sqlstate_and_position() {
    let mut c = Client::connect(start_server());
    let r = c.query("select 1;\nselect name frm people");
    let (code, _, pos) = &r.errors[0];
    assert_eq!(code, "42601");
    // `frm` parses as a column alias, so the error is at `people`, the 27th
    // character of the whole query string.
    assert_eq!(pos.as_deref(), Some("27"));

    assert_eq!(c.query("select * from nope").errors[0].0, "42P01");
    assert_eq!(c.query("select nope from people").errors[0].0, "42703");
}

#[test]
fn transaction_commands_are_accepted() {
    let mut c = Client::connect(start_server());
    let r = c.query("BEGIN");
    assert_eq!((r.tags[0].as_str(), r.status), ("BEGIN", 'T'));
    assert_eq!(c.query("SET search_path TO public").tags, ["SET"]);
    assert_eq!(c.query("select x from people").status, 'E');
    let r = c.query("select 1");
    assert_eq!((r.errors[0].0.as_str(), r.status), ("25P02", 'E'));
    let r = c.query("COMMIT");
    assert_eq!((r.tags[0].as_str(), r.status), ("ROLLBACK", 'I'));
}

#[test]
fn serves_clients_concurrently() {
    let addr = start_server();
    let handles: Vec<_> = (0..8)
        .map(|i| {
            thread::spawn(move || {
                let mut c = Client::connect(addr);
                for _ in 0..20 {
                    let r = c.query(&format!("select {i} + count(*) from people"));
                    assert_eq!(cells(&r.rows), [[(i + 3).to_string().as_str()]]);
                }
                c.send(b'X', &[]);
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}
