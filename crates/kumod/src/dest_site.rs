use crate::lua_config::load_config;
use crate::queue::QueueManager;
use crate::spool::SpoolManager;
use anyhow::Context;
use mail_auth::{IpLookupStrategy, Resolver};
use message::Message;
use mlua::prelude::*;
use prometheus::IntGauge;
use rfc5321::{ClientError, ForwardPath, ReversePath, SmtpClient};
use ringbuf::{HeapRb, Rb};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, MutexGuard, Notify};
use tokio::task::JoinHandle;

lazy_static::lazy_static! {
    static ref MANAGER: Mutex<SiteManager> = Mutex::new(SiteManager::new());
    static ref RESOLVER: Mutex<Resolver> = Mutex::new(Resolver::new_system_conf().unwrap());
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq, Copy)]
pub enum Tls {
    /// Use it if available. If the peer has invalid or self-signed certificates, then
    /// delivery will fail. Will NOT fallback to not using TLS if the peer advertises
    /// STARTTLS.
    Opportunistic,
    /// Use it if available, and allow self-signed or otherwise invalid server certs.
    /// Not recommended for sending to the public internet; this is for local/lab
    /// testing scenarios only.
    OpportunisticInsecure,
    /// TLS with valid certs is required.
    Required,
    /// Required, and allow self-signed or otherwise invalid server certs.
    /// Not recommended for sending to the public internet; this is for local/lab
    /// testing scenarios only.
    RequiredInsecure,
    /// Do not try to use TLS
    Disabled,
}

impl Tls {
    pub fn allow_insecure(&self) -> bool {
        match self {
            Self::OpportunisticInsecure | Self::RequiredInsecure => true,
            _ => false,
        }
    }
}

