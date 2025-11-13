use anyhow::Result;

#[cfg(feature = "websocket")]
mod websocket;

#[cfg(not(feature = "websocket"))]
mod jtag;

#[cfg(not(feature = "websocket"))]
fn main() -> Result<()> {
    jtag::run()
}

#[cfg(feature = "websocket")]
fn main() -> Result<()> {
    websocket::run()
}
