use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use url::Url;

use crate::config::Config;

#[derive(Debug, Clone)]
pub enum SttEvent {
    Ready,
    Partial {
        text: String,
        is_final: bool,
        speech_final: bool,
    },
    Done {
        text: String,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Deserialize)]
struct ServerEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    is_final: bool,
    #[serde(default)]
    speech_final: bool,
}

pub struct SttSession {
    audio_tx: Option<mpsc::Sender<Vec<u8>>>,
    event_rx: mpsc::Receiver<SttEvent>,
    writer: JoinHandle<()>,
    reader: JoinHandle<()>,
}

impl SttSession {
    pub async fn connect(cfg: &Config, bearer: &str) -> Result<Self> {
        let url = build_ws_url(cfg)?;
        let mut request = url
            .as_str()
            .into_client_request()
            .context("build websocket request")?;
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {bearer}")
                .parse()
                .context("auth header")?,
        );
        request.headers_mut().insert(
            "User-Agent",
            format!("xai-dict/{}", env!("CARGO_PKG_VERSION"))
                .parse()
                .context("user-agent")?,
        );

        let (ws, _) = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            tokio_tungstenite::connect_async(request),
        )
        .await
        .context("STT connect timed out")?
        .context("STT websocket connect")?;

        let (mut write, mut read) = ws.split();
        let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<u8>>(64);
        let (event_tx, event_rx) = mpsc::channel::<SttEvent>(64);

        let writer = tokio::spawn(async move {
            while let Some(chunk) = audio_rx.recv().await {
                if write.send(Message::Binary(chunk.into())).await.is_err() {
                    break;
                }
            }
            let _ = write
                .send(Message::Text(r#"{"type":"audio.done"}"#.into()))
                .await;
        });

        let reader = tokio::spawn(async move {
            while let Some(msg) = read.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        let ev = match serde_json::from_str::<ServerEvent>(&text) {
                            Ok(e) if e.kind == "transcript.created" => Some(SttEvent::Ready),
                            Ok(e) if e.kind == "transcript.partial" => Some(SttEvent::Partial {
                                text: e.text,
                                is_final: e.is_final,
                                speech_final: e.speech_final,
                            }),
                            Ok(e) if e.kind == "transcript.done" => {
                                Some(SttEvent::Done { text: e.text })
                            }
                            Ok(e) if e.kind == "error" => Some(SttEvent::Error {
                                message: if e.message.is_empty() {
                                    e.text
                                } else {
                                    e.message
                                },
                            }),
                            Ok(_) => None,
                            Err(err) => Some(SttEvent::Error {
                                message: format!("parse: {err}"),
                            }),
                        };
                        if let Some(ev) = ev {
                            if event_tx.send(ev).await.is_err() {
                                break;
                            }
                        }
                    }
                    Ok(_) => continue,
                    Err(e) => {
                        let _ = event_tx
                            .send(SttEvent::Error {
                                message: format!("connection lost: {e}"),
                            })
                            .await;
                        break;
                    }
                }
            }
        });

        let mut session = Self {
            audio_tx: Some(audio_tx),
            event_rx,
            writer,
            reader,
        };
        session.wait_ready().await?;
        Ok(session)
    }

    async fn wait_ready(&mut self) -> Result<()> {
        match tokio::time::timeout(std::time::Duration::from_secs(10), self.event_rx.recv()).await {
            Ok(Some(SttEvent::Ready)) => Ok(()),
            Ok(Some(SttEvent::Error { message })) => bail!("STT error: {message}"),
            Ok(Some(_)) => bail!("unexpected STT event before ready"),
            Ok(None) => bail!("STT closed before ready"),
            Err(_) => bail!("timed out waiting for transcript.created"),
        }
    }

    pub fn audio_sender(&self) -> Option<mpsc::Sender<Vec<u8>>> {
        self.audio_tx.clone()
    }

    pub fn finish_audio(&mut self) {
        self.audio_tx.take();
    }

    pub async fn recv(&mut self) -> Option<SttEvent> {
        self.event_rx.recv().await
    }
}

impl Drop for SttSession {
    fn drop(&mut self) {
        self.writer.abort();
        self.reader.abort();
    }
}

fn build_ws_url(cfg: &Config) -> Result<Url> {
    let mut base = cfg.api_base.trim().trim_end_matches('/').to_string();
    if base.starts_with("https://") {
        base = format!("wss://{}", base.trim_start_matches("https://"));
    } else if base.starts_with("http://") {
        bail!("insecure http api_base is not allowed");
    } else if !base.starts_with("wss://") {
        base = format!("wss://{base}");
    }

    // De-dupe /v1 when base already ends with /v1
    let path = if base.ends_with("/v1") && cfg.stt_path.starts_with("/v1/") {
        cfg.stt_path.trim_start_matches("/v1").to_string()
    } else {
        cfg.stt_path.clone()
    };

    let mut url = Url::parse(&format!(
        "{base}{}",
        if path.starts_with('/') {
            path
        } else {
            format!("/{path}")
        }
    ))
    .context("parse STT url")?;

    {
        let mut q = url.query_pairs_mut();
        q.append_pair("sample_rate", &cfg.sample_rate.to_string());
        q.append_pair("encoding", "pcm");
        q.append_pair(
            "interim_results",
            if cfg.interim_results { "true" } else { "false" },
        );
        q.append_pair("endpointing", &cfg.endpointing_ms.to_string());
        if !cfg.language.is_empty() && cfg.language != "auto" {
            q.append_pair("language", &cfg.language);
        }
        for term in &cfg.keyterms {
            if !term.trim().is_empty() {
                q.append_pair("keyterm", term.trim());
            }
        }
    }

    Ok(url)
}