impl Default for Tls {
    fn default() -> Self {
        Self::Opportunistic
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct DestSiteConfig {
    #[serde(default = "DestSiteConfig::default_connection_limit")]
    connection_limit: usize,

    #[serde(default)]
    enable_tls: Tls,

    #[serde(default = "DestSiteConfig::default_idle_timeout")]
    idle_timeout: u64,

    #[serde(default = "DestSiteConfig::default_max_ready")]
    max_ready: usize,
}

impl LuaUserData for DestSiteConfig {}

impl Default for DestSiteConfig {
    fn default() -> Self {
        Self {
            connection_limit: Self::default_connection_limit(),
            enable_tls: Tls::default(),
            idle_timeout: Self::default_idle_timeout(),
            max_ready: Self::default_max_ready(),
        }
    }
}

impl DestSiteConfig {
    fn default_connection_limit() -> usize {
        32
    }

    fn default_idle_timeout() -> u64 {
        60
    }

    fn default_max_ready() -> usize {
        1024
    }
}

pub struct SiteManager {
    sites: HashMap<String, SiteHandle>,
}

async fn resolve_mx(domain_name: &str) -> anyhow::Result<Vec<String>> {
    let resolver = RESOLVER.lock().await;
    match resolver.mx_lookup(domain_name).await {
        Ok(mxs) if mxs.is_empty() => Ok(vec![domain_name.to_string()]),
        Ok(mxs) => {
            let mut hosts = vec![];
            for mx in mxs.iter() {
                let mut hosts_this_pref: Vec<String> =
                    mx.exchanges.iter().map(|s| s.to_string()).collect();
                hosts_this_pref.sort();
                hosts.append(&mut hosts_this_pref);
            }
            Ok(hosts)
        }
        err @ Err(mail_auth::Error::DnsRecordNotFound(_)) => {
            match resolver.exists(domain_name).await {
                Ok(true) => Ok(vec![domain_name.to_string()]),
                _ => anyhow::bail!("{:#}", err.unwrap_err()),
            }
        }
        Err(err) => anyhow::bail!("MX lookup for {domain_name} failed: {err:#}"),
    }
}

impl SiteManager {
    pub fn new() -> Self {
        Self {
            sites: HashMap::new(),
        }
    }

    pub async fn get() -> MutexGuard<'static, Self> {
        MANAGER.lock().await
    }

    pub async fn resolve_domain(domain_name: &str) -> anyhow::Result<SiteHandle> {
        let mx = Arc::new(resolve_mx(domain_name).await?.into_boxed_slice());
        let name = factor_names(&mx);

        let mut config = load_config().await?;

        let site_config: DestSiteConfig = config.call_callback(
            "get_site_config",
            (domain_name.to_string(), name.to_string()),
        )?;

        let mut manager = Self::get().await;
        let handle = manager.sites.entry(name.clone()).or_insert_with(|| {
            tokio::spawn({
                let name = name.clone();
                async move {
                    loop {
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        let mut mgr = SiteManager::get().await;
                        let site = { mgr.sites.get(&name).cloned() };
                        match site {
                            None => break,
                            Some(site) => {
                                let mut site = site.lock().await;
                                if site.reapable() {
                                    tracing::debug!("idle out site {name}");
                                    mgr.sites.remove(&name);
                                    crate::metrics_helper::remove_metrics_for_service(&format!(
                                        "smtp_client:{name}"
                                    ));
                                    break;
                                }
                            }
                        }
                    }
                }
            });

            let connection_gauge =
                crate::metrics_helper::connection_gauge_for_service(&format!("smtp_client:{name}"));
            let ready = Arc::new(StdMutex::new(HeapRb::new(site_config.max_ready)));
            let notify = Arc::new(Notify::new());
            SiteHandle(Arc::new(Mutex::new(DestinationSite {
                name: name.clone(),
                ready,
                mx,
                notify,
                connections: vec![],
                last_change: Instant::now(),
                site_config,
                connection_gauge,
            })))
        });
        Ok(handle.clone())
    }
}

#[derive(Clone)]
pub struct SiteHandle(Arc<Mutex<DestinationSite>>);

impl SiteHandle {
    pub async fn lock(&self) -> MutexGuard<DestinationSite> {
        self.0.lock().await
    }
}

pub struct DestinationSite {
    name: String,
    mx: Arc<Box<[String]>>,
    ready: Arc<StdMutex<HeapRb<Message>>>,
    notify: Arc<Notify>,
    connections: Vec<JoinHandle<()>>,
    last_change: Instant,
    site_config: DestSiteConfig,
    connection_gauge: IntGauge,
}

impl DestinationSite {
    #[allow(unused)]
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn insert(&mut self, msg: Message) -> Result<(), Message> {
        self.ready.lock().unwrap().push(msg)?;
        self.notify.notify_waiters();
        self.maintain();
        self.last_change = Instant::now();

        Ok(())
    }

    pub fn ready_count(&self) -> usize {
        self.ready.lock().unwrap().len()
    }

    pub fn ideal_connection_count(&self) -> usize {
        ideal_connection_count(self.ready_count(), self.site_config.connection_limit)
    }

    pub fn maintain(&mut self) {
        // Prune completed connection tasks
        self.connections.retain(|handle| !handle.is_finished());

        // TODO: throttle rate at which connections are opened
        let ideal = self.ideal_connection_count();

        for _ in self.connections.len()..ideal {
            // Open a new connection
            let name = self.name.clone();
            let mx = self.mx.clone();
            let ready = Arc::clone(&self.ready);
            let notify = self.notify.clone();
            let site_config = self.site_config.clone();
            let connection_gauge = self.connection_gauge.clone();
            self.connections.push(tokio::spawn(async move {
                if let Err(err) =
                    Dispatcher::run(&name, mx, ready, notify, site_config, connection_gauge).await
                {
                    tracing::error!("Error in dispatch_queue for {name}: {err:#}");
                }
            }));
        }
    }

