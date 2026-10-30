//! A PostgreSQL wire protocol server, so `psql` and Postgres drivers such as
//! psycopg can query Quarry.
//!
//! Each connection gets its own thread; all of them share one read-only
//! catalog. There is no authentication, so the server should only listen on
//! localhost.

pub mod extended;
pub mod protocol;
pub mod session;
pub mod sql;

use std::io::{self, BufReader, BufWriter};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use std::thread;

use protocol::{
    authentication_ok, backend_key_data, error_response, parameter_status, read_message,
    read_startup, ready_for_query, startup_params, Out, PgError, CANCEL_REQUEST, GSSENC_REQUEST,
    PROTOCOL_3_0, SSL_REQUEST,
};
use session::Session;

use crate::catalog::Catalog;

/// Reported to clients, which use it to decide what features to expect.
/// Quarry speaks the protocol of Postgres 16.
pub const SERVER_VERSION: &str = "16.0 (Quarry)";

/// Accepts connections forever, serving each on its own thread.
pub fn serve(listener: TcpListener, catalog: Arc<Catalog>) -> io::Result<()> {
    let next_id = Arc::new(AtomicI32::new(1));
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("accept failed: {e}");
                continue;
            }
        };
        let catalog = Arc::clone(&catalog);
        let id = next_id.fetch_add(1, Ordering::Relaxed);
        thread::spawn(move || {
            let peer = stream.peer_addr().ok();
            if let Err(e) = handle_connection(stream, catalog, id) {
                if e.kind() != io::ErrorKind::UnexpectedEof {
                    eprintln!("connection {id} ({peer:?}): {e}");
                }
            }
        });
    }
    Ok(())
}

pub fn handle_connection(stream: TcpStream, catalog: Arc<Catalog>, id: i32) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);
    let mut out = Out::new();

    // Startup: decline encryption if asked, then expect protocol 3.0.
    loop {
        let Some((code, body)) = read_startup(&mut reader)? else {
            return Ok(());
        };
        match code {
            SSL_REQUEST | GSSENC_REQUEST => {
                out.raw(b'N');
                out.flush_to(&mut writer)?;
            }
            CANCEL_REQUEST => return Ok(()), // queries can't be cancelled
            PROTOCOL_3_0 => {
                let params = startup_params(&body)?;
                let user = params
                    .iter()
                    .find(|(k, _)| k == "user")
                    .map_or("quarry", |(_, v)| v.as_str());
                authentication_ok(&mut out);
                for (name, value) in [
                    ("server_version", SERVER_VERSION),
                    ("server_encoding", "UTF8"),
                    ("client_encoding", "UTF8"),
                    ("DateStyle", "ISO, MDY"),
                    ("integer_datetimes", "on"),
                    ("standard_conforming_strings", "on"),
                    ("TimeZone", "UTC"),
                    ("session_authorization", user),
                ] {
                    parameter_status(&mut out, name, value);
                }
                backend_key_data(&mut out, id, 0);
                ready_for_query(&mut out, b'I');
                out.flush_to(&mut writer)?;
                break;
            }
            other => {
                let e = PgError::new(
                    "0A000",
                    format!(
                        "unsupported frontend protocol {}.{}",
                        other >> 16,
                        other & 0xffff
                    ),
                );
                error_response(&mut out, &e);
                return out.flush_to(&mut writer);
            }
        }
    }

    let mut session = Session::new(catalog);
    while let Some(msg) = read_message(&mut reader)? {
        let keep_going = session.handle(&msg, &mut out)?;
        out.flush_to(&mut writer)?;
        if !keep_going {
            break;
        }
    }
    Ok(())
}
