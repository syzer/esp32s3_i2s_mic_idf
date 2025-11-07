use anyhow::Result;
use serialport::SerialPort;
use std::env;
use std::io::{self, Read, Write};
use std::time::Duration;

fn main() -> Result<()> {
    // Args: <port> [baud]
    let port = env::args()
        .nth(1)
        .unwrap_or("/dev/cu.usbmodem2101".into());
    let baud: u32 = env::args()
        .nth(2)
        .unwrap_or("921600".into())
        .parse()
        .unwrap();

    let mut sp = serialport::new(&port, baud)
        .timeout(Duration::from_millis(50))
        .open()
        .map_err(|e| anyhow::anyhow!("open {} @{} failed: {}", port, baud, e))?;

    // Optional toggles to match Python behavior
    let _ = sp.write_data_terminal_ready(false);
    let _ = sp.write_request_to_send(false);

    let mut buf = [0u8; 4096];
    let mut out = io::stdout().lock();

    loop {
        match sp.read(&mut buf) {
            Ok(n) if n > 0 => {
                let _ = out.write_all(&buf[..n]);
                let _ = out.flush();
            }
            Ok(_) => continue,
            Err(ref e) if e.kind() == io::ErrorKind::TimedOut => continue,
            Err(e) => return Err(anyhow::anyhow!("read error: {}", e)),
        }
    }
}


