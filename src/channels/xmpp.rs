use crate::channels::traits::{Channel, ChannelMessage, SendMessage};
use async_trait::async_trait;
use futures_util::StreamExt;
use portable_atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio_xmpp::parsers::jid::Jid;
use tokio_xmpp::parsers::message::{Lang, Message as XmppMessage, MessageType};
use tokio_xmpp::{Client, Event, Stanza};

static MSG_SEQ: AtomicU64 = AtomicU64::new(0);

/// Style note prepended to every XMPP message before it reaches the LLM.
/// XMPP clients render plain text — avoid markdown that won't render.
const XMPP_STYLE_PREFIX: &str = "\
[context: you are responding over XMPP/Jabber. \
Plain text only. No markdown, no tables, no XML/HTML tags. \
Be concise.]\n";

/// XMPP channel using StartTLS.
///
/// Connects to an XMPP server as the configured JID, listens for incoming
/// `<message/>` stanzas, and forwards them to the ZeroClaw message bus.
/// Outbound replies are sent back to the originating bare JID.
pub struct XmppChannel {
    jid: String,
    password: String,
    allowed_users: Vec<String>,
    /// Sender end of the outbound queue; set once `listen()` starts.
    outbound_tx: Arc<Mutex<Option<mpsc::Sender<SendMessage>>>>,
}

impl XmppChannel {
    pub fn new(jid: String, password: String, allowed_users: Vec<String>) -> Self {
        Self {
            jid,
            password,
            allowed_users,
            outbound_tx: Arc::new(Mutex::new(None)),
        }
    }

    fn is_user_allowed(&self, bare_jid: &str) -> bool {
        if self.allowed_users.iter().any(|u| u == "*") {
            return true;
        }
        self.allowed_users
            .iter()
            .any(|u| u.eq_ignore_ascii_case(bare_jid))
    }
}

/// Strip the resource part from a full JID (`user@server/resource` → `user@server`).
fn bare_jid(jid: &str) -> &str {
    jid.split('/').next().unwrap_or(jid)
}

#[async_trait]
impl Channel for XmppChannel {
    fn name(&self) -> &str {
        "xmpp"
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        let guard = self.outbound_tx.lock().await;
        let tx = guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("XMPP not connected"))?;
        tx.send(message.clone())
            .await
            .map_err(|e| anyhow::anyhow!("XMPP outbound queue closed: {e}"))?;
        Ok(())
    }

    async fn listen(&self, tx: mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        tracing::info!("XMPP channel connecting as {}...", self.jid);

        let (outbound_tx, mut outbound_rx) = mpsc::channel::<SendMessage>(64);
        {
            let mut guard = self.outbound_tx.lock().await;
            *guard = Some(outbound_tx);
        }

        let self_jid: Jid = self
            .jid
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid XMPP JID {:?}: {e}", self.jid))?;
        let mut client = Client::new(self_jid, self.password.clone());

        loop {
            tokio::select! {
                event = client.next() => {
                    match event {
                        None => anyhow::bail!("XMPP stream ended unexpectedly"),
                        Some(Event::Disconnected(err)) => {
                            anyhow::bail!("XMPP disconnected: {err}");
                        }
                        Some(Event::Online { bound_jid, .. }) => {
                            tracing::info!("XMPP online, bound JID: {bound_jid}");
                        }
                        Some(Event::Stanza(Stanza::Message(msg))) => {
                            if msg.type_ == MessageType::Error
                                || msg.type_ == MessageType::Headline
                            {
                                continue;
                            }

                            let body = match msg.get_best_body(vec![]) {
                                Some((_, b)) => b.clone(),
                                None => continue,
                            };
                            if body.trim().is_empty() {
                                continue;
                            }

                            let from = match &msg.from {
                                Some(j) => j.to_string(),
                                None => continue,
                            };
                            let bare = bare_jid(&from).to_string();

                            if !self.is_user_allowed(&bare) {
                                continue;
                            }

                            let seq = MSG_SEQ.fetch_add(1, Ordering::Relaxed);
                            let channel_msg = ChannelMessage {
                                id: format!(
                                    "xmpp_{}_{}",
                                    chrono::Utc::now().timestamp_millis(),
                                    seq
                                ),
                                sender: bare.clone(),
                                reply_target: bare,
                                content: format!("{XMPP_STYLE_PREFIX}{body}"),
                                channel: "xmpp".to_string(),
                                timestamp: std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_secs(),
                                thread_ts: None,
                            };

                            if tx.send(channel_msg).await.is_err() {
                                return Ok(());
                            }
                        }
                        Some(Event::Stanza(_)) => {} // Ignore Iq and Presence stanzas
                    }
                }

                Some(outbound) = outbound_rx.recv() => {
                    let to_jid: Jid = match outbound.recipient.parse() {
                        Ok(j) => j,
                        Err(e) => {
                            tracing::warn!(
                                "XMPP: invalid recipient JID {:?}: {e}",
                                outbound.recipient
                            );
                            continue;
                        }
                    };
                    let msg = XmppMessage::chat(to_jid)
                        .with_body(Lang::new(), outbound.content.clone());
                    if let Err(e) = client.send_stanza(Stanza::Message(msg)).await {
                        tracing::warn!("XMPP send error: {e}");
                    }
                }
            }
        }
    }

    /// Healthy once `listen()` has started and the outbound queue is open.
    async fn health_check(&self) -> bool {
        self.outbound_tx.lock().await.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_channel(allowed: Vec<String>) -> XmppChannel {
        XmppChannel::new("bot@example.com".into(), "secret".into(), allowed)
    }

    #[test]
    fn bare_jid_strips_resource() {
        assert_eq!(bare_jid("user@server.com/phone"), "user@server.com");
        assert_eq!(bare_jid("user@server.com"), "user@server.com");
        assert_eq!(bare_jid("server.com"), "server.com");
    }

    #[test]
    fn name_returns_xmpp() {
        assert_eq!(make_channel(vec![]).name(), "xmpp");
    }

    #[test]
    fn wildcard_allows_any_jid() {
        let ch = make_channel(vec!["*".into()]);
        assert!(ch.is_user_allowed("anyone@example.com"));
        assert!(ch.is_user_allowed("stranger@other.net"));
    }

    #[test]
    fn specific_user_allowed() {
        let ch = make_channel(vec!["alice@example.com".into()]);
        assert!(ch.is_user_allowed("alice@example.com"));
        assert!(!ch.is_user_allowed("eve@example.com"));
    }

    #[test]
    fn allowlist_case_insensitive() {
        let ch = make_channel(vec!["Alice@Example.COM".into()]);
        assert!(ch.is_user_allowed("alice@example.com"));
        assert!(ch.is_user_allowed("ALICE@EXAMPLE.COM"));
    }

    #[test]
    fn empty_allowlist_denies_all() {
        let ch = make_channel(vec![]);
        assert!(!ch.is_user_allowed("anyone@example.com"));
    }

    #[test]
    fn send_errors_when_not_connected() {
        let ch = make_channel(vec!["*".into()]);
        // outbound_tx is None before listen() — send() must error
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(ch.send(&SendMessage::new("hello", "user@example.com")))
            .unwrap_err();
        assert!(err.to_string().contains("not connected"));
    }

    #[test]
    fn health_check_false_before_listen() {
        let ch = make_channel(vec![]);
        let rt = tokio::runtime::Runtime::new().unwrap();
        assert!(!rt.block_on(ch.health_check()));
    }
}
