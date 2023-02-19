use crate::dest_site::SiteManager;
use crate::lua_config::load_config;
use crate::spool::SpoolManager;
use chrono::Utc;
use message::Message;
use mlua::prelude::*;
use prometheus::{IntGauge, IntGaugeVec};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use timeq::{PopResult, TimeQ, TimerError};
use tokio::sync::{Mutex, MutexGuard};
use tokio::task::JoinHandle;

lazy_static::lazy_static! {
    pub static ref MANAGER: Mutex<QueueManager> = Mutex::new(QueueManager::new());
    static ref DELAY_GAUGE: IntGaugeVec = {
        prometheus::register_int_gauge_vec!("delayed_count", "number of messages in the delayed queue", &["queue"]).unwrap()
    };
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct QueueConfig {
    /// Base retry interval to use in exponential backoff
    #[serde(default = "QueueConfig::default_retry_interval")]
    retry_interval: usize,

    /// Optional cap on the computed retry interval.
    /// Set to the same number as retry_interval to
    /// prevent using exponential backoff
    #[serde(default)]
    max_retry_interval: Option<usize>,

    /// Limits how long a message can remain in the queue
    #[serde(default = "QueueConfig::default_max_age")]
    max_age: usize,
}

impl LuaUserData for QueueConfig {}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            retry_interval: Self::default_retry_interval(),
            max_retry_interval: None,
            max_age: Self::default_max_age(),
        }
    }
}

impl QueueConfig {
    fn default_retry_interval() -> usize {
        60 * 60 * 20 // 20 minutes
    }

    fn default_max_age() -> usize {
        86400 * 7 // 1 week
    }

    pub fn get_max_age(&self) -> chrono::Duration {
        chrono::Duration::seconds(self.max_age as i64)
    }

    pub fn infer_num_attempts(&self, age: chrono::Duration) -> u16 {
        let age = age.num_seconds() as f64;
        let interval = self.retry_interval as f64;

        match self.max_retry_interval {
            None => age.powf(1.0 / interval).floor() as u16,
            Some(limit) => {
                let limit = limit as f64;
                (age / limit).floor() as u16
            }
        }
    }

    pub fn delay_for_attempt(&self, attempt: u16) -> chrono::Duration {
        let delay = self.retry_interval.saturating_pow(1 + attempt as u32);

        let delay = match self.max_retry_interval {
            None => delay,
            Some(limit) => delay.min(limit),
        };

        chrono::Duration::seconds(delay as i64)
    }

