//! Frames and the encrypted channel (PROTOCOL.md → transport, "Session").
//!
//! Every frame is `u32 big-endian length` + payload. Before the session key exists the payload is
//! UTF-8 JSON; after it, the payload is `AES-256-GCM(type byte || body)`, where the nonce is
//! `u32 direction || u64 counter` and the counters are implicit, so a replayed or reordered frame
//! fails to authenticate.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::Aes256Gcm;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_FRAME: usize = 4 * 1024 * 1024;
/// File bytes per data frame, as the phone sends them.
pub const CHUNK: usize = 256 * 1024;
pub const DIR_PHONE_TO_HOST: u32 = 1;
pub const DIR_HOST_TO_PHONE: u32 = 2;

const TYPE_JSON: u8 = 0x4a;
const TYPE_DATA: u8 = 0x44;

#[derive(Debug)]
pub enum Msg {
    Json(Value),
    Data(Vec<u8>),
}

impl Msg {
    pub fn into_json(self) -> anyhow::Result<Value> {
        match self {
            Msg::Json(v) => Ok(v),
            Msg::Data(_) => anyhow::bail!("expected a control message, got file bytes"),
        }
    }
}

/// Length-prefixed frames over any byte stream.
pub struct FrameIo<S> {
    stream: S,
}

impl<S> FrameIo<S> {
    pub fn new(stream: S) -> Self {
        Self { stream }
    }

    pub fn into_inner(self) -> S {
        self.stream
    }
}

impl<S: AsyncRead + Unpin> FrameIo<S> {
    pub async fn read_frame(&mut self) -> anyhow::Result<Vec<u8>> {
        let len = self.stream.read_u32().await? as usize;
        if len == 0 || len > MAX_FRAME {
            anyhow::bail!("bad frame length {len}");
        }
        let mut buf = vec![0u8; len];
        self.stream.read_exact(&mut buf).await?;
        Ok(buf)
    }

    pub async fn read_json(&mut self) -> anyhow::Result<Value> {
        let frame = self.read_frame().await?;
        Ok(serde_json::from_slice(&frame)?)
    }
}

impl<S: AsyncWrite + Unpin> FrameIo<S> {
    pub async fn write_frame(&mut self, body: &[u8]) -> anyhow::Result<()> {
        if body.is_empty() || body.len() > MAX_FRAME {
            anyhow::bail!("refusing to send a {} byte frame", body.len());
        }
        self.stream.write_u32(body.len() as u32).await?;
        self.stream.write_all(body).await?;
        self.stream.flush().await?;
        Ok(())
    }

    pub async fn write_json(&mut self, v: &Value) -> anyhow::Result<()> {
        self.write_frame(serde_json::to_vec(v)?.as_slice()).await
    }
}

/// One side of the AES-256-GCM channel, with its own counters per direction.
pub struct SecureChannel<S> {
    io: FrameIo<S>,
    cipher: Aes256Gcm,
    send_dir: u32,
    recv_dir: u32,
    send_counter: u64,
    recv_counter: u64,
}

impl<S> SecureChannel<S> {
    pub fn new(stream: S, key: &[u8; 32], send_dir: u32, recv_dir: u32) -> Self {
        Self {
            io: FrameIo::new(stream),
            cipher: Aes256Gcm::new(key.into()),
            send_dir,
            recv_dir,
            send_counter: 0,
            recv_counter: 0,
        }
    }
}

impl<S: AsyncWrite + Unpin> SecureChannel<S> {
    pub async fn send_json(&mut self, v: &Value) -> anyhow::Result<()> {
        let body = serde_json::to_vec(v)?;
        self.send(TYPE_JSON, &body).await
    }

    pub async fn send_data(&mut self, data: &[u8]) -> anyhow::Result<()> {
        self.send(TYPE_DATA, data).await
    }

    async fn send(&mut self, kind: u8, body: &[u8]) -> anyhow::Result<()> {
        let nonce = nonce(self.send_dir, self.send_counter);
        self.send_counter += 1;
        let mut plain = Vec::with_capacity(body.len() + 1);
        plain.push(kind);
        plain.extend_from_slice(body);
        let sealed = self
            .cipher
            .encrypt((&nonce).into(), plain.as_slice())
            .map_err(|_| anyhow::anyhow!("frame encryption failed"))?;
        self.io.write_frame(&sealed).await
    }
}

