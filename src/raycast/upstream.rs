//! One selected channel, independent of account rotation and replay policy.
use super::node_transport::NodeTransport;
use crate::{error::GatewayError, proxies::Transport};
use reqwest::{Client, Request, Response};
use std::sync::Arc;

#[derive(Clone)]
pub struct UpstreamClient {
    pub(crate) native: Client,
    pub(crate) node: Option<Arc<NodeTransport>>,
    pub(crate) channel: i32,
    pub(crate) transport: Transport,
}

impl From<Client> for UpstreamClient {
    fn from(native: Client) -> Self {
        Self {
            native,
            node: None,
            channel: -1,
            transport: Transport::ChatFetch,
        }
    }
}

impl UpstreamClient {
    pub(crate) async fn execute(&self, request: Request) -> Result<Response, GatewayError> {
        match &self.node {
            Some(node) => node.execute(request, self.transport, self.channel).await,
            None => Ok(self.native.execute(request).await?),
        }
    }

    pub(crate) async fn get(
        &self,
        url: &str,
        timeout: std::time::Duration,
    ) -> Result<Response, GatewayError> {
        let request = self.native.get(url).timeout(timeout).build()?;
        self.execute(request).await
    }
}
