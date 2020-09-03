mod ntt;
mod socks;
mod layer4;

use tokio::io::Result;
use tokio::prelude::*;
use bytes::{Bytes, BytesMut, BufMut, Buf};
use tokio::fs::File;
use crate::ntt::NTTStream;
use crate::layer4::recv_loop;

async fn copy<R, W>(src: &mut R, dst: &mut W, cap: usize)
    where R: AsyncRead + Unpin, W: AsyncWrite + Unpin {
}

#[tokio::main]
pub async fn main() -> Result<()> {
    recv_loop();
    Ok(())
}