    pub fn reapable(&mut self) -> bool {
        self.maintain();
        let ideal = self.ideal_connection_count();
        ideal == 0
            && self.connections.is_empty()
            && self.last_change.elapsed() > Duration::from_secs(10 * 60)
    }
}

#[derive(Debug, Clone)]
struct ResolvedAddress {
    #[allow(dead_code)] // used when logging, but rust warns anyway
    mx_host: String,
    addr: IpAddr,
}
async fn resolve_addresses(mx: &Arc<Box<[String]>>) -> Vec<ResolvedAddress> {
    let mut result = vec![];

    for mx_host in mx.iter() {
        match RESOLVER
            .lock()
            .await
            .ip_lookup(mx_host, IpLookupStrategy::default(), 32)
            .await
        {
            Err(err) => {
                tracing::error!("failed to resolve {mx_host}: {err:#}");
                continue;
            }
            Ok(addresses) => {
                for addr in addresses {
                    result.push(ResolvedAddress {
                        mx_host: mx_host.to_string(),
                        addr,
                    });
                }
            }
        }
    }
    result.reverse();
    result
}

struct Dispatcher {
    name: String,
    ready: Arc<StdMutex<HeapRb<Message>>>,
    notify: Arc<Notify>,
    addresses: Vec<ResolvedAddress>,
    msg: Option<Message>,
    client: Option<SmtpClient>,
    client_address: Option<ResolvedAddress>,
    ehlo_name: String,
    site_config: DestSiteConfig,
    connection_gauge: IntGauge,
}

impl Dispatcher {
    async fn run(
        name: &str,
        mx: Arc<Box<[String]>>,
        ready: Arc<StdMutex<HeapRb<Message>>>,
        notify: Arc<Notify>,
        site_config: DestSiteConfig,
        connection_gauge: IntGauge,
    ) -> anyhow::Result<()> {
        let ehlo_name = gethostname::gethostname()
            .to_str()
            .unwrap_or("[127.0.0.1]")
            .to_string();

        let addresses = resolve_addresses(&mx).await;
        let mut dispatcher = Self {
            name: name.to_string(),
            ready,
            notify,
            msg: None,
            client: None,
            client_address: None,
            addresses,
            ehlo_name,
            site_config,
            connection_gauge,
        };

        dispatcher.obtain_message();
        if dispatcher.msg.is_none() {
            // We raced with another dispatcher and there is no
            // more work to be done; no need to open a new connection.
            return Ok(());
        }

        loop {
            if !dispatcher.wait_for_message().await? {
                // No more messages within our idle time; we can close
                // the connection
                tracing::debug!("{} Idling out connection", dispatcher.name);
                return Ok(());
            }
            if let Err(err) = dispatcher.attempt_connection().await {
                dispatcher.connection_gauge.dec();
                if dispatcher.addresses.is_empty() {
                    return Err(err);
                }
                tracing::error!("{err:#}");
                // Try the next candidate MX address
                continue;
            }
            dispatcher.deliver_message().await?;
        }
    }

    fn obtain_message(&mut self) -> bool {
        if self.msg.is_some() {
            return true;
        }
        self.msg = self.ready.lock().unwrap().pop();
        self.msg.is_some()
    }

    async fn wait_for_message(&mut self) -> anyhow::Result<bool> {
        if self.obtain_message() {
            return Ok(true);
        }

        let idle_timeout = Duration::from_secs(self.site_config.idle_timeout);
        match tokio::time::timeout(idle_timeout, self.notify.notified()).await {
            Ok(()) => {}
            Err(_) => {}
        }
        Ok(self.obtain_message())
    }