impl<S: AsyncRead + Unpin> SecureChannel<S> {
    pub async fn recv(&mut self) -> anyhow::Result<Msg> {
        let frame = self.io.read_frame().await?;
        if frame.len() < 17 {
            anyhow::bail!("short secure frame ({} bytes)", frame.len());
        }
        let nonce = nonce(self.recv_dir, self.recv_counter);
        self.recv_counter += 1;
        let plain = self
            .cipher
            .decrypt((&nonce).into(), frame.as_slice())
            .map_err(|_| anyhow::anyhow!("could not decrypt: wrong key, or a forged frame"))?;
        match plain[0] {
            TYPE_JSON => Ok(Msg::Json(serde_json::from_slice(&plain[1..])?)),
            TYPE_DATA => Ok(Msg::Data(plain[1..].to_vec())),
            other => anyhow::bail!("unknown message type {other:#x}"),
        }
    }

    pub async fn recv_json(&mut self) -> anyhow::Result<Value> {
        self.recv().await?.into_json()
    }
}

fn nonce(dir: u32, counter: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[..4].copy_from_slice(&dir.to_be_bytes());
    n[4..].copy_from_slice(&counter.to_be_bytes());
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn plain_frames_round_trip() {
        let (a, b) = tokio::io::duplex(4096);
        let mut a = FrameIo::new(a);
        let mut b = FrameIo::new(b);
        a.write_json(&json!({"t": "ping", "n": 1})).await.unwrap();
        assert_eq!(b.read_json().await.unwrap()["t"], "ping");
        b.write_frame(b"raw").await.unwrap();
        assert_eq!(a.read_frame().await.unwrap(), b"raw");
    }

    #[tokio::test]
    async fn secure_channel_round_trip() {
        let (phone, host) = tokio::io::duplex(1 << 20);
        let key = [42u8; 32];
        let mut phone_ch = SecureChannel::new(phone, &key, DIR_PHONE_TO_HOST, DIR_HOST_TO_PHONE);
        let mut host_ch = SecureChannel::new(host, &key, DIR_HOST_TO_PHONE, DIR_PHONE_TO_HOST);

        phone_ch
            .send_json(&json!({"t": "send", "count": 1}))
            .await
            .unwrap();
        phone_ch.send_data(b"file bytes").await.unwrap();
        let first = host_ch.recv_json().await.unwrap();
        assert_eq!(first["count"], 1);
        match host_ch.recv().await.unwrap() {
            Msg::Data(d) => assert_eq!(d, b"file bytes"),
            other => panic!("expected data, got {other:?}"),
        }

        host_ch
            .send_json(&json!({"t": "ok", "saved": 1}))
            .await
            .unwrap();
        assert_eq!(phone_ch.recv_json().await.unwrap()["saved"], 1);
    }

    #[tokio::test]
    async fn a_replayed_frame_is_refused() {
        use tokio::io::AsyncWriteExt;

        let (mut phone, host) = tokio::io::duplex(4096);
        let key = [9u8; 32];
        let mut host_ch = SecureChannel::new(host, &key, DIR_HOST_TO_PHONE, DIR_PHONE_TO_HOST);

        let mut plain = vec![TYPE_JSON];
        plain.extend_from_slice(&serde_json::to_vec(&json!({"t": "ka"})).unwrap());
        let sealed = Aes256Gcm::new((&key).into())
            .encrypt((&nonce(DIR_PHONE_TO_HOST, 0)).into(), plain.as_slice())
            .unwrap();
        let mut framed = (sealed.len() as u32).to_be_bytes().to_vec();
        framed.extend_from_slice(&sealed);

        phone.write_all(&framed).await.unwrap();
        assert!(host_ch.recv().await.is_ok());
        phone.write_all(&framed).await.unwrap();
        assert!(host_ch.recv().await.is_err(), "a replay is refused");
    }
}