    pub fn compute_delay_based_on_age(
        &self,
        num_attempts: u16,
        age: chrono::Duration,
    ) -> Option<chrono::Duration> {
        let overall_delay: i64 = (1..num_attempts)
            .into_iter()
            .map(|i| self.delay_for_attempt(i).num_seconds())
            .sum();
        let overall_delay = chrono::Duration::seconds(overall_delay);

        if overall_delay >= self.get_max_age() {
            None
        } else if overall_delay <= age {
            // Ready now
            Some(chrono::Duration::seconds(0))
        } else {
            Some(overall_delay - age)
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// Returns the list of delays up until the max_age would be reached
    fn compute_schedule(config: &QueueConfig) -> Vec<i64> {
        let mut schedule = vec![];
        let mut age = 0;
        for attempt in 0.. {
            let delay = config.delay_for_attempt(attempt).num_seconds();
            age += delay;
            if age >= config.max_age as i64 {
                return schedule;
            }
            schedule.push(delay);
        }
        unreachable!()
    }

    #[test]
    fn calc_due() {
        let config = QueueConfig {
            retry_interval: 2,
            max_retry_interval: None,
            max_age: 1024,
            ..Default::default()
        };

        assert_eq!(
            compute_schedule(&config),
            vec![2, 4, 8, 16, 32, 64, 128, 256, 512]
        );
    }

    #[test]
    fn calc_due_capped() {
        let config = QueueConfig {
            retry_interval: 2,
            max_retry_interval: Some(8),
            max_age: 128,
            ..Default::default()
        };

        assert_eq!(
            compute_schedule(&config),
            vec![2, 4, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8]
        );
    }

    #[test]
    fn spool_in_delay() {
        let config = QueueConfig {
            retry_interval: 2,
            max_retry_interval: None,
            max_age: 256,
            ..Default::default()
        };

        let mut schedule = vec![];
        let mut age = 2;
        loop {
            let age_chrono = chrono::Duration::seconds(age);
            let num_attempts = config.infer_num_attempts(age_chrono);
            match config.compute_delay_based_on_age(num_attempts, age_chrono) {
                Some(delay) => schedule.push((age, num_attempts, delay.num_seconds())),
                None => break,
            }
            age += 4;
        }

        assert_eq!(
            schedule,
            vec![
                (2, 1, 0),
                (6, 2, 0),
                (10, 3, 2),
                (14, 3, 0),
                (18, 4, 10),
                (22, 4, 6),
                (26, 5, 34),
                (30, 5, 30),
                (34, 5, 26),
                (38, 6, 86),
                (42, 6, 82),
                (46, 6, 78),
                (50, 7, 202),
                (54, 7, 198),
                (58, 7, 194),
                (62, 7, 190)
            ]
        );
    }
}

#[derive(Clone)]
pub struct QueueHandle(Arc<Mutex<Queue>>);

impl QueueHandle {
    pub async fn lock(&self) -> MutexGuard<Queue> {
        self.0.lock().await
    }
}

pub struct Queue {
    name: String,
    queue: TimeQ<Message>,
    maintainer: Option<JoinHandle<()>>,
    last_change: Instant,
    queue_config: QueueConfig,
    delayed_gauge: IntGauge,
}

impl Drop for Queue {
    fn drop(&mut self) {
        if let Some(handle) = self.maintainer.take() {
            handle.abort();
        }
    }
}

impl Queue {
    pub async fn new(name: String) -> anyhow::Result<QueueHandle> {
        let mut config = load_config().await?;

        // TODO: could perhaps crack the standard queue name `campaign:tenant@domain`
        // into its components and pass those down here?
        let queue_config: QueueConfig =
            config.call_callback("get_queue_config", name.to_string())?;

        let delayed_gauge = DELAY_GAUGE.get_metric_with_label_values(&[&name])?;

        let handle = QueueHandle(Arc::new(Mutex::new(Queue {
            name: name.clone(),
            queue: TimeQ::new(),
            maintainer: None,
            last_change: Instant::now(),
            queue_config,
            delayed_gauge,
        })));

        let queue_clone = handle.clone();
        let maintainer = tokio::spawn(async move {
            if let Err(err) = maintain_named_queue(&queue_clone).await {
                tracing::error!(
                    "maintain_named_queue {}: {err:#}",
                    queue_clone.lock().await.name
                );
            }
        });
        handle.lock().await.maintainer.replace(maintainer);
        Ok(handle)
    }

    pub async fn requeue_message(
        &mut self,
        msg: Message,
        increment_attempts: bool,
    ) -> anyhow::Result<()> {
        let id = *msg.id();
        if increment_attempts {
            msg.increment_num_attempts();
            let delay = self.queue_config.delay_for_attempt(msg.get_num_attempts());
            let jitter = (rand::random::<f32>() * 60.) - 30.0;
            let delay = chrono::Duration::seconds(delay.num_seconds() + jitter as i64);

            let now = Utc::now();
            let max_age = self.queue_config.get_max_age();
            let age = msg.age(now);
            if delay + age > max_age {
                // FIXME: expire
                tracing::debug!("expiring {id} {age} > {max_age}");
                SpoolManager::remove_from_spool(id).await?;
                return Ok(());
            }
            msg.delay_by(delay);
        } else {
            msg.delay_with_jitter(60);
        }

        self.insert(msg).await?;

        Ok(())
    }

    async fn insert_delayed(&mut self, msg: Message) -> anyhow::Result<InsertResult> {
        match self.queue.insert(Arc::new(msg.clone())) {
            Ok(_) => {
                self.delayed_gauge.inc();
                if let Err(err) = self.did_insert_delayed(msg.clone()).await {
                    tracing::error!("while shrinking: {}: {err:#}", msg.id());
                }
                Ok(InsertResult::Delayed)
            }
            Err(TimerError::Expired(msg)) => Ok(InsertResult::Ready((*msg).clone())),
            Err(err) => anyhow::bail!("queue insert error: {err:#?}"),
        }
    }

    async fn force_into_delayed(&mut self, msg: Message) -> anyhow::Result<()> {
        loop {
            msg.delay_with_jitter(60);
            match self.insert_delayed(msg.clone()).await? {
                InsertResult::Delayed => return Ok(()),
                // Maybe delay_with_jitter computed an immediate
                // time? Let's try again
                InsertResult::Ready(_) => continue,
            }
        }
    }

    async fn did_insert_delayed(&self, msg: Message) -> anyhow::Result<()> {
        if msg.needs_save() {
            let data_spool = SpoolManager::get_named("data").await?;
            let meta_spool = SpoolManager::get_named("meta").await?;
            msg.save_to(&**meta_spool.lock().await, &**data_spool.lock().await)
                .await?;
        }
        msg.shrink()?;
        Ok(())
    }

    async fn insert_ready(&self, msg: Message) -> anyhow::Result<()> {
        let site = SiteManager::resolve_domain(&self.name).await?;
        let mut site = site.lock().await;
        site.insert(msg)
            .map_err(|_| anyhow::anyhow!("no room in ready queue"))
    }

    pub async fn insert(&mut self, msg: Message) -> anyhow::Result<()> {
        self.last_change = Instant::now();
        match self.insert_delayed(msg.clone()).await? {
            InsertResult::Delayed => Ok(()),
            InsertResult::Ready(msg) => {
                if let Err(_err) = self.insert_ready(msg.clone()).await {
                    self.force_into_delayed(msg).await?;
                }
                Ok(())
            }
        }
    }

    pub fn get_config(&self) -> &QueueConfig {
        &self.queue_config
    }
}

#[must_use]
enum InsertResult {
    Delayed,
    Ready(Message),
}

pub struct QueueManager {
    named: HashMap<String, QueueHandle>,
}

impl QueueManager {
    pub fn new() -> Self {
        Self {
            named: HashMap::new(),
        }
    }

    /// Insert message into a queue named `name`.
    pub async fn insert(&mut self, name: &str, msg: Message) -> anyhow::Result<()> {
        let entry = self.resolve(name).await?;
        let mut entry = entry.lock().await;
        entry.insert(msg).await
    }

    pub async fn resolve(&mut self, name: &str) -> anyhow::Result<QueueHandle> {
        match self.named.get(name) {
            Some(e) => Ok((*e).clone()),
            None => {
                let entry = Queue::new(name.to_string()).await?;
                self.named.insert(name.to_string(), entry.clone());
                Ok(entry)
            }
        }
    }

    pub async fn get() -> MutexGuard<'static, Self> {
        MANAGER.lock().await
    }
}

async fn maintain_named_queue(queue: &QueueHandle) -> anyhow::Result<()> {
    let mut sleep_duration = Duration::from_secs(60);

    loop {
        tokio::time::sleep(sleep_duration).await;
        {
            let mut q = queue.lock().await;
            tracing::debug!(
                "maintaining queue {} which has {} entries",
                q.name,
                q.queue.len()
            );
            let now = Utc::now();
            match q.queue.pop() {
                PopResult::Items(messages) => {
                    q.delayed_gauge.sub(messages.len() as i64);

                    match SiteManager::resolve_domain(&q.name).await {
                        Ok(site) => {
                            let mut site = site.lock().await;

                            let max_age = q.queue_config.get_max_age();

                            for msg in messages {
                                let msg = (*msg).clone();
                                let id = *msg.id();

                                let age = msg.age(now);
                                if age >= max_age {
                                    // TODO: log failure due to expiration
                                    tracing::debug!("expiring {id} {age} > {max_age}");
                                    SpoolManager::remove_from_spool(id).await?;
                                    continue;
                                }

                                match site.insert(msg.clone()) {
                                    Ok(_) => {}
                                    Err(_) => loop {
                                        msg.delay_with_jitter(60);
                                        if matches!(
                                            q.insert_delayed(msg.clone()).await?,
                                            InsertResult::Delayed
                                        ) {
                                            break;
                                        }
                                    },
                                }
                            }
                        }
                        Err(err) => {
                            tracing::error!("Failed to resolve {}: {err:#}", q.name);
                            for msg in messages {
                                q.force_into_delayed((*msg).clone()).await?;
                            }
                        }
                    }
                }
                PopResult::Sleep(duration) => {
                    // We sleep at most 1 minute in case some other actor
                    // re-inserts a message with ~1 minute delay. If we were
                    // sleeping for 4 hours, we wouldn't wake up soon enough
                    // to notice and dispatch it.
                    sleep_duration = duration.min(Duration::from_secs(60));
                }
                PopResult::Empty => {
                    sleep_duration = Duration::from_secs(60);

                    let mut mgr = QueueManager::get().await;
                    if q.last_change.elapsed() > Duration::from_secs(60 * 10) {
                        mgr.named.remove(&q.name);
                        tracing::debug!("idling out queue {}", q.name);
                        // Remove any metrics that go with it, so that we don't
                        // end up using a lot of memory remembering stats from
                        // what might be a long tail of tiny domains forever.
                        DELAY_GAUGE.remove_label_values(&[&q.name]).ok();
                        return Ok(());
                    }
                }
            }
        }
    }
}