    async fn attempt_connection(&mut self) -> anyhow::Result<()> {
        if self.client.is_some() {
            return Ok(());
        }

        self.connection_gauge.inc();

        let address = self
            .addresses
            .pop()
            .ok_or_else(|| anyhow::anyhow!("no more addresses to try!"))?;

        let timeout = Duration::from_secs(60);
        let ehlo_name = self.ehlo_name.to_string();
        let mx_host = address.mx_host.to_string();
        let enable_tls = self.site_config.enable_tls;

        let client = tokio::time::timeout(timeout, {
            let address = address.clone();
            async move {
                let mut client = SmtpClient::with_stream(
                    TcpStream::connect((address.addr, 25))
                        .await
                        .with_context(|| format!("connect to {address:?} port 25"))?,
                    &mx_host,
                );

                // Read banner
                let banner = client.read_response().await?;
                if banner.code != 220 {
                    return Err(ClientError::Rejected(banner).into());
                }

                // Say EHLO
                let caps = client.ehlo(&ehlo_name).await?;

                // Use STARTTLS if available.

                let has_tls = caps.contains_key("STARTTLS");
                match (enable_tls, has_tls) {
                    (Tls::Required | Tls::RequiredInsecure, false) => {
                        anyhow::bail!(
                            "tls policy is {enable_tls:?} but STARTTLS is not advertised",
                        );
                    }
                    (Tls::Disabled, _)
                    | (Tls::Opportunistic | Tls::OpportunisticInsecure, false) => {
                        // Do not use TLS
                    }
                    (
                        Tls::Opportunistic
                        | Tls::OpportunisticInsecure
                        | Tls::Required
                        | Tls::RequiredInsecure,
                        true,
                    ) => {
                        client.starttls(enable_tls.allow_insecure()).await?;
                    }
                }

                Ok::<SmtpClient, anyhow::Error>(client)
            }
        })
        .await??;

        self.client.replace(client);
        self.client_address.replace(address);
        Ok(())
    }

    async fn requeue_message(msg: Message, increment_attempts: bool) -> anyhow::Result<()> {
        let mut queue_manager = QueueManager::get().await;
        let queue_name = msg.get_queue_name()?;
        let queue = queue_manager.resolve(&queue_name).await?;
        let mut queue = queue.lock().await;
        queue.requeue_message(msg, increment_attempts).await
    }

    async fn deliver_message(&mut self) -> anyhow::Result<()> {
        let data;
        let sender: ReversePath;
        let recipient: ForwardPath;

        {
            let msg = self.msg.as_ref().unwrap();

            if !msg.is_meta_loaded() {
                let meta_spool = SpoolManager::get_named("meta").await?;
                msg.load_meta(&**meta_spool.lock().await).await?;
            }

            if !msg.is_data_loaded() {
                let data_spool = SpoolManager::get_named("data").await?;
                msg.load_data(&**data_spool.lock().await).await?;
            }

            data = msg.get_data();
            sender = msg
                .sender()?
                .try_into()
                .map_err(|err| anyhow::anyhow!("{err}"))?;
            recipient = msg
                .recipient()?
                .try_into()
                .map_err(|err| anyhow::anyhow!("{err}"))?;
        }

        match self
            .client
            .as_mut()
            .unwrap()
            .send_mail(sender, recipient, &*data)
            .await
        {
            Err(ClientError::Rejected(response)) if response.code >= 400 && response.code < 500 => {
                // Transient failure
                if let Some(msg) = self.msg.take() {
                    Self::requeue_message(msg, true).await?;
                }
                tracing::debug!(
                    "failed to send message to {} {:?}: {response:?}",
                    self.name,
                    self.client_address
                );
            }
            Err(ClientError::Rejected(response)) => {
                tracing::error!(
                    "failed to send message to {} {:?}: {response:?}",
                    self.name,
                    self.client_address
                );
                // FIXME: log permanent failure
                if let Some(msg) = self.msg.take() {
                    SpoolManager::remove_from_spool(*msg.id()).await?;
                }
                self.msg.take();
            }
            Err(err) => {
                // Transient failure; continue with another host
                tracing::error!(
                    "failed to send message to {} {:?}: {err:#}",
                    self.name,
                    self.client_address
                );
            }
            Ok(response) => {
                // FIXME: log success
                if let Some(msg) = self.msg.take() {
                    SpoolManager::remove_from_spool(*msg.id()).await?;
                }
                tracing::debug!("Delivered OK! {response:?}");
            }
        };
        Ok(())
    }
}

impl Drop for Dispatcher {
    fn drop(&mut self) {
        // Ensure that we re-queue any message that we had popped
        if let Some(msg) = self.msg.take() {
            tokio::spawn(async move {
                if let Err(err) = Dispatcher::requeue_message(msg, false).await {
                    tracing::error!("error requeuing message: {err:#}");
                }
            });
        }
        if self.client.is_some() {
            self.connection_gauge.dec();
        }
    }
}

/// Use an exponential decay curve in the increasing form, asymptotic up to connection_limit,
/// passes through 0.0, increasing but bounded to connection_limit.
///
/// Visualize on wolframalpha: "plot 32 * (1-exp(-x * 0.023)), x from 0 to 100, y from 0 to 32"
fn ideal_connection_count(queue_size: usize, connection_limit: usize) -> usize {
    let factor = 0.023;
    let goal = (connection_limit as f32) * (1. - (-1.0 * queue_size as f32 * factor).exp());
    goal.ceil() as usize
}

/// Given a list of host names, produce a pseudo-regex style alternation list
/// of the different elements of the hostnames.
/// The goal is to produce a more compact representation of the name list
/// with the common components factored out.
fn factor_names<S: AsRef<str>>(names: &[S]) -> String {
    let mut max_element_count = 0;

    let mut elements: Vec<Vec<&str>> = vec![];

    let mut split_names = vec![];
    for name in names {
        let name = name.as_ref();
        let mut fields: Vec<_> = name.split('.').map(|s| s.to_lowercase()).collect();
        fields.reverse();
        max_element_count = max_element_count.max(fields.len());
        split_names.push(fields);
    }

    fn add_element<'a>(elements: &mut Vec<Vec<&'a str>>, field: &'a str, i: usize) {
        match elements.get_mut(i) {
            Some(ele) => {
                if !ele.contains(&field) {
                    ele.push(field);
                }
            }
            None => {
                elements.push(vec![field]);
            }
        }
    }

