//! `earshot-worker`: the summary worker (T11.6, docs/sync.md section 10).
//!
//! A headless paired device in a container beside the relay: it joins the vault with its own
//! pairing code ([`worker::join`]), then runs the sync client, sends presence (kind `server`,
//! offering summaries) and answers the summary requests other devices address to it through the
//! runtime's own summary path ([`worker::Worker`]). The model runs in a stock llama.cpp server
//! container on the same compose network, reached as an OpenAI-compatible endpoint; whether an
//! endpoint counts as the worker's own local model is [`endpoint`]'s rule.
//!
//! # Privacy
//!
//! The worker sends sealed rows and presence to the configured relay, and transcript text to the
//! configured endpoint, and nothing else; it sends no telemetry and logs no content (its output
//! is fixed words and problem words). Whoever controls its host can read every synced meeting:
//! that is the cost of a key on a server, stated in docs/sync.md section 10.

pub mod config;
pub mod crash;
pub mod endpoint;
pub mod health;
pub mod vault;
pub mod worker;