    for fields in &split_names {
        for (i, field) in fields.iter().enumerate() {
            add_element(&mut elements, field, i);
        }
        for i in fields.len()..max_element_count {
            add_element(&mut elements, "?", i);
        }
    }

    let mut result = vec![];
    for mut ele in elements {
        let has_q = ele.contains(&"?");
        ele.retain(|&e| e != "?");
        let mut item_text = if ele.len() == 1 {
            ele[0].to_string()
        } else {
            format!("({})", ele.join("|"))
        };
        if has_q {
            item_text.push('?');
        }
        result.push(item_text);
    }
    result.reverse();

    result.join(".")
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn name_factoring() {
        assert_eq!(
            factor_names(&[
                "mta5.am0.yahoodns.net",
                "mta6.am0.yahoodns.net",
                "mta7.am0.yahoodns.net"
            ]),
            "(mta5|mta6|mta7).am0.yahoodns.net".to_string()
        );

        // Verify that the case is normalized to lowercase
        assert_eq!(
            factor_names(&[
                "mta5.AM0.yahoodns.net",
                "mta6.am0.yAHOodns.net",
                "mta7.am0.yahoodns.net"
            ]),
            "(mta5|mta6|mta7).am0.yahoodns.net".to_string()
        );

        // When the names have mismatched lengths, do we produce
        // something reasonable?
        assert_eq!(
            factor_names(&[
                "gmail-smtp-in.l.google.com",
                "alt1.gmail-smtp-in.l.google.com",
                "alt2.gmail-smtp-in.l.google.com",
                "alt3.gmail-smtp-in.l.google.com",
                "alt4.gmail-smtp-in.l.google.com",
            ]),
            "(alt1|alt2|alt3|alt4)?.gmail-smtp-in.l.google.com".to_string()
        );
    }

    #[test]
    fn connection_limit() {
        let sizes = [
            0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 20, 32, 64, 128, 256, 400, 512, 1024,
        ];
        let max_connections = 32;
        let targets: Vec<(usize, usize)> = sizes
            .iter()
            .map(|&queue_size| {
                (
                    queue_size,
                    ideal_connection_count(queue_size, max_connections),
                )
            })
            .collect();
        assert_eq!(
            vec![
                (0, 0),
                (1, 1),
                (2, 2),
                (3, 3),
                (4, 3),
                (5, 4),
                (6, 5),
                (7, 5),
                (8, 6),
                (9, 6),
                (10, 7),
                (20, 12),
                (32, 17),
                (64, 25),
                (128, 31),
                (256, 32),
                (400, 32),
                (512, 32),
                (1024, 32)
            ],
            targets
        );
    }
}
